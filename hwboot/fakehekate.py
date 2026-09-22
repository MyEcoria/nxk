#!/usr/bin/env python3
"""Etat d'ecran laisse par hekate, pour tester la console d'ecran sous QEMU.

    fakehekate.py amorce SORTIE.bin   # code AArch64 a charger en 0xF0000000
    fakehekate.py logo SORTIE.raw     # faux logo (image 720x1280 a 0xF5A00000)
    fakehekate.py lire IMAGE.raw      # relit en texte l'image 720x1280

`amorce` fait ce que l'entree L4T de hekate laisse derriere elle (affichage
« sans couture ») : horloges DISP1/HOST1X ouvertes, fenetre A du DC-A allumee
sur 0xF5A00000 en 720x1280 B8G8R8A8, puis saut sur nxk-bl31 (0xFFF00000).
Les chargeurs de QEMU ne savent pas ecrire dans des registres : d'ou ces
quelques instructions (movz/movk/str/br). `lire` reconnait les caracteres de
la police 8x16 de nxk (build/font.bin) dessines en gris clair sur noir, et
compte les pixels d'une autre couleur (reste du logo).
"""
import os
import struct
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FB, W, H = 0xF5A0_0000, 720, 1280
DC = 0x5420_0000


def mov(rd, v, w=False):
    """movz + movk : charge `v` dans x`rd` (w`rd` si `w`)."""
    out = []
    for hw in range(2 if w else 4):
        imm = (v >> (16 * hw)) & 0xFFFF
        if hw == 0:
            out.append((0x52800000 if w else 0xD2800000) | (imm << 5) | rd)
        elif imm:
            out.append((0x72800000 if w else 0xF2800000) | (hw << 21) | (imm << 5) | rd)
    return out


def amorce(path):
    dc = lambda i, v: (DC + i * 4, v)
    regs = [
        (0x6000_6010, 0x9800_0130),  # CLK_OUT_ENB_L : DISP1 | HOST1X (+ reset)
        (0x6000_6004, 0x04D3_D2C8),  # RST_DEVICES_L : DISP1, HOST1X hors reset
        dc(0x42, 1 << 4),            # DISPLAY_WINDOW_HEADER : fenetre A
        dc(0x409, (H << 16) | W),    # DISP_ACTIVE
        dc(0x703, 12),               # WIN_COLOR_DEPTH : B8G8R8A8
        dc(0x704, 0),                # WIN_POSITION
        dc(0x705, (H << 16) | W),    # WIN_SIZE
        dc(0x706, (H << 16) | W * 4),  # WIN_PRESCALED_SIZE
        dc(0x709, 0x1000_1000),      # WIN_DDA_INC
        dc(0x70A, (W * 2 << 16) | W * 4),  # WIN_LINE_STRIDE
        dc(0x800, FB),               # WINBUF_START_ADDR
        dc(0x700, 1 << 30),          # WIN_OPTIONS : WIN_ENABLE
        dc(0x32, 1 << 5),            # DISPLAY_COMMAND : continu
        dc(0x41, 0x300),             # STATE_CONTROL : UPDATE puis ACT_REQ
        dc(0x41, 0x3),
    ]
    code = []
    for a, v in regs:
        code += mov(1, a) + mov(2, v, w=True) + [0xB9000000 | (1 << 5) | 2]  # str w2, [x1]
    code += mov(0, 0xFFF0_0000) + [0xD61F0000]  # br x0
    with open(path, "wb") as f:
        f.write(b"".join(struct.pack("<I", i) for i in code))


def logo(path):
    line = b"".join(struct.pack("<I", 0xFF00_4080 | (x * 255 // W) << 16) for x in range(W))
    with open(path, "wb") as f:
        f.write(line * H)


def lire(path):
    font = open(os.path.join(ROOT, "build", "font.bin"), "rb").read()
    d = open(path, "rb").read()
    fg, bg = struct.pack("<I", 0xFFD8_D8D8), struct.pack("<I", 0xFF00_0000)
    autres = sum(1 for o in range(0, len(d), 4) if d[o:o + 4] not in (fg, bg))
    glyphs = {font[c * 16:(c + 1) * 16]: chr(c) for c in range(0x21, 0x7F)}
    glyphs[bytes(16)] = " "
    for row in range(H // 16):
        line = ""
        for col in range(W // 8):
            bm = bytearray(16)
            for dy in range(16):
                o = ((row * 16 + dy) * W + col * 8) * 4
                for dx in range(8):
                    if d[o + dx * 4:o + dx * 4 + 4] == fg:
                        bm[dy] |= 0x80 >> dx
            line += glyphs.get(bytes(bm), "#")
        print(line.rstrip())
    print(f"pixels hors texte : {autres}")


if __name__ == "__main__":
    if len(sys.argv) != 3 or sys.argv[1] not in ("amorce", "logo", "lire"):
        raise SystemExit(__doc__)
    globals()[sys.argv[1]](sys.argv[2])
