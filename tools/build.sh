#!/usr/bin/env bash
# Construit nxk : programmes utilisateur -> image de demarrage -> noyau.
# Les dossiers de compilation vivent hors du depot (~/.cache/nxk) : le projet
# est synchronise et n'a pas a transporter les artefacts.
set -euo pipefail

ici="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cache="${NXK_CACHE:-$HOME/.cache/nxk}"
export PATH="$HOME/.cargo/bin:$PATH"

progs=(init console shell hello ipcbench crash fptest bench true ktest clk i2c power sd fs display input usb net nettest)
# Programmes ordinaires (std), dossier apps/.
apps=(hello-std stdtest)

echo "== espace utilisateur"
# Police de l'ecran, convertie depuis une police console du systeme.
python3 "$ici/tools/mkfont.py" -o "$ici/build/font.bin"
# cargo lit .cargo/config.toml depuis le repertoire courant : on y entre.
(cd "$ici/user" && CARGO_TARGET_DIR="$cache/target-user" cargo build --release)
bins=()
for p in "${progs[@]}"; do bins+=("$cache/target-user/aarch64-unknown-none/release/$p"); done

echo "== programmes std"
# Chaine « nxk » : Rust stable dont la bibliotheque standard est portee.
python3 "$ici/tools/mkstd.py"
(cd "$ici/apps" && RUSTC_BOOTSTRAP=1 CARGO_TARGET_DIR="$cache/target-apps" cargo build --release)
for p in "${apps[@]}"; do bins+=("$cache/target-apps/aarch64-unknown-nxk/release/$p"); done

python3 "$ici/tools/mkbootfs.py" -o "$ici/build/bootfs.img" "${bins[@]}"

echo "== noyau"
# NXK_FEATURES=idle-poll : coeurs au repos en attente active (comparaison L4T).
(cd "$ici/kernel" && CARGO_TARGET_DIR="$cache/target-kernel" cargo build --release ${NXK_FEATURES:+--features "$NXK_FEATURES"})
ln -sf "$cache/target-kernel/aarch64-unknown-none-softfloat/release/nxk" "$ici/build/nxk.elf"
echo "== pret : build/nxk.elf"
