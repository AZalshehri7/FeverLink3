# Releasing FeverLink3

A release is a commit on `main` whose message starts with the version, built by GitHub Actions (`.github/workflows/release.yml`). Nothing is built or uploaded from a developer's PC. Every other commit only runs CI.

## Cut a release

1. Rename the `Unreleased` heading in `docs/CHANGELOG.md` to the version (its section becomes the release message and the changelog page). Make sure `main` is green in CI and `docs/ROADMAP.md` is current. Re-check the claims made in the listing and in `docs/SECURITY_FOR_USERS.md` against the code you are about to tag (the table is in `docs/nexus/README.md`): what a release says about the network, the files it creates and what is verified must match the tagged code.
2. Set the version (`[workspace.package] version` in `Cargo.toml`, for example `0.2.0`) and commit. The same version appears in the DLL's `native.log`.
3. Make the release commit and push it to `main`. Its first line is `V<version>: <title>`, for example `V0.1.0: First beta release`. A version with a dash (`V0.2.0-beta.1: Friend test`) becomes a GitHub pre-release, which the updater skips by default. The workflow creates the tag `v<version>` itself. Only the head commit of a push is read, so the release commit must be the last one pushed.
4. The workflow has three jobs. `build` (no secrets) runs the workspace tests (the essential set, the same as CI), `cargo build --release` (with `FEVERLINK3_OFFICIAL_BUILD=1`, which lets the module update itself in the game: "Automatic updates" below), then `feverlink3_release package`. **Releases are unsigned until 1.0.0 (owner decision 2026-10-22):** `sign` is skipped unless the repository variable `RELEASE_SIGNING` is `on`, and `publish` then runs without it. When it is on, `sign` runs in the protected environment `release-signing` and waits for the owner's approval in the Actions tab ("Review deployments"); it signs `manifest.json` with the environment secret `RELEASE_SIGNING_KEY` and checks the signature with `feverlink3_release verify`, which is the launcher's own check against its `TRUSTED_KEYS` (and the zip hash), so a key the launcher would refuse stops the release. `publish` creates the release with these assets:
   - `FeverLink3-<version>.zip`: the files to copy into the game's folder (`winhttp.dll`, `mods/release/feverlink3/`, no program; `feverlink3_release package --layout portable` makes the developer layout with `feverlink3.exe`), plus `README.txt` (the repository's `README.md`), `HOW-TO-PLAY.txt` (`docs/PLAYING.md`), `LICENSE`, `THIRD_PARTY_NOTICES.md`)
   - `FeverLink3-<version>.zip.sha256`
   - `manifest.json`: version, every file with size and SHA-256, and the zip's SHA-256
   - `manifest.json.sig` (only once signing is on): the detached signature, one line `feverlink3-ed25519 <key id> <Ed25519 signature over the exact bytes of manifest.json, hex>`. The key id is the first 8 bytes of the public key's SHA-256. The signature covers the zip through its hash in the manifest.

   The zip SHA-256 is also in the release notes. Until 1.0.0 every release (with or without a dash) is published without `manifest.json.sig` and its notes say so; the in-game updater accepts it on the hash checks alone ("Automatic updates" below, the trust model in `docs/SECURITY_FOR_USERS.md`). Once `RELEASE_SIGNING` is `on`, a release without a signature fails, because updaters that trust a key refuse unsigned releases.
The zip is reproducible: entries are sorted, timestamps are fixed to 1980-01-01 and permissions are fixed, so the same inputs give the same bytes.

## Tests: the essential set and the full set

CI and the release job run the essential set only (a few minutes): `cargo test` without ignored tests, and the lupa suite
without `FEVERLINK3_FULL_TESTS`. The full set adds the long simulations, the wider variants and the tests with tight
wall-clock bounds; it runs on a developer's PC on demand, for example before a release candidate or after a change to the
lockstep, the transfer or the native hooks:

```
cargo test --workspace --exclude feverlink3_native -- --include-ignored
cargo test -p feverlink3_native -- --test-threads=1 --include-ignored
$env:FEVERLINK3_FULL_TESTS = '1'; python -I -m unittest discover -s tests/lua -p "test_*.py"
```

A tight wall-clock test of the full set can fail on a busy PC; run it again alone before taking it for a regression.

## The signing key

**Unsigned until 1.0.0 (owner decision 2026-10-22).** Everything in this section is kept for 1.0.0 and is not in use before. To switch signing on: add the key to `TRUSTED_KEYS` (step 2 below), set up the environment (step 3), and set the repository variable `RELEASE_SIGNING` to `on` (Settings, Secrets and variables, Actions, Variables) in the same release. From that release on, every updater that has the key refuses an unsigned release.

The launcher trusts the public keys in `TRUSTED_KEYS` in `launcher/src/update.rs`, each with a last valid day. **The list is empty in the repository**, so every update is refused until the owner adds the real key. (The former placeholder key's private half is public, it is the tests' seed; a test makes sure it is never trusted.) The private key signs `manifest.json` and nothing else, and only inside the `sign` job.

Owner's steps before the first signed release (GitHub settings cannot be changed from the repository):

1. On an offline machine: `cargo run --release -p feverlink3_release -- keygen --out <folder outside the repo>`. This writes `feverlink3_release.key` (64 hex characters, secret) and `feverlink3_release.pub`, and prints the public key, its key id and the line to paste.
2. Add that line to `TRUSTED_KEYS`, with an expiry about two years ahead, for example `TrustedKey { name: "release key 2026", public_hex: "<64 hex>", expires: (2028, 12, 31) }`, and commit it through a normal commit (the launcher that trusts the key must be released before updates signed with it can be installed).
3. GitHub, repository Settings, Environments, New environment `release-signing`:
   - Required reviewers: the owner (and "Prevent self-review" off, since the owner both pushes and approves).
   - Deployment branches and tags: "Selected branches and tags", rule `main` only.
   - Allow administrators to bypass: off.
   - Environment secret `RELEASE_SIGNING_KEY`: the contents of the `.key` file.
4. Delete the old repository secret `FEVERLINK3_SIGNING_KEY` if it exists (repository secrets reach every job of every workflow; the workflow no longer reads it).
5. Keep an offline backup of the `.key` (password manager plus a printed or USB copy). The key is never committed; `.gitignore` excludes `*.key`.

Rotation: generate a new key, add it to `TRUSTED_KEYS` next to the old one and release that launcher (signed with the old key); then replace the environment secret with the new key. The old entry is removed later or simply expires. If a key leaks, remove it from `TRUSTED_KEYS` at once and replace the secret; launchers that still trust it stay exposed until its expiry, which is why the expiry is kept short. If every key a launcher trusts has expired, it refuses all updates and the player downloads a new launcher by hand: the safe failure.

To sign and check by hand: `feverlink3_release sign --key <file> --manifest manifest.json` writes `manifest.json.sig` next to it; `feverlink3_release verify --manifest manifest.json` checks it (and the zip beside it) against the launcher's `TRUSTED_KEYS`.

## The updater (`feverlink3 update`)

The developer program's `update` command (players use the automatic update below). It still requires a signature, so it refuses every release until 1.0.0. The launcher program's `update` command (`launcher/src/updatecmd.rs`, the checks in `launcher/src/update.rs`) runs only when typed:

- `feverlink3 update --check [--pre]` asks the GitHub API for the newest release (with `--pre` also pre-releases) and says whether it is newer, or why a newer one cannot be installed (for example no `manifest.json.sig`).
- `feverlink3 update [--pre]` downloads `manifest.json` and `manifest.json.sig`, checks the signature (trusted key, not expired, version newer than the running one, same version and zip name as the release), only then downloads the zip and checks its hash, refuses while Transport Fever 3 or another FeverLink3 program (a session) runs, shows what was verified and installs only after the player types `yes`. `--from DIR` does the same with the three files already in a folder.
- A portable release (with `feverlink3.exe`) goes into the exe's folder. The new program is then started with `update --probe` and must report its version within 30 s; otherwise the previous files come back from their `.old` copies and the version goes on a skip list (`feverlink3_update_skip.txt`).
- An overlay release (what the release job publishes) goes into the game folder: `winhttp.dll` and `mods/...` only, the documents are not copied there. The game cannot be started for the check, so the player starts it, then runs `feverlink3 update --confirm` or `--rollback`; until then no other update is installed.
- Downloads use the system's WinHTTP inside the process (`launcher/src/https.rs`, shared with the automatic update): HTTPS only, GitHub's hosts only (also for redirects), size and time limits; no HTTP crate is in the program.
## Automatic updates (in the game)

The module updates itself; players never run `feverlink3.exe` (owner decision 2026-10-22). The code is `launcher/src/autoupdate.rs`, its documentation comment has the whole order and the reasons; the player's view and the trust model are in `docs/SECURITY_FOR_USERS.md`, "Automatic updates". In short:

- **Which builds update themselves:** only modules built by the release workflow (`FEVERLINK3_OFFICIAL_BUILD=1` at `cargo build`). A module built on a developer's PC never replaces itself with GitHub's release (so a developer's test installs stay as they are), except for the real test below.
- **Check:** once per game start, on its own thread, not before 2 minutes after the module loaded (`FIRST_CHECK_AFTER`), never while a session runs: `https://api.github.com/repos/AZalshehri7/FeverLink3/releases/latest` (full releases only).
- **Trust until 1.0.0 (unsigned):** HTTPS (WinHTTP inside the game process, certificate checked by Windows, GitHub's hosts only, also for redirects; no program is ever started from the game: `launcher/tests/no_child_process.rs`), files only from `https://github.com/AZalshehri7/FeverLink3/releases/download/<tag>/`, version strictly newer, the zip's SHA-256 equal to `manifest.json` and to the `.sha256` asset, every extracted file equal to the manifest (size and SHA-256), names only `winhttp.dll` and `mods/release/feverlink3/**`. As soon as `TRUSTED_KEYS` has a key, the signature is required again (`update::accept_manifest`; a unit test covers both modes). So every release must publish `manifest.json` and the `.sha256` file (the workflow does).
- **Apply order:** staged in `<game>\feverlink3_update\<version>\` and the loaded `winhttp.dll` renamed to `winhttp.dll.old-<old>` with the new one put in place, in the same run (the run keeps old DLL + old mod in memory, so it can still play); at the next start the NEW DLL's `DllMain` swaps `mods\release\feverlink3` (old one to `feverlink3_update\backup-<old>\`) before the game reads mods; after 30 s in the game the backup and the old DLL are deleted. One restart; no run mixes versions. A run whose files on disk do not match its DLL refuses Host and Join.
- **Rollback:** a new version that does not stay up 30 s in two starts, or a mod swap that fails, puts the old DLL and mod back and skips the version (`feverlink3_update_skip.txt`).
- **Compatibility rule:** every later version's `DllMain` must keep calling `autoupdate::start` with the same `feverlink3_update\state.json`: the old version downloads, the new version finishes.
- **Logs:** `native.log`, lines starting with `update:`.

## Testing the auto update

Right after a release (0.2.1 or later; 0.2.0 was withdrawn) is published on GitHub (a full release, not a pre-release), on the owner's PC, with no session running:

1. Build the module of the commit that was released, on the PC: `cargo build --release -p feverlink3_native`. This build is not an official one, so it would not update itself.
2. Install it like a player: close the game, copy `target\release\feverlink3_native.dll` as `winhttp.dll` and `mod\feverlink3` as `mods\release\feverlink3` into the game folder.
3. Make it believe it is older: create `<game folder>\feverlink3_pretend_version.txt` with one line `0.1.9` (or set the user environment variable `FEVERLINK3_PRETEND_VERSION=0.1.9` and restart Steam so the game inherits it; remove the variable after the test, the file removes itself). No world needs to be loaded: everything happens in the main menu.
4. Start the game from Steam and wait about three minutes in the main menu (the check starts 2 minutes after the game started). The game must start normally, with no "Internal error" (the 0.2.1 crash). Expect in `%LOCALAPPDATA%\FeverLink3\native.log`: `update: downloading 0.2.1 (compared with 0.1.9)`, then `update: 0.2.1 staged and winhttp.dll replaced`. The window (Play together) shows "FeverLink3 was updated to 0.2.1: restart the game to finish". In the game folder: `winhttp.dll` now has the SHA-256 of the release's (compare with `manifest.json`), `winhttp.dll.old-0.2.1` is the local build, `feverlink3_update\0.2.1\` holds the new mod, `feverlink3_pretend_version.txt` is gone.
5. Close and start the game again. Expect `FeverLink3 native 0.2.1 loaded` and `update: at start Confirm`, then about 30 s later `update: 0.2.1 started well`; the window shows "FeverLink3 is now 0.2.1". `mods\release\feverlink3` is the release's (its files match `manifest.json`), and `feverlink3_update\`, `winhttp.dll.old-0.2.1` are gone. The next start logs `update: up to date`.
6. Rollback check (optional): repeat 2 to 4, then make the next two starts fail: end `TransportFever3.exe` in Task Manager within 20 s of starting it, twice (a start counts as good 30 s after the module loaded). The third start should log `at start Restart`, put the local build's DLL and mod back, show "Update failed: FeverLink3 0.2.1 did not start properly; you keep 0.2.1" and refuse Host/Join until a restart. Delete `feverlink3_update_skip.txt` afterwards, or 0.2.1 is never offered again on this PC.
7. Switch check: Troubleshooting, "Automatic updates: on" -> off; `%LOCALAPPDATA%\FeverLink3\settings.txt` says `autoupdate=off` and the next start logs `update: no check`.

## Verify a download by hand

PowerShell, in the folder with the files:

```powershell
(Get-FileHash FeverLink3-0.3.0.zip -Algorithm SHA256).Hash.ToLower()
Get-Content FeverLink3-0.3.0.zip.sha256
```

The two values must be equal, and equal to the SHA-256 in the release notes. The same hash is the `zip_sha256` field of `manifest.json`. Until 1.0.0 there is no signature to check. From then on the signature check is done by the launcher's updater (trusted public keys embedded in the exe), or by hand with `feverlink3_release verify --manifest manifest.json` from a checkout of the tagged source.

## SmartScreen

The exe is not code-signed yet, so the first run on a PC shows "Windows protected your PC" (More info, then Run anyway). This appears once per PC at the first run of a browser-downloaded file. Files the launcher downloads itself for updates carry no mark-of-the-web, so updates do not trigger it. Code signing is the only real fix; the options (the SignPath Foundation's free programme probably does not accept the project's PolyForm Noncommercial licence, so a paid certificate is the realistic route), their cost and the checklist are in `docs/SIGNING.md`.
