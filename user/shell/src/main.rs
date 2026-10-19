//! nxk interactive shell, over the serial console. Around each prompt
//! and command it prints the marks of shells integrated with their
//! terminal (OSC 7 directory, OSC 133 prompt, command and status):
//! terminals ignore them, the screen's TERMINAL page reads them.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI64, Ordering};

use libnx::abi::err;
use libnx::abi::info::{self, CpuInfo, MemInfo, ProcInfo};
use libnx::abi::proto::{audio, cpufreq, fan, gpu, pm, power};
use libnx::fs::{self, Kind};
use libnx::ipc::{self, Msg};
use libnx::seqpage::SeqPage;
use libnx::{console, env, ns, print, println, process, sys, time};

/// Outcome of the running command: 0, `-errno`, or a program's exit code.
static STATUS: AtomicI64 = AtomicI64::new(0);

/// The shell is one thread: its background jobs (name, process handle),
/// for `jobs` and `kill`.
struct Jobs(core::cell::RefCell<Vec<(String, u32)>>);
unsafe impl Sync for Jobs {}
static JOBS: Jobs = Jobs(core::cell::RefCell::new(Vec::new()));

fn fail(e: i64) {
    STATUS.store(e, Ordering::Relaxed);
}

libnx::entry!(main);

fn main() -> i32 {
    println!();
    banner();
    println!("nxk - shell. Type 'help' for the command list.");
    let mut buf = [0u8; 256];
    loop {
        // BEL-terminated OSC; "nxk> " stays whole for nxtest and usbterm.
        print!(
            "\x1b]7;file://nxk{}\x07\x1b]133;A\x07nxk> \x1b]133;B\x07",
            fs::absolute(".")
        );
        let n = console::read_line(&mut buf);
        if n == 0 && console::handle() == 0 {
            time::sleep_ms(1000); // no console left: avoid spinning idle
            continue;
        }
        let line = core::str::from_utf8(&buf[..n]).unwrap_or("").trim();
        if !line.is_empty() {
            print!("\x1b]133;C\x07");
            STATUS.store(0, Ordering::Relaxed);
            command(line);
            match STATUS.load(Ordering::Relaxed) {
                e if e < 0 => print!("\x1b]133;D;{};err={}\x07", -e, err::name(e)),
                code => print!("\x1b]133;D;{}\x07", code),
            }
        }
    }
}

/// Logo of `docs/logo/`: each letter is a `#` cell, the letter picks its color.
const LOGO: [&str; 11] = [
    "       aaaa   bbbb",
    "   aaaa           bbbb",
    " aa      ddddddd      bb      ff",
    "aa                     bb     ff",
    "aa   dd    fff    aa   bb     fffff   ff  ff  ff  ff",
    "     dd   fffff   aa          dd  dd   dddd   dd dd",
    "bb   dd    fff    aa   cc     aa  aa    aa    aaaa",
    "bb                     cc     aa  aa   aaaa   aa aa",
    " bb      aaaaaaa      cc      bb  bb  bb  bb  bb  bb",
    "   bbbb           cccc",
    "       bbbb   cccc",
];

fn banner() {
    // No ANSI colors on the debug_write fallback: the screen mirror prints them raw.
    let color = console::handle() != 0;
    let mut out = String::new();
    for (i, row) in LOGO.iter().enumerate() {
        let mut ink = ' ';
        for c in row.chars() {
            if color && c != ' ' && c != ink {
                ink = c;
                out.push_str(match c {
                    'a' => "\x1b[38;5;208m",
                    'b' => "\x1b[38;5;166m",
                    'c' => "\x1b[38;5;130m",
                    'd' => "\x1b[38;5;215m",
                    _ => "\x1b[38;5;223m",
                });
            }
            out.push(if c == ' ' { ' ' } else { '#' });
        }
        if color {
            out.push_str("\x1b[0m");
        }
        if i == LOGO.len() - 1 {
            out.push_str("            AArch64 microkernel for Tegra X1");
        }
        out.push('\n');
    }
    out.push('\n');
    // One message (< ipc::MAX_BYTES): services logging at boot cannot interleave.
    console::write(out.as_bytes());
}

/// Split a line into arguments; single or double quotes group
/// whitespace-separated words.
fn split_args(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in line.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    out.push(core::mem::take(&mut cur));
                    started = false;
                }
            }
            None => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

fn command(line: &str) {
    let words = split_args(line);
    let argv: Vec<&str> = words.iter().map(|s| s.as_str()).collect();
    let Some(&cmd) = argv.first() else { return };
    match cmd {
        "help" => help(),
        "echo" => echo(&argv[1..]),
        "ps" => ps(),
        "jobs" => jobs(),
        "kill" => kill(argv.get(1).copied().unwrap_or("")),
        "mem" => mem(),
        "cpu" => cpu(),
        "uptime" => println!("{} ms since boot", time::now_ns() / 1_000_000),
        "progs" => progs(),
        "ls" => ls(argv.get(1).copied().unwrap_or(".")),
        "cd" => cd(argv.get(1).copied().unwrap_or("/")),
        "pwd" => println!("{}", fs::absolute(".")),
        "cat" => argv[1..].iter().for_each(|p| cat(p)),
        "rm" => argv[1..].iter().for_each(|p| report(p, fs::remove_file(p))),
        "mkdir" => argv[1..].iter().for_each(|p| report(p, fs::mkdir(p))),
        "rmdir" => argv[1..].iter().for_each(|p| report(p, fs::remove_dir(p))),
        "mv" if argv.len() == 3 => report(argv[1], fs::rename(argv[1], argv[2])),
        "cp" if argv.len() == 3 => report(
            argv[1],
            fs::read(argv[1]).and_then(|d| fs::write(argv[2], &d)),
        ),
        "stat" => argv[1..].iter().for_each(|p| stat(p)),
        "df" => df(),
        "sync" => report("sync", fs::sync()),
        "fbecho" => fbecho(&argv[1..].join(" ")),
        "events" => events(argv.get(1).and_then(|v| v.parse().ok()).unwrap_or(5)),
        "env" => {
            for (k, v) in env::vars() {
                println!("{}={}", k, v);
            }
        }
        "export" => {
            for a in &argv[1..] {
                match a.split_once('=') {
                    Some((k, v)) if !k.is_empty() => env::set_var(k, v),
                    _ => {
                        println!("export: expected NAME=value ({})", a);
                        fail(-err::EINVAL);
                    }
                }
            }
        }
        "unset" => argv[1..].iter().for_each(|a| env::remove_var(a)),
        "date" => date(),
        "sleep" => time::sleep_ms(argv.get(1).and_then(|v| v.parse().ok()).unwrap_or(1000)),
        "temp" => temp(),
        "fan" => fan_cmd(argv.get(1).copied()),
        "cpufreq" => cpufreq_cmd(argv.get(1).copied()),
        "gpu" => gpu_cmd(&argv),
        "audio" => audio_cmd(&argv),
        "services" => {
            let mut buf = [0u8; 512];
            let n = ns::services(&mut buf);
            println!("{}", core::str::from_utf8(&buf[..n]).unwrap_or("?"));
        }
        "poweroff" => power_cmd(power::OFF),
        "reboot" => power_cmd(power::REBOOT),
        "recv" if argv.len() == 4 => recv(argv[1], argv[2], argv[3]),
        "restart" if argv.len() >= 2 => restart(argv[1], argv.get(2).copied()),
        "run" => run(&argv[1..]),
        _ => run(&argv), // a program name runs directly
    }
}

fn help() {
    println!("  help            this help");
    println!("  progs           boot image programs");
    println!("  ls [DIR]        list directory contents (SD card)");
    println!("  cd, pwd         current directory");
    println!("  cat FILE        print a file");
    println!("  echo T > F      write T into F (>>: append)");
    println!("  rm, mkdir, rmdir, mv A B, cp A B, stat, df, sync");
    println!("  fbecho TEXT     print TEXT on the screen");
    println!("  events [N]      show N input events (touchscreen, keyboard)");
    println!("  run PROG ARGS.. run PROG (trailing &: background); bare PROG works");
    println!("  jobs            background programs; kill N|NAME ends one");
    println!("  env             environment variables");
    println!("  export NAME=VAL set a variable (passed to spawned programs)");
    println!("  unset NAME      remove a variable");
    println!("  ps              processes");
    println!("  mem             physical memory");
    println!("  cpu             per-core statistics");
    println!("  uptime          time since boot");
    println!("  echo TEXT       print TEXT");
    println!("  date            wall-clock time (RTC)");
    println!("  sleep MS        wait MS milliseconds");
    println!("  temp            chip and package temperatures");
    println!("  fan [auto|PCT]  fan duty, speed, SoC temperature; PCT: set by hand");
    println!("  cpufreq [MHZ]   CPU clock, rail, steps; MHZ: a step, or boot");
    println!("  gpu [on|off]    GPU state, clock, rail; on / off: power it up / down");
    println!(
        "  audio [status|volume [PCT]|music|tone [HZ [MS [LEVEL]]]]  sound output and test melody"
    );
    println!("  gpu test        GPU self-test: 2D fill and copy, three compute kernels, 3D clear and triangle");
    println!("  gpu bench [MS]  GPU FFMA throughput for MS milliseconds (1000)");
    println!(
        "  gpu clock [MHZ] [force]  GPU clock, cap; MHZ: a sys-clk step (force: past the cap)"
    );
    println!("  gpu sweep [force]  gpu bench for 1 s at each step up to the cap");
    println!("  gpu load [MS]   share of MS milliseconds (1000) the GPU ran batches");
    println!("  services        published services");
    println!("  poweroff        power off the machine");
    println!("  reboot          reboot the machine");
    println!("  recv F SIZE CRC receive F as base64 lines (tools/usbterm.py --put)");
    println!("  restart DRV [F] replace boot driver DRV by F (default /bin/DRV)");
}

/// File typed as base64 lines (`tools/usbterm.py --put`): SIZE bytes,
/// checked against their CRC-32 (zlib's), then written. An empty line aborts.
fn recv(path: &str, size: &str, crc: &str) {
    let (Ok(size), Ok(crc)) = (size.parse::<usize>(), u32::from_str_radix(crc, 16)) else {
        println!("usage: recv FILE SIZE CRC32 (hex)");
        fail(-err::EINVAL);
        return;
    };
    let mut data = Vec::with_capacity(size);
    let mut line = [0u8; 1024];
    while data.len() < size {
        let n = console::read_line(&mut line);
        if n == 0 || !base64_decode(&line[..n], &mut data) {
            println!("recv: aborted after {} of {} bytes", data.len(), size);
            fail(-err::EIO);
            return;
        }
    }
    if data.len() != size || crc32(&data) != crc {
        println!(
            "recv: {} bytes, CRC {:08x}: corrupted",
            data.len(),
            crc32(&data)
        );
        fail(-err::EIO);
        return;
    }
    match fs::write(path, &data) {
        Ok(()) => println!("recv: {} ({} bytes)", path, size),
        Err(e) => {
            println!("recv: {}: {}", path, err::name(e));
            fail(e);
        }
    }
}

/// Standard base64 (padding allowed); false on a foreign character.
fn base64_decode(s: &[u8], out: &mut Vec<u8>) -> bool {
    let (mut acc, mut bits) = (0u32, 0);
    for &c in s {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return false,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    true
}

/// CRC-32 (IEEE 802.3, reflected), as zlib computes it.
fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0xEDB8_8320 & (c & 1).wrapping_neg());
        }
    }
    !c
}

/// Replace a boot driver by a program on the card, same hardware
/// grants (init's `RESTART`): the development loop for drivers.
fn restart(name: &str, path: Option<&str>) {
    let path = match path {
        Some(p) => fs::absolute(p),
        None => alloc::format!("/bin/{}", name),
    };
    let req = alloc::format!("{} {}", name, path);
    let r = ipc::call(
        process::pm_handle(),
        &ipc::pack(pm::RESTART, req.as_bytes()),
    )
    .and_then(ipc::checked);
    match r {
        Ok(_) => println!("{} restarted from {}", name, path),
        Err(e) => {
            println!("restart {}: {}", name, err::name(e));
            fail(e);
        }
    }
}

fn progs() {
    let mut buf = [0u8; 512];
    let n = process::list(&mut buf);
    println!("{}", core::str::from_utf8(&buf[..n]).unwrap_or("?"));
}

fn report<T>(what: &str, r: Result<T, i64>) {
    if let Err(e) = r {
        println!("{} : {}", what, err::name(e));
        fail(e);
    }
}

fn ls(path: &str) {
    match fs::read_dir(path) {
        Ok(entries) => {
            for e in entries {
                match e.stat.kind {
                    Kind::Dir => println!("  {:>10}  {}/", "", e.name),
                    Kind::File => println!("  {:>10}  {}", e.stat.size, e.name),
                }
            }
        }
        Err(e) => {
            println!("ls {}: {}", path, err::name(e));
            fail(e);
        }
    }
}

fn cd(path: &str) {
    let abs = fs::absolute(path);
    match fs::stat(&abs) {
        Ok(s) if s.kind == Kind::Dir => env::set_var("PWD", &abs),
        Ok(_) => {
            println!("cd {}: not a directory", path);
            fail(-err::ENOTDIR);
        }
        Err(e) => {
            println!("cd {}: {}", path, err::name(e));
            fail(e);
        }
    }
}

fn cat(path: &str) {
    match fs::read(path) {
        Ok(d) => console::write(&d),
        Err(e) => {
            println!("cat {}: {}", path, err::name(e));
            fail(e);
        }
    }
}

fn echo(args: &[&str]) {
    let redirect = args.iter().position(|a| *a == ">" || *a == ">>");
    let Some(k) = redirect else {
        println!("{}", args.join(" "));
        return;
    };
    let Some(&target) = args.get(k + 1) else {
        println!("echo: missing file after {}", args[k]);
        fail(-err::EINVAL);
        return;
    };
    let mut text = args[..k].join(" ");
    text.push('\n');
    let r = if args[k] == ">>" {
        fs::File::open(target, fs::O_READ | fs::O_WRITE | fs::O_CREATE).and_then(|mut f| {
            let end = f.size();
            f.write_at(end, text.as_bytes()).map(|_| ())
        })
    } else {
        fs::write(target, text.as_bytes())
    };
    report(target, r);
}

fn stat(path: &str) {
    match fs::stat(path) {
        Ok(s) => {
            let t = s.mtime as i64;
            let (y, m, d) = civil_from_days(t.div_euclid(86_400));
            let h = t.rem_euclid(86_400);
            println!(
                "{}: {}, {} bytes, modified {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                path,
                if s.kind == Kind::Dir { "dir" } else { "file" },
                s.size,
                y,
                m,
                d,
                h / 3600,
                h % 3600 / 60,
                h % 60
            );
        }
        Err(e) => {
            println!("stat {}: {}", path, err::name(e));
            fail(e);
        }
    }
}

fn df() {
    match fs::statfs() {
        Ok((free, total)) => println!("  SD card: {} MiB free of {} MiB", free >> 20, total >> 20),
        Err(e) => {
            println!("df: {}", err::name(e));
            fail(e);
        }
    }
}

fn run(argv: &[&str]) {
    let mut argv: Vec<&str> = argv.to_vec();
    let mut bg = false;
    if let Some(last) = argv.last_mut() {
        if *last == "&" {
            argv.pop();
            bg = true;
        } else if let Some(x) = last.strip_suffix('&') {
            *last = x;
            bg = true;
        }
    }
    let Some(&name) = argv.first() else {
        println!("run: missing program name");
        fail(-err::EINVAL);
        return;
    };
    let abs;
    if name.contains('/') {
        abs = fs::absolute(name);
        argv[0] = &abs;
    }
    let t0 = time::now_ns();
    let h = match process::spawn_args(&argv) {
        Ok(h) => h,
        Err(e) if e == -err::ENOENT => {
            println!("{}: unknown command or program (see 'help' and 'ls')", name);
            fail(e);
            return;
        }
        Err(e) => {
            println!("{}: failed to start ({})", name, err::name(e));
            fail(e);
            return;
        }
    };
    if bg {
        println!("[{} started in background]", name);
        JOBS.0.borrow_mut().push((String::from(name), h));
        return;
    } else {
        let code = process::wait(h);
        STATUS.store(code, Ordering::Relaxed);
        let ms = (time::now_ns() - t0) / 1_000_000;
        match code {
            0 => {}
            139 => println!("[{} killed by a memory fault, {} ms]", name, ms),
            c => println!("[{} exited with code {}, {} ms]", name, c, ms),
        }
    }
    let _ = sys::handle_close(h);
}

/// Background jobs still running (the ones that ended are let go).
fn jobs() {
    let mut j = JOBS.0.borrow_mut();
    j.retain(|(_, h)| {
        let done = sys::process_wait_timeout(*h, 1_000).is_ok();
        if done {
            let _ = sys::handle_close(*h);
        }
        !done
    });
    if j.is_empty() {
        println!("  no background jobs");
    }
    for (i, (name, _)) in j.iter().enumerate() {
        println!("  [{}] {}", i + 1, name);
    }
}

/// End a background job of this shell, by its number in `jobs` or its name.
fn kill(which: &str) {
    let mut j = JOBS.0.borrow_mut();
    let at = match which.parse::<usize>() {
        Ok(n) if n >= 1 && n <= j.len() => Some(n - 1),
        _ => j
            .iter()
            .rposition(|(name, _)| name.rsplit('/').next() == Some(which)),
    };
    match at {
        Some(i) => {
            let (name, h) = j.remove(i);
            let _ = sys::process_kill(h);
            let _ = sys::handle_close(h);
            println!("  {} killed", name);
        }
        None => {
            println!("kill: no such background job (see 'jobs')");
            fail(-err::ENOENT);
        }
    }
}

fn as_bytes_mut<T>(v: &mut [T]) -> &mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, core::mem::size_of_val(v)) }
}

fn ps() {
    let mut procs = [ProcInfo::default(); 64];
    let n = sys::sysinfo(info::PROCESSES, as_bytes_mut(&mut procs)).unwrap_or(0) as usize;
    println!("  PID  PPID  THR  PAGES  CPU(ms)  STATE   NAME");
    for p in &procs[..n.min(64)] {
        let len = p.name.iter().position(|&b| b == 0).unwrap_or(16);
        let name = core::str::from_utf8(&p.name[..len]).unwrap_or("?");
        let state = if p.state == info::PSTATE_ALIVE {
            "alive"
        } else {
            "zombie"
        };
        println!(
            "  {:>3}  {:>4}  {:>3}  {:>5}  {:>7}  {:<6}  {}",
            p.pid,
            p.parent,
            p.threads,
            p.pages,
            p.cpu_ns / 1_000_000,
            state,
            name
        );
    }
}

fn mem() {
    let mut m = [MemInfo::default()];
    if sys::sysinfo(info::MEMORY, as_bytes_mut(&mut m)).is_err() {
        return;
    }
    let m = m[0];
    let used = m.total_pages - m.free_pages;
    println!(
        "  total {} MiB, free {} MiB, used {} KiB (of which kernel {} KiB)",
        m.total_pages * 4 / 1024,
        m.free_pages * 4 / 1024,
        used * 4,
        m.kernel_pages * 4
    );
}

fn cpu() {
    let mut cpus = [CpuInfo::default(); 4];
    let n = sys::sysinfo(info::CPUS, as_bytes_mut(&mut cpus)).unwrap_or(0) as usize;
    println!("  CORE   SWITCHES     SYSCALLS    IRQ      IDLE");
    for (i, c) in cpus[..n.min(4)].iter().enumerate() {
        let total = (c.idle_ns + c.busy_ns).max(1);
        println!(
            "  {:>5}  {:>12}  {:>10}  {:>7}  {:>4}%",
            i,
            c.switches,
            c.syscalls,
            c.irqs,
            c.idle_ns * 100 / total
        );
    }
}

/// Civil date from a day count since 1970 (H. Hinnant).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn date() {
    let s = (sys::realtime_ns() / 1_000_000_000) as i64;
    let (y, m, d) = civil_from_days(s.div_euclid(86_400));
    let t = s.rem_euclid(86_400);
    println!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        y,
        m,
        d,
        t / 3600,
        t % 3600 / 60,
        t % 60
    );
}

/// Service `name`, waiting briefly if it is still starting.
fn service(name: &str) -> Option<u32> {
    match ns::wait_for_timeout(name, 3_000_000_000) {
        Ok(h) => Some(h),
        Err(_) => {
            println!("service {} missing", name);
            fail(-err::ENOENT);
            None
        }
    }
}

fn power_service() -> Option<u32> {
    service("power")
}

fn temp() {
    let Some(h) = power_service() else { return };
    match ipc::call(h, &Msg::new(power::TEMPERATURE, 0, 0)) {
        Ok(m) if m.label() == 0 => {
            let f = |v: u64| {
                let v = v as i64;
                (v / 1000, (v % 1000).abs() / 100)
            };
            let (a, b) = (f(m.mr[0]), f(m.mr[1]));
            println!("chip {}.{} C, package {}.{} C", a.0, a.1, b.0, b.1);
        }
        Ok(m) => {
            println!("temp: error {}", err::name(-(m.label() as i64)));
            fail(-(m.label() as i64));
        }
        Err(e) => {
            println!("temp: {}", err::name(e));
            fail(e);
        }
    }
    let _ = sys::handle_close(h);
}

/// `fan`: its state with a fresh speed reading (1 s); `fan auto` or
/// `fan PCT` (0-100) sets it.
fn fan_cmd(arg: Option<&str>) {
    let set = match arg {
        None => None,
        Some("auto") => Some(fan::AUTO),
        Some(p) => match p.parse::<u64>() {
            Ok(p) if p <= 100 => Some(p),
            _ => {
                println!("usage: fan [auto|0-100]");
                fail(-err::EINVAL);
                return;
            }
        },
    };
    let Some(h) = service("fan") else { return };
    let r = match set {
        Some(w) => ipc::call(h, &Msg::from_words(fan::SET, &[w])),
        None => {
            // No tachometer: the state still shows, without a speed.
            let _ = ipc::call(h, &Msg::new(fan::MEASURE, 0, 0));
            ipc::call(h, &Msg::new(fan::STATUS, 0, 0))
        }
    };
    match r.and_then(ipc::checked) {
        Ok(m) => {
            let (st, t, rpm) = (m.mr[1], m.mr[2], m.mr[3]);
            let mode = if st & fan::ST_HOT != 0 {
                "full speed: SoC hot"
            } else if st & fan::ST_NO_SENSOR != 0 {
                "no temperature: safe speed"
            } else if st & fan::ST_MANUAL != 0 {
                "manual"
            } else {
                "auto"
            };
            print!("fan {}% ({})", m.mr[0], mode);
            if t != fan::NO_TEMP {
                let t = t as i64;
                print!(", SoC {}.{} C", t / 1000, (t % 1000).abs() / 100);
            }
            if rpm != fan::NO_RPM {
                print!(", {} rpm", rpm);
            }
            println!();
        }
        Err(e) => {
            println!("fan: {}", err::name(e));
            fail(e);
        }
    }
    let _ = sys::handle_close(h);
}

/// `cpufreq`: CPU clock, rail and the steps this chip runs;
/// `cpufreq MHZ` or `cpufreq boot` sets it.
fn cpufreq_cmd(arg: Option<&str>) {
    let set = match arg {
        None => None,
        Some("boot") => Some(cpufreq::BOOT),
        Some(s) => match s.parse::<u64>() {
            Ok(m) if cpufreq::STEPS_MHZ.contains(&m) => Some(m),
            _ => {
                print!("usage: cpufreq [boot|MHZ], MHZ one of");
                cpufreq::STEPS_MHZ.iter().for_each(|m| print!(" {}", m));
                println!();
                fail(-err::EINVAL);
                return;
            }
        },
    };
    let Some(h) = service("cpufreq") else { return };
    let r = match set {
        Some(w) => ipc::call(h, &Msg::from_words(cpufreq::SET, &[w])),
        None => ipc::call(h, &Msg::new(cpufreq::STATUS, 0, 0)),
    };
    match r.and_then(ipc::checked) {
        Ok(m) => {
            let (khz, uv, speedo, st, allowed, step) =
                (m.mr[0], m.mr[1], m.mr[2], m.mr[3], m.mr[4], m.mr[5]);
            match khz {
                0 => print!("cpu ? MHz"),
                k => print!("cpu {}.{} MHz", k / 1000, k % 1000 / 100),
            }
            match step {
                cpufreq::BOOT => print!(" (boot clock)"),
                s => print!(" (step {})", s),
            }
            if uv != 0 {
                print!(", rail {} mV", uv / 1000);
            }
            if st & cpufreq::ST_UNSUPPORTED != 0 {
                println!(", boot clock only on this console");
                return;
            }
            println!(", speedo {}", speedo);
            if st & cpufreq::ST_HOT != 0 {
                println!("back to the boot clock: the SoC reached 80 C");
            }
            print!("steps:");
            let mut over = None;
            for (i, s) in cpufreq::STEPS_MHZ.iter().enumerate() {
                if allowed & 1 << i != 0 {
                    print!(" {}", s);
                } else {
                    over = over.or(Some(*s));
                }
            }
            match over {
                Some(s) => println!("; {} and up need more than 1225 mV here", s),
                None => println!(),
            }
        }
        Err(e) => {
            match -e {
                err::ERANGE => println!("cpufreq: this step needs more than 1225 mV on this chip"),
                err::ENODEV => println!("cpufreq: boot clock only on this console"),
                _ => println!("cpufreq: {}", err::name(e)),
            }
            fail(e);
        }
    }
    let _ = sys::handle_close(h);
}

/// Power-up stages (`gpu::STAGE_*`).
const GPU_STAGES: [&str; 11] = [
    "",
    "rail",
    "clocks",
    "PLL",
    "MMU",
    "firmware",
    "secure boot",
    "FECS",
    "context",
    "channel",
    "ready",
];

/// FFMA throughput of the GM20B per clock cycle: 2 SMs of 128 lanes.
const GPU_FLOPS_PER_CYCLE: u64 = 2 * 128 * 2;

/// kHz as "N.N MHz".
fn mhz(khz: u64) -> String {
    alloc::format!("{}.{} MHz", khz / 1000, khz % 1000 / 100)
}

/// A `CLOCK` request: `khz` (0: report) with `force`.
fn gpu_clock_call(h: u32, khz: u64, force: bool) -> Result<Msg, i64> {
    let flags = if force { gpu::CLOCK_FORCE } else { 0 };
    ipc::call(h, &Msg::from_words(gpu::CLOCK, &[khz, flags])).and_then(ipc::checked)
}

fn gpu_clock_error(e: i64) {
    match -e {
        err::ERANGE => println!("gpu clock: not a step, or above the cap (add force)"),
        err::ENODEV => println!("gpu clock: 76.8 MHz only on this console"),
        err::EIO => println!("gpu clock: measured off its step, back to 76.8 MHz"),
        _ => println!("gpu clock: {}", err::name(e)),
    }
    fail(e);
}

/// `gpu clock [MHZ] [force]`: the step set, its rail, the measured clock
/// and the power source's cap; MHZ as sys-clk writes it (76, 153, ... 921).
fn gpu_clock(argv: &[&str]) {
    let force = argv.last() == Some(&"force");
    let khz = match argv
        .get(2)
        .filter(|a| **a != "force")
        .map(|a| a.parse::<u64>())
    {
        None => Some(0),
        Some(Ok(m)) => gpu::STEPS_KHZ.iter().copied().find(|k| k / 1000 == m),
        Some(Err(_)) => None,
    };
    let Some(khz) = khz else {
        print!("gpu clock: steps (MHz):");
        for k in gpu::STEPS_KHZ {
            print!(" {}", k / 1000);
        }
        println!();
        fail(-err::EINVAL);
        return;
    };
    let Some(h) = service("gpu") else { return };
    match gpu_clock_call(h, khz, force) {
        Ok(m) => {
            let (step, uv, cap, supply, measured, ck) =
                (m.mr[0], m.mr[1], m.mr[2], m.mr[3], m.mr[4], m.mr[5]);
            print!("gpu clock: {}", mhz(step));
            match uv {
                0 => print!(" (GPU off: set at power-up)"),
                u => print!(", rail {} mV", u / 1000),
            }
            if measured != 0 {
                print!(", measured {}", mhz(measured));
            }
            let source = match supply {
                power::SUPPLY_BATTERY => "battery",
                power::SUPPLY_EXTERNAL => "external power",
                _ => "charger unreadable",
            };
            if ck & gpu::CK_FORCED != 0 {
                println!("; no cap (force), on {}", source);
            } else {
                println!("; cap {} on {}", mhz(cap), source);
            }
            if ck & gpu::CK_HOT != 0 {
                println!("gpu clock: back to the lowest step, the SoC reached 80 C");
            }
            if ck & gpu::CK_CAPPED != 0 {
                println!("gpu clock: lowered to the cap, the power source changed");
            }
        }
        Err(e) => gpu_clock_error(e),
    }
    let _ = sys::handle_close(h);
}

/// `gpu load [MS]`: share of MS milliseconds (1000) with a GPU batch
/// in flight, from the service's stats page (read while it runs one).
fn gpu_load(argv: &[&str]) {
    let ms = match argv.get(2).map(|a| a.parse::<u64>()) {
        None => 1000,
        Some(Ok(ms)) if (1..=60_000).contains(&ms) => ms,
        _ => {
            println!("usage: gpu load [MS], MS 1 to 60000");
            fail(-err::EINVAL);
            return;
        }
    };
    let Some(h) = service("gpu") else { return };
    let r = ipc::call(h, &Msg::from_words(gpu::STATS, &[0])).and_then(ipc::checked);
    let _ = sys::handle_close(h);
    let page = match r {
        Ok(m) if m.handles() == 1 => {
            let ph = ipc::received_handle(0);
            SeqPage::map(ph).inspect_err(|_| {
                let _ = sys::handle_close(ph);
            })
        }
        Ok(_) => Err(-err::EIO),
        Err(e) => Err(e),
    };
    let page = match page {
        Ok(p) => p,
        Err(e) => {
            println!("gpu load: {}", err::name(e));
            fail(e);
            return;
        }
    };
    // Time, busy time up to it (the batch in flight too), state.
    let sample = || {
        let now = time::now_ns();
        page.read::<{ gpu::STATS_WORDS }>().map(|w| {
            let running = match w[gpu::S_SINCE_NS] {
                0 => 0,
                t => now.saturating_sub(t),
            };
            (now, w[gpu::S_BUSY_NS] + running, w[gpu::S_STATE] & 0xFF)
        })
    };
    let first = sample();
    time::sleep_ms(ms);
    let (Some((t0, b0, _)), Some((t1, b1, state))) = (first, sample()) else {
        println!("gpu load: stats page kept changing");
        fail(-err::EIO);
        return;
    };
    let pct = (b1.saturating_sub(b0) * 100 / (t1 - t0).max(1)).min(100);
    println!(
        "gpu load: {}% over {} ms ({}{} ms busy since the service started)",
        pct,
        (t1 - t0) / 1_000_000,
        if state == gpu::ON { "" } else { "GPU off; " },
        b1 / 1_000_000
    );
}

/// `gpu sweep [force]`: `gpu bench` for 1 s at each step up to the cap
/// (all with force), then back to the step set before.
fn gpu_sweep(argv: &[&str]) {
    let force = argv.get(2) == Some(&"force");
    let Some(h) = service("gpu") else { return };
    let before = match gpu_clock_call(h, 0, false) {
        Ok(m) => m,
        Err(e) => {
            gpu_clock_error(e);
            let _ = sys::handle_close(h);
            return;
        }
    };
    let top = if force {
        *gpu::STEPS_KHZ.last().unwrap()
    } else {
        before.mr[2]
    };
    println!("gpu sweep: 1 s of FFMA per step, up to {}", mhz(top));
    println!("     MHz     mV  measured  GFLOP/s  of peak");
    for khz in gpu::STEPS_KHZ.iter().copied().filter(|&k| k <= top) {
        let m = match gpu_clock_call(h, khz, force) {
            Ok(m) => m,
            Err(e) => {
                gpu_clock_error(e);
                break;
            }
        };
        let b = match ipc::call(h, &Msg::from_words(gpu::BENCH, &[1000])).and_then(ipc::checked) {
            Ok(b) => b,
            Err(e) => {
                println!("gpu sweep: bench at {}: {}", mhz(khz), err::name(e));
                fail(e);
                break;
            }
        };
        let mflops = b.mr[0] * 1000 / b.mr[1].max(1);
        let peak = GPU_FLOPS_PER_CYCLE * khz / 1000;
        println!(
            "  {:>6}  {:>5}  {:>8}  {:>4}.{:02}  {:>5}.{}%",
            alloc::format!("{}.{}", khz / 1000, khz % 1000 / 100),
            m.mr[1] / 1000,
            alloc::format!("{}.{}", m.mr[4] / 1000, m.mr[4] % 1000 / 100),
            mflops / 1000,
            mflops % 1000 / 10,
            mflops * 100 / peak.max(1),
            mflops * 1000 / peak.max(1) % 10
        );
    }
    let was_forced = before.mr[5] & gpu::CK_FORCED != 0;
    match gpu_clock_call(h, before.mr[0], was_forced) {
        Ok(_) => println!("gpu sweep: back to {}", mhz(before.mr[0])),
        Err(e) => gpu_clock_error(e),
    }
    let _ = sys::handle_close(h);
}

/// `audio`: bring the codec up and show the hub; `audio volume [PCT]`:
/// speaker volume (0..100%); `audio music`: polyphonic test melody;
/// `audio tone [HZ [MS [LEVEL]]]`: a sine on the speakers (440 Hz, 1000 ms, level 25).
fn audio_cmd(argv: &[&str]) {
    let num = |i: usize, d: u64| argv.get(i).and_then(|v| v.parse().ok()).unwrap_or(d);
    let req = match argv.get(1).copied() {
        None | Some("status") => Msg::new(audio::STATUS, 0, 0),
        Some("music") => Msg::new(audio::MUSIC, 0, 0),
        Some("volume") | Some("vol") => {
            let v = match argv.get(2) {
                None => audio::VOL_GET,
                Some(s) => match s.parse::<u64>() {
                    Ok(v) if v <= 100 => v,
                    _ => {
                        println!("usage: audio volume [0..100]");
                        fail(-err::EINVAL);
                        return;
                    }
                },
            };
            Msg::from_words(audio::VOLUME, &[v])
        }
        Some("tone") => Msg::from_words(audio::TONE, &[num(2, 440), num(3, 1000), num(4, 25)]),
        _ => {
            println!("usage: audio [status|volume [0..100]|music|tone [HZ [MS [LEVEL]]]]");
            fail(-err::EINVAL);
            return;
        }
    };
    let Some(h) = service("audio") else { return };
    match ipc::call(h, &req).and_then(ipc::checked) {
        Ok(m) if req.label() == audio::TONE => println!("audio: tone for {} ms", m.mr[0]),
        Ok(m) if req.label() == audio::MUSIC => println!("audio: playing melody ({} ms)", m.mr[0]),
        Ok(m) if req.label() == audio::VOLUME => println!("audio: volume {}%", m.mr[0]),
        Ok(m) => {
            let ready = if m.mr[0] & audio::ST_READY != 0 {
                "ready"
            } else {
                "off"
            };
            let playing = if m.mr[0] & audio::ST_PLAYING != 0 {
                ", playing"
            } else {
                ""
            };
            println!(
                "audio: {}{}, volume {}%, {} underruns, {} periods played",
                ready, playing, m.mr[3], m.mr[1], m.mr[2]
            );
        }
        Err(e) => {
            println!("audio: {}", err::name(e));
            fail(e);
        }
    }
    let _ = sys::handle_close(h);
}

/// `gpu`: state, clock, rail and chip; `gpu on|off`; `gpu test`: the
/// 2D and compute self-test; `gpu irtest`: the shader compiler's, on
/// random programs and a draw; `gpu bench [MS]`: FFMA throughput;
/// `gpu clock`, `gpu sweep`: sys-clk's steps.
fn gpu_cmd(argv: &[&str]) {
    match argv.get(1).copied() {
        Some("clock") => return gpu_clock(argv),
        Some("sweep") => return gpu_sweep(argv),
        Some("load") => return gpu_load(argv),
        _ => {}
    }
    let req = match (argv.get(1).copied(), argv.get(2).map(|s| s.parse::<u64>())) {
        (None, _) => Msg::new(gpu::STATUS, 0, 0),
        (Some("on"), None) => Msg::from_words(gpu::POWER, &[1]),
        (Some("off"), None) => Msg::from_words(gpu::POWER, &[0]),
        (Some("test"), None) => Msg::new(gpu::TEST, 0, 0),
        (Some("irtest"), None) => Msg::new(gpu::TEST_IR, 0, 0),
        (Some("bench"), None) => Msg::from_words(gpu::BENCH, &[1000]),
        (Some("bench"), Some(Ok(ms))) if (1..=10_000).contains(&ms) => {
            Msg::from_words(gpu::BENCH, &[ms])
        }
        _ => {
            println!(
                "usage: gpu [on|off|test|irtest|bench [MS]|clock [MHZ] [force]|sweep [force]|load [MS]]"
            );
            fail(-err::EINVAL);
            return;
        }
    };
    let Some(h) = service("gpu") else { return };
    match ipc::call(h, &req).and_then(ipc::checked) {
        Ok(m) if req.label() == gpu::TEST || req.label() == gpu::TEST_IR => {
            let (pass, run, ns) = (m.mr[0], m.mr[1], m.mr[2]);
            for (bit, name) in [
                (gpu::T_FILL2D, "2D fill"),
                (gpu::T_COPY2D, "2D copy"),
                (gpu::T_KFILL, "fill kernel"),
                (gpu::T_KCOPY, "copy kernel"),
                (gpu::T_KSAXPY, "saxpy kernel"),
                (gpu::T3_CLEAR, "3D clear"),
                (gpu::T3_TRIANGLE, "3D triangle"),
                (gpu::T3_DEPTH, "3D depth"),
                (gpu::T3_LIT, "3D lit quad"),
                (gpu::T3_TEXTURE, "3D textured quad"),
                (gpu::T3_IR_COMPUTE, "IR compute vs reference"),
                (gpu::T3_IR_DRAW, "IR vertex+fragment draw"),
                (gpu::T3_IR_RAYS, "IR ray traversal vs CPU"),
                (gpu::T3_IR_PATH, "IR target, compute, fragment"),
                (gpu::T3_IR_SHADOWS, "IR shadow kernel vs reference"),
                (gpu::T3_IR_INSTANCE, "IR instanced draw"),
                (gpu::T3_IR_TARGET, "IR float target -> texture"),
            ] {
                if run & bit == 0 {
                    continue;
                }
                let r = if pass & bit != 0 { "ok" } else { "FAILED" };
                println!("  {:<13} {}", name, r);
            }
            println!(
                "gpu test: {}/{} passed, GPU time {} us",
                pass.count_ones(),
                run.count_ones(),
                ns / 1000
            );
            if pass != run {
                fail(-err::EIO);
            }
        }
        Ok(m) if req.label() == gpu::BENCH => {
            let (flops, ns) = (m.mr[0], m.mr[1].max(1));
            let mflops = flops * 1000 / ns;
            println!(
                "gpu bench: {}.{:02} GFLOP/s ({} MFLOP in {} ms)",
                mflops / 1000,
                mflops % 1000 / 10,
                flops / 1_000_000,
                ns / 1_000_000
            );
        }
        Ok(m) => {
            let (state, stage) = (m.mr[0] & 0xFF, (m.mr[0] >> 8) as usize);
            let (khz, uv, id, speedo) = (m.mr[1], m.mr[2], m.mr[3], m.mr[4]);
            let name = GPU_STAGES.get(stage).copied().unwrap_or("?");
            match state {
                gpu::ON => {
                    print!("gpu on: chip {:#x}", id);
                    match khz {
                        0 => print!(", bypass clock"),
                        k => print!(", {}.{} MHz", k / 1000, k % 1000 / 100),
                    }
                    print!(", rail {} mV", uv / 1000);
                    match speedo >> 32 {
                        0xFF => println!(),
                        i => println!(", speedo {} (ID {})", speedo as u32 as i32, i),
                    }
                }
                gpu::FAILED => {
                    println!(
                        "gpu failed at stage {} ({}): {}",
                        stage,
                        name,
                        err::name(m.mr[5] as i64)
                    );
                    fail(m.mr[5] as i64);
                }
                _ => println!("gpu off"),
            }
        }
        Err(e) => {
            println!("gpu: {}", err::name(e));
            fail(e);
        }
    }
    let _ = sys::handle_close(h);
}

fn power_cmd(label: u32) {
    let Some(h) = power_service() else { return };
    match ipc::call(h, &Msg::new(label, 0, 0)) {
        Ok(m) if m.label() != 0 => {
            println!("failed: {}", err::name(-(m.label() as i64)));
            fail(-(m.label() as i64));
        }
        Err(e) => {
            println!("failed: {}", err::name(e));
            fail(e);
        }
        _ => {}
    }
    let _ = sys::handle_close(h);
}

fn fbecho(text: &str) {
    if let Err(e) = libnx::drv::screen(&alloc::format!("{}\n", text)) {
        println!("fbecho: {}", err::name(e));
        fail(e);
    }
}

fn events(n: u32) {
    use libnx::abi::proto::input;
    let Some(h) = service("input") else { return };
    for _ in 0..n {
        let mut m = Msg::new(input::READ, 1, 0);
        m.mr[0] = 10_000_000_000; // 10 s
        match ipc::call(h, &m) {
            Ok(r) if r.label() == 0 => {
                let kind = match r.mr[0] {
                    input::KIND_TOUCH => match r.mr[1] {
                        input::TOUCH_DOWN => "touch: down",
                        input::TOUCH_MOVE => "touch: move",
                        _ => "touch: up",
                    },
                    input::KIND_KEY => {
                        let c = char::from_u32(r.mr[3] as u32).unwrap_or('?');
                        println!(
                            "  HID key {:#04x} {} '{}'",
                            r.mr[1],
                            if r.mr[2] != 0 { "pressed" } else { "released" },
                            c
                        );
                        continue;
                    }
                    _ => "event",
                };
                println!(
                    "  {} (finger {}) x={} y={}",
                    kind, r.mr[2], r.mr[3], r.mr[4]
                );
            }
            Ok(_) => {
                println!("  (no event in 10 s)");
                break;
            }
            Err(e) => {
                println!("events: {}", err::name(e));
                fail(e);
                break;
            }
        }
    }
    let _ = sys::handle_close(h);
}
