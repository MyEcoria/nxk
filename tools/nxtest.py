#!/usr/bin/env python3
"""Drive nxk in tegra_qemu: boot, shell commands, capture.

    nxtest.py                          # boot and print until the prompt
    nxtest.py -c "ps" -c "run hello"   # chain commands
    nxtest.py -c "ipcbench" --timeout 120
    nxtest.py --usb-cmd "ps"             # type over the USB-C console (usbterm.py host)
    nxtest.py --dbg "tap 640 670 shot net.png"   # debug channel (nxdbg.py) over the USB-C console
    nxtest.py --usb-cmd ps --uart-log uart.txt   # UART-A kept whole, logs read while USB runs too
    nxtest.py --tap 640,670:home.ppm     # touch the screen at (x, y), then capture it
    nxtest.py --usb-net share --usb-cmd "nettest ping 10.42.0.1"   # the PC shares its connection
    nxtest.py --bt -c "bt pair 10" -c "bt list"   # emulated BCM4356 and Pro Controller on UART-D

UART-A console is bound to a socket: the script reads everything from the
first kernel byte and types commands like a user. Exit code 1 if a command
missed the prompt in time.
"""
import argparse, bisect, os, re, shutil, socket, struct, subprocess, sys, tempfile, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.dont_write_bytecode = True
import usbterm  # noqa: E402  (console output filter, USB-C host)
import nxdbg  # noqa: E402  (debug channel: screen capture, touch, gamepad)
import btfake  # noqa: E402  (emulated Bluetooth controller on UART-D)
# UART output shown without the console's log marks (logs kept).
SHOWN = usbterm.Logs(True)
ROOT = os.path.dirname(HERE)
QEMU = os.environ.get("QEMU", os.path.expanduser("~/.cache/tegra_qemu/build/qemu-system-aarch64"))
PROMPT = b"nxk> "


# Raw UART-A bytes, also those drained unshown (--uart-log).
RAW = None


def read_until(sock, marker, timeout, out):
    """Read until `marker` (or timeout). Returns (text, found)."""
    buf = b""
    end = time.time() + timeout
    while time.time() < end:
        sock.settimeout(max(0.05, end - time.time()))
        try:
            chunk = sock.recv(65536)
        except socket.timeout:
            break
        if not chunk:
            break
        buf += chunk
        if RAW:
            RAW.write(chunk)
        out.write(SHOWN(chunk).decode("utf-8", "replace"))
        out.flush()
        if marker in buf:
            return buf, True
    return buf, False


def drain(sock):
    """UART-A read to the end (into --uart-log only)."""
    for chunk in iter(lambda: sock.recv(65536), b""):
        if RAW:
            RAW.write(chunk)


def symbolizer(elf):
    """Address -> 'symbol+offset' from the kernel symbol table."""
    try:
        out = subprocess.run(["nm", "-C", "--defined-only", elf], capture_output=True, text=True).stdout
    except OSError:
        return lambda a: hex(a)
    syms = []
    for line in out.splitlines():
        p = line.split(maxsplit=2)
        if len(p) == 3 and p[1] in "tTwW":
            syms.append((int(p[0], 16), p[2]))
    syms.sort()
    keys = [a for a, _ in syms]

    def name(a):
        i = bisect.bisect_right(keys, a) - 1
        return f"{syms[i][1]}+{a - syms[i][0]:#x}" if i >= 0 else hex(a)
    return name


def dump_cpus(mon, elf):
    """Per-core state (PC, level) via the QEMU monitor."""
    try:
        m = socket.socket(socket.AF_UNIX)
        m.connect(mon)
        m.settimeout(2)
        time.sleep(0.2)
        try:
            m.recv(65536)
        except socket.timeout:
            pass
        m.sendall(b"info registers -a\n")
        data = b""
        end = time.time() + 3
        while time.time() < end:
            try:
                c = m.recv(65536)
            except socket.timeout:
                break
            if not c:
                break
            data += c
        m.close()
    except OSError as e:
        print(f"[nxtest] monitor unreachable: {e}", file=sys.stderr)
        return
    name = symbolizer(elf)
    text = data.decode("utf-8", "replace")
    for cpu, pc, el in re.findall(r"CPU#(\d+).*?PC=([0-9a-f]+).*?EL(\d)", text, re.S):
        a = int(pc, 16)
        where = name(a) if a >= 0x80000000 and a < 0x100000000 else "userland"
        print(f"[nxtest] cpu{cpu} EL{el} PC={a:#x} {where}", file=sys.stderr)


def qmp_tap(path, x, y):
    """Press and release the touchscreen at landscape pixel (x, y): QMP
    absolute pointer events, which tegra_qemu's touch panel turns into
    STMFTS contact events."""
    s = socket.socket(socket.AF_UNIX)
    s.connect(path)
    s.settimeout(5)

    def cmd(c):
        s.sendall((__import__("json").dumps(c) + "\n").encode())
        buf = b""
        while b'"return"' not in buf and b'"error"' not in buf:
            buf += s.recv(65536)
    s.recv(65536)  # greeting
    cmd({"execute": "qmp_capabilities"})
    for down in (True, False):
        cmd({"execute": "input-send-event", "arguments": {"events": [
            {"type": "abs", "data": {"axis": "x", "value": x * 0x7FFF // 1280}},
            {"type": "abs", "data": {"axis": "y", "value": y * 0x7FFF // 720}},
            {"type": "btn", "data": {"button": "left", "down": down}}]}})
        time.sleep(0.15)
    s.close()


def checksum(b):
    if len(b) % 2:
        b += b"\0"
    s = sum(struct.unpack(f"!{len(b) // 2}H", b))
    s = (s >> 16) + (s & 0xFFFF)
    return ~(s + (s >> 16)) & 0xFFFF


def pc_network(link, stop, log, share):
    """The PC with the USB-C network function on. `share`: it shares its
    connection (NetworkManager's "shared" mode): DHCP server at 10.42.0.1
    leasing 10.42.0.23, ARP and ICMP echo answers for 10.42.0.1; else it
    answers nothing."""
    me, mac = bytes([10, 42, 0, 1]), bytes.fromhex("024E584B0002")
    lease = bytes([10, 42, 0, 23])
    link.net_on()
    log("[nxtest] USB-C network on, " + ("sharing 10.42.0.0/24" if share else "not shared"))

    def ipv4(dst_mac, src, dst, proto, payload):
        h = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(payload), 0, 0, 64, proto, 0, src, dst)
        h = h[:10] + struct.pack("!H", checksum(h)) + h[12:]
        link.net_send(dst_mac + mac + b"\x08\x00" + h + payload)

    while not stop.is_set():
        f = link.net_recv()
        if not f or not share:
            time.sleep(0.005)
            continue
        src_mac, kind = f[6:12], f[12:14]
        if kind == b"\x08\x06" and f[20:22] == b"\0\x01" and f[38:42] == me:  # ARP request
            arp = f[14:20] + b"\0\x02" + mac + me + f[22:28] + f[28:32]
            link.net_send(src_mac + mac + b"\x08\x06" + arp)
        elif kind == b"\x08\x00":
            ip, proto = f[14:34], f[23]
            body = f[34:14 + struct.unpack("!H", f[16:18])[0]]
            if proto == 17 and body[2:4] == b"\0\x43":  # DHCP to the server port
                bootp = body[8:]
                opts, msg = bootp[240:], None
                i = 0
                while i < len(opts) and opts[i] != 255:
                    if opts[i] == 53:
                        msg = opts[i + 2]
                    i += 2 + opts[i + 1] if opts[i] else 1
                reply = {1: 2, 3: 5}.get(msg)
                if reply is None:
                    continue
                out = bytearray(236)
                out[0:4] = b"\x02\x01\x06\x00"
                out[4:8] = bootp[4:8]  # transaction
                out[16:20], out[20:24] = lease, me
                out[28:44] = bootp[28:44]  # client hardware address
                out += b"\x63\x82\x53\x63" + bytes([53, 1, reply, 54, 4]) + me \
                    + bytes([51, 4, 0, 0, 0x0E, 0x10, 1, 4, 255, 255, 255, 0, 3, 4]) + me \
                    + bytes([6, 4]) + me + b"\xff"
                udp = struct.pack("!HHHH", 67, 68, 8 + len(out), 0) + bytes(out)
                ipv4(b"\xff" * 6, me, b"\xff" * 4, 17, udp)
                log(f"[nxtest] DHCP {'offer' if reply == 2 else 'ack'}: 10.42.0.23")
            elif proto == 1 and body[0] == 8 and ip[16:20] == me:  # ping
                echo = b"\0\0\0\0" + body[4:]
                echo = echo[:2] + struct.pack("!H", checksum(echo)) + echo[4:]
                ipv4(src_mac, me, ip[12:16], 1, echo)


def screendump(mon, path):
    m = socket.socket(socket.AF_UNIX)
    m.connect(mon)
    m.sendall(f"screendump {os.path.abspath(path)}\n".encode())
    time.sleep(1.5)
    m.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-c", "--cmd", action="append", default=[])
    ap.add_argument("--timeout", type=float, default=60, help="per-step timeout (s)")
    ap.add_argument("--kernel", default=os.path.join(ROOT, "build", "nxk.elf"))
    ap.add_argument("--smp", default=None, help="(reserved)")
    ap.add_argument("--machine", default="tegrax1", help="tegrax1 (Erista) or tegrax1plus (Mariko)")
    ap.add_argument("--monitor", default=None,
                    help="EL3 monitor (ELF, e.g. l4tboot) started before nxk: PSCI path")
    ap.add_argument("--dtb", default=None, help="device tree loaded at 0x8D000000 (monitor x0)")
    ap.add_argument("--sd", default=None, help="SD card image (SDMMC1 slot)")
    ap.add_argument("--screendump", default=None, help="screen capture (PPM) after the commands")
    ap.add_argument("--usb-kbd", action="store_true", help="USB keyboard on xHCI")
    ap.add_argument("--net", default=None, nargs="?", const="",
                    help="USB net card (QEMU usb-net); value: forwards, e.g. tcp::8080-:80")
    ap.add_argument("--keys", default=None, help="keys sent to the USB keyboard (sendkey) after the 1st command")
    ap.add_argument("--usb-cmd", action="append", default=[],
                    help="command typed on the USB-C console (usbcon), the script acting as the PC")
    ap.add_argument("--dbg", action="append", default=[], metavar="COMMANDS",
                    help="nxdbg.py commands over the USB-C console, e.g. 'shot a.png tap 640 670 button A'")
    ap.add_argument("--usb-put", action="append", default=[], metavar="LOCAL:REMOTE",
                    help="file copied to the SD card over the USB-C console, before --usb-cmd")
    ap.add_argument("--usb-net", choices=("share", "quiet"), default=None,
                    help="with --usb-cmd: the PC turns the USB-C network on and shares its "
                         "connection (10.42.0.1), or answers nothing")
    ap.add_argument("--tap", action="append", default=[], metavar="X,Y[:OUT.ppm]",
                    help="touch the screen at landscape (X, Y) after the commands, then capture it")
    ap.add_argument("--bt", action="store_true",
                    help="emulated BCM4356 on UART-D with a Pro Controller in pairing mode (btfake.py)")
    ap.add_argument("--bt-hw-errors", type=int, default=0, metavar="N",
                    help="with --bt: the first N HCI resets answer with a Hardware Error")
    ap.add_argument("--uart-log", default=None, metavar="FILE",
                    help="every UART-A byte to FILE, also while the USB-C console runs")
    a = ap.parse_args()
    global RAW
    if a.uart_log:
        RAW = open(a.uart_log, "wb", buffering=0)

    tmp = tempfile.mkdtemp(prefix="nxk-")
    uart = os.path.join(tmp, "uart.sock")
    usb = os.path.join(tmp, "usb.sock")
    mon = os.path.join(tmp, "mon.sock")
    qmp = os.path.join(tmp, "qmp.sock")
    bt = os.path.join(tmp, "bt.sock")
    cmd = [
        QEMU, "-machine", a.machine, "-m", "4G", "-display", "none",
        "-device", f"loader,file={a.kernel}",
    ] + ([
        "-device", f"loader,file={a.dtb},addr=0x8D000000,force-raw=on",
    ] if a.dtb else []) + ([
        "-device", f"loader,file={a.monitor}",
        "-global", "driver=tegra.evp,property=cpu-reset-vector,value=0xFFE00000",
    ] if a.monitor else [
        "-global", "driver=tegra.evp,property=cpu-reset-vector,value=0x80000000",
    ]) + [
        "-chardev", f"socket,id=uarta,path={uart},server=on,wait=on",
        "-serial", "chardev:uarta", "-serial", "null", "-serial", "null",
        "-serial", "chardev:uartd" if a.bt else "null",
        "-monitor", f"unix:{mon},server,nowait",
        "-qmp", f"unix:{qmp},server,nowait",
    ] + (["-chardev", f"socket,id=uartd,path={bt},server=on,wait=off"] if a.bt else []) \
      + (["-drive", f"if=sd,index=0,format=raw,file={a.sd}"] if a.sd else []) \
      + (["-global", "driver=tegra.xusb,property=slots,value=8"] if a.usb_kbd or a.net is not None else []) \
      + (["-device", "usb-kbd,bus=usb-bus.2"] if a.usb_kbd else []) \
      + (["-chardev", f"socket,id=usbotg,path={usb},server=on,wait=off"]
         if a.usb_cmd or a.usb_put or a.dbg else []) \
      + (["-netdev", "user,id=n0" + "".join(f",hostfwd={f}" for f in a.net.split()), "-device", "usb-net,netdev=n0,bus=usb-bus.2"]
         if a.net is not None else []) \
      + os.environ.get("NXTEST_QEMU_ARGS", "").split()
    log = open(os.path.join(tmp, "qemu.log"), "wb")
    proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT)
    ok = True
    fake = None
    if a.bt:
        fake = btfake.Fake(bt, hw_errors=a.bt_hw_errors)
        fake.start()
    try:
        s = socket.socket(socket.AF_UNIX)
        for _ in range(100):
            try:
                s.connect(uart)
                break
            except OSError:
                if proc.poll() is not None:
                    raise SystemExit(open(os.path.join(tmp, "qemu.log")).read())
                time.sleep(0.1)
        seen, found = read_until(s, PROMPT, a.timeout, sys.stdout)
        if not found:
            print("\n[nxtest] no shell prompt", file=sys.stderr)
            dump_cpus(mon, a.kernel)
            ok = False
        for i, c in enumerate(a.cmd if found else []):
            s.sendall(c.encode() + b"\r")
            if i == 0 and a.keys:
                time.sleep(1.0)
                m = socket.socket(socket.AF_UNIX)
                m.connect(mon)
                for k in a.keys.split():
                    m.sendall(f"sendkey {k}\n".encode())
                    time.sleep(0.15)
                m.close()
            out, found = read_until(s, PROMPT, a.timeout, sys.stdout)
            seen += out
            if not found:
                print(f"\n[nxtest] '{c}' missed the prompt in {a.timeout} s", file=sys.stderr)
                dump_cpus(mon, a.kernel)
                ok = False
                break
        print()
        if ok and (a.usb_cmd or a.usb_put or a.dbg):
            # Unread, the emulated UART stalls the console that feeds USB too.
            s.settimeout(None)
            threading.Thread(target=drain, args=(s,), daemon=True).start()
            print("[nxtest] USB-C console: typing as the PC")
            try:
                link = usbterm.EmuHost(usb, timeout=a.timeout)
                print(f"[nxtest] {link.name}")
                stop = threading.Event()
                if a.usb_net:
                    share = a.usb_net == "share"
                    threading.Thread(target=pc_network, args=(link, stop, print, share), daemon=True).start()
                    time.sleep(3)  # net looks at its links every 2 s
                puts = [tuple(p.rsplit(":", 1)) for p in a.usb_put]
                if (a.usb_cmd or puts) and not usbterm.run(link, a.usb_cmd, a.timeout, puts=puts):
                    dump_cpus(mon, a.kernel)
                    ok = False
                if a.dbg:
                    ch = nxdbg.Channel(link, a.timeout)
                    for c in a.dbg:
                        nxdbg.run(ch, c.split(), False)
                stop.set()
                link.close()
            except (OSError, ConnectionError, TimeoutError, ValueError, usbterm.Stall) as e:
                print(f"\n[nxtest] USB console: {e}", file=sys.stderr)
                dump_cpus(mon, a.kernel)
                ok = False
        if ok and a.tap and b"[input] touchscreen" not in seen and not (a.usb_cmd or a.usb_put or a.dbg):
            # input starts the panel once `power` is up, at times past the
            # prompt; a tap before that is lost to the controller's reset.
            read_until(s, b"[input] touchscreen", a.timeout, sys.stdout)
        for t in a.tap if ok else []:
            xy, _, shot = t.partition(":")
            x, y = (int(v) for v in xy.split(","))
            qmp_tap(qmp, x, y)
            print(f"[nxtest] tap {x},{y}")
            if shot:
                screendump(mon, shot)
            else:
                time.sleep(0.5)
        if a.screendump:
            screendump(mon, a.screendump)
        if fake:
            print(f"[nxtest] emulated Bluetooth controller: {fake.summary()}")
    finally:
        try:
            m = socket.socket(socket.AF_UNIX)
            m.connect(mon)
            m.sendall(b"quit\n")
            m.close()
        except OSError:
            pass
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        shutil.rmtree(tmp, ignore_errors=True)
    sys.exit(0 if ok else 1)


main()
