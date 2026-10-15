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
-- Every link file has exactly one writer, and is a generation log (C.newWriter, the launcher's gamelink "Generations"):
-- it never grows without bound and is never truncated; readers follow its `rotate` lines.

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

-- The size of a file, nil when it cannot be opened.
function C.fileSize(path)
	if not C.IO then return nil end
	local f = C.IO.open(path, "rb")
	if not f then return nil end
	local size = f:seek("end")
	f:close()
	return size
end

-- ------------------------------------------------------------------ generations
-- A log is a series of generation files: <stem>.<ext> (generation 1), <stem>.2.<ext>, <stem>.3.<ext>, ..., and <stem>.gen
-- holds the newest number. The same rule as the launcher's gamelink.rs ("Generations") and the native module's link.rs.
C.ROTATE_BYTES = 4 * 1024 * 1024   -- a writer starts a new generation once its current one holds this much
C.KEEP_GENERATIONS = 4             -- generation files a writer keeps, the newest included
C.ROTATE_KIND = "rotate"           -- the last line of a generation: seq 0, payload {["gen"]=<next>}

function C.genPath(base, gen)
	if gen == nil or gen <= 1 then return base end
	local stem, ext = base:match("^(.*)%.([^%.\\/]*)$")
	if not stem then return base .. "." .. C.integerText(gen) end
	return stem .. "." .. C.integerText(gen) .. "." .. ext
end

function C.genPointer(base)
	local stem = base:match("^(.*)%.[^%.\\/]*$")
	return (stem or base) .. ".gen"
end

-- The newest generation the pointer names; 1 without one.
function C.newestGen(base)
	local text = C.readFile(C.genPointer(base))
	local g = text and tonumber(text:match("%d+"))
	if g and g >= 1 then return g end
	return 1
end

-- Appends to a log that more than one Lua state writes (mod.log, trace.log: people read them, no program follows them): a
-- new generation starts when the current FILE holds `cap` bytes, whoever wrote them, so the limit holds across both halves
-- and across reloads. The generation before `keep` back is deleted (nil: none); after `maxGen` generations nothing more is
-- written (nil: no end). Returns true when written, false when not, "full" when the log reached `maxGen`.
local capGen = {}   -- per Lua state: log -> the generation it last wrote to
function C.appendCapped(base, text, cap, keep, maxGen)
	if not C.IO then return false end
	local g = capGen[base] or C.newestGen(base)
	local f = C.IO.open(C.genPath(base, g), "ab")
	if not f then return false end
	if (f:seek("end") or 0) >= cap then
		f:close()
		local newest = C.newestGen(base)
		local nextG = math.max(g + 1, newest)   -- the other half may have gone on already
		if maxGen and nextG > maxGen then return "full" end
		if newest < nextG then
			C.writeFile(C.genPointer(base), C.integerText(nextG) .. NL)
			if keep and nextG > keep then pcall(os.remove, C.genPath(base, nextG - keep)) end
		end
		g = nextG
		f = C.IO.open(C.genPath(base, g), "ab")
		if not f then return false end
	end
	capGen[base] = g
	f:write(text)
	f:close()
	return true
end

-- The session, as identity.txt names it. The session controller writes the file (launcher gamelink::Session::write_identity):
-- `session=<id>` and `start=<seq>` first, then id, name and `role=` last. The role is `host` or `player` while a session runs and
-- `idle` when there is none. The link files outlive sessions (quit to the menu, load another save, host again: the same
-- to_game.log), so a session is known by its id, and its lines are the ones above `start` (the last to_game.log line before it).
C.SESSION = nil       -- the id of the session this state is in ("" for an identity that names none: an older writer, the tests)
C.SESSION_START = 0   -- to_game.log lines at or below this sequence belong to earlier sessions
C.GEN = 0             -- counts the sessions this Lua state has seen begin: a half that started in another one must end first
C.ROLE = nil

-- identity.txt as a table, or nil when it is missing or incomplete (only a complete `role=` line counts: the writer puts it
-- last, so a read racing the write sees everything or no role)
function C.readIdentity()
	local text = C.readFile(C.path("identity.txt"))
	if not text then return nil end
	local role = text:match("role=(%a+)[" .. CR .. NL .. "]")
	if not role then return nil end
	return {
		role = role,
		id = tonumber(text:match("id=(%d+)")),
		name = text:match("name=([^" .. CR .. NL .. "]*)"),
		session = text:match("session=([%w%-]+)[" .. CR .. NL .. "]") or "",
		start = tonumber(text:match("start=(%d+)[" .. CR .. NL .. "]")) or 0,
	}
end

-- The session switch, for every half of the mod (both bridge halves, the UI hook, the window). identity.txt is read at most once a
-- second (cheap to call every frame; `force` reads it now). `host` or `player` makes the mod active; a session id that is not the
-- one this state is in starts another session (C.GEN grows: a half still in the old one ends it first, see the bridge's
-- sessionOff); any other complete role (`idle`) makes it inactive: the session is over, the game is single-player again. A
-- missing or incomplete file changes nothing. Without the file (a game the launcher started with FEVERLINK3_SESSION_DIR and no
-- identity yet) the environment's answer stands.
function C.poll(force)
	if not (C.DIR and C.IO) then return C.ACTIVE end
	local okT, now = pcall(os.clock)
	if not okT or type(now) ~= "number" then return C.ACTIVE end
	if not force and C.lastPoll and now >= C.lastPoll and now - C.lastPoll < 1 then return C.ACTIVE end
	C.lastPoll = now
	local idn = C.readIdentity()
	if not idn then return C.ACTIVE end
	C.ROLE = idn.role
	local on = (idn.role == "host" or idn.role == "player") and idn.id ~= nil
	if on and (not C.ACTIVE or idn.session ~= C.SESSION) then
		C.PLAYER_ID = idn.id
		C.PLAYER_NAME = (idn.name ~= nil and idn.name ~= "") and idn.name or ("player " .. tostring(idn.id))
		C.SESSION, C.SESSION_START = idn.session, idn.start
		C.ACTIVE, C.GEN = true, C.GEN + 1
		print("[feverlink3] session found: id=" .. tostring(idn.id) .. " role=" .. idn.role .. " session=" .. tostring(idn.session) .. " start=" .. tostring(idn.start))
	elseif not on and C.ACTIVE then
		C.ACTIVE = false
		print("[feverlink3] session over: role=" .. idn.role .. " (session " .. tostring(C.SESSION) .. ")")
	end
	return C.ACTIVE
end
C.poll(true)
print("[feverlink3] common loaded: active=" .. tostring(C.ACTIVE) .. " live=" .. tostring(C.LIVE) .. " io=" .. tostring(C.IO ~= nil) .. " id=" .. tostring(C.PLAYER_ID))

-- a logger writing "<clock> [<tag>] <message>" lines to mod.log in the session folder (and to the game's own log). Both halves
-- write it: it goes on in mod.2.log, mod.3.log ... every C.LOG_BYTES (whoever wrote them), and only the newest C.LOG_KEEP files
-- are kept (mod.gen names the newest).
C.LOG_BYTES, C.LOG_KEEP = 8 * 1024 * 1024, 4
function C.logger(tag)
	return function(msg)
		local line = "[FeverLink3 " .. tag .. "] " .. tostring(msg)
		pcall(function() if debugPrint then debugPrint(line) end end)
		if C.DIR and C.IO then
			pcall(C.appendCapped, C.path(C.LOG), os.date("%H:%M:%S") .. " " .. line .. NL, C.LOG_BYTES, C.LOG_KEEP)
		end
	end
end

-- Trace log (FEVERLINK3_TRACE, DEFAULT ON in test builds, FEVERLINK3_TRACE=0 switches it off): everything a real run needs to be explained
-- from the logs alone, in trace.log of the session folder (docs/TRACE.md). One line per event: "<HH:MM:SS> +<cpu s> <state> <tag> <text>".
-- Built for per-build and per-round events, never per tick. The file rolls over to trace.2.log, trace.3.log ... every 8 MB (nothing is
-- deleted); after TRACE_MAX_SEGMENTS segments it stops and says so once. The size is the file's, so the limit holds for both halves
-- together and across reloads (it was counted per Lua state and started again at a reload).
C.TRACE = true
pcall(function() C.TRACE = os.getenv("FEVERLINK3_TRACE") ~= "0" end)
local TRACE_SEGMENT, TRACE_MAX_SEGMENTS = 8 * 1024 * 1024, 12
local traceState = { stopped = false }
function C.tracer(state)
	return function(tag, text)
		if not C.TRACE or traceState.stopped or not (C.DIR and C.IO) then return end
		pcall(function()
			local line = string.format("%s +%.3f %s %s %s", os.date("%H:%M:%S"), os.clock(), state, tag, tostring(text))
			if #line > 200000 then line = line:sub(1, 200000) .. " ...(cut)" end
			local base = C.path("trace.log")
			if C.appendCapped(base, line .. NL, TRACE_SEGMENT, nil, TRACE_MAX_SEGMENTS) == "full" then
				traceState.stopped = true
				C.appendFile(C.genPath(base, TRACE_MAX_SEGMENTS), os.date("%H:%M:%S") .. " TRACE FULL: no more lines" .. NL)
			end
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

-- THE text of a number that may be integral, for every log line, record and key that can see money or another large value.
-- "%d" raises "bad argument #2 to 'format' (not a number in proper range)" above 2^31 in this game's Lua 5.2.2 (found by the sweep: a
-- balance of 5 billion made every update fail), and tostring() prints "5.0" for a float in some builds. "%.0f" prints the same digits
-- for every integral double below 2^53 (zero is kept free of a minus sign); anything else round-trips with "%.17g". Never use "%d" on a
-- value that is not a small count (tests/lua/test_mod_files.py guards the modules that carry values).
function C.integerText(v)
	if math.floor(v) == v and math.abs(v) < 9007199254740992 then return v == 0 and "0" or string.format("%.0f", v) end
	return string.format("%.17g", v)
end

local function serializeNumber(v)
	if v ~= v then return "(0/0)" end
	if v == math.huge then return "(1/0)" end
	if v == -math.huge then return "(-1/0)" end
	return C.integerText(v)
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
	return C.integerText(seq) .. TAB .. kind .. TAB .. tostring(from) .. TAB .. payload .. NL
end

function C.parseLine(line)
	local seq, kind, from, payload = line:match("^(%d+)" .. TAB .. "([^" .. TAB .. "]+)" .. TAB .. "([^" .. TAB .. "]*)" .. TAB .. "(.*)$")
	if not seq then return nil end
	return { seq = tonumber(seq), kind = kind, from = tonumber(from) or from, payload = payload }
end

-- The generation a `rotate` line names (seq 0, kind rotate), nil for every other record.
function C.rotateTarget(rec)
	if rec.seq ~= 0 or rec.kind ~= C.ROTATE_KIND then return nil end
	local g = tonumber(tostring(rec.payload):match("=%s*(%d+)"))
	if g and g >= 2 then return g end
	return nil
end

-- The highest sequence in the last 4 KB of a file, and the generation its last record names when that is a `rotate` line.
-- One open and one bounded read: a writer or reader calls it once, when it is made.
function C.lastSeqInFile(path)
	if not C.IO then return 0 end
	local f = C.IO.open(path, "rb")
	if not f then return 0 end
	local size = f:seek("end") or 0
	f:seek("set", math.max(0, size - 4096))
	local tail = f:read("*a") or ""
	f:close()
	if size > 4096 then tail = tail:gsub("^[^" .. NL .. "]*" .. NL, "", 1) end   -- the first piece may start mid-line
	local last, rotate = 0, nil
	for line in tail:gmatch("[^" .. NL .. "]+") do
		local rec = C.parseLine((line:gsub(CR .. "$", "")))
		if rec then
			if rec.seq > last then last = rec.seq end
			rotate = C.rotateTarget(rec)
		end
	end
	return last, rotate
end

-- Reader: tracks the generation and byte offset consumed and the last sequence dispatched. `opts`:
--   start = "oldest" (default: the oldest generation still there, from its first byte), "newest" (the newest generation from
--           its first byte: what a reloaded script reads as history), or "end" (whatever the log holds now is skipped);
--   keep  = true: the file stays open between polls (a file read every frame: no open per frame). A kept file cannot be
--           deleted while the reader holds it (Windows); readers close it when they go on to the next generation, so a
--           writer's deletion of old generations is not in the way, and the session folder is cleaned at game start.
function C.newReader(path, opts)
	opts = opts or {}
	local g = C.newestGen(path)
	if (opts.start or "oldest") == "oldest" and C.IO then
		while g > 1 and C.fileSize(C.genPath(path, g - 1)) do g = g - 1 end
	end
	local r = { base = path, gen = g, path = C.genPath(path, g), offset = 0, lastSeq = 0, partial = "", dropped = 0, keep = opts.keep == true }
	if opts.start == "end" then
		local size = C.fileSize(r.path)
		if size then
			local last, rotate = C.lastSeqInFile(r.path)
			r.offset, r.lastSeq = size, last
			if rotate then C.readerNext(r, rotate) end
		elseif g > 1 then
			r.lastSeq = C.lastSeqInFile(C.genPath(path, g - 1))
		end
	end
	return r
end

-- On to the start of generation `gen` (the one a `rotate` line named, or the newest after a deletion).
function C.readerNext(r, gen)
	if r.f then pcall(function() r.f:close() end) r.f = nil end
	r.gen, r.path, r.offset, r.partial, r.head, r.nextGen = gen, C.genPath(r.base or r.path, gen), 0, "", nil, nil
end

-- Pure part of the reader: feeds a chunk of text, returns the complete records with a new sequence number.
-- A trailing line without newline waits for the rest. Records whose seq is not above lastSeq are dropped and
-- counted (history re-written after a truncation, duplicates). A `rotate` line ends the feed: the writer wrote nothing
-- after it in this file, and r.nextGen says where the log goes on.
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
				local nextGen = C.rotateTarget(rec)
				if nextGen then
					r.nextGen, r.partial = nextGen, ""
					return out
				end
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

-- One read of the reader's generation file. Returns the records and whether the reader went on to another generation.
local HEAD = 64
local function readGeneration(r)
	local f = r.f
	if not f then
		f = C.IO.open(r.path, "rb")
		if not f then
			-- a file that was there and is gone: deleted under a reader that fell behind every kept generation (on with the newest),
			-- or the folder was cleaned (a new file starts from its first byte)
			if r.seen then
				r.seen = false
				local newest = C.newestGen(r.base or r.path)
				if newest > (r.gen or 1) then
					C.readerNext(r, newest)
					return {}, true
				end
			end
			return {}, false
		end
		if r.keep then r.f = f end
	end
	r.seen = true
	local size = f:seek("end")
	if not size then
		pcall(function() f:close() end)
		r.f = nil
		return {}, false
	end
	if size < r.offset then
		r.offset, r.partial, r.head = 0, "", nil
	elseif size > r.offset and r.offset > 0 and r.head then
		-- grown, but is it still the same file? A file truncated and written again past the old length starts with other bytes
		f:seek("set", 0)
		if f:read(#r.head) ~= r.head then r.offset, r.partial, r.head = 0, "", nil end
	end
	local out, switched = {}, false
	if size > r.offset then
		f:seek("set", r.offset)
		local chunk = f:read(size - r.offset) or ""
		if r.offset == 0 then r.head = chunk:sub(1, HEAD) end
		r.offset = r.offset + #chunk
		out = C.readerFeed(r, chunk)
		if r.nextGen then
			if not r.keep then f:close() end
			f = nil
			C.readerNext(r, r.nextGen)
			switched = true
		end
	end
	if f and not r.keep then f:close() end
	return out, switched
end

-- Reads what the log gained since the last poll, through the generations it went on to. A file that is not the one read
-- before (shorter, or other first bytes) was rewritten: reading restarts from its beginning and the sequence filter in
-- readerFeed keeps history from being dispatched again.
function C.readerPoll(r)
	if not C.IO then return {} end
	local out = {}
	for _ = 1, 5 do
		local recs, switched = readGeneration(r)
		for _, rec in ipairs(recs) do out[#out + 1] = rec end
		if not switched then break end
	end
	return out
end

-- Writer: appends records with a sequence that continues after whatever the log already holds, so a reload of the
-- savegame (which restarts the script and its counters) does not restart the sequence seen by the reader. Made ONCE per
-- Lua state and kept: the log's tail is read here and never again. A log has exactly one writer at a time (the native
-- control files are one per half: native_ctl.gui.log, native_ctl.sim.log). `opts.rotate` / `opts.keep` change the
-- generation size and how many generations are kept (tests).
function C.newWriter(path, from, opts)
	opts = opts or {}
	local g = C.newestGen(path)
	local w = { base = path, gen = g, path = C.genPath(path, g), from = from, seq = 0, size = 0, failed = 0,
		rotate = opts.rotate or C.ROTATE_BYTES, keep = opts.keep or C.KEEP_GENERATIONS }
	w.size = C.fileSize(w.path) or 0
	w.seq = C.lastSeqInFile(w.path)
	if w.seq == 0 and g > 1 then w.seq = C.lastSeqInFile(C.genPath(path, g - 1)) end
	return w
end

-- Ends the writer's generation with a `rotate` line and goes on in the next (the pointer is written, the generation `keep`
-- back is deleted). False when the `rotate` line could not be written: the writer stays where it is.
function C.writerRotate(w)
	local nextG = w.gen + 1
	if not C.appendFile(w.path, C.formatLine(0, C.ROTATE_KIND, w.from, C.serialize({ gen = nextG }))) then return false end
	C.writeFile(C.genPointer(w.base), C.integerText(nextG) .. NL)
	if nextG > w.keep then pcall(os.remove, C.genPath(w.base, nextG - w.keep)) end
	w.gen, w.path, w.size = nextG, C.genPath(w.base, nextG), 0
	return true
end

function C.writerSend(w, kind, payload)
	local ok, text = pcall(C.serialize, payload or {})
	if not ok then return false, text end
	if w.rotate and w.size > 0 and w.size >= w.rotate then C.writerRotate(w) end
	w.seq = w.seq + 1
	local line = C.formatLine(w.seq, kind, w.from, text)
	if C.appendFile(w.path, line) then
		w.size = (w.size or 0) + #line
		return true
	end
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
