-- FeverLink3 UI-state hook.
--
-- ENGINE-BOUND. The game's own windows and tools (loans, renaming, lines, vehicles, speed buttons) live in the interface
-- Lua state of the react toolkit, which is not the state the bridge's guiUpdate runs in: a wrap of api.cmd made there
-- never sees them (seen in a real game: a loan taken in the window never reached the bridge). This module is loaded by
-- the window script, which does run in that state, and wraps the command functions there.
--
-- What it does with a command the player issues (and only while a session is on):
--   * a speed button (makeGameSetSpeedCmd): not applied. It becomes a `speed` record for the bridge, which asks the
--     host; the session speed comes back and the bridge sets the engine speed. So the game's own bar is the speed
--     control, as in Transport Fever 2 Multiplayer.
--   * a command with a schema (feverlink3_cmds_*.lua, the same registry the bridge uses): encoded, handed to the bridge
--     as a `cmd` record (ui_out.log) and HELD: the engine does not get it here. The bridge stamps it and queues it like
--     every other action, so this game applies it at the stamp through the same queue as the other games (shipLocal's path).
--     Every entity the simulation creates takes its id from one FIFO of free ids, so a command applied at the click here and
--     at the stamp elsewhere shifts every later id (see the bridge's deferred completion of builds). The bridge answers with a
--     `result` record in ui_in.log once the stamp apply is known (applied, or refused there, which every game decides alike),
--     and the callback the window passed runs then, with the entity the command created (M.poll). Nothing here ever waits for that
--     answer: sendCommand returns at once and the window goes on. The commands one handler of the window issues in one go (a clone
--     of N vehicles is N purchases, then N line assignments from their callbacks) carry one group number, and the bridge ships each
--     group as one order: one stamp, applied in one step on every game, refused as a whole when any of it cannot be shipped.
--     One that cannot be encoded (it does not rebuild equal, it names what cannot travel, the hook itself fails) is REFUSED on
--     this game too: its callback fails and the player is told in the window's notices. It is never applied here alone (refuse).
--   * another mod's own script event (its window sends a setting to its game script, makeScriptingSendEventCmd): no schema of ours
--     knows it, but when its parameter is plain data it is shipped and held like the above (L.markForeignEvent), so every game's
--     copy of that mod gets it at the stamp. Told apart by where the calling code lies (C.callerOf), never by a list of mods.
--   * a player's build another mod's tool or window sends (makeWorldBuildProposalCmd): recorded with the build codec and held the same
--     way (the API layer); one the codec cannot record is left to the native module's hold at the command list (the safety net), or
--     refused when that is not on (handle).
--   * anything else (no schema, or an event no game replicates), and everything while no session is on: untouched.
-- Entity ids never travel; the keys of entities created in the session come from binds.txt, which the bridge writes.
--
-- Credit: wrapping the engine's command functions in the interface state, keeping the command objects in a weak table, and
-- keeping the originals in package.loaded follow MPFever's UI hook (mpfever_ui.script.lua, MIT). Holding the command until its
-- stamp and completing the window's callback with the stamp apply's result is this project's general rule for every command
-- (the builds' deferred completion, the bridge's own wrap), so the originator applies at the same step as everybody.
--
-- With FEVERLINK3_DEV=1 every command function of api.cmd is traced into dev_trace.txt (name and arguments), which is how
-- the real shape of what the game's windows issue is learned without guessing.

local okC, C = pcall(ug_require, "feverlink3_common.lua")
if not okC then C = ug_require("feverlink3::/feverlink3_common.lua") end
local okL, L = pcall(ug_require, "feverlink3_lockstep.lua")
if not okL then L = ug_require("feverlink3::/feverlink3_lockstep.lua") end
local okH, Hud = pcall(ug_require, "feverlink3_hud.lua")
if not okH then Hud = ug_require("feverlink3::/feverlink3_hud.lua") end

local log = C.logger("uihook")
local M = { installed = false }

-- the schemas of the other areas, loaded as the bridge loads them (each optional); `counters`: the world searches a schema made
-- (the timing line shows how many a command cost)
local COUNTERS = {}
for _, name in ipairs({ "feverlink3_cmds_build", "feverlink3_cmds_ops", "feverlink3_cmds_company" }) do
	local okM, mod = pcall(ug_require, name .. ".lua")
	if not okM then okM, mod = pcall(ug_require, "feverlink3::/" .. name .. ".lua") end
	if okM and type(mod) == "table" then
		L.registerCommands(mod.commands)
		if mod.events then L.registerEvents(mod.events) end
		if type(mod.counters) == "table" then COUNTERS[#COUNTERS + 1] = mod.counters end
	end
end

local function scans()
	local n, t = 0, 0
	for _, c in ipairs(COUNTERS) do n, t = n + (tonumber(c.scans) or 0), t + (tonumber(c.scanMs) or 0) end
	return n, t
end

local DEV = false
pcall(function() DEV = C.env("FEVERLINK3_DEV") == "1" end)

local U = {
	tags = setmetatable({}, { __mode = "k" }),   -- command object -> { name, args }
	seq = 0,                                      -- id of the next command record
	orig = nil, outbox = nil,
	lastPoll = -10, bindsText = nil, rev = {},
	pending = {},   -- entity -> { key = <its key>, at = clock }: created by a command of this window, not yet in binds.txt
	held = {},      -- record id -> { cb, progress, name, spec, at }: a command waiting for its stamp apply (the bridge's answer)
	speedWait = {}, -- { cb, n, at, from }: the callbacks of the speed buttons' requests, run once the session answered (speedAnswers)
	sess = nil, sessText = nil, sessAt = nil,   -- the session as hud_state.txt last said (M.sessionSpeed)
	inbox = nil,    -- reader of ui_in.log (the bridge's answers)
	stats = { shipped = 0, speed = 0, untouched = 0, failed = 0, answered = 0 },
	-- The group of the commands a window issues in one go (M.poll starts the next one) and, for the timing lines, when the window's refresh
	-- last ran (tickAt) and when this hook last handed control back to the window inside the current group (exitAt, exitGrp).
	grp = 1, tickAt = nil, exitAt = nil, exitGrp = nil,
}

-- How long a held command waits for the bridge's answer before its callback is fired with a failure (the bridge is gone, the
-- session ended): far above any stamp lead, so it only fires when nothing will answer.
local HELD_SECONDS = 120

-- A speed request repeated within this many seconds of the last one is not sent again (handle).
local SPEED_REPEAT_S = 1
-- A speed request's callback runs when the session shows the asked speed, when the session changed otherwise, or after this many
-- seconds (the host ignored it: no permission, a resync; the speed control then shows the session's speed again).
M.SPEED_ANSWER_S = 3
-- hud_state.txt (the session as the bridge sees it) is read at most this often (M.sessionSpeed)
M.SESSION_READ_S = 0.25

-- The one writer of ui_out.log in this Lua state (the window's chat uses it too). The bridge reads the file by position, so a new writer
-- after a reload simply goes on at its end.
function M.outbox()
	if not U.outbox then U.outbox = Hud.newOutbox(C) end
	return U.outbox
end

local function send(kind, payload)
	local ok, err = C.writerSend(M.outbox(), kind, payload)
	if not ok then log("cannot write ui_out.log: " .. tostring(err)) end
	return ok
end
M.send = send

local function trace(text)
	if DEV then C.dump("dev_trace.txt", string.format("%.2f ui ", C.clock()) .. text .. "\n") end
end

local function short(v)
	if type(v) == "table" then
		local ok, s = pcall(C.serialize, v)
		return ok and s:sub(1, 200) or "table"
	end
	return tostring(v):sub(1, 100)
end

-- The session these commands were held in is over (identity.txt says idle, or names another session). The bridge ends it too
-- (X.sessionOff) and answers each command: failed when it was not applied, or the engine's own answer when the engine had it already
-- (applied during a pause). A command whose answer never comes (its record never reached the bridge) fails OVER_SECONDS after the end
-- instead of the 120 s limit. Nothing is applied here: the command was the window's to retry.
local OVER_SECONDS = 5
function M.sessionOver(why)
	local n, now = 0, C.clock()
	for _, h in pairs(U.held) do
		if not h.over then
			h.over = now
			n = n + 1
		end
	end
	if n > 0 then log("session over (" .. tostring(why) .. "): " .. n .. " held command(s) wait " .. OVER_SECONDS .. " s for the bridge's last answer, then fail") end
end

-- is a session on? (the identity file the controller writes; read at most once a second). A session that ended, or was replaced by
-- another one, since this state last looked fails what it still holds (M.sessionOver).
local function active()
	local now = C.clock()
	if now - U.lastPoll > 1 or now < U.lastPoll then
		U.lastPoll = now
		pcall(C.poll)
	end
	if U.gen ~= nil and (not C.ACTIVE or U.gen ~= C.GEN) then
		pcall(M.sessionOver, C.ACTIVE and "another session began" or "the session ended")
	end
	U.gen = C.ACTIVE and C.GEN or nil
	return C.ACTIVE == true
end

-- entity -> key for what a replicated action created (the bridge's bindings), refreshed when the file changed
local function reverse()
	local text = C.readFile(C.path("binds.txt")) or ""
	if text ~= U.bindsText then
		U.bindsText = text
		U.rev = {}
		local t = C.parse(text)
		if type(t) == "table" then
			for key, e in pairs(t) do
				if type(e) == "number" then U.rev[e] = key end
			end
		end
	end
	return U.rev
end

-- How long a window-made entity keeps its placeholder key after the bridge's table should have it (the file is rewritten
-- about every half second, and read here at the next command).
local PENDING_SECONDS = 120

-- The key of an entity: the bridge's binding, else the key the bridge's answer gave the window command that created it a
-- moment ago (a placeholder "ui:<id>" is turned into the real key before a command leaves; L.rewriteUiKeys), else "s:<id>"
-- (the savegame).
local function keyOf(rev, e)
	local k = rev[e]
	if k then
		U.pending[e] = nil
		return k
	end
	local p = U.pending[e]
	if p then
		if C.clock() - p.at <= PENDING_SECONDS then return p.key end
		U.pending[e] = nil
	end
	return "s:" .. tostring(e)
end
M.keyOf = function(e) return keyOf(reverse(), e) end

local function originals()
	local t = package.loaded["feverlink3_ui_orig_cmds"]
	if type(t) ~= "table" then
		t = {}
		package.loaded["feverlink3_ui_orig_cmds"] = t
	end
	return t
end

local function memberNames(o)
	local ok, m = pcall(function() return o.__members end)
	if ok and type(m) == "table" then return m end
	local mt = getmetatable(o)
	if type(mt) == "table" then
		ok, m = pcall(function() return mt.__members end)
		if ok and type(m) == "table" then return m end
	end
	return nil
end

-- The bridge's answers (ui_in.log): read from where the file ends when this state first looks, so the answers of an earlier
-- run of this window state (whose record ids this run reuses) are never taken for this run's.
local function inbox()
	if U.inbox then return U.inbox end
	local path = C.path("ui_in.log")
	local ok, r = pcall(C.newReader, path, { start = "end" })
	if not ok then r = C.newReader(path) end
	U.inbox = r
	return r
end

-- Where the held callbacks run (U.fireCtx): the window's refresh (a react step timer) or a window's own sendCommand (a handler). A callback
-- the engine itself would have run after the command applied may call an API that the game refuses at the place it runs now: friend test
-- 2026-10-16, 31 x "API is currently restricted" from makeVehicleSetLineCmd's callback (manager_window.tl:4724 playRandomSoundEffect),
-- every one fired from the step timer; the rest of such a callback does not run. The line says where it ran, the full text the first
-- times only.
local function fire(id, h, res, success, entities)
	if not h.cb then return end
	local ok, err = pcall(h.cb, res, success, entities)
	if ok then return end
	local text = tostring(err)
	if text:find("currently restricted", 1, true) then
		U.restricted = (U.restricted or 0) + 1
		if U.restricted > 3 then text = text:match("^[^\n]*") end
		log(string.format("the callback of command %s #%s hit an API the game restricts where it ran (fired from %s; %d so far): %s",
			h.name, tostring(id), tostring(U.fireCtx or "?"), U.restricted, text))
		return
	end
	log("the callback of command " .. h.name .. " #" .. id .. " failed (fired from " .. tostring(U.fireCtx or "?") .. "): " .. text)
end

local function ms(a, b)
	if type(a) ~= "number" or type(b) ~= "number" then return "-" end
	return string.format("%.0f", (b - a) * 1000)
end

-- One line per window command once its answer is in: every step from the click to the callback, in ms after the click (the
-- sendCommand call), on this game's one clock (C.clock: the window state and the bridge's interface half run in one process).
-- since_tick: from the window's last refresh to the click (a stall in the window's own code before its first command shows here);
-- gap: from this hook's previous command of the group to this one (the window's and the engine's code between two commands);
-- maker/poll/encode/write: inside the hook (encode broken down: binds.txt read, each argument's codec with its dry run, each engine
-- object's read/wire/dry phases, and the world searches the codecs made); read/shipped: the bridge took the record and stamped it; applied/answered: the stamp
-- apply's answer written by the bridge and read here. Friend test 2026-10-16: a clone of two vehicles stood the game still for 16 s
-- with no line telling where.
local function timingLine(id, h, p)
	local t, b = h.tm or {}, type(p.tm) == "table" and p.tm or {}
	local c = t.c
	local detail = t.detail and (" {" .. t.detail .. "}") or ""
	local order = (type(b.i) == "number" and type(b.n) == "number") and string.format(" (%d of %d)", b.i, b.n) or ""
	return string.format("timing window #%d %s grp=%s%s stamp=%s now=%s: since_tick=%s gap=%s maker=%.1f poll=%.1f encode=%.1f%s write=%.1f"
		.. " | read=+%s shipped=+%s applied=+%s answered=+%s ms after the click", id, h.name, tostring(t.grp), order, tostring(b.at), tostring(b.now),
		ms(t.tick, c), t.exit and ms(t.exit, c) or "-", (t.mk or 0) * 1000, ((t.p or c or 0) - (c or 0)) * 1000, ((t.e or 0) - (t.p or 0)) * 1000, detail,
		((t.s or 0) - (t.e or 0)) * 1000, ms(c, b.r), ms(c, b.s), ms(c, b.a), ms(c, C.clock()))
end

-- The stamp apply of held commands, as the bridge reports it: the callback the window passed runs now, with what the command
-- created (the field the window reads, spec.result, and the entity list), or with a failure when every game refused it at the
-- stamp. Run by the window's refresh (M.poll) and before every command this hook takes. Never waits: what is not answered yet
-- stays held, and the window goes on.
local function answers()
	if next(U.held) == nil then return end
	active()   -- a session that ended fails what is held here (M.sessionOver)
	local okP, recs = pcall(C.readerPoll, inbox())
	for _, rec in ipairs(okP and recs or {}) do
		local p = rec.kind == "result" and C.parse(rec.payload) or nil
		local id = type(p) == "table" and tonumber(p.id) or nil
		local h = id and U.held[id]
		if h then
			U.held[id] = nil
			U.stats.answered = U.stats.answered + 1
			local success = p.success == true
			local e = type(p.e) == "number" and p.e or nil
			local res = {}
			if success and e and h.spec.creates then
				-- usable at once by the next command of the window, before the bridge's table (binds.txt) has it
				U.pending[e] = { key = type(p.key) == "string" and p.key or ("ui:" .. tostring(id)), at = C.clock() }
				if h.spec.result then res[h.spec.result] = e end
			end
			-- {{Engine.Entity, Engine.Revision}} as sendCommand's callback gets it (tf3-docs api/cmd): the answer names entities only, so
			-- each gets this game's revision of it (C.resultEntities; until 2026-10-18 the revision was a bare 0)
			local entities = type(p.entities) == "table" and p.entities or (e and { { e } } or {})
			local okR, list = pcall(C.resultEntities, entities)
			if okR then entities = list end
			-- a build's callback gets WorldBuildProposalCommandData (tf3-docs api/cmd; the bridge's X.buildCmdData, the same fields)
			if h.name == "makeWorldBuildProposalCmd" and type(h.args) == "table" then
				res = { proposal = h.args[1], context = h.args[2], ignoreErrors = h.args[3] == true, doDust = h.args[5], resultEntities = entities }
			end
			log("command " .. h.name .. " #" .. id .. " " .. (success and "applied" or "REFUSED") .. " at its stamp" .. (p.key and (" (" .. tostring(p.key) .. ")") or "")
				.. (e and (", created " .. tostring(e)) or "") .. (p.detail and (": " .. tostring(p.detail)) or "") .. "; its callback runs now")
			local okT, line = pcall(timingLine, id, h, p)
			if okT then log(line) end
			fire(id, h, res, success, entities)
		end
	end
	local now = C.clock()
	for id, h in pairs(U.held) do
		if now - h.at > HELD_SECONDS then
			U.held[id] = nil
			log("command " .. h.name .. " #" .. id .. " had no answer from the bridge in " .. HELD_SECONDS .. " s: its callback is fired with a failure")
			fire(id, h, {}, false, {})
		elseif h.over and (now - h.over > OVER_SECONDS or now < h.over) then
			U.held[id] = nil
			log("command " .. h.name .. " #" .. id .. ": the session ended and the bridge did not answer within " .. OVER_SECONDS .. " s: its callback is fired with a failure")
			fire(id, h, {}, false, {})
		end
	end
end

-- The speed the session shows, as the game's speed control knows speeds (0 = pause, else the speedup), from hud_state.txt, the
-- session the bridge applies (its `session` lines): 0 while it waits for the host's start or is paused (speed 0, or a pause time set:
-- the HUD's "Paused" says the same, Hud.statusText). nil: no session, or the bridge has written nothing yet.
function M.sessionSpeed()
	if not active() then return nil end
	local now = C.clock()
	if U.sessAt == nil or now - U.sessAt >= M.SESSION_READ_S or now < U.sessAt then
		U.sessAt = now
		-- by its text, not its version: a new session's model counts its versions from the start again
		local text = C.readFile(C.path(Hud.SNAPSHOT))
		if text ~= U.sessText then
			U.sessText = text
			local okR, m = pcall(function() return Hud.fromSnapshot(C.parse(text or "")) end)
			U.sess = okR and m and m.session or nil
			U.nativeHolds = okR and m and m.nativeHolds == true
		end
	end
	local s = U.sess
	if type(s) ~= "table" then return nil end
	if not s.started or (tonumber(s.speed) or 0) <= 0 or s.pauseAt ~= nil then return 0 end
	return tonumber(s.speed)
end

-- The callbacks of the speed buttons' requests (handle): each runs once the session shows the asked speed (success), once the session
-- changed otherwise since the request (the host decided something else), after M.SPEED_ANSWER_S, or when the session is over. Only then
-- does the game's speed control look at the speed again, so its icon does not jump back to the old speed while the request travels.
local function speedAnswers()
	if #U.speedWait == 0 then return end
	local shown, now = M.sessionSpeed(), C.clock()
	local keep = {}
	for _, w in ipairs(U.speedWait) do
		local done = shown == nil or shown == w.n or shown ~= w.from or now - w.at >= M.SPEED_ANSWER_S or now < w.at
		if done then
			-- GameSetSpeedCommandData { speedup } (tf3-docs api/cmd), as the engine's callback gets it
			fire(0, { cb = w.cb, name = "makeGameSetSpeedCmd" }, { speedup = w.n }, shown ~= nil and shown == w.n, {})
		else
			keep[#keep + 1] = w
		end
	end
	U.speedWait = keep
end
M.speedAnswers = speedAnswers

-- A command this game cannot replicate is refused here too, never applied on this game alone: the engine applying it on the
-- originator only made the worlds differ and the next resync erased the player's edit (friend test 2026-10-16: line edits that
-- "do not rebuild equal" were applied on the editing game only, 9 of the 14 resyncs of that test followed from it). The window's
-- callback fails, the log says why, and the player reads it in the window's notices (a `notice` record: the bridge adds it to this
-- game's notices only).
local function refuse(tag, cb, why)
	U.stats.failed = U.stats.failed + 1
	log("command " .. tag.name .. " is REFUSED: it cannot be replicated, so it is applied nowhere (not on this game alone either): " .. tostring(why))
	-- the bridge words it (feverlink3_text.lua, action_not_shared); the reason stays in the log line above
	send("notice", { msg = "action_not_shared", cmd = tag.name, sid = C.SESSION })
	fire(0, { cb = cb, name = tag.name }, {}, false, {})
	return true
end
M.refuse = refuse

-- The window's refresh (feverlink3_ui.script.lua, its step timer): it runs between the window's handlers, never inside one, so it
-- closes the group of commands the window issued since the last refresh. What one handler issues in one go (a clone of N vehicles:
-- N purchases in one loop of vehicle_react_util.HandleVehicleChanges) and what the callbacks of one refresh issue (their N line
-- assignments) carry one group number; the bridge ships each group as one order with one stamp (shipUiCommand).
function M.poll()
	U.grp = U.grp + 1
	U.tickAt = C.clock()
	U.fireCtx = "the window's refresh"
	pcall(speedAnswers)
	return answers()
end

-- the command a window sends: encoded, recorded for the bridge and HELD until the bridge reports its stamp apply (M.poll).
-- `t0`: when the window called sendCommand (the timing line's click).
local function handle(tag, cmd, cb, progress, t0)
	local tm = { c = t0, p = C.clock(), grp = U.grp, tick = U.tickAt, mk = tag.mk }
	if U.exitGrp == U.grp then tm.exit = U.exitAt end
	if tag.name == "makeGameSetSpeedCmd" then
		U.stats.speed = U.stats.speed + 1
		-- The same speed asked again within SPEED_REPEAT_S of the last request is not sent again: the game's own speed control asks
		-- every frame while the engine's speed is not the one it shows (friend test 2026-10-16, resync 4: 2760 "speed 4" records from this
		-- hook, 1210 "speed request ... ignored (resync in progress)" lines on the host while the worlds loaded). The host answers with the
		-- session speed, so one request a second loses nothing.
		local n, now = tag.args[1], C.clock()
		-- The window's callback is kept and run once the session answered (speedAnswers), the asked one too. It was dropped until
		-- 2026-10-18: the game's speed control (gui/main/game.tl, GameSpeedHelper) counts its commands in flight and reads the
		-- engine's speed only while none is (cmdCounter), so after the first click it never looked again and its icon kept the
		-- player's own last speed (friend test 2026-10-18 22:47: the other player paused, this game paused, its icon still ran).
		if cb then U.speedWait[#U.speedWait + 1] = { cb = cb, n = n, at = now, from = M.sessionSpeed() } end
		local last = U.lastSpeed
		if last and last.n == n and now >= last.at and now - last.at < SPEED_REPEAT_S then
			last.quiet = last.quiet + 1
			return true
		end
		if last and last.quiet > 0 then
			log(string.format("speed %s was asked %d more time(s) within %.0f s of a request: not sent again", tostring(last.n), last.quiet, SPEED_REPEAT_S))
		end
		U.lastSpeed = { n = n, at = now, quiet = 0 }
		send("speed", { n = n, sid = C.SESSION })
		return true
	end
	-- A build a script of this state sends (real session 2026-10-26: the bus and truck bay mod's upgrade tool, a react recipe calling
	-- api.cmd.sendCommand(api.cmd.makeWorldBuildProposalCmd(proposal, ctx, false, true), cb), was applied here at the click and on the
	-- others at the stamp: a free-id fork and a resync, twice). Two layers, the official API first (docs/ARCHITECTURE.md): a player's
	-- build is recorded with the build codec and held here like every window command; every game, this one included, rebuilds it with
	-- makeWorldBuildProposalCmd at the stamp and its callback gets that apply's result (answers). One the codec cannot record goes on to
	-- the engine's command list only while the native module holds builds there (the safety net: it holds the command and the bridge
	-- ships it from the command's own proposal and text); without it, it is refused. A build the script marks as not the player's
	-- (playerInitiated false) is the script's own business, as the engine treats it: untouched.
	if tag.name == "makeWorldBuildProposalCmd" and tag.args[4] ~= true then return false end
	local spec = L.COMMANDS[tag.name]
	if not spec or spec.virtual then return false end
	-- another mod's own event (its window sends its settings to its game script): shipped like a window command, delivered at the stamp
	-- on every game (L.markForeignEvent); one that is not plain data stays on this game as before
	if tag.mod and tag.args.foreign == nil then
		local okM, why = L.markForeignEvent(tag.args, tag.mod, C.plain)
		local seen = tostring(tag.mod) .. "|" .. tostring(tag.args[2]) .. "|" .. tostring(tag.args[3])
		U.foreignSeen = U.foreignSeen or {}
		if not U.foreignSeen[seen] then
			U.foreignSeen[seen] = true
			if okM then
				log("another mod's event " .. tostring(tag.args[2]) .. "/" .. tostring(tag.args[3]) .. " (mod=" .. tostring(tag.mod) .. ") is shared: every game delivers it at the stamp")
			elseif why ~= "registered" then
				log("another mod's event " .. tostring(tag.args[2]) .. "/" .. tostring(tag.args[3]) .. " (mod=" .. tostring(tag.mod) .. ") stays on this game unshared: " .. tostring(why))
			end
		end
	end
	local category = L.categoryOf(tag.name, tag.args)
	-- what no game replicates (an event nobody registered: notifications, tutorial steps) is this game's own business
	if category == "local" then return false end
	local rev = reverse()
	tm.r = C.clock()
	-- argMs / objMs: the schema's own timing of each argument and of each engine object's dry run (feverlink3_cmds_ops.lua)
	local ctx = { plain = C.plain, keyOf = function(e) return keyOf(rev, e) end, argMs = {}, objMs = {} }
	local n0, ms0 = scans()
	local ok, body, why = pcall(L.encodeCommand, tag.name, tag.args, ctx)
	if not ok then body, why = nil, tostring(body) end
	local n1, ms1 = scans()
	local parts = {}
	for i = 1, (tag.args and tag.args.n) or 0 do parts[i] = ctx.argMs[i] and string.format("%.1f", ctx.argMs[i]) or "-" end
	tm.detail = string.format("binds=%.1f args=[%s] objects=[%s] world_searches=%d (%.0f ms)", (tm.r - tm.p) * 1000, table.concat(parts, " "),
		table.concat(ctx.objMs, "; "), n1 - n0, ms1 - ms0)
	if not body and tag.name == "makeWorldBuildProposalCmd" then
		M.sessionSpeed()   -- the snapshot, read at most every SESSION_READ_S
		if U.nativeHolds then
			log("command " .. tag.name .. " cannot be recorded through the API (" .. tostring(why) .. "): handed to the native safety net at the command list")
			return false
		end
	end
	if not body then return refuse(tag, cb, why) end
	local okS, text = pcall(C.serialize, body)
	if not okS then return refuse(tag, cb, text) end
	U.seq = U.seq + 1
	local id = U.seq
	tm.e = C.clock()
	-- Nothing goes to the engine here: the refusal the engine would give (not enough money, a depot that is gone) is the stamp
	-- apply's, decided alike on every game from the same world, and reaches the callback through the bridge's answer. `progress`
	-- is kept with the command but never called: the engine never had this command, so it has no progress of its own to report.
	inbox()   -- opened before the record leaves, so its answer lies after the place this state starts reading from
	U.held[id] = { cb = cb, progress = progress, name = tag.name, spec = spec, at = C.clock(), tm = tm, args = tag.args }
	-- `sid`: the bridge acts only on records of its own session (the file outlives sessions). `grp`: the records of one group are
	-- one order (M.poll); `t`: the click and the end of the encode, for the bridge's part of the timing line.
	local sent = send("cmd", { id = id, body = text, category = category, sid = C.SESSION, grp = tm.grp, t = { c = tm.c, e = tm.e } })
	tm.s = C.clock()
	U.exitAt, U.exitGrp = tm.s, tm.grp
	if not sent then
		-- fail closed: applying it here alone would fork the worlds
		U.held[id] = nil
		U.stats.failed = U.stats.failed + 1
		log("command " .. tag.name .. " #" .. id .. " is refused: its record could not be written for the bridge")
		fire(id, { cb = cb, name = tag.name }, {}, false, {})
		return true
	end
	U.stats.shipped = U.stats.shipped + 1
	log("command " .. tag.name .. " #" .. id .. " recorded (" .. #text .. " bytes, " .. category .. "): held until its stamp"
		.. (tag.name == "makeWorldBuildProposalCmd" and " (a build: held by the API layer)" or "") .. "; group " .. tm.grp
		.. string.format(", %.1f ms in the hook (encode %.1f: %s)", (tm.s - (tm.c or tm.p)) * 1000, (tm.e - tm.p) * 1000, tm.detail or "") .. (tm.exit and (", " .. ms(tm.exit, tm.c) .. " ms in the window since its previous command") or ""))
	U.exitAt = C.clock()
	return true
end

-- UNDOCUMENTED API (docs/ARCHITECTURE.md section 2, "Undocumented API uses and why"): the window state's command factories and sendCommand are
-- replaced: a window command must be held and stamped before any game applies it, and the API has no command hook.
function M.install()
	if M.installed then return true end
	if not (C.DIR and C.IO and api and api.cmd) then return false end
	M.installed = true
	local orig = originals()
	if not orig.sendCommand then
		orig.sendCommand = api.cmd.sendCommand
		orig.factory = {}
		local wanted = {}
		for name, spec in pairs(L.COMMANDS) do
			if not spec.virtual then wanted[name] = true end
		end
		wanted.makeGameSetSpeedCmd = true
		if DEV then
			for _, name in ipairs(memberNames(api.cmd) or {}) do wanted[name] = true end
		end
		wanted.sendCommand = nil
		for name in pairs(wanted) do
			-- in the window's Lua state the engine's command makers are callable tables, not plain functions (seen in a real
			-- game): anything that is there is kept and called the way the windows call it
			local okf, f = pcall(function() return api.cmd[name] end)
			if okf and f ~= nil and type(f) ~= "boolean" and type(f) ~= "number" and type(f) ~= "string" then orig.factory[name] = f end
		end
	end
	U.orig = orig
	local wrapped = 0
	for name, fn in pairs(orig.factory) do
		local okw = pcall(function()
			api.cmd[name] = function(...)
				local t = C.clock()
				local cmd = fn(...)
				-- mk: the seconds the engine's own command maker took (the timing line)
				if cmd ~= nil then U.tags[cmd] = { name = name, args = { n = select("#", ...), ... }, mk = C.clock() - t } end
				return cmd
			end
		end)
		if okw then wrapped = wrapped + 1 end
	end
	local oks = pcall(function()
		local wrapper
		wrapper = function(cmd, cb, progress)
			local t0 = C.clock()
			local tag = cmd ~= nil and U.tags[cmd] or nil
			if tag ~= nil and tag.name == "makeScriptingSendEventCmd" and tag.mod == nil then
				-- who sends it: another mod's script (handle, L.markForeignEvent), or FeverLink3's / the game's own (nil)
				local okW, _, mod = pcall(C.callerOf, wrapper)
				if okW then tag.mod = mod or false end
			end
			if tag ~= nil then
				if DEV then
					local parts = {}
					for i = 1, tag.args.n do parts[#parts + 1] = short(tag.args[i]) end
					trace("sendCommand " .. tag.name .. "(" .. table.concat(parts, " | ") .. ")" .. (active() and "" or " [no session]"))
				end
				if active() then
					U.fireCtx = "a window's sendCommand (" .. tag.name .. ")"
					pcall(answers)   -- not M.poll: a command issued inside a window's handler stays in that handler's group
					local okH2, handled = pcall(handle, tag, cmd, cb, progress, t0)
					if okH2 and handled then return end
					-- a fault in the hook is refused like a command that cannot be replicated: passing it on would apply it here alone
					if not okH2 then
						pcall(refuse, tag, cb, "the hook failed: " .. tostring(handled))
						return
					end
				end
			elseif DEV then
				trace("sendCommand (untagged) " .. tostring(cmd))
			end
			U.stats.untouched = U.stats.untouched + 1
			return orig.sendCommand(cmd, cb, progress)
		end
		api.cmd.sendCommand = wrapper
	end)
	log("UI hook installed: " .. wrapped .. " command functions wrapped" .. (oks and "" or "; sendCommand is read-only"))
	return oks
end

-- DEV only: runs dev_ui.lua from the session folder in this Lua state, like the probe in the bridge's two halves
function M.devPoll()
	if not DEV then return end
	local path = C.path("dev_ui.lua")
	local text = C.readFile(path)
	if type(text) ~= "string" or text == "" then return end
	C.writeFile(path, "")
	local lines = {}
	local env = setmetatable({
		api = api, C = C, L = L, Hook = M, U = U,
		print = function(...)
			local t = {}
			for i = 1, select("#", ...) do t[#t + 1] = tostring((select(i, ...))) end
			lines[#lines + 1] = table.concat(t, "\t")
		end,
	}, { __index = _G })
	local f, err = load(text, "dev_ui", "t", env)
	local out
	if not f then
		out = "COMPILE ERROR: " .. tostring(err)
	else
		local ok, res = pcall(f)
		local head = #lines > 0 and (table.concat(lines, "\n") .. "\n") or ""
		if ok then
			local okS, ser = pcall(C.serialize, res)
			out = head .. "RESULT: " .. (okS and ser or tostring(res))
		else
			out = head .. "ERROR: " .. tostring(res)
		end
	end
	C.dump("dev_ui_out.txt", "---- t=" .. tostring(C.clock()) .. "\n" .. out .. "\n")
end

-- DEV only: the same as devPoll, but for the file dev_click.lua and called from a button's click handler. The engine lets
-- a command be sent only from where a player's click leads (not from a timer), so a command made with the game's own api
-- through here goes the way a window's command goes: the hook sees it and holds it, every game applies it at its stamp.
function M.devClick()
	if not DEV then return end
	local path = C.path("dev_click.lua")
	local text = C.readFile(path)
	if type(text) ~= "string" or text == "" then
		C.dump("dev_click_out.txt", "---- t=" .. tostring(C.clock()) .. "\nNOTHING TO RUN\n")
		return
	end
	C.writeFile(path, "")
	local lines = {}
	local env = setmetatable({
		api = api, C = C, L = L, Hook = M, U = U,
		print = function(...)
			local t = {}
			for i = 1, select("#", ...) do t[#t + 1] = tostring((select(i, ...))) end
			lines[#lines + 1] = table.concat(t, "\t")
		end,
	}, { __index = _G })
	local f, err = load(text, "dev_click", "t", env)
	local out
	if not f then
		out = "COMPILE ERROR: " .. tostring(err)
	else
		local ok, res = pcall(f)
		local head = #lines > 0 and (table.concat(lines, "\n") .. "\n") or ""
		if ok then
			local okS, ser = pcall(C.serialize, res)
			out = head .. "RESULT: " .. (okS and ser or tostring(res))
		else
			out = head .. "ERROR: " .. tostring(res)
		end
	end
	C.dump("dev_click_out.txt", "---- t=" .. tostring(C.clock()) .. "\n" .. out .. "\n")
end

M.dev = DEV
M.stats = U.stats
return M
