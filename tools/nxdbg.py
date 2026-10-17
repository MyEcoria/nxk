#!/usr/bin/env python3
"""Debug channel to the Switch over its USB-C console (usbcon): screen
capture, touch and gamepad input, as if a person used the console.

The requests travel inside the serial stream of tools/usbterm.py as APC
frames (nxk_abi::proto::dbg); do not run both on the same port at once.
Standard library only.

    nxdbg.py ping
    nxdbg.py shot out.png                 # what the screen shows, landscape
    nxdbg.py tap 640 360                  # landscape pixels, 1280x720
    nxdbg.py swipe 1000 300 200 300 400   # x1 y1 x2 y2 [ms]
    nxdbg.py button A                     # A B X Y L R ZL ZR PLUS MINUS
                                          # UP DOWN LEFT RIGHT LSTICK RSTICK
                                          # HOME CAPTURE VOL_UP VOL_DOWN POWER POWER_LONG, [hold ms]
    nxdbg.py tap 640 360 wait 0.5 shot after.png    # chained, in order
    nxdbg.py --emu /tmp/usb.sock shot out.png       # tegra_qemu's USB port
"""
import argparse, base64, struct, sys, time, zlib
from array import array

import usbterm

START, END = b"\x1b_N", b"\x1b\\"
# nxk_abi::proto::dbg
PING, SCREEN, TOUCH, BUTTON = 0, 1, 2, 3
DONE, MORE, ERROR = 0, 1, 2
# nxk_abi::proto::input
TOUCH_DOWN, TOUCH_MOVE, TOUCH_UP = 1, 2, 3
BUTTONS = ["A", "B", "X", "Y", "L", "R", "ZL", "ZR", "PLUS", "MINUS", "UP",
           "DOWN", "LEFT", "RIGHT", "LSTICK", "RSTICK", "HOME", "CAPTURE", "VOL_UP", "VOL_DOWN",
           "POWER", "POWER_LONG"]
# Bytes held back by usbcon after the terminal opens (100 ms), and a margin.
SETTLE = 0.3


class Channel:
    def __init__(self, link, timeout=10.0):
        self.link, self.timeout = link, timeout
        self.buf = b""
        self.next_id = 0
        # A fresh tty may echo what arrives before it is raw; read it out.
        self.pump(SETTLE)
        self.buf = b""

    def pump(self, wait):
        data = self.link.read(wait)
        self.buf += data
        return data

    def frame(self, wait):
        """Next complete frame's payload, or None after `wait` seconds."""
        end = time.time() + wait
        while True:
            s = self.buf.find(START)
            if s >= 0:
                e = self.buf.find(END, s + len(START))
                if e >= 0:
                    body = self.buf[s + len(START):e]
                    self.buf = self.buf[e + len(END):]
                    try:
                        return base64.b64decode(body)
                    except ValueError:
                        continue
            else:
                # Keep a possible start cut by the read.
                self.buf = self.buf[-(len(START) - 1):]
            left = end - time.time()
            if left <= 0:
                return None
            self.pump(min(left, 0.2))

    def request(self, op, args=b""):
        """Send a request; payloads of its replies, until the last one."""
        self.next_id = self.next_id % 255 + 1
        rid = self.next_id
        payload = bytes([rid, op]) + args
        self.link.write(START + base64.b64encode(payload) + END)
        out = []
        while True:
            p = self.frame(self.timeout)
            if p is None:
                raise TimeoutError(f"no reply to op {op}")
            if len(p) < 2 or p[0] != rid:
                continue  # a reply to an older request
            if p[1] == ERROR:
                raise OSError(struct.unpack_from("<H", p, 2)[0], "nxk refused the request")
            out.append(p[2:])
            if p[1] == DONE:
                return out

    def ping(self):
        return self.request(PING)[0][0]

    def touch(self, action, x, y, finger=0):
        self.request(TOUCH, struct.pack("<BBHH", action, finger, x, y))

    def button(self, name, value):
        self.request(BUTTON, bytes([BUTTONS.index(name) + 1, value]))

    def screen(self):
        """The image on display: width, height, pixels (R, G, B, A bytes),
        in scan order whatever the layout in memory."""
        data = b"".join(self.request(SCREEN))
        w, h, _fmt, lay = struct.unpack_from("<HHBB", data, 0)
        px, i = array("I"), 6
        while i < len(data):
            (n,) = struct.unpack_from("<H", data, i)
            i += 2
            if n & 0x8000:
                px.extend(array("I", data[i:i + 4]) * ((n & 0x7FFF) + 1))
                i += 4
            else:
                px.frombytes(data[i:i + 4 * (n + 1)])
                i += 4 * (n + 1)
        if len(px) != w * h:
            raise ValueError(f"capture is {len(px)} pixels, not {w}x{h}")
        if lay & 0xF == 1:
            px = detile(px, w, h, lay >> 4)
        return w, h, px


def detile(px, w, h, block_log2):
    """Pixels of a block-linear image (GOBs of 64 x 8 bytes, 1 << block_log2
    high per block, one wide) in scan order."""
    gobs_x = (w * 4 + 63) // 64
    gob_rows = 8 << block_log2
    out = array("I", bytes(4 * w * h))
    # Offset of (x, y) in its GOB, in pixels: x bits 3-0 of the byte
    # offset, y bit 0, x bit 4, y bits 2-1, x bit 5.
    inner = [[(((x * 4) % 64) // 32) * 256 + (y // 2) * 64 + (((x * 4) % 32) // 16) * 32
              + (y % 2) * 16 + (x * 4) % 16 for x in range(16)] for y in range(8)]
    for y in range(h):
        gy, row = divmod(y, 8)
        blk, in_blk = gy >> block_log2, gy & ((1 << block_log2) - 1)
        for gx in range(gobs_x):
            base = ((blk * gobs_x + gx) * (512 << block_log2) + in_blk * 512) // 4
            x0 = gx * 16
            n = min(16, w - x0)
            o = y * w + x0
            for k in range(n):
                out[o + k] = px[base + inner[row][k] // 4]
    return out


def write_png(path, w, h, rows):
    """`rows`: h byte strings of w RGB pixels."""
    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data))
    raw = b"".join(b"\0" + r for r in rows)
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)))
        f.write(chunk(b"IDAT", zlib.compress(raw, 6)))
        f.write(chunk(b"IEND", b""))


def rgb(row):
    b = row.tobytes()
    out = bytearray(len(b) // 4 * 3)
    out[0::3], out[1::3], out[2::3] = b[0::4], b[1::4], b[2::4]
    return bytes(out)


def shot(ch, path, panel):
    w, h, px = ch.screen()
    if panel or h <= w:
        rows = [rgb(px[y * w:(y + 1) * w]) for y in range(h)]
        write_png(path, w, h, rows)
        return w, h
    # Portrait panel, landscape UI turned: pixel (x, y) is image row
    # h-1-x, column y.
    rows = []
    for y in range(w):
        col = px[y::w]
        col.reverse()
        rows.append(rgb(col))
    write_png(path, h, w, rows)
    return h, w


ARITY = {"ping": 0, "shot": 1, "tap": 2, "down": 2, "move": 2, "up": 2,
         "swipe": 4, "button": 1, "wait": 1}
OPTIONAL = {"tap": 1, "swipe": 1, "button": 1}


def take(args, verb):
    """The verb's arguments: its fixed ones, then optional numbers."""
    n = ARITY[verb]
    got = args[:n]
    if len(got) < n:
        sys.exit(f"nxdbg: {verb} takes {n} arguments")
    rest = args[n:]
    for _ in range(OPTIONAL.get(verb, 0)):
        if rest and rest[0].lstrip("-").replace(".", "", 1).isdigit():
            got.append(rest.pop(0))
    return got, rest


def run(ch, cmds, panel):
    while cmds:
        verb = cmds.pop(0)
        if verb not in ARITY:
            sys.exit(f"nxdbg: unknown command {verb!r}")
        a, cmds = take(cmds, verb)
        if verb == "ping":
            print(f"nxk debug channel, protocol {ch.ping()}")
        elif verb == "shot":
            t = time.time()
            w, h = shot(ch, a[0], panel)
            print(f"{a[0]}: {w}x{h} in {time.time() - t:.2f} s")
        elif verb == "tap":
            x, y = int(a[0]), int(a[1])
            ch.touch(TOUCH_DOWN, x, y)
            time.sleep((int(a[2]) if len(a) > 2 else 60) / 1000)
            ch.touch(TOUCH_UP, x, y)
        elif verb in ("down", "move", "up"):
            ch.touch({"down": TOUCH_DOWN, "move": TOUCH_MOVE, "up": TOUCH_UP}[verb],
                     int(a[0]), int(a[1]))
        elif verb == "swipe":
            x1, y1, x2, y2 = map(int, a[:4])
            ms = int(a[4]) if len(a) > 4 else 300
            steps = max(2, ms // 30)
            ch.touch(TOUCH_DOWN, x1, y1)
            for i in range(1, steps + 1):
                time.sleep(ms / 1000 / steps)
                ch.touch(TOUCH_MOVE if i < steps else TOUCH_UP,
                         x1 + (x2 - x1) * i // steps, y1 + (y2 - y1) * i // steps)
        elif verb == "button":
            name = a[0].upper()
            if name not in BUTTONS:
                sys.exit(f"nxdbg: button is one of {' '.join(BUTTONS)}")
            ch.button(name, 1)
            time.sleep((int(a[1]) if len(a) > 1 else 60) / 1000)
            ch.button(name, 0)
        elif verb == "wait":
            time.sleep(float(a[0]))


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--dev", help="serial device (default: found by USB ID)")
    ap.add_argument("--emu", help="tegra_qemu USB socket instead of a serial device")
    ap.add_argument("--panel", action="store_true", help="shot: panel orientation (portrait)")
    ap.add_argument("--timeout", type=float, default=10.0)
    ap.add_argument("commands", nargs="+", help="ping | shot F | tap X Y [MS] | swipe X1 Y1 X2 Y2 [MS] | "
                    "button NAME [MS] | down/move/up X Y | wait S")
    a = ap.parse_args()
    if a.emu:
        link = usbterm.EmuHost(a.emu, timeout=a.timeout)
    else:
        path = a.dev or usbterm.find_tty()
        if not path:
            sys.exit("nxdbg: no nxk console (lsusb should list 1209:0001)")
        link = usbterm.Serial(path)
    ch = Channel(link, a.timeout)
    try:
        run(ch, a.commands, a.panel)
    except (TimeoutError, OSError, ValueError) as e:
        sys.exit(f"nxdbg: {e}")


if __name__ == "__main__":
    main()
