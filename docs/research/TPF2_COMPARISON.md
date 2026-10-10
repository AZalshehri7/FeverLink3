# What FeverLink3 takes from TpF2 Multiplayer and from MPFever

Both projects are MIT-licensed (see THIRD_PARTY_NOTICES.md). TpF2 Multiplayer (silver2127) is the more mature one: it has been
used by many players on Transport Fever 2. MPFever (dkdknight) is the one that ran on Transport Fever 3. Written 2026-10-10,
after the first test of building and window commands in two real games showed where our own design was weak.

## How each captures what a player does

| | TpF2 Multiplayer | MPFever | FeverLink3 now |
|---|---|---|---|
| Construction tools (roads, tracks, stations, depots, bulldozer) | A native DLL ("slice") hooks the command factories and `CommandList::Add`, decodes the command, **cancels** it and ships a record; every game, the originator's included, applies it at the stamp (strict). | The tool applies natively; the game script sees the engine event `onPreBuildProposal` just before, captures the proposal and ships it; the others rebuild it at the same game time. | MPFever's way: the simulation half listens to `apply_command` / `onPreBuildProposal`, captures with the build module (roads, constructions and bulldozing seen working in two real games on 2026-10-10), the others rebuild at the stamp. |
| Commands from the game's windows (loans, renaming, lines, vehicles) | The same native hooks see them (they pass the command factories whatever Lua state made them). | A hook in the **UI Lua state**, where the windows live, holds and ships them. | A hook in the UI Lua state (`feverlink3_uihook.lua`); the command is recorded for the bridge and applied natively (replay on peers). Found necessary on 2026-10-10: a wrap made in the bridge's own state never saw a window's command. |
| Game speed | Not a command. The session speed is set by the host's buttons and by votes; other players' clicks are cancelled and become votes. | Held and shipped like any command. | The game's own speed buttons become `speed_request`s to the host (UI hook); the host's answer sets the engine speed everywhere. The mod has no speed buttons of its own. |

## Money, loans and what drifts

TpF2 does not correct money continuously. Balance follows from every game applying the same actions; after a construction
replay the originator's balance is reconciled when the gap is above a threshold; differences are only logged. The **loan is
polled** (every 15 ticks) and shipped as the new absolute loan, and the others book the difference.

FeverLink3 keeps the host's money values as an authority at every checksum round and applies the difference. That never ran in
the first real session: the host's values arrive when the host reaches the round, a game that is a little behind has not yet
recorded its own account for it, and the values were thrown away every round. They are now kept until the game reports the
round. A polled absolute loan (TpF2's way) is the alternative if loans still do not follow once the window command is captured.

## Stamps

TpF2 stamps an action at "now + a delay + how far the fastest other player's clock is ahead" (at most 15 units) and logs
`LATE` for an action that arrives after its stamp. FeverLink3 stamped at "now + lead" from the originator's own clock only; it now
uses the larger of its own clock and the fastest peer's last reported clock.

## Detecting divergence (TpF2) and what we do

TpF2 hashes by geometry and content, never by entity id, in lanes (vehicle count, constructions and stops, edge end points,
balance and loan, town buildings, people), compares at common stamps, and declares a desync only after two mismatches in a row.
A hash is "a sample at a sim time": two samples of one stamp taken at different sim times are not compared. FeverLink3's
checksum parts (money, loan, constructions, lines, time) compared at agreed stamps follow the same idea with fewer lanes; the
policy reloads after two consecutive differing checks. Vehicles and town growth are not in our lanes yet.

## Things TpF2 does that we have not taken

- Line creation from a pre-made spare line, so that the new line has the same entity everywhere.
- Per-sender sequence numbers with NACK and acknowledgement of lost commands above the transport (ours rides a reliable stream).
- Votes for the session speed (the mean of the players' wishes).
- Shared previews of unconfirmed roads and track.
- Companies mode details (separate registries per player) beyond what feverlink3_cmds_company.lua does.
