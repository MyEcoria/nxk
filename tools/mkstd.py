#!/usr/bin/env python3
"""""Prepare the nxk Rust toolchain (ported standard library).

1. Copy the rustup stable toolchain to ~/.cache/nxk/toolchain (hardlinks: no
   space or time cost), except the library sources, really copied since we
   patch them.
2. Install the port there: std/library files (new `nxk` modules), the kernel
   ABI (abi/src/lib.rs, single source) and hooks in std dispatch files (one
   `target_os = "nxk"` line next to Motor OS, another libc-less system).
3. Write the std/aarch64-unknown-nxk.json target (link script included).
4. Register the toolchain with rustup as `nxk`.

Idempotent: a file is rewritten only when its content changes. When the port
changes, std fingerprints are wiped in ~/.cache/nxk/target-apps (cargo -Z
build-std does not watch toolchain sources); for another build dir, run
`cargo clean`.

Then:  cargo +nxk build -Z build-std=std,panic_abort \\
               --target std/aarch64-unknown-nxk.json   (see apps/)
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
        path.unlink()  # break any hardlink to the pristine toolchain
    path.write_bytes(data)
    return True


# Hooks: (file, old text, new text). The old text must occur exactly once in
# the pristine source.
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
        # Generic `output` (pipes + both-output read).
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

# Futex sync primitives: nxk joins the Motor list.
for f in ["mutex", "condvar", "once", "rwlock", "thread_parking"]:
    PATCHES[f"std/src/sys/sync/{f}/mod.rs"] = ["motor-line"]


def patch(src, rules, name):
    for rule in rules:
        if rule == "motor-line":
            lines = src.split("\n")
            idx = [i for i, l in enumerate(lines) if l.strip() == 'target_os = "motor",']
            if len(idx) != 1:
                sys.exit(f"mkstd: {name}: motor line missing or repeated")
            i = idx[0]
            lines.insert(i + 1, lines[i].replace('"motor"', '"nxk"'))
            src = "\n".join(lines)
            continue
        old, new = rule
        n = src.count(old)
        if n != 1:
            sys.exit(f"mkstd: {name}: pattern found {n} times:\n{old}")
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
    # After the "-z text" rustc adds for static-PIE: the RELRO segment is
    # read-only and the kernel relocates it.
    "post-link-args": {"gnu-lld": ["-z", "notext"]},
    "metadata": {"description": "ARM64 nxk (Rust microkernel, Tegra X1)", "host_tools": False, "std": True, "tier": 3},
}


def main():
    sysroot = pathlib.Path(run("rustup", "run", "stable", "rustc", "--print", "sysroot"))
    version = run("rustup", "run", "stable", "rustc", "-V")
    orig_lib = sysroot / LIB
    if not (orig_lib / "std").is_dir():
        sys.exit("mkstd: library sources missing (rustup component add rust-src)")

    stamp = TC / ".nxk-version"
    if not stamp.exists() or stamp.read_text() != version:
        print(f"mkstd: copying toolchain {version} -> {TC}")
        if TC.exists():
            shutil.rmtree(TC)
        CACHE.mkdir(parents=True, exist_ok=True)
        subprocess.run(["cp", "-al", str(sysroot), str(TC)], check=True)
        shutil.rmtree(TC / LIB)
        shutil.copytree(orig_lib, TC / LIB, symlinks=True)
        stamp.write_text(version)

    changed = []
    lib = TC / LIB
    # Dispatch files, from the pristine source.
    for rel, rules in PATCHES.items():
        src = (orig_lib / rel).read_text()
        if write_if_changed(lib / rel, patch(src, rules, rel)):
            changed.append(rel)
    # New modules.
    port = ROOT / "std" / "library"
    for p in sorted(port.rglob("*.rs")):
        rel = p.relative_to(port)
        if write_if_changed(lib / rel, p.read_bytes()):
            changed.append(str(rel))
    # Kernel ABI, single source shared with the kernel and libnx.
    abi = (ROOT / "abi" / "src" / "lib.rs").read_text().replace("#![no_std]\n", "")
    if write_if_changed(lib / "std/src/sys/pal/nxk/abi.rs", abi):
        changed.append("std/src/sys/pal/nxk/abi.rs")

    # cargo -Z build-std ignores toolchain sources: wipe its fingerprints here
    # in the known build dirs (others: `cargo clean`).
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
        print("mkstd: toolchain \"nxk\" registered with rustup")

    print(f"mkstd: {len(changed)} file(s) updated" + (" : " + ", ".join(changed) if changed and len(changed) < 12 else ""))


if __name__ == "__main__":
    main()
