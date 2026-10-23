-- FeverLink3 in-game interface: the game bar icon, its pop-up, and the window (start screen; in a session the tabs Players,
-- Chat and Host).
--
-- ENGINE-BOUND. Everything with a rule in it (roster, history, status line, icon colour, notices, the snapshot and outbox files)
-- is in feverlink3_hud.lua and is tested; this file draws that model with the game's react toolkit. The lobby talks to the
-- native module, which runs the session inside the game: commands go out as lines of ctl_cmd.log, the answer is
-- ctl_status.txt (see launcher/src/live.rs).
--
-- Documented calls only (owner, 2026-10-21; tf3-docs reference/base): builtin widgets (scripts/builtin: Window, BoxLayout,
-- Component, TextView, ImageView, Button, TextInputField, TabWidget, TabWidgetChild, ProgressBar), react hooks
-- (scripts/react: RegisterRecipe, RegisterWrapperRecipe, RegisterPluginRecipe, useState, useSelfRef, onStep, onStepTimer,
-- Meta fields class / tooltip / styleSheet, ReactNodeRef getPosition), styleutil.makeStyle (gui/main/styleutil: color, size),
-- game_react_globals.getDefaultWindowApi (gui/main/game_react_globals), WindowAPI addSingletonWindow / addWindow /
-- removeAllWindows, popover_react_util.PopoverWindow (gui/main/popover_react_util), GameBarInfoDisplayExtension
-- (gui/game_bar/game_bar_widgets). Style class names are the base game's (gui/main/default.css.lua: primary, secondary,
-- error-tape; font-scale-*). Marked "UNPROVEN" below: what was not yet seen in a real game.
--
-- Registered through feverlink3_entry.res.lua (react-plugin on GameBarInfoDisplayExtension). The chat talks to the bridge
-- through two files in the session folder, never directly: the bridge owns the file link.

local okC, C = pcall(ug_require, "feverlink3_common.lua")
if not okC then C = ug_require("feverlink3::/feverlink3_common.lua") end
local okH, Hud = pcall(ug_require, "feverlink3_hud.lua")
if not okH then Hud = ug_require("feverlink3::/feverlink3_hud.lua") end

-- The hook that wraps the game's command functions in this Lua state (the windows' own), feverlink3_uihook.lua
local okK, Hook = pcall(ug_require, "feverlink3_uihook.lua")
if not okK then okK, Hook = pcall(ug_require, "feverlink3::/feverlink3_uihook.lua") end
if not okK or type(Hook) ~= "table" then
	print("[feverlink3] UI hook not loaded: " .. tostring(Hook))
	Hook = nil
end

local log = C.logger("ui")
local M = {}
-- rev counts changes of ctl or model; every recipe re-renders when it differs from what it drew
local U = { model = nil, outbox = nil, ctl = nil, cmd = nil, rev = 0 }

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
	if not U.firstSeen then U.firstSeen = C.clock() end
	local s = C.parse(C.readFile(C.path(CTL_STATUS)) or "")
	if type(s) ~= "table" then return false end
	if U.ctl and U.ctl.version == s.version then return false end
	U.ctl = s
	return true
end

-- defined below (it needs withMods and command); declared here so the refresh's closure sees the local, not a global nil
-- (two windows 2026-10-23: the refresh called a global nil, no `mods` command was ever sent)
local shareMods

-- One throttled refresh for every recipe (bar, pop-up, window). The step timer is not guaranteed to fire while the game is
-- paused, so the window also drives it from onStep (every interface frame); Hud.newTicker keeps it to once per 0.5 s.
U.refresh = Hud.newTicker(0.5, C.clock, function()
	if Hook then Hook.devPoll() end
	local a = poll()
	local b = pollCtl()
	pcall(shareMods)
	if a or b then U.rev = U.rev + 1 end
end)

-- `host` / `join` carry this game's mods and build for the join's comparison (docs/PROTOCOL.md, "Mod list"); read at
-- the click, so a mod installed meanwhile counts. getBuildVersion() is documented (tf3-docs reference/api/main.html).
-- `quiet`: the periodic re-read (shareMods) says nothing.
local function withMods(params, quiet)
	local mods, why = Hud.modList(app)
	if mods then params.mods = mods elseif not quiet then log("mod list not readable: " .. tostring(why)) end
	local okB, build = pcall(function() return getBuildVersion() end)
	if okB and build ~= nil then params.game = tostring(build) end
	return params
end

local function command(kind, params)
	if not U.cmd then U.cmd = C.newWriter(C.path(CTL_CMD), 0) end
	return C.writerSend(U.cmd, kind, params)
end

-- A join through a Steam invite has no window command of its own (friend session 2026-10-23: the host warned "could not list
-- its mods", the friend's report said "game build: unknown"), so the window tells the controller its mods and build by
-- itself (`mods`, docs/LIVE_CONTROL.md): once the controller answers, then again only when they changed (re-read every 30 s).
local MODS_EVERY = 30
shareMods = function()
	-- no list read yet (the repository is refused for a moment after the load: "API is currently restricted", two windows
	-- 2026-10-23): try again in 2 s, not 30
	local every = U.modsRead and MODS_EVERY or 2
	if not U.ctl or (U.modsAt and C.clock() - U.modsAt < every) then return end
	U.modsAt = C.clock()
	local params = withMods({}, U.modsKey ~= nil)
	U.modsRead = params.mods ~= nil
	local key = Hud.modsKey(params)
	if key ~= U.modsKey and command("mods", params) then U.modsKey = key end
end

-- ui_out.log has one writer in this Lua state, which the hook owns when it is there
local function outbox()
	if Hook then return Hook.outbox() end
	if not U.outbox then U.outbox = Hud.newOutbox(C) end
	return U.outbox
end

local function send(text)
	return Hud.sendChat(C, outbox(), text)
end

local function nowTime()
	local ok, t = pcall(C.time)
	return ok and t or nil
end

-- The window's state: what the controller says, or why there is nothing to say
local function windowState(ctl)
	if ctl and ctl.state then return ctl.state end
	if not C.LIVE then return "launcher" end   -- started by the feverlink3 launcher (developer mode): no in-game controller
	if U.firstSeen and C.clock() - U.firstSeen > 15 then return "missing" end
	return "starting"
end

-- ---- drawing

local WIDTH = 480      -- the window's fixed content width, every state (audit 2026-10-21: the width changed per state, title clipped)
local POP_W = 340      -- the pop-up's content width
-- The one inner margin of the window's and the pop-up's content, every side (owner, 2026-10-23: no text touches the border).
-- 8 px is what the game gives its own: the window title (builtin.css.lua Window::Title margin 4 + padding 4), a tab's content
-- (TabWidget::Content padding 8) and a pop-up dialog's content (popover_react_util.css.lua padding 8), so our text lines up
-- with the title above it.
local PAD = 8
-- How many characters of body text fit on a line (real game 2026-10-22: the pop-up cut a line after 38 characters, the
-- window after 64, both without PAD), with a margin; a heading's larger font gets 4/5 of it. Two characters fewer than
-- before for the 2 x PAD the padding takes (~7.5 px a character). Set by the recipe that draws (pop-up or window).
local POP_CHARS, WINDOW_CHARS = 32, 54
local wrapChars = WINDOW_CHARS
local MOD = "feverlink3::/"   -- <modId>::/path, a file in this mod's content/ (tf3-docs guides/mod-structure)
local COLOR = {
	info = { 0.85, 0.85, 0.85, 1 }, warn = { 0.855, 0.647, 0.125, 1 }, error = { 0.94, 0.45, 0.45, 1 },
	green = { 0.18, 0.69, 0.29, 1 }, yellow = { 0.95, 0.76, 0.19, 1 }, red = { 0.94, 0.45, 0.45, 1 }, white = { 1, 1, 1, 1 },
}

local function setup(react, builtin, X)
	local V = builtin.type.Orientation.Vertical
	local H = builtin.type.Orientation.Horizontal
	-- styleutil.makeStyle: documented keys color and size only; nil when the module is missing (then the game's default look)
	local styles = {}
	local function style(key, t)
		if not X.styleutil then return nil end
		if styles[key] == nil then
			local ok, s = pcall(X.styleutil.makeStyle, t)
			styles[key] = ok and s or false
		end
		return styles[key] or nil
	end
	local function tinted(level, alpha)
		local c = COLOR[level] or COLOR.info
		local a = math.floor((alpha or 1) * 10 + 0.5) / 10
		return style("c:" .. tostring(level) .. ":" .. a, { color = { c[1], c[2], c[3], a } })
	end
	-- two text sizes only (audit: several sizes of small grey text): body and heading. A text longer than a line becomes a
	-- column of TextViews, one per line (Hud.wrapLines: TextView itself does not wrap and cut long texts at the edge)
	local function text(s, level, alpha, class)
		class = class or "font-scale-title-4"
		local meta = { class = class, styleSheet = level and tinted(level, alpha) or nil }
		local lines = Hud.wrapLines(s, class == "font-scale-title-4" and wrapChars or math.floor(wrapChars * 4 / 5))
		if #lines == 1 then return builtin.TextView{ meta = meta, text = s } end
		local views = {}
		for i, line in ipairs(lines) do views[i] = builtin.TextView{ meta = meta, text = line } end
		return builtin.BoxLayout{ orientation = V, children = views }
	end
	local function heading(s, level) return text(s, level, nil, "font-scale-title-2") end
	local function box(orientation, children) return builtin.BoxLayout{ orientation = orientation, children = children } end
	local function column(children) return builtin.Component{ layout = box(V, children) } end
	-- a column of a fixed width and its content's height: size { w, -1 }, -1 = "not fixed" on that axis, the way the game sizes
	-- its own columns (gui/entity_window/industry_react_util.tl: a Component with makeStyle{ size = { w, -1 } } around a
	-- BoxLayout). An empty spacer(WIDTH, 1) row did not hold the window's width (UI run 2026-10-21). UNPROVEN in the real game.
	-- The column also carries the one PAD margin on every side: api.gui.StyleSheet documents `padding` (Vec4f, one value per
	-- side, top first; tf3-docs reference api/api/gui), which styleutil.makeStyle has no key for, so it is set on the sheet
	-- makeStyle returns. Every row of the window and the pop-up sits inside it; no row has a margin of its own. If the field
	-- cannot be set, the column keeps its size without the margin. UNPROVEN in the real game (an inline padding 0 on the bar's
	-- Button did not take, 2026-10-21; a Component has no class padding to override it).
	local function fixedColumn(w, children)
		local key = "s:" .. w .. "x-1:p" .. PAD
		if styles[key] == nil then
			local s = style("s:" .. w .. "x-1", { size = { w, -1 } })
			if s then pcall(function() s.padding = api.type.Vec4f.new(PAD, PAD, PAD, PAD) end) end
			styles[key] = s or false
		end
		return builtin.Component{ meta = { styleSheet = styles[key] or nil }, layout = box(V, children) }
	end
	-- an empty component of a fixed size (the gaps between rows; the pop-up's width)
	local function spacer(w, h)
		return builtin.Component{ meta = { styleSheet = style("s:" .. w .. "x" .. h, { size = { w, h } }) }, layout = box(H, {}) }
	end
	local function gap() return spacer(1, 8) end
	-- kind: "primary" (the one main action of a screen), "secondary" (default) or "negative" (Leave, Stop, Kick, Refuse): the
	-- base game's red "error-tape" look (gui/main/default.css.lua), a filled red button (UNPROVEN on a Button)
	local function button(label, onClick, kind)
		local class = kind == "negative" and "error-tape" or (kind or "secondary")
		return builtin.Button{ meta = { class = class }, content = text(label), onClick = onClick }
	end
	local function field(placeholder, value, onTyping, displayOnly)
		return builtin.TextInputField{
			meta = { class = "font-scale-title-4" }, placeholderText = placeholder, value = value, onTyping = onTyping,
			displayOnly = displayOnly, maxLength = 200,
		}
	end
	local function image(path, size)
		return builtin.ImageView{ meta = { styleSheet = style("s:" .. size .. "x" .. size, { size = { size, size } }) }, path = path }
	end
	local function dot(colour) return image(MOD .. "feverlink3_dot_" .. colour .. ".tga", 12) end
	-- Shared refresh for a recipe: re-render when ctl or the model changed, and once a second while a notice fades.
	-- perFrame: also from onStep (the window must notice the host ending the session while the game is paused).
	local function useLive(perFrame)
		local drawn = react.useState("")
		local function drive()
			-- the stamp applies of the window commands the hook holds: their callbacks run as soon as the bridge has answered
			if Hook and Hook.poll then pcall(Hook.poll) end
			U.refresh()
			local now = nowTime()
			local fading = now and Hud.latestNotice(U.model, now, Hud.NOTICE_SHOWN_S) and now or ""
			local key = U.rev .. ":" .. tostring(fading)
			if drawn:old() ~= key then drawn:set(key) end
		end
		react.onStepTimer(drive, 0.5)   -- onStepTimer(fn, intervalSeconds?, jitter?) (tf3-docs base/scripts/react)
		if perFrame and react.onStep then react.onStep(drive) end
	end

	local function windowApi()
		if not X.globals then return nil end
		local ok, api = pcall(X.globals.getDefaultWindowApi)
		return ok and api or nil
	end

	-- one player line: coloured dot, name, (host) / (you), the ping ("12 ms") or "reconnecting"; Kick next to it for the host
	local function playerRow(p, ctl, canKick)
		local colour, ping = Hud.playerState(p, ctl.me, U.model)
		local label = tostring(p.name) .. (p.id == 0 and "  (host)" or "") .. (p.id == ctl.me and "  (you)" or "")
		local children = { dot(colour), text("  " .. label) }
		if ping ~= "" then children[#children + 1] = text("   " .. ping, ping == "reconnecting" and "warn" or "info") end
		if canKick and p.id ~= 0 and p.id ~= ctl.me then
			local id = p.id
			children[#children + 1] = text("   ")
			children[#children + 1] = button("Kick", function() command("kick", { id = id }) end, "negative")
		end
		return box(H, children)
	end

	-- the join requests the host must answer, each with Let in / Refuse; letIn: the button's kind (the one primary of the pop-up,
	-- a secondary in the window, whose primary is the start or the chat's Send)
	local function requestRows(ctl, add, letIn)
		if ctl.state ~= "hosting" or type(ctl.invites) ~= "table" then return end
		for _, r in ipairs(ctl.invites.requests or {}) do
			local id = r.id
			add(text(Hud.requestLabel(r), "warn"))
			add(box(H, {
				button("Let in", function() command("admit", { id = id }) end, letIn),
				text("  "),
				button("Refuse", function() command("refuse", { id = id }) end, "negative"),
			}))
		end
		if ctl.result and (ctl.result.kind == "admit" or ctl.result.kind == "refuse") then add(text(tostring(ctl.result.message))) end
	end

	local function chatInput(typed, add, kind)
		add(field("Type a message", typed:old(), function(v) typed:set(v) end))
		add(button("Send", function() if send(typed:old()) then typed:set("") end end, kind))
	end

	-- "Report a problem": the note, the button, then what the controller says (ctl.report, launcher/src/live/report.rs). The
	-- words are feverlink3_text.lua's T.REPORT; the note lives in U, not in a hook, because the pop-up shows this only while red.
	local function texts()
		if U.T == nil then
			local ok, T = pcall(ug_require, "feverlink3_text.lua")
			if not ok then ok, T = pcall(ug_require, "feverlink3::/feverlink3_text.lua") end
			U.T = ok and type(T) == "table" and T or false
		end
		return U.T
	end

	-- the automatic update's line (launcher/src/autoupdate.rs): downloading, restart to finish, failed (T.UPDATE's words)
	local function updateRow(add, ctl)
		local T = texts()
		if not (T and T.updateLine and type(ctl) == "table") then return end
		local line, level = T.updateLine(ctl.update)
		if line then add(text(line, level)) end
	end

	-- "Save and quit" under the update line (Hud.saveAndQuit): only while an update waits and no session runs. U.quit: nil, or
	-- { at = clock } while the save runs, or { failed = reason }; a save that has not called back after QUIT_WAIT_S gives the
	-- button back (the game stays open, today's "Restart the game to finish" stays).
	local QUIT_WAIT_S = 300
	local function quitRows(add, ctl)
		local T = texts()
		if not (T and T.UPDATE.quit and Hud.offerSaveAndQuit(ctl)) then return end
		local q = U.quit
		if q and q.at and C.clock() - q.at > QUIT_WAIT_S then
			log("save and quit: no answer from the save after " .. QUIT_WAIT_S .. " s")
			q = { failed = "the save did not finish" }
			U.quit = q
		end
		if q and q.at then add(text(T.UPDATE.saving)); return end
		if q and q.failed then add(text(T.quitFailed(q.failed), "warn")) end
		add(button(T.UPDATE.quit, function()
			if U.quit and U.quit.at then return end
			U.quit = { at = C.clock() }
			Hud.saveAndQuit(app, log, function(reason) U.quit = { failed = reason }; U.rev = U.rev + 1 end)
			U.rev = U.rev + 1
		end))
		add(text(T.UPDATE.quitNote))
	end

	-- this run must restart before a session (the module refuses Host and Join too)
	local function mustRestart(ctl)
		return type(ctl) == "table" and type(ctl.update) == "table" and ctl.update.restart == true
	end

	local function reportRows(add, ctl)
		texts()
		local R = U.T and U.T.REPORT or { button = "Report a problem", note = "What were you doing? (optional)", explain = "", contents = "" }
		local r = type(ctl) == "table" and type(ctl.report) == "table" and ctl.report or nil
		add(field(R.note, U.reportNote or "", function(v) U.reportNote = v end))
		if not (r and (r.state == "making" or r.state == "collecting")) then
			add(button(R.button, function() command("report", withMods({ note = U.reportNote or "" })) end))
		end
		if not r then add(text(R.explain)); return end
		add(text(tostring(r.message or ""), r.state == "failed" and "error" or nil))
		if r.state == "done" and type(r.contents) == "table" then
			add(text(R.contents))
			for i, line in ipairs(r.contents) do if i <= 12 then add(text("  " .. tostring(line))) end end
			if #r.contents > 12 then add(text("  ... and " .. (#r.contents - 12) .. " more")) end
		end
	end

	local function openWindow() end   -- set below, once the window recipe exists

	-- ---- the pop-up of the bar icon (popover_react_util.PopoverWindow)
	local Popover = react.RegisterRecipe("FeverLink3Popover", function(params)
		useLive(false)
		wrapChars = POP_CHARS
		local typed = react.useState("")
		local ctl, m = U.ctl, U.model
		local rows = { spacer(POP_W - 2 * PAD, 1) }   -- the width inside the padded column (fixedColumn)
		local function add(node) rows[#rows + 1] = node end
		local now = nowTime()
		add(text(Hud.statusLine(ctl, m), Hud.iconState(ctl, m, now) == "red" and "error" or nil, nil, "font-scale-title-3"))
		local n, age = Hud.latestNotice(m, now, Hud.NOTICE_SHOWN_S)
		-- the newest bridge notice, fading over its last seconds
		if n then add(text(n.text, n.level, age > 7 and math.max(0.3, (Hud.NOTICE_SHOWN_S - age) / 3) or 1)) end
		updateRow(add, ctl)
		quitRows(add, ctl)
		if Hud.iconState(ctl, m, now) == "red" then add(gap()); reportRows(add, ctl) end
		local st = windowState(ctl)
		if st == "hosting" or st == "joined" then
			add(gap())
			for _, p in ipairs(ctl.players or {}) do add(playerRow(p, ctl, false)) end
			requestRows(ctl, add, "primary")
			if m and C.ACTIVE then
				add(gap())
				for _, line in ipairs(Hud.chatLines(m, 3)) do add(text(line)) end
				chatInput(typed, add, "secondary")
			end
		end
		add(gap())
		add(button("Open window", function()
			if params and params.close then params.close() end
			openWindow()
		end, (st == "hosting" or st == "joined") and "secondary" or "primary"))
		-- a layout itself: PopoverWindowContent refuses a Component ("Recipe child must be a layout", real game 2026-10-21),
		-- around the same padded column as the window's
		return box(V, { fixedColumn(POP_W, rows) })
	end)

	-- ---- the window
	local Window = react.RegisterWrapperRecipe("FeverLink3Window", builtin.Window, function(wp)
		useLive(true)
		wrapChars = WINDOW_CHARS
		local typed = react.useState("")
		local nameIn = react.useState("")
		local codeIn = react.useState("")
		local steamIn = react.useState("")
		local mbIn = react.useState(32)
		local modeIn = react.useState("reliable")
		local showTest = react.useState(false)
		local tab = react.useState("players")
		local ctl = U.ctl
		local m = U.model
		local rows = {}
		local function add(node) rows[#rows + 1] = node end
		local function myName() return nameIn:old() ~= "" and nameIn:old() or tostring(ctl and ctl.name or "") end
		local state = windowState(ctl)
		-- every pending invite shows its Join row: one that waits for a world to start by itself (auto) used to be hidden, so a
		-- friend whose auto-start could not run saw nothing (friend test 2026-10-21)
		local inviteJoin = ctl and type(ctl.invite) == "table"

		-- the troubleshooting tools, folded away (the Steam test shows the player's Steam ID)
		local function troubleshooting()
			local st = ctl.steam
			reportRows(add, ctl)
			add(gap())
			-- the automatic update's switch (stored by the module in %LOCALAPPDATA%\FeverLink3\settings.txt, default on)
			local T, u = texts(), ctl.update
			if T and type(u) == "table" then
				local on = u.auto ~= false
				add(button(on and T.UPDATE.on or T.UPDATE.off, function() command("autoupdate", { on = not on }) end))
				add(text(u.official == false and T.UPDATE.dev or T.UPDATE.explain))
				if ctl.result and ctl.result.kind == "autoupdate" and ctl.result.ok == false then add(text(tostring(ctl.result.message), "warn")) end
				add(gap())
			end
			add(text("Steam connection test", nil, nil, "font-scale-title-3"))
			if st and st.id then
				add(text("Your Steam ID (a friend pastes it below):"))
				add(field("your Steam ID", tostring(st.id), function() end, true))
			else
				add(text("Steam: " .. tostring(st and st.state or "unknown")))
			end
			add(field("the other player's Steam ID", steamIn:old(), function(v) steamIn:set(v) end))
			add(button("Test the connection", function() command("steamtest", { id = steamIn:old() }) end))
			add(gap())
			-- Speed test: how long would the save of a synced game take over Steam? Both players keep this window open.
			add(text("Speed test (data sent each way; a synced save is about 80 MB)", nil, nil, "font-scale-title-3"))
			local sizes = {}
			for _, mb in ipairs(C.SPEED_CHOICES) do
				local chosen = (C.speedChoice(mbIn:old()) == mb)
				sizes[#sizes + 1] = button((chosen and "[ " or "") .. mb .. " MB" .. (chosen and " ]" or ""), function() mbIn:set(mb) end)
			end
			add(box(H, sizes))
			local modes = {}
			for _, mode in ipairs(C.SPEED_MODES) do
				local chosen = (C.speedMode(modeIn:old()) == mode)
				local label = C.SPEED_MODE_LABELS[mode]
				modes[#modes + 1] = button((chosen and "[ " or "") .. label .. (chosen and " ]" or ""), function() modeIn:set(mode) end)
			end
			add(box(H, modes))
			add(button("Run the speed test", function() command("steamtest", C.speedParams(steamIn:old(), mbIn:old(), modeIn:old())) end))
			if st and st.speed then
				for _, line in ipairs(C.speedLines(st.speed)) do add(text(line)) end
			end
			if st and st.note then add(text(tostring(st.note))) end
			if st and st.test then
				local t = st.test
				local line = "Test: " .. tostring(t.state) .. ", answers " .. tostring(t.pongs)
				if t.rtt then line = line .. ", last round trip " .. tostring(t.rtt) .. " ms" end
				if t.route and t.route ~= "" then line = line .. ", " .. tostring(t.route) end
				add(text(line))
			end
			if st and st.pings and st.pings > 0 then add(text("Pings received from other players: " .. tostring(st.pings))) end
		end

		local function playersTab(host)
			local list = {}
			for _, p in ipairs(ctl.players or {}) do list[#list + 1] = playerRow(p, ctl, host) end
			if host and ctl.result and ctl.result.kind == "kick" and ctl.result.message then list[#list + 1] = text(tostring(ctl.result.message)) end
			list[#list + 1] = gap()
			if host and not ctl.started then
				list[#list + 1] = button("Start the session", function() command("start", {}) end, "primary")
			elseif not host then
				list[#list + 1] = button("Leave the session", function() command("leave", {}) end, "negative")
			end
			-- at the bottom, while the icon is green too: a player reports what it saw (the pop-up has it only while red)
			list[#list + 1] = gap()
			reportRows(function(node) list[#list + 1] = node end, ctl)
			return column(list)
		end

		local function chatTab()
			local list = {}
			if m and C.ACTIVE then
				for _, line in ipairs(Hud.chatLines(m, 10)) do list[#list + 1] = text(line) end
				chatInput(typed, function(node) list[#list + 1] = node end, "primary")
			else
				list[#list + 1] = text("The chat opens once the session runs in this game.")
			end
			-- the bridge's system messages, apart from what players wrote
			local notices = Hud.notices(m)
			if #notices > 0 then
				list[#list + 1] = gap()
				list[#list + 1] = text("Events", nil, nil, "font-scale-title-3")
				for i = math.max(1, #notices - 9), #notices do list[#list + 1] = text(notices[i].text, notices[i].level) end
			end
			return column(list)
		end

		local function hostTab()
			local list = {}
			local function put(node) list[#list + 1] = node end
			-- the game's text fields cannot copy, so the button puts the code on the clipboard (done by the module)
			put(text("Join code"))
			put(field("join code", tostring(ctl.code or ""), function() end, true))
			local tools = { button("Copy code", function() command("copy", {}) end) }
			-- Steam invites: Steam's own invite window (Shift+Tab overlay); friends who accept join without a code
			if type(ctl.invites) == "table" then
				tools[#tools + 1] = text("  ")
				tools[#tools + 1] = button("Invite friends (Steam)", function() command("invite", {}) end)
			end
			put(box(H, tools))
			if ctl.result and (ctl.result.kind == "copy" or ctl.result.kind == "invite") then
				local copied = ctl.result.kind == "copy" and ctl.result.ok ~= false
				put(text(copied and "Copied. Paste it into a message (Ctrl+V)." or tostring(ctl.result.message or "")))
			end
			if ctl.password then put(text("They also need the password.")) end
			put(gap())
			if ctl.started then
				put(button("Sync everyone to my world", function() command("resync", {}) end))
			end
			put(button("Stop the session", function() command("leave", {}) end, "negative"))
			return column(list)
		end

		if state == "launcher" then
			add(text("This game was started by the FeverLink3 launcher (developer mode); the session is managed there."))
		elseif state == "missing" then
			add(heading("FeverLink3 is not running", "error"))
			add(text("winhttp.dll is not next to TransportFever3.exe. Close the game, copy it there (see HOW-TO-PLAY.txt) and start the game from Steam."))
		elseif state == "starting" then
			add(text("Starting the multiplayer module..."))
		elseif state == "idle" or state == "failed" then
			add(heading("Play together"))
			updateRow(add, ctl)
			quitRows(add, ctl)
			if ctl.notice then
				-- how the last session ended by itself: the world was closed, the host ended it or removed this player
				add(text(tostring(ctl.notice), state == "failed" and "error" or "warn"))
			elseif state == "failed" then
				add(text("Could not connect: " .. tostring(ctl.message or "unknown problem"), "error"))
			end
			add(gap())
			add(text("Your name"))
			add(field("name", myName(), function(v) nameIn:set(v) end))
			add(gap())
			if mustRestart(ctl) then
				-- the files on disk are another version than this game runs: no Host or Join until the restart
				add(text(texts() and texts().UPDATE.restart or "Restart the game before you play together.", "warn"))
			else
				add(button("Host a game", function() command("host", withMods({ name = myName() })) end, inviteJoin and "secondary" or "primary"))
				add(gap())
				add(text("Or join with a code"))
				add(box(H, {
					field("paste the join code here", codeIn:old(), function(v) codeIn:set(v) end),
					text("  "),
					button("Join", function() command("join", withMods({ name = myName(), code = codeIn:old() })) end),
				}))
			end
			add(gap())
			add(button(showTest:old() and "Hide troubleshooting" or "Troubleshooting", function() showTest:set(not showTest:old()) end))
			if showTest:old() then troubleshooting() end
		elseif state == "joining" then
			-- a dropped link: the controller retries and the host catches this game up within its grace (no reload)
			add(heading(Hud.statusLine(ctl, m), "warn"))
			add(text(tostring(ctl.progress or "contacting the host")))
			add(gap())
			add(button("Cancel", function() command("leave", {}) end, "negative"))
		elseif state == "hosting" or state == "joined" then
			local host = state == "hosting"
			local sync = Hud.syncInfo(ctl.sync, host)
			if sync then
				-- one heading, a progress bar (when the step has a count) and the sentence of the step
				add(heading("Syncing: " .. sync.step))
				if sync.fraction then
					add(builtin.ProgressBar{
						meta = { styleSheet = style("s:bar", { size = { WIDTH - 2 * PAD, 20 } }) },
						value = sync.fraction, label = math.floor(sync.fraction * 100) .. "%",
					})
				end
				if sync.detail ~= "" then add(text(sync.detail)) end
				if sync.after then add(text(sync.after, "warn")) end
			else
				local colour = Hud.iconState(ctl, m, nowTime())
				add(text(Hud.statusLine(ctl, m), colour == "red" and "error" or (colour == "yellow" and "warn" or nil), nil, "font-scale-title-3"))
			end
			requestRows(ctl, add, "secondary")
			add(gap())
			local tabs = {
				builtin.TabWidgetChild{ localKey = "players", value = "players", indicator = text("Players"), item = playersTab(host) },
				builtin.TabWidgetChild{ localKey = "chat", value = "chat", indicator = text("Chat"), item = chatTab() },
			}
			if host then
				tabs[#tabs + 1] = builtin.TabWidgetChild{ localKey = "host", value = "host", indicator = text("Host"), item = hostTab() }
			end
			local current = tab:old()
			if current == "host" and not host then current = "players" end
			add(builtin.TabWidget{ tabs = tabs, value = current, onValueChange = function(v) if v then tab:set(v) end end })
		end
		-- the mods: only when something differs from the host's game (a refused join, or the session's warnings)
		local modLines = Hud.modLines(ctl)
		if #modLines > 0 then
			add(gap())
			add(text("Mods (they must match the host's world):", "warn"))
			for _, line in ipairs(modLines) do add(text("   " .. line)) end
		end
		-- an invite Steam handed to this game (a friend's "Join game" or an accepted invite) that did not start by itself:
		-- this game is in a session (Join leaves it first), or it is free and waits for a loaded world (auto). TF3 has no
		-- documented call to load a save from the main menu, so an invite accepted there waits until any world is loaded
		-- (UNPROVEN in a two-PC Steam run); Join starts it now.
		if inviteJoin then
			local who = tostring(ctl.invite.host or "A friend")
			add(gap())
			add(text(ctl.invite.auto and (who .. " invited you. Load any world and you join automatically")
				or (who .. " invited you through Steam"), "warn"))
			add(box(H, {
				button("Join", function() command("accept_invite", {}) end, "primary"),
				text("  "),
				button("Not now", function() command("dismiss_invite", {}) end),
			}))
		end
		-- developer test button (FEVERLINK3_DEV=1 only): runs dev_click.lua from inside a click, where commands may be sent.
		-- The first row, so that its place in the window does not depend on what the window shows.
		if Hook and Hook.dev then
			table.insert(rows, 1, button("DEV run", function() Hook.devClick() end))
		end
		return builtin.Window{
			title = "FeverLink3 Multiplayer",
			id = "window.feverlink3",
			initialX = 0.3,
			initialY = 0.22,
			movable = true,
			closable = true,
			onClose = wp.onClose,
			content = fixedColumn(WIDTH, rows),
		}
	end)

	openWindow = function()
		local api = windowApi()
		if not api then log("no window api"); return end
		local okO, err = pcall(function()
			api.addSingletonWindow(Window, { onClose = function() api.removeAllWindows(Window) end })
		end)
		if not okO then log("cannot open the window: " .. tostring(err)) end
	end

	-- the pop-up: popover_react_util.PopoverWindow in the window container, as TogglePopoverButton does it
	-- (gui/main/popover_react_util.tl); our own button so that the icon can have the bar's size and change its colour.
	-- It closes on a click outside; a click on the icon then must not open it again at once.
	local function closePopover()
		local api = windowApi()
		if api and X.popover then pcall(api.removeAllWindows, X.popover.PopoverWindow) end
		U.popoverOpen = false
		U.popoverClosedAt = C.clock()
	end
	local function togglePopover(selfRef)
		local api = windowApi()
		if not api or not X.popover then openWindow(); return end   -- the documented fallback: the window
		if U.popoverOpen then closePopover(); return end
		if U.popoverClosedAt and C.clock() - U.popoverClosedAt < 0.3 then return end
		local x, y = nil, nil
		local okP, errP = pcall(function()
			-- the icon's top centre: getPosition(gravityX, gravityY) answers in fractions of the screen (0.58, 0.99 in the real
			-- game 2026-10-21), the same units as a window's initialX/initialY; the pop-up gets that point as its x, y
			local pos = selfRef:get():getPosition(0.5, 0.0)
			x, y = pos.x or pos[1], pos.y or pos[2]
			log("pop-up: icon at " .. tostring(x) .. "," .. tostring(y))
		end)
		if not okP then log("pop-up: no icon position: " .. tostring(errP)) end
		local okA, err = pcall(function()
			api.removeAllWindows(X.popover.PopoverWindow)
			api.addWindow(X.popover.PopoverWindow, "feverlink3", {
				onClose = closePopover, x = x, y = y, windowTitle = "FeverLink3",
				recipe = Popover, params = { close = closePopover },
			})
		end)
		if okA then U.popoverOpen = true else log("cannot open the pop-up: " .. tostring(err)); openWindow() end
	end

	-- the bar plugin: one round icon in the bar's colour of the session state, the status line as its tooltip
	return function()
		-- Guarded: if this recipe cannot hold state in this game build, the icon stays as it was drawn.
		local okT, tick = pcall(react.useState, "")
		if not okT then tick = nil end
		local okR, selfRef = pcall(react.useSelfRef)
		if not okR then selfRef = nil end
		if tick then
			pcall(function()
				react.onStepTimer(function()
					-- the bar is always there, the window not: held window commands get their stamp applies here too
					if Hook and Hook.poll then pcall(Hook.poll) end
					U.refresh()
					local ctl = U.ctl
					local key = Hud.iconState(ctl, U.model, nowTime()) .. "|" .. Hud.statusLine(ctl, U.model)
					if key ~= tick:old() then tick:set(key) end
					-- a sync round beginning, a new join request (host) or a Steam invite (friend) opens the window
					local syncing = ctl ~= nil and ctl.sync ~= nil
					if syncing and not U.syncShown then pcall(openWindow) end
					U.syncShown = syncing
					local asks = Hud.askCount(ctl)
					if asks > (U.asksShown or 0) then pcall(openWindow) end
					U.asksShown = asks
				end, 0.5)
			end)
		end
		local colour = Hud.iconState(U.ctl, U.model, nowTime())
		local line = Hud.statusLine(U.ctl, U.model)
		-- The click, as the game's own CheckBox and ToggleButton take theirs (gui/main/builtin.lua): react.onMouseEvent on this
		-- recipe, a left press over the node, the toggle on its release inside it (selfRef:containsPosition). Documented in
		-- tf3-docs reference base/scripts/react (onMouseEvent, containsPosition) and api/gui (Gui.Mouse.Event: button 0 = left,
		-- Type.Pressed "over the component", Type.Released). No Button: builtin.css.lua pads every Button and ToggleButton 8 px
		-- a side, more than the info row holds (real game 2026-10-21: the icon drew at 8 px or squashed at every size we gave).
		if selfRef then
			local function onMouse(evt)
				if evt.button ~= 0 then return false end
				local T = api.gui.mouse.Event.Type
				local inside = selfRef:get():containsPosition(api.type.Vec2i.new(evt.x, evt.y))
				if evt.type == T.Pressed then
					U.iconPressed = inside
					return inside
				elseif evt.type == T.Released then
					local was = U.iconPressed
					U.iconPressed = false
					if was and inside then togglePopover(selfRef) end
					return was == true
				elseif evt.type == T.Clicked then
					return inside
				end
				return false
			end
			-- an error in here must not reach the game's input handling: unhandled, the event goes on to the game
			pcall(react.onMouseEvent, function(evt)
				local ok, handled = pcall(onMouse, evt)
				return ok and handled == true
			end)
			pcall(react.setMouseTransparent, false)
		end
		-- The structure of the game's own info-row plugin (gui/game_bar/game_bar_display_transported_plugin: a Component with
		-- the tooltip around a plain ImageView, no size style): the image draws at its own size like the row's people and cargo
		-- symbols. Those are 48 x 48 files under an @2x name only (construction/menu/symbol_person@2x.tga, glyph 36 px inside),
		-- asked for without the @2x; ours are the same, a 36 px disc in 48 (@2x) and 18 in 24 (1x). UNPROVEN in the real game.
		return builtin.BoxLayout{
			orientation = H,
			children = {
				builtin.Component{
					meta = { tooltip = "FeverLink3: " .. line },
					layout = builtin.BoxLayout{
						orientation = H,
						children = { builtin.ImageView{ path = MOD .. "feverlink3_bar_" .. colour .. ".tga" } },
					},
				},
			},
		}
	end
end

do
	local okR, react = pcall(ug_require, "::/gui/main/react.lua")
	local okB, builtin = pcall(ug_require, "::/gui/main/builtin.lua")
	local okE, entry = pcall(ug_require, "::/gui/game_bar/game_bar_widgets.tl")
	-- optional parts: without them the window still opens (game default look, no pop-up)
	local X = {}
	local okS, su = pcall(ug_require, "::/gui/main/styleutil.tl")
	if okS then X.styleutil = su end
	local okP, pop = pcall(ug_require, "::/gui/main/popover_react_util.tl")
	if okP then X.popover = pop end
	local okG, globals = pcall(ug_require, "::/gui/main/game_react_globals.tl")
	if okG then X.globals = globals end
	print("[feverlink3] ui: active=" .. tostring(C.ACTIVE) .. " dir=" .. tostring(C.DIR) .. " id=" .. tostring(C.PLAYER_ID) .. " io=" .. tostring(C.IO ~= nil) .. " react=" .. tostring(okR) .. " builtin=" .. tostring(okB) .. " entry=" .. tostring(okE)
		.. " styleutil=" .. tostring(okS) .. " popover=" .. tostring(okP) .. " globals=" .. tostring(okG))
	-- which official replacements of the link module's io/os uses the window's state offers (observation only, lab)
	pcall(function() C.logger("ui")("official api probe (window state): " .. C.officialProbe()) end)
	pcall(function() C.logger("ui")("app keys (window): " .. C.appKeys()) end)
	-- once per load: can this state read the mod repository, and what each step returns (lab L3 2026-10-19 saw no mod list)
	pcall(function() C.logger("ui")("mod list (window): " .. Hud.modShape(app)) end)
	if Hook and C.DIR and C.IO then
		local okI, errI = pcall(Hook.install)
		print("[feverlink3] UI hook: " .. (okI and (errI and "installed" or "not installed") or ("failed: " .. tostring(errI))))
	end
	if okR and okB and okE and entry and entry.GameBarInfoDisplayExtension then
		local okU, recipeFn = pcall(function()
			if C.DIR and C.IO then return setup(react, builtin, X) end
			-- not started by the launcher: an empty, invisible entry so the game has a recipe to mount
			return function() return builtin.BoxLayout { children = {} } end
		end)
		if okU then
			local okQ, recipe = pcall(react.RegisterPluginRecipe, entry.GameBarInfoDisplayExtension, "FeverLink3Bar", recipeFn)
			if okQ then M.FeverLink3Bar = recipe else print("[feverlink3] entry registration failed: " .. tostring(recipe)) end
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
