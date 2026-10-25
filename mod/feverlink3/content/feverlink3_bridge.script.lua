-- FeverLink3 bridge: the game-script glue between the engine, the lockstep core and the launcher's file link.
-- Thin on purpose: every rule lives in feverlink3_lockstep.lua, every byte format in feverlink3_common.lua.
--
-- Two halves, two Lua states (finding from MPFever (MIT): the game runs update/handleEvent in the simulation
-- state and guiUpdate/guiHandleEvent in the interface state; module locals are not shared between them, the
-- game-script `state` object is readable from both and written from the simulation side, and the interface
-- reaches the simulation through api.cmd.makeScriptingSendEventCmd -> handleEvent).
--
--   interface half (guiUpdate, every frame)
--     reads to_game.log, keeps session and peer clocks, paces the engine speed, sends clock lines,
--     wraps api.cmd.sendCommand so a player's command is held, encoded and shipped as an `action`,
--     forwards stamped items to the simulation half, applies due items itself while the game is paused,
--     hands the originator's callback the real result, writes checksum reports and refusals,
--     strict mode (when the native module is there): has a construction tool's build held and shipped; the other games replay it
--     at the stamp and this game releases its own held command at the stamp, so the tool stays alive
--     (native_ctl.<half>.w<id>.log / native_events.log, docs/NATIVE_LINK.md section 7).
--   simulation half (update + postUpdate, once per step: update only reads, postUpdate writes; handleEvent)
--     keeps the queue in the game-script state, applies due items at their exact step, computes checksum
--     parts, installs the deterministic math.random.
--
-- Without FEVERLINK3_SESSION_DIR (game not started by FeverLink3.exe) every entry point returns at once.

local okC, C = pcall(ug_require, "feverlink3_common.lua")
if not okC then C = ug_require("feverlink3::/feverlink3_common.lua") end
local okL, L = pcall(ug_require, "feverlink3_lockstep.lua")
if not okL then L = ug_require("feverlink3::/feverlink3_lockstep.lua") end
L.clock = C.clock   -- the real clock of its checksum timing (the lockstep module names no os function: feverlink3_link.lua)

local okH, Hud = pcall(ug_require, "feverlink3_hud.lua")
if not okH then Hud = ug_require("feverlink3::/feverlink3_hud.lua") end
-- every system message a player reads (feverlink3_text.lua); one local, the file's main chunk is near the game's limit of locals
local T
okH, T = pcall(ug_require, "feverlink3_text.lua")
if not okH then T = ug_require("feverlink3::/feverlink3_text.lua") end

-- Command schemas of the other areas (construction, operations, companies): one module each, all optional. A module
-- that is missing or fails to load is skipped; the commands it would have carried stay native.
local CMDS = {}
local Co = nil -- companies: registry, attribution, ledger (feverlink3_cmds_company.lua, docs/COMPANIES.md); nil without that module
for _, name in ipairs({ "feverlink3_cmds_build", "feverlink3_cmds_ops", "feverlink3_cmds_company" }) do
	local okM, M = pcall(ug_require, name .. ".lua")
	if not okM then okM, M = pcall(ug_require, "feverlink3::/" .. name .. ".lua") end
	if okM and type(M) == "table" then
		L.registerCommands(M.commands)
		CMDS[#CMDS + 1] = M
		if M.events then L.registerEvents(M.events) end
		if type(M.company) == "table" then Co = M.company end
	end
end

-- The portable text of the native module's held commands as a table (B2); optional, nil without the module.
local okP, Portable = pcall(ug_require, "feverlink3_portable.lua")
if not okP then okP, Portable = pcall(ug_require, "feverlink3::/feverlink3_portable.lua") end
if not okP or type(Portable) ~= "table" then Portable = nil end

-- The construction module captures what the native tools build (see simCaptureBuild).
local Build = nil
for _, M in ipairs(CMDS) do
	if type(M.capture) == "function" then Build = M end
end

-- Developer probe (only when the game was started with FEVERLINK3_DEV=1, which a player never does): Lua text the developer
-- drops into the session folder as dev_gui.lua / dev_sim.lua is run once in that half's Lua state (the interface state
-- or the simulation state) and its result is appended to dev_gui_out.txt / dev_sim_out.txt. It is how the real shapes of
-- the engine's objects are looked at without restarting the game for every question. Inert without the variable.
local DEV = false
pcall(function() DEV = C.env("FEVERLINK3_DEV") == "1" end)

-- FEVERLINK3_NATIVE_PATCH=1 (default off): the native module may set the converted proposal's frozenNodes at the Add of the next build command (the
-- control order patch_next, docs/NATIVE_LINK.md), which a SimpleProposal cannot carry. Only with it, and with a module that reports the capability,
-- the portable replay builds a construction (its frozen node); without either the build falls back to the record path.
local NATIVE_PATCH = false
pcall(function() NATIVE_PATCH = C.env("FEVERLINK3_NATIVE_PATCH") == "1" end)
-- FEVERLINK3_NATIVE_INJECT (default ON since 2026-10-14, owner; `=0` switches it off): the originator ships the native module's portable text of
-- the held command and every game has the module build the engine's Proposal from it and swap it into its replay's command (inject_next,
-- docs/NATIVE_LINK.md). The old record path still serves what does not go through injection (no native text, a module without the capability).
local NATIVE_INJECT = true
pcall(function() NATIVE_INJECT = C.env("FEVERLINK3_NATIVE_INJECT") ~= "0" end)
-- the largest native text a record released without one of its own carries (a complete text of a station is ~80 KB)
local NATIVE_TEXT_MAX = 1024 * 1024
-- the longest action body the host relays (core HostSession::MAX_ACTION_BODY): a longer one is dropped there and reaches no other game
local ACTION_BODY_MAX = 480 * 1024

-- Strict mode, how the ORIGINATING game applies its own build. FEVERLINK3_STRICT_CANCEL:
--   "0"          hold-and-release for every build (developer opt-out; was the default until 2026-10-13). The tool's command is held until the stamp and then released, so the tool gets its
--                completion callback and its preview/ghost keeps working. The originator's engine applies the tool's own command while the
--                other games replay a rebuilt record: they allocate different entity ids for the same build and the worlds drift apart a few
--                game minutes later (lab runs 2026-10-12), which the checksum safety net repairs with a resync.
--   "plans"      cancel-and-replay on EVERY game, the originator included, for the plans street, combined and bulldoze (all originators).
--   "host"       the same, but only when this game is the host (the guest keeps hold-and-release).
--   "1"          cancel-and-replay for every kind and every originator.
--   unset / "removals"  DEFAULT. Cancel-and-replay ONLY for builds that take things away and add nothing (a bulldozer removing a town building or a
--                segment), every originator, and only when the module fires the tool's callback (FEVERLINK3_STRICT_FIRE_DONE=1). Why: in
--                hold-and-release the originator's engine applies its own command 1000-1600 ms of game time after the stamp (real run
--                2026-10-13, screenshot pass: stamp 2505800, engine event at 2506800) while every other game applies it AT the stamp; a freed
--                town lot is then seen by the town's growth logic at different times and a different building grows (constructions /
--                townBuildings drift, resync). Cancel-and-replay applies it at the stamp on every game.
-- Cancel-and-replay keeps all worlds identical (N0 runs: 0 resyncs in 30 game minutes) but a cancelled command never gives the tool its
-- completion callback: in the first real two-player session the guest's street tool showed no ghost for the second road he built
-- (the click still built it). TpF2 fires the callback before it cancels; that is not implemented here (the callback's signature on this
-- build is unverified), so the cancel flow is opt-in until it is.
-- Real two-window test 2026-10-13: 11 of 11 demolitions of town buildings with the bulldozer, cancelled and replayed, left the checksums
-- identical and the bulldozer usable; in hold-and-release 2 of about 6 drifted (the originator's engine applied 4-5 steps after the stamp).
local STRICT_CANCEL = "removals"
pcall(function()
	local v = C.env("FEVERLINK3_STRICT_CANCEL")
	if v == "0" then STRICT_CANCEL = "off"
	elseif v == "1" then STRICT_CANCEL = "all" elseif v == "plans" or v == "host" or v == "removals" then STRICT_CANCEL = v end
end)
-- FEVERLINK3_STRICT_FIRE_DONE (default ON since 2026-10-13, =0 switches it off; the native module reads the same variable): the native module fires the tool's own completion callback before it drops a
-- cancelled command (the TpF2 recipe; docs/NATIVE_LINK.md), which re-arms the tool: without it the tool's busy flag stays set and its
-- ghost/preview never comes back. It does so only for the tools whose callback was read out of the exe (street, bulldozer: the held event
-- says `done = true`). With the switch on, a build is cancelled only when its held command says so; otherwise it stays hold-and-release.
local FIRE_DONE = true
pcall(function() FIRE_DONE = C.env("FEVERLINK3_STRICT_FIRE_DONE") ~= "0" end)
-- SECTIONS. Lua allows 200 locals per function and the main chunk of this file is one function: the game refused the bridge with "too
-- many local variables (limit is 200) in main function" at 197 top-level names (real run rc1, 2026-10-14; the game counts a few more than
-- lupa does), and every entry point then failed. So the larger sections below sit in `do ... end` blocks, not indented to keep diffs
-- small: a name declared in a section is local to that section, and the names other sections use are declared just above its `do`
-- (`local a, b` there, `function a()` / `a = ...` inside). A new helper goes into the section that uses it (declared above the `do` only
-- when another section needs it), new module-level state into X. tests/lua/test_mod_files.py measures the peak with the compiler.
-- The few module-level values added by the 2026-10-14 work live in one table.
local X = {}
-- Which load of this file this is, and in which Lua state (a research aid, docs/research/TODO.md option 1): X is made anew each time the
-- file runs, the state's marker (C.STATE, in package.loaded) only once per Lua state. Two simulation Lua states show two state
-- addresses; the script loaded twice into one state shows one state address with two X addresses and two load ids. The same text is
-- the `from` column of every control line this load writes; the module logs it for each new writer (docs/NATIVE_LINK.md section 3).
X.LOAD_ID = C.newId()
X.SRC = tostring(C.STATE_ADDR) .. "/" .. tostring(C.STATE.id) .. "/" .. X.LOAD_ID .. "/" .. C.addressOf(X)
pcall(C.logger("load"), "script load: bridge " .. X.SRC .. " (" .. C.stateTag() .. ")")
-- which official replacements of the link module's io/os uses this state offers (observation only, lab: docs/ARCHITECTURE.md section 2)
pcall(function() C.logger("load")("official api probe: " .. C.officialProbe()) end)
-- Test switch (never set by a player): FEVERLINK3_DEBUG_REFUSE_INJECT=1 (or the older values link / fail) makes the native module refuse the
-- inject order of this game's own replay of its deferred build, after the order's identity checks, like any real refusal: the command is
-- emptied (nothing of the build is applied here), the held command fails (its tool callback is fired with a failed result) and this game
-- asks for a resync. Proves in a real run that a refusal applies nothing and leaves no tool busy. Since 2026-10-15 there is no record-path
-- fallback to force: a build with the originator's text is injected or not applied.
X.DEBUG_REFUSE_INJECT = nil
pcall(function()
	local v = C.env("FEVERLINK3_DEBUG_REFUSE_INJECT")
	if v == "1" or v == "link" or v == "fail" then X.DEBUG_REFUSE_INJECT = v end
end)
-- Test switches (never set by a player): FEVERLINK3_TEST_NO_MARGIN / FEVERLINK3_TEST_NO_LEAD take the measured stamp margin (and the host's
-- lead) away from deferred builds, to measure what they buy (L.testMargin). Unset: nil, nothing changes.
X.TEST_MARGIN = nil
pcall(function() X.TEST_MARGIN = L.testMargin(C.env("FEVERLINK3_TEST_NO_MARGIN"), C.env("FEVERLINK3_TEST_NO_LEAD")) end)
local STRICT_CANCEL_PLANS = { street = true, combined = true, bulldoze = true }

local function devPoll(half)
	if not DEV then return end
	local path = C.path("dev_" .. half .. ".lua")
	local text = C.readFile(path)
	if type(text) ~= "string" or text == "" then return end
	C.writeFile(path, "")
	local lines = {}
	local env = setmetatable({
		api = api, C = C, L = L, Build = Build, CMDS = CMDS,
		print = function(...)
			local t = {}
			for i = 1, select("#", ...) do t[#t + 1] = tostring((select(i, ...))) end
			lines[#lines + 1] = table.concat(t, "\t")
		end,
	}, { __index = _G })
	local f, err = load(text, "dev_" .. half, "t", env)
	local out
	if not f then
		out = "COMPILE ERROR: " .. tostring(err)
	else
		local ok, res = pcall(f)
		if ok then
			local okS, ser = pcall(C.serialize, res)
			out = (#lines > 0 and (table.concat(lines, "\n") .. "\n") or "") .. "RESULT: " .. (okS and ser or tostring(res))
		else
			out = (#lines > 0 and (table.concat(lines, "\n") .. "\n") or "") .. "ERROR: " .. tostring(res)
		end
	end
	C.dump("dev_" .. half .. "_out.txt", "---- t=" .. tostring(C.clock()) .. "\n" .. out .. "\n")
end

local STEP = L.STEP
-- A deferred held command not completed this long after its stamp (game time AND real seconds since the stamp was reached) fails.
X.DEFER_GRACE_GAME_MS = 10 * L.STEP
X.DEFER_GRACE_S = 3

-- ---- per-build timing (one `build.timing` line per build on every game, test builds; owner 2026-10-14)
-- Every stage time is wall-clock milliseconds since 1970, so the lines of two games compare. The native module gives the wall clock and
-- the C runtime's clock at one instant (`timing clock`); C.clock() (the C runtime's clock() through feverlink3_link.lua) reads that same C runtime clock, so wall = X.WALL0 + C.clock() * 1000.
-- The format is fixed: "timing key=<k> held=<id> click=<wall>@g<game time> cap=<start>..<end> cap_ms=<ms> bytes=<n> ... ship=<wall>@g<t>
-- recv=<wall>@g<t> stamp=<t> step=<t> ... rebuild=<start>..<end> rebuild_ms=<ms> how=<injected|refused (nothing applied)|refused (NOT emptied)|record (complete_next)>
-- apply=<wall> accepted=<bool> total_ms=<apply - click> late_steps=<(step - stamp) / STEP>".
X.WALL0 = nil
function X.wallNow()
	if not X.WALL0 then return 0 end
	return math.floor(X.WALL0 + C.clock() * 1000 + 0.5)
end

-- ---- the latency logs (owner list 2026-10-17, items 4 and 6: the input barrier's stand-stills and the build lag), test builds, always on
-- Every clock and action line this game writes carries `hop = { g = <wall ms> }`; the launchers and the host add their stamps on the way
-- (core Hops) and the receiving game measures each hop and the gaps between a peer's clocks (feverlink3_linkstats.lua, X.LS; nil when
-- that module is missing). Lines: `barrier hold ... peer_t= behind_steps= room_steps= clock_age_ms= rtt_ms= frame_ms= engine_speed=`,
-- `frame.slow`, `build.lag`, and a minute's `link.minute` / `link.peer` (trace.log; link.minute also in mod.log). Observation only.
X.LS = nil
pcall(function()
	local okS, M = pcall(ug_require, "feverlink3_linkstats.lua")
	if not okS then okS, M = pcall(ug_require, "feverlink3::/feverlink3_linkstats.lua") end
	if okS and type(M) == "table" then X.LS = M end
end)
-- the first stamp of a clock or action line; none before the native module gave the wall clock (X.WALL0)
function X.stampHop(payload)
	local w = X.wallNow()
	if w > 0 and type(payload) == "table" then payload.hop = { g = w } end
	return payload
end
local REPLAY_STEPS = 12 -- a replayed build's engine event is expected within this many steps of the command
local OUT_KEEP = 40
local RESULT_KEEP = 60
local WATCH_STEPS = 10
local unpack = table.unpack or unpack

-- ================================================================== engine accessor

local O = {} -- the game's own command functions, before wrapping (filled by installWrap)

-- Every engine read the core needs, in one table so the tests can mock it. The functions below are the TF3 API
-- surface this mod relies on (names from MPFever's use of the API, MIT): GAME_TIME and GAME_SPEED are components
-- of the world entity, ACCOUNT of the player entity, a construction's position is the translation column
-- (indices 13..15) of its transf matrix, lines come from lineSystem.getLines() with their stops in the LINE
-- component.
local function entitiesWith(compName)
	local ids = {}
	if compName == "LINE" then
		pcall(function()
			local l = api.engine.system.lineSystem.getLines()
			for i = 1, #l do ids[#ids + 1] = l[i] end
		end)
	elseif compName == "GROUP_STATION" then
		-- every station (road-side stops included) as the station groups list them: the world scan for EDGE_OBJECT finds none of a stop
		-- (sweep 20261013-053645/055034), and a stop beside a station joins a group that exists, so no group is new either
		local groups = {}
		pcall(api.engine.forEachEntityWithComponent, function(e) groups[#groups + 1] = e end, api.type.ComponentType.STATION_GROUP)
		for _, g in ipairs(groups) do
			pcall(function()
				local sg = api.engine.getComponent(g, api.type.ComponentType.STATION_GROUP)
				for i = 1, sg and sg.stations and #sg.stations or 0 do ids[#ids + 1] = sg.stations[i] end
			end)
		end
	else
		pcall(api.engine.forEachEntityWithComponent, function(e) ids[#ids + 1] = e end, api.type.ComponentType[compName])
	end
	table.sort(ids)
	return ids
end

local engine = {
	-- the entity ids of the world's edges, constructions and town buildings (sorted): the `ids` checksum lane. Every game hands out ids
	-- from the same free-id FIFO, so they are equal while every build is applied at the same step everywhere; a build applied at
	-- another step on one game (any path that misses the stamp rule) makes them differ, and the lane detects that for a repair.
	entityIds = function()
		return { edges = entitiesWith("BASE_EDGE"), constructions = entitiesWith("CONSTRUCTION"), townBuildings = entitiesWith("TOWN_BUILDING") }
	end,
	gameTime = function()
		return api.engine.getComponent(api.engine.util.getWorld(), api.type.ComponentType.GAME_TIME).gameTime
	end,
	speed = function()
		local gs = api.engine.getComponent(api.engine.util.getWorld(), api.type.ComponentType.GAME_SPEED)
		return gs and gs.speedup or 0
	end,
	player = function()
		return api.engine.util.getPlayer()
	end,
	account = function()
		local a = api.engine.getComponent(api.engine.util.getPlayer(), api.type.ComponentType.ACCOUNT)
		return a.balance, a.loan
	end,
	-- the constructions checksum lane, one entity at a time (engine.construction below; a spread round reads a chunk a step, L.SPREAD_LANES)
	constructionIds = function() return entitiesWith("CONSTRUCTION") end,
	lines = function()
		local out = {}
		for _, e in ipairs(entitiesWith("LINE")) do
			local l = api.engine.getComponent(e, api.type.ComponentType.LINE)
			out[#out + 1] = { stops = l and l.stops and #l.stops or 0 }
		end
		return out
	end,
	entitiesWith = entitiesWith,

	-- Books `amount` as a journal entry of the given type (OTHER, LOAN...) on the player's account: how a game's
	-- balance is brought to the host's value (finding from MPFever (MIT): JournalEntry with time -1 and category OTHER
	-- through makeJournalBookAssetCmd). Sent with the game's own sendCommand, so it is never replicated.
	bookJournal = function(amount, kind)
		local entry = api.type.JournalEntry.new()
		entry.amount = amount
		entry.time = -1
		entry.category.type = api.type.JournalEntry.Type[kind or "OTHER"]
		local send = O.sendCommand or api.cmd.sendCommand
		send(api.cmd.makeJournalBookAssetCmd(api.engine.util.getPlayer(), entry))
	end,
	-- adjustLoan is defined below the table (it uses the account reader above).

	-- Save and load go through the application object, which exists in the interface state (finding from
	-- MPFever (MIT): app.saveGame(name, callback, false, true); a savegame is found with
	-- app.findAllSavegames(app.SaveGameNamespace.getSavegame()) and loaded with app.loadGame(id, false)).
	saveGame = function(name, done)
		local ok, err = pcall(app.saveGame, name, function() done(true) end, false, true)
		if not ok then done(false, tostring(err)) end
	end,
	loadGame = function(name)
		local ns = app.SaveGameNamespace.getSavegame()
		for _, s in ipairs(app.findAllSavegames(ns) or {}) do
			if s.saveName == name then
				local id = api.type.SavegameId.new()
				id.path = s.path
				id.saveGameName = s.saveName
				id.saveGameNamespace = ns
				app.loadGame(id, false)
				return true
			end
		end
		return false, "savegame '" .. tostring(name) .. "' not found"
	end,
}

-- Moves the loan by `delta` without moving the balance. TF3 keeps no loan command: the loan is a game script
-- (game_mechanics/finance/loan.script.tl) and the only thing that changes an account's loan is a journal entry of
-- category LOAN. The script pays a loan out with one (+amount) and takes its repayment with one (-amount), so such an
-- entry moves the balance by the same amount; the sum is booked back as OTHER to leave the balance alone. That a LOAN
-- entry of +x raises the reported loan by x is read from the script, not measured: should a correction move the loan
-- the wrong way, the next one notices and this switches itself off instead of doubling the error every round.
do
	local last, off = nil, false
	local logEngine = C.logger("engine")
	engine.adjustLoan = function(delta)
		if off then error("loan correction is off: the loan moved the wrong way after the last one") end
		local _, loan = engine.account()
		if last and type(loan) == "number" and type(last.loan) == "number" then
			local moved = loan - last.loan
			if moved * last.delta < 0 and math.abs(moved) >= math.abs(last.delta) / 2 then
				off = true
				local why = "loan correction is off: the loan moved by " .. moved .. " after a correction of " .. last.delta
				logEngine(why .. " (a LOAN journal entry is assumed to raise the loan by its amount; see docs/COVERAGE_OPS.md)")
				error(why)
			end
		end
		engine.bookJournal(delta, "LOAN")
		engine.bookJournal(-delta, "OTHER")
		last = { delta = delta, loan = loan }
	end
end

-- Read-only accessors behind the wider checksum lanes (stations, edges, nodeConfigs, vehicles, names; see
-- L.checksumParts). Plain data out, no ids. Field choices after MPFever (MIT) mpfever_refs.lua contentHash. Each reads
-- one component per entity, so a round costs about one component read per station group + terminal node, road edge
-- (+ node, cached, + edge object), vehicle, named entity.
do
	local function comp(e, name)
		local ok, c = pcall(api.engine.getComponent, e, api.type.ComponentType[name])
		return ok and c or nil
	end
	local function nameOf(e)
		local n = comp(e, "NAME")
		return n and n.name and tostring(n.name) or ""
	end
	local function guard(fn) local ok, v = pcall(fn); return ok and v or nil end
	local function lineKey(e)
		local l = comp(e, "LINE")
		return nameOf(e) .. "#" .. tostring(l and l.stops and #l.stops or 0)
	end
	local function nodeAt(cache, n)
		local p = cache[n]
		if p == nil then
			local c = comp(n, "BASE_NODE")
			p = c and c.position and { x = c.position.x, y = c.position.y } or false
			cache[n] = p
		end
		return p
	end

	engine.stations = function()
		local out, nodes = {}, {}
		for _, g in ipairs(entitiesWith("STATION_GROUP")) do
			local sg = comp(g, "STATION_GROUP")
			local item = { stations = 0, terminals = 0, at = {} }
			for i = 1, sg and sg.stations and #sg.stations or 0 do
				item.stations = item.stations + 1
				local st = comp(sg.stations[i], "STATION")
				for k = 1, st and st.terminals and #st.terminals or 0 do
					item.terminals = item.terminals + 1
					local p = nodeAt(nodes, st.terminals[k].vehicleNodeId)
					if p then item.at[#item.at + 1] = p end
				end
			end
			out[#out + 1] = item
		end
		return out
	end

	-- The street lanes (edges, nodeConfigs) read one entity at a time, so a spread round reads them a chunk a step (L.SPREAD_LANES). The
	-- lists are taken once, at the round's stamp: every edge and every node of the street system, sorted (the same order on every game).
	engine.streetIds = function()
		local edges, nodes, seen = {}, {}, {}
		local map = guard(function() return api.engine.system.streetSystem.getNode2SegmentMap() end) or {}
		for n, segs in pairs(map) do
			nodes[#nodes + 1] = n
			for i = 1, #segs do
				local e = segs[i]
				if not seen[e] then seen[e] = true; edges[#edges + 1] = e end
			end
		end
		table.sort(edges)
		table.sort(nodes)
		return { edges = edges, nodes = nodes }
	end

	-- A spread round reads an entity some steps after it took the lists: one removed since then (a building the town replaced, a road a
	-- player bulldozed: on every game at the same step) reads as nothing, never as a component read of an entity that is gone.
	local function gone(e)
		local ok, v = pcall(api.engine.entityExists, e)
		return ok and v == false
	end

	engine.construction = function(e)
		local c = (not gone(e)) and comp(e, "CONSTRUCTION")
		if not c then return nil end
		local m = c.transf
		return { file = c.fileName, x = m and m[13], y = m and m[14], z = m and m[15] }
	end

	-- `ctx` (a table the round keeps): its node positions are read once a round
	engine.edge = function(e, ctx)
		ctx.nodes = ctx.nodes or {}
		local c = (not gone(e)) and comp(e, "BASE_EDGE")
		local a, b = c and nodeAt(ctx.nodes, c.node0), c and nodeAt(ctx.nodes, c.node1)
		if not (a and b) then return nil end
		local objs = {}
		for k = 1, #(c.objects or {}) do
			local eo = comp(c.objects[k][1], "EDGE_OBJECT")
			objs[#objs + 1] = eo and tostring(eo.edgeObjectConstruction) or "?"
		end
		return {
			x0 = a.x, y0 = a.y, x1 = b.x, y1 = b.y, type = tostring(c.type), typeIndex = c.typeIndex,
			roadType = tostring(c.roadType), template = tostring(c.roadTemplate), style = tostring(c.roadStyle),
			decorations = guard(function() return #c.edgeDecorations end), objects = objs,
		}
	end

	engine.nodeConfig = function(n)
		if gone(n) then return nil end
		local nc, c = comp(n, "BASE_NODE_CONFIG"), comp(n, "BASE_NODE")
		if not (nc and c and c.position) then return nil end
		return {
			x = c.position.x, y = c.position.y, lanes = #(nc.laneConnections or {}), crosswalks = #(nc.crosswalks or {}),
			preference = tostring(nc.trafficLightPreference),
			light = guard(function() return nc.trafficLightConfig.trafficLightType end), slip = nc.doubleSlipSwitch,
		}
	end

	-- The town-buildings lane (idea from MPFever (MIT, see THIRD_PARTY_NOTICES.md): a `townBuildings` part), one building at a time. Read-only:
	-- a TOWN_BUILDING component gives its town, level (1-4), depth, timeBuilt and its CONSTRUCTION entity; that construction gives the model
	-- file and the transform. The ids of the buildings never enter a hash (the constructions rule); the town id names the part
	-- (L.SPREAD_LANES, CS.finishLane).
	engine.townBuildingIds = function() return entitiesWith("TOWN_BUILDING") end
	engine.townBuilding = function(e)
		local tb = (not gone(e)) and comp(e, "TOWN_BUILDING")
		if not tb then return nil end
		local con = tb.construction and not gone(tb.construction) and comp(tb.construction, "CONSTRUCTION") or nil
		local m = con and con.transf
		local parcels = 0
		pcall(function() parcels = #tb.parcels end)
		return { town = tb.town, file = con and con.fileName, x = m and m[13] or 0, y = m and m[14] or 0, z = m and m[15] or 0,
			level = tb.level, depth = tb.depth, parcels = parcels, timeBuilt = tb.timeBuilt }
	end

	engine.vehicles = function()
		local out, keys = {}, {}
		for _, e in ipairs(entitiesWith("TRANSPORT_VEHICLE")) do
			local v = comp(e, "TRANSPORT_VEHICLE")
			if v then
				local line = v.line and v.line >= 0 and v.line or nil
				if line and not keys[line] then keys[line] = lineKey(line) end
				out[#out + 1] = { line = line and keys[line] or "-", state = tostring(v.state), stopped = v.userStopped, sell = v.sellOnArrival }
			end
		end
		return out
	end

	engine.names = function()
		local out = {}
		for kind, cname in pairs({ stations = "STATION_GROUP", lines = "LINE", towns = "TOWN", players = "PLAYER" }) do
			local list = {}
			for _, e in ipairs(entitiesWith(cname)) do list[#list + 1] = nameOf(e) end
			out[kind] = list
		end
		return out
	end
end

-- companies: reads and bookings on a company's account (balance, journal entries, line results, the local-company switch)
if Co then Co.extendEngine(engine, { sender = function() return O.sendCommand or api.cmd.sendCommand end }) end

-- The test harness (or a game build with other accessors) replaces any of the above through a global table.
do
	local okE, override = pcall(function() return FEVERLINK3_ENGINE end)
	if okE and type(override) == "table" then
		for k, v in pairs(override) do engine[k] = v end
	end
end

-- used outside this section:
local traceSim, traceGui, checksumParts, actionCtx, coBefore, coEffects, coBalance, coAfter, engineVerdict, buildCategory, actionCategory,
	shadowCompare, update, handleEvent
do -- section: the simulation half (not indented, see SECTIONS at the top)
local logTowns = C.logger("towns")   -- the town trace (the town-buildings lane itself is L.SPREAD_LANES over engine.townBuilding)

-- Town trace (feverlink3_towntrace.lua): off unless FEVERLINK3_TOWN_TRACE is set. Read-only; one file per game.
local TRACE = nil
local TRACE_FAILS = 0
local townTraceGuard
local function townTrace()
	if TRACE ~= nil then return TRACE or nil end
	TRACE = false
	local ok, err = pcall(function()
		local spec = C.env("FEVERLINK3_TOWN_TRACE")
		if type(spec) ~= "string" or spec == "" then return end
		local okM, M = pcall(ug_require, "feverlink3_towntrace.lua")
		if not okM then okM, M = pcall(ug_require, "feverlink3::/feverlink3_towntrace.lua") end
		if not okM or type(M) ~= "table" then error("module not loaded: " .. tostring(M)) end
		local t = M.new({
			spec = spec,
			acc = M.apiAccessors(api, engine.entitiesWith),
			hash = C.hashValue,
			write = function(text) return C.dump(M.FILE, text) end,
			-- FEVERLINK3_TOWN_TRACE_CMDS=1: log the payload of the game script's makeTownUpdateSizeCmd / makeTownUpdateCargoNeedsCmd
			cmds = C.env("FEVERLINK3_TOWN_TRACE_CMDS") == "1",
			tick = function() return math.floor(engine.gameTime() / STEP) end,
		})
		if t.enabled then
			TRACE = t
			logTowns("town trace on: " .. spec .. " -> " .. M.FILE)
			if t.cmds then
				-- the holder lives in package.loaded so a savegame reload finds the wrappers instead of wrapping them again
				local holder = package.loaded["feverlink3_towntrace_holder"]
				if type(holder) ~= "table" then holder = {}; package.loaded["feverlink3_towntrace_holder"] = holder end
				holder.tracer = t
				local n = M.installCmdWrappers(api.cmd, holder, function(fn) townTraceGuard(fn) end)
				logTowns("town trace: " .. n .. " command factories wrapped (makeTownUpdateSizeCmd, makeTownUpdateCargoNeedsCmd)")
			end
		end
	end)
	if not ok then logTowns("town trace failed to start: " .. tostring(err)) end
	return TRACE or nil
end

-- a trace step must never break the simulation: after a few failures it switches itself off
function townTraceGuard(fn)
	local t = townTrace()
	if not t then return end
	local ok, err = pcall(fn, t)
	if not ok then
		TRACE_FAILS = TRACE_FAILS + 1
		logTowns("town trace error: " .. tostring(err))
		if TRACE_FAILS >= 5 then
			TRACE = false
			pcall(function() local h = package.loaded["feverlink3_towntrace_holder"]; if type(h) == "table" then h.tracer = nil end end)
			logTowns("town trace switched off after 5 errors")
		end
	end
end

-- `co` is the company registry of the half that asks; its digest is a part of its own once anything was configured or recorded
-- Checksum cost (observation only, never raises, behaviour unchanged): the seconds of every part are measured with C.clock. A
-- checksum step that takes more than CS_SLOW_MS logs one line naming the worst parts (a round hashed whole: the round; a spread round:
-- its costliest step); every CS_SUMMARY rounds one summary line (count, average, maximum and the part that was the slowest in the
-- maximum round). Each half (interface, simulation) counts its own.
traceSim, traceGui = C.tracer("sim"), C.tracer("gui")   -- trace.log (docs/TRACE.md)
local DETAIL = {}   -- the itemised keys of the last full checksum round of this state (dumped when a resync starts)
local CS_SLOW_MS, CS_SUMMARY = 50, 20
local CS = { n = 0, sum = 0, max = 0, maxPart = "?", maxWhere = "", nLight = 0, sumLight = 0, maxLight = 0 }
local function checksumNote(timing, total, where, light)
	local ok = pcall(function()
		local ms = total * 1000
		local names = {}
		for k in pairs(timing) do names[#names + 1] = k end
		table.sort(names, function(a, b) if timing[a] ~= timing[b] then return timing[a] > timing[b] end return a < b end)
		local worst = names[1] or "?"
		CS.n = CS.n + 1
		CS.sum = CS.sum + ms
		if light then
			CS.nLight, CS.sumLight = CS.nLight + 1, CS.sumLight + ms
			if ms > CS.maxLight then CS.maxLight = ms end
		end
		if ms > CS.max then CS.max, CS.maxPart, CS.maxWhere = ms, worst, where end
		if ms > CS_SLOW_MS then
			local top = {}
			for i = 1, math.min(5, #names) do top[i] = string.format("%s %.0f", names[i], timing[names[i]] * 1000) end
			C.logger(where)(string.format("checksum slow: total %.0f ms: %s", ms, table.concat(top, ", ")))
		end
		if CS.n % CS_SUMMARY == 0 then
			C.logger(where)(string.format("checksum cost over %d rounds: avg %.1f ms, max %.0f ms (%s, slowest part %s); %d light rounds: avg %.1f ms, max %.0f ms",
				CS.n, CS.sum / CS.n, CS.max, CS.maxWhere, CS.maxPart, CS.nLight, CS.nLight > 0 and CS.sumLight / CS.nLight or 0, CS.maxLight))
			CS.n, CS.sum, CS.max, CS.nLight, CS.sumLight, CS.maxLight = 0, 0, 0, 0, 0, 0
		end
	end)
	return ok
end

-- A checksum round of this half (L.newChecksumRound), begun at its step: the time, the light lanes, the ids lane, the entity lists of
-- the heavy lanes, the companies digest and the account are taken now. `it` is the queue item ({ n, full, paused, spread }); a spread
-- round (it.spread > 0, full, not paused) reads its heavy lanes a chunk at a time with X.csStep, every other round at once (checksumParts).
-- `light` (the round asked for no heavy lanes): constructions, edges, node settings and town buildings are not read.
function X.csClock() local ok, v = pcall(C.clock); return ok and tonumber(v) or 0 end
function X.csBegin(co, where, it)
	local clock = X.csClock
	local w0 = clock()
	local light = it.full == false
	-- each key is hashed as it is read (C.fnv1a of the key itself): a lane is the sum of its keys' hashes, so no step sorts or rehashes it
	-- it.chunk: the host's chunk size (from the games' measured cost), the same on every game, so every game cuts the lists alike
	local r = L.newChecksumRound(engine, C.hashValue, light, { detail = (not light) and {} or nil, clock = clock, keyHash = C.fnv1a, chunk = it.chunk })
	r.n, r.where, r.wall0, r.slip, r.late, r.over = it.n, where or "gui", w0, 0, 0, 0
	r.spread = (not light and not it.paused and type(it.spread) == "number") and it.spread or 0
	r.detailShared = r.spread > 0 and it.detail == true or nil
	r.start = r.time
	local t1 = clock()
	-- `co` false: this half's registry is not read yet (X.guiChecksum), so the part is not ready rather than the empty registry's
	if Co and co == false then
		r.parts.companies = X.NOT_READY
	else
		local okD, digest = false, nil
		if Co then okD, digest = pcall(Co.digest, co) end
		if okD and digest ~= nil then r.parts.companies = C.hashValue(digest) end
		if Co and not okD then r.parts.companies = "error" end   -- a digest that failed is unknown, like any lane that failed
	end
	r.timing.companies = clock() - t1
	local okA, account = pcall(L.accountSnapshot, engine)
	r.account = okA and account or nil
	r.startCost = clock() - w0
	return r
end

-- The next chunk of a spread round, when its step has come: chunk k is read at exactly start + k * spread steps on every game (each is
-- a landing mark the interface half set, L.checksumMarks). A chunk read later than its step (a batch that did not land on its mark, a
-- step the script did not run at) is counted: the round then reports another time than the games that read it on time, and the host
-- does not compare it (time_skew; X.csLateNote makes a run of them loud). Catching up reads one chunk a step, never more (the budget).
-- True once the round is complete.
function X.csStep(r, now)
	if r.done then return true end
	local at = L.checksumChunkAt(r, r.steps + 1)
	if now < at then return false end
	if now > at then r.slip, r.late = r.slip + (now - at), r.late + 1 end
	L.checksumRoundStep(r)
	r.mine = (r.mine or 0) + 1   -- the chunks this Lua state read (its itemised keys: DETAIL)
	local s, read = 0, {}
	for name, v in pairs(r.frame) do s = s + v; read[#read + 1] = string.format("%s %.1f", name, v * 1000) end
	table.sort(read)
	r.work = (r.work or 0) + s
	if s * 1000 > L.CHECKSUM_FRAME_BUDGET_MS then r.over = r.over + 1 end
	pcall(traceSim, "checksum.chunk", string.format("round %s chunk %d/%d at t=%s%s: %.1f ms (%s)", tostring(r.n), r.steps, r.chunks,
		C.integerText(now), now > at and (" (due " .. C.integerText(at) .. ")") or "", s * 1000, table.concat(read, ", ")))
	return r.done
end

-- What the bridge adds to a round's progress (L.checksumRoundSave keeps the rest): its number, session, half, start, spread, how late
-- its chunks were, its cost so far and the account taken at its stamp.
X.CS_FIELDS = { "n", "epoch", "where", "wall0", "slip", "late", "over", "spread", "start", "work", "account", "detailShared" }

-- ---- the itemised keys of a detail round, shared by the simulation Lua states (TWO_STATE_AUDIT_20261019, DETAIL.last)
--
-- A spread round's chunks are read by whichever simulation Lua state runs their step, so a state's own keys (DETAIL.last) cover only
-- its chunks, and the two games' dumps covered different chunks: their diff misled. Keeping every round's keys in the game-script state
-- costs about 0.5 MB of state a round, so only a round the host marks `detail` (it does after a round that differed, until one is
-- identical again: the rounds before a repair) keeps them there: each chunk's keys go into the round's record as one text per lane
-- (rec.dkeys), the start's light lanes too (rec.dstart). The finished round leaves them in st.csDetail, which the dump at a resync
-- writes and clears (the save stays small); a later round without `detail` clears them too. Same round, same chunks, same steps on
-- every game, and complete whichever state read what.
function X.csDetailStart(r, rec)
	local d = {}
	for name, items in pairs(r.detail or {}) do d[name] = table.concat(items, "\n") end
	rec.dstart, rec.dkeys = d, {}
end

-- the keys this state read in its last chunk (each lane's keys past `had`) into the record
function X.csDetailChunk(r, rec, had)
	rec.dkeys = rec.dkeys or {}
	for i, ln in ipairs(r.lanes) do
		local keys = ln.keys or {}
		if #keys > (had[i] or 0) then
			local name = ln.spec.name
			local list = rec.dkeys[name] or {}
			list[#list + 1] = table.concat(keys, "\n", (had[i] or 0) + 1, #keys)
			rec.dkeys[name] = list
		end
	end
end

-- the complete keys of a finished detail round: { time, n, lanes = { name = text } }
function X.csDetailDone(rec, t)
	local lanes = {}
	for name, text in pairs(rec.dstart or {}) do lanes[name] = text end
	for name, list in pairs(rec.dkeys or {}) do lanes[name] = table.concat(list, "\n") end
	return { time = t, n = rec.n, lanes = lanes }
end

-- Round r's progress into its record in the game-script state (`rec`, or a new one): every simulation Lua state reads it from there
-- (L.checksumRoundSave says why).
function X.csSave(r, rec)
	rec = L.checksumRoundSave(r, rec)
	for _, k in ipairs(X.CS_FIELDS) do rec[k] = r[k] end
	return rec
end

-- This Lua state's own copy of the round `rec` holds: the one it keeps (`live`, by round), brought up to the record's progress, or one
-- made from the record (the state did not begin it). A made one keeps only the itemised keys of the chunks this state reads.
function X.csLive(live, rec)
	local key = tostring(rec.epoch) .. "/" .. tostring(rec.n) .. "/" .. tostring(rec.start)
	local r = live[key]
	if r then
		L.checksumRoundSync(r, rec)
	else
		r = L.checksumRoundRestore(engine, C.hashValue, rec, { detail = (not rec.light) and {} or nil, clock = X.csClock, keyHash = C.fnv1a })
		r.mine = 0
		live[key] = r
	end
	for _, k in ipairs(X.CS_FIELDS) do r[k] = rec[k] end
	return r, key
end

-- The simulation half's spread rounds (`st.csRounds`, the game-script state, so that whichever simulation Lua state runs a step reads
-- the chunk due at it), each step before the step's items are applied: every round whose chunk is due reads it; a complete one is
-- stored as a report. `holder.csLive` (the SIM table) keeps this Lua state's own copies. A round of a session that ended is dropped.
-- Never raises: the step's actions are applied whatever happens here. Returns the number of the last round stored, or nil.
function X.csAdvance(holder, st, now)
	local stored = nil
	local keep, kept = {}, {}
	local live = holder.csLive or {}
	for _, rec in ipairs(st.csRounds or {}) do
		local key = nil
		local ok, done = pcall(function()
			if rec.epoch ~= st.epoch then
				C.logger("sim")("checksum round " .. tostring(rec.n) .. " dropped: its session ended before it was read")
				return nil
			end
			local r
			r, key = X.csLive(live, rec)
			local steps = r.steps
			local had = {}
			for i, ln in ipairs(r.lanes) do had[i] = ln.keys and #ln.keys or 0 end
			local done = X.csStep(r, now)
			if r.detailShared and r.steps ~= steps then X.csDetailChunk(r, rec, had) end
			if not done then
				if r.steps ~= steps then X.csSave(r, rec) end
				return false
			end
			local parts, t = X.csFinish(r)
			-- a detail round leaves its complete keys for the dump at a resync; a round without (the last one was identical) clears them
			st.csDetail = r.detailShared and X.csDetailDone(rec, t) or nil
			st.reports[#st.reports + 1] = { n = r.n, time = t, parts = parts, account = r.account, cost = L.checksumCost(r) }
			stored = r.n
			pcall(X.csLateNote, st, r, now)
			return true
		end)
		if not ok then
			C.logger("sim")("checksum round " .. tostring(rec.n) .. " failed and is dropped: " .. tostring(done))
		elseif done == false then
			keep[#keep + 1] = rec
			if key then kept[key] = true end
		end
	end
	-- the copies of rounds that are complete (here or in another Lua state) or dropped go
	for key in pairs(live) do if not kept[key] then live[key] = nil end end
	holder.csLive = live
	st.csRounds = #keep > 0 and keep or nil
	return stored
end

-- A spread round with a chunk read after its step reports its start plus that lateness (X.csFinish): the host compares it only with the
-- games that were late by exactly as much, and skips it as skewed otherwise. Since 2026-10-18 every regular round is spread, so rounds
-- that are late again and again are desync detection lost (the real-game bug above went unseen for a day: its only trace was a clause at
-- the end of a long line). Counted in the game-script state (every simulation Lua state finishes rounds): a WARNING line once
-- CS_LATE_WARN rounds in a row were late (and every 10 more), a line when they are read on time again, and every game minute with a
-- late round a line with the minute's counts.
X.CS_LATE_WARN = 3
function X.csLateNote(st, r, now)
	local log = C.logger("sim")
	local s = type(st.csLate) == "table" and st.csLate or { streak = 0, at = now, rounds = 0, late = 0 }
	s.rounds = s.rounds + 1
	if (r.late or 0) > 0 then
		s.late, s.streak = s.late + 1, s.streak + 1
		if s.streak == X.CS_LATE_WARN or (s.streak > X.CS_LATE_WARN and (s.streak - X.CS_LATE_WARN) % 10 == 0) then
			log(string.format("WARNING: %d checksum rounds in a row read chunks after their steps (round %s: %d chunk(s), +%s ms): each reports a moved time, so the host compares it only with games late by exactly as much; desync detection may be lost",
				s.streak, tostring(r.n), r.late, C.integerText(r.slip or 0)))
		end
	else
		if s.streak >= X.CS_LATE_WARN then log(string.format("checksum rounds read on time again after %d late round(s) in a row (round %s)", s.streak, tostring(r.n))) end
		s.streak = 0
	end
	if now - s.at >= 60000 then
		if s.late > 0 then
			log(string.format("checksum minute: %d of %d spread round(s) since t=%s read chunks after their steps (not comparable unless every game was late alike); %d in a row now",
				s.late, s.rounds, C.integerText(s.at), s.streak))
		end
		s.at, s.rounds, s.late = now, 0, 0
	end
	st.csLate = s
end

-- A finished round: its cost logged, its part-level breakdown traced, its itemised keys kept. Returns the parts and the time to report:
-- the round's start, plus the game time its late chunks were read after their steps (0 when every chunk was on time).
function X.csFinish(r)
	local clock = r.clock
	for name, err in pairs(r.errors) do C.logger(r.where)("checksum lane " .. name .. " failed: " .. tostring(err)) end
	local parts, t, detail = r.parts, r.start + (r.slip or 0), r.detail
	if r.spread > 0 then
		-- what the player feels is the costliest step: the start or the costliest chunk
		local worst, step = r.frameLanes, r.frameMax
		if r.startCost >= step then worst, step = { start = r.startCost }, r.startCost end
		checksumNote(worst, step, r.where, false)
		pcall(function()
			local names = {}
			for name in pairs(r.timing) do names[#names + 1] = name end
			table.sort(names, function(a, b) if r.timing[a] ~= r.timing[b] then return r.timing[a] > r.timing[b] end return a < b end)
			local lanes = {}
			for i = 1, math.min(6, #names) do lanes[i] = string.format("%s %.0f", names[i], r.timing[names[i]] * 1000) end
			local late = r.late > 0 and string.format("; %d chunk(s) read after their step (+%s ms game time): this round reports t=%s, compared only with games late by as much", r.late, C.integerText(r.slip), C.integerText(t)) or ""
			-- the cost per entity goes to the host with the report (core ChecksumPolicy::chunk_for sizes the next rounds' chunks from it)
			local cost = L.checksumCost(r)
			C.logger(r.where)(string.format("checksum round %s spread: %d entities in %d chunk(s) of %d, every %d step(s) from t=%s to t=%s; start %.1f ms, step max %.1f ms (budget %d ms, %d chunk(s) over), work %.0f ms, wall %.2f s, cost %s us an entity%s%s; lanes: %s",
				tostring(r.n), r.total, r.steps, r.chunk, r.spread, C.integerText(r.start), C.integerText(L.checksumChunkAt(r, r.steps)), r.startCost * 1000,
				r.frameMax * 1000, L.CHECKSUM_FRAME_BUDGET_MS, r.over, (r.startCost + (r.work or 0)) * 1000, clock() - r.wall0,
				cost and string.format("%.1f", cost.entityNs / 1000) or "?", r.detailShared and ", detail kept (complete, in the game-script state)" or "",
				late, table.concat(lanes, ", ")))
		end)
	else
		checksumNote(r.timing, clock() - r.wall0, r.where, r.light)
	end
	-- the part-level breakdown: every lane's hash, the item counts, and the itemised keys of the full round are kept for a dump when a resync starts
	pcall(function()
		local counts = {}
		for k, v in pairs(detail or {}) do counts[#counts + 1] = k .. "=" .. #v end
		table.sort(counts)
		local tr = (r.where == "sim") and traceSim or traceGui
		tr("checksum", string.format("t=%s light=%s lanes=%s items{%s} parts=%s spread=%s", tostring(t), tostring(r.light), tostring(next(parts) ~= nil), table.concat(counts, " "), C.serialize(parts), tostring(r.spread)))
		-- a spread round's keys are those of the chunks this Lua state read (the others' were read by another simulation state)
		local partial = (r.spread or 0) > 0 and (r.mine or 0) < r.steps and string.format("%d of %d chunks", r.mine or 0, r.steps) or nil
		if detail then DETAIL.last = { time = t, lanes = detail, partial = partial } end
	end)
	return parts, t
end

-- A round hashed whole, now (a light round, a paused one, a request without `spread`).
function checksumParts(co, where, light)
	local r = X.csBegin(co, where, { full = light ~= true })
	while not r.done do L.checksumRoundStep(r) end
	return X.csFinish(r)
end

-- ================================================================== simulation half

local logSim = C.logger("sim")
-- ---- one game, several simulation Lua states (docs/research/TWO_STATE_AUDIT_20261019.md, local)
--
-- The game runs this script's simulation half in more than one Lua state and shares one game's steps between them: two states install
-- the session's math.random at the same game time (friend test of 2026-10-18, 22:01:34 and every reload after it), a spread
-- checksum round kept in one state read its chunks at two steps in four while an action was applied at its stamp in between (see
-- X.csAdvance). Each event reaches one of them: both states log their first event "at handleEvent" (lab K 2026-10-19 00:17:33), and none
-- of the 136 actions the host's "queue" events brought in that friend test was applied twice. The docs name no such split: update "is
-- executed in parallel for every game script" (wiki modding:scripting:gamescripts), and the one thing every state of the script shares is
-- its GameScriptState: "Persistent state object of a game script, passed to update, postUpdate and handleEvent ... The state table is
-- saved with the game; change it by reading with get, modifying and writing back with set" (tf3-docs reference/base/scripts/gamescript).
-- Module tables are a Lua state's own ("it is not possible to directly share variables between them", the wiki on threads).
-- So, one rule for every value of this half: what a step or an event must see whichever state runs it lives in the game-script state
-- `st` (the queue, bindings, watches, binders, replay marks, held-command notes, checksum rounds, costs, the native module's
-- capabilities, the session it belongs to, the results of script builds); module values (SIM, SIMO, X) keep only what cannot go there or
-- is this state's own: functions (a script's build callback, the wrapped api.cmd), live copies of shared records (X.csLive), this state's
-- control writer, caches and log throttles. A state's own values are brought up to the shared record at each of its steps and events
-- (X.simCatchUp). Work on shared data is done once, by whichever state runs the step or gets the event; work on a state's own values
-- (its math.random, its callbacks) in every state, and never twice on the shared data (X.simSessionOff).
local SIM = { rng = nil, subscribed = false }

-- Live vs loaded log (feverlink3_livevsloaded.lua): off unless FEVERLINK3_LIVE_VS_LOADED=1. Observe-only, own guard.
local LVL = nil
local LVL_FAILS = 0
local function livevsloaded()
	if LVL ~= nil then return LVL or nil end
	LVL = false
	local ok, err = pcall(function()
		if C.env("FEVERLINK3_LIVE_VS_LOADED") ~= "1" then return end
		local okM, M = pcall(ug_require, "feverlink3_livevsloaded.lua")
		if not okM then okM, M = pcall(ug_require, "feverlink3::/feverlink3_livevsloaded.lua") end
		if not okM or type(M) ~= "table" then error("module not loaded: " .. tostring(M)) end
		local okT, TT = pcall(ug_require, "feverlink3_towntrace.lua")
		if not okT then okT, TT = pcall(ug_require, "feverlink3::/feverlink3_towntrace.lua") end
		local bits32 = okT and type(TT) == "table" and TT.bits32 or nil
		local t = M.new({
			enabled = true,
			towns = C.env("FEVERLINK3_TOWN_TRACE"),
			acc = M.apiAccessors(api, engine.entitiesWith),
			bits32 = bits32,
			write = function(text) return C.dump(M.FILE, text) end,
			-- L4: facts about math.random that cost no draw. nil until this script's override is installed.
			-- The game scripts' own Lua states are not reachable from here, so a draw cannot be compared there.
			randomProbe = function()
				if not SIM.rng then return nil end
				local facts = { installed = 1, rng_t = tostring(SIM.rng.t), rng_n = tostring(SIM.rng.n) }
				-- the math.random that a chunk compiled with the default environment sees (the VM's global table)
				local okL, seen = pcall(function() return load("return math.random")() end)
				facts.global_is_override = (okL and seen == math.random) and 1 or 0
				facts.package_math_is_ours = (package.loaded.math == nil or package.loaded.math == math) and 1 or 0
				return facts
			end,
		})
		if t.enabled then
			LVL = t
			logSim("live-vs-loaded log on -> " .. M.FILE)
		end
	end)
	if not ok then logSim("live-vs-loaded log failed to start: " .. tostring(err)) end
	return LVL or nil
end

local function livevsloadedGuard(fn)
	local t = livevsloaded()
	if not t then return end
	local ok, err = pcall(fn, t)
	if not ok then
		LVL_FAILS = LVL_FAILS + 1
		logSim("live-vs-loaded log error: " .. tostring(err))
		if LVL_FAILS >= 5 then
			LVL = false
			logSim("live-vs-loaded log switched off after 5 errors")
		end
	end
end

local function simState(state)
	local st = state:get()
	if type(st) ~= "table" then st = {} end
	st.queue = st.queue or {}
	st.results = st.results or {}
	st.reports = st.reports or {}
	st.refused = st.refused or {}
	st.bind = st.bind or {}
	st.watches = st.watches or {}
	st.binders = st.binders or {}
	if Co and st.co == nil then st.co = Co.new() end
	return st
end

local function trim(list, keep)
	while #list > keep do table.remove(list, 1) end
end

-- The context an action is decoded and applied in: the player entity its `player` arguments become (this game's own; in a
-- company session the acting player's company, see Co.actor), the sender, the stamp, the action key and the registry.
function actionCtx(it, co, resolve)
	local ctx = { player = engine.canonicalPlayer and engine.canonicalPlayer() or engine.player(), resolve = resolve, co = co }
	if it then ctx.from, ctx.at, ctx.key = it.from, it.at, it.key end
	if Co then pcall(Co.actor, ctx) end
	return ctx
end

local function simCtx(st, it)
	local ctx = actionCtx(it, st.co, function(key) return L.resolveKey(st.bind, key) end)
	-- the native module's capabilities, told by the interface half (event nativecaps, which reaches one simulation state): kept in the
	-- game-script state, so a stamp reached by the other state injects the originator's copy too (it decoded by the record path before)
	local caps = type(st.caps) == "table" and st.caps or {}
	ctx.canInject = NATIVE_INJECT and caps.inject == true
	ctx.canPatch = NATIVE_PATCH and caps.patch == true
	ctx.canTerrain = caps.terrain == true
	return ctx
end

-- companies. Guarded calls into the company module: a fault in that layer is logged and the action goes ahead, since
-- losing a player's action is worse than a missing ledger line.
function coBefore(ctx, it, spec, args, category, log)
	if not Co then return true end
	local ok, a, b = pcall(Co.beforeApply, ctx, it, spec, args, category)
	if not ok then
		log("company layer failed: " .. tostring(a))
		return true
	end
	return a, b
end

function coEffects(ctx, fx, log)
	if not (Co and fx) then return end
	if fx.note then log("company: " .. fx.note) end
	if fx.postings then
		local ok, done, failed = pcall(Co.runEffects, ctx, fx, engine)
		if not ok then
			log("company layer failed: " .. tostring(done))
			return
		end
		for _, what in ipairs(failed) do log("company: could not book a " .. what) end
	end
end

-- The balance an action's cost lands on (the acting company's account); nil when it cannot be read.
function coBalance(ctx)
	if not Co or not engine.balanceOf then return nil end
	local ok, v = pcall(engine.balanceOf, ctx.player)
	if ok and type(v) == "number" and v == v then return v end
	return nil
end

-- After the engine command went out: the ledger, with the cost measured as the account's difference. Returns the cost.
function coAfter(ctx, it, spec, category, balance0, log)
	if not Co then return nil end
	local cost = nil
	local balance1 = balance0 ~= nil and coBalance(ctx) or nil
	if balance0 ~= nil and balance1 ~= nil then cost = balance0 - balance1 end
	local ok, err = pcall(function() Co.afterApply(ctx, it, spec, Co.info(it, spec, category, cost)) end)
	if not ok then log("company layer failed: " .. tostring(err)) end
	return cost
end

-- The simulation state's sendCommand only queues the command: the engine runs it after this update (its apply event carries the same
-- game time), so the account read by coAfter right after the send has not moved and the ledger booked nothing (friend test 2026-10-16:
-- 82 builds, 15 vehicles bought, "spent 0" in every ledger; lab 2026-10-18: build.send balance=5035288423, build.result two steps
-- later money 5035288423 -> 5035252123). An action that costs money (build, bulldoze, spend) whose account did not move at once is
-- measured again at this half's next step (X.simBookCosts). Not for this game's own held command (the interface half releases it later)
-- nor an event (a game script handles it, Co.info). What else moved the account in that step is in the figure too; an action of a
-- step of its own is exact, several of one account in one step share the difference equally. A refund (less money spent than none)
-- is booked only for bulldozing; for the others it can only be income of the same step.
function X.simQueueCost(st, ctx, it, category, balance0, cost, now)
	if not Co or cost ~= 0 or balance0 == nil or category == "other" then return end
	if it.heldId ~= nil and it.from == C.PLAYER_ID then return end
	if Co.nameOf(it.body.fn, it.body.args) ~= it.body.fn then return end
	st.costs = st.costs or {}
	st.costs[#st.costs + 1] = { from = it.from, at = it.at, key = it.key, cat = category, b0 = balance0, step = now, p = ctx.player }
end

function X.simBookCosts(st, now)
	local list = st.costs
	if type(list) ~= "table" or #list == 0 then return end
	local keep, groups, order = {}, {}, {}
	for _, c in ipairs(list) do
		if (tonumber(c.step) or now) >= now then
			keep[#keep + 1] = c
		elseif now - c.step <= REPLAY_STEPS * STEP then   -- older (a reload, a long pause of this half): not this action's money any more
			local g = tostring(c.step) .. "/" .. tostring(c.p)
			if not groups[g] then groups[g] = {}; order[#order + 1] = g end
			table.insert(groups[g], c)
		end
	end
	st.costs = #keep > 0 and keep or nil
	for _, g in ipairs(order) do
		local cs = groups[g]
		local b1 = coBalance({ player = cs[1].p })
		if b1 ~= nil and type(cs[1].b0) == "number" then
			local total = math.floor(cs[1].b0 - b1 + 0.5)
			local share = (total >= 0) and math.floor(total / #cs) or -math.floor(-total / #cs)
			local keys = {}
			for i, c in ipairs(cs) do
				local part = share + ((i == 1) and (total - share * #cs) or 0)
				if part < 0 and c.cat ~= "bulldoze" then part = 0 end
				pcall(Co.bookCost, st.co, c.from, c.at, c.cat, part)
				keys[#keys + 1] = tostring(c.key) .. "=" .. tostring(part)
			end
			traceSim("company.cost", string.format("step t=%s measured at t=%s: account %s -> %s, booked %s", tostring(cs[1].step), tostring(now),
				tostring(cs[1].b0), tostring(b1), table.concat(keys, " ")))
		end
	end
end

-- The bulldoze refund made natively (owner decision 2026-10-18): a build whose inject order carries refund entries (the module puts them
-- into the command's own Context, refund.rs) has its account read before the send and again at this half's next step, after the engine
-- applied it, so that every game's mod.log shows what the removal cost there with the list in the command ("refund (native): ...").
-- MPFever measured no refund from a script Context's list; whether the native list refunds is what these lines prove (unproven so far).
-- The account: the acting company's (coBalance), else this game's (engine.account). Not booked anywhere, a measurement only.
function X.refundMoney(ctx)
	local v = ctx and coBalance(ctx) or nil
	if v == nil then pcall(function() v = engine.account() end) end
	return type(v) == "number" and v or nil
end

function X.watchRefund(st, key, n, money0, ctx, now)
	st.refundWatch = st.refundWatch or {}
	st.refundWatch[#st.refundWatch + 1] = { key = key, n = n, b0 = money0, step = now, ctx = ctx and { player = ctx.player } or nil }
end

function X.simRefundMoney(st, now)
	local list = st.refundWatch
	if type(list) ~= "table" or #list == 0 then return end
	local keep = {}
	for _, w in ipairs(list) do
		if (tonumber(w.step) or now) >= now then
			keep[#keep + 1] = w
		else
			local b1 = X.refundMoney(w.ctx)
			local spent = (type(w.b0) == "number" and type(b1) == "number") and string.format("%.0f", w.b0 - b1) or "?"
			logSim(string.format("refund (native): action %s: %d refund entries ordered into the command's Context (the module's line in native.log says how many it set); account before %s, after %s (spent %s, measured at t=%s; anything else of that step is in the figure too)",
				tostring(w.key), w.n, tostring(w.b0), tostring(b1), spent, tostring(now)))
		end
	end
	st.refundWatch = #keep > 0 and keep or nil
end

-- build.result (trace.log and mod.log): ONE line for every build this game applies at a stamp, whatever it is (street, track,
-- construction, stop, removal, bulldozing; this game's own and the others'; refused ones too), all by the same path. CLAUDE.md: full
-- build records in this testing phase; before 2026-10-19 only builds that key entities (constructions, stops, depots) had one.
-- X.buildResultBegin, at the apply (simApplyAction): the verdict this game reached there (refused with its class, sent, or applied
-- natively by its own tool), the account before, and, for a build that is sent, the world's entity lists of X.BRES_COMPS (one read per
-- step, shared by the step's builds). X.simBuildResults, at the next step (the engine runs a command the simulation sent after that
-- update: lab 2026-10-18, the account had not moved right after the send): the lists again, the difference (entities added and
-- removed), whether the engine announced the build (its replay mark was spent), the account after, and the line. Everything lives in
-- the game-script state: the next step may run in the other simulation Lua state (TWO_STATE_AUDIT_20261019). Builds of one step share
-- the step's difference and money (the line names them: shared=[keys]); a town's own build in that step is in the figures too.
-- Money: the acting company's account (api.engine.util.finance.getPlayersBalance, "Get balance of the player's bank account",
-- tf3-docs reference/api/api/engine/util) or this game's player's ACCOUNT component (Account.balance, tf3-docs Engine.Component), as
-- X.refundMoney reads it. The engine's own cost of the build: ProposalData.costs from the replay's verdict (engineVerdict; a native
-- Proposal has none); a negative one is the refund the engine reports, else a rise of the account is the refund measured.
X.BRES_COMPS = { "BASE_NODE", "BASE_EDGE", "CONSTRUCTION", "EDGE_OBJECT", "TOWN_BUILDING" }

-- The lists as text (cheap to keep in st). The engine does not loop over BASE_NODE / BASE_EDGE ("Cannot loop over this component type"):
-- the nodes and the segments come from the street graph, streetSystem.getNode2SegmentMap (as the build trace reads them).
function X.bresSnapshot()
	local s = {}
	local nodes, edges, seen = {}, {}, {}
	pcall(function()
		for n, segs in pairs(api.engine.system.streetSystem.getNode2SegmentMap()) do
			nodes[#nodes + 1] = n
			for i = 1, #segs do
				if not seen[segs[i]] then seen[segs[i]] = true; edges[#edges + 1] = segs[i] end
			end
		end
	end)
	table.sort(nodes)
	table.sort(edges)
	s.BASE_NODE, s.BASE_EDGE = table.concat(nodes, ","), table.concat(edges, ",")
	for _, comp in ipairs(X.BRES_COMPS) do
		if not s[comp] then s[comp] = table.concat(entitiesWith(comp), ",") end
	end
	return s
end

-- The facts of a build at its apply (both halves: the pause path applies from the interface half, guiApplyNow). `how`:
-- { verdict = "refused"|"sent"|"native", class, detail, held, costs }. Nil for an action that is no build.
function X.bresEntry(it, ctx, now, how)
	if not (it.body and it.body.fn == "makeWorldBuildProposalCmd") then return nil end
	local plan = "?"
	pcall(function() plan = tostring(Build.recSummary(it.body).plan) end)
	return { key = it.key, at = now, verdict = how.verdict, class = how.class, detail = how.detail and tostring(how.detail):sub(1, 400) or nil,
		held = how.held or nil, costs = how.costs, plan = plan, kind = it.body.category or buildCategory(it.body),
		role = (it.from == C.PLAYER_ID) and (it.scriptOut and "originator(script)" or "originator") or "replay",
		b0 = X.refundMoney(ctx), p = ctx and ctx.player or nil }
end

-- The line itself, the same for both halves. `changes`: X.bresDiff's text or nil (not measured); `announced`: yes/no/n/a/callback.
function X.bresWrite(trace, log, e, now, changes, announced, shared)
	local verdict = e.verdict
	if verdict == "sent" then
		-- unseen: nothing changed here and the engine announced nothing: refused by the engine, or emptied by the native module (build.verdict)
		verdict = ((changes ~= nil and changes ~= "") or announced == "yes" or announced == "callback") and "applied" or "unseen"
	elseif verdict == "refused" then
		verdict = "refused(" .. tostring(e.class) .. ")"
	end
	local b1 = X.refundMoney(e.p and { player = e.p } or nil)
	local spent, refund = "?", "none"
	if type(e.b0) == "number" and type(b1) == "number" then
		spent = string.format("%.0f", e.b0 - b1)
		if b1 > e.b0 then refund = string.format("%.0f(account)", b1 - e.b0) end
	end
	if type(e.costs) == "number" and e.costs < 0 then refund = string.format("%.0f(engine)", -e.costs) end
	local line = string.format("%s role=%s kind=%s plan=%s verdict=%s announced=%s entities[%s] money %s -> %s spent=%s engine_costs=%s refund=%s applied_t=%s measured_t=%s%s%s",
		tostring(e.key), e.role, tostring(e.kind), tostring(e.plan), verdict, tostring(announced or "n/a"),
		changes == nil and "n/a" or (changes == "" and "none" or changes), e.b0 and string.format("%.0f", e.b0) or "?", b1 and string.format("%.0f", b1) or "?",
		spent, e.costs and string.format("%.0f", e.costs) or "n/a", refund, tostring(e.at), tostring(now),
		(shared and #shared > 0) and (" shared=[" .. table.concat(shared, " ") .. "]") or "",
		e.detail and (" detail=" .. tostring(e.detail):gsub("%s+", " "):sub(1, 200)) or "")
	trace("build.result", line)
	log("build.result " .. line)
	return verdict
end

function X.buildResultBegin(st, it, ctx, now, how)
	local ok, err = pcall(function()
		local e = X.bresEntry(it, ctx, now, how)
		if not e then return end
		if how.verdict == "sent" then
			st.bresSnap = type(st.bresSnap) == "table" and st.bresSnap or {}
			local k = tostring(now)
			if not st.bresSnap[k] then st.bresSnap[k] = X.bresSnapshot() end
			e.snap = k
		end
		st.bres = type(st.bres) == "table" and st.bres or {}
		st.bres[#st.bres + 1] = e
	end)
	if not ok then logSim("build.result of " .. tostring(it.key) .. " cannot be prepared: " .. tostring(err)) end
end

-- A later fact of the build begun at `now` (an engine refusal at the send, the engine's costs). Returns whether the build's entry has
-- its step snapshot (so X.simBuildResults will know what it made).
function X.buildResultSet(st, key, now, fields)
	local snapped = false
	for _, e in ipairs(type(st.bres) == "table" and st.bres or {}) do
		if e.key == key and e.at == now then
			for k, v in pairs(fields) do e[k] = v end
			if fields.verdict == "refused" then e.snap = nil end
			snapped = snapped or e.snap ~= nil
		end
	end
	return snapped
end

-- The entities a build made, from its step's two snapshots: the ids of BASE_NODE, BASE_EDGE, CONSTRUCTION and EDGE_OBJECT that are new
-- (what a WorldBuildProposalCommandData's resultEntities names: the proposal's new entities; town buildings are the town's), sorted.
X.RESULT_COMPS = { "BASE_NODE", "BASE_EDGE", "CONSTRUCTION", "EDGE_OBJECT" }
function X.bresAdded(before, after)
	local out = {}
	for _, comp in ipairs(X.RESULT_COMPS) do
		local had = {}
		for id in (before[comp] or ""):gmatch("[^,]+") do had[id] = true end
		for id in (after[comp] or ""):gmatch("[^,]+") do
			if not had[id] and tonumber(id) then out[#out + 1] = tonumber(id) end
		end
	end
	table.sort(out)
	return out
end

-- The answer of this game's own script build (shipLocal: a build an interface-state script sent with a callback), given a step after
-- its apply, when the snapshots say what it made: the engine runs a command the simulation sends after that update, so the callback
-- comes when single-player's would, with the build's result entities. Lab M 2026-10-19 (X6A): the answer at the apply carried none and
-- no command data, so a callback that hands `cmdData.resultEntities` to api.gui.construction.updateRefundableEntities (as the game's
-- bridge_and_tunnel.tl:195-203 does) could not, the new street never entered the interface's refundable list, and its bulldozing
-- 7 s later refunded nothing on either game. Builds of one step share the difference (`shared`): each gets all of it, said in the log.
function X.answerBuild(st, e, snaps, after, changes, announced, shared)
	local ok, list = pcall(function()
		if not (e.snap and snaps[e.snap] and after) then return {} end
		return X.bresAdded(snaps[e.snap], after)
	end)
	if not ok then list = {} end
	local success = e.verdict ~= "refused" and ((changes ~= nil and changes ~= "") or announced == "yes" or (changes == nil and announced == "n/a"))
	st.results[#st.results + 1] = { key = e.key, at = e.at, success = success, entities = list }
	logSim(string.format("action %s: answered to its script with %d result entit%s%s%s", tostring(e.key), #list, #list == 1 and "y" or "ies",
		success and "" or " (nothing of it is seen here: failed)", (shared and #shared > 0) and (" (shared with " .. table.concat(shared, " ") .. ")") or ""))
end

-- "+BASE_EDGE[1:50012] -CONSTRUCTION[2:54 60]" (at most 12 ids a list), "" when nothing changed
function X.bresDiff(before, after)
	local parts = {}
	for _, comp in ipairs(X.BRES_COMPS) do
		local b, a = before[comp] or "", after[comp] or ""
		if a ~= b then
			local had, has = {}, {}
			for id in b:gmatch("[^,]+") do had[id] = true end
			local added, removed = {}, {}
			for id in a:gmatch("[^,]+") do
				has[id] = true
				if not had[id] then added[#added + 1] = id end
			end
			for id in b:gmatch("[^,]+") do if not has[id] then removed[#removed + 1] = id end end
			local function list(sign, l)
				if #l == 0 then return end
				local shown = {}
				for i = 1, math.min(#l, 12) do shown[i] = l[i] end
				parts[#parts + 1] = string.format("%s%s[%d:%s%s]", sign, comp, #l, table.concat(shown, " "), #l > 12 and " ..." or "")
			end
			list("+", added)
			list("-", removed)
		end
	end
	return table.concat(parts, " ")
end

function X.simBuildResults(st, now)
	local list = st.bres
	if type(list) ~= "table" or #list == 0 then return end
	local keep, done, diffs, groups = {}, {}, {}, {}
	for _, e in ipairs(list) do
		-- a held command (this game's own, released by the interface half after the stamp step) is measured a step later
		local due = (tonumber(e.at) or now) + (e.held and 2 or 1) * STEP <= now
		if due then
			done[#done + 1] = e
			groups[tostring(e.at)] = (groups[tostring(e.at)] or 0) + 1
		else
			keep[#keep + 1] = e
		end
	end
	st.bres = #keep > 0 and keep or nil
	local snaps = type(st.bresSnap) == "table" and st.bresSnap or {}
	local marks = {}
	for _, m in ipairs(type(st.replay) == "table" and type(st.replay.marks) == "table" and st.replay.marks or {}) do marks[tostring(m.k)] = true end
	local after = nil
	for _, e in ipairs(done) do
		local changes, announced = nil, "n/a"
		if e.snap and snaps[e.snap] then
			if diffs[e.snap] == nil then
				after = after or X.bresSnapshot()
				diffs[e.snap] = X.bresDiff(snaps[e.snap], after)
			end
			changes = diffs[e.snap]
			announced = (e.held and "n/a") or (marks[tostring(e.key)] and "no" or "yes")
		end
		local shared = {}
		if (groups[tostring(e.at)] or 0) > 1 then
			for _, x in ipairs(done) do if x ~= e and x.at == e.at then shared[#shared + 1] = tostring(x.key) end end
		end
		local okW, errW = pcall(X.bresWrite, traceSim, logSim, e, now, changes, announced, shared)
		if not okW then logSim("build.result of " .. tostring(e.key) .. " failed: " .. tostring(errW)) end
		-- what really happened here, on every game: a verdict and a refusal notice that wait for it (X.tellRefused). Every role, not
		-- only the originator: a receiver's module can close its order as no_command too, and its verdict must say what it applied.
		st.results[#st.results + 1] = { key = tostring(e.key) .. "#outcome", of = e.key, at = e.at, success = true, outcome = okW and errW or "?" }
		if e.answer then
			local okA, errA = pcall(X.answerBuild, st, e, snaps, after, changes, announced, shared)
			if not okA then
				st.results[#st.results + 1] = { key = e.key, at = e.at, success = true }
				logSim("action " .. tostring(e.key) .. ": its script's answer has no result entities: " .. tostring(errA))
			end
		end
	end
	-- the step snapshots no waiting build refers to any more
	local used = {}
	for _, e in ipairs(keep) do if e.snap then used[e.snap] = true end end
	for k in pairs(snaps) do if not used[k] then snaps[k] = nil end end
	st.bresSnap = next(snaps) and snaps or nil
end

-- Applies one due action in the simulation state. No callback here (finding from MPFever (MIT): the engine
-- rejects callbacks from inside the simulation update; postUpdate, where this runs now, is taken to be the same: unproven), so an entity the command creates is found afterwards by
-- comparing the entity lists (a watch), and the result reaches the originator through the state.
-- (The replay marks below follow the counter MPFever keeps for the same purpose in captureNativeBuild, MIT.)
-- The engine tells the game script about every build it applies (`apply_command` / `onPreBuildProposal`, just before
-- it runs; finding from MPFever (MIT)). A replay of an action (a build rebuilt from a shipped record) must not be
-- shipped again, so each one is counted here when it is sent and spent when its event arrives.
-- The engine's own verdict on a rebuilt build proposal, without applying it (what a tool's preview reads): critical flag, messages and
-- warnings of the proposal data's errorState. The second argument of makeProposalData must be a Context (a command whose context
-- was left out made `bad argument #2 to 'makeProposalData'`): a default one with this game's player is made when the command has none.
-- Returns `critical, messages, warnings` (lists of strings), the collisions and the proposal's `costs` ("Total cost of the proposal, in
-- game currency", ProposalData.costs, tf3-docs reference/api/api/type; nil when not readable), or nil and the reason. Never raises.
function engineVerdict(args)
	local ok, critical, msgs, warns, coll, costs = pcall(function()
		local ctx = args[2]
		if ctx == nil or ctx == false then
			ctx = api.type.Context.new()
			pcall(function() ctx.player = api.engine.util.getPlayer() end)
		end
		local pd = api.engine.util.proposal.makeProposalData(args[1], ctx)
		local es = pd and pd.errorState
		if es == nil then return nil end
		local function list(l)
			local out = {}
			if l ~= nil then for i = 1, math.min(#l, 6) do out[#out + 1] = tostring(l[i]) end end
			return out
		end
		-- what collides: the entities named in collisionInfo, as kind and place (the errorState of a refused track build is often only the
		-- generic "Construction Not Possible"; the collisions say what is in the way)
		local coll = {}
		pcall(function()
			local ci = pd.collisionInfo
			local l = ci and ci.collisionEntities
			if l == nil then return end
			for i = 1, math.min(#l, 4) do
				local x = l[i]
				local id = x
				pcall(function() id = x.entity or x[1] or x end)
				local d = tostring(id)
				pcall(function()
					local be = api.engine.getComponent(id, api.type.ComponentType.BASE_EDGE)
					if be then
						-- street or track: BaseEdge.roadType, a RoadType (STREET or TRACK; tf3-docs api/engine). The BASE_EDGE_TRACK component
						-- read here before 2026-10-18 is not in the API's ComponentType list.
						local track = be.roadType == api.type["enum"].RoadType.TRACK
						local function pos(n) local c = api.engine.getComponent(n, api.type.ComponentType.BASE_NODE); return c and string.format("%.0f,%.0f", c.position.x, c.position.y) or "?" end
						d = string.format("%s %s edge %s-%s", tostring(id), track and "TRACK" or "street", pos(be.node0), pos(be.node1))
						return
					end
					local co = api.engine.getComponent(id, api.type.ComponentType.CONSTRUCTION)
					if co then d = string.format("%s construction %s", tostring(id), tostring(co.fileName)) end
				end)
				coll[#coll + 1] = d
			end
		end)
		local costs = nil
		pcall(function() if type(pd.costs) == "number" then costs = pd.costs end end)
		return es.critical == true, list(es.messages), list(es.warnings), coll, costs
	end)
	if not ok then return nil, tostring(critical):sub(1, 160) end
	if critical == nil then return nil, "no errorState" end
	return critical, msgs, warns, coll, costs
end

-- One log line for EVERY build this game applies, before it is applied (the replay of another game's build, and the originator's own
-- build when it is held and released): the plan, the counts, what it adds and removes, how many removals resolved to an entity in THIS
-- world (and which), and the engine's own verdict on the proposal as this game rebuilt it (errorState), so that a build the engine
-- refuses has its reason in the log (the simulation state has no callback and a refused command raises nothing).
--   build <key> (replay|originator): plan=.. cons=.. rem=.. nodes=.. segs=.. rmSegs=.. objs=..; adds [file@x,y ..]; removes [file@x,y[key] ..];
--   resolved removals <n> of <m> [ids]; verdict critical=<bool> messages=[..] warnings=[..]; ie=<bool> pi=<bool>
-- Observation only, never raises. Returns what the failure line of the binder needs (the same facts).
local REPLAY_VERDICT = { failed = false }
local function describeBuild(it, args, role)
	local ok, text, info = pcall(function()
		local sum = Build and Build.recSummary and Build.recSummary(it.body)
		if not sum then return nil end
		local parts = { string.format("plan=%s cons=%d rem=%d nodes=%d segs=%d rmSegs=%d objs=%d", sum.plan, sum.cons, sum.rem, sum.nodes, sum.segs, sum.rmSegs, sum.objs) }
		if #sum.add > 0 then parts[#parts + 1] = "adds [" .. table.concat(sum.add, " ") .. "]" end
		if #sum.remove > 0 then parts[#parts + 1] = "removes [" .. table.concat(sum.remove, " ") .. "]" end
		local ids, n = {}, 0
		pcall(function()
			local l = args[1].constructionsToRemove
			n = #l
			for i = 1, math.min(n, 6) do ids[#ids + 1] = tostring(l[i]) end
		end)
		if sum.rem > 0 or n > 0 then
			-- (a build of several commands carries its removals in a later one: the first command's list says nothing about them; see the
			-- build.removal trace lines for each removal's resolution)
			local chained = type(args) == "table" and type(args.chain) == "table" and #args.chain > 0
			parts[#parts + 1] = string.format("resolved removals %d of %d [%s]%s", n, sum.rem, table.concat(ids, " "), (chained and n < sum.rem) and " (first command only; the others are in the chained command)" or "")
		end
		local verdict, costs = "n/a", nil
		-- (real session 2026-10-13: makeProposalData answered "SimpleProposal expected, got Proposal" for a removal, and the one failure
		-- switched the reader off for every build. It only fails for the native Proposal type, so only that type is skipped.)
		local nativeType = false
		pcall(function() nativeType = args[1].toAdd ~= nil end)
		if nativeType and REPLAY_VERDICT.nativeNoData then
			verdict = "n/a (a native Proposal: the engine reads proposal data of a SimpleProposal only)"
		elseif not REPLAY_VERDICT.failed then
			local critical, msgs, warns, coll, pdCosts = engineVerdict(args)
			costs = pdCosts
			if critical == nil and msgs and tostring(msgs):find("SimpleProposal expected", 1, true) then
				REPLAY_VERDICT.nativeNoData = true
				verdict = "n/a (a native Proposal: the engine reads proposal data of a SimpleProposal only)"
				logSim("the engine reads proposal data of a SimpleProposal only; builds that rebuild as a native Proposal (removal, upgrade, bulldoze, node configurations) show no replay verdict; the originator's line carries the tool's own verdict")
			elseif critical == nil and msgs then
				REPLAY_VERDICT.failed = true
				logSim("the engine's verdict on a rebuilt build cannot be read from this state (" .. tostring(msgs) .. "); the build lines say verdict n/a from now on")
			elseif critical ~= nil then
				verdict = string.format("critical=%s messages=[%s] warnings=[%s] collisions=[%s]", tostring(critical), table.concat(msgs, "; "), table.concat(warns, "; "), table.concat(coll or {}, "; "))
				if critical == false and #msgs > 0 and args[3] ~= true then
					verdict = verdict .. string.format("; WARNING flag mismatch: the engine reports non-critical messages and this build is applied with ignoreErrors = false, "
						.. "which refuses it silently (the record carries the originator's flag: %s)", tostring(it.body and it.body.args and it.body.args[3]))
				end
			end
		end
		parts[#parts + 1] = "verdict " .. verdict
		parts[#parts + 1] = string.format("ie=%s pi=%s", tostring(args[3] == true), tostring(args[4] == true))
		return table.concat(parts, "; "), { sum = sum, removed = ids, removedN = n, verdict = verdict, costs = costs }
	end)
	if ok and text then
		logSim(string.format("build %s (%s): %s", it.key, role, text))
		traceSim("build.line", string.format("%s role=%s %s", it.key, role, text))
		return info
	end
	return nil
end

-- Which components an entity of this world has (a fixed probe list), for the trace of what a build made: "id{CONSTRUCTION,NAME,...}"
-- (only types of the API's ComponentType list, tf3-docs api/engine: BASE_EDGE_STREET marks a street edge; there is no track counterpart)
local PROBE_COMPS = { "CONSTRUCTION", "STATION_GROUP", "STATION", "EDGE_OBJECT", "BASE_EDGE", "BASE_EDGE_STREET", "BASE_NODE", "TOWN_BUILDING",
	"VEHICLE_DEPOT", "NAME", "PLAYER_OWNED", "MODEL_INSTANCE_LIST", "SUBCONSTRUCTION", "LINE", "TRANSPORT_VEHICLE" }
local function entLine(e)
	local have = {}
	for _, name in ipairs(PROBE_COMPS) do
		local okc, c = pcall(function() return api.engine.getComponent(e, api.type.ComponentType[name]) end)
		if okc and c then have[#have + 1] = name end
	end
	return tostring(e) .. "{" .. table.concat(have, ",") .. "}"
end

-- How a CONSTRUCTION entity of this world looks in the log: file@x,y
local function conLine(e)
	local ok, t = pcall(function()
		local c = api.engine.getComponent(e, api.type.ComponentType.CONSTRUCTION)
		local m = c and c.transf
		return string.format("%s@%.1f,%.1f", tostring(c and c.fileName), m and m[13] or 0, m and m[14] or 0)
	end)
	return ok and t or tostring(e)
end

-- `noPlayer` (the command's playerInitiated flag is false: the second command of a construction next to a street) counts in its
-- own counter, spent only by an event whose playerInitiated is false too; a town's own build has that flag as well, so such a
-- mark can still be spent by the wrong event, but a real tool build (flag true) can never be swallowed by it.
local function markReplay(st, now, state, key, noPlayer)
	local r = st.replay
	if type(r) == "table" and (r.n > 0 or (r.nf or 0) > 0) and now - (r.at or 0) > REPLAY_STEPS * STEP then
		-- A mark that no engine event ever spent: the engine did not run the command that was sent (an apply the engine refuses
		-- raises no event and no error here, the simulation state has no callback). This is the first sign of a world that
		-- diverges from the originator's, so it is logged and noted for the actions that follow (it ships with them).
		-- The marks left name the builds (r.marks): the counter alone named the LAST build sent, which was not the one (owner one-PC test
		-- 2026-10-15: 2:18 got no apply event on either game, 2:19's event spent 2:18's mark, 2:20's event 2:19's, and "2:20 not run" was
		-- logged on both games although 2:20 was applied on both).
		local names = {}
		for _, m in ipairs(type(r.marks) == "table" and r.marks or {}) do names[#names + 1] = tostring(m.k or "?") .. "@" .. tostring(m.at) end
		local which = #names > 0 and table.concat(names, ", ") or tostring(r.key or "?")
		logSim("the engine did not run the replayed build " .. which .. " (last sent at t=" .. tostring(r.at) .. "): no apply event within " .. REPLAY_STEPS .. " steps (refused by the engine?)")
		st.unrun = (st.unrun or 0) + 1
		st.notes = st.notes or {}
		L.addNote(st.notes, now, "the engine did not run the replayed build " .. which)
		r = nil
	end
	if type(r) ~= "table" or now - (r.at or 0) > REPLAY_STEPS * STEP then r = { n = 0, nf = 0, at = now, marks = {} } end
	if key then r.key = key end
	if noPlayer then r.nf = (r.nf or 0) + 1 else r.n = r.n + 1 end
	r.marks = type(r.marks) == "table" and r.marks or {}
	r.marks[#r.marks + 1] = { k = key, at = now, f = noPlayer and true or nil }
	r.at = now
	st.replay = r
	-- The engine may announce the build while the command is still being sent (seen in a real game: the guest captured its
	-- own replay about half the time), before this update has stored its state. Store it now, so that the event finds the mark.
	if state then state:set(st) end
end

-- After the command went out, the event handler may have spent the mark and added to the outgoing list in the stored
-- state: take that over, or this update's own copy would put the mark back (and swallow the next real build).
local function takeBackEvents(st, state)
	if not state then return end
	local fresh = state:get()
	if type(fresh) ~= "table" then return end
	st.replay, st.out, st.outSeq = fresh.replay, fresh.out or st.out, fresh.outSeq or st.outSeq
end

local function spendReplay(st, now, noPlayer)
	local r = st.replay
	if type(r) ~= "table" or (r.n <= 0 and (r.nf or 0) <= 0) then return false end
	if now - (r.at or 0) > REPLAY_STEPS * STEP then
		st.replay = nil
		return false
	end
	if noPlayer then
		if (r.nf or 0) <= 0 then return false end
		r.nf = r.nf - 1
	else
		if r.n <= 0 then return false end
		r.n = r.n - 1
	end
	-- which mark: the engine runs a command the simulation sent at the step it was sent (every apply event in the logs carries the send's
	-- game time), so the mark of this step goes first; else the oldest of the same kind. A mark of a build the engine refused is then left
	-- for the report above instead of taking the next build's event.
	if type(r.marks) == "table" then
		local pick = nil
		for i, m in ipairs(r.marks) do
			if (m.f == true) == (noPlayer == true) then
				if m.at == now then pick = i break end
				pick = pick or i
			end
		end
		if pick then table.remove(r.marks, pick) end
	end
	return true
end

-- Strict mode, the originating game: a shipped build whose own copy the native module still holds. If the module gives that
-- command back BEFORE the stamp (its silence rule, its age limit, hold_off), the build lands in this world early and its engine
-- event comes with no replay mark; this list tells the capture that it is that build, not a new one, so it is not shipped a second
-- time. The interface half adds an entry when it ships (event "heldout") and removes it when it released the command itself or
-- cancelled it ("heldgone"); an entry lives 35 s of wall time at most (the module's own limit is 30 s) and never across a reload.
local HELD_WALL = 35
local function heldOutLive(e)
	local age = C.clock() - (tonumber(e.wall) or 0)
	return age >= 0 and age <= HELD_WALL
end

local function takeHeldOut(st)
	local list = st.heldOut
	if type(list) ~= "table" then return nil end
	local keep, taken = {}, nil
	for _, e in ipairs(list) do
		if not heldOutLive(e) then
			-- expired: dropped
		elseif not taken then
			taken = e
		else
			keep[#keep + 1] = e
		end
	end
	st.heldOut = keep
	return taken
end

function buildCategory(body)
	local rec = type(body) == "table" and type(body.args) == "table" and body.args[1] or nil
	if type(rec) ~= "table" then return "build" end
	-- the rule of the build module's categoryFor, counted on the record: a build that adds no construction, node or edge
	-- object and takes something away (a construction, edge objects, or more segments than it puts back) is bulldozing
	local adds = #(rec.cons or {}) + #(rec.nodes or {}) + #(rec.objs or {})
	local removes = #(rec.rem or {})
	if adds == 0 and (removes > 0 or #(rec.segs or {}) < #(rec.rmSegs or {}) or #(rec.rmObjs or {}) > 0) then return "bulldoze" end
	return "build"
end

-- The category an action is booked under (permissions, the company ledger): for a build record it is fixed once, by the game
-- that made the build, and travels inside the record, so that every game books it the same way. Counting it again on each
-- game from the rebuilt proposal made the ledgers differ (a bulldozing that leaves merged road pieces was "build" on one game
-- and "bulldoze" on the others, and the `companies` part of the checksum differed for good). Only the two build categories
-- are taken from a record, and only for the build command; anything else is counted the usual way.
function actionCategory(body, args)
	if type(body) == "table" and body.fn == "makeWorldBuildProposalCmd" and (body.category == "build" or body.category == "bulldoze") then
		return body.category
	end
	return L.categoryOf(body.fn, args)
end

-- What this game could not replicate or key, kept in the state (L.addNote) and shipped with the actions that follow, so a
-- refusal on another game can name its first cause (docs/ARCHITECTURE.md, references).
local function simNote(st, now, text)
	st.notes = st.notes or {}
	L.addNote(st.notes, now, text)
end

-- A build the player made with a native tool (roads, tracks, stations, depots, bulldozer...): the engine applies it
-- itself, here and now. It is captured as a build record and handed to the interface half (st.out), which stamps and
-- ships it like any other action; every OTHER game rebuilds it at the stamp.
-- `post` = the second look at a build whose first capture (onPreBuildProposal) found an edge object (a stop, a signal)
-- without its model: the engine names the model only when the build has run, so the capture is repeated from
-- onPostBuildProposal, where the object exists (the way MPFever reads the stops it ships, MIT). The replay marks were
-- spent by the first look, and the result list of the event is given to the capture.
-- Keys for what the originator's OWN native build made (a stop, a station, a depot read after the build ran): the build was applied by the
-- engine at the click and shipped by the capture, so the stamp-time binder of a replay never exists here. The entity lists are taken
-- BEFORE the build (the pre-build event), the interface half says under which key the record was shipped ("nativebind"), and a binder
-- like a replay's then keys the new stops and their station groups on this game too: a line or a vehicle bought here can then name
-- them for the other game (real session 2026-10-12: "stationGroup s:45494 does not exist here", "unresolved line reference 1:3").
local NATIVE_BIND_COMPS = { "CONSTRUCTION", "EDGE_OBJECT", "STATION_GROUP", "GROUP_STATION" }
local function snapshotComps(comps)
	local before = {}
	for _, comp in ipairs(comps) do before[comp] = engine.entitiesWith(comp) end
	return before
end
-- Where each existing entity the native text names is in this (the originator's) world, read before the build runs: every receiver checks
-- its own entity of that id against it before the injection (Build.checkInjectRefs). Never raises; a build without prints is still
-- checked by kind and by the text's node positions.
function X.natRefPrints(tracer, who, body, text)
	local ok, prints = pcall(function() return Build and Build.refPrints and Build.refPrints(text) end)
	if ok and type(prints) == "table" then
		body.refPrints = prints
		local n = 0
		for _ in pairs(prints) do n = n + 1 end
		tracer("build.inject", string.format("%s reference prints: %d existing entities located for the receivers' check", who, n))
	else
		tracer("build.inject", string.format("%s reference prints NOT made (%s)", who, tostring(prints)))
	end
end

-- B2.1, shadow: the record against the text the native module read from the held command, member by member (Portable.compare); one
-- trace line (build.portable) on whichever half has both. Never raises.
function shadowCompare(tracer, who, text, rec)
	pcall(function()
		local tree, why = Portable.parse(text)
		if not tree then
			tracer("build.portable", string.format("%s TEXT UNUSABLE (%s)", who, tostring(why)))
			return
		end
		local diffs = Portable.compare(tree, rec)
		tracer("build.portable", string.format("%s diffs=%d%s; text counts[%s]", who, #diffs, #diffs > 0 and (" [" .. table.concat(diffs, "; "):sub(1, 700) .. "]") or "",
			Portable.countsText(Portable.counts(tree))))
		if #diffs > 0 then tracer("build.portable.all", who .. " " .. table.concat(diffs, "; "):sub(1, 6000)) end
	end)
end

-- The control file of THIS load of the script (docs/NATIVE_LINK.md section 3): a file of its own in the half's family (native_ctl.sim
-- for the simulation half, native_ctl.gui for the interface half; X.HALF, set by the entry points, see data()), named by the writer
-- itself (C.newOwnWriter) and kept: native_ctl.<half>.w<id>.log. The game runs the simulation half in more than one Lua state (real
-- runs b3real, b3real2: two writers of one file), and two states appending to one file from two threads overwrite or split each other's
-- lines, so no file has two writers. The module lists the folder for the files and reads each by position; nothing is dropped for its
-- number (it used to drop every repeated number, and the builds with them). Nil when no file can be named: the order fails closed.
X.logCtl = C.logger("ctl")
function X.ctlWriter()
	if not X.ctl then
		local half = X.HALF or "gui"
		local w, why = C.newOwnWriter(C.path("native_ctl." .. half), C.integerText(C.PLAYER_ID or 0) .. "@" .. X.SRC)
		if not w then
			if X.ctlWhy ~= why then pcall(X.logCtl, "NO CONTROL FILE for the " .. half .. " half: " .. tostring(why)) end
			X.ctlWhy = why
			return nil
		end
		-- the inject keys of this writer: a 32-bit hash of its id, times 2^20, plus its own count (X.nextInjectKey)
		w.keyBase, w.orders = C.fnv1a(w.id) * 1048576, 0
		X.ctl = w
		pcall(X.logCtl, "control writer of the " .. half .. " half: " .. tostring(w.path:match("[^\\/]+$")) .. " made by " .. X.SRC .. " (" .. C.stateTag() .. ")")
	end
	return X.ctl
end

function X.ctlSend(kind, payload)
	local w = X.ctlWriter()
	if not w then return false, "no control file (" .. tostring(X.ctlWhy) .. ")", nil end
	local ok, why = C.writerSend(w, kind, payload)
	return ok, why, w
end

-- "written to native_ctl.sim.w<id>.log as its line 12" / "NOT WRITTEN (why)", for the trace lines of the orders
function X.ctlWritten(ok, err)
	local w = X.ctl
	if not ok then return "NOT WRITTEN (" .. tostring(err) .. ")" end
	return "written to " .. tostring(w and w.path:match("[^\\/]+$")) .. " as its line " .. tostring(w and C.integerText(w.lines))
end

-- The key of an inject order (its native_inject_<key>.txt and the module's answers name it): the order's own id, made by its writer,
-- never by its place in a file: fnv1a(writer id) * 2^20 + the writer's own count of orders (below 2^52, exact in a double; printed with
-- C.integerText, never %d, which stops at 2^31 in the game's Lua). Two writers share a key only if their ids hash alike (1 in 2^32;
-- two simulation states' own counts gave two orders one key and one text file, runs b3real/b3real2). A writer that has used its
-- 2^20 - 1 keys hands over to a new file.
function X.nextInjectKey()
	local w = X.ctlWriter()
	if w and w.orders >= 1048575 then
		X.ctl = nil
		w = X.ctlWriter()
	end
	if not w then return nil end
	w.orders = w.orders + 1
	return w.keyBase + w.orders
end
-- The control order that makes the native module set the converted proposal's frozenNodes at the Add of the command sent next (patch_next:
-- frozen = 0-based indices into the proposal's added nodes, adds = how many nodes it adds). Written right before the command, by this
-- state's control writer (X.ctlSend).
local function sendPatchOrder(patch, key)
	local ok, err = pcall(function()
		local sent, why = X.ctlSend("patch_next", { frozen = patch.frozen, adds = patch.adds, at = patch.at })
		if not sent then error(why, 0) end
	end)
	traceSim("build.patch", string.format("%s patch_next frozen=[%s] adds=%d %s", tostring(key), table.concat(patch.frozen, ","), patch.adds or -1,
		X.ctlWritten(ok, err)))
end

-- A held-command order from the simulation half (`complete_next {held}`: this replay's command is this game's stamp apply of deferred
-- command `held`; `fail {id}`: that command gets no stamp apply result), written right before the command like the inject order.
-- The build's timing text as this game's simulation half applies it at the stamp (see X.wallNow): the record's own stages plus this game's.
function X.timingText(it, now)
	local b = it.body or {}
	local function whole(v) return string.format("%.0f", tonumber(v) or 0) end
	return string.format("key=%s %s recv=%s@g%s stamp=%s step=%s", tostring(it.key), type(b.tm) == "string" and b.tm or "held=- click=0@g0",
		whole(it.tRecv), whole(it.gRecv), whole(it.at), whole(now))
end

function X.sendHeldOrder(kind, payload, key)
	local ok, err = pcall(function()
		local sent, why = X.ctlSend(kind, payload)
		if not sent then error(why, 0) end
	end)
	traceSim("build.inject", string.format("%s %s %s %s", tostring(key), kind, C.serialize(payload), X.ctlWritten(ok, err)))
	return ok
end

-- The end of the mod's own send of an order's command (`send_done {key=n}` for an inject_next, `{held=h}` for a complete_next), written
-- whether the send returned or threw. The module takes the order only for the command of that send (the one carrying the conversion the
-- send made: positive identity, owner decision B 2026-10-21) and refuses an order still unused at this line, so it never waits on to
-- take a later command.
function X.sendDone(open, key)
	if open then return X.sendHeldOrder("send_done", open, key) end
end

-- The inject order: the shipped text goes to native_inject_<n>.txt (closed before the order is written), then `inject_next {key=n}`, then the caller
-- sends the replay's command at once (nothing in between). The module answers `injected` / `inject_refused` in native_events.log.
-- `held`: this game's own held command of the same build (the originator, deferred completion): it takes this command's result, or fails
-- when the module refuses the order. `refuse`: the test switch's forced refusal (nil: none). Returns true when the order was written; the
-- caller sends the command only then (an order that did not reach the module would leave the placeholder to be applied as it is).
local function sendInjectOrder(inj, key, held, tm, refuse)
	-- the order's own key (X.nextInjectKey): unique to this writer, whichever half or Lua state writes it
	local n = X.nextInjectKey()
	if not n then
		traceSim("build.inject", string.format("%s inject_next NOT WRITTEN (no control file: %s)", tostring(key), tostring(X.ctlWhy)))
		return false, "no control file (" .. tostring(X.ctlWhy) .. ")"
	end
	-- the refundable entities this game resolved (the decode's inject path): native_refund_<n>.txt, one entry per line (entity, key integer,
	-- three revision integers, value integer), and the order says how many; the module inserts them into the command's own Context
	-- refundableEntities with the engine's insert (refund.rs) and refuses the order when it cannot (the build is applied nowhere here)
	if type(inj.terrain) == "table" then
		-- a shared terrain height edit (native terrain.rs): the cells go to native_terrain_in_<n>.txt, then `terrain_apply`; the module writes
		-- them into the carrier this game sends next and answers under the same key as an inject order (injected / inject_refused)
		local t = inj.terrain
		local okT, errT = pcall(function()
			if not C.writeFile(C.path("native_terrain_in_" .. C.integerText(n) .. ".txt"), inj.text) then error("cannot write the cells file", 0) end
			local sent, why = X.ctlSend("terrain_apply", { key = n, x0 = t.x0, y0 = t.y0, w = t.w, h = t.h })
			if not sent then error(why, 0) end
		end)
		traceSim("build.inject", string.format("%s terrain_apply key=%s %dx%d cells at %d,%d (%d bytes of text) %s", tostring(key), C.integerText(n), t.w, t.h, t.x0, t.y0,
			#inj.text, X.ctlWritten(okT, errT)))
		return okT, errT, n
	end
	local refund = type(inj.refund) == "table" and #inj.refund > 0 and inj.refund or nil
	local ok, err = pcall(function()
		if not C.writeFile(C.path("native_inject_" .. C.integerText(n) .. ".txt"), inj.text) then error("cannot write the text file", 0) end
		if refund then
			local rows = {}
			for i, x in ipairs(refund) do
				rows[i] = table.concat({ C.integerText(x.e), C.integerText(x.k), C.integerText(x.r[1]), C.integerText(x.r[2]), C.integerText(x.r[3]), C.integerText(x.n) }, "\t")
			end
			if not C.writeFile(C.path("native_refund_" .. C.integerText(n) .. ".txt"), table.concat(rows, "\n") .. "\n") then error("cannot write the refund file", 0) end
		end
		local sent, why = X.ctlSend("inject_next", { key = n, held = held, tm = tm, refuse = refuse, refund = refund and #refund or nil })
		if not sent then error(why, 0) end
	end)
	traceSim("build.inject", string.format("%s inject_next key=%s held=%s%s%s (%d bytes of text) %s", tostring(key), C.integerText(n), tostring(held), refuse and (" refuse=" .. refuse) or "",
		refund and (" refund=" .. #refund) or "", #inj.text, X.ctlWritten(ok, err)))
	return ok, err, n   -- n: the module answers the order under it (injected / inject_refused), which decides this game's verdict
end-- the interface half's apply during a pause writes the same orders (guiApplyNow)
X.sendInjectOrder, X.sendPatchOrder = sendInjectOrder, sendPatchOrder

-- A refusal that means "the exact copy of the build cannot be applied on this game" (the decode's reference check or placeholder, an inject
-- order that could not be written): nothing was applied here.
function X.isInjectRefusal(err)
	return type(err) == "string" and err:find("^inject_refused") ~= nil
end

-- A refusal at the stamp is this game's VERDICT of the action, decided from its own world: identical worlds refuse alike, so it asks
-- for no resync. The interface half sends every build's verdict (refused, or applied: X.simVerdict) and the host compares them; only
-- verdicts that differ between games are a fork and get the repair (docs/PROTOCOL.md "Action verdicts"). Lab run 2026-10-18 (A-stress):
-- a town replaced the industrial building a build removed during its lead; both games refused it at the stamp ("toRemove[0] names
-- construction 39513, which does not exist here") and each asked for a resync of two identical worlds.
function X.refusalClass(err, stage)
	if X.isInjectRefusal(err) then return "inject_refused" end
	return stage
end

-- A build applied at its stamp on this game: its verdict, for the interface half to send (X.verdictOf). `injects`: the keys of the
-- inject orders written for it; the verdict then waits for the module's answer to each (a refused order applies nothing).
function X.simVerdict(st, it, now, injects)
	if not (it.body and it.body.fn == "makeWorldBuildProposalCmd") then return end
	st.results[#st.results + 1] = { key = it.key .. "#verdict", of = it.key, from = it.from, seq = it.seq, at = now, success = true, verdict = true,
		injects = (injects and #injects > 0) and injects or nil, proof = it.proof }
end

-- The first few builds of each reason that could not be shipped leave the raw proposal in raw_builds.txt, to see the real shapes
-- (members of engine objects). A terrain tool's command emptied by the native module ("nothing to build") is not one of them: the owner's
-- run terrain-owner-20261025 spent all four dumps on those, and the tree brush's self-check failures were never dumped.
function X.rawDump(st, proposal, now, why)
	local reason = tostring(why):sub(1, 60)
	if reason == "nothing to build" then return end
	st.rawDumped = type(st.rawDumped) == "table" and st.rawDumped or {}
	st.rawDumped[reason] = (st.rawDumped[reason] or 0) + 1
	if st.rawDumped[reason] > 3 or not (Build.adapter and Build.adapter.dump) then return end
	pcall(function()
		local text = C.serialize(Build.adapter.dump(proposal))
		C.dump("raw_builds.txt", "==== t=" .. now .. " why: " .. tostring(why) .. "\n" .. text:sub(1, 200000) .. "\n")
	end)
end

local function simCaptureBuild(state, param, post, deferredBefore, deferredIe, deferredPre)
	local st = simState(state)
	local now = engine.gameTime()
	-- A build the engine or a game script made on its own (a town growing a road: playerInitiated false) is nobody's tool build:
	-- it must not use up a replay mark or the "early release" entry of a held command. It did in real run 2026-10-11: a town's
	-- build at t=13624000 was taken for the early release of the host's held build 0:6, which then came out of the module's 30 s
	-- timeout unrecognised and was shipped a second time as 0:7.
	local piEarly = nil
	pcall(function() piEarly = param[4] end)
	-- A replay that is playerInitiated (the flag of an event) spends only a mark counted for that flag: a town's own road (false) that
	-- the engine announces between the mark and the replay's event must not spend the mark of a flag-true replay, or the replay is
	-- taken for a new tool build and shipped back (the echo seen in lab runs N0-1 and N0-4: the other game refused the duplicate
	-- road). The second command of a construction beside a street has the flag false and is counted apart (markReplay).
	if not post and spendReplay(st, now, piEarly == false) then
		state:set(st)
		return
	end
	if not post and piEarly ~= false then
		local e = takeHeldOut(st)
		if e then
			state:set(st)
			logSim("a native build at t=" .. now .. " is the early release of the held command of " .. tostring(e.key) .. " (already shipped): not shipped again")
			return
		end
	end
	local proposal, playerInitiated, result = nil, nil, nil
	pcall(function() proposal, playerInitiated, result = param[1], param[4], param[3] end)
	if proposal == nil or playerInitiated == false or not Build then return end
	-- The flags the originator's engine applies the build with travel in the record: a replay with other flags can be refused where the
	-- originator's engine accepted (sweep 20261013-061744 B5: sent with ignoreErrors = true after "Empty Construction", replayed with false,
	-- the host built nothing and the games drifted). Event arguments: (proposal, context, ignoreErrors, playerInitiated); the post event
	-- carries the result entities at [3], so its flag is the one read before the build.
	local ignoreErrors = false
	if post then
		ignoreErrors = deferredIe == true
	else
		pcall(function() ignoreErrors = param[3] == true end)
	end
	local rev = {}
	for key, e in pairs(st.bind) do rev[e] = key end
	local t0 = C.clock()
	-- The Context is the event's second argument (a userdata with the flags the tool built the command with); the record carries it, so every
	-- game rebuilds the build with the originator's real flags. The post event passes the result at [3] and no Context at [2].
	local ctxObj = nil
	if not post then pcall(function() ctxObj = param[2] end) end
	if post and deferredPre then ctxObj = deferredPre.ctx end   -- read at the pre-build event: the post event carries no Context
	local ok, body, why = pcall(Build.capture, proposal, { playerInitiated = true, ignoreErrors = ignoreErrors, ctx = ctxObj, cons = post and deferredPre and deferredPre.cons or nil, result = post and result or nil }, {
		plain = C.plain, keyOf = function(e) return L.keyFor(rev, e) end,
	})
	if not ok then body, why = nil, tostring(body) end
	if not body and not post and tostring(why):find("has no model", 1, true) then
		SIM.deferredBuild = { at = now, before = select(2, pcall(snapshotComps, NATIVE_BIND_COMPS)), ie = ignoreErrors, pre = Build.preState(proposal, ctxObj) }
		logSim("a native build at t=" .. now .. " has an edge object that the engine has not made yet: read again after the build")
		state:set(st)
		return
	end
	if not body then
		logSim("a native build at t=" .. now .. " is not replicated: " .. tostring(why))
		simNote(st, now, "a native build at t=" .. now .. " was not replicated (" .. tostring(why) .. ")")
		-- the first few failures leave the raw proposal in a file, to see the real shapes (members of engine objects)
		X.rawDump(st, proposal, now, why)
		state:set(st)
		return
	end
	body.native = true
	do
		local hi = st.heldInput
		st.heldInput = nil
		if hi and Portable and C.clock() - hi.wall < 30 then
			body.input = hi.text
			shadowCompare(traceSim, "t=" .. now .. " side=originator", hi.text, body.args[1])
		end
		local hn = st.heldNative
		st.heldNative = nil
		if hn and C.clock() - hn.wall < 30 then
			body.nativeText = hn.text
			traceSim("build.inject", string.format("t=%d held=%s the native text of the released command goes with the record (%d bytes)", now, tostring(hn.id), #hn.text))
			X.natRefPrints(traceSim, "t=" .. now .. " held=" .. tostring(hn.id), body, hn.text)
		end
	end
	traceSim("build.ctx", string.format("t=%d side=originator source=%s ctx{%s} ie=%s", now, post and (deferredPre and "pre-build event (read before the deferred build)" or "post-event (no context)") or "pre-build event", Build.contextText(body.args[2]), tostring(body.args[3])))
	body.category = buildCategory(body)
	if Build.constructionsBlocked and Build.constructionsBlocked(body) then
		-- degraded mode (no native module): the build is in this world already and cannot be taken back; the others would refuse it
		logSim("a native build at t=" .. now .. " adds constructions: not shipped in this test build (" .. tostring(Build.CONSTRUCTIONS_BLOCKED_TEXT) .. "); this world will differ until a resync")
		simNote(st, now, "a build with constructions was not shipped (" .. tostring(Build.CONSTRUCTIONS_BLOCKED_TEXT) .. ")")
		state:set(st)
		return
	end
	local okS, text = pcall(C.serialize, body)
	if not okS then
		logSim("a native build at t=" .. now .. " is not replicated: " .. tostring(text))
		return
	end
	-- The self-check, BEFORE the record goes out: could this record be rebuilt here, the way another game will rebuild it? Only the
	-- engine's command factory is called, nothing is sent. Skipped after the build (`post`): the world has changed already, the segment it
	-- replaces is the new one, a check would mislead. The engine applies this build here whatever the check says (it was given back to the
	-- engine, or never held), so a failed check cannot refuse it; but its record is NOT shipped: every other game would refuse it the same
	-- way (friend test 2026-10-16, host street 0:2: "native build self-check FAILED: inject_refused ..." and the record went out anyway).
	-- The interface half asks the host for the repair instead (shipNative: `unbuildable`): this world has a build no other game can make.
	local unbuildable = nil
	if not post then
		local okC, args, whyC = pcall(L.decodeCommand, C.parse(text), simCtx(st, { from = C.PLAYER_ID, at = now, key = "self-check" }))
		if not okC then args, whyC = nil, tostring(args) end
		if args then
			logSim("native build self-check: can be rebuilt (" .. tostring(args.chain and (#args.chain + 1) or 1) .. " command(s))")
		else
			unbuildable = tostring(whyC)
			logSim("native build self-check FAILED: " .. unbuildable .. "; its record is NOT shipped (no other game could rebuild it either)")
			X.rawDump(st, proposal, now, "self-check: " .. unbuildable)
		end
	end
	st.out = st.out or {}
	st.outSeq = (st.outSeq or 0) + 1
	st.out[#st.out + 1] = { n = st.outSeq, at = now, body = (not unbuildable) and text or nil, unbuildable = unbuildable }
	trim(st.out, OUT_KEEP)
	if unbuildable then
		state:set(st)
		return
	end
	local okB, whyB = pcall(function()
		local spec = L.COMMANDS.makeWorldBuildProposalCmd
		local comps = spec and L.bindsComp(spec, body)
		if not comps then
			traceSim("native.snapshot", string.format("output %d: nothing to key (%s)", st.outSeq, spec and "no stop or construction in the record" or "no command schema"))
			return
		end
		local before = deferredBefore
		if not before and not post then before = snapshotComps(comps) end
		if type(before) ~= "table" then
			traceSim("native.snapshot", string.format("output %d: NO SNAPSHOT (deferred=%s post=%s before=%s)", st.outSeq, tostring(deferredBefore ~= nil), tostring(post), tostring(before)))
			return
		end
		SIM.nativeBinds = SIM.nativeBinds or {}
		SIM.nativeBinds[st.outSeq] = { body = body, comps = comps, before = before }
		SIM.nativeBinds[st.outSeq - 8] = nil
		-- also in the saved state, which the interface half's key message finds whatever happened to the table above (sweep run
		-- 20261013-081155: no snapshot was found for any native build of a real game, stops and constructions included)
		st.nbinds = st.nbinds or {}
		st.nbinds[tostring(st.outSeq)] = { comps = comps, before = before, text = text }
		st.nbinds[tostring(st.outSeq - 8)] = nil
		traceSim("native.snapshot", string.format("output %d: snapshot of [%s] stored", st.outSeq, table.concat(comps, " ")))
	end)
	-- (sweep run 20261013-074329: the originator's own native builds were never keyed in a real game and nothing said why)
	if not okB then logSim("native build " .. tostring(st.outSeq) .. ": its entities cannot be keyed here (" .. tostring(whyB) .. ")") end
	state:set(st)
	logSim("native build captured at t=" .. now .. " (" .. #text .. " bytes, " .. string.format("%.3f", C.clock() - t0) .. " s)")
end

-- A build command a script sends in this (simulation) state: the simulation state runs a command as it is sent, so the build used to
-- land on the sending game at once and on every other game at the stamp, in another step (real defaults run 2026-10-14: the sweep's
-- scripted probes left the free-id lists of the two engine copies different, and the next real depot named an id the host did not
-- have: "Collision with 52163", resync). Now a script's player build goes the way every tool build goes: it is NOT sent here; its
-- record is handed to the interface half (st.out, `script = true`), which stamps and ships it and queues it on this game too, so the
-- simulation half applies it at the stamp from the record exactly as every other game does (simApplyAction, role replay). The mod's
-- own sends (replays at the stamp) use SIMO, the unwrapped functions. Only player builds (playerInitiated = true, the rule of
-- simCaptureBuild): the game's own scripts share this state's api.cmd and send world builds that every game makes by itself
-- (terrain/reforestation.script, playerInitiated false: real run 2026-10-14), which stay native. Outside a session (C.ACTIVE false)
-- everything passes through. The game-script state cannot be read from inside another script's call (state:get raised "Unknown
-- exception" there), so the record waits in SIMO.pending until this script's own postUpdate hands it on (simFlushScriptBuilds).
local SIMO = { send = nil, factory = nil, tags = setmetatable({}, { __mode = "k" }), cbs = {}, pending = {} }

local function simScriptBuild(cmd, cb)
	local tag = SIMO.tags[cmd]
	if not (tag and C.ACTIVE and Build) or tag.args[4] ~= true then return false end
	local body, why = L.encodeCommand("makeWorldBuildProposalCmd", tag.args, { plain = C.plain, keyOf = function(e) return L.keyFor(SIMO.rev or {}, e) end })
	if not body then
		logSim("a script's build command is sent natively (it cannot be recorded: " .. tostring(why) .. ")")
		return false
	end
	body.category = buildCategory(body)
	local okS, text = pcall(C.serialize, body)
	if not okS then return false end
	local now = engine.gameTime()
	SIMO.pending[#SIMO.pending + 1] = { at = now, text = text, cb = cb }
	logSim("native build captured at t=" .. now .. " (" .. #text .. " bytes, a script's command: not sent here, every game applies it at the stamp)")
	return true
end

-- In this script's own postUpdate: the script builds waiting since the last one go to the interface half.
local function simFlushScriptBuilds(st)
	local rev = {}
	for key, e in pairs(st.bind) do rev[e] = key end
	SIMO.rev = rev
	if #SIMO.pending == 0 then return end
	st.out = st.out or {}
	for _, p in ipairs(SIMO.pending) do
		st.outSeq = (st.outSeq or 0) + 1
		-- `foreign`: another mod's command captured on the host (simForeign), shipped the same way
		st.out[#st.out + 1] = { n = st.outSeq, at = p.at, body = p.text, script = true, foreign = p.foreign }
		if p.cb then SIMO.cbs[st.outSeq] = p.cb end
	end
	SIMO.pending = {}
	trim(st.out, OUT_KEEP)
end

-- A script build applied at its stamp (simApplyAction): its result goes to the game-script state, where the state that took the build
-- and keeps its callback finds it (X.simCatchUp); the stamp may be reached by another simulation state.
function X.scriptBuildDone(st, n, ok)
	st.scriptDone = type(st.scriptDone) == "table" and st.scriptDone or {}
	st.scriptDone[#st.scriptDone + 1] = { n = n, ok = ok == true }
	trim(st.scriptDone, OUT_KEEP)   -- a result whose state is gone (a reload) is not kept for ever
end

-- This simulation state's own values brought up to the game-script state `st` (the rule at the top of this half): at every step and
-- every event of the state, before and after its work. Writes only what belongs to this state, plus the removal of the script results
-- it delivered; the caller stores st.
function X.simCatchUp(st)
	local caps = type(st.caps) == "table" and st.caps or nil
	if caps and type(caps.wall0) == "number" then X.WALL0 = caps.wall0 end   -- the timing lines' wall clock (X.wallNow)
	local done = st.scriptDone
	if type(done) == "table" and #done > 0 then
		local keep = {}
		for _, d in ipairs(done) do
			local cb = SIMO.cbs[d.n]
			if cb then
				SIMO.cbs[d.n] = nil
				pcall(cb, nil, d.ok == true)
			else
				keep[#keep + 1] = d   -- another state's, or one whose state is gone
			end
		end
		st.scriptDone = #keep > 0 and keep or nil
	end
end

-- Another mod's command from a game script (owner 2026-10-23, option A; before that WARN ONLY, owner 2026-10-21). Friend test 0.2.0
-- (ANALYSIS_NAMES_BUILD.md A): the Auto Line Namer renamed lines with its own sendCommand(makeEntitySetNameCmd) on each game at
-- wall-clock times; nothing of it was shared, the line names drifted and the host resynced. Auto Color Lines (makeEntitySetColorCmd) and
-- Auto Assign Terminals (makeLineUpdateCmd) sweep the lines the same way (their code, mod.io 6416672 / 6416135: throttled by the wall
-- clock, so each game decides at another step). Now, during a session, such a command of a kind the codecs know (L.COMMANDS, the
-- same encode a player's command takes) is: on the HOST captured and not sent here; its record goes the way of a script's build
-- (SIMO.pending -> st.out, `script = true` -> shipNative), stamped and applied on every game, the host's included, at the stamp. On every
-- OTHER game it is dropped (logged once per mod and kind): the host's copy of the mod decides for every game. A kind no codec can encode
-- (or a build, which has its own rules) goes to the engine as before, logged, and the player is told once per session and mod/command
-- kind (simFlushForeign -> X.foreignTell). The sender is the first Lua frame below the wrapped sendCommand (C.callerOf); FeverLink3's own
-- sends (journal bookings, the company sender) are recognised by their source and skipped. Only a source on a mod path counts: a source
-- that is not known (no debug library, or the game's own scripts, which every game runs alike) is only logged, once per source and kind.
SIMO.kinds = setmetatable({}, { __mode = "k" })   -- a command made in this state -> its factory's name
SIMO.cargs = setmetatable({}, { __mode = "k" })   -- a command made in this state -> its factory's arguments (simForeign encodes them)
SIMO.foreign, SIMO.foreignSeen, SIMO.foreignSent = {}, {}, {}

-- FeverLink3's own code in this Lua state (the bridge, the company sender): never another mod's (C.callerOf)
function X.ownSources()
	if X.OWN then return X.OWN end
	local own = {}
	pcall(function()
		own[debug.getinfo(1, "S").source] = true
		if Co and Co.extendEngine then own[debug.getinfo(Co.extendEngine, "S").source] = true end
	end)
	X.OWN = own
	return own
end

-- In the wrapped sendCommand during a session. Returns true when the command is taken (captured on the host, dropped elsewhere): it is
-- not sent. No game-script state access here (state:get raises inside another script's call): what the interface half must hear waits
-- in SIMO.pending / SIMO.foreign for this script's own postUpdate. `cb`: the mod's callback, kept with the record (X.simCatchUp).
local function simForeign(cmd, cb)
	local kind = SIMO.kinds[cmd] or (SIMO.tags[cmd] and "makeWorldBuildProposalCmd") or "an unknown command"
	local src, mod, own = C.callerOf(SIMO.wrapper, X.ownSources())
	if own then return false end   -- FeverLink3's own send
	local args, spec = SIMO.cargs[cmd], L.COMMANDS[kind]
	if mod and args and spec and not spec.virtual and kind ~= "makeWorldBuildProposalCmd" then
		if kind == "makeScriptingSendEventCmd" then L.markForeignEvent(args, mod, C.plain) end
		-- the same encode as a player's command; the keys are this state's (SIMO.rev, refreshed at every postUpdate), alike on every game
		local okE, body, why = pcall(L.encodeCommand, kind, args, { plain = C.plain, keyOf = function(e) return L.keyFor(SIMO.rev or {}, e) end })
		if not okE then body, why = nil, body end
		local okS, text = false, nil
		if body then okS, text = pcall(C.serialize, body) end
		if okS then
			local host = C.PLAYER_ID == 0
			local seen = "shared|" .. mod .. "|" .. kind
			if not SIMO.foreignSeen[seen] then
				SIMO.foreignSeen[seen] = true
				logSim(host and ("another mod's command is shared: kind=" .. kind .. " mod=" .. mod .. " (captured on the host, not sent here: every game applies it at the stamp; logged once)")
					or ("another mod's command is dropped here: kind=" .. kind .. " mod=" .. mod .. " (the host's copy of the mod sends it for every game; logged once)"))
			end
			if host then SIMO.pending[#SIMO.pending + 1] = { at = engine.gameTime(), text = text, cb = cb, foreign = mod } end
			return true
		end
		local seenW = "why|" .. mod .. "|" .. kind
		if not SIMO.foreignSeen[seenW] then
			SIMO.foreignSeen[seenW] = true
			logSim("another mod's command cannot be shared: kind=" .. kind .. " mod=" .. mod .. ": " .. tostring(why or text))
		end
	end
	local seen = tostring(src) .. "|" .. kind
	if mod or not SIMO.foreignSeen[seen] then
		logSim("another script's command goes to the engine unshared: kind=" .. kind .. " source=" .. tostring(src or "unknown") ..
			(mod and (" mod=" .. mod) or " (not a mod's path: the player is not told)"))
	end
	SIMO.foreignSeen[seen] = true
	if mod and #SIMO.foreign < 20 then SIMO.foreign[#SIMO.foreign + 1] = { mod = mod, kind = kind } end
	return false
end

-- In this script's own postUpdate: the mods seen since the last one go to the interface half (st.foreign), once per session and kind.
local function simFlushForeign(st)
	if #SIMO.foreign == 0 then return end
	for _, f in ipairs(SIMO.foreign) do
		local key = tostring(st.epoch) .. "|" .. f.mod .. "|" .. f.kind
		if not SIMO.foreignSent[key] then
			SIMO.foreignSent[key] = true
			st.foreign = type(st.foreign) == "table" and st.foreign or {}
			st.foreign[#st.foreign + 1] = { epoch = st.epoch, mod = f.mod, kind = f.kind }
			trim(st.foreign, 20)
		end
	end
	SIMO.foreign = {}
end

-- The command factories of this state learn the name and the arguments of what they make (SIMO.kinds, SIMO.cargs: another mod's
-- command is encoded from them, simForeign); the command itself is returned untouched. makeWorldBuildProposalCmd has its own
-- wrapper (SIMO.tags).
-- The names come from the codec's table (L.COMMANDS: only those can be shared anyway), as the interface half's wrapper does:
-- the real game's api.cmd lists nothing through pairs, so every other mod's command stayed "an unknown command" (two windows,
-- build 40423, 2026-10-23: Auto Color Lines and Auto Line Namer after new lines, both sent unshared on both games).
local function simTagKinds()
	local names = {}
	for name in pairs(L.COMMANDS) do
		if type(name) == "string" and name:find("^make") and name ~= "makeWorldBuildProposalCmd" then names[#names + 1] = name end
	end
	table.sort(names)
	local wrapped = 0
	for _, name in ipairs(names) do
		-- guarded as in installWrap: a build without this factory may refuse the lookup instead of answering nil
		local okf, f = pcall(function() return api.cmd[name] end)
		if okf and f ~= nil then
			local okw = pcall(function()
				api.cmd[name] = function(...)
					local cmd = f(...)
					if cmd ~= nil then
						SIMO.kinds[cmd] = name
						SIMO.cargs[cmd] = { n = select("#", ...), ... }
					end
					return cmd
				end
			end)
			if okw then wrapped = wrapped + 1 end
		end
	end
	logSim("other mods' commands: " .. wrapped .. " of " .. #names .. " command factories of this state learn what they make")
end

-- UNDOCUMENTED API (docs/ARCHITECTURE.md section 2, "Undocumented API uses and why"): sendCommand and makeWorldBuildProposalCmd are replaced in
-- this state's api.cmd; the API has no command hook, and a script's player build must be stamped like every other build.
local function simWrapCommands()
	if SIMO.send then return end
	local okI = pcall(function()
		SIMO.send, SIMO.factory = api.cmd.sendCommand, api.cmd.makeWorldBuildProposalCmd
		if not (SIMO.send and SIMO.factory) then error("no sendCommand / makeWorldBuildProposalCmd") end
		api.cmd.makeWorldBuildProposalCmd = function(...)
			local cmd = SIMO.factory(...)
			if cmd ~= nil then SIMO.tags[cmd] = { args = { n = select("#", ...), ... } } end
			return cmd
		end
		api.cmd.sendCommand = function(cmd, cb, ...)
			if cmd ~= nil and SIMO.tags[cmd] then
				local okB, taken = pcall(simScriptBuild, cmd, cb)
				if okB and taken then return end
				if not okB then logSim("a script's build command is sent natively (" .. tostring(taken) .. ")") end
			end
			if C.ACTIVE and cmd ~= nil then
				local okF, taken = pcall(simForeign, cmd, cb)
				if okF and taken then return end
			end
			return SIMO.send(cmd, cb, ...)
		end
		SIMO.wrapper = api.cmd.sendCommand
		pcall(simTagKinds)
	end)
	if not okI then
		SIMO.send, SIMO.factory = nil, nil
		logSim("script build commands stay native here: the command functions cannot be wrapped")
	end
end

-- D (owner, 2026-10-21), one rule for every path where the engine applies a command the mod sent: the command is made first (a factory
-- error is a clean refusal, nothing reached the engine), then handed over; `mark.applying` is true from the hand-over until the send
-- returns. An exception then is NOT "nothing applied": the apply is not transactional (friend test of 2026-10-20: stop 0:22 threw
-- IsValidTransition mid-apply and left a half-built edge), so the caller says the world may be PARTLY built and asks for the repair
-- (X.partlyBuilt), which never takes its world from such a game. Paths: the stamp apply (simSendCommand) and the pause apply from the
-- interface (guiApplyNow). `cb` is passed only when there is one (the simulation state refuses callbacks).
function X.handOver(send, cmd, cb, mark)
	if mark then mark.applying = true end
	if cb ~= nil then send(cmd, cb) else send(cmd) end
	if mark then mark.applying = false end
end

-- the mod's own sends of the simulation half: never through the wrapper above. The simulation state applies the command inside
-- sendCommand, so `mark` (X.handOver) says whether an exception came while the engine had it (simApplyAction)
local function simSendCommand(fn, a, mark)
	local factory = (fn == "makeWorldBuildProposalCmd" and SIMO.factory) or api.cmd[fn]
	local cmd = factory(unpack(a, 1, a.n))
	X.handOver(SIMO.send or api.cmd.sendCommand, cmd, nil, mark)
end

local function simApplyAction(st, it, now)
	if it.body and it.body.native and it.from == C.PLAYER_ID then
		-- the engine applied this build when its player made it; the record only exists to reach the other games
		st.results[#st.results + 1] = { key = it.key, at = now, success = true }
		X.simVerdict(st, it, now)
		X.buildResultBegin(st, it, simCtx(st, it), now, { verdict = "native", detail = "applied by this game's own tool when it was made; the record only reaches the other games" })
		return
	end
	if it.lateSteps and it.lateSteps > 0 and not it.waited then
		-- the input barrier makes this impossible by construction; if it still happens the entity ids have most likely forked (runs
		-- n1-n6, e1-e7: 9 of 10 first late applies did). Applied all the same (the other games have it), but never silently: the
		-- interface half asks the host for the repair (collectFromSim).
		logSim("LATE action " .. it.key .. " for t=" .. it.at .. " applied at " .. now .. " (" .. it.lateSteps .. " step(s) late): a FORK, the repair is requested")
		st.late = (st.late or 0) + 1
		st.lateLast = it.lateSteps
		st.lateKey = it.key
	end
	local ctx = simCtx(st, it)
	local tDecode = C.clock()
	if it.body and it.body.fn == "makeWorldBuildProposalCmd" then
		ctx.traceKey = it.key
		pcall(function()
			traceSim("build.record", string.format("%s role=%s from=%s stamp=%s now=%s late=%s balance=%s record=%s", it.key,
				(it.heldId and it.from == C.PLAYER_ID) and "originator" or "replay", tostring(it.from), tostring(it.at), tostring(now),
				tostring(it.lateSteps or 0), tostring(engine.account()), C.serialize(it.body)))
		end)
	end
	local args, err = L.decodeCommand(it.body, ctx)
	if it.body and it.body.fn == "makeWorldBuildProposalCmd" then
		traceSim("build.decode", string.format("%s ok=%s ms=%.1f%s", it.key, tostring(args ~= nil), (C.clock() - tDecode) * 1000, args and "" or (" error=" .. tostring(err))))
	end
	if not args then
		-- It names an entity another action created, which the watch has not found yet (the engine shows a new entity a step
		-- after the command): wait for it, a few steps at most, instead of refusing. Every game waits alike.
		local waitsFor = (it.waited or 0) < WATCH_STEPS + 2 and L.pendingRef(it.body, function(k) return L.keyIsPending(k, st.watches, st.binders) end) or nil
		if waitsFor then
			if not it.waited then logSim("action " .. it.key .. " waits for the entity " .. waitsFor .. " names") end
			-- counted in game steps, not calls: a call at a time that has not moved (a game standing still) is no step of waiting, or a
			-- game that stood still would give up where a running one applied it (MPFever issue #35, its watch)
			if it.waitAt ~= now then
				it.waited = (it.waited or 0) + 1
				it.waitAt = now
			end
			L.enqueue(st.queue, it)
			return
		end
		err = L.refusalDetail(err, it.body)
		logSim("action " .. it.key .. " refused: " .. tostring(err))
		traceSim("build.refused", it.key .. " " .. tostring(err))
		-- this game's own deferred command gets no stamp apply: it fails now (tool callback fired), not after the grace
		if it.deferId and it.from == C.PLAYER_ID then X.sendHeldOrder("fail", { id = it.deferId }, it.key) end
		-- refused at the stamp from this world: this game's verdict, no resync of its own (X.refusalClass)
		st.refused[#st.refused + 1] = { key = it.key, from = it.from, seq = it.seq, detail = err, class = X.refusalClass(err, "decode"),
			build = (it.body and it.body.fn == "makeWorldBuildProposalCmd") or nil }
		X.buildResultBegin(st, it, ctx, now, { verdict = "refused", class = X.refusalClass(err, "decode"), detail = err })
		if it.scriptOut then X.scriptBuildDone(st, it.scriptOut, false) end   -- a script's callback hears the refusal (X.simCatchUp)
		return
	end
	local spec = L.COMMANDS[it.body.fn]
	local category = actionCategory(it.body, args)
	-- companies: the spending limit and the spec's registry change come first; a mod-level action has no engine command
	local okC, fx = coBefore(ctx, it, spec, args, category, logSim)
	if not okC then
		logSim("action " .. it.key .. " refused: " .. tostring(fx))
		st.refused[#st.refused + 1] = { key = it.key, from = it.from, seq = it.seq, detail = fx, class = "company",
			build = (it.body and it.body.fn == "makeWorldBuildProposalCmd") or nil }
		X.buildResultBegin(st, it, ctx, now, { verdict = "refused", class = "company", detail = fx })
		if it.scriptOut then X.scriptBuildDone(st, it.scriptOut, false) end
		return
	end
	coEffects(ctx, fx, logSim)
	if spec.virtual then
		logSim("action " .. it.key .. " " .. it.body.fn .. " applied at t=" .. now)
		st.results[#st.results + 1] = { key = it.key, at = now, success = true }
		return
	end
	local before = spec.creates and engine.entitiesWith(spec.creates) or nil
	local bindComps = L.bindsComp(spec, it.body)
	local bindBefore = nil
	if bindComps then
		bindBefore = {}
		for _, comp in ipairs(bindComps) do bindBefore[comp] = engine.entitiesWith(comp) end
	end
	local balance0 = coBalance(ctx)
	-- Strict mode, the originating game: the native module still holds the tool's own command (it.heldId, set by natDecide, never
	-- on the wire). Nothing is sent here; everything else is as on every other game (the ledger, the snapshot of the entity
	-- lists, the binders that key what the build creates). The interface half releases the held command when it sees the
	-- "#release" result below, after this snapshot (so the new entities are not in it).
	local held = it.heldId ~= nil and it.from == C.PLAYER_ID
	local sending = {}   -- simSendCommand: applying = the engine threw (if it threw) while it applied the command
	-- build.result: the account and the world's entity lists before this build is sent (completed at the next step)
	X.buildResultBegin(st, it, ctx, now, { verdict = "sent", held = held })
	local ok, e = true, nil
	local preInfo = nil
	local injects = {}   -- the inject orders written for this build: the module's answers decide this game's verdict (X.simVerdict)
	if held and it.body.fn == "makeWorldBuildProposalCmd" then preInfo = describeBuild(it, args, "originator") end
	if not held then
		ok, e = pcall(function()
			L.sendNow(args, function(a)
				local isBuild = it.body.fn == "makeWorldBuildProposalCmd"
				if isBuild then
					markReplay(st, now, SIM.state, it.key, a[4] == false)
					if isBuild and not preInfo then preInfo = describeBuild(it, a, "replay") end
					if isBuild then
						pcall(function()
							local c = a[2]
							traceSim("build.send", string.format("%s ie=%s pi=%s dust=%s ctx{align=%s cleanup=%s buildings=%s fields=%s redo=%s player=%s} chain=%s now=%s balance=%s",
								it.key, tostring(a[3]), tostring(a[4]), tostring(a[5]), tostring(c and c.checkTerrainAlignment), tostring(c and c.cleanupStreetGraph),
								tostring(c and c.gatherBuildings), tostring(c and c.gatherFields), tostring(c and c.extendProposalRedoPillars), tostring(c and c.player),
								tostring(a.chain and (#a.chain + 1) or 1), tostring(engine.gameTime()), tostring(engine.account())))
							if Portable and type(it.body.input) == "string" then
								shadowCompare(traceSim, it.key .. " side=replay", it.body.input, Build.normalise(it.body.args[1]))
								pcall(function()
									traceSim("build.portable", string.format("%s side=replay this game's first proposal counts[%s]", it.key, Build.proposalCounts(a[1])))
								end)
							end
							traceSim("build.ctx", string.format("%s side=replay source=%s ctx{%s} ie=%s pi=%s", it.key, tostring(a.ctxSource or "record"), Build.contextText(c), tostring(a[3]), tostring(a[4])))
						end)
					end
				end
				if a.patch then sendPatchOrder(a.patch, it.key) end
				local refuse = nil
				if a.inject and it.deferId and X.DEBUG_REFUSE_INJECT then
					refuse = "FEVERLINK3_DEBUG_REFUSE_INJECT=" .. X.DEBUG_REFUSE_INJECT
					logSim("action " .. it.key .. ": " .. refuse .. ": the module is told to refuse this game's own replay (a forced refusal: nothing is applied here)")
				end
				local tm = isBuild and X.timingText(it, now) or nil
				local open = nil   -- the order of this send, closed by send_done once the send returned or threw (X.sendDone)
				if a.inject then
					local okO, errO, nO = sendInjectOrder(a.inject, it.key, it.deferId, tm, refuse)
					if okO and nO then
						injects[#injects + 1] = nO
						open = { key = nO }
					end
					-- no order, no command: the placeholder alone is this game's guess of the build, never the build (the pcall below refuses it)
					if not okO then error("inject_refused: the inject order could not be written (" .. tostring(errO) .. "); nothing is applied here", 0) end
				elseif it.deferId then
					-- not injected because the decode took the record path (this simulation half was not told of the module's inject capability;
					-- a refused injection never gets here any more, it refuses the build): this replay's command is still this game's stamp apply
					-- of the build, so the held command takes its result (run rt1 C12: without this the bulldozer stayed busy until the 30 s age limit)
					logSim("action " .. it.key .. ": held " .. tostring(it.deferId) .. " is deferred and this replay is not injected: it completes with this replay's result")
					if X.sendHeldOrder("complete_next", { held = it.deferId, tm = tm }, it.key) then open = { held = it.deferId } end
				elseif isBuild then
					-- no order: the record's rebuild is applied by the engine without the module's help; the line ends at the send
					traceSim("build.timing", string.format("timing %s rebuild=- how=record (no order) apply=%.0f accepted=? total_ms=? late_steps=%s", tm, X.wallNow(), tostring((now - it.at) / STEP)))
				end
				-- a build whose order carries refund entries: the account before, measured again at the next step (X.simRefundMoney)
				local refundN = a.inject and type(a.inject.refund) == "table" and #a.inject.refund or 0
				local okM, money0 = pcall(function() return refundN > 0 and X.refundMoney(ctx) or nil end)
				local okS, errS = true, nil
				if okM then okS, errS = pcall(simSendCommand, it.body.fn, a, sending) else okS, errS = false, money0 end
				X.sendDone(open, it.key)
				if not okS then error(errS, 0) end
				if a.inject and a.inject.terrain then it.proof = X.terrainProof(a.inject.terrain, it.key, logSim) end
				if refundN > 0 then X.watchRefund(st, it.key, refundN, money0, ctx, now) end
				if isBuild then takeBackEvents(st, SIM.state) end
			end)
		end)
	end
	if not ok then
		if it.deferId then
			-- no stamp apply on this game: the deferred held command fails now (tool callback fired), it does not wait for the age limit
			X.sendHeldOrder("fail", { id = it.deferId }, it.key)
		end
		-- D (owner, 2026-10-21): an exception while the engine applied the command is not "nothing applied": the apply is not
		-- transactional (friend test of 2026-10-20: stop 0:22 threw IsValidTransition mid-apply, left a half-built edge, and the game crashed a
		-- second later). Said as such, to the player too, and the repair is asked for at once (collectFromSim), not left to the checksums.
		local partial = sending.applying == true
		if partial then
			logSim("action " .. it.key .. ": the engine threw DURING its apply (" .. tostring(e) .. "): this game's world may be PARTLY built; the repair is asked for now")
		end
		logSim("action " .. it.key .. " refused by the engine: " .. tostring(e))
		traceSim("build.error", string.format("%s type=%s value=%s during_apply=%s", it.key, type(e), tostring(e), tostring(partial)))
		st.refused[#st.refused + 1] = { key = it.key, from = it.from, seq = it.seq, detail = tostring(e), class = X.refusalClass(e, "engine"),
			build = (it.body and it.body.fn == "makeWorldBuildProposalCmd") or nil, partial = partial or nil }
		X.buildResultSet(st, it.key, now, { verdict = "refused", class = X.refusalClass(e, "engine"), detail = tostring(e) })
		if it.scriptOut then X.scriptBuildDone(st, it.scriptOut, false) end
		return
	end
	-- What appeared during this action's own send: the simulation state runs a command as it is sent (a build's entities are keyed at
	-- the step of its apply, "steps=1"), so this is exactly what this command made, whatever ids the engine handed out; several
	-- commands of one step never take each other's entities (L.claimOwn, L.sentOwners). Empty for an engine that shows them later
	-- (the watch then falls back to the order of the commands) and for this game's own held build (released by the interface half).
	local sentW, sentB = nil, nil
	if not held then
		if spec.creates and before then sentW = L.newEntities(before, engine.entitiesWith(spec.creates)) end
		if bindComps then
			sentB = {}
			for _, comp in ipairs(bindComps) do sentB[comp] = L.newEntities(bindBefore[comp] or {}, engine.entitiesWith(comp)) end
		end
	end
	X.simVerdict(st, it, now, injects)
	if preInfo and preInfo.costs ~= nil then X.buildResultSet(st, it.key, now, { costs = preInfo.costs }) end
	X.simQueueCost(st, ctx, it, category, balance0, coAfter(ctx, it, spec, category, balance0, logSim), now)
	logSim("action " .. it.key .. " " .. it.body.fn .. (held and (" due at t=" .. now .. ": the native module holds this game's own copy (held " .. tostring(it.heldId) .. "), the interface half releases it") or (" applied at t=" .. now)))
	-- a window command (shipUiCommand): its timing line on every game, the originator's and the others' (click, read, received, stamp, apply)
	if type(it.body.ui) == "table" then
		pcall(function() logSim(string.format("timing window command %s apply=%.0f late_steps=%s", X.timingText(it, now), X.wallNow(), tostring((now - it.at) / STEP))) end)
	end
	if bindComps then
		st.binders[#st.binders + 1] = { key = it.key, fn = it.body.fn, body = it.body, comps = bindComps, before = bindBefore, steps = 0, pre = preInfo, money0 = balance0,
			sent = sentB, at = now }
	end
	if spec.creates then
		st.watches[#st.watches + 1] = { key = it.key, comp = spec.creates, result = spec.result, before = before, steps = 0, at = now, sent = sentW }
	elseif not held and it.from == C.PLAYER_ID and not it.scriptOut and it.body.fn == "makeWorldBuildProposalCmd"
		and X.buildResultSet(st, it.key, now, { answer = true }) then
		-- this game's own script build: answered a step later with what it made (X.answerBuild)
	else
		st.results[#st.results + 1] = { key = it.key, at = now, success = true }
	end
	if held then
		st.results[#st.results + 1] = { key = it.key .. "#release" .. tostring(it.heldId), of = it.key, at = now, success = true, release = it.heldId }
	end
	-- a script's build: its callback is kept by the simulation state that took the build, which may not be this one (X.simCatchUp)
	if it.scriptOut then X.scriptBuildDone(st, it.scriptOut, true) end
end

-- companies: an action the interface half applied during a pause ran its registry and ledger part on the interface
-- mirror (guiApplyNow); this is the same part on the simulation state, which the savegame keeps. The journal entries of
-- a payment were booked by the interface half already, so there are no effects here. `param.cost` is what it measured.
local function simReplayCompany(st, param)
	local it = param.it
	local spec = it.body and L.COMMANDS[it.body.fn]
	if not spec then return end
	local ctx = simCtx(st, it)
	local args, category
	if type(param.category) == "string" and type(it.body.args) == "table" then
		-- an action this game made itself and applied natively: the shipped arguments name it, nothing is rebuilt
		args, category = it.body.args, param.category
	else
		args = L.decodeCommand(it.body, ctx)
		if not args then return end
		category = actionCategory(it.body, args)
	end
	local okC = coBefore(ctx, it, spec, args, category, logSim)
	if okC and param.ok and not spec.virtual then
		local ok, err = pcall(function() Co.afterApply(ctx, it, spec, Co.info(it, spec, category, param.cost)) end)
		if not ok then logSim("company layer failed: " .. tostring(err)) end
	end
end

-- A key bound to an entity on this game: st.bindAt keeps the game time of the binding, so that a later watch can tell a binding
-- of its own time (that entity is taken) from an old one whose entity is gone and whose id the engine handed out again.
function X.simBind(st, key, e, now)
	st.bind[key] = e
	st.bindAt = st.bindAt or {}
	st.bindAt[key] = now
end

-- One step of a watch or binder: counted only when the game time moved since its last count (a call while the game stands still
-- is no step: MPFever issue #35, "the watch counts its steps only when the game time has moved").
function X.watchStep(w, now)
	if w.seenAt ~= now then
		w.steps = w.steps + 1
		w.seenAt = now
	end
end

local function simAdvanceWatches(st, now)
	if #st.watches == 0 then return end
	-- An entity another command made at its own send, or one bound to a key since this watch's command ran, is not this watch's:
	-- five vehicles bought in one step appear together. A binding older than the command names an entity that was gone when the
	-- command ran (it is not in the snapshot), so the id was handed out again and the binding does not hold the new entity.
	local sentBy = L.sentOwners(st.watches, st.binders)
	local owners = L.bindOwners(st.bind, st.bindAt)
	local keep = {}
	for _, w in ipairs(st.watches) do
		X.watchStep(w, now)
		local function taken(e)
			if sentBy[e] ~= nil and sentBy[e] ~= w.key then return true end
			return L.isTakenAt(owners[e], w.at)
		end
		local after = engine.entitiesWith(w.comp)
		local created = L.claimOwn(w.sent, after, taken) or L.claimEntity(w.before, after, taken)
		if created then
			owners[created] = { key = w.key, at = now }
			X.simBind(st, w.key, created, now)
			st.results[#st.results + 1] = { key = w.key, at = w.at, success = true, data = { [w.result] = created }, entities = { { created } } }   -- (the revision is added where the callback runs: C.resultEntities)
			logSim("action " .. w.key .. " created entity " .. created .. " (" .. w.comp .. ")")
		elseif w.steps >= WATCH_STEPS then
			st.results[#st.results + 1] = { key = w.key, at = w.at, success = false, detail = "no " .. w.comp .. " appeared" }
			logSim("action " .. w.key .. ": nothing created after " .. w.steps .. " steps")
			simNote(st, now, "action " .. w.key .. " created nothing that was found")
		else
			keep[#keep + 1] = w
		end
	end
	st.watches = keep
end

-- Keys for several entities an action created (the constructions of a build, their depots and stations, the stops it
-- put on a road and their station groups): looked for on each step until the schema says it has found them all, or
-- WATCH_STEPS have passed. What another action's keys already name is not offered again, so two builds in one step do
-- not take each other's entities (the engine runs the commands in the order they were applied, on every game).
local function simAdvanceBinders(st, now)
	if #st.binders == 0 then return end
	-- (as the watches: what another pending command made at its own send is not this one's, and a binding older than this command
	-- does not hold a new entity)
	local sentBy = L.sentOwners(st.watches, st.binders)
	local owners = L.bindOwners(st.bind, st.bindAt)
	local keep = {}
	for _, b in ipairs(st.binders) do
		X.watchStep(b, now)
		local mine = b.key .. "#"
		local made, seen = {}, {}
		for _, comp in ipairs(b.comps) do
			for _, e in ipairs(L.newEntities(b.before[comp] or {}, engine.entitiesWith(comp))) do
				local o = owners[e]
				local other = L.isTakenAt(o, b.at) and o.key or nil
				local byOther = sentBy[e] ~= nil and sentBy[e] ~= b.key
				if not seen[e] and not byOther and (other == nil or other:sub(1, #mine) == mine) then
					seen[e] = true
					made[#made + 1] = e
				end
			end
		end
		table.sort(made)
		local map, complete = L.bindMap(L.COMMANDS[b.fn], b.body, b.key, made)
		for key, e in pairs(map) do
			X.simBind(st, key, e, now)
			owners[e] = { key = key, at = now }
		end
		if complete then
			logSim("action " .. b.key .. " created " .. #made .. " " .. table.concat(b.comps, "/") .. " entities, keyed")
			pcall(function()
				if b.fn == "makeWorldBuildProposalCmd" and Build and Build.readPieces then
					for _, line in ipairs(Build.readPieces(b.body)) do traceSim("build.pieces", b.key .. " " .. line) end
				end
			end)
			pcall(function()
				local ents = {}
				for i = 1, math.min(#made, 16) do ents[#ents + 1] = entLine(made[i]) end
				local keys = {}
				for k, e in pairs(map) do keys[#keys + 1] = k .. "=" .. tostring(e) end
				table.sort(keys)
				traceSim("build.bind", string.format("%s ok=true steps=%d made=%d [%s] keys[%s] money %s -> %s", b.key, b.steps, #made, table.concat(ents, " "),
					table.concat(keys, " "), tostring(b.money0), tostring(engine.account())))
			end)
		elseif b.steps >= WATCH_STEPS then
			-- what is missing and why: the counts the record promised against what this world shows now
			local ok2, why = pcall(function()
				local pre = b.pre
				local sum = pre and pre.sum or (Build and Build.recSummary and Build.recSummary(b.body)) or {}
				-- the counts straight from the record, so the line never depends on the summary reader being there (a missing one read "expected 0")
				pcall(function()
					local rec = b.body.args[1]
					local function n(l) return type(l) == "table" and #l or 0 end
					local stops = 0
					for _, o in ipairs(type(rec.objs) == "table" and rec.objs or {}) do
						if type(o) == "table" and o.kind ~= 2 then stops = stops + 1 end
					end
					sum = { cons = n(rec.cons), objs = n(rec.objs), stops = stops, segs = n(rec.segs), rmSegs = n(rec.rmSegs), nodes = n(rec.nodes), add = sum.add }
				end)
				local found = {}
				local counts = {}
				for _, comp in ipairs(b.comps) do
					local n = 0
					for _, e in ipairs(L.newEntities(b.before[comp] or {}, engine.entitiesWith(comp))) do
						n = n + 1
						if comp == "CONSTRUCTION" and #found < 6 then found[#found + 1] = conLine(e) end
					end
					counts[#counts + 1] = comp .. " " .. n
				end
				local keyed, mine = 0, b.key .. "#o"
				for k in pairs(st.bind) do
					if k:sub(1, #mine) == mine and not k:find("/", 1, true) then keyed = keyed + 1 end
				end
				local still, total = 0, 0
				for _, id in ipairs(pre and pre.removed or {}) do
					total = total + 1
					local idn = tonumber(id)
					-- getComponent wants "a valid, existing entity" (tf3-docs api/engine): a removed building is asked whether it exists first
					if idn and pcall(function() assert(api.engine.entityExists(idn) and api.engine.getComponent(idn, api.type.ComponentType.CONSTRUCTION)) end) then still = still + 1 end
				end
				return string.format("expected: %d construction(s) %s, %d road-side stop(s) (EDGE_OBJECT + STATION, keyed #o<n>), %d edge object(s) in all, %d segment(s) added / %d removed, %d node(s) added; "
					.. "found new by component: [%s]; constructions [%s]; stops keyed %d of %d; removed buildings still present: %d of %d checked; the engine's verdict before applying: %s",
					sum.cons or 0, "[" .. table.concat(sum.add or {}, " ") .. "]", sum.stops or 0, sum.objs or 0, sum.segs or 0, sum.rmSegs or 0, sum.nodes or 0,
					#counts > 0 and table.concat(counts, ", ") or "nothing was watched", table.concat(found, " "), keyed, sum.stops or 0, still, total,
					pre and pre.verdict or "n/a (see the build line)")
			end)
			pcall(function()
				local ents = {}
				for _, comp in ipairs(b.comps) do
					for _, e in ipairs(L.newEntities(b.before[comp] or {}, engine.entitiesWith(comp))) do
						if #ents < 16 then ents[#ents + 1] = comp .. ":" .. entLine(e) end
					end
				end
				traceSim("build.bind", string.format("%s ok=false steps=%d new entities [%s] money %s -> %s record=%s", b.key, b.steps, table.concat(ents, " "),
					tostring(b.money0), tostring(engine.account()), C.serialize(b.body)))
			end)
			pcall(function()
				if b.fn == "makeWorldBuildProposalCmd" and Build and Build.readPieces then
					for _, line in ipairs(Build.readPieces(b.body)) do traceSim("build.pieces", b.key .. " " .. line) end
				end
			end)
			logSim("action " .. b.key .. ": RESULT DIFFERS from the record after " .. b.steps .. " steps: " .. (ok2 and why or ("(" .. tostring(why) .. ")")) .. " [not every entity it created was found; a build the engine refuses creates nothing]")
			simNote(st, now, "action " .. b.key .. " did not find every entity it created")
		else
			keep[#keep + 1] = b
		end
	end
	st.binders = keep
end

-- Build trace (FEVERLINK3_BUILD_TRACE=1, observation only): every 30 game seconds one block in build_trace.txt of the road graph:
-- counts and hashes of the BASE_NODE and BASE_EDGE entities in id order and in position order, the pairs() order of the
-- node-to-segment map, and the ids and positions of the nodes/edges that appeared since the last block. Two games that ended
-- up with the same road graph by different paths show it as equal position hashes with different id or order hashes.
local BT = { on = nil, last = -1, seenN = {}, seenE = {}, fails = 0 }
local function buildTraceStep(tick)
	if BT.on == nil then
		local ok, v = pcall(function() return C.env("FEVERLINK3_BUILD_TRACE") == "1" end)
		BT.on = ok and v or false
	end
	if not BT.on or BT.fails >= 5 or not C.DIR then return end
	if tick % 50 ~= 0 or tick == BT.last then return end
	BT.last = tick
	local ok, err = pcall(function()
		local function comp(e, name)
			local o, c = pcall(api.engine.getComponent, e, api.type.ComponentType[name])
			return o and c or nil
		end
		local function r1(v) return string.format("%.1f", v or 0) end
		local lines = {}
		local nodes, ids, pos = {}, {}, {}
		local order = {}
		local okm, map = pcall(function() return api.engine.system.streetSystem.getNode2SegmentMap() end)
		map = okm and map or {}
		local segSeen, edgeIds = {}, {}
		for n, segs in pairs(map) do
			order[#order + 1] = tostring(n) .. "/" .. tostring(#segs)
			local c = comp(n, "BASE_NODE")
			local p = c and c.position and (r1(c.position.x) .. "," .. r1(c.position.y) .. "," .. r1(c.position.z)) or "?"
			nodes[n] = p
			ids[#ids + 1] = n
			pos[#pos + 1] = p
			for i = 1, #segs do
				if not segSeen[segs[i]] then segSeen[segs[i]] = true; edgeIds[#edgeIds + 1] = segs[i] end
			end
		end
		table.sort(ids)
		table.sort(pos)
		local idKeys = {}
		for i = 1, #ids do idKeys[i] = ids[i] .. "@" .. nodes[ids[i]] end
		local edgePos, edgeKeys = {}, {}
		table.sort(edgeIds)
		for i = 1, #edgeIds do
			local e = edgeIds[i]
			local c = comp(e, "BASE_EDGE")
			if c then
				local a, b = nodes[c.node0] or tostring(c.node0), nodes[c.node1] or tostring(c.node1)
				if a > b then a, b = b, a end
				edgePos[#edgePos + 1] = a .. "-" .. b .. "/" .. tostring(c.type) .. "/" .. tostring(c.typeIndex)
				edgeKeys[e] = tostring(c.node0) .. ">" .. tostring(c.node1) .. "@" .. a .. "-" .. b
			end
		end
		table.sort(edgePos)
		local eIdKeys = {}
		for i = 1, #edgeIds do eIdKeys[i] = edgeIds[i] .. ":" .. tostring(edgeKeys[edgeIds[i]]) end
		local cons = entitiesWith("CONSTRUCTION")
		table.sort(cons)
		local bal = 0
		pcall(function() bal = engine.account() end)
		local maxVeh, nVeh = 0, 0
		for _, e in ipairs(entitiesWith("TRANSPORT_VEHICLE")) do nVeh = nVeh + 1; if e > maxVeh then maxVeh = e end end
		lines[#lines + 1] = string.format("t=%d veh=%d maxVeh=%d maxCons=%s maxEdge=%s balance=%s nodes=%d idsHash=%s posHash=%s maxId=%s edges=%d eIdsHash=%s ePosHash=%s mapOrder=%s mapN=%d cons=%d consHash=%s",
			tick, nVeh, maxVeh, tostring(cons[#cons]), tostring(edgeIds[#edgeIds]), tostring(bal), #ids, C.hashValue(idKeys), C.hashValue(pos), tostring(ids[#ids]), #edgeIds, C.hashValue(eIdKeys), C.hashValue(edgePos),
			C.hashValue(order), #order, #cons, C.hashValue(cons))
		local nNew = 0
		for i = 1, #ids do
			if not BT.seenN[ids[i]] then
				BT.seenN[ids[i]] = true
				if BT.last > 0 and BT.started and nNew < 40 then nNew = nNew + 1; lines[#lines + 1] = "  newnode " .. ids[i] .. "@" .. nodes[ids[i]] end
			end
		end
		for i = 1, #edgeIds do
			if not BT.seenE[edgeIds[i]] then
				BT.seenE[edgeIds[i]] = true
				if BT.started and nNew < 80 then nNew = nNew + 1; lines[#lines + 1] = "  newedge " .. edgeIds[i] .. ":" .. tostring(edgeKeys[edgeIds[i]]) end
			end
		end
		BT.started = true
		C.dump("build_trace.txt", table.concat(lines, "\n") .. "\n")
	end)
	if not ok then
		BT.fails = BT.fails + 1
		if BT.fails == 1 then logSim("build trace error: " .. tostring(err)) end
	end
end

-- ---- the end of a session in the simulation half
--
-- identity.txt says the session is over (`idle`) or names another one (common.lua C.poll), or the interface half says so (event
-- `sessionoff`, sent first in its own transition, X.sessionOff): what this half keeps for the ended session goes, so nothing of it is
-- applied later. The queue of stamped actions is dropped: no peer applies them at their stamps any more, and a stamp of an ended session
-- means nothing to the next one (it would be due at once). This game's own builds in it are not lost silently: a build whose command
-- the native module holds goes back to the engine when the interface half switches holding off (the module fails a deferred one), and
-- the callbacks of commands that were stamped and not applied are failed there. What belongs to the world stays: the bindings
-- (st.bind) and the company registry. Script builds this half took for stamping are failed (their callbacks run with a failure).
-- Every simulation Lua state sees the end on its own (identity.txt on its own clock, or the `sessionoff` event, which reaches one of
-- them), so the game-script state is cleared only by the first: it is cleared when it still belongs to the session that ends (st.sid,
-- X.simJoin), never when another state has cleared it or a newer session has taken it (the late state emptied the next session's queue
-- and epoch: test_session_end TwoSimulationStatesSessionTest). This state's own values (its script builds and callbacks, math.random,
-- live round copies) go in every state.
function X.sessionKey()
	return tostring(C.SESSION) .. "@" .. C.posText(C.SESSION_START)
end

-- This state joins the session C.SESSION names: the game-script state is stamped with it (the session its queue and outputs belong to).
function X.simJoin(state)
	SIM.gen, SIM.sid = C.GEN, X.sessionKey()
	pcall(function()
		local st = state:get()
		if type(st) == "table" and st.sid ~= SIM.sid then
			st.sid = SIM.sid
			state:set(st)
		end
	end)
end

function X.simSessionOff(state, why)
	local ending = SIM.sid or X.sessionKey()
	SIM.gen, SIM.sid = nil, nil
	local n = { queue = 0, out = 0, binders = 0, scripts = #SIMO.pending }
	local shared = false
	pcall(function()
		local st = simState(state)
		if not (st.sid == ending or (st.sid == nil and st.offSid ~= ending)) then return end
		shared = true
		n.queue, n.out, n.binders = #st.queue, #(st.out or {}), #st.binders
		st.queue, st.out, st.binders, st.watches = {}, nil, {}, {}
		st.heldOut, st.heldInput, st.heldNative, st.nbinds = nil, nil, nil, nil
		st.results, st.reports, st.refused, st.notes, st.late = {}, {}, {}, nil, 0
		st.costs, st.refundWatch, st.scriptDone = nil, nil, nil
		st.bres, st.bresSnap = nil, nil   -- build.result lines not yet written (X.simBuildResults)
		st.csRounds, st.csLate, st.csDetail = nil, nil, nil   -- the spread checksum rounds of the session and a detail round's keys (X.csAdvance)
		st.caps = nil    -- the native module's capabilities: the interface half of the next session tells them again
		st.epoch = nil   -- the next interface run stamps it again
		st.sid, st.offSid = nil, ending
		state:set(st)
	end)
	for _, p in ipairs(SIMO.pending) do
		if p.cb then pcall(p.cb, nil, false) end
	end
	for _, cb in pairs(SIMO.cbs) do pcall(cb, nil, false) end
	SIMO.pending, SIMO.cbs = {}, {}
	SIM.deferredBuild, SIM.nativeBinds, SIM.csLive = nil, nil, nil
	-- the session's deterministic math.random (L.installRandom) gives way to the game's own; the next session installs a new one
	if SIM.rng then
		if SIM.random0 then math.random = SIM.random0 end
		SIM.rng = nil
	end
	if shared then
		logSim(string.format("session over (%s): single-player again; dropped %d queued action(s), %d native build(s) not yet shipped, %d binder(s), %d script build(s) (failed)",
			tostring(why), n.queue, n.out, n.binders, n.scripts))
	else
		logSim(string.format("session over (%s): single-player again; the game-script state was cleared by another simulation state already (or belongs to a newer session): left as it is; %d script build(s) of this state failed",
			tostring(why), n.scripts))
	end
	traceSim("session", string.format("off why=%s shared=%s queue=%d out=%d binders=%d scripts=%d", tostring(why), tostring(shared), n.queue, n.out, n.binders, n.scripts))
end

-- Every update and every event of the interface half: a session this half started in that is over (or replaced by another) ends here.
function X.simSessionCheck(state)
	if SIM.gen ~= nil and (not C.ACTIVE or SIM.gen ~= C.GEN) then
		X.simSessionOff(state, C.ACTIVE and "another session began" or "the session ended")
	end
	if C.ACTIVE and SIM.gen == nil then X.simJoin(state) end
end

-- The simulation step, in two calls (official wiki, modding:scripting:gamescripts): "updateScript ... As it is executed in parallel for
-- every game script, it may not write the state itself. Its result struct is forwarded to the next script." and "postUpdateScript is
-- a callback that is executed after all the update scripts are done." The typed API pairs them the same way (script-doc
-- content/scripts/gamescript.html): `update(..., GameScriptState<T>, number): any` and `postUpdate(..., GameScriptState<T>, number,
-- any)`, the last argument being update's result. So update only reads the step's game time and hands it on; everything that writes
-- the game-script state or sends an engine command (the stamped actions, the checksum rounds, the hand-offs to the interface half) runs
-- in postUpdate of the same step, at the time update read. An action stamped for step T is applied at T as before, in the same order;
-- only other game scripts' updates (the base game's, every game alike) now run before it in that step instead of next to it.
-- The fallback (coordinator's decision 2026-10-18: never lose applies): an update that finds the step before still pending (no
-- postUpdate took it) runs that step itself, at that step's time, and then runs its own step too, until a postUpdate shows up again.
-- The stamps follow update's time either way, so the order of the applies is the same on every game; the caught-up step itself lands one
-- engine step late in world time. Logged once per session, and counted in a line every game minute (`postUpdate missing`).
function update(userParams, state, dt)
	local at = nil
	pcall(function() at = engine.gameTime() end)
	if SIM.upAt ~= nil then
		-- the step before is still pending: no postUpdate took it
		local prev = SIM.upAt
		SIM.upAt = nil
		SIM.noPost = true   -- until a postUpdate is seen again
		X.fallbackStep(state, prev, prev)
	end
	if SIM.noPost and at ~= nil then
		X.fallbackStep(state, at, SIM.noPostSince)
		return { at = at, done = true }
	end
	SIM.upAt = at
	return { at = at }
end

-- One step applied in update (the fallback above): logged once per session, counted in a line every game minute while in use.
function X.fallbackStep(state, at, missedAt)
	SIM.noPostSince = SIM.noPostSince or missedAt
	if SIM.noPostGen ~= C.GEN then
		SIM.noPostGen, SIM.noPostN, SIM.noPostAt = C.GEN, 0, nil
		logSim("postUpdate was not called after the update at t=" .. tostring(missedAt) .. ": the step is applied in update instead (wiki rule not followed by this game)")
	end
	SIM.noPostN = SIM.noPostN + 1
	local ok, err = pcall(X.simStep, state, at)
	if not ok then logSim("update fallback failed: " .. tostring(err)) end
	if SIM.noPostAt == nil or at - SIM.noPostAt >= 60000 then
		SIM.noPostAt = at
		logSim("postUpdate missing: " .. C.integerText(SIM.noPostN) .. " step(s) of this session applied in update instead (wiki rule not followed by this game)")
	end
end

function X.postUpdate(userParams, state, dt, res)
	SIM.noPost, SIM.noPostSince = false, nil
	-- no step pending: this step was applied in update (the fallback, or `done` in its result), never twice
	if SIM.upAt == nil or (type(res) == "table" and res.done) then return end
	local upAt = SIM.upAt
	SIM.upAt = nil
	local at = type(res) == "table" and res.at or nil
	if at == nil then
		-- no result forwarded: the time update read in this Lua state (the same step)
		at = upAt
		if not SIM.noResult then
			SIM.noResult = true
			logSim("postUpdate received no result from update (" .. type(res) .. "); the step's time is taken from this state's update")
		end
	end
	local nowEngine = nil
	pcall(function() nowEngine = engine.gameTime() end)
	if nowEngine ~= at and (SIM.timeSkew or 0) < 5 then
		-- the wiki says postUpdate follows the step's updates; this says whether the game time moved in between (unproven either way)
		SIM.timeSkew = (SIM.timeSkew or 0) + 1
		logSim("postUpdate runs at t=" .. tostring(nowEngine) .. " after an update at t=" .. tostring(at) .. ": the stamps follow the update's time")
	end
	X.simStep(state, at)
end

-- One simulation step's work (postUpdate): `now` is the step's game time as update read it.
function X.simStep(state, now)
	C.poll()
	X.simSessionCheck(state)
	pcall(function() buildTraceStep(math.floor(now / STEP)) end)
	townTraceGuard(function(t)
		if C.DIR then t.step(math.floor(now / STEP)) end
	end)
	livevsloadedGuard(function(t)
		if C.DIR then t.step(math.floor(now / STEP)) end
	end)
	if not C.ACTIVE then
		return
	end
	if not SIM.rng then
		SIM.random0 = SIM.random0 or math.random
		SIM.rng = L.newRandom(engine.gameTime)
		L.installRandom(SIM.rng)
		-- Which Lua state and which load of this file installs it, and what the game-script state already holds (friend test 2026-10-16: the
		-- friend's game logged this line mid-session with no reload, at 01:18:33, 01:54:43 and 02:10:47, the last two shortly before the ids
		-- forks of resyncs 10 and 11; that run had no load lines). A load that finds a session in the game-script state (an epoch, queued
		-- actions, binders) started mid-session: this load's module values (SIM: native build keys, script build callbacks) are new and
		-- empty, those of the load before are gone with it. (The spread checksum rounds live in the game-script state: X.csAdvance.)
		pcall(function()
			local st = state:get()
			st = type(st) == "table" and st or {}
			logSim(string.format("deterministic math.random installed by bridge %s (%s) at t=%s; the game-script state holds epoch=%s queue=%d binders=%d out=%d",
				X.SRC, C.stateTag(), C.integerText(engine.gameTime()), tostring(st.epoch), #(st.queue or {}), #(st.binders or {}), #(st.out or {})))
		end)
	end
	-- one line per game start for the string-hash seed experiment; inert unless FEVERLINK3_LUA_SEED_LOG=1
	if not SIM.seedProbed then
		SIM.seedProbed = true
		pcall(C.luaSeedProbeOnce, logSim)
	end
	X.subscribeSim(state, "postUpdate")
	simWrapCommands()
	SIM.devTick = (SIM.devTick or 0) + 1
	if SIM.devTick % 4 == 0 then devPoll("sim") end
	SIM.state = state
	local ok, err = pcall(function()
		local stored = nil
		local st = simState(state)
		X.simCatchUp(st)
		-- the cost of the actions the engine ran after the last step (X.simQueueCost), before this step's actions move the account
		local okK, errK = pcall(X.simBookCosts, st, now)
		if not okK then logSim("company cost booking failed: " .. tostring(errK)) end
		local okR, errR = pcall(X.simRefundMoney, st, now)
		if not okR then logSim("refund (native): the money measurement failed: " .. tostring(errR)) end
		-- the builds applied at an earlier step: one build.result line each (before this step's actions change the world)
		local okBR, errBR = pcall(X.simBuildResults, st, now)
		if not okBR then logSim("build.result failed: " .. tostring(errBR)) end
		-- spread checksum rounds: a chunk due at this step is read before the step's items, as a round's own start is (L.itemBefore)
		stored = X.csAdvance(SIM, st, now)
		local due = L.takeDue(st.queue, now)
		for _, it in ipairs(due) do
			if it.kind == "checksum" and (it.spread or 0) > 0 and it.full ~= false and not it.paused then
				-- a full round spread over the next steps (L.SPREAD_LANES): no step reads more than the round's chunk (the host's, else L.CHECKSUM_CHUNK). The step's
				-- actions are applied as always, at their stamps; a chunk after them reads what they built, at the same step on every game.
				local okB, r = pcall(X.csBegin, st.co, "sim", it)
				if okB then
					r.epoch = st.epoch
					-- the round's progress goes to the game-script state, so that whichever simulation Lua state runs a chunk's step reads it
					-- (X.csAdvance); this state keeps its own copy with the entity lists already unpacked
					r.mine = 0
					st.csRounds = st.csRounds or {}
					local rec = X.csSave(r, nil)
					if r.detailShared then X.csDetailStart(r, rec) end
					st.csRounds[#st.csRounds + 1] = rec
					SIM.csLive = SIM.csLive or {}
					SIM.csLive[tostring(rec.epoch) .. "/" .. tostring(rec.n) .. "/" .. tostring(rec.start)] = r
					traceSim("checksum.spread", string.format("round %s begun at t=%s: %d entities, %d chunk(s) of %d every %d step(s); start %.1f ms",
						tostring(it.n), C.integerText(now), r.total, r.chunks, r.chunk, r.spread, r.startCost * 1000))
				else
					-- no report rather than one hashed another way than on the other games (that would read as a difference)
					logSim("checksum round " .. tostring(it.n) .. " could not begin and is not reported: " .. tostring(r))
				end
			elseif it.kind == "checksum" then
				local parts, t = checksumParts(st.co, "sim", it.full == false)
				st.reports[#st.reports + 1] = { n = it.n, time = t, parts = parts, account = L.accountSnapshot(engine) }
				stored = it.n
			elseif it.kind == "action" then
				simApplyAction(st, it, now)
			end
		end
		simAdvanceWatches(st, now)
		simAdvanceBinders(st, now)
		simFlushScriptBuilds(st)
		pcall(simFlushForeign, st)
		X.simCatchUp(st)   -- what this step's applies did for this state's own values (a script build's callback)
		trim(st.results, RESULT_KEEP)
		trim(st.reports, RESULT_KEEP)
		trim(st.refused, RESULT_KEEP)
		state:set(st)
		-- A round just handed over: read the state back once, so a hand-off that loses it is on record (sweep 2026-10-13: the rounds were
		-- computed here and never reached the interface half; nothing said where they were lost).
		if stored ~= nil then
			local okB, back = pcall(function() return state:get() end)
			local found = false
			if okB and type(back) == "table" and type(back.reports) == "table" then
				for _, r in ipairs(back.reports) do if r.n == stored then found = true end end
			end
			traceSim("checksum.handoff", "round " .. C.integerText(stored) .. " stored in the game-script state, read back " .. (found and "ok" or ("LOST (" .. (okB and type(back) or tostring(back)) .. ")")))
			if not found then logSim("checksum round " .. C.integerText(stored) .. " was lost in the game-script state hand-off") end
		end
	end)
	if not ok then logSim("postUpdate failed: " .. tostring(err)) end
end

-- Event subscriptions. GameScriptState:subscribeToEvent(name) subscribes handleEvent "to events with the given name"
-- (https://tf3-docs.transportfever.net/en/reference/base/scripts/gamescript.html; the typed signature
-- GameScriptState.subscribeToEvent(GameScriptState<T>, string), https://wiki.transportfever3.com/script-doc/content/scripts/gamescript.html),
-- and the base scripts subscribe to exactly the names they handle (state:subscribeToEvent("SkipPhase"), scripting guide
-- https://tf3-docs.transportfever.net/en/guides/scripting.html). subscribeToAllEvents "makes handleEvent receive every event"; the base
-- game uses it once (mission_sim). These are the names handleEvent below acts on, with the test that covers each:
--   id "apply_command" (the engine announcing a command; not in the docs, finding from MPFever (MIT)):
--     onPreBuildProposal, onPostBuildProposal   capture of a build at its apply (test_native_capture, test_strict_build)
--   id "feverlink3" (this file's own messages, toSim -> makeScriptingSendEventCmd, "broadcasted to all scripts", api.cmd):
--     session (test_lockstep start/resync), sessionoff (test_session_end), queue (every stamped action), drop (vetoes,
--     test_strict_build), bind (test_lockstep line/vehicle keys), replay, heldout, heldgone, heldinput, heldnative, nativebind,
--     nativecaps (test_strict_build, test_native_capture), outack (test_lockstep), dumpdetail (every game at a sync, test_checksum_spread), co (companies,
--     test_cmds_company). test_lockstep EventSubscriptionTest checks that every name toSim sends is here.
-- "nop" is not in the list: it is the strict mode's trigger, a command whose only purpose is that the engine runs it (the native module
-- runs its queue at the command, natTrigger); handleEvent ignores it. Since 2026-10-18 the script subscribes to ALL events again (see
-- X.subscribeEvents below: a subscription by these names cut the interface half off the build tools' events).
X.SIM_EVENTS = { "onPreBuildProposal", "onPostBuildProposal", "session", "sessionoff", "queue", "drop", "bind", "replay", "heldout",
	"heldgone", "heldinput", "heldnative", "nativebind", "nativecaps", "outack", "dumpdetail", "co" }
-- The interface half's guiHandleEvent acts on the tool events builder.proposalCreate / builder.proposalApply only (the strict capture and
-- the construction schema's log, test_strict_build); the log of other tool-like events (seenOtherEvents) needs every event: DEV only.
X.GUI_EVENTS = { "builder.proposalCreate", "builder.proposalApply" }

-- Subscribes once per load of this file and half. The simulation half subscribes to ALL events; the interface half makes no call.
-- Evidence (real sessions, 2026-10-18): the game's interface state has neither subscribeToEvent nor subscribeToAllEvents ("attempt to
-- call method ... (a nil value)" for both); guiHandleEvent receives the events of the script's one subscription, made in the simulation
-- state. Earlier test builds subscribed the simulation half to the names above only, so the build tools' builder.* events no longer reached
-- guiHandleEvent: owner and friend tests had 0 builds captured (earlier sessions: 56, 39, 13), every build went the release-at-the-click
-- path, ids drifted and repair syncs followed. The lab did not see it: FEVERLINK3_DEV subscribed to all events. The names list above is
-- kept as the record of what handleEvent acts on (its test), not as a subscription.
function X.subscribeEvents(st, names, half, where, log)
	X.subscribedHalves = X.subscribedHalves or {}
	if X.subscribedHalves[half] then return end
	X.subscribedHalves[half] = true
	if half == "gui" then
		log(string.format("events (gui, at %s): no subscription here (the interface state has none): guiHandleEvent receives the events the simulation half subscribed to (all)", where))
	else
		local ok, err = pcall(function() st:subscribeToAllEvents() end)
		log(string.format("events (sim, at %s): all%s", where, ok and "" or (" - SUBSCRIBING FAILED: " .. tostring(err))))
	end
	-- once per load of this file and half (lab): what this state's `app` and `main` hold (the start gate's functions, X.appFn)
	pcall(function() log("app keys (" .. half .. "): " .. C.appKeys()) end)
end
function X.subscribeSim(state, where)
	X.subscribeEvents(state, X.SIM_EVENTS, "sim", where, logSim)
end

-- Messages from the interface half (makeScriptingSendEventCmd with id "feverlink3").
function handleEvent(userParams, state, src, id, name, param)
	-- the base scripts subscribe in handleEvent when the game's own init events arrive (gamescript reference above): an event that
	-- reaches this script before its first postUpdate subscribes it at once
	X.subscribeSim(state, "handleEvent")
	if DEV and id == "apply_command" then
		pcall(function()
			local pi = nil
			pcall(function() pi = param[4] end)
			if pi ~= false then logSim("event apply_command/" .. tostring(name) .. " at t=" .. tostring(engine.gameTime()) .. " playerInitiated=" .. tostring(pi)) end
		end)
	end
	-- UNDOCUMENTED API (docs/ARCHITECTURE.md section 2, "Undocumented API uses and why"): the engine's `apply_command` events (onPreBuildProposal,
	-- onPostBuildProposal) are the only notice a game script gets of a build at its apply step; no documented event replaces them.
	if id == "apply_command" then
		townTraceGuard(function(t)
			local pi, town = nil, nil
			pcall(function() pi = param[4] end)
			pcall(function()
				for i = 1, 3 do
					local d = param[i]
					if type(d) == "table" and d.townEntity ~= nil then town = d.townEntity end
				end
			end)
			if type(pi) ~= "boolean" then pi = nil end
			t.noteCommand(math.floor(engine.gameTime() / STEP), name, town, pi)
		end)
	end
	if id == "apply_command" and name == "onPreBuildProposal" then
		C.poll()
		X.simSessionCheck(state)
		if not C.ACTIVE then return end
		local ok, err = pcall(simCaptureBuild, state, param)
		if not ok then logSim("onPreBuildProposal capture failed: " .. tostring(err)) end
		return
	end
	if id == "apply_command" and name == "onPostBuildProposal" then
		local d = SIM.deferredBuild
		SIM.deferredBuild = nil
		if d and d.at == engine.gameTime() then
			C.poll()
			if not C.ACTIVE then return end
			local ok, err = pcall(simCaptureBuild, state, param, true, d.before, d.ie, d.pre)
			if not ok then logSim("onPostBuildProposal capture failed: " .. tostring(err)) end
		end
		return
	end
	if id ~= "feverlink3" then return end
	if name == "nop" then return end   -- strict mode's trigger: a command that makes the native module run its queue (natTrigger)
	-- The interface half talks: this half catches up with its view of the session first. Both read identity.txt on their own clock, a
	-- second apart at most; the interface half's first lines of a session must not be dropped because this half has not looked yet.
	local sid = type(param) == "table" and name == "session" and param.sid or nil
	C.poll(not C.ACTIVE or (sid ~= nil and sid ~= C.SESSION))
	X.simSessionCheck(state)
	if name == "sessionoff" then
		-- the interface half ended its session (X.sessionOff): it sends this before it hands held commands back to the engine
		if SIM.gen ~= nil or not C.ACTIVE then X.simSessionOff(state, "the interface half ended the session") end
		return
	end
	if not C.ACTIVE then return end
	local ok, err = pcall(function()
		local st = simState(state)
		if name == "session" and type(param) == "table" then
			-- A new run of the interface half. The state is saved with the game, so a save made in the middle of an earlier session
			-- carries that session's outputs (sweep 2026-10-13: 40 stored checksum reports, rounds 2..40 of the earlier session, which the
			-- new session's interface half read as its own rounds 2..40 and so never sent the real ones). The outputs are dropped and the
			-- state is stamped with the epoch of the interface half that may read it; the queue, the bindings and the registry belong to
			-- the world and stay.
			if st.epoch ~= param.epoch then
				local dropped = #st.reports + #st.results + #st.refused + #(st.out or {})
				st.reports, st.results, st.refused, st.out, st.notes, st.late, st.detailAck = {}, {}, {}, nil, nil, 0, nil
				st.epoch = param.epoch
				traceSim("session", "epoch " .. tostring(param.epoch) .. ": dropped " .. C.integerText(dropped) .. " outputs of an earlier session")
				if param.fresh then
					-- A session begins in this world (not a resync reload, which resumes its session from the saved queue): the queue and the
					-- pending bindings of the save are another session's. A save made mid-session (the guest's save at the end, a player's own)
					-- carries them; applied now, every stamp in them would be due at once.
					local stale = #st.queue + #st.binders
					st.queue, st.binders, st.watches, st.heldOut, st.heldInput, st.heldNative, st.nbinds = {}, {}, {}, nil, nil, nil, nil
					-- the module's capabilities a save carries are another game's: this session's interface half tells them (nativecaps)
					st.caps, st.scriptDone = nil, nil
					if stale > 0 then logSim("a new session: dropped " .. C.integerText(stale) .. " queued action(s) and binder(s) of an earlier session found in this world") end
				end
			end
		elseif name == "queue" and type(param) == "table" then
			L.enqueue(st.queue, param)
		elseif name == "dumpdetail" and type(param) == "table" then
			-- the complete keys of the last detail round (the game-script state: every chunk, whichever state read it, X.csDetailStart);
			-- without one (a repair asked by hand, no round differed before it) this state's own keys of the last full round, said partial
			local shared, d = st.csDetail, nil
			if type(shared) == "table" and type(shared.lanes) == "table" then
				d = { time = shared.time, lanes = {}, complete = shared.n }
				for lane, text in pairs(shared.lanes) do
					local items = {}
					for k in tostring(text):gmatch("[^\n]+") do items[#items + 1] = k end
					d.lanes[lane] = items
				end
				st.csDetail = nil   -- written once; the resync save does not carry it
			else
				d = DETAIL.last
			end
			if d then
				local lines = {}
				for lane, items in pairs(d.lanes) do
					for _, k in ipairs(items) do lines[#lines + 1] = lane .. "\t" .. tostring(k) end
				end
				-- sorted, so the two games' files diff line by line (a spread round keeps its keys in the order it read them)
				table.sort(lines)
				table.insert(lines, 1, "# itemised keys of the last full checksum round of this game: time=" .. tostring(d.time) .. " why=" .. tostring(param.why)
					.. (d.complete and (" complete: detail round " .. tostring(d.complete) .. ", every chunk at its step, the same on every game") or "")
					.. (d.partial and (" partial: the keys of the " .. d.partial .. " this simulation Lua state read (no detail round ran: the host asks for one after a round that differed)") or ""))
				-- detail_s<sync>_<time>_<player>.txt: every game of one sync writes the same s<sync> (X.syncDetail), so `detail_s3_*` are that
				-- sync's files of all games; the same <time> says they are of the same round
				local fname = "detail_s" .. tostring(param.sync or 0) .. "_" .. tostring(d.time) .. "_" .. tostring(C.PLAYER_ID) .. ".txt"
				C.dumpReplace(fname, table.concat(lines, "\n") .. "\n")
				traceSim("detail", string.format("%s: %d lines written to %s (diff the games' files of this sync to see which entities differ)", tostring(param.why), #lines - 1, fname))
			else
				traceSim("detail", tostring(param.why) .. ": no full checksum round has run in this game yet, no detail file")
			end
			-- the interface half waits for this before it loads (X.syncDetail): the event must run before the load replaces this world
			st.detailAck = param.ack
		elseif name == "nativebind" and type(param) == "table" then
			local nb = SIM.nativeBinds and SIM.nativeBinds[param.n]
			local saved = st.nbinds and st.nbinds[tostring(param.n)]
			if not nb and saved then
				local okP, body = pcall(C.parse, saved.text)
				if okP and type(body) == "table" then nb = { body = body, comps = saved.comps, before = saved.before } end
			end
			if not nb then
				local have = {}
				for k in pairs(SIM.nativeBinds or {}) do have[#have + 1] = tostring(k) end
				local haveSaved = {}
				for k in pairs(st.nbinds or {}) do haveSaved[#haveSaved + 1] = tostring(k) end
				traceSim("native.bind", string.format("%s: no snapshot for output %s (type %s; in memory [%s], in the state [%s]; see native.snapshot for why none was taken)",
					tostring(param.key), tostring(param.n), type(param.n), table.concat(have, " "), table.concat(haveSaved, " ")))
			end
			if st.nbinds then st.nbinds[tostring(param.n)] = nil end
			if nb and type(param.key) == "string" then
				if SIM.nativeBinds then SIM.nativeBinds[param.n] = nil end
				st.binders[#st.binders + 1] = { key = param.key, fn = "makeWorldBuildProposalCmd", body = nb.body, comps = nb.comps, before = nb.before, steps = 0 }
				logSim("action " .. param.key .. ": this game's own native build is keyed here like a replay (" .. table.concat(nb.comps, "/") .. ")")
			end
		elseif name == "replay" then
			-- the interface half's own sends of a build (the pause path, a held command released): the key names the build in the report
			-- of a mark no engine event spent (friend test 2026-10-18 printed "nil@14282800, nil@14285000": these events carried no key)
			markReplay(st, engine.gameTime(), nil, type(param) == "table" and type(param.key) == "string" and param.key or nil)
		elseif name == "nativecaps" and type(param) == "table" then
			-- into the game-script state: this event reaches one simulation state, every state's stamps decode with them (simCtx)
			st.caps = { patch = param.patch == true, inject = param.inject == true, terrain = param.terrain == true, wall0 = type(param.wall0) == "number" and param.wall0 or nil }
		elseif name == "heldinput" and type(param) == "table" and type(param.text) == "string" then
			-- the input-level text of a held command whose record is read after the build ran (the interface half cannot capture it)
			st.heldInput = { text = param.text, wall = C.clock() }
		elseif name == "heldnative" and type(param) == "table" and type(param.text) == "string" then
			-- the engine's own held command (native_text_<id>.txt) of a command released without a record: the capture ships it along
			st.heldNative = { text = param.text, id = param.id, wall = C.clock() }
		elseif name == "heldout" and type(param) == "table" then
			st.heldOut = st.heldOut or {}
			st.heldOut[#st.heldOut + 1] = { key = param.key, wall = C.clock() }
			trim(st.heldOut, 8)
		elseif name == "drop" and type(param) == "table" and param.key then
			-- a peer vetoed this build (it could not rebuild it): it is not applied on any game. Gone from the queue, or too late.
			local keep, dropped = {}, false
			for _, q in ipairs(st.queue or {}) do
				if q.key == param.key then dropped = true else keep[#keep + 1] = q end
			end
			st.queue = keep
			local applied = false
			for _, r in ipairs(st.results or {}) do
				if r.key == param.key then applied = true end
			end
			local what
			if dropped then
				what = " dropped from the queue: a game vetoed it ("
			elseif param.own then
				-- the game that casts the veto never queued the build: nothing to drop here, nothing was applied
				what = " never entered this game's queue: this game vetoed it ("
			elseif applied then
				what = " vetoed too late: it was applied already ("
			else
				what = " not in the queue (not received yet, or applied by the interface half during a pause): vetoed ("
			end
			logSim("action " .. tostring(param.key) .. what .. tostring(param.why) .. ")")
		elseif name == "heldgone" and type(param) == "table" then
			local keep = {}
			for _, e in ipairs(st.heldOut or {}) do
				if e.key ~= param.key then keep[#keep + 1] = e end
			end
			st.heldOut = keep
		elseif name == "outack" and type(param) == "table" then
			local keep = {}
			for _, o in ipairs(st.out or {}) do
				if o.n > (tonumber(param.n) or 0) then keep[#keep + 1] = o end
			end
			st.out = keep
		elseif name == "bind" and type(param) == "table" and param.key then
			local okT, t = pcall(engine.gameTime)
			X.simBind(st, param.key, param.e, okT and t or nil)
		elseif name == "co" and Co and type(param) == "table" and type(param.it) == "table" then
			simReplayCompany(st, param)
		end
		X.simCatchUp(st)
		state:set(st)
	end)
	if not ok then logSim("event " .. tostring(name) .. " failed: " .. tostring(err)) end
end
end -- section: the simulation half

-- ================================================================== interface half

local logGui = C.logger("gui")
local G = {
	started = false, frames = 0,
	perms = L.newPermissions(),
	hud = nil, hudWritten = { version = nil }, uiReader = nil,
	marks = {},               -- checksum stamps ahead: the batch is shortened to land on each (L.pace)
	accountAt = {},           -- checksum round n -> this game's account at that round
	loading = false,          -- a resync load was started: the rest of this batch of lines belongs to the reloaded game
	session = L.newSession(), peers = {},
	reader = nil, writer = nil,
	seq = 0,                  -- local action sequence (the `seq` of this player's actions)
	pending = {},             -- key -> callback of a command this player issued
	bind = {}, rev = {},      -- key -> local entity, local entity -> key (mirror of the simulation's bind)
	co = nil,                 -- companies: mirror of the simulation's registry; ahead of it while a pause applies actions
	localCompany = nil,       -- the player entity this game's windows act as (separate mode only)
	coWritten = nil,          -- stamp of the last companies.txt
	coFault = nil,            -- the last fault of the company layer in a frame (logged once, not every frame)
	wrapped = false,          -- api.cmd.sendCommand was wrapped: commands are held, stamped and shipped
	tags = setmetatable({}, { __mode = "k" }),  -- command object -> { name, args }
	lastSpeedSet = nil, lastClock = nil,
	resultsSeen = {}, reportsSent = {}, refusedSent = {}, lateSeen = 0,
	margin = L.newMargin(),   -- the measured stamp margin of builds (L.marginSample): this game's need as a receiver
	outSeen = {},             -- native builds the simulation half captured that this half already shipped
	uiSeq = {},               -- id of a command record from the UI-state hook -> the sequence number it was shipped as
	graces = {},              -- key -> a held build of a PAUSED game that waits for vetoes before it is committed
	parked = {},              -- key -> a held build of another player's paused game, received but not applied until its commit
	buildAts = {},            -- key -> stamp of the build actions this game knows (the veto check waits for the earlier ones)
	dry = {},                 -- key -> a build whose dry run waits for the earlier builds to land
	vetoed = {},              -- key -> why, builds a game vetoed
	authPending = {},         -- round -> a host authority that arrived before this game reached that round
	authLast = nil,           -- the money differences the previous round measured (a correction needs the same one twice)
	bindsWritten = nil,       -- text of the last binds.txt (the UI-state hook reads it to name entities)
}

-- The originals are kept in package.loaded so a savegame reload (which re-runs this file in the same Lua state)
-- finds them instead of wrapping the wrapper (finding from MPFever (MIT)).
local function originals()
	local ok, t = pcall(function()
		local t = package.loaded["feverlink3_orig_cmds"]
		if type(t) ~= "table" then t = {}; package.loaded["feverlink3_orig_cmds"] = t end
		return t
	end)
	if ok then return t end
	G.orig = G.orig or {}
	return G.orig
end

-- This player's action numbers, kept over a savegame reload: a reload runs this file again in the same Lua state, so the counter of the
-- session lives in package.loaded (as the original commands do, originals()). With the numbers the game-script state still names
-- (L.seqFloor) it is where X.seqResume numbers on from: an action number is never given twice in a world, so a key names one entity on
-- every game (friend test 2026-10-21 21:51: numbers began at 1 again after a resync, the new 1:4, a vehicle, took over the key of a line
-- made before the resync, and three set-line commands that named the line were refused on both games).
function X.seqCarry()
	local ok, t = pcall(function()
		local t = package.loaded["feverlink3_seq"]
		if type(t) ~= "table" then t = {}; package.loaded["feverlink3_seq"] = t end
		return t
	end)
	if ok then return t end
	X.seqTable = X.seqTable or {}
	return X.seqTable
end

-- an action number went out (every ship of an action line passes here, before or after its own send)
function X.seqShipped(seq)
	seq = tonumber(seq)
	if not seq or seq <= 0 then return end
	local t = X.seqCarry()
	if t.sid ~= C.SESSION or t.player ~= C.PLAYER_ID then t.sid, t.player, t.seq = C.SESSION, C.PLAYER_ID, 0 end
	if seq > (t.seq or 0) then t.seq = seq end
end

-- A session begins in this Lua state (the first frame after a load included): number on after every action number this player already
-- used in the session (the carried counter) and every one the loaded world still names (bindings, queue, binders, watches).
function X.seqResume(state)
	local fromState = 0
	pcall(function() fromState = L.seqFloor(C.PLAYER_ID, state:get()) end)
	local t = X.seqCarry()
	local carried = (t.sid == C.SESSION and t.player == C.PLAYER_ID) and (tonumber(t.seq) or 0) or 0
	local floor = math.max(fromState, carried)
	if floor > G.seq then
		logGui(string.format("action numbers go on from %s:%d (the loaded world names up to %d, this session shipped up to %d): no key is given twice",
			tostring(C.PLAYER_ID), floor + 1, fromState, carried))
		G.seq = floor
	end
end

local function send(kind, payload)
	if kind == "clock" or kind == "action" then X.stampHop(payload) end
	if kind == "action" and type(payload) == "table" then X.seqShipped(payload.seq) end
	local ok, err = C.writerSend(G.writer, kind, payload)
	if not ok then logGui("cannot send " .. kind .. ": " .. tostring(err)) end
end

-- companies: the frames that carry the company layer also pace the game and answer the host, so a fault in it is logged
-- once and skipped, never allowed to end the frame.
local function coGuard(what, fn, ...)
	local ok, err = pcall(fn, ...)
	if not ok then
		local text = what .. ": " .. tostring(err)
		if G.coFault ~= text then
			G.coFault = text
			logGui("company layer failed in " .. text)
		end
	end
	return ok
end

local function toSim(name, param)
	-- makeScriptingSendEventCmd(src, id, name, param): src is "the filename of the game_script that should be the pretended origin of
	-- the event" (tf3-docs api/cmd, cmd.d.tl:788); this message has no such origin, and the base game sends its own with src "" in
	-- every one of its calls. Until 2026-10-18 src was "feverlink3", which names no game script. handleEvent tells ours by id.
	O.sendCommand(O.event("", "feverlink3", name, param))
end

local function setSpeed(sp)
	if sp ~= G.lastSpeedSet or G.frames % 60 == 0 then
		G.lastSpeedSet = sp
		O.sendCommand(O.setSpeed(sp))
	end
end

-- This game's clock line with its promise (L.promise, the input barrier): every item it ships after this line is stamped at or after
-- `safe`; `pause` = stopped at a pause, `safe` is the pause time and the peers may reach it exactly. Sent every frame the clock moved,
-- every X.CLOCK_KEEP_S of real time anyway (X.clockDue), and before the first item stamped at a pause (X.promisePause).
function X.reportClock(now)
	X.clockSentAt = C.clock()
	local safe, floor, pause = L.promise(G.session, now, G.promiseFloor)
	G.promiseFloor, G.promisePause = floor, pause
	local ahead = L.aheadFor(G.session.speed, G.session.lead)
	send("clock", { t = now, speed = engine.speed(), ahead = ahead, need = L.marginNeed(G.margin, G.session.speed), safe = safe, pause = pause or nil,
		stop = (pause and X.curStop == now) and now or nil })
end

-- A clock line is due although the clock did not move: X.CLOCK_KEEP_S of real time since the last one. It was only every 30 frames,
-- which is half a second at 60 fps and six at 5 fps: a game standing still (at the barrier, the pacing, a pause) then told its peers of
-- a larger promise (a longer lead from the host, a new margin need) and the host of where it stopped only that much later. (The 30
-- frames stay as well: at a high frame rate they come sooner, as before.)
X.CLOCK_KEEP_S = 0.25
function X.clockDue()
	return X.clockSentAt == nil or C.clock() - X.clockSentAt >= X.CLOCK_KEEP_S
end

-- A game stopped at a pause promises the pause time itself, and it must have said so before it ships the first item stamped there:
-- a peer still holding its earlier promise could otherwise leave the pause (the host's resume) and pass the stamp before the item.
function X.promisePause(now)
	if G.session.pauseAt and now >= G.session.pauseAt and not G.promisePause then X.reportClock(now) end
end

-- The waits of the input barrier, for the logs: a hold is a stretch of frames in which a running game stands still because its next
-- step would pass the barrier (a peer's clock is late: jitter, a stalled frame, a slow link). Every hold is traced (`barrier`); one
-- longer than a second also goes to mod.log, and a count every minute.
function X.barrierWatch(now, sp)
	local s = G.session
	local bar = (s.started and (s.speed or 0) > 0) and L.barrier(s, G.peers) or nil
	local held = bar ~= nil and sp == 0 and now + STEP > bar and not (s.pauseAt and now >= s.pauseAt)
	local B = G.barrier or { n = 0, wait = 0, logged = C.clock() }
	G.barrier = B
	local clock = C.clock()
	if held and not B.since then
		B.since, B.at = clock, now
		B.who, B.safe = L.barrierPeer(G.peers)
		traceGui("barrier", string.format("hold at t=%s: the next step passes the barrier %s (player %s promised %s) x%s%s",
			C.integerText(now), C.integerText(bar), tostring(B.who), tostring(B.safe), tostring(s.speed), X.holdDetail(now, bar, B.who, clock)))
		if X.LS and G.ls then X.LS.holdStart(G.ls, clock) end
	elseif not held and B.since then
		local d = clock - B.since
		B.n, B.wait, B.since = B.n + 1, B.wait + d, nil
		traceGui("barrier", string.format("released at t=%s after %.3f s (player %s)%s", C.integerText(now), d, tostring(B.who), X.holdDetail(now, nil, B.who, clock)))
		if X.LS and G.ls then X.LS.holdEnd(G.ls, clock, d) end
		if d >= 1 then
			logGui(string.format("input barrier: held %.2f s at t=%s waiting for player %s's clock (its promise %s)", d, C.integerText(B.at), tostring(B.who), tostring(B.safe)))
		end
	end
	if clock - B.logged >= 60 and B.n > 0 then
		logGui(string.format("input barrier: %d hold(s), %.2f s in all, in the last %.0f s", B.n, B.wait, clock - B.logged))
		B.n, B.wait, B.logged = 0, 0, clock
	end
	X.linkMinute(clock)
end

-- ---- smooth pacing (bug 4; feverlink3_lockstep.lua "smooth pacing", native pace.rs). Every frame: the room before the nearer of the
-- stops a peer's clock moves, when that stop is expected to move next, this game's batch time, the factor wanted and its smoothed value; the factor goes to
-- the native module (`pace {permille}`) when it moved by X.PACE_STEP, when it came back to 1, and every X.PACE_REFRESH_S while it is not 1
-- (the module drops a factor not refreshed for 3 s). Nothing is sent unless the module said it can (`batch_pace` among its hooks).
-- FEVERLINK3_SMOOTH_PACE=0 keeps the factor at 1 (the native module has the same switch). Wall-clock pacing only: no step, stamp or
-- mechanic changes.
X.PACE_STEP = 0.03
X.PACE_REFRESH_S = 1.0
X.PACE_ON = true
pcall(function() X.PACE_ON = C.env("FEVERLINK3_SMOOTH_PACE") ~= "0" end)
function X.paceTick(now)
	local P = G.pace or L.paceNew()
	G.pace = P
	local s, clock = G.session, C.clock()
	local stop = L.paceStop(s, G.peers, L.aheadFor(s.speed, s.lead))
	L.paceObserve(P, stop, clock)
	L.paceOwn(P, now, clock, (P.ranSp or 0) > 0, P.sent or 1)
	P.ranSp = G.lastSpeedSet
	local target, left = 1, nil
	if X.PACE_ON and stop and s.pauseAt == nil and s.holdAt == nil then
		target, left = L.paceTarget(stop - now, s.speed, L.paceNext(P, clock), L.paceBatch(P), P.ownAt and (clock - P.ownAt) * 1000 or 0)
	end
	-- both ways: behind the furthest peer with room before the stop, the batches come sooner (L.paceHurry)
	if X.PACE_ON and target == 1 and s.started and (s.speed or 0) > 0 and s.pauseAt == nil and s.holdAt == nil then
		local far = L.furthestPeer(G.peers)
		if far then
			target = L.paceHurry((far - now) / STEP, stop and (stop - now) / STEP or nil, s.speed, L.catchUp(s, G.peers, now) ~= nil)
		end
	end
	P.room, P.left = stop and (stop - now) or nil, left
	-- the window says how far this game is behind while the host has it as stalled (the HUD's "catching up")
	if X.selfStalled() then
		local behind = L.catchUp(s, G.peers, now)
		pcall(Hud.setBehind, G.hud, behind and behind / L.stepsPerSecond(s.speed, L.paceBatch(P), #P.batches >= 4) or 0)
	end
	local trim = L.paceSmooth(P, target, clock)
	if X.LS and G.ls then X.LS.pace(G.ls, trim, clock) end
	X.paceSend(P, clock, now)
end

-- The factor to the native module (see above); a stretch of slowing down starts and ends with a trace line.
function X.paceSend(P, clock, now)
	local N = G.nat
	if not (N and N.ready and N.ready.pace) then return end
	local sent = P.sent or 1
	local due = math.abs(P.trim - sent) >= X.PACE_STEP or (P.trim == 1 and sent ~= 1) or (P.trim ~= 1 and clock - (P.sentAt or -100) >= X.PACE_REFRESH_S)
	if not due then return end
	local permille = math.floor(P.trim * 1000 + 0.5)
	-- (X.ctlSend, not natSend: that local of the strict-mode section is declared further down the chunk)
	if X.ctlSend("pace", { permille = permille }) then
		N.lastWrite = C.clock()
		if (sent == 1) ~= (P.trim == 1) then
			traceGui("pace", string.format("%s at t=%s: factor %.2f (target %.2f, room_steps=%s, room_lasts_ms=%s, stop_moves_in_ms=%s, batch_ms=%.0f, jitter_ms=%.0f, x%s)",
				P.trim == 1 and "back to 1" or (P.trim > 1 and "slowing" or "hurrying"), C.integerText(now), P.trim, P.target or 1, P.room and string.format("%.1f", P.room / STEP) or "-",
				P.left and string.format("%.0f", P.left) or "-", (function() local w = L.paceNext(P, clock); return w and string.format("%.0f", w) or "-" end)(), L.paceBatch(P), L.paceJitter(P), tostring(G.session.speed)))
		end
		P.sent, P.sentAt = P.trim, clock
	end
end

-- The session ends (or this game leaves it): the factor goes back to 1 at once.
function X.paceEnd(why)
	local P = G.pace
	G.pace = nil
	if P and (P.sent or 1) ~= 1 and G.nat and G.nat.ready and G.nat.ready.pace then
		X.ctlSend("pace", { permille = 1000 })
		traceGui("pace", "back to 1: " .. tostring(why))
	end
end

-- ---- the latency logs (see X.LS at the top). The state of this session's link statistics, made at its first use.
function X.ls()
	if not X.LS then return nil end
	if not G.ls then G.ls = X.LS.new(C.clock()) end
	return G.ls
end

-- What a barrier hold line adds (start and end): the peer's last clock and how far it is behind this game, the room left to the
-- barrier, the age of that peer's last clock line here, the round trip of its path via the host, this game's last frame time, and the
-- engine's speed (the session speed is the line's x). Missing values print "?".
function X.holdDetail(now, bar, who, clock)
	local okD, text = pcall(function()
		local pc = who ~= nil and G.peers[who] or nil
		local age, rtt
		if X.LS and G.ls and who ~= nil then age, rtt = X.LS.peerNow(G.ls, who, clock) end
		local function f(v) return type(v) == "number" and string.format("%.0f", v) or "?" end
		local okE, es = pcall(engine.speed)
		local P = G.pace
		return string.format(" peer_t=%s behind_steps=%s room_steps=%s clock_age_ms=%s rtt_ms=%s frame_ms=%s engine_speed=%s pace=%s pace_target=%s pace_batch_ms=%s pace_jitter_ms=%s",
			pc and C.integerText(pc.t) or "?", pc and string.format("%.1f", (now - pc.t) / STEP) or "?", bar and string.format("%.1f", (bar - now) / STEP) or "-",
			f(age), f(rtt), f(G.ls and G.ls.lastFrame), okE and tostring(es) or "?",
			P and string.format("%.2f", P.trim) or "-", P and string.format("%.2f", P.target or 1) or "-", P and f(L.paceBatch(P)) or "-", P and f(L.paceJitter(P)) or "-")
	end)
	return okD and text or (" detail=? (" .. tostring(text) .. ")")
end

-- A peer clock or an action this game read (H.peerclock, H.action): its stamps into the link statistics; an action's offsets are kept
-- for its build.lag line (X.buildLag), a bounded few.
function X.linkRead(kind, from, p)
	local st = X.ls()
	if not st or type(p) ~= "table" or from == nil or from == C.PLAYER_ID then return end
	if kind == "peerclock" then
		X.LS.peerClock(st, from, p.hop, C.clock(), X.wallNow())
	else
		X.LS.action(st, from, p.hop, X.wallNow())
		if type(p.hop) == "table" and p.seq ~= nil then
			X.actHop = X.actHop or {}
			X.actHopN = (X.actHopN or 0) + 1
			if X.actHopN > 256 then X.actHop, X.actHopN = {}, 1 end
			X.actHop[L.itemKey(from, p.seq)] = { so = p.hop.so, ro = p.hop.ro }
		end
	end
end

-- The interface frame: its time since the frame before into the statistics, and a `frame.slow` line for a frame that took
-- X.SLOW_FRAME_S or more, with what this script spent of it, part by part (X.frameMark): a stalled frame sends no clock line, so every
-- peer waits at its barrier for it (bug 4: holds on a 5 s cadence).
X.SLOW_FRAME_S = 0.25
-- (each call under pcall in guiUpdate: a clock that fails must not cost the frame)
function X.frameStart()
	local clock = C.clock()
	local last = X.frameAt
	X.frameAt, X.markAt = clock, clock
	X.fwLast, X.fw = X.fw, {}
	if not last then return end
	local dt = clock - last
	local st = X.ls()
	if st then X.LS.frame(st, dt) end
	if dt >= X.SLOW_FRAME_S then
		local parts, work = {}, 0
		for _, k in ipairs({ "read", "window", "companies", "collect", "nat", "pace" }) do
			local v = X.fwLast and X.fwLast[k]
			if v then parts[#parts + 1] = k .. "=" .. string.format("%.0f", v); if k ~= "companies" then work = work + v end end
		end
		traceGui("frame.slow", string.format("dt_ms=%.0f mod_ms=%.0f %s x%s", dt * 1000, work, table.concat(parts, " "), tostring(G.session and G.session.speed)))
	end
end

function X.frameMark(name)
	local c = C.clock()
	if X.fw and X.markAt then X.fw[name] = (c - X.markAt) * 1000 end
	X.markAt = c
end

-- Once a minute: the link statistics' lines (trace.log; the summary also in mod.log), and build.lag lines that waited too long for
-- their callback.
function X.linkMinute(clock)
	local st = X.ls()
	if not st then return end
	local lines = X.LS.minute(st, clock)
	if not lines then return end
	for i, line in ipairs(lines) do
		local tag, rest = line:match("^(%S+) (.*)$")
		traceGui(tag or "link", rest or line)
		if i == 1 then logGui(line) end
	end
	for key, w in pairs(X.lagWait or {}) do
		if clock - w.t > 10 then
			X.lagWait[key] = nil
			X.buildLag(w.detail, nil)
		end
	end
end

-- The build's one line on this game (`build.lag`), from the native module's `timing apply` detail (click, ship, recv, apply: wall ms;
-- stamp and step: game time) and the tool callback's wall time when this game is the originator of a deferred build (`cb`, from the
-- module's `completed`; "-" when there is none). Durations across two machines are on the host's clock (the action's offsets, X.linkRead);
-- "?" where an offset is not known.
function X.buildLag(detail, cb)
	local okL, line, total = pcall(function()
		local key = detail:match("key=(%S+)")
		local function n(name) return tonumber(detail:match(name .. "=(%d+)")) end
		local click, ship, recv, apply = n("click"), n("ship"), n("recv"), n("apply")
		local own = key ~= nil and key:match("^(%d+):") == tostring(C.PLAYER_ID)
		local off = (not own) and X.actHop and X.actHop[key] or nil
		local so, ro = own and 0 or (off and tonumber(off.so)), own and 0 or (off and tonumber(off.ro))
		local function d(a, b, oa, ob)
			if a and b and a > 0 and b > 0 and oa and ob then return (b - ob) - (a - oa) end
			return nil
		end
		local function f(v) return v and string.format("%.0f", v) or "?" end
		local tot = d(click, apply, so, ro)
		return string.format("key=%s own=%s click=%s ship=%s recv=%s apply=%s cb=%s stamp=%s step=%s ship_ms=%s wire_ms=%s wait_ms=%s cb_ms=%s total_ms=%s cb_total_ms=%s off=%s/%s",
			tostring(key), own and 1 or 0, f(click), f(ship), f(recv), f(apply), cb and f(cb) or "-", detail:match("stamp=(%d+)") or "?", detail:match("step=(%d+)") or "?",
			f(d(click, ship, 0, 0)), f(d(ship, recv, so, ro)), f(d(recv, apply, 0, 0)), f(cb and d(apply, cb, 0, 0)), f(tot), f(cb and d(click, cb, so, ro)),
			so and tostring(so) or "?", ro and tostring(ro) or "?"), tot
	end)
	traceGui("build.lag", okL and line or ("the line could not be made: " .. tostring(line)))
	if okL and total and X.ls() then X.LS.build(X.ls(), total) end
end

local function guiCtx(it)
	local ctx = actionCtx(it, G.co, function(key) return L.resolveKey(G.bind, key) end)
	ctx.canPatch = NATIVE_PATCH and G.nat ~= nil and G.nat.ready ~= nil and G.nat.ready.patch == true
	ctx.canInject = NATIVE_INJECT and G.nat ~= nil and G.nat.ready ~= nil and G.nat.ready.inject == true
	ctx.canTerrain = G.nat ~= nil and G.nat.ready ~= nil and G.nat.ready.terrain == true
	return ctx
end

-- What the engine says when it refuses a command (errorState of the result), for the log: without it a refused build is
-- only "REFUSED" and the reason has to be guessed from the record.
local function refusalWhy(data)
	local parts = {}
	pcall(function()
		local es = data.resultProposalData.errorState
		for _, k in ipairs({ "messages", "warnings" }) do
			local list = es[k]
			if list ~= nil then
				for i = 1, math.min(#list, 6) do parts[#parts + 1] = k:sub(1, 1) .. ":" .. tostring(list[i]) end
			end
		end
		if es.critical ~= nil then parts[#parts + 1] = "critical=" .. tostring(es.critical) end
	end)
	if #parts == 0 then return "" end
	return " (" .. table.concat(parts, "; "):sub(1, 400) .. ")"
end

-- The first argument of a build's callback, `data : WorldBuildProposalCommandData` (tf3-docs api/cmd: sendCommand's callback
-- (data, success, resultEntities); cmd.d.tl:510-524 its fields), for a build this game's script sent and every game applied at the
-- stamp: the proposal and context the script passed, its flags, and the result entities of this game's apply. The game's own windows
-- read `cmdData.resultEntities` and `cmdData.proposal.proposal` (bridge_and_tunnel.tl:195-203, updateRefundableEntities: how a build
-- enters the interface's refundable list that the bulldozer copies into its Context). Before 2026-10-19 the callback got nil.
-- `proposal` is the script's object: a native Proposal as the game's windows pass (createBridgeOrTunnelProposal) is exactly the
-- engine's; a SimpleProposal stays one (the engine's converted Proposal lives only where the command ran: unproven gap).
function X.buildCmdData(args, list)
	if type(args) ~= "table" then return nil end
	return { proposal = args[1], context = args[2], ignoreErrors = args[3] == true, doDust = args[5], resultEntities = list }
end

local function deliverResult(key, data, success, entities)
	local cb = G.pending[key]
	local args = G.pendingArgs and G.pendingArgs[key]
	if G.pendingArgs then G.pendingArgs[key] = nil end
	if cb then
		G.pending[key] = nil
		-- the callback's third argument is {{Engine.Entity, Engine.Revision}} (C.resultEntities): a result from the simulation half
		-- names the entity only, and gets this game's revision of it back here
		local okR, list = pcall(C.resultEntities, entities)
		if data == nil and args then data = X.buildCmdData(args, okR and list or {}) end
		local ok, err = pcall(cb, data, success, okR and list or entities)
		if not ok then logGui("callback of " .. key .. " failed: " .. tostring(err)) end
	end
end

-- Applies a due item from the interface state (the game is stopped at a pause). Callbacks are allowed here, so
-- the engine's own result binds the created entity and answers the originator.
-- A finished checksum round: remembered (the host's authority is measured against it) and sent. The host's report
-- carries its account as `authority`; the players apply it when it comes back as an `authority` line.
-- The host's money values of a checksum round, applied as the difference to this game's own at the same round. The host
-- reaches a round and sends its values at once; a game that is a little behind has no account of its own for that round
-- yet, so the values wait here (seen in a real game: every round was skipped, so money and loans never followed).
local function applyAuthority(p)
	local own = G.accountAt[p.n]
	if not own then return false end
	local applied, failed, measured = L.applyAuthority(engine, own, p.values, G.authLast)
	G.authLast = measured
	if next(measured) ~= nil and next(applied) == nil and #failed == 0 then
		logGui("host authority: round " .. tostring(p.n) .. " differs (balance " .. tostring(measured.balance) .. ", loan " .. tostring(measured.loan) .. "); corrected only if the next round shows the same")
	end
	if applied.balance then logGui("host authority: balance corrected by " .. applied.balance .. " (round " .. tostring(p.n) .. ")") end
	if applied.loan then logGui("host authority: loan corrected by " .. applied.loan .. " (round " .. tostring(p.n) .. ")") end
	for _, name in ipairs(failed) do logGui("host authority: " .. name .. " differs but this build cannot correct it") end
	return true
end

-- A part this game could not hash is not ready: unknown in this round, never a difference. Its value is a marker (X.NOT_READY, or
-- "error" from a lane that failed); the report names it in `notReady` instead of sending the marker as a hash, and the host leaves it
-- out of the comparison (core ChecksumOutcome.not_ready; a verify round is asked again). A part that both games hashed is compared as
-- always, and a part one world simply lacks (a town only it has) is still a difference.
X.NOT_READY = "notready"
-- `cost` (a spread round's, L.checksumCost): what the round cost this game, for the host to size the chunks of the next rounds
local function reportRound(n, t, parts, account, cost)
	G.reportsSent[n] = true
	-- a spread round is complete: the marks of the chunks it did not need go (one entry each: another round's mark at the same step stays)
	local cm = G.csMarks and G.csMarks[n]
	if cm then
		G.csMarks[n] = nil
		for _, m in ipairs(cm) do
			for i = #G.marks, 1, -1 do if G.marks[i] == m then table.remove(G.marks, i); break end end
		end
	end
	G.accountAt[n] = account
	G.accountAt[n - 40] = nil
	local notReady, measured = {}, {}
	for k, v in pairs(parts or {}) do
		if v == X.NOT_READY or v == "error" then notReady[#notReady + 1] = k else measured[k] = v end
	end
	table.sort(notReady)
	if #notReady > 0 then
		logGui("checksum round " .. C.integerText(n) .. ": not ready here: " .. table.concat(notReady, ", ") .. " (reported as not ready, not compared in this round)")
		parts = measured
	end
	send("checksum_report", { n = n, time = t, parts = parts, authority = (C.PLAYER_ID == 0) and account or nil, notReady = (#notReady > 0) and notReady or nil,
		cost = type(cost) == "table" and cost or nil })
	local waiting = G.authPending[n]
	if waiting then
		G.authPending[n] = nil
		local ok, err = pcall(applyAuthority, waiting)
		if not ok then logGui("host authority failed: " .. tostring(err)) end
	end
end

-- A paused round is hashed by this half, with its mirror of the company registry (G.co). A session's first run of this half (a
-- resync reload too) starts the mirror empty (Co.new) and fills it from the game-script state, which carries the registry the save
-- loaded, only in collectFromSim; a round that arrived before that hashed the empty mirror and reported no companies part (friend test
-- 2026-10-16: five repairs "still differ after the reload: companies", each reload worked). So the round waits until the mirror has
-- read the loaded state (G.coReady), at most CS_WAIT_S seconds; then the part goes out not ready instead of as the empty registry.
X.CS_WAIT_S = 10
function X.guiChecksum(it)
	local ready = (not Co) or G.coReady == true
	local okC, now = pcall(C.clock)
	now = okC and tonumber(now) or 0
	G.csWait = G.csWait or {}
	local w = G.csWait[it.n]
	if not ready then
		if not w then
			w = { it = it, since = now }
			G.csWait[it.n] = w
			logGui("checksum round " .. C.integerText(it.n) .. ": the company registry is not read from the loaded state yet; the round waits for it (at most " .. X.CS_WAIT_S .. " s)")
		end
		if now - w.since < X.CS_WAIT_S then return end
		logGui("checksum round " .. C.integerText(it.n) .. ": the company registry is still not read after " .. X.CS_WAIT_S .. " s; companies are reported not ready")
	elseif w then
		logGui(string.format("checksum round %s: the company registry was read from the loaded state after %.1f s; the round is hashed now", C.integerText(it.n), now - w.since))
	end
	G.csWait[it.n] = nil
	local parts, t = checksumParts(ready and G.co or false, "gui", it.full == false)
	reportRound(it.n, t, parts, L.accountSnapshot(engine))
end

-- every frame, after collectFromSim: the rounds that wait for the registry are tried again
function X.checksumWaitTick()
	if not G.csWait or next(G.csWait) == nil then return end
	local waiting = {}
	for _, w in pairs(G.csWait) do waiting[#waiting + 1] = w.it end
	table.sort(waiting, function(a, b) return a.n < b.n end)
	for _, it in ipairs(waiting) do X.guiChecksum(it) end
end

-- companies: hands an action this half applied to the simulation half, which keeps the registry in the savegame
local function coToSim(it, ok, cost, category)
	if not Co then return end
	toSim("co", { it = { from = it.from, at = it.at, seq = it.seq, key = it.key, body = it.body }, ok = ok == true, cost = cost, category = category })
end

-- An action this game applied natively when its player made it (a build with a native tool that was not held): every
-- OTHER game runs the company layer when it applies the action at its stamp (the ledger of who built and spent what is part of
-- the checksum), so this game must run it too, with the same stamp, or its ledger is the only one without the action and the
-- games "differ in companies" until a resync copies everything (seen in a real game: a reload every two minutes).
local function recordOriginator(body, seq, at, category)
	if not Co or type(body) ~= "table" then return end
	local spec = L.COMMANDS[body.fn]
	if not spec or spec.virtual then return end
	local it = { kind = "action", at = at, from = C.PLAYER_ID, seq = seq, body = body, key = L.itemKey(C.PLAYER_ID, seq) }
	coGuard("originator ledger", function()
		local ctx = guiCtx(it)
		Co.afterApply(ctx, it, spec, Co.info(it, spec, category, nil))
		coToSim(it, true, nil, category)
	end)
end

-- strict mode (defined in its section below): a game paused at the stamp releases its own held command; a refusal cancels it
local natPauseRelease, natHeldRefused, natVeto, vetoChat

-- used outside this section:
local route, stampAfterFastest, PARK_MAX, pausedNow, graceMs, VETO_STEPS, CONSTRUCTION_VETO_STEPS, CONSTRUCTION_GRACE_MS, noteBuild,
	pendingBefore, vetoCheck, dryPump, shipNative, shipUiCommand, shipLocal, uiReply
do -- section: applying items, the veto, shipping (not indented, see SECTIONS at the top)
local function guiApplyNow(it, now)
	if it.kind == "action" and it.body and it.body.native and it.from == C.PLAYER_ID then return end
	-- an action that names what an earlier action applied from here made, before the engine answered that one, waits for the answer
	-- (with everything after it, a checksum round too, in arrival order): X.guiParks
	if X.guiParks(it, now) then return end
	if it.kind == "checksum" then
		X.guiChecksum(it)   -- waits (bounded) until the registry mirror holds what the save loaded
		return
	end
	if it.lateSteps and it.lateSteps > 0 then logGui("LATE action " .. it.key .. " for t=" .. it.at .. " applied at " .. now) end
	local ctx = guiCtx(it)
	-- build.result of a build applied during the pause (X.bresWrite): the game stands still, so it is measured right at the engine's answer
	local function bres(how, changes, announced)
		pcall(function()
			local e = X.bresEntry(it, ctx, now, how)
			if e then
				if how.b0 ~= nil then e.b0 = how.b0 end
				local v = X.bresWrite(traceGui, logGui, e, now, changes, announced)
				if it.from == C.PLAYER_ID then X.buildOutcome(it.key, v) end
			end
		end)
	end
	local args, err = L.decodeCommand(it.body, ctx)
	if not args then
		err = L.refusalDetail(err, it.body)
		bres({ verdict = "refused", class = X.refusalClass(err, "decode"), detail = err })
		logGui("action " .. it.key .. " refused: " .. tostring(err))
		send("action_refused", { seq = it.seq, from = it.from, detail = err })
		deliverResult(it.key, nil, false, {})
		if it.heldId and natHeldRefused then pcall(natHeldRefused, it) end
		return
	end
	local spec = L.COMMANDS[it.body.fn]
	local category = actionCategory(it.body, args)
	-- companies: the registry and ledger part runs here on the mirror and is replayed on the simulation state (coToSim)
	local okC, fx = coBefore(ctx, it, spec, args, category, logGui)
	if not okC then
		bres({ verdict = "refused", class = "company", detail = fx })
		logGui("action " .. it.key .. " refused: " .. tostring(fx))
		send("action_refused", { seq = it.seq, from = it.from, detail = fx })
		deliverResult(it.key, nil, false, {})
		coToSim(it)
		if it.heldId and natHeldRefused then pcall(natHeldRefused, it) end
		return
	end
	coEffects(ctx, fx, logGui)
	if spec.virtual then
		logGui("action " .. it.key .. " " .. it.body.fn .. " applied during the pause at t=" .. now)
		deliverResult(it.key, {}, true, {})
		coToSim(it)
		return
	end
	local balance0 = coBalance(ctx)
	-- what the build made is what the engine reports plus what appeared among the components the schema looks at
	local bindComps = spec.bind and L.bindsComp(spec, it.body) or nil
	local bindBefore = {}
	for _, comp in ipairs(bindComps or {}) do bindBefore[comp] = engine.entitiesWith(comp) end
	-- this game's verdict of the build (docs/PROTOCOL.md "Action verdicts"); `injects`: the inject orders written for it
	local verdict = { key = it.key, of = it.key, from = it.from, seq = it.seq }
	local injects = {}
	local isBuildAction = it.body.fn == "makeWorldBuildProposalCmd"
	local bresBefore, bresB0 = nil, nil
	if isBuildAction then
		pcall(function() bresBefore, bresB0 = X.bresSnapshot(), X.refundMoney(ctx) end)
	end
	local function finish(data, success, entities)
		if it.body.fn == "makeWorldBuildProposalCmd" then
			local okD, changes = pcall(function() return bresBefore and X.bresDiff(bresBefore, X.bresSnapshot()) or nil end)
			bres({ verdict = success and "sent" or "refused", class = "engine", detail = (not success) and ("the engine refused it" .. refusalWhy(data)) or nil, b0 = bresB0 },
				okD and changes or nil, "callback")
			verdict.injects = (#injects > 0) and injects or nil
			if success and verdict.terrain then verdict.proof = X.terrainProof(verdict.terrain, it.key, logGui) end
			if success then pcall(X.verdictOf, verdict) else pcall(X.sendVerdict, verdict, false, "the engine refused it" .. refusalWhy(data), "engine") end
		end
		if X.engineHas then X.engineHas[it.key] = nil end
		if spec.creates and success then
			-- the engine's result is userdata (VehicleBuyCommandData...) with the field the game's own window reads
			local okd, e = pcall(function() return data[spec.result] end)
			if not okd then e = nil end
			if e == nil and type(entities) == "table" and entities[1] then e = entities[1][1] end
			if e ~= nil then
				G.bind[it.key] = e
				G.rev[e] = it.key
				toSim("bind", { key = it.key, e = e })
			end
		end
		if spec.bind and success then
			local made, seen = {}, {}
			local function add(e)
				if type(e) == "number" and not seen[e] then
					seen[e] = true
					made[#made + 1] = e
				end
			end
			for _, r in ipairs(type(entities) == "table" and entities or {}) do add(type(r) == "table" and r[1] or r) end
			for _, comp in ipairs(bindComps or {}) do
				for _, e in ipairs(L.newEntities(bindBefore[comp] or {}, engine.entitiesWith(comp))) do add(e) end
			end
			table.sort(made)
			for sub, e in pairs((L.bindMap(spec, it.body, it.key, made))) do
				G.bind[sub] = e
				G.rev[e] = sub
				toSim("bind", { key = sub, e = e })
			end
		end
		-- the game is paused, so nothing else moved the account since the command went out
		coToSim(it, success, success and coAfter(ctx, it, spec, category, balance0, logGui) or nil)
		logGui("action " .. it.key .. " " .. it.body.fn .. " applied during the pause at t=" .. now .. (success and "" or (" REFUSED" .. refusalWhy(data))))
		-- this game's deferred held command took this apply's result (inject_next / complete_next): its world has the apply now
		if it.deferId and X.deferApplied then pcall(X.deferApplied, it.deferId) end
		deliverResult(it.key, data, success, entities)
		-- its keys are bound here and on their way to the simulation half (toSim "bind" above): what waited for them goes on
		X.guiMade(it.key)
	end
	-- the engine's factory may reject what was rebuilt: that refuses the action, like any other refusal
	local okS, errS
	local sending = {}   -- X.handOver: applying = the engine threw (if it threw) after it had the command
	-- from here the engine has the command: its own answer (finish) reaches the callback, even after the session ended (X.sessionOff)
	X.engineHas = X.engineHas or {}
	X.engineHas[it.key] = true
	-- an action that creates entities (a purchase, a line, a build): its keys exist only once the engine answers, a frame or more later
	if spec.creates or bindComps then
		X.guiMaking = X.guiMaking or {}
		X.guiMaking[it.key] = C.clock()
	end
	if it.heldId and natPauseRelease then
		-- the tool's own command is held by the native module: it is released instead of sent (the whole build is that one command)
		okS, errS = pcall(natPauseRelease, it, finish)
	else
		okS, errS = pcall(L.runChained, args, function(a, cb)
			local isBuild = it.body.fn == "makeWorldBuildProposalCmd"
			if isBuild then toSim("replay", { key = it.key }) end
			-- the native orders exactly as at a stamp apply in the simulation half (simApplyAction), right before the command: a paused
			-- build is the originator's exact copy (inject) where the decode chose it, and this game's deferred held command takes its
			-- result. (The pause path used to send the record's donor command alone: another build than on a running game.)
			if a.patch then X.sendPatchOrder(a.patch, it.key) end
			local open = nil   -- the order of this send, closed by send_done once the send returned or threw (X.sendDone)
			if a.inject and a.inject.terrain then verdict.terrain = a.inject.terrain end
			if a.inject then
				local okO, errO, nO = X.sendInjectOrder(a.inject, it.key, it.deferId, isBuild and X.timingText(it, now) or nil, nil)
				if okO and nO then
					injects[#injects + 1] = nO
					open = { key = nO }
				end
				if not okO then error("inject_refused: the inject order could not be written (" .. tostring(errO) .. "); nothing is applied here", 0) end
			elseif it.deferId then
				if X.sendHeldOrder("complete_next", { held = it.deferId }, it.key) then open = { held = it.deferId } end
			end
			local okS2, errS2 = pcall(function()
				local cmd = (O.factory[it.body.fn] or api.cmd[it.body.fn])(unpack(a, 1, a.n))
				X.handOver(O.sendCommand, cmd, cb, sending)
			end)
			X.sendDone(open, it.key)
			if not okS2 then error(errS2, 0) end
		end, finish)
	end
	if not okS then
		X.engineHas[it.key] = nil
		-- the same rule as at a stamp apply (X.handOver): thrown after the engine had the command, the world may be partly built
		local partial = sending.applying == true
		if isBuildAction then bres({ verdict = "refused", class = X.refusalClass(errS, "engine"), detail = tostring(errS), b0 = bresB0 }) end
		traceGui("build.error", string.format("%s type=%s value=%s during_apply=%s path=pause", it.key, type(errS), tostring(errS), tostring(partial)))
		logGui("action " .. it.key .. " refused by the engine: " .. tostring(errS))
		-- refused at its (pause) stamp from this world: this game's verdict, no resync of its own (X.refusalClass)
		X.sendVerdict(verdict, false, tostring(errS), X.refusalClass(errS, "engine"), partial)
		if partial then
			X.partlyBuilt(it.key, errS, "applied during the pause")
		elseif it.body.fn == "makeWorldBuildProposalCmd" then
			pcall(X.tellRefused, verdict, tostring(errS))
		end
		-- no apply on this game: the deferred held command fails now (tool callback fired)
		if it.deferId then X.sendHeldOrder("fail", { id = it.deferId }, it.key) end
		deliverResult(it.key, nil, false, {})
		coToSim(it)
		if it.heldId and natHeldRefused then pcall(natHeldRefused, it) end
		X.guiMade(it.key)
	end
end

-- ---- what an action applied from here makes, before the engine answered (MPFever issue #36, re-designed)
--
-- Applied from this interface (a pause, a slot's record), a command's entities are known only from the engine's answer, a frame or
-- more later (finish above: G.bind, then toSim "bind" for the simulation half). Two things must not overtake that answer:
--   * an action applied from here that names one of its keys (a purchase and the line assignment its callback made, read in one
--     frame on a game that stalled): it waits, and so does everything after it, in arrival order, until the answer came
--     (X.guiParks / X.guiUnpark), at most X.GUI_MAKE_MAX real seconds, after which it is decoded as it is (refused if still unknown);
--   * the simulation: a later action of the pause is applied there at its stamp when the game runs on, and its reference resolves
--     from the simulation half's keys, which the "bind" message brings. So the game does not run on while an answer is outstanding
--     (X.guiHolds, speed 0 in the frame), and the message goes out before the speed command that lets the game step. Every game
--     applies the same actions at the same stamps; only this one waits a frame longer at the pause.
X.GUI_MAKE_MAX = 5.0

-- whether `key` (or a sub-key "<key>#...") belongs to an action applied from here whose engine answer is outstanding
function X.guiIsMaking(key)
	if type(key) ~= "string" then return false end
	for k in pairs(X.guiMaking or {}) do
		if key == k or key:sub(1, #k + 1) == k .. "#" then return true end
	end
	return false
end

-- True (and the item kept) when `it` must wait: something waits already (the order is kept), or it names an outstanding key.
function X.guiParks(it, now)
	X.guiParked = X.guiParked or {}
	local key = nil
	if #X.guiParked == 0 then
		if it.kind ~= "action" or next(X.guiMaking or {}) == nil then return false end
		key = L.pendingRef(it.body, X.guiIsMaking)
		if key == nil then return false end
	end
	X.guiParked[#X.guiParked + 1] = { it = it, now = now, wall = C.clock() }
	logGui("action " .. tostring(it.key) .. " waits for the engine's answer to " .. (key and ("the action that makes " .. key) or "an earlier action")
		.. " (applied from here at t=" .. tostring(now) .. "; " .. #X.guiParked .. " waiting)")
	return true
end

-- The engine answered the action `key` applied from here (or it failed): its keys are bound; what waited goes on.
function X.guiMade(key)
	if X.guiMaking then X.guiMaking[key] = nil end
	local ok, err = pcall(X.guiUnpark)
	if not ok then logGui("the actions waiting for an engine answer failed: " .. tostring(err)) end
end

-- Applies the waiting actions in order up to the first that still names an outstanding key (within the time limit).
function X.guiUnpark()
	local P = X.guiParked
	if not P or #P == 0 then return end
	X.guiParked = {}
	for i, p in ipairs(P) do
		local key = p.it.kind == "action" and L.pendingRef(p.it.body, X.guiIsMaking) or nil
		if key ~= nil and C.clock() - p.wall < X.GUI_MAKE_MAX then
			for j = i, #P do X.guiParked[#X.guiParked + 1] = P[j] end
			return
		end
		if key ~= nil then logGui("action " .. tostring(p.it.key) .. ": no engine answer for " .. key .. " within " .. X.GUI_MAKE_MAX .. " s: applied as it is") end
		guiApplyNow(p.it, p.now)
	end
end

-- Every frame: true while an action applied from here waits for its engine answer (the game then stays at speed 0); an answer
-- outstanding for longer than X.GUI_MAKE_MAX is given up (logged) so that the game cannot stand still for good.
function X.guiHolds()
	local M = X.guiMaking
	if not M or next(M) == nil then return false end
	local clock = C.clock()
	for k, t in pairs(M) do
		if clock - t >= X.GUI_MAKE_MAX then
			M[k] = nil
			logGui("action " .. tostring(k) .. ": no engine answer within " .. X.GUI_MAKE_MAX .. " s; the game goes on without it")
		end
	end
	pcall(X.guiUnpark)
	return next(M) ~= nil
end

-- ---- the pause order (docs/PROTOCOL.md, "The pause order")
--
-- An action made while the session stands at its pause is shipped flagged `pause` and this game keeps it (G.pauseOwn) until the host
-- sends it back. Applied at once from this state in arrival order, the actions of a pause were in a different order on each game (the
-- originator's own first); applied at the pause stamp, they were late on every game, because no game stands AT the pause: speed 0
-- takes effect a few steps late, by a different amount on each game (real run pause3, 2026-10-15: pauseAt 534400, both games stood at
-- 535000, "PAUSE ORDER LATE" and a resync for every paused build). So every game says where it really stands still (X.stopAt, the
-- clock's `stop`), the host stamps the action one step past the furthest of those places and sends it, numbered, to every game, this
-- one included (X.pauseArrived). Every game applies it at that stamp through the simulation like any action, exactly there whatever its
-- own overshoot; a game standing still runs on to it (L.pace, pauseRun), a step or a few: the pause's world moves that little for each
-- build, and the build shows a round trip and that step later. The next stamp waits until every game stands still again at or past it.
-- At the resume the host gives a held one a normal stamp; one that reaches it after the resume gets one too.

-- an item stamped at the session's pause: a pause action
function X.isPauseStamp(at)
	local p = G.session.pauseAt
	return p ~= nil and at == p
end

-- This game is stalled in the host's eyes (core host/stall.rs: its clocks did not come for the stall timeout, the others ran on and the
-- host keeps a promise for it that this game's own stamps may lie before): every action it ships goes the way of the pause order, stamped
-- by the host past every promise and applied by every game at that stamp, this one too (X.routeOwn, X.pauseArrived). Until the host says
-- it has caught up.
function X.selfStalled()
	return L.isStalled(G.session, C.PLAYER_ID)
end

-- An action stamped `at` is stamped by the host: made during the pause, or by this game while it is stalled.
function X.hostStamps(at)
	if X.isPauseStamp(at) then return true end
	return X.selfStalled() and not pausedNow()
end

-- After this game shipped `it`: a pause action (or one of a stalled game, X.hostStamps) waits for the host's stamp; anything else goes
-- the usual way.
function X.routeOwn(it)
	if not it.pause then return route(it) end
	G.pauseOwn = G.pauseOwn or {}
	G.pauseOwn[it.key] = it
	if X.isPauseStamp(it.at) then
		logGui("action " .. it.key .. " made during the pause (t=" .. tostring(it.at) .. "): applied when the host's pause order brings it back")
	else
		logGui("stall: action " .. it.key .. " made while this game catches up (its own stamp " .. tostring(it.at) .. "): applied at the stamp the host gives it")
	end
end

-- Where this game really stands during a pause (ClockMessage.stop: the host stamps the actions of the pause one step past the furthest):
-- its time once it has not moved for X.STOP_SECONDS of real time and X.STOP_FRAMES frames with speed 0 set (the interface sees the time
-- of the last frame the simulation finished; a few frames of one time prove nothing at x1). nil while it may still move.
X.STOP_SECONDS, X.STOP_FRAMES = 0.5, 5
pcall(function() X.STOP_SECONDS = tonumber(C.env("FEVERLINK3_TEST_STOP_SECONDS")) or X.STOP_SECONDS end)   -- test switch
function X.stopAt(now, sp)
	local s = G.session
	local W = G.still or {}
	G.still = W
	if not (s.started and s.pauseAt and now >= s.pauseAt and sp == 0) then
		W.t = nil
		return nil
	end
	if W.t ~= now then
		W.t, W.n, W.since = now, 0, C.clock()
		return nil
	end
	W.n = W.n + 1
	if W.n >= X.STOP_FRAMES and C.clock() - W.since >= X.STOP_SECONDS then return now end
	return nil
end

-- An action of the pause came back from the host, numbered and STAMPED (one step past the furthest place a game stands still at the
-- pause, real run pause3: no game stands at the pause stamp itself): this game's own (it waited in G.pauseOwn) or another player's.
-- Every game applies it at that stamp through the simulation like any action; a game standing still runs on to it (L.pace, pauseRun).
function X.pauseArrived(it, n)
	it.pauseSeq = n
	if it.from == C.PLAYER_ID then
		local own = G.pauseOwn and G.pauseOwn[it.key]
		if not own then
			logGui("pause order #" .. tostring(n) .. ": " .. it.key .. " is this game's own but nothing waits for it here (made before a reload?): applied as received")
			-- a build: its tool's held command went with the load (failed at hold_off); its build.result tells the player (X.buildOutcome)
			if it.body and it.body.fn == "makeWorldBuildProposalCmd" then
				G.builtLate = G.builtLate or {}
				G.builtLate[tostring(it.key)] = true
			end
		else
			G.pauseOwn[it.key] = nil
			own.pauseSeq, own.at = n, it.at
			it = own
		end
	end
	X.pauseStamped(it)
	if it.applied and it.from == C.PLAYER_ID then
		-- this game's own native build of the pause (shipNative): its engine applied it at this stamp already, nothing is applied again
		return
	end
	if it.body and it.body.fn == "pauseSlot" then return X.slotOpen(it) end
	route(it)
end

-- ---- the slot of a build without a record (X.slotHold, in the strict section): every game holds at it until the record came
X.SLOT_MAX = 20.0       -- real seconds a game waits for a slot's record before it goes on without it (its originator gone)
X.SLOT_SHIP_MAX = 5.0   -- real seconds the originator waits for the capture of its released command before it gives the slot up

function X.slotOpen(it)
	local s = G.session
	local own = it.from == C.PLAYER_ID and it.slot or nil
	G.slot = { n = it.pauseSeq, key = it.key, from = it.from, at = it.at, run = it.at + STEP, wall = C.clock(), queue = {}, own = own, still = {} }
	s.holdAt = G.slot.run
	logGui("pause order #" .. tostring(it.pauseSeq) .. ": the place of " .. it.key .. ", a build of player " .. tostring(it.from) .. " applied where every game stands: "
		.. (own and "this game releases it once every game stands still at or past t=" .. C.integerText(G.slot.run) or "this game holds at t=" .. C.integerText(G.slot.run) .. " until its record came"))
end

-- An item of the pause order numbered after an open slot waits for the slot's record (in arrival order). One of the slot's own flush still
-- counts for the flush's run-on (X.pauseOrderSeen: every game runs on to the slot's stamp and a step, where the slot is held).
function X.slotQueued(kind, rec, p)
	local S = G.slot
	if not (S and type(p) == "table" and type(p.pauseSeq) == "number" and p.pauseSeq > S.n) then return false end
	if type(p.stamp) == "number" and p.stamp <= S.at then pcall(X.pauseOrderSeen, p) end
	S.queue[#S.queue + 1] = { kind = kind, rec = rec, p = p }
	logGui("pause order #" .. tostring(p.pauseSeq) .. " (" .. kind .. ") waits for the record of " .. S.key .. " (pause order #" .. S.n .. ")")
	return true
end

-- The record of the open slot arrived (another player's build): this game runs on to its stamp, where the simulation applies it.
function X.slotContent(it)
	local S = G.slot
	local now = engine.gameTime()
	if type(it.at) == "number" and it.at < now then
		logGui("SLOT LATE: the record of " .. S.key .. " (" .. it.key .. ") is stamped " .. C.integerText(it.at) .. ", this game stands at " .. C.integerText(now))
	end
	-- standing at its stamp (held there): applied from here now, as at a pause, also when the session resumed meanwhile (the simulation
	-- has run that step already: queued, it would apply a step late)
	X.slotClose("its record " .. it.key .. " came, stamped " .. C.integerText(it.at), it.at, function()
		if it.at == now then guiApplyNow(it, now) else route(it) end
	end)
end

-- The slot is over (its record applied here, or given up): the hold goes and the waiting numbers follow, none before the step after `after`
-- (the time the build was applied: every game applies them after it, in their order; the same time on every game, so the same restamp).
function X.slotClose(why, after, first)
	local S = G.slot
	if not S then return end
	G.slot = nil
	local s = G.session
	s.holdAt = nil
	after = after or S.run
	if s.pauseAt ~= nil then s.pauseRun = math.max(s.pauseRun or 0, after + STEP) end
	logGui("pause order #" .. S.n .. " (" .. S.key .. ") closed: " .. tostring(why) .. "; " .. #S.queue .. " later number(s) follow, none before " .. C.integerText(after + STEP))
	if first then first() end
	for i, q in ipairs(S.queue) do
		if G.slot then
			-- a later slot opened: the rest waits for it
			for j = i, #S.queue do G.slot.queue[#G.slot.queue + 1] = S.queue[j] end
			return
		end
		-- one step after the build, on every game the same (paused or resumed meanwhile): a game standing at `after` has simulated that step
		if type(q.p.stamp) == "number" and q.p.stamp <= after then q.p.stamp = after + STEP end
		q.p.pauseUpto = nil   -- each counts for itself now (its flush was read)
		local ok, err = pcall(X.slotReplay, q.kind, q.rec, q.p)
		if not ok then logGui("pause order #" .. tostring(q.p.pauseSeq) .. " after the slot failed: " .. tostring(err)) end
	end
end

-- Real time and frames a game time `t` has not changed in `w` (as X.stopAt): true once it stands.
function X.slotStill(w, t)
	if w.t ~= t then
		w.t, w.n, w.since = t, 0, C.clock()
		return false
	end
	w.n = w.n + 1
	return w.n >= X.STOP_FRAMES and C.clock() - w.since >= X.STOP_SECONDS
end

-- every frame while a slot is open: the originator's release (step 3 of X.slotHold), and the time limits
function X.slotPump()
	local S = G.slot
	if not S then return end
	local s, now, clock = G.session, engine.gameTime(), C.clock()
	if S.own and not S.released then
		-- this game and every other one stand still, each at or past the slot's run-on; this game at or past every other one
		local ready, ahead = now >= S.run and X.slotStill(S.still, now), nil
		for id, pc in pairs(G.peers) do
			if type(pc) == "table" and type(pc.t) == "number" then
				S.still[id] = S.still[id] or {}
				if not (X.slotStill(S.still[id], pc.t) and pc.t >= S.run) then ready = false end
				if pc.t > now then ahead = math.max(ahead or 0, pc.t) end
			end
		end
		if ahead then
			-- a game stands further on (its stop at the pause was past the slot): this one runs on to it first
			local to = math.ceil(ahead / STEP) * STEP
			s.holdAt = math.max(s.holdAt or to, to)
			if s.pauseAt ~= nil then s.pauseRun = math.max(s.pauseRun or 0, to) end
			return
		end
		if ready then X.slotRelease(S) end
		return
	end
	if S.own and S.released and not S.shipped and clock - S.relWall > X.SLOT_SHIP_MAX then
		X.slotGiveUp(S, "its released command was not captured within " .. X.SLOT_SHIP_MAX .. " s")
	elseif not S.own and clock - S.wall > X.SLOT_MAX then
		X.slotClose("its record did not come within " .. X.SLOT_MAX .. " s (its originator gone?): nothing of it is applied here", nil, nil)
	end
end

-- The originator has no record for its slot (its capture failed or nothing came): the others are told, and every game goes on.
function X.slotGiveUp(S, why)
	S.shipped = true
	send("build_veto", { seq = tonumber(S.key:match(":(%d+)$")), owner = C.PLAYER_ID, why = "no record of the build at its place in the pause order: " .. tostring(why), name = C.PLAYER_NAME })
	X.slotClose("given up by its originator: " .. tostring(why), S.released, nil)
end

-- An item of the host's pause order was read (an action or a commit; also one applied nowhere here, which still belongs to its flush):
-- the run-on to its stamp and one step on (pauseRun): the simulation finds what the action created (a bought vehicle shows a step after
-- its command) and keys it, so the window's callback runs and a later action of the pause can name it.
--   * only once the whole flush is read: every item of one flush has one stamp and names the number of the flush's last item
--     (`pauseUpto`); running on after the first would pass the stamp of a later one still on its way (mock with jitter, 2026-10-18,
--     seed 29: two of one flush read in different frames, the second arrived a step after its stamp). Without `pauseUpto` (an older
--     host) each item counts as a flush of its own, as before;
--   * only while the pause lasts: one the host stamped after the resume has a normal stamp, not one past every game, and the run-on it
--     would allow under a peer's last (pause) promise (L.barrier) let a game pass the stamp of the next such item (stamped 45000,
--     arrived at 45200).
function X.pauseOrderSeen(p)
	if type(p) ~= "table" or type(p.pauseSeq) ~= "number" or type(p.stamp) ~= "number" then return end
	local s = G.session
	if s.pauseAt ~= nil then X.pauseRunWant = math.max(X.pauseRunWant or 0, p.stamp + STEP) end
	local upto = tonumber(p.pauseUpto)
	if upto == nil or p.pauseSeq >= upto then
		if X.pauseRunWant and s.pauseAt ~= nil then s.pauseRun = math.max(s.pauseRun or 0, X.pauseRunWant) end
		X.pauseRunWant = nil
	end
end

-- The host's stamp of an action of the pause: the native module's bookkeeping of this game's own held build follows it, and the game
-- runs on to it while paused.
function X.pauseStamped(it)
	local s = G.session
	if it.pauseSeq then
		-- one ordered link from the host: a number that is not the next one would mean a lost line
		local want = (G.pauseLast or (it.pauseSeq - 1)) + 1
		if it.pauseSeq ~= want then logGui("PAUSE ORDER GAP: expected #" .. tostring(want) .. ", got #" .. tostring(it.pauseSeq) .. " (" .. tostring(it.key) .. ")") end
		G.pauseLast = it.pauseSeq
	end
	-- (the run-on to its stamp: X.pauseOrderSeen, which the handlers call for every item of the pause order)
	if it.body and it.body.fn == "makeWorldBuildProposalCmd" then noteBuild(it.key, it.at) end
	local N = G.nat
	if N and it.from == C.PLAYER_ID then
		-- the real stamp is known now: the deferred completion (natTick) and the watchdog may count from it
		local d = it.deferId and N.deferred[it.deferId]
		if d then d.at, d.waitStamp = it.at, nil end
		local h = N.heldByKey[it.key]
		if h then h.at, h.waitStamp = it.at, nil end
	end
	logGui("pause order #" .. tostring(it.pauseSeq) .. ": " .. tostring(it.key) .. " stamped " .. tostring(it.at) .. " by the host (pause " .. tostring(s.pauseAt or s.lastPauseAt) .. ", this game at t=" .. tostring(engine.gameTime()) .. ")")
end

function route(it)
	local now = engine.gameTime()
	-- a checksum round at a pause is stamped where the furthest game stands still: a game behind runs on to it (pauseRun). An item at or
	-- before the time this game stands at, at a pause, is applied from here in arrival order (a checksum, or the barrier breached)
	local s = G.session
	if it.kind == "checksum" and s.pauseAt and type(it.at) == "number" and it.at > now then s.pauseRun = math.max(s.pauseRun or 0, it.at) end
	if L.applyNow(s, now, it) then
		if it.kind == "checksum" or it.at == now then
			guiApplyNow(it, now)
		else
			-- an action stamped before the time this game stands at (the barrier breached): applied by the simulation, late (a fork,
			-- repaired as every late apply)
			logGui("PAUSE LATE: action " .. tostring(it.key) .. " for t=" .. tostring(it.at) .. " reached this game standing at t=" .. tostring(now))
			toSim("queue", it)
		end
		return
	end
	if it.kind == "action" and type(it.at) == "number" and it.at <= now and not (it.body and it.body.native and it.from == C.PLAYER_ID) then
		-- the input barrier should make this impossible: the simulation is at or past the stamp already, so it applies the item late
		-- (a fork; the simulation half logs LATE and this half asks for the repair)
		logGui("INPUT BARRIER BREACHED: action " .. tostring(it.key) .. " stamped " .. C.integerText(it.at) .. " arrived at t=" .. C.integerText(now)
			.. " (promise of player " .. tostring(it.from) .. ": " .. tostring(G.peers[it.from] and G.peers[it.from].safe) .. ")")
	end
	toSim("queue", it)
end

-- A build the player made with a native tool, captured by the simulation half (simCaptureBuild): stamped and shipped
-- like any other action. The engine applied it already, so it is not queued here; the other games rebuild it at the stamp.
-- (TpF2 Multiplayer stamps past the fastest other clock for the same reason: a game that has passed the stamp
-- applies the action late, and the worlds fork.)
-- extraSteps: a held strict build is stamped this many steps later, so that a veto (a game that cannot rebuild it says so as soon
-- as it has the record) is back at the originator and at every other game before the stamp. Not at a pause (the stamp is then
-- the pause time: the build is applied during the pause, a veto cannot come first).
-- marginSteps: the measured stamp margin of a build every game applies at its stamp (L.marginExtra), counted from the fastest clock
-- plus the lead like the rest, and like the rest held to this game's promise: the stamp is the later of the two, not the promise plus
-- the margin. Until 2026-10-18 the margin was added after the promise, so every such build paid both (friend test 2026-10-16: the
-- guest's builds 10 steps past its clock at x1, its promise 6 plus the default margin 4; 1.65 s from click to its own apply). The
-- promise alone keeps every receiver marginTarget steps before the stamp (the input barrier); the margin still adds what a receiver
-- measured beyond that. Returns the stamp and, with marginSteps, the extra it has past the fastest clock plus the lead (the receivers
-- measure their slack against that, L.marginSample).
function stampAfterFastest(now, extraSteps, marginSteps)
	local base = now
	local peers = false
	for _, pc in pairs(G.peers) do
		if type(pc) == "table" and type(pc.t) == "number" then
			peers = true
			if pc.t > base then base = pc.t end
		end
	end
	local at = L.stampFor(G.session, base)
	local plain = at
	if extraSteps and peers and not (G.session.pauseAt and base >= G.session.pauseAt) then
		-- the veto travels from a peer to the host to the originator: about a whole round trip, while the lead covers half of one plus
		-- fixed handling. The host's lead grows with the slowest round trip, so the extra grows with it (half the lead, one step at least).
		at = at + math.max(extraSteps, math.floor(L.leadOf(G.session) / STEP / 2)) * STEP
	end
	if marginSteps then at = at + marginSteps * STEP end
	if X.selfStalled() and not (G.session.pauseAt and base >= G.session.pauseAt) then
		-- stalled (host/stall.rs): the host keeps a promise for this game that follows the others' promises, and refuses on every game an
		-- action stamped before it. Most actions are stamped by the host (X.hostStamps); one that cannot be (a build held until its stamp)
		-- lies past the furthest promise known here plus a lead, the time that promise may have moved on meanwhile
		local far = nil
		for _, pc in pairs(G.peers) do
			if type(pc) == "table" and type(pc.safe) == "number" and not pc.pause and (far == nil or pc.safe > far) then far = pc.safe end
		end
		if far then at = math.max(at, L.roundUpStep(far + L.leadOf(G.session))) end
	end
	-- the input barrier: never before what this game promised its peers (L.clampStamp); at a pause, the pause stop is promised first
	X.promisePause(now)
	at = L.clampStamp(G.session, now, at, G.promiseFloor)
	return at, marginSteps and math.floor((at - plain) / STEP + 0.5) or nil
end

-- ---- a held build in a PAUSED game
--
-- A paused game applies every action at once (the stamp is the pause time), so the veto design (peers dry-run the record while the
-- originator still holds its command) has no time to work: the originator released at once and a peer's veto came too late (real
-- run 2026-10-11: a road the host could not build was built on the guest's game). In a paused game the record is therefore shipped
-- with `commit = true`: the peers dry-run it and PARK it (nothing is applied), the originator keeps its command held for a
-- real-time grace, and only then sends `build_commit {seq, owner, stamp}`; every game applies the build on the commit. A veto
-- during the grace drops it everywhere; a peer that never answers casts no veto (silence is yes); a peer that never gets the
-- commit (its originator vanished) drops the parked record after PARK_MAX seconds.
local GRACE_MIN, GRACE_MAX, GRACE_DRY_MS, GRACE_MARGIN_MS = 250, 1500, 100, 100
PARK_MAX = 10.0       -- seconds a peer keeps a parked record without its commit

function pausedNow()
	local s = G.session
	return s.pauseAt ~= nil and engine.gameTime() >= s.pauseAt
end

-- The grace in real milliseconds: two round trips' worth (the record out, the veto back) plus a dry run and a margin. The round
-- trip is read from the host's lead (lead_for: half a round trip plus handling, in steps), which the host derives from its
-- measurement of the slowest connection.
function graceMs()
	local steps = math.floor(L.leadOf(G.session) / STEP + 0.5)
	local rtt = math.max(0, 2000 * ((steps - 2) / 6 - 0.06))
	local ms = math.max(GRACE_MIN, math.min(GRACE_MAX, math.floor(2 * rtt + GRACE_DRY_MS + GRACE_MARGIN_MS)))
	return ms, rtt
end

-- ---- veto-only validation of a held strict build, on the games that did not make it
--
-- A strict build is stamped and shipped with `held = true` in its record: its originator keeps the tool's command held until the
-- stamp. Every other game dry-runs the record as soon as it has it, long before the stamp (decodeCommand: every node, segment and
-- construction it refers to must resolve and the engine's command factory must accept the rebuilt proposal; the engine's own
-- errorState is read with makeProposalData where the game lets a script do that). Silence means "I can build it". A game that
-- cannot sends ONE message, `build_veto`, with the first cause; the host relays it to everybody: the originator cancels the held
-- command (nothing is applied on its world) and every game drops the queued replay, so no world runs ahead of a refusal. A
-- game that is slow, paused or gone casts no veto. The dry run waits for earlier builds that have not landed yet (they create
-- what this one may refer to); if they do not land before the stamp it is skipped (silence).
VETO_STEPS = 1          -- extra steps of lead for a held build (the veto's way back)
-- A build that adds or removes constructions takes the other games much longer to dry-run (every building has to be found, the engine's
-- factory runs on a big proposal; real session 2026-10-12: a veto 2 s after the build was released after a 626 ms grace): a longer
-- window, so that a veto reaches the originator before it releases its command.
CONSTRUCTION_VETO_STEPS = 12   -- extra steps of lead
CONSTRUCTION_GRACE_MS = 4000   -- a paused game waits at least this long
local VETO_MARGIN = 1         -- a dry run later than this many steps before the stamp is useless and skipped

function noteBuild(key, at)
	G.buildAts[key] = at
	local n = 0
	for _ in pairs(G.buildAts) do n = n + 1 end
	if n > 40 then
		local now = engine.gameTime()
		for k, a in pairs(G.buildAts) do if a < now - 20 * STEP then G.buildAts[k] = nil end end
	end
end

-- builds that stamp earlier than `it` and have not landed on this game yet
function pendingBefore(it, now)
	local out = nil
	for k, a in pairs(G.buildAts) do
		if k ~= it.key and a <= it.at and now < a + STEP then out = math.max(out or 0, a) end
	end
	return out
end

-- A reason that proves "this game cannot build it" (a reference that does not resolve, a factory refusal, the engine's errorState);
-- anything else (an entity key not bound yet, a fault in the check) proves nothing and casts no veto.
local function isVetoReason(why)
	why = tostring(why or "")
	return why:find("^unresolved ") or why:find("^ambiguous ") or why:find("command factory refused", 1, true)
		or why:find("^engine:", 1, true) or why:find("cannot set ", 1, true)
end

-- the engine's own verdict on the rebuilt proposal, without applying it (what a tool's preview reads); nil when the game does not
-- let a script ask (the first call that throws switches this off and says so once)
local function proposalVerdict(args)
	if G.noProposalData then return nil end
	local critical, msgs, _, coll = engineVerdict(args)
	if critical == nil then
		if msgs ~= "no errorState" then
			G.noProposalData = true
			logGui("veto check: the engine's errorState cannot be read from a script here (" .. tostring(msgs) .. "); only the references and the command factory are checked")
		end
		return nil
	end
	if critical or (args[3] ~= true and #msgs > 0) then
		return "engine: " .. (critical and "critical error " or "errors ") .. table.concat(msgs, ", ") .. ((coll and #coll > 0) and (" (colliding: " .. table.concat(coll, "; ") .. ")") or "")
	end
	return nil
end

-- true: this game can build it (or cannot tell); false and why: it cannot
local function dryRun(it)
	local ok, args, why = pcall(L.decodeCommand, it.body, guiCtx(it))
	if not ok then args, why = nil, tostring(args) end
	if not args then
		why = L.refusalDetail(why, it.body)
		if isVetoReason(why) then return false, tostring(why) end
		return true, "inconclusive: " .. tostring(why)
	end
	-- a build that the native module patches at the Add (frozen nodes) is judged by the engine before the patch: the unpatched proposal always collides with the construction's own street piece (b29), so that verdict is no veto
	local bad = nil
	if not (type(args) == "table" and (args.patch or args.inject)) then bad = proposalVerdict(args) end
	-- the engine's verdict and the form that was judged, for every build that adds or removes constructions (real session 2026-10-13:
	-- an underground station entrance was refused with "Construction Not Possible" and the log did not say which plan or form)
	if args.plan == "construction" or args.plan == "withStreet" or args.plan == "combined" or args.plan == "bulldoze" then
		traceGui("build.verdict", string.format("%s plan=%s form=%s %s ie=%s pi=%s", tostring(it.key), tostring(args.plan), tostring(args.form),
			bad and ("REFUSED " .. bad) or "accepted", tostring(args[3] == true), tostring(args[4] == true)))
	end
	if bad then return false, bad end
	return true
end

local function sendVeto(it, why)
	G.vetoed[it.key] = why
	toSim("drop", { key = it.key, why = why, own = true })
	send("build_veto", { seq = it.seq, owner = it.from, why = tostring(why):sub(1, 300), name = C.PLAYER_NAME })
	-- (the notice is the originator's, once per vetoed build: it alone knows whether its build was still held; see H.build_veto)
	logGui("VETO of " .. it.key .. ": this game cannot build it (" .. tostring(why) .. "); the build is dropped on this game")
end

function vetoCheck(it, now)
	local ok, why = dryRun(it)
	if ok then
		if why then logGui("dry run of " .. it.key .. ": " .. why) end
		return
	end
	sendVeto(it, why)
end

-- every frame: dry runs that waited for earlier builds
function dryPump()
	if next(G.dry) == nil then return end
	local now = engine.gameTime()
	for key, it in pairs(G.dry) do
		if now >= it.at - VETO_MARGIN * STEP then
			G.dry[key] = nil
			logGui("dry run of " .. key .. " skipped: the builds before it had not landed " .. VETO_MARGIN .. " step(s) before its stamp (no veto)")
		elseif not pendingBefore(it, now) then
			G.dry[key] = nil
			vetoCheck(it, now)
		end
	end
end

-- The permission category of a captured build record: a record that adds nothing only removes (bulldoze).
-- (buildCategory and actionCategory are defined above the simulation half)

function shipNative(o)
	if o.unbuildable then
		-- A build this game's engine applied (released or never held) whose record failed this game's own self-check (simCaptureBuild):
		-- every other game would refuse it the same way, so it is not shipped. It cannot be taken back here, so this world now has a build
		-- no other game can make: the host is asked for the repair at once (instead of after the other games' refusals) and the player told.
		local why = tostring(o.unbuildable)
		local S = G.slot
		if S and S.own and S.released and not S.shipped then pcall(X.slotGiveUp, S, "its record failed its own self-check") end
		logGui("native build captured at t=" .. tostring(o.at) .. " NOT shipped: its own self-check failed (" .. why .. "); this game has it, no other game can build it: asking the host for a resync")
		send("replay_failed", { detail = "a build applied on its originator only (its record cannot be rebuilt, not shipped): " .. why:sub(1, 160) })
		pcall(vetoChat, "unbuildable:" .. tostring(o.n) .. ":" .. tostring(C.clock()), "build_not_shared_sync")
		return false, "its own self-check failed"
	end
	local body = C.parse(o.body)
	if type(body) ~= "table" or type(body.args) ~= "table" then return false, "unreadable record" end
	-- another mod's command captured on the host (simForeign) is any command kind: its category is counted as a player's would be
	local category = o.foreign and L.categoryOf(body.fn, body.args) or buildCategory(body)
	local now = engine.gameTime()
	local S = G.slot
	if S and S.own and S.released and not S.shipped and not o.script then
		-- the record of this game's build released at its place in the pause order (X.slotHold): stamped where this game applied it, which
		-- every other game stands at or before and does not pass until it has the record (no promise is broken: they wait at the slot)
		S.shipped = true
		G.seq = G.seq + 1
		body.pauseSlot = S.key
		local text = C.serialize(body)
		send("action", { stamp = o.at, seq = G.seq, body = text, category = category })
		G.nativeShipped = (G.nativeShipped or 0) + 1
		logGui("native build " .. L.itemKey(C.PLAYER_ID, G.seq) .. " captured at t=" .. tostring(o.at) .. " at its place " .. S.key .. " in the pause order: stamped "
			.. tostring(o.at) .. " (now " .. now .. ", " .. #text .. " bytes); every other game applies it there")
		recordOriginator(body, G.seq, o.at, category)
		pcall(toSim, "nativebind", { n = o.n, key = L.itemKey(C.PLAYER_ID, G.seq) })
		X.slotClose("this game's build was applied here at t=" .. tostring(o.at) .. " and its record sent", o.at, nil)
		return true
	end
	G.seq = G.seq + 1
	local at = stampAfterFastest(now)
	-- a script's command made during the pause is applied in the host's pause order (X.routeOwn)
	local pause = (o.script and X.hostStamps(at)) or nil
	local applied = nil
	if not o.script and X.isPauseStamp(at) then
		-- a native build no hold caught (no slot, X.slotHold), made during the pause: this game's engine applied it where this game
		-- stands still, so that is its stamp, and it goes through the host's pause order like every other item of the pause, its stamp
		-- kept (`applied`): numbered in the one order and relayed after every stamp given before it, every other game runs on to it
		-- (pauseRun) or stands there and applies it at that step. Until 2026-10-23 it was stamped at the pause stamp and relayed outside
		-- the order (real session 2026-10-23: 1:399, a crossing's node configuration, stamped 50775200 while every game stood at 50885800
		-- after the pause order's run-ons: "PAUSE LATE ... 553 steps late" on the host, a resync)
		at, pause, applied = (type(o.at) == "number" and o.at) or now, true, true
		local key = L.itemKey(C.PLAYER_ID, G.seq)
		G.pauseOwn = G.pauseOwn or {}
		G.pauseOwn[key] = { kind = "action", at = at, from = C.PLAYER_ID, seq = G.seq, body = body, key = key, pause = true, applied = true }
	end
	send("action", { stamp = at, seq = G.seq, body = o.body, category = category, pause = pause, applied = applied })
	G.nativeShipped = (G.nativeShipped or 0) + 1
	if o.script then
		-- a script's build command (simScriptBuild): not applied anywhere yet, so this game queues it like every other game does
		local it = { kind = "action", at = at, from = C.PLAYER_ID, seq = G.seq, body = body, key = L.itemKey(C.PLAYER_ID, G.seq), scriptOut = o.n, pause = pause }
		if o.foreign then
			logGui("action " .. it.key .. " " .. tostring(body.fn) .. " (another mod's command, mod=" .. tostring(o.foreign) .. ") stamped " .. at .. " (now " .. now .. "): applied at the stamp on every game, this one too")
		else
			logGui("native build " .. it.key .. " (a script's command) stamped " .. at .. " (now " .. now .. ", " .. #o.body .. " bytes): applied at the stamp on this game too")
		end
		X.routeOwn(it)
		return true
	end
	logGui("native build " .. L.itemKey(C.PLAYER_ID, G.seq) .. " captured at t=" .. tostring(o.at) .. ", stamped " .. at .. " (now " .. now .. ", " .. #o.body .. " bytes)"
		.. (applied and ": made during the pause, applied here already; numbered in the host's pause order at this stamp, every other game applies it there" or ""))
	recordOriginator(body, G.seq, at, category)
	pcall(toSim, "nativebind", { n = o.n, key = L.itemKey(C.PLAYER_ID, G.seq) })
	return true
end

-- The answer to the UI-state hook about a window command it holds (ui_in.log, read by Hook.poll): applied or refused at the
-- stamp, the entity it created and its key. The window's callback runs on that answer. `tm`: this half's part of the command's
-- timing line (shipUiCommand: when the record was read and stamped, the stamp, its place in its order); the answer's own time is added.
function uiReply(id, key, data, success, entities, detail, tm)
	if not G.uiInbox then G.uiInbox = C.newWriter(C.path("ui_in.log"), C.PLAYER_ID) end
	local e = nil
	if success and key then e = G.bind[key] end
	if e == nil and success and data ~= nil then
		local fn = G.uiFn and G.uiFn[key]
		local spec = fn and L.COMMANDS[fn]
		if spec and spec.result then pcall(function() e = data[spec.result] end) end
		if type(e) ~= "number" then e = nil end
	end
	local okE, list = pcall(C.plain, entities)
	if G.uiFn and key then G.uiFn[key] = nil end
	if type(tm) == "table" then tm.a = C.clock() end
	local ok, err = C.writerSend(G.uiInbox, "result", { id = id, key = key, success = success == true, e = e,
		entities = (okE and type(list) == "table" and next(list) ~= nil) and list or nil, detail = detail, tm = type(tm) == "table" and tm or nil })
	logGui("window command #" .. tostring(id) .. " (" .. tostring(key) .. ") " .. (success and "applied" or "REFUSED") .. " at its stamp"
		.. (e and (", created " .. tostring(e)) or "") .. ": answered to the window" .. (ok and "" or (" - CANNOT WRITE ui_in.log: " .. tostring(err))))
end

-- A command a window issued, recorded and HELD by the UI-state hook (feverlink3_uihook.lua): stamped, shipped and queued here
-- like every other action (shipLocal), so this game applies it at the stamp through the same queue as the other games. Applied at
-- the click instead, it took entity ids from the engine's free-id FIFO at another step than everywhere else, and every later id
-- could differ (the rule at the deferred completion of builds). The window's callback runs on uiReply, after the stamp apply.
--
-- `ps`: the records of one ORDER, in the order the window issued them: the commands one handler of the window issued in one go (the
-- hook's group, serviceWindow). Friend test 2026-10-16: a clone of two vehicles is two purchases and then two line assignments. An order
-- is stamped once (every command of it applies in one step on every game, in this order) and is fail-closed as a whole: when any of
-- it cannot be shipped, nothing of it leaves (false and the reason; the caller answers every command of it with the failure). A later
-- command of an order may name an entity an earlier one creates (its "ui:" placeholder becomes that command's key).
function shipUiCommand(ps)
	local now, tRead = engine.gameTime(), C.clock()
	local n, seq0, made, old = #ps, G.seq, {}, {}
	local note = L.recentNotes(G.notes, now)
	for i, p in ipairs(ps) do
		local body = C.parse(p.body)
		local why = nil
		if type(body) ~= "table" or type(body.args) ~= "table" then why = "unreadable record" end
		if not why then
			body.native = nil   -- nothing of it was applied here
			old[#old + 1] = { id = p.id, seq = G.uiSeq[p.id] }
			G.uiSeq[p.id] = seq0 + i
			-- entities a window command made a moment ago travel under the key of the record that made them (L.rewriteUiKeys)
			if p.body:find("ui:", 1, true) then
				local okR, missing = L.rewriteUiKeys(body, function(id) return G.uiSeq[id] end, C.PLAYER_ID)
				if not okR then why = "it names the entity window command #" .. tostring(missing) .. " made, which was never shipped" end
			end
		end
		if why then
			for j = #old, 1, -1 do G.uiSeq[old[j].id] = old[j].seq end
			return false, (n > 1 and ("command #" .. tostring(p.id) .. " of an order of " .. n .. ": ") or "") .. why
		end
		made[i] = { p = p, body = body }
	end
	local at = stampAfterFastest(now)
	local pause = X.hostStamps(at) or nil
	local wall = X.wallNow()
	local first = L.itemKey(C.PLAYER_ID, seq0 + 1)
	-- every text first: nothing of the order is sent before all of it is ready
	for i, m in ipairs(made) do
		local p, body = m.p, m.body
		local t = type(p.t) == "table" and p.t or {}
		local click = type(t.c) == "number" and (wall - (tRead - t.c) * 1000) or 0
		-- `ui`: a window command and its place in its order; `tm`: the start of the timing line every game writes at its apply (X.timingText)
		body.ui = { id = p.id, grp = p.grp, i = i, n = n, first = first }
		body.tm = string.format("ui=#%s grp=%s order=%s(%d/%d) click=%.0f@g%s read=%.0f", tostring(p.id), tostring(p.grp), first, i, n, click, C.integerText(now), wall)
		if #note > 0 then body.diverged = note end
		m.text = C.serialize(body)
	end
	G.seq = seq0 + n
	G.uiFn = G.uiFn or {}
	for i, m in ipairs(made) do
		local p, body, seq = m.p, m.body, seq0 + i
		local category = type(p.category) == "string" and p.category or "other"
		local it = { kind = "action", at = at, from = C.PLAYER_ID, seq = seq, body = body, key = L.itemKey(C.PLAYER_ID, seq), pause = pause, tRecv = wall, gRecv = now }
		send("action", { stamp = at, seq = seq, body = m.text, category = category, pause = pause })
		G.uiFn[it.key] = body.fn
		local tm = { r = tRead, s = C.clock(), at = at, now = now, i = i, n = n }
		G.pending[it.key] = function(data, success, entities) uiReply(p.id, it.key, data, success, entities, nil, tm) end
		logGui("window command " .. tostring(body.fn) .. " #" .. tostring(p.id) .. " " .. it.key .. " stamped " .. at .. " (now " .. now .. "): held in the window, applied here at the stamp too"
			.. (n > 1 and (" (" .. i .. " of " .. n .. " of order " .. first .. ")") or ""))
		X.routeOwn(it)
	end
	logGui(string.format("window order %s (group %s): %d command(s) stamped once at %s (now %s), read %.0f ms after the first click", first, tostring(ps[1].grp), n, C.integerText(at), C.integerText(now),
		type(ps[1].t) == "table" and type(ps[1].t.c) == "number" and (tRead - ps[1].t.c) * 1000 or -1))
	return true
end

-- A command this player issued (held by the wrapped sendCommand): encode, stamp, ship, queue locally.
function shipLocal(tag, cb)
	local body, err = L.encodeCommand(tag.name, tag.args, { plain = C.plain, keyOf = function(e) return L.keyFor(G.rev, e) end })
	if not body then return false, err end
	local now = engine.gameTime()
	local note = L.recentNotes(G.notes, now)
	if #note > 0 then body.diverged = note end
	G.seq = G.seq + 1
	local at = stampAfterFastest(now)
	local it = { kind = "action", at = at, from = C.PLAYER_ID, seq = G.seq, body = body, key = L.itemKey(C.PLAYER_ID, G.seq), pause = X.hostStamps(at) or nil }
	-- the category rides along so the host can mark an action its sender had no grant for (docs/PROTOCOL.md, Permissions)
	send("action", { stamp = at, seq = G.seq, body = C.serialize(body), category = L.categoryOf(tag.name, tag.args), pause = it.pause })
	if cb then
		G.pending[it.key] = cb
		-- a build's callback gets its command data (deliverResult): the script's own proposal and context
		if tag.name == "makeWorldBuildProposalCmd" then
			G.pendingArgs = G.pendingArgs or {}
			G.pendingArgs[it.key] = tag.args
		end
	end
	logGui("action " .. it.key .. " " .. tag.name .. " stamped " .. at .. " (now " .. now .. ")")
	X.routeOwn(it)
	return true
end
end -- section: applying items, the veto, shipping

-- ================================================================== strict mode: the native module holds a tool's build
--
-- Credit: holding the command a construction tool queued, cancelling it, and letting EVERY game, the originator's included,
-- apply the build at the stamp ("strict" replication) follows TpF2 Multiplayer (MIT). The module is native/ (docs/NATIVE_LINK.md);
-- this is the mod's half of that contract (section 7 there). Everything here is guarded and fails open: when strict mode is not
-- available, or anything in it is in doubt, the tool's build goes through natively and the simulation half ships it the old
-- way (replay on peers, simCaptureBuild).
--
-- One build with a native tool:
--   1. the tool event builder.proposalCreate (the tool preview) reaches guiHandleEvent. natCapture encodes the proposal (Build.captureToolBuild)
--      and keeps the result in N.caps for 2 s (a failed capture is kept too, as an entry without a body, so that the order of
--      captures and `held` events stays aligned);
--   2. the module holds the tool's command at CommandList::Add and writes `held {id}` to native_events.log. natPair matches it
--      with the oldest capture. If `held` is read first, it waits (N.parked) for the capture for a short grace;
--   3. natDecide: the player's permission, a check that this game could rebuild the record, then the action is shipped with a
--      stamp exactly as shipLocal does and the command STAYS HELD (cancelling it left the originator's tool dead: a cancelled
--      command never gives the tool its "applied" callback). The item is routed to this game's own queue with a local heldId.
--      At the stamp the simulation half (simApplyAction) does what every game does (ledger, snapshot, binders that key the
--      entities) except sending, and reports a "#release" result; natRelease then writes the replay mark, `release` and the
--      trigger, so the engine applies the tool's own command and the tool lives on. Every OTHER game replays the build at the
--      stamp. No recordOriginator: the normal apply path books the company ledger;
--   4. a held command without a tool capture (no preview, or a stop read after the build) is released: `release {id}` lets the native
--      build go through and the simulation half's onPreBuildProposal capture ships it the old way (after its own self-check);
--   5. a held command that fails a check here (its capture failed, its record cannot be rebuilt here or is too big, the action cannot be
--      written, a fault in the verdict) is REFUSED (X.natRefuseHeld): failed, nothing is shipped and nothing is applied on any game.
-- A capture is never shipped without its `held`, and a held command is either cancelled-and-shipped, released or refused, never two.
--
-- Switching it on (once per script start; a reloaded script starts again from the file, which keeps `ready`):
--   native_events.log appears -> its records are read; the first batch is history (state, never news)
--   `ready` with "command_list_add" in hooks (or no `ready` at all in a trimmed file) -> ping, wait for the `pong`
--   `pong` -> hold_on {kinds = {"build_proposal"}} -> `holding {on = true}` -> strict mode is on
--   while it is on: a ping at least every NAT_PING seconds, whatever else is written (the module switches holding off after 10 s of
--   silence; the mod's watchdog needs a pong or another event of the module within NAT_WATCHDOG seconds)
--   switched off by the mod for want of an answer: pinged every X.NAT_REARM_PING s and armed again when the module answers (natGiveUp)
-- Switching it off (hold_off): before a resync load, when the session ends (identity.txt says idle), when the mod is not
-- active, on a fault, when the module stops answering, and when the module itself says `holding {on = false}` (it retries
-- with a back-off). Without native_events.log nothing of this runs and nothing is written.

-- used outside this section:
local NAT_PING, NAT_CAP_TTL, NAT_GRACE, NAT_PREVIEW_TTL, NAT_CONFIRM, NAT_ALIVE, NAT_WATCHDOG, SITES, natNew, natSay, natSend, natTrigger,
	natStop, natFault, natPing, natOrder, natRelease, natSessionOn, natSite, natDecide
do -- section: strict mode: the native link and the verdict (not indented, see SECTIONS at the top)
NAT_PING = 2.0      -- longest pause between lines written to the control file while holding
NAT_CAP_TTL = 2.0   -- a capture without its `held` is dropped after this many seconds
local NAT_CAP_MAX = 6     -- captures kept
NAT_GRACE = 0.4     -- a `held` waits this long (and 3 frames) for its capture; then decided without one (refused: X.natUncaptured)
NAT_PREVIEW_TTL = 30.0 -- the latest preview of a tool counts for a `held` this long after it was made
NAT_CONFIRM = 3.0   -- hold_on must be answered by `holding on` within this
NAT_ALIVE = 6.0     -- a pong this recent proves the module reads the control file
NAT_WATCHDOG = 8.0  -- no proof of life (a pong or any other event) for this long while holding: the module is gone
-- Strict mode switched off by the mod (no answer, hold_on not confirmed, the watchdog, the module's own switch-offs) is not off for
-- good: the mod keeps pinging every X.NAT_REARM_PING s and, when the module answers again, arms strict mode again (hold_on), after a
-- back-off of X.NAT_REARM_MIN s that doubles with every re-arm up to X.NAT_REARM_MAX s (reset after a minute of holding). Real run
-- 2026-10-14 (rail-crash): the watchdog fired on a live module during fast building (no ping went out while builds wrote the file, so
-- no pong came) and the session went on unheld for good; the host's next station was applied at the click, the worlds forked and the
-- guest's next track crashed the host.
-- (fields of X, not locals: the bridge's main chunk is at the game's limit of 200 locals; real run rc1 2026-10-14 did not load with 6 more)
X.NAT_REARM_PING = 5.0
X.NAT_REARM_MIN = 5.0
X.NAT_REARM_MAX = 60.0
X.NAT_FAULT_COOL = 60.0  -- three faults switch strict mode off for this long, then it is armed again with the fault count reset

-- The interface's calls of CommandList::Add that the module may hold (build 40420, names from the address tool's report) and the
-- tool ids whose preview (builder.proposalCreate) can belong to a command added there; the pairs are the ones real sessions showed.
-- `tools = {}`: the site's tool sends NO preview, so there is nothing to pair (real session 2026-10-13: the lane / crosswalk tool,
-- UI::LaneModifier::Apply, was paired with a 12 s old preview of the upgrade tool and shipped as that upgrade). `delta`: what that tool
-- changes is node configurations only, which the world can say after the build ran (Build.nodeConfigDelta). A site that is not listed
-- is paired as before.
SITES = {
	[0x4d396f] = { name = "Bulldozer::Apply", tools = { bulldozer = true } },
	[0x51b56c] = { name = "ConstructionBuilder::MousePressed", tools = { constructionBuilder = true } },
	[0x5891b4] = { name = "StreetBuilder::UpdateEngine", tools = { streetBuilder = true, trackBuilder = true } },
	[0x5948dd] = { name = "stop tool", tools = { streetTerminalBuilder = true } },
	[0x5bf08d] = { name = "TrackModifier::Build", tools = { streetTrackModifier = true } },
	[0x537ee5] = { name = "LaneModifier::Apply", tools = {}, delta = true },
	[0x538649] = { name = "LaneModifier::Apply", tools = {}, delta = true },
	[0x5387cd] = { name = "LaneModifier::Apply", tools = {}, delta = true },
}

function natNew()
	return {
		unaccounted = {},   -- held id -> a command given back with no record of its own, until it is known whether anything shipped it
		-- read every frame: kept open (no open per frame); a reloaded script reads only the newest generation as history, whose top
		-- carries the module's `ready` and clock (native link.rs EventWriter), not the whole log again
		reader = C.newReader(C.path("native_events.log"), { start = "newest", keep = true }),
		seen = false, read = false,   -- native_events.log has records / its first batch (history) was read
		ready = nil, hookOk = nil,    -- the latest `ready`; true/false: command_list_add is / is not among its hooks; nil: no `ready` seen
		pingN = 0, pingAt = -100, pingsTried = 0, pongAt = nil, watchFrom = nil, lastTick = nil, lastWrite = -100,
		aliveAt = nil,      -- the newest proof of life of the module: any event of this run read from native_events.log (a pong among them)
		holdSent = false, holdAt = 0, active = false, activeSince = 0,
		retryAt = nil, retries = 0, gaveUp = false,
		gaveUpAt = nil, gaveUpWhy = nil, rearms = 0, rearmPingAt = -100,   -- strict mode given up: when, why, how often it was re-armed
		deadAt = nil,       -- three faults switched strict mode off at this time (re-armed after X.NAT_FAULT_COOL)
		caps = {}, parked = {}, awaiting = {}, owed = {}, deferred = {},
		heldCaps = {},      -- held id -> the capture of that command's own proposal (the native module's preview `held:<id>`, natCapture)
		heldByKey = {}, byId = {}, finishes = {},   -- shipped builds whose own copy is held: by action key / by held id; pause-path completions
		logWin = -100, logN = 0, logLost = 0, faults = 0, dead = false,
	}
end

-- One line each, at most 12 per 10 s (the rest are counted): a stuck state must not fill mod.log.
function natSay(text)
	traceGui("native", text)
	local N = G.nat
	if N then
		local now = C.clock()
		if now - N.logWin >= 10 then
			if N.logLost > 0 then logGui("strict: " .. N.logLost .. " more line(s) held back in the last 10 s") end
			N.logWin, N.logN, N.logLost = now, 0, 0
		end
		if N.logN >= 12 then
			N.logLost = N.logLost + 1
			return
		end
		N.logN = N.logN + 1
	end
	logGui("strict: " .. text)
end

function natSend(kind, payload)
	local N = G.nat
	if not N then return false end
	-- this load's own control file and writer (X.ctlWriter): no other writer shares the file, nothing is re-read per line
	local ok, err, w = X.ctlSend(kind, payload)
	if kind == "hold_off" then pcall(X.terrainShareOff) end   -- the module stops sharing terrain edits with it (terrain.rs)
	if ok then
		N.lastWrite = C.clock()
	else
		natSay("cannot write " .. tostring(w and w.path) .. ": " .. tostring(err))
	end
	return ok
end

-- A paused game applied the build of deferred held command `id` from this state (route -> guiApplyNow): this interface's world
-- has the apply now, which a running game learns from its game time passing the stamp (natTick). The module completes the command.
function X.deferApplied(id)
	local N = G.nat
	local d = N and N.deferred[id]
	if d and not d.ready then
		d.ready = true
		natSend("release", { id = id })
	end
end

-- The module hands a held command back (or destroys it) at the next CommandList::Add of ANY command; any command we send
-- is one. This one does nothing in the simulation half (docs/NATIVE_LINK.md section 4, "Why a trigger").
function natTrigger()
	pcall(toSim, "nop", {})
end

-- Stops strict mode: hold_off, so that every held command goes back to the engine at the next Add (and one is sent).
function natStop(why)
	local N = G.nat
	if not N then return end
	if N.holdSent then
		N.holdSent, N.active = false, false
		-- hold_off gives every held command back at the next Add, before its stamp: the builds enter this world early. Each gets
		-- its replay mark first (the commands queue in this order), or the simulation half would ship it a second time.
		for _, h in pairs(N.heldByKey) do
			if not (h.released or h.done or h.cancelled) then
				h.released, h.early = C.clock(), true
				pcall(toSim, "replay", { key = h.key })
				pcall(toSim, "heldgone", { key = h.key })
				natSay("held " .. h.id .. " (" .. h.key .. ") goes back to the engine before its stamp (" .. tostring(why) .. ")")
			end
		end
		-- a command still waiting for its verdict has no record: `hold_off` would give it back to run on this game alone, so it is
		-- refused first (X.natUncaptured; the module takes the `fail` before the `hold_off`)
		for _, h in ipairs(N.parked) do
			pcall(X.natUncaptured, h, "strict mode went off (" .. tostring(why) .. ") before its verdict")
		end
		N.parked = {}
		pcall(natSend, "hold_off", {})
		natTrigger()
		natSay("mode off (" .. tostring(why) .. "), hold_off written")
	end
	N.holdSent, N.active, N.parked, N.caps, N.owed = false, false, {}, {}, {}
end

-- A fault in strict mode never reaches the bridge: it is logged, and the third one switches strict mode off for X.NAT_FAULT_COOL
-- seconds (natTick arms it again afterwards, with the count reset).
function natFault(where, err)
	local N = G.nat
	if not N then return end
	N.faults = N.faults + 1
	natSay("fault in " .. where .. ": " .. tostring(err))
	if N.faults >= 3 and not N.dead then
		N.dead, N.deadAt = true, C.clock()
		pcall(natStop, "three faults")
		natSay("mode is off (three faults): it is armed again in " .. X.NAT_FAULT_COOL .. " s")
	end
end

-- Strict mode is given up for now (no answer, no confirmation, the watchdog, the module's own switch-offs): one line says why and when it
-- may come back. natTick re-arms it when the module answers again (natRearm).
function X.natGiveUp(why)
	local N = G.nat
	if not N then return end
	local now = C.clock()
	N.gaveUp, N.gaveUpAt, N.gaveUpWhy = true, now, why
	natSay(string.format("mode is off: %s; holding is off until the native module answers again (re-armed after at least %.0f s)", why,
		math.min(X.NAT_REARM_MAX, X.NAT_REARM_MIN * 2 ^ math.min(N.rearms, 4))))
end

-- The session's game time stands still: it stands at its pause (pauseAt reached) or runs at speed 0. Every ping tells the native module,
-- and a change is pinged at once (natTick): what the module holds does not age meanwhile, so a build deferred at a pause that lasts
-- minutes (the host waits for every game's stop report and a round trip before it stamps the pause's builds) is not failed by the
-- module's age limit while the mod keeps pinging.
function X.sessionStill()
	local ok, still = pcall(function()
		local s = G.session
		return s ~= nil and (pausedNow() or (tonumber(s.speed) or 1) <= 0)
	end)
	return ok and still == true
end

function natPing()
	local N = G.nat
	N.pingN = N.pingN + 1
	N.pingAt = C.clock()
	local still = X.sessionStill()
	N.stillSent = still
	return natSend("ping", { n = N.pingN, paused = still or nil })
end

-- release or cancel, and make sure an Add follows: the order is only carried out at the next Add.
function natOrder(kind, id)
	local N = G.nat
	local ok = natSend(kind, { id = id })
	if ok then
		N.awaiting[id] = { kind = kind, t = C.clock(), tries = 0 }
		natTrigger()
	end
	return ok
end

-- The stamp of a shipped build has come on this game (the simulation half took the snapshot of its entity lists and booked the
-- ledger, as every other game does): the held command goes back to the engine, which applies it. The replay mark comes first
-- in the engine's command list, so the build's event finds it and the simulation half does not capture this build as a new one.
function natRelease(key)
	local N = G.nat
	local h = N and N.heldByKey[key]
	if not h or h.released or h.done or h.cancelled then return false end
	h.released = C.clock()
	h.releasedAt = engine.gameTime()
	pcall(toSim, "replay", { key = key })
	local ok = natOrder("release", h.id)
	pcall(toSim, "heldgone", { key = key })
	natSay(string.format("held %d (%s) released at game time %d (stamp %d); the engine applies it here, the other games replay it at the stamp",
		h.id, key, engine.gameTime(), h.at or 0))
	return ok
end

-- THE rule for every kind of tool (street, track, stop, construction, bulldoze ...): a held command is cancelled only when the module can
-- fire the tool's completion callback. A cancel without it leaves the tool dead (real runs 2026-10-11 and 2026-10-13), so otherwise the
-- command goes back to the engine (the build exists on this game, the other games are repaired): what MPFever does for every tool.
local function natCancelAllowed(h)
	return h.fireable == true
end

-- The action was refused at its stamp (the same rule that refuses it on every other game): the held command is destroyed.
function natHeldRefused(it)
	local N = G.nat
	local h = N and N.heldByKey[it.key]
	if not h or h.released or h.done or h.cancelled then return end
	if not natCancelAllowed(h) then
		-- Released until bug 8 (2026-10-18): the build every game refused at the stamp was then applied on this game alone. Failed now, as
		-- natVeto does (`fail` frees the tool; an older module gets the cancel): no game has the build.
		local how = X.natFailHeld(h.id, it.key, h.at, false, h, false)
		natSay("held " .. h.id .. " (" .. it.key .. ") was refused at its stamp: FAILED, NOT released (the tool's callback cannot be fired by a cancel); " .. tostring(how))
		return
	end
	h.cancelled = true
	pcall(toSim, "heldgone", { key = it.key })
	natSay("held " .. h.id .. " (" .. it.key .. ") was refused at its stamp: cancelled")
	natOrder("cancel", h.id)
end

-- A game could not rebuild this game's held build (build_veto): the held command is destroyed, so the build exists on no game. A
-- cancelled command gives the tool no "applied" callback: the tool of this build stays dead (a command that is held cannot be
-- taken back without either applying it or cancelling it; the module has no third verdict). Too late (released already: the stamp
-- came first) is said, not hidden: the worlds then differ and the checksums repair them.
function natVeto(key, who, why)
	local N = G.nat
	local h = N and N.heldByKey[key]
	if not h then return nil end   -- not a held build of this game
	if h.released or h.done or h.cancelled then
		natSay("veto of " .. key .. " by " .. tostring(who) .. " came too late (the command was released already): " .. tostring(why))
		return false
	end
	if not natCancelAllowed(h) then
		-- The module cannot fire this tool's callback, so a cancel would leave the tool dead (real session 2026-10-13: an underground
		-- station entrance, the owner got stuck and a resync followed). It was released instead until 2026-10-15: the build then existed
		-- on this game only (every other game drops it on the veto) and a resync followed. Now it is FAILED (X.natFailHeld: `fail` fires
		-- the callback with a failed result, nothing is applied here); no game has the build, so no repair. Only a module without `fail`
		-- still gets a cancel without the callback (the tool may stay busy), never a release.
		local how = X.natFailHeld(h.id, key, h.at, false, h, false)
		natSay("held " .. h.id .. " (" .. key .. ") FAILED on the veto, NOT released: " .. tostring(who) .. " could not build it (" .. tostring(why) .. "); " .. tostring(how))
		return true
	end
	h.cancelled = true
	pcall(toSim, "heldgone", { key = key })
	natSay("held " .. h.id .. " (" .. key .. ") cancelled: " .. tostring(who) .. " could not build it (" .. tostring(why) .. ")")
	natOrder("cancel", h.id)
	return true
end

-- A game that is paused at the stamp applies the action itself (guiApplyNow): the held command is released and `finish` (the
-- ledger and the keys of what the build made) runs when the module reports that the engine applied it.
function natPauseRelease(it, finish)
	local N = G.nat
	local h = N and N.heldByKey[it.key]
	if not h or h.done then
		finish({}, true, {})   -- the command went back to the engine earlier: the build is in this world already
		return
	end
	if h.cancelled then
		finish({}, false, {})
		return
	end
	h.finish = finish
	if h.released then return end   -- released already (hold_off): finish when the engine reports it
	if not natRelease(it.key) then
		h.finish = nil
		error("the release of held " .. h.id .. " could not be written")
	end
end

-- the session switch is common.lua's (C.poll); the end of a session switches strict mode off in X.sessionOff
function natSessionOn()
	return C.ACTIVE == true
end

-- ---- the build of one held command

function natSite(h)
	return type(h.site) == "number" and string.format("0x%x", h.site) or "?"
end

-- A command this half gives back WITHOUT a record of its own enters this world and nobody else's, unless the simulation half's capture of
-- native builds ships it (shipNative). Nothing may be missed silently: it is written down with the name of its site, and when the
-- engine reports it applied, `natLedger` says whether something shipped it. For a site whose changes the world can describe afterwards
-- (SITES.delta) the state before is kept, and the difference is shipped when nothing else did.
local function natUnaccounted(h, why)
	local N = G.nat
	local info = SITES[h.site]
	local u = { id = h.id, site = natSite(h), name = info and info.name or "a site the table does not know", why = why, t = C.clock(), shipped0 = G.nativeShipped or 0 }
	if info and info.delta and Build and Build.nodeConfigSnapshot then
		local ok, snap = pcall(Build.nodeConfigSnapshot)
		if ok then u.before = snap end
	end
	N.unaccounted[h.id] = u
	-- With the native copy on, the engine's own held command goes with the record the simulation half captures, so the other games
	-- inject that command instead of rebuilding the record (run inj2, 2026-10-14: a station configure from a site the table does not
	-- know and a stop replacing a stop were released here and rebuilt from the record: factory "Unknown exception" on both games).
	if NATIVE_INJECT then
		local okT, text = pcall(C.readFile, C.path("native_text_" .. h.id .. ".txt"))
		if okT and type(text) == "string" and #text > NATIVE_TEXT_MAX then
			-- inj3: a station configure's text was 7.6 MB (the old/new node maps' values walked into the world, 32768 blocks, truncated:
			-- the rebuild cannot take it anyway) and the record never reached the host
			traceGui("build.inject", string.format("held=%d released without a record: its native text (%d bytes) is over %d bytes, not shipped; the record path builds it", h.id, #text, NATIVE_TEXT_MAX))
		elseif okT and type(text) == "string" and text ~= "" then
			pcall(toSim, "heldnative", { text = text, id = h.id })
			traceGui("build.inject", string.format("held=%d released without a record: its native text (%d bytes) goes to the simulation half's capture", h.id, #text))
		end
	end
	natSay("held " .. h.id .. " (site " .. u.site .. ", " .. u.name .. "): " .. why .. "; release written, the native build goes through")
	natOrder("release", h.id)
end

-- ---- a build without a record, made during a pause: released at its place in the pause order (owner decision 2026-10-18)
--
-- A held command without a capture has no record before the engine applies it (the simulation half reads it at the apply). Released
-- at the click in a pause, it was applied at this game's own stop and shipped stamped at the pause; the host's pause order stamps one
-- step past the FURTHEST stop, so the originator's world had the build one step or more before every other game, and a game that had
-- stopped further on applied it late (PAUSE LATE). Refuted therefore that "nothing runs in between": the originator simulates the steps
-- from its stop to the others' apply with the build, the others without it (the free-id FIFO and the step's world differ), and a game
-- standing past the originator's stop cannot apply at that stop at all. So it waits like every other build of the pause:
--   1. the command stays held and only its PLACE goes out: an action `pauseSlot` flagged `pause`, which the host numbers like any other;
--   2. that number reached, every game holds there (X.slotOpen): the later numbers wait, no game runs past the slot's run-on (holdAt);
--   3. the originator releases its command once it stands still at or past every game (each stood still at or past the run-on): the
--      engine applies it at that time Q, the simulation half captures it there and shipNative sends the record stamped Q, its slot named;
--   4. every other game stands at or before Q and moves no further until the record arrives; it runs on to Q, where the simulation applies
--      it exactly, and then the waiting numbers follow (stamped at least Q, after the build, in their order). The originator does not apply
--      it again (its own native record is never queued here). The resume waits too: the hold holds a running game as well.
-- One rule for every tool and site: only "no record before the engine applies it" decides.
function X.slotWanted()
	return pausedNow() and G.writer ~= nil
end

function X.slotHold(h, why)
	local seq = G.seq + 1
	local key = L.itemKey(C.PLAYER_ID, seq)
	local at = G.session.pauseAt
	local body = { fn = "pauseSlot", held = h.id }
	-- `build`: the host's grant decides as for a build (the record itself goes out with its own category later)
	local ok, err = C.writerSend(G.writer, "action", X.stampHop({ stamp = at, seq = seq, body = C.serialize(body), category = "build", pause = true }))
	X.seqShipped(seq)
	if not ok then return X.natRefuseHeld(h, "its place in the pause order cannot be sent (" .. tostring(err) .. ")") end
	G.seq = seq
	h.slotted = true
	G.pauseOwn = G.pauseOwn or {}
	G.pauseOwn[key] = { kind = "action", at = at, from = C.PLAYER_ID, seq = seq, body = body, key = key, pause = true, slot = { h = h, why = why } }
	pcall(natSay, "held " .. h.id .. " (site " .. natSite(h) .. "): " .. tostring(why) .. "; made during the pause: HELD, its place " .. key
		.. " goes to the host's pause order; released there, where every game stands, and its record applied there by every game")
end

-- Step 3 above: the originator's command goes back to the engine (natUnaccounted: the simulation half's capture ships it).
function X.slotRelease(S)
	local o = S.own
	S.released, S.relWall = engine.gameTime(), C.clock()
	natUnaccounted(o.h, tostring(o.why) .. "; released at its place " .. S.key .. " (pause order #" .. tostring(S.n) .. ") at t=" .. C.integerText(S.released)
		.. ", where every game stands")
end

-- The held command `h` has the capture `cap`: ship it and cancel the command, or release it.
-- The input-level text the native module wrote for a held command (native_input_<id>.txt), read and parsed once (h.input = the tree).
-- Shadow only until the replay uses it: the trace line says what it holds. Never raises.
local function natInput(h)
	if h.input ~= nil or not Portable then return h.input end
	local ok, tree, info = pcall(function()
		local text = C.readFile(C.path("native_input_" .. h.id .. ".txt"))
		if not text or text == "" then return nil, "missing" end
		local t, why = Portable.parse(text)
		if not t then return nil, why end
		h.inputText = text
		return t, { bytes = #text, counts = Portable.countsText(Portable.counts(t)) }
	end)
	if not ok then tree, info = nil, tostring(tree) end
	h.input = tree or false
	if tree then
		traceGui("build.input", string.format("held=%d bytes=%d counts[%s]", h.id, info.bytes, info.counts))
	else
		traceGui("build.input", string.format("held=%d NOT AVAILABLE (%s)", h.id, tostring(info)))
	end
	return h.input
end

-- B2.1, shadow: the record the capture made against the text the native module read from the held command (member by member), and
-- the text goes with the record so the other games can make the same comparison. Never changes the record's meaning: the text is an
-- extra member of the body that nothing reads yet.
local function natShadow(h, body)
	local tree = natInput(h)
	if not (tree and Portable and body and body.args) then return end
	body.input = h.inputText
	if NATIVE_INJECT then
		local okT, text = pcall(C.readFile, C.path("native_text_" .. h.id .. ".txt"))
		if okT and type(text) == "string" and text ~= "" then
			body.nativeText = text
			traceGui("build.inject", string.format("held=%d the native module's text of the command goes with the record (%d bytes)", h.id, #text))
			X.natRefPrints(traceGui, "held=" .. h.id, body, text)
		else
			traceGui("build.inject", string.format("held=%d NO native text (native_text_%d.txt missing): the record path builds it", h.id, h.id))
		end
	end
	shadowCompare(traceGui, "held=" .. h.id .. " side=originator", h.inputText, body.args[1])
	return true
end

-- The originator decides (open bug 7): the native module refused the held command's own text before anything was shipped
-- (native_refused_<id>.txt, its reason in the first line), because every game would refuse that text at the stamp (owner airport run
-- 2026-10-15 and friend test 2026-10-16: shipped, then refused on every game, the originator's own copy included; the build was lost after
-- the player saw it accepted, and resyncs followed). Only with a module that advertises the inject order (an older one writes no such file).
-- Returns the reason, or nil.
function X.natTextRefused(h)
	local N = G.nat
	if not (NATIVE_INJECT and N and N.ready and N.ready.inject) then return nil end
	local ok, text = pcall(C.readFile, C.path("native_refused_" .. h.id .. ".txt"))
	if not ok or type(text) ~= "string" or text == "" then return nil end
	return (text:match("^[^\n]*") or text)
end

-- A held build that is shipped to nobody: failed here before the ship (its tool callback fired with a failed result where the module can
-- fail it; else cancelled), never released, so that no game has it. Same rule as natVeto: never a release (the build would be on this game only).
function X.natFailUnshipped(h, why)
	local N = G.nat
	local how
	if N.ready and N.ready.defer then
		N.deferred[h.id] = { key = "unshipped", at = 0, t = C.clock(), ready = true, failed = true }
		local ok = natSend("fail", { id = h.id })
		natTrigger()
		how = ok and "fail written (tool callback fired with a failed result)" or "FAIL NOT WRITTEN"
	else
		local ok = natOrder("cancel", h.id)
		how = (ok and "cancel written" or "CANCEL NOT WRITTEN") .. ((FIRE_DONE and h.done) and " (the tool's callback is fired)" or " (this module cannot fire the tool's callback: the tool may stay busy)")
	end
	natSay("held " .. h.id .. " (site " .. natSite(h) .. "): REFUSED BEFORE SHIPPING, every game would refuse its exact copy: " .. tostring(why) .. "; " .. how .. "; nothing is shipped and no game builds it")
	pcall(vetoChat, "unshipped:" .. tostring(h.id) .. ":" .. tostring(C.clock()), "build_cancelled")
end

local function natDecideUnsafe(h, cap)
	local N = G.nat
	local id = h.id
	natInput(h)
	do
		local refusedWhy = X.natTextRefused(h)
		if refusedWhy then return X.natFailUnshipped(h, refusedWhy) end
	end
	-- `release`: a held command with no record of its own before the engine applies it. Made during a pause: released at its place in
	-- the host's pause order, where every game stands (X.slotHold). Otherwise REFUSED (X.natUncaptured): released at the click it ran
	-- on this game alone, ~16 steps before the record the simulation half captured from it reached the others (friend test of 2026-10-20
	-- 2026-10-21: 4 of 8 free-id forks began with "no capture of a tool build; release written"). `refuse`: a check of this build failed
	-- here; it is never given back (X.natRefuseHeld, fail closed).
	local function release(why)
		if X.slotWanted() then return X.slotHold(h, why) end
		X.natUncaptured(h, why)
	end
	local function refuse(why, msg) X.natRefuseHeld(h, why, msg) end
	-- A command whose tool gave no preview is recorded from its own proposal: the native module gives every held build command to this
	-- script as the preview of the tool `held:<id>` before announcing it (natCapture keeps it in N.heldCaps). One rule for every tool;
	-- run s4 2026-10-14: a station module added via Configure (UI::ModuleBuilder, site 0x542eba) sends no preview, was released at once
	-- and applied 7 steps before the stamp here only (other entity ids than on the other games from then on).
	local own = N.heldCaps[id]
	N.heldCaps[id] = nil
	if not cap and own and own.raw and Build.bodyOf then
		local okB, body, whyB, text = pcall(Build.bodyOf, own.raw, { playerInitiated = true })
		if not okB then body, whyB = nil, tostring(body) end
		if body then
			body.category = buildCategory(body)
			local okS, s2 = pcall(C.serialize, body)
			if okS then own.body, own.text = body, s2 else own.why = tostring(s2) end
		else
			own.why = whyB
		end
	end
	if not cap and own and own.body then
		traceGui("build.capture", string.format("held=%d site=%s: no tool preview; recorded from the held command's own proposal (%d bytes, %.1f ms)", id, natSite(h), #(own.text or ""), own.ms or 0))
		cap = own
	end
	if not cap then return release("no capture of a tool build" .. ((own and own.why) and (" (its own proposal: " .. tostring(own.why) .. ")") or "")) end
	if not cap.body then
		-- A build that cannot be captured cannot be shipped: the others never get it, and the safety net's resync would erase it here
		-- later (real session 2026-10-12: a guest's bus stops, line and buses). It is refused and the player told (X.natRefuseHeld).
		-- With the native copy on, the record is completed from the held command instead (its model, name and position along the
		-- segment are in the module's input text), so a stop is shipped before the build and applied at the stamp like every build.
		if cap.partial and tostring(cap.why):find("has no model", 1, true) then
			local tree = natInput(h)
			local objs = tree and tree.proposal and Portable and Portable.list(tree.proposal.edgeObjectsToAdd) or {}
			local okF, done, whyF = pcall(Build.fillPendingObjects, cap.partial, objs)
			local okS, s2 = false, nil
			if okF and done then
				cap.partial.category = buildCategory(cap.partial)
				okS, s2 = pcall(C.serialize, cap.partial)
			end
			if okS then
				traceGui("build.capture", string.format("held=%d the edge object(s) of the preview were completed from the held command's text (%d object(s)): shipped before the build", id, #objs))
				cap = { tool = cap.tool, body = cap.partial, text = s2, t = cap.t, ms = cap.ms, verdict = cap.verdict }
			else
				traceGui("build.capture", string.format("held=%d completing the edge object(s) from the held command's text failed: %s", id, tostring(okF and whyF or done)))
			end
		end
	end
	if not cap.body then
		if tostring(cap.why):find("has no model", 1, true) then
			-- an edge object (a stop) gets its model and parameters only when the build runs, and completing it from the held command's
			-- text failed above: no record before the build, so the same rule as no capture (refused; in a pause: its place in the pause
			-- order, the record read after the build by the simulation half, SIM.deferredBuild)
			if X.slotWanted() and natInput(h) then pcall(toSim, "heldinput", { text = h.inputText }) end
			return release("the capture of " .. tostring(cap.tool) .. " has no record before the build (" .. tostring(cap.why) .. ")")
		end
		-- A capture that failed is a failed check: released, the build went into this world at the click and was shipped (if at all) by
		-- the simulation half's own capture, applied on the other games at another step (fail open). Refused on every module now: the
		-- module that can `fail` frees the tool (an older one gets the cancel, its callback fired where it can).
		return refuse("the capture of " .. tostring(cap.tool) .. " failed (" .. tostring(cap.why) .. ")", "build_kind_unsupported")
	end
	local body, text = cap.body, cap.text
	-- The preview the capture was made from carries no Context; the held command does (the native module read it). Its flags replace the
	-- capture's, so the record that travels has the originator's real Context, ignoreErrors and playerInitiated. Its refundable entities are
	-- named by reference here, while the command is held (keyed as every capture keys entities), so that each game's replay refunds its own.
	local _, shadowed = pcall(natShadow, h, body)
	local okF, changed = pcall(function() return Build and Build.withHeldFlags and Build.withHeldFlags(body, h.ctx, { plain = C.plain, keyOf = function(e) return L.keyFor(G.rev, e) end }) end)
	if (okF and changed) or shadowed then
		local okS, s = pcall(C.serialize, body)
		if okS then text = s end
	end
	pcall(function()
		traceGui("build.ctx", string.format("held=%d side=originator source=%s ctx{%s} ie=%s pi=%s", id, (okF and changed) and "held command" or "none (old native module: capture's guess)",
			Build and Build.contextText(body.args[2]) or "?", tostring(body.args[3]), tostring(body.args[4])))
	end)
	local category = buildCategory(body)
	if not L.permitted(G.perms, category) then
		-- refused before stamping, like the wrapped sendCommand: nothing was built yet, so nothing is sent to the others
		-- the cancel first: a fault in the lines after it must not reach natDecide's handler with no verdict written (it would release it)
		natOrder("cancel", id)
		natSay("held " .. id .. ": not allowed to " .. category .. ", cancelled")
		send("action_refused", { seq = 0, detail = "permission denied: " .. category })
		return
	end
	-- A build that adds constructions (depot, station, ...): the player gets an informational line. With FEVERLINK3_REMOTE_CONSTRUCTIONS=0
	-- it is cancelled instead, but only when the native module can fire the tool's callback (a cancel without it leaves the tool dead).
	if Build and Build.addsConstructions and Build.addsConstructions(body) then
		if Build.constructionsBlocked(body) and h.done then
			natSay("held " .. id .. ": adds constructions (" .. tostring(cap.tool) .. "), refused in this test build: cancelled")
			natOrder("cancel", id)
			vetoChat("gate:" .. tostring(id) .. ":" .. tostring(C.clock()), "build_kind_unsupported")
			return
		end
		-- this player's own notice, once per session: the others need not read it
		G.vetoChatted = G.vetoChatted or {}
		if not G.vetoChatted["info:constructions"] then
			G.vetoChatted["info:constructions"] = true
			pcall(X.noticeLocal, "constructions_experimental")
		end
	end
	-- never ship, and never apply, what this game could not rebuild itself
	local wire = C.parse(text)
	if #text > ACTION_BODY_MAX and type(wire.input) == "string" then
		-- the input-level text is a diagnostic and a fallback; the native text and the record are what the other games build from
		wire.input = nil
		text = C.serialize(wire)
		traceGui("build.ship", string.format("held=%d the record was over %d bytes: sent without its input-level text (%d bytes now)", id, ACTION_BODY_MAX, #text))
	end
	if #text > ACTION_BODY_MAX then
		return refuse(string.format("its record (%d bytes) is over the %d bytes an action may have", #text, ACTION_BODY_MAX), "build_too_big")
	end
	local now = engine.gameTime()
	local okD, args, whyD = pcall(L.decodeCommand, wire, guiCtx({ kind = "action", at = now, from = C.PLAYER_ID, seq = 0, key = "strict-check" }))
	if not okD then args, whyD = nil, tostring(args) end
	-- The self-check: this game rebuilds its own record (the reference check of its native text included) the way every other game will.
	-- Failed, the build is refused, never released (friend test 2026-10-16, host street 0:2: "it cannot be rebuilt here (inject_refused:
	-- ... names node 89384 ... 11.16 m away)"; release written, applied on the host, refused the same way on the friend's game: a fork).
	if not args then return refuse("it cannot be rebuilt here (" .. tostring(whyD) .. ")") end
	local seq = G.seq + 1
	local at
	local cancelFlow = STRICT_CANCEL == "all"
	-- "plans": every originator, "host": only the host, for the proven plans (see the comment at STRICT_CANCEL).
	if STRICT_CANCEL == "plans" or (STRICT_CANCEL == "host" and C.PLAYER_ID == 0) then
		local okP, plan = pcall(function() return Build and Build.planOf and Build.planOf(body) end)
		cancelFlow = okP and STRICT_CANCEL_PLANS[plan or ""] == true
	end
	if STRICT_CANCEL == "removals" then
		-- only a build that removes and adds nothing (the bulldozer), judged by what the record holds, not by the plan that rebuilds it: the
		-- plan test it replaced ("removal" or "construction" without an add) missed a bulldozing of town buildings together with a street
		-- piece (plan "bulldoze"), which then went hold-and-release, was applied 1000 ms after the stamp on the originator and drifted (sweep
		-- 20261013-234802, real bulldozer over two houses and a road end). The tool's callback must be fireable, or the tool stays dead.
		local okA, addsNothing = pcall(function() return Build and Build.addsNothing and Build.addsNothing(body) end)
		cancelFlow = okA and addsNothing == true and FIRE_DONE and h.done == true
	end
	-- With the native copy on, every game applies the build at the stamp through the engine Proposal built from this text, this game too:
	-- the held command is cancelled (its tool callback fired) and replayed like everywhere else. Why: every entity the simulation creates
	-- takes its id from one FIFO of free ids (ecs::Engine::AddEntity takes the front, EndModification appends the removed ids), so a build
	-- applied at another step than on the other games shifts every later id, town growth's included. Native id trace (FEVERLINK3_ID_TRACE,
	-- 2026-10-14): loading and idle play hand out identical ids on both games; a hold-and-release road applied 3 steps late on the originator
	-- and a stop applied 7 steps early gave other ids from that build on; the same road cancelled and injected at the stamp on both games
	-- gave identical id streams (every AddEntity/RemoveEntity equal). General: no plan or tool filter, only "the tool's callback can be fired".
	if NATIVE_INJECT and STRICT_CANCEL ~= "off" and N.ready and N.ready.inject and type(body.nativeText) == "string" and body.nativeText ~= ""
		and FIRE_DONE and h.done == true then
		cancelFlow = true
	end
	-- Deferred completion (the general form of the rule above, for every tool): the held command stays held until this game's own stamp
	-- apply of the injected build, then takes that apply's result and its tool callback is fired with it, as the engine itself does after
	-- applying a command. No empty-result callback (the construction tool's callback, its configure mode included, cannot take one) and
	-- no build applied at another step than on the other games: the originator applies at the stamp like everyone (ids stay equal).
	local deferFlow = NATIVE_INJECT and STRICT_CANCEL ~= "off" and N.ready and N.ready.inject and N.ready.defer
		and type(body.nativeText) == "string" and body.nativeText ~= ""
	if deferFlow then cancelFlow = true end
	if cancelFlow and not deferFlow and FIRE_DONE and not h.done then
		natSay("held " .. id .. " (site " .. natSite(h) .. "): the native module cannot fire this tool's callback: hold-and-release instead of cancel")
		cancelFlow = false
	end
	if not cancelFlow then
		-- the record says it is held: the other games dry-run it and veto it if they cannot build it (see "veto-only validation")
		wire.held = true
		if pausedNow() then wire.commit = true end   -- the peers park it until the originator's commit (see "a held build in a PAUSED game")
		text = C.serialize(wire)
		local heavy = Build and Build.addsOrRemovesConstructions and Build.addsOrRemovesConstructions(body)
		at = stampAfterFastest(now, heavy and CONSTRUCTION_VETO_STEPS or VETO_STEPS)
	else
		-- every game applies the record at the stamp, the originator too, and a game that has passed the stamp applies it late and
		-- allocates other entity ids (real run A1-1: the host was 1.8 game s ahead of the guest's clock when the guest's build
		-- arrived, 3 steps late). Since the input barrier, the stamp is this game's promise floor (L.floorStamp); without it (below) the
		-- fastest known clock plus the host's lead plus the MEASURED extra (L.marginExtra: the largest need the peers report from the
		-- slack of the builds they received; was a fixed 6 steps, 1.8 s at x1). The extra goes with the record, so every receiver can
		-- measure what it would have needed.
		at = stampAfterFastest(now)
		local floorAt = (not pausedNow() and not X.TEST_MARGIN and not X.selfStalled() and L.barrierCovers(G.peers))
			and L.floorStamp(G.session, now, G.promiseFloor) or nil
		if floorAt then
			-- the input barrier covers every peer: the stamp is this game's promise floor (L.floorStamp says why it is never late on any
			-- game), with no extra (option A, 2026-10-22: the fastest clock + lead + an unmeasured default extra of aheadFor(speed)+1 was
			-- 20 steps at x4, the floor about 16). wire.xs = 0: every receiver still measures its slack (build.margin), now as a check
			at, wire.xs = floorAt, 0
			text = C.serialize(wire)
		elseif not pausedNow() and next(G.peers) ~= nil then
			-- a peer without promises (an older mod), no promise sent yet, this game stalled or a test switch: the stamp as before
			-- wire.xs: the extra the stamp really has past the fastest clock plus the lead (at least the measured margin)
			at, wire.xs = stampAfterFastest(now, nil, L.marginExtra(G.peers, G.session.speed, X.TEST_MARGIN))
			-- test switch FEVERLINK3_TEST_NO_LEAD: the host's lead goes too, down to leadOf's floor of one step past the fastest clock
			if X.TEST_MARGIN and X.TEST_MARGIN.noLead then at = at - (L.leadOf(G.session) - STEP) end
			text = C.serialize(wire)
		end
	end
	-- the build's timing so far (click and the native capture from the module, ship now), carried with the record to every game
	local wShip = X.wallNow()
	local okT, errT = pcall(function()
		local t = N.timing and N.timing[id] or {}
		local cap = t.capture or ""
		local c0, c1 = cap:match("start=(%d+)"), cap:match("end=(%d+)")
		-- every number through %s of a whole value: the interface's game time can be fractional, and %d refuses that in Lua 5.2 (r1: no text)
		local function whole(v) return string.format("%.0f", tonumber(v) or 0) end
		wire.tm = string.format("held=%s click=%s@g%s cap=%s..%s cap_ms=%s bytes=%s mask_ms=%s encode_ms=%s write_ms=%s input_ms=%s seen=%s ship=%s@g%s",
			whole(id), c0 or whole(h.wSeen), whole(h.gClick), c0 or "?", c1 or "?", cap:match("ms=([%d.]+)") or "?", cap:match("bytes=(%d+)") or "?",
			cap:match("mask_ms=([%d.]+)") or "?", cap:match("encode_ms=([%d.]+)") or "?",
			cap:match("write_ms=([%d.]+)") or "?", (t.input or ""):match("input_ms=([%d.]+)") or "?", whole(h.wSeen), whole(wShip), whole(now))
		if N.timing then N.timing[id] = nil end
		text = C.serialize(wire)
	end)
	if not okT then traceGui("build.timing", "held=" .. tostring(id) .. " the timing text could not be made: " .. tostring(errT)) end
	-- THE SHIP IS THE LAST IRREVERSIBLE STEP. Once the action is written, every other game applies the build at the stamp, so this game may
	-- only apply it at the stamp too (defer, cancel-and-replay, or hold until the stamp) or, if that cannot be done, fail it (tool freed,
	-- nothing applied here) and ask for a repair; never release it at the click (natDecide). Everything that can fail is therefore made
	-- here, before the write: the item, the flow, every log text. A fault in the ship trace (Build.planOf, string.format) used to reach
	-- natDecide's handler, which released the command: applied at the click here, at the stamp everywhere else, and captured and shipped a
	-- second time by the simulation half.
	local site = natSite(h)
	local it = { kind = "action", at = at, from = C.PLAYER_ID, seq = seq, body = wire, key = L.itemKey(C.PLAYER_ID, seq), tRecv = wShip, gRecv = now }
	local flow = (cancelFlow and deferFlow) and "defer" or (cancelFlow and "cancel" or "held")
	if flow ~= "held" then wire.held = nil end
	local function say(fmt, ...)
		local okF, s = pcall(string.format, fmt, ...)
		return okF and s or ("held " .. tostring(id) .. " " .. it.key .. " (a log line could not be made: " .. tostring(s) .. ")")
	end
	local planOk, plan = pcall(function() return Build and Build.planOf and Build.planOf(body) end)
	local shipLine = say("%s held=%s site=%s done=%s tool=%s plan=%s stamp=%s now=%s lead=%s extra=%s bytes=%d capture_ms=%s balance=%s record=%s", it.key, tostring(id),
		site, tostring(h.done), tostring(cap.tool), planOk and tostring(plan) or ("? (" .. tostring(plan) .. ")"), tostring(at), tostring(now), tostring(at - now),
		tostring(wire.xs), #text, string.format("%.1f", tonumber(cap.ms) or 0), tostring(select(2, pcall(engine.account))), text)
	local done
	if flow == "defer" then
		done = function(okV) return say("held %s (site %s) matched the capture of %s (%s bytes): %s stamped %s (now %s), deferred: %s", id, site, tostring(cap.tool),
			#text, it.key, tostring(at), tostring(now), okV and "kept until this game's own stamp apply, then completed with its result" or "DEFER NOT WRITTEN") end
	elseif flow == "cancel" then
		done = function(okV) return say("held %s (site %s) matched the capture of %s (%s bytes): %s stamped %s (now %s), cancel flow: %s", id, site, tostring(cap.tool),
			#text, it.key, tostring(at), tostring(now), okV and "cancel written, this game replays the record too" or "CANCEL NOT WRITTEN (the build may be applied twice here)") end
	else
		local line = say("held %s (site %s) matched the capture of %s (%s bytes, %.0f ms to capture, %.0f ms old): %s stamped %s (now %s), held until the stamp; the tool's verdict %s",
			id, site, tostring(cap.tool), #text, tonumber(cap.ms) or 0, (C.clock() - (tonumber(cap.t) or C.clock())) * 1000, it.key, tostring(at), tostring(now), tostring(cap.verdict or "n/a"))
		done = function() return line end
	end
	local grace, graceLine = nil, nil
	if flow == "held" and wire.commit then
		local ms, rtt = graceMs()
		local okH, heavy = pcall(function() return Build and Build.addsOrRemovesConstructions and Build.addsOrRemovesConstructions(body) end)
		if okH and heavy then ms = math.max(ms, CONSTRUCTION_GRACE_MS) end
		grace = { it = it, ms = ms }
		graceLine = say("%s: the game is paused, so the build waits %d ms (round trip about %.0f ms) for a veto before it is committed", it.key, ms, rtt)
	end
	local ship = { it = it, flow = flow, shipping = true }
	h.ship = ship   -- natDecide's fault handler reads it: from here on a fault never releases the command
	-- made during the pause (defer and cancel flows: applied by every game at once): every game applies it in the host's pause order,
	-- this one too when it comes back numbered (X.routeOwn). A held build of a paused game is parked by the others and its COMMIT is.
	it.pause = (flow ~= "held" and X.hostStamps(at)) or nil
	local okS, errS = C.writerSend(G.writer, "action", X.stampHop({ stamp = at, seq = seq, body = text, category = category, pause = it.pause }))
	X.seqShipped(seq)
	if not okS then
		h.ship = nil   -- nothing went out: no game has the build, and given back it would be on this game alone
		return refuse("the action cannot be sent (" .. tostring(errS) .. ")")
	end
	ship.shipped = true
	G.seq = seq
	-- after the ship: only what makes this game apply the build at the stamp, each step marked (ship.step) for the fault handler; every
	-- log line is made already and written under pcall, so a log cannot fail the decision
	local function log(fn, ...) pcall(fn, ...) end
	ship.step = "book"
	noteBuild(it.key, at)
	log(traceGui, "build.ship", shipLine)
	if flow == "defer" then
		ship.step = "defer"
		it.deferId = id
		-- made during a pause: `at` is only the pause stamp, a placeholder. The host gives the real stamp, past where every game stands
		-- (X.pauseStamped); until then nothing may count from it (b3pause2, 2026-10-15: the host stood 6 steps past the pause, natTick took
		-- the placeholder as passed and wrote `release` at once, the module completed the tool's command at the simulation's apply, before
		-- the interface's world had the new road, and the street tool's callback stopped the game on an entity it did not know)
		N.deferred[id] = { key = it.key, at = at, t = C.clock(), waitStamp = it.pause or nil }
		local okD = natSend("defer", { id = id })
		ship.verdict = okD and "defer" or nil
		log(natSay, done(okD))
	elseif flow == "cancel" then
		ship.step = "cancel"
		local okC = natOrder("cancel", id)
		ship.verdict = okC and "cancel" or nil
		log(natSay, done(okC))
	else
		-- The command stays HELD until the stamp (nobody's world runs ahead of the others). It is not cancelled: a cancelled command
		-- never gives the tool its "applied" callback and the tool stays dead (real run 2026-10-11). At the stamp this game's
		-- simulation half does everything the other games do except sending (simApplyAction, it.heldId) and this half then releases
		-- the held command (natRelease); every OTHER game replays the build at the stamp.
		ship.step = "hold"
		it.heldId = id
		local rec = { id = id, key = it.key, at = at, t = C.clock(), fireable = FIRE_DONE and h.done == true }
		N.heldByKey[it.key], N.byId[id] = rec, rec
		ship.rec = rec
		pcall(toSim, "heldout", { key = it.key })
		log(natSay, done())
		if grace then
			grace.due = C.clock() + grace.ms / 1000
			G.graces[it.key] = grace
			ship.step, ship.routed = nil, true   -- natCommit routes it when the grace is over
			log(natSay, graceLine)
			return
		end
	end
	ship.step = "route"
	X.routeOwn(it)
	ship.step, ship.routed = nil, true
end

-- THE rule for a held command whose action was SHIPPED (every other game applies it at the stamp) and that cannot be applied at the stamp
-- here: it is failed, never released (a release applies it at another time than on the other games: other entity ids from then on, and
-- the simulation half may ship it a second time). `fail` fires the tool's callback with a failed result (the module that can defer has it:
-- native runtime, both or neither); an older module gets a cancel (its callback fired only where the module can). This game's replay is
-- dropped. Returns what was done, for the caller's log line. Used by the verdict (natDecide), the commit of a paused build (natCommit) and
-- the held watchdog.
function X.natFailHeld(id, key, at, fireable, rec, cancelledAlready)
	local N = G.nat
	if rec then rec.cancelled = true end   -- natRelease never gives it back at the stamp now
	G.graces[key] = nil
	pcall(toSim, "drop", { key = key, why = "its originator failed it after the ship", own = true })
	pcall(toSim, "heldgone", { key = key })
	if cancelledAlready then return "cancelled already" end
	if N and N.ready and N.ready.defer then
		N.deferred[id] = { key = key, at = at, t = C.clock(), ready = true, failed = true }
		local ok = natSend("fail", { id = id })
		natTrigger()
		return ok and "fail written (tool callback fired with a failed result)" or "FAIL NOT WRITTEN"
	end
	if N then N.deferred[id] = nil end
	local ok = natOrder("cancel", id)
	return (ok and "cancel written" or "CANCEL NOT WRITTEN") .. ((FIRE_DONE and fireable) and " (the tool's callback is fired)" or " (this module cannot fire the tool's callback: the tool may stay busy)")
end

-- The other games have a build this game failed after shipping it: the host is asked to repair them (and the player told).
function X.natShippedRepair(key, why)
	send("replay_failed", { detail = "build " .. key .. " was shipped but failed on its originator (" .. tostring(why):sub(1, 100) .. "): the other games have it" })
	pcall(vetoChat, "shipfail:" .. key, "build_failed_after_share")
end

-- THE rule for a held command that fails a check on its originator BEFORE anything was shipped (the batch-2 rule, fail closed): it is
-- refused, never given back to the engine. Friend test 2026-10-16, host street 0:2: this game's own rebuild of its record failed (the
-- reference check of its native text: "names node 89384 ... 11.16 m away"), the command was released anyway ("release written, the
-- native build goes through"), so the host applied it at the click, the simulation half shipped it although its own self-check FAILED
-- too, and the friend's game refused it the same way: the worlds forked until a resync. Now nothing is shipped and nothing is applied on
-- any game: `fail` fires the tool's callback with a failed result (the tool is free; the module that can defer has it), an older module
-- gets a cancel (its callback fired only where the module can), and the player is told in the notices. No game has the build, so
-- no repair is needed. One rule for every tool and every check (natDecideUnsafe's refuse, natDecide's fault handler). Returns what was done.
function X.natRefuseHeld(h, why, msg, chatKey)
	local N = G.nat
	local id = h.id
	h.refused = true
	local how
	if N and N.ready and N.ready.defer then
		N.deferred[id] = { key = "held " .. tostring(id), at = engine.gameTime(), t = C.clock(), ready = true, failed = true }
		how = natSend("fail", { id = id }) and "fail written (tool callback fired with a failed result)" or "FAIL NOT WRITTEN"
		natTrigger()
	elseif N then
		how = (natOrder("cancel", id) and "cancel written" or "CANCEL NOT WRITTEN")
			.. ((FIRE_DONE and h.done == true) and " (the tool's callback is fired)" or " (this module cannot fire the tool's callback: the tool may stay busy)")
	else
		how = "no native link"
	end
	natSay("held " .. tostring(id) .. " (site " .. natSite(h) .. ") REFUSED: " .. tostring(why) .. "; NOT released, nothing is shipped or applied on any game; " .. how)
	-- `msg`: the notice (feverlink3_text.lua, default build_cancelled); `chatKey`: one notice per key and session (vetoChat), for a
	-- refusal that can repeat with every click
	pcall(vetoChat, chatKey or ("refused:" .. tostring(id) .. ":" .. tostring(C.clock())), msg or "build_cancelled")
	return how
end

-- THE rule for a held command with no record of its own before the engine applies it (no capture of its tool, none from its own
-- proposal, a stop whose model the record cannot get before the build, more than one command held at once): it is refused like a
-- failed check (X.natRefuseHeld: failed, its tool callback fired as "not applied", nothing applied or shipped on any game), never
-- released. Released at the click (until 2026-10-21), the engine applied it on this game at once and the simulation half's capture
-- shipped it stamped ~16 steps later, so the other games applied it at another step or not at all: friend test of 2026-10-20, 4 of 8 free-id
-- forks (3 of them the first held build after a resync load). The one exception is a pause (X.slotHold: released where every game stands).
function X.natUncaptured(h, why)
	return X.natRefuseHeld(h, "no record before the engine applies it (" .. tostring(why) .. "), so it cannot be carried to the other games at its stamp",
		"build_cancelled", "uncaptured")
end

-- A fault AFTER the action was shipped (natDecideUnsafe, h.ship): the other games apply the build at the stamp, so this game must not
-- apply it at the click. Its command is failed instead: the tool's callback is fired with a failed result (`fail`, the module that can
-- defer) or the command is cancelled (an older module; the tool's callback is fired only where the module can), nothing is applied here,
-- this game's replay is dropped from the queue, and the host is asked to repair the games (the others have the build, this one not).
-- A paused game's build whose commit was never sent is parked on the other games and dropped there: no repair is needed for that one.
local function natFailShipped(h, err)
	local N, ship, id = G.nat, h.ship, h.id
	local it = ship.it
	local key = it.key
	if ship.routed and not ship.step then
		-- everything was in place before the fault (a log line after the last step): the build is applied at the stamp here too
		natSay(string.format("held %d (%s): a fault after the ship (%s), but its stamp apply here was already arranged (%s): nothing changed", id, key, tostring(err), ship.flow))
		return
	end
	if ship.rec and (ship.rec.released or ship.rec.done) then
		-- a paused game applies at once (route -> guiApplyNow -> natPauseRelease): the command went back at its stamp (the pause time)
		natSay(string.format("held %d (%s): a fault after the ship (%s), but it was released at its stamp already: nothing changed", id, key, tostring(err)))
		return
	end
	local how = X.natFailHeld(id, key, it.at, h.done == true, ship.rec, ship.verdict == "cancel")
	natSay(string.format("held %d (%s) FAILED after the ship at step %s of the %s flow (%s): NOT released, nothing is applied on this game; %s",
		id, key, tostring(ship.step), ship.flow, tostring(err), how))
	if it.body and it.body.commit and not ship.routed then
		natSay("held " .. id .. " (" .. key .. "): its commit was never sent, the other games drop the parked record: no repair needed")
		return
	end
	X.natShippedRepair(key, err)
end

-- A fault while deciding must not leave the command held (it would stay in the ring until the module's 30 s age limit): before the ship
-- it is refused (X.natRefuseHeld: nothing was sent, so no game has the build; given back, it was on this game alone), unless the verdict
-- was already written. After the ship it is NEVER given back: it is applied at the stamp here too or failed (natFailShipped).
function natDecide(h, cap)
	h.ship = nil
	local ok, err = pcall(natDecideUnsafe, h, cap)
	if ok then return end
	natFault("the verdict on held " .. tostring(h.id), err)
	local N = G.nat
	if h.ship then
		local okF, errF = pcall(natFailShipped, h, err)
		if not okF then pcall(natSay, "held " .. tostring(h.id) .. ": the failure after the ship itself failed (" .. tostring(errF) .. "); the command is NOT released") end
		return
	end
	if N and not N.awaiting[h.id] and not h.refused and not h.slotted then pcall(X.natRefuseHeld, h, "a fault in the verdict (" .. tostring(err) .. ")") end
end
end -- section: strict mode: the native link and the verdict

-- used outside this section:
local natSettle, commitPump, natCapture, natTick
do -- section: strict mode: commits, the ledger, events (not indented, see SECTIONS at the top)
-- The grace of a paused game's held build is over (or a resync / the end of the session cuts it short): tell the others to apply it
-- and release this game's own command. If the game was unpaused meanwhile the normal stamp logic takes over: a new stamp ahead.
-- The commit is an irreversible send like the action itself (natDecide): after it the other games apply the build at the stamp, so this
-- game routes it to its own stamp apply or fails its command (X.natFailHeld), never leaves it held for the watchdog to release late. A
-- fault BEFORE the commit fails it too, without a repair: the other games drop the parked record when no commit comes (PARK_MAX).
local function natCommitUnsafe(key, why, g, st)
	local it = g.it
	local now = engine.gameTime()
	if not pausedNow() then
		it.at = stampAfterFastest(now)
		noteBuild(key, it.at)
		local h = G.nat and G.nat.heldByKey[key]
		if h then h.at = it.at end
		pcall(logGui, "commit of " .. key .. ": the game is no longer paused, the build gets the stamp " .. tostring(it.at) .. " (now " .. tostring(now) .. ")")
	end
	-- still paused: the commit is numbered in the host's pause order and every game, this one too, applies the build when it comes back
	it.pause = X.isPauseStamp(it.at) or nil
	if it.pause then
		-- the commit's stamp is the host's (X.pauseStamped): the watchdog does not count from the pause stamp meanwhile
		local h = G.nat and G.nat.heldByKey[key]
		if h then h.waitStamp = true end
	end
	st.sent = true
	send("build_commit", { seq = it.seq, owner = C.PLAYER_ID, stamp = it.at, pause = it.pause })
	pcall(logGui, "commit of " .. key .. " sent (" .. tostring(why) .. "): no veto within " .. tostring(g.ms) .. " ms; every game applies it at " .. tostring(it.at))
	X.routeOwn(it)
	st.routed = true
end

local function natCommit(key, why)
	local g = G.graces[key]
	if not g then return end
	G.graces[key] = nil
	local st = {}
	local ok, err = pcall(natCommitUnsafe, key, why, g, st)
	if ok or st.routed then return end
	local N = G.nat
	local rec = N and N.heldByKey[key]
	if rec and (rec.released or rec.done) then
		-- a paused game applies at once (route -> guiApplyNow -> natPauseRelease): it went back at its stamp before the fault
		pcall(natSay, "commit of " .. key .. ": a fault (" .. tostring(err) .. ") after the command was released at its stamp: nothing changed")
		return
	end
	local okF, how = true, "no held command of it here"
	if rec and not rec.cancelled then okF, how = pcall(X.natFailHeld, rec.id, key, g.it and g.it.at, rec.fireable == true, rec, false) end
	pcall(natSay, string.format("held %s (%s) FAILED at its commit (%s, %s): NOT released, nothing is applied on this game; %s", tostring(rec and rec.id), key,
		st.sent and "after the commit was sent" or "before the commit was sent", tostring(err), okF and tostring(how) or ("the failure itself failed: " .. tostring(how))))
	if st.sent then
		pcall(X.natShippedRepair, key, err)
	else
		pcall(natSay, "commit of " .. key .. " was never sent: the other games drop the parked record, no repair needed")
	end
end

-- Before the game is saved for a resync: no build may stay held across the save. A grace is cut short (committed); a build whose
-- stamp has not come is taken out of the queue and released now, so the save contains it and no game applies it a second time
-- after the reload. Returns the number of releases that must be confirmed by the engine before the save.
function natSettle(why)
	local N = G.nat
	local waiting = 0
	if not N then return 0 end
	for key in pairs(G.graces) do natCommit(key, why) end
	for key, h in pairs(N.heldByKey) do
		if not (h.released or h.done or h.cancelled) then
			pcall(toSim, "drop", { key = key, why = "released before the " .. why, own = true })
			if natRelease(key) then waiting = waiting + 1 end
			natSay("held " .. h.id .. " (" .. key .. ") released before the " .. why .. ", " .. tostring((h.at or 0) - engine.gameTime()) .. " ms before its stamp: the saved world contains the build")
		elseif h.released and not h.applied and not h.done then
			waiting = waiting + 1
		end
	end
	return waiting
end

local function settleDone()
	local N = G.nat
	if not N then return true end
	for _, h in pairs(N.heldByKey) do
		if h.released and not h.applied and not h.done and C.clock() - h.released < 4 then return false end
	end
	return true
end

-- Held builds that cannot go the normal way. (1) Their stamp passed and nobody released them (the interface half did not see the
-- simulation half's release result: seen once in a real run, the command was then kept for the module's 30 s limit): released now
-- and the cause logged. (2) The stamp lies ahead but the game stands still (a pause before it): the module gives a command back
-- after 30 s, which would put the build into this world alone, so the build is cancelled on every game before that.
local HELD_STUCK_STEPS = 3
local HELD_PAUSED_MAX = 20.0
local function heldWatchdog(N, now)
	local gt = engine.gameTime()
	for key, h in pairs(N.heldByKey) do
		if not (h.released or h.done or h.cancelled) and not G.graces[key] and h.at and not h.waitStamp then
			if gt >= h.at + HELD_STUCK_STEPS * STEP and now - h.t > 2 then
				-- Every command in heldByKey was shipped: the other games applied it at its stamp. A release now would apply it here at
				-- another step (other entity ids from then on, and the simulation half could ship it again): it is failed and the games
				-- repaired instead (X.natFailHeld, the same rule as a fault after the ship). Was a release until 2026-10-15.
				local how = X.natFailHeld(h.id, key, h.at, h.fireable == true, h, false)
				natSay(string.format("WATCHDOG: the stamp %s of held %s (%s) passed %s ms ago (game time %s) and the command is still held; the simulation half's release result %s; FAILED, NOT released: %s",
					tostring(h.at), tostring(h.id), key, tostring(gt - h.at), tostring(gt), G.resultsSeen[key .. "#release" .. tostring(h.id)] and "WAS seen" or "was not seen", how))
				X.natShippedRepair(key, "its stamp passed while it was still held")
			elseif gt < h.at and now - h.t > HELD_PAUSED_MAX then
				local why = "held for " .. HELD_PAUSED_MAX .. " s and its stamp still lies ahead (the game is paused)"
				G.vetoed[key] = why
				pcall(toSim, "drop", { key = key, why = why, own = true })
				send("build_veto", { seq = tonumber(key:match(":(%d+)$")), owner = C.PLAYER_ID, why = why, name = C.PLAYER_NAME })
				-- cancelled, or FAILED where the callback cannot be fired (natVeto): never released, the other games drop it on the veto
				natVeto(key, C.PLAYER_NAME, why)
				vetoChat(key, "build_paused_too_long")
			end
		end
	end
end

-- every frame: graces that are over, parked records whose commit never came
function commitPump()
	local clock = C.clock()
	for key, g in pairs(G.graces) do
		local h = G.nat and G.nat.heldByKey[key]
		if G.vetoed[key] or (h and h.cancelled) then
			G.graces[key] = nil
		elseif clock >= g.due then
			natCommit(key, "grace over")
		end
	end
	if G.pendingSave and (settleDone() or clock - G.pendingSave.since > 4) then
		local ps = G.pendingSave
		G.pendingSave = nil
		ps.go()
	end
	-- a resync load waits for this world's detail dump (H.resync_load, X.syncDetail)
	local written = G.pendingLoad and X.detailWritten()
	if written then
		local pl = G.pendingLoad
		G.pendingLoad = nil
		logGui(string.format("resync: the detail of sync %s %s after %.1f s; loading now", tostring(G.detailSync), written, clock - pl.since))
		pl.go()
	end
	for key, pk in pairs(G.parked) do
		if clock - pk.wall > PARK_MAX then
			G.parked[key] = nil
			G.vetoed[key] = "no commit"
			logGui("build " .. key .. " was parked for " .. PARK_MAX .. " s and its originator never committed it: dropped")
		end
	end
end

-- Every command given back without a record of its own (natUnaccounted) is accounted for: shipped by the simulation half's capture of native
-- builds, shipped here as a node-configuration difference, or named as NOT REPLICATED. Called every frame.
local NAT_LEDGER_WAIT = 2.0    -- the simulation half's capture ships within this after the engine's `applied`
local NAT_LEDGER_GIVEUP = 20.0 -- no `applied` within this: the command was destroyed or never ran
local function natLedger(N, now)
	for id, u in pairs(N.unaccounted) do
		local done
		if u.applied and now - u.applied >= NAT_LEDGER_WAIT then
			if (G.nativeShipped or 0) > u.shipped0 then
				natSay(string.format("held %d (site %s, %s): accounted for, the simulation half shipped a native build", id, u.site, u.name))
			elseif u.before then
				local body, why, text = Build.nodeConfigDelta(u.before, { plain = C.plain, keyOf = function(e) return L.keyFor(G.rev, e) end })
				if body then
					shipNative({ body = text, at = engine.gameTime() })
					natSay(string.format("held %d (site %s, %s): nothing shipped it, the node configurations that changed (%s) were shipped", id, u.site, u.name, tostring(#body.args[1].ncfg)))
				else
					natSay(string.format("held %d (site %s, %s): NOT REPLICATED, applied here only (%s)", id, u.site, u.name, tostring(why)))
				end
			else
				natSay(string.format("held %d (site %s, %s): NOT REPLICATED, applied here only (%s, and no simulation capture shipped it)", id, u.site, u.name, u.why))
			end
			done = true
		elseif now - u.t >= NAT_LEDGER_GIVEUP and not u.applied then
			natSay(string.format("held %d (site %s, %s): never reported applied within %.0f s", id, u.site, u.name, NAT_LEDGER_GIVEUP))
			done = true
		end
		if done then N.unaccounted[id] = nil end
	end
end

-- Drops the captures that waited longer than NAT_CAP_TTL for their `held` (the native build went through unheld).
local function natExpire(now)
	local N = G.nat
	local keep = {}
	for _, c in ipairs(N.caps) do
		if now - c.t > NAT_CAP_TTL then
			if c.body then
				natSay(string.format("capture of %s dropped: no held command within %.0f s (the native build went through, the simulation half ships it)", c.tool, NAT_CAP_TTL))
			end
		else
			keep[#keep + 1] = c
		end
	end
	N.caps = keep
end

-- Pairs the oldest `held` that waits for a capture with the oldest capture, until one list is empty.
local function natPair()
	local N = G.nat
	local now = C.clock()
	natExpire(now)
	-- The tool's own event for an applied build (builder.proposalApply) only comes AFTER the engine applied it, so for a
	-- command that is held it never comes before the release (seen in a real game: every capture arrived after its held
	-- command had been given back). What is known while the command is held is the tool's latest PREVIEW (builder.proposalCreate):
	-- the tool is blocked waiting for its command, so no newer preview can come, and the last one is the proposal that was clicked.
	-- Two or more commands held at once cannot be told apart by one preview (a real tool is blocked after its click, so this
	-- does not happen): refuse them all (no record: X.natUncaptured) rather than ship the wrong record for one of them.
	if #N.parked > 1 and not N.caps[1] then
		N.preview = nil
		while N.parked[1] do
			natSay("held " .. N.parked[1].id .. ": more than one command held at once; no record can be told apart for it")
			natDecide(table.remove(N.parked, 1), nil)
		end
		return
	end
	-- A command is paired with the preview of ITS tool only: a site whose tool sends no preview gets none, and a preview of another tool stays
	-- for its own command (the command is given back and written down, natUnaccounted).
	local keep = {}
	for _, h in ipairs(N.parked) do
		local info = SITES[h.site]
		local own = N.heldCaps[h.id]
		if (info and next(info.tools) == nil) or (not info and own and own.raw) then
			-- no preview of its tool can belong to it (or the site is unknown and its own proposal was recorded: no guess from the
			-- newest preview of another tool); natDecideUnsafe takes the command's own record
			natDecide(h, nil)
		else
			keep[#keep + 1] = h
		end
	end
	N.parked = keep
	while N.parked[1] and (N.caps[1] or (N.preview and now - N.preview.t <= NAT_PREVIEW_TTL)) do
		local h = table.remove(N.parked, 1)
		local cap, fromList = N.caps[1], true
		if not cap then cap, fromList = N.preview, false end
		local info = SITES[h.site]
		if info and not info.tools[cap.tool] then
			natSay(string.format("held %d (site %s, %s): the newest preview is the %s tool's, which does not issue commands there; not paired", h.id, natSite(h), info.name, tostring(cap.tool)))
			natDecide(h, nil)
		else
			if fromList then table.remove(N.caps, 1) else N.preview = nil end
			natDecide(h, cap)
		end
	end
end

local function natOnHeld(N, p)
	local id = tonumber(p.id)
	if not id then return end
	local h = { id = id, site = tonumber(p.site), t = C.clock(), frame = G.frames, done = p.done == true, ctx = type(p.ctx) == "table" and p.ctx or nil, gClick = engine.gameTime(), wSeen = X.wallNow() }
	if not N.holdSent then
		-- strict mode is being switched off: the module has not caught up. Outside a session the command goes back to the engine; in a
		-- session it has no record, so it is refused like every held command without one (X.natUncaptured)
		if natSessionOn() then return X.natUncaptured(h, "held while strict mode is off") end
		natSay("held " .. id .. " while strict mode is off and no session runs; release written")
		natOrder("release", id)
		return
	end
	N.parked[#N.parked + 1] = h
	if not N.batching then natPair() end
end

-- Called from guiHandleEvent for builder.proposalCreate (the tool's preview): keeps the newest capture for the `held` that follows a click.
function natCapture(id, name, param)
	local N = G.nat
	if not N or N.dead or not N.holdSent or name ~= "builder.proposalCreate" then return end
	if not (Build and Build.captureToolBuild) then return end
	local t0 = C.clock()
	local heldId = tonumber(tostring(id):match("^held:(%d+)$"))
	if heldId then
		-- the held command's own proposal, given by the native module from inside the tool's mouse handler (Runtime::give_preview), before its
		-- `held` event: only read here (Build.captureRaw, no dry run); the record's body is made in natDecideUnsafe. Kept for exactly that
		-- command, never as a tool's preview.
		traceGui("build.capture", string.format("held=%d own proposal: reading", heldId))
		local okR, rec, whyR = pcall(function() return Build.captureRaw((param[1] or param.proposal), { plain = C.plain, keyOf = function(e) return L.keyFor(G.rev, e) end }) end)
		if not okR then rec, whyR = nil, tostring(rec) end
		local own = { tool = tostring(id), raw = rec, why = whyR, t = C.clock() }
		own.ms = (own.t - t0) * 1000
		N.heldCaps[heldId] = own
		for k, c in pairs(N.heldCaps) do
			if own.t - c.t > NAT_PREVIEW_TTL then N.heldCaps[k] = nil end
		end
		traceGui("build.capture", string.format("held=%d own proposal read: %s (%.1f ms)", heldId, rec and "record read" or ("no record: " .. tostring(whyR)), own.ms))
		return
	end
	local ok, body, why, text = pcall(Build.captureToolBuild, id, name, param, {
		plain = C.plain, keyOf = function(e) return L.keyFor(G.rev, e) end,
	})
	if not ok then body, why = nil, tostring(body) end
	local cap = { tool = tostring(id), why = why }
	if NATIVE_INJECT and not body and tostring(why):find("has no model", 1, true) and Build.fillPendingObjects then
		-- the preview of an edge object (a stop) has no model yet; the held command that follows the click has it: the record is captured
		-- now with the edge objects marked and completed from that command's text (natDecideUnsafe). The event's proposal cannot be read
		-- again later (real run s2 2026-10-14: "nothing to build" a frame after the click).
		local okP, partial = pcall(Build.captureToolBuild, id, name, param, {
			plain = C.plain, keyOf = function(e) return L.keyFor(G.rev, e) end, pendingModels = true,
		})
		if okP and type(partial) == "table" then cap.partial = partial end
	end
	if body then
		body.category = buildCategory(body)
		local okS, s = pcall(C.serialize, body)
		if okS then text = s else body, cap.why = nil, tostring(s) end
		if body then cap.body, cap.text = body, text end
	end
	cap.t = C.clock()
	cap.ms = (cap.t - t0) * 1000
	-- the tool's own verdict: the preview event carries the ProposalData the tool made (the only place a native Proposal's verdict can be read)
	pcall(function()
		local es = (param[2] or param.data).errorState
		local msgs = {}
		for i = 1, math.min(#es.messages, 4) do msgs[#msgs + 1] = tostring(es.messages[i]) end
		cap.verdict = string.format("critical=%s messages=[%s]", tostring(es.critical == true), table.concat(msgs, "; "))
	end)
	-- only the newest preview is kept, and a preview that could not be captured replaces a good one before it: a `held`
	-- must never be paired with the record of an older preview than the one on screen
	N.preview = cap
	natPair()
end

-- The refusal hint: what guiHandleEvent returns for a tool preview (builder.proposalCreate) that FeverLink3 will refuse to carry, so the
-- player reads why in the tool itself, red like the game's own "cannot build here", before the click instead of a cancel after it.
-- UNDOCUMENTED API (owner rule 2026-10-19: allowed with caution for hints and messages only). tf3-docs (guides/scripting.html,
-- reference/api/api/gui.html) gives guiHandleEvent the return type `any` and says only that a GUI script event "can return a value (like
-- an error string to prevent the operation)"; the wiki's Game Scripts / User Interface pages are "Coming Soon"; neither names builder
-- events or `errorMessages`. Evidence for the shape `{ errorMessages = { [text] = true } }`: the TPF3-MP build probe (MIT, read only,
-- docs/research/TPF3MP_REVIEW_20261019.md 2.8) notes the base game's company script refuses a proposal this way from proposalCreate.
-- It only reads what natCapture already decided for this very preview (no engine read, no work per frame beyond the tool's own preview
-- events), and it is given only where natDecideUnsafe refuses the held command anyway (a capture that failed for a reason other than
-- "has no model", of a tool some held site pairs with): the outcome is the same, nothing is built and nothing is sent to any game, so
-- the hint has no gameplay or sync effect. Empty previews ("nothing to build") get none. Any fault drops the hint (pcall at the caller).
function X.refusalHint(id)
	local N = G.nat
	if not N or N.dead or not N.holdSent then return nil end
	local cap = N.preview
	if not cap or cap.body or cap.tool ~= tostring(id) then return nil end
	local why = tostring(cap.why)
	if why:find("has no model", 1, true) or why:find("nothing to build", 1, true) or why:find("carries no", 1, true) then return nil end
	local paired = false
	for _, info in pairs(SITES) do if info.tools[cap.tool] then paired = true break end end
	if not paired then return nil end
	return { errorMessages = { ["FeverLink3 cannot carry this build to the other players: " .. why:sub(1, 120)] = true } }
end

-- ---- events of the module

-- The module refused an inject order (`inject_refused {key, held, outcome, reason}`). The build is applied exactly as the originator's text
-- says or not at all: `nothing` = the module emptied the command, nothing of it was applied here; `untouched` = it could not, and the mod's
-- placeholder (this game's own guess of the build) was applied; `no_command` = no command took the order (expired, replaced). The refusal is
-- this game's verdict of the build (X.injectAnswered): the host compares it with every other game's and asks for the repair only when they
-- differ (another game injected it). After `nothing` the verdict is refused; after `untouched` / `no_command` it is what this game's
-- build.result saw (X.tellRefused: the command may have applied as it was). The module fails the held command of the same build itself (`held`): the originator's tool is freed
-- with a failed result.
function X.onInjectRefused(p)
	local key, outcome = tonumber(p.key) or 0, tostring(p.outcome or "?")
	natSay(string.format("the native module refused inject order %s (%s): %s", C.integerText(key), outcome, tostring(p.reason or p.detail)))
	if key <= 0 then return end
	logGui(string.format("inject order %s refused (outcome %s, held %s): %s; the host compares this game's verdict with the other games'",
		C.integerText(key), outcome, tostring(p.held or 0), outcome == "nothing" and "nothing of the build is applied here"
			or "the command was not emptied, the verdict follows this game's build.result"))
	X.injectAnswered(key, string.format("inject order %s refused here (%s): %s", C.integerText(key), outcome, tostring(p.reason or ""):sub(1, 160)))
end

-- ---- action verdicts (docs/PROTOCOL.md "Action verdicts"): this game's verdict of every build at its stamp, for the host

-- `partial`: the engine threw after it had the command (X.handOver): this game's world may be partly built; the host never repairs
-- the others from it (ActionRefusedMessage.partial)
function X.sendVerdict(r, applied, detail, class, partial)
	if r.from == nil or r.seq == nil then return end
	-- an applied terrain edit names its height proof (X.terrainProof): the host takes two applied verdicts with different details for a fork
	local p = { seq = r.seq, from = r.from, detail = applied and (r.proof and ("terrain heights " .. tostring(r.proof)) or "") or tostring(detail or ""):sub(1, 300), class = (not applied) and class or nil,
		partial = (not applied and partial) and true or nil }
	send(applied and "action_applied" or "action_refused", p)
	traceGui("build.verdict", string.format("%s %s%s", tostring(r.of or r.key), applied and "applied" or "refused",
		applied and "" or (" class=" .. tostring(class) .. ": " .. p.detail)))
end

-- D: the engine threw while it applied action `key` here (X.handOver): said in the log and to the player, and the repair is asked for
-- at once (replay_failed), not left to the checksums. The verdict sent before it carries `partial`, so the host takes the repaired
-- world from a game where the action did not fail half-way (the host's own world included: HostSession::repair_source).
function X.partlyBuilt(key, err, where)
	logGui("action " .. tostring(key) .. " (" .. tostring(where) .. "): the engine threw DURING its apply: this game's world may be PARTLY built; asking the host for a repair now")
	send("replay_failed", { detail = "action " .. tostring(key) .. ": the engine threw during its apply, the world may be partly built here: " .. tostring(err):sub(1, 140) })
	if X.noticeLocal then
		pcall(X.noticeLocal, "action_failed_midway")
	end
end

-- The originator's player is told that their build was not applied (a notice on this game only: nothing of it changed any world).
-- The build key and the engine's reason stay in the log.
-- A refusal of the native module that did not empty the command (`no_command`: no command took the order; `untouched`: it could not be
-- emptied) says nothing of what this game applied: the command may have applied as it was (the mod's own rebuild of the record). The
-- notice, and with `verdict` this game's verdict for the host (X.injectStep), then wait for this game's build.result of the build
-- (X.buildOutcome): "applied" sends action_applied and tells nobody it failed; anything else is the refusal as before.
-- Two-window run 2026-10-22 (x4, build 0:77): the host was told "Your build could not be built ... Nothing was built. Try again" and
-- the road stood on both games (build.result verdict=applied, +5 nodes +6 segments). Friend session 2026-10-23c (0:374): the host's
-- module said no_command and its road stood (build.result applied, +2 nodes +4 segments), the friend refused it; the host's verdict
-- still said refused, so the host logged "refused alike on every game: no repair needed" and the fork stayed hidden. The verdict now
-- says what this world has, and the host's compare sees the fork.
function X.tellRefused(r, why, verdict)
	local mine = r.from == C.PLAYER_ID
	if not mine and not verdict then return end
	local key = tostring(r.of or r.key)
	if tostring(why or ""):find("refused here %((no_command)%)") or tostring(why or ""):find("refused here %((untouched)%)") then
		local seen = G.bresOutcome and G.bresOutcome[key]
		if seen == nil then
			G.noticeWait = G.noticeWait or {}
			G.noticeWait[key] = { r = r, why = why, t = C.clock(), verdict = verdict }
			logGui("build " .. key .. ": the native module refused its order without emptying the command: the " .. (verdict and "verdict" or "notice") .. " waits for this game's build.result")
			return
		end
		G.bresOutcome[key] = nil
		if seen == "applied" then
			logGui("build " .. key .. " was applied here (build.result verdict=applied) although the native module refused its order: " .. (verdict and "the verdict says applied, " or "") .. "the player is not told it failed")
			if verdict then X.sendVerdict(r, true) end
			-- the module failed the tool's held command with the refusal (its callback fired with a failed result): the player is told it stands
			if mine and X.noticeLocal then pcall(X.noticeLocal, "build_built_after_all", { thing = "build" }) end
			return
		end
	end
	if verdict then X.sendVerdict(r, false, why, "module") end
	if not mine then return end
	why = tostring(why or ""):gsub("^inject_refused: ", ""):gsub(" %(refused from this game's own world.*$", "")
	-- the native compare of the rebuilt Proposal with the originator's text (native inject.rs) is not a world change: the copy differs
	-- (friend test 2026-10-21: an empty town-building name; the player was told "the world changed ... Try again" and retried twice)
	local remade = why:find("differs from the text", 1, true) ~= nil
	logGui("build " .. tostring(r.of or r.key) .. " was not applied: " .. (remade and "the rebuilt build differs from the original: " or "the world changed before its turn: ") .. why:sub(1, 200))
	if X.noticeLocal then pcall(X.noticeLocal, remade and "build_not_remade" or "build_world_changed", { thing = "build" }) end
end

-- This game's build.result verdict of build `key` ("applied", "unseen", "refused(...)"), every role: a verdict or notice waiting for it
-- is settled now (X.tellRefused); else it is kept for a refusal that comes later (the last 64).
-- A build of this game's player that a resync load cut from its tool (X.pauseArrived: G.builtLate; its held command was failed at
-- hold_off, the tool's callback fired with a failed result) and that the host's pause order applied afterwards: the player is told it
-- stands. Friend session 2026-10-23c (0:498): failed at 17:02:30 by hold_off, built on every game at 17:03:12, nobody said so.
function X.buildOutcome(key, verdict)
	key = tostring(key)
	G.bresOutcome, G.bresOrder = G.bresOutcome or {}, G.bresOrder or {}
	G.bresOutcome[key] = tostring(verdict)
	G.bresOrder[#G.bresOrder + 1] = key
	if #G.bresOrder > 64 then G.bresOutcome[table.remove(G.bresOrder, 1)] = nil end
	if G.builtLate and G.builtLate[key] then
		G.builtLate[key] = nil
		if tostring(verdict) == "applied" then
			logGui("build " .. key .. " was cut from its tool by the resync and applied after it (build.result verdict=applied): the player is told it stands")
			if X.noticeLocal then pcall(X.noticeLocal, "build_built_after_all", { thing = "build" }) end
		end
	end
	local w = G.noticeWait and G.noticeWait[key]
	if w then
		G.noticeWait[key] = nil
		return X.tellRefused(w.r, w.why, w.verdict)
	end
end

-- A waiting notice whose build.result has not come within X.NOTICE_WAIT_S of real time (it comes one or two steps after the stamp, or
-- at once during a pause) is given as before: the player is never left untold (X.buildOutcome answers it as "unseen").
X.NOTICE_WAIT_S = 5
function X.noticeTimeouts(now)
	for key, w in pairs(G.noticeWait or {}) do
		if now - (w.t or now) > X.NOTICE_WAIT_S then
			logGui("build " .. key .. ": no build.result within " .. X.NOTICE_WAIT_S .. " s: the notice is given as it is")
			X.buildOutcome(key, "unseen")
		end
	end
end

-- A build applied at its stamp (X.simVerdict): its verdict goes now, or, with inject orders, once the module answered every one of them.
function X.verdictOf(r)
	if not r.injects then return X.sendVerdict(r, true) end
	local w = { r = r, left = #r.injects }
	G.injWait = G.injWait or {}
	for _, k in ipairs(r.injects) do
		local id = C.integerText(k)
		local done = G.injDone and G.injDone[id]
		if done ~= nil then X.injectStep(w, done) else G.injWait[id] = w end
	end
end

-- `refusal`: false = the order was injected, a text = why it was refused
function X.injectStep(w, refusal)
	if w.sent then return end
	if refusal then
		w.sent = true
		-- the verdict (and the originator's notice) of a refusal that did not empty the command follow this game's build.result
		return X.tellRefused(w.r, refusal, true)
	end
	w.left = w.left - 1
	if w.left <= 0 then
		w.sent = true
		X.sendVerdict(w.r, true)
	end
end

-- The module answered inject order `key` (`injected`: refusal false; `inject_refused`: its reason). The answer can come before the
-- simulation half's verdict reaches this half, so it is kept (the last 64).
function X.injectAnswered(key, refusal)
	local id = C.integerText(key)
	G.injDone, G.injOrder = G.injDone or {}, G.injOrder or {}
	G.injDone[id] = refusal or false
	G.injOrder[#G.injOrder + 1] = id
	if #G.injOrder > 64 then G.injDone[table.remove(G.injOrder, 1)] = nil end
	local w = G.injWait and G.injWait[id]
	if w then
		G.injWait[id] = nil
		X.injectStep(w, refusal or false)
	end
end

-- The native module failed LOUD (native/src/failsafe.rs, 2026-10-19): a hook could not do its part and this game may now differ from
-- the others. `repair`: one thing went wrong (a tool's command reached the engine at the click, ...): the player is told and the host
-- is asked for the repair. `stop`: a determinism piece is missing for the rest of this game run (a patch not in place), so every repair
-- would fork again: the player is told to leave and restart the game. A stop fault is the module's state (a reloaded script finds it
-- in the history and says it again); an old repair fault is past. At most one repair request per 10 s.
function X.natFaultEvent(p, history)
	local place, detail, action = tostring(p.where or "?"), tostring(p.detail or ""), tostring(p.action or "repair")
	if history and action ~= "stop" then return end
	logGui(string.format("native module FAULT [%s] (%s): %s", place, action, detail))
	if action == "stop" then
		pcall(vetoChat, "natstop:" .. place, "game_cannot_sync")
	else
		pcall(vetoChat, "natfault:" .. place .. ":" .. math.floor(C.clock() / 60), "game_fault")
	end
	local now = C.clock()
	if not history and (X.natFaultAt == nil or now - X.natFaultAt >= 10) then
		X.natFaultAt = now
		pcall(send, "replay_failed", { detail = "native module fault [" .. place .. "] (" .. action .. "): " .. detail:sub(1, 160) })
	end
end

local function natEvent(N, rec, p, history)
	local kind = rec.kind
	local now = C.clock()
	-- any line the module wrote while this script runs proves it is alive (a `completed`, an `injected`, a `timing` as much as a `pong`)
	if not history then N.aliveAt = now end
	if kind == "timing" and p.what == "clock" then
		-- written right after `ready` (history too): the wall clock of this process's C runtime clock, for the timing lines
		local detail = tostring(p.detail or "")
		local w, c = tonumber(detail:match("wall=(%d+)")), tonumber(detail:match("clock=(%-?%d+)"))
		if w and c then
			X.WALL0 = w - c
			pcall(toSim, "nativecaps", { patch = N.ready and N.ready.patch, inject = N.ready and N.ready.inject, terrain = N.ready and N.ready.terrain, wall0 = X.WALL0 })
			traceGui("build.timing", string.format("clock wall0=%.0f (native wall=%.0f clock=%.0f; this script's C.clock()=%.3f -> wall %.0f)", X.WALL0, w, c, C.clock(), X.wallNow()))
		end
		return
	end
	if kind == "ready" then
		local hooks = {}
		for _, h in ipairs(type(p.hooks) == "table" and p.hooks or {}) do hooks[#hooks + 1] = tostring(h) end
		N.ready = { build = p.build, hooks = hooks, patch = p.patch == true }
		for _, h in ipairs(hooks) do
			if h == "patch_next" then N.ready.patch = true end
			if h == "inject_next" then N.ready.inject = true end
			if h == "defer" then N.ready.defer = true end
			if h == "batch_pace" then N.ready.pace = true end   -- native pace.rs: `pace` lines stretch the batch interval
			if h == "terrain_apply" then N.ready.terrain = true end   -- native terrain.rs: `terrain_share` and `terrain_apply`
		end
		pcall(toSim, "nativecaps", { patch = N.ready.patch, inject = N.ready.inject, terrain = N.ready.terrain, wall0 = X.WALL0 })
		N.hookOk = false
		for _, h in ipairs(hooks) do
			if h == "command_list_add" then N.hookOk = true end
		end
		-- a (new) module link: whatever it held is gone
		for _, h in pairs(N.heldByKey) do h.done = true end
		-- a (new) module link: the handshake starts again
		N.holdSent, N.active, N.pongAt, N.pingsTried, N.gaveUp, N.retryAt, N.retries = false, false, nil, 0, false, nil, 0
		N.gaveUpAt, N.gaveUpWhy, N.rearms = nil, nil, 0
		if N.hookOk then
			natSay("native module found (build " .. tostring(p.build) .. ", hooks " .. table.concat(hooks, ",") .. ")")
			traceGui("version", string.format("player=%s name=%s role=%s native_build=%s native_version=%s hooks=%s record_version=%s strict_cancel=%s fire_done=%s", tostring(C.PLAYER_ID),
				tostring(C.PLAYER_NAME), (C.PLAYER_ID == 0) and "host" or "guest", tostring(p.build), tostring(p.version), table.concat(hooks, ","), tostring(Build and Build.VERSION),
				tostring(STRICT_CANCEL), tostring(FIRE_DONE)))
		else
			natSay("mode is off: the native module has no command_list_add hook (hooks: " .. table.concat(hooks, ",") .. ")")
		end
	elseif kind == "pong" then
		if not history then N.pongAt = now end   -- an answer from before this script started proves nothing
	elseif kind == "holding" then
		if p.on == true then
			if N.holdSent and not history and not N.active then
				N.active, N.activeSince = true, now
				natSay("mode ON (the native module holds build commands; hold_on confirmed)")
				X.terrainShareOn(N)
			elseif p.reason == "resume" and not history and not N.holdSent and not N.gaveUp then
				-- the module held again on this game's first line after a silence (the pause menu stops every script): same as a confirmed hold_on
				N.holdSent, N.holdAt, N.active, N.activeSince, N.retryAt = true, now, true, now, nil
				natSay("mode ON again (the native module resumed holding after a silence: the game's scripts had stopped, e.g. the pause menu)")
				X.terrainShareOn(N)
			end
		elseif p.on == false and not history and p.reason ~= "hold_off" then
			-- the module switched holding off by itself (silence, control file gone): ours to notice, and to try again later
			if N.holdSent then
				N.holdSent, N.active, N.parked, N.caps, N.owed = false, false, {}, {}, {}
				N.retries = N.retries + 1
				if N.retries > 3 then
					X.natGiveUp("the native module switched holding off (" .. tostring(p.reason) .. ") " .. N.retries .. " times")
				else
					N.retryAt = now + 3 * N.retries
					natSay("mode is off: the native module switched holding off (" .. tostring(p.reason) .. "); trying again in " .. (3 * N.retries) .. " s")
				end
			end
		end
	elseif kind == "fault" then
		X.natFaultEvent(p, history)
	elseif history then
		return   -- held / released / dropped / audit of an earlier run of this script: not news
	elseif kind == "held" then
		natOnHeld(N, p)
	elseif kind == "terrain" then
		pcall(X.shipTerrain, p)
	elseif kind == "terrain_refused" then
		natSay("terrain: an edit was refused by the native module: " .. tostring(p.reason))
		pcall(X.noticeLocal, "terrain_refused")
	elseif kind == "injected" or kind == "inject_refused" then
		traceGui("build.inject", kind .. " " .. tostring(pcall(C.serialize, p) and C.serialize(p) or "?"))
		if kind == "inject_refused" then X.onInjectRefused(p) elseif tonumber(p.key) then X.injectAnswered(tonumber(p.key), false) end
	elseif kind == "timing" then
		local what, detail = tostring(p.what), tostring(p.detail or "")
		if what == "capture" or what == "input" then
			local id = tonumber(p.id)
			if id then
				N.timing = N.timing or {}
				local t = N.timing[id] or {}
				N.timing[id] = t
				t[what] = detail
			end
			traceGui("build.timing", what .. " " .. detail)
		elseif what == "apply" then
			-- the build's one timing line on this game: the mod's stages (shipped with the record), the rebuild and the apply
			local click = tonumber(detail:match("click=(%d+)"))
			local apply = tonumber(detail:match("apply=(%d+)"))
			local stamp, step = tonumber(detail:match("stamp=(%d+)")), tonumber(detail:match("step=(%d+)"))
			traceGui("build.timing", string.format("timing %s total_ms=%s late_steps=%s", detail,
				(click and apply and click > 0) and tostring(apply - click) or "?", (stamp and step) and tostring((step - stamp) / STEP) or "?"))
			-- the build.lag line: this game's own deferred build waits for its tool callback (completed), any other build is written now
			local key = detail:match("key=(%S+)")
			local waits = false
			for _, d in pairs(N.deferred or {}) do
				if key and d.key == key then waits = true end
			end
			if waits then
				X.lagWait = X.lagWait or {}
				X.lagWait[key] = { detail = detail, t = now }
			else
				X.buildLag(detail, nil)
			end
		end
	elseif kind == "completed" then
		local id = tonumber(p.id)
		local d = id and N.deferred[id]
		if id then N.deferred[id] = nil end
		local lw = d and X.lagWait and X.lagWait[d.key]
		if lw then
			X.lagWait[d.key] = nil
			X.buildLag(lw.detail, X.wallNow())
		end
		if p.reason ~= nil and p.reason ~= "stamp_apply" then
			natSay(string.format("held %s (%s) FAILED without a stamp apply (%s): tool callback fired with a failed result %s at game time %d, stamp %s",
				tostring(id), d and d.key or "?", tostring(p.reason), tostring(p.fired), engine.gameTime(), d and tostring(d.at) or "?"))
		else
			natSay(string.format("held %s (%s) completed with this game's stamp apply (accepted %s, tool callback fired %s) at game time %d, stamp %s",
				tostring(id), d and d.key or "?", tostring(p.ok), tostring(p.fired), engine.gameTime(), d and tostring(d.at) or "?"))
		end
	elseif kind == "patched" or kind == "patch_refused" then
		traceGui("build.patch", kind .. " " .. tostring(pcall(C.serialize, p) and C.serialize(p) or "?"))
		if kind == "patch_refused" then natSay("the native module refused the frozen-node patch: " .. tostring(p.reason or p.detail)) end
	elseif kind == "released" or kind == "dropped" then
		local id = tonumber(p.id)
		if id then
			N.awaiting[id] = nil
			if N.deferred[id] then
				natSay("held " .. id .. " (" .. N.deferred[id].key .. ") was deferred and is " .. kind .. " (" .. tostring(p.reason) .. ") without its completion")
				N.deferred[id] = nil
			end
			for i, h in ipairs(N.parked) do
				if h.id == id then
					table.remove(N.parked, i)
					break
				end
			end
			local rec = N.byId[id]
			if rec and not rec.done then
				rec.done = true
				if kind == "dropped" then
					natSay("held " .. id .. " destroyed by the module (" .. tostring(p.reason) .. ")")
				elseif not rec.released then
					-- given back by the module itself (silence, age, hold_off, the control file gone) before this game asked: the build
					-- enters this world before its stamp. The mark keeps the simulation half from shipping it again.
					-- no replay mark here: the build was applied before this line was read, so its event is already past; the entry the
					-- simulation half keeps for it ("heldout") is what stops the capture. It is dropped by the next build event.
					rec.released, rec.early = C.clock(), true
					natSay("held " .. id .. " (" .. rec.key .. ") was released by the module (" .. tostring(p.reason) .. ") before its stamp: the build is in this world early and is not shipped twice")
				end
			end
		end
	elseif kind == "applied" then
		local id = tonumber(p.id)
		local u = id and N.unaccounted[id]
		if u and not u.applied then u.applied, u.ok = C.clock(), p.ok ~= false end
		local rec = id and N.byId[id]
		if rec then
			rec.applied = true
			natSay(string.format("held %d (%s) applied by the engine (accepted %s) at game time %d, stamp %d", id, rec.key, tostring(p.ok), engine.gameTime(), rec.at or 0))
			-- every other game applied this build AT the stamp: the lag is the time this game's world differed from theirs (town growth
			-- reacting to a freed lot in that window is the drift of the real run 2026-10-13). FEVERLINK3_STRICT_CANCEL=removals closes it for demolitions.
			pcall(function()
				local lag = engine.gameTime() - (rec.at or 0)
				traceGui("strict.lag", string.format("%s released_at=%s stamp=%s seen_applied_at=%d lag_ms=%d lag_steps=%.1f", rec.key, tostring(rec.releasedAt), tostring(rec.at), engine.gameTime(), lag, lag / STEP))
			end)
			if rec.finish then N.finishes[#N.finishes + 1] = { rec = rec, ok = p.ok ~= false, frame = G.frames + 2 } end
		end
	elseif kind == "error" then
		natSay("native module error [" .. tostring(p.where) .. "]: " .. tostring(p.detail))
	end
end

local function natReadEvents(N)
	local recs = C.readerPoll(N.reader)
	if #recs == 0 then return end
	local history = not N.read   -- the first records are what the file held before this script started: state, not news
	N.read, N.seen = true, true
	-- the `held` events of one batch are paired together afterwards, so that two held at once are seen as two
	N.batching = true
	for _, rec in ipairs(recs) do
		local p = C.parse(rec.payload)
		if type(p) ~= "table" then p = {} end
		local ok, err = pcall(natEvent, N, rec, p, history)
		if not ok then natFault("event " .. tostring(rec.kind), err) end
	end
	N.batching = false
	local ok, err = pcall(natPair)
	if not ok then natFault("pairing", err) end
end

-- Every frame (guarded by the caller). Does nothing until native_events.log has a record.
function natTick()
	local N = G.nat
	if not N then return end
	if N.dead then
		-- three faults: off for X.NAT_FAULT_COOL s, then armed again with the count reset (the handshake below starts over)
		if not (N.deadAt and C.clock() - N.deadAt >= X.NAT_FAULT_COOL) then return end
		N.dead, N.deadAt, N.faults = false, nil, 0
		natSay("mode re-armed after three faults (" .. X.NAT_FAULT_COOL .. " s ago); the handshake starts again")
	end
	if not N.seen then
		-- without the module there is no file: look for it a few times a second, not every frame
		local t = C.clock()
		if t < (N.probeAt or 0) then return end
		N.probeAt = t + 0.25
	end
	natReadEvents(N)
	if not N.seen then return end
	local now = C.clock()
	-- a deferred command completes at a CommandList::Add after this game's stamp apply, once this interface's world has that apply: the
	-- tool's callback reads the entities of its result here. The interface's game time past the stamp says so (`release` of the
	-- deferred id); then give it an Add, a few times a second. (h1/h2, 2026-10-14: with a fast capture and rebuild the callback came
	-- at the first Add after the simulation's apply, before the interface had it; the road tool read a removed house and the game
	-- stopped.)
	if next(N.deferred) then
		local gt = engine.gameTime()
		for id, d in pairs(N.deferred) do
			-- (a build made during a pause waits for the host's stamp first: its `at` is only the pause stamp until then)
			if d.waitStamp then
				-- nothing: no release, no age, no trigger
			elseif gt > d.at and not d.ready then
				d.ready = true
				natSend("release", { id = id })
			end
			if gt >= d.at and not d.dueSeen then d.dueSeen = now end
			if d.dueSeen and gt >= d.at + X.DEFER_GRACE_GAME_MS and now - d.dueSeen > X.DEFER_GRACE_S and not d.failed then
				-- this game is well past the stamp (game time, so a pause menu does not count) and the build's stamp apply never completed
				-- the held command (the replay was refused, dropped or never ran): it fails now, its tool callback fired, instead of
				-- leaving the tool busy until the module's age limit
				d.failed = true
				natSay(string.format("held %d (%s) was not completed %d ms of game time after its stamp %d: fail written (tool callback fired with a failed result)", id, d.key, gt - d.at, d.at))
				natSend("fail", { id = id })
			elseif gt >= d.at and now - (d.trig or 0) > 0.15 then
				d.trig = now
				natTrigger()
			end
		end
	end
	if N.lastTick and now - N.lastTick > 3 then N.watchFrom = now end   -- the game stood still (a load, a save): the module could not be answered either
	N.lastTick = now
	natExpire(now)
	local okL, errL = pcall(natLedger, N, now)
	if not okL then natFault("ledger", errL) end
	local usable = Build ~= nil and type(Build.captureToolBuild) == "function"
	local reason = nil
	if N.dead then reason = "faults"
	elseif not usable then reason = "the build module has no tool capture"
	elseif N.hookOk == false then reason = "the native module has no command_list_add hook"
	elseif G.loading then reason = "a resync load is under way"
	elseif not natSessionOn() then reason = "the session ended" end
	if reason then
		if N.holdSent then natStop(reason) end
	elseif N.gaveUp then
		-- given up for now: keep asking (a ping every X.NAT_REARM_PING s); a pong newer than the give-up, after the back-off, arms it again
		-- (a new `ready` does too, see natEvent)
		local wait = math.min(X.NAT_REARM_MAX, X.NAT_REARM_MIN * 2 ^ math.min(N.rearms, 4))
		if N.pongAt and N.gaveUpAt and N.pongAt > N.gaveUpAt and now - N.gaveUpAt >= wait then
			N.rearms = N.rearms + 1
			natSay(string.format("mode re-armed: the native module answers again (pong %.1f s ago); it was off for %.0f s (%s); hold_on follows (re-arm %d)",
				now - N.pongAt, now - N.gaveUpAt, tostring(N.gaveUpWhy), N.rearms))
			traceGui("native", string.format("rearm n=%d off_s=%.1f why=%s", N.rearms, now - N.gaveUpAt, tostring(N.gaveUpWhy)))
			N.gaveUp, N.gaveUpAt, N.gaveUpWhy, N.pingsTried, N.retries, N.retryAt = false, nil, nil, 0, 0, nil
		elseif now - N.pingAt >= X.NAT_REARM_PING then
			natPing()
		end
	elseif not N.holdSent then
		if not (N.retryAt and now < N.retryAt) then
			if not (N.pongAt and now - N.pongAt < NAT_ALIVE) then
				if now - N.pingAt >= 2.5 then
					if N.pingsTried >= 6 then
						X.natGiveUp("the native module does not answer pings")
					else
						N.pingsTried = N.pingsTried + 1
						natPing()
					end
				end
			else
				N.pingsTried = 0
				if natSend("hold_on", { kinds = { "build_proposal" } }) then
					N.holdSent, N.holdAt, N.active = true, now, false
				else
					N.retryAt = now + 5
				end
			end
		end
	else
		-- a ping every NAT_PING s whatever else is written: the module answers only pings with a pong, so a session that writes other
		-- lines all the time (a build every 2 s: defer, inject_next, release) must still ping, or the watchdog below starves (rail-crash run)
		-- and at once when the session's game time stops or runs again (the module's held commands age only while it runs)
		if now - N.lastWrite >= NAT_PING or now - N.pingAt >= NAT_PING or X.sessionStill() ~= (N.stillSent == true) then natPing() end
		if not N.active then
			if now - N.holdAt > NAT_CONFIRM then
				X.natGiveUp("hold_on was not confirmed within " .. NAT_CONFIRM .. " s (is the hold hook installed?)")
				natStop("hold_on not confirmed")
			end
		elseif now - math.max(N.pongAt or 0, N.aliveAt or 0, N.watchFrom or 0, N.activeSince or 0) > NAT_WATCHDOG then
			X.natGiveUp(string.format("the native module stopped answering (no pong and no other event for %.1f s; last pong %.1f s ago)",
				now - math.max(N.pongAt or 0, N.aliveAt or 0), N.pongAt and (now - N.pongAt) or -1))
			natStop("the native module stopped answering")
		else
			if N.retries > 0 and now - N.activeSince > 30 then N.retries = 0 end
			if N.rearms > 0 and now - N.activeSince > 60 then N.rearms = 0 end
		end
	end
	-- a `held` whose capture did not come: decided after the grace without one (no record: refused); an order whose Add did not come: another trigger
	while N.parked[1] and now - N.parked[1].t >= NAT_GRACE and G.frames - N.parked[1].frame >= 3 do
		N.owed[#N.owed + 1] = now
		natDecide(table.remove(N.parked, 1), nil)
	end
	-- a game paused at the stamp finishes its bookkeeping a couple of frames after the engine applied the build (the entities show a
	-- moment later), or gives up after 4 s
	if #N.finishes > 0 then
		local keep = {}
		for _, f in ipairs(N.finishes) do
			if G.frames >= f.frame then
				local fin = f.rec.finish
				f.rec.finish = nil
				if fin then
					local okF, errF = pcall(fin, {}, f.ok, {})
					if not okF then natFault("finishing " .. f.rec.key, errF) end
				end
			else
				keep[#keep + 1] = f
			end
		end
		N.finishes = keep
	end
	heldWatchdog(N, now)
	for key, h in pairs(N.heldByKey) do
		if h.finish and h.released and now - h.released > 4 then
			local fin = h.finish
			h.finish = nil
			natSay("held " .. h.id .. " (" .. key .. "): the engine did not report the build within 4 s")
			pcall(fin, {}, false, {})
		end
		if now - h.t > 120 then
			N.heldByKey[key] = nil
			N.byId[h.id] = nil
		end
	end
	for id, a in pairs(N.awaiting) do
		if now - a.t > 0.6 * (a.tries + 1) then
			a.tries = a.tries + 1
			if a.tries > 5 then
				N.awaiting[id] = nil
				natSay(a.kind .. " of held " .. id .. " was never confirmed by the native module")
			else
				natTrigger()
			end
		end
	end
end
end -- section: strict mode: commits, the ledger, events

-- used outside this section:
local installWrap, readLink, collectFromSim, serviceWindow, syncLocalCompany
do -- section: the wrap, link handlers, the service window (not indented, see SECTIONS at the top)
-- A notice for this player alone (the window's notices, not the chat): message `msg` of feverlink3_text.lua with `args`, e.g. an
-- action of theirs that was refused before it left this game. Nothing is sent to the others: nothing of it reached them.
local function noticeLocal(msg, args)
	local okT, text, level = pcall(T.say, msg, args)
	if not okT then return end
	logGui("told the player: " .. tostring(text))
	pcall(Hud.addNotice, G.hud, text, level, C.time())
end
X.noticeLocal = noticeLocal   -- also for a build refused at its stamp (X.tellRefused)

-- Another mod changed the world with a command FeverLink3 cannot share (simForeign: a kind no codec encodes): one warn notice per
-- session and mod. A command that is shared (captured on the host, dropped on the others) is not told.
function X.foreignTell(st)
	if type(st.foreign) ~= "table" then return end
	G.foreignTold = G.foreignTold or {}
	for _, f in ipairs(st.foreign) do
		local key = tostring(f.epoch) .. "|" .. tostring(f.mod)
		if f.epoch == G.epoch and not G.foreignTold[key] then
			G.foreignTold[key] = true
			logGui("another mod (" .. tostring(f.mod) .. ") sent " .. tostring(f.kind) .. " on its own: not shared, the worlds may drift")
			noticeLocal("foreign_mod_change", { what = Hud.modName(app, f.mod) or tostring(f.mod) })
		end
	end
end

-- Wraps the command factories (to know a command's name and arguments) and sendCommand (to hold it). A command the mod
-- replicates but cannot encode is refused on this game too, and the player is told: applied here alone it made the worlds
-- differ, and the next resync erased it (friend test 2026-10-16, the line edits). Only what no game replicates (category
-- "local": an event nobody registered) goes to the engine untouched; another mod's own event (L.markForeignEvent) is shipped.
-- UNDOCUMENTED API (docs/ARCHITECTURE.md section 2, "Undocumented API uses and why"): the command factories and sendCommand are replaced (no
-- documented command hook), and api.cmd gets the mod's own factories for its virtual actions (feverlink3Company).
function installWrap()
	local orig = originals()
	if not orig.sendCommand then
		orig.sendCommand = api.cmd.sendCommand
		orig.setSpeed = api.cmd.makeGameSetSpeedCmd
		orig.event = api.cmd.makeScriptingSendEventCmd
		orig.factory = {}
		for name in pairs(L.COMMANDS) do
			-- a build without this factory may refuse the lookup instead of answering nil (MPFever (MIT) looked each one up guarded)
			local okf, f = pcall(function() return api.cmd[name] end)
			if okf then orig.factory[name] = f end
		end
	end
	O.sendCommand, O.setSpeed, O.event, O.factory = orig.sendCommand, orig.setSpeed, orig.event, orig.factory
	local wrapped = 0
	for name, fn in pairs(orig.factory) do
		if fn ~= nil then
			local okw = pcall(function()
				api.cmd[name] = function(...)
					local cmd = fn(...)
					if cmd ~= nil then G.tags[cmd] = { name = name, args = { n = select("#", ...), ... } } end
					return cmd
				end
			end)
			if okw then wrapped = wrapped + 1 end
		end
	end
	pcall(function()
		api.cmd.makeGameSetSpeedCmd = function(...)
			local cmd = orig.setSpeed(...)
			if cmd ~= nil then G.tags[cmd] = { name = "makeGameSetSpeedCmd", args = { n = select("#", ...), ... } } end
			return cmd
		end
	end)
	-- companies: a mod-level action (spec.virtual) has no engine function, so its factory only makes the tag. Always made
	-- anew: after a savegame reload the old closure would hold the previous run's tags.
	for name, spec in pairs(L.COMMANDS) do
		if spec.virtual then
			pcall(function()
				api.cmd[name] = function(...)
					local cmd = {}
					G.tags[cmd] = { name = name, args = { n = select("#", ...), ... } }
					return cmd
				end
			end)
		end
	end
	local oks = pcall(function()
		local wrapper
		wrapper = function(cmd, cb, progress)
			local tag = cmd ~= nil and G.tags[cmd] or nil
			if tag == nil then return O.sendCommand(cmd, cb, progress) end
			-- another mod's own script event, sent in this state (a mod's game-script interface half): shipped like the player's commands
			-- (L.markForeignEvent, the same rule as the UI hook's); one that is not plain data stays on this game as before
			if tag.name == "makeScriptingSendEventCmd" and tag.args.foreign == nil and X.live() then
				local okW, _, mod = pcall(C.callerOf, wrapper, X.ownSources())
				if okW and mod then
					local okM, why = L.markForeignEvent(tag.args, mod, C.plain)
					if not okM and why ~= "registered" then
						logGui("another mod's event " .. tostring(tag.args[2]) .. "/" .. tostring(tag.args[3]) .. " (mod=" .. tostring(mod) .. ") stays on this game unshared: " .. tostring(why))
					end
				end
			end
			if not X.live() then
				-- no session (none began, or it ended: X.sessionOff): every command goes straight to the engine as in single-player, the
				-- speed buttons included. A mod-level action has no engine command: it fails.
				if L.COMMANDS[tag.name] and L.COMMANDS[tag.name].virtual then
					if cb then pcall(cb, {}, false, {}) end
					return
				end
				return O.sendCommand(cmd, cb, progress)
			end
			if tag.name == "makeGameSetSpeedCmd" then
				-- the player's speed buttons become requests to the host; the session speed comes back in `session`
				send("speed_request", { speed = tag.args[1] })
				return
			end
			local category = L.categoryOf(tag.name, tag.args)
			-- what no game replicates (an event nobody registered: notifications, tutorial steps; a player's own setting such as the
			-- Local time-of-day mode) is this game's own business, as in single-player; the normal case, not worth a line each
			if category == "local" then return O.sendCommand(cmd, cb, progress) end
			if not L.permitted(G.perms, category) then
				-- cancelled before stamping: nothing is sent to the other games, the window learns it failed
				logGui("command " .. tag.name .. " refused: not allowed to " .. category)
				send("action_refused", { seq = 0, detail = "permission denied: " .. category })
				if cb then pcall(cb, {}, false, {}) end
				return
			end
			-- companies: the player's spending limit, checked at once so the window hears about it; every game checks again at the stamp
			local okP, okG, why = true, true, nil
			if Co and G.co then
				okP, okG, why = pcall(function()
					return Co.gate(G.co, C.PLAYER_ID, category, engine.gameTime(), Co.freeOfLimit(tag.name, tag.args))
				end)
			end
			if okP then
				if not okG then
					logGui("command " .. tag.name .. " refused: " .. tostring(why))
					send("action_refused", { seq = 0, detail = why })
					if cb then pcall(cb, {}, false, {}) end
					return
				end
			end
			local ok, err = shipLocal(tag, cb)
			if ok then return end
			if L.COMMANDS[tag.name] and L.COMMANDS[tag.name].virtual then
				-- a mod-level action has no engine command to fall back on
				logGui("action " .. tag.name .. " not sent: " .. tostring(err))
				if cb then pcall(cb, {}, false, {}) end
				return
			end
			-- a build has a second way to every game: the simulation half captures it where the engine applies it
			-- (onPreBuildProposal) and ships it from there, as the UI hook leaves every build to it
			if tag.name == "makeWorldBuildProposalCmd" then
				logGui("command " .. tag.name .. " goes to the engine, the simulation half ships it from its apply: " .. tostring(err))
				return O.sendCommand(cmd, cb, progress)
			end
			-- fail closed: applied here alone it would fork the worlds
			logGui("command " .. tag.name .. " REFUSED: it cannot be replicated, so it is applied nowhere (not on this game alone either): " .. tostring(err))
			noticeLocal("action_not_shared", { thing = T.thing(tag.name) })
			if cb then pcall(cb, {}, false, {}) end
			return
		end
		api.cmd.sendCommand = wrapper
	end)
	G.wrapped = oks
	logGui("wrapped " .. wrapped .. " command factories" .. (oks and "" or "; sendCommand is read-only, commands stay native"))
end

local H = {}

H.session = function(rec, p)
	G.session.self = C.PLAYER_ID   -- this game's id, for L.catchUp
	Hud.setSession(G.hud, p)
	local loaded, asked = G.session.loadedAt, type(p) == "table" and tonumber(p.pauseAt) or nil
	if loaded and asked and asked < loaded and G.session.pauseAt ~= loaded then
		logGui("session: pauseAt " .. C.integerText(asked) .. " is before this world's load at " .. C.integerText(loaded)
			.. " (the pause of the world before the resync load): the pause is " .. C.integerText(loaded))
	end
	if L.applySession(G.session, p) then
		logGui("session: started=" .. tostring(G.session.started) .. " speed=" .. G.session.speed .. " pauseAt=" .. tostring(G.session.pauseAt) .. " lead=" .. tostring(G.session.lead)
			.. ((G.session.stalledKey or "") ~= "" and (" stalled=" .. G.session.stalledKey) or ""))
	end
	-- the stall timeout (host/stall.rs): this game was stalled and catches up, or it has caught up
	local mine = X.selfStalled()
	if mine ~= (G.wasStalled == true) then
		G.wasStalled = mine
		local now = engine.gameTime()
		local far = L.furthestPeer(G.peers)
		if mine then
			logGui("stall: the host has this game as stalled (its clocks did not come for the stall timeout; the others ran on): catching up from t="
				.. C.integerText(now) .. (far and (", " .. string.format("%.0f", (far - now) / STEP) .. " step(s) behind the furthest peer") or "") .. "; every action is stamped by the host until it has caught up")
		else
			logGui("stall: caught up at t=" .. C.integerText(now) .. ": actions are stamped by this game again")
			pcall(Hud.setBehind, G.hud, nil)
		end
	end
end

H.peerclock = function(rec, p)
	L.applyPeerClock(G.peers, p, C.PLAYER_ID)
	if X.LS then pcall(X.linkRead, "peerclock", type(p) == "table" and tonumber(p.from) or nil, p) end
end

H.player_left = function(rec, p)
	if type(p) == "table" and p.id ~= nil then
		L.removePeer(G.peers, p.id)
		if X.LS and G.ls then X.LS.forget(G.ls, p.id) end
		-- a parked build whose originator left is never committed
		for key, pk in pairs(G.parked) do
			if pk.it.from == p.id then
				G.parked[key] = nil
				G.vetoed[key] = "originator left"
				logGui("build " .. key .. " was parked and its originator left: dropped")
			end
		end
	end
	Hud.playerLeft(G.hud, p)
end

H.player_joined = function(rec, p)
	Hud.playerJoined(G.hud, p)
end

H.welcome = function(rec, p)
	Hud.applyWelcome(G.hud, p)
end

-- The host's grant for this player: which categories of local action are allowed.
H.permissions = function(rec, p)
	if L.applyPermissions(G.perms, p) then
		logGui("permissions: build=" .. tostring(G.perms.build) .. " bulldoze=" .. tostring(G.perms.bulldoze) .. " spend=" .. tostring(G.perms.spend) .. " other=" .. tostring(G.perms.other))
	end
end

-- Host authority at a checkpoint: this game's money is corrected to the host's values, measured at the same round.
H.authority = function(rec, p)
	if C.PLAYER_ID == 0 or type(p) ~= "table" or type(p.values) ~= "table" or type(p.n) ~= "number" then return end
	if not applyAuthority(p) then
		-- this game has not reached the round yet: keep the values until it reports it
		G.authPending[p.n] = p
		G.authPending[p.n - 40] = nil
	end
end

-- The itemised keys of this game's last full checksum round (dumpdetail, detail_s<sync>_<time>_<player>.txt), written by EVERY game
-- of a sync while it still has its own world: the host where it saves (H.resync_save), every other game before its load replaces that
-- world (H.resync_load; join, return, start and repair syncs all end in it). Until 0.2.2 only the host's save wrote one, so the guest's
-- world before the load was lost and a drift could not be diffed (friend test 2026-10-21). Once per sync: the host, which saves and
-- then loads, writes one file. Returns true when the dump of this sync is done (asked earlier), false when it was asked now: the
-- simulation half writes it at its next event and answers with st.detailAck (collectFromSim), which a load waits for (G.pendingLoad).
-- Reads only the keys kept from the last round; no world is read and no command changes the world.
function X.syncDetail(p, where)
	local sync = tonumber(p.id) or 0
	if G.detailSync == sync then return true end
	G.detailSync = sync
	G.detailAsked = tostring(G.epoch) .. "/" .. tostring(sync)
	traceGui("resync", string.format("sync %s (%s) %s: the itemised keys of the last full checksum round of this game follow in detail_s%s_*.txt",
		tostring(sync), tostring(p.name), where, tostring(sync)))
	pcall(toSim, "dumpdetail", { why = "sync " .. tostring(sync) .. " " .. where .. " " .. tostring(p.name), sync = sync, ack = G.detailAsked })
	return false
end

-- The simulation half has written this sync's detail file, or it did not answer in DETAIL_WAIT_S (the load goes on: a missing file
-- must not hold a sync). Called every frame (commitPump).
local DETAIL_WAIT_S = 3
function X.detailWritten()
	if G.simDetailAck == G.detailAsked then return "written" end
	if C.clock() - G.pendingLoad.since > DETAIL_WAIT_S then return "NOT confirmed by the simulation half within " .. DETAIL_WAIT_S .. " s" end
	return nil
end

H.resync_save = function(rec, p)
	if type(p) ~= "table" or type(p.name) ~= "string" then logGui("malformed resync_save"); return end
	local function save()
		logGui("resync: saving the game as '" .. p.name .. "'")
		traceGui("resync", string.format("saving as %s id=%s now=%s balance=%s", tostring(p.name), tostring(p.id), tostring(engine.gameTime()), tostring(engine.account())))
		pcall(X.syncDetail, p, "save")
		engine.saveGame(p.name, function(ok, err)
			logGui("resync: save " .. (ok and "done" or ("failed: " .. tostring(err))))
			-- the mods this world uses, read from the save just made: the host compares them with every player's before
			-- any of the save is sent (docs/PROTOCOL.md, "Mod list"); nil when they cannot be read (the host then warns)
			local mods, why = nil, nil
			if ok then mods, why = Hud.worldMods(app, p.name) end
			if why then logGui("resync: MOD WARNING: the world's mods could not be read: " .. tostring(why)) end
			send("save_done", { id = p.id, name = p.name, ok = ok == true, detail = (not ok) and tostring(err) or nil, mods = mods })
		end)
	end
	-- no held build may stay held across the save (a held command is in no world, a queued replay is in the save): settle them first
	local okS, waiting = pcall(natSettle, "resync save")
	if okS and waiting and waiting > 0 then
		logGui("resync: " .. waiting .. " held build(s) are released first; the game is saved when the engine has applied them")
		G.pendingSave = { since = C.clock(), go = save }
		return
	end
	save()
end

-- Loading restarts the scripts, and with them this reader. The place right after this very line, and the place the window's records
-- were read to, are written to the cursor file first, so the reloaded mod resumes there and nothing already dispatched is dispatched
-- again, nothing after it is lost; its hello (sent by the normal start-up) tells the host this game is back.
H.resync_load = function(rec, p)
	if type(p) ~= "table" or type(p.name) ~= "string" then logGui("malformed resync_load"); return end
	logGui("resync: loading '" .. p.name .. "'")
	-- a reload while the native module holds commands is unsafe (docs/NATIVE_LINK.md stage 2, H): holding goes off first. The
	-- reloaded script finds `ready` in native_events.log and switches strict mode on again.
	pcall(natStop, "resync load")
	local after = C.recordEnd(rec)
	C.writeFile(C.path(C.CURSOR), C.posText(after) .. " " .. C.posText(C.readerPos(G.uiReader)))
	Hud.writeSnapshot(C, G.hud, { version = nil })
	-- the lines after this one belong to the reloaded game from here on, also while the load waits for the detail dump below
	G.loading = true
	local function load(deferred)
		-- the loaded game waits for this file's start (X.readyLoad / X.readyBegin / X.readyGate)
		X.readyAsked = false
		pcall(X.readyLoad)
		local ok, err = engine.loadGame(p.name)
		if ok then
			pcall(X.readyLoaded)
		else
			G.loading = false
			-- a load that waited for the dump: the lines read with this one after it were skipped (readLink), so the link is read
			-- again right after this line and they are dispatched as without the wait (a load at once goes on in the same batch)
			if deferred and G.reader then C.readerNext(G.reader, after.gen, after.offset) end
			C.writeFile(C.path(C.CURSOR), "")
			logGui("resync: load failed: " .. tostring(err))
			send("replay_failed", { detail = "resync_load failed: " .. tostring(err) })
		end
	end
	-- this world's detail first (X.syncDetail): app.loadGame tears the world down within the frame (friend test 2026-10-21: the
	-- interface's destroy events in the same second as "resync: loading"), before the simulation half would run a dump sent with it
	local okD, done = pcall(X.syncDetail, p, "load")
	if not okD or done then
		load()
	else
		G.pendingLoad = { since = C.clock(), go = function() load(true) end }
	end
end

H.action = function(rec, p)
	if X.slotQueued("action", rec, p) then return end   -- numbered after an open slot: waits for the slot's record (X.slotHold)
	pcall(X.pauseOrderSeen, p)   -- before anything else: an item of the pause order counts for its flush even when it is dropped here
	if type(p) == "table" and p.allowed == false and rec.from == C.PLAYER_ID and p.pauseSeq ~= nil then
		-- this game's own paused action, refused by the host's permissions: applied nowhere, this one included (it waited for the echo)
		local key = L.itemKey(rec.from, p.seq)
		local own = G.pauseOwn and G.pauseOwn[key]
		if G.pauseOwn then G.pauseOwn[key] = nil end
		logGui("action " .. key .. " made during the pause came back with allowed=false: applied on no game")
		if own and own.deferId then X.sendHeldOrder("fail", { id = own.deferId }, key) end
		if own and own.heldId and natHeldRefused then pcall(natHeldRefused, own) end
		if own and own.slot then pcall(X.natRefuseHeld, own.slot.h, "the host refused its place in the pause order (no permission)", "build_not_allowed") end
		deliverResult(key, nil, false, {})
		return
	end
	if type(p) == "table" and p.allowed == false then
		-- the host relayed an action its sender had no permission for: it is not applied here, and since the
		-- sender's game may have run it, this game asks for a resync
		logGui("action " .. tostring(rec.from) .. ":" .. tostring(p.seq) .. " relayed with allowed=false: not applied, requesting a resync")
		send("replay_failed", { detail = "action " .. tostring(rec.from) .. ":" .. tostring(p.seq) .. " was not allowed" })
		return
	end
	local it, err = L.actionItem(p, rec.from, C.parse)
	if not it then logGui("action from " .. tostring(rec.from) .. " dropped: " .. tostring(err)); return end
	it.tRecv, it.gRecv = X.wallNow(), engine.gameTime()
	if X.LS then pcall(X.linkRead, "action", tonumber(rec.from), p) end
	if type(p.from) == "number" and p.from ~= rec.from then logGui("action payload claims sender " .. p.from .. ", launcher says " .. tostring(rec.from)) end
	if G.vetoed[it.key] then logGui("action " .. it.key .. " was vetoed already: dropped"); return end
	-- made during a pause: applied in the host's pause order, by every game (this one's own came back too)
	if type(p.pauseSeq) == "number" then return X.pauseArrived(it, p.pauseSeq) end
	-- the record of the build whose place in the pause order every game holds at (X.slotHold)
	if G.slot and it.body and it.body.pauseSlot == G.slot.key then return X.slotContent(it) end
	if it.body and it.body.fn == "makeWorldBuildProposalCmd" then
		noteBuild(it.key, it.at)
		if type(it.body.xs) == "number" and not pausedNow() then
			-- the measured stamp margin (L.marginSample): how early this build arrived, and what that says about the margin needed
			local okM, sample, slack, need, how = pcall(L.marginSample, G.margin, G.session.speed, it.body.xs, it.at, it.gRecv, C.clock())
			if okM then
				traceGui("build.margin", string.format("%s recv g%s stamp %s slack=%d steps extra_used=%s target=%d sample=%d need=%d (%s) x%s", it.key,
					string.format("%.0f", it.gRecv), tostring(it.at), slack, tostring(it.body.xs), L.marginTarget(G.session.speed), sample, need, how, tostring(G.session.speed)))
			end
		end
		if it.body.held == true then
			-- the originator holds its command until the stamp and cancels it on a veto: dry-run now, veto when this game cannot build it
			local now = engine.gameTime()
			if it.body.commit == true then
				-- the originator's game is paused: dry-run now, then park the record until its commit arrives (a veto in between drops it)
				local okV, errV = pcall(vetoCheck, it, now)
				if not okV then logGui("veto check of " .. it.key .. " failed: " .. tostring(errV)) end
				if G.vetoed[it.key] then return end
				G.parked[it.key] = { it = it, wall = C.clock() }
				logGui("build " .. it.key .. " parked until its originator commits it (it holds its own command through a grace)")
				return
			end
			if L.applyNow(G.session, now, it) or not pendingBefore(it, now) then
				local okV, errV = pcall(vetoCheck, it, now)
				if not okV then logGui("veto check of " .. it.key .. " failed: " .. tostring(errV)) end
				if G.vetoed[it.key] then return end   -- vetoed by this game: it never enters the queue
			else
				G.dry[it.key] = it
			end
		end
	end
	route(it)
end

-- A game could not build a held strict build (rec.from is that game). Every game drops the queued replay; the originator
-- cancels its held command and tells its player.
-- A notice about this player's action for every player (the window's notices, not the chat), once per `key` and session: message
-- `msg` of feverlink3_text.lua, "Your ..." here, "<name>'s ..." for the others. The host relays it as a chat line with a level, which
-- every game files under its notices (H.chat). The technical cause stays in the caller's log line.
vetoChat = function(key, msg, args)
	G.vetoChatted = G.vetoChatted or {}
	if G.vetoChatted[key] then return end
	G.vetoChatted[key] = true
	local text, level = T.say(msg, args)
	logGui("told the player: " .. text)
	pcall(Hud.addNotice, G.hud, text, level, C.time())
	send("chat", { text = (T.say(msg, args, C.PLAYER_NAME)), level = level })
end

H.build_veto = function(rec, p)
	if type(p) ~= "table" or type(p.seq) ~= "number" or type(p.owner) ~= "number" then logGui("malformed build_veto"); return end
	local key = L.itemKey(p.owner, p.seq)
	local why = tostring(p.why or "?")
	local by = tonumber(p.from) or tonumber(rec.from)
	local who = tostring(p.name or by)
	-- the host's own refusal (host/stall.rs: an action of a stalled game stamped before the promise the host kept for it) holds for every
	-- game, the sender's and the host's own included
	local byHost = p.host == true and tonumber(rec.from) == 0
	if by == C.PLAYER_ID and not byHost then return end
	if G.slot and G.slot.key == key then
		-- the originator gave up the slot (no record of its build): nothing of it is applied here, the waiting numbers follow
		X.slotClose("its originator has no record of the build (" .. why .. "): nothing of it is applied here", nil, nil)
		return
	end
	G.vetoed[key] = why
	G.dry[key] = nil
	toSim("drop", { key = key, why = who .. ": " .. why })
	logGui("VETO of " .. key .. " by " .. who .. ": " .. why)
	G.graces[key], G.parked[key] = nil, nil
	if byHost and p.owner == C.PLAYER_ID then
		-- this game's own action: nothing of it may stay here (queued: dropped above; held: cancelled; a window waits: it fails)
		local pending = G.pauseOwn and G.pauseOwn[key]
		if G.pauseOwn then G.pauseOwn[key] = nil end
		local cancelled = natVeto(key, who, why)
		if pending and pending.deferId then X.sendHeldOrder("fail", { id = pending.deferId }, key) end
		deliverResult(key, nil, false, {})
		logGui("stall: action " .. key .. " of this game was refused by the host on every game (" .. why .. ")")
		X.noticeLocal("action_undone_catching_up")
		if cancelled == false then
			send("replay_failed", { detail = "action " .. key .. " was applied here before the host refused it (stalled game): " .. why:sub(1, 100) })
		end
		return
	end
	if p.owner == C.PLAYER_ID then
		local cancelled = natVeto(key, who, why)
		local args = { who = who, cause = T.cause(why) }
		if cancelled == nil then
			-- not held here (the old flow): the veto changes nothing on this game
		elseif cancelled then
			vetoChat(key, "build_refused_elsewhere", args)
		else
			-- the command was released already: the build exists on this game, the vetoing game dropped it. The games differ now.
			vetoChat(key, "build_only_here", args)
			logGui("veto of " .. key .. " came too late: this game has the build, " .. who .. " does not; asking the host for a resync")
			send("replay_failed", { detail = "build " .. key .. " exists on one game only (a veto came after it was applied): " .. why:sub(1, 100) })
		end
	end
end

-- The originator of a parked build commits it: apply it now (the pause time) or at the stamp it names.
H.build_commit = function(rec, p)
	if X.slotQueued("commit", rec, p) then return end   -- as in H.action
	pcall(X.pauseOrderSeen, p)   -- as in H.action
	if type(p) ~= "table" or type(p.seq) ~= "number" or type(p.owner) ~= "number" then logGui("malformed build_commit"); return end
	local key = L.itemKey(p.owner, p.seq)
	local by = tonumber(p.from) or tonumber(rec.from)
	if by ~= p.owner then logGui("build_commit of " .. key .. " from " .. tostring(by) .. " ignored: only its originator commits"); return end
	if type(p.pauseSeq) == "number" then
		-- committed during the pause: applied in the host's pause order by every game, the originator too (its build waited in G.pauseOwn)
		local it
		if p.owner == C.PLAYER_ID then
			it = G.pauseOwn and G.pauseOwn[key]
			if G.pauseOwn then G.pauseOwn[key] = nil end
		else
			local pk = G.parked[key]
			G.parked[key] = nil
			it = pk and pk.it
		end
		if not it or G.vetoed[key] then logGui("commit of " .. key .. " (pause order #" .. p.pauseSeq .. "): nothing to apply here (vetoed, expired or not received)"); return end
		if type(p.stamp) == "number" then it.at = p.stamp end
		logGui("build " .. key .. " committed by its originator during the pause: pause order #" .. p.pauseSeq)
		it.pauseSeq = p.pauseSeq
		X.pauseStamped(it)
		route(it)
		return
	end
	local pk = G.parked[key]
	if not pk then logGui("commit of " .. key .. ": nothing parked for it (vetoed, expired or not received)"); return end
	G.parked[key] = nil
	if G.vetoed[key] then return end
	if type(p.stamp) == "number" then pk.it.at = p.stamp end
	noteBuild(key, pk.it.at)
	logGui("build " .. key .. " committed by its originator: applied at " .. tostring(pk.it.at) .. " (now " .. engine.gameTime() .. ")")
	route(pk.it)
end

-- a number of the pause order that waited for a slot's record (X.slotClose), handled now as it arrived
function X.slotReplay(kind, rec, p)
	return (kind == "commit" and H.build_commit or H.action)(rec, p)
end

H.checksum_request = function(rec, p)
	local it = L.checksumItem(p)
	if not it then logGui("malformed checksum_request"); return end
	local now = engine.gameTime()
	-- the round's stamp, and for a spread round the step of every chunk it may read (L.checksumMarks), are landing marks: every game
	-- reaches each of them exactly (L.pace). Appended here rather than through L.addMark's short list, which only prunes the marks passed:
	-- a spread round's marks would otherwise crowd out the stamp of the next round.
	L.addMark(G.marks, nil, now)
	if it.at > now then G.marks[#G.marks + 1] = it.at end
	local marks = L.checksumMarks(it)
	if #marks > 0 then
		G.csMarks = G.csMarks or {}
		G.csMarks[it.n] = marks
		for _, m in ipairs(marks) do G.marks[#G.marks + 1] = m end
	end
	route(it)
end

-- A line with a level (info/warn/error) is a system message, the session's (the host's announcements) or another player's game's
-- (vetoChat): it goes to the window's notices. A line without one is a player's chat.
H.chat = function(rec, p)
	if type(p) ~= "table" then return end
	if T.LEVELS[tostring(p.level)] then
		logGui("notice " .. tostring(p.name or rec.from) .. ": " .. tostring(p.text))
		Hud.addNotice(G.hud, p.text, p.level, C.time())
	else
		logGui("chat " .. tostring(p.name or rec.from) .. ": " .. tostring(p.text))
		Hud.addChat(G.hud, p.name, p.text, p.from or rec.from)
	end
end

function readLink()
	for _, rec in ipairs(C.readerPoll(G.reader)) do
		if G.loading then
			-- a resync load is under way: the reloaded game reads these lines again from the cursor
			break
		elseif rec.from ~= 0 and rec.kind ~= "action" then
			logGui("line " .. C.posText(C.recordStart(rec)) .. " kind " .. rec.kind .. " from " .. tostring(rec.from) .. " ignored: only the host controls the session")
		else
			local h = H[rec.kind]
			if h then
				local p = C.parse(rec.payload)
				local ok, err = pcall(h, rec, p or {})
				if not ok then logGui("handler " .. rec.kind .. " failed: " .. tostring(err)) end
			end
		end
	end
end

-- Results, refusals, reports and bindings produced by the simulation half.
function collectFromSim(state)
	local ok, st = pcall(function() return state:get() end)
	if not ok or type(st) ~= "table" then
		-- silent before: a state the interface half cannot read stalls every simulation result and checksum round with no trace
		local why = ok and ("a " .. type(st)) or tostring(st)
		if G.stateFault ~= why then
			G.stateFault = why
			logGui("the game-script state cannot be read: " .. why)
		end
		return
	end
	G.stateFault = nil
	G.simDetailAck = st.detailAck   -- the detail dump a resync load waits for (X.detailWritten)
	-- companies: the mirror takes the state's registry when that is newer; during a pause it is ahead of the state
	if Co and type(st.co) == "table" and (G.co == nil or (st.co.rev or 0) > (G.co.rev or 0)) then
		coGuard("mirror", function() G.co = C.plain(st.co) end)
	end
	-- the mirror has now been held against the state's registry (the one the save loaded) at least once: a paused round may hash it
	if Co and type(st.co) == "table" and G.co ~= nil and not G.coReady then
		G.coReady = true
		traceGui("checksum", "company registry read from the game-script state (rev " .. tostring(G.co.rev) .. ")")
	end
	for key, e in pairs(st.bind or {}) do
		if G.bind[key] ~= e then G.bind[key] = e; G.rev[e] = key end
	end
	-- Outputs of the simulation half belong to the run of this interface half that the state is stamped for: a state still stamped for an
	-- earlier run (a save made mid-session) holds that run's results, refusals, reports and native builds, none of which may be read.
	if st.epoch ~= G.epoch then
		local stale = #(st.reports or {}) + #(st.results or {}) + #(st.refused or {}) + #(st.out or {})
		if stale > 0 and G.staleSeen ~= stale then
			G.staleSeen = stale
			logGui("ignoring " .. C.integerText(stale) .. " stored output(s) (reports, results, refusals, native builds) of an earlier session until the simulation half takes the new session")
		end
		if G.frames % 30 == 0 then toSim("session", G.sessionMsg or { epoch = G.epoch }) end
		return
	end
	G.notes = st.notes
	pcall(X.foreignTell, st)
	local highest = nil
	for _, o in ipairs(st.out or {}) do
		if not G.outSeen[o.n] then
			G.outSeen[o.n] = true
			local okN, okShip, whyShip = pcall(shipNative, o)
			if not okN then logGui("shipping a native build failed: " .. tostring(okShip))
			elseif not okShip then logGui("a native build was not shipped: " .. tostring(whyShip)) end
		end
		if highest == nil or o.n > highest then highest = o.n end
	end
	if highest ~= nil and G.outAcked ~= highest then
		G.outAcked = highest
		toSim("outack", { n = highest })
	end
	for _, r in ipairs(st.results or {}) do
		if not G.resultsSeen[r.key] then
			G.resultsSeen[r.key] = true
			deliverResult(r.key, r.data, r.success, r.entities or {})
			-- strict mode: the simulation half reached the stamp of a build whose own copy this game's native module holds
			if r.release and r.of and G.nat then pcall(natRelease, r.of) end
			-- this game's build.result of its player's own build: a refusal notice may wait for it (X.tellRefused)
			if r.outcome and r.of then pcall(X.buildOutcome, r.of, r.outcome) end
			-- a build applied at its stamp: this game's verdict, for the host (X.verdictOf)
			if r.verdict then
				local okV, errV = pcall(X.verdictOf, r)
				if not okV then logGui("the verdict of " .. tostring(r.of) .. " could not be sent: " .. tostring(errV)) end
			end
		end
	end
	if G.noticeWait then pcall(X.noticeTimeouts, C.clock()) end
	for _, r in ipairs(st.refused or {}) do
		if not G.refusedSent[r.key] then
			G.refusedSent[r.key] = true
			if G.nat then pcall(natHeldRefused, { key = r.key }) end
			-- Refused at its stamp from this game's own world: this game's verdict, no resync of its own. Identical worlds refuse alike;
			-- the host compares every game's verdict and repairs only when they differ (X.refusalClass, lab run 2026-10-18).
			X.sendVerdict(r, false, r.detail, r.class, r.partial)
			if r.partial then
				-- the engine threw while it applied the command (simApplyAction, D): the world may be partly built, so the repair now
				X.partlyBuilt(r.key, r.detail, "at its stamp")
			elseif r.build then
				logGui("action " .. tostring(r.key) .. " refused at its stamp (" .. tostring(r.class) .. "): nothing of it is applied here; the worlds may well be equal (the host compares the games' verdicts)")
				pcall(X.tellRefused, r, r.detail)
			end
			deliverResult(r.key, nil, false, {})
		end
	end
	for _, r in ipairs(st.reports or {}) do
		if not G.reportsSent[r.n] then
			traceGui("checksum.collect", "round " .. C.integerText(r.n) .. " read from the game-script state (account " .. tostring(r.account and r.account.balance) .. ")")
			reportRound(r.n, r.time, r.parts, r.account, r.cost)
		end
	end
	if (st.late or 0) > G.lateSeen then
		G.lateSeen = st.late
		logGui("the simulation applied an action late (" .. st.late .. " so far): the worlds may have forked, asking the host for a repair")
		send("replay_failed", { detail = "late apply of " .. tostring(st.lateKey) .. " (" .. tostring(st.lateLast) .. " step(s) after its stamp): the input barrier was breached, the entity ids may have forked" })
		-- the measured stamp margin grows at once (see L.marginSample): a late apply must not happen twice
		local need = L.marginLate(G.margin, G.session.speed, st.lateLast)
		traceGui("build.margin", string.format("late apply (%s steps late): need now %d steps at x%s", tostring(st.lateLast), need, tostring(G.session.speed)))
	end
end

-- companies: the window's company page is a snapshot file of its own (companies.txt = Co.status, docs/COMPANIES.md
-- section 6), rewritten when the registry or the roster changed, and every few seconds while there are lines whose
-- result moves with the game.
-- The lines' results are read in passes (X.coPage): one pass every X.CO_PAGE_EVERY_S and one when the registry or the roster
-- changed (the page is written at once with the results it has, again when the pass ends), each call of this function reading lines
-- until it has spent X.CO_PAGE_BUDGET_S (one line at least), the file written with the results of the last finished pass. It read
-- every line in one frame before (one engine.lineResult, a balance since the game's start, per line, every 5 s): a frame that stalls
-- sends no clock line, and every peer waits at its input barrier for it (bug 4: friend test 2026-10-16, holds of 0.3 s and more on a
-- 4.7-4.8 s median cadence; frame.slow's companies= shows what it cost).
-- The budget is checked between reads, so one read that is slow by itself is not sliced: it was (friend test 2026-10-18,
-- companies= 450-610 ms in one frame twice every 5 s: calculateBalance on the entity of a deleted line, engine.lineResult now reads
-- only live lines). Whatever else makes one read slow, a line whose single read took more than X.CO_READ_SLOW_S is not read again
-- while its key stays bound to that entity (it keeps the result it had; logged once): at most one such frame per line and binding.
X.CO_PAGE_EVERY_S, X.CO_PAGE_BUDGET_S, X.CO_READ_SLOW_S = 5, 0.002, 0.05
local function writeCompanies()
	if not (Co and G.co) then return end
	local P = X.coPage or { cache = {}, pass = 0, slow = {} }
	X.coPage = P
	local clock = C.clock()
	local seen = tostring(G.co.rev) .. ":" .. tostring(G.hud.version)
	if P.keys == nil and (P.nextAt == nil or clock >= P.nextAt or P.seen ~= seen) then
		-- the keys first, the pass only once they are read (a registry that cannot be read fails here, the same way every call: coGuard
		-- logs it once)
		local keys = {}
		for key, m in pairs(G.co.made) do
			if m.kind == "LINE" then keys[#keys + 1] = key end
		end
		P.keys, P.i, P.fresh, P.at, P.seen = keys, 1, {}, engine.gameTime(), seen
		if #P.keys == 0 then P.keys, P.cache, P.nextAt = nil, {}, clock + X.CO_PAGE_EVERY_S end
	end
	if P.keys then
		repeat
			local key = P.keys[P.i]
			local e = G.bind[key]
			if e ~= nil and P.slow[key] == e then
				P.fresh[key] = P.cache[key]
			elseif e ~= nil then
				local t0 = C.clock()
				local ok, v = pcall(engine.lineResult, e, 0, P.at)
				if ok and type(v) == "number" then P.fresh[key] = v end
				local took = C.clock() - t0
				if took > X.CO_READ_SLOW_S then
					P.slow[key], P.fresh[key] = e, P.fresh[key] or P.cache[key]
					logGui(string.format("company page: reading the result of line %s (entity %s) took %d ms in one engine call: not read again while that key names that entity", tostring(key), tostring(e), math.floor(took * 1000 + 0.5)))
				end
			end
			P.i = P.i + 1
		until P.i > #P.keys or C.clock() - clock >= X.CO_PAGE_BUDGET_S
		if P.i > #P.keys then
			P.cache, P.keys, P.fresh, P.pass, P.nextAt = P.fresh, nil, nil, P.pass + 1, clock + X.CO_PAGE_EVERY_S
		end
	end
	local stamp = seen .. ":" .. tostring(P.pass)
	if G.coWritten == stamp then return end
	local names = {}
	for id, p in pairs(G.hud.roster) do names[id] = p.name end
	local now = engine.gameTime()
	local earned = Co.earned(G.co, function(key) return P.cache[key] end)
	local okY, year = pcall(engine.yearLength)
	local okM, month = pcall(engine.monthLength)
	local view = {
		self = C.PLAYER_ID, names = names, now = now, earned = earned, yearLength = okY and year or nil, monthLength = okM and month or nil, perms = G.perms,
		hostEntityKey = L.keyFor(G.rev, engine.canonicalPlayer and engine.canonicalPlayer() or engine.player()),
	}
	local okS, text = pcall(function() return C.serialize(Co.status(G.co, view)) end)
	if okS and C.writeFile(C.path("companies.txt"), text) then G.coWritten = stamp end
end

-- The window's side of the hand-off: chat typed there arrives as records in ui_out.log, the model goes back as a
-- snapshot file. Throttled: a few frames apart is plenty for a chat box.
-- A record of ui_out.log belongs to this session when it names it (`sid`, the UI hook's commands and speed requests), or, without a
-- name (the window's chat), when it lies after the place ui_out.log ended when this session began here (X.uiFloor). The file outlives
-- sessions; a record of an earlier one is never acted on (its window was answered, or failed, when that session ended), and never answered
-- here: a new window state numbers its commands from 1 again, so an answer to an old id could hit one of its own.
function X.uiRecordOfSession(rec, p)
	if type(p) == "table" and p.sid ~= nil then return p.sid == G.sid end
	return X.uiFloor == nil or not C.posBefore(C.recordStart(rec), X.uiFloor)
end

function serviceWindow()
	if G.frames % 3 ~= 0 then return end
	local ignored = 0
	-- Window commands of one group (the UI hook's `grp`, one handler of the window) that follow each other are one order. This half
	-- runs between the window's handlers, never inside one, so one poll here sees all of a handler's records.
	local order = nil
	local function shipOrder()
		local ps = order
		order = nil
		if not ps then return end
		local okN, okShip, whyShip = pcall(shipUiCommand, ps)
		if not okN then logGui("shipping a window order failed: " .. tostring(okShip))
		elseif not okShip then logGui("a window order of " .. #ps .. " command(s) was not shipped, none of it: " .. tostring(whyShip)) end
		-- the window holds them: it learns that they were applied nowhere (their callbacks run with a failure)
		if not (okN and okShip) then
			for _, p in ipairs(ps) do pcall(uiReply, p.id, nil, nil, false, {}, tostring(okN and whyShip or okShip)) end
			noticeLocal("action_not_shared", { thing = "action" })
		end
	end
	for _, rec in ipairs(C.readerPoll(G.uiReader)) do
		local inSession = X.uiRecordOfSession(rec, C.parse(rec.payload))
		local p = (inSession and rec.kind == "cmd") and C.parse(rec.payload) or nil
		local cmd = type(p) == "table" and type(p.body) == "string" and type(p.id) == "number"
		if not (cmd and order and p.grp ~= nil and order[1].grp == p.grp) then shipOrder() end
		if not inSession then
			ignored = ignored + 1
		elseif cmd then
			order = order or {}
			order[#order + 1] = p
		elseif rec.kind == "chat" then
			local p = C.parse(rec.payload)
			local text = type(p) == "table" and Hud.cleanText(p.text) or nil
			if text then
				send("chat", { text = text })
				Hud.addChat(G.hud, C.PLAYER_NAME, text, C.PLAYER_ID)   -- the host does not echo a message to its sender
			end
		elseif rec.kind == "notice" then
			-- the UI hook refused a window command it cannot replicate (feverlink3_uihook.lua, refuse): the player reads why here
			local p = C.parse(rec.payload)
			if type(p) == "table" and type(p.msg) == "string" then noticeLocal(p.msg, { thing = T.thing(p.cmd) }) end
		elseif rec.kind == "speed" then
			-- a speed button of the window: the session speed is the host's to decide, so it is a request, like the
			-- one the game's own speed buttons make (0 pauses)
			local p = C.parse(rec.payload)
			local n = type(p) == "table" and tonumber(p.n) or nil
			if n and n >= 0 and n <= 4 and n == math.floor(n) then send("speed_request", { speed = n }) end
		elseif rec.kind == "company" and Co then
			-- an operation typed in the company page (Co.windowCommand), sent like any command the player issues
			coGuard("window record", function()
				local p = C.parse(rec.payload)
				local cmd = type(p) == "table" and Co.windowCommand(p, api) or nil
				if cmd and G.wrapped then
					api.cmd.sendCommand(cmd)
				elseif cmd then
					logGui("company action not sent: sendCommand is not wrapped, it would apply on this game alone")
				end
			end)
		end
	end
	shipOrder()
	if ignored > 0 then logGui("window: " .. ignored .. " record(s) of ui_out.log belong to an earlier session: ignored") end
	Hud.writeSnapshot(C, G.hud, G.hudWritten)
	local okC0, c0 = pcall(C.clock)
	coGuard("page", writeCompanies)
	local okC1, c1 = pcall(C.clock)
	if X.fw and okC0 and okC1 and type(c0) == "number" and type(c1) == "number" then X.fw.companies = (c1 - c0) * 1000 end   -- the company page is rewritten every 5 s while lines exist (frame.slow)
	if G.frames % 30 == 0 then
		-- serialized only when the bindings changed (their count and the sum of their entities): one key per entity a session made
		-- grows without end, and making its text every 30 frames to compare it cost a frame more the longer the game ran
		local n, sum = 0, 0
		for _, e in pairs(G.bind) do n, sum = n + 1, sum + (tonumber(e) or 0) end
		local sig = tostring(n) .. ":" .. tostring(sum)
		if sig ~= G.bindsSig then
			local okB, text = pcall(C.serialize, G.bind)
			if okB and (text == G.bindsWritten or C.writeFile(C.path("binds.txt"), text)) then G.bindsWritten, G.bindsSig = text, sig end
		end
	end
end

-- companies: in separate mode (experimental, docs/COMPANIES.md section 8) this game's windows act as the player's own
-- company, by making api.engine.util.getPlayer say so; in shared mode, or outside any company, as the game's own player.
function syncLocalCompany()
	if not (Co and G.co and engine.setLocalCompany) then return end
	local want = nil
	if G.co.mode == "separate" then
		local c = G.co.companies[G.co.members[C.PLAYER_ID]]
		if c and c.entity then want = L.resolveKey(G.bind, c.entity) end
	end
	if want ~= G.localCompany then
		G.localCompany = want
		local ok = engine.setLocalCompany(want)
		logGui("local company: " .. (want and ("entity " .. tostring(want)) or "the game's own player") .. (ok and "" or " (could not be set)"))
	end
end

-- The presence lock: world.lock in the session folder, opened once by this interface script and never closed, so the handle lives exactly as
-- long as this script's Lua state. The session controller (launcher/src/live.rs, PresenceLock) takes a lock that nobody holds as the
-- positive sign that the world was closed to the main menu; the pause menu only stops the script, the handle stays open.
X.WORLD_LOCK = nil
function X.holdWorldLock()
	if X.WORLD_LOCK or not C.IO then return end
	X.WORLD_LOCK = C.openHeld(C.path("world.lock"))
	if X.WORLD_LOCK then logGui("presence lock world.lock held for the life of this world's script") end
end

-- A guest whose session ends while its world is still there (the host stopped, it was removed, the connection was lost, it left) saves its
-- world first with the game's own save, so nothing it built is lost; the game itself is never closed by FeverLink3.
function X.guestSaveOnEnd()
	local role = C.ROLE   -- identity.txt as C.poll last read it
	if not role then return end
	local was = G.lastRole
	G.lastRole = role
	if was == "player" and role == "idle" and not G.guestSaved then
		G.guestSaved = true
		local name = "FeverLink3 guest " .. C.date("%Y-%m-%d %H-%M-%S")
		logGui("the session ended for this guest while its world is open: saving it as '" .. name .. "' first")
		engine.saveGame(name, function(ok, err)
			logGui("guest save '" .. name .. "': " .. (ok and "saved" or ("FAILED: " .. tostring(err))))
		end)
	elseif role == "player" then
		G.guestSaved = false
	end
end

-- A session runs on this game: identity.txt names one (C.ACTIVE) and this interface half started in that very one. Everything the
-- session does to the player's commands (the wrap's stamping and shipping, the speed requests) asks this.
function X.live()
	return C.ACTIVE == true and G.started and G.gen == C.GEN
end

-- THE end of a session on this game, one transition back to single-player: identity.txt says `idle` (the host ended it, the player
-- left or was removed, the world was closed, the controller gave up reconnecting) or names another session. Until 2026-10-15 nothing
-- switched the mod off: after the session the wrap still stamped and shipped every command to nobody, the speed stayed forced by the
-- pacing, a stale peer's promise could freeze the game at the input barrier, and the next session in the same Lua state inherited all of
-- it. What it does, in this order:
--   1. tells the simulation half (`sessionoff`, X.simSessionOff; sent before anything below, and events run in order): its queue of
--      stamped actions goes, so nothing of the ended session is applied there later, nor twice with what follows;
--   2. hands this game's held native commands back to the engine (natStop: hold_off): a held build is applied NOW. The batch-2 rule (a
--      shipped build is never applied at another time than the stamp its peers use) protects the peers' worlds; the session is over,
--      there are no peers any more, so the player's build lands as in single-player. A deferred command (its build was to come from
--      this game's stamp apply, which no longer happens) is failed by the module at hold_off: its tool callback fires with a failed
--      result, nothing is applied, the tool lives on. A build parked for a paused game's grace is one of the held ones (released);
--   3. answers what the simulation half applied or refused at its stamps already (its results in the state), then fails the callbacks of
--      commands that were stamped and not applied (the wrap's own, and the window commands the UI hook holds: they get a failed answer),
--      since the engine never had them; one applied from this state during a pause is with the engine already and keeps its callback;
--   4. drops what the session knew: peers and their promises (the input barrier), the session state, checksum marks, the pause order
--      (queued and own items), graces, parked and deferred dry runs, vetoes, authority values, the stamp margin, permissions;
--   5. gives the speed back: the session's speed when it was running, else the speed the game had when the session began; from now
--      on the wrap passes the player's speed buttons to the engine;
--   6. remembers where the link stands (G.linkFloor), so a next session in this Lua state never dispatches a line of this one.
function X.sessionOff(why, state)
	if X.readyHold then pcall(X.readyStart, "the session ended: " .. tostring(why)) end
	pcall(X.terrainRestore)
	if not G.started then return end
	G.started = false
	local s = G.session
	local n = { held = 0, deferred = 0, pending = 0, peers = 0, pause = 0, graces = 0, parked = 0 }
	-- what the simulation half applied (or refused) at its stamps before the end is answered as it happened, not failed below
	if state then
		pcall(function()
			local st = state:get()
			if type(st) ~= "table" or st.epoch ~= G.epoch then return end
			for _, r in ipairs(st.results or {}) do
				if not G.resultsSeen[r.key] then
					G.resultsSeen[r.key] = true
					deliverResult(r.key, r.data, r.success, r.entities or {})
				end
			end
			for _, r in ipairs(st.refused or {}) do
				if not G.refusedSent[r.key] then
					G.refusedSent[r.key] = true
					deliverResult(r.key, nil, false, {})
				end
			end
		end)
	end
	pcall(toSim, "sessionoff", { sid = G.sid })
	local N = G.nat
	if N then
		for _, h in pairs(N.heldByKey) do
			if not (h.released or h.done or h.cancelled) then n.held = n.held + 1 end
		end
		for _ in pairs(N.deferred) do n.deferred = n.deferred + 1 end
		local okS, errS = pcall(natStop, why)
		if not okS then logGui("session over: switching strict mode off failed: " .. tostring(errS)) end
	end
	G.nat = nil   -- the next session starts strict mode afresh (natNew)
	local pending = G.pending
	G.pending = {}
	for key, cb in pairs(pending) do
		if X.engineHas and X.engineHas[key] then
			G.pending[key] = cb   -- applied from this state during a pause: the engine runs it and its answer reaches the callback
		else
			n.pending = n.pending + 1
			if G.pendingArgs then G.pendingArgs[key] = nil end
			local okC, errC = pcall(cb, nil, false, {})
			if not okC then logGui("session over: the callback of " .. tostring(key) .. " failed: " .. tostring(errC)) end
		end
	end
	for _ in pairs(G.peers) do n.peers = n.peers + 1 end
	for _ in pairs(G.pauseOwn or {}) do n.pause = n.pause + 1 end
	n.pause = n.pause + #(G.pauseQ or {})
	for _ in pairs(G.graces) do n.graces = n.graces + 1 end
	for _ in pairs(G.parked) do n.parked = n.parked + 1 end
	G.peers, G.session, G.marks = {}, L.newSession(), {}
	G.graces, G.parked, G.dry, G.buildAts, G.vetoed = {}, {}, {}, {}, {}
	G.pauseQ, G.pauseOwn, G.pauseLast, G.slot = nil, nil, nil, nil
	G.bresOutcome, G.bresOrder, G.noticeWait, G.builtLate = nil, nil, nil, nil   -- build keys start again in the next session (X.buildOutcome)
	-- where this game stood still at the pause (the clock's `stop`); the run on to a host's pause stamp (pauseRun) goes with G.session
	G.still, X.curStop, G.lastStop = nil, nil, nil
	G.promiseFloor, G.promisePause, G.barrier = nil, nil, nil
	pcall(X.paceEnd, "the session ended")   -- the engine's own batch interval again (smooth pacing)
	G.ls, X.lagWait, X.actHop, X.frameAt = nil, nil, nil, nil   -- the latency logs start afresh with the next session
	X.coPage, X.clockSentAt = nil, nil   -- the company page's line results (keys of this session) and the clock keepalive
	X.pauseRunWant = nil                 -- a run-on of the pause order waiting for the rest of its flush
	X.guiParked, X.guiMaking = nil, nil  -- actions waiting for an engine answer (their callbacks failed with the pending ones above)
	G.authPending, G.accountAt, G.authLast, G.pendingSave, G.pendingLoad = {}, {}, nil, nil, nil
	G.perms, G.margin = L.newPermissions(), L.newMargin()
	G.resultsSeen, G.reportsSent, G.refusedSent, G.lateSeen = {}, {}, {}, 0
	G.injWait, G.injDone, G.injOrder = nil, nil, nil
	G.outSeen, G.outAcked, G.uiSeq, G.uiFn, G.vetoChatted = {}, nil, {}, nil, nil
	G.loading = false
	G.linkFloor = C.posMax(G.linkFloor, G.reader and C.readerPos(G.reader) or nil)
	G.reader, G.writer = nil, nil
	if G.localCompany ~= nil and engine.setLocalCompany then
		pcall(engine.setLocalCompany, nil)
		G.localCompany = nil
	end
	local sp = (s.started and (s.speed or 0) > 0) and s.speed or (G.speedBefore or 1)
	G.lastSpeedSet = nil
	local okV = pcall(function() O.sendCommand(O.setSpeed(sp)) end)
	pcall(Hud.setSession, G.hud, { started = false, speed = 0 })
	pcall(Hud.writeSnapshot, C, G.hud, G.hudWritten)
	logGui(string.format("session over (%s; identity.txt role %s): single-player again. Strict mode off with %d held command(s) handed back to the engine and %d deferred one(s) failed by the module;"
		.. " %d stamped command(s) not applied (callbacks failed); dropped %d peer(s), %d pause-order item(s), %d grace(s), %d parked build(s); speed %s%s; the link resumes at %s",
		tostring(why), tostring(C.ROLE), n.held, n.deferred, n.pending, n.peers, n.pause, n.graces, n.parked, tostring(sp), okV and "" or " (COULD NOT BE SET)", C.posText(G.linkFloor)))
	traceGui("session", string.format("off why=%s sid=%s held=%d deferred=%d pending=%d peers=%d pause=%d graces=%d parked=%d speed=%s floor=%s",
		tostring(why), tostring(G.sid), n.held, n.deferred, n.pending, n.peers, n.pause, n.graces, n.parked, tostring(sp), C.posText(G.linkFloor)))
end
end -- section: the wrap, link handlers, the service window

-- The start gate of a loaded game (join and resync both load through H.resync_load). The documented application functions
-- (https://wiki.transportfever3.com/script-doc/app.html, the interface state's `app`, as app.loadGame):
--   App.setWaitForStartReadyGame(): "Wait for the game to be ready to start. This is called when loading a game to prevent the game
--     from starting automatically"
--   App.isWaitForStartReadyGame(): boolean: "Return if the setWaitForStartReadyGame had been called for this game."
--   App.startReadyGame(): "Start Game that is fully loaded and ready to play. This is called after the progress page has finished
--     loading the game."
-- Without them a loaded game starts at once and runs steps until the mod's first frame has set speed 0 and the engine has applied it
-- (WIKI_AUDIT_20261018 E3: LATE actions and barrier breaches at join and resync). With them the game waits after the load and this
-- file starts it at the first frame in which the session lets it run (X.readyGate). The docs do not say whether this
-- script's guiUpdate runs while the game waits, nor whether the flag set before the load belongs to the loaded game (both logged:
-- "ready:" lines). The setSpeed(0) of the first frame stays as the fallback hold, and a game held longer than
-- FEVERLINK3_READY_WAIT_SECONDS (default 60) is started and left to it. FEVERLINK3_READY_WAIT=0 switches the gate off.
-- The app function `name` of this state, or nil. The engine's bound functions are not Lua functions: the 2026-10-18 lab run logged
-- `app` and its functions as tables in the interface state ("saveUserdata=table"; app.saveGame is called the same way and works),
-- and the gate's earlier `type(...) == "function"` test called them all missing. A table or userdata value counts as callable.
function X.appFn(name)
	local ok, f = pcall(function() return app[name] end)
	if not ok then return nil end
	local t = type(f)
	if t == "function" or t == "table" or t == "userdata" then return f end
	return nil
end

function X.readyLoad()
	-- Off unless FEVERLINK3_READY_WAIT=1 (lab 2026-10-18, run H-new: after setWaitForStartReadyGame the loaded game showed its own
	-- loading screen with a "Start Game" button and this script's guiUpdate never ran, so nothing released it: the resync waited
	-- until a player clicked. Only the window state ran there. Back on once the release comes from a state that runs.)
	if C.env("FEVERLINK3_READY_WAIT") ~= "1" then
		logGui("ready: the start gate is off (FEVERLINK3_READY_WAIT is not 1): the loaded game starts by itself and is held by speed 0 (the old path)")
		return
	end
	if not (X.appFn("setWaitForStartReadyGame") and X.appFn("startReadyGame")) then
		local okA, ta = pcall(function() return type(app) end)
		local okS, ts = pcall(function() return type(app.setWaitForStartReadyGame) end)
		local okR, tr = pcall(function() return type(app.startReadyGame) end)
		logGui(string.format("ready: app.setWaitForStartReadyGame / app.startReadyGame missing (app=%s setWait=%s start=%s): the loaded game starts by itself and is held by speed 0 (the old path)",
			okA and ta or "error", okS and ts or "error", okR and tr or "error"))
		return
	end
	local ok, err = pcall(app.setWaitForStartReadyGame)
	logGui("ready: app.setWaitForStartReadyGame() before the load " .. (ok and "called" or ("FAILED: " .. tostring(err))))
	X.readyAsked = ok
end

-- after app.loadGame was requested: the flag "for this game" may belong to the game that was running, so it is asked once more
function X.readyLoaded()
	if not X.readyAsked or not X.appFn("isWaitForStartReadyGame") then return end
	local okW, waiting = pcall(app.isWaitForStartReadyGame)
	if okW and waiting == true then return end
	local ok, err = pcall(app.setWaitForStartReadyGame)
	logGui("ready: isWaitForStartReadyGame()=" .. tostring(waiting) .. " after the load request: setWaitForStartReadyGame() again " .. (ok and "called" or ("FAILED: " .. tostring(err))))
end

-- Once per load of this file, at its first frame (before the session check, so a game that waits is never left waiting).
function X.readyBegin()
	X.readyChecked = true
	if not (X.appFn("isWaitForStartReadyGame") and X.appFn("startReadyGame")) then return end
	local ok, waiting = pcall(app.isWaitForStartReadyGame)
	if not ok or waiting ~= true then
		if C.ACTIVE and C.readFile(C.path(C.CURSOR)) and C.readFile(C.path(C.CURSOR)) ~= "" then
			logGui("ready: a resync load without the start wait (isWaitForStartReadyGame()=" .. tostring(waiting) .. "): held by speed 0 (the old path)")
		end
		return
	end
	X.readyHold = { since = C.clock() }
	if not C.ACTIVE then
		X.readyStart("the mod is not active")
		return
	end
	logGui("ready: this loaded game waits (isWaitForStartReadyGame()=true at the first frame, t=" .. C.integerText(engine.gameTime()) .. ") until the session lets it run")
	traceGui("ready", "hold t=" .. C.integerText(engine.gameTime()))
end

function X.readyStart(why)
	local h = X.readyHold
	if not h then return end
	X.readyHold = nil
	local ok, err = pcall(app.startReadyGame)
	logGui(string.format("ready: app.startReadyGame() after %.1f s at t=%s (%s)%s", C.clock() - h.since, C.integerText(engine.gameTime()), tostring(why),
		ok and "" or (" FAILED: " .. tostring(err))))
	traceGui("ready", "start why=" .. tostring(why) .. " ok=" .. tostring(ok))
end

-- Every frame of a session, with the speed the session gives. The game may run once the host's session line read after the load says
-- the session runs (started), whatever its speed: a session paused at the load (the pause of a resync) must still apply the pause's
-- actions, which only an engine that runs does (test_resync_pause). The first frame's setSpeed(0) is queued by then, so a paused
-- game starts at speed 0.
function X.readyGate(sp)
	local h = X.readyHold
	if not h then return end
	if G.session.started or (sp or 0) > 0 then
		X.readyStart("the session runs, speed " .. tostring(sp))
		return
	end
	local limit = tonumber(C.env("FEVERLINK3_READY_WAIT_SECONDS") or "") or 60
	if C.clock() - h.since >= limit then X.readyStart("not released within " .. tostring(limit) .. " s: started, held by speed 0 (the old path)") end
end

local function guiUpdate(userParams, state, guiState)
	C.poll()
	if not X.readyChecked then pcall(X.readyBegin) end
	if G.started and not X.live() then
		local okO, errO = pcall(X.sessionOff, C.ACTIVE and "another session began" or "the session ended", state)
		if not okO then
			G.started = false
			logGui("ending the session failed: " .. tostring(errO))
		end
	end
	pcall(X.guestSaveOnEnd)
	-- The presence lock is held whenever a world is loaded and the session controller is there (it wrote identity.txt, C.ROLE), also
	-- outside a session: an accepted Steam invite starts by itself only on a game that holds it (launcher live.rs invites_step).
	-- Friend test 2026-10-21: an idle player with a world loaded never took it here, so three join requests did nothing.
	if C.ACTIVE or C.ROLE then pcall(X.holdWorldLock) end
	if not C.ACTIVE then
		if G.nat and G.nat.holdSent then pcall(natStop, "the mod is not active") end
		if X.readyHold then pcall(X.readyStart, "the mod is not active") end
		return
	end
	G.frames = G.frames + 1
	if not G.started then
		-- a session begins on this game: the first frame of this Lua state in one, or the next one after X.sessionOff
		G.started, G.gen, G.sid = true, C.GEN, C.SESSION
		-- The link: lines at or below the session's start (identity.txt `start=`) are an earlier session's, and so is anything this Lua
		-- state dispatched before (G.linkFloor). The live folder keeps to_game.log across sessions; reading from its first line fed an
		-- earlier session's session, action and resync lines into this world.
		local floor = C.posMax(C.SESSION_START, G.linkFloor)
		G.reader = C.newReader(C.path(C.TO_GAME), floor and { at = floor } or nil)
		G.writer = C.newWriter(C.path(C.FROM_GAME), C.PLAYER_ID)
		G.hud = Hud.new(C.PLAYER_ID, C.PLAYER_NAME)
		G.co = Co and Co.new() or nil
		G.coReady, G.csWait = false, nil   -- the mirror is empty until collectFromSim reads the state's registry (X.guiChecksum)
		-- companies: a reload runs this file again in the same Lua state, so an override of the previous run may still be in place
		if Co and engine.setLocalCompany then engine.setLocalCompany(nil) end
		-- the window's records: kept reading where this state stopped; the ones without a session id count from where the file ends now
		-- (X.uiRecordOfSession)
		G.uiReader = G.uiReader or C.newReader(C.path(Hud.OUTBOX))
		X.uiFloor = C.logEnd(C.path(Hud.OUTBOX))
		-- after a resync load: resume the link right after the line that asked for it, and the window's records where they were read
		-- to, and keep the window's roster and chat
		local cursorText, uiText = (C.readFile(C.path(C.CURSOR)) or ""):match("^(%S+) (%S+)")
		local cursor, uiCursor = C.parsePos(cursorText), C.parsePos(uiText)
		local resumed = false
		if cursor and (floor == nil or C.posBefore(floor, cursor)) then
			resumed = true
			G.reader = C.newReader(C.path(C.TO_GAME), { at = cursor })
			if uiCursor then
				G.uiReader = C.newReader(C.path(Hud.OUTBOX), { at = uiCursor })
				X.uiFloor = uiCursor
			end
			C.writeFile(C.path(C.CURSOR), "")
			local restored = Hud.fromSnapshot(C.parse(C.readFile(C.path(Hud.SNAPSHOT)) or ""))
			if restored and restored.selfId == C.PLAYER_ID then G.hud = restored end
			-- every game loaded the same save and stands at its time: the session's pause cannot lie before it (L.applySession). The
			-- lines after the cursor still carry the pause of the world this game left (friend test 2026-10-16: 10341800, loaded at 10342400)
			L.loadedAt(G.session, engine.gameTime())
			logGui("reloaded after a resync at t=" .. C.integerText(engine.gameTime()) .. ": the link resumes at " .. C.posText(cursor) .. " of to_game.log, the window's records at " .. C.posText(uiCursor) .. " of ui_out.log")
		elseif cursor then
			C.writeFile(C.path(C.CURSOR), "")
			logGui("a resync cursor of an earlier session (" .. C.posText(cursor) .. ", this session starts at " .. C.posText(floor) .. ") is ignored")
		end
		if not X.wrapped then
			X.wrapped = true
			installWrap()
		end
		local okN, nat = pcall(natNew)
		G.nat = okN and nat or nil
		X.subscribeEvents(guiState, X.GUI_EVENTS, "gui", "the session's first frame", logGui)
		-- a game that waits for app.startReadyGame without a resync load to resume is not this session's to hold (X.readyBegin)
		if X.readyHold and not resumed then pcall(X.readyStart, "this load is no resync load of the session") end
		G.speedBefore = engine.speed()
		-- the old hold, kept under the start gate: it is what holds a load the gate could not hold (functions missing, the wait not
		-- carried over the load, the gate's time limit)
		setSpeed(0)
		G.epoch = tostring(C.time()) .. "-" .. tostring(math.floor(C.clock() * 1000)) .. "-" .. tostring(C.PLAYER_ID)
		-- `fresh`: a session begins in this world (no resync reload resumes one): the simulation half drops a saved queue of another session
		G.sessionMsg = { epoch = G.epoch, sid = C.SESSION, fresh = (not resumed) or nil }
		pcall(X.seqResume, state)
		toSim("session", G.sessionMsg)
		logGui("session " .. tostring(C.SESSION) .. " begins here (" .. (resumed and "resumed after a resync load" or "fresh") .. "): to_game.log from " .. C.posText(C.readerPos(G.reader)))
		traceGui("session", string.format("on sid=%s gen=%s start=%s floor=%s resumed=%s", tostring(C.SESSION), tostring(C.GEN), C.posText(C.SESSION_START), C.posText(C.readerPos(G.reader)), tostring(resumed)))
		send("hello", { time = engine.gameTime(), name = C.PLAYER_NAME })
		-- the promise goes out with the hello, before anything this game could ship (the input barrier, L.promise)
		X.reportClock(engine.gameTime())
		logGui("started: player " .. tostring(C.PLAYER_ID) .. " (" .. C.PLAYER_NAME .. ") dir " .. C.DIR)
	end
	if G.frames % 20 == 0 then devPoll("gui") end
	if G.frames % 60 == 0 and G.nat and G.nat.ready and (G.nat.ready.patch or G.nat.ready.inject or G.nat.ready.terrain) then pcall(toSim, "nativecaps", { patch = G.nat.ready.patch, inject = G.nat.ready.inject, terrain = G.nat.ready.terrain, wall0 = X.WALL0 }) end
	local ok, err = pcall(function()
		pcall(X.frameStart)
		readLink()
		pcall(X.frameMark, "read")
		serviceWindow()
		pcall(X.frameMark, "window")
		collectFromSim(state)
		pcall(X.frameMark, "collect")
		local okW, errW = pcall(X.checksumWaitTick)
		if not okW then logGui("checksum wait failed: " .. tostring(errW)) end
		local okT, errT = pcall(natTick)
		if not okT then natFault("frame", errT) end
		local okG, errG = pcall(commitPump)
		if not okG then logGui("commit pump failed: " .. tostring(errG)) end
		local okD, errD = pcall(dryPump)
		if not okD then logGui("veto check failed: " .. tostring(errD)) end
		local okP, errP = pcall(X.slotPump)
		if not okP then logGui("pause slot pump failed: " .. tostring(errP)) end
		pcall(X.frameMark, "nat")
		coGuard("local company", syncLocalCompany)
		local now = engine.gameTime()
		local ahead = L.aheadFor(G.session.speed, G.session.lead)
		local sp = L.pace(G.session, G.peers, now, ahead, G.marks)
		-- an action applied from here waits for its engine answer: no step before its keys reached the simulation half (X.guiHolds)
		if sp > 0 then
			local okH, holds = pcall(X.guiHolds)
			if okH and holds then sp = 0 end
		end
		pcall(X.readyGate, sp)
		setSpeed(sp)
		X.barrierWatch(now, sp)
		local okPc, errPc = pcall(X.paceTick, now)
		if not okPc and not X.paceFault then
			X.paceFault = true
			logGui("smooth pacing failed (the factor stays where it is; the native module goes back to 1 after 3 s): " .. tostring(errPc))
		end
		-- where this game really stands at a pause, for the host's stamps of the pause's actions (said at once when it changes)
		X.curStop = X.stopAt(now, sp)
		if now ~= G.lastClock or G.frames % 30 == 0 or X.clockDue() or X.curStop ~= G.lastStop then
			G.lastStop = X.curStop
			G.lastClock = now
			X.reportClock(now)
		end
		pcall(X.frameMark, "pace")
	end)
	if not ok then logGui("frame failed: " .. tostring(err)) end
end

-- The engine's tool events (builder.proposalCreate / builder.proposalApply, id = the tool): the construction schema logs
-- what it sees and what it would capture, so that the first game run shows the real shapes (finding from MPFever (MIT):
-- these events reach guiHandleEvent). Nothing is replicated from them except in strict mode (natCapture).
local seenOtherEvents, seenOtherN = {}, 0

-- Terrain edits are refused during a session until terrain is replicated (owner decision 2026-10-21, option A3). A terrain edit's
-- cells cannot be read from a script and the native module does not copy them, so the edit ran on its maker's game alone: friend test
-- friend test 2026-10-20, 2 of 8 free-id forks began with the friend's terrain edits ("a native build ... is not replicated: a terrain edit"), which
-- freed and re-created original-world entities on his game only. The terrain tools' commands cannot be held and cancelled like the
-- other tools' (their call site is not on the hold list; MPFever (MIT) left it out because holding its edit crashed the tool), so the
-- tools are switched off where the player picks them: every GUI component of the terrain menu (`landscaping_terrain`, seen in the logs as
-- menu.construction.landscaping.landscaping_terrain.::/gui/construction/tools/terrain_*_tool.res) is disabled as soon as an event names
-- it, and enabled again at the end of the session. Official API: api.gui.byId.setEnabled(idOrTag, enabled, idOnly?), "nil sets to
-- enabled and removes the rule" (tf3-docs reference/api/api/gui.html, Gui.ById; the base game's mission_sim.script.tl uses it the same
-- way). Nothing changes outside a session (guiHandleEvent returns before this when no session runs). The player is told once.
--
-- Height edits are shared since 2026-10-25 (owner decisions, TERRAIN_DESIGN step 2) when the native module says it can carry them
-- (`terrain_apply` in its hooks) and has been told to (X.terrainShareOn, once holding is on): raise, lower, flatten, smooth and Roughen
-- (terrain_heightmap_tool, owner 2026-10-25: its result cells travel as data, so its randomness, if any, runs on the originator only; the
-- module refuses an edit that carries anything beside its height grid), with the menu's own tab and indicator, then stay usable; anything
-- else the menu may hold stays switched off exactly as before. Ground paint and the tree, plant and rock brushes are not in this menu and
-- are unchanged (owner decision 1: not gated).
local TERRAIN_CARRIED = { "terrain_raise_tool.res", "terrain_lower_tool.res", "terrain_flatten_tool.res", "terrain_smooth_tool.res",
	"terrain_heightmap_tool.res" }

function X.terrainCarried(id)
	if id:sub(-4) == ".tab" or id:sub(-10) == ".indicator" then return true end
	for _, name in ipairs(TERRAIN_CARRIED) do
		if id:find("/" .. name, 1, true) then return true end
	end
	return false
end

function X.terrainGate(id)
	if type(id) ~= "string" or not id:find("landscaping_terrain", 1, true) then return end
	if G.terrainShared and X.terrainCarried(id) then
		G.terrainSeen = G.terrainSeen or {}
		G.terrainSeen[id] = true
		return
	end
	G.terrainOff = G.terrainOff or {}
	if G.terrainOff[id] then return end
	api.gui.byId.setEnabled(id, false, true)
	G.terrainOff[id] = true
	logGui("terrain tools: " .. id .. " switched off for this session (terrain edits are not copied to the other games yet)")
	if not G.terrainTold then
		G.terrainTold = true
		X.noticeLocal("terrain_off")
	end
end

-- Holding is on (hold_on confirmed) and the module can carry height edits: it is told to share them (`terrain_share {on = true}`: a
-- terrain tool's own command is emptied at its apply and its cells come back as a `terrain` event, X.shipTerrain), and the carried tools
-- this session switched off are enabled again. Said once per session to the player.
function X.terrainShareOn(N)
	if not (C.ACTIVE and N and N.ready and N.ready.terrain) then return end
	if not natSend("terrain_share", { on = true }) then return end
	G.terrainShared = true
	for id in pairs(G.terrainOff or {}) do
		if X.terrainCarried(id) and pcall(api.gui.byId.setEnabled, id, nil, true) then G.terrainOff[id] = nil end
	end
	natSay("terrain: height edits are shared (raise, lower, flatten, smooth, roughen)")
	if not G.terrainSharedTold then
		G.terrainSharedTold = true
		pcall(X.noticeLocal, "terrain_shared")
	end
end

-- hold_off (a fault, a resync load, the session's end): the module stops sharing by itself (terrain.rs), so the carried tools are switched
-- off again where an event names them, never left to apply on this game alone
function X.terrainShareOff()
	if not G.terrainShared then return end
	G.terrainShared = nil
	for id in pairs(G.terrainSeen or {}) do pcall(X.terrainGate, id) end
	G.terrainSeen = nil
	natSay("terrain: height edits are not shared any more (holding is off): the terrain tools are switched off again")
end

-- The originator: the module took a terrain tool's height edit out of its command (applied nowhere yet) and wrote its cells
-- (native_terrain_out_<id>.txt). It is stamped and shipped like every action, and queued on this game too: every game, this one
-- included, applies the same cells at the same stamp (the carrier, Build: decodeTerrain), never at the click (that forks). The stamp is
-- the shortest the pacing allows (stampAfterFastest, as a window command: no veto wait). An edit that cannot be shipped is applied
-- nowhere and the player is told. Every edit's size is logged for the lab (owner: the size cap is set after a run measures strokes).
function X.shipTerrain(p)
	local id, w, h, x0, y0 = tonumber(p.id), tonumber(p.w), tonumber(p.h), tonumber(p.x0), tonumber(p.y0)
	local function refuse(why)
		logGui(string.format("terrain edit %s (%sx%s cells) NOT shared: %s; it is applied on no game", tostring(id), tostring(w), tostring(h), why))
		pcall(X.noticeLocal, "terrain_refused")
	end
	if not (id and w and h and x0 and y0 and w >= 1 and h >= 1) then return refuse("the module's event is incomplete") end
	if not C.ACTIVE then return refuse("no session runs") end
	local okT, text = pcall(C.readFile, C.path("native_terrain_out_" .. C.integerText(id) .. ".txt"))
	if not okT or type(text) ~= "string" or text == "" then return refuse("its cells file cannot be read") end
	if not L.permitted(G.perms, "build") then return refuse("this player may not build") end
	local body = { fn = "makeWorldBuildProposalCmd", args = { n = 4 }, terrain = { x0 = x0, y0 = y0, w = w, h = h, cells = text }, category = "build" }
	local wire = C.serialize(body)
	if #wire > ACTION_BODY_MAX then return refuse(string.format("its record (%d bytes) is over the %d bytes an action may have", #wire, ACTION_BODY_MAX)) end
	local now = engine.gameTime()
	local at = stampAfterFastest(now)
	G.seq = G.seq + 1
	local it = { kind = "action", at = at, from = C.PLAYER_ID, seq = G.seq, body = body, key = L.itemKey(C.PLAYER_ID, G.seq), pause = X.hostStamps(at) or nil }
	send("action", { stamp = at, seq = G.seq, body = wire, category = "build", pause = it.pause })
	logGui(string.format("terrain edit %d: %dx%d = %d cells at %d,%d (%d bytes of cells, %d of record) %s stamped %s (now %s, %s step(s) ahead): applied at the stamp on every game, this one too",
		id, w, h, w * h, x0, y0, #text, #wire, it.key, C.integerText(at), C.integerText(now), tostring((at - now) / STEP)))
	X.routeOwn(it)
end

-- The proof of a terrain height edit after its apply, the same on every game with the same world: the documented
-- api.engine.terrain.getBaseHeightAt(Vec2f) at the centre of every edited cell (4 m cells, x = (x0 + i + 0.5) * 4: the grid's
-- resolution as TerrainTileHeightmap documents it; the exact centre is not proven, but every game samples the same points), hashed.
-- It goes with this game's applied verdict, and the host takes two applied verdicts with different proofs for a fork (core
-- VerdictBook): the repair follows, nothing is corrected after the fact.
function X.terrainProof(t, key, say)
	local ok, proof = pcall(function()
		local parts = {}
		for j = 0, t.h - 1 do
			for i = 0, t.w - 1 do
				parts[#parts + 1] = string.format("%.4f", api.engine.terrain.getBaseHeightAt(api.type.Vec2f.new((t.x0 + i + 0.5) * 4, (t.y0 + j + 0.5) * 4)))
			end
		end
		return string.format("%d:%08x", t.w * t.h, C.fnv1a(table.concat(parts, ",")))
	end)
	if not ok then proof = "unreadable (" .. tostring(proof):sub(1, 80) .. ")" end
	if say then say(string.format("terrain edit %s: %dx%d cells at %d,%d applied at the stamp, heights proof %s", tostring(key), t.w, t.h, t.x0, t.y0, proof)) end
	return proof
end

-- The end of a session: every terrain tool switched off by X.terrainGate is enabled again (the rule removed).
function X.terrainRestore()
	local n = 0
	for id in pairs(G.terrainOff or {}) do
		local ok, err = pcall(api.gui.byId.setEnabled, id, nil, true)
		if ok then n = n + 1 else logGui("terrain tools: " .. id .. " could not be switched on again: " .. tostring(err)) end
	end
	if n > 0 then logGui("terrain tools: " .. n .. " switched on again (the session ended)") end
	G.terrainOff, G.terrainTold, G.terrainShared, G.terrainSharedTold, G.terrainSeen = nil, nil, nil, nil, nil
end
-- UNDOCUMENTED API (docs/ARCHITECTURE.md section 2, "Undocumented API uses and why"): the `builder.*` event names are the tools' own; the docs
-- list no GUI event names.
local function guiHandleEvent(userParams, state, guiState, src, id, name, param)
	if not C.ACTIVE or type(name) ~= "string" then return end
	if not name:find("^builder%.") then
		local okT, errT = pcall(X.terrainGate, id)
		if not okT then logGui("terrain tools: switching off failed: " .. tostring(errT)) end
		-- A tool whose events are not "builder.*" is invisible to everything below. Each such event that smells of a tool is named once
		-- (real session 2026-10-13: the lane / crosswalk tool produced no event at all that any log shows).
		local key = tostring(id) .. "/" .. name
		if seenOtherN < 40 and not seenOtherEvents[key] and (key:find("[Mm]odifier") or key:find("[Ll]ane") or key:find("[Cc]ross") or key:find("[Tt]raffic") or key:find("[Bb]uilder") or key:find("[Tt]ool")) then
			seenOtherEvents[key] = true
			seenOtherN = seenOtherN + 1
			logGui("gui event outside the builder family: " .. key)
		end
		return
	end
	local ownPreview = tostring(id):find("^held:") ~= nil   -- a held command's own proposal (natCapture), not a tool's
	for _, M in ipairs(ownPreview and {} or CMDS) do
		if type(M.onToolEvent) == "function" then
			local ok, err = pcall(M.onToolEvent, id, name, param, logGui)
			if not ok then logGui("tool event handler failed: " .. tostring(err)) end
		end
	end
	-- strict mode (only while the native module holds build commands): keep the build for the `held` event that follows
	if name == "builder.proposalCreate" and G.nat then
		local ok, err = pcall(natCapture, id, name, param)
		if not ok then natFault("capture", err) end
		-- the refusal hint for this preview (X.refusalHint, undocumented return): a fault only drops it
		if ok and not ownPreview then
			local okH, hint = pcall(X.refusalHint, id)
			if okH and hint then return hint end
		end
	end
end

-- Each entry point says which Lua state runs it (X.HALF): the simulation state runs update / postUpdate / handleEvent, the interface state
-- guiUpdate / guiHandleEvent, and module values are per state. The functions both halves share (the stamp apply's orders) write
-- to their own state's control file with it (X.ctlWriter).
function data()
	return {
		update = function(...) X.HALF = "sim" return update(...) end,
		postUpdate = function(...) X.HALF = "sim" return X.postUpdate(...) end,
		handleEvent = function(...) X.HALF = "sim" return handleEvent(...) end,
		guiUpdate = function(...) X.HALF = "gui" return guiUpdate(...) end,
		guiHandleEvent = function(...) X.HALF = "gui" return guiHandleEvent(...) end,
	}
end
