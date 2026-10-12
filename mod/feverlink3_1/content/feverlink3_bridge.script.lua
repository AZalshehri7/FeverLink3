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
--     (native_ctl.log / native_events.log, docs/NATIVE_LINK.md section 7).
--   simulation half (update, once per step; handleEvent)
--     keeps the queue in the game-script state, applies due items at their exact step, computes checksum
--     parts, installs the deterministic math.random.
--
-- Without FEVERLINK3_SESSION_DIR (game not started by FeverLink3.exe) every entry point returns at once.

local okC, C = pcall(ug_require, "feverlink3_common.lua")
if not okC then C = ug_require("feverlink3_1::/feverlink3_common.lua") end
local okL, L = pcall(ug_require, "feverlink3_lockstep.lua")
if not okL then L = ug_require("feverlink3_1::/feverlink3_lockstep.lua") end

local okH, Hud = pcall(ug_require, "feverlink3_hud.lua")
if not okH then Hud = ug_require("feverlink3_1::/feverlink3_hud.lua") end

-- Command schemas of the other areas (construction, operations, companies): one module each, all optional. A module
-- that is missing or fails to load is skipped; the commands it would have carried stay native.
local CMDS = {}
local Co = nil -- companies: registry, attribution, ledger (feverlink3_cmds_company.lua, docs/COMPANIES.md); nil without that module
for _, name in ipairs({ "feverlink3_cmds_build", "feverlink3_cmds_ops", "feverlink3_cmds_company" }) do
	local okM, M = pcall(ug_require, name .. ".lua")
	if not okM then okM, M = pcall(ug_require, "feverlink3_1::/" .. name .. ".lua") end
	if okM and type(M) == "table" then
		L.registerCommands(M.commands)
		CMDS[#CMDS + 1] = M
		if M.events then L.registerEvents(M.events) end
		if type(M.company) == "table" then Co = M.company end
	end
end

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
pcall(function() DEV = os.getenv("FEVERLINK3_DEV") == "1" end)

-- Strict mode, how the ORIGINATING game applies its own build. FEVERLINK3_STRICT_CANCEL:
--   unset / "0"  DEFAULT: hold-and-release for every build. The tool's command is held until the stamp and then released, so the tool gets its
--                completion callback and its preview/ghost keeps working. The originator's engine applies the tool's own command while the
--                other games replay a rebuilt record: they allocate different entity ids for the same build and the worlds drift apart a few
--                game minutes later (lab runs 2026-10-12), which the checksum safety net repairs with a resync.
--   "plans"      cancel-and-replay on EVERY game, the originator included, for the plans street, combined and bulldoze (all originators).
--   "host"       the same, but only when this game is the host (the guest keeps hold-and-release).
--   "1"          cancel-and-replay for every kind and every originator.
-- Cancel-and-replay keeps all worlds identical (N0 runs: 0 resyncs in 30 game minutes) but a cancelled command never gives the tool its
-- completion callback: in the first real two-player session the guest's street tool showed no ghost for the second road he built
-- (the click still built it). TpF2 fires the callback before it cancels; that is not implemented here (the callback's signature on this
-- build is unverified), so the cancel flow is opt-in until it is.
local STRICT_CANCEL = "off"
pcall(function()
	local v = os.getenv("FEVERLINK3_STRICT_CANCEL")
	if v == "1" then STRICT_CANCEL = "all" elseif v == "plans" or v == "host" then STRICT_CANCEL = v end
end)
-- FEVERLINK3_STRICT_FIRE_DONE=1 (default off, experimental): the native module fires the tool's own completion callback before it drops a
-- cancelled command (the TpF2 recipe; docs/NATIVE_LINK.md), which re-arms the tool: without it the tool's busy flag stays set and its
-- ghost/preview never comes back. It does so only for the tools whose callback was read out of the exe (street, bulldozer: the held event
-- says `done = true`). With the switch on, a build is cancelled only when its held command says so; otherwise it stays hold-and-release.
local FIRE_DONE = false
pcall(function() FIRE_DONE = os.getenv("FEVERLINK3_STRICT_FIRE_DONE") == "1" end)
local STRICT_CANCEL_PLANS = { street = true, combined = true, bulldoze = true }
local CANCEL_EXTRA_STEPS = 6   -- extra steps of lead for a build every game applies at the stamp (see natDecideUnsafe)

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
	C.appendFile(C.path("dev_" .. half .. "_out.txt"), "---- t=" .. tostring(os.clock()) .. "\n" .. out .. "\n")
end

local STEP = L.STEP
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
	else
		pcall(api.engine.forEachEntityWithComponent, function(e) ids[#ids + 1] = e end, api.type.ComponentType[compName])
	end
	table.sort(ids)
	return ids
end

local engine = {
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
	constructions = function()
		local out = {}
		for _, e in ipairs(entitiesWith("CONSTRUCTION")) do
			local c = api.engine.getComponent(e, api.type.ComponentType.CONSTRUCTION)
			if c then
				local m = c.transf
				out[#out + 1] = { file = c.fileName, x = m and m[13], y = m and m[14], z = m and m[15] }
			end
		end
		return out
	end,
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

	engine.edges = function()
		local out, seen, nodes = {}, {}, {}
		local map = guard(function() return api.engine.system.streetSystem.getNode2SegmentMap() end) or {}
		for _, segs in pairs(map) do
			for i = 1, #segs do
				local e = segs[i]
				if not seen[e] then
					seen[e] = true
					local c = comp(e, "BASE_EDGE")
					local a, b = c and nodeAt(nodes, c.node0), c and nodeAt(nodes, c.node1)
					if a and b then
						local objs = {}
						for k = 1, #(c.objects or {}) do
							local eo = comp(c.objects[k][1], "EDGE_OBJECT")
							objs[#objs + 1] = eo and tostring(eo.edgeObjectConstruction) or "?"
						end
						out[#out + 1] = {
							x0 = a.x, y0 = a.y, x1 = b.x, y1 = b.y, type = tostring(c.type), typeIndex = c.typeIndex,
							roadType = tostring(c.roadType), template = tostring(c.roadTemplate), style = tostring(c.roadStyle),
							decorations = guard(function() return #c.edgeDecorations end), objects = objs,
						}
					end
				end
			end
		end
		return out
	end

	engine.nodeConfigs = function()
		local out = {}
		local map = guard(function() return api.engine.system.streetSystem.getNode2SegmentMap() end) or {}
		for n in pairs(map) do
			local nc, c = comp(n, "BASE_NODE_CONFIG"), comp(n, "BASE_NODE")
			if nc and c and c.position then
				out[#out + 1] = {
					x = c.position.x, y = c.position.y, lanes = #(nc.laneConnections or {}), crosswalks = #(nc.crosswalks or {}),
					preference = tostring(nc.trafficLightPreference),
					light = guard(function() return nc.trafficLightConfig.trafficLightType end), slip = nc.doubleSlipSwitch,
				}
			end
		end
		return out
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

-- The town-buildings lane: what the towns have grown, hashed per town so a log can name the town that differs. Idea from
-- MPFever (MIT, see THIRD_PARTY_NOTICES.md): a `townBuildings` part. Read-only: only components are read, nothing is
-- changed. A TOWN_BUILDING component gives its town, level (1-4), depth, timeBuilt and its CONSTRUCTION entity; that
-- construction gives the model file and the transform. Entity ids of the buildings never enter a hash (the same rule as
-- the constructions part), the town entity id only names the part (towns come from the savegame, so it agrees).
-- Parts: `townBuildings` = "<total>/<hash of all towns>", and `townBuildings:<town id>` = "<count>/<hash>" for the first
-- TOWN_PART_LIMIT towns (the host accepts at most 128 parts; the aggregate covers the rest).
local TOWN_PART_LIMIT = 48
local logTowns = C.logger("towns")
local function townBuildingsPart(parts)
	local ok, err = pcall(function()
		local CTYPE = api.type.ComponentType
		local byTown, townIds = {}, {}
		for _, e in ipairs(engine.entitiesWith("TOWN_BUILDING")) do
			local tb = api.engine.getComponent(e, CTYPE.TOWN_BUILDING)
			if tb then
				local con = tb.construction and api.engine.getComponent(tb.construction, CTYPE.CONSTRUCTION)
				local m = con and con.transf
				local parcels = 0
				pcall(function() parcels = #tb.parcels end)
				local key = string.format("%s@%.1f,%.1f,%.1f/L%s/D%s/P%d/T%s", tostring(con and con.fileName), m and m[13] or 0,
					m and m[14] or 0, m and m[15] or 0, tostring(tb.level), tostring(tb.depth), parcels, tostring(tb.timeBuilt))
				local town = tb.town
				if byTown[town] == nil then byTown[town] = {}; townIds[#townIds + 1] = town end
				local list = byTown[town]
				list[#list + 1] = key
			end
		end
		table.sort(townIds)
		local all, total = {}, 0
		for i, town in ipairs(townIds) do
			local list = byTown[town]
			table.sort(list)
			total = total + #list
			all[#all + 1] = { town = town, n = #list, items = list }
			if i <= TOWN_PART_LIMIT then parts["townBuildings:" .. tostring(town)] = #list .. "/" .. C.hashValue(list) end
		end
		parts.townBuildings = total .. "/" .. C.hashValue(all)
	end)
	if not ok then
		parts.townBuildings = "error"
		logTowns("townBuildings lane failed: " .. tostring(err))
	end
end

-- Town trace (feverlink3_towntrace.lua): off unless FEVERLINK3_TOWN_TRACE is set. Read-only; one file per game.
local TRACE = nil
local TRACE_FAILS = 0
local townTraceGuard
local function townTrace()
	if TRACE ~= nil then return TRACE or nil end
	TRACE = false
	local ok, err = pcall(function()
		local spec = os.getenv("FEVERLINK3_TOWN_TRACE")
		if type(spec) ~= "string" or spec == "" then return end
		local okM, M = pcall(ug_require, "feverlink3_towntrace.lua")
		if not okM then okM, M = pcall(ug_require, "feverlink3_1::/feverlink3_towntrace.lua") end
		if not okM or type(M) ~= "table" then error("module not loaded: " .. tostring(M)) end
		local t = M.new({
			spec = spec,
			acc = M.apiAccessors(api, engine.entitiesWith),
			hash = C.hashValue,
			write = function(text) return C.appendFile(C.path(M.FILE), text) end,
			-- FEVERLINK3_TOWN_TRACE_CMDS=1: log the payload of the game script's makeTownUpdateSizeCmd / makeTownUpdateCargoNeedsCmd
			cmds = os.getenv("FEVERLINK3_TOWN_TRACE_CMDS") == "1",
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
-- Checksum cost (observation only, never raises, behaviour unchanged): the seconds of every part are measured with os.clock. A
-- checksum that takes more than CS_SLOW_MS in total logs one line naming the worst parts; every CS_SUMMARY rounds one summary line
-- (count, average, maximum and the part that was the slowest in the maximum round). Each half (interface, simulation) counts its own.
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

-- `light` (the round asked for no heavy lanes): constructions, edges, node settings and town buildings are not read.
local function checksumParts(co, where, light)
	local function clock() local ok, v = pcall(os.clock); return ok and tonumber(v) or 0 end
	local t0 = clock()
	local timing = {}
	local parts, t = L.checksumParts(engine, C.hashValue, timing, light)
	local okD, digest = false, nil
	local t1 = clock()
	if Co then okD, digest = pcall(Co.digest, co) end
	if okD and digest ~= nil then parts.companies = C.hashValue(digest) end
	timing.companies = clock() - t1
	local t2 = clock()
	if not light then townBuildingsPart(parts) end
	timing.townBuildings = clock() - t2
	checksumNote(timing, clock() - t0, where or "gui", light == true)
	return parts, t
end

-- ================================================================== simulation half

local logSim = C.logger("sim")
local SIM = { rng = nil, subscribed = false }

-- Live vs loaded log (feverlink3_livevsloaded.lua): off unless FEVERLINK3_LIVE_VS_LOADED=1. Observe-only, own guard.
local LVL = nil
local LVL_FAILS = 0
local function livevsloaded()
	if LVL ~= nil then return LVL or nil end
	LVL = false
	local ok, err = pcall(function()
		if os.getenv("FEVERLINK3_LIVE_VS_LOADED") ~= "1" then return end
		local okM, M = pcall(ug_require, "feverlink3_livevsloaded.lua")
		if not okM then okM, M = pcall(ug_require, "feverlink3_1::/feverlink3_livevsloaded.lua") end
		if not okM or type(M) ~= "table" then error("module not loaded: " .. tostring(M)) end
		local okT, TT = pcall(ug_require, "feverlink3_towntrace.lua")
		if not okT then okT, TT = pcall(ug_require, "feverlink3_1::/feverlink3_towntrace.lua") end
		local bits32 = okT and type(TT) == "table" and TT.bits32 or nil
		local t = M.new({
			enabled = true,
			towns = os.getenv("FEVERLINK3_TOWN_TRACE"),
			acc = M.apiAccessors(api, engine.entitiesWith),
			bits32 = bits32,
			write = function(text) return C.appendFile(C.path(M.FILE), text) end,
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
local function actionCtx(it, co, resolve)
	local ctx = { player = engine.canonicalPlayer and engine.canonicalPlayer() or engine.player(), resolve = resolve, co = co }
	if it then ctx.from, ctx.at, ctx.key = it.from, it.at, it.key end
	if Co then pcall(Co.actor, ctx) end
	return ctx
end

local function simCtx(st, it)
	return actionCtx(it, st.co, function(key) return L.resolveKey(st.bind, key) end)
end

-- companies. Guarded calls into the company module: a fault in that layer is logged and the action goes ahead, since
-- losing a player's action is worse than a missing ledger line.
local function coBefore(ctx, it, spec, args, category, log)
	if not Co then return true end
	local ok, a, b = pcall(Co.beforeApply, ctx, it, spec, args, category)
	if not ok then
		log("company layer failed: " .. tostring(a))
		return true
	end
	return a, b
end

local function coEffects(ctx, fx, log)
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
local function coBalance(ctx)
	if not Co or not engine.balanceOf then return nil end
	local ok, v = pcall(engine.balanceOf, ctx.player)
	if ok and type(v) == "number" and v == v then return v end
	return nil
end

-- After the engine command went out: the ledger, with the cost measured as the account's difference. Returns the cost.
local function coAfter(ctx, it, spec, category, balance0, log)
	if not Co then return nil end
	local cost = nil
	local balance1 = balance0 ~= nil and coBalance(ctx) or nil
	if balance0 ~= nil and balance1 ~= nil then cost = balance0 - balance1 end
	local ok, err = pcall(function() Co.afterApply(ctx, it, spec, Co.info(it, spec, category, cost)) end)
	if not ok then log("company layer failed: " .. tostring(err)) end
	return cost
end

-- Applies one due action in the simulation state. No callback here (finding from MPFever (MIT): the engine
-- rejects callbacks from inside the simulation update), so an entity the command creates is found afterwards by
-- comparing the entity lists (a watch), and the result reaches the originator through the state.
-- (The replay marks below follow the counter MPFever keeps for the same purpose in captureNativeBuild, MIT.)
-- The engine tells the game script about every build it applies (`apply_command` / `onPreBuildProposal`, just before
-- it runs; finding from MPFever (MIT)). A replay of an action (a build rebuilt from a shipped record) must not be
-- shipped again, so each one is counted here when it is sent and spent when its event arrives.
-- The engine's own verdict on a rebuilt build proposal, without applying it (what a tool's preview reads): critical flag, messages and
-- warnings of the proposal data's errorState. The second argument of makeProposalData must be a Context (a command whose context
-- was left out made `bad argument #2 to 'makeProposalData'`): a default one with this game's player is made when the command has none.
-- Returns `critical, messages, warnings` (lists of strings) or nil and the reason it could not be read. Never raises.
local function engineVerdict(args)
	local ok, critical, msgs, warns = pcall(function()
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
		return es.critical == true, list(es.messages), list(es.warnings)
	end)
	if not ok then return nil, tostring(critical):sub(1, 160) end
	if critical == nil then return nil, "no errorState" end
	return critical, msgs, warns
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
		if sum.rem > 0 or n > 0 then parts[#parts + 1] = string.format("resolved removals %d of %d [%s]", n, sum.rem, table.concat(ids, " ")) end
		local verdict = "n/a"
		if not REPLAY_VERDICT.failed then
			local critical, msgs, warns = engineVerdict(args)
			if critical == nil and msgs then
				REPLAY_VERDICT.failed = true
				logSim("the engine's verdict on a rebuilt build cannot be read from this state (" .. tostring(msgs) .. "); the build lines say verdict n/a from now on")
			elseif critical ~= nil then
				verdict = string.format("critical=%s messages=[%s] warnings=[%s]", tostring(critical), table.concat(msgs, "; "), table.concat(warns, "; "))
			end
		end
		parts[#parts + 1] = "verdict " .. verdict
		parts[#parts + 1] = string.format("ie=%s pi=%s", tostring(args[3] == true), tostring(args[4] == true))
		return table.concat(parts, "; "), { sum = sum, removed = ids, removedN = n, verdict = verdict }
	end)
	if ok and text then
		logSim(string.format("build %s (%s): %s", it.key, role, text))
		return info
	end
	return nil
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
		logSim("the engine did not run the replayed build " .. tostring(r.key or "?") .. " sent at t=" .. tostring(r.at) .. ": no apply event within " .. REPLAY_STEPS .. " steps (refused by the engine?)")
		st.unrun = (st.unrun or 0) + 1
		if r.key then st.notes = st.notes or {}; L.addNote(st.notes, now, "the engine did not run the replayed build " .. r.key) end
		r = nil
	end
	if type(r) ~= "table" or now - (r.at or 0) > REPLAY_STEPS * STEP then r = { n = 0, nf = 0, at = now } end
	if key then r.key = key end
	if noPlayer then r.nf = (r.nf or 0) + 1 else r.n = r.n + 1 end
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
	return true
end

-- Strict mode, the originating game: a shipped build whose own copy the native module still holds. If the module gives that
-- command back BEFORE the stamp (its silence rule, its age limit, hold_off), the build lands in this world early and its engine
-- event comes with no replay mark; this list tells the capture that it is that build, not a new one, so it is not shipped a second
-- time. The interface half adds an entry when it ships (event "heldout") and removes it when it released the command itself or
-- cancelled it ("heldgone"); an entry lives 35 s of wall time at most (the module's own limit is 30 s) and never across a reload.
local HELD_WALL = 35
local function heldOutLive(e)
	local age = os.clock() - (tonumber(e.wall) or 0)
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

local function buildCategory(body)
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
local function actionCategory(body, args)
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
local NATIVE_BIND_COMPS = { "CONSTRUCTION", "EDGE_OBJECT", "STATION_GROUP" }
local function snapshotComps(comps)
	local before = {}
	for _, comp in ipairs(comps) do before[comp] = engine.entitiesWith(comp) end
	return before
end
local function simCaptureBuild(state, param, post, deferredBefore)
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
	local rev = {}
	for key, e in pairs(st.bind) do rev[e] = key end
	local t0 = os.clock()
	local ok, body, why = pcall(Build.capture, proposal, { playerInitiated = true, result = post and result or nil }, {
		plain = C.plain, keyOf = function(e) return L.keyFor(rev, e) end,
	})
	if not ok then body, why = nil, tostring(body) end
	if not body and not post and tostring(why):find("has no model", 1, true) then
		SIM.deferredBuild = { at = now, before = select(2, pcall(snapshotComps, NATIVE_BIND_COMPS)) }
		logSim("a native build at t=" .. now .. " has an edge object that the engine has not made yet: read again after the build")
		state:set(st)
		return
	end
	if not body then
		logSim("a native build at t=" .. now .. " is not replicated: " .. tostring(why))
		simNote(st, now, "a native build at t=" .. now .. " was not replicated (" .. tostring(why) .. ")")
		-- the first few failures leave the raw proposal in a file, to see the real shapes (members of engine objects)
		st.rawDumped = (st.rawDumped or 0) + 1
		if st.rawDumped <= 4 and Build.adapter and Build.adapter.dump then
			pcall(function()
				local text = C.serialize(Build.adapter.dump(proposal))
				C.appendFile(C.path("raw_builds.txt"), "==== t=" .. now .. " why: " .. tostring(why) .. "\n" .. text:sub(1, 60000) .. "\n")
			end)
		end
		state:set(st)
		return
	end
	body.native = true
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
	st.out = st.out or {}
	st.outSeq = (st.outSeq or 0) + 1
	st.out[#st.out + 1] = { n = st.outSeq, at = now, body = text }
	trim(st.out, OUT_KEEP)
	pcall(function()
		local spec = L.COMMANDS.makeWorldBuildProposalCmd
		local comps = spec and L.bindsComp(spec, body)
		if not comps then return end
		local before = deferredBefore
		if not before and not post then before = snapshotComps(comps) end
		if type(before) ~= "table" then return end
		SIM.nativeBinds = SIM.nativeBinds or {}
		SIM.nativeBinds[st.outSeq] = { body = body, comps = comps, before = before }
		SIM.nativeBinds[st.outSeq - 8] = nil
	end)
	state:set(st)
	logSim("native build captured at t=" .. now .. " (" .. #text .. " bytes, " .. string.format("%.3f", os.clock() - t0) .. " s)")
	-- self-check: could this record be rebuilt here, the way another game will rebuild it? Only the engine's command
	-- factory is called, nothing is sent.
	if post then return end   -- the world has changed already: the segment it replaces is the new one, a check would mislead
	local okC, args, whyC = pcall(L.decodeCommand, body, simCtx(st, { from = C.PLAYER_ID, at = now, key = "self-check" }))
	if not okC then args, whyC = nil, tostring(args) end
	if args then
		logSim("native build self-check: can be rebuilt (" .. tostring(args.chain and (#args.chain + 1) or 1) .. " command(s))")
	else
		logSim("native build self-check FAILED: " .. tostring(whyC))
	end
end

local function simApplyAction(st, it, now)
	if it.body and it.body.native and it.from == C.PLAYER_ID then
		-- the engine applied this build when its player made it; the record only exists to reach the other games
		st.results[#st.results + 1] = { key = it.key, at = now, success = true }
		return
	end
	if it.lateSteps and it.lateSteps > 0 and not it.waited then
		logSim("LATE action " .. it.key .. " for t=" .. it.at .. " applied at " .. now .. " (" .. it.lateSteps .. " step(s) late)")
		st.late = (st.late or 0) + 1
	end
	local ctx = simCtx(st, it)
	local args, err = L.decodeCommand(it.body, ctx)
	if not args then
		-- It names an entity another action created, which the watch has not found yet (the engine shows a new entity a step
		-- after the command): wait for it, a few steps at most, instead of refusing. Every game waits alike.
		local waitsFor = (it.waited or 0) < WATCH_STEPS + 2 and L.pendingRef(it.body, function(k) return L.keyIsPending(k, st.watches, st.binders) end) or nil
		if waitsFor then
			if not it.waited then logSim("action " .. it.key .. " waits for the entity " .. waitsFor .. " names") end
			it.waited = (it.waited or 0) + 1
			L.enqueue(st.queue, it)
			return
		end
		err = L.refusalDetail(err, it.body)
		logSim("action " .. it.key .. " refused: " .. tostring(err))
		st.refused[#st.refused + 1] = { key = it.key, from = it.from, seq = it.seq, detail = err }
		return
	end
	local spec = L.COMMANDS[it.body.fn]
	local category = actionCategory(it.body, args)
	-- companies: the spending limit and the spec's registry change come first; a mod-level action has no engine command
	local okC, fx = coBefore(ctx, it, spec, args, category, logSim)
	if not okC then
		logSim("action " .. it.key .. " refused: " .. tostring(fx))
		st.refused[#st.refused + 1] = { key = it.key, from = it.from, seq = it.seq, detail = fx }
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
	local ok, e = true, nil
	local preInfo = nil
	if held and it.body.fn == "makeWorldBuildProposalCmd" then preInfo = describeBuild(it, args, "originator") end
	if not held then
		ok, e = pcall(function()
			L.sendNow(args, function(a)
				local isBuild = it.body.fn == "makeWorldBuildProposalCmd"
				if isBuild then
					markReplay(st, now, SIM.state, it.key, a[4] == false)
					if isBuild and not preInfo then preInfo = describeBuild(it, a, "replay") end
				end
				api.cmd.sendCommand(api.cmd[it.body.fn](unpack(a, 1, a.n)))
				if isBuild then takeBackEvents(st, SIM.state) end
			end)
		end)
	end
	if not ok then
		logSim("action " .. it.key .. " refused by the engine: " .. tostring(e))
		st.refused[#st.refused + 1] = { key = it.key, from = it.from, seq = it.seq, detail = tostring(e) }
		return
	end
	coAfter(ctx, it, spec, category, balance0, logSim)
	logSim("action " .. it.key .. " " .. it.body.fn .. (held and (" due at t=" .. now .. ": the native module holds this game's own copy (held " .. tostring(it.heldId) .. "), the interface half releases it") or (" applied at t=" .. now)))
	if bindComps then
		st.binders[#st.binders + 1] = { key = it.key, fn = it.body.fn, body = it.body, comps = bindComps, before = bindBefore, steps = 0, pre = preInfo }
	end
	if spec.creates then
		st.watches[#st.watches + 1] = { key = it.key, comp = spec.creates, result = spec.result, before = before, steps = 0, at = now }
	else
		st.results[#st.results + 1] = { key = it.key, at = now, success = true }
	end
	if held then
		st.results[#st.results + 1] = { key = it.key .. "#release" .. tostring(it.heldId), of = it.key, at = now, success = true, release = it.heldId }
	end
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

local function simAdvanceWatches(st, now)
	if #st.watches == 0 then return end
	-- an entity bound to one key is not there for the next watch: five vehicles bought in one step appear together
	local claimed = {}
	for _, e in pairs(st.bind) do claimed[e] = true end
	local keep = {}
	for _, w in ipairs(st.watches) do
		w.steps = w.steps + 1
		local created = L.claimEntity(w.before, engine.entitiesWith(w.comp), claimed)
		if created then
			claimed[created] = true
			st.bind[w.key] = created
			st.results[#st.results + 1] = { key = w.key, at = w.at, success = true, data = { [w.result] = created }, entities = { { created, 0 } } }
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
	local owner = {}
	for k, e in pairs(st.bind) do owner[e] = k end
	local keep = {}
	for _, b in ipairs(st.binders) do
		b.steps = b.steps + 1
		local mine = b.key .. "#"
		local made, seen = {}, {}
		for _, comp in ipairs(b.comps) do
			for _, e in ipairs(L.newEntities(b.before[comp] or {}, engine.entitiesWith(comp))) do
				local other = owner[e]
				if not seen[e] and (other == nil or other:sub(1, #mine) == mine) then
					seen[e] = true
					made[#made + 1] = e
				end
			end
		end
		table.sort(made)
		local map, complete = L.bindMap(L.COMMANDS[b.fn], b.body, b.key, made)
		for key, e in pairs(map) do
			st.bind[key] = e
			owner[e] = key
		end
		if complete then
			logSim("action " .. b.key .. " created " .. #made .. " " .. table.concat(b.comps, "/") .. " entities, keyed")
		elseif b.steps >= WATCH_STEPS then
			-- what is missing and why: the counts the record promised against what this world shows now
			local ok2, why = pcall(function()
				local pre = b.pre
				local sum = pre and pre.sum or (Build and Build.recSummary and Build.recSummary(b.body)) or {}
				local found = {}
				for _, comp in ipairs(b.comps) do
					if comp == "CONSTRUCTION" then
						for _, e in ipairs(L.newEntities(b.before[comp] or {}, engine.entitiesWith(comp))) do
							if #found < 6 then found[#found + 1] = conLine(e) end
						end
					end
				end
				local still, total = 0, 0
				for _, id in ipairs(pre and pre.removed or {}) do
					total = total + 1
					local idn = tonumber(id)
					if idn and pcall(function() assert(api.engine.getComponent(idn, api.type.ComponentType.CONSTRUCTION)) end) then still = still + 1 end
				end
				return string.format("expected %d construction(s) %s, found %d new CONSTRUCTION-like entit%s [%s]; removed buildings still present: %d of %d checked; the engine's verdict before applying: %s",
					sum.cons or 0, "[" .. table.concat(sum.add or {}, " ") .. "]", #found, #found == 1 and "y" or "ies", table.concat(found, " "), still, total,
					pre and pre.verdict or "n/a (see the build line)")
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
		local ok, v = pcall(function() return os.getenv("FEVERLINK3_BUILD_TRACE") == "1" end)
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
		C.appendFile(C.path("build_trace.txt"), table.concat(lines, "\n") .. "\n")
	end)
	if not ok then
		BT.fails = BT.fails + 1
		if BT.fails == 1 then logSim("build trace error: " .. tostring(err)) end
	end
end

local function update(userParams, state, dt)
	C.poll()
	pcall(function() buildTraceStep(math.floor(engine.gameTime() / STEP)) end)
	townTraceGuard(function(t)
		if C.DIR then t.step(math.floor(engine.gameTime() / STEP)) end
	end)
	livevsloadedGuard(function(t)
		if C.DIR then t.step(math.floor(engine.gameTime() / STEP)) end
	end)
	if not C.ACTIVE then
		return
	end
	if not SIM.rng then
		SIM.rng = L.newRandom(engine.gameTime)
		L.installRandom(SIM.rng)
		logSim("deterministic math.random installed")
	end
	-- one line per game start for the string-hash seed experiment; inert unless FEVERLINK3_LUA_SEED_LOG=1
	if not SIM.seedProbed then
		SIM.seedProbed = true
		pcall(C.luaSeedProbeOnce, logSim)
	end
	if not SIM.subscribed then
		SIM.subscribed = true
		pcall(function() state:subscribeToAllEvents() end)
	end
	SIM.devTick = (SIM.devTick or 0) + 1
	if SIM.devTick % 4 == 0 then devPoll("sim") end
	SIM.state = state
	local ok, err = pcall(function()
		local st = simState(state)
		local now = engine.gameTime()
		local due = L.takeDue(st.queue, now)
		for _, it in ipairs(due) do
			if it.kind == "checksum" then
				local parts, t = checksumParts(st.co, "sim", it.full == false)
				st.reports[#st.reports + 1] = { n = it.n, time = t, parts = parts, account = L.accountSnapshot(engine) }
			elseif it.kind == "action" then
				simApplyAction(st, it, now)
			end
		end
		simAdvanceWatches(st, now)
		simAdvanceBinders(st, now)
		trim(st.results, RESULT_KEEP)
		trim(st.reports, RESULT_KEEP)
		trim(st.refused, RESULT_KEEP)
		state:set(st)
	end)
	if not ok then logSim("update failed: " .. tostring(err)) end
end

-- Messages from the interface half (makeScriptingSendEventCmd with id "feverlink3").
local function handleEvent(userParams, state, src, id, name, param)
	if DEV and id == "apply_command" then
		pcall(function()
			local pi = nil
			pcall(function() pi = param[4] end)
			if pi ~= false then logSim("event apply_command/" .. tostring(name) .. " at t=" .. tostring(engine.gameTime()) .. " playerInitiated=" .. tostring(pi)) end
		end)
	end
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
			local ok, err = pcall(simCaptureBuild, state, param, true, d.before)
			if not ok then logSim("onPostBuildProposal capture failed: " .. tostring(err)) end
		end
		return
	end
	C.poll()
	if not C.ACTIVE or id ~= "feverlink3" then return end
	if name == "nop" then return end   -- strict mode's trigger: a command that makes the native module run its queue (natTrigger)
	local ok, err = pcall(function()
		local st = simState(state)
		if name == "queue" and type(param) == "table" then
			L.enqueue(st.queue, param)
		elseif name == "nativebind" and type(param) == "table" then
			local nb = SIM.nativeBinds and SIM.nativeBinds[param.n]
			if nb and type(param.key) == "string" then
				SIM.nativeBinds[param.n] = nil
				st.binders[#st.binders + 1] = { key = param.key, fn = "makeWorldBuildProposalCmd", body = nb.body, comps = nb.comps, before = nb.before, steps = 0 }
				logSim("action " .. param.key .. ": this game's own native build is keyed here like a replay (" .. table.concat(nb.comps, "/") .. ")")
			end
		elseif name == "replay" then
			markReplay(st, engine.gameTime())
		elseif name == "heldout" and type(param) == "table" then
			st.heldOut = st.heldOut or {}
			st.heldOut[#st.heldOut + 1] = { key = param.key, wall = os.clock() }
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
			st.bind[param.key] = param.e
		elseif name == "co" and Co and type(param) == "table" and type(param.it) == "table" then
			simReplayCompany(st, param)
		end
		state:set(st)
	end)
	if not ok then logSim("event " .. tostring(name) .. " failed: " .. tostring(err)) end
end

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

local function send(kind, payload)
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
	O.sendCommand(O.event("feverlink3", "feverlink3", name, param))
end

local function setSpeed(sp)
	if sp ~= G.lastSpeedSet or G.frames % 60 == 0 then
		G.lastSpeedSet = sp
		O.sendCommand(O.setSpeed(sp))
	end
end

local function guiCtx(it)
	return actionCtx(it, G.co, function(key) return L.resolveKey(G.bind, key) end)
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

local function deliverResult(key, data, success, entities)
	local cb = G.pending[key]
	if cb then
		G.pending[key] = nil
		local ok, err = pcall(cb, data, success, entities)
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

local function reportRound(n, t, parts, account)
	G.reportsSent[n] = true
	G.accountAt[n] = account
	G.accountAt[n - 40] = nil
	send("checksum_report", { n = n, time = t, parts = parts, authority = (C.PLAYER_ID == 0) and account or nil })
	local waiting = G.authPending[n]
	if waiting then
		G.authPending[n] = nil
		local ok, err = pcall(applyAuthority, waiting)
		if not ok then logGui("host authority failed: " .. tostring(err)) end
	end
end

-- companies: hands an action this half applied to the simulation half, which keeps the registry in the savegame
local function coToSim(it, ok, cost, category)
	if not Co then return end
	toSim("co", { it = { from = it.from, at = it.at, seq = it.seq, key = it.key, body = it.body }, ok = ok == true, cost = cost, category = category })
end

-- An action this game applied natively when its player made it (a build with a native tool, a command from a window): every
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

local function guiApplyNow(it, now)
	if it.kind == "action" and it.body and it.body.native and it.from == C.PLAYER_ID then return end
	if it.kind == "checksum" then
		local parts, t = checksumParts(G.co, "gui", it.full == false)
		reportRound(it.n, t, parts, L.accountSnapshot(engine))
		return
	end
	if it.lateSteps and it.lateSteps > 0 then logGui("LATE action " .. it.key .. " for t=" .. it.at .. " applied at " .. now) end
	local ctx = guiCtx(it)
	local args, err = L.decodeCommand(it.body, ctx)
	if not args then
		err = L.refusalDetail(err, it.body)
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
	local function finish(data, success, entities)
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
		deliverResult(it.key, data, success, entities)
	end
	-- the engine's factory may reject what was rebuilt: that refuses the action, like any other refusal
	local okS, errS
	if it.heldId and natPauseRelease then
		-- the tool's own command is held by the native module: it is released instead of sent (the whole build is that one command)
		okS, errS = pcall(natPauseRelease, it, finish)
	else
		okS, errS = pcall(L.runChained, args, function(a, cb)
			if it.body.fn == "makeWorldBuildProposalCmd" then toSim("replay", {}) end
			O.sendCommand((O.factory[it.body.fn] or api.cmd[it.body.fn])(unpack(a, 1, a.n)), cb)
		end, finish)
	end
	if not okS then
		logGui("action " .. it.key .. " refused by the engine: " .. tostring(errS))
		send("action_refused", { seq = it.seq, from = it.from, detail = tostring(errS) })
		deliverResult(it.key, nil, false, {})
		coToSim(it)
		if it.heldId and natHeldRefused then pcall(natHeldRefused, it) end
	end
end

local function route(it)
	local now = engine.gameTime()
	if L.applyNow(G.session, now, it) then guiApplyNow(it, now) else toSim("queue", it) end
end

-- A build the player made with a native tool, captured by the simulation half (simCaptureBuild): stamped and shipped
-- like any other action. The engine applied it already, so it is not queued here; the other games rebuild it at the stamp.
-- (TpF2 Multiplayer stamps past the fastest other clock for the same reason: a game that has passed the stamp
-- applies the action late, and the worlds fork.)
-- extraSteps: a held strict build is stamped this many steps later, so that a veto (a game that cannot rebuild it says so as soon
-- as it has the record) is back at the originator and at every other game before the stamp. Not at a pause (the stamp is then
-- the pause time: the build is applied during the pause, a veto cannot come first).
local function stampAfterFastest(now, extraSteps)
	local base = now
	local peers = false
	for _, pc in pairs(G.peers) do
		if type(pc) == "table" and type(pc.t) == "number" then
			peers = true
			if pc.t > base then base = pc.t end
		end
	end
	local at = L.stampFor(G.session, base)
	if extraSteps and peers and not (G.session.pauseAt and base >= G.session.pauseAt) then
		-- the veto travels from a peer to the host to the originator: about a whole round trip, while the lead covers half of one plus
		-- fixed handling. The host's lead grows with the slowest round trip, so the extra grows with it (half the lead, one step at least).
		at = at + math.max(extraSteps, math.floor(L.leadOf(G.session) / STEP / 2)) * STEP
	end
	return at
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
local PARK_MAX = 10.0       -- seconds a peer keeps a parked record without its commit

local function pausedNow()
	local s = G.session
	return s.pauseAt ~= nil and engine.gameTime() >= s.pauseAt
end

-- The grace in real milliseconds: two round trips' worth (the record out, the veto back) plus a dry run and a margin. The round
-- trip is read from the host's lead (lead_for: half a round trip plus handling, in steps), which the host derives from its
-- measurement of the slowest connection.
local function graceMs()
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
local VETO_STEPS = 1          -- extra steps of lead for a held build (the veto's way back)
-- A build that adds or removes constructions takes the other games much longer to dry-run (every building has to be found, the engine's
-- factory runs on a big proposal; real session 2026-10-12: a veto 2 s after the build was released after a 626 ms grace): a longer
-- window, so that a veto reaches the originator before it releases its command.
local CONSTRUCTION_VETO_STEPS = 12   -- extra steps of lead
local CONSTRUCTION_GRACE_MS = 4000   -- a paused game waits at least this long
local VETO_MARGIN = 1         -- a dry run later than this many steps before the stamp is useless and skipped

local function noteBuild(key, at)
	G.buildAts[key] = at
	local n = 0
	for _ in pairs(G.buildAts) do n = n + 1 end
	if n > 40 then
		local now = engine.gameTime()
		for k, a in pairs(G.buildAts) do if a < now - 20 * STEP then G.buildAts[k] = nil end end
	end
end

-- builds that stamp earlier than `it` and have not landed on this game yet
local function pendingBefore(it, now)
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
	local critical, msgs = engineVerdict(args)
	if critical == nil then
		if msgs ~= "no errorState" then
			G.noProposalData = true
			logGui("veto check: the engine's errorState cannot be read from a script here (" .. tostring(msgs) .. "); only the references and the command factory are checked")
		end
		return nil
	end
	if critical or (args[3] ~= true and #msgs > 0) then
		return "engine: " .. (critical and "critical error " or "errors ") .. table.concat(msgs, ", ")
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
	local bad = proposalVerdict(args)
	if bad then return false, bad end
	return true
end

local function sendVeto(it, why)
	G.vetoed[it.key] = why
	toSim("drop", { key = it.key, why = why, own = true })
	send("build_veto", { seq = it.seq, owner = it.from, why = tostring(why):sub(1, 300), name = C.PLAYER_NAME })
	-- (the chat line is the originator's, once per vetoed build: it alone knows whether its build was still held; see H.build_veto)
	logGui("VETO of " .. it.key .. ": this game cannot build it (" .. tostring(why) .. "); the build is dropped on this game")
end

local function vetoCheck(it, now)
	local ok, why = dryRun(it)
	if ok then
		if why then logGui("dry run of " .. it.key .. ": " .. why) end
		return
	end
	sendVeto(it, why)
end

-- every frame: dry runs that waited for earlier builds
local function dryPump()
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

local function shipNative(o)
	local body = C.parse(o.body)
	if type(body) ~= "table" or type(body.args) ~= "table" then return false, "unreadable record" end
	local category = buildCategory(body)
	local now = engine.gameTime()
	G.seq = G.seq + 1
	local at = stampAfterFastest(now)
	send("action", { stamp = at, seq = G.seq, body = o.body, category = category })
	logGui("native build " .. L.itemKey(C.PLAYER_ID, G.seq) .. " captured at t=" .. tostring(o.at) .. ", stamped " .. at .. " (now " .. now .. ", " .. #o.body .. " bytes)")
	recordOriginator(body, G.seq, at, category)
	pcall(toSim, "nativebind", { n = o.n, key = L.itemKey(C.PLAYER_ID, G.seq) })
	return true
end

-- A command a window issued, recorded by the UI-state hook (feverlink3_uihook.lua), which applied it natively already:
-- stamped and shipped; the other games apply it at the stamp.
local function shipUiCommand(p)
	local body = C.parse(p.body)
	if type(body) ~= "table" or type(body.args) ~= "table" then return false, "unreadable record" end
	local now = engine.gameTime()
	-- entities a window command made a moment ago travel under the key of the record that made them (L.rewriteUiKeys)
	local text = p.body
	local note = L.recentNotes(G.notes, now)
	if text:find("ui:", 1, true) or #note > 0 then
		local okR, missing = L.rewriteUiKeys(body, function(id) return G.uiSeq[id] end, C.PLAYER_ID)
		if not okR then return false, "it names the entity window command #" .. tostring(missing) .. " made, which was never shipped" end
		if #note > 0 then body.diverged = note end
		text = C.serialize(body)
	end
	G.seq = G.seq + 1
	G.uiSeq[p.id] = G.seq
	local at = stampAfterFastest(now)
	local category = type(p.category) == "string" and p.category or "other"
	send("action", { stamp = at, seq = G.seq, body = text, category = category })
	logGui("window command " .. tostring(body.fn) .. " " .. L.itemKey(C.PLAYER_ID, G.seq) .. " stamped " .. at .. " (now " .. now .. ")")
	recordOriginator(body, G.seq, at, category)
	return true
end

-- A command this player issued (held by the wrapped sendCommand): encode, stamp, ship, queue locally.
local function shipLocal(tag, cb)
	local body, err = L.encodeCommand(tag.name, tag.args, { plain = C.plain, keyOf = function(e) return L.keyFor(G.rev, e) end })
	if not body then return false, err end
	local now = engine.gameTime()
	local note = L.recentNotes(G.notes, now)
	if #note > 0 then body.diverged = note end
	G.seq = G.seq + 1
	local at = stampAfterFastest(now)
	local it = { kind = "action", at = at, from = C.PLAYER_ID, seq = G.seq, body = body, key = L.itemKey(C.PLAYER_ID, G.seq) }
	-- the category rides along so the host can mark an action its sender had no grant for (docs/PROTOCOL.md, Permissions)
	send("action", { stamp = at, seq = G.seq, body = C.serialize(body), category = L.categoryOf(tag.name, tag.args) })
	if cb then G.pending[it.key] = cb end
	logGui("action " .. it.key .. " " .. tag.name .. " stamped " .. at .. " (now " .. now .. ")")
	route(it)
	return true
end

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
--   4. whatever does not fit (no usable capture, a record that cannot be rebuilt here) is released: `release {id}` lets the
--      native build go through and the simulation half's onPreBuildProposal capture ships it the old way.
-- A capture is never shipped without its `held`, and a held command is either cancelled-and-shipped or released, never both.
--
-- Switching it on (once per script start; a reloaded script starts again from the file, which keeps `ready`):
--   native_events.log appears -> its records are read; the first batch is history (state, never news)
--   `ready` with "command_list_add" in hooks (or no `ready` at all in a trimmed file) -> ping, wait for the `pong`
--   `pong` -> hold_on {kinds = {"build_proposal"}} -> `holding {on = true}` -> strict mode is on
--   while it is on: a line at least every NAT_PING seconds (the module switches holding off after 10 s of silence)
-- Switching it off (hold_off): before a resync load, when the session ends (identity.txt says idle), when the mod is not
-- active, on a fault, when the module stops answering, and when the module itself says `holding {on = false}` (it retries
-- with a back-off). Without native_events.log nothing of this runs and nothing is written.

local NAT_PING = 2.0      -- longest pause between lines written to native_ctl.log while holding
local NAT_CAP_TTL = 2.0   -- a capture without its `held` is dropped after this many seconds
local NAT_CAP_MAX = 6     -- captures kept
local NAT_GRACE = 0.4     -- a `held` waits this long (and 3 frames) for its capture before it is released
local NAT_PREVIEW_TTL = 30.0 -- the latest preview of a tool counts for a `held` this long after it was made
local NAT_CONFIRM = 3.0   -- hold_on must be answered by `holding on` within this
local NAT_ALIVE = 6.0     -- a pong this recent proves the module reads the control file
local NAT_WATCHDOG = 8.0  -- no pong for this long while holding: the module is gone

local function natNew()
	return {
		reader = C.newReader(C.path("native_events.log")), writer = nil,
		seen = false, read = false,   -- native_events.log has records / its first batch (history) was read
		ready = nil, hookOk = nil,    -- the latest `ready`; true/false: command_list_add is / is not among its hooks; nil: no `ready` seen
		pingN = 0, pingAt = -100, pingsTried = 0, pongAt = nil, watchFrom = nil, lastTick = nil, lastWrite = -100,
		holdSent = false, holdAt = 0, active = false, activeSince = 0,
		retryAt = nil, retries = 0, gaveUp = false,
		caps = {}, parked = {}, awaiting = {}, owed = {},
		heldByKey = {}, byId = {}, finishes = {},   -- shipped builds whose own copy is held: by action key / by held id; pause-path completions
		sessionOn = true, sessionAt = -100,
		logWin = -100, logN = 0, logLost = 0, faults = 0, dead = false,
	}
end

-- One line each, at most 12 per 10 s (the rest are counted): a stuck state must not fill mod.log.
local function natSay(text)
	local N = G.nat
	if N then
		local now = os.clock()
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

local function natSend(kind, payload)
	local N = G.nat
	if not N then return false end
	if not N.writer then N.writer = C.newWriter(C.path("native_ctl.log"), C.PLAYER_ID) end
	local ok, err = C.writerSend(N.writer, kind, payload)
	if ok then
		N.lastWrite = os.clock()
	else
		natSay("cannot write native_ctl.log: " .. tostring(err))
	end
	return ok
end

-- The module hands a held command back (or destroys it) at the next CommandList::Add of ANY command; any command we send
-- is one. This one does nothing in the simulation half (docs/NATIVE_LINK.md section 4, "Why a trigger").
local function natTrigger()
	pcall(toSim, "nop", {})
end

-- Stops strict mode: hold_off, so that every held command goes back to the engine at the next Add (and one is sent).
local function natStop(why)
	local N = G.nat
	if not N then return end
	if N.holdSent then
		N.holdSent, N.active = false, false
		-- hold_off gives every held command back at the next Add, before its stamp: the builds enter this world early. Each gets
		-- its replay mark first (the commands queue in this order), or the simulation half would ship it a second time.
		for _, h in pairs(N.heldByKey) do
			if not (h.released or h.done or h.cancelled) then
				h.released, h.early = os.clock(), true
				pcall(toSim, "replay", {})
				pcall(toSim, "heldgone", { key = h.key })
				natSay("held " .. h.id .. " (" .. h.key .. ") goes back to the engine before its stamp (" .. tostring(why) .. ")")
			end
		end
		pcall(natSend, "hold_off", {})
		natTrigger()
		natSay("mode off (" .. tostring(why) .. "), hold_off written")
	end
	N.holdSent, N.active, N.parked, N.caps, N.owed = false, false, {}, {}, {}
end

-- A fault in strict mode never reaches the bridge: it is logged, and the third one switches strict mode off for good.
local function natFault(where, err)
	local N = G.nat
	if not N then return end
	N.faults = N.faults + 1
	natSay("fault in " .. where .. ": " .. tostring(err))
	if N.faults >= 3 and not N.dead then
		N.dead = true
		pcall(natStop, "three faults")
	end
end

local function natPing()
	local N = G.nat
	N.pingN = N.pingN + 1
	N.pingAt = os.clock()
	return natSend("ping", { n = N.pingN })
end

-- release or cancel, and make sure an Add follows: the order is only carried out at the next Add.
local function natOrder(kind, id)
	local N = G.nat
	local ok = natSend(kind, { id = id })
	if ok then
		N.awaiting[id] = { kind = kind, t = os.clock(), tries = 0 }
		natTrigger()
	end
	return ok
end

-- The stamp of a shipped build has come on this game (the simulation half took the snapshot of its entity lists and booked the
-- ledger, as every other game does): the held command goes back to the engine, which applies it. The replay mark comes first
-- in the engine's command list, so the build's event finds it and the simulation half does not capture this build as a new one.
local function natRelease(key)
	local N = G.nat
	local h = N and N.heldByKey[key]
	if not h or h.released or h.done or h.cancelled then return false end
	h.released = os.clock()
	pcall(toSim, "replay", {})
	local ok = natOrder("release", h.id)
	pcall(toSim, "heldgone", { key = key })
	natSay(string.format("held %d (%s) released at game time %d (stamp %d); the engine applies it here, the other games replay it at the stamp",
		h.id, key, engine.gameTime(), h.at or 0))
	return ok
end

-- The action was refused at its stamp (the same rule that refuses it on every other game): the held command is destroyed.
function natHeldRefused(it)
	local N = G.nat
	local h = N and N.heldByKey[it.key]
	if not h or h.released or h.done or h.cancelled then return end
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

local function natSessionOn(N, now)
	if now - N.sessionAt >= 1 then
		N.sessionAt = now
		local text = C.readFile(C.path("identity.txt"))
		local role = text and text:match("role=([%a]+)") or nil
		if role then N.sessionOn = (role == "host" or role == "player") end
	end
	return N.sessionOn
end

-- ---- the build of one held command

local function natSite(h)
	return type(h.site) == "number" and string.format("0x%x", h.site) or "?"
end

-- The held command `h` has the capture `cap`: ship it and cancel the command, or release it.
local function natDecideUnsafe(h, cap)
	local N = G.nat
	local id = h.id
	local function release(why)
		natSay("held " .. id .. " (site " .. natSite(h) .. "): " .. why .. "; release written, the native build goes through")
		natOrder("release", id)
	end
	if not cap then return release("no capture of a tool build") end
	if not cap.body then
		-- A build that cannot be captured cannot be shipped: the others never get it, and the safety net's resync would erase it here
		-- later (real session 2026-10-12: a guest's bus stops, line and buses). With the tool's callback fireable the build is cancelled and
		-- the player told; otherwise it still goes through (fail open: the tool would stay dead).
		if tostring(cap.why):find("has no model", 1, true) then
			-- an edge object (a stop) gets its model and parameters only when the build runs: the record is read AFTER the build (the
			-- simulation half's deferred read, SIM.deferredBuild) and shipped then; nothing to cancel
			return release("the capture of " .. tostring(cap.tool) .. " waits for the build (" .. tostring(cap.why) .. "): the record is read after it ran and shipped then")
		end
		if FIRE_DONE and h.done then
			natSay("held " .. id .. " (site " .. natSite(h) .. "): the capture of " .. tostring(cap.tool) .. " failed (" .. tostring(cap.why) .. "): cancelled, this kind of build is not replicated")
			natOrder("cancel", id)
			vetoChat("nocap:" .. tostring(id) .. ":" .. tostring(os.clock()), "This kind of build (" .. tostring(cap.tool) .. ") is not supported yet in this test build; it was cancelled so that it is not lost later. Roads, bulldozing and constructions still work.")
			return
		end
		return release("the capture of " .. cap.tool .. " failed (" .. tostring(cap.why) .. ")")
	end
	local body, text = cap.body, cap.text
	local category = buildCategory(body)
	if not L.permitted(G.perms, category) then
		-- refused before stamping, like the wrapped sendCommand: nothing was built yet, so nothing is sent to the others
		natSay("held " .. id .. ": not allowed to " .. category .. ", cancelled")
		send("action_refused", { seq = 0, detail = "permission denied: " .. category })
		natOrder("cancel", id)
		return
	end
	-- A build that adds constructions (depot, station, ...): the player gets an informational line. With FEVERLINK3_REMOTE_CONSTRUCTIONS=0
	-- it is cancelled instead, but only when the native module can fire the tool's callback (a cancel without it leaves the tool dead).
	if Build and Build.addsConstructions and Build.addsConstructions(body) then
		if Build.constructionsBlocked(body) and h.done then
			natSay("held " .. id .. ": adds constructions (" .. tostring(cap.tool) .. "), refused in this test build: cancelled")
			natOrder("cancel", id)
			vetoChat("gate:" .. tostring(id) .. ":" .. tostring(os.clock()), "Your build was cancelled: " .. Build.CONSTRUCTIONS_BLOCKED_TEXT .. ".")
			return
		end
		vetoChat("info:constructions", Build.CONSTRUCTIONS_INFO_TEXT .. " (a depot or station you build may not reach the other game, or may upset it).")
	end
	-- never cancel what this game could not rebuild itself (docs/ARCHITECTURE.md section 6)
	local wire = C.parse(text)
	local now = engine.gameTime()
	local okD, args, whyD = pcall(L.decodeCommand, wire, guiCtx({ kind = "action", at = now, from = C.PLAYER_ID, seq = 0, key = "strict-check" }))
	if not okD then args, whyD = nil, tostring(args) end
	if not args then return release("it cannot be rebuilt here (" .. tostring(whyD) .. ")") end
	local seq = G.seq + 1
	local at
	local cancelFlow = STRICT_CANCEL == "all"
	-- "plans": every originator, "host": only the host, for the proven plans (see the comment at STRICT_CANCEL).
	if STRICT_CANCEL == "plans" or (STRICT_CANCEL == "host" and C.PLAYER_ID == 0) then
		local okP, plan = pcall(function() return Build and Build.planOf and Build.planOf(body) end)
		cancelFlow = okP and STRICT_CANCEL_PLANS[plan or ""] == true
	end
	if cancelFlow and FIRE_DONE and not h.done then
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
		-- arrived, 3 steps late). A game may run `aheadFor(speed)` steps plus a batch past the slowest clock, so the lead gets that
		-- on top.
		at = stampAfterFastest(now, CANCEL_EXTRA_STEPS)
	end
	local okS, errS = C.writerSend(G.writer, "action", { stamp = at, seq = seq, body = text, category = category })
	if not okS then return release("the action cannot be sent (" .. tostring(errS) .. ")") end
	G.seq = seq
	local it = { kind = "action", at = at, from = C.PLAYER_ID, seq = seq, body = wire, key = L.itemKey(C.PLAYER_ID, seq) }
	noteBuild(it.key, at)
	if cancelFlow then
		-- cancel flow: the first strict flow. The command is cancelled, every game (this one too) replays the record.
		wire.held = nil
		local okC = natOrder("cancel", id)
		natSay(string.format("held %d (site %s) matched the capture of %s (%d bytes): %s stamped %d (now %d), cancel flow: %s",
			id, natSite(h), cap.tool, #text, it.key, at, now, okC and "cancel written, this game replays the record too" or "CANCEL NOT WRITTEN (the build may be applied twice here)"))
		route(it)
		return
	end
	-- The command stays HELD until the stamp (nobody's world runs ahead of the others). It is not cancelled: a cancelled command
	-- never gives the tool its "applied" callback and the tool stays dead (real run 2026-10-11). At the stamp this game's
	-- simulation half does everything the other games do except sending (simApplyAction, it.heldId) and this half then releases
	-- the held command (natRelease); every OTHER game replays the build at the stamp.
	it.heldId = id
	local rec = { id = id, key = it.key, at = at, t = os.clock() }
	N.heldByKey[it.key], N.byId[id] = rec, rec
	pcall(toSim, "heldout", { key = it.key })
	natSay(string.format("held %d (site %s) matched the capture of %s (%d bytes, %.0f ms to capture, %.0f ms old): %s stamped %d (now %d), held until the stamp",
		id, natSite(h), cap.tool, #text, cap.ms or 0, (os.clock() - cap.t) * 1000, it.key, at, now))
	if wire.commit then
		local ms, rtt = graceMs()
		if Build and Build.addsOrRemovesConstructions and Build.addsOrRemovesConstructions(body) then ms = math.max(ms, CONSTRUCTION_GRACE_MS) end
		G.graces[it.key] = { it = it, due = os.clock() + ms / 1000, ms = ms }
		natSay(string.format("%s: the game is paused, so the build waits %d ms (round trip about %.0f ms) for a veto before it is committed", it.key, ms, rtt))
		return
	end
	route(it)
end

-- A fault while deciding must not leave the command held (it would stay in the ring until the module's 30 s age limit): it
-- is given back, unless the verdict was already written.
local function natDecide(h, cap)
	local ok, err = pcall(natDecideUnsafe, h, cap)
	if ok then return end
	natFault("the verdict on held " .. tostring(h.id), err)
	local N = G.nat
	if N and not N.awaiting[h.id] then pcall(natOrder, "release", h.id) end
end

-- The grace of a paused game's held build is over (or a resync / the end of the session cuts it short): tell the others to apply it
-- and release this game's own command. If the game was unpaused meanwhile the normal stamp logic takes over: a new stamp ahead.
local function natCommit(key, why)
	local g = G.graces[key]
	if not g then return end
	G.graces[key] = nil
	local it = g.it
	local now = engine.gameTime()
	if not pausedNow() then
		it.at = stampAfterFastest(now)
		noteBuild(key, it.at)
		local h = G.nat and G.nat.heldByKey[key]
		if h then h.at = it.at end
		logGui("commit of " .. key .. ": the game is no longer paused, the build gets the stamp " .. it.at .. " (now " .. now .. ")")
	end
	send("build_commit", { seq = it.seq, owner = C.PLAYER_ID, stamp = it.at })
	logGui("commit of " .. key .. " sent (" .. tostring(why) .. "): no veto within " .. tostring(g.ms) .. " ms; every game applies it at " .. it.at)
	route(it)
end

-- Before the game is saved for a resync: no build may stay held across the save. A grace is cut short (committed); a build whose
-- stamp has not come is taken out of the queue and released now, so the save contains it and no game applies it a second time
-- after the reload. Returns the number of releases that must be confirmed by the engine before the save.
local function natSettle(why)
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
		if h.released and not h.applied and not h.done and os.clock() - h.released < 4 then return false end
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
		if not (h.released or h.done or h.cancelled) and not G.graces[key] and h.at then
			if gt >= h.at + HELD_STUCK_STEPS * STEP and now - h.t > 2 then
				natSay(string.format("WATCHDOG: the stamp %d of held %d (%s) passed %d ms ago (game time %d) and the command is still held; the simulation half's release result %s; releasing it now",
					h.at, h.id, key, gt - h.at, gt, G.resultsSeen[key .. "#release" .. tostring(h.id)] and "WAS seen" or "was not seen"))
				natRelease(key)
			elseif gt < h.at and now - h.t > HELD_PAUSED_MAX then
				local why = "held for " .. HELD_PAUSED_MAX .. " s and its stamp still lies ahead (the game is paused)"
				G.vetoed[key] = why
				pcall(toSim, "drop", { key = key, why = why, own = true })
				send("build_veto", { seq = tonumber(key:match(":(%d+)$")), owner = C.PLAYER_ID, why = why, name = C.PLAYER_NAME })
				natVeto(key, C.PLAYER_NAME, why)
				vetoChat(key, tostring(C.PLAYER_NAME) .. "'s build was cancelled on every game: the game stayed paused too long before it could be applied.")
			end
		end
	end
end

-- every frame: graces that are over, parked records whose commit never came
local function commitPump()
	local clock = os.clock()
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
	for key, pk in pairs(G.parked) do
		if clock - pk.wall > PARK_MAX then
			G.parked[key] = nil
			G.vetoed[key] = "no commit"
			logGui("build " .. key .. " was parked for " .. PARK_MAX .. " s and its originator never committed it: dropped")
		end
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
	local now = os.clock()
	natExpire(now)
	-- The tool's own event for an applied build (builder.proposalApply) only comes AFTER the engine applied it, so for a
	-- command that is held it never comes before the release (seen in a real game: every capture arrived after its held
	-- command had been given back). What is known while the command is held is the tool's latest PREVIEW (builder.proposalCreate):
	-- the tool is blocked waiting for its command, so no newer preview can come, and the last one is the proposal that was clicked.
	-- Two or more commands held at once cannot be told apart by one preview (a real tool is blocked after its click, so this
	-- does not happen): give them all back rather than ship the wrong record for one of them.
	if #N.parked > 1 and not N.caps[1] then
		N.preview = nil
		while N.parked[1] do
			natSay("held " .. N.parked[1].id .. ": more than one command held at once; released (the build goes through the old way)")
			natDecide(table.remove(N.parked, 1), nil)
		end
		return
	end
	while N.parked[1] and (N.caps[1] or (N.preview and now - N.preview.t <= NAT_PREVIEW_TTL)) do
		local h, cap = table.remove(N.parked, 1), table.remove(N.caps, 1)
		if not cap then
			cap = N.preview
			N.preview = nil
		end
		natDecide(h, cap)
	end
end

local function natOnHeld(N, p)
	local id = tonumber(p.id)
	if not id then return end
	local h = { id = id, site = tonumber(p.site), t = os.clock(), frame = G.frames, done = p.done == true }
	if not N.holdSent then
		-- strict mode is being switched off: the module has not caught up; give the command back
		natSay("held " .. id .. " while strict mode is off; release written")
		natOrder("release", id)
		return
	end
	N.parked[#N.parked + 1] = h
	if not N.batching then natPair() end
end

-- Called from guiHandleEvent for builder.proposalCreate (the tool's preview): keeps the newest capture for the `held` that follows a click.
local function natCapture(id, name, param)
	local N = G.nat
	if not N or N.dead or not N.holdSent or name ~= "builder.proposalCreate" then return end
	if not (Build and Build.captureToolBuild) then return end
	local t0 = os.clock()
	local ok, body, why, text = pcall(Build.captureToolBuild, id, name, param, {
		plain = C.plain, keyOf = function(e) return L.keyFor(G.rev, e) end,
	})
	if not ok then body, why = nil, tostring(body) end
	local cap = { tool = tostring(id), why = why }
	if body then
		body.category = buildCategory(body)
		local okS, s = pcall(C.serialize, body)
		if okS then text = s else body, cap.why = nil, tostring(s) end
		if body then cap.body, cap.text = body, text end
	end
	cap.t = os.clock()
	cap.ms = (cap.t - t0) * 1000
	-- only the newest preview is kept, and a preview that could not be captured replaces a good one before it: a `held`
	-- must never be paired with the record of an older preview than the one on screen
	N.preview = cap
	natPair()
end

-- ---- events of the module

local function natEvent(N, rec, p, history)
	local kind = rec.kind
	local now = os.clock()
	if kind == "ready" then
		local hooks = {}
		for _, h in ipairs(type(p.hooks) == "table" and p.hooks or {}) do hooks[#hooks + 1] = tostring(h) end
		N.ready = { build = p.build, hooks = hooks }
		N.hookOk = false
		for _, h in ipairs(hooks) do
			if h == "command_list_add" then N.hookOk = true end
		end
		-- a (new) module link: whatever it held is gone
		for _, h in pairs(N.heldByKey) do h.done = true end
		-- a (new) module link: the handshake starts again
		N.holdSent, N.active, N.pongAt, N.pingsTried, N.gaveUp, N.retryAt, N.retries = false, false, nil, 0, false, nil, 0
		if N.hookOk then
			natSay("native module found (build " .. tostring(p.build) .. ", hooks " .. table.concat(hooks, ",") .. ")")
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
			end
		elseif p.on == false and not history and p.reason ~= "hold_off" then
			-- the module switched holding off by itself (silence, control file gone): ours to notice, and to try again later
			if N.holdSent then
				N.holdSent, N.active, N.parked, N.caps, N.owed = false, false, {}, {}, {}
				N.retries = N.retries + 1
				if N.retries > 3 then
					N.gaveUp = true
					natSay("mode is off: the native module switched holding off (" .. tostring(p.reason) .. ") " .. N.retries .. " times; giving up until it reports ready again")
				else
					N.retryAt = now + 3 * N.retries
					natSay("mode is off: the native module switched holding off (" .. tostring(p.reason) .. "); trying again in " .. (3 * N.retries) .. " s")
				end
			end
		end
	elseif history then
		return   -- held / released / dropped / audit of an earlier run of this script: not news
	elseif kind == "held" then
		natOnHeld(N, p)
	elseif kind == "released" or kind == "dropped" then
		local id = tonumber(p.id)
		if id then
			N.awaiting[id] = nil
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
					rec.released, rec.early = os.clock(), true
					natSay("held " .. id .. " (" .. rec.key .. ") was released by the module (" .. tostring(p.reason) .. ") before its stamp: the build is in this world early and is not shipped twice")
				end
			end
		end
	elseif kind == "applied" then
		local id = tonumber(p.id)
		local rec = id and N.byId[id]
		if rec then
			rec.applied = true
			natSay(string.format("held %d (%s) applied by the engine (accepted %s) at game time %d, stamp %d", id, rec.key, tostring(p.ok), engine.gameTime(), rec.at or 0))
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
local function natTick()
	local N = G.nat
	if not N or N.dead then return end
	if not N.seen then
		-- without the module there is no file: look for it a few times a second, not every frame
		local t = os.clock()
		if t < (N.probeAt or 0) then return end
		N.probeAt = t + 0.25
	end
	natReadEvents(N)
	if not N.seen then return end
	local now = os.clock()
	if N.lastTick and now - N.lastTick > 3 then N.watchFrom = now end   -- the game stood still (a load, a save): the module could not be answered either
	N.lastTick = now
	natExpire(now)
	local usable = Build ~= nil and type(Build.captureToolBuild) == "function"
	local reason = nil
	if N.dead then reason = "faults"
	elseif not usable then reason = "the build module has no tool capture"
	elseif N.hookOk == false then reason = "the native module has no command_list_add hook"
	elseif G.loading then reason = "a resync load is under way"
	elseif not natSessionOn(N, now) then reason = "the session ended" end
	if reason then
		if N.holdSent then natStop(reason) end
	elseif N.gaveUp then
		-- waiting for a new `ready`
	elseif not N.holdSent then
		if not (N.retryAt and now < N.retryAt) then
			if not (N.pongAt and now - N.pongAt < NAT_ALIVE) then
				if now - N.pingAt >= 2.5 then
					if N.pingsTried >= 6 then
						N.gaveUp = true
						natSay("mode is off: the native module does not answer pings; waiting for it to report ready")
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
		if now - N.lastWrite >= NAT_PING then natPing() end
		if not N.active then
			if now - N.holdAt > NAT_CONFIRM then
				natSay("mode is off: hold_on was not confirmed within " .. NAT_CONFIRM .. " s (is the hold hook installed?)")
				N.gaveUp = true
				natStop("hold_on not confirmed")
			end
		elseif now - math.max(N.pongAt or 0, N.watchFrom or 0, N.activeSince or 0) > NAT_WATCHDOG then
			natSay("mode is off: the native module stopped answering pings")
			N.gaveUp = true
			natStop("the native module stopped answering")
		elseif N.retries > 0 and now - N.activeSince > 30 then
			N.retries = 0
		end
	end
	-- a `held` whose capture did not come: released after the grace; an order whose Add did not come: another trigger
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

-- Wraps the command factories (to know a command's name and arguments) and sendCommand (to hold it). Commands
-- the mod cannot decode go to the engine untouched: losing a player's action is worse than a visible divergence.
local function installWrap()
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
		api.cmd.sendCommand = function(cmd, cb, progress)
			local tag = cmd ~= nil and G.tags[cmd] or nil
			if tag == nil then return O.sendCommand(cmd, cb, progress) end
			if tag.name == "makeGameSetSpeedCmd" then
				-- the player's speed buttons become requests to the host; the session speed comes back in `session`
				send("speed_request", { speed = tag.args[1] })
				return
			end
			local category = L.categoryOf(tag.name, tag.args)
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
			-- an event nobody replicates (notifications, tutorial steps...) is the normal case, not worth a line each
			if category ~= "local" then logGui("command " .. tag.name .. " sent natively: " .. tostring(err)) end
			return O.sendCommand(cmd, cb, progress)
		end
	end)
	G.wrapped = oks
	logGui("wrapped " .. wrapped .. " command factories" .. (oks and "" or "; sendCommand is read-only, commands stay native"))
end

local H = {}

H.session = function(rec, p)
	Hud.setSession(G.hud, p)
	if L.applySession(G.session, p) then
		logGui("session: started=" .. tostring(G.session.started) .. " speed=" .. G.session.speed .. " pauseAt=" .. tostring(G.session.pauseAt) .. " lead=" .. tostring(G.session.lead))
	end
end

H.peerclock = function(rec, p)
	L.applyPeerClock(G.peers, p, C.PLAYER_ID)
end

H.player_left = function(rec, p)
	if type(p) == "table" and p.id ~= nil then
		L.removePeer(G.peers, p.id)
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

H.resync_save = function(rec, p)
	if type(p) ~= "table" or type(p.name) ~= "string" then logGui("malformed resync_save"); return end
	local function save()
		logGui("resync: saving the game as '" .. p.name .. "'")
		engine.saveGame(p.name, function(ok, err)
			logGui("resync: save " .. (ok and "done" or ("failed: " .. tostring(err))))
			send("save_done", { id = p.id, name = p.name, ok = ok == true, detail = (not ok) and tostring(err) or nil })
		end)
	end
	-- no held build may stay held across the save (a held command is in no world, a queued replay is in the save): settle them first
	local okS, waiting = pcall(natSettle, "resync save")
	if okS and waiting and waiting > 0 then
		logGui("resync: " .. waiting .. " held build(s) are released first; the game is saved when the engine has applied them")
		G.pendingSave = { since = os.clock(), go = save }
		return
	end
	save()
end

-- Loading restarts the scripts, and with them this reader. The sequence of this very line is written to the cursor
-- file first, so the reloaded mod resumes after it and nothing already dispatched is dispatched again; its hello
-- (sent by the normal start-up) tells the host this game is back.
H.resync_load = function(rec, p)
	if type(p) ~= "table" or type(p.name) ~= "string" then logGui("malformed resync_load"); return end
	logGui("resync: loading '" .. p.name .. "'")
	-- a reload while the native module holds commands is unsafe (docs/NATIVE_LINK.md stage 2, H): holding goes off first. The
	-- reloaded script finds `ready` in native_events.log and switches strict mode on again.
	pcall(natStop, "resync load")
	C.writeFile(C.path(C.CURSOR), string.format("%d %d", rec.seq, G.uiReader.lastSeq))
	Hud.writeSnapshot(C, G.hud, { version = nil })
	local ok, err = engine.loadGame(p.name)
	if ok then
		G.loading = true
	else
		C.writeFile(C.path(C.CURSOR), "")
		logGui("resync: load failed: " .. tostring(err))
		send("replay_failed", { detail = "resync_load failed: " .. tostring(err) })
	end
end

H.action = function(rec, p)
	if type(p) == "table" and p.allowed == false then
		-- the host relayed an action its sender had no permission for: it is not applied here, and since the
		-- sender's game may have run it, this game asks for a resync
		logGui("action " .. tostring(rec.from) .. ":" .. tostring(p.seq) .. " relayed with allowed=false: not applied, requesting a resync")
		send("replay_failed", { detail = "action " .. tostring(rec.from) .. ":" .. tostring(p.seq) .. " was not allowed" })
		return
	end
	local it, err = L.actionItem(p, rec.from, C.parse)
	if not it then logGui("action from " .. tostring(rec.from) .. " dropped: " .. tostring(err)); return end
	if type(p.from) == "number" and p.from ~= rec.from then logGui("action payload claims sender " .. p.from .. ", launcher says " .. tostring(rec.from)) end
	if G.vetoed[it.key] then logGui("action " .. it.key .. " was vetoed already: dropped"); return end
	if it.body and it.body.fn == "makeWorldBuildProposalCmd" then
		noteBuild(it.key, it.at)
		if it.body.held == true then
			-- the originator holds its command until the stamp and cancels it on a veto: dry-run now, veto when this game cannot build it
			local now = engine.gameTime()
			if it.body.commit == true then
				-- the originator's game is paused: dry-run now, then park the record until its commit arrives (a veto in between drops it)
				local okV, errV = pcall(vetoCheck, it, now)
				if not okV then logGui("veto check of " .. it.key .. " failed: " .. tostring(errV)) end
				if G.vetoed[it.key] then return end
				G.parked[it.key] = { it = it, wall = os.clock() }
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
-- The cause of a veto in words for the chat; the technical text stays in the log.
local function vetoWords(why, who)
	why = tostring(why or "")
	if why:find("^unresolved node") or why:find("^unresolved segment") or why:find("^unresolved construction") or why:find("^unresolved object") then
		return "a road or building it connects to is missing on " .. who .. "'s game"
	elseif why:find("^ambiguous") then
		return "it matches more than one road on " .. who .. "'s game"
	elseif why:find("^engine:") then
		return "the engine on " .. who .. "'s game refuses it (" .. why:gsub("^engine: ", ""):sub(1, 60) .. ")"
	end
	return "it cannot be made on " .. who .. "'s game (" .. why:sub(1, 60) .. ")"
end

vetoChat = function(key, text)
	G.vetoChatted = G.vetoChatted or {}
	if G.vetoChatted[key] then return end
	G.vetoChatted[key] = true
	text = "[FeverLink3] " .. text
	pcall(Hud.addChat, G.hud, "FeverLink3", text, -1)
	send("chat", { text = text })   -- the host relays it to everybody else: one line per vetoed build
end

H.build_veto = function(rec, p)
	if type(p) ~= "table" or type(p.seq) ~= "number" or type(p.owner) ~= "number" then logGui("malformed build_veto"); return end
	local key = L.itemKey(p.owner, p.seq)
	local why = tostring(p.why or "?")
	local by = tonumber(p.from) or tonumber(rec.from)
	local who = tostring(p.name or by)
	if by == C.PLAYER_ID then return end
	G.vetoed[key] = why
	G.dry[key] = nil
	toSim("drop", { key = key, why = who .. ": " .. why })
	logGui("VETO of " .. key .. " by " .. who .. ": " .. why)
	G.graces[key], G.parked[key] = nil, nil
	if p.owner == C.PLAYER_ID then
		local cancelled = natVeto(key, who, why)
		local me, cause = tostring(C.PLAYER_NAME), vetoWords(why, who)
		if cancelled == nil then
			-- not held here (the old flow): the veto changes nothing on this game
		elseif cancelled then
			vetoChat(key, me .. "'s build was cancelled on every game: " .. cause .. ".")
		else
			-- the command was released already: the build exists on this game, the vetoing game dropped it. The games differ now.
			vetoChat(key, me .. "'s build exists on their game only (" .. cause .. "); the games will be synchronised.")
			logGui("veto of " .. key .. " came too late: this game has the build, " .. who .. " does not; asking the host for a resync")
			send("replay_failed", { detail = "build " .. key .. " exists on one game only (a veto came after it was applied): " .. why:sub(1, 100) })
		end
	end
end

-- The originator of a parked build commits it: apply it now (the pause time) or at the stamp it names.
H.build_commit = function(rec, p)
	if type(p) ~= "table" or type(p.seq) ~= "number" or type(p.owner) ~= "number" then logGui("malformed build_commit"); return end
	local key = L.itemKey(p.owner, p.seq)
	local by = tonumber(p.from) or tonumber(rec.from)
	if by ~= p.owner then logGui("build_commit of " .. key .. " from " .. tostring(by) .. " ignored: only its originator commits"); return end
	local pk = G.parked[key]
	if not pk then logGui("commit of " .. key .. ": nothing parked for it (vetoed, expired or not received)"); return end
	G.parked[key] = nil
	if G.vetoed[key] then return end
	if type(p.stamp) == "number" then pk.it.at = p.stamp end
	noteBuild(key, pk.it.at)
	logGui("build " .. key .. " committed by its originator: applied at " .. tostring(pk.it.at) .. " (now " .. engine.gameTime() .. ")")
	route(pk.it)
end

H.checksum_request = function(rec, p)
	local it = L.checksumItem(p)
	if not it then logGui("malformed checksum_request"); return end
	L.addMark(G.marks, it.at, engine.gameTime())
	route(it)
end

H.chat = function(rec, p)
	if type(p) == "table" then
		logGui("chat " .. tostring(p.name or rec.from) .. ": " .. tostring(p.text))
		Hud.addChat(G.hud, p.name, p.text, p.from or rec.from)
	end
end

local function readLink()
	for _, rec in ipairs(C.readerPoll(G.reader)) do
		if G.loading then
			-- a resync load is under way: the reloaded game reads these lines again from the cursor
			break
		elseif rec.from ~= 0 and rec.kind ~= "action" then
			logGui("line " .. rec.seq .. " kind " .. rec.kind .. " from " .. tostring(rec.from) .. " ignored: only the host controls the session")
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
local function collectFromSim(state)
	local ok, st = pcall(function() return state:get() end)
	if not ok or type(st) ~= "table" then return end
	-- companies: the mirror takes the state's registry when that is newer; during a pause it is ahead of the state
	if Co and type(st.co) == "table" and (G.co == nil or (st.co.rev or 0) > (G.co.rev or 0)) then
		coGuard("mirror", function() G.co = C.plain(st.co) end)
	end
	for key, e in pairs(st.bind or {}) do
		if G.bind[key] ~= e then G.bind[key] = e; G.rev[e] = key end
	end
	G.notes = st.notes
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
		end
	end
	for _, r in ipairs(st.refused or {}) do
		if not G.refusedSent[r.key] then
			G.refusedSent[r.key] = true
			if G.nat then pcall(natHeldRefused, { key = r.key }) end
			send("action_refused", { seq = r.seq, from = r.from, detail = r.detail })
			deliverResult(r.key, nil, false, {})
		end
	end
	for _, r in ipairs(st.reports or {}) do
		if not G.reportsSent[r.n] then
			reportRound(r.n, r.time, r.parts, r.account)
		end
	end
	if (st.late or 0) > G.lateSeen then
		G.lateSeen = st.late
		logGui("the simulation applied an action late (" .. st.late .. " so far)")
	end
end

-- companies: the window's company page is a snapshot file of its own (companies.txt = Co.status, docs/COMPANIES.md
-- section 6), rewritten when the registry or the roster changed, and every few seconds while there are lines whose
-- result moves with the game.
local function writeCompanies()
	if not (Co and G.co) then return end
	local hasLines = false
	for _, m in pairs(G.co.made) do
		if m.kind == "LINE" then hasLines = true; break end
	end
	local stamp = tostring(G.co.rev) .. ":" .. tostring(G.hud.version) .. (hasLines and (":" .. tostring(math.floor(os.clock() / 5))) or "")
	if G.coWritten == stamp then return end
	local names = {}
	for id, p in pairs(G.hud.roster) do names[id] = p.name end
	local now = engine.gameTime()
	local earned = Co.earned(G.co, function(key)
		local e = G.bind[key]
		if e == nil then return nil end
		local ok, v = pcall(engine.lineResult, e, 0, now)
		return ok and v or nil
	end)
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
local function serviceWindow()
	if G.frames % 3 ~= 0 then return end
	for _, rec in ipairs(C.readerPoll(G.uiReader)) do
		if rec.kind == "chat" then
			local p = C.parse(rec.payload)
			local text = type(p) == "table" and Hud.cleanText(p.text) or nil
			if text then
				send("chat", { text = text })
				Hud.addChat(G.hud, C.PLAYER_NAME, text, C.PLAYER_ID)   -- the host does not echo a message to its sender
			end
		elseif rec.kind == "cmd" then
			local p = C.parse(rec.payload)
			if type(p) == "table" and type(p.body) == "string" and type(p.id) == "number" then
				local okN, okShip, whyShip = pcall(shipUiCommand, p)
				if not okN then logGui("shipping a window command failed: " .. tostring(okShip))
				elseif not okShip then logGui("a window command was not shipped: " .. tostring(whyShip)) end
			end
		elseif rec.kind == "bind" then
			-- the entity a window command created here, named by the key the other games give theirs
			local p = C.parse(rec.payload)
			local seq = type(p) == "table" and G.uiSeq[p.id] or nil
			if seq and type(p.e) == "number" then
				local key = L.itemKey(C.PLAYER_ID, seq)
				G.bind[key] = p.e
				G.rev[p.e] = key
				toSim("bind", { key = key, e = p.e })
			end
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
	Hud.writeSnapshot(C, G.hud, G.hudWritten)
	coGuard("page", writeCompanies)
	if G.frames % 30 == 0 then
		local okB, text = pcall(C.serialize, G.bind)
		if okB and text ~= G.bindsWritten and C.writeFile(C.path("binds.txt"), text) then G.bindsWritten = text end
	end
end

-- companies: in separate mode (experimental, docs/COMPANIES.md section 8) this game's windows act as the player's own
-- company, by making api.engine.util.getPlayer say so; in shared mode, or outside any company, as the game's own player.
local function syncLocalCompany()
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

local function guiUpdate(userParams, state, guiState)
	C.poll()
	if not C.ACTIVE then
		if G.nat and G.nat.holdSent then pcall(natStop, "the mod is not active") end
		return
	end
	G.frames = G.frames + 1
	if not G.started then
		G.started = true
		G.reader = C.newReader(C.path(C.TO_GAME))
		G.writer = C.newWriter(C.path(C.FROM_GAME), C.PLAYER_ID)
		G.hud = Hud.new(C.PLAYER_ID, C.PLAYER_NAME)
		G.co = Co and Co.new() or nil
		-- companies: a reload runs this file again in the same Lua state, so an override of the previous run may still be in place
		if Co and engine.setLocalCompany then engine.setLocalCompany(nil) end
		G.uiReader = C.newReader(C.path(Hud.OUTBOX))
		-- after a resync load: resume the link after the line that asked for it, and keep the window's roster and chat
		local cursor, uiCursor = (C.readFile(C.path(C.CURSOR)) or ""):match("^(%d+) (%d+)")
		cursor, uiCursor = tonumber(cursor), tonumber(uiCursor)
		if cursor and cursor > 0 then
			G.reader.lastSeq = cursor
			G.uiReader.lastSeq = uiCursor or 0
			C.writeFile(C.path(C.CURSOR), "")
			local restored = Hud.fromSnapshot(C.parse(C.readFile(C.path(Hud.SNAPSHOT)) or ""))
			if restored and restored.selfId == C.PLAYER_ID then G.hud = restored end
			logGui("reloaded after a resync: link resumes after line " .. string.format("%d", cursor))
		end
		installWrap()
		local okN, nat = pcall(natNew)
		G.nat = okN and nat or nil
		pcall(function() guiState:subscribeToAllEvents() end)
		setSpeed(0)
		send("hello", { time = engine.gameTime(), name = C.PLAYER_NAME })
		logGui("started: player " .. tostring(C.PLAYER_ID) .. " (" .. C.PLAYER_NAME .. ") dir " .. C.DIR)
	end
	if G.frames % 20 == 0 then devPoll("gui") end
	local ok, err = pcall(function()
		readLink()
		serviceWindow()
		collectFromSim(state)
		local okT, errT = pcall(natTick)
		if not okT then natFault("frame", errT) end
		local okG, errG = pcall(commitPump)
		if not okG then logGui("commit pump failed: " .. tostring(errG)) end
		local okD, errD = pcall(dryPump)
		if not okD then logGui("veto check failed: " .. tostring(errD)) end
		coGuard("local company", syncLocalCompany)
		local now = engine.gameTime()
		local ahead = L.aheadFor(G.session.speed)
		setSpeed(L.pace(G.session, G.peers, now, ahead, G.marks))
		if now ~= G.lastClock or G.frames % 30 == 0 then
			G.lastClock = now
			send("clock", { t = now, speed = engine.speed(), ahead = ahead })
		end
	end)
	if not ok then logGui("frame failed: " .. tostring(err)) end
end

-- The engine's tool events (builder.proposalCreate / builder.proposalApply, id = the tool): the construction schema logs
-- what it sees and what it would capture, so that the first game run shows the real shapes (finding from MPFever (MIT):
-- these events reach guiHandleEvent). Nothing is replicated from them except in strict mode (natCapture).
local function guiHandleEvent(userParams, state, guiState, src, id, name, param)
	if not C.ACTIVE or type(name) ~= "string" or not name:find("^builder%.") then return end
	for _, M in ipairs(CMDS) do
		if type(M.onToolEvent) == "function" then
			local ok, err = pcall(M.onToolEvent, id, name, param, logGui)
			if not ok then logGui("tool event handler failed: " .. tostring(err)) end
		end
	end
	-- strict mode (only while the native module holds build commands): keep the build for the `held` event that follows
	if name == "builder.proposalCreate" and G.nat then
		local ok, err = pcall(natCapture, id, name, param)
		if not ok then natFault("capture", err) end
	end
end

function data()
	return {
		update = update,
		handleEvent = handleEvent,
		guiUpdate = guiUpdate,
		guiHandleEvent = guiHandleEvent,
	}
end
