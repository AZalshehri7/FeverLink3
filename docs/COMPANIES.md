# Companies: several players, one economy or several

Status: research and design. The findings in section 1 come from reading the game's own scripts and MPFever's code; nothing here has been run in the real game. The parts that need no unknown engine behaviour are implemented in `mod/feverlink3_1/content/feverlink3_cmds_company.lua` and tested against a mocked engine in `tests/lua/test_cmds_company.py`. Everything that depends on an engine call nobody has made yet sits behind an adapter and is listed in section 8.

## 1. Findings: what Transport Fever 3 offers

Sources, cited as `where:line`:

- `tealdef/...` are the Teal declarations in `<game>/base/tealdef/`.
- `<archive>.zip!path:line` is a Teal source inside `<game>/base/content/<archive>.zip` (game build 40420). The game ships these as plain text, so they are the best description of what the scripts may call.
- `MPFever/...` is the reference clone (MIT). Its authors explored the same question; their findings file `re/FINDINGS.txt` is not published, so only the code and the changelog line below are available.

### 1.1 Short answers

| Question | Answer |
|---|---|
| Is there more than one player (company) entity? | Yes. The game itself creates "AI players" with `api.cmd.makeGameAddPlayerCmd`: its debug panel on every open, and its mission framework whenever a mission hands a line or a building to someone else. They are ordinary entities with a PLAYER component. |
| Can a script create one and address it? | Create: yes, `makeGameAddPlayerCmd(name, Vec3f colour)`, the new entity arrives as `resultEntity` of the command result. Address: yes, commands take it as their `player` argument (below). |
| Do commands accept a player argument? | Yes: `makeVehicleBuyCmd(player, depot, config)`, `makeLineCreateCmd(name, colour, player, line)`, `makeJournalBookAssetCmd(player, entry[, position])`, `makeEntitySetPlayerCmd(entity, player)`; build proposals carry the owner in `Context.player` and in `playerEntity` fields of their parts. |
| Is ownership per player? | Yes. Vehicles, lines, station groups, stations, depots, constructions and street edges (with their edge objects) each have a PLAYER_OWNED component; `makeEntitySetPlayerCmd(entity, player)` moves one (`-1` clears it). The engine can filter by owner (`getLinesForPlayer`, `requireOwnedByPlayer`). |
| Does an added player have an account? | **Unknown.** The game never reads the ACCOUNT of a player other than `getPlayer()`. MPFever's probe exists to find out; its result is not published. |
| Can the game's windows and scripts act as a second company? | **Not from anything the shipped scripts do.** There is exactly one "local player", `api.engine.util.getPlayer()`; about a hundred call sites in the game's own scripts read it, and none sets it. MPFever tried two ways to change it (a native memory patch, a Lua override of the function); the outcome is not published. |

### 1.2 A second player entity exists and can be created

| Fact | Evidence |
|---|---|
| `makeGameAddPlayerCmd("aiPlayer", api.type.Vec3f.new(1, 1, 1))`; the callback receives `GameAddPlayerCommandData` whose `resultEntity` is the new player. | `mission.zip!mission/tasks/transfer_ownership/transfer_ownership_util.tl:4-13`, declared in `tealdef/mission/tasks/transfer_ownership/transfer_ownership_util.d.tl:4` |
| The mission task "transfer ownership" creates a fresh AI player for every line or entity it takes away from the human player, then moves the entities to it. | `mission.zip!mission/tasks/transfer_ownership/transfer_ownership.tl:11-33, 62-91`; `mission.zip!mission/tasks/vehicle_util/mission_vehicle_util.tl:16-36` |
| The developer debug panel spawns an AI player when it opens and offers *Forsake* (give the selected entity to it), *Claim* (take it back to the human player) and *Remove* (`makeEntitySetPlayerCmd(e, -1)`). | `gui.zip!gui/debug_panel/make_entity_debug_panel.tl:271-282, 388-427` |
| A player entity has components PLAYER (field `headquarters`: the construction entity of its headquarters, `-1` or nil when none) and NAME (the company name; the finance and company windows edit it with `makeEntitySetNameCmd(player, name)`). | `game_mechanics.zip!game_mechanics/towns/town_util.tl:1935-1945`; `gui.zip!gui/entity_window/view_manager_util.tl:44-46`; `game_mechanics.zip!game_mechanics/finance/account.tl:132-146`; `game_mechanics.zip!game_mechanics/company/company.tl:932-943` |
| The factories exist in `api.cmd`: MPFever lists every factory (`pairs()` cannot enumerate the native table) and includes `makeGameAddPlayerCmd`, `makeEntitySetPlayerCmd`, `makeJournalBookAssetCmd`, `makeEntitySetNameCmd`. | `MPFever/mod/mpfever_1/content/mpfever_common.lua:1044-1060` |
| MPFever's exploration creates "MPF Company 2", books 5 000 000 to it with `makeJournalBookAssetCmd`, builds a road with `Context.player = P2` and buys a vehicle with `makeVehicleBuyCmd(P2, depot, config)`, logging balances and owners. It was written to answer the questions marked unknown above. | `MPFever/mod/mpfever_1/content/mpfever_bridge.script.lua:3017-3140` |
| MPFever's changelog for the version that contains that probe: "extra player companies are possible in the engine (see re\FINDINGS.txt)". No more detail is public. | `MPFever/docs/CHANGELOG.txt:55-56` |

### 1.3 Ownership is per entity

| Fact | Evidence |
|---|---|
| Owner = component PLAYER_OWNED with field `player`; `isOwnedByPlayer`, `isOwnedByPlayerOrNotOwned` compare it with `getPlayer()`. | `scripts.zip!scripts/entity_util.tl:74-86`, `tealdef/scripts/entity_util.d.tl:18-20` |
| `makeEntitySetPlayerCmd(entity, player)` changes it. A line moves with its vehicles, its station groups, their stations and the stations' constructions; a construction moves with its frozen edges (and their edge objects), depots (and the vehicles inside), stations with their station groups, industries. | `mission.zip!mission/tasks/transfer_ownership/transfer_ownership_util.tl:15-49`; `mission.zip!mission/mission_framework/mission_framework_util_entity.tl:3-60` |
| `-1` removes the owner. | `mission.zip!mission/mission_savegame_util.tl:25, 36` |
| New constructions take their owner from `ConstructionEntity.playerEntity` of the proposal (`-1` = nobody). | `mission.zip!mission/mission_framework/mission_framework_util_entity.tl:89, 110`; `mission.zip!mission/tasks/utility/substitute_constructions.tl:64` |
| The engine filters by owner: `api.engine.system.lineSystem.getLinesForPlayer(player)` and `api.engine.getEntitiesWithComponent(comp, { requireOwnedByPlayer = player })`. | `gui.zip!gui/line_vehicle_mgmt/manager_window.tl:150, 1358-1364` |
| The windows know three ownership states: `None`, `Player` (owned by `getPlayer()`), `Foreign` (owned by anyone else). A foreign line or station shows no controls. | `tealdef/gui/entity_window/eow_extension_util.d.tl:18-22`; `gui.zip!gui/entity_window/eow_extension_util.tl:5-17`; `gui.zip!gui/entity_window/line/line_eow.script.tl:691-693` |
| A vehicle can only be bought in a depot the buyer owns (the window offers only `requireOwnedByPlayer` depots). The engine's own rule is not visible in the scripts. | `gui.zip!gui/line_vehicle_mgmt/manager_window.tl:1358-1364` |

### 1.4 Commands that carry the owner

| Command | Player in | Evidence |
|---|---|---|
| `makeVehicleBuyCmd(player, depot, config)` | argument 1 | `gui.zip!gui/line_vehicle_mgmt/vehicle_react_util.tl:349-354` |
| `makeLineCreateCmd(name, colour, player, line)` | argument 3 | `gui.zip!gui/line_vehicle_mgmt/manager_window.tl:6534-6542` |
| `makeJournalBookAssetCmd(player, entry[, position])` | argument 1 | `gui.zip!gui/game_bar/game_bar.tl:229-234`; `game_mechanics.zip!game_mechanics/finance/loan.script.tl:101-104, 136, 175` |
| `makeEntitySetPlayerCmd(entity, player)` | argument 2 | see 1.3 |
| `makeWorldBuildProposalCmd(proposal, context, ...)` | `context.player`, `proposal...playerEntity` | `gui.zip!gui/entity_window/bridge_and_tunnel.tl:187-188`; `mission.zip!mission/mission_framework/mission_framework_util_entity.tl:83-95`; MPFever `mpfever_bridge.script.lua:3063-3128` |

The mod's schema (`feverlink3_lockstep.lua`, `L.COMMANDS`) already marks the player argument of the first two, so a captured command travels with `{__ref = "player"}` and each game substitutes its own player entity. That substitution is the hook for attribution (section 4.2).

### 1.5 Money and the "company" of the game

| Fact | Evidence |
|---|---|
| The account is a component of the player entity (`balance`, `loan`). `api.engine.util.finance.getPlayersBalance(player)` returns the balance, or nil for infinite money (sandbox). | `game_mechanics.zip!game_mechanics/finance/account.tl:14-17`; `gui.zip!gui/game_bar/game_bar.tl:216-236` |
| Lines, vehicles and constructions have a balance of their own: `finance.calculateBalance({entities}, fromTime, toTime, true)` is what the line window, the vehicle window and the statistics use for "Balance". `finance.calculateEarnings(player)` is the bar's "Earnings". `headquarters.getCompaniesValue()` gives balance, assets and debt. | `gui.zip!gui/line_vehicle_mgmt/line_react_util.tl:621-627`; `gui.zip!gui/statistics/statistic_lines.tl:289-294`; `gui.zip!gui/game_bar/game_bar_display_earnings_plugin/game_bar_display_earnings.script.tl:10-12`; `game_mechanics.zip!game_mechanics/finance/finances_charts.tl:262` |
| A journal entry has `time` (`-1` = now), `amount` and a category; the game bar edits the balance with one `OTHER` entry. Categories: LOAN, INTEREST, CONSTRUCTION, ACQUISITION, MAINTENANCE, INCOME, OTHER, SUBSIDY. | `tealdef/scripts/journal.d.tl:3-69`; `gui.zip!gui/game_bar/game_bar.tl:229-234` |
| "Company" in the game means progression: rank, permits, prospecting, marketing, greenifying. The state is keyed by player entity (`CompaniesState.companyStates[].companyEntity`, `CompaniesGrowthState.companyState : {Entity : CompanyData}`), and a PLAYER-component loop was written and commented out. | `tealdef/game_mechanics/company/company.d.tl:3-22`; `tealdef/game_mechanics/company/company_progression_util.d.tl:3-11`; `game_mechanics.zip!game_mechanics/company/company.script.tl:284-285`; `game_mechanics.zip!game_mechanics/company/company_util.tl:254-271` |
| ...but only the local player is ever updated: XP and levels, permit cooldowns, prospecting. A player without progression state has rank 0. | `game_mechanics.zip!game_mechanics/company/company_growth.script.tl:94-110`; `game_mechanics.zip!game_mechanics/company/company_progression_util.tl:74-93`; `gui.zip!gui/construction/construction.tl:209-212` |
| One loan table, booked to `getPlayer()`. Subsidies are booked to `getPlayer()`. The ticket-price multiplier answers for every vehicle with the rank of `getPlayer()`. | `game_mechanics.zip!game_mechanics/finance/loan.script.tl:47, 101-104, 136`; `game_mechanics.zip!game_mechanics/subventions/subvention_util.tl:125-135`; `game_mechanics.zip!game_mechanics/company/company_growth.script.tl:159-170` |

### 1.6 The one local player

`api.engine.util.getPlayer()` appears in at least 97 lines of the game's scripts (gui 39, game_mechanics 41, mission 15, scripts 2). The windows use it for everything that belongs to "the" player: the lines and depots lists (`manager_window.tl:150, 232, 1337, 1358, 2468`), the balance (`game_bar.tl:220`, `construction_react_util.tl:1226`, `vehicle_store_window.tl:3530`), what is owned and so editable (`entity_util.tl:78-86`), the headquarters (`view_manager_util.tl:44-46`), the company name (`company.tl:933-943`). The simulation scripts use it for loans, subsidies, company progression, notifications and marketing (`loan.script.tl:47`, `company_growth.script.tl:110, 161`, `notifications.script.tl:307-309`, `towns.script.tl:171`). Nothing in the shipped scripts assigns it, and there is no setter in the Teal declarations.

MPFever treated it as a field of the engine: its native module scans memory for words equal to the player's entity id and patches one at a time to the second player's id, to find the one `getPlayer()` reads (`MPFever/native/mpfever_native.cpp:394-454, 1215-1217`, `mpfever_bridge.script.lua:3142-3175`). It also tried replacing the Lua function itself while it built a proposal (`mpfever_bridge.script.lua:3076`). Neither result is published.

### 1.7 What this adds up to

1. Second player entities exist, are created by a command the mod can replicate, are addressed by the `player` argument of the commands the mod already captures, and own entities. Command-level separate companies are within the API.
2. The game's interface and simulation scripts have no notion of playing as anyone but `getPlayer()`. A second human company that *plays* (windows, builder tools, money bar, permits, loans) needs that function to answer differently on each game, which no shipped script does. Whether a Lua override in the interface state is enough, whether a native patch is needed, and what breaks for a company without progression state are exactly the unknowns, and none can be settled without the game.
3. Even with the switch, the simulation scripts still serve one company: loans, subsidies, XP and the ticket-price rank stay with the savegame's player.

## 2. Decision

**Ship one shared company with a per-player ledger now (option b); build the registry and the attribution so that real per-player companies (option a) are a mode of the same machinery, switched on only after the experiments in section 8.**

Why not (a) first: the API supports it at command level, but a company that cannot be played from the game's own windows is not a product, and playability rests on a global the scripts cannot set. Building (a) first would mean committing to the one thing that needs the game. Why (b) is not a dead end: its registry already holds companies, members and an entity key per company, `player` arguments are already rewritten per sender, and the ledger is per player. In (b) every company key resolves to the one player entity and the rewrite is the identity; in (a) it resolves to the company's own entity.

The owner's decisions are collected in section 9.
