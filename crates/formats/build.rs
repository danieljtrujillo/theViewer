// The C++ runtime link arguments live in `build_link.rs`, which `crates/app/build.rs` includes
// too. See that file for why the order on the link line is what it is.

include!("build_link.rs");

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_link.rs");
    emit_trailing_link_args();
}
