#!/usr/bin/env python3
"""Drive an `oracle.sh -g PORT` run over QEMU's gdb stub (no gdb needed).

    gdbrsp.py PORT hang ADDR          nxk-ipl: at ADDR (a Thumb function, e.g.
                                      `llvm-nm` of nxk_emc::train::train), write
                                      a branch-to-self: the watchdog must fire
    gdbrsp.py PORT ptsa OUT           hekate: the DRAM table at bpmpfw's entry
                                      and at bl31's (OUT.bpmpfw, OUT.bl31), the
                                      three PTSA words of each entry printed
    gdbrsp.py PORT watch ADDR         hekate: from bpmpfw's entry, the first
                                      BPMP write to ADDR (PC and registers)

The A57s and the BPMP are two inferiors (QEMU CPU clusters); both are
attached. Breakpoints apply to every CPU.
"""
import socket
import struct
import sys
import time

BPMPFW_ENTRY = 0xFFEC0100  # hekate l4t.c BPMPFW_BASE + 0x100 (T210)
BL31_ENTRY = 0xFFF00000  # TZDRAM_BASE
MTCTABLE = 0xFFE80000
ENTRY_BYTES = 4928
LA_SCALE = 1185  # first word of la_scale_regs


class Stub:
    def __init__(self, port):
        for _ in range(100):
            try:
                self.s = socket.create_connection(("127.0.0.1", port))
                break
            except OSError:
                time.sleep(0.2)
        else:
            sys.exit(f"no gdb stub on port {port}")
        self.buf = b""
        self.cmd("qSupported:multiprocess+")
        for pid in (1, 2):
            self.cmd(f"vAttach;{pid}")

    def send(self, p):
        self.s.sendall(f"${p}#{sum(p.encode()) & 0xFF:02x}".encode())

    def recv(self, timeout=300):
        self.s.settimeout(timeout)
        while True:
            i = self.buf.find(b"$")
            j = self.buf.find(b"#", i)
            if i >= 0 and j >= 0 and len(self.buf) >= j + 3:
                pkt = self.buf[i + 1:j].decode()
                self.buf = self.buf[j + 3:]
                self.s.sendall(b"+")
                return pkt
            self.buf += self.s.recv(65536)

    def cmd(self, p, timeout=30):
        self.send(p)
        return self.recv(timeout)

    def monitor(self, c):
        self.send("qRcmd," + c.encode().hex())
        out = ""
        while True:
            r = self.recv(30)
            if r.startswith("O") and r != "OK":
                out += bytes.fromhex(r[1:]).decode()
                continue
            return out

    def run_to(self, kind, addr):
        self.cmd(f"Z{kind},{addr:x},4")
        self.send("vCont;c")
        stop = self.recv()
        self.cmd(f"z{kind},{addr:x},4")
        return stop

    def regs(self):
        g = self.cmd("g")
        return [struct.unpack("<I", bytes.fromhex(g[i * 8:i * 8 + 8]))[0] for i in range(16)]


def table(stub, path):
    stub.monitor(f'pmemsave {MTCTABLE:#x} {5 * ENTRY_BYTES} "{path}"')
    time.sleep(1)
    d = open(path, "rb").read()
    return [[struct.unpack_from("<I", d, e * ENTRY_BYTES + (LA_SCALE + k) * 4)[0] for k in range(3)]
            for e in range(5)]


def main():
    port, what = int(sys.argv[1]), sys.argv[2]
    stub = Stub(port)
    if what == "hang":
        addr = int(sys.argv[3], 16)
        print("stop:", stub.run_to(0, addr))
        print("patch:", stub.cmd(f"M{addr:x},2:fee7"))
    elif what == "ptsa":
        out = sys.argv[3]
        for name, addr in (("bpmpfw", BPMPFW_ENTRY), ("bl31", BL31_ENTRY)):
            stub.run_to(0, addr)
            print(name, [[hex(w) for w in e] for e in table(stub, f"{out}.{name}")])
    elif what == "watch":
        addr = int(sys.argv[3], 16)
        stub.run_to(0, BPMPFW_ENTRY)
        print("stop:", stub.run_to(2, addr))
        print("r0-r15:", [hex(r) for r in stub.regs()])
    else:
        sys.exit(__doc__)
    stub.send("vCont;c")


main()
