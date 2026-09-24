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

Interactive mode edits lines as zsh does: on the PC (Emacs keys, history
in ~/.local/state/nxk/history with Up/Down prefix search and Ctrl-R,
syntax highlighting), each typed whole once Enter is pressed. A two-line
prompt (powerlevel10k "lean") shows the directory, the last status and
duration, and the time. `clear` (the client's own) wipes the PC's screen
and shows the logo again. Ctrl-D on an empty line, `exit` or Ctrl-] quits;
--raw keeps a plain terminal (keys typed as they come, the shell's echo).

Logs (services, kernel: what the console puts between its LOG marks) are
left out but with --logs (dimmed when interactive) or --reload.

In the emulator, QEMU needs
`-chardev socket,id=usbotg,path=SOCK,server=on,wait=off`; this script then
plays the host side of the bus (enumeration, bulk transfers).
"""
import argparse, base64, codecs, glob, io, os, re, select, signal, socket, struct, sys, termios, threading, time, tty, unicodedata, zlib

VID, PID = 0x1209, 0x0001
PROMPT = b"nxk> "
QUIT = 0x1D  # Ctrl-]
# base64 characters per typed line: the console keeps at most 1000.
PUT_LINE = 512
# nxk_abi console::LOG_START / LOG_END: a log comes between them.
LOG_START, LOG_END = b"\x1b]7700;log\x07", b"\x1b]7700;end\x07"


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
        # One request at a time: the console and a network thread share it.
        self.lock = threading.Lock()
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
        with self.lock:
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
        # CDC-ECM function, if any: control interface, data interface, its
        # bulk pair (alternate setting 1).
        self.ecm, self.ecm_data, self.net_in, self.net_out = None, None, None, None
        sub = None
        p = 0
        while p + 2 <= len(cfg) and cfg[p]:
            if cfg[p + 1] == 4:
                iface, klass, sub = cfg[p + 2], cfg[p + 5], cfg[p + 6]
                if klass == 0x02 and sub == 0x02 and self.comm is None:
                    self.comm = iface
                if klass == 0x02 and sub == 0x06:
                    self.ecm = iface
                if klass == 0x0A and self.ecm is not None:
                    self.ecm_data = iface
            elif cfg[p + 1] == 5 and klass == 0x0A and cfg[p + 3] & 3 == 2:
                addr = cfg[p + 2]
                if self.ecm is not None and iface == self.ecm_data:
                    if addr & 0x80:
                        self.net_in = addr & 0xF
                    else:
                        self.net_out = addr
                elif self.comm is not None and iface == self.comm + 1:
                    self.mps = struct.unpack_from("<H", cfg, p + 4)[0]
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

    def net_on(self):
        """Turn the network function on, as a host driver does."""
        if self.net_in is None:
            raise ConnectionError("no CDC-ECM function in the configuration")
        self.control(0x01, 11, 1, self.ecm_data)  # SET_INTERFACE alt 1
        self.control(0x21, 0x43, 0x000E, self.ecm)  # packet filter

    def net_send(self, frame):
        for i in range(0, len(frame), self.mps):
            self._out(self.net_out, frame[i:i + self.mps])
        if len(frame) % self.mps == 0:
            self._out(self.net_out, b"")

    def net_recv(self):
        """One frame from the device, or None if it has none."""
        buf = b""
        while True:
            st, data = self._req(b"I", self.net_in, self.mps)
            if st == b"S":
                raise Stall("network IN")
            if st != b"D":
                if not buf:
                    return None
                time.sleep(0.001)
                continue
            buf += data
            if len(data) < self.mps:
                return buf

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


class Logs:
    """Console output with its logs (between the LOG marks) kept or left
    out, the marks removed."""

    def __init__(self, keep):
        self.keep, self.inside, self.carry = keep, False, b""

    def __call__(self, data):
        buf, self.carry, out = self.carry + data, b"", []
        while buf:
            found = [(buf.find(m), m) for m in (LOG_START, LOG_END)]
            k, mark = min(((i, m) for i, m in found if i >= 0), default=(-1, b""))
            if k < 0:
                e = buf.rfind(b"\x1b")
                if e >= 0 and any(m.startswith(buf[e:]) for m in (LOG_START, LOG_END)):
                    buf, self.carry = buf[:e], buf[e:]  # a mark cut in two
                k = len(buf)
            if buf[:k] and (self.keep or not self.inside):
                out.append(buf[:k])
            if mark and k < len(buf):
                self.inside = mark == LOG_START
                k += len(mark)
            buf = buf[k:]
        return b"".join(out)


def read_until(link, marker, timeout, out, shown=None):
    """Output until `marker` shows up (or `timeout`): (text, found).
    `shown`: what of each piece goes to `out` (a `Logs`)."""
    buf = b""
    end = time.time() + timeout
    while time.time() < end:
        data = link.read(min(0.2, max(0.0, end - time.time())))
        if data:
            buf += data
            out.write((shown(data) if shown else data).decode("utf-8", "replace"))
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


def run(link, cmds, timeout, out=sys.stdout, puts=(), logs=True):
    """Type each command, print its output up to the next prompt (the
    logs too if `logs`)."""
    shown = Logs(logs)
    drain(link)
    link.write(b"\r")
    if not read_until(link, PROMPT, timeout, out, shown)[1]:
        print("\n[usbterm] no nxk prompt", file=sys.stderr)
        return False
    for local, remote in puts:
        if not put(link, local, remote, timeout):
            return False
    for c in cmds:
        link.write(c.encode() + b"\r")
        if not read_until(link, PROMPT, timeout, out, shown)[1]:
            print(f"\n[usbterm] '{c}' missed the prompt in {timeout} s", file=sys.stderr)
            return False
    out.write("\n")
    return True


# Words user/shell's command() runs itself (keep in step with it).
BUILTINS = set("help echo ps mem cpu uptime progs ls cd pwd cat rm mkdir rmdir mv cp stat df "
               "sync fbecho events env export unset date sleep temp fan services poweroff reboot "
               "recv restart run".split())
# Commands the client runs itself, never typed on the Switch.
LOCAL = {"clear", "exit", "logout"}
# user/shell's banner() (keep in step with its LOGO): each letter is a `#`
# of its color (xterm 256), the others 223.
LOGO = [
    "       aaaa   bbbb",
    "   aaaa           bbbb",
    " aa      ddddddd      bb      ff",
    "aa                     bb     ff",
    "aa   dd    fff    aa   bb     fffff   ff  ff  ff  ff",
    "     dd   fffff   aa          dd  dd   dddd   dd dd",
    "bb   dd    fff    aa   cc     aa  aa    aa    aaaa",
    "bb                     cc     aa  aa   aaaa   aa aa",
    " bb      aaaaaaa      cc      bb  bb  bb  bb  bb  bb",
    "   bbbb           cccc",
    "       bbbb   cccc",
]
LOGO_INK = {"a": 208, "b": 166, "c": 130, "d": 215}
STATE = os.path.join(os.environ.get("XDG_STATE_HOME") or os.path.expanduser("~/.local/state"), "nxk")
HISTORY_MAX = 5000
LINE_MAX = 1000  # bytes the console keeps per line
# zsh's default WORDCHARS: word motions and Ctrl-W stop at other punctuation.
WORDCHARS = "*?_-.[]~=/&;!#$%^(){}<>"
# powerlevel10k "lean" and zsh-syntax-highlighting colors (256-color SGR).
DIR, OK, ERR, DUR, CLOCK, DIM = "1;38;5;39", "38;5;76", "38;5;196", "38;5;101", "38;5;66", "38;5;242"
CMD, BAD, QUOTE, PATH, MATCH = "38;5;76", "1;38;5;196", "38;5;178", "4", "1;4"
CSI_KEYS = {"A": "up", "B": "down", "C": "right", "D": "left", "H": "home", "F": "end"}
TILDE_KEYS = {"1": "home", "7": "home", "4": "end", "8": "end", "3": "delete"}
PASTE_ON, PASTE_OFF = "\x1b[200~", "\x1b[201~"


def sgr(style, text):
    return f"\x1b[{style}m{text}\x1b[0m" if text else ""


def cells(text):
    """Terminal columns taken by `text` (no escape sequences)."""
    return sum(0 if unicodedata.combining(c) else 2 if unicodedata.east_asian_width(c) in "WF" else 1
               for c in text)


def is_word(c):
    return c.isalnum() or c in WORDCHARS


def boot_programs():
    """Program names of the local boot image (build/bootfs.img), if built."""
    img = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "build", "bootfs.img")
    try:
        with open(img, "rb") as f:
            head = f.read(16)
            table = f.read(32 * struct.unpack_from("<I", head, 8)[0]) if head[:8] == b"NXKBOOT1" else b""
    except (OSError, struct.error):
        return set()
    return {table[i:i + 24].rstrip(b"\0").decode("utf-8", "replace") for i in range(0, len(table), 32)}


def quoted(word):
    """`word` with its quoted parts colored."""
    out, seg, quote = [], "", None
    for c in word:
        if quote:
            seg += c
            if c == quote:
                out.append(sgr(QUOTE, seg))
                seg, quote = "", None
        elif c in "\"'":
            out.append(seg)
            seg, quote = c, c
        else:
            seg += c
    out.append(sgr(QUOTE, seg) if quote else seg)
    return "".join(out)


def highlight(line, known):
    """zsh-syntax-highlighting's main rules: a known command green, an
    unknown one red, a path underlined, quotes and redirections yellow."""
    out, i, n, command = [], 0, len(line), True
    while i < n:
        j = i
        if line[i].isspace():
            while j < n and line[j].isspace():
                j += 1
            out.append(line[i:j])
            i = j
            continue
        quote = None
        while j < n and (quote or not line[j].isspace()):
            if quote and line[j] == quote:
                quote = None
            elif not quote and line[j] in "\"'":
                quote = line[j]
            j += 1
        word = line[i:j]
        bare = word.replace('"', "").replace("'", "")
        if word in (">", ">>"):
            out.append(sgr(QUOTE, word))
        elif command:
            out.append(sgr(CMD if bare in known else PATH if "/" in bare else BAD, word))
            command = bare == "run"
        else:
            out.append(quoted(word))
        i = j
    return "".join(out)


def keys(text):
    """Keys of typed text: characters (control ones too) as themselves,
    escape sequences by name ("up", "ctrl-left", "alt-b", "esc")."""
    out, i, n = [], 0, len(text)
    while i < n:
        c = text[i]
        if c != "\x1b":
            out.append(c)
            i += 1
        elif i + 1 == n:
            out.append("esc")
            i += 1
        elif text[i + 1] in "[O":
            j = i + 2
            while j < n and not "\x40" <= text[j] <= "\x7e":
                j += 1
            if j == n:
                break  # truncated sequence
            params, final = text[i + 2:j], text[j]
            name = TILDE_KEYS.get(params.split(";")[0]) if final == "~" else CSI_KEYS.get(final)
            mod = params.split(";")[1] if ";" in params else ""
            if name and mod in ("3", "5", "9"):
                name = ("ctrl-" if mod == "5" else "alt-") + name
            out.append(name or "unknown")
            i = j + 1
        else:
            out.append("alt-backspace" if text[i + 1] in "\x7f\x08" else "alt-" + text[i + 1])
            i += 2
    return out


def banner():
    """The shell's header: logo, tagline, greeting."""
    rows = [re.sub(r"(\S)\1*", lambda m: sgr(f"38;5;{LOGO_INK.get(m.group(1), 223)}", "#" * len(m.group(0))), r)
            for r in LOGO]
    rows[-1] += "            AArch64 microkernel for Tegra X1"
    return "\r\n" + "\r\n".join(rows) + "\r\n\r\nnxk - shell. Type 'help' for the command list.\r\n"


def duration(t):
    if t < 1:
        return f"{t * 1000:.0f}ms"
    return f"{t:.1f}s" if t < 60 else f"{int(t // 60)}m{int(t % 60):02d}s"


class History:
    """Command lines, oldest first, kept in a file (zsh's HIST_IGNORE_DUPS
    and HIST_IGNORE_SPACE)."""

    def __init__(self, path):
        self.path = path
        try:
            with open(path, encoding="utf-8", errors="replace") as f:
                lines = [l.rstrip("\n") for l in f]
        except OSError:
            lines = []
        self.lines = lines[-HISTORY_MAX:]
        if len(lines) > 2 * HISTORY_MAX:
            self.save("w", self.lines)

    def add(self, line):
        if not line.strip() or line[0] == " " or self.lines[-1:] == [line]:
            return
        self.lines.append(line)
        self.save("a", [line])

    def save(self, mode, lines):
        try:
            os.makedirs(os.path.dirname(self.path), exist_ok=True)
            with open(self.path, mode, encoding="utf-8") as f:
                f.writelines(l + "\n" for l in lines)
        except OSError:
            pass


class Zsh:
    """zsh-style terminal: the line is edited here and typed whole on
    Enter. The shell's OSC marks (7: directory, 133: prompt, command,
    status) drive the prompt drawn here; its `nxk> ` and echo are hidden.
    While a command runs, keys go through as typed (programs read lines)."""

    def __init__(self, link, logs=False):
        self.link, self.logs, self.in_log = link, logs, False
        self.history = History(os.path.join(STATE, "history"))
        self.learned_path = os.path.join(STATE, "commands")
        try:
            self.learned = set(open(self.learned_path, encoding="utf-8").read().split())
        except OSError:
            self.learned = set()
        self.known = BUILTINS | LOCAL | boot_programs() | self.learned
        # Line editor: characters, cursor, cursor row below the prompt's
        # second line, kill buffer, last key, Ctrl-R state, pasted lines.
        self.buf, self.cur, self.row = [], 0, 0
        self.killed, self.last, self.search, self.paste, self.queue = "", None, None, None, []
        self.hpos, self.saved, self.prefix = 0, "", ""
        # What the shell reported, and where the screen stands.
        self.cwd, self.status, self.took = "/", None, None
        self.editing = self.at_prompt = self.drawn = self.dirty = self.in_prompt = False
        self.bol = True
        self.carry = b""
        # Echo of the line sent, expected until `deadline`; output held
        # while editing, until its line ends.
        self.expect, self.etail, self.ebuf, self.deadline = None, True, b"", 0.0
        self.pending, self.pending_t = b"", 0.0
        self.sent_t, self.word, self.cmd_out = None, None, b""
        # Typed while a command ran: the console's open line, whole lines.
        self.typed, self.typed_lines = [], []
        self.t0 = self.last_rx = time.time()
        self.started = self.resized = self.quit = False
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.obuf = []

    # -- screen ------------------------------------------------------------

    def out(self, data):
        self.obuf.append(data.encode() if isinstance(data, str) else data)

    def flush(self):
        data = b"".join(self.obuf)
        self.obuf = []
        while data:
            data = data[os.write(sys.stdout.fileno(), data):]

    @staticmethod
    def cols():
        try:
            return max(os.get_terminal_size(sys.stdout.fileno()).columns, 20)
        except OSError:
            return 80

    @staticmethod
    def up(n):
        return f"\x1b[{n}A" if n > 0 else ""

    def newline(self):
        if not self.bol:
            self.out("\r\n")
            self.bol = True

    def mark(self):
        ok = self.status is None or self.status[0] == 0
        return sgr(OK if ok else ERR, "❯") + " "

    def info(self):
        """First prompt line: directory left; status, duration, time right."""
        cols = self.cols()
        left = sgr(DIR, self.cwd)
        width = cells(self.cwd)
        if self.queue:
            more = f" ({len(self.queue)} pasted line{'s' if len(self.queue) > 1 else ''} next)"
            left += sgr(DIM, more)
            width += cells(more)
        right = []
        if self.status is not None:
            code, name = self.status
            right.append((OK, "✔") if code == 0 else (ERR, f"✘ {code}" + (f" {name}" if name else "")))
        if self.took is not None:
            right.append((DUR, duration(self.took)))
        right.append((CLOCK, " " + time.strftime("%H:%M:%S")))
        while right:
            text = " ".join(t for _, t in right)
            if width + 1 + cells(text) <= cols - 1:
                pad = " " * (cols - 1 - width - cells(text))
                return left + pad + " ".join(sgr(s, t) for s, t in right)
            right.pop()
        return left

    def draw(self):
        """Redraw the edited line (from the prompt's second line down)."""
        cols = self.cols()
        text = "".join(self.buf)
        s = self.search
        if s is not None and s["query"] and s["idx"] < len(self.history.lines):
            p, q = s["pos"], len(s["query"])
            body = text[:p] + sgr(MATCH, text[p:p + q]) + text[p + q:]
        else:
            body = highlight(text, self.known)
        end, at = 2 + cells(text), 2 + cells(text[:self.cur])
        out = [self.up(self.row), "\r\x1b[J", self.mark(), body]
        end_row = end // cols
        if end % cols == 0:
            out.append("\r\n")  # leave the pending wrap: rows stay countable
        if s is not None:
            if end % cols:
                out.append("\r\n")
                end_row += 1
            label = ("failing " if s["failing"] else "") + "bck-i-search: " + s["query"] + "_"
            out.append(label[-(cols - 1):])
        self.row = at // cols
        out.append(self.up(end_row - self.row) + "\r" + (f"\x1b[{at % cols}C" if at % cols else ""))
        self.out("".join(out))
        self.bol = False

    def draw_full(self, static=False):
        """Whole prompt; `static`: its second line only up to the mark."""
        self.out("\x1b[0m" + self.info() + "\r\n")
        self.row = 0
        if static:
            self.out(self.mark())
            self.bol = False
        else:
            self.draw()

    def erase(self):
        """Remove the prompt being edited (output goes in its place)."""
        self.out(self.up(self.row + 1) + "\r\x1b[J")
        self.row = 0
        self.bol = True

    # -- output of the Switch -----------------------------------------------

    def on_output(self, data):
        self.last_rx = time.time()
        if self.expect is not None:
            self.ebuf += data
            data = self.unecho(False)
            if data is None:
                return
        self.parse(data)

    def unecho(self, force):
        """Held output less the echo of the line sent; None while waiting."""
        buf, echo = self.ebuf, self.expect
        for tail in (b"\r\n", b"\n") if self.etail else (b"",):
            k = buf.find(echo + tail)
            if k >= 0:
                self.expect, self.ebuf = None, b""
                return buf[:k] + buf[k + len(echo) + len(tail):]
        if force or b"\x1b]133;" in buf:  # echo lost: show all
            self.expect, self.ebuf = None, b""
            return buf
        return None

    def parse(self, data):
        """Split output into text and OSC sequences (ESC ] ... BEL)."""
        buf, self.carry = self.carry + data, b""
        start = 0
        while True:
            k = buf.find(b"\x1b]", start)
            if k < 0:
                if buf.endswith(b"\x1b"):
                    self.text(buf[start:-1])
                    self.carry = b"\x1b"
                else:
                    self.text(buf[start:])
                return
            self.text(buf[start:k])
            e = buf.find(b"\x07", k)
            if e < 0:
                if len(buf) - k < 512:
                    self.carry = buf[k:]
                else:
                    self.text(buf[k:])
                return
            self.osc(buf[k + 2:e].decode("utf-8", "replace"))
            start = e + 1

    def osc(self, body):
        if f"\x1b]{body}\x07".encode() in (LOG_START, LOG_END):
            self.in_log = body.endswith("log")
        elif self.in_log:
            pass
        elif body.startswith("7;"):
            m = re.match(r"7;file://[^/]*(/.*)", body)
            self.cwd = m.group(1) if m else self.cwd
        elif body == "133;A":
            self.in_prompt = True
        elif body.startswith("133;B"):
            self.in_prompt = False
            if not self.editing:
                self.at_prompt, self.drawn, self.dirty = True, False, False
        elif body.startswith("133;C"):
            if self.editing:  # someone typed on UART-A
                self.flush_pending(True)
                self.erase()
                self.editing = False
            elif self.at_prompt and self.typed_lines:
                # A line typed during the last command, run by the shell now.
                line = self.typed_lines.pop(0)
                if not self.drawn:
                    self.newline()
                    self.draw_full(static=True)
                self.out(highlight(line, self.known) + "\r\n")
                self.bol = True
                self.started_cmd(line)
            self.at_prompt = False
        elif body.startswith("133;D"):
            parts = body.split(";")
            code = int(parts[2]) if len(parts) > 2 and parts[2].lstrip("-").isdigit() else 0
            self.status = (code, next((p[4:] for p in parts[3:] if p.startswith("err=")), ""))
            self.took = time.time() - self.sent_t if self.sent_t else None
            self.sent_t = None
            self.learn()
        else:
            self.text(f"\x1b]{body}\x07".encode())

    def text(self, data):
        if not data or self.in_prompt or self.in_log and not self.logs:
            return
        if self.in_log:
            data = f"\x1b[{DIM}m".encode() + data + b"\x1b[0m"
        elif self.word and len(self.cmd_out) < 8192:
            self.cmd_out += data
        if self.editing:
            if not self.pending:
                self.pending_t = time.time()
            self.pending += data
            self.flush_pending(False)
            return
        if self.at_prompt:
            # Output after a prompt not edited yet: an echo in the history
            # replayed at start, or a message; drawn after that prompt.
            if not self.drawn:
                self.newline()
                self.draw_full(static=True)
                self.drawn = True
            self.dirty = True
        self.out(data)
        self.bol = data.endswith(b"\n")

    def flush_pending(self, force):
        """Output that came while editing, above the prompt: whole lines,
        or everything once `force`d (a line left open for 100 ms)."""
        if not self.pending:
            return
        if force:
            chunk, self.pending = self.pending, b""
        else:
            k = self.pending.rfind(b"\n")
            if k < 0:
                return
            chunk, self.pending = self.pending[:k + 1], self.pending[k + 1:]
            self.pending_t = time.time()
        self.erase()
        self.out(chunk)
        self.bol = chunk.endswith(b"\n")
        self.newline()
        self.draw_full()

    def learn(self):
        """Commands learned from their outcome, for the highlighting."""
        word, self.word = self.word, None
        out = self.cmd_out.decode("utf-8", "replace")
        new = set(self.learned)
        if word == "progs" and self.status[0] == 0:
            new |= {w for w in out.split() if re.fullmatch(r"[\w.+-]+", w)}
        elif word and word not in BUILTINS and "/" not in word:
            if "unknown command or program" in out:
                new.discard(word)
                self.known.discard(word)
            else:
                new.add(word)
        if new != self.learned:
            self.learned = new
            self.known |= new
            try:
                os.makedirs(STATE, exist_ok=True)
                with open(self.learned_path, "w", encoding="utf-8") as f:
                    f.write("\n".join(sorted(new)) + "\n")
            except OSError:
                pass

    def tick(self):
        now = time.time()
        if self.expect is not None and now > self.deadline:
            self.parse(self.unecho(True))
        if self.pending and now - self.pending_t > 0.1:
            self.flush_pending(True)
        if not self.started and (now - self.t0 > 0.3 and now - self.last_rx > 0.15 or now - self.t0 > 3):
            # History replayed; no prompt at its end (a command runs, or
            # nothing new since the last terminal): ask for one.
            self.started = True
            if not self.at_prompt and not self.editing:
                self.send("")
        if self.at_prompt and not self.editing and now - self.last_rx > 0.05:
            self.activate()
        if self.resized and self.editing:
            self.draw()
        self.resized = False

    # -- line editor ---------------------------------------------------------

    def activate(self):
        self.editing = True
        self.typed_lines = []
        if self.typed:
            # Typed during the last command, still open in the console:
            # erased there, edited here (zsh's type-ahead).
            n = len(self.typed)
            self.buf, self.typed = self.typed + self.buf, []
            self.cur = len(self.buf)
            self.link.write(b"\x7f" * n)
            self.expect, self.etail, self.ebuf, self.deadline = b"\x08 \x08" * n, False, b"", time.time() + 1.0
        if self.queue and not self.buf:
            self.set_line(self.queue.pop(0))
        if self.drawn and not self.dirty:
            self.row = 0
            self.draw()
        else:
            self.newline()
            self.draw_full()

    def send(self, line):
        data = line.encode()
        self.link.write(data + b"\r")
        self.expect, self.etail, self.ebuf, self.deadline = data, True, b"", time.time() + 1.0

    def started_cmd(self, line):
        """A command sent: its word (learned from its outcome), start time."""
        words = line.split()
        if words and words[0] == "run":
            words = words[1:]
        self.word = words[0].replace('"', "").replace("'", "") if words else None
        self.cmd_out = b""
        self.sent_t = time.time() if words else None
        if not words:
            self.took = None

    def submit(self):
        line = "".join(self.buf)
        self.search = None
        self.cur = len(self.buf)
        self.draw()
        self.out("\r\n")
        self.bol, self.row = True, 0
        self.editing = self.at_prompt = False
        self.buf, self.cur = [], 0
        if line.strip() in ("exit", "logout"):
            self.quit = True
            return
        self.history.add(line)
        self.hpos = len(self.history.lines)
        if line.strip() == "clear":
            # The PC's screen and scrollback only: the shell still waits.
            self.out("\x1b[H\x1b[2J\x1b[3J" + banner())
            self.status, self.took = (0, ""), None
            self.at_prompt, self.drawn = True, False
            self.activate()
            return
        self.started_cmd(line)
        self.send(line)

    def set_line(self, text):
        self.buf, self.cur = list(text), len(text)

    def insert(self, text):
        text = "".join(c for c in text if c.isprintable())
        room = LINE_MAX - len("".join(self.buf).encode())
        while text and len(text.encode()) > room:
            text = text[:-1]
        self.buf[self.cur:self.cur] = text
        self.cur += len(text)

    def kill(self, a, b):
        if a < b:
            self.killed = "".join(self.buf[a:b])
            del self.buf[a:b]
            self.cur = a

    def word_left(self):
        i = self.cur
        while i > 0 and not is_word(self.buf[i - 1]):
            i -= 1
        while i > 0 and is_word(self.buf[i - 1]):
            i -= 1
        return i

    def word_right(self):
        i, n = self.cur, len(self.buf)
        while i < n and not is_word(self.buf[i]):
            i += 1
        while i < n and is_word(self.buf[i]):
            i += 1
        return i

    def history_move(self, step, prefix):
        """Up/Down: older/newer lines starting like the text before the
        cursor (up-line-or-beginning-search); Ctrl-P/N: any line."""
        h = self.history.lines
        if self.last not in ("up", "down", "\x10", "\x0e"):
            self.hpos, self.saved = len(h), "".join(self.buf)
            self.prefix = "".join(self.buf[:self.cur]) if prefix else ""
        now = "".join(self.buf)
        i = self.hpos + step
        while 0 <= i < len(h) and (not h[i].startswith(self.prefix) or h[i] == now):
            i += step
        if i < 0:
            return
        self.hpos = min(i, len(h))
        self.set_line(h[i] if i < len(h) else self.saved)

    def find(self, start, step):
        s, h = self.search, self.history.lines
        i = start
        while s["query"] and 0 <= i < len(h):
            k = h[i].find(s["query"])
            if k >= 0:
                s.update(idx=i, pos=k, failing=False)
                self.set_line(h[i])
                self.cur = k
                return
            i += step
        s["failing"] = bool(s["query"])

    def isearch(self, t):
        """Ctrl-R incremental search; False: the key leaves the search and
        then acts on the line found."""
        s, h = self.search, self.history.lines
        if t in ("\x12", "\x13"):
            self.find(s["idx"] + (-1 if t == "\x12" else 1), -1 if t == "\x12" else 1)
        elif t in ("\x7f", "\x08"):
            s["query"] = s["query"][:-1]
            s.update(idx=len(h), failing=False)
            self.set_line(s["orig"])
            self.find(len(h) - 1, -1)
        elif t in ("\x07", "esc"):
            self.set_line(s["orig"])
            self.search = None
        elif isinstance(t, str) and len(t) == 1 and t.isprintable():
            s["query"] += t
            self.find(min(s["idx"], len(h) - 1), -1)
        else:
            self.search = None
            return False
        return True

    def edit(self, t):
        if self.search is not None and self.isearch(t):
            self.last = t
            self.draw()
            return
        b = self.buf
        if isinstance(t, tuple):  # bracketed paste: one line, others next
            lines = [l for l in t[1].replace("\r\n", "\n").replace("\r", "\n").replace("\t", " ").split("\n")]
            self.insert(lines[0])
            self.queue += [l for l in lines[1:] if l.strip()]
            if len(lines) > 1:
                self.erase()
                self.draw_full()
                self.last = None
                return
        elif t in ("\r", "\n"):
            self.last = None
            return self.submit()
        elif t == "\x04" and not b:
            self.quit = True
            return
        elif t in ("\x04", "delete"):
            del b[self.cur:self.cur + 1]
        elif t in ("\x7f", "\x08"):
            if self.cur:
                self.cur -= 1
                del b[self.cur]
        elif t in ("left", "\x02"):
            self.cur = max(0, self.cur - 1)
        elif t in ("right", "\x06"):
            self.cur = min(len(b), self.cur + 1)
        elif t in ("home", "\x01"):
            self.cur = 0
        elif t in ("end", "\x05"):
            self.cur = len(b)
        elif t in ("alt-b", "ctrl-left", "alt-left"):
            self.cur = self.word_left()
        elif t in ("alt-f", "ctrl-right", "alt-right"):
            self.cur = self.word_right()
        elif t in ("\x17", "alt-backspace"):
            self.kill(self.word_left(), self.cur)
        elif t == "alt-d":
            self.kill(self.cur, self.word_right())
        elif t == "\x0b":
            self.kill(self.cur, len(b))
        elif t == "\x15":
            self.kill(0, len(b))
        elif t == "\x19":
            self.insert(self.killed)
        elif t == "\x14" and 0 < self.cur:  # Ctrl-T: swap the last two
            i = min(self.cur, len(b) - 1)
            if i > 0:
                b[i - 1], b[i] = b[i], b[i - 1]
                self.cur = i + 1
        elif t in ("up", "\x10", "down", "\x0e"):
            self.history_move(-1 if t in ("up", "\x10") else 1, t in ("up", "down"))
        elif t == "\x12":
            h = self.history.lines
            self.search = dict(query="", idx=len(h), pos=0, failing=False, orig="".join(b))
        elif t == "\x0c":  # Ctrl-L
            self.out("\x1b[H\x1b[2J")
            self.draw_full()
            self.last = t
            return
        elif t == "\x03":  # Ctrl-C: leave the line, new prompt
            self.search, self.cur, self.queue = None, len(b), []
            self.draw()
            self.out("\r\n")
            self.buf, self.cur = [], 0
            self.draw_full()
            self.last = t
            return
        elif len(t) == 1 and t.isprintable():
            self.insert(t)
        self.last = t
        self.draw()

    # -- keyboard ------------------------------------------------------------

    def on_keys(self, data):
        if QUIT in data:
            data = data[:data.index(QUIT)]
            self.quit = True
        text = self.decoder.decode(data)
        while text:
            if self.paste is not None:
                k = text.find(PASTE_OFF)
                self.paste += text if k < 0 else text[:k]
                if k < 0:
                    return
                text, pasted, self.paste = text[k + len(PASTE_OFF):], self.paste, None
                self.key(("paste", pasted))
                continue
            k = text.find(PASTE_ON)
            head, text = (text, "") if k < 0 else (text[:k], text[k + len(PASTE_ON):])
            for t in keys(head):
                self.key(t)
            if k >= 0:
                self.paste = ""

    def key(self, t):
        if self.quit:
            return
        if self.editing:
            self.edit(t)
        elif self.at_prompt:
            self.activate()
            self.edit(t)
        elif isinstance(t, tuple) or len(t) == 1:
            # A command runs: keys typed as they come (programs read lines).
            text = t[1].replace("\r\n", "\r").replace("\n", "\r") if isinstance(t, tuple) else t
            for c in text:
                if c == "\r":
                    self.typed_lines.append("".join(self.typed))
                    self.typed = []
                elif c in "\x7f\x08":
                    self.typed = self.typed[:-1]
                elif c.isprintable():
                    self.typed.append(c)
                self.link.write(c.encode())

    def run(self):
        print(f"[usbterm] {self.link.name}; Ctrl-D or Ctrl-] quits\r", file=sys.stderr)
        stdin = sys.stdin.fileno()
        saved = termios.tcgetattr(stdin)
        tty.setraw(stdin)
        old = signal.signal(signal.SIGWINCH, lambda *_: setattr(self, "resized", True))
        serial = isinstance(self.link, Serial)
        fds = [stdin] + ([self.link.fd] if serial else [])
        self.out("\x1b[?2004h")  # bracketed paste
        self.flush()
        try:
            while not self.quit:
                busy = (self.expect is not None or self.pending or not self.started
                        or self.at_prompt and not self.editing)
                ready = select.select(fds, [], [], (0.02 if busy else 0.25) if serial else 0.0)[0]
                if stdin in ready:
                    self.on_keys(os.read(stdin, 4096))
                if not serial or self.link.fd in ready:
                    data = self.link.read(0.0 if serial else 0.02)
                    if data:
                        self.on_output(data)
                self.tick()
                self.flush()
        finally:
            self.obuf = []
            self.out("\x1b[?2004l\x1b[0m")
            self.flush()
            signal.signal(signal.SIGWINCH, old)
            termios.tcsetattr(stdin, termios.TCSADRAIN, saved)
            print(("" if self.bol else "\r\n") + "[usbterm] bye", file=sys.stderr)


def interactive(link, raw=False, logs=False):
    """zsh-style terminal, or the plain one (`raw`, or not a terminal)."""
    if raw or not (os.isatty(sys.stdin.fileno()) and os.isatty(sys.stdout.fileno())):
        passthrough(link)
    else:
        Zsh(link, logs).run()


def passthrough(link):
    """Keyboard to the Switch, its output to the screen, until Ctrl-]."""
    print(f"[usbterm] {link.name}; Ctrl-] quits\r", file=sys.stderr)
    stdin = sys.stdin.fileno()
    saved = termios.tcgetattr(stdin) if os.isatty(stdin) else None
    if saved:
        tty.setraw(stdin)
    fds = [stdin]
    try:
        link.write(b"\r")
        while True:
            if select.select(fds, [], [], 0.0)[0]:
                typed = os.read(stdin, 1024)
                if not typed:
                    fds = []  # end of piped input
                if QUIT in typed:
                    link.write(typed[:typed.index(QUIT)])
                    break
                link.write(typed)
            data = link.read(0.02)
            if data:
                os.write(sys.stdout.fileno(), data)
    finally:
        if saved:
            termios.tcsetattr(stdin, termios.TCSADRAIN, saved)
        print("\r\n[usbterm] bye", file=sys.stderr)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--dev", help="serial device (default: /dev/ttyACM* with ID 1209:0001)")
    ap.add_argument("--emu", metavar="SOCKET", help="tegra_qemu 'usbotg' chardev socket")
    ap.add_argument("-c", "--cmd", action="append", default=[], help="command to type (repeatable)")
    ap.add_argument("--timeout", type=float, default=30, help="per-command timeout (s)")
    ap.add_argument("--raw", action="store_true",
                    help="plain terminal: keys typed as they come, the shell's own prompt and echo")
    ap.add_argument("--logs", action="store_true",
                    help="show the logs (services, kernel) too; --reload implies it")
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
            sys.exit(0 if run(link, cmds, a.timeout, puts=puts, logs=a.logs or bool(a.reload)) else 1)
        interactive(link, a.raw, a.logs)
    except (ConnectionError, TimeoutError, Stall) as e:
        sys.exit(f"\n[usbterm] {e}")
    finally:
        link.close()


if __name__ == "__main__":
    main()
