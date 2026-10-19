#!/usr/bin/env python3
"""Emulated BCM4356 Bluetooth controller on UART-D, for nxtest.py --bt.

The script acts as the chip behind HCI over H4: it answers the bring-up
(reset, Broadcom baud and patch RAM commands, address), and holds one
virtual Pro Controller in pairing mode. The host finds it by inquiry,
pairs (Secure Simple Pairing, Just Works), opens the HID channels and
gets full input reports every 15 ms, with A pressed and the left stick
pushed right for a moment. Then the controller drops the link and pages
the host again, as a paired pad does on a button press: the link key
is checked and the pad opens the channels itself. `hw_errors` resets
answer with a Hardware Error first, as the Erista's core does after a
cold boot.
"""
import queue, select, socket, struct, threading, time

PAD_ADDR = bytes([0x56, 0x34, 0x12, 0xE9, 0xB6, 0x98])  # 98:B6:E9:12:34:56
PAD_CLASS = bytes([0x08, 0x25, 0x00])                   # peripheral, gamepad
PAD_NAME = b"Pro Controller"
HANDLE = 0x0040
CHIP_NAME = b"BCM4356A2 emulated"
KEY = bytes(range(0x10, 0x20))


def le16(b, i=0):
    return b[i] | b[i + 1] << 8


class Fake:
    def __init__(self, path, log=print, reconnect=True, hw_errors=0):
        self.path, self.log, self.reconnect = path, log, reconnect
        self.hw_errors = hw_errors
        self.sock = None
        self.addr = bytes(6)
        self.stop = threading.Event()
        self.timers = []
        self.lock = threading.Lock()
        # Sends go through their own thread: a UART is full duplex, and
        # QEMU takes bytes only as the guest drains its FIFO.
        self.out = queue.Queue()
        self.reset_state()
        self.stats = {"commands": 0, "patch": 0, "reports": 0, "subcmds": [], "pairs": 0,
                      "reconnects": 0, "key_ok": None, "hw_errors": 0}

    def reset_state(self):
        self.connected = False
        self.chans = {}          # our cid -> [psm, host cid, conf_in, conf_out]
        self.next_cid = 0x0040
        self.ident = 0
        self.mode = None
        self.streaming = False
        self.ready_at = None

    # --- transport ---

    def start(self):
        threading.Thread(target=self.run, daemon=True).start()

    def connect(self):
        # QEMU creates the socket once UART-A's client is in.
        for _ in range(300):
            try:
                self.sock = socket.socket(socket.AF_UNIX)
                self.sock.connect(self.path)
                return True
            except OSError:
                time.sleep(0.1)
        self.log("[btfake] no UART-D socket")
        return False

    def send(self, b):
        self.out.put(b)

    def writer(self):
        while not self.stop.is_set():
            try:
                b = self.out.get(timeout=0.1)
            except queue.Empty:
                continue
            try:
                self.sock.sendall(b)
            except OSError:
                self.stop.set()

    def event(self, code, params):
        self.send(bytes([0x04, code, len(params)]) + params)

    def complete(self, op, ret=b"\x00"):
        self.event(0x0E, bytes([1]) + struct.pack("<H", op) + ret)

    def status(self, op, st=0):
        self.event(0x0F, bytes([st, 1]) + struct.pack("<H", op))

    def acl(self, data):
        self.send(struct.pack("<BHH", 0x02, HANDLE | 0x2000, len(data)) + data)

    def l2cap(self, cid, payload):
        self.acl(struct.pack("<HH", len(payload), cid) + payload)

    def signal(self, code, ident, data):
        self.l2cap(1, bytes([code, ident]) + struct.pack("<H", len(data)) + data)

    def later(self, delay, fn):
        with self.lock:
            self.timers.append((time.time() + delay, fn))

    def run(self):
        if not self.connect():
            return
        threading.Thread(target=self.writer, daemon=True).start()
        buf = b""
        next_report = 0
        while not self.stop.is_set():
            try:
                if select.select([self.sock], [], [], 0.005)[0]:
                    chunk = self.sock.recv(65536)
                    if not chunk:
                        break
                    buf += chunk
                    self.stats["bytes"] = self.stats.get("bytes", 0) + len(chunk)
            except OSError:
                break
            buf = self.parse(buf)
            now = time.time()
            with self.lock:
                due = [t for t in self.timers if t[0] <= now]
                self.timers = [t for t in self.timers if t[0] > now]
            for _, fn in due:
                fn()
            if self.streaming and now >= next_report and self.out.qsize() < 4:
                self.report()
                next_report = now + 0.015

    def parse(self, buf):
        while buf:
            t = buf[0]
            if t == 0x01:
                if len(buf) < 4 or len(buf) < 4 + buf[3]:
                    break
                op, n = le16(buf, 1), buf[3]
                self.command(op, buf[4:4 + n])
                buf = buf[4 + n:]
            elif t == 0x02:
                if len(buf) < 5 or len(buf) < 5 + le16(buf, 3):
                    break
                n = le16(buf, 3)
                self.acl_in(buf[5:5 + n])
                buf = buf[5 + n:]
                # One buffer back to the host per packet.
                self.event(0x13, bytes([1]) + struct.pack("<HH", HANDLE, 1))
            else:
                buf = buf[1:]
        return buf

    # --- HCI commands ---

    def command(self, op, p):
        self.stats["commands"] += 1
        if op == 0xFC4C:
            self.stats["patch"] += 1
        if op == 0x0C03 and self.hw_errors > 0:   # reset: Hardware Error 0x00
            self.hw_errors -= 1
            self.stats["hw_errors"] += 1
            self.event(0x10, b"\x00")
        elif op == 0x1001:   # local version: Broadcom
            self.complete(op, struct.pack("<BBHBHH", 0, 6, 0x1000, 6, 0x000F, 0x230F))
        elif op == 0x0C14:   # local name
            self.complete(op, b"\x00" + CHIP_NAME.ljust(248, b"\x00"))
        elif op == 0xFC01:   # Broadcom write BD_ADDR
            self.addr = bytes(p[:6])
            self.complete(op)
        elif op == 0x1009:
            self.complete(op, b"\x00" + self.addr)
        elif op == 0x1005:   # buffer size: 1021-byte ACL, 8 buffers
            self.complete(op, struct.pack("<BHBHH", 0, 1021, 64, 8, 0))
        elif op == 0x0401:   # inquiry: the pad is in pairing mode
            self.status(op)
            if not self.connected and self.stats["pairs"] == 0:
                res = bytes([1]) + PAD_ADDR + bytes([1, 0]) + PAD_CLASS + b"\x00\x00" + bytes([0xD0])
                self.later(0.3, lambda: self.event(0x22, res))
            self.later(1.0, lambda: self.event(0x01, b"\x00"))
        elif op == 0x0402:
            self.complete(op)
        elif op == 0x0405:   # create connection
            self.status(op)
            if bytes(p[:6]) == PAD_ADDR:
                self.later(0.05, self.conn_complete)
        elif op == 0x0409:   # accept connection
            self.status(op)
            self.later(0.05, self.conn_complete)
        elif op == 0x0419:   # remote name
            self.status(op)
            self.event(0x07, b"\x00" + PAD_ADDR + PAD_NAME.ljust(248, b"\x00"))
        elif op == 0x0411:   # authentication: link key asked first
            self.status(op)
            self.event(0x17, PAD_ADDR)
        elif op == 0x040B:   # link key reply
            self.complete(op, b"\x00" + PAD_ADDR)
            ok = bytes(p[6:22]) == KEY
            self.stats["key_ok"] = ok
            self.event(0x06, bytes([0 if ok else 0x06]) + struct.pack("<H", HANDLE))
            if ok and self.incoming:
                # A reconnecting pad encrypts and opens its channels itself.
                self.event(0x08, b"\x00" + struct.pack("<H", HANDLE) + b"\x01")
                self.later(0.1, lambda: self.open(0x11))
        elif op == 0x040C:   # no key: Secure Simple Pairing
            self.complete(op, b"\x00" + PAD_ADDR)
            self.event(0x31, PAD_ADDR)
        elif op == 0x042B:   # IO capability reply
            self.complete(op, b"\x00" + PAD_ADDR)
            self.event(0x32, PAD_ADDR + bytes([3, 0, 4]))
            self.event(0x33, PAD_ADDR + struct.pack("<I", 123456))
        elif op == 0x042C:   # user confirmation
            self.complete(op, b"\x00" + PAD_ADDR)
            self.event(0x36, b"\x00" + PAD_ADDR)
            self.event(0x18, PAD_ADDR + KEY + bytes([4]))
            self.event(0x06, b"\x00" + struct.pack("<H", HANDLE))
            self.stats["pairs"] += 1
        elif op == 0x0413:   # encryption on
            self.status(op)
            self.event(0x08, b"\x00" + struct.pack("<H", HANDLE) + b"\x01")
        elif op == 0x0803:   # sniff mode: granted at the asked interval
            self.status(op)
            self.event(0x14, b"\x00" + struct.pack("<H", HANDLE) + b"\x02" + bytes(p[2:4]))
            self.stats["sniff"] = self.stats.get("sniff", 0) + 1
        elif op == 0x0406:   # disconnect
            self.status(op)
            self.dropped(p[2])
        else:
            self.complete(op)

    def conn_complete(self):
        self.reset_state()
        self.connected = True
        self.event(0x03, b"\x00" + struct.pack("<H", HANDLE) + PAD_ADDR + b"\x01\x00")
        if self.incoming:
            # A paired pad authenticates its host: link key asked.
            self.later(0.02, lambda: self.event(0x17, PAD_ADDR))

    def dropped(self, reason):
        if self.connected:
            self.connected = False
            self.streaming = False
            self.event(0x05, b"\x00" + struct.pack("<H", HANDLE) + bytes([reason]))

    def page_host(self):
        """A paired pad woken by a button press connects to its host."""
        self.incoming = True
        self.stats["reconnects"] += 1
        self.event(0x04, PAD_ADDR + PAD_CLASS + b"\x01")

    # --- L2CAP (device side) ---

    def open(self, psm):
        cid = self.next_cid
        self.next_cid += 1
        self.chans[cid] = [psm, 0, False, False]
        self.ident += 1
        self.signal(0x02, self.ident, struct.pack("<HH", psm, cid))

    def conf(self, cid):
        self.ident += 1
        self.signal(0x04, self.ident, struct.pack("<HH", self.chans[cid][1], 0))

    def acl_in(self, d):
        n, cid = struct.unpack("<HH", d[:4])
        payload = d[4:4 + n]
        if cid == 1:
            self.sig(payload)
        else:
            for ours, c in self.chans.items():
                if ours == cid and c[0] == 0x13:
                    self.hid_out(payload)

    def sig(self, p):
        while len(p) >= 4:
            code, ident, n = p[0], p[1], le16(p, 2)
            d, p = p[4:4 + n], p[4 + n:]
            if code == 0x02:      # connection request (host opens)
                psm, hcid = struct.unpack("<HH", d[:4])
                cid = self.next_cid
                self.next_cid += 1
                self.chans[cid] = [psm, hcid, False, False]
                self.signal(0x03, ident, struct.pack("<HHHH", cid, hcid, 0, 0))
                self.conf(cid)
            elif code == 0x03:    # connection response
                hcid, cid, result = struct.unpack("<HHH", d[:6])
                if cid in self.chans and result == 0:
                    self.chans[cid][1] = hcid
                    self.conf(cid)
            elif code == 0x04:    # configuration request
                cid = le16(d)
                c = self.chans[cid]
                c[2] = True
                self.signal(0x05, ident, struct.pack("<HHH", c[1], 0, 0))
                self.opened(cid)
            elif code == 0x05:    # configuration response
                cid = le16(d)
                self.chans[cid][3] = True
                self.opened(cid)
            elif code == 0x0A:    # information request
                self.signal(0x0B, ident, d[:2] + b"\x01\x00")

    def opened(self, cid):
        c = self.chans[cid]
        if not (c[2] and c[3]):
            return
        if c[0] == 0x11 and self.incoming and not any(x[0] == 0x13 for x in self.chans.values()):
            self.open(0x13)

    # --- HID ---

    def hid_cid(self):
        for c in self.chans.values():
            if c[0] == 0x13:
                return c[1]
        return None

    def input_report(self, rid, extra=b""):
        t = time.time() - (self.ready_at or time.time())
        buttons = 0x000008 if 0.5 <= t < 0.8 else 0       # A
        lx = 3100 if 1.2 <= t < 1.5 else 2048            # left stick right
        stick = lambda x, y: bytes([x & 0xFF, (x >> 8) | ((y & 0xF) << 4), y >> 4])
        r = bytes([rid, 0, 0x90]) + struct.pack("<I", buttons)[:3] + stick(lx, 2048) + stick(2048, 2048) + b"\x00"
        r += extra
        return bytes([0xA1]) + r.ljust(49, b"\x00")

    def report(self):
        cid = self.hid_cid()
        if cid is None:
            return
        self.stats["reports"] += 1
        self.l2cap(cid, self.input_report(0x30))

    def hid_out(self, p):
        if len(p) < 13 or p[0] != 0xA2 or p[1] != 0x01:
            return
        sub, arg = p[11], p[12]
        self.stats["subcmds"].append(sub)
        data = b""
        if sub == 0x02:   # device info: firmware 4.33, Pro Controller
            data = bytes([0x04, 0x21, 0x03, 0x02]) + PAD_ADDR[::-1] + b"\x01\x02"
        elif sub == 0x03:
            self.mode = arg
        self.l2cap(self.hid_cid(), self.input_report(0x21, bytes([0x80 | (sub == 0x02) * 2, sub]) + data))
        if sub == 0x30:   # player lights: last setup step
            self.streaming = self.mode == 0x30
            self.ready_at = time.time()
            if self.reconnect and self.stats["reconnects"] == 0:
                self.later(3.0, lambda: self.dropped(0x13))
                self.later(4.5, self.page_host)

    incoming = False

    def summary(self):
        s = self.stats
        return (f"bytes {s.get('bytes', 0)}, commands {s['commands']}, patch records {s['patch']}, paired {s['pairs']}, "
                f"reports {s['reports']}, subcommands {[hex(x) for x in s['subcmds']]}, "
                f"reconnects {s['reconnects']}, hardware errors {s['hw_errors']}, sniff {s.get('sniff', 0)}, link key {'checked' if s['key_ok'] else s['key_ok']}")
