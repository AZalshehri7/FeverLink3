//! Release tool. Run by `.github/workflows/release.yml`; see `docs/RELEASING.md`.
//!
//! ```text
//! feverlink3_release package --version V --out DIR [--root DIR] [--layout overlay|portable]
//! feverlink3_release sign    --key FILE --manifest FILE
//! feverlink3_release verify  --manifest FILE      (against the launcher's TRUSTED_KEYS, and the zip beside it)
//! feverlink3_release keygen  --out DIR
//! ```

mod manifest;
mod package;
mod sign;

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn required(args: &[String], name: &str) -> Result<String, String> {
    flag(args, name).ok_or_else(|| format!("missing {name}"))
}

fn run(args: &[String]) -> Result<(), String> {
    let rest = args.get(1..).unwrap_or(&[]);
    match args.first().map(String::as_str) {
        Some("package") => {
            let version = required(rest, "--version")?;
            let out = PathBuf::from(required(rest, "--out")?);
            let root = PathBuf::from(flag(rest, "--root").unwrap_or_else(|| ".".into()));
            let inputs = package::Inputs::from_root(&root);
            // the default is what players get: no program, the files to copy into the game folder
            let layout = match flag(rest, "--layout").as_deref() {
                None | Some("overlay") => package::Layout::Overlay,
                Some("portable") => package::Layout::Portable,
                Some(other) => return Err(format!("unknown layout {other:?} (overlay or portable)")),
            };
            let built = package::package_with(&inputs, &version, &out, layout)?;
            println!("{}", built.zip_path.display());
            println!("sha256 {}", built.manifest.zip_sha256);
            Ok(())
        }
        Some("sign") => {
            let key = PathBuf::from(required(rest, "--key")?);
            let manifest = PathBuf::from(required(rest, "--manifest")?);
            let sig = sign::sign_file(&key, &manifest)?;
            println!("{}", sig.display());
            Ok(())
        }
        Some("verify") => {
            // the release job runs this after signing: a signature the launcher would refuse fails the release
            let manifest = PathBuf::from(required(rest, "--manifest")?);
            let key = sign::verify_file(
                &manifest,
                feverlink3_launcher::update::TRUSTED_KEYS,
                feverlink3_launcher::update::now_unix(),
            )?;
            println!("signature ok (trusted key {key:?}); zip matches the manifest");
            Ok(())
        }
        Some("keygen") => {
            let out = PathBuf::from(required(rest, "--out")?);
            let public = sign::keygen(&out)?;
            println!("public key: {public} (key id {})", sign::key_id_of(&public)?);
            println!("add it to TRUSTED_KEYS in launcher/src/update.rs:");
            println!("    TrustedKey {{ name: \"release key\", public_hex: \"{public}\", expires: (YYYY, MM, DD) }},");
            Ok(())
        }
        _ => Err("usage: feverlink3_release package --version V --out DIR [--root DIR] [--layout overlay|portable] | sign --key FILE --manifest FILE | verify --manifest FILE | keygen --out DIR".into()),
    }
}
