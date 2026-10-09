//! Gives `feverlink3.exe` an icon, version information and an application manifest, so that Explorer, the Task
//! Manager and the file properties say what the program is (and an unsigned download looks like what it is).
//!
//! The resources are written by `resources/winres.rs` as a `.res` file and handed to the linker, which takes it as
//! it is. Nothing here needs the Windows SDK. Only the MSVC linker does that; for any other target the script does
//! nothing. It never fails the build: a program without an icon works, a build that stops over one helps nobody, and
//! the tests of `winres.rs` catch a broken icon or manifest long before.

#[path = "resources/winres.rs"]
mod winres;

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=resources");
    if let Err(message) = write_resources() {
        println!("cargo:warning=feverlink3.exe gets no resources (icon, version information, manifest): {message}");
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

fn write_resources() -> Result<(), String> {
    if env("CARGO_CFG_TARGET_OS") != "windows" || env("CARGO_CFG_TARGET_ENV") != "msvc" {
        return Ok(());
    }
    let dir = Path::new(&env("CARGO_MANIFEST_DIR")).join("resources");
    let manifest =
        std::fs::read_to_string(dir.join("feverlink3.manifest")).map_err(|e| format!("feverlink3.manifest: {e}"))?;
    let ico = match std::fs::read(dir.join("feverlink3.ico")) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            println!("cargo:warning=feverlink3.exe gets no icon: feverlink3.ico: {e}");
            None
        }
    };

    let part = |name: &str| env(name).parse::<u16>().unwrap_or(0);
    let version = env("CARGO_PKG_VERSION");
    let text = |name: &str, value: &str| (name.to_owned(), value.to_owned());
    let info = winres::VersionInfo {
        version: [part("CARGO_PKG_VERSION_MAJOR"), part("CARGO_PKG_VERSION_MINOR"), part("CARGO_PKG_VERSION_PATCH"), 0],
        prerelease: !env("CARGO_PKG_VERSION_PRE").is_empty(),
        strings: vec![
            text("CompanyName", &env("CARGO_PKG_AUTHORS")),
            text("FileDescription", "FeverLink3 launcher"),
            text("FileVersion", &version),
            text("InternalName", "feverlink3"),
            text("LegalCopyright", "Copyright (c) FeverLink3 contributors. MIT licence."),
            text("OriginalFilename", "feverlink3.exe"),
            text("ProductName", "FeverLink3"),
            text("ProductVersion", &version),
            text(
                "Comments",
                &format!("Cooperative multiplayer for Transport Fever 3. {}", env("CARGO_PKG_REPOSITORY")),
            ),
        ],
    };

    let res = winres::resource_file(ico.as_deref(), &info, &manifest)?;
    let out = PathBuf::from(env("OUT_DIR")).join("feverlink3.res");
    std::fs::write(&out, res).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("cargo:rustc-link-arg-bins={}", out.display());
    Ok(())
}
