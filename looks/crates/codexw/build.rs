//! `reference_captures` is set when the captures of the real tools sit in `reference/` at the
//! workspace root. They are not in this repository, and the tests that compare against them are
//! ignored without them. After adding the directory, touch this file so cargo looks again.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(reference_captures)");
    println!("cargo::rerun-if-changed=build.rs");
    let captures = std::path::Path::new("../../reference");
    if captures.is_dir() {
        println!("cargo::rerun-if-changed=../../reference");
        println!("cargo::rustc-cfg=reference_captures");
    }
}
