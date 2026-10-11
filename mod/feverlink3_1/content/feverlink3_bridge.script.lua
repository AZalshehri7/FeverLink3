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
--     hands the originator's callback the real result, writes checksum reports and refusals.
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

-- `co` is the company registry of the half that asks; its digest is a part of its own once anything was configured or recorded
local function checksumParts(co)
	local parts, t = L.checksumParts(engine, C.hashValue)
	local okD, digest = false, nil
	if Co then okD, digest = pcall(Co.digest, co) end
	if okD and digest ~= nil then parts.companies = C.hashValue(digest) end
	return parts, t
end

-- ================================================================== simulation half

local logSim = C.logger("sim")
local SIM = { rng = nil, subscribed = false }

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
local function markReplay(st, now, state)
	local r = st.replay
	if type(r) ~= "table" or now - (r.at or 0) > REPLAY_STEPS * STEP then r = { n = 0, at = now } end
	r.n = r.n + 1
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

local function spendReplay(st, now)
	local r = st.replay
	if type(r) ~= "table" or r.n <= 0 then return false end
	if now - (r.at or 0) > REPLAY_STEPS * STEP then
		st.replay = nil
		return false
	end
	r.n = r.n - 1
	return true
end

-- A build the player made with a native tool (roads, tracks, stations, depots, bulldozer...): the engine applies it
-- itself, here and now. It is captured as a build record and handed to the interface half (st.out), which stamps and
-- ships it like any other action; every OTHER game rebuilds it at the stamp.
local function simCaptureBuild(state, param)
	local st = simState(state)
	local now = engine.gameTime()
	if spendReplay(st, now) then
		state:set(st)
		return
	end
	local proposal, playerInitiated = nil, nil
	pcall(function() proposal, playerInitiated = param[1], param[4] end)
	if proposal == nil or playerInitiated == false or not Build then return end
	local rev = {}
	for key, e in pairs(st.bind) do rev[e] = key end
	local t0 = os.clock()
	local ok, body, why = pcall(Build.capture, proposal, { playerInitiated = true }, {
		plain = C.plain, keyOf = function(e) return L.keyFor(rev, e) end,
	})
	if not ok then body, why = nil, tostring(body) end
	if not body then
		logSim("a native build at t=" .. now .. " is not replicated: " .. tostring(why))
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
	local okS, text = pcall(C.serialize, body)
	if not okS then
		logSim("a native build at t=" .. now .. " is not replicated: " .. tostring(text))
		return
	end
	st.out = st.out or {}
	st.outSeq = (st.outSeq or 0) + 1
	st.out[#st.out + 1] = { n = st.outSeq, at = now, body = text }
	trim(st.out, OUT_KEEP)
	state:set(st)
	logSim("native build captured at t=" .. now .. " (" .. #text .. " bytes, " .. string.format("%.3f", os.clock() - t0) .. " s)")
	-- self-check: could this record be rebuilt here, the way another game will rebuild it? Only the engine's command
	-- factory is called, nothing is sent.
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
	if it.lateSteps and it.lateSteps > 0 then
		logSim("LATE action " .. it.key .. " for t=" .. it.at .. " applied at " .. now .. " (" .. it.lateSteps .. " step(s) late)")
		st.late = (st.late or 0) + 1
	end
	local ctx = simCtx(st, it)
	local args, err = L.decodeCommand(it.body, ctx)
	if not args then
		logSim("action " .. it.key .. " refused: " .. tostring(err))
		st.refused[#st.refused + 1] = { key = it.key, from = it.from, seq = it.seq, detail = err }
		return
	end
	local spec = L.COMMANDS[it.body.fn]
	local category = L.categoryOf(it.body.fn, args)
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
	local ok, e = pcall(function()
		L.sendNow(args, function(a)
			local isBuild = it.body.fn == "makeWorldBuildProposalCmd"
			if isBuild then markReplay(st, now, SIM.state) end
			api.cmd.sendCommand(api.cmd[it.body.fn](unpack(a, 1, a.n)))
			if isBuild then takeBackEvents(st, SIM.state) end
		end)
	end)
	if not ok then
		logSim("action " .. it.key .. " refused by the engine: " .. tostring(e))
		st.refused[#st.refused + 1] = { key = it.key, from = it.from, seq = it.seq, detail = tostring(e) }
		return
	end
	coAfter(ctx, it, spec, category, balance0, logSim)
	logSim("action " .. it.key .. " " .. it.body.fn .. " applied at t=" .. now)
	if bindComps then
		st.binders[#st.binders + 1] = { key = it.key, fn = it.body.fn, body = it.body, comps = bindComps, before = bindBefore, steps = 0 }
	end
	if spec.creates then
		st.watches[#st.watches + 1] = { key = it.key, comp = spec.creates, result = spec.result, before = before, steps = 0, at = now }
	else
		st.results[#st.results + 1] = { key = it.key, at = now, success = true }
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
		category = L.categoryOf(it.body.fn, args)
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
local function simAdvanceBinders(st)
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
			logSim("action " .. b.key .. ": not every entity it created was found after " .. b.steps .. " steps")
		else
			keep[#keep + 1] = b
		end
	end
	st.binders = keep
end

local function update(userParams, state, dt)
	C.poll()
	if not C.ACTIVE then return end
	if not SIM.rng then
		SIM.rng = L.newRandom(engine.gameTime)
		L.installRandom(SIM.rng)
		logSim("deterministic math.random installed")
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
				local parts, t = checksumParts(st.co)
				st.reports[#st.reports + 1] = { n = it.n, time = t, parts = parts, account = L.accountSnapshot(engine) }
			elseif it.kind == "action" then
				simApplyAction(st, it, now)
			end
		end
		simAdvanceWatches(st, now)
		simAdvanceBinders(st)
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
	if id == "apply_command" and name == "onPreBuildProposal" then
		C.poll()
		if not C.ACTIVE then return end
		local ok, err = pcall(simCaptureBuild, state, param)
		if not ok then logSim("onPreBuildProposal capture failed: " .. tostring(err)) end
		return
	end
	C.poll()
	if not C.ACTIVE or id ~= "feverlink3" then return end
	local ok, err = pcall(function()
		local st = simState(state)
		if name == "queue" and type(param) == "table" then
			L.enqueue(st.queue, param)
		elseif name == "replay" then
			markReplay(st, engine.gameTime())
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

local function guiApplyNow(it, now)
	if it.kind == "action" and it.body and it.body.native and it.from == C.PLAYER_ID then return end
	if it.kind == "checksum" then
		local parts, t = checksumParts(G.co)
		reportRound(it.n, t, parts, L.accountSnapshot(engine))
		return
	end
	if it.lateSteps and it.lateSteps > 0 then logGui("LATE action " .. it.key .. " for t=" .. it.at .. " applied at " .. now) end
	local ctx = guiCtx(it)
	local args, err = L.decodeCommand(it.body, ctx)
	if not args then
		logGui("action " .. it.key .. " refused: " .. tostring(err))
		send("action_refused", { seq = it.seq, from = it.from, detail = err })
		deliverResult(it.key, nil, false, {})
		return
	end
	local spec = L.COMMANDS[it.body.fn]
	local category = L.categoryOf(it.body.fn, args)
	-- companies: the registry and ledger part runs here on the mirror and is replayed on the simulation state (coToSim)
	local okC, fx = coBefore(ctx, it, spec, args, category, logGui)
	if not okC then
		logGui("action " .. it.key .. " refused: " .. tostring(fx))
		send("action_refused", { seq = it.seq, from = it.from, detail = fx })
		deliverResult(it.key, nil, false, {})
		coToSim(it)
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
		logGui("action " .. it.key .. " " .. it.body.fn .. " applied during the pause at t=" .. now .. (success and "" or " REFUSED"))
		deliverResult(it.key, data, success, entities)
	end
	-- the engine's factory may reject what was rebuilt: that refuses the action, like any other refusal
	local okS, errS = pcall(L.runChained, args, function(a, cb)
		if it.body.fn == "makeWorldBuildProposalCmd" then toSim("replay", {}) end
		O.sendCommand((O.factory[it.body.fn] or api.cmd[it.body.fn])(unpack(a, 1, a.n)), cb)
	end, finish)
	if not okS then
		logGui("action " .. it.key .. " refused by the engine: " .. tostring(errS))
		send("action_refused", { seq = it.seq, from = it.from, detail = tostring(errS) })
		deliverResult(it.key, nil, false, {})
		coToSim(it)
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
local function stampAfterFastest(now)
	local base = now
	for _, pc in pairs(G.peers) do
		if type(pc) == "table" and type(pc.t) == "number" and pc.t > base then base = pc.t end
	end
	return L.stampFor(G.session, base)
end

local function shipNative(o)
	local body = C.parse(o.body)
	if type(body) ~= "table" or type(body.args) ~= "table" then return false, "unreadable record" end
	local rec = body.args[1]
	local adds = 0
	if type(rec) == "table" then
		adds = #(rec.cons or {}) + #(rec.nodes or {}) + #(rec.segs or {}) + #(rec.objs or {})
	end
	local category = adds == 0 and "bulldoze" or "build"
	local now = engine.gameTime()
	G.seq = G.seq + 1
	local at = stampAfterFastest(now)
	send("action", { stamp = at, seq = G.seq, body = o.body, category = category })
	logGui("native build " .. L.itemKey(C.PLAYER_ID, G.seq) .. " captured at t=" .. tostring(o.at) .. ", stamped " .. at .. " (now " .. now .. ", " .. #o.body .. " bytes)")
	recordOriginator(body, G.seq, at, category)
	return true
end

-- A command a window issued, recorded by the UI-state hook (feverlink3_uihook.lua), which applied it natively already:
-- stamped and shipped; the other games apply it at the stamp.
local function shipUiCommand(p)
	local body = C.parse(p.body)
	if type(body) ~= "table" or type(body.args) ~= "table" then return false, "unreadable record" end
	local now = engine.gameTime()
	G.seq = G.seq + 1
	G.uiSeq[p.id] = G.seq
	local at = stampAfterFastest(now)
	local category = type(p.category) == "string" and p.category or "other"
	send("action", { stamp = at, seq = G.seq, body = p.body, category = category })
	logGui("window command " .. tostring(body.fn) .. " " .. L.itemKey(C.PLAYER_ID, G.seq) .. " stamped " .. at .. " (now " .. now .. ")")
	recordOriginator(body, G.seq, at, category)
	return true
end

-- A command this player issued (held by the wrapped sendCommand): encode, stamp, ship, queue locally.
local function shipLocal(tag, cb)
	local body, err = L.encodeCommand(tag.name, tag.args, { plain = C.plain, keyOf = function(e) return L.keyFor(G.rev, e) end })
	if not body then return false, err end
	local now = engine.gameTime()
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
	if type(p) == "table" and p.id ~= nil then L.removePeer(G.peers, p.id) end
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
	logGui("resync: saving the game as '" .. p.name .. "'")
	engine.saveGame(p.name, function(ok, err)
		logGui("resync: save " .. (ok and "done" or ("failed: " .. tostring(err))))
		send("save_done", { id = p.id, name = p.name, ok = ok == true, detail = (not ok) and tostring(err) or nil })
	end)
end

-- Loading restarts the scripts, and with them this reader. The sequence of this very line is written to the cursor
-- file first, so the reloaded mod resumes after it and nothing already dispatched is dispatched again; its hello
-- (sent by the normal start-up) tells the host this game is back.
H.resync_load = function(rec, p)
	if type(p) ~= "table" or type(p.name) ~= "string" then logGui("malformed resync_load"); return end
	logGui("resync: loading '" .. p.name .. "'")
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
	route(it)
end

H.checksum_request = function(rec, p)
	local it = L.checksumItem(p)
	if not it then logGui("malformed checksum_request"); return end
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
		end
	end
	for _, r in ipairs(st.refused or {}) do
		if not G.refusedSent[r.key] then
			G.refusedSent[r.key] = true
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
	if not C.ACTIVE then return end
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
		coGuard("local company", syncLocalCompany)
		local now = engine.gameTime()
		local ahead = L.aheadFor(G.session.speed)
		setSpeed(L.pace(G.session, G.peers, now, ahead))
		if now ~= G.lastClock or G.frames % 30 == 0 then
			G.lastClock = now
			send("clock", { t = now, speed = engine.speed(), ahead = ahead })
		end
	end)
	if not ok then logGui("frame failed: " .. tostring(err)) end
end

-- The engine's tool events (builder.proposalCreate / builder.proposalApply, id = the tool): the construction schema logs
-- what it sees and what it would capture, so that the first game run shows the real shapes (finding from MPFever (MIT):
-- these events reach guiHandleEvent). Nothing is replicated from them yet.
local function guiHandleEvent(userParams, state, guiState, src, id, name, param)
	if not C.ACTIVE or type(name) ~= "string" or not name:find("^builder%.") then return end
	for _, M in ipairs(CMDS) do
		if type(M.onToolEvent) == "function" then
			local ok, err = pcall(M.onToolEvent, id, name, param, logGui)
			if not ok then logGui("tool event handler failed: " .. tostring(err)) end
		end
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
