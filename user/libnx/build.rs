// The link script is shared by all programs: changing it must re-link.
fn main() {
    println!("cargo:rerun-if-changed=../user.ld");
}
