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

-- The hook that wraps the game's command functions in this Lua state (the windows' own), feverlink3_uihook.lua
local okK, Hook = pcall(ug_require, "feverlink3_uihook.lua")
if not okK then okK, Hook = pcall(ug_require, "feverlink3_1::/feverlink3_uihook.lua") end
if not okK or type(Hook) ~= "table" then
	print("[feverlink3] UI hook not loaded: " .. tostring(Hook))
	Hook = nil
end

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
	if not U.firstSeen then U.firstSeen = os.clock() end
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

-- ui_out.log has one writer (its sequence numbers only increase), which the hook owns when it is there
local function outbox()
	if Hook then return Hook.outbox() end
	if not U.outbox then U.outbox = Hud.newOutbox(C) end
	return U.outbox
end

local function send(text)
	return Hud.sendChat(C, outbox(), text)
end

-- ---- drawing

local WIDTH = 560 -- width of the window's content

local function setup(react, builtin)
	local V = builtin.type.Orientation.Vertical
	local H = builtin.type.Orientation.Horizontal
	local function text(s, class, width)
		return builtin.TextView{ meta = { class = class or "font-scale-title-4" }, text = s, width = width or WIDTH }
	end
	local function gap() return text(" ", "font-scale-annotation") end
	local function heading(s) return text(s, "font-scale-title-2") end
	local function section(s) return text(s, "font-scale-title-3") end
	local function note(s) return text(s, "font-scale-body") end
	local function box(orientation, children) return builtin.BoxLayout{ orientation = orientation, children = children } end
	local function column(children) return builtin.Component{ layout = box(V, children) } end
	-- kind: "primary" (the main action), "secondary" (default) or "negative" (stops something)
	local function button(label, onClick, kind, width)
		return builtin.Button{
			meta = { class = kind or "secondary" },
			content = text(label, "font-scale-title-4", width or WIDTH),
			onClick = onClick,
		}
	end
	local function field(placeholder, value, onTyping, displayOnly)
		return builtin.TextInputField{
			meta = { class = "font-scale-title-4" }, placeholderText = placeholder, value = value, onTyping = onTyping,
			displayOnly = displayOnly, maxLength = 200,
		}
	end
	-- "Anna  (host)" / "Ben  42 ms" / "Cleo  reconnecting"; width: narrower when a button sits next to the line
	local function playerLine(p, me, width)
		local line = "   " .. tostring(p.name)
		if p.id == 0 then line = line .. "  (host)" end
		if p.id == me then line = line .. "  (you)" end
		if p.state == "away" then
			line = line .. "  - reconnecting"
		elseif p.rtt and p.rtt > 0 and p.id ~= me then
			line = line .. "  - " .. tostring(p.rtt) .. " ms"
		end
		return text(line, nil, width)
	end

	local Window = react.RegisterWrapperRecipe("FeverLink3Window", builtin.Window, function(wp)
		local tick = react.useState(0)
		local typed = react.useState("")
		local nameIn = react.useState("")
		local codeIn = react.useState("")
		local steamIn = react.useState("")
		local showTest = react.useState(false)
		-- Two drivers share one throttle: the step timer, and the per-frame onStep, which still runs while the game is
		-- paused (a guest's window must notice the host ending the session then too).
		if not U.refresh then
			U.refresh = Hud.newTicker(0.5, os.clock, function()
				if Hook then Hook.devPoll() end
				local a = poll()
				local b = pollCtl()
				return a or b
			end)
		end
		local refresh = U.refresh
		local function onRefresh() if refresh() then tick:set(tick:old() + 1) end end
		react.onStepTimer(onRefresh, 0.5)   -- assumption: onStepTimer(fn, seconds), as in MPFever
		if react.onStep then react.onStep(onRefresh) end
		local ctl = U.ctl
		local rows = {}
		local function add(node) rows[#rows + 1] = node end
		local function myName() return nameIn:old() ~= "" and nameIn:old() or tostring(ctl and ctl.name or "") end
		-- canRemove (the host's list): a "Remove" button next to every player but the host, and under the list what the
		-- last one did ("Pat was removed", or why it was refused)
		local function players(me, canRemove)
			add(section("Players"))
			for _, p in ipairs(ctl.players or {}) do
				if canRemove and p.id ~= 0 and p.id ~= me then
					local id = p.id
					add(box(H, {
						playerLine(p, me, WIDTH - 140),
						button("Remove", function() command("kick", { id = id }) end, "negative", 120),
					}))
				else
					add(playerLine(p, me))
				end
			end
			if canRemove and ctl.result and ctl.result.kind == "kick" and ctl.result.message then
				add(note(tostring(ctl.result.message)))
			end
		end

		local state = ctl and ctl.state
		if not state then
			if not C.LIVE then
				state = "launcher"   -- started by the feverlink3 launcher (developer mode): no in-game controller
			elseif U.firstSeen and os.clock() - U.firstSeen > 15 then
				state = "missing"
			else
				state = "starting"
			end
		end
		if state == "launcher" then
			add(text("This game was started by the FeverLink3 launcher (developer mode); the session is managed there."))
		elseif state == "missing" then
			add(heading("FeverLink3 is not running"))
			add(text("The helper file winhttp.dll is not next to TransportFever3.exe. Close the game, copy winhttp.dll into the game's folder (see HOW-TO-PLAY.txt) and start the game again from Steam."))
		elseif state == "starting" then
			add(text("Starting the multiplayer module..."))
		elseif state == "idle" or state == "failed" then
			add(heading("Play together"))
			if ctl.notice then
				-- how the last session ended by itself: the world was closed, the host ended it or removed this player
				add(text(tostring(ctl.notice), "font-scale-title-3"))
			elseif state == "failed" then
				add(text("Could not connect: " .. tostring(ctl.message or "unknown problem"), "font-scale-title-3"))
			end
			if state ~= "failed" then
				add(note("One player hosts and sends a code; the others join with it."))
			end
			add(gap())
			add(section("Your name"))
			add(field("name", myName(), function(v) nameIn:set(v) end))
			add(gap())
			add(button("Host a game", function() command("host", { name = myName() }) end, "primary"))
			add(gap())
			add(section("Join a friend's game"))
			add(field("paste the join code here", codeIn:old(), function(v) codeIn:set(v) end))
			add(button("Join", function() command("join", { name = myName(), code = codeIn:old() }) end, "primary"))

			-- Steam connection test: can two players reach each other through Steam, without any router setup?
			-- Folded away by default (it shows the player's Steam ID).
			local st = ctl.steam
			add(gap())
			add(button(showTest:old() and "Hide the connection test" or "Connection test...", function() showTest:set(not showTest:old()) end, "secondary"))
			if showTest:old() then
			add(section("Steam connection test"))
			if st and st.id then
				add(note("Your Steam ID (send it to a friend, who pastes it below):"))
				add(field("your Steam ID", tostring(st.id), function() end, true))
			else
				add(note("Steam: " .. tostring(st and st.state or "unknown")))
			end
			add(field("paste the other player's Steam ID", steamIn:old(), function(v) steamIn:set(v) end))
			add(button("Test the connection", function() command("steamtest", { id = steamIn:old() }) end))
			if st and st.note then add(note(tostring(st.note))) end
			if st and st.test then
				local t = st.test
				local line = "Test: " .. tostring(t.state) .. ", answers " .. tostring(t.pongs)
				if t.rtt then line = line .. ", last round trip " .. tostring(t.rtt) .. " ms" end
				if t.route and t.route ~= "" then line = line .. ", " .. tostring(t.route) end
				add(note(line))
			end
			if st and st.pings and st.pings > 0 then add(note("Pings received from other players: " .. tostring(st.pings))) end
			end
		elseif state == "hosting" then
			add(heading("You are hosting"))
			add(note("Send this join code to the other players:"))
			-- the game's text fields cannot copy, so the button puts the code on the clipboard (done by the module)
			add(field("join code", tostring(ctl.code or ""), function() end, true))
			add(button("Copy code", function() command("copy", {}) end, "secondary"))
			if ctl.result and ctl.result.kind == "copy" then
				add(note(tostring(ctl.result.ok and "Copied. Paste it into a message to your friend (Ctrl+V)." or ctl.result.message)))
			end
			if ctl.password then add(note("They also need the password.")) end
			add(gap())
			players(ctl.me, true)
			add(gap())
			if ctl.started and ctl.resync and ctl.resync ~= "Idle" then
				add(note("Sending your world to the players: " .. tostring(ctl.resync)))
			end
			if ctl.started then
				add(button("Sync everyone to my world", function() command("resync", {}) end))
			end
			if ctl.started then
				-- the speed buttons of the Session section pause and resume
				add(button("Stop hosting", function() command("leave", {}) end, "negative"))
			else
				add(box(H, {
					button("Start the session", function() command("start", {}) end, "primary", math.floor(WIDTH / 2) - 10),
					button("Stop hosting", function() command("leave", {}) end, "negative", math.floor(WIDTH / 2) - 10),
				}))
			end
		elseif state == "joining" then
			add(heading("Joining..."))
			add(text(tostring(ctl.progress or "contacting the host"), "font-scale-title-3"))
			add(gap())
			add(button("Cancel", function() command("leave", {}) end, "negative"))
		elseif state == "joined" then
			add(heading("Connected"))
			if ctl.started then
				add(note("The session is running. Anyone can change the speed or chat; the host can sync everyone to their world."))
			else
				add(note("Wait for the host to start the session. The host's world is sent to you and your game reloads into it."))
			end
			add(gap())
			players(ctl.me)
			add(gap())
			add(button("Leave the session", function() command("leave", {}) end, "primary"))
		end

		-- the game side: status and chat, once the mod is part of a session
		local m = U.model
		if m and C.ACTIVE then
			add(gap())
			add(section("Session"))
			add(note(Hud.statusText(m)))
			for _, line in ipairs(Hud.chatLines(m, 12)) do add(note(line)) end
			add(field("Type a message and press Send", typed:old(), function(v) typed:set(v) end))
			add(button("Send", function()
				if send(typed:old()) then typed:set("") end
			end))
		end
		-- developer test button (FEVERLINK3_DEV=1 only): runs dev_click.lua from inside a click, where commands may be sent.
		-- The first row, so that its place in the window does not depend on what the window shows.
		if Hook and Hook.dev then
			table.insert(rows, 1, button("DEV run", function() Hook.devClick() end, "secondary"))
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
	if Hook and C.DIR and C.IO then
		local okI, errI = pcall(Hook.install)
		print("[feverlink3] UI hook: " .. (okI and (errI and "installed" or "not installed") or ("failed: " .. tostring(errI))))
	end
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
