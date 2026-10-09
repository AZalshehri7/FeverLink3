---
title: FeverLink3
---

# FeverLink3

**Cooperative multiplayer for Transport Fever 3.** Several players build in one world at the same time, each in their own copy of the game, over the internet or a LAN.

> **Pre-alpha.** Nothing here plays a game yet. This page will carry the download, the how-to-play guide and the known issues once the first playable version exists. Until then, the repository is where things happen.

[Source on GitHub](https://github.com/AZalshehri7/FeverLink3) · [Roadmap](ROADMAP.html) · [Architecture](ARCHITECTURE.html) · [Protocol](PROTOCOL.html) · [Security](SECURITY.html)

## How it will work

Transport Fever 3 has no network code, so FeverLink3 keeps every copy of the world identical instead of sending the world around:

- **Commands, not state.** Every action is captured, stamped with a future simulation step, and applied at that step on every game, including the one where the player clicked.
- **Host authority.** Games compare world checksums every few seconds. A lasting difference is repaired by reloading the host's savegame everywhere.
- **Sealed transport.** A join code is the credential. Everything on the wire is encrypted and authenticated.
- **No installer.** One zip, one exe, automatic updates. The launcher places a small module in the game folder and the mod in your mods folder, and removes both on request.

## Credits

The design follows [TpF2 Multiplayer](https://github.com/silver2127/tpf2-multiplayer) by silver2127 (Transport Fever 2) and [MPFever](https://github.com/dkdknight/MPFever) by dkdknight (Transport Fever 3). FeverLink3 is a fresh implementation; see the [credits in the README](https://github.com/AZalshehri7/FeverLink3#credits).

## Contact

Discord: `azalshehri`. Bugs and requests: [GitHub issues](https://github.com/AZalshehri7/FeverLink3/issues). Want to be in the first test sessions? Message me on Discord.

## Licence

MIT.
