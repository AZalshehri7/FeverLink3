# Code signing: plan, application text and checklist

Written on 2026-10-10 from public pages read that day (addresses at the end). Rules and prices change; the pages are the authority, this document is the preparation. Nothing here has been applied for yet.

## Why sign, and what signing will not do

The release has two programs that Windows and security products look at: `feverlink3.exe` and `winhttp.dll`. Neither is signed, which means:

- Windows SmartScreen shows "Windows protected your PC" the first time `feverlink3.exe` is run from a browser download.
- Antivirus heuristics are less forgiving of an unsigned, rarely seen proxy DLL that sits next to a game (`docs/SECURITY_FOR_USERS.md` explains why).
- Windows 11 Smart App Control, in its "On" setting, blocks programs and DLLs that Microsoft does not recognise unless they are signed with a trusted certificate. The game loads `winhttp.dll` by itself at start, so a block would stop the game from starting. We have not tested this.

What signing does not do: it does not switch SmartScreen off at once (reputation is built from downloads over time, for a new publisher too), and it does not stop every antivirus false positive. It makes the publisher visible, ties later builds to the same identity, and is the route that Microsoft's SmartScreen documentation points to.

What would be signed: the two PE files, `feverlink3.exe` and `winhttp.dll`, before they go into the zip. A zip cannot carry an Authenticode signature. In the end state the installer is only a testing tool and the native module is delivered separately (`docs/WORKSHOP.md`), so `winhttp.dll` is the file that matters most for players, and the one the game loads by itself. The release manifest already has its own Ed25519 signature for the in-app updater (`docs/RELEASING.md`); that is separate and stays.

## The options

| Option | Cost | Who can use it | Verdict |
|---|---|---|---|
| **SignPath Foundation** (free code signing for open-source projects) | free of charge for open-source projects | open-source projects that meet the conditions below; the Foundation decides and is "under no obligation to accept" | Best fit. Apply once a release exists |
| **Azure Artifact Signing** (formerly Trusted Signing) | Basic tier: 5,000 signatures a month; the amounts are not readable on Microsoft's pricing page (see below) | Public Trust certificates: organisations in the US, Canada, the EU, the UK, Australia, New Zealand, Japan, South Korea, Singapore, Switzerland, Norway and Israel; individuals only in the US and Canada | Not available to this project's maintainer, whose country is not on the list |
| **Certum "Open Source Code Signing"** (cloud) | listed from EUR 49 in the shop; the term is not stated there | natural persons; identity verification with an ID; the project must be visible on a public website | Possible fallback, with caveats below |
| **Commercial OV / EV certificates** from other authorities | not researched here; get quotes | organisations or individuals after validation; the key must be on hardware or in a cloud HSM | Last resort |
| **Self-signed** | free | anyone | Useless: Windows does not trust it |

## SignPath Foundation

Page: https://signpath.org (the terms page, https://signpath.org/terms, is marked "Draft" by the Foundation, so read it again before applying).

### The conditions, as the terms page states them

Free open-source subscription:

1. "The project must not contain malware or potentially unwanted programs."
2. "The project must use an OSI-approved Open Source license without commercial dual-licensing for all components."
3. "The project may not contain any proprietary, non open-source component."
4. "The project must be actively maintained."
5. "The project must already be released in the form that should be signed."
6. The functionality must be described "on its download page or in the app store entry".

What can be signed: the signing team must also be the development team; only software built from the team's own source code; no features "designed to identify or exploit security vulnerabilities"; no features that "compromise the privacy or security of users and their systems"; no changes to "the user's system configuration without proper warnings"; software with installation facilities must have instructions or automated facilities for uninstallation.

Team: every team member uses multi-factor authentication for SignPath and for the source repository; the roles are authors, reviewers (changes from non-committers must be reviewed by a team member) and approvers (every signing request is approved by a trusted team member; "every release needs manual approval for signing").

Project home page: must contain the words "Code signing policy" and the sentence "Free code signing provided by SignPath.io, certificate by SignPath Foundation.", list the roles and their members, and link a privacy policy (or state that the program does not transfer information to other networked systems unless the user requests it).

Build: the binaries must be built from the source "in a verifiable way" ("origin verification") on a trusted build system such as GitHub Actions; build settings come from a file in source control, not from overrides in the CI job; earlier builds are not reused from a cache; build scripts and CI configuration count as source code for reviews. The artifact metadata must be set and enforced: the product name must match the project name and the product version must be the same in each build.

The Foundation can pause or end a subscription and revoke the certificate, even retroactively, if the Code of Conduct is broken, and accepts no liability for software signed with its certificates. Applicants must help with investigations.

### How FeverLink3 measures up

| Condition | State today | What to do |
|---|---|---|
| OSI-approved licence | MIT (`LICENSE`) | nothing |
| No proprietary component | the dependencies are Rust crates; the game's `steam_api64.dll` is used from the player's installation and not shipped | before applying, list the licences of all crates in `Cargo.lock` (for example with `cargo-license`) |
| Actively maintained | the repository started on 2026-10-09 and is changing daily | the terms set no minimum age or popularity, but a project that is days old is a risk; apply after a few weeks of releases if the first reply is a no |
| Already released in the form to be signed | **no release yet** | publish the pre-release `v0.2.0-test1` (unsigned) first |
| Functionality described on the download page | the README, the site (`docs/index.md`) and the release notes describe it | link them in the application |
| No malware, no features that compromise users | the proxy-DLL technique draws questions; `docs/SECURITY_FOR_USERS.md` explains it file by file | link that page, and say plainly that it is not a cheat and not a security tool: it adds multiplayer to a single-player game |
| No system changes without warnings | the installer writes into the game folder and the Steam user folder, prints each change, supports `--dry-run` | said in the README and `docs/PLAYING.md` |
| Uninstallation | `feverlink3 uninstall` | nothing |
| Multi-factor authentication | the owner's accounts | switch it on for GitHub and for SignPath |
| Roles | one maintainer | list the owner as author, reviewer and approver |
| "Code signing policy" on the home page | not published (it would be false before acceptance) | text below, to be added after acceptance |
| Privacy policy | `docs/site/privacy.md` | link it |
| Trusted build system, build fully determined by the repository | the release workflow builds on GitHub Actions from a tag; the Rust toolchain is not pinned (`dtolnay/rust-toolchain@stable`) | pin the toolchain version in the repository and build with `--locked` |
| Metadata set and enforced (product name, version) | **the exe and the DLL carry no version information resource** | add a version resource with product name `FeverLink3` and the release version to both (a build step, for the hardening work) |

### What to enter in the application

The form, as a third party described it in 2026 (check the live form, https://signpath.org/apply): the project's repository address, the licence, the address where users get the builds, and a description of the project and of the artifact types. A reachable contact email is needed because the follow-up is by email.

Draft text for the description:

> FeverLink3 is an open-source (MIT) cooperative multiplayer mod for the PC game Transport Fever 3 (Steam). It has three parts: a Lua game mod; a small Windows DLL, `winhttp.dll`, that the game loads from its own folder and that runs the multiplayer session inside the game; and a command-line installer and development tool, `feverlink3.exe`. Releases are zip files built by GitHub Actions from a tag. We would sign the two PE files, `feverlink3.exe` and `winhttp.dll`, before they are packaged.
>
> The DLL is a proxy: it passes the 14 WinHTTP functions that the game imports to the system library unchanged, and starts the session controller. It does not inject into other processes, persist, collect data or download code. The page https://github.com/AZalshehri7/FeverLink3/blob/main/docs/SECURITY_FOR_USERS.md lists everything it does and does not do and how to verify and remove it. The installer changes only the game folder and the user's game data folder, prints every change, supports `--dry-run`, and has an uninstall command.
>
> It is a fresh implementation that builds on the published ideas and findings of two earlier MIT-licensed projects, TpF2 Multiplayer (silver2127) and MPFever (dkdknight), which are credited in the README and in THIRD_PARTY_NOTICES.md.
>
> The project started in October 2026 and is maintained by one person. We apply because an unsigned DLL in a game folder triggers SmartScreen, Windows 11 Smart App Control and antivirus heuristics for our users.
>
> Repository: https://github.com/AZalshehri7/FeverLink3 - Releases: https://github.com/AZalshehri7/FeverLink3/releases - Licence: MIT.

### The "Code signing policy" text to publish after acceptance

Do not publish this before the Foundation has accepted the project: it would be a false statement.

> ## Code signing policy
>
> Free code signing provided by [SignPath.io](https://signpath.io), certificate by [SignPath Foundation](https://signpath.org).
>
> Signed files: `feverlink3.exe` and `winhttp.dll`, built by GitHub Actions from the tagged commit of this repository. Releases are not built or signed on any developer's PC.
>
> Team roles: Authors - Abdulaziz Alshehri. Reviewers - Abdulaziz Alshehri. Approvers - Abdulaziz Alshehri. Every signing request is approved by hand.
>
> Privacy: see the [privacy notes](site/privacy.html).
>
> A binary that carries this signature but does not match the hashes in the release's `manifest.json`, or that you did not get from the release page, is not ours: please do not run it and report it with its SHA-256 and the place you got it from.

### Changes to the release workflow once accepted

For the hardening work or the owner; none is made here.

1. The build job uploads the unsigned `feverlink3.exe` and `winhttp.dll` as a GitHub Actions artifact and submits them to SignPath with SignPath's GitHub action (see the GitHub trusted build system page of docs.signpath.io); the owner approves the request in SignPath; the job downloads the signed files.
2. `tools/release` packages the signed files: it reads `target/release/feverlink3.exe` and `feverlink3_native.dll` today (`tools/release/src/package.rs`), so it needs a way to take the signed files instead.
3. The manifest is still signed with the Ed25519 key afterwards; its hashes then cover the signed binaries.
4. Pin the Rust toolchain, build with `--locked`, add the version resources.

## Azure Artifact Signing, as the alternative

Microsoft renamed Trusted Signing to Artifact Signing. The quickstart (updated 2026-10-08) states: "Public Trust certificates are available to organizations in the United States, Canada, the European Union, the United Kingdom, Australia, New Zealand, Japan, South Korea, Singapore, Switzerland, Norway, and Israel. Individual developers must be located in the United States or Canada. These geographic restrictions do not apply to Private Trust certificates." Private Trust certificates are not publicly trusted, so they do not help against SmartScreen or Smart App Control.

- **Eligibility:** this project's maintainer is not located in any listed country, so the service is closed to the project. It would open only if the list gains the maintainer's country or if a legal entity in a listed country were to sign, which is not planned.
- **Price:** Microsoft's pricing page shows two tiers, Basic (5,000 signatures a month, one certificate profile of each type) and Premium (100,000 signatures a month, ten of each); the dollar amounts are filled in by a script and could not be read when this was written. Third-party posts report about US$9.99 a month for Basic. Check the Azure pricing calculator. Charges start when the account is created.
- **Validation:** organisations: 1 to 20 business days with business documents; individuals: a government ID checked through a third party (AU10TIX) with a selfie, and the Azure billing account must be of the "Individual" type with the legal name and address matching the ID.

## Certum and commercial certificates

- **Certum "Open Source Code Signing" in the cloud:** the shop lists it from EUR 49 (the term is not stated), for natural persons only, with identity verification and a public website for the project. The name in the certificate is "Open Source Developer". Reports about how Windows treats this certificate conflict: Certum says Microsoft trusts it, an older third-party issue thread questions its Authenticode support. Test by signing a throwaway executable with SHA-256 and checking it on a clean Windows machine before paying. The key stays in Certum's HSM and each login needs a one-time code from a phone, which fits a manual signing session and does not fit an unattended GitHub Actions job; signing on the maintainer's PC would break the rule that nothing is built or signed on a developer's PC.
- **Commercial OV and EV certificates:** not researched here. Since 2023 the certificate authorities require the private key to be on hardware or in a cloud HSM, which pushes the price up and makes CI use awkward. Ask for written quotes if the free routes fail.

## Decisions for the owner

1. Approve applying to the SignPath Foundation after `v0.2.0-test1` is published.
2. Switch on multi-factor authentication for the GitHub account and for SignPath.
3. Approve the version-resource work (for the exe and the DLL) and the toolchain pinning.
4. Choose the fallback if SignPath refuses or does not answer within a few weeks: Certum, or no signing and a plain explanation (`docs/SECURITY_FOR_USERS.md`).

## Sources (all read on 2026-10-10)

- SignPath Foundation: https://signpath.org , terms (draft): https://signpath.org/terms , application: https://signpath.org/apply
- SignPath origin verification: https://docs.signpath.io/origin-verification
- An accepted project's code signing policy page, as an example: https://super-productivity.com/code-signing/
- Application experience, by a third party: https://zenn.dev/shm_7ec/articles/signpath-oss-code-signing
- Azure Artifact Signing quickstart: https://learn.microsoft.com/en-us/azure/artifact-signing/quickstart , pricing: https://azure.microsoft.com/en-us/pricing/details/artifact-signing/
- SmartScreen reputation: https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/smartscreen-reputation
- Smart App Control: https://learn.microsoft.com/en-us/windows/apps/develop/smart-app-control/overview
- Certum open-source certificate: https://shop.certum.eu/open-source-code-signing-on-simplysign.html
