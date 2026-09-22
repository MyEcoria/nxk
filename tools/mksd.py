#!/usr/bin/env python3
"""SD card image for nxk: MBR partition table, one FAT32 partition, then
files copied inside (without mounting).

    mksd.py -o build/sd.img                   # 128 MiB, programs in /bin
    mksd.py -o sd.img --size 256 --add file:/dir/name

Uses mkfs.fat (dosfstools) and pyfatfs (~/.cache/tegra_qemu/fsvenv env).
"""
import argparse, os, struct, subprocess, sys

VENV = os.path.expanduser("~/.cache/tegra_qemu/fsvenv/bin/python")
try:
    import warnings
    warnings.filterwarnings("ignore")
    from pyfatfs.PyFatFS import PyFatFS
except ImportError:
    if os.path.exists(VENV) and sys.executable != VENV:
        os.execv(VENV, [VENV] + sys.argv)
    raise SystemExit("pyfatfs missing (pip install pyfatfs)")

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CACHE = os.environ.get("NXK_CACHE", os.path.expanduser("~/.cache/nxk"))
START = 2048  # first partition sector (1 MiB alignment)

ap = argparse.ArgumentParser()
ap.add_argument("-o", required=True)
ap.add_argument("--size", type=int, default=128, help="size in MiB (power of 2 for QEMU)")
ap.add_argument("--add", action="append", default=[], help="SOURCE:DESTINATION")
ap.add_argument("--no-bin", action="store_true", help="skip copying programs to /bin")
a = ap.parse_args()

size = a.size << 20
sectors = size // 512
with open(a.o, "wb") as f:
    f.truncate(size)
    # MBR: one FAT32 LBA partition (type 0x0C) from START to the end.
    entry = struct.pack("<B3sB3sII", 0x00, b"\xfe\xff\xff", 0x0C, b"\xfe\xff\xff", START, sectors - START)
    f.seek(446)
    f.write(entry)
    f.seek(510)
    f.write(b"\x55\xaa")
part_kib = (sectors - START) // 2
subprocess.run(["mkfs.fat", "-F", "32", "-n", "NXK", f"--offset={START}", a.o, str(part_kib)],
               check=True, stdout=subprocess.DEVNULL)

fs = PyFatFS(a.o, offset=START * 512)
def put(dest, data):
    d = os.path.dirname(dest)
    if d and not fs.exists(d):
        fs.makedirs(d)
    fs.writebytes(dest, data)

if not a.no_bin:
    bindir = os.path.join(CACHE, "target-user", "aarch64-unknown-none", "release")
    for prog in ["hello", "ktest", "true", "fptest", "crash", "bench", "ipcbench"]:
        p = os.path.join(bindir, prog)
        if os.path.exists(p):
            put("/bin/" + prog, open(p, "rb").read())
    # std programs (apps/ dir).
    appdir = os.path.join(CACHE, "target-apps", "aarch64-unknown-nxk", "release")
    for prog in ["hello-std", "stdtest"]:
        p = os.path.join(appdir, prog)
        if os.path.exists(p):
            put("/bin/" + prog, open(p, "rb").read())
put("/etc/motd", "Welcome to nxk: this file comes from the SD card (FAT32).\n".encode())
put("/Documents/Read me.txt",
    "Long name and spaces: FAT32 LFN entries.\n".encode())
put("/Documents/Data/numbers.txt", "".join(f"{i}\n" for i in range(1000)).encode())
for spec in a.add:
    src, dst = spec.split(":", 1)
    put(dst, open(src, "rb").read())
fs.close()
print(f"{a.o}: {a.size} MiB, FAT32 from sector {START}")
