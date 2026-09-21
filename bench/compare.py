#!/usr/bin/env python3
"""Compare les resultats `RESULT <cle> <valeur> <unite>` de nxk et de L4T.

    compare.py --nxk nxk1.log nxk2.log --l4t l4t1.log [--md sortie.md]

Plusieurs journaux par systeme : on garde la mediane de chaque mesure (les
temps sous emulation varient d'une execution a l'autre).
"""
import argparse, re, statistics

# (cle nxk, cle L4T, libelle, sens : 'bas' = plus petit est meilleur)
ROWS = [
    ("boot_to_init_ms", "boot_to_init_ms", "Demarrage jusqu'au premier programme (ms)", "bas"),
    ("null_syscall_ns", "null_syscall_ns", "Appel systeme nul (ns)", "bas"),
    ("ipc_same_core_ns", "ipc_pipe_same_core_ns", "IPC aller-retour meme coeur (ns) - L4T : tube", "bas"),
    ("ipc_same_core_ns", "ipc_futex_same_core_ns", "IPC aller-retour meme coeur (ns) - L4T : futex", "bas"),
    ("ipc_cross_core_ns", "ipc_pipe_cross_core_ns", "IPC aller-retour 2 coeurs (ns) - L4T : tube", "bas"),
    ("ipc_cross_core_ns", "ipc_futex_cross_core_ns", "IPC aller-retour 2 coeurs (ns) - L4T : futex", "bas"),
    ("ipc_bulk_4k_mbs", "ipc_bulk_4k_mbs", "Debit, messages de 4 KiB (Mo/s)", "haut"),
    ("thread_create_join_us", "thread_create_join_us", "Creation + attente d'un thread (us)", "bas"),
    ("spawn_wait_us", "spawn_wait_us", "Lancement + attente d'un programme (us)", "bas"),
    ("compute_ms", "compute_ms", "Calcul pur, temoin (ms)", "bas"),
    ("mem_total_kb", "mem_total_kb", "RAM occupee par le systeme, noyau compris (KiB)", "bas"),
    ("kernel_mem_kb", "kernel_mem_kb", "Noyau en memoire (KiB)", "bas"),
    ("proc_rss_kb", "proc_rss_kb", "RAM du programme de mesure (KiB)", "bas"),
]


def load(paths):
    vals = {}
    for p in paths:
        for m in re.finditer(r"RESULT (\S+) (\d+)", open(p, errors="replace").read()):
            vals.setdefault(m.group(1), []).append(int(m.group(2)))
    v = {k: statistics.median(x) for k, x in vals.items()}
    # Perimetre commun : tout ce que le systeme occupe apres demarrage.
    if "mem_not_managed_kb" in v:  # Linux : hors allocateur (image, reserves, struct page) + utilise
        v["mem_total_kb"] = v["mem_not_managed_kb"] + v.get("mem_used_kb", 0)
    elif "mem_used_kb" in v:  # nxk : le total gere couvre deja noyau et metadonnees
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
    ap.add_argument("--l4t-extra", nargs="*", default=[], help="valeurs fixes cle=valeur (ex. taille du noyau)")
    a = ap.parse_args()
    n, l = load(a.nxk), load(a.l4t)
    for kv in a.l4t_extra:
        k, v = kv.split("=")
        l[k] = int(v)
    lines = ["| Mesure | nxk | L4T | Rapport |", "|---|---:|---:|---:|"]
    for kn, kl, label, sens in ROWS:
        vn, vl = n.get(kn), l.get(kl)
        ratio = "-"
        if vn and vl:
            r = vl / vn if sens == "bas" else vn / vl
            ratio = f"nxk x{r:.1f}" if r >= 1 else f"L4T x{1 / r:.1f}"
        lines.append(f"| {label} | {fmt(vn)} | {fmt(vl)} | {ratio} |")
    out = "\n".join(lines)
    print(out)
    if a.md:
        open(a.md, "w").write(out + "\n")


main()
