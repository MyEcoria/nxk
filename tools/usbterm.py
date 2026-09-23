#!/usr/bin/env python3
"""Terminal on the nxk console over the Switch's USB-C port.

nxk's usbcon driver makes the Switch a USB serial device (CDC-ACM,
1209:0001): the PC's keyboard types into the nxk shell, its output comes
back. Standard library only.

    usbterm.py                       # physical Switch: /dev/ttyACM* found by ID
    usbterm.py --dev /dev/ttyACM1    # explicit serial device
    usbterm.py -c "ps" -c "mem"      # type commands, print until the prompt
    usbterm.py --put LOCAL:/bin/x    # copy a file to the card (shell `recv`)
    usbterm.py --reload wlan -c "wifi status"   # new driver build, restarted
    usbterm.py --emu /tmp/usb.sock   # tegra_qemu: act as the PC's USB host

Interactive mode: Ctrl-] quits. In the emulator, QEMU needs
`-chardev socket,id=usbotg,path=SOCK,server=on,wait=off`; this script then
plays the host side of the bus (enumeration, bulk transfers).
"""
import argparse, base64, glob, io, os, select, socket, struct, sys, termios, time, tty, zlib

VID, PID = 0x1209, 0x0001
PROMPT = b"nxk> "
QUIT = 0x1D  # Ctrl-]
# base64 characters per typed line: the console keeps at most 1000.
PUT_LINE = 512


class Stall(Exception):
    pass


class Serial:
    """Switch seen by the PC's cdc_acm driver (raw tty)."""

    def __init__(self, path):
        self.fd = os.open(path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        # Raw at once, nothing flushed: echoed bytes would be typed.
        tty.setraw(self.fd, termios.TCSANOW)
        self.name = path

    def read(self, timeout):
        if not select.select([self.fd], [], [], timeout)[0]:
            return b""
        try:
            data = os.read(self.fd, 4096)
        except BlockingIOError:
            return b""
        if not data:
            raise ConnectionError("serial device closed")
        return data

    def write(self, data):
        while data:
            try:
                data = data[os.write(self.fd, data):]
            except BlockingIOError:
                select.select([], [self.fd], [], 1.0)

    def close(self):
        os.close(self.fd)


class EmuHost:
    """USB host of tegra_qemu's OTG port: one request per USB token.

    request:  op, endpoint, length (LE16), payload (SETUP: 8 bytes, OUT data)
    response: status (A ack, N nak, S stall, D data), length (LE16), payload
    """

    def __init__(self, path, timeout=10.0):
        self.sock = socket.socket(socket.AF_UNIX)
        end = time.time() + timeout
        while True:
            try:
                self.sock.connect(path)
                break
            except OSError:
                if time.time() > end:
                    raise
                time.sleep(0.1)
        self.timeout = timeout
        self.name = f"tegra_qemu ({path})"
        self.enumerate()

    def _recv(self, n):
        buf = b""
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise ConnectionError("emulator closed the USB link")
            buf += chunk
        return buf

    def _req(self, op, ep, length, payload=b""):
        self.sock.sendall(struct.pack("<cBH", op, ep, length) + payload)
        st, n = struct.unpack("<cH", self._recv(3))
        return st, self._recv(n)

    def _until(self, what, attempt):
        """Retry `attempt` while the device NAKs."""
        end = time.time() + self.timeout
        while True:
            st, data = attempt()
            if st == b"S":
                raise Stall(what)
            if st != b"N":
                return data
            if time.time() > end:
                raise TimeoutError(f"{what}: device keeps NAKing")
            time.sleep(0.001)

    def _in(self, ep, maxlen):
        return self._until(f"IN {ep}", lambda: self._req(b"I", ep, maxlen))

    def _out(self, ep, data):
        self._until(f"OUT {ep}", lambda: self._req(b"O", ep, len(data), data))

    def control(self, kind, req, value, index, length=0, data=b""):
        """Control transfer on EP0: IN data returned, OUT `data` sent."""
        n = length if kind & 0x80 else len(data)
        setup = struct.pack("<BBHHH", kind, req, value, index, n)
        self._until("SETUP", lambda: self._req(b"S", 0, 8, setup))
        if kind & 0x80:
            buf = b""
            while len(buf) < length:
                chunk = self._in(0, 64)
                buf += chunk
                if len(chunk) < 64:
                    break
            self._out(0, b"")
            return buf
        for i in range(0, len(data), 64):
            self._out(0, data[i:i + 64])
        if self._in(0, 64):
            raise ConnectionError("status stage carried data")
        return b""

    def string(self, index):
        if not index:
            return ""
        d = self.control(0x80, 6, 0x0300 | index, 0x0409, 255)
        return d[2:d[0]].decode("utf-16-le", "replace")

    def enumerate(self):
        end = time.time() + self.timeout
        while not self._req(b"Q", 0, 0)[1][0] & 1:
            if time.time() > end:
                raise TimeoutError("device never attached (usbcon not running?)")
            time.sleep(0.05)
        self._until("bus reset", lambda: self._req(b"R", 0, 0))
        time.sleep(0.01)
        dev = self.control(0x80, 6, 0x0100, 0, 64)
        self.control(0x00, 5, 1, 0)  # SET_ADDRESS
        time.sleep(0.002)
        dev = self.control(0x80, 6, 0x0100, 0, 18)
        vid, pid = struct.unpack_from("<HH", dev, 8)
        cfg = self.control(0x80, 6, 0x0200, 0, 9)
        cfg = self.control(0x80, 6, 0x0200, 0, struct.unpack_from("<H", cfg, 2)[0])
        self.comm, self.bulk_in, self.bulk_out, self.mps, iface, klass = None, None, None, 64, None, None
        p = 0
        while p + 2 <= len(cfg) and cfg[p]:
            if cfg[p + 1] == 4:
                iface, klass = cfg[p + 2], cfg[p + 5]
                if klass == 0x02 and self.comm is None:
                    self.comm = iface
            elif cfg[p + 1] == 5 and klass == 0x0A and cfg[p + 3] & 3 == 2:
                addr, self.mps = cfg[p + 2], struct.unpack_from("<H", cfg, p + 4)[0]
                if addr & 0x80:
                    self.bulk_in = addr & 0xF
                else:
                    self.bulk_out = addr
            p += cfg[p]
        if None in (self.comm, self.bulk_in, self.bulk_out):
            raise ConnectionError("no CDC-ACM interfaces in the configuration")
        product = self.string(dev[15])
        self.control(0x00, 9, cfg[5], 0)  # SET_CONFIGURATION
        self.control(0x21, 0x20, 0, self.comm, data=struct.pack("<IBBB", 115200, 0, 0, 8))
        self.control(0x21, 0x22, 3, self.comm)  # DTR and RTS: terminal open
        speed = "high" if self._req(b"Q", 0, 0)[1][0] & 2 else "full"
        self.name = f"{product} {vid:04x}:{pid:04x}, {speed} speed, {self.name}"

    def read(self, timeout):
        end = time.time() + timeout
        buf = b""
        while True:
            st, data = self._req(b"I", self.bulk_in, self.mps)
            if st == b"S":
                raise Stall("bulk IN")
            if st == b"D":
                buf += data
                continue
            if buf or time.time() >= end:
                return buf
            time.sleep(0.002)

    def write(self, data):
        for i in range(0, len(data), self.mps):
            self._out(self.bulk_out, data[i:i + self.mps])

    def close(self):
        self.sock.close()


def find_tty():
    """ttyACM of the nxk console: udev symlink, else by USB IDs."""
    if os.path.exists("/dev/nxk-console"):
        return "/dev/nxk-console"
    for d in sorted(glob.glob("/sys/class/tty/ttyACM*")):
        usb = os.path.dirname(os.path.realpath(os.path.join(d, "device")))
        try:
            ids = [open(os.path.join(usb, f)).read().strip() for f in ("idVendor", "idProduct")]
        except OSError:
            continue
        if ids == [f"{VID:04x}", f"{PID:04x}"]:
            return "/dev/" + os.path.basename(d)
    return None


def read_until(link, marker, timeout, out):
    """Output until `marker` shows up (or `timeout`): (text, found)."""
    buf = b""
    end = time.time() + timeout
    while time.time() < end:
        data = link.read(min(0.2, max(0.0, end - time.time())))
        if data:
            buf += data
            out.write(data.decode("utf-8", "replace"))
            out.flush()
            if marker in buf:
                return buf, True
    return buf, False


def drain(link, quiet=0.3, limit=5.0):
    """Swallow pending output (history kept while no terminal read)."""
    end = time.time() + limit
    while time.time() < end and link.read(quiet):
        pass


def put(link, local, remote, timeout):
    """File to the Switch's card through the shell's `recv`: base64 lines,
    each one echoed before the next (flow control), CRC-32 checked there."""
    data = open(local, "rb").read()
    enc = base64.b64encode(data)
    quiet = io.StringIO()
    link.write(f"recv {remote} {len(data)} {zlib.crc32(data):08x}\r".encode())
    seen, ok = read_until(link, b"\n", timeout, quiet)
    t0 = time.time()
    for i in range(0, len(enc), PUT_LINE):
        if not ok:
            break
        link.write(enc[i:i + PUT_LINE] + b"\r")
        buf, ok = read_until(link, b"\n", timeout, quiet)
        seen += buf
    if ok and PROMPT not in seen:
        buf, ok = read_until(link, PROMPT, timeout, quiet)
        seen += buf
    verdict = [l for l in seen.decode("utf-8", "replace").splitlines() if "recv:" in l]
    print(f"[usbterm] {local} -> {remote}: {len(data)} bytes in "
          f"{time.time() - t0:.1f} s; {verdict[-1] if verdict else 'no answer'}")
    return ok and bool(verdict) and f"recv: {remote} (" in verdict[-1]


def run(link, cmds, timeout, out=sys.stdout, puts=()):
    """Type each command, print its output up to the next prompt."""
    drain(link)
    link.write(b"\r")
    if not read_until(link, PROMPT, timeout, out)[1]:
        print("\n[usbterm] no nxk prompt", file=sys.stderr)
        return False
    for local, remote in puts:
        if not put(link, local, remote, timeout):
            return False
    for c in cmds:
        link.write(c.encode() + b"\r")
        if not read_until(link, PROMPT, timeout, out)[1]:
            print(f"\n[usbterm] '{c}' missed the prompt in {timeout} s", file=sys.stderr)
            return False
    out.write("\n")
    return True


def interactive(link):
    """Keyboard to the Switch, its output to the screen, until Ctrl-]."""
    print(f"[usbterm] {link.name}; Ctrl-] quits\r", file=sys.stderr)
    stdin = sys.stdin.fileno()
    saved = termios.tcgetattr(stdin)
    tty.setraw(stdin)
    try:
        link.write(b"\r")
        while True:
            if select.select([stdin], [], [], 0.0)[0]:
                keys = os.read(stdin, 1024)
                if QUIT in keys:
                    keys = keys[:keys.index(QUIT)]
                    link.write(keys)
                    break
                link.write(keys)
            data = link.read(0.02)
            if data:
                os.write(sys.stdout.fileno(), data)
    finally:
        termios.tcsetattr(stdin, termios.TCSADRAIN, saved)
        print("\r\n[usbterm] bye", file=sys.stderr)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--dev", help="serial device (default: /dev/ttyACM* with ID 1209:0001)")
    ap.add_argument("--emu", metavar="SOCKET", help="tegra_qemu 'usbotg' chardev socket")
    ap.add_argument("-c", "--cmd", action="append", default=[], help="command to type (repeatable)")
    ap.add_argument("--timeout", type=float, default=30, help="per-command timeout (s)")
    ap.add_argument("--put", action="append", default=[], metavar="LOCAL:REMOTE",
                    help="copy a file to the Switch's card first (repeatable)")
    ap.add_argument("--reload", action="append", default=[], metavar="DRIVER",
                    help="put the built DRIVER in /bin and restart it (./tools/build.sh first)")
    a = ap.parse_args()
    puts = [tuple(p.rsplit(":", 1)) for p in a.put]
    cache = os.environ.get("NXK_CACHE", os.path.expanduser("~/.cache/nxk"))
    for d in a.reload:
        puts.append((os.path.join(cache, "target-user/aarch64-unknown-none/release", d), f"/bin/{d}"))
    cmds = [f"restart {d}" for d in a.reload] + a.cmd
    try:
        if a.emu:
            link = EmuHost(a.emu, timeout=a.timeout)
        else:
            path = a.dev or find_tty()
            if not path:
                sys.exit("[usbterm] no nxk console on USB (1209:0001): is nxk running with "
                         "the cable plugged in? See hwboot/README.md, or use --dev")
            link = Serial(path)
    except PermissionError as e:
        sys.exit(f"[usbterm] {e}: install hwboot/99-nxk-console.rules (or join the "
                 "dialout/uucp group)")
    except (OSError, ConnectionError, TimeoutError, Stall) as e:
        sys.exit(f"[usbterm] {e}")
    try:
        if cmds or puts:
            print(f"[usbterm] {link.name}", file=sys.stderr)
            sys.exit(0 if run(link, cmds, a.timeout, puts=puts) else 1)
        interactive(link)
    except (ConnectionError, TimeoutError, Stall) as e:
        sys.exit(f"\n[usbterm] {e}")
    finally:
        link.close()


if __name__ == "__main__":
    main()
