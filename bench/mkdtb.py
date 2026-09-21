#!/usr/bin/env python3
"""Prepare l'arbre de peripheriques officiel de la Switch (2017) pour un
demarrage direct du noyau L4T dans l'emulateur, a la maniere de boot.scr :

  - 4 GiB de DRAM (le DTB n'en declare que 2 : hekate/U-Boot corrigent) ;
  - zone du moniteur l4tboot reservee et non projetee ;
  - UART-A active comme console (comme boot.scr avec uart_port=1, earlycon) ;
  - initramfs et ligne de commande dans /chosen.

Requiert le paquet python `fdt`.
"""
import argparse, struct
import fdt

MON_BASE, MON_SIZE = 0xFFE00000, 0x200000


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dtimg", required=True, help="nx-plat.dtimg de Switchroot")
    ap.add_argument("--index", type=int, default=0, help="0 = Switch 2017 (T210)")
    ap.add_argument("--initrd-start", type=lambda v: int(v, 0), required=True)
    ap.add_argument("--initrd-size", type=lambda v: int(v, 0), required=True)
    ap.add_argument("--bootargs", required=True)
    ap.add_argument("--disable", default="", help="noeuds a desactiver, separes par des virgules")
    ap.add_argument("-o", "--output", required=True)
    a = ap.parse_args()

    d = open(a.dtimg, "rb").read()
    magic, total, hsz, esz, cnt, eoff = struct.unpack(">6I", d[:24])
    assert magic == 0xD7B7AB1E, "pas une image DT Android"
    size, off = struct.unpack(">II", d[eoff + a.index * esz: eoff + a.index * esz + 8])
    t = fdt.parse_dtb(d[off:off + size])

    t.set_property("reg", [0, 0x80000000, 1, 0], path="/memory@80000000")

    rm = t.get_node("/reserved-memory")
    node = fdt.Node(f"l4tboot@{MON_BASE:x}")
    node.append(fdt.PropWords("reg", 0, MON_BASE, 0, MON_SIZE))
    node.append(fdt.Property("no-map"))
    rm.append(node)

    t.set_property("compatible", "nvidia,tegra20-uart", path="/serial@70006000")
    t.set_property("status", "okay", path="/serial@70006000")
    t.set_property("reset-names", "noreset", path="/serial@70006000")

    # Peripheriques que l'emulateur ne modelise pas assez pour Linux.
    for path in filter(None, a.disable.split(",")):
        t.set_property("status", "disabled", path=path)

    end = a.initrd_start + a.initrd_size
    t.set_property("bootargs", a.bootargs, path="/chosen")
    t.set_property("linux,initrd-start", [a.initrd_start >> 32, a.initrd_start & 0xFFFFFFFF], path="/chosen")
    t.set_property("linux,initrd-end", [end >> 32, end & 0xFFFFFFFF], path="/chosen")

    open(a.output, "wb").write(t.to_dtb(version=17))
    print(f"DTB : {t.get_property('model').value} -> {a.output}")


main()
