//! nxk interactive shell, over the serial console.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use libnx::abi::err;
use libnx::abi::info::{self, CpuInfo, MemInfo, ProcInfo};
use libnx::abi::proto::power;
use libnx::fs::{self, Kind};
use libnx::ipc::{self, Msg};
use libnx::{console, env, ns, print, println, process, sys, time};

libnx::entry!(main);

fn main() -> i32 {
    println!();
    banner();
    println!("nxk - shell. Type 'help' for the command list.");
    let mut buf = [0u8; 256];
    loop {
        print!("nxk> ");
        let n = console::read_line(&mut buf);
        if n == 0 && console::handle() == 0 {
            time::sleep_ms(1000); // no console left: avoid spinning idle
            continue;
        }
        let line = core::str::from_utf8(&buf[..n]).unwrap_or("").trim();
        if !line.is_empty() {
            command(line);
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
                    _ => println!("export: expected NAME=value ({})", a),
                }
            }
        }
        "unset" => argv[1..].iter().for_each(|a| env::remove_var(a)),
        "date" => date(),
        "sleep" => time::sleep_ms(argv.get(1).and_then(|v| v.parse().ok()).unwrap_or(1000)),
        "temp" => temp(),
        "services" => {
            let mut buf = [0u8; 512];
            let n = ns::services(&mut buf);
            println!("{}", core::str::from_utf8(&buf[..n]).unwrap_or("?"));
        }
        "poweroff" => power_cmd(power::OFF),
        "reboot" => power_cmd(power::REBOOT),
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
    println!("  services        published services");
    println!("  poweroff        power off the machine");
    println!("  reboot          reboot the machine");
}

fn progs() {
    let mut buf = [0u8; 512];
    let n = process::list(&mut buf);
    println!("{}", core::str::from_utf8(&buf[..n]).unwrap_or("?"));
}

fn report<T>(what: &str, r: Result<T, i64>) {
    if let Err(e) = r {
        println!("{} : {}", what, err::name(e));
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
        Err(e) => println!("ls {}: {}", path, err::name(e)),
    }
}

fn cd(path: &str) {
    let abs = fs::absolute(path);
    match fs::stat(&abs) {
        Ok(s) if s.kind == Kind::Dir => env::set_var("PWD", &abs),
        Ok(_) => println!("cd {}: not a directory", path),
        Err(e) => println!("cd {}: {}", path, err::name(e)),
    }
}

fn cat(path: &str) {
    match fs::read(path) {
        Ok(d) => console::write(&d),
        Err(e) => println!("cat {}: {}", path, err::name(e)),
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
        Err(e) => println!("stat {}: {}", path, err::name(e)),
    }
}

fn df() {
    match fs::statfs() {
        Ok((free, total)) => println!("  SD card: {} MiB free of {} MiB", free >> 20, total >> 20),
        Err(e) => println!("df: {}", err::name(e)),
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
            return;
        }
        Err(e) => {
            println!("{}: failed to start ({})", name, err::name(e));
            return;
        }
    };
    if bg {
        println!("[{} started in background]", name);
    } else {
        let code = process::wait(h);
        let ms = (time::now_ns() - t0) / 1_000_000;
        match code {
            0 => {}
            139 => println!("[{} killed by a memory fault, {} ms]", name, ms),
            c => println!("[{} exited with code {}, {} ms]", name, c, ms),
        }
    }
    let _ = sys::handle_close(h);
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
        Ok(m) => println!("temp: error {}", err::name(-(m.label() as i64))),
        Err(e) => println!("temp: {}", err::name(e)),
    }
    let _ = sys::handle_close(h);
}

fn power_cmd(label: u32) {
    let Some(h) = power_service() else { return };
    match ipc::call(h, &Msg::new(label, 0, 0)) {
        Ok(m) if m.label() != 0 => println!("failed: {}", err::name(-(m.label() as i64))),
        Err(e) => println!("failed: {}", err::name(e)),
        _ => {}
    }
    let _ = sys::handle_close(h);
}

fn fbecho(text: &str) {
    let Some(h) = service("display") else { return };
    let mut t = alloc::string::String::from(text);
    t.push('\n');
    let _ = ipc::call(
        h,
        &ipc::pack(libnx::abi::proto::display::WRITE, t.as_bytes()),
    );
    let _ = sys::handle_close(h);
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
                break;
            }
        }
    }
    let _ = sys::handle_close(h);
}
