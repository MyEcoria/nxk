#!/usr/bin/env python3
"""Rebuild the x86-64 test guests of the PS4 layer and their reference output.

    ps4/guest/build.py          # writes ps4/guest/bin/

Needs gcc on an x86-64 Linux PC. Outputs (committed, so neither build.sh nor CI
needs gcc):
  libmini.so   guest "libc" module (standard ELF dynamic tags)
  main.so      main program, imports by name (standard ELF)
  *-jaguar.so  both again at -O3 for the PS4's CPU (-march=btver2: AVX, BMI1, SSE4.2)
  x87.so, x87-expected.txt
               long double guest (x87), and its native output
  fp.so, fp-jaguar.so, fp-expected.txt
               the floating-point guest (fpmain.c), generic and for Jaguar, and its native
               output (no fast-math, no contraction: IEEE fixes every bit)
  main.sce     main.so repacked in the PS4 layout: ET_SCE_DYNEXEC, dynamic tables in a
               PT_SCE_DYNLIBDATA segment under the SCE tags, imports named NID#lib#module
  expected.txt stdout of the same C built natively, then "exit N"

The SCE tag numbers are those upstream's port/scripts/prepare.py reads.
"""
import base64, hashlib, os, struct, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "bin")
# The PS4 toolchain emits neither CET nor stack-protector-through-fs:0x28 nor lazy PLTs.
COMMON = ["-O2", "-fno-stack-protector", "-fcf-protection=none", "-fno-asynchronous-unwind-tables",
          "-fno-ident", "-ffreestanding", "-nostdlib"]
SO = COMMON + ["-fPIC", "-shared", "-Wl,-z,now", "-Wl,--hash-style=sysv", "-Wl,--build-id=none"]
LIBMINI_EXTRA = ["-fno-tree-loop-distribute-patterns"]  # or memcpy's loop becomes a call to memcpy
# Jaguar builds; the reference stays a generic native build (btver2 adds SSE4A, absent on Intel).
JAGUAR = ["-O3", "-march=btver2"]
FP = ["-ffp-contract=off", "-fno-math-errno"]

PT_DYNAMIC, PT_GNU_STACK, PT_SCE_DYNLIBDATA = 2, 0x6474E551, 0x61000000
ET_SCE_DYNEXEC = 0xFE10
DT = dict(NULL=0, NEEDED=1, PLTRELSZ=2, STRTAB=5, SYMTAB=6, RELA=7, RELASZ=8, STRSZ=10, JMPREL=23,
          HASH=4)
SCE = dict(NEEDED_MODULE=0x6100000F, IMPORT_LIB=0x61000015, JMPREL=0x61000029, PLTRELSZ=0x6100002D,
           RELA=0x6100002F, RELASZ=0x61000031, STRTAB=0x61000035, STRSZ=0x61000037,
           SYMTAB=0x61000039, SYMTABSZ=0x6100003F)
# Imports the runtime serves as HLE (libkernel); the rest come from the guest libc.
KERNEL = {"write", "_exit"}


def run(cmd, **kw):
    print("+", " ".join(cmd))
    return subprocess.run(cmd, check=True, **kw)


def nid(name):
    salt = bytes.fromhex("518d64a635ded8c1e6b039b1c3e55230")
    digest = hashlib.sha1(name.encode() + salt).digest()[:8][::-1]
    return base64.b64encode(digest).decode().rstrip("=").replace("/", "-")


def enc_id(n):
    """Module / library id as the PS4 writes it in symbol names (base64 digits, '-' for '/')."""
    alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+-"
    s = ""
    while True:
        s = alphabet[n % 64] + s
        n //= 64
        if not n:
            return s


def vaddr_to_off(phdrs, va):
    for p in phdrs:
        if p["type"] == 1 and p["vaddr"] <= va < p["vaddr"] + p["filesz"]:
            return p["offset"] + va - p["vaddr"]
    raise ValueError(f"{va:#x} not in a file-backed PT_LOAD")


def read_phdrs(elf):
    phoff, = struct.unpack_from("<Q", elf, 0x20)
    phnum, = struct.unpack_from("<H", elf, 0x38)
    keys = ("type", "flags", "offset", "vaddr", "paddr", "filesz", "memsz", "align")
    return phoff, [dict(zip(keys, struct.unpack_from("<IIQQQQQQ", elf, phoff + 56 * i)))
                   for i in range(phnum)]


def repack_sce(elf):
    """main.so -> the PS4 layout, same code and relocation targets."""
    elf = bytearray(elf)
    phoff, phdrs = read_phdrs(elf)
    dyn = next(p for p in phdrs if p["type"] == PT_DYNAMIC)
    tags = {}
    for pos in range(dyn["offset"], dyn["offset"] + dyn["filesz"], 16):
        t, v = struct.unpack_from("<qQ", elf, pos)
        if t == 0:
            break
        tags.setdefault(t, v)
    strtab = vaddr_to_off(phdrs, tags[DT["STRTAB"]])
    nsyms, = struct.unpack_from("<I", elf, vaddr_to_off(phdrs, tags[DT["HASH"]]) + 4)
    symtab = vaddr_to_off(phdrs, tags[DT["SYMTAB"]])

    def cstr(off):
        end = elf.index(0, strtab + off)
        return elf[strtab + off:end].decode()

    # Library ids 1 = libkernel (HLE), 2 = libc (the guest module); module ids alike.
    strings = bytearray(b"\0")

    def add(s):
        off = len(strings)
        strings.extend(s.encode() + b"\0")
        return off

    lib_off = {1: add("libkernel"), 2: add("libc")}
    syms = bytearray()
    for i in range(nsyms):
        name, info, other, shndx, value, size = struct.unpack_from("<IBBHQQ", elf, symtab + 24 * i)
        n = cstr(name) if name else ""
        if n:
            lib = 1 if n in KERNEL else 2
            name = add(f"{nid(n)}#{enc_id(lib)}#{enc_id(lib)}" if shndx == 0 else nid(n))
        syms += struct.pack("<IBBHQQ", name, info, other, shndx, value, size)
    blob = bytearray()

    def put(b):
        while len(blob) % 8:
            blob.append(0)
        off = len(blob)
        blob.extend(b)
        return off

    rela = put(elf[vaddr_to_off(phdrs, tags[DT["RELA"]]):][:tags.get(DT["RELASZ"], 0)]) if DT["RELA"] in tags else 0
    jmprel = put(elf[vaddr_to_off(phdrs, tags[DT["JMPREL"]]):][:tags[DT["PLTRELSZ"]]]) if DT["JMPREL"] in tags else 0
    sym_off, str_off = put(syms), put(strings)
    entries = [(SCE["NEEDED_MODULE"], (2 << 48) | lib_off[2]), (SCE["IMPORT_LIB"], (1 << 48) | lib_off[1]),
               (SCE["IMPORT_LIB"], (2 << 48) | lib_off[2]),
               (SCE["STRTAB"], str_off), (SCE["STRSZ"], len(strings)),
               (SCE["SYMTAB"], sym_off), (SCE["SYMTABSZ"], len(syms)),
               (SCE["RELA"], rela), (SCE["RELASZ"], tags.get(DT["RELASZ"], 0)),
               (SCE["JMPREL"], jmprel), (SCE["PLTRELSZ"], tags.get(DT["PLTRELSZ"], 0)), (0, 0)]
    dyn_tab = b"".join(struct.pack("<qQ", t, v) for t, v in entries)
    while len(elf) % 16:
        elf.append(0)
    lib_seg = len(elf)
    elf += blob
    dyn_off = len(elf)
    elf += dyn_tab
    # PT_DYNAMIC now points at the SCE table (outside PT_LOAD, as on the PS4); PT_GNU_STACK
    # becomes the DYNLIBDATA segment.
    for i, p in enumerate(phdrs):
        if p["type"] == PT_DYNAMIC:
            struct.pack_into("<IIQQQQQQ", elf, phoff + 56 * i, PT_DYNAMIC, 4, dyn_off, 0, 0,
                             len(dyn_tab), 0, 8)
        elif p["type"] == PT_GNU_STACK:
            struct.pack_into("<IIQQQQQQ", elf, phoff + 56 * i, PT_SCE_DYNLIBDATA, 4, lib_seg, 0, 0,
                             len(blob), 0, 16)
    if not any(p["type"] == PT_GNU_STACK for p in phdrs):
        raise ValueError("no PT_GNU_STACK slot to turn into PT_SCE_DYNLIBDATA")
    struct.pack_into("<H", elf, 16, ET_SCE_DYNEXEC)
    struct.pack_into("<Q", elf, 0x28, 0)  # no section headers (e_shoff, then shentsize/num/strndx)
    struct.pack_into("<HHH", elf, 0x3A, 0, 0, 0)
    return bytes(elf)


def main():
    os.makedirs(OUT, exist_ok=True)
    src = lambda n: os.path.join(HERE, n)
    run(["gcc", *SO, *LIBMINI_EXTRA, "-Wl,-soname,libmini.so", src("libmini.c"), "-o", os.path.join(OUT, "libmini.so")])
    run(["gcc", *SO, "-Wl,-e,_start", src("main.c"), "-L" + OUT, "-lmini", "-o", os.path.join(OUT, "main.so")])
    run(["gcc", *SO, *JAGUAR, *LIBMINI_EXTRA, "-Wl,-soname,libmini.so", src("libmini.c"), "-o",
         os.path.join(OUT, "libmini-jaguar.so")])
    run(["gcc", *SO, *JAGUAR, "-Wl,-e,_start", src("main.c"), "-L" + OUT, "-lmini", "-o",
         os.path.join(OUT, "main-jaguar.so")])
    run(["gcc", *SO, *FP, "-Wl,-e,_start", src("fpmain.c"), "-L" + OUT, "-lmini", "-o",
         os.path.join(OUT, "fp.so")])
    run(["gcc", *SO, *JAGUAR, *FP, "-Wl,-e,_start", src("fpmain.c"), "-L" + OUT, "-lmini", "-o",
         os.path.join(OUT, "fp-jaguar.so")])
    run(["gcc", *SO, *FP, "-Wl,-e,_start", src("x87main.c"), "-L" + OUT, "-lmini", "-o",
         os.path.join(OUT, "x87.so")])
    with tempfile.TemporaryDirectory() as tmp:
        exe = os.path.join(tmp, "x87.native")
        run(["gcc", *COMMON, *FP, *LIBMINI_EXTRA, "-static", "-no-pie", "-Wl,-e,native_entry",
             src("x87main.c"), src("libmini.c"), src("native.c"), "-o", exe])
        r = subprocess.run([exe], stdout=subprocess.PIPE)
    with open(os.path.join(OUT, "x87-expected.txt"), "wb") as f:
        f.write(r.stdout + f"exit {r.returncode}\n".encode())
    with tempfile.TemporaryDirectory() as tmp:
        exe = os.path.join(tmp, "fp.native")
        run(["gcc", *COMMON, *FP, *LIBMINI_EXTRA, "-static", "-no-pie", "-Wl,-e,native_entry",
             src("fpmain.c"), src("libmini.c"), src("native.c"), "-o", exe])
        r = subprocess.run([exe], stdout=subprocess.PIPE)
    with open(os.path.join(OUT, "fp-expected.txt"), "wb") as f:
        f.write(r.stdout + f"exit {r.returncode}\n".encode())
    with tempfile.TemporaryDirectory() as tmp:
        exe = os.path.join(tmp, "main.native")
        run(["gcc", *COMMON, *LIBMINI_EXTRA, "-static", "-no-pie", "-Wl,-e,native_entry",
             src("main.c"), src("libmini.c"), src("native.c"), "-o", exe])
        r = subprocess.run([exe], stdout=subprocess.PIPE)
    with open(os.path.join(OUT, "expected.txt"), "wb") as f:
        f.write(r.stdout + f"exit {r.returncode}\n".encode())
    with open(os.path.join(OUT, "main.so"), "rb") as f:
        sce = repack_sce(f.read())
    with open(os.path.join(OUT, "main.sce"), "wb") as f:
        f.write(sce)
    sys.stdout.write(r.stdout.decode() + f"exit {r.returncode}\n")


if __name__ == "__main__":
    main()
