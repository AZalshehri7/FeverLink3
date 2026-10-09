-- FeverLink3 in-game window: status, player list and chat.
--
-- ENGINE-BOUND AND UNTESTED. Everything with a rule in it (roster, history, status text, the snapshot and outbox
-- files) is in feverlink3_hud.lua and is tested; this file only draws that model with the game's react toolkit and
-- turns a typed line into an outbox record. It has never run in a game: the names below come from MPFever (MIT,
-- mpfever_ui.script.lua and mpfever_menu.lua) and are marked where they are assumptions. If any of it is missing
-- the window is simply not registered, the rest of the mod is unaffected.
--
-- Registered through feverlink3_entry.res.lua (react-plugin on ModEntryPointExtension; finding from MPFever (MIT):
-- the plugin is mounted once for the whole UI session). The window talks to the bridge through two files in the
-- session folder, never directly: the bridge owns the file link.

local okC, C = pcall(ug_require, "feverlink3_common.lua")
if not okC then C = ug_require("feverlink3_1::/feverlink3_common.lua") end
local okH, Hud = pcall(ug_require, "feverlink3_hud.lua")
if not okH then Hud = ug_require("feverlink3_1::/feverlink3_hud.lua") end

local log = C.logger("ui")
local M = {}
local U = { model = nil, outbox = nil, typed = "" }

local function poll()
	local seen = U.model and U.model.version or nil
	local m = Hud.readSnapshot(C, seen)
	if m then U.model = m; return true end
	return false
end

local function send(text)
	if not U.outbox then U.outbox = Hud.newOutbox(C) end
	return Hud.sendChat(C, U.outbox, text)
end

-- ---- drawing (untested)

local function setup(react, builtin)
	local V = builtin.type.Orientation.Vertical
	local function text(s) return builtin.TextView{ meta = { class = "font-scale-title-4" }, text = s } end
	local function column(children) return builtin.Component{ layout = builtin.BoxLayout{ orientation = V, children = children } } end

	local Window = react.RegisterWrapperRecipe("FeverLink3Window", builtin.Window, function(wp)
		local tick = react.useState(0)
		local typed = react.useState("")
		react.onStepTimer(function() if poll() then tick:set(tick:old() + 1) end end, 0.5)   -- assumption: onStepTimer(fn, seconds), as in MPFever
		local m = U.model
		local rows = {}
		if m then
			rows[#rows + 1] = text(Hud.statusText(m))
			rows[#rows + 1] = text("Players: " .. Hud.rosterText(m))
			for _, line in ipairs(Hud.chatLines(m, 15)) do rows[#rows + 1] = text(line) end
		else
			rows[#rows + 1] = text("Waiting for the FeverLink3 launcher...")
		end
		rows[#rows + 1] = builtin.TextInputField{
			meta = { class = "font-scale-title-4" },
			placeholderText = "Type a message",
			value = typed:old(),
			onTyping = function(v) typed:set(v) end,
		}
		rows[#rows + 1] = builtin.Button{
			meta = { class = "secondary" },
			content = text("Send"),
			onClick = function()
				if send(typed:old()) then typed:set("") end
			end,
		}
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

	-- the entry point: a small button that opens the window; the windowContainer path is the one MPFever's menu
	-- uses on the main page (assumption: the entry plugin's params carry it too)
	return function(params)
		local function open()
			local wc = params.commonParams.windowContainer:get():getApi()
			wc.addSingletonWindow(Window, { onClose = function() params.commonParams.windowContainer:get():getApi().removeAllWindows(Window) end })
		end
		return builtin.FloatingLayout{
			children = {
				builtin.FloatingLayoutChild{
					h = 0.99, v = 0.12,
					item = builtin.Button{ meta = { class = "secondary" }, content = text("FeverLink3"), onClick = open },
				},
			},
		}
	end
end

do
	local okR, react = pcall(ug_require, "::/gui/main/react.lua")
	local okB, builtin = pcall(ug_require, "::/gui/main/builtin.lua")
	local okE, entry = pcall(ug_require, "::/gui/main/mod_entry_point.tl")
	print("[feverlink3] ui: active=" .. tostring(C.ACTIVE) .. " dir=" .. tostring(C.DIR) .. " id=" .. tostring(C.PLAYER_ID) .. " io=" .. tostring(C.IO ~= nil) .. " react=" .. tostring(okR) .. " builtin=" .. tostring(okB) .. " entry=" .. tostring(okE))
	if okR and okB and okE and entry and entry.ModEntryPointExtension then
		local okS, recipeFn = pcall(function()
			if C.ACTIVE then return setup(react, builtin) end
			-- not started by the launcher: an empty, invisible entry so the game has a recipe to mount
			return function() return builtin.BoxLayout { children = {} } end
		end)
		if okS then
			local okP, recipe = pcall(react.RegisterPluginRecipe, entry.ModEntryPointExtension, "FeverLink3Entry", recipeFn)
			if okP then M.FeverLink3Entry = recipe else print("[feverlink3] entry registration failed: " .. tostring(recipe)) end
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
