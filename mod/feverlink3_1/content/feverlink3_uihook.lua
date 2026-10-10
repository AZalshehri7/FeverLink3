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
--     control, as in Transport Fever 2 Multiplayer (docs/research/TPF2_COMPARISON.md).
--   * a command with a schema (feverlink3_cmds_*.lua, the same registry the bridge uses): encoded, handed to the bridge
--     as a `cmd` record (ui_out.log), and applied natively here and now, exactly as without the mod ("replay on peers":
--     the other games apply it at the stamp the bridge gives it). Its callback runs as usual; a created entity (a line)
--     is reported back as a `bind` record so later commands can name it.
--   * anything else, and everything while no session is on: untouched.
-- Entity ids never travel; the keys of entities created in the session come from binds.txt, which the bridge writes.
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
	stats = { shipped = 0, speed = 0, untouched = 0, failed = 0 },
}

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

-- is a session on? (the identity file the controller writes; read at most once a second)
local function active()
	local now = os.clock()
	if now - U.lastPoll > 1 then
		U.lastPoll = now
		pcall(C.poll)
	end
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

-- the command a window sends: held for nothing, encoded, recorded for the bridge, applied natively
local function handle(tag, cmd, cb, progress, O)
	if tag.name == "makeGameSetSpeedCmd" then
		U.stats.speed = U.stats.speed + 1
		send("speed", { n = tag.args[1] })
		return true
	end
	-- builds are captured where the engine applies them (the bridge's onPreBuildProposal), whoever issued them
	if tag.name == "makeWorldBuildProposalCmd" then return false end
	local spec = L.COMMANDS[tag.name]
	if not spec or spec.virtual then return false end
	local category = L.categoryOf(tag.name, tag.args)
	if category == "local" then return false end
	local rev = reverse()
	local ctx = { plain = C.plain, keyOf = function(e) return rev[e] or ("s:" .. tostring(e)) end }
	local ok, body, why = pcall(L.encodeCommand, tag.name, tag.args, ctx)
	if not ok then body, why = nil, tostring(body) end
	if not body then
		U.stats.failed = U.stats.failed + 1
		log("command " .. tag.name .. " is not replicated: " .. tostring(why))
		return false
	end
	body.native = true   -- applied natively here; the other games rebuild it at the stamp
	local okS, text = pcall(C.serialize, body)
	if not okS then
		U.stats.failed = U.stats.failed + 1
		log("command " .. tag.name .. " is not replicated: " .. tostring(text))
		return false
	end
	U.seq = U.seq + 1
	local id = U.seq
	send("cmd", { id = id, body = text, category = category })
	U.stats.shipped = U.stats.shipped + 1
	log("command " .. tag.name .. " #" .. id .. " recorded (" .. #text .. " bytes, " .. category .. ")")
	local function after(res, success, entities)
		if success and spec.creates then
			local e = nil
			pcall(function() e = res[spec.result] end)
			if e == nil and type(entities) == "table" and type(entities[1]) == "table" then e = entities[1][1] end
			if type(e) == "number" then send("bind", { id = id, e = e }) end
		end
		if cb then return cb(res, success, entities) end
	end
	O.sendCommand(cmd, after, progress)
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
					local okH2, handled = pcall(handle, tag, cmd, cb, progress, orig)
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

M.stats = U.stats
return M
