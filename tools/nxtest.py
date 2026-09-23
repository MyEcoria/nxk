#!/usr/bin/env python3
"""Drive nxk in tegra_qemu: boot, shell commands, capture.

    nxtest.py                          # boot and print until the prompt
    nxtest.py -c "ps" -c "run hello"   # chain commands
    nxtest.py -c "ipcbench" --timeout 120
    nxtest.py --usb-cmd "ps"             # type over the USB-C console (usbterm.py host)

UART-A console is bound to a socket: the script reads everything from the
first kernel byte and types commands like a user. Exit code 1 if a command
missed the prompt in time.
"""
import argparse, bisect, os, re, shutil, socket, subprocess, sys, tempfile, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
QEMU = os.environ.get("QEMU", os.path.expanduser("~/.cache/tegra_qemu/build/qemu-system-aarch64"))
PROMPT = b"nxk> "


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
        out.write(chunk.decode("utf-8", "replace"))
        out.flush()
        if marker in buf:
            return buf, True
    return buf, False


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
    ap.add_argument("--usb-put", action="append", default=[], metavar="LOCAL:REMOTE",
                    help="file copied to the SD card over the USB-C console, before --usb-cmd")
    a = ap.parse_args()

    tmp = tempfile.mkdtemp(prefix="nxk-")
    uart = os.path.join(tmp, "uart.sock")
    usb = os.path.join(tmp, "usb.sock")
    mon = os.path.join(tmp, "mon.sock")
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
        "-serial", "chardev:uarta", "-serial", "null", "-serial", "null", "-serial", "null",
        "-monitor", f"unix:{mon},server,nowait",
    ] + (["-drive", f"if=sd,index=0,format=raw,file={a.sd}"] if a.sd else []) \
      + (["-global", "driver=tegra.xusb,property=slots,value=8"] if a.usb_kbd or a.net is not None else []) \
      + (["-device", "usb-kbd,bus=usb-bus.2"] if a.usb_kbd else []) \
      + (["-chardev", f"socket,id=usbotg,path={usb},server=on,wait=off"]
         if a.usb_cmd or a.usb_put else []) \
      + (["-netdev", "user,id=n0" + "".join(f",hostfwd={f}" for f in a.net.split()), "-device", "usb-net,netdev=n0,bus=usb-bus.2"]
         if a.net is not None else []) \
      + os.environ.get("NXTEST_QEMU_ARGS", "").split()
    log = open(os.path.join(tmp, "qemu.log"), "wb")
    proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT)
    ok = True
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
        _, found = read_until(s, PROMPT, a.timeout, sys.stdout)
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
            _, found = read_until(s, PROMPT, a.timeout, sys.stdout)
            if not found:
                print(f"\n[nxtest] '{c}' missed the prompt in {a.timeout} s", file=sys.stderr)
                dump_cpus(mon, a.kernel)
                ok = False
                break
        print()
        if ok and (a.usb_cmd or a.usb_put):
            sys.path.insert(0, HERE)
            sys.dont_write_bytecode = True
            import usbterm
            # Unread, the emulated UART stalls the console that feeds USB too.
            s.settimeout(None)
            threading.Thread(target=lambda: all(iter(lambda: s.recv(65536), b"")), daemon=True).start()
            print("[nxtest] USB-C console: typing as the PC")
            try:
                link = usbterm.EmuHost(usb, timeout=a.timeout)
                print(f"[nxtest] {link.name}")
                puts = [tuple(p.rsplit(":", 1)) for p in a.usb_put]
                if not usbterm.run(link, a.usb_cmd, a.timeout, puts=puts):
                    dump_cpus(mon, a.kernel)
                    ok = False
                link.close()
            except (OSError, ConnectionError, TimeoutError, usbterm.Stall) as e:
                print(f"\n[nxtest] USB console: {e}", file=sys.stderr)
                dump_cpus(mon, a.kernel)
                ok = False
        if a.screendump:
            m = socket.socket(socket.AF_UNIX)
            m.connect(mon)
            m.sendall(f"screendump {os.path.abspath(a.screendump)}\n".encode())
            time.sleep(1.5)
            m.close()
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
