fn main() {
    println!("cargo:rerun-if-changed=link.ld");
    println!("cargo:rerun-if-env-changed=NXK_BL33_SIZE");
}
