#!/usr/bin/env python3
"""Demarre le noyau L4T sur le Tegra X1 emule (meme machine que nxk) et
capture la console UART-A.

    run_l4t.py --dir ~/.cache/tegra_qemu/l4t/run --until 'RESULT END' --timeout 900

Charge : moniteur l4tboot (EL3, PSCI) a 0xFFE00000, Image a 0x80080000,
DTB a 0x8D000000, initramfs a 0x92000000 (adresses de boot.scr).
"""
import argparse, os, re, shutil, socket, subprocess, sys, tempfile, time

QEMU = os.environ.get("QEMU", os.path.expanduser("~/.cache/tegra_qemu/build/qemu-system-aarch64"))


def dump_pcs(mon):
    """Releve 3 fois les PC des 4 coeurs via le moniteur QEMU."""
    m = socket.socket(socket.AF_UNIX)
    m.connect(mon)
    m.settimeout(3)
    for k in range(3):
        m.sendall(b"info registers -a\n")
        time.sleep(2)
        data = b""
        try:
            while True:
                chunk = m.recv(1 << 20)
                if not chunk:
                    break
                data += chunk
        except socket.timeout:
            pass
        txt = data.decode(errors="replace")
        pcs = re.findall(r"CPU#(\d+)\s*\n\s*PC=([0-9a-f]{16})[\s\S]*?PSTATE=([0-9a-f]+)", txt)
        print(f"[run_l4t] releve {k}: " + "  ".join(f"cpu{c}={pc}/{ps}" for c, pc, ps in pcs[:4]))
    m.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", required=True, help="dossier contenant Image, board.dtb, initramfs.cpio, l4tboot.elf")
    ap.add_argument("--timeout", type=float, default=600)
    ap.add_argument("--until", default=None, help="expression qui termine la capture")
    ap.add_argument("--log", default=None)
    ap.add_argument("--stall", type=float, default=0,
                    help="si > 0 : apres ce silence (s), releve les PC de chaque coeur et s'arrete")
    a = ap.parse_args()
    d = os.path.expanduser(a.dir)

    tmp = tempfile.mkdtemp(prefix="l4t-")
    uart, mon = os.path.join(tmp, "uart.sock"), os.path.join(tmp, "mon.sock")
    cmd = [
        QEMU, "-machine", "tegrax1", "-m", "4G", "-display", "none",
        "-device", f"loader,file={d}/l4tboot.elf",
        "-global", "driver=tegra.evp,property=cpu-reset-vector,value=0xFFE00000",
        "-device", f"loader,file={d}/Image,addr=0x80080000,force-raw=on",
        "-device", f"loader,file={d}/board.dtb,addr=0x8D000000,force-raw=on",
        "-device", f"loader,file={d}/initramfs.cpio,addr=0x92000000,force-raw=on",
        "-chardev", f"socket,id=uarta,path={uart},server=on,wait=on",
        "-serial", "chardev:uarta", "-serial", "null", "-serial", "null", "-serial", "null",
        "-monitor", f"unix:{mon},server,nowait",
    ]
    qlog = open(os.path.join(tmp, "qemu.log"), "wb")
    proc = subprocess.Popen(cmd, stdout=qlog, stderr=subprocess.STDOUT)
    out = open(a.log, "w", buffering=1) if a.log else None
    buf = ""
    t0 = time.time()
    last_out = time.time()
    try:
        s = socket.socket(socket.AF_UNIX)
        for _ in range(100):
            try:
                s.connect(uart)
                break
            except OSError:
                time.sleep(0.1)
        while time.time() - t0 < a.timeout:
            s.settimeout(1.0)
            try:
                chunk = s.recv(65536)
            except socket.timeout:
                if proc.poll() is not None:
                    break
                if a.stall and time.time() - last_out > a.stall:
                    dump_pcs(mon)
                    break
                continue
            last_out = time.time()
            if not chunk:
                break
            txt = chunk.decode("utf-8", "replace")
            sys.stdout.write(txt)
            sys.stdout.flush()
            if out:
                out.write(txt)
            buf += txt
            if a.until and re.search(a.until, buf):
                break
        print(f"\n[run_l4t] fin de capture apres {time.time() - t0:.0f} s")
    finally:
        try:
            m = socket.socket(socket.AF_UNIX)
            m.connect(mon)
            m.sendall(b"quit\n")
        except OSError:
            pass
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        qlog.close()
        tail = open(os.path.join(tmp, "qemu.log"), errors="replace").read().splitlines()
        noise = ("No secret with id",)
        tail = [l for l in tail if not any(n in l for n in noise)]
        if tail:
            print("[run_l4t] qemu :", *tail[-10:], sep="\n  ")
        shutil.rmtree(tmp, ignore_errors=True)


main()
