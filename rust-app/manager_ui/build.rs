//! Puts the app icon and the version into the exe (GNU toolchain, the one the release is built with), so Explorer
//! and the taskbar show them. windres runs inside OUT_DIR on short file names: it cannot take paths with spaces.
use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=../../manager_ui/icon.ico");
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("gnu") {
        println!("cargo:warning=not the GNU toolchain: the exe has no icon and no version info");
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::copy("../../manager_ui/icon.ico", out.join("icon.ico")).expect("copy icon.ico");
    let v = |k: &str| env::var(k).unwrap();
    let (major, minor, patch) = (v("CARGO_PKG_VERSION_MAJOR"), v("CARGO_PKG_VERSION_MINOR"), v("CARGO_PKG_VERSION_PATCH"));
    let rc = format!(
        "1 ICON \"icon.ico\"\n1 VERSIONINFO\nFILEVERSION {major},{minor},{patch},0\nPRODUCTVERSION {major},{minor},{patch},0\n\
         BEGIN\n BLOCK \"StringFileInfo\"\n BEGIN\n  BLOCK \"040904B0\"\n  BEGIN\n\
         \x20  VALUE \"FileDescription\", \"CP2077 AI Mod Manager\"\n   VALUE \"ProductName\", \"CP2077 AI Mod Manager\"\n\
         \x20  VALUE \"FileVersion\", \"{major}.{minor}.{patch}\"\n   VALUE \"ProductVersion\", \"{major}.{minor}.{patch}\"\n\
         \x20 END\n END\n BLOCK \"VarFileInfo\"\n BEGIN\n  VALUE \"Translation\", 0x409, 1200\n END\nEND\n"
    );
    fs::write(out.join("app.rc"), rc).expect("write app.rc");
    let ok = Command::new("windres")
        .current_dir(&out)
        .args(["app.rc", "-O", "coff", "-o", "app.res"])
        .status()
        .is_ok_and(|s| s.success());
    assert!(ok, "windres could not build the exe icon (put MinGW's bin folder on PATH)");
    println!("cargo:rustc-link-arg-bins={}", out.join("app.res").display());
}
