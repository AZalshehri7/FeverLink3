-- FeverLink3 in-game window model: the pure parts. Player roster, bounded chat history, status text, and the two
-- small file hand-offs between the bridge (which owns the file link) and the window (which lives in the UI):
--   hud_state.txt   bridge -> window: a versioned snapshot, rewritten when something changed
--   ui_out.log      window -> bridge: appended records "seq <TAB> chat <TAB> 0 <TAB> {text=...}", one writer only
-- No engine calls here; the drawing is in feverlink3_ui.lua. Everything in this file is tested by
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

function H.setSession(h, s)
	local started = s.started == true
	local speed = tonumber(s.speed) or 0
	local pauseAt = tonumber(s.pauseAt)
	local cur = h.session
	if cur.started ~= started or cur.speed ~= speed or cur.pauseAt ~= pauseAt then
		h.session = { started = started, speed = speed, pauseAt = pauseAt }
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
		session = { started = h.session.started, speed = h.session.speed, pauseAt = h.session.pauseAt },
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
		h.session = { started = snap.session.started == true, speed = tonumber(snap.session.speed) or 0, pauseAt = tonumber(snap.session.pauseAt) }
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

-- The window's writer for ui_out.log. Records are `chat` with {text}; the sequence is the writer's own.
function H.newOutbox(C)
	return C.newWriter(C.path(H.OUTBOX), 0)
end

-- Returns true when the text was queued for the bridge.
function H.sendChat(C, outbox, text)
	text = H.cleanText(text)
	if not text then return false end
	return (C.writerSend(outbox, "chat", { text = text }))
end

return H
