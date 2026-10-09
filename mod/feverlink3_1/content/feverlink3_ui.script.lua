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

local WIDTH = 560 -- width of the window's content

local function setup(react, builtin)
	local V = builtin.type.Orientation.Vertical
	local H = builtin.type.Orientation.Horizontal
	local function text(s, class, width)
		return builtin.TextView{ meta = { class = class or "font-scale-title-4" }, text = s, width = width or WIDTH }
	end
	local function gap() return text(" ", "font-scale-body") end
	local function box(orientation, children) return builtin.BoxLayout{ orientation = orientation, children = children } end
	local function column(children) return builtin.Component{ layout = box(V, children) } end
	local function button(label, onClick, width)
		return builtin.Button{ meta = { class = "secondary" }, content = text(label, "font-scale-title-4", width or WIDTH), onClick = onClick }
	end
	local function field(placeholder, value, onTyping, displayOnly)
		return builtin.TextInputField{
			meta = { class = "font-scale-title-4" }, placeholderText = placeholder, value = value, onTyping = onTyping,
			displayOnly = displayOnly, maxLength = 200,
		}
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
		local function add(node) rows[#rows + 1] = node end
		local function myName() return nameIn:old() ~= "" and nameIn:old() or tostring(ctl and ctl.name or "") end

		local state = ctl and ctl.state or "starting"
		if state == "starting" then
			add(text("Starting the multiplayer module..."))
		elseif state == "idle" or state == "failed" then
			if state == "failed" then
				add(text("Could not connect: " .. tostring(ctl.message or "unknown problem"), "font-scale-title-3"))
				add(gap())
			end
			add(text("Play together: one player hosts, the others join with the host's code."))
			add(gap())
			add(text("Your name"))
			add(field("name", myName(), function(v) nameIn:set(v) end))
			add(gap())
			add(button("Host a game", function() command("host", { name = myName() }) end))
			add(gap())
			add(text("Or join a friend"))
			add(field("paste the join code here", codeIn:old(), function(v) codeIn:set(v) end))
			add(button("Join", function() command("join", { name = myName(), code = codeIn:old() }) end))
		elseif state == "hosting" then
			add(text("You are hosting", "font-scale-title-2"))
			add(text("Send this join code to the other players (click it, Ctrl+A, Ctrl+C):"))
			add(field("join code", tostring(ctl.code or ""), function() end, true))
			if ctl.password then add(text("They also need the password.")) end
			if ctl.public then
				add(text("Internet address: " .. tostring(ctl.public), "font-scale-body"))
			end
			if ctl.lookup and ctl.lookup ~= "" then add(text(tostring(ctl.lookup), "font-scale-body")) end
			local where = {}
			for _, ep in ipairs(ctl.endpoints or {}) do
				if ep ~= ctl.public then where[#where + 1] = tostring(ep) end
			end
			if #where > 0 then add(text("Also reachable inside your network / VPN: " .. table.concat(where, "  /  "), "font-scale-body")) end
			add(gap())
			add(text("Players", "font-scale-title-3"))
			for _, p in ipairs(ctl.players or {}) do
				add(text("  " .. tostring(p.name) .. (p.id == 0 and "  (host)" or ""), "font-scale-title-4"))
			end
			add(gap())
			if ctl.started then
				add(button("Sync everyone to my world", function() command("resync", {}) end))
			end
			add(box(H, {
				button(ctl.started and "Pause" or "Start the session", function() command(ctl.started and "pause" or "start", {}) end, math.floor(WIDTH / 2) - 10),
				button("Stop hosting", function() command("leave", {}) end, math.floor(WIDTH / 2) - 10),
			}))
		elseif state == "joining" then
			add(text("Connecting to the host...", "font-scale-title-3"))
			add(button("Cancel", function() command("leave", {}) end))
		elseif state == "joined" then
			add(text("Connected", "font-scale-title-2"))
			add(text("Wait for the host to start the session."))
			add(gap())
			add(text("Players", "font-scale-title-3"))
			for _, p in ipairs(ctl.players or {}) do
				add(text("  " .. tostring(p.name) .. (p.id == 0 and "  (host)" or ""), "font-scale-title-4"))
			end
			add(gap())
			add(button("Leave", function() command("leave", {}) end))
		end

		-- the game side: status and chat, once the mod is part of a session
		local m = U.model
		if m and C.ACTIVE then
			add(gap())
			add(text(Hud.statusText(m), "font-scale-body"))
			for _, line in ipairs(Hud.chatLines(m, 12)) do add(text(line, "font-scale-body")) end
			add(field("Type a message and press Send", typed:old(), function(v) typed:set(v) end))
			add(button("Send", function()
				if send(typed:old()) then typed:set("") end
			end))
		end
		return builtin.Window{
			title = "FeverLink3 Multiplayer",
			id = "window.feverlink3",
			initialX = 0.3,
			initialY = 0.22,
			movable = true,
			closable = true,
			onClose = wp.onClose,
			content = column(rows),
		}
	end)

	-- the bar plugin: a button in the game bar that opens the window
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
		-- like the game's own icon buttons (eye, music): a default-styled button holding an auto-fitted image
		return builtin.BoxLayout{
			children = {
				builtin.Button{
					meta = { tooltip = "FeverLink3 multiplayer" },
					content = builtin.TextView{ meta = { class = "font-scale-body" }, text = "FeverLink3" },
					onClick = open,
				},
			},
		}
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
