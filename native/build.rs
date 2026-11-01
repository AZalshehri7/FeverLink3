//! Gives the native module (shipped as `winhttp.dll`) version information, so that the file properties, and the
//! scanners that read them, say what the file is: FeverLink3's module, not a nameless DLL in a game folder (0.2.12:
//! Windows Defender's cloud verdict removed the official release's DLL, which had no version information and no
//! publisher, as `Trojan:Win32/Posilod.CA!cl`).
//!
//! The `.res` file is written by the launcher's `resources/winres.rs` (shared with `launcher/build.rs`, where its tests
//! run) and handed to the linker for the cdylib only. As there, nothing here needs the Windows SDK, and the script
//! never fails the build.

#[path = "../launcher/resources/winres.rs"]
mod winres;

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../launcher/resources/winres.rs");
    if let Err(message) = write_resources() {
        println!("cargo:warning=the native module gets no version information: {message}");
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

fn write_resources() -> Result<(), String> {
    if env("CARGO_CFG_TARGET_OS") != "windows" || env("CARGO_CFG_TARGET_ENV") != "msvc" {
        return Ok(());
    }
    let part = |name: &str| env(name).parse::<u16>().unwrap_or(0);
    let version = env("CARGO_PKG_VERSION");
    let text = |name: &str, value: &str| (name.to_owned(), value.to_owned());
    let info = winres::VersionInfo {
        version: [part("CARGO_PKG_VERSION_MAJOR"), part("CARGO_PKG_VERSION_MINOR"), part("CARGO_PKG_VERSION_PATCH"), 0],
        prerelease: !env("CARGO_PKG_VERSION_PRE").is_empty(),
        dll: true,
        strings: vec![
            text("CompanyName", &env("CARGO_PKG_AUTHORS")),
            text("FileDescription", "FeverLink3 native module for Transport Fever 3"),
            text("FileVersion", &version),
            text("InternalName", "feverlink3_native"),
            text(
                "LegalCopyright",
                &format!("Copyright (c) FeverLink3 contributors. Licensed under {}.", env("CARGO_PKG_LICENSE")),
            ),
            // the name the game loads it under (the release puts it next to the game's program)
            text("OriginalFilename", "winhttp.dll"),
            text("ProductName", "FeverLink3"),
            text("ProductVersion", &version),
            text(
                "Comments",
                &format!(
                    "Cooperative multiplayer for Transport Fever 3; loaded by the game from its own folder. {}",
                    env("CARGO_PKG_REPOSITORY")
                ),
            ),
        ],
    };
    let res = winres::version_file(&info)?;
    let out = PathBuf::from(env("OUT_DIR")).join("feverlink3_native.res");
    std::fs::write(&out, res).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("cargo:rustc-link-arg-cdylib={}", out.display());
    Ok(())
}
