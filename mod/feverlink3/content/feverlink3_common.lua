-- FeverLink3 common code: environment, deterministic serialisation, string hash, and the file link to the
-- launcher (its file side: feverlink3_link.lua; logging: feverlink3_log.lua). Shared by both halves of the game script (simulation state and interface state). No engine calls
-- here: everything in this file runs anywhere Lua 5.2 runs, which is how the tests drive it.
--
-- File link (docs/ARCHITECTURE.md section 2, docs/PROTOCOL.md): two append-only text files in the session folder.
--   to_game.log    launcher -> mod
--   from_game.log  mod -> launcher
-- One record per line: kind <TAB> from <TAB> payload. A line carries no number: its place in the file is its order, and
-- every reader reads by position (the generation and the bytes it consumed, C.newReader), complete lines only. Nothing is
-- dropped for what a line says. (The lines used to carry a sequence number the readers filtered on; after a resync reload
-- the window's writer read only the last 4 KB of its file, found no number in a 4.7 KB line, started again at 1, and the
-- bridge dropped every later window command as old: friend test 2026-10-16, "could not buy cars".) `from` is the sender
-- id the launcher attached (0 = host); the mod trusts the column, not anything inside the payload. The payload is one line
-- produced by C.serialize.
-- Every link file is written from one thread only (C.newWriter says why), and is a generation log (the launcher's gamelink
-- "Generations"): it never grows without bound and is never truncated; readers follow its `rotate` lines.

local C = {}

C.TAB = string.char(9)
C.NL = string.char(10)
C.CR = string.char(13)
C.BS = string.char(92)
C.QUOTE = string.char(34)
C.STEP = 200 -- game-time units per simulation step (docs/ARCHITECTURE.md section 5)

local TAB, NL, CR, BS, QUOTE = C.TAB, C.NL, C.CR, C.BS, C.QUOTE

-- ------------------------------------------------------------------ the link module and the log module

-- Every use of Lua's io and os libraries (not part of the official TF3 scripting API) is in feverlink3_link.lua, which says why each
-- one has no official counterpart; every log and diagnostic file is written through feverlink3_log.lua. Both add their functions to C
-- under the names the mod has always used (C.IO, C.env, C.clock, C.readFile, C.newReader ..., C.logger, C.tracer, C.dump). Without the
-- link module the mod has no file access, no environment and no clock, and stays inactive (fail closed); without the log module it
-- logs to the game's own log only (debugPrint).
do
	local function need(name)
		local ok, M = pcall(ug_require, name .. ".lua")
		if not ok then ok, M = pcall(ug_require, "feverlink3::/" .. name .. ".lua") end
		if ok and type(M) == "table" then return M end
		return nil
	end
	local function none() return nil end
	local function no() return false end
	local Link, Log = need("feverlink3_link"), need("feverlink3_log")
	C.LINK_MODULE, C.LOG_MODULE = Link ~= nil, Log ~= nil
	if Link then
		Link.install(C)
	else
		C.IO = nil
		C.env, C.remove, C.openHeld, C.readFile, C.fileSize = none, none, none, none, none
		C.clock, C.time = function() return 0 end, function() return 0 end
		C.date = function() return "" end
		C.officialProbe = function() return "link module missing" end
		C.appKeys = C.officialProbe
		C.appendFile, C.writeFile, C.appendBelow, C.writerRotate = no, no, no, no
		C.genPath = function(base) return base end
		C.genPointer = function(base) return base .. ".gen" end
		C.newestGen = function() return 1 end
		C.newReader = function(path) return { base = path, gen = 1, path = path, consumed = 0, partial = "", dropped = 0 } end
		C.readerPos = function(r) return { gen = r.gen, offset = r.consumed } end
		C.readerNext = function() end
		C.readerFeed, C.readerPoll = function() return {} end, function() return {} end
		C.newWriter = function(path, from) return { base = path, gen = 1, path = path, from = from, lines = 0, failed = 0, size = 0 } end
		C.writerSend = function(w) w.failed = (w.failed or 0) + 1 return false, "no file access" end
		C.newOwnWriter = function() return nil, "no file access" end
	end
	if Log then
		Log.install(C)
	else
		C.LOG, C.LOG_BYTES, C.LOG_KEEP, C.TRACE = "mod.log", 0, 0, false
		C.appendCapped, C.dump, C.dumpReplace = no, no, no
		C.logger = function(tag)
			return function(msg) pcall(function() if debugPrint then debugPrint("[FeverLink3 " .. tag .. "] " .. tostring(msg)) end end) end
		end
		C.tracer = function() return function() end end
	end
end

-- ------------------------------------------------------------------ environment

C.DIR = C.env("FEVERLINK3_SESSION_DIR")
C.PLAYER_ID = tonumber(C.env("FEVERLINK3_PLAYER_ID") or "")
C.PLAYER_NAME = C.env("FEVERLINK3_PLAYER_NAME")
if C.PLAYER_NAME == nil or C.PLAYER_NAME == "" then C.PLAYER_NAME = "player " .. tostring(C.PLAYER_ID or "?") end
-- without the session folder (game not started by FeverLink3.exe) the mod does nothing
C.LIVE = false -- the native module hosts the session itself: files live in a fixed folder and identity.txt says who we are
if C.DIR == nil or C.DIR == "" then
	-- FEVERLINK3_LIVE_DIR: a second game on the same PC (tests, recordings) keeps its files apart from the first's
	local override = C.env("FEVERLINK3_LIVE_DIR")
	local base = C.env("LOCALAPPDATA")
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
C.CURSOR = "resync_cursor.txt" -- written before a resync load: the places in to_game.log and ui_out.log the reloaded mod resumes at

-- ------------------------------------------------------------------ generations
-- A log is a series of generation files: <stem>.<ext> (generation 1), <stem>.2.<ext>, <stem>.3.<ext>, ..., and <stem>.gen
-- holds the newest number (C.genPath, C.genPointer, C.newestGen: the link module). The same rule as the launcher's gamelink.rs
-- ("Generations") and the native module's link.rs.
C.ROTATE_BYTES = 4 * 1024 * 1024   -- a writer starts a new generation once its current one holds this much
C.KEEP_GENERATIONS = 4             -- generation files a writer keeps, the newest included
C.ROTATE_KIND = "rotate"           -- the last line of a generation: payload {["gen"]=<next>,["skip"]=<header bytes>}

-- The session, as identity.txt names it. The session controller writes the file (launcher gamelink::Session::write_identity):
-- `session=<id>` and `start=<gen>:<offset>` first, then id, name and `role=` last. The role is `host` or `player` while a session runs
-- and `idle` when there is none. The link files outlive sessions (quit to the menu, load another save, host again: the same
-- to_game.log), so a session is known by its id, and its lines are the ones after `start` (where to_game.log ended before it).
C.SESSION = nil       -- the id of the session this state is in ("" for an identity that names none: an older writer, the tests)
C.SESSION_START = nil -- {gen, offset}: to_game.log lines before this place belong to earlier sessions (nil: none named)
C.GEN = 0             -- counts the sessions this Lua state has seen begin: a half that started in another one must end first
C.ROLE = nil

-- `start=<gen>:<offset>` as a place in to_game.log (nil without one)
local function startPos(text)
	local g, o = text:match("start=(%d+):(%d+)[" .. CR .. NL .. "]")
	g, o = tonumber(g), tonumber(o)
	if g and o and g >= 1 then return { gen = g, offset = o } end
	return nil
end

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
		start = startPos(text),
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
	local okT, now = pcall(C.clock)
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
		print("[feverlink3] session found: id=" .. tostring(idn.id) .. " role=" .. idn.role .. " session=" .. tostring(idn.session) .. " start=" .. (idn.start and (string.format("%.0f:%.0f", idn.start.gen, idn.start.offset)) or "-"))
	elseif not on and C.ACTIVE then
		C.ACTIVE = false
		print("[feverlink3] session over: role=" .. idn.role .. " (session " .. tostring(C.SESSION) .. ")")
	end
	return C.ACTIVE
end
C.poll(true)
print("[feverlink3] common loaded: active=" .. tostring(C.ACTIVE) .. " live=" .. tostring(C.LIVE) .. " io=" .. tostring(C.IO ~= nil) .. " id=" .. tostring(C.PLAYER_ID)
	.. " link=" .. tostring(C.LINK_MODULE) .. " log=" .. tostring(C.LOG_MODULE))

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
	-- a record keeps the sign of zero (integerText drops it for log lines and money): the engine stores a roundabout's
	-- transform with -0.0 (80000000) and the proof compares the rebuilt command bit for bit (two windows 2026-10-23:
	-- "DIFF toAdd[0].transf: m4:3f800000,80000000,... <> m4:3f800000,00000000,...", every roundabout refused)
	if v == 0 and 1 / v < 0 then return "-0.0" end
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
-- The xor of two bytes is the cost (eight halvings a byte in Lua): Lua 5.2's bit32.bxor (or LuaJIT's bit.bxor) does it natively when the
-- state has it. Both operands are below 256, so the result is the same byte either way (LuaJIT's signed results cannot arise); a library
-- that disagrees with xor8 on a probe is not used. C.FNV_XOR names the path taken ("bit32", "bit" or "lua"); C.fnv1aWith(s, xor) is the
-- loop over any xor (the tests compare the paths).
function C.fnv1aWith(s, xor)
	local h = 2166136261
	local byte = string.byte
	for i = 1, #s do
		local lo = h % 256
		h = h - lo + xor(lo, byte(s, i))
		h = ((h % 256) * 16777216 + h * 403) % 4294967296
	end
	return h
end

C.FNV_XOR, C.fnvXor = "lua", xor8
for _, lib in ipairs({ "bit32", "bit" }) do
	-- a global read that may raise (a strict environment) or be absent (a sandbox without the library)
	local okL, B = pcall(function() if lib == "bit32" then return bit32 end return bit end)
	local f = okL and type(B) == "table" and B.bxor
	if C.FNV_XOR == "lua" and type(f) == "function" then
		local same = pcall(function()
			for a = 0, 255, 17 do for b = 0, 255, 13 do if f(a, b) ~= xor8(a, b) then error("differs") end end end
		end)
		if same then C.FNV_XOR, C.fnvXor = lib, f end
	end
end

function C.fnv1a(s)
	return C.fnv1aWith(s, C.fnvXor)
end

-- hash of any serialisable value, as 8 hex digits: what a checksum part carries
function C.hashValue(v)
	return string.format("%08x", C.fnv1a(C.serialize(v)))
end

-- ------------------------------------------------------------------ which Lua state this is (a research aid)

-- A fresh id, 8 hex digits, made without math.random (the session installs a deterministic one in the simulation half; drawing
-- from it here would change what the game draws): the address of a new table, the clocks and a counter, hashed.
local idCount = 0
function C.newId()
	idCount = idCount + 1
	local okT, t = pcall(C.time)
	local okC, c = pcall(C.clock)
	return string.format("%08x", C.fnv1a(tostring({}) .. "|" .. tostring(okT and t) .. "|" .. tostring(okC and c) .. "|" .. idCount))
end

-- The address part of tostring(t) ("table: 0x0123..." or, from the Windows C runtime, "table: 000001...").
function C.addressOf(t)
	local s = tostring(t)
	return s:match(":%s*(.+)$") or s
end

-- The Lua state's marker: one table per Lua state, kept in package.loaded (which belongs to the state, not to a script), made by
-- the first script of the state that loads this file. A script loaded again into the same state finds the same marker (same
-- address, same id, `loads` one more); another Lua state has a marker of its own. Question this answers (docs/research/TODO.md,
-- option 1): runs b3real/b3real2 saw two writers of the simulation half's control file; are they two simulation Lua states, or
-- one state that loaded the script twice? The log line of every control writer and every control line say which state wrote it, and the bridge
-- logs it with its own load id when it is loaded (feverlink3_bridge.script.lua, "script load").
do
	local okP, loaded = pcall(function() return package.loaded end)
	local marker = okP and type(loaded) == "table" and loaded["feverlink3_lua_state"] or nil
	if type(marker) ~= "table" then
		marker = { id = C.newId(), loads = 0 }
		if okP and type(loaded) == "table" then loaded["feverlink3_lua_state"] = marker end
	end
	marker.loads = marker.loads + 1
	C.STATE = marker
	C.STATE_ADDR = C.addressOf(marker)
	C.LOAD_ID = C.newId()   -- this load of this file (a cached module is loaded once per state)
end

-- "state=<marker address>/<state id> loads=<n> common=<this file's load id>": the same state shows the same address and id.
function C.stateTag()
	return "state=" .. tostring(C.STATE_ADDR) .. "/" .. tostring(C.STATE.id) .. " loads=" .. tostring(C.STATE.loads) .. " common=" .. tostring(C.LOAD_ID)
end
print("[feverlink3] lua state: " .. C.stateTag())

-- ------------------------------------------------------------------ link records

-- kind <TAB> from <TAB> payload. No number: the line's place in the file is its order.
function C.formatLine(kind, from, payload)
	return kind .. TAB .. tostring(from) .. TAB .. payload .. NL
end

function C.parseLine(line)
	local kind, from, payload = line:match("^([^" .. TAB .. "]*)" .. TAB .. "([^" .. TAB .. "]*)" .. TAB .. "(.*)$")
	if not kind then return nil end
	return { kind = kind, from = tonumber(from) or from, payload = payload }
end

-- The generation a `rotate` line names and the bytes at the top of it that a reader coming through the rotation has
-- read already (the writer's header; the mod's own writers have none), nil for every other record.
function C.rotateTarget(rec)
	if rec.kind ~= C.ROTATE_KIND then return nil end
	local p = tostring(rec.payload)
	local g = tonumber(p:match('%["gen"%]%s*=%s*(%d+)'))
	if not g or g < 2 then return nil end
	return g, tonumber(p:match('%["skip"%]%s*=%s*(%d+)')) or 0
end

-- ------------------------------------------------------------------ positions
-- A place in a link log: { gen = <generation>, offset = <bytes into its file> }, written "<gen>:<offset>" (identity.txt
-- `start=`, the resync cursor). Every reader reads by position: the session's first line, the line after a resync load, the
-- window's records of this session are all places in a file, never numbers the lines carry.

function C.posText(p)
	if type(p) ~= "table" then return "-" end
	return C.integerText(p.gen) .. ":" .. C.integerText(p.offset)
end

function C.parsePos(text)
	local g, o = tostring(text or ""):match("^%s*(%d+):(%d+)%s*$")
	g, o = tonumber(g), tonumber(o)
	if g and o and g >= 1 then return { gen = g, offset = o } end
	return nil
end

-- a strictly before b
function C.posBefore(a, b)
	if a.gen ~= b.gen then return a.gen < b.gen end
	return a.offset < b.offset
end

-- the later of two places (nil counts as none)
function C.posMax(a, b)
	if a == nil then return b end
	if b == nil then return a end
	return C.posBefore(a, b) and b or a
end

-- Where a log ends now: its newest generation and that file's size.
function C.logEnd(base)
	local g = C.newestGen(base)
	return { gen = g, offset = C.fileSize(C.genPath(base, g)) or 0 }
end

-- The place right after a record a reader handed out, and the place it starts at.
function C.recordEnd(rec) return { gen = rec.gen, offset = rec.after } end
function C.recordStart(rec) return { gen = rec.gen, offset = rec.start } end

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

-- ------------------------------------------------------------------ who called a wrapped command function

-- The folder of the mod a script source path belongs to, or nil when the path is not a mod's (the game's own scripts, which every
-- game runs alike). The interface half turns it into the mod's own name (Hud.modName).
-- ".../mods/6414403/content/auto_line_namer/aln.script.lua" -> "6414403"
function C.modOfSource(src)
	local p = tostring(src or ""):gsub("^@", ""):gsub("\\", "/")
	local low = p:lower()
	if not (low:find("/mods/", 1, true) or low:find("mod.io", 1, true) or low:find("staging_area", 1, true) or low:find("/workshop/", 1, true)) then
		return nil
	end
	return p:match("^.-([^/]+)/content/") or p:match("([^/]+)$")
end

-- The code that called `wrapper` (a wrapped api.cmd.sendCommand): the first Lua frame below it on the stack (debug.getinfo; C frames
-- and the wrapper's own pcalls are passed over). Returns its source and its mod (C.modOfSource); nil, nil, true for FeverLink3's own
-- code (a source that names feverlink3, or one in `own`); nil when it is not known (no debug library). Another mod's commands are told
-- apart this way only (owner 2026-10-23, option A): no list of mods, every mod's code is judged by where it lies.
function C.callerOf(wrapper, own)
	if type(debug) ~= "table" or type(debug.getinfo) ~= "function" then return nil end
	local past = false
	for level = 2, 24 do
		local info = debug.getinfo(level, "Sf")
		if not info then break end
		if info.func == wrapper then
			past = true
		elseif past and type(info.source) == "string" and info.source ~= "=[C]" then
			if (own and own[info.source]) or info.source:lower():find("feverlink3", 1, true) then return nil, nil, true end
			return info.source, C.modOfSource(info.source)
		end
	end
	return nil
end

-- ------------------------------------------------------------------ a command callback's result entities

-- The third argument of a sendCommand callback is `resultEntities : {{Engine.Entity, Engine.Revision}}` (tf3-docs api/cmd,
-- cmd.d.tl:598; the base game reads `resultEntities[1][1]`, and an entity is checked against its revision with `rev.num[1]`,
-- api/engine getRevision). A result that crossed from the simulation half or the bridge's files names the entity only (a revision
-- cannot be stored in the game-script state or a link file), so before a callback runs each pair gets this game's revision of the
-- entity back (api.engine.getRevision; read-only). A revision already given (the engine's object, or a table with `num`) stays; one
-- that cannot be read leaves the pair as it came. Returns a new list; entries that name no entity are left out.
function C.resultEntities(list)
	local out = {}
	if type(list) ~= "table" then return out end
	for _, r in ipairs(list) do
		local e, rev = r, nil
		if type(r) == "table" then e, rev = r[1], r[2] end
		if type(e) == "number" then
			local given = type(rev) == "userdata" or (type(rev) == "table" and rev.num ~= nil)
			if not given then
				local ok, v = pcall(function()
					if api.engine.entityExists(e) then return api.engine.getRevision(e) end
					return nil
				end)
				if ok and v ~= nil then rev = v end
			end
			out[#out + 1] = { e, rev }
		end
	end
	return out
end

-- ------------------------------------------------------------------ transport modes ({TransportMode : boolean})

-- The transportModes of a lane (LaneConfig), a line (LineVehicleInfo), an edge or a terminal is a map keyed by the values of
-- api.type.enum.TransportMode ("transportModes: {TransportMode : boolean}", script-doc api/type.html and the game's own
-- tealdef/api/type.d.tl). The game's own scripts read and write it by those keys
-- (`lineComponent.vehicleInfo.transportModes[api.type["enum"].TransportMode.SHIP]`, achievements.script.tl;
-- `newFilter.transportModes[tmShip] = true`, vehicle_store_util.tl). Nothing documents an index base or a shift, so a map travels
-- by the enum's NAMES (the same on every game, whatever number the enum gives a name) and is rebuilt by setting each named key to
-- this game's enum value. A key the enum does not name, or a name this game's enum lacks, fails closed (nil and the reason): the
-- caller refuses the command or the build instead of guessing. (This replaces C.writeEngineArray, the write-read-back-and-shift
-- rule borrowed from MPFever's lane measurement, of which no measured result exists; 2026-10-18 wiki audit A5.)

-- the enum's names in the order the documentation lists them
C.TRANSPORT_MODE_NAMES = { "PERSON", "CARGO", "CAR", "BUS", "TRUCK", "TRAM", "ELECTRIC_TRAM", "TRAIN", "ELECTRIC_TRAIN", "AIRCRAFT",
	"SHIP", "SMALL_AIRCRAFT", "SMALL_SHIP", "HELICOPTER", "TRAM_TRACK", "ELECTRIC_TRAM_TRACK" }

-- name -> value and value -> name of this game's api.type.enum.TransportMode; nil and the reason where the enum is not there
function C.transportModeEnum()
	local okE, enum = pcall(function() return api.type["enum"].TransportMode end)
	if not okE or enum == nil then return nil, "no api.type.enum.TransportMode here" end
	local byName, byValue = {}, {}
	for _, name in ipairs(C.TRANSPORT_MODE_NAMES) do
		local okV, v = pcall(function() return enum[name] end)
		if okV and v ~= nil then byName[name], byValue[v] = v, name end
	end
	if next(byName) == nil then return nil, "api.type.enum.TransportMode names no mode" end
	return byName, byValue
end

-- A transport-mode map as read from the engine ({<enum value> = boolean}) -> { <NAME> = boolean }. nil and the reason for a key
-- the enum does not name or a value that is not a boolean.
function C.modeNames(map)
	if type(map) ~= "table" and type(map) ~= "userdata" then return nil, "transport modes are not a map" end
	local byName, byValue = C.transportModeEnum()
	if not byName then return nil, byValue end   -- (the second value is the reason then)
	local okP, out, why = pcall(function()
		local o = {}
		for k, v in pairs(map) do
			local name = byValue[k]
			if name == nil then return nil, "transport mode key " .. tostring(k) .. " is not a value of api.type.enum.TransportMode" end
			if type(v) ~= "boolean" then return nil, "transport mode " .. name .. " is " .. type(v) .. ", not a boolean" end
			o[name] = v
		end
		return o
	end)
	if not okP then return nil, "transport modes cannot be read: " .. tostring(out) end
	return out, why
end

-- The inverse: { <NAME> = boolean } -> the map keyed by this game's enum values, ready to assign to the member. nil and the reason
-- for a name this game's enum lacks.
function C.modeMap(names)
	if type(names) ~= "table" then return nil, "transport modes are not a map of names" end
	local byName, why = C.transportModeEnum()
	if not byName then return nil, why end
	local out = {}
	for name, v in pairs(names) do
		local key = byName[name]
		if key == nil then return nil, "transport mode " .. tostring(name) .. " is not in api.type.enum.TransportMode here" end
		if type(v) ~= "boolean" then return nil, "transport mode " .. tostring(name) .. " is " .. type(v) .. ", not a boolean" end
		out[key] = v
	end
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
	pcall(function() on = C.env("FEVERLINK3_LUA_SEED_LOG") == "1" end)
	if not on or C.seedProbed then return false end
	C.seedProbed = true
	local ok, h = pcall(C.luaSeedProbe)
	pcall(log, "luaseed probe order=" .. (ok and string.format("%.0f", h) or "error"))
	return true
end

return C
