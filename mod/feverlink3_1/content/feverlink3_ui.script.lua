-- FeverLink3 in-game window: lobby (host or join), status, player list and chat.
--
-- ENGINE-BOUND. Everything with a rule in it (roster, history, status text, the snapshot and outbox files) is in
-- feverlink3_hud.lua and is tested; this file draws that model with the game's react toolkit. The lobby talks to
-- the native module, which runs the session inside the game: commands go out as lines of ctl_cmd.log, the answer
-- is ctl_status.txt (see launcher/src/live.rs). The names of the toolkit calls come from MPFever (MIT,
-- mpfever_ui.script.lua and mpfever_menu.lua); the ones not yet seen working in a game are marked.
--
-- Registered through feverlink3_entry.res.lua (react-plugin on ModEntryPointExtension; finding from MPFever (MIT):
-- the plugin is mounted once for the whole UI session). The chat talks to the bridge through two files in the
-- session folder, never directly: the bridge owns the file link.

local okC, C = pcall(ug_require, "feverlink3_common.lua")
if not okC then C = ug_require("feverlink3_1::/feverlink3_common.lua") end
local okH, Hud = pcall(ug_require, "feverlink3_hud.lua")
if not okH then Hud = ug_require("feverlink3_1::/feverlink3_hud.lua") end

local log = C.logger("ui")
local M = {}
local U = { model = nil, outbox = nil, ctl = nil, ctlVersion = nil, cmd = nil }

local CTL_STATUS = "ctl_status.txt"
local CTL_CMD = "ctl_cmd.log"

local function poll()
	local seen = U.model and U.model.version or nil
	local m = Hud.readSnapshot(C, seen)
	if m then U.model = m; return true end
	return false
end

-- the native module's state (nil until it has written the first one): true when it changed
local function pollCtl()
	C.poll()
	local s = C.parse(C.readFile(C.path(CTL_STATUS)) or "")
	if type(s) ~= "table" then return false end
	if U.ctl and U.ctl.version == s.version then return false end
	U.ctl = s
	return true
end

local function command(kind, params)
	if not U.cmd then U.cmd = C.newWriter(C.path(CTL_CMD), 0) end
	return C.writerSend(U.cmd, kind, params)
end

local function send(text)
	if not U.outbox then U.outbox = Hud.newOutbox(C) end
	return Hud.sendChat(C, U.outbox, text)
end

-- ---- drawing

local function setup(react, builtin)
	local V = builtin.type.Orientation.Vertical
	local function text(s) return builtin.TextView{ meta = { class = "font-scale-title-4" }, text = s } end
	local function column(children) return builtin.Component{ layout = builtin.BoxLayout{ orientation = V, children = children } } end
	local function button(label, onClick)
		return builtin.Button{ meta = { class = "secondary" }, content = text(label), onClick = onClick }
	end
	local function field(placeholder, value, onTyping)
		return builtin.TextInputField{ meta = { class = "font-scale-title-4" }, placeholderText = placeholder, value = value, onTyping = onTyping }
	end

	local Window = react.RegisterWrapperRecipe("FeverLink3Window", builtin.Window, function(wp)
		local tick = react.useState(0)
		local typed = react.useState("")
		local nameIn = react.useState("")
		local codeIn = react.useState("")
		react.onStepTimer(function()
			local a = poll()
			local b = pollCtl()
			if a or b then tick:set(tick:old() + 1) end
		end, 0.5)   -- assumption: onStepTimer(fn, seconds), as in MPFever
		local ctl = U.ctl
		local rows = {}

		-- lobby
		local state = ctl and ctl.state or "starting"
		if state == "starting" then
			rows[#rows + 1] = text("Waiting for the FeverLink3 module...")
		elseif state == "idle" or state == "failed" then
			if state == "failed" then rows[#rows + 1] = text("Problem: " .. tostring(ctl.message or "unknown")) end
			rows[#rows + 1] = text("Your name")
			rows[#rows + 1] = field("name", nameIn:old() ~= "" and nameIn:old() or tostring(ctl.name or ""), function(v) nameIn:set(v) end)
			rows[#rows + 1] = button("Host a game", function()
				command("host", { name = nameIn:old() ~= "" and nameIn:old() or ctl.name })
			end)
			rows[#rows + 1] = text("or join with a code")
			rows[#rows + 1] = field("join code", codeIn:old(), function(v) codeIn:set(v) end)
			rows[#rows + 1] = button("Join", function()
				command("join", { name = nameIn:old() ~= "" and nameIn:old() or ctl.name, code = codeIn:old() })
			end)
		elseif state == "hosting" then
			rows[#rows + 1] = text("Hosting. Give this code to the other players:")
			rows[#rows + 1] = field("join code", tostring(ctl.code or ""), function() end)
			if ctl.password then rows[#rows + 1] = text("(they also need the password)") end
			for _, ep in ipairs(ctl.endpoints or {}) do rows[#rows + 1] = text("address " .. tostring(ep)) end
			rows[#rows + 1] = button(ctl.started and "Pause" or "Start the session", function()
				command(ctl.started and "pause" or "start", {})
			end)
			rows[#rows + 1] = button("Stop hosting", function() command("leave", {}) end)
		elseif state == "joining" then
			rows[#rows + 1] = text("Joining...")
			rows[#rows + 1] = button("Cancel", function() command("leave", {}) end)
		elseif state == "joined" then
			rows[#rows + 1] = text("Joined the session.")
			rows[#rows + 1] = button("Leave", function() command("leave", {}) end)
		end
		if ctl and ctl.players and #ctl.players > 0 then
			local names = {}
			for _, p in ipairs(ctl.players) do names[#names + 1] = tostring(p.name) end
			rows[#rows + 1] = text("Players: " .. table.concat(names, ", "))
		end

		-- the game side: status and chat, once the mod is part of a session
		local m = U.model
		if m and C.ACTIVE then
			rows[#rows + 1] = text(Hud.statusText(m))
			for _, line in ipairs(Hud.chatLines(m, 15)) do rows[#rows + 1] = text(line) end
			rows[#rows + 1] = field("Type a message", typed:old(), function(v) typed:set(v) end)
			rows[#rows + 1] = button("Send", function()
				if send(typed:old()) then typed:set("") end
			end)
		end
		return builtin.Window{
			title = "FeverLink3",
			id = "window.feverlink3",
			initialX = 0.02,
			initialY = 0.3,
			movable = true,
			closable = true,
			onClose = wp.onClose,
			content = column(rows),
		}
	end)

	-- the bar plugin: a button in the game bar that opens the window (getDefaultWindowApi is the game's own global)
	local function open()
		local okG, globals = pcall(ug_require, "::/gui/main/game_react_globals.tl")
		local okA, api = false, nil
		if okG and globals then okA, api = pcall(globals.getDefaultWindowApi) end
		if not okA then api = nil end
		if not api then log("no window api"); return end
		local okO, err = pcall(function()
			api.addSingletonWindow(Window, { onClose = function() api.removeAllWindows(Window) end })
		end)
		if not okO then log("cannot open the window: " .. tostring(err)) end
	end
	return function()
		return builtin.BoxLayout{ orientation = builtin.type.Orientation.Horizontal, children = { button("FeverLink3", open) } }
	end
end

do
	local okR, react = pcall(ug_require, "::/gui/main/react.lua")
	local okB, builtin = pcall(ug_require, "::/gui/main/builtin.lua")
	local okE, entry = pcall(ug_require, "::/gui/game_bar/game_bar_widgets.tl")
	print("[feverlink3] ui: active=" .. tostring(C.ACTIVE) .. " dir=" .. tostring(C.DIR) .. " id=" .. tostring(C.PLAYER_ID) .. " io=" .. tostring(C.IO ~= nil) .. " react=" .. tostring(okR) .. " builtin=" .. tostring(okB) .. " entry=" .. tostring(okE))
	if okR and okB and okE and entry and entry.GameBarInfoDisplayExtension then
		local okS, recipeFn = pcall(function()
			if C.DIR and C.IO then return setup(react, builtin) end
			-- not started by the launcher: an empty, invisible entry so the game has a recipe to mount
			return function() return builtin.BoxLayout { children = {} } end
		end)
		if okS then
			local okP, recipe = pcall(react.RegisterPluginRecipe, entry.GameBarInfoDisplayExtension, "FeverLink3Bar", recipeFn)
			if okP then M.FeverLink3Bar = recipe else print("[feverlink3] entry registration failed: " .. tostring(recipe)) end
		else
			print("[feverlink3] window setup failed: " .. tostring(recipeFn))
		end
	else
		log("UI unavailable: react=" .. tostring(okR) .. " builtin=" .. tostring(okB) .. " entry=" .. tostring(okE))
	end
end

function data()
	return M
end
