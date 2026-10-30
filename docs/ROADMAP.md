# Roadmap

Status as of 2026-10-30. The project started on 2026-10-09. What changed in each version is in the [changelog](CHANGELOG.html); what has been seen working in the real game is on the [status page](site/status.html).

## Done

**0.1.0 (2026-10-14): the first working session.** Host and join from the in-game window; builds copied from the game's own build command and applied at the same step on every game (roads, rail, stations, stops, depots, bulldozing); lines, vehicles, renames and loans; world checks and the repair from the host's world. Seen with two games on one PC.

**0.1.5 (2026-10-19): sessions over Steam.** Two PCs over Steam with a join code, Steam invites, the mod check at join, catching up after a stall. Played over the internet with a friend through Steam; that session showed problems (object id order, held builds, a crash) that 0.2.x fixes. Steam invites were not tried between two PCs.

**0.2.1 (2026-10-22): first public beta** (0.2.0 was withdrawn). A new in-game interface, Report a problem, a faster session start, automatic updates inside the game, and many fixes. Seen in the real game:

- Played over the internet with a friend through Steam, in sessions of up to 3.5 hours (with the build before the release): building together, towns growing, shared speed, pause and chat, the new interface and Report a problem. In the longest session, 11 repair syncs: 1 for the join, 5 because another mod (Auto Line Namer) renamed lines on its own, 5 from object ids in a different order on the two games (each found within seconds and repaired automatically): about one such repair every 40 minutes.
- Automatic updates on the maintainer's PC: 0.2.1 downloaded, checked and installed itself.

Not yet seen in the real game at that time: joining from a Steam invite by itself (fixed in 0.2.1; seen between two PCs on 2026-10-23), the warning about mods that change the world on their own, putting the previous version back when an update does not start.

**0.2.2 and 0.2.3 (2026-10-22): faster builds and fixes.** A road stands on both games 1.4 s after the click at game speed x1 and 0.8 s at x4 (two games on one PC); no false "could not be built" on a road that joins an existing road (two games on one PC); every game keeps its world details at a sync. Seen in the real game: the automatic update from 0.2.2 to 0.2.3 on the maintainer's PC, and the warning about other mods on two PCs.

**0.2.4 (2026-10-23): the game update and other mods.** Support for game build 40423 (the game's update of 2026-10-22), and a clear message in the window on a game build FeverLink3 does not know. Other mods that change the world on their own stay in sync: the host's copy makes the change and every game applies it at the same step. Seen with two games on one PC; not yet between two PCs. Joining from a Steam invite was played between two PCs over the internet.

**0.2.5 (2026-10-23): fixes from the friend sessions.** Roundabouts and streets with a bus stop build in a session; a repair needed right after a sync is no longer dropped; a change made while paused no longer forces a sync; the host learns when a build stood on one game only; mods compared for Steam invite joins; text margins in the window; a Save and quit button after an update. Roundabouts, bus stops, mods and margins seen with two games on one PC.

**0.2.6 (2026-10-24): game build 40425.** Support for the game's update of 2026-10-22. Seen working with two games on one PC.

**0.2.7: terrain editing, partly.** Terrain heights (raise, lower, flatten, smooth, roughen) and ground paint are shared: every game applies the same change at the same moment. FeverLink3 finds its places in the game by itself at every start, so a game update no longer always needs a FeverLink3 update. Builds land sooner (0.8 s at x1). Fixes from issue #3: no repair sync for industry names in different languages; the tree, plant and rock tools are grayed out in a session. Seen with two games on one PC.

**0.2.8 (2026-10-26): trees, plants and rocks shared.** The tree, plant and rock brushes and the vegetation eraser work in a session, and every game applies the same change at the same moment. Seen with two games on one PC.

**0.2.9 (2026-10-27): other mods' tools and big builds.** A build another mod's tool or window sends (the bus bay upgrade, the crossing window) is applied at the same moment on every game; big builds such as 3-track stations travel in parts, up to 3 MB (before, anything over about 480 KB was refused; one Steam message carries at most 512 KB); the update check runs when a host with a newer FeverLink3 refuses you. Seen with two games on one PC (builds of 620 KB and 1.23 MB); big builds in parts over Steam between two PCs not seen yet.

**0.2.10 (2026-10-30): one shared company and fixes from play between two PCs.** The host starts with "One shared company"; "Company for each player" shows as upcoming. A bus stop on another mod's road (bus and truck bay roads) is shared; the window title shows the version. Seen with two games on one PC, not yet between two PCs.

## Next

- Find the cause of the object id order differences, so these repair syncs stop.
- See other mods' shared changes and settings between two PCs, and make line edits apply faster.
- Company for each player (own money, builds, lines, loans and subsidies; in the code, shown grayed as upcoming in 0.2.10): play it between two PCs, and reach the end of a subsidy (reward or fine) in a company game.
- See the games autosave together between two PCs (seen so far with two game windows on one PC).
- Code signing of the files at 1.0.0 (releases are unsigned until then).

## Later

- More than two players in one session (planned for in the design, not played yet).
- Host permissions in the window.
- Distribution beyond GitHub and Nexus Mods (live since 0.2.2): the game's Mod Hub if it accepts a mod with a native part. Transport Fever 3 has no Steam Workshop.
- A dedicated server: a game copy that hosts permanently.

## Credits

FeverLink3 is a fresh implementation that builds on two earlier MIT-licensed projects: [TpF2 Multiplayer](https://silver2127.github.io/tpf2-multiplayer/) by silver2127 (Transport Fever 2) and [MPFever](https://github.com/dkdknight/MPFever) by dkdknight / arthur (Transport Fever 3). Their licences are in [THIRD_PARTY_NOTICES.md](https://github.com/AZalshehri7/FeverLink3/blob/main/THIRD_PARTY_NOTICES.md).
