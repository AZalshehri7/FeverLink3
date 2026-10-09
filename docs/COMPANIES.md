# Companies: several players, one economy or several

Status: research, design and the parts that need no unknown engine behaviour. The findings in section 1 come from reading the game's own scripts and MPFever's code; nothing here has been run in the real game. The shared-company layer (registry, attribution, ledger, limits, payments, the window's data) is implemented in `mod/feverlink3_1/content/feverlink3_cmds_company.lua` and tested against a mocked engine in `tests/lua/test_cmds_company.py`. Everything that depends on an engine call nobody has made yet sits behind an adapter (section 4) and is listed in section 8 with the experiment that settles it. Section 6 is the contract for the in-game window; section 9 lists what the owner has to decide.

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

The mod's schema (`feverlink3_lockstep.lua`, `L.COMMANDS`) already marks the player argument of the first two, so a captured command travels with `{__ref = "player"}` and each game substitutes its own player entity. That substitution is the hook for attribution (section 3.3).

### 1.5 Money and the "company" of the game

| Fact | Evidence |
|---|---|
| The account is a component of the player entity (`balance`, `loan`). `api.engine.util.finance.getPlayersBalance(player)` returns the balance, or nil for infinite money (sandbox). | `game_mechanics.zip!game_mechanics/finance/account.tl:14-17`; `gui.zip!gui/game_bar/game_bar.tl:216-236`; the ACCOUNT component with `balance` and `loan`: `MPFever/mod/mpfever_1/content/mpfever_bridge.script.lua:146, 190` |
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

## 3. Design

One registry, two modes. Nothing is stored anywhere but the game-script state of the mod (`st.co`, kept by the simulation half and therefore carried by the savegame), and nothing changes it except actions that every game applies at the same stamp, in the same order, after checking the sender id the launcher attached.

| | **shared** (default, shipped) | **separate** (experimental, off) |
|---|---|---|
| Money | one account, the savegame's player | one account per company (a player entity each) |
| `player` arguments of a replicated command become | this game's own player entity, for every sender | the sender's company entity (`Co.actor`) |
| What a player's companies are | groups for the report (`found`, `assign`, `join`) | real player entities (`makeGameAddPlayerCmd`), owners of what their members build |
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
| `found` | `name`, `color`?, `entity`?, `open`? | host | New company `c<n>` (keys are never reused; at most 16 at a time). In separate mode it needs `entity`, the key of its player entity. |
| `bind` | `company`, `entity` | host | Sets the player entity (key) a company acts through. One entity, one company. |
| `assign` | `player`, `company`? | host | Puts a player in a company; no `company` takes them out. |
| `join` | `company` | anyone if the company is `open`; host and owner always | The sender joins. |
| `leave` | | anyone | The sender leaves. |
| `rename` | `company`, `name` | host, owner | Registry label only (the player entity's own name is `makeEntitySetNameCmd`). |
| `open` | `company`, `open` | host, owner | Whether players may `join` on their own. |
| `remove` | `company` | host | Removes it from the registry; members are taken out; the player entity stays in the game. |
| `mode` | `mode` = `shared` or `separate`, `experimental` | host | `separate` needs `experimental = true` and a player entity on every company. |
| `limit` | `player`, `amount`?, `per`? | host | Spending limit; no `amount` removes it. `per` is the period in game-time units (see 3.5). Counting starts from zero. |
| `perm` | `player`, `category`, `allowed` | host | Forbids (`allowed = false`) or allows one category of action (`build`, `bulldoze`, `spend`, `other`) for one player. Every game checks it at the stamp, in addition to the grant the launcher sends to the player (`docs/PROTOCOL.md`, Permissions); company operations are never blocked by it. |
| `reset` | `player`? | host | Clears what is counted against the limit (one player or all); the history stays. |
| `pay` | `to`, `amount`, `memo`?, `from`? | anyone; `from` (charge someone else: a fee) host only | A payment from the sender to another player: ledger entries (`paid`, `received`) and, in separate mode between two different companies that both have an entity, a journal entry of `-amount` on one account and `+amount` on the other. |

Engine commands the module registers: `makeGameAddPlayerCmd(name, colour)` (host only; colour is a `Vec3f`, sent as `{r, g, b}` and rebuilt on every game) and `makeEntitySetPlayerCmd(entity, player | -1)` (host only; both arguments travel as entity keys).

### 3.3 Attribution

`Co.actor(ctx)` runs when a game builds the context of an action. In shared mode, and for a player who belongs to no company, `ctx.player` stays what it was: this game's own player entity. In separate mode a member's `ctx.player` becomes the entity of the company's key on this game. If the key is not bound on this game yet, `ctx.refuse` is set and `L.decodeCommand` refuses the action, so a command is never applied with a guessed id; the other games, which do know the entity, go ahead, and the checksums show the difference.

Every `{__ref = "player"}` the generic path decodes, and everything a decode hook takes from `ctx.player`, therefore follows the sender.

### 3.4 Ledger

Per player (by sender id, created on first use): how many actions of each category were applied (`build`, `bulldoze`, `spend`, `other`), the money they took by category (`gross`), money that came back (`refund`), counts by command name (`made`), `paid` and `received`, the spending counted against the limit (`per`), and the refusals. Per entity a command created: component name, creator, stamp (`made`, the newest 400).

The cost of an action is the difference of the acting company's account across the engine command (`engine.balanceOf`), measured where the command runs: inside the simulation update on every game, or in the command's callback when the interface half applies it during a pause (nothing else moves the account then). If the account cannot be read (infinite money, a build where the call differs) the action is counted and no money is booked. Whether the engine books a purchase inside the same update is not known (E8); the ledger's counts do not depend on it.

"Earned" is not stored. The window gets, for each player, the engine's balance of the lines that player created (`engine.lineResult`, every few seconds): income minus running costs, which is what the line window calls Balance. `net` is that, plus refunds, minus spending.

### 3.5 Limits

A limit stops a player's next building (`build`) and buying (`spend`) once what they have spent in the period reaches the amount. It is soft by one action: the check happens before an action, so the action that crosses the amount goes through. Bulldozing, selling, and everything of category `other` are never stopped. Only the host sets limits; by default there are none.

A category the host forbade with `perm` is refused by the same two checks, before any limit is looked at (`not allowed to spend`).

The check is made twice, and both read only the registry: by the originator right before it stamps (so the window hears about it at once: callback `false`, `action_refused {seq = 0, detail}`, nothing shipped), and by every game at the stamp (so an action a modified client sends anyway is refused everywhere; its own game then differs from the others and is repaired by the normal resync). The period is a game-time length in the stamp's units (`status.yearLength / 12` is a month): periods are `floor(stamp / per)`, so every game puts an action in the same period whatever its own clock says.

### 3.6 Checksums, resync, hot join

- Once anything was configured or recorded, the checksum report has a part `companies` (hash of `Co.digest`): mode, companies, members, limits, forbidden categories, and per player the counts, payments and refusals. Measured money (`gross`, `refund`) is left out because a measurement is not a command; a game that measures differently must not cause a resync every time. Until the first action is recorded there is no part, so the report is what it was before this layer existed.
- The registry is part of the script state, so a resync (everyone loads the host's save) and a hot join carry it along. After a reload the interface half starts a new mirror from the state.
- Not compared: the accounts of companies other than the savegame's player (`balance` and `loan` read the savegame's player on purpose, also while a local company is set). The host's money authority corrects that account only. Per-company authority is open (section 7).

### 3.7 Contract for command schemas (build, operations)

Other areas register their commands in `feverlink3_cmds_*.lua`. To follow the sender's company they need nothing but this:

1. In a decode hook, `ctx.player` is the entity the acting player's commands apply to. Use it for every owner: `Context.player`, `ConstructionEntity.playerEntity`, `EdgeObject.playerEntity`, `PlayerOwned.player`, `makeJournalBookAssetCmd`'s player. Never `api.engine.util.getPlayer()`: on the originator it is the local player, and in separate mode it is the local company, neither of which is the sender's company on the other games.
2. In an encode hook, write the player entity as `{__ref = "player"}` and any other owner as an entity reference by key (`ctx.keyOf`). A hook that only copies plain data needs nothing.
3. Also available to a decode hook: `ctx.from` (sender id), `ctx.at` (stamp), `ctx.key` (action key), `ctx.co` (the registry: read it, do not change it).
4. `spec.category` decides which launcher grant and which `perm` apply (all four categories can be forbidden) and whether the spending limit does (only `build` and `spend`).
5. The ledger and the cost need no code in the hook: every replicated command is counted and measured by the bridge. A spec that must change the registry sets `spec.apply(ctx, it, args)` (and `spec.virtual = true` when there is no engine command).

## 4. Adapters

The engine calls the company code makes, all in `Co.extendEngine`, all added to the bridge's `engine` accessor unless it has them already (a test, or a build that knows better, replaces any through `FEVERLINK3_ENGINE`). Each is the call the game's own scripts make for the same purpose; none has been run for this mod.

| Adapter | Default | The game's own use of it | Used for | Experiment |
|---|---|---|---|---|
| `balanceOf(entity)` | `api.engine.util.finance.getPlayersBalance(entity)`, nil = infinite | `gui.zip!gui/game_bar/game_bar.tl:220` | cost of an action | E8 |
| `bookJournalFor(entity, n)` | `makeJournalBookAssetCmd(entity, entry{amount, time -1, OTHER})` through the original `sendCommand` | `gui.zip!gui/game_bar/game_bar.tl:229-234` | payments between companies | E2 |
| `lineResult(entity, t0, t1)` | `finance.calculateBalance({entity}, t0, t1, true)` | `gui.zip!gui/line_vehicle_mgmt/line_react_util.tl:621-627` | "earned" in the report | E9 |
| `yearLength()` | `api.util.getDefaultYearDuration()` | same file, line 625 | monthly presets in the page | |
| `setLocalCompany(entity)` | replaces `api.engine.util.getPlayer` (nil restores the original, kept in `package.loaded`) | none: this is the experiment | separate mode only | E6 |
| `canonicalPlayer()` | the savegame's player, also while `setLocalCompany` is active | | `ctx.player` when nobody's company applies; the host's company key | E6 |
| `account`, `bookJournal` (existing) | wrapped so they keep reading and booking the savegame's player while a local company is set | | checksum and host authority | E6 |

## 5. Files

| File | Role |
|---|---|
| `mod/feverlink3_1/content/feverlink3_cmds_company.lua` | The module: `commands` (registered in `L.COMMANDS`) and `company` (`Co`, the functions the bridge calls). Pure Lua except `Co.extendEngine`. |
| `mod/feverlink3_1/content/feverlink3_lockstep.lua` | `spec.virtual` and `spec.apply` documented; `decodeCommand` honours `ctx.refuse`. |
| `mod/feverlink3_1/content/feverlink3_bridge.script.lua` | Hunks marked `companies`: the context of an action, the guarded calls around the engine command, the registry in the state and its mirror, the replay of pause-time actions, the pre-check, the factory of mod-level actions, `companies.txt`, the window records, the local-company switch. |
| `tests/lua/test_cmds_company.py` | 24 unit tests of the module and 25 tests of two mocked games (more than one player entity, per-player accounts, ownership, pauses, reloads, forged actions, faults in the company layer). They also pass on Lua 5.3, 5.4 and LuaJIT 2.1 (run by hand with lupa's other runtimes; CI runs the harness' 5.2). |

## 6. The window

### 6.1 Reading: `companies.txt`

The interface half writes `companies.txt` into the session folder (next to `hud_state.txt`) whenever the registry or the roster changed, and every few seconds while a player has created lines (their results move with the game). The content is one Lua literal produced by `C.serialize`; read it like the snapshot: `C.parse(C.readFile(C.path("companies.txt")))`, and read it again on the next poll when it does not parse (the file was caught half written). Arrays are tables with keys 1..n. Every field below can be nil unless it says otherwise.

| Field | Type | Meaning |
|---|---|---|
| `version` | number | Revision of the registry. Redraw when it changes. |
| `mode` | `"shared"` or `"separate"` | |
| `self` | number | This player's id (0 = host). |
| `isHost` | boolean | Show the host's controls. |
| `hostEntityKey` | string | Key of the savegame's player entity (`"s:7"`): the `entity` for the host's own company when switching to separate mode. |
| `yearLength` | number | The game's year in game-time units; a month is a twelfth. For `limit.per`. |
| `perms` | table | What the host allows this player: `build`, `bulldoze`, `spend`, `other` (booleans). The mod knows only its own grant; the grants of the others are the host launcher's (`docs/PROTOCOL.md`, Permissions). |
| `openCompanies` | array of string | Keys of the companies this player may `join`. |
| `warnings` | array of string | Ready-made lines: a player reached or nears (80%) the spending limit. |
| `lines` | array of string | One report line per player, e.g. `Alice (you): 1 built, 1 bought; spent 48,000; lines 800; limit 50,000, used 48,000`. |
| `companies` | array of company row | |
| `players` | array of player row | Every player the roster, the registry or the ledger knows, by id. |

Company row: `key`, `name`, `color` (`{r, g, b}` 0..1), `entity` (key or nil), `bound` (has an entity), `open`, `owner` (id), `members` (array of ids), `spent` and `refunded` (sums over the members), `you` (this player is a member).

Player row: `id`, `name` (roster name, else `player <id>`), `you`, `present` (in the roster: a player who left stays in the report), `company` (key or nil), `built`, `bought`, `bulldozed`, `other` (counts of applied actions by category `build`, `spend`, `bulldoze`, `other`), `spent` (`{build, spend, other, bulldoze}` money), `total`, `refunded`, `paid`, `received`, `earned` (result of the lines the player created, or nil when no line could be read), `net` (`earned + refunded - total`, nil with `earned`), `refused` (count) and `lastRefusal` (text), `denied` (array of the categories the host forbade, only when there are any), `limit` (`{amount, per}` or nil) and, only with a limit, `used` (spent in the current period), `remaining` and `blocked`, `made` (array of `{fn, label, n}`: how many of each command, with a readable label such as `vehicles bought`).

### 6.2 Writing: `company` records in `ui_out.log`

The window appends a record of kind `company` with the operation as payload, exactly like its chat records:

```lua
C.writerSend(outbox, "company", { op = "limit", player = 1, amount = 50000, per = status.yearLength / 12 })
C.writerSend(outbox, "company", { op = "found", name = "Ironworks", open = true })
C.writerSend(outbox, "company", { op = "add", name = "Mill", color = { 0.9, 0.2, 0.2 } })   -- a company with its own player entity (makeGameAddPlayerCmd)
```

`outbox` is `Hud.newOutbox(C)`. The bridge turns the record into the command (`Co.windowCommand`) and sends it through the wrapped `sendCommand`, so it is permission-checked, stamped, shipped and applied like anything a player does. The result shows in `companies.txt` after the stamp. Operations the page should offer, with the status field that decides whether to show them: everything in 3.2 for `isHost`; `join` for each key in `openCompanies`; `leave` when the player's row has a `company`; `pay` for everyone; `mode = "separate"` only in a build that has passed section 8.

## 7. Not done

Shared mode:

- Costs are a balance difference (E8). If the engine books a purchase later than the update that sent it, `gross` stays at zero and only the counts are right.
- Commands the mod does not replicate (no schema) are not in the ledger.
- The window is not wired (this document is its contract). The ledger shows nothing to a player until it is.
- No enforcement in the launcher: authorisation and limits are checked by every game at the stamp, not by the host process. A modified game can only make itself diverge, which the checksums repair.

Separate mode:

- Per-company money is not compared or corrected: `balance` and `loan` are the savegame's player's. A company's accounts can drift without a resync noticing. Needs one `authority` value per company (the protocol carries a name-to-number map, so no Rust change) and `bookJournalFor` to correct them.
- The simulation scripts serve one company: loans (one table), subsidies, rank and XP, permits, prospecting, marketing, the ticket-price multiplier (section 1.5). A second company has rank 0 in every window that shows it.
- Headquarters: `PLAYER.headquarters` is per player; nothing builds or moves one.
- The native build tools presumably take the owner of what they build from the engine's own local player (E5, E6); the construction schema must set the owners from `ctx.player` (3.7) or new roads belong to the wrong company.
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
| E10 | Two games with the mod, a session started: the host sends `{ op = "add", name = "Mill" }` through the window record | `mod.log` of both: `company: company c1 (Mill) added as player entity 0:<n>` and `action 0:<n> created entity <id> (PLAYER)` with different ids; `companies.txt` identical | That the watch finds a PLAYER entity and the key binds on every game. |
| E11 | Save with P2, its vehicles and a registry, load | PLAYER entities and owners intact; `companies.txt` shows the same registry | That the savegame carries the script state. |
| E12 | Move a line with its vehicles, stations and station groups to P2 (the recipe of `transfer_ownership_util.tl:15-33`) | The line window's ownership state for P1; P2's vehicles keep running | `makeEntitySetPlayerCmd` as a way to hand assets over. |

What `mod.log` says while the layer runs (both halves write it):

| Line | Meaning |
|---|---|
| `company: company c1 (Mill) founded` / `... added as player entity 0:3` | A registry operation or `makeGameAddPlayerCmd` registered a company (the key `0:3` is bound to each game's own entity by the next line, `action 0:3 created entity <id> (PLAYER)`). |
| `action <key> refused: <reason>` | An operation or a spending limit refused it, on this game; every game logs the same. |
| `command <name> refused: spending limit reached (...)` / `not allowed to <category>` | The originator's pre-check; nothing was shipped. |
| `local company: entity <id>` / `local company: the game's own player` | `syncLocalCompany` switched the windows (separate mode only). `(could not be set)` if the assignment failed (E6). |
| `company: could not book a journal entry of ...` | A payment between companies did not go in on this game (E2); the games now differ in money. |
| `company layer failed: ...` / `company layer failed in <part>: ...` | A fault in the company code. The action went ahead without its ledger line; frame faults are logged once. Report it. |

## 9. Decisions for the owner

1. **Ship shared first** (this work), keep separate mode hidden: the window must not offer `mode = separate` until E1-E7 have passed. If you would rather not carry the unverified part, delete the `separate` branches of `OPS.mode`, `Co.actor` and `syncLocalCompany` (about sixty lines); the registry, ledger and limits do not depend on them.
2. **Limits are soft by one action and only for building and buying.** A hard limit needs the cost before the command runs. The schemas could pass an estimate (the vehicle's price, the cost the build tool shows) and the gate would use it; nobody has read those numbers yet.
3. **Who may do what:** only the host founds, assigns, sets limits and switches modes; players join open companies, leave, and pay each other. Say if players should be able to found their own.
4. **Fees:** manual (`pay`, and the host charging a player). Automatic fees for using another company's station or road need an engine read nobody has made (which vehicle used which station) and a rate table; not started.
5. **What the report shows:** counts and money per player, and the result of their lines. If E8 fails, the report shows counts only until a cost source is chosen.
6. **Enforcement stays in the games, not in the launcher.** Moving it into the host means the Rust core learning the registry; the current model (every game refuses at the stamp, a cheater diverges and is resynced) needed no protocol change.
7. **Two layers of permission.** The launcher's grant (`build`, `bulldoze`, `spend`, `speed`; checked by the player's own game) stays as it is; `perm` adds a grant every game checks at the stamp, which the in-game window can set today. Keep both, or let the window drive only `perm` and retire the launcher's.
8. **The numbering bug after a reload** (end of section 7) is outside this work; it affects every command's callback, not only companies.
