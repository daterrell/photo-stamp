//! Windows builds: compile `assets/photo-stamp.rc` (the icons and the application manifest) with
//! mingw's windres and link it into photo-stamp.exe. Other targets need nothing.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/photo-stamp.rc");
    println!("cargo:rerun-if-changed=assets/photo-stamp.ico");
    println!("cargo:rerun-if-changed=assets/photo-stamp-paused.ico");
    println!("cargo:rerun-if-changed=assets/photo-stamp.manifest");
    println!("cargo:rerun-if-env-changed=WINDRES");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "x86_64".into());
    let windres = std::env::var("WINDRES").unwrap_or_else(|_| format!("{arch}-w64-mingw32-windres"));
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("photo-stamp-res.o");
    let status = Command::new(&windres)
        .current_dir("assets")
        .args(["--input", "photo-stamp.rc", "--output-format=coff", "--output"])
        .arg(&out)
        .status();
    match status {
        Ok(s) if s.success() => println!("cargo:rustc-link-arg-bins={}", out.display()),
        Ok(s) => panic!("{windres} failed ({s}) compiling assets/photo-stamp.rc"),
        // E.g. building natively with MSVC: no icon, but everything else works.
        Err(e) => println!("cargo:warning=no Windows resources (icon, manifest): {windres}: {e}"),
    }
}
