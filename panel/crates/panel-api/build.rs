fn main() {
    // SQLx's stable migrate! macro cannot track newly added files itself.
    println!("cargo:rerun-if-changed=migrations");
}
