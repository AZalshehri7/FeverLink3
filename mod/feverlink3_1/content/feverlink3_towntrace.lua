-- Town trace: a read-only experiment that finds the FIRST tick at which two games' towns differ and what differed.
--
-- Off unless the game was started with FEVERLINK3_TOWN_TRACE=<comma list of town entity ids | all>. Nothing here
-- changes the game: only components are read. Each game writes `town_trace.txt` in its own live folder. Per simulation
-- step (200 ms tick), for every traced town, and ONLY when its state changed since the step before:
--
--   t=<tick> town=<id> n=<building count> h=<hash of the sorted building keys>
--   + <file>@x,y,z L=<level> D=<depth> P=<parcels> T=<timeBuilt>        a building appeared
--   - <file>@x,y,z L=.. D=.. P=.. T=..                                  a building is gone
--   ~ <file>@x,y,z L=1>2 D=.. P=.. T=..                                 same place, a field changed (old>new)
--
-- the town component values that influence growth (developmentActive, size factors, capacities, cargo needs, noise,
-- pollution; the ratings and the population live in the game script's state, which the sim script cannot read):
--
--   t=<tick> town=<id> info dev=1 size=.. cap=.. scale=.. needs=.. weights=.. noise=.. poll=.. emis=..
--
-- written at every change and at least every INFO_EVERY ticks (5 s of game time); and the commands the sim script saw
-- applied (the other writers of town buildings: TownUpdateSize/CargoNeeds/DevelopAt, bulldoze, roads...):
--
--   t=<tick> cmd <name> [town=<id>] [pi=<0|1>]
--
-- Keys never contain entity ids (those differ between the games); the town id names the lane, as in the checksum's
-- `townBuildings:<id>` part. With `all` the world is sampled every ALL_EVERY ticks to keep the cost low and the baseline
-- lists no buildings. Lines are buffered and written in one append per flush (a second of wall time or a few hundred
-- lines), so the sim never waits on the disk; a failed write drops the buffer, it never raises.
-- tools/gametest/compare_town_trace.py reads this file together with growth.log.

local M = {}

M.FILE = "town_trace.txt"
M.INFO_EVERY = 25 -- ticks (5 s of game time) between two info lines of an unchanged town
M.ALL_EVERY = 5 -- ticks between two scans in `all` mode
M.FLUSH_LINES = 200
M.FLUSH_SECONDS = 1.0
M.BUFFER_MAX = 20000

-- "all" or "12,34": {enabled, all, ids = {[id] = true}, list = {ids in order}}
function M.parseSpec(text)
	local spec = { enabled = false, all = false, ids = {}, list = {} }
	if type(text) ~= "string" then return spec end
	for part in text:gmatch("[^,]+") do
		part = part:gsub("^%s+", ""):gsub("%s+$", "")
		if part:lower() == "all" then
			spec.all, spec.enabled = true, true
		elseif part:match("^%d+$") then
			local id = tonumber(part)
			if not spec.ids[id] then
				spec.ids[id] = true
				spec.list[#spec.list + 1] = id
			end
			spec.enabled = true
		end
	end
	return spec
end

-- A deterministic one-line text of a value read from a component (numbers rounded, tables in key order).
local function flat(v, depth)
	depth = depth or 0
	local t = type(v)
	if t == "number" then
		if v == math.floor(v) and math.abs(v) < 1e15 then return string.format("%d", v) end
		return string.format("%.4g", v)
	elseif t == "boolean" then
		return v and "1" or "0"
	elseif t == "table" then
		if depth > 4 then return "..." end
		local out, n = {}, #v
		if n > 0 then
			for i = 1, n do out[#out + 1] = flat(v[i], depth + 1) end
			return "[" .. table.concat(out, ",") .. "]"
		end
		local keys = {}
		for k in pairs(v) do keys[#keys + 1] = k end
		if #keys == 0 then return "[]" end
		table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
		for _, k in ipairs(keys) do out[#out + 1] = tostring(k) .. ":" .. flat(v[k], depth + 1) end
		return "{" .. table.concat(out, ",") .. "}"
	end
	return tostring(v)
end
M.flat = flat

-- Accessors on the real (or mocked) engine api; `entitiesWith(name)` returns the sorted ids with a component.
function M.apiAccessors(api, entitiesWith)
	local CT = api.type.ComponentType
	local acc = {}
	function acc.buildings() return entitiesWith("TOWN_BUILDING") end
	function acc.building(e)
		local tb = api.engine.getComponent(e, CT.TOWN_BUILDING)
		if not tb then return nil end
		local parcels = 0
		pcall(function() parcels = #tb.parcels end)
		return { town = tb.town, construction = tb.construction, level = tb.level, depth = tb.depth, parcels = parcels, timeBuilt = tb.timeBuilt }
	end
	function acc.construction(c)
		local con = c and api.engine.getComponent(c, CT.CONSTRUCTION)
		if not con then return nil end
		local m = con.transf
		return { file = con.fileName, x = m and m[13] or 0, y = m and m[14] or 0, z = m and m[15] or 0 }
	end
	function acc.town(e)
		local tc = api.engine.getComponent(e, CT.TOWN)
		if not tc then return nil end
		local out = {}
		for _, k in ipairs({ "developmentActive", "sizeFactors", "initialLandUseCapacities", "capacityScalingFactor", "cargoNeeds",
				"levelToCapacityDistributionWeights", "noise", "pollution" }) do
			local ok, v = pcall(function() return tc[k] end)
			if ok then out[k] = v end
		end
		local ok, emis = pcall(function() return api.engine.util.getTownEmissionDB(e) end)
		if ok then out.emission = emis end
		return out
	end
	function acc.towns() return entitiesWith("TOWN") end
	return acc
end

local function keyOf(file, x, y, z)
	return string.format("%s@%.1f,%.1f,%.1f", tostring(file), x or 0, y or 0, z or 0)
end

local function attrText(b)
	return string.format("L=%s D=%s P=%s T=%s", tostring(b.level), tostring(b.depth), tostring(b.parcels), tostring(b.timeBuilt))
end

-- `opts`: spec (text of the env var), acc (accessors), write (function(text) -> boolean, one append), hash
-- (function(table) -> string), clock (function() -> seconds of wall time, default os.clock)
function M.new(opts)
	local self = {}
	local spec = M.parseSpec(opts.spec)
	self.spec = spec
	if not spec.enabled then
		function self.step() end
		function self.noteCommand() end
		function self.flush() end
		self.enabled = false
		return self
	end
	self.enabled = true
	local acc, write, hash = opts.acc, opts.write, opts.hash
	local clock = opts.clock or os.clock
	local buf, lastFlush = {}, clock()
	local cache = {} -- building entity -> { sig, key } (never logged)
	local prev = {} -- town -> { n, h, items = { key -> attrText }, order = { key... } }
	local infoPrev = {} -- town -> { text, tick }
	local lastTick = nil
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

	local function buildingKey(e, b)
		local sig = tostring(b.construction) .. "|" .. tostring(b.level) .. "|" .. tostring(b.depth) .. "|" .. tostring(b.parcels) .. "|" .. tostring(b.timeBuilt)
		local c = cache[e]
		if c and c.sig == sig then return c.key end
		local con = acc.construction(b.construction)
		local key = keyOf(con and con.file, con and con.x, con and con.y, con and con.z)
		cache[e] = { sig = sig, key = key }
		return key
	end

	-- the sorted building list of every wanted town: town -> { keys = { "key" sorted }, attrs = { key -> text } }
	local function scan()
		local byTown, seen = {}, {}
		local wanted = spec.all and function() return true end or function(t) return spec.ids[t] end
		for _, e in ipairs(acc.buildings()) do
			seen[e] = true
			local b = acc.building(e)
			if b and wanted(b.town) then
				local l = byTown[b.town]
				if not l then l = { entries = {} }; byTown[b.town] = l end
				l.entries[#l.entries + 1] = { key = buildingKey(e, b), attr = attrText(b) }
			end
		end
		for e in pairs(cache) do
			if not seen[e] then cache[e] = nil end
		end
		for _, l in pairs(byTown) do
			table.sort(l.entries, function(a, b)
				if a.key ~= b.key then return a.key < b.key end
				return a.attr < b.attr
			end)
			l.keys, l.attrs, l.flat = {}, {}, {}
			local dup = {}
			for _, en in ipairs(l.entries) do
				local k = en.key
				dup[k] = (dup[k] or 0) + 1
				if dup[k] > 1 then k = k .. "#" .. dup[k] end
				l.keys[#l.keys + 1] = k
				l.attrs[k] = en.attr
				l.flat[#l.flat + 1] = k .. " " .. en.attr
			end
			l.entries = nil
		end
		return byTown
	end

	local function infoOf(town)
		local ok, t = pcall(acc.town, town)
		if not ok or not t then return nil end
		return string.format("dev=%s size=%s cap=%s scale=%s needs=%s weights=%s noise=%s poll=%s emis=%s",
			flat(t.developmentActive), flat(t.sizeFactors), flat(t.initialLandUseCapacities), flat(t.capacityScalingFactor),
			flat(t.cargoNeeds), flat(t.levelToCapacityDistributionWeights), flat(t.noise), flat(t.pollution), flat(t.emission))
	end

	local function diff(tick, town, old, new)
		for _, k in ipairs(new.keys) do
			local o = old.attrs[k]
			if o == nil then
				emit("+ " .. k .. " " .. new.attrs[k])
			elseif o ~= new.attrs[k] then
				-- same place, other fields: print old>new for the fields that changed
				local parts, oldF, newF = {}, {}, {}
				for name, val in o:gmatch("(%u)=(%S*)") do oldF[name] = val end
				for name, val in new.attrs[k]:gmatch("(%u)=(%S*)") do newF[#newF + 1] = { name, val } end
				for _, nf in ipairs(newF) do
					local ov = oldF[nf[1]]
					parts[#parts + 1] = nf[1] .. "=" .. ((ov ~= nf[2]) and (tostring(ov) .. ">" .. nf[2]) or nf[2])
				end
				emit("~ " .. k .. " " .. table.concat(parts, " "))
			end
		end
		for _, k in ipairs(old.keys) do
			if new.attrs[k] == nil then emit("- " .. k .. " " .. old.attrs[k]) end
		end
	end

	-- One simulation step. Does nothing twice for the same tick, and (in `all` mode) only every ALL_EVERY ticks.
	function self.step(tick)
		if tick == lastTick then return end
		lastTick = tick
		if not started then
			started = true
			emit(string.format("# town_trace start tick=%s spec=%s", tostring(tick), tostring(opts.spec)))
		end
		if spec.all and tick % M.ALL_EVERY ~= 0 then
			self.flush()
			return
		end
		local byTown = scan()
		local towns = {}
		if spec.all then
			for t in pairs(byTown) do towns[#towns + 1] = t end
			for t in pairs(prev) do if not byTown[t] then towns[#towns + 1] = t end end
		else
			for _, t in ipairs(spec.list) do towns[#towns + 1] = t end
		end
		table.sort(towns)
		local last
		for _, town in ipairs(towns) do
			if town ~= last then
				last = town
				local cur = byTown[town] or { keys = {}, attrs = {}, flat = {} }
				cur.n = #cur.keys
				cur.h = hash(cur.flat)
				local old = prev[town]
				local changed = old == nil or old.n ~= cur.n or old.h ~= cur.h
				if changed then
					emit(string.format("t=%s town=%s n=%d h=%s%s", tostring(tick), tostring(town), cur.n, tostring(cur.h), old == nil and " base" or ""))
					if old ~= nil then
						diff(tick, town, old, cur)
					elseif not spec.all then
						for _, k in ipairs(cur.keys) do emit("+ " .. k .. " " .. cur.attrs[k]) end
					end
					prev[town] = { n = cur.n, h = cur.h, keys = cur.keys, attrs = cur.attrs }
				end
				local info = infoOf(town)
				if info then
					local ip = infoPrev[town]
					if ip == nil or ip.text ~= info or tick - ip.tick >= M.INFO_EVERY then
						emit(string.format("t=%s town=%s info %s", tostring(tick), tostring(town), info))
						infoPrev[town] = { text = info, tick = tick }
					end
				end
			end
		end
		self.flush()
	end

	-- A command the sim script saw applied; `town` and `pi` (player initiated) when known.
	function self.noteCommand(tick, name, town, pi)
		local s = string.format("t=%s cmd %s", tostring(tick), tostring(name))
		if town ~= nil then s = s .. " town=" .. tostring(town) end
		if pi ~= nil then s = s .. " pi=" .. (pi and "1" or "0") end
		emit(s)
		self.flush()
	end

	return self
end

return M
