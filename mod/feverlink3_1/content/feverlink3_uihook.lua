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
--     and the callback the window passed runs then, with the entity the command created (M.poll).
--   * anything else, and everything while no session is on: untouched.
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
if not okC then C = ug_require("feverlink3_1::/feverlink3_common.lua") end
local okL, L = pcall(ug_require, "feverlink3_lockstep.lua")
if not okL then L = ug_require("feverlink3_1::/feverlink3_lockstep.lua") end
local okH, Hud = pcall(ug_require, "feverlink3_hud.lua")
if not okH then Hud = ug_require("feverlink3_1::/feverlink3_hud.lua") end

local log = C.logger("uihook")
local M = { installed = false }

-- the schemas of the other areas, loaded as the bridge loads them (each optional)
for _, name in ipairs({ "feverlink3_cmds_build", "feverlink3_cmds_ops", "feverlink3_cmds_company" }) do
	local okM, mod = pcall(ug_require, name .. ".lua")
	if not okM then okM, mod = pcall(ug_require, "feverlink3_1::/" .. name .. ".lua") end
	if okM and type(mod) == "table" then
		L.registerCommands(mod.commands)
		if mod.events then L.registerEvents(mod.events) end
	end
end

local DEV = false
pcall(function() DEV = os.getenv("FEVERLINK3_DEV") == "1" end)

local U = {
	tags = setmetatable({}, { __mode = "k" }),   -- command object -> { name, args }
	seq = 0,                                      -- id of the next command record
	orig = nil, outbox = nil,
	lastPoll = -10, bindsText = nil, rev = {},
	pending = {},   -- entity -> { key = <its key>, at = clock }: created by a command of this window, not yet in binds.txt
	held = {},      -- record id -> { cb, progress, name, spec, at }: a command waiting for its stamp apply (the bridge's answer)
	inbox = nil,    -- reader of ui_in.log (the bridge's answers)
	stats = { shipped = 0, speed = 0, untouched = 0, failed = 0, answered = 0 },
}

-- How long a held command waits for the bridge's answer before its callback is fired with a failure (the bridge is gone, the
-- session ended): far above any stamp lead, so it only fires when nothing will answer.
local HELD_SECONDS = 120

-- The one writer of ui_out.log (the window's chat uses it too: the file has a single writer so its sequence numbers
-- keep increasing).
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
	if DEV then C.appendFile(C.path("dev_trace.txt"), string.format("%.2f ui ", os.clock()) .. text .. "\n") end
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
	local n, now = 0, os.clock()
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
	local now = os.clock()
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
		if os.clock() - p.at <= PENDING_SECONDS then return p.key end
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

local function fire(id, h, res, success, entities)
	if not h.cb then return end
	local ok, err = pcall(h.cb, res, success, entities)
	if not ok then log("the callback of command " .. h.name .. " #" .. id .. " failed: " .. tostring(err)) end
end

-- The stamp apply of held commands, as the bridge reports it: the callback the window passed runs now, with what the command
-- created (the field the window reads, spec.result, and the entity list), or with a failure when every game refused it at the
-- stamp. Called by the window's refresh (feverlink3_ui.script.lua) and before every command this hook takes.
function M.poll()
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
				U.pending[e] = { key = type(p.key) == "string" and p.key or ("ui:" .. tostring(id)), at = os.clock() }
				if h.spec.result then res[h.spec.result] = e end
			end
			local entities = type(p.entities) == "table" and p.entities or (e and { { e, 0 } } or {})
			log("command " .. h.name .. " #" .. id .. " " .. (success and "applied" or "REFUSED") .. " at its stamp" .. (p.key and (" (" .. tostring(p.key) .. ")") or "")
				.. (e and (", created " .. tostring(e)) or "") .. (p.detail and (": " .. tostring(p.detail)) or "") .. "; its callback runs now")
			fire(id, h, res, success, entities)
		end
	end
	local now = os.clock()
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

-- the command a window sends: encoded, recorded for the bridge and HELD until the bridge reports its stamp apply (M.poll)
local function handle(tag, cmd, cb, progress)
	if tag.name == "makeGameSetSpeedCmd" then
		U.stats.speed = U.stats.speed + 1
		send("speed", { n = tag.args[1], sid = C.SESSION })
		return true
	end
	-- builds are captured where the engine applies them (the bridge's onPreBuildProposal), whoever issued them
	if tag.name == "makeWorldBuildProposalCmd" then return false end
	local spec = L.COMMANDS[tag.name]
	if not spec or spec.virtual then return false end
	local category = L.categoryOf(tag.name, tag.args)
	if category == "local" then return false end
	local rev = reverse()
	local ctx = { plain = C.plain, keyOf = function(e) return keyOf(rev, e) end }
	local ok, body, why = pcall(L.encodeCommand, tag.name, tag.args, ctx)
	if not ok then body, why = nil, tostring(body) end
	if not body then
		U.stats.failed = U.stats.failed + 1
		log("command " .. tag.name .. " is not replicated: " .. tostring(why))
		return false
	end
	local okS, text = pcall(C.serialize, body)
	if not okS then
		U.stats.failed = U.stats.failed + 1
		log("command " .. tag.name .. " is not replicated: " .. tostring(text))
		return false
	end
	U.seq = U.seq + 1
	local id = U.seq
	-- Nothing goes to the engine here: the refusal the engine would give (not enough money, a depot that is gone) is the stamp
	-- apply's, decided alike on every game from the same world, and reaches the callback through the bridge's answer. `progress`
	-- is kept with the command but never called: the engine never had this command, so it has no progress of its own to report.
	inbox()   -- opened before the record leaves, so its answer lies after the place this state starts reading from
	U.held[id] = { cb = cb, progress = progress, name = tag.name, spec = spec, at = os.clock() }
	-- `sid`: the bridge acts only on records of its own session (the file outlives sessions)
	if not send("cmd", { id = id, body = text, category = category, sid = C.SESSION }) then
		-- fail closed: applying it here alone would fork the worlds
		U.held[id] = nil
		U.stats.failed = U.stats.failed + 1
		log("command " .. tag.name .. " #" .. id .. " is refused: its record could not be written for the bridge")
		fire(id, { cb = cb, name = tag.name }, {}, false, {})
		return true
	end
	U.stats.shipped = U.stats.shipped + 1
	log("command " .. tag.name .. " #" .. id .. " recorded (" .. #text .. " bytes, " .. category .. "): held until its stamp")
	return true
end

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
				local cmd = fn(...)
				if cmd ~= nil then U.tags[cmd] = { name = name, args = { n = select("#", ...), ... } } end
				return cmd
			end
		end)
		if okw then wrapped = wrapped + 1 end
	end
	local oks = pcall(function()
		api.cmd.sendCommand = function(cmd, cb, progress)
			local tag = cmd ~= nil and U.tags[cmd] or nil
			if tag ~= nil then
				if DEV then
					local parts = {}
					for i = 1, tag.args.n do parts[#parts + 1] = short(tag.args[i]) end
					trace("sendCommand " .. tag.name .. "(" .. table.concat(parts, " | ") .. ")" .. (active() and "" or " [no session]"))
				end
				if active() then
					pcall(M.poll)
					local okH2, handled = pcall(handle, tag, cmd, cb, progress)
					if okH2 and handled then return end
					if not okH2 then log("command " .. tag.name .. " failed in the hook: " .. tostring(handled)) end
				end
			elseif DEV then
				trace("sendCommand (untagged) " .. tostring(cmd))
			end
			U.stats.untouched = U.stats.untouched + 1
			return orig.sendCommand(cmd, cb, progress)
		end
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
	C.appendFile(C.path("dev_ui_out.txt"), "---- t=" .. tostring(os.clock()) .. "\n" .. out .. "\n")
end

-- DEV only: the same as devPoll, but for the file dev_click.lua and called from a button's click handler. The engine lets
-- a command be sent only from where a player's click leads (not from a timer), so a command made with the game's own api
-- through here goes the way a window's command goes: the hook sees it and holds it, every game applies it at its stamp.
function M.devClick()
	if not DEV then return end
	local path = C.path("dev_click.lua")
	local text = C.readFile(path)
	if type(text) ~= "string" or text == "" then
		C.appendFile(C.path("dev_click_out.txt"), "---- t=" .. tostring(os.clock()) .. "\nNOTHING TO RUN\n")
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
	C.appendFile(C.path("dev_click_out.txt"), "---- t=" .. tostring(os.clock()) .. "\n" .. out .. "\n")
end

M.dev = DEV
M.stats = U.stats
return M
