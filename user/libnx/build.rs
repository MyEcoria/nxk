// Le script de link est commun a tous les programmes : sa modification doit
// provoquer une nouvelle edition des liens.
fn main() {
    println!("cargo:rerun-if-changed=../user.ld");
}
