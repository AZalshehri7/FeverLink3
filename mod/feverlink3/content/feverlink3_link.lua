-- FeverLink3 link: the ONE module of the mod that uses Lua's `io` and `os` libraries. Nothing else in the mod names them (the only
-- other users of an io file handle are the logging module's writes, which go through C.appendBelow here). Owner decision 2026-10-18:
-- the mod uses the official TF3 scripting API wherever it can; what has no official counterpart is kept, but only here, documented.
--
-- The official API (https://wiki.transportfever3.com/script-doc/, read 2026-10-18: main, app, api.*, content index) has no file
-- access beyond app.saveUserdata/loadUserdata (whole Lua tables in the userdata folder: no append, no read by position, nothing the
-- native module can follow), no real-time clock (game time stops while the game is paused, and the mod measures time while paused),
-- no wall clock or date, and no environment. So every item below is NOT official, and why it stays:
--   io (C.IO, C.appendFile ... C.newReader / C.newWriter / C.newOwnWriter)
--       the link with the native module (winhttp.dll) and its session controller: to_game.log / from_game.log, identity.txt,
--       ctl_cmd.log / ctl_status.txt, native_ctl.<half>.w<id>.log / native_events.log, native_text_* / native_input_* /
--       native_refused_* / native_inject_*, world.lock (a handle held open as a presence sign); and the mod's own files between its
--       Lua states (hud_state.txt, companies.txt, binds.txt, ui_out.log, ui_in.log, resync_cursor.txt), which the native module
--       cleans with the session folder at game start. The interface state hides the `io` global: it is taken from package.loaded.
--   os.getenv (C.env)    the session folder, player id and name of a dev-launched game, LOCALAPPDATA (the folder the native module
--                        uses), and the test and lab switches (FEVERLINK3_*). The game passes no environment or launch option to scripts.
--   os.clock (C.clock)   real seconds: timeouts, pacing and cost budgets that must run while the game is paused, and the C runtime's
--                        clock the native module stamps its events with (`timing clock`, X.wallNow in the bridge).
--   os.time (C.time)     a coarse wall clock: unique ids (C.newId, the session epoch, own-writer file names), gap detection.
--   os.date (C.date)     the time of day in log lines and the guest's end-of-session save name.
--   os.remove (C.remove) deletes a link log's generation `keep` back (the generation rule of the launcher and the native module).
-- Every capability is looked up when it is used (tests swap os.clock), and every one fails closed: no io means no file is read or
-- written (C.ACTIVE stays false, the mod does nothing), a missing os function raises where the old direct call raised.
--
-- File link format (docs/ARCHITECTURE.md section 2, docs/PROTOCOL.md): append-only text logs in generations, read by position.
-- The pure parts of the format (records, rotate lines, positions) stay in feverlink3_common.lua; Link.install(C) adds the parts
-- that touch files to C, under the names the mod has always used.

local Link = {}

local NL, CR = string.char(10), string.char(13)

-- ------------------------------------------------------------------ raw capabilities

-- the game may run scripts with a reduced standard library: take io only if it is there
do
	local ok, v = pcall(function() return io end)
	if not (ok and type(v) == "table") then
		-- the game removes the io global but leaves the module loaded (finding from MPFever (MIT): package.loaded.io)
		ok, v = pcall(function() return package.loaded["io"] end)
	end
	Link.IO = ok and type(v) == "table" and v or nil
end

-- An environment variable, nil when unset or when the state has no os.getenv.
function Link.env(name)
	local ok, v = pcall(function() return os.getenv(name) end)
	if ok then return v end
	return nil
end
function Link.clock() return os.clock() end
function Link.time() return os.time() end
function Link.date(fmt) return os.date(fmt) end
function Link.remove(path) return os.remove(path) end

-- Observation only (lab): which official replacements this Lua state offers, as one log text. Calls nothing that changes anything.
function Link.officialProbe()
	local parts = {}
	local function has(label, get)
		local ok, v = pcall(get)
		parts[#parts + 1] = label .. "=" .. ((ok and v ~= nil) and type(v) or "no")
	end
	has("app", function() return app end)
	has("saveUserdata", function() return app.saveUserdata end)
	has("getUserDataFolder", function() return app.getUserDataFolder end)
	has("fireGuiScriptEvent", function() return api.gui.fireGuiScriptEvent end)
	has("sendScriptEvent", function() return api.cmd.makeScriptingSendEventCmd end)
	has("io_global", function() return io end)
	local okB, build = pcall(function() return getBuildVersion() end)
	parts[#parts + 1] = "build=" .. (okB and tostring(build) or "no")
	local okF, folder = pcall(function() return app.getUserDataFolder() end)
	parts[#parts + 1] = "userdata=" .. (okF and tostring(folder) or "no")
	return table.concat(parts, " ")
end

-- Observation only (lab): every key of the global `app` and `main` this Lua state sees, with each value's type, and the start-gate
-- functions by name (a key may come from an __index the pairs walk does not show). The 2026-10-18 lab run found `app` a table in the
-- interface and window states whose functions are tables (bound engine functions, "saveUserdata=table"), not Lua functions.
function Link.appKeys()
	local function keys(get)
		local okG, t = pcall(get)
		if not okG or t == nil then return "none" end
		if type(t) ~= "table" then return type(t) end
		local out = {}
		pcall(function()
			for k, v in pairs(t) do out[#out + 1] = tostring(k) .. ":" .. type(v) end
		end)
		table.sort(out)
		return #out .. " [" .. table.concat(out, " ") .. "]"
	end
	local named = {}
	for _, n in ipairs({ "setWaitForStartReadyGame", "isWaitForStartReadyGame", "startReadyGame", "getProgressMonitor" }) do
		local ok, v = pcall(function() return app[n] end)
		named[#named + 1] = n .. "=" .. (ok and type(v) or "error")
	end
	return "app " .. keys(function() return app end) .. "; by name: " .. table.concat(named, " ") .. "; main " .. keys(function() return main end)
end

-- ------------------------------------------------------------------ installed into the common module

function Link.install(C)
	C.IO = Link.IO
	C.env, C.clock, C.time, C.date, C.remove = Link.env, Link.clock, Link.time, Link.date, Link.remove
	C.officialProbe = Link.officialProbe
	C.appKeys = Link.appKeys

	-- ---------------------------------------------------------------- files

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

	-- Appends `text` when the file holds fewer than `cap` bytes, with one open: true when written, "over" when the file is full
	-- (nothing written), false when it cannot be opened. The capped logs' write (feverlink3_log.lua).
	function C.appendBelow(path, text, cap)
		if not C.IO then return false end
		local f = C.IO.open(path, "ab")
		if not f then return false end
		if (f:seek("end") or 0) >= cap then
			f:close()
			return "over"
		end
		f:write(text)
		f:close()
		return true
	end

	-- A handle opened for appending and kept: the caller never closes it (world.lock, the presence sign of a world's script).
	function C.openHeld(path)
		if not C.IO then return nil end
		return C.IO.open(path, "ab")
	end

	-- ---------------------------------------------------------------- generations
	-- A log is a series of generation files: <stem>.<ext> (generation 1), <stem>.2.<ext>, <stem>.3.<ext>, ..., and <stem>.gen
	-- holds the newest number. The same rule as the launcher's gamelink.rs ("Generations") and the native module's link.rs.

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

	-- ---------------------------------------------------------------- reader

	-- The size of a file, its last byte, and where it leads when its last line is a `rotate` line. One open, one bounded read.
	local function tailState(path)
		if not C.IO then return nil end
		local f = C.IO.open(path, "rb")
		if not f then return nil end
		local size = f:seek("end") or 0
		f:seek("set", math.max(0, size - 4096))
		local tail = f:read("*a") or ""
		f:close()
		local last = tail:sub(-1)
		if size > 4096 then tail = tail:gsub("^[^" .. NL .. "]*" .. NL, "", 1) end   -- the first piece may start mid-line
		local lastLine
		for line in tail:gmatch("[^" .. NL .. "]+") do
			if line:gsub(CR .. "$", "") ~= "" then lastLine = line end
		end
		local rec = lastLine and C.parseLine((lastLine:gsub(CR .. "$", "")))
		local g, skip
		if rec and last == NL then g, skip = C.rotateTarget(rec) end
		return size, last, g, skip
	end

	-- Reader: reads a log by position, the generation and the bytes of its file it has consumed (complete lines only; an unfinished
	-- last line waits for its newline). Nothing is dropped for what a line says. `opts`:
	--   start = "oldest" (default: the oldest generation still there, from its first byte), "newest" (the newest generation from
	--           its first byte: what a reloaded script reads as history), or "end" (whatever the log holds now is skipped, an unfinished
	--           last line too);
	--   at    = a place ({gen, offset} or "<gen>:<offset>"): the lines after it (a session's start, a resync cursor);
	--   keep  = true: the file stays open between polls (a file read every frame: no open per frame). A kept file cannot be
	--           deleted while the reader holds it (Windows); readers close it when they go on to the next generation, so a
	--           writer's deletion of old generations is not in the way, and the session folder is cleaned at game start.
	-- A file shorter than what was consumed, or one whose first bytes changed, was replaced (only a cleaned folder does that): the
	-- reader starts again at its first byte.
	function C.newReader(path, opts)
		opts = opts or {}
		local g = C.newestGen(path)
		local r = { base = path, gen = g, path = C.genPath(path, g), consumed = 0, partial = "", dropped = 0, keep = opts.keep == true }
		local at = opts.at
		if type(at) == "string" then at = C.parsePos(at) end
		if type(at) == "table" then
			r.gen, r.path, r.consumed = at.gen, C.genPath(path, at.gen), at.offset
		elseif (opts.start or "oldest") == "oldest" then
			if C.IO then
				while g > 1 and C.fileSize(C.genPath(path, g - 1)) do g = g - 1 end
			end
			r.gen, r.path = g, C.genPath(path, g)
		elseif opts.start == "end" then
			local size, last, rotG, rotSkip = tailState(r.path)
			if size then
				r.consumed = size
				if size > 0 and last ~= NL then
					r.skipping = true   -- the rest of a line that was being written is history too
				elseif rotG then
					C.readerNext(r, rotG, rotSkip)
				end
			end
		end
		return r
	end

	-- Where the lines handed out so far end: the next line starts here.
	function C.readerPos(r)
		return { gen = r.gen, offset = r.consumed }
	end

	-- On to generation `gen` (the one a `rotate` line named, or the newest after a deletion), after its first `skip` bytes.
	function C.readerNext(r, gen, skip)
		if r.f then pcall(function() r.f:close() end) r.f = nil end
		r.gen, r.path, r.consumed, r.partial, r.head, r.nextGen, r.nextSkip, r.skipping =
			gen, C.genPath(r.base or r.path, gen), skip or 0, "", nil, nil, nil, false
	end

	-- Pure part of the reader: feeds the bytes that follow what the reader consumed, returns the complete records in file order. Each
	-- record carries its place: rec.gen, rec.start (the offset of its first byte) and rec.after (the offset after its newline). A
	-- trailing line without newline waits for the rest. A `rotate` line ends the feed: the writer wrote nothing after it in this
	-- file, and r.nextGen / r.nextSkip say where the log goes on.
	function C.readerFeed(r, text)
		local out = {}
		local buf = r.partial .. text
		local base = r.consumed or 0   -- the file offset of buf's first byte
		local pos = 1
		if r.skipping then
			local i = buf:find(NL, 1, true)
			if not i then
				r.consumed, r.partial = base + #buf, ""
				return out
			end
			pos, r.skipping = i + 1, false
		end
		while true do
			local i = buf:find(NL, pos, true)
			if not i then break end
			local line = buf:sub(pos, i - 1)
			local start = base + pos - 1
			pos = i + 1
			if line:sub(-1) == CR then line = line:sub(1, -2) end
			if #line > 0 then
				local rec = C.parseLine(line)
				if rec then
					local nextGen, skip = C.rotateTarget(rec)
					if nextGen then
						r.nextGen, r.nextSkip, r.partial, r.consumed = nextGen, skip, "", base + pos - 1
						return out
					end
					rec.gen, rec.start, rec.after = r.gen, start, base + pos - 1
					out[#out + 1] = rec
				else
					r.dropped = r.dropped + 1
				end
			end
		end
		r.consumed, r.partial = base + pos - 1, buf:sub(pos)
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
						C.readerNext(r, newest, 0)
						return {}, true
					end
					r.consumed, r.partial, r.head, r.skipping = 0, "", nil, false
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
		local have = r.consumed + #r.partial
		if size < have then
			r.consumed, r.partial, r.head, r.skipping, have = 0, "", nil, false, 0
		elseif size > have and have > 0 and r.head then
			-- grown, but is it still the same file? A file truncated and written again past the old length starts with other bytes
			f:seek("set", 0)
			if f:read(#r.head) ~= r.head then r.consumed, r.partial, r.head, r.skipping, have = 0, "", nil, false, 0 end
		end
		local out, switched = {}, false
		if size > have then
			f:seek("set", have)
			local chunk = f:read(size - have) or ""
			if have == 0 then r.head = chunk:sub(1, HEAD) end
			out = C.readerFeed(r, chunk)
			if r.nextGen then
				if not r.keep then f:close() end
				f = nil
				C.readerNext(r, r.nextGen, r.nextSkip)
				switched = true
			end
		end
		if f and not r.keep then f:close() end
		return out, switched
	end

	-- Reads what the log gained since the last poll, through the generations it went on to.
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

	-- ---------------------------------------------------------------- writer

	-- Writer: appends records to a log, each line with one open, one write and one close. Made once per Lua state and kept.
	-- ONE THREAD PER FILE: the C runtime's append ("ab") is a seek to the end and then a write, two steps, so two Lua states appending
	-- to one file at the same moment from two threads overwrite or split each other's lines (tests/lua/test_link_files.py measures it).
	-- Writers on one thread take turns, and every line lands whole. A file that more than one Lua state may write at once (the native
	-- control files: the game runs the simulation half in more than one Lua state) has one file per writer instead (C.newOwnWriter).
	-- `opts.rotate` / `opts.keep` change the generation size and how many generations are kept (tests).
	function C.newWriter(path, from, opts)
		opts = opts or {}
		local g = C.newestGen(path)
		local w = { base = path, gen = g, path = C.genPath(path, g), from = from, lines = 0, failed = 0,
			rotate = opts.rotate or C.ROTATE_BYTES, keep = opts.keep or C.KEEP_GENERATIONS }
		w.size = C.fileSize(w.path) or 0
		return w
	end

	-- Ends the writer's generation with a `rotate` line and goes on in the next (the pointer is written, the generation `keep`
	-- back is deleted). False when the `rotate` line could not be written: the writer stays where it is.
	function C.writerRotate(w)
		local nextG = w.gen + 1
		if not C.appendFile(w.path, C.formatLine(C.ROTATE_KIND, w.from, C.serialize({ gen = nextG, skip = 0 }))) then return false end
		C.writeFile(C.genPointer(w.base), C.integerText(nextG) .. NL)
		if nextG > w.keep then pcall(C.remove, C.genPath(w.base, nextG - w.keep)) end
		w.gen, w.path, w.size = nextG, C.genPath(w.base, nextG), 0
		return true
	end

	-- Appends one record. The append open tells the file's size: when it is not what this writer left (another writer object of this
	-- log wrote meanwhile on this thread, e.g. a reloaded script's), the log may have gone on to a newer generation, and the writer
	-- follows the pointer before it writes, so no line lands after another writer's `rotate` line.
	function C.writerSend(w, kind, payload)
		local ok, text = pcall(C.serialize, payload or {})
		if not ok then return false, text end
		if not C.IO then
			w.failed = w.failed + 1
			return false, "no file access"
		end
		local f = C.IO.open(w.path, "ab")
		local size = f and f:seek("end") or 0
		if f and size ~= w.size then
			local g = C.newestGen(w.base)
			if g > w.gen then
				f:close()
				w.gen, w.path = g, C.genPath(w.base, g)
				f = C.IO.open(w.path, "ab")
				size = f and f:seek("end") or 0
			end
		end
		if f and w.rotate and size > 0 and size >= w.rotate then
			f:close()
			C.writerRotate(w)
			f = C.IO.open(w.path, "ab")
			size = f and f:seek("end") or 0
		end
		if f then
			local line = C.formatLine(kind, w.from, text)
			f:write(line)
			f:close()
			w.size, w.lines = size + #line, w.lines + 1
			return true
		end
		w.failed = w.failed + 1
		return false, "cannot append to " .. w.path
	end

	-- One file per writer (the native control files, docs/NATIVE_LINK.md section 3): the writer names its own file
	-- <family>.w<id>.log (family: a path stem such as <dir>\native_ctl.sim), with an id unique without any coordination between writers:
	-- the address of the writer's own table (two writers alive at once in one process never share an address), then the C runtime's
	-- clock in ms, the wall clock in seconds and a per-state count, all as hex digits. No rename, no exclusive open: the game's Lua has
	-- neither (real run 2026-10-18 02:33: no os.rename). A name whose file is there already (a dead writer's table address reused in the
	-- same millisecond) is skipped. The module finds the files by listing the folder. Returns the writer (w.id), or nil and why.
	local ownCount = 0
	function C.newOwnWriter(family, from, opts)
		if not C.IO then return nil, "no file access" end
		local w = C.newWriter(family .. ".w0.log", from, opts)   -- the table whose address names the file
		local addr = C.addressOf(w):lower():gsub("^0x", ""):gsub("[^0-9a-f]", "")
		local okC, c = pcall(C.clock)
		local okT, t = pcall(C.time)
		for _ = 1, 64 do
			ownCount = ownCount + 1
			local id = addr .. string.format("%x%x%x", math.floor((okC and tonumber(c) or 0) * 1000) % 1048576,
				(okT and tonumber(t) or 0) % 1048576, ownCount)
			local path = family .. ".w" .. id .. ".log"
			if not C.fileSize(path) then
				w.base, w.gen, w.path, w.size, w.id = path, 1, path, 0, id
				return w
			end
		end
		return nil, "no free control file name"
	end
end

return Link
