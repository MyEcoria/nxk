#!/usr/bin/env python3
"""Compare `RESULT <key> <value> <unit>` results of nxk and L4T.

    compare.py --nxk nxk1.log nxk2.log --l4t l4t1.log [--md out.md]

Several logs per system: keep the median of each measure (emulated times
vary run to run).
"""
import argparse, re, statistics

# (nxk key, L4T key, label, sense: 'low' = smaller is better)
ROWS = [
    ("boot_to_init_ms", "boot_to_init_ms", "Boot to first program (ms)", "low"),
    ("null_syscall_ns", "null_syscall_ns", "Null syscall (ns)", "low"),
    ("ipc_same_core_ns", "ipc_pipe_same_core_ns", "IPC round-trip same core (ns) - L4T: pipe", "low"),
    ("ipc_same_core_ns", "ipc_futex_same_core_ns", "IPC round-trip same core (ns) - L4T: futex", "low"),
    ("ipc_cross_core_ns", "ipc_pipe_cross_core_ns", "IPC round-trip 2 cores (ns) - L4T: pipe", "low"),
    ("ipc_cross_core_ns", "ipc_futex_cross_core_ns", "IPC round-trip 2 cores (ns) - L4T: futex", "low"),
    ("ipc_bulk_4k_mbs", "ipc_bulk_4k_mbs", "Throughput, 4 KiB messages (MB/s)", "high"),
    ("thread_create_join_us", "thread_create_join_us", "Thread create + join (us)", "low"),
    ("spawn_wait_us", "spawn_wait_us", "Program spawn + wait (us)", "low"),
    ("compute_ms", "compute_ms", "Pure compute, control (ms)", "low"),
    ("mem_total_kb", "mem_total_kb", "RAM used by the system, kernel incl. (KiB)", "low"),
    ("kernel_mem_kb", "kernel_mem_kb", "Kernel in memory (KiB)", "low"),
    ("proc_rss_kb", "proc_rss_kb", "Measure program RAM (KiB)", "low"),
]


def load(paths):
    vals = {}
    for p in paths:
        for m in re.finditer(r"RESULT (\S+) (\d+)", open(p, errors="replace").read()):
            vals.setdefault(m.group(1), []).append(int(m.group(2)))
    v = {k: statistics.median(x) for k, x in vals.items()}
    # Common scope: everything the system occupies after boot.
    if "mem_not_managed_kb" in v:  # Linux: off-allocator (image, reserves, struct page) + used
        v["mem_total_kb"] = v["mem_not_managed_kb"] + v.get("mem_used_kb", 0)
    elif "mem_used_kb" in v:  # nxk: managed total already covers kernel and metadata
        v["mem_total_kb"] = v["mem_used_kb"]
    if "mem_kernel_kb" in v:
        v["kernel_mem_kb"] = v["mem_kernel_kb"]
    return v


def fmt(v):
    return "-" if v is None else f"{v:,.0f}".replace(",", " ")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--nxk", nargs="+", required=True)
    ap.add_argument("--l4t", nargs="+", required=True)
    ap.add_argument("--md")
    ap.add_argument("--l4t-extra", nargs="*", default=[], help="fixed key=value pairs (e.g. kernel size)")
    a = ap.parse_args()
    n, l = load(a.nxk), load(a.l4t)
    for kv in a.l4t_extra:
        k, v = kv.split("=")
        l[k] = int(v)
    lines = ["| Measure | nxk | L4T | Ratio |", "|---|---:|---:|---:|"]
    for kn, kl, label, sense in ROWS:
        vn, vl = n.get(kn), l.get(kl)
        ratio = "-"
        if vn and vl:
            r = vl / vn if sense == "low" else vn / vl
            ratio = f"nxk x{r:.1f}" if r >= 1 else f"L4T x{1 / r:.1f}"
        lines.append(f"| {label} | {fmt(vn)} | {fmt(vl)} | {ratio} |")
    out = "\n".join(lines)
    print(out)
    if a.md:
        open(a.md, "w").write(out + "\n")


main()
