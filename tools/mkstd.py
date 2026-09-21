#!/usr/bin/env python3
"""Prepare la chaine Rust de nxk (bibliotheque standard portee).

1. Copie la chaine stable de rustup dans ~/.cache/nxk/toolchain (liens durs :
   ni place ni temps), sauf les sources de la bibliotheque, copiees pour de
   bon puisqu'on les modifie.
2. Y installe le portage : les fichiers de std/library (nouveaux modules
   `nxk`), l'ABI du noyau (abi/src/lib.rs, source unique) et les branchements
   dans les fichiers d'aiguillage de std (une ligne `target_os = "nxk"` a cote
   de celle de Motor OS, autre systeme sans libc).
3. Ecrit la cible std/aarch64-unknown-nxk.json (script de liaison inclus).
4. Declare la chaine a rustup sous le nom `nxk`.

Idempotent : un fichier n'est recrit que si son contenu change. Quand le
portage change, les empreintes de std sont effacees dans ~/.cache/nxk/target-apps
(cargo -Z build-std ne surveille pas les sources de la chaine) ; pour un autre
dossier de compilation, faire `cargo clean`.

Ensuite :  cargo +nxk build -Z build-std=std,panic_abort \\
               --target std/aarch64-unknown-nxk.json   (voir apps/)
"""
import json
import os
import pathlib
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
CACHE = pathlib.Path.home() / ".cache" / "nxk"
TC = CACHE / "toolchain"
LIB = pathlib.Path("lib/rustlib/src/rust/library")


def run(*cmd):
    return subprocess.run(cmd, check=True, capture_output=True, text=True).stdout.strip()


def write_if_changed(path, data):
    path = pathlib.Path(path)
    if isinstance(data, str):
        data = data.encode()
    if path.exists() and path.read_bytes() == data:
        return False
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        path.unlink()  # casse un eventuel lien dur vers la chaine d'origine
    path.write_bytes(data)
    return True


# Branchements : (fichier, ancien texte, nouveau texte). L'ancien texte doit
# figurer exactement une fois dans la source d'origine.
def arm_after(anchor, arm):
    return (anchor, anchor + arm)


PATCHES = {
    "std/build.rs": [
        ('        || target_os == "motor"\n', '        || target_os == "motor"\n        || target_os == "nxk"\n'),
    ],
    "std/src/sys/pal/mod.rs": [
        arm_after(
            '    target_os = "motor" => {\n        mod motor;\n        pub use self::motor::*;\n    }\n',
            '    target_os = "nxk" => {\n        mod nxk;\n        pub use self::nxk::*;\n    }\n',
        ),
    ],
    "std/src/sys/alloc/mod.rs": [
        arm_after(
            '    target_os = "motor" => {\n        mod motor;\n    }\n',
            '    target_os = "nxk" => {\n        mod nxk;\n    }\n',
        ),
    ],
    "std/src/sys/args/mod.rs": [
        ('    target_os = "motor",\n    target_os = "uefi",\n', '    target_os = "motor",\n    target_os = "nxk",\n    target_os = "uefi",\n'),
        arm_after(
            '    target_os = "motor" => {\n        mod motor;\n        pub use motor::*;\n    }\n',
            '    target_os = "nxk" => {\n        mod nxk;\n        pub use nxk::*;\n    }\n',
        ),
    ],
    "std/src/sys/env/mod.rs": [
        ('    target_os = "motor",\n    all(target_vendor = "fortanix"', '    target_os = "motor",\n    target_os = "nxk",\n    all(target_vendor = "fortanix"'),
        arm_after(
            '    target_os = "motor" => {\n        mod motor;\n        pub use motor::*;\n    }\n',
            '    target_os = "nxk" => {\n        mod nxk;\n        pub use nxk::*;\n    }\n',
        ),
    ],
    "std/src/sys/exit.rs": [
        arm_after(
            '        target_os = "motor" => {\n            moto_rt::process::exit(code)\n        }\n',
            '        target_os = "nxk" => {\n            crate::sys::pal::syscall::process_exit(code as i64)\n        }\n',
        ),
    ],
    "std/src/sys/fs/mod.rs": [
        arm_after(
            '    target_os = "motor" => {\n        mod motor;\n        use motor as imp;\n    }\n',
            '    target_os = "nxk" => {\n        mod nxk;\n        use nxk as imp;\n        pub use nxk::absolute as nxk_absolute;\n    }\n',
        ),
    ],
    "std/src/sys/io/error/mod.rs": [
        arm_after(
            '    target_os = "motor" => {\n        mod motor;\n        pub use motor::*;\n    }\n',
            '    target_os = "nxk" => {\n        mod nxk;\n        pub use nxk::*;\n    }\n',
        ),
    ],
    "std/src/sys/net/connection/mod.rs": [
        arm_after(
            '    target_os = "motor" => {\n        mod motor;\n        pub use motor::*;\n    }\n',
            '    target_os = "nxk" => {\n        mod nxk;\n        pub use nxk::*;\n    }\n',
        ),
    ],
    "std/src/sys/paths/mod.rs": [
        ('    target_os = "motor" => {\n        mod motor;\n        #[expect(dead_code)]\n',
         '    target_os = "nxk" => {\n        mod nxk;\n        use nxk as imp;\n    }\n'
         '    target_os = "motor" => {\n        mod motor;\n        #[expect(dead_code)]\n'),
    ],
    "std/src/sys/process/mod.rs": [
        arm_after(
            '    target_os = "motor" => {\n        mod motor;\n        use motor as imp;\n    }\n',
            '    target_os = "nxk" => {\n        mod nxk;\n        use nxk as imp;\n    }\n',
        ),
        # `output` generique (tubes + lecture des deux sorties).
        ('    target_os = "windows",\n    target_os = "motor"\n))]\n', '    target_os = "windows",\n    target_os = "motor",\n    target_os = "nxk"\n))]\n'),
        ('    target_os = "windows",\n    target_os = "motor"\n)))]\n', '    target_os = "windows",\n    target_os = "motor",\n    target_os = "nxk"\n)))]\n'),
    ],
    "std/src/sys/io/mod.rs": [
        arm_after(
            '        target_os = "motor" => {\n            mod motor;\n            pub use motor::*;\n        }\n',
            '        target_os = "nxk" => {\n            mod nxk;\n            pub use nxk::*;\n        }\n',
        ),
    ],
    "std/src/sys/pipe/mod.rs": [
        arm_after(
            '    target_os = "motor" => {\n        mod motor;\n        pub use motor::{Pipe, pipe};\n    }\n',
            '    target_os = "nxk" => {\n        mod nxk;\n        pub use nxk::{Pipe, pipe};\n        pub(crate) use nxk::{read_from, write_to};\n    }\n',
        ),
    ],
    "std/src/sys/random/mod.rs": [
        arm_after(
            '    target_os = "motor" => {\n        mod motor;\n        pub use motor::fill_bytes;\n    }\n',
            '    target_os = "nxk" => {\n        mod nxk;\n        pub use nxk::fill_bytes;\n    }\n',
        ),
    ],
    "std/src/sys/stdio/mod.rs": [
        arm_after(
            '    target_os = "motor" => {\n        mod motor;\n        pub use motor::*;\n    }\n',
            '    target_os = "nxk" => {\n        mod nxk;\n        pub use nxk::*;\n    }\n',
        ),
    ],
    "std/src/sys/thread/mod.rs": [
        arm_after(
            '    target_os = "motor" => {\n        mod motor;\n        pub use motor::*;\n    }\n',
            '    target_os = "nxk" => {\n        mod nxk;\n        pub use nxk::{Thread, available_parallelism, current_os_id, set_name, sleep, yield_now, DEFAULT_MIN_STACK_SIZE};\n    }\n',
        ),
    ],
    "std/src/sys/time/mod.rs": [
        arm_after(
            '    target_os = "motor" => {\n        use moto_rt::time as imp;\n    }\n',
            '    target_os = "nxk" => {\n        mod nxk;\n        use nxk as imp;\n    }\n',
        ),
    ],
    "std/src/sys/thread_local/mod.rs": [
        ('        any(\n            target_os = "hermit",\n            target_os = "xous",\n        ) => {\n',
         '        any(\n            target_os = "hermit",\n            target_os = "nxk",\n            target_os = "xous",\n        ) => {\n'),
    ],
    "std/src/os/mod.rs": [
        ('#[cfg(target_os = "motor")]\npub mod motor;\n', '#[cfg(target_os = "motor")]\npub mod motor;\n#[cfg(target_os = "nxk")]\npub mod nxk;\n'),
    ],
    "std/src/sys/env_consts.rs": [
        ('#[cfg(target_os = "openbsd")]\n',
         '#[cfg(target_os = "nxk")]\npub mod os {\n    pub const FAMILY: &str = "";\n    pub const OS: &str = "nxk";\n'
         '    pub const DLL_PREFIX: &str = "";\n    pub const DLL_SUFFIX: &str = "";\n    pub const DLL_EXTENSION: &str = "";\n'
         '    pub const EXE_SUFFIX: &str = "";\n    pub const EXE_EXTENSION: &str = "";\n}\n\n#[cfg(target_os = "openbsd")]\n'),
    ],
}

# Primitives de synchronisation par futex : nxk rejoint la liste de Motor.
for f in ["mutex", "condvar", "once", "rwlock", "thread_parking"]:
    PATCHES[f"std/src/sys/sync/{f}/mod.rs"] = ["motor-line"]


def patch(src, rules, name):
    for rule in rules:
        if rule == "motor-line":
            lines = src.split("\n")
            idx = [i for i, l in enumerate(lines) if l.strip() == 'target_os = "motor",']
            if len(idx) != 1:
                sys.exit(f"mkstd: {name}: ligne motor introuvable ou multiple")
            i = idx[0]
            lines.insert(i + 1, lines[i].replace('"motor"', '"nxk"'))
            src = "\n".join(lines)
            continue
        old, new = rule
        n = src.count(old)
        if n != 1:
            sys.exit(f"mkstd: {name}: motif trouve {n} fois :\n{old}")
        src = src.replace(old, new)
    return src


TARGET = {
    "arch": "aarch64",
    "data-layout": "e-m:e-p270:32:32-p271:32:32-p272:64:64-i8:8:32-i16:16:32-i64:64-i128:128-n32:64-S128-Fn32",
    "llvm-target": "aarch64-unknown-none",
    "target-pointer-width": 64,
    "max-atomic-width": 128,
    "os": "nxk",
    "cpu": "cortex-a57",
    "features": "+v8a,+neon",
    "linker": "rust-lld",
    "linker-flavor": "gnu-lld",
    "panic-strategy": "abort",
    "relocation-model": "pie",
    "position-independent-executables": True,
    "static-position-independent-executables": True,
    "crt-static-default": True,
    "crt-static-respected": True,
    "has-thread-local": True,
    "tls-model": "local-exec",
    "stack-probes": {"kind": "inline"},
    "eh-frame-header": False,
    "emit-debug-gdb-scripts": False,
    "pre-link-args": {"gnu-lld": ["--pack-dyn-relocs=none", "-z", "max-page-size=4096"]},
    # Apres le « -z text » que rustc ajoute pour un static-PIE : le segment
    # RELRO est en lecture seule et c'est le noyau qui le reloge.
    "post-link-args": {"gnu-lld": ["-z", "notext"]},
    "metadata": {"description": "ARM64 nxk (micro-noyau Rust, Tegra X1)", "host_tools": False, "std": True, "tier": 3},
}


def main():
    sysroot = pathlib.Path(run("rustup", "run", "stable", "rustc", "--print", "sysroot"))
    version = run("rustup", "run", "stable", "rustc", "-V")
    orig_lib = sysroot / LIB
    if not (orig_lib / "std").is_dir():
        sys.exit("mkstd: sources de la bibliotheque absentes (rustup component add rust-src)")

    stamp = TC / ".nxk-version"
    if not stamp.exists() or stamp.read_text() != version:
        print(f"mkstd: copie de la chaine {version} -> {TC}")
        if TC.exists():
            shutil.rmtree(TC)
        CACHE.mkdir(parents=True, exist_ok=True)
        subprocess.run(["cp", "-al", str(sysroot), str(TC)], check=True)
        shutil.rmtree(TC / LIB)
        shutil.copytree(orig_lib, TC / LIB, symlinks=True)
        stamp.write_text(version)

    changed = []
    lib = TC / LIB
    # Fichiers d'aiguillage, depuis la source d'origine.
    for rel, rules in PATCHES.items():
        src = (orig_lib / rel).read_text()
        if write_if_changed(lib / rel, patch(src, rules, rel)):
            changed.append(rel)
    # Nouveaux modules.
    port = ROOT / "std" / "library"
    for p in sorted(port.rglob("*.rs")):
        rel = p.relative_to(port)
        if write_if_changed(lib / rel, p.read_bytes()):
            changed.append(str(rel))
    # ABI du noyau, source unique partagee avec le noyau et libnx.
    abi = (ROOT / "abi" / "src" / "lib.rs").read_text().replace("#![no_std]\n", "")
    if write_if_changed(lib / "std/src/sys/pal/nxk/abi.rs", abi):
        changed.append("std/src/sys/pal/nxk/abi.rs")

    # cargo -Z build-std tient les sources de la chaine pour immuables : il ne
    # recompile pas std quand elles changent. On efface donc ses empreintes
    # dans les dossiers de compilation connus (les autres : `cargo clean`).
    if changed:
        for fp in CACHE.glob("target-apps/*/*/.fingerprint/std-*"):
            shutil.rmtree(fp)

    target = dict(TARGET)
    target["link-script"] = (ROOT / "std" / "nxk.ld").read_text()
    if write_if_changed(ROOT / "std" / "aarch64-unknown-nxk.json", json.dumps(target, indent=2) + "\n"):
        changed.append("aarch64-unknown-nxk.json")

    toolchains = run("rustup", "toolchain", "list", "-v")
    if not any(l.split()[0] == "nxk" for l in toolchains.splitlines() if l.strip()):
        subprocess.run(["rustup", "toolchain", "link", "nxk", str(TC)], check=True)
        print("mkstd: chaine « nxk » declaree a rustup")

    print(f"mkstd: {len(changed)} fichier(s) mis a jour" + (" : " + ", ".join(changed) if changed and len(changed) < 12 else ""))


if __name__ == "__main__":
    main()
