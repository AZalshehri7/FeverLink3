# Companies: several players, one economy or several

Status (0.2.10, 2026-10-30): 0.2.10 offers only **One shared company**; "Company for each player (upcoming)" shows as a grayed button (`SEPARATE_OFFERED = false`). The separate-company code ships but is **not offered in 0.2.10**; the sections on it below are developer documentation. Separate companies (money, build owner and cost, Pay, Give line, the Companies tab, a paused start) have run in two game windows on one PC on the lab save (section 8, release lab of 2026-10-27), not yet between two PCs, and the owner has not played them yet. The findings in section 1 come from reading the game's own scripts and MPFever's code; the engine experiments of section 8 settled the questions they left open. The shared-company layer (registry, attribution, ledger, limits, payments, the window's data) is implemented in `mod/feverlink3/content/feverlink3_cmds_company.lua` and tested against a mocked engine in `tests/lua/test_cmds_company.py`. Everything that depends on an engine call nobody has made yet sits behind an adapter (section 4) and is listed in section 8 with the experiment that settles it. Section 6 is the contract for the in-game window; section 9 lists what the owner has to decide.

**Gameplay type (owner, 2026-10-27).** The host starts a session with one of two buttons in the window's Players tab: **One shared company** (the default and normal choice, mode `shared`) or **Company for each player** (mode `separate`). In 0.2.10 the second is not offered: it is a disabled "Company for each player (upcoming)" button (`SEPARATE_OFFERED = false` in `feverlink3_ui.script.lua`). Once offered, it is enabled only when the native module reports the `acting_player` capability (on by default, `FEVERLINK3_ACTING_PLAYER=0` off); otherwise it shows disabled as "Company for each player (not available on this game)", because the game's own build tools otherwise build and pay as the savegame's player (section 8, results of 2026-10-27). In shared mode companies are groups inside the one account (who built, bought, spent what; payments are ledger entries); nobody gets a company automatically. The window has a **Companies** tab for every player: the companies with their players and money, Join / Leave / Remove, and Create company. Companies, names, colours, members, mode and ledger live in the registry, which the savegame carries; members are kept by the player's stable identity (3.2), so a player who rejoins a saved world finds their company again.

## 1. Findings: what Transport Fever 3 offers

Sources, cited as `where:line`:

- `tealdef/...` are the Teal declarations in `<game>/base/tealdef/` (the game scripts' own modules).
- `api/...` are the declarations of the engine's own API in `<game>/api/tealdef/api/`: the commands `api.cmd.*`, the components, `api.engine.util.*`. They are the authoritative signatures, each with a line of documentation. (`base/tealdef` has none of this; it was found last.)
- `<archive>.zip!path:line` is a Teal source inside `<game>/base/content/<archive>.zip` (game build 40420). The game ships these as plain text, so they are the best description of what the scripts may call.
- `MPFever/...` is the reference clone (MIT). Its authors explored the same question; their findings file `re/FINDINGS.txt` is not published, so only the code and the changelog line below are available.

### 1.1 Short answers

| Question | Answer |
|---|---|
| Is there more than one player (company) entity? | Yes. The game itself creates "AI players" with `api.cmd.makeGameAddPlayerCmd`: its debug panel on every open, and its mission framework whenever a mission hands a line or a building to someone else. They are ordinary entities with a PLAYER component. |
| Can a script create one and address it? | Create: yes, `makeGameAddPlayerCmd(name, Vec3f colour)` ("when executed, a new player entity is created", `api/cmd.d.tl:688-693`), the new entity arrives as `resultEntity` of the command result (`api/cmd.d.tl:142-149`). Address: yes, commands take it as their `player` argument (below). |
| Do commands accept a player argument? | Yes: `makeVehicleBuyCmd(player, depot, config)`, `makeLineCreateCmd(name, colour, player, line)`, `makeJournalBookAssetCmd(player, entry[, position])`, `makeEntitySetPlayerCmd(entity, player)`; build proposals carry the owner in `Context.player` and in `playerEntity` fields of their parts. |
| Is ownership per player? | Yes. Vehicles, lines, station groups, stations, depots, constructions and street edges (with their edge objects) each have a PLAYER_OWNED component; `makeEntitySetPlayerCmd(entity, player)` moves one (`-1` clears it). The engine can filter by owner (`getLinesForPlayer`, `requireOwnedByPlayer`). |
| Does an added player have an account? | **Unknown.** The component is declared ("usually attached to the player Entity", `api/engine.d.tl:21-29`), but nothing says a created player gets one, and the game never reads the account of a player other than `getPlayer()`. MPFever's probe exists to find out; its result is not published. |
| Can the game's windows and scripts act as a second company? | **Not from anything the shipped scripts do.** There is exactly one "local player", `api.engine.util.getPlayer()`; about a hundred call sites in the game's own scripts read it, and none sets it. MPFever tried two ways to change it (a native memory patch, a Lua override of the function); the outcome is not published. |

### 1.2 A second player entity exists and can be created

| Fact | Evidence |
|---|---|
| Declared: `makeGameAddPlayerCmd(name : string, color : Vec3f) : Command<GameAddPlayerCommandData>`, data fields `name`, `color` and `resultEntity` ("created player entity id (result only)"). | `api/tealdef/api/cmd.d.tl:142-149, 688-693` |
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
| Declared: `makeEntitySetPlayerCmd(entity, player)` "the entity is set or unset to the player" (data `entity`, `playerEntity`); `PlayerOwned { player }`: "an entity with player owner is property of the owner and can be bulldozed only by him". So in separate companies nobody can bulldoze another company's assets. | `api/tealdef/api/cmd.d.tl:132-139, 680-685`; `api/tealdef/api/engine.d.tl:854-859` |
| `-1` removes the owner. | `mission.zip!mission/mission_savegame_util.tl:25, 36` |
| New constructions take their owner from `ConstructionEntity.playerEntity` of the proposal (`-1` = nobody). | `mission.zip!mission/mission_framework/mission_framework_util_entity.tl:89, 110`; `mission.zip!mission/tasks/utility/substitute_constructions.tl:64` |
| The engine filters by owner: `api.engine.system.lineSystem.getLinesForPlayer(player)` and `api.engine.getEntitiesWithComponent(comp, { requireOwnedByPlayer = player })` (declared with `filterFn`). | `gui.zip!gui/line_vehicle_mgmt/manager_window.tl:150, 1358-1364`; `api/tealdef/api/engine.d.tl:1692-1713` |
| The windows know three ownership states: `None`, `Player` (owned by `getPlayer()`), `Foreign` (owned by anyone else). Foreign vehicles and warehouses are not modifiable, and panels such as a line's income graph are hidden for them. | `tealdef/gui/entity_window/eow_extension_util.d.tl:18-22`; `gui.zip!gui/entity_window/eow_extension_util.tl:5-17`; `gui.zip!gui/entity_window/vehicle/vehicle.tl:377`; `gui.zip!gui/entity_window/warehouse/warehouse.tl:38`; `gui.zip!gui/entity_window/line/line_eow.script.tl:691-693` |
| The line editor ignores a click on a station group owned by someone else; stations owned by nobody (`-1`) or by the player are usable. So infrastructure shared between companies has to be unowned, or owned by whoever builds the line. | `gui.zip!gui/line_vehicle_mgmt/manager_window.tl:295, 531`; `gui.zip!gui/statistics/statistic_stations.tl:264`; `scripts.zip!scripts/entity_util.tl:83-86` |
| A vehicle can only be bought in a depot the buyer owns (the window offers only `requireOwnedByPlayer` depots). The engine's own rule is not visible in the scripts. | `gui.zip!gui/line_vehicle_mgmt/manager_window.tl:1358-1364` |

### 1.4 Commands that carry the owner

| Command | Player in | Evidence |
|---|---|---|
| `makeVehicleBuyCmd(player, depot, config)` | argument 1 | `gui.zip!gui/line_vehicle_mgmt/vehicle_react_util.tl:349-354` |
| `makeLineCreateCmd(name, colour, player, line)` | argument 3 | `gui.zip!gui/line_vehicle_mgmt/manager_window.tl:6534-6542` |
| `makeJournalBookAssetCmd(player, entry[, position])` | argument 1 | `gui.zip!gui/game_bar/game_bar.tl:229-234`; `game_mechanics.zip!game_mechanics/finance/loan.script.tl:101-104, 136, 175` |
| `makeEntitySetPlayerCmd(entity, player)` | argument 2 | see 1.3 |
| `makeWorldBuildProposalCmd(proposal, context, ...)` | `context.player`, `proposal...playerEntity` | `gui.zip!gui/entity_window/bridge_and_tunnel.tl:187-188`; `mission.zip!mission/mission_framework/mission_framework_util_entity.tl:83-95`; MPFever `mpfever_bridge.script.lua:3063-3128` |

The mod's schemas mark the player argument of these commands, the generic ones in `feverlink3_lockstep.lua` (`L.COMMANDS`) and the codecs of the operations module (`feverlink3_cmds_ops.lua`, which also owns `makeEntitySetNameCmd`, `makeEntitySetColorCmd` and `makeJournalBookAssetCmd` and the events with a `companyEntity`), so a captured command travels with `{__ref = "player"}` and each game substitutes its own player entity. That substitution is the hook for attribution (section 3.3).

### 1.5 Money and the "company" of the game

| Fact | Evidence |
|---|---|
| The account is a component, "usually attached to the player Entity" (`Account { balance, loan }`; `api/tealdef/api/engine.d.tl:21-29`). Lines and vehicles have one too (below). `api.engine.util.finance.getPlayersBalance(player)` returns the balance, or nil for infinite money (sandbox; `api/tealdef/api/engine/util.d.tl:858-861`). | `game_mechanics.zip!game_mechanics/finance/account.tl:14-17`; `gui.zip!gui/game_bar/game_bar.tl:216-236`; the ACCOUNT component with `balance` and `loan`: `MPFever/mod/mpfever_1/content/mpfever_bridge.script.lua:146, 190` |
| Lines, vehicles and constructions have a balance of their own: `finance.calculateBalance({entities}, fromTime, toTime, true)` is what the line window, the vehicle window and the statistics use for "Balance". `finance.calculateEarnings(player)` is the bar's "Earnings" (this year so far, `api/tealdef/api/engine/util.d.tl:849-852`); `calculateBalance` with its flag set counts only maintenance and income entries (`util.d.tl:838-848`); `calcIncomeSince(time, player)` is the income of a player since a time (`util.d.tl:862-866`). `headquarters.getCompaniesValue()` gives balance, assets and debt. | `gui.zip!gui/line_vehicle_mgmt/line_react_util.tl:621-627`; `gui.zip!gui/statistics/statistic_lines.tl:289-294`; `gui.zip!gui/game_bar/game_bar_display_earnings_plugin/game_bar_display_earnings.script.tl:10-12`; `game_mechanics.zip!game_mechanics/finance/finances_charts.tl:262` |
| A journal entry has `time` (`-1` = now), `amount` and a category; the game bar edits the balance with one `OTHER` entry. Categories: LOAN, INTEREST, CONSTRUCTION, ACQUISITION, MAINTENANCE, INCOME, OTHER, SUBSIDY. | `tealdef/scripts/journal.d.tl:3-69`; `gui.zip!gui/game_bar/game_bar.tl:229-234` |
| "Company" in the game means progression: rank, permits, prospecting, marketing, greenifying. The state is keyed by player entity (`CompaniesState.companyStates[].companyEntity`, `CompaniesGrowthState.companyState : {Entity : CompanyData}`), and a PLAYER-component loop was written and commented out. | `tealdef/game_mechanics/company/company.d.tl:3-22`; `tealdef/game_mechanics/company/company_progression_util.d.tl:3-11`; `game_mechanics.zip!game_mechanics/company/company.script.tl:284-285`; `game_mechanics.zip!game_mechanics/company/company_util.tl:254-271` |
| ...but only the local player is ever updated: XP and levels, permit cooldowns, prospecting. A player without progression state has rank 0. | `game_mechanics.zip!game_mechanics/company/company_growth.script.tl:94-110`; `game_mechanics.zip!game_mechanics/company/company_progression_util.tl:74-93`; `gui.zip!gui/construction/construction.tl:209-212` |
| One loan table, booked to `getPlayer()`. Subsidies are booked to `getPlayer()`. The ticket-price multiplier answers for every vehicle with the rank of `getPlayer()`. | `game_mechanics.zip!game_mechanics/finance/loan.script.tl:47, 101-104, 136`; `game_mechanics.zip!game_mechanics/subventions/subvention_util.tl:125-135`; `game_mechanics.zip!game_mechanics/company/company_growth.script.tl:159-170` |

### 1.6 The one local player

`api.engine.util.getPlayer()` appears in at least 97 lines of the game's scripts (gui 39, game_mechanics 41, mission 15, scripts 2). The windows use it for everything that belongs to "the" player: the lines and depots lists (`manager_window.tl:150, 232, 1337, 1358, 2468`), the balance (`game_bar.tl:220`, `construction_react_util.tl:1226`, `vehicle_store_window.tl:3530`), what is owned and so editable (`entity_util.tl:78-86`), the headquarters (`view_manager_util.tl:44-46`), the company name (`company.tl:933-943`). The simulation scripts use it for loans, subsidies, company progression, notifications and marketing (`loan.script.tl:47`, `company_growth.script.tl:110, 161`, `notifications.script.tl:307-309`, `towns.script.tl:171`). Nothing in the shipped scripts assigns it, and there is no setter in the Teal declarations: the engine API declares only `getPlayer() : Engine.Entity`, "obtains the player entity" (`api/tealdef/api/engine/util.d.tl:1021-1023`). The PLAYER component declares one field, `headquarters` (`api/tealdef/api/engine.d.tl:846-853`).

MPFever treated it as a field of the engine: its native module scans memory for words equal to the player's entity id and patches one at a time to the second player's id, to find the one `getPlayer()` reads (`MPFever/native/mpfever_native.cpp:394-454, 1215-1217`, `mpfever_bridge.script.lua:3142-3175`). It also tried replacing the Lua function itself while it built a proposal (`mpfever_bridge.script.lua:3076`). Neither result is published.

### 1.7 What this adds up to

1. Second player entities exist, are created by a command the mod can replicate, are addressed by the `player` argument of the commands the mod already captures, and own entities. Command-level separate companies are within the API.
2. The game's interface and simulation scripts have no notion of playing as anyone but `getPlayer()`. A second human company that *plays* (windows, builder tools, money bar, permits, loans) needs that function to answer differently on each game, which no shipped script does. Whether a Lua override in the interface state is enough, whether a native patch is needed, and what breaks for a company without progression state are exactly the unknowns, and none can be settled without the game.
3. Even with the switch, the simulation scripts still serve one company: XP and the ticket-price rank stay with the savegame's player. Loans and subsidies are run for every company in separate mode (code of 0.2.10, not offered in 0.2.10) (section 7, "Loans and subsidies").

## 2. Decision

**Ship one shared company with a per-player ledger now (option b); build the registry and the attribution so that real per-player companies (option a) are a mode of the same machinery, switched on only after the experiments in section 8.**

Why not (a) first: the API supports it at command level, but a company that cannot be played from the game's own windows is not a product, and playability rests on a global the scripts cannot set. Building (a) first would mean committing to the one thing that needs the game. Why (b) is not a dead end: its registry already holds companies, members and an entity key per company, `player` arguments are already rewritten per sender, and the ledger is per player. In (b) every company key resolves to the one player entity and the rewrite is the identity; in (a) it resolves to the company's own entity.

The owner's decisions are collected in section 9.

## 3. Design

One registry, two modes. Nothing is stored anywhere but the game-script state of the mod (`st.co`, kept by the simulation half and therefore carried by the savegame), and nothing changes it except actions that every game applies at the same stamp, in the same order, after checking the sender id the launcher attached.

| | **shared**: One shared company (default) | **separate**: Company for each player (needs `acting_player`) |
|---|---|---|
| Money | one account, the savegame's player | one account per company (a player entity each) |
| `player` arguments of a replicated command become | this game's own player entity, for every sender | the sender's company entity (`Co.actor`) |
| What a player's companies are | groups for the report (`found`, `join`, `leave`) | real player entities (`makeGameAddPlayerCmd`, bound by the host's game), owners of what their members build; every player belongs to one (one of their own unless they joined another) |
| Ledger, limits, payments | per player; payments are ledger entries | the same; a payment between two companies is also a journal entry on each account |
| The player's windows act as | the savegame's player (nothing changes) | the player's company (`api.engine.util.getPlayer` is replaced in the interface state) |
| Needs the game to show | the cost measurement and the line results (section 8, E8-E9) | everything in section 8 |

### 3.1 What runs where

```
 company page (UI)      interface half of the bridge                      host            every game, at the stamp
 ui_out.log             wrapped sendCommand: permissions, spending        relays          simulation half (or the interface half
  "company" record  ->  pre-check, encode, stamp, ship               ->   the action  ->  when the game is paused):
                                                                                           decode (Co.actor: who acts?)
 companies.txt  <-  Co.status  <-  mirror of the registry  <-  st.co  <-  Co.beforeApply  (limit gate, the operation)
 (read by the page)                                                       engine command (not for a mod-level action)
                                                                          Co.afterApply   (the ledger)
```

- A registry operation is a replicated action like a game command, with the engine command left out: `api.cmd.feverlink3Company({ op = ..., ... })`. The bridge makes that factory (`spec.virtual`), so tests, the window and a future console all issue it the same way.
- `makeGameAddPlayerCmd` is the only way a company gets a player entity: the engine creates the entity on every game, the bridge's watch binds the action's key (`<sender>:<seq>`) to each game's own entity id, and the same action registers the company with that key (`Co.addPlayer`). Entity ids never travel.
- The interface half keeps a mirror of the registry. While the game is paused it applies actions itself (`guiApplyNow`); it runs the registry and ledger part on the mirror at once, so the next action in the same batch sees it, and replays the same part on the simulation state with an event (`co`), which is the copy the savegame keeps. The mirror takes the state's registry back when the state is newer (`rev`).
- A fault in the company layer is logged (`company layer failed`) and the action goes ahead: losing a player's action is worse than a missing ledger line.

### 3.2 Registry operations

`feverlink3Company({ op = <name>, ... })`. Every row is checked on every game with the trusted sender id; a refused operation changes no company data; the refusal is counted in the sender's ledger (`refused`, `last`) and reaches the sender's callback as a failure.

| op | fields | who | effect |
|---|---|---|---|
| `me` | `name` | anyone | The sender says who they are: their session id is seated on their **stable identity**, the name they play under (trimmed, spaces collapsed, ASCII letters lower case). An earlier seat of that identity is cleared (a rejoin, the next session), and their company comes back with it. In separate mode someone without a company gets one named after them. The bridge sends it by itself once the world uses companies (`X.companyChores`); every window record also carries the name as `as`, which seats the sender before the operation runs (that seat stands even if the operation is refused). |
| `found` | `name`, `color`? | anyone with a seat | New company `c<n>` (keys are never reused; at most 16 at a time; names unique, case ignored). The founder owns it and moves into it. |
| `bind` | `company`, `entity` | host | Sets the player entity (key) a company acts through. One entity, one company. In separate mode the host's game binds every company by itself: its own company to the savegame's player, every other one to a new player entity (`makeGameAddPlayerCmd(name, colour)`, which binds the unbound company of that name: `Co.addPlayer`). |
| `join` | `company` | anyone with a seat | The sender joins any company (and leaves their old one). A company holds several players. |
| `leave` | | anyone | The sender leaves their company. Refused in separate mode: there everyone plays as a company (outside one they would play as the savegame's player, the host's); they join another instead. |
| `rename` | `company`, `name` | host, owner | Registry label only (unique). The player entity's own name is the operations module's `makeEntitySetNameCmd`, which the company window sends with the player as `{__ref = "player"}`: in separate mode that is the sender's company. |
| `remove` | `company` | host, owner | Removes it from the registry; its players are taken out (in separate mode only an empty company can go); the player entity stays in the game. |
| `mode` | `mode` = `shared` or `separate`, `acting`, `players` | host | The gameplay type the host picked at the start. `separate` needs `acting = true`, which the host's bridge adds from its native module's `acting_player` capability; it also adds the roster as `players = {{id, name}}`, who are seated and each get a company of their own when they have none. |
| `limit` | `player`, `amount`?, `per`? | host | Spending limit; no `amount` removes it. `per` is the period in game-time units (see 3.5). Counting starts from zero. |
| `perm` | `player`, `category`, `allowed` | host | Forbids (`allowed = false`) or allows one category of action (`build`, `bulldoze`, `spend`, `other`) for one player. Every game checks it at the stamp, in addition to the grant the launcher sends to the player (`docs/PROTOCOL.md`, Permissions); company operations are never blocked by it. |
| `reset` | `player`? | host | Clears what is counted against the limit (one player or all); the history stays. |
| `pay` | `to` or `toCompany`, `amount`, `memo`?, `from`? | anyone; `from` (charge someone else: a fee) host only | A payment from the sender to another player: ledger entries (`paid`, `received`) and, in separate mode between two different companies that both have an entity, a journal entry of `-amount` on one account and `+amount` on the other. `toCompany` (the window's Pay) pays a company: from the sender's company to that one, separate mode only (refused in shared mode: one account), both with an entity, never with `from`; the sender's ledger counts `paid`, the receiving company's owner's `received`. |

Engine commands the module registers: `makeGameAddPlayerCmd(name, colour)` (host only; colour is a `Vec3f`, sent as `{r, g, b}` and rebuilt on every game) and `makeEntitySetPlayerCmd(entity, player | -1)` (both arguments travel as entity keys). The host may move anything. In separate mode a player may also hand over what their own company owns (Give line, 6.2): every game checks at the stamp that the entity's owner there (`PLAYER_OWNED`, `ctx.ownerOf`) is the entity of the sender's company, which is also the action's `ctx.player`, and that the new owner is a registry company's entity or `-1`; anything else is refused everywhere ("only the host, or the company that owns it, can hand it over"). The one event it registers is `Companies` / `applyLevel` (the company window applies a rank the player has earned, `game_mechanics.zip!game_mechanics/company/company.tl:422`), because the progression script of every game must apply it. Names, colours, journal bookings, loans, marketing, prospecting and greening are the operations module's (`docs/COVERAGE_OPS.md`); they follow the sender's company through the same `ctx.player`.

### 3.3 Attribution

`Co.actor(ctx)` runs when a game builds the context of an action. In shared mode, and for a player who belongs to no company, `ctx.player` stays what it was: this game's own player entity. In separate mode a member's `ctx.player` becomes the entity of the company's key on this game. If the key is not bound on this game yet, `ctx.refuse` is set and `L.decodeCommand` refuses the action, so a command is never applied with a guessed id; the other games, which do know the entity, go ahead, and the checksums show the difference.

Every `{__ref = "player"}` the generic path decodes, and everything a decode hook takes from `ctx.player`, therefore follows the sender.

### 3.4 Ledger

Per player (by stable identity once the player has a seat; by session id before, merged into the identity's ledger when the seat comes; created on first use): how many actions of each category were applied (`build`, `bulldoze`, `spend`, `other`), the money they took by category (`gross`), money that came back (`refund`), counts by command name (`made`), `paid` and `received`, the spending counted against the limit (`per`), and the refusals. Per entity a command created: component name, creator, stamp (`made`, the newest 400).

The cost of an action is the difference of the acting company's account across the engine command (`engine.balanceOf`), measured where the command runs: inside the simulation update on every game, or in the command's callback when the interface half applies it during a pause (nothing else moves the account then). If the account cannot be read (infinite money, a build where the call differs) the action is counted and no money is booked. Whether the engine books a purchase inside the same update is not known (E8); the ledger's counts do not depend on it.

An event (a loan, a marketing campaign, a level...) is counted under the name `event <id>/<name>` and books no money: a game script handles it later, so what the account does meanwhile is not its cost. Its category is the operations module's (loans, marketing, prospecting and greening are `spend`).

"Earned" is not stored. The window gets, for each player, the engine's balance of the lines that player created (`engine.lineResult`, every few seconds): income minus running costs, which is what the line window calls Balance. `net` is that, plus refunds, minus spending.

### 3.5 Limits

A limit stops a player's next building (`build`) and buying (`spend`) once what they have spent in the period reaches the amount. It is soft by one action: the check happens before an action, so the action that crosses the amount goes through. Bulldozing, selling, and everything of category `other` are never stopped, nor is repaying a loan (`Loan` / `Repay`, `Co.freeOfLimit`). Taking a loan, a marketing campaign, prospecting and greening are `spend` events and are stopped like a purchase. Only the host sets limits; by default there are none.

A category the host forbade with `perm` is refused by the same two checks, before any limit is looked at (`not allowed to spend`).

The check is made twice, and both read only the registry: by the originator right before it stamps (so the window hears about it at once: callback `false`, `action_refused {seq = 0, detail}`, nothing shipped), and by every game at the stamp (so an action a modified client sends anyway is refused everywhere; its own game then differs from the others and is repaired by the normal resync). The period is a game-time length in the stamp's units (`status.monthLength` is a month): periods are `floor(stamp / per)`, so every game puts an action in the same period whatever its own clock says.

### 3.6 Checksums, resync, hot join

- Once anything was configured or recorded, the checksum report has a part `companies` (hash of `Co.digest`): mode, companies, people and seats, limits, forbidden categories, and per player the counts, payments and refusals. Measured money (`gross`, `refund`) is left out because a measurement is not a command; a game that measures differently must not cause a resync every time. Until the first action is recorded there is no part, so the report is what it was before this layer existed.
- The registry is part of the script state (`st.co`, kept by the simulation half's `state:get`/`state:set`, which the savegame carries), so a save/load, a resync (everyone loads the host's save) and a hot join carry it along. After a reload the interface half starts a new mirror from the state. A registry of an older save (version 1, members by session id) loses its membership on load (`Co.upgrade`): session ids name other players in the next session.
- Session ids are handed out again in every session. Membership, owners and the ledger are therefore kept by identity (`people`, `seats`); limits and forbidden categories are the host's decisions for the session's players and stay by session id. Until a player's `me` arrives, a session id may still be seated on whoever held it in the saved session (a few frames at the start).
- Not compared: the accounts of companies other than the savegame's player (`balance` and `loan` read the savegame's player on purpose, also while a local company is set). The host's money authority corrects that account only. Per-company authority is open (section 7).

### 3.7 Contract for command schemas (build, operations)

Other areas register their commands in `feverlink3_cmds_*.lua`. To follow the sender's company they need nothing but this:

1. In a decode hook, `ctx.player` is the entity the acting player's commands apply to. Use it for every owner: `Context.player`, `ConstructionEntity.playerEntity`, `EdgeObject.playerEntity`, `PlayerOwned.player`, `makeJournalBookAssetCmd`'s player. Never `api.engine.util.getPlayer()`: on the originator it is the local player, and in separate mode it is the local company, neither of which is the sender's company on the other games.
2. In an encode hook, write the player entity as `{__ref = "player"}` and any other owner as an entity reference by key (`ctx.keyOf`). A hook that only copies plain data needs nothing.
3. Also available to a decode hook: `ctx.from` (sender id), `ctx.at` (stamp), `ctx.key` (action key), `ctx.co` (the registry: read it, do not change it).
4. `spec.category` (or `spec.categoryFor(args)`, for an event) decides which launcher grant and which `perm` apply (all four categories can be forbidden) and whether the spending limit does (only `build` and `spend`). The operations module's codecs already follow points 1 and 2 (`PLAYER` writes and reads `{__ref = "player"}` through `ctx.player`); nothing in them changes for companies.
5. The ledger and the cost need no code in the hook: every replicated command is counted and measured by the bridge. A spec that must change the registry sets `spec.apply(ctx, it, args)` (and `spec.virtual = true` when there is no engine command).

## 4. Adapters

The engine calls the company code makes, all in `Co.extendEngine`, all added to the bridge's `engine` accessor unless it has them already (a test, or a build that knows better, replaces any through `FEVERLINK3_ENGINE`). Each is the call the game's own scripts make for the same purpose; none has been run for this mod.

| Adapter | Default | The game's own use of it | Used for | Experiment |
|---|---|---|---|---|
| `balanceOf(entity)` | `api.engine.util.finance.getPlayersBalance(entity)`, nil = infinite | `gui.zip!gui/game_bar/game_bar.tl:220` | cost of an action | E8 |
| `bookJournalFor(entity, n, kind)` | `makeJournalBookAssetCmd(entity, entry{amount, time -1, type kind or OTHER})` through the original `sendCommand`, as the bridge's `bookJournal(amount, kind)` does for the savegame's player; never replicated | `gui.zip!gui/game_bar/game_bar.tl:229-234` | payments between companies | E2 |
| `lineResult(entity, t0, t1)` | `finance.calculateBalance({entity}, t0, t1, true)` | `gui.zip!gui/line_vehicle_mgmt/line_react_util.tl:621-627` | "earned" in the report | E9 |
| `monthLength()`, `yearLength()` | `api.util.getDefaultMonthDuration()`, `getDefaultYearDuration()` | `api/tealdef/api/util.d.tl:114-120` | monthly presets in the page | |
| `setLocalCompany(entity)` | replaces `api.engine.util.getPlayer` (nil restores the original, kept in `package.loaded`) | none: this is the experiment | separate mode only | E6 |
| `canonicalPlayer()` | the savegame's player, also while `setLocalCompany` is active | | `ctx.player` when nobody's company applies; the host's company key | E6 |
| `ownerOf`, `lineVehicles`, `lineGroups`, `groupStations`, `stationConstruction`, `linesOf`, `nameOf` | `PLAYER_OWNED.player`; `transportVehicleSystem.getLineVehicles`; the `LINE` stops' `stationGroup`; `STATION_GROUP.stations`; `streetConnectorSystem.getConstructionEntityForSubconstruction`; `lineSystem.getLines()` filtered by `PLAYER_OWNED`; `NAME.name` | `mission.zip!mission/tasks/transfer_ownership/transfer_ownership_util.tl:15-33`; `scripts.zip!scripts/entity_util.tl:74-86` | Give line, the stamp check of `makeEntitySetPlayerCmd`, `selected` | E12 |
| `account`, `bookJournal` (existing) | wrapped so they keep reading and booking the savegame's player while a local company is set | | checksum and host authority | E6 |

## 5. Files

| File | Role |
|---|---|
| `mod/feverlink3/content/feverlink3_cmds_company.lua` | The module: `commands` (registered in `L.COMMANDS`), `events` (`Companies` / `applyLevel`) and `company` (`Co`, the functions the bridge calls). Pure Lua except `Co.extendEngine`. |
| `mod/feverlink3/content/feverlink3_lockstep.lua` | `spec.virtual` and `spec.apply` documented; `decodeCommand` honours `ctx.refuse`. |
| `mod/feverlink3/content/feverlink3_bridge.script.lua` | Hunks marked `companies`: the context of an action, the guarded calls around the engine command, the registry in the state and its mirror, the replay of pause-time actions, the pre-check, the factory of mod-level actions, `companies.txt`, the window records, the local-company switch. |
| `tests/lua/test_cmds_company.py` | 5 unit tests of the module (with the window's `Hud.companyView`) and 10 tests of two mocked games (more than one player entity, stable identities, per-player accounts, ownership, pauses, reloads, forged actions, events, the operations module's commands, faults in the company layer). |

## 6. The window

### 6.1 Reading: `companies.txt`

The interface half writes `companies.txt` into the session folder (next to `hud_state.txt`) whenever the registry or the roster changed, and every few seconds while a player has created lines (their results move with the game). The content is one Lua literal produced by `C.serialize`; read it like the snapshot: `C.parse(C.readFile(C.path("companies.txt")))`, and read it again on the next poll when it does not parse (the file was caught half written). Arrays are tables with keys 1..n. Every field below can be nil unless it says otherwise.

| Field | Type | Meaning |
|---|---|---|
| `version` | number | Revision of the registry. Redraw when it changes. |
| `mode` | `"shared"` or `"separate"` | The gameplay type. |
| `acting` | boolean | This game's native module reports `acting_player`: the host's Start offers "Company for each player". |
| `identity`, `company` | string | This player's stable identity and company key (nil without a seat or a company). |
| `self` | number | This player's id (0 = host). |
| `isHost` | boolean | Show the host's controls. |
| `hostEntityKey` | string | Key of the savegame's player entity (`"s:7"`): the `entity` for the host's own company when switching to separate mode. |
| `monthLength`, `yearLength` | number | The game's month and year in game-time units. For `limit.per` (a monthly limit is `per = monthLength`). |
| `perms` | table | What the host allows this player: `build`, `bulldoze`, `spend`, `other` (booleans). The mod knows only its own grant; the grants of the others are the host launcher's (`docs/PROTOCOL.md`, Permissions). |
| `warnings` | array of string | Ready-made lines: a player reached or nears (80%) the spending limit. |
| `lines` | array of string | One report line per player, e.g. `Alice (you): 1 built, 1 bought; spent 48,000; lines 800; limit 50,000, used 48,000`. |
| `companies` | array of company row | |
| `players` | array of player row | Every player the roster, the registry or the ledger knows, by id. |
| `localEntity` | number | Separate mode: the player entity this game's windows act as (`syncLocalCompany`). |
| `selected` | table | The line the player has selected in the game (6.2, "Selected line"): `id` (this game's entity id), `name`, `vehicles`, `stops` (counts), `mine` (this game's company owns it), `company` and `companyName` (the registry company whose player entity owns it, nil for none). Nil while nothing is selected; read again when the selection changes and every 5 s. For Give line. |

Company row: `key`, `name`, `color` (`{r, g, b}` 0..1), `entity` (key or nil), `bound` (has an entity), `owner` (identity) and `ownerName`, `mine` (this player owns it), `mayEdit` (host or owner), `members` (array of `{identity, name, you, online}`; `online`: seated on an id the roster lists), `spent` and `refunded` (sums over the members' ledgers) with `spentText`, `balance` and `balanceText` (separate mode: the account of its player entity), `you` (this player is a member).

Player row: `id` (nil for an identity with a ledger but no seat in this session), `identity`, `name` (roster name, else the identity's name, else `player <id>`), `you`, `present` (in the roster: a player who left stays in the report), `company` (key or nil), `built`, `bought`, `bulldozed`, `other` (counts of applied actions by category `build`, `spend`, `bulldoze`, `other`), `spent` (`{build, spend, other, bulldoze}` money), `total`, `refunded`, `paid`, `received`, `earned` (result of the lines the player created, by ledger key, or nil when no line could be read), `net` (`earned + refunded - total`, nil with `earned`), `refused` (count) and `lastRefusal` (text), `denied` (array of the categories the host forbade, only when there are any), `limit` (`{amount, per}` or nil) and, only with a limit, `used` (spent in the current period), `remaining` and `blocked`, `made` (array of `{fn, label, n}`: how many of each command, with a readable label such as `vehicles bought`).

### 6.2 Writing: `company` records in `ui_out.log`

The window appends a record of kind `company` with the operation as payload, exactly like its chat records:

```lua
C.writerSend(outbox, "company", { op = "limit", player = 1, amount = 50000, per = status.monthLength })
C.writerSend(outbox, "company", { op = "found", name = "Ironworks" })
C.writerSend(outbox, "company", { op = "join", company = "c2" })
C.writerSend(outbox, "company", { op = "mode", mode = "shared" })   -- the host's Start; the bridge adds acting and players
C.writerSend(outbox, "company", { op = "pay", toCompany = "c2", amount = 250000 })
C.writerSend(outbox, "company", { op = "give", line = 30412, company = "c2" })   -- not a registry operation, see below
```

`outbox` is `Hud.newOutbox(C)`. The bridge turns the record into the command (`Co.windowCommand`) and sends it through the wrapped `sendCommand`, so it is permission-checked, stamped, shipped and applied like anything a player does. The bridge adds `as` (the player's name, `C.PLAYER_NAME`) to every record, and `acting` and `players` to a `mode` record. The result shows in `companies.txt` after the stamp. What the window offers (`feverlink3_ui.script.lua`, `Hud.companyView`): the host's Start (Players tab, before the session starts) writes `mode` and starts the session once the page shows it (3 s at most), so the mode is in the host's world before a sync at the start copies it; Companies tab: Join on every company but one's own, Leave on one's own in shared mode, Remove for `mayEdit` (in separate mode only a company without players and without its own player entity), Create company (a name field), and the player's last refusal (`lastRefusal`).

The window's title carries the version this game runs ("FeverLink3 Multiplayer 0.2.10"; the dev build showed "0.3.0" in the release lab 2026-10-27; `ctl_status.txt` `update.current`, the native module's own version). The start choice "Company for each player" is drawn as a Button with `meta.enabled = false` and no `onClick` (label "Company for each player (not available on this game)") when the native module does not report `acting_player`: `enabled` is a documented Meta field ("False disables the node", tf3-docs reference base/scripts/react), the way the base game greys its own buttons (`gui.zip!gui/debug_panel/make_entity_debug_panel.tl:392-424`, `gui/game_bar/game_bar.tl:634`; `builtin.css.lua` `Button:disabled` dims the text), its label dimmed by the mod as well. Pay and Give use the same disabled button until there is an amount or a picked company.

**Pay and Give line (separate mode only).** In shared mode there is one account and one owner, so nothing moves between companies and neither is shown (decided 2026-10-27; the `pay` operation to a player stays for the ledger and the host's fees). With a company for each player, every other company that has an account gets Pay and Give line under its row (`Hud.companyView` `pay`), and a section below the list holds the amount field and the line the player has selected in the game (`selected`). Pay writes `pay` with `toCompany`. Give line writes a `give` record, which the bridge does not turn into a registry operation: `X.giveLine` checks that this game's company owns the line, makes the plan (`Co.transferPlan`) and sends one `makeEntitySetPlayerCmd` per step through the wrapped `sendCommand`, so each is stamped, permission-checked at the stamp (3.2) and applied on every game. The plan is the base game's own recipe (`mission.zip!mission/tasks/transfer_ownership/transfer_ownership_util.tl:15-33`: the line's vehicles, the line, each stop's station group, its stations and each station's construction), limited to what the giving company owns.

**Selected line (owner 2026-10-27).** The tab does not list the company's lines: it shows only the line the player has selected in the game, "Selected line: <name> (<n> vehicles, <m> stops)", then "Give to:" with one toggle button per other company with an account and Give (disabled until a company is picked). Without a selection it says "Open a line in the line manager or click one of its vehicles". A line another company owns shows whose it is and no Give. The interface half's `guiHandleEvent` follows the selection (`X.noteSelection`, UNDOCUMENTED API: the base game's own event names and window ids): `mainView` / `select` with the clicked entity (`gui.zip!gui/main/selector_react_util.tl:106-110`, `gui/layers/layer_terrain.tl:196-197`; fired with the documented `api.gui.fireGuiScriptEvent`, which is "meant to be handled by game scripts (guiHandleEvent)", `api/gui.d.tl:50-57`), and the component events of the entity window `temp.view.entity_<entity>` (`gui/entity_window/view_manager.tl:62-64`, the Window's id at `:324`), which opens for a clicked vehicle or line, for a vehicle picked in a list (`gui/line_vehicle_mgmt/vehicle_react_util.tl:68`) and for the line manager's "Open Line Window" (`gui/line_vehicle_mgmt/manager_window.tl:2154-2162`). A vehicle gives its line (`TRANSPORT_VEHICLE.line`); anything else leaves the selection as it is; the window's `destroy` (or `visibilityChange` to false) ends the selection it made. A pick in the line manager's list alone fires no event a script receives (`manager_window.tl:2614-2652`, react state only): there the player opens the line window. Each change is logged: `company: selected line <id> (<name>) from <id>/<event>, entity <e>` and `company: selected line <id> cleared (...)`. Not yet seen in the real game: which of these events the game delivers to a mod's `guiHandleEvent` (only `menu.*` component events and `temp.view.entity.content.crossing` are seen in logs so far).

**Station rule (2026-10-27).** A station group (with its stations and their constructions) that only the given line of the giver uses goes to the receiver. One that another line of the giver still uses goes to nobody (`-1`): a line cannot use a station group another company owns (1.3), so it can neither go to the receiver (the giver's other line would lose its stop) nor stay with the giver (the given line would). A station nobody owns can be used by every company; whether anyone may then bulldoze it is not known (1.3: an owned entity "can be bulldozed only by him"). Stations of a third company, or of nobody, are left as they are. Not yet seen in the real game: whether vehicles keep running across the change of owner, and whether the engine refuses a move (E12; probe `tools/gametest/probes/e12_transfer.lua`, local).

## 7. Not done

Shared mode:

- Costs are a balance difference (E8). If the engine books a purchase later than the update that sent it, `gross` stays at zero and only the counts are right.
- Commands the mod does not replicate (no schema) are not in the ledger.
- The Companies tab shows companies, players and money; the per-player report lines (`lines`, limits, permissions, payments) are in `companies.txt` but not drawn yet.
- No enforcement in the launcher: authorisation and limits are checked by every game at the stamp, not by the host process. A modified game can only make itself diverge, which the checksums repair.

Separate mode:

- Per-company money is not compared or corrected: `balance` and `loan` are the savegame's player's. A company's accounts can drift without a resync noticing. Needs one `authority` value per company (the protocol carries a name-to-number map, so no Rust change) and `bookJournalFor` to correct them.
- Loans and subsidies (separate mode, not offered in 0.2.10): the game's own loan and subsidy scripts run for every company (`Co.scriptCompanies`). They share the simulation Lua state with the bridge, so their module tables in `package.loaded` are wrapped once a world is loaded, in each simulation Lua state. Loans: the savegame's player keeps the script's table; every other company has one of its own in the same script state (`fl3["p<player entity>"]`, saved with the world), and the script's `update` and its `Loan` events run on it with `api.engine.util.getPlayer` answering that company's player entity. The `Loan` and `Subvention/onAccept` events carry the sender's company as `fl3player` (cmds_ops `actorEvent`, from `ctx.player` at the stamp). Subsidies stay one pool of offers; the accepting company owns one (`fl3player`, set inside the script's own save), and every booking of it (`subvention_util.applyBonusMalus`) goes to the company owning the subsidy that holds the bonus. The interface state shows the local company's loan table in the Loans tab (`api.engine.getComponent` for the loan script's `GAME_SCRIPT` only). The subsidy wrapper never reads the state itself and never wraps a step in `pcall`: a first version that did froze a game within minutes at top speed (3 of 3 soaks; `tools/gametest/loan_soak.ps1`), the loan wrapper alone ran clean, and the pass-through version ran 2 soaks of 8 minutes clean. The game's Lua has no `rawequal`. `FEVERLINK3_COMPANY_SCRIPTS=0` switches it off (must match on every game).
- The simulation scripts serve one company for the rest: rank and XP (`Companies` / `applyLevel` applies the level of the savegame's player), permits, prospecting, marketing, the ticket-price multiplier (section 1.5). A second company has rank 0 in every window that shows it.
- Headquarters: `PLAYER.headquarters` is per player; nothing builds or moves one.
- The native build tools take the owner of what they build from the engine's own local player (E6, real game 2026-10-27). Static research of build 40425 (`docs/NATIVE_LINK.md` section 12, not yet seen in the game): a tool writes that player into its own command (`context.player` and the owners of its segments and edge objects); the engine charges the build to the command's `context.player`, not to a global. FeverLink3 applies every native build as the mod's carrier, whose Context already holds `ctx.player`, so the money follows the acting company; the owners in the shipped text are rewritten to the carrier's player by the native module (`FEVERLINK3_ACTING_PLAYER`, on by default since 0.2.10, `=0` off; `ready` lists `acting_player`), and the build codec's capture takes the engine's own player for `"player"`. Every game of a separate-mode session needs the switch. Script builds were checked in the game on 2026-10-27 (E5, single player): the money follows `Context.player`, the owner the segment's `playerOwned`. The originator applies its own build like every other game (deferred: the held command is never applied, its game injects the carrier at the stamp, so the rewrite runs there too). In separate mode a build that would go hold-and-release (the held command released as the tool made it, as the savegame's player), or a pause-made build without a record, is refused before it is shipped, and so is every native build when the module does not list `acting_player`.
- The interface player (separate mode, not offered in 0.2.10; `FEVERLINK3_INTERFACE_PLAYER`, on by default, `=0` off; `docs/NATIVE_LINK.md` "Interface player"): the engine's own interface takes its local player from one field of the simulation state it shows (`GameState+0x20c`), not from Lua `getPlayer`: in the two-window labs of 2026-10-28/30 the guest's bulldozer did not remove its own company's roads (no command) and `findBestDepotForLine` answered -1 for its own line. The native module writes this game's company into the state the interface reads and gives every simulation copy the savegame's player back (static research of build 40425, `native/src/iplayer.rs`), so the simulation, the game scripts and the savegames stay the same on every game. The build tools copy the player when the game interface is made, at a world load: the host starts one resync when a company's player entity is made in the session (`X.companyChores`), the mod sends the order as soon as its company's entity is known (`X.syncInterfacePlayer`), and the module keeps it across the load. While it acts, the savegame's player the module reports (`interface_player` event) is the canonical player (`engine.savegamePlayer`: checksum account, record `player`). Not yet seen in the game; which game-script state reads the interface's state is not proven.
- Another company's road (separate mode, not offered in 0.2.10): the native rewrite keeps the owner of every piece of a split segment. In `native/src/acting.rs` `kept_pieces`, an added segment the text's player owns whose two ends and three points between (t = 0.25, 0.5, 0.75) lie within 1 m (`ON_CURVE_M`) of the curves of removed segments that player owns is a piece of such a segment and keeps that owner, and so do the edge objects on it; everything else the text's player owns becomes the carrier's. In a separate session the text's player (the savegame's player) is also the host's company, so the host company's road stays the host company's when a guest's road crosses or joins it, and the new road is the guest's. Seen in the release lab (below).
- Another company's construction (separate mode, not offered in 0.2.10): a build that removes or replaces a construction another company owns (a station upgraded, a depot rebuilt; the text's `toRemove` entities) is refused before it is shipped (`X.foreignRemoval` in the bridge), with the notice "Your build was cancelled: it changes a station or building of another company." The rewrite would otherwise hand the rebuilt construction to the builder. Constructions nobody owns stay open to everyone. Not yet seen in the game.
- A stop placed on another company's road is not built (the game's own rule); the notice is the general one ("the world changed before its turn"). Known issue.
- Station access: a line cannot use a station group another company owns (section 1.3). Shared stations must be owned by nobody or by the line's company. Fees for using someone else's infrastructure need usage detection and are not attempted; `pay` is the manual form.
- Missions and scenarios that call `makeGameAddPlayerCmd` themselves create players the registry does not know; they are harmless.

Found while testing, outside this work: after a savegame reload the bridge numbers the local player's actions from 1 again (`G.seq`), so the key of a new action can equal a key the state still holds a result for; the originator's callback of such an action is never delivered (the action itself applies correctly). Starting `G.seq` above the largest own key in the state would fix it.

## 8. NEEDS GAME VERIFICATION

Run in the interface Lua state of a single-player game (callbacks are allowed there), paused, with at least one road, one depot and one vehicle on a line. `P1 = api.engine.util.getPlayer()`. Log everything with `print`. The game's own scripts make every call below; what is unknown is the result.

| # | Experiment | Observe | Decides |
|---|---|---|---|
| E1 | `api.cmd.sendCommand(api.cmd.makeGameAddPlayerCmd("FL3 test", api.type.Vec3f.new(0.9, 0.2, 0.2)), function(res, ok) ... end)`; `P2 = res.resultEntity` | `ok`; `api.engine.entityExists(P2)`; `getComponent(P2, ComponentType.X) ~= nil` for PLAYER, ACCOUNT, NAME, PLAYER_OWNED; `getEntityName(P2)`; `finance.getPlayersBalance(P2)` | Whether a created player has an account at all. If not, separate mode cannot work. |
| E2 | Book 1 000 000 into P2: `entry = api.type.JournalEntry.new(); entry.amount = 1000000; entry.time = -1; entry.category.type = api.type.JournalEntry.Type.OTHER`, `makeJournalBookAssetCmd(P2, entry)` | `getPlayersBalance(P2)` before and after; P1's balance | Whether payments between companies (`bookJournalFor`) work, and the starting money of a company. |
| E3 | Buy for P2: `makeVehicleBuyCmd(P2, depot, config)` with `config = api.engine.getComponent(vehicle, ComponentType.TRANSPORT_VEHICLE).transportVehicleConfig`, (a) in a depot P1 owns, (b) after moving the depot to P2 (`makeEntitySetPlayerCmd(depot, P2)` and its construction, `api.engine.util.construction.getConstructionEntity(depot)`) | `ok`; the new vehicle's `PLAYER_OWNED.player`; `getPlayersBalance` of P1 and P2 before and after | The rule for depot ownership; whose money is spent. |
| E4 | `makeLineCreateCmd("t", Vec3f, P2, line)` with stops at a station group owned by P1, by P2, by nobody (`makeEntitySetPlayerCmd(group, -1)`); put a P2 vehicle on it; run two minutes | `ok`; whether the vehicle drives; the line's `finance.calculateBalance({line}, 0, now, true)`; P2's balance | Which station groups a company's line may use (shared infrastructure). |
| E5 | MPFever's road test (`mpfever_bridge.script.lua:3063-3128`): a street proposal with `Context.player = P2` and `playerEntity = P2` on its parts | Owner of the new edges; whose balance fell | Whether the construction schema can set the owner from `ctx.player`. |
| E6 | `local ok = pcall(function() api.engine.util.getPlayer = function() return P2 end end)`; open the lines manager, the vehicle store, the finance window, the company window, the construction menu, the game bar; use the road tool; restore | `ok`; `getPlayer() == P2`; each window shows P2's data or P1's; any Lua error in the game's log (company rank of P2 is 0: `construction.tl:209-212`); owner of what the native tool builds | The switch itself. If it fails, separate mode needs a native patch (MPFever's `setplayer`) or stays impossible. |
| E7 | With P2 owning vehicles and a line, play two game years at the top speed | Lua errors in the game's log from `company`, `loan`, `notifications`, `towns`; P2's balance moving | That the simulation scripts cope with a second player. |
| E8 | In the mod's simulation `update`: `b0 = finance.getPlayersBalance(P1)`, send `makeVehicleBuyCmd(P1, depot, config)`, `b1 = ...` in the same update, and again in the next update | `b0 - b1` against the price in the vehicle store | Whether the ledger's cost is right (shared mode). If `b1 == b0`, the cost is booked later and the measurement needs a settle step. |
| E9 | After a game year: `finance.calculateBalance({line}, 0, now, true)` and `... (getLineVehicles(line), 0, now, true)` | Both against the line window's *Balance* | Which call "earned" should use. |
| E10 | Two games with the mod, a session started in separate mode (needs `acting_player`): the host's game adds the player entity of a company by itself (`X.companyChores`) | `mod.log` of both: `company: company c1 (Mill) added as player entity 0:<n>` and `action 0:<n> created entity <id> (PLAYER)` with different ids; `companies.txt` identical | That the watch finds a PLAYER entity and the key binds on every game. |
| E11 | Save with P2, its vehicles and a registry, load | PLAYER entities and owners intact; `companies.txt` shows the same registry | That the savegame carries the script state. |
| E12 | Move a line with its vehicles, stations and station groups to P2 (the recipe of `transfer_ownership_util.tl:15-33`, as `Co.transferPlan` makes it; probe `tools/gametest/probes/e12_transfer.lua`: DEV run three times, `E12 BEFORE/PLAN/SENT/AFTER` lines in `dev_click_out.txt`) | The line window's ownership state for P1; P2's vehicles keep running (`state`/`stopIndex` change between two AFTER runs); the callbacks' `ok` | `makeEntitySetPlayerCmd` as a way to hand assets over: Give line. |

### Results, 2026-10-27 (one single-player game, testingLink, build 40425, FeverLink3 0.2.9, not saved)

Commands were sent from the interface state through a click (the DEV run button); from a timer the engine refuses them ("API is currently restricted").

| # | Result |
|---|---|
| E1 | **Passes.** `makeGameAddPlayerCmd("FL3 test", color)`: ok, `resultEntity` 52855 (player 1 is 35419). It has PLAYER (headquarters -1), ACCOUNT (balance 0, loan 0), NAME and COLOR. |
| E2 | **Passes.** `makeJournalBookAssetCmd(P2, +1 000 000)`: P2 0 -> 1 000 000, P1 unchanged. A payment (P2 -250 000, P1 +250 000, two bookings) moved exactly that: P1 250 000, P2 750 000. |
| E6 | **Half.** `api.engine.util.getPlayer = function() return P2 end` is accepted in the interface state, and the game's own windows follow it: the bar's Account shows P2's 750 000, the rank box shows "Achievements" instead of "Junior" (P2 has no progression), the finance window is titled "FL3 test" with P2's revenue and expenses, the line manager and the road menu open without errors. **But the native road tool still builds as the engine's local player:** a road drawn with the tool cost P1 (250 000 -> 196 517) and its six edges are owned by P1 (35419). The Lua switch changes what the windows show, not who builds and pays. |
| E5 | **Passes (second run, a well-formed proposal).** Three 40 m roads from the interface state: A `ctx.player = P2`, owner P2; B `ctx.player = P2`, no owner; C `ctx.player = P1`, owner P2. Costs 8 003 / 7 838 / 7 547: P2 paid exactly A + B (1 000 000 -> 984 159), P1 paid C (0 -> -7 547). Owners: A P2, B none, C P2. **The money follows the command's `context.player`, the owner follows each segment's `playerOwned`**, so rewriting both in a held build makes it the acting company's (route 2). The first run's proposal (copied BASE_EDGE members) built nothing. |

So separate companies take route 2: the native module rewrites, in every held tool command it injects, the originator's tool player to the carrier command's player (`context.player` and the owners of segments, edge objects and constructions), switch `FEVERLINK3_ACTING_PLAYER` (on by default since 0.2.10, `=0` off, the same on every game), announced as the hook `acting_player` in its `ready` line (`N.ready.acting` in the bridge); "Company for each player" shows disabled without it. That path ran in the release lab below.

### Results, 2026-10-27 release lab (two game windows on one PC, the lab save, build 40425, FeverLink3 0.3.0; not between two PCs, not played by the owner)

| Item | Result |
|---|---|
| Start | The host chose "Company for each player"; every player got a company with its own player entity (E10). One company per player in three runs, also when the world started paused (the duplicate company of the same name, seen before the fix, is gone). A world saved paused: the gameplay type applied at the start, the session ran on 6 steps and paused again. |
| Build owner and cost | A guest's road was paid by the guest's company and owned by it, on both games. The money bar, the line manager and the finance window showed the player's own company. |
| Split pieces | A guest's road across the host company's highway: the 6 highway pieces stayed the host company's, the 5 new pieces were the guest's, paid by the guest; checksums identical. |
| Pay | 500 000 and 2 000 000 moved from the host's company to the guest's, the same on both games. |
| Companies tab | The host created "Temp", moved into it (its windows switched to Temp's account, 0 money) and back; Temp kept existing with its own entity and 0 money, without players. |
| Give line | Earlier on 2026-10-27 in the lab: a line with 4 buses moved to another company, also while paused (E12, the vehicles' running across the change not checked). A pick in the line manager's list alone does not select a line. |
| Title | "FeverLink3 Multiplayer 0.3.0". |
| Loans | The guest's Loans tab showed its company's own 4 offers; it took a 14 000 000 loan: its company's balance and loan rose by 14 000 000, the host's loan stayed 0, on both games. A month later the repayment and interest came from the guest's company (loan 14 000 000 -> 13 416 667). Repay (after a Pay of 2 000 000 from the host) set its loan to 0. Checksums identical. |
| Subsidies | The guest accepted a subsidy ("Supply Industry", 5 400 000 on acceptance): the 5 400 000 went to the guest's company, the subsidy is marked as the guest company's on both games. |
| Top speed | With loans and subsidies per company, two soaks of 8 minutes at the top speed ran without a freeze (after the fix above). |
| Not seen | The refusal for another company's construction; the reward or fine at the end of a subsidy of a company (unit test only). |

### Results, 2026-10-28 to 2026-10-30 pre-release labs (two game windows on one PC, fl3sweep-base at x4, build 40425, FeverLink3 0.3.0-beta.1; not between two PCs)

`tools/gametest/quick.ps1 -BaseSave fl3sweep-base -NoTrees [-Separate]` (the guest's company is paid 200 000 000 at the start), logs in `docs/research/v030*-quick-2026103*` (local).

| Item | Result |
|---|---|
| One shared company | Full quick lab twice (also with the interface player in the module): real-tool roads, station, depot, the refund bulldozer, pause, stops, line, buses, clone, airport, forced resync, a save on one game, session end; checksums identical, no unexpected resync, no crash dump. The airport's Configure or runway item failed now and then on the host in 0.2.9 as well (nothing reached the build flow: a lab timing item). |
| Interface player | Before it: the guest's bulldozer did not take its own company's road (nothing sent) and findBestDepotForLine gave -1 for its line. With it: one resync when the guest's company was made, then the guest bulldozed its own road with the real tool (refund 6 299 = 75 % into its account, host and guest equal), the depot search found its depot, it bought 2 buses, put them on its line, added and removed a stop, edited the line and cloned 3 buses; native.log: the simulation's copy put back to the savegame player on every step (45 corrections in the first minute), checksums identical. In shared mode the module writes nothing. |
| Stop tool | Guest stops from the game's stop tool on two unowned roads (a town street and another): applied on both games, the stop owned by the guest's company, the street still nobody's. On a road the host had just built (owner the savegame player): the tool shows "Place Along Road" and takes no click, nothing is sent. |
| Another company's road | The guest's bulldozer over the host's road: no highlight, nothing sent, the road still the host's on both games. |
| Save, load, rejoin | A guest's save from a separate session loaded by both games (`tools/gametest/company_reload.ps1`): mode separate, the guest back in its company c2 (entity 28388, 193 853 062, its own loan table), the host in c1; the guest left and joined again under the same name: the same; checksums identical. Joining under another name gives a new company (a company belongs to the player's name). |
| Tree brush | Picking the tree brush tile crashed the game (TransportFever3.exe+0x5499e7, main thread) with 0.3.0 and with 0.2.9 alike since 2026-10-28; not this version's. |

What `mod.log` says while the layer runs (both halves write it):

| Line | Meaning |
|---|---|
| `company: company c1 (Mill) founded` / `... added as player entity 0:3` | A registry operation or `makeGameAddPlayerCmd` registered a company (the key `0:3` is bound to each game's own entity by the next line, `action 0:3 created entity <id> (PLAYER)`). |
| `action <key> refused: <reason>` | An operation or a spending limit refused it, on this game; every game logs the same. |
| `command <name> refused: spending limit reached (...)` / `not allowed to <category>` | The originator's pre-check; nothing was shipped. |
| `company: this game says who it is: player <id> is <name>` | `X.companyChores` sent `me` (once the world uses companies, again after 10 s if it did not arrive). The next line on every game is `company: player <id> is <name>, company c<n>`. |
| `company: binding the host's company c<n> (...) to the savegame's player s:<id>` / `company: adding a player entity for company c<n> (...)` | The host's game in separate mode gives a company its player entity (every 30 s per company until it has one). |
| `company: company c<n> (...) is player entity 0:<seq>` | `makeGameAddPlayerCmd` bound the company of that name, on every game. |
| `start: gameplay type shared` / `start: the page does not show the gameplay type ...` | (`ui` lines) The host's Start; the second when the mode did not show within 3 s (the session starts anyway). |
| `local company: entity <id>` / `local company: the game's own player` | `syncLocalCompany` switched the windows (separate mode only). `(could not be set)` if the assignment failed (E6). |
| `company: could not book a journal entry of ...` | A payment between companies did not go in on this game (E2); the games now differ in money. |
| `company layer failed: ...` / `company layer failed in <part>: ...` | A fault in the company code. The action went ahead without its ledger line; frame faults are logged once. Report it. |

## 9. Decisions for the owner

1. **Decided 2026-10-27:** the host picks the gameplay type at the start; "One shared company" is the default, "Company for each player" is a disabled button "(upcoming)" in 0.2.10 (`SEPARATE_OFFERED = false`, owner decision 2026-10-30: not offered until played between two PCs); once offered it still needs the native `acting_player` capability. No automatic companies in shared mode.
10. **Pay and Give line (2026-10-27, agent's choice, owner to confirm):** separate mode only (hidden with one shared company); Pay goes company to company (`toCompany`); Give line follows the base game's recipe and the station rule of 6.2 (a station group another line of the giver uses goes to nobody). Alternative for that rule: keep such a group with the giver and refuse the give, or move it and let the giver's other lines lose the stop.
2. **Limits are soft by one action and only for building and buying.** A hard limit needs the cost before the command runs. The schemas could pass an estimate (the vehicle's price, the cost the build tool shows) and the gate would use it; nobody has read those numbers yet.
3. **Who may do what (2026-10-27):** any player creates companies (at most 16, unique names), joins any company and leaves (not in separate mode); the creator owns a company; the owner and the host rename and remove it; only the host sets limits, permissions and the mode. Say if joining should need the owner's consent (the old `open` flag was dropped as unused).
4. **Fees:** manual (`pay`, and the host charging a player). Automatic fees for using another company's station or road need an engine read nobody has made (which vehicle used which station) and a rate table; not started.
5. **What the report shows:** counts and money per player, and the result of their lines. If E8 fails, the report shows counts only until a cost source is chosen.
6. **Enforcement stays in the games, not in the launcher.** Moving it into the host means the Rust core learning the registry; the current model (every game refuses at the stamp, a cheater diverges and is resynced) needed no protocol change.
7. **Two layers of permission.** The launcher's grant (`build`, `bulldoze`, `spend`, `speed`; checked by the player's own game) stays as it is; `perm` adds a grant every game checks at the stamp, which the in-game window can set today. Keep both, or let the window drive only `perm` and retire the launcher's.
8. **The numbering bug after a reload** (end of section 7) is outside this work; it affects every command's callback, not only companies.
9. **Stable identity = the player's name.** It is the only identity every game knows about every player; the Steam ID stays in the transport and must not go into a savegame. Two players with the same name are one person to the registry, and a player who changes their name is a new one. A hash of the Steam ID would be more robust; it needs a new field in the protocol's player info (`core/src/session/messages.rs` PlayerInfo, the welcome and `player_joined`) and in `identity.txt`.
