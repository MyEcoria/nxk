//! The PC-built guests (`ps4/guest/`) linked and run in the interpreter: their output must
//! equal the native run's, in the standard and in the PS4 (SCE) layout.

use ps4rt::{hle, LinkReport, Outcome, Process};

const LIBMINI: &[u8] = include_bytes!("../../guest/bin/libmini.so");
const MAIN_SO: &[u8] = include_bytes!("../../guest/bin/main.so");
const MAIN_SCE: &[u8] = include_bytes!("../../guest/bin/main.sce");
const EXPECTED: &str = include_str!("../../guest/bin/expected.txt");

fn run(main: &[u8]) -> (String, LinkReport) {
    let mut p = Process::new(hle::libkernel());
    p.load("libmini.so", "libc", LIBMINI).unwrap();
    let m = p.load("main", "main", main).unwrap();
    let report = p.link().unwrap();
    let out = p.run(m, 100_000_000);
    let mut text = String::from_utf8(p.stdout).unwrap();
    match out {
        Outcome::Exited(code) => text.push_str(&format!("exit {code}\n")),
        other => panic!("guest stopped: {other:?}\noutput so far:\n{text}"),
    }
    (text, report)
}

#[test]
fn standard_layout_matches_native() {
    let (out, report) = run(MAIN_SO);
    assert_eq!(out, EXPECTED);
    assert_eq!(report.missing, 0, "{report:?}");
    assert!(report.to_modules >= 6 && report.to_hle >= 2, "{report:?}");
}

#[test]
fn sce_layout_matches_native() {
    let (out, report) = run(MAIN_SCE);
    assert_eq!(out, EXPECTED);
    assert_eq!(report.missing, 0, "{report:?}");
}

#[test]
fn missing_import_is_named() {
    // Without libmini, main's imports have no provider: the first call names its NID.
    let mut p = Process::new(hle::libkernel());
    let m = p.load("main", "main", MAIN_SCE).unwrap();
    let report = p.link().unwrap();
    assert!(report.missing > 0);
    match p.run(m, 1_000_000) {
        Outcome::Missing(Some(lib), nid) => {
            assert_eq!(lib, "libc");
            assert_eq!(nid, ps4fmt::nid::nid("puts_n"));
        }
        other => panic!("{other:?}"),
    }
}
