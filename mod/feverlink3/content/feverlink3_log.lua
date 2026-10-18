-- FeverLink3 log: the ONE way the mod writes its logs and diagnostic files. Log.install(C) adds to the common module:
--   C.logger(tag)      mod.log lines (and the game's own log through the official debugPrint)
--   C.tracer(state)    trace.log lines (FEVERLINK3_TRACE, default on in test builds; docs/TRACE.md)
--   C.dump(name, text) appends to a diagnostic file of the session folder (raw_builds.txt, build_trace.txt, the town trace, dev output)
--   C.dumpReplace(name, text)  writes a diagnostic file whole (detail_<time>_<id>.txt)
--   C.appendCapped     the generation rule of mod.log / trace.log
-- The files are written through the link module's primitives (feverlink3_link.lua: C.appendBelow, C.appendFile, C.writeFile,
-- C.remove), the time of day comes from C.date: the official API has no file access and no wall clock (that module says why). Logging
-- is debugging only; the owner allowed it to keep its own files (2026-10-18). Nothing here raises into its caller.

local Log = {}

local NL = string.char(10)

function Log.install(C)
	C.LOG = "mod.log"

	-- Appends to a log that more than one Lua state writes (mod.log, trace.log: people read them, no program follows them): a
	-- new generation starts when the current FILE holds `cap` bytes, whoever wrote them, so the limit holds across both halves
	-- and across reloads. The generation before `keep` back is deleted (nil: none); after `maxGen` generations nothing more is
	-- written (nil: no end). Returns true when written, false when not, "full" when the log reached `maxGen`.
	local capGen = {}   -- per Lua state: log -> the generation it last wrote to
	function C.appendCapped(base, text, cap, keep, maxGen)
		if not C.IO then return false end
		local g = capGen[base] or C.newestGen(base)
		local done = C.appendBelow(C.genPath(base, g), text, cap)
		if done == "over" then
			local newest = C.newestGen(base)
			local nextG = math.max(g + 1, newest)   -- the other half may have gone on already
			if maxGen and nextG > maxGen then return "full" end
			if newest < nextG then
				C.writeFile(C.genPointer(base), C.integerText(nextG) .. NL)
				if keep and nextG > keep then pcall(C.remove, C.genPath(base, nextG - keep)) end
			end
			g = nextG
			done = C.appendFile(C.genPath(base, g), text)
		end
		if done ~= true then return false end
		capGen[base] = g
		return true
	end

	-- a logger writing "<clock> [<tag>] <message>" lines to mod.log in the session folder (and to the game's own log). Both halves
	-- write it: it goes on in mod.2.log, mod.3.log ... every C.LOG_BYTES (whoever wrote them), and only the newest C.LOG_KEEP files
	-- are kept (mod.gen names the newest).
	C.LOG_BYTES, C.LOG_KEEP = 8 * 1024 * 1024, 4
	function C.logger(tag)
		return function(msg)
			local line = "[FeverLink3 " .. tag .. "] " .. tostring(msg)
			pcall(function() if debugPrint then debugPrint(line) end end)
			if C.DIR and C.IO then
				pcall(function() C.appendCapped(C.path(C.LOG), C.date("%H:%M:%S") .. " " .. line .. NL, C.LOG_BYTES, C.LOG_KEEP) end)
			end
		end
	end

	-- Trace log (FEVERLINK3_TRACE, DEFAULT ON in test builds, FEVERLINK3_TRACE=0 switches it off): everything a real run needs to be explained
	-- from the logs alone, in trace.log of the session folder (docs/TRACE.md). One line per event: "<HH:MM:SS> +<cpu s> <state> <tag> <text>".
	-- Built for per-build and per-round events, never per tick. The file rolls over to trace.2.log, trace.3.log ... every 8 MB (nothing is
	-- deleted); after TRACE_MAX_SEGMENTS segments it stops and says so once. The size is the file's, so the limit holds for both halves
	-- together and across reloads (it was counted per Lua state and started again at a reload).
	C.TRACE = true
	pcall(function() C.TRACE = C.env("FEVERLINK3_TRACE") ~= "0" end)
	local TRACE_SEGMENT, TRACE_MAX_SEGMENTS = 8 * 1024 * 1024, 12
	local traceState = { stopped = false }
	function C.tracer(state)
		return function(tag, text)
			if not C.TRACE or traceState.stopped or not (C.DIR and C.IO) then return end
			pcall(function()
				local line = string.format("%s +%.3f %s %s %s", C.date("%H:%M:%S"), C.clock(), state, tag, tostring(text))
				if #line > 200000 then line = line:sub(1, 200000) .. " ...(cut)" end
				local base = C.path("trace.log")
				if C.appendCapped(base, line .. NL, TRACE_SEGMENT, nil, TRACE_MAX_SEGMENTS) == "full" then
					traceState.stopped = true
					C.appendFile(C.genPath(base, TRACE_MAX_SEGMENTS), C.date("%H:%M:%S") .. " TRACE FULL: no more lines" .. NL)
				end
			end)
		end
	end

	-- Diagnostic files of the session folder: true when written. Never raises.
	function C.dump(name, text)
		if not C.DIR then return false end
		local ok, done = pcall(C.appendFile, C.path(name), text)
		return ok and done == true
	end
	function C.dumpReplace(name, text)
		if not C.DIR then return false end
		local ok, done = pcall(C.writeFile, C.path(name), text)
		return ok and done == true
	end
end

return Log
