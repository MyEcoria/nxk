//! First program with the full standard library.

use std::collections::HashMap;

fn main() {
    println!("Hello from std on nxk!");
    let args: Vec<String> = std::env::args().collect();
    println!("arguments: {args:?}");
    let mut m = HashMap::new();
    for w in "the small cat and the big cat".split(' ') {
        *m.entry(w).or_insert(0) += 1;
    }
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort();
    println!("words: {v:?}");
}
