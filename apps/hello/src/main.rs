//! Premier programme avec la bibliotheque standard complete.

use std::collections::HashMap;

fn main() {
    println!("Bonjour depuis std sur nxk !");
    let args: Vec<String> = std::env::args().collect();
    println!("arguments : {args:?}");
    let mut m = HashMap::new();
    for w in "le petit chat et le grand chat".split(' ') {
        *m.entry(w).or_insert(0) += 1;
    }
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort();
    println!("mots : {v:?}");
}
