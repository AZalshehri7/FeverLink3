-- Live vs loaded experiment: observe-only log of what a LIVE game and a freshly LOADED game see differently.
--
-- Off unless the game was started with FEVERLINK3_LIVE_VS_LOADED=1. Nothing here changes the game: queries only, each
-- under pcall, a hard cap of work per simulation step, and five internal errors switch it off (like the town trace).
-- Each game writes `live_vs_loaded.txt` next to `town_trace.txt`. All lines start with the game tick (200 ms steps) so
-- two games' files can be paired by tick:
--
--   # lvl start tick=<t> towns=<spec>
--   t=<tick> resume why=<start|gap> window=<last tick of the sampling window>
--   L1 (every 12 ticks, per traced town; the traced towns are those of FEVERLINK3_TOWN_TRACE, all towns for `all`/unset)
--   t=<tick> L1 town=<id> lu=<bits32 of getTownLineUsage> lun=<the number> sd=<supply/limit per land use 0,1,2, sums over the
--            cargo types in id order> xp=<experience> lvl=<level>
--   L2 (at a resume and every 60 ticks (tick % 60 == 0) for 5 game minutes after it)
--   t=<tick> L2 count n=<SimPersonSystem.getCount()>
--   t=<tick> L2 dest   id=<destination> n=<persons> o=<ordered hash> s=<sorted hash>     20 lowest destination ids
--   t=<tick> L2 tn     id=<network>     n=.. o=.. s=..                                   10 lowest transport networks
--   t=<tick> L2 line   id=<line>        n=.. o=.. s=..                                   3 lowest lines
--   L4 (once) t=<tick> L4 ...  whether the game scripts see the mod's deterministic math.random (see noteRandom)
--
-- Reading: the ordered hash `o` follows the container's order, the sorted hash `s` only its set. s equal and o different
-- = container order differs; s different = another set. Hashes are two 32 bit folds (16 hex digits) of the entity ids.
--
-- A "resume" is the first step of this Lua state (a freshly loaded game, a new process) or a step after a wall clock gap of
-- GAP_SECONDS or more (a paused game going on: the resync pause of a live host). The sim script is not told about a
-- resume, so this is the only place it can be noticed.
--
-- XP and level come from the town game script's state through the GAME_SCRIPT component (as town_cargo_util.getTownCargoState
-- reads it); if that read fails the fields print `-` (and the source is disabled after two failures).

local M = {}

M.FILE = "live_vs_loaded.txt"
M.L1_EVERY = 12 -- ticks
M.L2_EVERY = 60 -- ticks (absolute: tick % L2_EVERY == 0)
M.L2_WINDOW = 1500 -- ticks after a resume (5 game minutes)
M.DESTINATIONS, M.NETWORKS, M.LINES = 20, 10, 3
M.JOBS_PER_STEP = 6 -- hard cap of API reading jobs per step
M.QUEUE_MAX = 400
M.GAP_SECONDS = 5
M.ERRORS_MAX = 5
M.SOURCE_FAILS_MAX = 2 -- an API source that fails this often is disabled (logged), without counting as a trace error
M.FLUSH_LINES = 200
M.FLUSH_SECONDS = 1.0
M.BUFFER_MAX = 20000

local TWO32 = 4294967296

local function mul32(a, b)
	local ah, al = math.floor(a / 65536), a % 65536
	return ((ah * b) % 65536 * 65536 + al * b) % TWO32
end

-- two order sensitive folds of a list of numbers: 16 hex digits
function M.fold(list)
	local h1, h2 = 2166136261, 5381
	for i = 1, #list do
		local v = list[i]
		if type(v) ~= "number" then v = 0 end
		v = v % TWO32
		h1 = (mul32(h1, 16777619) + v + 1) % TWO32
		h2 = (mul32(h2, 1664525) + v * 2 + 7) % TWO32
	end
	return string.format("%08x%08x", h1, h2)
end

-- ordered and sorted hash of a list of entity ids
function M.hashes(list)
	local sorted = {}
	for i = 1, #list do sorted[i] = list[i] end
	table.sort(sorted, function(a, b)
		if type(a) == "number" and type(b) == "number" then return a < b end
		return tostring(a) < tostring(b)
	end)
	return M.fold(list), M.fold(sorted)
end

-- "all" / "12,34" / nil -> { all = bool, ids = { [id] = true } } (same text as FEVERLINK3_TOWN_TRACE)
function M.parseTowns(text)
	local spec = { all = true, ids = {} }
	if type(text) ~= "string" then return spec end
	local any = false
	for part in text:gmatch("[^,]+") do
		part = part:gsub("^%s+", ""):gsub("%s+$", "")
		if part:lower() == "all" then
			spec.all = true
			return spec
		elseif part:match("^%d+$") then
			spec.ids[tonumber(part)] = true
			any = true
		end
	end
	spec.all = not any
	return spec
end

-- The common module's integral number text (feverlink3_common.lua C.integerText: "%d" raises above 2^31 in the game's Lua); this module
-- also runs where the common module is not loadable (its own tests), then "%.0f", which is what the helper prints.
local integerText, commonClock, commonTime
do
	local okC, C = pcall(ug_require, "feverlink3_common.lua")
	if not okC then okC, C = pcall(ug_require, "feverlink3::/feverlink3_common.lua") end
	integerText = okC and type(C) == "table" and C.integerText or function(n) return string.format("%.0f", n) end
	-- the real clock and the wall clock of the link module (feverlink3_link.lua); a caller without the common module passes its own
	commonClock = okC and type(C) == "table" and C.clock or nil
	commonTime = okC and type(C) == "table" and C.time or nil
end

local function num(v)
	if type(v) ~= "number" then return tostring(v) end
	if v ~= v then return "nan" end
	if v == math.floor(v) and math.abs(v) < 1e15 then return integerText(v) end
	return string.format("%.17g", v)
end

-- the elements of a native sequence or table as a Lua list of numbers (indexed from 1); errors raise to the caller's pcall
local function listOf(v, cap)
	local out = {}
	if v == nil then return out end
	local n = #v
	if cap and n > cap then n = cap end
	for i = 1, n do out[i] = v[i] end
	return out
end

-- keys of a native map or table, as a list
local function keysOf(v)
	local out = {}
	for k in pairs(v) do out[#out + 1] = k end
	return out
end

local function lowest(list, n)
	local nums = {}
	for _, v in ipairs(list) do if type(v) == "number" then nums[#nums + 1] = v end end
	table.sort(nums)
	local out = {}
	for i = 1, math.min(n, #nums) do out[i] = nums[i] end
	return out
end
M.lowest = lowest

-- Accessors on the real (or a mocked) engine api. Every function may raise: the caller guards.
function M.apiAccessors(api, entitiesWith, bits32)
	local CT = api.type.ComponentType
	local acc = {}
	function acc.towns() return entitiesWith("TOWN") end
	function acc.networks() return entitiesWith("TRANSPORT_NETWORK") end
	function acc.lines() return entitiesWith("LINE") end
	function acc.lineUsage(e) return api.engine.util.town.getTownLineUsage(e) end
	-- per land use, as getCargoSupplyAndLimit types it (`landUse? : LandUseType`): this game's api.type.enum.LandUseType values in the
	-- documented order RESIDENTIAL, COMMERCIAL, INDUSTRIAL (tf3-docs api/type, api/engine/system); raises without the enum
	function acc.supplyDemand(e)
		local out = {}
		local enum = api.type["enum"].LandUseType
		for _, name in ipairs({ "RESIDENTIAL", "COMMERCIAL", "INDUSTRIAL" }) do
			local lu = enum[name]
			if lu == nil then error("api.type.enum.LandUseType has no " .. name, 0) end
			local all = api.engine.system.townBuildingSystem.getCargoSupplyAndLimit(e, lu)
			local ids = {}
			for id in pairs(all) do ids[#ids + 1] = id end
			table.sort(ids, function(a, b) return tostring(a) < tostring(b) end)
			local s, d = 0, 0
			for _, id in ipairs(ids) do s = s + all[id][1]; d = d + all[id][2] end
			out[#out + 1] = { s, d }
		end
		return out
	end
	-- experience and level of the town game script's state (town_cargo.gs), as town_cargo_util.getTownCargoState reads it
	function acc.xp(e)
		local gs = api.engine.system.gameScriptSystem.getEntityForGameScript("::/game_mechanics/towns/town_cargo.gs")
		-- getComponent wants "a valid, existing entity" (tf3-docs api/engine): a game without that game script answers no entity here
		if type(gs) ~= "number" or gs < 0 or not api.engine.entityExists(gs) then return nil end
		local comp = api.engine.getComponent(gs, CT.GAME_SCRIPT)
		local data
		local native = comp and comp.state_native
		if native then
			local node = native:findPath({ "townState", e })
			if node then data = node:asTable() end
		end
		if not data and comp and comp.state and comp.state.townState then data = comp.state.townState[e] end
		if not data then return nil end
		return data.experience, data.level
	end
	local sps = function() return api.engine.system.simPersonSystem end
	function acc.count() return sps().getCount() end
	function acc.destinations() return keysOf(sps().getDestination2SpMap()) end
	function acc.forDestination(d) return listOf(sps().getSimPersonsForDestination(d)) end
	function acc.forNetwork(tn) return listOf(sps().getSimPersonsAtTerminalForTransportNetwork(tn)) end
	function acc.forLine(l) return listOf(sps().getSimPersonsForLine(l)) end
	return acc
end

-- opts: enabled (bool), towns (text of FEVERLINK3_TOWN_TRACE), acc, write(text) -> boolean, bits32(number) -> hex text,
-- clock() -> seconds of wall time for the flush (default C.clock), wall() -> whole seconds for gap detection (default C.time),
-- randomProbe() -> table of facts for L4 (optional)
function M.new(opts)
	local self = { enabled = false, errors = 0 }
	if not opts.enabled then
		function self.step() end
		function self.flush() end
		function self.noteResume() end
		return self
	end
	self.enabled = true
	local acc, write = opts.acc, opts.write
	local clock = opts.clock or commonClock or function() return 0 end
	local wall = opts.wall or commonTime or function() return 0 end
	local bits32 = opts.bits32 or function(x) return num(x) end
	local townSpec = M.parseTowns(opts.towns)

	local buf, lastFlush = {}, clock()
	local queue, qHead, qTail = {}, 1, 0 -- a FIFO with explicit ends (a table with leading holes has no reliable #)
	local lastTick, lastWall = nil, nil
	local windowEnd = nil
	local disabledSrc, srcFails = {}, {}
	local l4Done = false
	local started = false

	local function emit(line)
		if #buf < M.BUFFER_MAX then buf[#buf + 1] = line end
	end

	function self.flush(force)
		if #buf == 0 then return end
		if not force and #buf < M.FLUSH_LINES and clock() - lastFlush < M.FLUSH_SECONDS then return end
		lastFlush = clock()
		local text = table.concat(buf, "\n") .. "\n"
		buf = {}
		pcall(write, text)
	end

	-- call an API source under pcall; a source failing SOURCE_FAILS_MAX times is disabled with one comment line
	local function src(name, fn, ...)
		if disabledSrc[name] then return false end
		local ok, a, b = pcall(fn, ...)
		if ok then return true, a, b end
		srcFails[name] = (srcFails[name] or 0) + 1
		if srcFails[name] >= M.SOURCE_FAILS_MAX then
			disabledSrc[name] = true
			emit("# lvl source " .. name .. " disabled after " .. srcFails[name] .. " failures: " .. tostring(a))
		end
		return false
	end

	local function push(job)
		if qTail - qHead + 1 < M.QUEUE_MAX then qTail = qTail + 1; queue[qTail] = job end
	end

	local function queueEmpty() return qHead > qTail end

	local function l1Town(tick, town)
		local luText, luNum, sdText, xp, lvl = "-", "-", "-", "-", "-"
		local ok, lu = src("lineUsage", acc.lineUsage, town)
		if ok and type(lu) == "number" then luText, luNum = bits32(lu), num(lu) end
		local okS, sd = src("supplyDemand", acc.supplyDemand, town)
		if okS and type(sd) == "table" then
			local parts = {}
			for i, p in ipairs(sd) do parts[i] = num(p[1]) .. "/" .. num(p[2]) end
			sdText = table.concat(parts, ",")
		end
		local okX, e, l = src("xp", acc.xp, town)
		if okX then
			if e ~= nil then xp = num(e) end
			if l ~= nil then lvl = num(l) end
		end
		emit(string.format("t=%s L1 town=%s lu=%s lun=%s sd=%s xp=%s lvl=%s", tostring(tick), tostring(town), luText, luNum, sdText, xp, lvl))
	end

	local function l2Item(tick, kind, fn, id)
		local ok, list = src("L2" .. kind, fn, id)
		if not ok or type(list) ~= "table" then
			emit(string.format("t=%s L2 %s id=%s err", tostring(tick), kind, tostring(id)))
			return
		end
		local o, s = M.hashes(list)
		emit(string.format("t=%s L2 %s id=%s n=%d o=%s s=%s", tostring(tick), kind, tostring(id), #list, o, s))
	end

	-- one L2 sample: pick the ids (a job of its own), then one job per id
	local function l2Select(tick)
		emit(string.format("t=%s L2 sample", tostring(tick)))
		local okC, count = src("count", acc.count)
		emit(string.format("t=%s L2 count n=%s", tostring(tick), okC and num(count) or "-"))
		local okD, dests = src("destinations", acc.destinations)
		if okD and type(dests) == "table" then
			for _, d in ipairs(lowest(dests, M.DESTINATIONS)) do push({ kind = "dest", id = d, fn = acc.forDestination }) end
		end
		local okN, nets = src("networks", acc.networks)
		if okN and type(nets) == "table" then
			for _, n in ipairs(lowest(nets, M.NETWORKS)) do push({ kind = "tn", id = n, fn = acc.forNetwork }) end
		end
		local okL, lines = src("lines", acc.lines)
		if okL and type(lines) == "table" then
			for _, l in ipairs(lowest(lines, M.LINES)) do push({ kind = "line", id = l, fn = acc.forLine }) end
		end
	end

	local function l1Round()
		local okT, towns = src("towns", acc.towns)
		if not okT or type(towns) ~= "table" then return end
		local list = {}
		for _, t in ipairs(towns) do
			if townSpec.all or townSpec.ids[t] then list[#list + 1] = t end
		end
		table.sort(list)
		for _, t in ipairs(list) do push({ kind = "l1", id = t }) end
	end

	function self.noteResume(tick, why)
		windowEnd = tick + M.L2_WINDOW
		emit(string.format("t=%s resume why=%s window=%s", tostring(tick), tostring(why), tostring(windowEnd)))
		push({ kind = "select" })
	end

	-- L4: whether the game scripts see the deterministic math.random. A draw cannot be compared without consuming one of the
	-- stream (that would shift the draws of the real scripts), and the game scripts' own Lua states cannot be reached from
	-- here, so only facts that cost no draw are logged: `randomProbe` (the bridge) reports, for example, whether the math.random
	-- that a freshly loaded chunk sees is the installed override and the override's t and n.
	local l4Tries = 0
	local function l4(tick)
		if not opts.randomProbe then
			l4Done = true
			emit(string.format("t=%s L4 probe=none", tostring(tick)))
			return
		end
		local ok, facts = pcall(opts.randomProbe)
		-- nil = the override is not installed yet (the sim script installs it one step after this trace runs): ask again
		if ok and facts == nil and l4Tries < 50 then
			l4Tries = l4Tries + 1
			return
		end
		l4Done = true
		if not ok or type(facts) ~= "table" then
			emit(string.format("t=%s L4 probe=%s %s", tostring(tick), ok and "not-installed" or "error", tostring(facts)))
			return
		end
		local keys = {}
		for k in pairs(facts) do keys[#keys + 1] = k end
		table.sort(keys)
		local parts = {}
		for _, k in ipairs(keys) do parts[#parts + 1] = k .. "=" .. tostring(facts[k]) end
		emit(string.format("t=%s L4 %s", tostring(tick), table.concat(parts, " ")))
	end

	local function stepInner(tick)
		local now = wall()
		if not started then
			started = true
			emit(string.format("# lvl start tick=%s towns=%s", tostring(tick), tostring(opts.towns)))
			self.noteResume(tick, "start")
		elseif lastWall and now - lastWall >= M.GAP_SECONDS then
			self.noteResume(tick, "gap")
		end
		lastWall = now
		if not l4Done then l4(tick) end
		if tick % M.L1_EVERY == 0 and qTail - qHead + 1 < M.QUEUE_MAX / 4 then l1Round() end
		if windowEnd and tick <= windowEnd and tick % M.L2_EVERY == 0 and qTail - qHead + 1 < M.QUEUE_MAX / 2 then
			push({ kind = "select" })
		end
		local budget = M.JOBS_PER_STEP
		while budget > 0 and not queueEmpty() do
			local job = queue[qHead]
			queue[qHead] = nil
			qHead = qHead + 1
			budget = budget - 1
			if job.kind == "l1" then l1Town(tick, job.id)
			elseif job.kind == "select" then l2Select(tick)
			else l2Item(tick, job.kind, job.fn, job.id) end
		end
		if queueEmpty() then queue, qHead, qTail = {}, 1, 0 end
		self.flush()
	end

	-- One simulation step; does nothing twice for the same tick. Never raises; five internal errors switch it off.
	function self.step(tick)
		if not self.enabled or tick == lastTick then return end
		lastTick = tick
		local ok, err = pcall(stepInner, tick)
		if not ok then
			self.errors = self.errors + 1
			emit("# lvl error: " .. tostring(err))
			if self.errors >= M.ERRORS_MAX then
				self.enabled = false
				emit("# lvl switched off after " .. self.errors .. " errors")
			end
			self.flush(true)
		end
	end

	return self
end

return M
