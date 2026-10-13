-- FeverLink3 common code: environment, deterministic serialisation, string hash, and the file link to the
-- launcher. Shared by both halves of the game script (simulation state and interface state). No engine calls
-- here: everything in this file runs anywhere Lua 5.2 runs, which is how the tests drive it.
--
-- File link (docs/ARCHITECTURE.md section 2, docs/PROTOCOL.md): two append-only text files in the session folder.
--   to_game.log    launcher -> mod
--   from_game.log  mod -> launcher
-- One record per line: seq <TAB> kind <TAB> from <TAB> payload. `seq` grows by one per line per file; a reader
-- remembers the last sequence it dispatched and never dispatches a lower one, so a truncation (or a re-write of
-- history) cannot replay old lines. `from` is the sender id the launcher attached (0 = host); the mod trusts the
-- column, not anything inside the payload. The payload is one line produced by C.serialize.

local C = {}

C.TAB = string.char(9)
C.NL = string.char(10)
C.CR = string.char(13)
C.BS = string.char(92)
C.QUOTE = string.char(34)
C.STEP = 200 -- game-time units per simulation step (docs/ARCHITECTURE.md section 5)

local TAB, NL, CR, BS, QUOTE = C.TAB, C.NL, C.CR, C.BS, C.QUOTE

-- ------------------------------------------------------------------ environment

C.DIR = os.getenv("FEVERLINK3_SESSION_DIR")
C.PLAYER_ID = tonumber(os.getenv("FEVERLINK3_PLAYER_ID") or "")
C.PLAYER_NAME = os.getenv("FEVERLINK3_PLAYER_NAME")
if C.PLAYER_NAME == nil or C.PLAYER_NAME == "" then C.PLAYER_NAME = "player " .. tostring(C.PLAYER_ID or "?") end
-- the game may run scripts with a reduced standard library: take io only if it is there
do
	local ok, v = pcall(function() return io end)
	if not (ok and type(v) == "table") then
		-- the game removes the io global but leaves the module loaded (finding from MPFever (MIT): package.loaded.io)
		ok, v = pcall(function() return package.loaded["io"] end)
	end
	C.IO = ok and type(v) == "table" and v or nil
end
-- without the session folder (game not started by FeverLink3.exe) the mod does nothing
C.LIVE = false -- the native module hosts the session itself: files live in a fixed folder and identity.txt says who we are
if C.DIR == nil or C.DIR == "" then
	-- FEVERLINK3_LIVE_DIR: a second game on the same PC (tests, recordings) keeps its files apart from the first's
	local override = os.getenv("FEVERLINK3_LIVE_DIR")
	local base = os.getenv("LOCALAPPDATA")
	if override ~= nil and override ~= "" then
		C.DIR = override
		C.LIVE = true
	elseif base ~= nil and base ~= "" then
		C.DIR = base .. BS .. "FeverLink3" .. BS .. "live"
		C.LIVE = true
	end
end
C.ACTIVE = (not C.LIVE) and C.DIR ~= nil and C.DIR ~= "" and C.PLAYER_ID ~= nil and C.IO ~= nil

local SEP = (C.DIR and C.DIR:find("/", 1, true) and not C.DIR:find(BS, 1, true)) and "/" or BS
function C.path(fileName)
	return C.DIR .. SEP .. fileName
end

C.TO_GAME = "to_game.log"
C.FROM_GAME = "from_game.log"
C.LOG = "mod.log"
C.CURSOR = "resync_cursor.txt" -- written before a resync load: the sequence the reloaded mod resumes after

-- ------------------------------------------------------------------ files and logging

function C.appendFile(path, text)
	if not C.IO then return false end
	local f = C.IO.open(path, "ab")
	if not f then return false end
	f:write(text)
	f:close()
	return true
end

-- Replaces the file's content (snapshots, cursors): not for the append-only link files.
function C.writeFile(path, text)
	if not C.IO then return false end
	local f = C.IO.open(path, "wb")
	if not f then return false end
	f:write(text)
	f:close()
	return true
end

function C.readFile(path)
	if not C.IO then return nil end
	local f = C.IO.open(path, "rb")
	if not f then return nil end
	local s = f:read("*a")
	f:close()
	return s
end

-- Live mode: identity.txt (id, name, role) is written by the native module; the mod becomes active when the role is
-- host or player, without a reload. Cheap to call every frame: the file is read at most once a second.
function C.poll(force)
	if not C.LIVE or C.ACTIVE then return C.ACTIVE end
	local now = os.clock()
	if not force and C.lastPoll and now - C.lastPoll < 1 then return false end
	C.lastPoll = now
	local text = C.readFile(C.path("identity.txt"))
	if not text then return false end
	local id = tonumber(text:match("id=(%d+)"))
	local name = text:match("name=([^" .. CR .. NL .. "]*)")
	local role = text:match("role=([%a]+)")
	if id and (role == "host" or role == "player") then
		C.PLAYER_ID = id
		C.PLAYER_NAME = (name ~= nil and name ~= "") and name or ("player " .. tostring(id))
		C.ACTIVE = C.IO ~= nil
		print("[feverlink3] session found: id=" .. tostring(id) .. " role=" .. role)
	end
	return C.ACTIVE
end
C.poll(true)
print("[feverlink3] common loaded: active=" .. tostring(C.ACTIVE) .. " live=" .. tostring(C.LIVE) .. " io=" .. tostring(C.IO ~= nil) .. " id=" .. tostring(C.PLAYER_ID))

-- a logger writing "<clock> [<tag>] <message>" lines to mod.log in the session folder (and to the game's own log)
function C.logger(tag)
	return function(msg)
		local line = "[FeverLink3 " .. tag .. "] " .. tostring(msg)
		pcall(function() if debugPrint then debugPrint(line) end end)
		if C.DIR and C.IO then
			pcall(C.appendFile, C.path(C.LOG), os.date("%H:%M:%S") .. " " .. line .. NL)
		end
	end
end

-- Trace log (FEVERLINK3_TRACE, DEFAULT ON in test builds, FEVERLINK3_TRACE=0 switches it off): everything a real run needs to be explained
-- from the logs alone, in trace.log of the session folder (docs/TRACE.md). One line per event: "<HH:MM:SS> +<cpu s> <state> <tag> <text>".
-- Built for per-build and per-round events, never per tick. The file rolls over to trace.2.log, trace.3.log ... every 8 MB (nothing is
-- deleted); after TRACE_MAX_SEGMENTS segments it stops and says so once.
C.TRACE = true
pcall(function() C.TRACE = os.getenv("FEVERLINK3_TRACE") ~= "0" end)
local TRACE_SEGMENT, TRACE_MAX_SEGMENTS = 8 * 1024 * 1024, 12
local traceState = { seg = 1, bytes = 0, stopped = false }
function C.tracer(state)
	return function(tag, text)
		if not C.TRACE or traceState.stopped or not (C.DIR and C.IO) then return end
		pcall(function()
			local line = string.format("%s +%.3f %s %s %s", os.date("%H:%M:%S"), os.clock(), state, tag, tostring(text))
			if #line > 200000 then line = line:sub(1, 200000) .. " ...(cut)" end
			if traceState.bytes > TRACE_SEGMENT then
				traceState.seg, traceState.bytes = traceState.seg + 1, 0
				if traceState.seg > TRACE_MAX_SEGMENTS then
					traceState.stopped = true
					C.appendFile(C.path("trace." .. TRACE_MAX_SEGMENTS .. ".log"), os.date("%H:%M:%S") .. " TRACE FULL: no more lines" .. NL)
					return
				end
			end
			local name = traceState.seg == 1 and "trace.log" or ("trace." .. traceState.seg .. ".log")
			C.appendFile(C.path(name), line .. NL)
			traceState.bytes = traceState.bytes + #line + 1
		end)
	end
end

-- ------------------------------------------------------------------ deterministic serialisation

-- Writes a value as one line of Lua literal syntax with a fixed order: keys sorted (numbers, then strings, then
-- booleans), integers as %d, other numbers as %.17g (round-trips every double), strings escaped. Two tables with
-- equal contents serialise to the same bytes in every Lua state, so the result is both the wire form and the
-- input of the checksum hash. Functions, userdata and threads raise an error: a command holding one cannot be
-- replicated and must stay native.

local ESCAPES = { [NL] = BS .. "n", [CR] = BS .. "r", [TAB] = BS .. "t", [QUOTE] = BS .. QUOTE, [BS] = BS .. BS }

local function escapeChar(c)
	return ESCAPES[c] or (BS .. string.format("%03d", string.byte(c)))
end

local function serializeString(s)
	return QUOTE .. s:gsub("[%c" .. QUOTE .. BS .. BS .. "]", escapeChar) .. QUOTE
end

local function serializeNumber(v)
	if v ~= v then return "(0/0)" end
	if v == math.huge then return "(1/0)" end
	if v == -math.huge then return "(-1/0)" end
	-- "%d" raises "not a number in proper range" above 2^31 in this game's Lua (found by the sweep: a balance of 5 billion made every
	-- update fail); "%.0f" prints the same digits for every integral double below 2^53 (zero is kept free of a minus sign).
	if math.floor(v) == v and math.abs(v) < 9007199254740992 then return v == 0 and "0" or string.format("%.0f", v) end
	return string.format("%.17g", v)
end

local TYPE_RANK = { number = 1, string = 2, boolean = 3 }

local function keyBefore(a, b)
	local ta, tb = type(a), type(b)
	if ta ~= tb then return TYPE_RANK[ta] < TYPE_RANK[tb] end
	if ta == "boolean" then return (not a) and b end
	return a < b
end

local function serialize(v, depth)
	local t = type(v)
	if t == "nil" then return "nil" end
	if t == "boolean" then return v and "true" or "false" end
	if t == "number" then return serializeNumber(v) end
	if t == "string" then return serializeString(v) end
	if t ~= "table" then error("cannot serialise a " .. t) end
	if depth > 48 then error("table too deep (cycle?)") end
	local keys = {}
	for k in pairs(v) do
		if TYPE_RANK[type(k)] == nil then error("cannot serialise a table keyed by " .. type(k)) end
		keys[#keys + 1] = k
	end
	table.sort(keys, keyBefore)
	local parts = {}
	for i, k in ipairs(keys) do
		parts[i] = "[" .. serialize(k, depth + 1) .. "]=" .. serialize(v[k], depth + 1)
	end
	return "{" .. table.concat(parts, ",") .. "}"
end

function C.serialize(v)
	return serialize(v, 0)
end

-- Parses what C.serialize wrote. The text is evaluated as a Lua expression in an empty environment, so it can
-- hold no function calls, no globals, no side effects. Returns the value, or nil and a reason.
function C.parse(s)
	if type(s) ~= "string" then return nil, "not a string" end
	local chunk, err = load("return " .. s, "=payload", "t", {})
	if not chunk then return nil, err end
	local ok, v = pcall(chunk)
	if not ok then return nil, v end
	return v
end

-- ------------------------------------------------------------------ hashing

local function xor8(a, b)
	local r, bit = 0, 1
	for _ = 1, 8 do
		local x, y = a % 2, b % 2
		if x ~= y then r = r + bit end
		a, b, bit = (a - x) / 2, (b - y) / 2, bit * 2
	end
	return r
end

-- FNV-1a, 32 bits, in double arithmetic (Lua 5.2 has no integers): h = (h xor byte) * 16777619 mod 2^32.
-- 16777619 = 2^24 + 403, so the product is split to stay exact below 2^53.
function C.fnv1a(s)
	local h = 2166136261
	for i = 1, #s do
		local lo = h % 256
		h = h - lo + xor8(lo, s:byte(i))
		h = ((h % 256) * 16777216 + h * 403) % 4294967296
	end
	return h
end

-- hash of any serialisable value, as 8 hex digits: what a checksum part carries
function C.hashValue(v)
	return string.format("%08x", C.fnv1a(C.serialize(v)))
end

-- ------------------------------------------------------------------ link records

-- seq <TAB> kind <TAB> from <TAB> payload
function C.formatLine(seq, kind, from, payload)
	return string.format("%d", seq) .. TAB .. kind .. TAB .. tostring(from) .. TAB .. payload .. NL
end

function C.parseLine(line)
	local seq, kind, from, payload = line:match("^(%d+)" .. TAB .. "([^" .. TAB .. "]+)" .. TAB .. "([^" .. TAB .. "]*)" .. TAB .. "(.*)$")
	if not seq then return nil end
	return { seq = tonumber(seq), kind = kind, from = tonumber(from) or from, payload = payload }
end

-- Reader: tracks the byte offset consumed and the last sequence dispatched.
function C.newReader(path)
	return { path = path, offset = 0, lastSeq = 0, partial = "", dropped = 0 }
end

-- Pure part of the reader: feeds a chunk of text, returns the complete records with a new sequence number.
-- A trailing line without newline waits for the rest. Records whose seq is not above lastSeq are dropped and
-- counted (history re-written after a truncation, duplicates).
function C.readerFeed(r, text)
	local out = {}
	local buf = r.partial .. text
	local pos = 1
	while true do
		local i = buf:find(NL, pos, true)
		if not i then break end
		local line = buf:sub(pos, i - 1)
		pos = i + 1
		if line:sub(-1) == CR then line = line:sub(1, -2) end
		if #line > 0 then
			local rec = C.parseLine(line)
			if rec then
				if rec.seq > r.lastSeq then
					r.lastSeq = rec.seq
					out[#out + 1] = rec
				else
					r.dropped = r.dropped + 1
				end
			end
		end
	end
	r.partial = buf:sub(pos)
	return out
end

-- Reads what the file gained since the last poll. A file shorter than the offset was truncated: reading restarts
-- from the beginning and the sequence filter in readerFeed keeps history from being dispatched again.
function C.readerPoll(r)
	if not C.IO then return {} end
	local f = C.IO.open(r.path, "rb")
	if not f then return {} end
	local size = f:seek("end") or 0
	if size < r.offset then
		r.offset = 0
		r.partial = ""
	end
	local out = {}
	if size > r.offset then
		f:seek("set", r.offset)
		local chunk = f:read(size - r.offset) or ""
		r.offset = r.offset + #chunk
		out = C.readerFeed(r, chunk)
	end
	f:close()
	return out
end

-- Writer: appends records with a sequence that continues after whatever the file already holds, so a reload of
-- the savegame (which restarts the script and its counters) does not restart the sequence seen by the launcher.
function C.lastSeqInFile(path)
	local s = C.readFile(path)
	if not s or #s == 0 then return 0 end
	local tail = s:sub(-4096)
	local last = 0
	for line in tail:gmatch("[^" .. NL .. "]+") do
		local seq = line:match("^(%d+)" .. TAB)
		if seq then last = tonumber(seq) end
	end
	return last
end

function C.newWriter(path, from)
	return { path = path, from = from, seq = C.lastSeqInFile(path), failed = 0 }
end

function C.writerSend(w, kind, payload)
	local ok, text = pcall(C.serialize, payload or {})
	if not ok then return false, text end
	w.seq = w.seq + 1
	if C.appendFile(w.path, C.formatLine(w.seq, kind, w.from, text)) then return true end
	w.failed = w.failed + 1
	return false, "cannot append to " .. w.path
end

-- ------------------------------------------------------------------ plain values

-- Deep copy of a plain value (tables of numbers, strings, booleans). Anything else raises: such a command cannot
-- be replicated and must stay native (docs/ARCHITECTURE.md section 6).
function C.plain(v, depth)
	depth = depth or 0
	local t = type(v)
	if t == "number" or t == "string" or t == "boolean" or t == "nil" then return v end
	if t ~= "table" then error("value of type " .. t .. " cannot be replicated") end
	if depth > 48 then error("table too deep") end
	local out = {}
	for k, x in pairs(v) do out[C.plain(k, depth + 1)] = C.plain(x, depth + 1) end
	return out
end

-- ------------------------------------------------------------------ Steam speed test (the window's text)

-- The sizes the window offers, in MB. 80 is about the size of the save that has to cross when a game is synced.
C.SPEED_CHOICES = { 8, 32, 80 }

-- The size to test: a number the window offers, else 32.
function C.speedChoice(mb)
	for _, choice in ipairs(C.SPEED_CHOICES) do
		if tonumber(mb) == choice then return choice end
	end
	return 32
end

-- How the data travels: Steam's reliable stream (the default), our own datagram protocol, both one after the other, the
-- reliable stream on 1, 2 and 4 connections at once, or "auto": the way a resync save is sent (several Steam connections,
-- their number from the size of the file). The names are the launcher's `mode` words.
C.SPEED_MODES = { "reliable", "dgram", "both", "parallel", "auto" }
C.SPEED_MODE_LABELS = { reliable = "Reliable", dgram = "Datagrams", both = "Both", parallel = "Parallel", auto = "Auto (scaled)" }

-- The mode to run: one of the offered words, else reliable.
function C.speedMode(mode)
	for _, m in ipairs(C.SPEED_MODES) do
		if mode == m then return m end
	end
	return "reliable"
end

-- The command parameters of the speed test button.
function C.speedParams(steamId, mb, mode)
	return { id = steamId, mb = C.speedChoice(mb), mode = C.speedMode(mode) }
end

-- The lines to show under the buttons for the controller's steam.speed value: the result lines of a finished test, the
-- progress or failure line otherwise, nothing when no test was started.
function C.speedLines(speed)
	if type(speed) ~= "table" then return {} end
	local out = {}
	if type(speed.lines) == "table" then
		for i = 1, #speed.lines do out[#out + 1] = tostring(speed.lines[i]) end
	end
	if #out == 0 and speed.status ~= nil then out[1] = tostring(speed.status) end
	return out
end

-- ------------------------------------------------------------------ string-hash seed probe (observation only)

-- Builds a table of 30 fixed string keys and folds the order `pairs` visits them in into one number. Lua hashes
-- short strings with a per-state seed, so the order, and with it this number, differs between processes unless the
-- native module pins the seed (FEVERLINK3_LUA_SEED=fixed). Nothing here changes the game; it only reads.
function C.luaSeedProbe()
	local keys = {}
	for i = 1, 30 do keys[i] = "lane_" .. string.format("%02d", i) .. "_probe" end
	local t = {}
	for i = 1, #keys do t[keys[i]] = i end
	local h, n = 0, 0
	for _, i in pairs(t) do
		n = n + 1
		h = (h * 131 + i * 7 + n) % 4294967291
	end
	return h
end

-- Writes `luaseed probe order=<number>` with `log` once per Lua state, only when FEVERLINK3_LUA_SEED_LOG=1. Never
-- raises. Returns true when it wrote the line.
function C.luaSeedProbeOnce(log)
	local on = false
	pcall(function() on = os.getenv("FEVERLINK3_LUA_SEED_LOG") == "1" end)
	if not on or C.seedProbed then return false end
	C.seedProbed = true
	local ok, h = pcall(C.luaSeedProbe)
	pcall(log, "luaseed probe order=" .. (ok and string.format("%.0f", h) or "error"))
	return true
end

return C
