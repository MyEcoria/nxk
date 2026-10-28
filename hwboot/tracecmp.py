#!/usr/bin/env python3
"""Compare the BPMP's register accesses of two oracle.sh -t traces.

    tracecmp.py HEKATE.trace IPL.trace [--context N] [--skip REGION ...]
                [--sync REGION:ADDR]
    tracecmp.py --state HEKATE.trace IPL.trace [--skip REGION ...]

Both runs must do the same writes, in the same order, with the same
values, and the same reads that decide something. Normalized first: only
the BPMP (QEMU cpu 4); the microsecond timer and the RTC (busy-wait
clocks): a run of microsecond counter reads (a delay) becomes one WAIT,
RTC reads (timeouts) are left out; a run of identical accesses (polling)
counted once.
--sync starts both at their first access to REGION at ADDR (hekate's
loader runs before its IPL; the default is hw_init's first read, the chip
id). Prints the length of the common prefix, then where the two part.

--state compares what the BPMP leaves instead, when the order may differ:
the last value it wrote to each register before the CPU cluster's first
access (on hekate's side, NVIDIA's bpmpfw.bin included), per register,
grouped by region; busy registers (SD, I2C, UART, timers) left out.
"""
import argparse
import re

LINE = re.compile(r"memory_region_ops_(read|write) cpu (\d+) .*?addr (0x[0-9a-f]+) value (0x[0-9a-f]+) size (\d+) name '([^']*)'")
WAIT = ("WAIT", "", 0, 0, 0)


def load(path, skip):
    out = []
    for line in open(path, errors="replace"):
        m = LINE.search(line)
        if not m or m.group(2) != "4":
            continue
        op, _, addr, val, size, name = m.groups()
        # tz-ppc-port[N] repeats the access the device itself logs.
        if name == "tegra.rtc" or name.startswith("tz-ppc-port") or name in skip:
            continue
        if name == "tegra.timer_us" and op == "read":
            acc = WAIT
        else:
            acc = (op[0].upper(), name, int(addr, 16), int(val, 16), int(size))
        if out and out[-1] == acc:
            continue
        out.append(acc)
    return out


NOISY = {"sdhci", "tegra.sdmmcvendor", "tegra.i2c", "tegra.timer_us", "tegra.rtc", "tegra.uart",
         "tegra.flow", "tegra.avpcache", "tegra.ape"}


def final_state(path, skip):
    """(region, address) -> last value the BPMP wrote before the CPU ran."""
    state = {}
    for line in open(path, errors="replace"):
        m = LINE.search(line)
        if not m:
            continue
        op, cpu, addr, val, _, name = m.groups()
        if cpu in ("0", "1", "2", "3"):
            break
        if cpu != "4" or op != "write" or name.startswith("tz-ppc-port") or name in NOISY or name in skip:
            continue
        state[(name, int(addr, 16))] = int(val, 16)
    return state


def compare_state(a):
    ref, new = final_state(a.ref, set(a.skip)), final_state(a.new, set(a.skip))
    keys = sorted(set(ref) | set(new))
    diff = [k for k in keys if ref.get(k) != new.get(k)]
    print(f"registers written: reference {len(ref)}, new {len(new)}; different: {len(diff)}")
    region = None
    for k in diff:
        if k[0] != region:
            region = k[0]
            print(f"-- {region}")
        r, n = ref.get(k), new.get(k)
        show_v = lambda v: "unwritten " if v is None else f"{v:#010x}"
        print(f"   +{k[1]:#010x}: reference {show_v(r)}  new {show_v(n)}")


def show(acc):
    if acc == WAIT:
        return "WAIT"
    op, name, addr, val, size = acc
    return f"{op} {name:<22} +{addr:#07x} = {val:#010x} ({size})"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--state", action="store_true", help="compare the state left at the CPU's start")
    ap.add_argument("ref")
    ap.add_argument("new")
    ap.add_argument("--context", type=int, default=6)
    ap.add_argument("--skip", nargs="*", default=[], help="regions left out of both")
    ap.add_argument("--sync", default="tegra.apb_misc:0x70000804")
    a = ap.parse_args()
    if a.state:
        return compare_state(a)
    ref, new = load(a.ref, set(a.skip)), load(a.new, set(a.skip))
    if a.sync:
        region, addr = a.sync.split(":")
        start = lambda t: next(i for i, x in enumerate(t) if x[1] == region and x[2] == int(addr, 16))
        ref, new = ref[start(ref):], new[start(new):]
    n = 0
    while n < min(len(ref), len(new)) and ref[n] == new[n]:
        n += 1
    print(f"common prefix: {n} accesses (reference {len(ref)}, new {len(new)})")
    if n == len(ref) == len(new):
        print("IDENTICAL")
        return
    lo = max(0, n - a.context)
    print("-- before the split:")
    for acc in ref[lo:n]:
        print("   " + show(acc))
    print("-- reference then:")
    for acc in ref[n:n + a.context]:
        print(" < " + show(acc))
    print("-- new then:")
    for acc in new[n:n + a.context]:
        print(" > " + show(acc))


main()
