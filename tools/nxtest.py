#!/usr/bin/env python3
"""Pilote nxk dans tegra_qemu : demarrage, commandes au shell, capture.

    nxtest.py                          # demarre et affiche jusqu'a l'invite
    nxtest.py -c "ps" -c "run hello"   # enchaine des commandes
    nxtest.py -c "ipcbench" --timeout 120

La console UART-A est reliee a une socket : le script lit tout depuis le
premier octet emis par le noyau et tape les commandes comme un utilisateur.
Code de sortie 1 si une commande n'a pas rendu l'invite a temps.
"""
import argparse, bisect, os, re, shutil, socket, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
QEMU = os.environ.get("QEMU", os.path.expanduser("~/.cache/tegra_qemu/build/qemu-system-aarch64"))
PROMPT = b"nxk> "


def read_until(sock, marker, timeout, out):
    """Lit jusqu'a `marker` (ou fin du delai). Rend (texte, trouve)."""
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
    """Adresse -> 'symbole+decalage' d'apres la table des symboles du noyau."""
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
    """Etat de chaque coeur (PC, niveau) via le moniteur QEMU."""
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
        print(f"[nxtest] moniteur injoignable : {e}", file=sys.stderr)
        return
    name = symbolizer(elf)
    text = data.decode("utf-8", "replace")
    for cpu, pc, el in re.findall(r"CPU#(\d+).*?PC=([0-9a-f]+).*?EL(\d)", text, re.S):
        a = int(pc, 16)
        where = name(a) if a >= 0x80000000 and a < 0x100000000 else "espace utilisateur"
        print(f"[nxtest] cpu{cpu} EL{el} PC={a:#x} {where}", file=sys.stderr)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-c", "--cmd", action="append", default=[])
    ap.add_argument("--timeout", type=float, default=60, help="delai par etape (s)")
    ap.add_argument("--kernel", default=os.path.join(ROOT, "build", "nxk.elf"))
    ap.add_argument("--smp", default=None, help="(reserve)")
    ap.add_argument("--machine", default="tegrax1", help="tegrax1 (Erista) ou tegrax1plus (Mariko)")
    ap.add_argument("--monitor", default=None,
                    help="moniteur EL3 (ELF, ex. l4tboot) lance avant nxk : chemin PSCI")
    ap.add_argument("--dtb", default=None, help="arbre de peripheriques charge a 0x8D000000 (x0 du moniteur)")
    ap.add_argument("--sd", default=None, help="image de carte SD (lecteur SDMMC1)")
    ap.add_argument("--screendump", default=None, help="capture de l'ecran (PPM) apres les commandes")
    ap.add_argument("--usb-kbd", action="store_true", help="clavier USB sur le xHCI")
    ap.add_argument("--net", default=None, nargs="?", const="",
                    help="carte reseau USB (QEMU usb-net) ; valeur : redirections, ex. tcp::8080-:80")
    ap.add_argument("--keys", default=None, help="touches envoyees au clavier USB (sendkey) apres la 1re commande")
    a = ap.parse_args()

    tmp = tempfile.mkdtemp(prefix="nxk-")
    uart = os.path.join(tmp, "uart.sock")
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
            print("\n[nxtest] pas d'invite du shell", file=sys.stderr)
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
                print(f"\n[nxtest] '{c}' n'a pas rendu l'invite en {a.timeout} s", file=sys.stderr)
                dump_cpus(mon, a.kernel)
                ok = False
                break
        print()
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
