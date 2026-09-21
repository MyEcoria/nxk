#!/usr/bin/env python3
"""Image de carte SD pour nxk : table de partitions MBR, une partition FAT32,
puis des fichiers copies dedans (sans monter l'image).

    mksd.py -o build/sd.img                   # 128 Mio, programmes dans /bin
    mksd.py -o sd.img --size 256 --add fichier:/dossier/nom

Utilise mkfs.fat (dosfstools) et pyfatfs (environnement ~/.cache/tegra_qemu/fsvenv).
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
    raise SystemExit("pyfatfs introuvable (pip install pyfatfs)")

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CACHE = os.environ.get("NXK_CACHE", os.path.expanduser("~/.cache/nxk"))
START = 2048  # secteur de debut de la partition (alignement 1 Mio)

ap = argparse.ArgumentParser()
ap.add_argument("-o", required=True)
ap.add_argument("--size", type=int, default=128, help="taille en Mio (puissance de 2 pour QEMU)")
ap.add_argument("--add", action="append", default=[], help="SOURCE:DESTINATION")
ap.add_argument("--no-bin", action="store_true", help="ne pas copier les programmes dans /bin")
a = ap.parse_args()

size = a.size << 20
sectors = size // 512
with open(a.o, "wb") as f:
    f.truncate(size)
    # MBR : une partition FAT32 (LBA, type 0x0C) de START a la fin.
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
    # Programmes std (dossier apps/).
    appdir = os.path.join(CACHE, "target-apps", "aarch64-unknown-nxk", "release")
    for prog in ["hello-std", "stdtest"]:
        p = os.path.join(appdir, prog)
        if os.path.exists(p):
            put("/bin/" + prog, open(p, "rb").read())
put("/etc/motd", "Bienvenue sur nxk : ce fichier vient de la carte SD (FAT32).\n".encode())
put("/Documents/Lisez-moi du système.txt",
    "Nom long, accents et espaces : les entrees LFN de FAT32.\n".encode())
put("/Documents/Données/chiffres.txt", "".join(f"{i}\n" for i in range(1000)).encode())
for spec in a.add:
    src, dst = spec.split(":", 1)
    put(dst, open(src, "rb").read())
fs.close()
print(f"{a.o} : {a.size} Mio, FAT32 a partir du secteur {START}")
