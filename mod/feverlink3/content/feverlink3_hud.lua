-- FeverLink3 in-game window model: the pure parts. Player roster, bounded chat history, status text, and the two
-- small file hand-offs between the bridge (which owns the file link) and the window (which lives in the UI):
--   hud_state.txt   bridge -> window: a versioned snapshot, rewritten when something changed
--   ui_out.log      window -> bridge: appended records "seq <TAB> chat <TAB> 0 <TAB> {text=...}", one writer only
-- No engine calls here; the drawing is in feverlink3_ui.script.lua. Everything in this file is tested by
-- tests/lua/test_lockstep.py.

local H = {}

H.HISTORY_MAX = 200
H.TEXT_MAX = 500
H.SNAPSHOT = "hud_state.txt"
H.OUTBOX = "ui_out.log"

-- ------------------------------------------------------------------ model

function H.new(selfId, selfName)
	local h = {
		selfId = selfId, selfName = selfName,
		roster = {},                                    -- id -> { id, name, ready }
		history = {},                                   -- oldest first, at most HISTORY_MAX
		session = { started = false, speed = 0 },
		version = 0,
	}
	h.roster[selfId] = { id = selfId, name = selfName, ready = false }
	return h
end

local function touch(h) h.version = h.version + 1 end

local function putPlayer(h, p)
	if type(p) ~= "table" or type(p.id) ~= "number" then return false end
	local name = type(p.name) == "string" and p.name or ("player " .. tostring(p.id))
	h.roster[p.id] = { id = p.id, name = name, ready = p.ready == true }
	return true
end

-- `welcome` {you, hostName, version, players: [{id,name,ready}], session}: the roster as the host knows it.
function H.applyWelcome(h, p)
	if type(p) ~= "table" then return end
	h.roster = {}
	h.roster[h.selfId] = { id = h.selfId, name = h.selfName, ready = false }
	if type(p.hostName) == "string" and h.selfId ~= 0 then h.roster[0] = { id = 0, name = p.hostName, ready = true } end
	for _, q in ipairs(type(p.players) == "table" and p.players or {}) do putPlayer(h, q) end
	touch(h)
end

-- `player_joined` {player: {id,name,ready}}
function H.playerJoined(h, p)
	if type(p) == "table" and putPlayer(h, p.player) then touch(h) end
end

-- `player_left` {id, name}
function H.playerLeft(h, p)
	if type(p) ~= "table" or type(p.id) ~= "number" or p.id == h.selfId then return end
	if h.roster[p.id] then
		h.roster[p.id] = nil
		touch(h)
	end
end

-- The stalled games (core SessionState::stalled, host/stall.rs) as a sorted list of ids, and its text for comparison.
local function stalledList(s)
	local list = {}
	for _, id in ipairs(type(s.stalled) == "table" and s.stalled or {}) do
		if type(id) == "number" then list[#list + 1] = id end
	end
	table.sort(list)
	return list, table.concat(list, ",")
end

function H.setSession(h, s)
	local started = s.started == true
	local speed = tonumber(s.speed) or 0
	local pauseAt = tonumber(s.pauseAt)
	local stalled, key = stalledList(s)
	local cur = h.session
	if cur.started ~= started or cur.speed ~= speed or cur.pauseAt ~= pauseAt or key ~= select(2, stalledList(cur)) then
		h.session = { started = started, speed = speed, pauseAt = pauseAt, stalled = stalled, behind = cur.behind }
		touch(h)
	end
end

-- How many seconds of play this game is behind the others while it catches up (nil: not behind); whole seconds, so the window is
-- rewritten once a second at most.
function H.setBehind(h, seconds)
	local v = seconds and math.max(0, math.floor(seconds + 0.5)) or nil
	if h.session.behind ~= v then
		h.session.behind = v
		touch(h)
	end
end

-- Text typed by a player or received in a chat line: control characters out, trimmed, at most TEXT_MAX bytes.
-- Returns nil when nothing is left.
function H.cleanText(text)
	if type(text) ~= "string" then return nil end
	text = text:gsub("[%c]", " "):gsub("^%s+", ""):gsub("%s+$", "")
	if #text > H.TEXT_MAX then text = text:sub(1, H.TEXT_MAX) end
	if #text == 0 then return nil end
	return text
end

-- Appends to the bounded history. Returns true when a line was added.
function H.addChat(h, name, text, from)
	text = H.cleanText(text)
	if not text then return false end
	h.history[#h.history + 1] = { name = type(name) == "string" and name or ("player " .. tostring(from)), text = text, from = from }
	while #h.history > H.HISTORY_MAX do table.remove(h.history, 1) end
	touch(h)
	return true
end

-- ------------------------------------------------------------------ text

function H.rosterList(h)
	local list = {}
	for _, p in pairs(h.roster) do list[#list + 1] = p end
	table.sort(list, function(a, b) return a.id < b.id end)
	return list
end

-- "Waiting for the host to start" / "Paused" / "Running x2", plus the player count.
function H.statusText(h)
	local s = h.session
	local what
	if not s.started then
		what = "Waiting for the host to start"
	elseif s.speed <= 0 or s.pauseAt ~= nil then
		what = "Paused"
	else
		what = "Running x" .. string.format("%d", s.speed)
	end
	-- a stalled game (host/stall.rs): this one catches up; or another one stood still and this game runs on without waiting for it
	local mine, others = false, {}
	for _, id in ipairs(s.stalled or {}) do
		if id == h.selfId then
			mine = true
		else
			local p = h.roster[id]
			others[#others + 1] = p and p.name or ("player " .. tostring(id))
		end
	end
	if s.started and mine then
		what = what .. ", catching up" .. (s.behind and s.behind > 0 and (" (" .. s.behind .. " s behind)") or "")
	elseif s.started and #others > 0 then
		what = what .. ", " .. table.concat(others, ", ") .. " stood still: running on, " .. (#others == 1 and "it catches" or "they catch") .. " up"
	end
	local n = 0
	for _ in pairs(h.roster) do n = n + 1 end
	return what .. " - " .. n .. (n == 1 and " player" or " players")
end

function H.rosterText(h)
	local names = {}
	for _, p in ipairs(H.rosterList(h)) do
		names[#names + 1] = p.name .. (p.id == h.selfId and " (you)" or "") .. (p.id == 0 and " [host]" or "")
	end
	return table.concat(names, ", ")
end

-- The last `count` chat lines as "name: text".
function H.chatLines(h, count)
	local out = {}
	for i = math.max(1, #h.history - (count or 15) + 1), #h.history do
		local e = h.history[i]
		out[#out + 1] = e.name .. ": " .. e.text
	end
	return out
end

-- ------------------------------------------------------------------ snapshot hand-off (bridge -> window)

function H.snapshot(h)
	local roster = {}
	for i, p in ipairs(H.rosterList(h)) do roster[i] = { id = p.id, name = p.name, ready = p.ready } end
	local history = {}
	for i, e in ipairs(h.history) do history[i] = { name = e.name, text = e.text, from = e.from } end
	return {
		version = h.version, selfId = h.selfId, selfName = h.selfName,
		session = { started = h.session.started, speed = h.session.speed, pauseAt = h.session.pauseAt, stalled = h.session.stalled, behind = h.session.behind },
		roster = roster, history = history,
	}
end

-- A model from a snapshot (the window reads one; the bridge restores one after a savegame reload).
function H.fromSnapshot(snap)
	if type(snap) ~= "table" or type(snap.selfId) ~= "number" then return nil end
	local h = H.new(snap.selfId, type(snap.selfName) == "string" and snap.selfName or "")
	h.roster = {}
	for _, p in ipairs(type(snap.roster) == "table" and snap.roster or {}) do putPlayer(h, p) end
	if not h.roster[h.selfId] then h.roster[h.selfId] = { id = h.selfId, name = h.selfName, ready = false } end
	for _, e in ipairs(type(snap.history) == "table" and snap.history or {}) do
		if type(e) == "table" and type(e.text) == "string" then
			h.history[#h.history + 1] = { name = tostring(e.name), text = e.text, from = e.from }
		end
	end
	while #h.history > H.HISTORY_MAX do table.remove(h.history, 1) end
	if type(snap.session) == "table" then
		h.session = { started = snap.session.started == true, speed = tonumber(snap.session.speed) or 0, pauseAt = tonumber(snap.session.pauseAt),
			stalled = (stalledList(snap.session)), behind = tonumber(snap.session.behind) }
	end
	h.version = tonumber(snap.version) or 0
	return h
end

-- Rewrites the snapshot file when the model changed since `lastWritten` (a table { version }). Returns true when
-- it wrote.
function H.writeSnapshot(C, h, lastWritten)
	if lastWritten.version == h.version then return false end
	local ok, text = pcall(C.serialize, H.snapshot(h))
	if not ok then return false end
	if C.writeFile(C.path(H.SNAPSHOT), text) then
		lastWritten.version = h.version
		return true
	end
	return false
end

-- The window's side: the snapshot when its version differs from `seenVersion`, else nil. A file caught half
-- written does not parse and is simply read again on the next poll.
function H.readSnapshot(C, seenVersion)
	local text = C.readFile(C.path(H.SNAPSHOT))
	if not text or #text == 0 then return nil end
	local snap = C.parse(text)
	if type(snap) ~= "table" or snap.version == seenVersion then return nil end
	return H.fromSnapshot(snap)
end

-- ------------------------------------------------------------------ outbox hand-off (window -> bridge)

-- The window's writer for ui_out.log. Records are `chat` with {text}; the bridge reads them by position.
function H.newOutbox(C)
	return C.newWriter(C.path(H.OUTBOX), 0)
end

-- Returns true when the text was queued for the bridge.
function H.sendChat(C, outbox, text)
	text = H.cleanText(text)
	if not text then return false end
	return (C.writerSend(outbox, "chat", { text = text }))
end

-- ------------------------------------------------------------------ mod list (docs/PROTOCOL.md, "Mod list")

-- This game's installed mods for the `host` / `join` command: { {id, revision, cosmetic}, ... }, or nil and why when the
-- repository cannot be read (then a host compares nothing and a comparing host refuses the join). Documented API only:
-- app.getUserProfile():getModRep(), ModRep:getInstalledMods() (iterated with ipairs as in the docs' example),
-- ModRep:getModDesc(modId).revision / .cosmetic, Mod.ModId.name (tf3-docs reference/api/api/type, UserProfile and ModRep;
-- reference/api/api/type/mod.html). The calling convention is the base game's own (res/content/gui.zip):
-- gui/menu/main_menu.tl 710-723 `local modRep = app.getUserProfile():getModRep()` then `ipairs(modRep:getInstalledMods())`,
-- gui/menu/mod_selector_page.tl 86-90 `modRep:getModDesc(modId), modId.name`, and in the in-game interface
-- gui/main/script_param_util.tl 668 `app.getUserProfile():getModRep():getModDesc(modId)`; the tealdefs agree
-- (api/tealdef/app.d.tl 122 getUserProfile : function() : UserProfile, api/type.d.tl 3004-3006 and 3437: methods taking the
-- record as first argument, plain return values, no Async). The window's `app` holds its functions as callable tables
-- ("getUserProfile:table", app keys (window) line, lab L3 2026-10-19), as for app.saveGame. The API has no list of the
-- mods active in this world, so these are the installed ones.
--
-- `seen` (optional) collects the shape of each intermediate value; a failure names the step that failed.
local function shapeOf(v)
	local t = type(v)
	if t == "string" then return "string:" .. v:sub(1, 24) end
	if t == "number" or t == "boolean" then return t .. ":" .. tostring(v) end
	if t ~= "table" then return t end
	local keys, n = {}, 0
	for k in pairs(v) do
		n = n + 1
		if n <= 6 then keys[#keys + 1] = tostring(k) end
	end
	return string.format("table#%d/%d[%s]", #v, n, table.concat(keys, ","))
end

local function readMods(appApi, seen)
	local step = "app.getUserProfile"
	local function note(k, v)
		if seen then seen[#seen + 1] = k .. "=" .. shapeOf(v) end
		return v
	end
	local ok, res = pcall(function()
		note("getUserProfile", appApi.getUserProfile)
		step = "app.getUserProfile()"
		local profile = note("profile", appApi.getUserProfile())
		step = "profile:getModRep()"
		local rep = note("rep", profile:getModRep())
		step = "rep:getInstalledMods()"
		local ids = note("installed", rep:getInstalledMods())
		local out = {}
		for i, modId in ipairs(ids) do
			step = "installed[" .. i .. "].name"
			local name = modId.name
			step = "rep:getModDesc(installed[" .. i .. "])"
			local desc = rep:getModDesc(modId)
			if i == 1 then
				note("id1", modId); note("id1.name", name); note("desc1", desc)
				note("desc1.revision", desc.revision); note("desc1.cosmetic", desc.cosmetic)
			end
			out[#out + 1] = { id = tostring(name), revision = math.floor(tonumber(desc.revision) or 0), cosmetic = desc.cosmetic == true }
		end
		return out
	end)
	if ok then return res end
	return nil, step .. ": " .. tostring(res)
end

function H.modList(appApi)
	return readMods(appApi, nil)
end

-- One compact line for the log (once per load of the window, and the lab's mod_list probe): the count or the failing step
-- with its error, then the type of every value read on the way.
function H.modShape(appApi)
	local seen = {}
	local list, why = readMods(appApi, seen)
	local head = list and (#list .. " mods") or ("FAILED at " .. tostring(why))
	return head .. "; " .. table.concat(seen, " ")
end

-- The ids of the mods a saved world uses, for the host's check between the save and the transfer: nil and why when they
-- cannot be read. Documented API: app.findAllSavegames(app.SaveGameNamespace.getSavegame()) to find the save by name,
-- app.getSavegameInfo(id):get() (Async<SaveGameData>, "Block until the task is complete and then return its result"),
-- SaveGameData.info.metadata.gameModDescs[i].modId.name (tf3-docs reference/api/app.html, reference/api/api/type:
-- SaveGameData, GameMetadata, Async; Mod.GameModDesc on reference/api/api/type/mod.html).
function H.worldMods(appApi, name)
	local ok, ids = pcall(function()
		local ns = appApi.SaveGameNamespace.getSavegame()
		for _, s in ipairs(appApi.findAllSavegames(ns) or {}) do
			if s.saveName == name then
				local id = api.type.SavegameId.new()
				id.path = s.path
				id.saveGameName = s.saveName
				id.saveGameNamespace = ns
				local out = {}
				for _, d in ipairs(appApi.getSavegameInfo(id):get().info.metadata.gameModDescs) do
					out[#out + 1] = tostring(d.modId.name)
				end
				return out
			end
		end
		error("savegame '" .. tostring(name) .. "' not found")
	end)
	if ok then return ids end
	return nil, tostring(ids)
end

-- The window's mod lines: a join refused over mods (ctl.mods from the controller: { {block, text}, ... }, blocking ones
-- first as the controller sorts them) and the session's warnings (ctl.modWarnings: what could not be compared, cosmetic
-- differences).
function H.modLines(ctl)
	local out = {}
	if type(ctl) ~= "table" then return out end
	for _, m in ipairs(type(ctl.mods) == "table" and ctl.mods or {}) do
		if type(m) == "table" and m.text then
			out[#out + 1] = (m.block and "Must match: " or "Warning: ") .. tostring(m.text)
		end
	end
	for _, w in ipairs(type(ctl.modWarnings) == "table" and ctl.modWarnings or {}) do
		out[#out + 1] = "Warning: " .. tostring(w)
	end
	return out
end

-- A throttled ticker for the window's refresh. The step timer (react.onStepTimer) is not guaranteed to fire while the
-- game is paused, which left a guest's window saying "Connected" after the host had ended the session; the window now
-- also calls the ticker from react.onStep (every interface frame, which runs while paused). The ticker runs fn at most
-- once per `interval` seconds of the real clock (C.clock, feverlink3_link.lua), however many drivers call it, and returns fn's result
-- (nil when it skipped this call).
function H.newTicker(interval, clock, fn)
	local last
	return function()
		local now = clock()
		if last and now >= last and now - last < interval then return nil end
		last = now
		return fn()
	end
end

return H
