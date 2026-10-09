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
for _, name in ipairs({ "feverlink3_cmds_build", "feverlink3_cmds_ops", "feverlink3_cmds_company" }) do
	local okM, M = pcall(ug_require, name .. ".lua")
	if not okM then okM, M = pcall(ug_require, "feverlink3_1::/" .. name .. ".lua") end
	if okM and type(M) == "table" then
		L.registerCommands(M.commands)
		if M.events then L.registerEvents(M.events) end
	end
end

local STEP = L.STEP
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
	engine.adjustLoan = function(delta)
		if off then error("loan correction is off: the loan moved the wrong way after the last one") end
		local _, loan = engine.account()
		if last and type(loan) == "number" and type(last.loan) == "number" then
			local moved = loan - last.loan
			if moved * last.delta < 0 and math.abs(moved) >= math.abs(last.delta) / 2 then
				off = true
				error("loan correction is off: the loan moved by " .. moved .. " after a correction of " .. last.delta)
			end
		end
		engine.bookJournal(delta, "LOAN")
		engine.bookJournal(-delta, "OTHER")
		last = { delta = delta, loan = loan }
	end
end

-- The test harness (or a game build with other accessors) replaces any of the above through a global table.
do
	local okE, override = pcall(function() return FEVERLINK3_ENGINE end)
	if okE and type(override) == "table" then
		for k, v in pairs(override) do engine[k] = v end
	end
end

local function checksumParts()
	return L.checksumParts(engine, C.hashValue)
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
	return st
end

local function trim(list, keep)
	while #list > keep do table.remove(list, 1) end
end

local function simCtx(st)
	return { player = engine.player(), resolve = function(key) return L.resolveKey(st.bind, key) end }
end

-- Applies one due action in the simulation state. No callback here (finding from MPFever (MIT): the engine
-- rejects callbacks from inside the simulation update), so an entity the command creates is found afterwards by
-- comparing the entity lists (a watch), and the result reaches the originator through the state.
local function simApplyAction(st, it, now)
	if it.lateSteps and it.lateSteps > 0 then
		logSim("LATE action " .. it.key .. " for t=" .. it.at .. " applied at " .. now .. " (" .. it.lateSteps .. " step(s) late)")
		st.late = (st.late or 0) + 1
	end
	local args, err = L.decodeCommand(it.body, simCtx(st))
	if not args then
		logSim("action " .. it.key .. " refused: " .. tostring(err))
		st.refused[#st.refused + 1] = { key = it.key, from = it.from, seq = it.seq, detail = err }
		return
	end
	local spec = L.COMMANDS[it.body.fn]
	local before = spec.creates and engine.entitiesWith(spec.creates) or nil
	local ok, e = pcall(function()
		api.cmd.sendCommand(api.cmd[it.body.fn](unpack(args, 1, args.n)))
	end)
	if not ok then
		logSim("action " .. it.key .. " refused by the engine: " .. tostring(e))
		st.refused[#st.refused + 1] = { key = it.key, from = it.from, seq = it.seq, detail = tostring(e) }
		return
	end
	logSim("action " .. it.key .. " " .. it.body.fn .. " applied at t=" .. now)
	if spec.creates then
		st.watches[#st.watches + 1] = { key = it.key, comp = spec.creates, result = spec.result, before = before, steps = 0, at = now }
	else
		st.results[#st.results + 1] = { key = it.key, at = now, success = true }
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
	local ok, err = pcall(function()
		local st = simState(state)
		local now = engine.gameTime()
		local due = L.takeDue(st.queue, now)
		for _, it in ipairs(due) do
			if it.kind == "checksum" then
				local parts, t = checksumParts()
				st.reports[#st.reports + 1] = { n = it.n, time = t, parts = parts, account = L.accountSnapshot(engine) }
			elseif it.kind == "action" then
				simApplyAction(st, it, now)
			end
		end
		simAdvanceWatches(st, now)
		trim(st.results, RESULT_KEEP)
		trim(st.reports, RESULT_KEEP)
		trim(st.refused, RESULT_KEEP)
		state:set(st)
	end)
	if not ok then logSim("update failed: " .. tostring(err)) end
end

-- Messages from the interface half (makeScriptingSendEventCmd with id "feverlink3").
local function handleEvent(userParams, state, src, id, name, param)
	C.poll()
	if not C.ACTIVE or id ~= "feverlink3" then return end
	local ok, err = pcall(function()
		local st = simState(state)
		if name == "queue" and type(param) == "table" then
			L.enqueue(st.queue, param)
		elseif name == "bind" and type(param) == "table" and param.key then
			st.bind[param.key] = param.e
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
	tags = setmetatable({}, { __mode = "k" }),  -- command object -> { name, args }
	lastSpeedSet = nil, lastClock = nil,
	resultsSeen = {}, reportsSent = {}, refusedSent = {}, lateSeen = 0,
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

local function toSim(name, param)
	O.sendCommand(O.event("feverlink3", "feverlink3", name, param))
end

local function setSpeed(sp)
	if sp ~= G.lastSpeedSet or G.frames % 60 == 0 then
		G.lastSpeedSet = sp
		O.sendCommand(O.setSpeed(sp))
	end
end

local function guiCtx()
	return { player = engine.player(), resolve = function(key) return L.resolveKey(G.bind, key) end }
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
local function reportRound(n, t, parts, account)
	G.reportsSent[n] = true
	G.accountAt[n] = account
	G.accountAt[n - 40] = nil
	send("checksum_report", { n = n, time = t, parts = parts, authority = (C.PLAYER_ID == 0) and account or nil })
end

local function guiApplyNow(it, now)
	if it.kind == "checksum" then
		local parts, t = checksumParts()
		reportRound(it.n, t, parts, L.accountSnapshot(engine))
		return
	end
	if it.lateSteps and it.lateSteps > 0 then logGui("LATE action " .. it.key .. " for t=" .. it.at .. " applied at " .. now) end
	local args, err = L.decodeCommand(it.body, guiCtx())
	if not args then
		logGui("action " .. it.key .. " refused: " .. tostring(err))
		send("action_refused", { seq = it.seq, from = it.from, detail = err })
		deliverResult(it.key, nil, false, {})
		return
	end
	local spec = L.COMMANDS[it.body.fn]
	local cmd = (O.factory[it.body.fn] or api.cmd[it.body.fn])(unpack(args, 1, args.n))
	O.sendCommand(cmd, function(data, success, entities)
		if spec.creates and success then
			local e = type(data) == "table" and data[spec.result] or nil
			if e == nil and type(entities) == "table" and entities[1] then e = entities[1][1] end
			if e ~= nil then
				G.bind[it.key] = e
				G.rev[e] = it.key
				toSim("bind", { key = it.key, e = e })
			end
		end
		logGui("action " .. it.key .. " " .. it.body.fn .. " applied during the pause at t=" .. now .. (success and "" or " REFUSED"))
		deliverResult(it.key, data, success, entities)
	end)
end

local function route(it)
	local now = engine.gameTime()
	if L.applyNow(G.session, now, it) then guiApplyNow(it, now) else toSim("queue", it) end
end

-- A command this player issued (held by the wrapped sendCommand): encode, stamp, ship, queue locally.
local function shipLocal(tag, cb)
	local body, err = L.encodeCommand(tag.name, tag.args, { plain = C.plain, keyOf = function(e) return L.keyFor(G.rev, e) end })
	if not body then return false, err end
	local now = engine.gameTime()
	G.seq = G.seq + 1
	local at = L.stampFor(G.session, now)
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
			-- a build without this factory may refuse the lookup instead of answering nil (MPFever looked each one up guarded)
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
			local ok, err = shipLocal(tag, cb)
			if ok then return end
			logGui("command " .. tag.name .. " sent natively: " .. tostring(err))
			return O.sendCommand(cmd, cb, progress)
		end
	end)
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
	if C.PLAYER_ID == 0 or type(p) ~= "table" or type(p.values) ~= "table" then return end
	local own = G.accountAt[p.n]
	if not own then logGui("authority for round " .. tostring(p.n) .. " skipped: no account recorded for it"); return end
	local applied, failed = L.applyAuthority(engine, own, p.values)
	if applied.balance then logGui("host authority: balance corrected by " .. applied.balance .. " (round " .. tostring(p.n) .. ")") end
	if applied.loan then logGui("host authority: loan corrected by " .. applied.loan .. " (round " .. tostring(p.n) .. ")") end
	for _, name in ipairs(failed) do logGui("host authority: " .. name .. " differs but this build cannot correct it") end
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
	for key, e in pairs(st.bind or {}) do
		if G.bind[key] ~= e then G.bind[key] = e; G.rev[e] = key end
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
		end
	end
	Hud.writeSnapshot(C, G.hud, G.hudWritten)
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
	local ok, err = pcall(function()
		readLink()
		serviceWindow()
		collectFromSim(state)
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

local function guiHandleEvent(userParams, state, guiState, src, id, name, param)
	-- nothing yet: tool events are not needed for the replication modes implemented here
end

function data()
	return {
		update = update,
		handleEvent = handleEvent,
		guiUpdate = guiUpdate,
		guiHandleEvent = guiHandleEvent,
	}
end
