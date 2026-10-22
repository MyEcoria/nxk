<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/logo/nxk-logo-dark.svg">
    <img alt="nxk" src="docs/logo/nxk-logo-light.svg" width="384">
  </picture>
</p>

# nxk — AArch64 microkernel for Tegra X1

Microkernel written **entirely in Rust**, with no host OS, for the
NVIDIA Tegra X1 SoC in the Nintendo Switch (Erista and Mariko). It runs
directly on the four Cortex-A57 cores. It is a **video game console**:
the screen is a game library (`display`), the GPU draws 3D in games
(`gpu`, the engine written from its registers up), games are ordinary
Rust programs on a small SDK (`jeux/nxgfx`); and it stays a
microkernel with a shell over the USB-C console and a system dashboard.

Like Linux, it is the interface between hardware and programs, which
are written on top of system calls. But as a true microkernel, it keeps
only what requires EL1 privilege: address spaces, threads, scheduling,
IPC, capabilities, interrupt routing. **Drivers and services (files,
network…) are ordinary EL0 programs**, holding only the capabilities
entrusted to them.

Programs are written in **ordinary Rust, with the standard library**
(`std` is ported: target `aarch64-unknown-nxk`). Application software
(web server, SSH…) does not belong to the system: it is just programs
like any other.

```
EL0  Rust programs (std) · shell
     services: fs (FAT32) · net (TCP/IP) · display (library, screen, input) · input · power
     drivers : console (UART) · usbcon (USB-C) · sd · usb (xHCI) · i2c · clk · gpu · bluetooth
     init: process manager and name service
     ─────────── ported std · libnx (no_std) ───────────
     ─────────── svc #0: 53 system calls ───────────
EL1  nxk: memory · threads · scheduler · IPC · capabilities · IRQ · futex
```

Everything but the GPU has been verified in the `tegra_qemu` emulator. On
a **real Switch** (Erista), nxk boots through hekate on its four cores;
the `display` service shows the **game library** (cards read from
`/games`, played with touch or a gamepad, HOME to leave) and the
dashboard pages (overview, system, network, a mirror of the USB-C
terminal, screen messages); games draw with the **GPU's 3D engine**
(vertex and fragment shaders, depth, textures, block-linear
surfaces, 60 Hz flips); the shell is reached over the USB-C console (`usbcon`: the
Switch as a USB serial device, `tools/usbterm.py` on the PC), which also
carries a debug channel (`tools/nxdbg.py`: screen capture, touch and
gamepad input), with the SD card and Wi-Fi (see "Limits" and
`hwboot/README.md`).

## Games

`jeux/` holds the game SDK (`nxgfx`, a library on `nxk-abi` alone:
screen, GPU command streams, meshes, a 2D canvas with text, input, math)
and the games. `jeux/starlane` is the sample, a 3D lane runner:

```bash
./tools/build.sh && ./tools/mksd.py -o build/sd.img --firmware firmware
# a game is /games/NAME/NAME on the card, with /games/NAME/game.txt:
#   title=Starlane  publisher=nxk  version=1.4.0  description=...  color=FF50C8
# and its cover art /games/NAME/cover.bin (tools/mkcovers.py, build/covers/)
./tools/usbterm.py --put ~/.cache/nxk/target-apps/aarch64-unknown-nxk/release/starlane:/games/starlane/starlane
```

A game opens `Graphics::open()`, which powers the GPU, takes the screen
(three block-linear images the GPU draws on, flipped at the vertical
blank) and gets the input `display` routes to it (`+` stays the
console's: it opens the quick settings drawer over the game, as the
volume buttons show their indicator); each frame is a
command stream (`Frame`: clear, pipeline, uniforms, indexed draws,
canvas) the GPU service checks and runs. The panel is portrait:
`Graphics::projection` turns the landscape picture. HOME (or the
`Event::Button` the SDK passes) ends a game; the library is back.

### Shaders and compute

Beyond the built-in pipelines, a game writes its own GPU programs with
`nxgfx::ir::Builder` (the IR of the `shader/` crate: typed registers,
`if`, loops bounded by a trip count, texture samples, range-checked
buffer loads and stores, atomics), vertex and fragment shaders as well as
compute shaders. `Graphics::shader` sends one to the `gpu` service, which
validates it and compiles it to the Tegra X1's instruction set (nothing a
program does can reach memory it was not given, and no loop runs without
end); `pipeline_from` joins a vertex and a fragment shader with their
vertex layout, culling, depth state and blending; a `Frame` then does
`use_pipeline`, `bind_buffer`, `bind_texture`, `uniforms`, `draw_buffers`
and `dispatch`, with `barrier` between passes that read each other's
output. `gpu irtest` checks the compiler on the console itself: random
programs of the IR run by 64 threads against the CPU reference, a
vertex and fragment pair drawing a pixel, four instances of a triangle,
and a gradient rendered to an R32F, an RGBA16F and an RGBA32F target
then sampled back through a texture of the same memory. The emulator runs no shaders:
on a PC, `cd shader && cargo test` runs the same programs through a
simulator of the generated code that also checks the GPU's latency and
scoreboard rules.

`Graphics::render_target` makes an image of a game's own to draw on and
then sample (`RenderTarget::texture`): RGBA8, R32F (shadow maps), RGBA16F
(HDR) or RGBA32F, with a depth buffer if asked. The service attaches the
same memory as a target and as a texture (`TARGET_ATTACH`,
`TEXTURE_ATTACH` with the same format and block height); `Frame::target`
switches to it and back (`Frame::screen`), waits for the pass before and
drops the texture caches, so the next pass samples what was drawn.
`Graphics::texture_mipped` makes the chain of mip levels on the CPU (a
2 x 2 average per level, in the block-linear layout of `abi`'s
`gpu_layout.rs`) and the sampler blends two levels, trilinear; a shader
may also pick the level (`sample_lod`).

`Frame::draw_instanced` draws a mesh once per instance (up to 4096): the
shader sees `InstanceId` and the pipeline's instance attributes
(`PipelineDesc::instance_stride`, `instance_attributes`) advance through
a buffer one instance at a time; the service issues one `BEGIN` per
instance, the way Mesa's nvc0 driver does. `Graphics::fullscreen` is a
triangle that covers the target with a stock vertex shader (texture
coordinates as varying 0), for post-processing passes
(`Frame::draw_fullscreen`). None of this has run on the console's GPU yet
(the emulator draws nothing; `gpu irtest` checks the instanced draw there).

### Raytracing and Lumen

The GPU has no ray-tracing hardware, so rays are a compute shader's job.
`nxk_shader::rt` builds a four-wide BVH on the CPU (binned SAH; 40,000
triangles in about 35 ms on the PC), and two traversal kernels written in
the IR (closest hit and any hit, the stack in the thread's local memory)
answer buffers of rays; `Graphics::rt_scene` uploads a scene and
`Frame::trace` records a query. A game shoots a few rays per pixel at
reduced resolution (shadows, ambient occlusion) next to its raster passes.
On the Erista (768 MHz), per 65,536 rays: random rays through 4,000
triangles 42 ms (closest hit) and 27 ms (any hit), about 1.5 and 2.4
million rays per second; in a valley of 41,000 triangles (a height field
and 1,000 trees, the size of Lumen's) a camera's 256 x 256 rays take 9.8 ms
(6.7 Mrays/s) and the sun's shadow rays from what it sees 4.9 ms (13.5
Mrays/s). Rays that go the same way in a warp cost about 2.3 times less
than the same rays in random order (10.8 ms for the shadows shuffled), so a
game shoots them in screen order, at reduced resolution.

`jeux/lumen` is the game built on all of this: a valley from dawn to
night to run through, third person (left stick, right stick for the
camera, A to jump, R to run), a sun and sky that follow the time of day,
24 lights to collect, all drawn by its own IR shaders. Its **sun shadows
are ray traced**: a pre-pass draws the world positions of what the camera
sees at a quarter of the resolution into an RGBA32F target, an 8 x 4
compute kernel (`nxk_shader::rt::shadow`) shoots one any-hit ray per texel
toward the sun over the valley's BVH (built once at load), and the lit
shader filters the mask per pixel with a joint bilateral upsample
(distance weights that follow the slope). The valley's share accumulates
over frames (history reprojected with the last camera from the four
texels around the point, weighted by nearness and by distance agreement;
a sun disc of 1.5 degrees jittered per texel and frame), which gives the
edges penumbrae;
half the texels trace each frame, plus every texel whose point did not
trace the frame before (the checkerboard is the screen's, so a point
moving an odd number of texels a frame would otherwise never trace and
drag its old shadow along). The six boxes of the moving figure keep no
history: four rays across the sun's disc test them every frame near its
bounding sphere, from 2 cm off the surface (an arm shades the body), so
its shadow follows it without trail. At 1280 x 720
it holds 60 fps; the GPU is busy 52% without the shadows and 72-81% with
them (pre-pass 1 ms, rays about 1.2 ms with half the texels, mask lookup
0.8 ms, history from four texels 0.7 ms); the CPU's frame is 0.2 ms. The
render targets of a game that draws upright on the panel are 720 wide and
1280 high (`Graphics::panel` turns the picture a quarter), so a mask for
the screen is too. L + R + Minus opens its hidden **Lab**: frame-time
strip and counters, a draw-call stress room, a particle room (additive
blending from a buffer rewritten each frame), a ray room that traces a
96 x 54 grid of camera rays and their shadow rays into the valley's 41,000
triangles and shows them as a heat map, and, on the Overview, Up / Down
steps through the shadows' cost (all, no mask in the lit shader, positions
only, none), shows the mask itself (distance error, filter weight, factor)
and traces with the history forgotten every frame.

### Users and save data

The console has users. On the first start `display` runs a setup (name
on a touch and gamepad keyboard, a pixel-art face and color, the theme,
an optional 4-digit PIN); later ones come from the Users page (the avatar
on the home screen, or System Settings, Users). Each user is
`/users/ID/` on the card: `profile.cfg`, `playtime.cfg` and `saves/GAME/`.
`/users/last` remembers who played last: the console starts on them (or
on their PIN lock). The Users page switches, renames, changes the look
or PIN of, and deletes users, lists what each game keeps for the user
and erases it per game. The PIN is a hash kept on the card, not
encryption: it only keeps others out of a profile from the screen.

A game keeps everything it saves in the directory `display` hands it as
`NXK_SAVE_DIR` (`/users/ID/saves/GAME`, created before the start);
`NXK_USER` and `NXK_USER_ID` give the user's name and id. Never write
under `/games`: `jeux/starlane` keeps its best score in its save directory.

### Services: game catalogs

The home screen shows one catalog of games at a time ("services" on
screen). **Intern** is the console's own: the games of `/games`, built
into `display`. Every other one is a program on the card,
`/services/NAME/NAME`, with `/services/NAME/service.txt` (`title=`,
`color=`, `description=`): it lists its games and their covers, says how
to launch them, and takes the screen as a game does. L and R (or a tap on
the tabs at the top) go from one catalog to the next; the choice is the
user's (`service=` in `profile.cfg`).

```bash
./tools/build.sh && ./tools/mksd.py -o build/sd.img   # puts services/*/ in /services
./tools/usbterm.py --put ~/.cache/nxk/target-apps/aarch64-unknown-nxk/release/demo:/services/demo/demo
```

`display` starts the catalog picked, for the user playing, with
`NXK_PROVIDER_KEY` (a random key), `NXK_SERVICE_DIR`
(`/users/ID/services/NAME`, its own data: accounts, caches; FAT keeps no
permissions, so a token there is readable by anyone with the card),
`NXK_USER` and `NXK_USER_ID`. The program hands its endpoint back with
`display::PROVIDER` and the key, then serves `abi::proto::provider`:
`STATE` (loading, ready, sign-in needed with the code and URL, launching
with the place in a queue, playing, error; held until it changes),
`GAME` and `COVER` (square RGB565, up to 256x256, read for the games near
the selection only), `LAUNCH`, `STOP`, and `SUSPEND`/`RESUME` around
sleep. Two threads of `display` make these calls with timeouts, and every
reply is checked, so a slow or broken catalog never holds the UI. While a
game starts, a card shows its cover, the catalog's words, the place in the
queue and the seconds waited; B cancels (`STOP`). HOME over a catalog's
game sends `STOP` rather than killing it (a cloud session must be ended),
and kills it only past `STOP_NS` (8 s). A catalog that exits is shown
stopped and started again after 2 s, then longer when it keeps failing.
Play time counts per game as `NAME~ID`.

`services/nxsvc` is the SDK: `connect()` says hello and serves the
protocol on a thread; the program fills a `Catalog` (`set_games`,
`set_cover`, `set_state`) and answers `Request`s (`Launch`, `Stop`,
`Suspend`, `Resume`); `nxsvc::screen` fills `SCANOUT`'s images from the
CPU (their handles carry `WRITE`) for screens that need no GPU.
`services/demo` lists four games with no network, waits in a pretend
queue, shows a game as its color, and has "Crash", whose catalog exits
while launching: it checks this whole path in the emulator
(`nxtest.py --dbg 'button R shot a.png button A ...'`).

## Build, run, test

Prerequisites: stable Rust with `rust-src`
(`rustup component add rust-src`; targets `aarch64-unknown-none` and
`aarch64-unknown-none-softfloat`), the compiled `tegra_qemu` emulator,
`mkfs.fat` and `pyfatfs` for the SD-card image.

```bash
./tools/build.sh                  # std toolchain, programs, boot image, kernel
./tools/mksd.py -o build/sd.img   # FAT32 SD card (programs in /bin)
./tools/run.sh                    # boots; shell on the terminal (Ctrl-A X to quit)
./tools/nxtest.py -c ktest -c stdtest --sd build/sd.img   # automated tests
```

`nxtest.py` options: `--sd IMAGE` (SD card), `--net [REDIRECTIONS]`
(USB network card, e.g. `tcp::8080-:80`), `--usb-kbd` and `--keys` (USB
keyboard), `--machine tegrax1plus` (Mariko), `--monitor ELF --dtb DTB`
(boot in EL2 under a PSCI monitor), `--screendump` (screen capture),
`--usb-cmd CMD` (CMD typed on the USB-C console, the script acting as
the PC), `--usb-put LOCAL:REMOTE` (file copied to the SD card over that
console first), `--usb-net share|quiet` (with `--usb-cmd`: the PC also
turns the USB-C network on and shares its connection at 10.42.0.1, or
answers nothing), `--tap X,Y[:OUT.ppm]` (touch the screen at landscape
pixel X,Y after the commands and the `[input] touchscreen` line, then capture it), `--dbg COMMANDS` (the debug channel's `tools/nxdbg.py` commands over the USB-C console, e.g. `"shot a.png tap 640 670 button R"`). On a real Switch, `tools/usbterm.py --reload DRIVER`
replaces a running boot driver by a new build without touching the SD
card (`hwboot/README.md`, "Driver updates over USB-C").

`--usb-cmd` needs the USB device mode of the OTG controller in
`tegra_qemu`, which upstream lacks:
`git -C ../tegra_qemu apply ../nxk/tools/tegra_qemu-usb-otg.patch`, then
rebuild it. QEMU exposes the bus on chardev `usbotg`, where
`tools/usbterm.py --emu SOCKET` plays the PC's USB host. `--tap` needs
the touch panel's interrupt line, wired to GPIO X1 as on the Switch:
`git -C ../tegra_qemu apply ../nxk/tools/tegra_qemu-touch-irq.patch`
(after the OTG patch). `gpu on` gets past the GPU's secure boot only with
GM20B's unit counts in the emulator's GPU model:
`git -C ../tegra_qemu apply ../nxk/tools/tegra_qemu-gpu.patch`; the GPU
firmware goes on the image with `tools/gm20b-fw.sh` then
`tools/mksd.py -o build/sd.img --firmware firmware`. Under `nxk-bl31`
(`hwboot/test-bl31.sh`, `NXTEST_QEMU_ARGS` below) the monitor's MiB stays
out of the kernel's free memory only if the emulated MC keeps the TX1
`MC_SECURITY_CFG0/1` offsets (its model has Tegra 2's):
`git -C ../tegra_qemu apply ../nxk/tools/tegra_qemu-mc-tz.patch`. Without
it the first PSCI call after boot (`cores`, `reboot`) finds the monitor
overwritten.

Build directories live in `~/.cache/nxk/`: the project is synced and
does not have to carry build artifacts.

## Writing a program

A program is an ordinary Rust crate in `apps/`, added to
`apps/Cargo.toml` and to the `apps` list in `tools/build.sh`:

```rust
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

fn main() -> std::io::Result<()> {
    let l = TcpListener::bind("0.0.0.0:8080")?;
    for s in l.incoming() {
        let mut s = s?;
        thread::spawn(move || {
            let mut req = [0u8; 1024];
            let _ = s.read(&mut req);
            let _ = s.write_all(b"HTTP/1.0 200 OK\r\n\r\nhello from nxk\n");
        });
    }
    Ok(())
}
```

`tools/mkstd.py` prepares the `nxk` toolchain: a copy of stable Rust
(hard links) whose standard library receives the `std/library` port,
the `std/aarch64-unknown-nxk.json` target and its rustup declaration.
`build.sh` then compiles `apps/`; by hand:

```bash
cd apps && RUSTC_BOOTSTRAP=1 CARGO_TARGET_DIR=~/.cache/nxk/target-apps cargo build --release
```

(`RUSTC_BOOTSTRAP` for `-Z build-std`, see `apps/.cargo/config.toml`;
this is the build directory whose `std` `mkstd.py` invalidates when the
port changes.) Programs start from the boot image (by their name) or
from the SD card (`/bin/NAME`, or a path).

### What `std` provides

| Area | Supported | Missing |
|---|---|---|
| Memory | `Box`, `Vec`, collections; 36-size-class allocator (O(1), header-free), large blocks returned to the kernel, recycled addresses | |
| Threads | `spawn`, `scope`, names, `join`, `park`, `sleep`, `available_parallelism`, `thread_local!` with destructors; guarded stacks for later threads; `std::os::nxk::thread` (priority, core) | |
| Synchronization | `Mutex`, `RwLock`, `Condvar`, `Once`, `OnceLock`, `Barrier`, `mpsc` (kernel futex) | |
| Time | `Instant` with no system call, `SystemTime` (real-time clock) | |
| Environment | `args`, variables, current directory (`PWD`), `consts::OS = "nxk"`, `getrandom` for `HashMap` | `current_exe` approximated |
| Files | FAT32: read, write, append, `seek`, directories, `rename`, `copy`, `remove_dir_all`, `metadata` (size, date), `try_clone` | links, permissions, `set_times`, locks |
| Network | IPv4: `TcpStream`, `TcpListener`, `UdpSocket`, timeouts, non-blocking mode, `try_clone`, DNS resolution | IPv6, `peek`, multicast, loopback |
| Processes | `Command`: arguments, environment, directory; `spawn`, `status`, `output`, `wait`, `try_wait`, `kill`, `id`; pipes (`piped`, `io::pipe`, `a \| b` with no relay), `null` | redirection to a file |
| Standard I/O | console or pipes, `IsTerminal` | |
| Panics | message on standard error, abort (`panic=abort`) | unwinding, backtraces |

## Layout

```
abi/        nxk-abi: syscall numbers, errors, rights, service protocols
            (shared by the kernel, libnx and std: single source of truth)
kernel/     the microkernel
shader/     nxk-shader: the shader IR games write GPU programs in, its
            validator, a CPU reference machine, and the compiler to
            Maxwell SASS (used by `gpu`; `cargo test` runs it on the PC)
user/       libnx (no_std system library), init, shell, drivers and services
std/        Rust standard library port, linker script, target
apps/       ordinary Rust (std) programs: hello-std, stdtest
jeux/       games (std) and nxgfx, their SDK: Starlane, a cube
services/   game catalogs (std) and nxsvc, their SDK: a demo catalog
bench/      comparison against L4T: EL3 monitor, Linux measurement program, scripts
tools/      build, SD card, std toolchain, launch, automated tests,
            usbterm.py (USB-C console terminal on the PC),
            nxdbg.py (screen capture, touch and gamepad over USB-C)
hwboot/     physical Switch bring-up: RCM injector, hekate entry, nxk-bl31 EL3 monitor
refs/       hekate, Atmosphere: hardware documentation (never compiled)
docs/logo/  pixel-art logo (regenerate with tools/mklogo.py)
```

About 8,000 lines of Rust for the kernel, 7,000 for drivers, services
and libnx, 3,800 for the `std` port.

## System calls

Linux arm64 conventions: `svc #0`, number in `x8`, arguments in
`x0`–`x5`, result in `x0`, errors as `-errno` (Linux values). Blocking
calls take a timeout (ns; 0 = unbounded).

| Family | Calls |
|---|---|
| Base | `nop`, `yield`, `sleep`, `clock` (monotonic, real-time), `clock_set`, `getcpu`, `getpid`, `gettid`, `getrandom`, `debug_write`, `thread_exit`, `process_exit`, `system_power` (reboot, off, CPU cores on/off under a PSCI monitor) |
| Handles | `handle_close`, `handle_dup` (reduced rights, badge), `handle_revoke` |
| Memory | `mem_map` (on demand or populated, free or fixed address, W^X), `mem_unmap`, `mem_protect`, `shm_create`/`shm_map`/`shm_paddr` (contiguous, uncached, below 4 GiB for DMA), `mmio_map` (read-only without the `WRITE` right), `mmio_derive` |
| Threads | `thread_create`, `thread_set` (priority, core), `futex_wait`, `futex_wake` |
| Processes | `process_spawn` (static-PIE ELF, arguments, environment), `process_wait`, `process_kill` |
| IPC | `endpoint_create` (pipe option), `ipc_call`, `ipc_send`, `ipc_recv`, `ipc_reply`, `ipc_reply_recv`, `ipc_reply_to` |
| Notifications, IRQ | `notify_create`, `notify_signal`, `notify_wait`, `notify_bind`, `irq_get`, `irq_bind`, `irq_ack` |
| Inspection, debugging | `sysinfo` (processes, threads with their state, core and deadline, memory, cores with their timer-interrupt lateness, wake-ups for nothing, nearest deadline and timer), `klog_read`, `klog_bind` (kernel log, console only), `debug_attach`, `debug_regs`, `debug_mem`, `debug_threads`, `debug_stop` |

Time is read with no system call: the kernel opens `CNTVCT_EL0` to EL0
(like the Linux vDSO).

## Kernel design

**One kernel stack per core, not per thread** (seL4 model). Registers
are stored directly in the thread control block; the kernel never blocks
in the middle of a call, and never waits on a device: its messages go
to an 8 KiB log ring that the `console` service drains (`klog_bind`,
`klog_read`); only the boot log, before that service starts, and panics
are printed directly.

**One global lock**, as in multiprocessor seL4: kernel paths last a few
microseconds. Calls with no shared state (`clock`, `getcpu`, `gettid`)
take a lock-free fast path. Long work (zeroing a large shared-memory
object or populated mapping, unmapping megabytes) lets the lock go every
64 pages, so other cores wait at most one such batch.

**Capabilities**: every object (endpoint, thread, process,
notification, shared memory, interrupt line, register range) is named by
a handle carrying rights and a generation; revocation by generation
change. Growable object tables.

**Synchronous IPC**: 6 words in registers, up to 4 KiB via each
thread's control page (UTCB), up to 4 handles transferred; the kernel
attaches the sender's badge and pid to every message. Client and server
on the same core: direct switch with no scheduler. Timeouts on all
blocking calls. A "pipe" endpoint signals end of stream when its last
writer disappears.

**Tickless scheduler**: 32 priorities, O(1) selection, one queue per
core; the timer is programmed for the next deadline (wakeup, or end of
quantum only if a thread of the same priority is waiting). Balancing
toward idle cores, migration, adjustable affinity and priority. A core
with no work gets no periodic interrupt: `ktest` counts the interrupts
that leave a core idle (`sysinfo`), not the ones services cause by
polling (statistics each second, the fan's temperature every 2 s).

**Memory**: buddy allocator (4 KiB–4 MiB); pages zeroed with `DC ZVA`;
ASID-tagged address spaces; on-first-access anonymous pages and stacks
(including multi-core races); enforced W^X; ASLR (static-PIE executables
relocated by the loader, heap, stacks, registers); reserved areas
(memory-controller carveouts, `/reserved-memory` from the device tree)
excluded.

**Security**: Spectre v2 mitigation (predictor invalidated on process
switch), W^X, ASLR, fault isolation (a faulty program is terminated,
the rest keeps running).

**Debugging**: a debugger (endpoint) receives a process's faults,
breakpoints and single-steps, reads and writes registers and memory,
stops threads.

**Boot**: directly from reset (EL3), or in EL2 under a PSCI monitor
(as on a Switch with its secure firmware); DRAM size detected; device
tree read when provided. On a Switch, hekate's L4T entry loads
`hwboot/nxk-bl31`, an EL3 monitor doing ATF's job, which enters nxk in
EL2 and starts the other cores through PSCI `CPU_ON`; `CPU_OFF` powers
cores 1-3 down again (`cores MASK` in the shell, `display` at sleep), the
dying core handing its threads, deadlines, FP state and interrupt lines to
core 0 first. If the loader left the screen on
(hekate), the kernel log is copied there until the `display` service
takes over: the only visible output without a UART adapter.

## Services and drivers

All in EL0, started by `init` with only their capabilities.

| Service | Role | State |
|---|---|---|
| `console` | UART-A, line editing; mirrored to a terminal driver (input and all output). Output never waits: one 16 KiB ring, drained to UART-A by its TX-empty interrupt and read by the terminal driver at its own pace; the kernel log is drained into it. The screen reads the same ring without taking from it (`TAIL`, screen badge). Only the shell's handle (session badge, inherited by the programs it starts) writes the session: every other write and the kernel log come between `LOG_START`/`LOG_END` marks (private OSC 7700), so readers tell logs from commands and their results | emulator; on a Switch, UART-A pads are internal (not on the Joy-Con rail) |
| `usbcon` | USB-C port as a composite USB device (1209:0001) on the Erista OTG controller: a serial console (CDC-ACM), the terminal of a PC (`tools/usbterm.py`), and a network function (CDC-ECM) published as link `usb0`, over which the PC may share its connection (`hwboot/README.md`, "Internet over USB-C"). One thread: console output is pulled into the DMA buffer when the IN endpoint is free, 100 ms after the PC opens the terminal (its tty echoes until it is raw); frames queued each way, the carrier given by the host's choice of the data interface's alternate setting; traffic counters and state for the screen (`STATS`); a debug channel inside the serial stream (APC frames `ESC _ N base64 ESC \\`, taken out of the shell's input): screen capture, touch and gamepad injection for `tools/nxdbg.py` (`hwboot/README.md`, "Debug channel") | emulator (virtual host: console, network with DHCP and ping via `nxtest.py --usb-net`); **Erista**: the way in to the shell; network to validate; Mariko has no such controller (stops) |
| `clk` | clocks and resets (CAR), USB2 PLLs, power-partition state (PMC); the PWM block is never reset nor stopped once running (channel 0 dims the Erista backlight) | emulator |
| `i2c` | six I2C buses; a write and a read joined by a repeated start on request (`REPEATED_START`, packet mode) | emulator |
| `power` | power off and reboot (MAX77620 PMIC), real-time clock, temperatures (TMP451, started again as hekate's `tmp451_init`: Nyx shuts it down, freezing its last reading; Mariko's -12.5 C offset), SD, PCIe and touchscreen rails (LDO2, LDO1, LDO6), the charger's input status (BQ24193 power good: battery or external power, charge state) and the battery fuel gauge (MAX17050 on I2C1, as hekate's `bdk/power/max17050.c`: charge %, cell voltage, current, temperature, capacity, age, cycles); `MEASURE`: the system's draw from the battery, the charger's input cut (BQ24193 HIZ, its 40 s I2C watchdog as the safety net) and the gauge read at 1 Hz (`/bin/powertest start N [SETTLE]`, `result`, `wait`) | emulator; **Erista**: charger read on USB power, fuel gauge to validate |
| `fan` | the fan as hekate's `bdk/thermal/fan.c` drives it: PWM channel 1 (inverted duty, 236 steps), pad V4, the battery's 5V regulator (GPIO A5, also the Joy-Con rails' source, which stays on while `joycon` asks (`RAIL`, one bit per rail); Erista's USB VBUS source, CC4, kept off). Every 2 s from the SoC temperature (`power`, TMP451 remote): Nyx's steps (off up to 39 C, 22% from 42 C, 32% from 47 C, 43% from 52 C), full speed from 60 C until below 57 C; `SET` puts a duty by hand, still overridden when hot; a sensor lost for 3 readings gives at least 43%. Speed from the tachometer (GPIO S7, two pulses a turn) counted on the GPIO bank 4 interrupt (SPI 55) for 1 s: on `MEASURE`, 3 s after each change, then every minute. Nyx stops the fan before chaining to nxk; SUSPEND: fan off, the 5V rail off without users, the SoC checked every 8 s (full speed when hot) | emulator (no PWM output nor tachometer there: 0 rpm); **Erista**: to validate |
| `sd` | SD card (SDHCI, ADMA2 straight into `fs`'s uncached, contiguous buffer: no bounce copy), slot power cycle (239 ms off, as hekate) then power-up (pads, GPIOs, PMC I/O power, PMIC LDO2): a card hekate left in 1.8 V signaling starts over at 3.3 V. Block 0 is read before publishing; a card that answers commands but sends no data is reported (host, pads, rails, CMD13 state) and retried: first on a 1-bit bus in place (ACMD6, card kept powered: a slot that carries DAT0 alone, a loose connector, `hwboot/README.md` runs 19 and 20), then after a 2 s power cycle, on a 1-bit bus after a 239 ms one, last at 1.8 V; the ready line gives the bus width; **Erista**: 64 GB card read and written | 3.3 V default speed; the 1-bit fallback (about 3 MB/s at 25 MHz) not yet run on hardware |
| `fs` | FAT32 (long names), write-back cache with readahead | emulator |
| `cpufreq` | the CPU clock at sys-clk's steps (612 to 1785 MHz) on PLLX, parked on PLLP while PLLX relocks (the PLLP_OUT_CPU branch opened first, CCLKG switched, PLLX turned off only once a timed loop shows the CPU slowed down); the CPU rail (MAX77621, I2C5) at the voltage NVIDIA's L4T gives a PLLX-sourced CPU (`tegra210-dvfs.c`, CPU_PLL_CVB_TABLE_ODN: the chip's CPU speedo from the fuses, +30% margin, 950 to 1225 mV), raised before a faster clock and lowered after a slower one. Steps above 1225 mV are refused (ERANGE): about 1122 to 1224 MHz on an Erista, the rest needs the DFLL (not written). Every boot keeps the loader's clock (hekate: 998.4 MHz, 950 mV), and the service goes back to it when the SoC reaches 80 C. Erista Switch (SKU 0x83) only; the pure part (`dvfs.rs`) has host tests | emulator (sequence checked with fake fuses, not committed; no fuses there: boot clock only); **Erista**: to validate |
| `emc` | the DRAM clock: switches the EMC between the rates hekate's Minerva trained (204 / 408 / 800 / 1331 / 1600 MHz; tables copied to DRAM at 0xFFE80000, read-only through a window `init` derives from the kernel's firmware-tables capability) with a Rust port of Minerva's switch sequence (`emc/`, crate `nxk-emc`: CCFIFO-queued pad ramp, DLL prelock, FSP flip, clock change, MC latency-allowance reload; training and periodic compensation are not ported, an entry needing them is refused). Before each SET the 33 MC burst registers are compared with the running entry (a table that does not describe the running DRAM is refused), the sequence is run once on a register file that touches nothing (code and data resident), then runs at top priority. `emc [MHZ]` in the shell; `emc sleep on` lets `display`'s sleep drop the DRAM to 204 MHz after everything else is off and restore it first at wake. The DRAM voltage is never touched. The port is checked on the PC against hekate's C code: the register-access traces are identical for 240 random tables and for every pair of hekate's own odinx02 entries (`emc/oracle`, a scratch build of hekate's module with a recording bus; `cd emc && cargo test`) | emulator (plumbing only: its EMC is a plain register store); **Erista**: to validate |
| `display` | screen (window A) as a Switch 2-style touch and gamepad UI with runtime **Basic Black / Basic White** theme, PWM0 backlight brightness (`0x7000_A000`), screen-off sleep mode (backlight off, window A kept scanning out, CPU at minimum step, idle timer), and settings persisted in `/config/display.cfg`; **users** (first-start setup with animated steps, per-user theme, PIN lock, **USERS** page, each game's save data in `/users/ID/saves/GAME` as `NXK_SAVE_DIR`): player avatar on the left and status cluster on the right (clock, Wi-Fi, battery level and percentage with a bolt on input power), **HOME** (the games of `/games`, last played first, as a horizontal row of 256x256 square icons with a cyan selection frame and the selected game's title and play time above it counted per user in `/users/ID/playtime.cfg`, then the centered row of round buttons underneath: System, Network, Terminal, Logs, Controllers, System Settings and Power), **CONTROLLERS** (the rails' Joy-Cons and the Bluetooth controllers with their state and battery, Pair a Controller, Forget; a Test tab drawing each connected controller's buttons and sticks live from the events' source word, HOME to leave), the left stick moving through the menus like the D-pad (past 55% of its travel, released under 35%, repeating while held; games still get it raw), the quick settings drawer (`+`/`-`: Sleep Mode, brightness, volume, Flight Mode, System Settings; automatic brightness shown, not controlled yet), **over a game** window D (B if D fails the output-CRC check made at start; premultiplied alpha, only the drawn area fetched, taken at the vertical blank like window A) blends the volume indicator, toasts and the drawer (`+`, kept from the game; without System Settings) on top of the game's images, the game's `FLIP` and `EVENT` served by their own thread on the endpoint `SCANOUT` hands out, so a redraw never costs the game a frame; Sleep Mode (drawer or a short Power press) holds the game in its next `FLIP` until the wake, **SYSTEM SETTINGS** (Display, Sound, Internet, Controllers (Bluetooth pairing, paired controllers with their battery, Forget), Performance, Power, Battery, System), **SYSTEM** (Overview and Details: per-core CPU and GPU load, clocks and rails, syscall/IRQ/switch rates, memory, temperatures, fan auto/manual sheet, CPU/GPU clock step sheets), **NETWORK** (active link, Ethernet, Wi-Fi scan/retry/leave), **TERMINAL** (USB-C console in Terminus font) and **LOGS**. Text in Lato (OFL, `tools/mkuifont.py` 4-bit antialiased atlas, Noto Sans when missing), art drawn at build time (`tools/mkcovers.py`: avatar, round buttons and header glyphs built in, each game's square `cover.bin` on the card), into RGB565 cached back/shown buffers widened to 32-bit only for changed spans; `SNAPSHOT` hands out the displayed image read-only; **sleep** (Power or the drawer, a game too): the game waits in its next `FLIP` and its clock (`FLIP`'s reply, `nxgfx` `seconds`) leaves the sleep out, the services get `proto::SUSPEND` (Joy-Con, fan, touch, audio, Bluetooth, Wi-Fi) and `RESUME` on the wake, CPU and GPU at their lowest step, window A stops fetching, the display path goes off (`panel/`, hekate's `display_end`: panel off by its own deinit sequence for its id, sleep in, reset low, AVDD off, DSI and the controller stopped, PLLD, the clocks and LDO0 off; granted by init: CAR, GPIO, pinmux, MIPI CAL, APB_MISC) and comes back from reset at the wake (`display_init` in two phases around the panel's sleep out, so the GPU resume fits in its wait; the windows replayed from a shadow, `dcgate.rs`, which also keeps a stopped controller from being touched); without those grants or on another chip the older DCS sleep stays (display off, sleep in, sleep out, display on); the picture is back 443 ms after the Power press (223 ms with the older DCS-only sleep, runs 48, 49), the services 560 ms, the radios a few seconds later; -0.14 W asleep against the DCS-only sleep (run 53) | emulator (touch via `--tap`); **Erista**: takes over the 720x1280 panel hekate left on; touch and Joy-Con work, the Controllers page's Test tab with a rail Joy-Con and a Bluetooth Pro Controller (run 37); own panel bring-up and power-down run (run 53: InnoLux panel, CRC and 57 registers equal after the wake); JDI and AUO deinit tables untested |
| `input` | touchscreen (FTM3BD56) powered up and started as hekate does (reset pin J7, LDO6 through `power`, system reset, finger mode; Nyx leaves it off), read with repeated starts when its interrupt line says it has events (GPIO X1, level low, as in the Switch's Linux device tree; bank 6 interrupt, SPI 87), polled if the line is missing or stuck; each reader asks for the event kinds it wants (`READ` word 1); USB keyboard; button events (gamepad, `BTN_*`) and stick events (`KIND_STICK`) injected by other drivers, each event carrying its source controller (word 5: `SRC_RAIL_L`/`SRC_RAIL_R`, `SRC_BT` with the address, 0 for the rest): `usbcon`'s debug channel, the volume buttons (`BTN_VOL_UP` and `BTN_VOL_DOWN`, repeating while held; `display` takes them for the speaker volume and its on-screen indicator, both at once are `BTN_A`), the Power button (PMIC ONOFFSTAT polled every 100 ms: `BTN_POWER` on release of a short press, `BTN_POWER_LONG` after 3 s held; `display` sleeps on the first and opens the Power sheet on the second), `joycon` and `bluetooth`; SUSPEND: touch off (J7 low, LDO6 off, its interrupt masked), the Power button still polled; RESUME starts it again (its interrupt then reads stuck low: polled) | emulator (with the touch patch); **Erista**: power-up and interrupt pin to confirm (hekate polls) |
| `joycon` | the Joy-Cons on the console's rails (handheld mode) as hekate's `bdk/input/joycon.c` drives them: UART-C (left) and UART-B (right), attached when GPIO E6 / H6 read low (the TX pads turned into inputs for the read), 5V rail through `fan` (`RAIL`), UART clocks through `clk` (`UART_BAUD`: 1 Mbaud for the handshake, 3 Mbaud after). Wake bytes and handshake, controller info, 3 Mbaud, HID connection, 15 ms reports, a short rumble pulse and the charger held at supply only (no Joy-Con is charged), then an input report asked every 15 ms (RTS held off the Joy-Con but while reading, 36-byte FIFO, the parsed 24 bytes first). Buttons become `KIND_BUTTON` events (D-pad as UP/DOWN/LEFT/RIGHT, SL / SR not mapped), sticks `KIND_STICK` (-32767..32767, centre taken from the first steady reports, dead zone 170 of a ~1150 range, not the unit's factory calibration; `libnx::jc`, shared with `bluetooth`). Attach, detach, battery and a report count every 600 polls are logged; a held button is released on detach or after 1.8 s of silence. Published as `joycon`: `STATUS` gives each rail's state and battery, answered between polls; SUSPEND: polling and handshakes stop, the 5V rail is released, the UART pads parked and their clocks off; RESUME handshakes again | emulator: the rail lines read as attached and no Joy-Con answers (logs `no handshake`); **Erista**: left Joy-Con works (run 32); right one not detected |
| `bluetooth` | Bluetooth controllers, published as `bt`: the BCM4356's Bluetooth half on UART-D (HCI over H4, 8N1 with RTS/CTS by the UART, receive interrupt SPI 90), BT_RST on GPIO H4 (the same pins on every board, Atmosphere's pad tables); power cycle, reset at 115200 baud (a Hardware Error, as after an Erista cold boot, power-cycles the core again after 1, 2, 4 ... 30 s), 3 Mbaud (`0xFC18`), the patch RAM download from `/firmware/brcm/BCM4356A3.hcd` (Linux `btbcm.c`; the ROM firmware when absent), the adapter's address (random, kept in `/config/bt.cfg`: CAL0 is out of reach). Host: inquiry while pairing (`bt pair`, or Pair a Controller in System Settings, Controllers: 30 s window, gamepad class of device or a controller's name), Secure Simple Pairing (Just Works, link keys in `/config/bt.cfg`; legacy PIN "0000"), encryption, L2CAP basic mode (signaling, HID control 0x11 and interrupt 0x13, either side opening them), then the Joy-Con / Pro Controller protocol: device info, full reports (0x30), player 1 light. A paired controller pages the console on a button press: the connection is accepted, its key given back, its channels taken. Reports go through `libnx::jc` (shared with `joycon`): buttons and sticks to `input`, a Joy-Con (L) and (R) together make one gamepad, a Pro Controller one by itself; all controllers merge into the one gamepad stream. `bt status`, `bt list`, `bt forget ADDRESS\|all`, `bt retry` in the shell; SUSPEND: links dropped (HCI Disconnect 0x15), scans off, the core held in reset, UART-D clock off; RESUME: the full bring-up in the background, a paired controller reconnects on a button press | emulator: a BCM4356 and a Pro Controller emulated on UART-D (`nxtest.py --bt`, `tools/btfake.py`): bring-up, Hardware Errors at reset (`--bt-hw-errors N`), patch download, pairing, reports, a reconnection with the saved key; without it `bt status` says no adapter answers. **Erista** (run 36): a Pro Controller pairs, reconnects on HOME and streams its buttons; the hardware also needed 115200 baud outside the 16x divider mode, BT_WAKE low, a name request before connecting and sniff mode at 15 ms. Not yet: Joy-Cons, several controllers, rumble, factory stick calibration, one gamepad per player |
| `usb` | xHCI: HID keyboard, CDC-ECM network card | emulator; the real xHCI needs an NVIDIA firmware; stops if `clk` does not report it powered |
| `net` | TCP/IP (smoltcp) over one link at a time: `usb0` (the PC's shared connection over USB-C), `eth0` (USB adapter), then `wlan0`, the first with a carrier, checked every 2 s; a wired link without a DHCP address for 10 s gives way until its carrier drops; a switch changes the MAC, starts DHCP over and aborts TCP connections. DHCP (retried every 2 s, started over when a link drops), DNS, TCP, UDP, ICMP echo (`nettest ping HOST`), traffic counters and the link in use (`STATS`); **Erista**: over Wi-Fi, `nettest ping google.com` answered | |
| `wlan` | Wi-Fi station: Tegra PCIe root complex (rail, UPHY lanes, AFI windows, link, BAR assignment) and BCM4356 identification, firmware boot (TCM download, NVRAM, shared area), msgbuf control and data rings (RX buffers, a TX flow ring) driven by the chip's interrupt (PCIe2 mailbox, legacy INTx), scans, joins of open and WPA2-Personal networks (CCMP, our own supplicant: 4-way and group key handshakes, keys handed to the firmware), radio on/off (`wifi radio on\|off`, `WLC_UP`/`WLC_DOWN` for Flight Mode); publishes the link as `wlan0` once associated (`net` takes it when there is no `eth0`: DHCP, DNS, TCP) and its control as `wifi` (`wifi status` names the step where bring-up stopped, `wifi retry` runs it again or resets a running chip; `wifi connect` returns once `net` has its DHCP address); SUSPEND: leaves the network, chip off (enable and reset low), PERST#, UPHY PLL and PCIe clocks off (LDO1 kept: `SUSPEND_RAIL_OFF`); RESUME: the boot bring-up in the background | the chip still resets itself at times, joined or idle (hardware runs 16, 20, 21, cause unknown): `wlan` checks it every second, reports the loss in `wifi status` and brings it back by itself, rejoining the last network. WPA (TKIP), WPA/WPA2 mixed, WPA3-only and networks requiring management frame protection are refused with `EOPNOTSUPP`. Without a PCIe device (emulator) a simulated adapter answers, so `wifi` and the `net` attach are still exercised |

The `fs` and `net` services attach each open file or socket to the
process that opened it: another process cannot use it, and the ones of a
program that vanished without closing them (crash) are reclaimed.

## Features

| Area | Done | Remaining |
|---|---|---|
| Boot | 4 cores, EL3 or EL2 + PSCI, Erista and Mariko, DRAM detection; real Erista via hekate + nxk-bl31 (4 cores, shell over USB-C) | Mariko hardware |
| Memory | on demand, W^X, `mprotect`, shared memory, DMA buffers, ASLR, reserved areas | |
| Threads | priorities, affinity, migration, balancing, tickless, futex | deep sleep states |
| IPC | synchronous, timeouts, handles, notifications, pipes, sender identity | wait on several objects (`poll`) |
| Programs | static-PIE ELF, arguments, environment, TLS, load from SD card | dynamic linking (ruled out) |
| Rust | `no_std` (libnx) and **`std`** (see "What `std` provides") | panic unwinding, backtraces |
| Storage | SD, FAT32, file API | eMMC, ext4 |
| Network | USB-Ethernet, Wi-Fi (BCM4356: scans, open and WPA2 joins, traffic on `wlan0`), TCP/IP, sockets | IPv6 |
| Bluetooth | BCM4356 on UART-D (patch RAM, 3 Mbaud), pairing (SSP), HID host for Joy-Cons and Pro Controllers, reconnection on a button press (a Pro Controller on an Erista) | Joy-Cons on hardware, rumble, motion sensors, players, Bluetooth audio, BLE |
| Hardware | UART, I2C, PMIC, RTC, temperatures, fan, CPU clock on PLLX, screen, touchscreen, USB host, USB device (serial console, Ethernet), speakers (codec, audio hub), GPU (secure boot, 2D engine, compute kernels, 3D engine with shaders, depth and textures, sys-clk's clock steps 76.8 to 921.6 MHz) | GPU mipmaps and float render targets (written, not yet run on the console), audio quality in games, the right Joy-Con (not detected on the Erista tried), Joy-Con charging, rumble, the Power and volume buttons (read, not yet checked by hand), CPU DFLL and other clocks (DVFS), battery |
| Security | capabilities, revocation, Spectre v2, isolation | |
| Tools | build, SD card, tests (`ktest` 29, `stdtest` 17, `wlan` crypto and WPA2 on the PC, an emulated Bluetooth controller), debugger (gdb), L4T comparison, USB-C terminal | |

## Limits

- **On a real Switch** (Erista, hekate L4T entry) nxk reaches the
  shell over the USB-C console (`usbcon`, `tools/usbterm.py`): UART-A
  pads are internal. Last hardware run: single core (before PSCI in
  nxk-bl31) and no SD card (see `hwboot/README.md`, "Hardware runs").
  A USB keyboard does not help: its keys go to `input`, not to the
  shell, and hekate's L4T path leaves the xHCI unpowered. Seen there:
  4 cores, SD card and filesystem, `ktest` 29/29, `stdtest` 17/17
  (`hwboot/README.md`, "Hardware runs").
- Kernel messages (`[nxk]` fault reports) reach the USB console through
  `console`, boot log included; before `console` starts and on a panic
  they go to UART-A and the loader's screen. Mariko lacks the USB2 OTG
  device controller (its OTG port belongs to XUSB): no USB console
  there.
- The real USB controller (xHCI) needs a proprietary NVIDIA firmware;
  the SD reader needs power and pin setup; without hekate the screen
  needs DSI panel init; buttons need GPIOs the emulator cannot drive.
- Wi-Fi: `wlan` powers the module (PMIC LDO1,
  WIFI_EN on PH0, WIFI_RST on PH1), sets the PEX and module pads as
  Horizon does, brings the PCIe root complex up, then should train the
  link, read the BCM4356 chip identity and boot the console's own
  firmware in it: on an Erista, `wifi status` reports `BCM4356 rev 3 on
  PCIe port 1, firmware up` with the firmware's console (`hwboot/README.md`,
  runs 5 to 14). Commands, scans (`wifi scan`) and joins go through
  Broadcom's msgbuf rings. The console's firmware has no supplicant
  (Horizon runs its own), so `wlan` has one for WPA2-Personal (`wpa.rs`,
  with SHA-1, PBKDF2, the 802.11 PRF and AES key unwrap in `crypto.rs`):
  this firmware reports received EAPOL frames as `E_EAPOL_MSG` events,
  the replies leave on the TX flow ring, and the keys go back with
  `wsec_key`. Their known-answer and handshake tests run on the PC:
  `rustc --edition 2021 --test user/wlan/hosttest.rs -o T && ./T`.
  Data frames use the RX and TX rings, drained on the chip's interrupt
  (a 500 ms poll stays as a safety net). The chip still resets itself
  at times, joined or idle, with no trap or assert from its firmware
  (hardware runs 16, 20, 21): `wlan` checks it every second (crash
  flags, a bus reading all ones, memory decoding off), reports the loss
  (`wifi status`: link, config space, module pins, the firmware's last
  console lines) and brings it back by itself (module reset under
  PERST#, link, firmware, then the last network with the passphrase it
  keeps), retrying after 2 s, then up to every minute; `wifi retry`
  does the same by hand. Its firmware
  and calibration data are proprietary and never in the repository:
  they are extracted from the console's own
  system titles with `hwboot/nxfw-extract.py` into `firmware/` (git
  ignores it), then copied to the card by `tools/mksd.py --firmware DIR`
  or `hwboot/mksd-hw.sh MOUNT DIR`, which put them in `/firmware/brcm`.
  The whole procedure is in `hwboot/README.md` ("Wi-Fi firmware"). The
  station MAC is a locally administered one: the real address lives in
  CAL0 on the eMMC, which nxk cannot read. None of the PCIe path runs
  in the emulator, which has no root complex to model: there a
  simulated adapter answers `wifi` instead.
- Bluetooth: `bluetooth` drives the BCM4356's Bluetooth core on UART-D
  as Linux `hci_bcm` does (BT_RST on PH4, RTS/CTS, 115200 then 3
  Mbaud) and loads the console's own patch file, `BCM4356A3.hcd`,
  extracted with `hwboot/nxfw-extract.py bluetooth` and copied with the
  Wi-Fi blobs to `/firmware/brcm`. Joy-Cons and Pro Controllers pair
  with their sync button (`bt pair`, or System Settings, Controllers)
  and reconnect on a button press; their keys stay in
  `/config/bt.cfg`. A Pro Controller pairs, reconnects and plays on an
  Erista (`hwboot/README.md`, run 36), which needed more than Linux's
  sequence: 115200 baud outside the CAR's 16x divider mode, BT_WAKE
  low, a name request before each connection, sniff mode at 15 ms.
  `nxtest.py --bt` replays that flow against an emulated chip and
  controller. Not done: rumble, motion sensors, factory stick calibration,
  one gamepad per player (all controllers merge into one), and Joy-Con
  pairing through the rails (Horizon writes the console's address and
  key into a Joy-Con attached there).
- GPU (GM20B): `gpu` powers it only when asked (`gpu on`, or a 2D or
  compute request): its rail (the MAX77621 at 0x1C, NVIDIA's voltage
  for 76.8 MHz plus 25 mV), clocks and clamps through `clk`, the GPCPLL
  in noise-aware mode at 76.8 MHz, then NVIDIA's secure boot:
  the ACR loader from linux-firmware runs on the PMU, copies FECS into
  the GPU firmware carveout hekate's L4T entry locks, and starts it;
  GPCCS, the golden context and one channel follow nouveau's GM20B code.
  Asleep, `display` has it suspended (`proto::SUSPEND`): rail off, its
  memory and the clients' state kept, programmed again at the wake.
  It runs 2D fills and copies on surfaces clients attach, three
  hand-assembled SM 5.3 kernels (fill, copy, SAXPY) and an FFMA
  benchmark (`gpu test`, `gpu bench`). `gpu clock` moves it among
  sys-clk's steps (76.8 to 921.6 MHz, 76.8 MHz apart) while it runs, as
  nouveau's `gm20b_clk_prog` does: the rail at NVIDIA's voltage for the
  step (`gm20b_na_cvb_coef`, the GPU speedo, plus 25 mV) raised first
  and lowered last, a safe PLL while the noise-aware detector moves,
  then an NDIV slide. Each change is measured (SM cycles against the
  CPU's clock; more than 5% off goes back to 76.8 MHz). sys-clk's caps
  apply from `power`'s charger status: 460.8 MHz on battery, 768 MHz on
  input power; 844.8 and 921.6 MHz need `force` (the official charger
  and the dock are not told from a PC port yet). Above 76.8 MHz the SoC
  reaching 80 C sends it back there. `gpu sweep` benchmarks each step,
  `bench full` loads the CPU and the GPU together. A stats page
  (`gpu::STATS`, shared read-only, changed under a sequence count)
  carries its state, clock settings, the steps this chip runs and its
  busy time (batches from doorbell to fence, the one in flight
  included), so the screen and `gpu load` follow it without waiting on a
  service busy with a batch; the screen's GPU panel overclocks past the
  cap once confirmed. Its firmware is NVIDIA's
  redistributable set (`tools/gm20b-fw.sh` copies it from the PC's
  `/lib/firmware`), kept out of the repository like the Wi-Fi blobs.
  Runs on an Erista Switch (`gpu test` 5/5 at every step, `gpu bench`
  30.4 GFLOP/s at 76.8 MHz to 365.8 at 921.6; `hwboot/README.md`, "GPU"
  and runs 26 and 27); Erista only (Mariko's GPU rail is on a
  MAX77812).
  The emulator's GPU model runs the whole power-up with
  `tools/tegra_qemu-gpu.patch` but renders and computes nothing:
  `gpu test` fails there by design.
- Audio: the `audio` service plays 16-bit stereo 48 kHz PCM on the Switch's
  speakers (ALC5639 on I2S1, ADMA ring, `clk::AUDIO`; `nxgfx::Audio` and
  `Sound` for games; `audio tone HZ MS LEVEL` in the shell). Asleep
  (`proto::SUSPEND`) the codec's blocks, the hub and the audio clocks are
  off, its LDO kept (cut, the codec missed its reset after it). Not done:
  headphones and jack detection, volume control, capture, mixing several
  clients, and a clean sound while a game runs (TASKS.md 3). Joy-Con
  charging and rumble, and frequency scaling are not done either (the last
  one risks damaging hardware).
- A pipe has no buffer: the writer waits until a reader takes its data.
  `output()` reads both outputs in parallel for this reason.
- The `fs` service refuses to delete a file that is still open
  (`EBUSY`), like Windows rather than Unix.

## Hardware traps

1. The GIC starts with all its lines in Group 0 (secure world): an EL1
   non-secure kernel receives nothing until they are switched to Group 1
   from EL3 — the role of the secure monitor on a real system.
2. Waking a secondary core requires, in order: reset vector in the SB
   block, power partition at the PMC, voltage-lock removal, CAR reset
   lines.
3. Several registers have an unknown reset value on silicon but a sane
   one under QEMU: CNTFRQ_EL0 and CPUECTLR_EL1.SMPEN (to set in EL3
   before caches and MMU), SCTLR_EL2 and MDCR_EL2 (the latter can trap
   EL1 debug-register accesses to EL2).
4. With the MMU off, memory is Device type: exclusive accesses of
   atomics (LDXR/STXR, no LSE on A57) are not guaranteed there. No lock
   before the MMU is enabled.
5. hekate (L4T entry) places memory-controller carveouts below TZ-DRAM
   (running BPMP firmware, GPU areas forbidden to the CPU): they are
   excluded from free memory like TZ-DRAM.
6. Reading a block with no clock, in reset, or whose PMC partition is
   off hangs the bus: the whole system stops when it has a single core.
   The emulator answers anyway. A partition-gated block driver (XUSB)
   therefore asks `clk` for `ENABLE` then `STATUS`, which never powers a
   partition on by itself, and only touches its registers once
   `ST_READY` is complete. Fuses too: `usbcon` reads its PHY trims only
   when `clk` reports the fuse block clocked and visible.
7. hekate turns the AHB memory prefetcher on for USB when it serves UMS
   and flushes it before every transfer; left on, a device re-reading a
   buffer the CPU rewrote can get stale data. The kernel turns it off at
   boot, so uncached `shm` buffers stay coherent for DMA.

8. GPU shaders: a vertex attribute slot declared inactive still needs the
   RGBA32 float format; a pixel shader with no input must still declare
   position.w in its header; a pitch-linear render target wants rows of at
   least 256 bytes; the hardware's sine is exact only near zero (the
   fuzzer of `gpu test` keeps arguments within six radians). The shader
   self-test of `gpu test` found each of them.

9. GPU memory: pages the GMMU maps volatile bypass L2, and a dependent load
   from them costs a DRAM round trip. A 16 KB table chased by 65,536
   threads took 56 ms volatile, 3 ms cached. `gpu` maps what a client
   attaches (buffers, textures) cached; targets, scanout images,
   semaphores, pushbuffers and the GPU's own structures stay volatile.
   That is coherent because every batch is preceded by an L2 invalidate and
   followed by an L2 flush; memory the CPU reads while a batch runs must
   stay volatile.

10. GPU loads read their sources late: a load queues behind the others and takes
    its address registers when its turn comes, so scratch registers rewritten
    right after it can corrupt it. With vector loads written in place (no copy
    to wait on), 33,480 of 65,536 rays came out wrong. Loads, local loads and
    atomics signal a read scoreboard, and a write to their sources waits on it;
    the simulator keeps their sources in use until the result is waited on.
    The multi-function unit and the conversions (`F2I`, `I2F`) do the same
    when their unit is busy: every variable-latency instruction now signals
    a read scoreboard. Before, a 4-cycle window let Lumen's shadow kernel
    get 18 to 103 of 4800 texels wrong in about half its launches; none in
    96 since. `gpu test` launches that kernel 8 times per case against the
    reference machine to catch the like.

11. The reconvergence stack (SSY / PBK entries) spills to local memory past
    its on-chip entries. QMDs and IR headers declare 0x800 bytes per warp
    for it and `gpu` reserves them, as NVK and nouveau do; no failure was
    seen here without it (the shadow kernel was clean in 96 launches with
    a CRS size of 0), but nothing bounds the nesting depth either.

## L4T comparison

See `bench/README.md` and `bench/RESULTS.md`. In the emulator, creating
then waiting for a thread costs about 300 µs with `std` on nxk (226 µs
with libnx), versus 836 µs with pthread on L4T.

## Licenses

The screen font comes from Terminus (Dimitar Zhekov, SIL Open Font
License 1.1), converted at build time from the host system console font.
