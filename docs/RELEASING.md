# Releasing FeverLink3

A release is a commit on `main` whose message starts with the version, built by GitHub Actions (`.github/workflows/release.yml`). Nothing is built or uploaded from a developer's PC. Every other commit only runs CI.

## Cut a release

1. Rename the `Unreleased` heading in `docs/CHANGELOG.md` to the version (its section becomes the release message and the changelog page). Make sure `main` is green in CI and `docs/ROADMAP.md` is current. Re-check the claims made in the listing and in `docs/SECURITY_FOR_USERS.md` against the code you are about to tag (the table is in `docs/nexus/README.md`): what a release says about the network, the files it creates and what is verified must match the tagged code.
2. Set the version (`[workspace.package] version` in `Cargo.toml`, for example `0.2.0-test1`) and commit. The same version appears in the DLL's `native.log`.
3. Make the release commit and push it to `main`. Its first line is `V<version>: <title>`, for example `V0.1.0: First beta release`. A version with a dash (`V0.2.0-test1: Friend test`) becomes a GitHub pre-release, which the updater skips by default. The workflow creates the tag `v<version>` itself. Only the head commit of a push is read, so the release commit must be the last one pushed.
4. The job runs the workspace tests, `cargo build --release`, then `feverlink3_release package`, signs the manifest if the secret `FEVERLINK3_SIGNING_KEY` is set, and creates the release with these assets:
   - `FeverLink3-<version>.zip`: the files to copy into the game's folder (`winhttp.dll`, `mods/release/feverlink3_1/`, no program; `feverlink3_release package --layout portable` makes the developer layout with `feverlink3.exe`), plus `README.txt` (the repository's `README.md`), `HOW-TO-PLAY.txt` (`docs/PLAYING.md`), `LICENSE`, `THIRD_PARTY_NOTICES.md`)
   - `FeverLink3-<version>.zip.sha256`
   - `manifest.json`: version, every file with size and SHA-256, and the zip's SHA-256
   - `manifest.json.sig`: Ed25519 signature over the exact bytes of `manifest.json` (hex)

   The zip SHA-256 is also in the release notes. A build without the secret is published unsigned and says so; the in-app updater refuses it.

The zip is reproducible: entries are sorted, timestamps are fixed to 1980-01-01 and permissions are fixed, so the same inputs give the same bytes.

## The signing key

The public key is embedded in the launcher (`UPDATE_PUBLIC_KEY_HEX` in `launcher/src/update.rs`). The private key signs `manifest.json` and nothing else.

**The value in the repository is a placeholder** whose private half is public (it is in the unit tests). Before the first release:

1. On an offline machine: `cargo run --release -p feverlink3_release -- keygen --out <folder outside the repo>`. This writes `feverlink3_release.key` (64 hex characters, secret) and `feverlink3_release.pub`.
2. Put the `.pub` value into `UPDATE_PUBLIC_KEY_HEX`, and replace `TEST_SEED_HEX` in the update tests with a fresh throwaway test seed (and its public key into the test constant) so the tests never use the real key.
3. Store the `.key` contents as the repository secret `FEVERLINK3_SIGNING_KEY`, and keep an offline backup (password manager plus a printed or USB copy). The key is never committed; `.gitignore` excludes `*.key`.
4. If the key leaks or is lost, ship a new launcher with a new public key by a normal manual download: old launchers cannot be moved to a new key by an update.

To sign by hand: `feverlink3_release sign --key <file> --manifest manifest.json` writes `manifest.json.sig` next to it.

## Verify a download by hand

PowerShell, in the folder with the files:

```powershell
(Get-FileHash FeverLink3-0.3.0.zip -Algorithm SHA256).Hash.ToLower()
Get-Content FeverLink3-0.3.0.zip.sha256
```

The two values must be equal, and equal to the SHA-256 in the release notes. The same hash is the `zip_sha256` field of `manifest.json`. The signature check is done by the launcher's updater (public key embedded in the exe); a hand check of `manifest.json.sig` needs an Ed25519 tool and the public key published on the project site.

## SmartScreen

The exe is not code-signed yet, so the first run on a PC shows "Windows protected your PC" (More info, then Run anyway). This appears once per PC at the first run of a browser-downloaded file. Files the launcher downloads itself for updates carry no mark-of-the-web, so updates do not trigger it. Code signing is the only real fix; the options (the SignPath Foundation's free programme probably does not accept the project's PolyForm Noncommercial licence, so a paid certificate is the realistic route), their cost and the checklist are in `docs/SIGNING.md`.
