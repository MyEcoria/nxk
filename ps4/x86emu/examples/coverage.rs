//! Decoder coverage of a disassembly: which instructions `decode` rejects, by mnemonic.
//!
//!     objdump -d --no-show-raw-insn ... | ...   (see docs/PS4.md for the pipeline)
//!
//! Reads `mnemonic<TAB>hex bytes` lines on stdin, prints unsupported mnemonics by count.

use std::collections::BTreeMap;
use std::io::BufRead;

fn main() {
    let (mut total, mut bad) = (0usize, BTreeMap::<String, usize>::new());
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let Some((mnemonic, hex)) = line.split_once('\t') else {
            continue;
        };
        let bytes: Vec<u8> = hex
            .split_whitespace()
            .filter_map(|b| u8::from_str_radix(b, 16).ok())
            .collect();
        total += 1;
        match x86emu::decode::decode(&bytes) {
            Ok(i) if i.len as usize == bytes.len() => {}
            Ok(_) => {
                *bad.entry(format!("{mnemonic} (length differs)"))
                    .or_default() += 1
            }
            Err(_) => *bad.entry(mnemonic.to_owned()).or_default() += 1,
        }
    }
    let missing: usize = bad.values().sum();
    println!("{total} instructions, {missing} not decoded");
    let mut v: Vec<_> = bad.into_iter().collect();
    v.sort_by_key(|e| std::cmp::Reverse(e.1));
    for (m, n) in v {
        println!("{n:6} {m}");
    }
}
