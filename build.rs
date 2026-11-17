//! Build-time link fixes that cannot live in Cargo.toml, and the optional
//! embedded Wizard GUI (`embed_gui` below).
//!
//! LuaJIT (pulled in by `mlua` with the `luajit` + `vendored` features) calls
//! `__clear_cache` on aarch64 to flush the instruction cache after emitting
//! JIT code. glibc and the dynamic libgcc path provide that symbol; a fully
//! static `aarch64-unknown-linux-musl` link with `musl-gcc` does not, so the
//! final link fails with an undefined reference. Pulling libgcc in closes the
//! gap.
//!
//! The Android NDK has the same hole for a different reason: rustc links with
//! `-nodefaultlibs`, so clang never adds its compiler-rt builtins archive,
//! which is where `__clear_cache` lives there. The linker is asked where that
//! archive is rather than the path being spelled out, because it moves with
//! every NDK's clang version. Other targets are unaffected.

fn main() {
    embed_gui();
    println!("cargo:rerun-if-env-changed=RUSTC_LINKER");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "aarch64-unknown-linux-musl" {
        println!("cargo:rustc-link-arg=-lgcc");
    }
    if target.ends_with("-linux-android")
        && let Some(builtins) = android_builtins()
    {
        println!("cargo:rustc-link-arg={builtins}");
    }
}

/// `WIZARD_GUI=<path to a built wizard-gui>` puts the desktop app inside this
/// binary, gzipped, so `wizard gui` can unpack and start it with nothing else
/// installed (see `src/desktop_app.rs`). The release sets it for the targets
/// the GUI is built for. Unset, as in a `cargo install`, nothing is embedded
/// and `wizard gui` looks for an installed copy instead. A relative path is
/// relative to this directory.
fn embed_gui() {
    use std::io::Write;
    use std::path::PathBuf;

    use sha2::Digest;

    println!("cargo:rustc-check-cfg=cfg(wizard_gui)");
    println!("cargo:rerun-if-env-changed=WIZARD_GUI");
    let Some(src) = std::env::var_os("WIZARD_GUI").filter(|s| !s.is_empty()) else {
        return;
    };
    let src = PathBuf::from(src);
    println!("cargo:rerun-if-changed={}", src.display());
    let bytes = std::fs::read(&src).unwrap_or_else(|e| panic!("WIZARD_GUI={}: {e}", src.display()));
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("wizard-gui.gz");
    let file = std::fs::File::create(&out).expect("create OUT_DIR/wizard-gui.gz");
    let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    gz.write_all(&bytes).expect("compress the GUI");
    gz.finish().expect("compress the GUI");
    let digest = sha2::Sha256::digest(&bytes);
    let id: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    println!("cargo:rustc-cfg=wizard_gui");
    println!("cargo:rustc-env=WIZARD_GUI_ID={id}");
}

/// The NDK clang's compiler-rt builtins archive, from the configured linker.
/// `None` when no linker is configured or it cannot answer, which leaves the
/// link to fail with the undefined symbol named.
fn android_builtins() -> Option<String> {
    let linker = std::env::var("RUSTC_LINKER").ok()?;
    let output = std::process::Command::new(linker)
        .arg("-print-libgcc-file-name")
        .output()
        .ok()?;
    let path = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (output.status.success() && std::path::Path::new(&path).is_file()).then_some(path)
}
