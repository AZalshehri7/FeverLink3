# Operations coverage

Every command maker of Transport Fever 3's `api.cmd` that is not about construction, what the mod does with it, and
what is still open. Construction (`makeWorldBuildProposalCmd`, terrain) belongs to `feverlink3_cmds_build.lua`,
companies (`makeEntitySetPlayerCmd`, `makeGameAddPlayerCmd`, company levels and permits) to
`feverlink3_cmds_company.lua`; speed and pause belong to the session. The code is
`mod/feverlink3_1/content/feverlink3_cmds_ops.lua`, the tests `tests/lua/test_cmds_ops.py`.

## Where the list comes from

`base/tealdef` declares no `api.cmd.*`: the engine's own API is not in it. The list of factories is MPFever's
(`C.FACTORIES`, MIT, 54 names, found by probing the engine); the argument shapes, result fields and who calls what are
read from the call sites in the game's own scripts (`base/content/*.zip`, build 40420). Where a shape is only known
from one call site the table says so. `tealdef/scripts/journal.d.tl` gives the journal enums, `tealdef/game_mechanics/finance`
the loan records.

## How a replicated command behaves

All of these are **strict** in the sense of `docs/ARCHITECTURE.md` section 6: the wrapped `sendCommand` holds the command,
the host's stamp decides the step, and every game, the sender's included, applies it there; the sender's window gets
its callback with the real local result. None needs *replay on peers* or *poll*.

- **Entities by key.** An entity from the savegame is `s:<id>`, one a replicated command created is `<sender>:<seq>`
  (the lockstep core binds it on every game). A depot or station group also carries its building's key and its place
  in that building (`at`), so one made in the session is found even if only the building was bound; the entity's own
  id is then not offered. A construction carries its file and position and is searched for by them when the key does
  not fit (town buildings and industries made by game scripts have no shared key).
- **Every reference is checked where it is applied**: it must exist and be of the expected kind (a vehicle where a
  vehicle belongs, a line where a line belongs), a model resource must be installed. A failure refuses the action
  (`action_refused`, reason in `mod.log`); it is never applied to whatever shares the id.
- **Result binding.** `makeVehicleBuyCmd`, `makeVehicleReplaceCmd` and `makeLineCreateCmd` create an entity. The key of
  the action that made it names it on every game; several creations in one step are served in the order the actions
  applied (`L.claimEntity`), so the n-th purchase is the n-th new vehicle everywhere, and a purchase the account could
  not pay finishes with `success = false` for its own window only.
- **Engine objects** (vehicle configuration, line, colour, date, journal entry) are read through their member lists,
  rebuilt with `api.type.<Name>.new()` and filled member by member (findings from MPFever, MIT). The sender rebuilds
  every object before shipping it and compares it with what it read; a difference keeps the command native and
  `mod.log` names the member (`sent natively: argument 3: does not rebuild equal at TransportVehicleConfig.vehicles.1.x`).
  Members the engine refuses with "read only" are left to it and noted once.
- **Model resources** travel by name beside the id (`modelRep.getName` / `find`, both used by the game's achievement
  script); the receiver uses its own id and notes it when the two differ.
- **Permission category.** The mod now sends the category with every action (the host's grants only ever saw `other`
  before). Table below.

## Commands

| Command | Arguments (call sites) | Category | Mode / status |
|---|---|---|---|
| `makeVehicleBuyCmd` | player, depot, `TransportVehicleConfig`; result `resultVehicleEntity` (`vehicle_react_util.tl`) | spend | **Replicated**, creates a vehicle |
| `makeVehicleReplaceCmd` | vehicle, config (`vehicle_react_util.tl`, mission `replace_vehicles.tl`) | spend | **Replicated**; treated as creating a vehicle (MPFever did), see verification V6 |
| `makeVehicleSellCmd` | **list** of vehicles (`vehicle.tl`, `manager_window.tl`) | bulldoze | **Replicated** |
| `makeVehicleSetLineCmd` | vehicle, line, stop index (`manager_window.tl`, `vehicle_react_util.tl`) | other | **Replicated** (was generic) |
| `makeVehicleSendToDepotCmd` | vehicle, flag (`vehicle.tl`, `manager_window.tl`) | other | **Replicated** |
| `makeVehicleReverseCmd` | vehicle | other | **Replicated** |
| `makeVehicleSetStoppedByUserCmd` | vehicle, flag | other | **Replicated** |
| `makeVehicleTryToDepartCmd`, `makeVehicleSetManualDepartureCmd`, `makeVehicleSetModifiersCmd` | no call site in the base scripts; vehicle first (MPFever) | other | Registered with the vehicle checked and the rest as plain values; unverified |
| `makeLineCreateCmd` | name, colour `Vec3f`, player, `Line`; result `resultEntities[1][1]` (`manager_window.tl`) | build | **Replicated**, creates a line |
| `makeLineUpdateCmd` | line, `Line` ("update cannot change entity id"): stops, waiting times, load modes, cargo filters, priority (`manager_window.tl`, `cargofilter_window.tl`, `line.tl`) | build | **Replicated**; not with waypoints (N3) |
| `makeLineDestroyCmd` | line (the window sells the line's vehicles first, in the same frame) | bulldoze | **Replicated** |
| `makeEntitySetNameCmd` | any entity: line, vehicle, station group, depot, town, the company (the player entity) | other | **Replicated**; the player travels as "own company" |
| `makeEntitySetColorCmd` | entity, `Vec3f` (lines, vehicles) | other | **Replicated** |
| `makeJournalBookAssetCmd` | player, `JournalEntry`, optional popup position; from windows: the sandbox balance edit (`game_bar.tl`), the cost of a marketing campaign (`marketing_campaign_tool.script.tl`) | spend | **Replicated** when a window sends it; the bridge's own corrections use the game's `sendCommand` and are never replicated |
| `makeGameSetCalendarSpeedCmd`, `makeGameSetDateCmd` (a `Date`: year, month, day), `makeGameSetTimeOfDayCmd`, `makeGameSetCloudCoverageCmd` | calendar widget (`game_bar_widgets.tl`) | other | **Replicated** (only the window's calls: the game-time script's own run in every game) |
| `makeStockListDiscardCargoCmd` | stock owner, cargo ids, fraction (`entity_window_util.tl`) | other | **Replicated** |
| `makeTownBuildingSetBlockedDevelopmentCmd` | town building ("historic preservation", `town_building.tl`) | other | **Replicated**; the building is found by file and position |
| `makeTownSetDevelopmentActiveCmd`, `makeTownSetInitialLandUseCapacitiesCmd`, `makeTownCustomDistributionWeightsCmd`, `makeTownUpdateCargoNeedsCmd` | town, plain values (`town_eow.script.tl`, the town editor) | other | **Replicated** |

### Events (`makeScriptingSendEventCmd(src, id, name, param)`)

One engine command carries dozens of messages to game scripts. It has one spec, in the lockstep core; modules register
the `(id, name)` pairs they replicate (`events` in the module's return table). **An event nobody registered stays
native and is never refused**, whatever the grants (category `local`): notifications, tutorial and mission steps,
guide events and the like are per-game state and intentionally local.

| Event | Parameter | Category | Status |
|---|---|---|---|
| `Loan` / `Obtain`, `Repay` | `{offer-replacement or nil, loan}` (`finances_loan_gui.tl`) | spend | **Replicated**; the loan script of every game pays out and books the repayments (see Money) |
| `Subvention` / `onAccept`, `onDecline` | `{uid}` (`subventions_gui.tl`) | other | **Replicated** |
| `Companies` / `startMarketingCampaign` | town, company, marketing params, permit key (`marketing_campaign_tool.script.tl`) | spend | **Replicated**; the cost is a second command, `makeJournalBookAssetCmd`, which the tool sends in its callback |
| `Companies` / `spawnIndustry` | company, town, types, permit key, cargo type (`construction_react_util.tl`) | spend | **Replicated** (prospecting) |
| `Companies` / `MakeGreen` | company, industry construction, permit key (`industry_greenify_tool.script.tl`) | spend | **Replicated** |
| `Towns` / `setTownRatingSensitivity`, `resetTownRatingSensitivity` | town (`town_eow.script.tl`) | other | **Replicated** |
| `GameTime` / `SetMode` (not `Local`), `OnManualGameTimeChanged`, `SkipPhase`; `CloudCoverage` / `SetMode`, `OnManualCloudCoverageChanged`, `SkipPhase` | calendar widget | other | **Replicated**; `Local` is N4 |
| `Companies` / `applyLevel` | `{level}` (`company.tl`) | | company module |

### Everything else, and why it is not here

| Command | What it is | Left to |
|---|---|---|
| `makeWorldBuildProposalCmd`, `makeWorldReplaceTerrainCmd` | construction, terrain | construction module |
| `makeEntitySetPlayerCmd`, `makeGameAddPlayerCmd` | ownership, new players (missions, debug panel) | company module |
| `makeGameSetSpeedCmd` | speed and pause | the session (the bridge turns it into `speed_request`) |
| `makeIndustrySetDespawnTimeCmd`, `makeJournalLogEntryCmd`, `makeTownUpdateSizeCmd`, `makeEntitySetEmissionsCmd`, `makeCustomEntity*Cmd`, `makeCustomVehicleCreateOrUpdateCmd` | issued by game scripts in the simulation state; every game runs the same script at the same step | not replicated, by design |
| `makeTownCreateCmd`, `makeTownDestroyCmd`, `makeTownAutoDetectConnectionsCmd`, `makeTownConnectWithIndustriesCmd`, `makeWorldSetBulldozableCmd` | map editor and mission setup | not available in a played session |
| `makeCreateIndustryExtendProposalCmd` | asks the engine for a proposal (the industry window's expand preview); the build that follows is `makeWorldBuildProposalCmd` | construction module |
| `makeAnimalSetStateCmd`, `makeAnimalSpawnAtCmd`, `makeIndustrySetManualDevelopmentCmd`, `makeJournalClearAllCmd`, `makeStockListSetModifiersCmd`, `makeStockListSetStocksCargoTypeCmd`, `makeTownDevelopAtCmd`, `makeWorldChangeWindCmd` | engine factories with no call site in the base scripts (`makeStockListSetStocksCargoTypeCmd` is commented out in `entity_window_util.tl`) | revisit if a mod calls them |
| `api.cmd.debug.*` (`makeStockSetCargoAmountCmd`, `makeSimPersonSetStateCmd`, `makeMaintenanceCostUpdateCmd`) | debug panel | not replicated |

The base windows have no per-vehicle orders or timetables: waiting times, load modes, cargo filters and the reservation
priority are fields of the line's stops and travel in `makeLineCreateCmd` / `makeLineUpdateCmd`. Cargo and passenger
routing is a consequence of lines and stations, not a command. No settings command appears in the base scripts; the
only "settings" that reach the engine from a window are the calendar and weather ones above.

## Shared with the other modules

`L.registerCommands` replaces a spec of the same name, and the bridge loads build, then ops, then company. Names this
module registers that another module may want too: `makeEntitySetNameCmd` (the company's name), `makeEntitySetColorCmd`,
`makeJournalBookAssetCmd`. A module that registers one of them replaces this module's spec; give it the same
argument handling (references checked, objects rebuilt) or leave it out. "Own company" (`{ __ref = "player" }`) means
the player entity of the game that applies the action, which is right while all players share one company; separate
companies (roadmap 0.6) need the company module to say whose company a name, a purchase or a booking belongs to.
Events do not collide: register them through
the `events` table of the module (`{ Companies = { applyLevel = {...} } }`) and they are added to the others'. A
module that registers `makeScriptingSendEventCmd` as a command would replace the dispatcher and silence every event;
`events` is the way.

## Permission categories

`build` line create and update; `bulldoze` vehicle sale and line destruction; `spend` vehicle purchase and
replacement, journal bookings, loans, marketing, prospecting, greening; `other` the rest. `local` is the category of
an event nobody replicates: no grant covers it, so a host that denies `other` does not stop a player from dismissing a
notification. Back-compat: `makeVehicleBuyCmd` is still `spend`, `makeLineCreateCmd` `build`, `makeVehicleSetLineCmd`
`other`.

## Money

The balance authority (host's `balance` and `loan` values, corrected at checkpoints) is unchanged. What changed is the
loan, and it was a placeholder:

- TF3 has **no loan command**. The loan is a game script (`game_mechanics/finance/loan.gs`, state `LoanTable`: offers,
  obtained loans, `freeId`). The window sends the event `Loan` / `Obtain` or `Repay`; the script books one
  `JournalEntry` of category `LOAN` through `makeJournalBookAssetCmd` (+amount to pay a loan out, -amount to repay), and
  its `update` books a `LOAN` and an `INTEREST` entry every month. The account's `loan` and `balance` follow the
  journal.
- So the loan is replicated by replicating the **event**; every game's script then does the rest with the same state,
  offers, and (the mod's deterministic `math.random`) cooldowns. The values in the event are the sender's offers; only
  the offer type is used from the replacement slot.
- `engine.adjustLoan(delta)`, the authority's last resort, now books a `LOAN` entry of `delta` and the same sum back as
  `OTHER`, so the balance is untouched. That a `LOAN` entry raises the reported loan by its amount is read from the
  script, not measured (V8); if a correction moves the loan the wrong way the adapter notices at the next one, logs why,
  and stops instead of doubling the error every round.

## Not replicable yet

- **N1. Session entities nobody bound.** A key is trustworthy for savegame entities and for what a replicated command
  created. A road-side stop (an edge object, no building) made in the session has no building to hang a hint on: until
  `feverlink3_cmds_build.lua` binds station groups of such stops, a line over one falls back to `s:<id>` (the check on
  arrival catches a missing or wrong-kind entity, not a wrong one of the right kind). The same holds for any entity a
  game script made. Constructions are the exception: found by file and position.
- **N2. Entity ids in other arguments.** Only the arguments listed are translated. A command whose arguments hold
  entities somewhere this module does not look would carry raw ids; none of the registered ones does.
- **N3. Waypoints on a line** name network edges (`edgePos.edgeId.entity`), whose ids no two games share. A line with any
  waypoint stays native (`sent natively: ... has waypoints`), which is a visible divergence repaired by the next resync.
  Needs the build module's edge references.
- **N4. The `Local` time-of-day mode** follows each machine's wall clock (`os.date` in `game_time.script.tl`): no two
  games agree. The event stays native and the mod logs why; do not use it in a session.
- **N5. Anything a game script sends from the simulation state** is run by every game itself. If such a script is not
  deterministic (wall clock, a random call that is not the mod's), it diverges and only the checksum round notices.
- **N6. Callback success.** For commands that create nothing, the originator's callback gets `success = true` once the
  simulation applied the action; the simulation state cannot read the engine's verdict (callbacks are refused there).
  A purchase is the exception (its watch times out after 10 steps when nothing appeared).
- **N7. Checksum coverage.** The checksum parts are balance, loan, constructions and line stop counts. Vehicles, names,
  colours, loans and the other things above are not hashed, so a divergence in them is found by the player, not by the
  host. Adding parts changes `test_checksum_rounds` in `test_lockstep.py`, which fixes the set of part names.
- **N8. Scale.** Finding a construction by file and position walks all constructions (a few thousand to a few hundred
  thousand with town buildings) and only happens when its key did not fit.

## Needs game verification

None of this has run in the game. Each item says what to do and what to compare; `mod.log` is in
`%LOCALAPPDATA%\FeverLink3\live` (one per game; do not read them while a game is writing, copy first).

**One game is enough for most of it.** A host with no other player still captures, encodes (including the dry run that
rebuilds every engine object), stamps and applies its own commands at their stamp, so the codec guesses (V1, V2, V4's
hint, V5, V6, V8, V9, V12, V13) show up as a `sent natively` line or as an `applied` / `created entity` line in its own
`mod.log`. Items that need a second game say so (V3, V10, and the "same on both" comparisons).

1. **V1. Engine objects.** In a session (host alone is enough), buy one bus in a depot of the savegame. Log: an
   `action <id>:<n> makeVehicleBuyCmd stamped ...` line, then `applied at t=<T>` and `created entity <id>
   (TRANSPORT_VEHICLE)` a step or two later, and **no** `sent natively: argument 3: ...` line. With a second game: the
   other game's log shows the same `<T>`, both vehicle lists show the bus, the money in the top bar is the same. If a
   game says `sent natively`, the text after it names the member (`does not rebuild equal at ...`, or the engine's own
   message): that is what to fix in the schema or the read-only rule.
2. **V2. Read-only members.** `[FeverLink3 ops] read-only member 'x' of Line is left to the engine` lines list what the
   engine derives. Expected for a few members, not for `stops`, `vehicles` or `purchaseTime`.
3. **V3. Model ids.** After V1, `[FeverLink3 ops] model <name> is <n> here and <m> on the sending game` in the host log
   means the two games number the model differently (the name was used). No line means the ids are the same.
4. **V4. Depots and station groups of the session.** The host builds a depot (construction module), the client buys in
   it and creates a line over two new stations. Compare the `action` lines in the client's `from_game.log`: the depot
   and the stop's station group should carry `at` with the building's key (`0:<seq>`), not only `s:<id>`. Both games
   then show the bus on the line. If the stops are road-side stops, see N1.
5. **V5. Lines.** Create a line, change a waiting time and a cargo filter, rename and recolour it, from each side. Compare
   the line manager on both games; `sent natively` must not appear. Delete a line with a vehicle on it: refused in
   both; sell the vehicle first: works in both.
6. **V6. Replacing a vehicle.** Upgrade one vehicle to another model. Host log, one of `action <k> created entity <id>
   (TRANSPORT_VEHICLE)` (a new entity: replacement works as modelled, later commands on it find it) or `action <k>:
   nothing created after 10 steps` (the engine changed the vehicle in place: set `creates = nil` for
   `makeVehicleReplaceCmd`, one line in the module).
7. **V7. Several purchases in one step.** Clone a vehicle five times (the window sends five purchases). Five new vehicles on
   both; sell the third one: the same one disappears on both (compare names or positions in the list).
8. **V8. Loans.** Take a loan, with the game running and again paused (host alone is enough for the first half). Log:
   `action <id>:<n> makeScriptingSendEventCmd stamped ...` and `applied`, no `sent natively`; the finance window shows
   the obtained loan. With a second game both show the same balance and loan in the next report (parts `balance` and
   `loan` equal in the two `checksum_report` lines of `from_game.log`). This also verifies the assumption behind
   `adjustLoan`: in the host's `checksum_report` lines the `authority` field holds the real `balance` and `loan`;
   two reports on either side of the loan must differ by the loan's amount in **both** numbers (+amount each; the
   money spent meanwhile moves only the balance, so take the loan with nothing else running). Then repay it: both go
   back down. `adjustLoan` itself only runs when a divergence is found; the unit tests cover it with a mock.
9. **V9. Marketing and money.** Start a marketing campaign on a town from the client: both balances drop by the same
   cost, once. `from_game.log` of the client holds two `action` lines (the event and the journal booking).
10. **V10. Permissions.** The host revokes `spend` for the client in the window. The client's purchase, loan and campaign
    are refused at once (the window's callback gets a failure), `action_refused` appears in the host's log, and the
    host's `mod.log`/launcher shows `may not spend` if the category arrived (the category is new in the payload).
    Dismissing a notification still works with `other` denied.
11. **V11. Calendar.** Change the calendar speed and the time of day on the client; the date and the sky agree on both
    after a minute. Switch time of day to Local on the client: the log says it is not replicated.
12. **V12. Towns and industries.** Greenify an industry, set a town rating sensitivity, accept a subsidy, toggle
    historic preservation on a building; the other game shows the same industry/town state. If the historic
    preservation click ends in `sent natively: argument 1: cannot refer to entity <n> as a construction`, a town
    building is not a construction entity in this build and its reference needs the building's own component.
13. **V13. Sale and rename.** Sell a bus from the client, rename a station group and the company from the host: same
    result on both; the company name is the same although the two games' player entities differ.
14. **V14. Time cost.** Note any frame hitch when an action is applied, in particular after N8 (an industry or building
    found by position).
