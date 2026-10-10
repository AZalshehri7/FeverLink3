# Construction coverage

What the mod does with every construction-related command and proposal call of Transport Fever 3 (stage 0.4: "constructions against streets, upgrades, signals, decorations, terrain"). Code: `mod/feverlink3_1/content/feverlink3_cmds_build.lua`. Tests: `tests/lua/test_cmds_build.py`. Design background: `docs/ARCHITECTURE.md` sections 5 and 6, `docs/NATIVE_LINK.md`.

**Nothing here has run in the real game.** The argument lists below come from the game's own scripts (`gui.zip`, `mission.zip`, `game_mechanics.zip` in the game's `base\content`, read-only) and from MPFever (MIT), which ran them. Everything the code assumes about the engine without proof is listed in "Needs game verification" at the end, each with the in-game steps and the log line that settles it.

## 1. How a build reaches the mod

| Path | What happens | State |
|---|---|---|
| **A. Lua** | A game window or script calls `api.cmd.makeWorldBuildProposalCmd(...)` and `api.cmd.sendCommand`. The bridge's wrapped `sendCommand` holds it, `encode` turns the proposal into a build record, the host stamps it, every game (the originator included) rebuilds and applies it at the stamp. | Implemented and tested with doubles of the engine's userdata. Reachable in the game from the entity windows: bridge/tunnel swap, double slip switch, traffic-light configuration, construction parameters panel (section 9, steps 6 to 8). |
| **B. Tool** | The street, track, station, depot, stop, signal, bulldozer, upgrade, lane and terrain tools build natively: the command never passes through Lua. | With the native module's hold on (`docs/NATIVE_LINK.md`, stage 2; off by default) the mod runs **strict mode** for the tools whose command it can hold: the tool event `builder.proposalApply` is captured with `M.captureToolBuild` (the same body as `encode`), matched to the module's `held` event, shipped with a stamp, the held command is cancelled and every game, the originator's included, applies the build at the stamp (`NATIVE_LINK.md` section 7; tests `tests/lua/test_strict_build.py` against a fake module; **not run in the game**, stage 4 there). A build that cannot be captured or rebuilt is released and goes the old way: the engine applies it at the click, the simulation half captures it from `onPreBuildProposal` and the peers rebuild it at the stamp (replay on peers, the whole path without the module). The bridge also logs tool events and rebuilds previews as a dry run (see 7). |
| **C. Simulation script** | Town growth, industry extension, missions call `makeWorldBuildProposalCmd` inside the simulation state (`industries.script`, `company.script`, `mission_*`). | Industry extension and missions: not replicated, on purpose, every game runs the same script at the same step. The wrap only exists in the interface half. Town growth is not a script (the engine builds the town's buildings itself) and it is not deterministic, so the simulation half holds it still during a session (`ARCHITECTURE.md` section 7). |

Replication modes (`ARCHITECTURE.md` section 6): **strict** = held and applied at the stamp everywhere; **replay on peers** = applied natively at the click, peers a few steps later; **poll** = detected afterwards by checksum.

## 2. Command makers (`api.cmd`)

Construction-related ones only. Arguments as the game's scripts call them.

| Name | What it does | Arguments | Mode | Status |
|---|---|---|---|---|
| `makeWorldBuildProposalCmd` | Applies a proposal: roads, tracks, segments, nodes, edge objects (stops, signals), node configurations, constructions to add or remove, terrain grid. The one command behind all construction. | `proposal` (SimpleProposal or Proposal), `context` (Context or nil), `ignoreErrors`, `playerInitiated`, `doDust` (optional). Callback `(data, success, resultEntities)`; `data.proposal`, `data.resultProposalData.errorState.messages`, `data.resultEntities` = `{ {entity, revision} }`. | A: strict. B: strict needs the hold. C: not replicated. | A implemented (9 plans, section 5). B not wired. |
| `makeWorldReplaceTerrainCmd` | Map editor: regenerate the whole terrain from a heightmap. Not available in a running game. The in-game raise/lower/level/paint tools use the proposal's terrain grid instead (see 4). | `map, terrainConfig, seed, world, keepAssets` | none | Registered only to refuse with a clear reason; stays native. |
| `makeWorldSetBulldozableCmd` | Marks a construction as not bulldozable (missions). | `entity, bulldozable` | none | Not replicated: sent by the mission script in the simulation state on every game. |
| `makeCreateIndustryExtendProposalCmd` | Asks the engine for an industry-extension proposal (a query: the window shows it). The extension itself is a `makeWorldBuildProposalCmd`. | `constructionEntity, 0, 1` | none | Not a change of the world. |
| `makeEntitySetPlayerCmd` | Changes the owner of a construction, segment, vehicle (missions, ownership transfer). | `entity, player` | strict | Company area, not covered here. |
| `makeTownBuildingSetBlockedDevelopmentCmd` | Blocks a town building from development (town window). | `buildingEntity, blocked` | strict | Town/operations area, not covered here. |
| `makeTownCreateCmd`, `makeTownDestroyCmd` | Map editor only. | | none | Not available in a running game. |

## 3. Proposal and helper API (`api.engine.util`, `api.engine.system`, `api.gui`)

How each is used by the game, and what the mod does about it. "Query" = changes nothing.

| Name | What it does | Arguments | Role here | Status |
|---|---|---|---|---|
| `util.proposal.createProposalReplaceConstruction` | Proposal replacing a construction with new parameters (construction parameters panel, stock-list cargo pin, missions). | `entity, params [, fileName]` | Reaches `makeWorldBuildProposalCmd` through path A: record with `cons`, `rem`, `old2new` | Plan `construction`. Native `old2new` value type unverified. |
| `util.proposal.createProposalRemove` | Proposal removing a construction. The game's own industry removal removes the street pieces that go with it (`findSegmentsToDelete`) in a second proposal. | `entity, context` | Path A or B: `rem`, plus segments when the tool took them | Plans `construction` / `bulldoze`. |
| `util.proposal.refreshConstruction` | Rebuilds a construction from its script (debug panel, missions). | `entity [, fileName]` | Path A | Plan `construction`. |
| `util.proposal.makeSegmentsRemoveProposal` | The engine's own proposal for removing segments (and merging what is left). | `edges` | **Used by the mod** to replay a bulldozing (plan `removal`) | Used; behaviour from MPFever. |
| `util.proposal.replaceSegment` | The engine's own upgrade of one segment to a template. Not used by the game's scripts; found by MPFever. | `edge [, template]` | **Used by the mod** to replay an upgrade (plan `upgrade`), one command per segment | Used; behaviour from MPFever. |
| `util.proposal.createBridgeOrTunnelProposal` | Swaps the bridge or tunnel type of a parallel strip (entity window). | `stripEntity, id` | Path A: segments of another kind. Parallel-strip data and id maps of the proposal are not carried | Plan `street`; **partial**, verify. |
| `util.proposal.createDoubleSlipSwitchProposal` | Toggles a double slip switch on a node. | `node, bool` | Path A: node configuration | Plan `nodecfg`. |
| `util.proposal.createTrafficLightProposal` | Sets the traffic-light configuration of a node. | `node, trafficLightConfig, preference` | Path A: node configuration. Lane connections, crosswalks, preference, double slip are carried; the phases of `trafficLightConfig` are **not** | Plan `nodecfg`; **partial**. |
| `util.proposal.createIndustryProposal`, `makeIndustrySpawnProposal`, `makeIndustryExtendProposal` | Industries (map editor, company growth, industry window). | | Map editor, or simulation state | Not replicated. |
| `util.proposal.createSubconstructionLotList`, `getConstructionArea`, `checkProposalConnectsNodes`, `getBridgeOrTunnelRelatedRevisions`, `didBridgeOrTunnelRelatedRevisionsChange` | Queries about a proposal. | | none | Not needed. |
| `util.construction.findSegmentsToDelete` | Query: the street pieces that go with a construction. | `construction [, flag]` | none | Not needed; the record carries what the tool removed. |
| `util.construction.getConstructionEntity`, `getConstructionResult` | Queries. | | none | Not needed. |
| `util.octree.findEntitiesInCircle` | Query: entities with a component within a circle. | `Vec2f, radius, ComponentType` | **Used** to find a node (BASE_NODE, or the ends of BASE_EDGE) and a construction by place | Used; which component types it answers for is probed (section 6). |
| `system.streetSystem.getNodeStreetSegments`, `getNodeTrackSegments` | Query: the segments at a node. | `node` | **Used** to find a segment from its end node | Used (the game's own entity windows call them). |
| `system.streetSystem.getNode2SegmentMap`, `getEdgeForEdgeObject` | Queries (MPFever). | | Fallback node scan; the segment carrying an object | Used as fallbacks. |
| `system.stationGroupSystem.getStationGroup` | Query: the group of a station. | `station` | Keys for the station groups of a built station | Used. |
| `api.gui.construction.getRefundableEntities`, `updateRefundableEntities`, `provideDustWheneverPossible` | Refund list after a bridge swap; the dust cloud. | | none | Not replicated (money is corrected by host authority). |

### The tools (`Builtin.Type.ConstructionAction.*`)

Each builds natively (path B). What its proposal contains and the plan that rebuilds it:

| Tool | The player | Proposal | Plan | Status |
|---|---|---|---|---|
| `StreetEdgeBuilder`, `TrackEdgeBuilder` | Lays roads and tracks, joins, splits | New nodes and segments, removed segments for a split | `street` (or `combined` when it moves town buildings) | Codec tested; capture not wired |
| `StreetEdgeNodeModifier`, `TrackEdgeModifier` | Upgrades/downgrades, bus lane, tram track, catenary, decorations | Segments replaced between the same nodes | `upgrade` (a replacement by another kind of segment is `street`) | Codec tested; capture not wired |
| `EdgeObjectBuilder` | Stops, signals, waypoints | The segment replaced by itself with one object more; the object has **no model before the build ran** | `stop`; needs a capture after the build | Codec tested with a post-build capture; **cannot be captured before** |
| `ConstructionBuilder` | Stations, depots, buildings | A construction, plus street pieces when placed against a street | `construction`, `withStreet`, `combined` | Codec tested; capture not wired |
| `ModuleBuilder`, `ModuleBulldozer` | Modules of modular stations | Construction replaced with other parameters | `construction` | Assumed; verify |
| `Bulldozer` | Demolishes | Removed segments, constructions, objects | `removal`, `construction`, `bulldoze`, `stop` | Codec tested; capture not wired |
| `LaneModifier`, `CrosswalkModifier`, traffic-light tool | Lane connections, crosswalks, lights | Node configurations only | `nodecfg` | Codec tested; the phases of a light are not carried |
| `TerrainModifier` | Raise, lower, smooth, flatten | The proposal's `terrain.baseHeightMod` grid | none | **Not replicable from Lua** (section 8) |
| `TerrainPainter`, `AssetBrush` | Ground paint, trees, rocks, decoration | Terrain layers, asset groups | none | **Not replicated** (section 8) |
| `TownBuilder` | Towns (map editor) | | none | Not in a running game |

## 4. The proposal types as the mod reads them

Names from the game's scripts and MPFever. Native lists also have a `<name>_native` view (`size()`, `at(i)`) that is faster than copying the list.

- **SimpleProposal** (what scripts build): `constructionsToAdd` (ConstructionEntity: `fileName, params, transf, name, playerEntity`), `constructionsToRemove` (ids), `old2new` (`{ [oldEntity] = index into constructionsToAdd, 0-based }`), `streetProposal` (SimpleStreetProposal: `nodesToAdd`, `edgesToAdd`, `nodesToRemove`, `edgesToRemove`, `edgeObjectsToAdd`, `edgeObjectsToRemove`, `nodeConfigsToAdd`, `nodeConfigsToRemove`). New entities carry negative placeholder ids.
- **Proposal** (what tools and `api.engine.util.proposal.*` return): `toAdd`, `toRemove`, `old2new`, `proposal` (`addedNodes`, `addedSegments`, `removedNodes`, `removedSegments` as records with `entity` and `comp`, `edgeObjectsToAdd`, `edgeObjectsToRemove`, node configurations, and id maps the mod does not carry), `terrain.baseHeightMod` (a grid: `x0, y0, width, height`).
- **NodeAndEntity** `{ entity, comp = { position } }`; **SegmentAndEntity** `{ entity, type (0 street, 1 track), comp = { node0, node1, tangent0, tangent1, position0, position1, type (normal/bridge/tunnel), typeIndex, roadType, roadTemplate, roadStyle, laneConfigs, objects = { {object, kind} }, edgeDecorations, distance, roadDevelopmentLocked }, streetEdge = { precedenceNode0, precedenceNode1 }, playerOwned = { player } }`; edge object kinds: 0 stop on the left, 1 on the right, 2 signal.
- **Context** `{ player, checkTerrainAlignment, cleanupStreetGraph, gatherBuildings, gatherFields, refundableEntities }`.
- Properties of engine objects return **copies**: every nested change is read, changed and written back (finding from MPFever).

## 5. The build record and the plans

`encode` returns `{ fn = "makeWorldBuildProposalCmd", args = { n = 5, <record>, <context>, ignoreErrors, playerInitiated, doDust } }`. The record is plain data (`feverlink3_common.lua` serialises it; positions keep every digit):

```
v = 1, src = "simple" | "native"
cons[]     { file, params, transf (16 numbers), name, owner }      constructions to add
rem[]      construction references                                 constructions to remove
old2new[]  { rank in rem, value }
nodes[]    { id (placeholder or reference), pos }
segs[]     { id, type, n0, n1, c = { type, typeIndex, roadType, roadTemplate, roadStyle, dev, distance, t0, t1, p0, p1, deco },
             lanes (only when not the template's), objects[] { object, kind }, prec, owner }
rmNodes[], rmSegs[] (+ ty, ti, rt, st: the kind of the removed segment), objs[], rmObjs[], ncfg[], rmNcfg[]
terrain    { x0, y0, w, h }   a marker; the cells are not carried
```

The street pieces a construction brings with it (templates under `/constructions/`, or `simple.street_template`) are recorded **lightly** (template, kind, end nodes if readable): the construction's own script makes them, and what cannot be read about them must not stop the construction from being copied.

`context` is `false` (the original had none), `{ align, cleanup, buildings, fields }`, or `{ native = true }` (a tool's build: the plan's default is used).

`classify(record)` picks the plan from the record's shape; `decode` runs it:

| Plan | Shape | Rebuilt as | Commands | Origin of the approach |
|---|---|---|---|---|
| `street` | New nodes/segments, splits, objects; or any street change not below | SimpleProposal with a street part | 1 | Game: `track_builder`, `electrify`; MPFever "minimal raw" forms |
| `upgrade` | Each added segment replaces a removed one between the same nodes, same kind | `util.proposal.replaceSegment(edge, template)` per segment, decorations and custom lanes put on its proposal | 1 per segment (chained) | MPFever |
| `removal` | Removed segments, nothing added but merges the engine redoes | `util.proposal.makeSegmentsRemoveProposal(edges)` | 1 | MPFever |
| `stop` | One segment replaced, objects added/removed | SimpleProposal replacing the segment, new objects marked by placeholders `-400000000 - k` in its object list | 1 | MPFever |
| `construction` | Constructions added/removed/replaced; any street part is the construction's own | SimpleProposal with constructions only; default seed `7x + 3y + z` | 1 | Game: `mission_framework_util_entity.buildConstruction`, `upgradeConstruction`, `bulldoze` |
| `withStreet` | A construction with its own street pieces (depot/street station against a road) | Street part first (without the construction's own pieces), then the construction with `ignoreErrors = true, playerInitiated = false` | 2 (chained) | MPFever (the engine refuses both together) |
| `combined` | Street work that also moves or replaces constructions (a road that takes town buildings) | One SimpleProposal with both parts | 1 | MPFever |
| `bulldoze` | Constructions removed together with segments | Constructions first, then the segments through `makeSegmentsRemoveProposal` | 2 (chained) | Game: `industry_util.removeIndustries` |
| `nodecfg` | Node configurations only | SimpleProposal with `nodeConfigsToAdd` | 1 | MPFever |

Each plan lists **forms** tried in order until the engine's factory accepts one. A factory that rejects has no side effect, so every game goes through the same attempts. Forms: node configurations left out (the engine makes its own), owner/precedence members left out, placeholders renumbered the way a script does (nodes and segments each from -1), edge object model with or without the `::/` prefix. The log says `(form N)` when the first did not do. If no form passes, the action is refused on every game.

Contexts when the record has none of its own (`{ native = true }`): `terrainNoCleanup` (align, no cleanup, buildings, fields) for `street`, `nodecfg`, `construction`, `withStreet`, `combined`; `terrain` (align, cleanup, buildings, fields) for `removal`, `upgrade` and the segments of a `bulldoze`; `plain` (no align, cleanup, no buildings, fields) for `stop` and the constructions of a `bulldoze`. Names and flag sets are MPFever's replay forms; which of them the tools really use is open (verification 2).

Order and determinism: records keep the engine's list order; candidates are sorted by distance, then by place; map-like data travels as ranked lists; nothing reads `pairs` order, time or `math.random`.

## 6. References and keys

An entity that exists already is a table with its **key** and its **place**:

| Kind | Place | Key |
|---|---|---|
| node | position | `ctx.keyOf(id)` |
| segment | both end positions and both tangents | same |
| construction | file and position | same |
| edge object | the segment that carries it and its rank there | none |
| player | | `{ __ref = "player" }` |

The key is `s:<id>` for an entity of the savegame, `<sender>:<seq>#c<i>` (construction i of a build), `.../d<j>` (its depots), `.../s<j>` (stations), `.../g<j>` (their station groups) for what a replicated build created: the bridge binds them when the construction appears (matched by file and place, not by id, so a town building that grew in the same step is not taken for ours). Later actions name the depot or station through these keys; `tests/lua/test_cmds_build.py` buys a vehicle at a depot the session built.

**Road-side stops** have no building to be named by (the operations module's `describe` finds no owner for them, `COVERAGE_OPS.md` N1). The build record says how many stops it puts on a street (every edge object that is not a signal), and the bridge looks for the new `EDGE_OBJECT` and `STATION_GROUP` entities of the build: `<sender>:<seq>#o<n>` is the n-th new edge object that is also a station (ascending entity id, which is creation order), and `<sender>:<seq>#o<n>/g` the station group it belongs to **when that group is new**. A stop that joins a group which was there before gets no group key: the group keeps the key it had. Binding is all or none: fewer new stops than the record asked for means the engine is not done, and it is looked for again on the next step (up to ten). Two builds applied in the same step do not take each other's entities (what another action's keys name is not offered again). The names mean the same stop on every game only if the engine makes the stops of one proposal in the same order on every game, which is what the lockstep needs of it anyway; nothing else tells two games' stops apart (verification 14).

Resolving on the applying game: the key is used **only if the entity it names lies where the reference says** (0.5 m; construction 1.0 m); otherwise the place is searched (nodes 1.0 m, constructions 3.0 m, segments by their end nodes, tangents break ties). Nothing found: `unresolved <what>`; two equally good: `ambiguous <what>`. Either refuses the **whole action** (`action_refused`) rather than apply it with a wrong id.

Nodes are looked up by the octree on BASE_NODE if it answers, else on the ends of BASE_EDGE, else a full scan of the street graph (BASE_NODE and BASE_EDGE cannot be walked with `forEachEntityWithComponent`: finding from MPFever). The first lookup probes which works and logs `references: nodes are looked up by ...`.

`ARCHITECTURE.md` section 6 says roads match "within 1.5 m, else split edge or create node". The mod searches within 1.0 m and **never creates or splits on its own**: a node it cannot find refuses the action. (A tool's proposal already contains the split it made; creating another would be a second guess.)

Nothing from the wire is trusted: `normalise` checks version, list sizes (200 constructions, 4000 nodes or segments ...), types, finite numbers, file names (`.con`), text length, parameter depth. `encode` and `decode` never raise.

## 7. What the bridge and lockstep modules gained

All additive (no existing line changed in meaning). `tests/lua/test_cmds_build.py` covers each.

- The permission category of a build comes from main's `spec.categoryFor(args)` hook (the build schema returns `bulldoze` for a proposal that only removes things, `build` for anything else, and never raises); the spec's `category = "build"` is the fallback. (An earlier version of this branch had its own function-valued `category`; the merge with main dropped it.)
- Actions of more than one command: `args.chain` (argument lists or functions) with `L.sendNow` (simulation half, commands run as they are sent) and `L.runChained` (interface half, one callback per command, stops at the first failure).
- Keys for created entities: `spec.bindsComp` (a component name, a list of names, or a function of the body returning either) and `spec.bind(body, key, made)` with `L.bindsComp`, `L.bindMap`, `L.newEntities`; `made` is the ascending list of the new entities of all the named components. A watch list `st.binders` in the simulation state (up to `WATCH_STEPS` steps) drops what another action's keys already name. The interface half, when it applies an action during a pause, takes `made` from the engine's callback and from the same component diff.
- Owners follow the company contract (`docs/COMPANIES.md` 3.7): the record writes the player's own entity as `"player"` and another owner (a second company's entity) as `{ __ref = "owner", key }`; the applying game takes `"player"` from `ctx.player` (the sender's company in a company session, never `getPlayer()`) for `Context.player`, `ConstructionEntity.playerEntity`, `EdgeObject.playerEntity` and `PlayerOwned.player`, and resolves a key through the action's bindings or refuses (`unresolved owner <key>`). A town building's owner stays `-1`.
- The interface half now catches a command factory that rejects what was rebuilt (it used to raise out of `route`) and answers `action_refused`.
- `guiHandleEvent` forwards `builder.*` tool events to the schemas' `onToolEvent`. The build schema logs each kind of event once (`tool event streetBuilder/builder.proposalCreate`), and for the first previews of each tool and plan **captures the proposal, sends it through the wire form and rebuilds it, calling the factory but sending nothing**: `tool preview streetBuilder: street rebuilds (3.1 ms, 2310 bytes)` or `... does NOT rebuild: <reason>` or `not capturable: <reason>`. For the first applied builds it logs `tool build ...: <plan>, counts` and the member names of the first segment, node and edge object, to compare with the fields the module reads.

## 8. What cannot be replicated yet, and why

1. **Terrain edits.** The grid is in `proposal.terrain.baseHeightMod`; a script sees its size (`x0, y0, width, height`), never its cells, and cannot write them (MPFever). Sending an empty grid of the right size would flatten the ground. Needs the native module's cell copy (`terrain_apply` in `NATIVE_LINK.md`). Until then a terrain edit stays native, is refused with a reason, and the difference stays until a checkpoint sees it: there is **no terrain checksum part** yet. A terrain grid that rides with other work is dropped from the copy and logged.
2. **Ground paint, trees, rocks, asset brush.** Terrain layers and asset groups are not in the part of the proposal the mod reads; not copied.
3. **Tool builds (path B)** until the mod uses the native hold (the module has it, `NATIVE_LINK.md`; the mod's half is written, strict mode in section 7 there, and not yet run in the game) or an interim `builder.proposalApply` capture feeds `M.capture`. Roads built with a tool today exist on one game only, and **the checkpoint does not see it**: the checksum parts are `constructions` and `lines`, so a lost segment or node is invisible. Add road-graph parts (`edges`, `nodes`) before relying on any of this.
4. **Stops and signals before the build ran.** The tool's edge object carries no model name until the engine has built it (MPFever); a capture before the build is refused (`an edge object has no model`). They need a capture after the build (`builder.proposalApply`, with the created entities), which means replay on peers, not strict.
5. **A depot or street station against a road** is two commands (street part, then the construction). The simulation half cannot see the engine's verdict of the first before sending the second (no callbacks there), so a refusal of the second command shows only as a difference at the next checkpoint. MPFever found this route by trying forms against the engine's verdicts; the mod can only try forms against the factory.
6. **What the proposals of the engine's helpers carry beyond segments and constructions.** Bridge/tunnel swaps (parallel-strip data), traffic-light phases, the old-to-new maps of nodes and segments, frozen nodes are not carried. The originator applies the rebuilt version too, so the games agree with each other, not necessarily with what the player previewed.
7. **Callbacks of Lua-originated builds** receive `nil` data in the running game (the result comes through the simulation state). Windows that read `cmdData.resultEntities` log a failed callback; nothing else breaks.
8. **Engine refusals in the running game.** A build the engine refuses (collision, money) is invisible to the simulation half; peers refuse or accept alike only if their worlds are alike. The checkpoint and `replay_failed` cover the rest.
9. **Keys of stops rest on creation order.** `#o<n>` is the n-th new stop by entity id. If the engine ever hands out an id out of creation order (a reused id), two games can give the same name to different stops, and nothing in the build record can tell (an edge object's world position is not read). The count is checked, the order is not (verification 14).
10. **Waypoints on a line** (`COVERAGE_OPS.md` N3) name network edges. This module can describe an edge (its end positions and tangents) and find it again on another game, but the operations module does not call it, so a line with a waypoint stays native.
11. **Hold-less stops.** Until the native hold or the interim capture feeds `M.capture`, no stop a tool puts on a road reaches another game, so the stop keys above are reached only by scripted builds. The tests cover the logic with a mock of the engine; the game has not seen it.

## 9. Needs game verification

Run two games from the in-game window (host and player), open `%LOCALAPPDATA%\FeverLink3\live\mod.log` on both. Entries marked **(no second game)** need only one game.

1. **Tool events arrive in the interface half. (no second game)** Select the street tool and move the cursor; click once to build a road. Expect `tool event streetBuilder/builder.proposalCreate` and `.../builder.proposalApply` (the tool ids may differ; list them). If nothing appears the events are not delivered to `guiHandleEvent` and the capture entry has to move (the simulation state receives `apply_command` / `onPreBuildProposal` instead).
2. **Real tool proposals capture and rebuild. (no second game)** For each action below, read the `tool preview <tool>: ...` and `tool build ...` lines: new road in an empty field; junction on an existing road; road through town buildings; road with a bridge; new track; track onto an existing track end; rail station in a field; road depot against a road; bus stop on a road; signal on a track; bulldoze a road segment, a house, a station; upgrade a road and electrify a track; lane connection; traffic light; terrain raise. Expected plans: `street`, `street`, `combined`, `street`, `street`, `street`, `construction`, `withStreet`, `not capturable: an edge object has no model`, same, `removal`, `construction`, `construction`/`bulldoze`, `upgrade`, `upgrade`, `nodecfg`, `nodecfg`, `a terrain edit: ...`. Anything `does NOT rebuild` or `not capturable` otherwise is a capture or rebuild gap; send the line with the `segment members:`, `node members:` and `edge object:` lines.
3. **Members the module reads are the engine's.** Compare the `segment members:` line with `node0,node1,tangent0,tangent1,position0,position1,type,typeIndex,objects,laneConfigs,roadTemplate,roadStyle,roadType,edgeDecorations,distance,roadDevelopmentLocked`, `node members:` with `position`, and the `edge object:` line with `edgeEntity | segmentEntity, param, oneWay, left, model, category, resultEntity, playerEntity, name`. A member that matters and is missing from the module's list is added to `readSegment` (and the rebuild).
4. **Node lookup. (no second game)** The first reference resolved logs `references: nodes are looked up by octree-node | octree-edge | scan`. `scan` means the octree answers for neither BASE_NODE nor BASE_EDGE (or ignores the radius): builds near other builds will be slow on a big map. To trigger it: use path A on a junction (step 6).
5. **Post-build capture of a signal. (no second game)** Place a signal on a track. In the `tool build streetTerminalBuilder/builder.proposalApply: ...` line expect plan `stop`. If it says `an edge object has no model` even after the build, the created object does not carry the model in `edgeObjectConstruction`; read the `edge object:` line and fix `readEdgeObject` (adapter `A.comp(e, "EDGE_OBJECT")`).
6. **Path A end to end.** On the host, select a road junction with three or more street segments and change its traffic-light preference in the entity window (or toggle a double slip switch on a four-track diamond). Expect on the host `action 0:N makeWorldBuildProposalCmd stamped T`, then on both `build nodecfg (native): ...` and `applied at t=T`; no `(form N)`. Compare the junction on both screens, and the next `constructions` checksum.
7. **Construction replaced by path A.** Select an existing modular station or depot in the construction menu, change a parameter (length, tracks). Expect `build construction (native): 1+1 constructions` on both games and the new construction at the same place. Then check the keys: in the simulation log `action 0:N created 1 CONSTRUCTION entities, keyed`.
8. **Bridge swap.** Select a bridge, change its type in the entity window. Expect `build street (native)` (a segment replaced by another kind) on both games and the same bridge. If the games differ, the parallel-strip data matter (section 8.6).
9. **Order and verdict of chained commands.** Needs the hold; when it exists: build a depot against a road on the client. Expect on the host two `build` lines in order (`withStreet ... 2 commands`), the depot at the same place, and `1:N#c1/d1` bound. In the running game the second command's verdict is not visible: compare the depot on both games.
10. **Units and sides.** An upgrade with a decoration on a segment that runs the other way round on the other game: the decoration must stay on the same side (the mod flips it when the local segment's first node is not at the record's first node).
11. **Lane write shift.** Not needed unless the log shows lanes differing: `A.laneShift()` measures it on first use; compare a tram track laid on a road on both games.
12. **`::/` prefix of edge object models.** First form strips it; the second keeps it. `(form 2)` on a `stop` build tells which the engine wants.
13. **Previews are cheap.** The `tool preview` line prints milliseconds; on a long road (hundreds of segments) it should stay in the low tens of milliseconds, or the capture needs the `_native` views checked.
14. **Stops and their groups. (needs the hold, or a scripted stop)** Check first, on one game, that a stop answers as assumed: its entity has components `EDGE_OBJECT` and `STATION`, `api.engine.system.stationGroupSystem.getStationGroup(stop)` returns a group, and `forEachEntityWithComponent` accepts `EDGE_OBJECT` and `STATION_GROUP` (a failing call is swallowed and shows as `not every entity it created was found after 10 steps` in the simulation log). Then place two bus stops in one proposal (left and right side) and a third in another build, and create a line over the first stop from the client: both games must show the line over the same stop (rename its station group on one game and look at the other). Log: `action 0:N created K EDGE_OBJECT/STATION_GROUP entities, keyed`; K counts the stops and the groups that are new. If the two games name different stops alike, the engine does not hand out ids in creation order: the module then needs a geometric name for a stop (the segment it stands on and its place on it).

15. **Owner of what is built in a company session. (needs two companies, `COMPANIES.md` E5 and E6)** Build a road and a depot as the second company. The new segments, the depot and the money taken must be that company's on every game. If the engine ignores `Context.player` and the `playerEntity` fields in favour of its own `getPlayer()`, the builds belong to whoever the applying game's local company is, and the construction schema cannot fix that from a script.

## 10. Wiring the native capture

For the person writing the hold (`NATIVE_LINK.md`):

- `local body, why = M.capture(proposal, { ctx = <Context or nil>, ignoreErrors = false, playerInitiated = true, result = <created entities, after the build> })` (module `feverlink3_cmds_build.lua`, returned by `ug_require`) returns the same body as the wrapped `sendCommand` would ship, or `nil` and the reason (the build then stays native).
- Ship it as an `action` line with a stamp. In strict mode as built the held command is **cancelled** and the item is routed to the originator's own queue too (`route(it)` in the bridge), so the originator rebuilds it at the stamp like every other game; for a build the engine applied itself (replay on peers) the simulation half sets `body.native` and the originator does not apply it again.
- The keys of what the build created (constructions, depots, stations, road-side stops and their groups) are bound by the bridge when the originator's action is queued like any other (`bindsComp`), so the originator must queue it too if it wants keys for its own new constructions and stops; for a natively applied build it can bind them by calling `M.commands.makeWorldBuildProposalCmd.bind(body, key, createdEntities)` itself, passing every entity the build made (the new ones with components `CONSTRUCTION`, `EDGE_OBJECT` and `STATION_GROUP` at least).
- After a build ran, entities it removed are gone from the world; the capture uses what the proposal itself says about them (full records in a tool's `removedNodes`/`removedSegments`). A removed **construction** has no such record: capture a bulldozing **before** it is applied.
