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
-- Command payload trace (hypothesis H2; only with FEVERLINK3_TOWN_TRACE_CMDS=1 on top of the town list): the game's own
-- growth script (game_mechanics/towns/town_growth.script.tl) hands the three size factors to the native TownDeveloper
-- through api.cmd.makeTownUpdateSizeCmd (and the cargo needs through makeTownUpdateCargoNeedsCmd). If those numbers are
-- computed per game from per-process statistics they differ BEFORE the native step. The bridge wraps those two factories
-- (M.installCmdWrappers); the wrapper logs first, then calls the original with the very same arguments and returns what
-- it returned, so nothing changes for the game. Lines:
--
--   t=<tick> uc=<updateCount> mk <factory> town=<id> k=<n> flag=<0|1> f32=<hex,hex,hex> f64=<hex,hex,hex> v=<num,num,num>
--   t=<tick> uc=<updateCount> mk <factory> town=<id> k=<n> flag=<0|1> needs=<flat> nh=<hash>
--   t=<tick> town=<id> in k=<n> lu=<line usage> sd=<supply/demand per land use> dev=.. size=.. cap=.. scale=.. needs=.. bld=<n>:<hash>
--   t=<tick> town=<id> order pos=<index of the town> towns=<n>:<hash> cargos=<n>:<hash>
--
-- `f32`/`f64` are the exact bits of the three floats (the native side copies single precision floats; the double is what
-- the script computed), `v` the numbers with 17 digits, `k` the number of this factory call for this town in this tick,
-- `bld` the building count and hash of the last trace step. `order` is the order in which forEachEntityWithComponent(TOWN)
-- (the game script's loop, whose index decides which towns grow in which step) and pairs(cargoTypeRep.getAll()) (the
-- candidate order for new cargo needs) hand out their entities, hashed to one number; written when it changes and at
-- least every INFO_EVERY ticks. tools/gametest/compare_town_cmds.py pairs the two games' lines.
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

-- Exact text of numbers (17 digits) and of native sequences (tables or userdata indexed from 1), for the command lines.
local function num(v)
	if type(v) ~= "number" then return tostring(v) end
	if v ~= v then return "nan" end
	if v == math.floor(v) and math.abs(v) < 1e15 then return string.format("%d", v) end
	return string.format("%.17g", v)
end
M.num = num

local function seqOf(v, depth)
	depth = depth or 0
	local t = type(v)
	if t == "number" then return num(v) end
	if t == "boolean" then return v and "1" or "0" end
	if t == "nil" then return "nil" end
	if t ~= "table" and t ~= "userdata" then return t end
	if depth > 4 then return "..." end
	local okN, n = pcall(function() return #v end)
	if not okN or type(n) ~= "number" then return t end
	local out = {}
	for i = 1, math.min(n, 64) do
		local okI, item = pcall(function() return v[i] end)
		out[#out + 1] = okI and seqOf(item, depth + 1) or "?"
	end
	return "[" .. table.concat(out, ",") .. "]"
end
M.seq = seqOf

-- IEEE 754 bits by arithmetic, for a Lua without string.pack (5.1, 5.2, LuaJIT): exact in doubles, round to nearest even.
local function frexp(x)
	if math.frexp then return math.frexp(x) end
	local e = math.floor(math.log(x) / math.log(2)) + 1
	local m = x / 2 ^ e
	while m >= 1 do m = m / 2; e = e + 1 end
	while m < 0.5 do m = m * 2; e = e - 1 end
	return m, e
end
local function roundEven(y)
	local f = math.floor(y)
	local d = y - f
	if d > 0.5 or (d == 0.5 and f % 2 == 1) then return f + 1 end
	return f
end
local function softBits(x, expBits, mantBits)
	local bias = 2 ^ (expBits - 1) - 1
	local maxE = 2 ^ expBits - 1
	local sign = 0
	if x < 0 or (x == 0 and 1 / x < 0) then sign = 1; x = -x end
	local ef, frac
	if x ~= x then
		ef, frac = maxE, 2 ^ (mantBits - 1)
	elseif x == math.huge then
		ef, frac = maxE, 0
	elseif x == 0 then
		ef, frac = 0, 0
	else
		local m0, e0 = frexp(x)
		local E = e0 - 1
		if E + bias <= 0 then
			frac = roundEven(x / 2 ^ (1 - bias - mantBits))
			ef = 0
			if frac >= 2 ^ mantBits then ef, frac = 1, frac - 2 ^ mantBits end
		else
			frac = roundEven((2 * m0 - 1) * 2 ^ mantBits)
			ef = E + bias
			if frac >= 2 ^ mantBits then frac, ef = 0, ef + 1 end
			if ef >= maxE then ef, frac = maxE, 0 end
		end
	end
	if mantBits == 23 then
		return string.format("%08x", sign * 2 ^ 31 + ef * 2 ^ 23 + frac)
	end
	local hi = sign * 2 ^ 31 + ef * 2 ^ 20 + math.floor(frac / 2 ^ 32)
	return string.format("%08x%08x", hi, frac % 2 ^ 32)
end
M.softBits = softBits

local function hexOf(fmt, x)
	if type(x) ~= "number" then return "-" end
	if string.pack then
		local ok, packed = pcall(string.pack, fmt, x)
		if ok then return (packed:gsub(".", function(c) return string.format("%02x", c:byte()) end)) end
	end
	local ok, s = pcall(softBits, x, fmt == ">f" and 8 or 11, fmt == ">f" and 23 or 52)
	return ok and s or "?"
end
-- the bits of a number as a float (8 hex digits) and as a double (16)
function M.bits32(x) return hexOf(">f", x) end
function M.bits64(x) return hexOf(">d", x) end

-- the game script's factories this trace wraps
M.CMD_FACTORIES = { makeTownUpdateSizeCmd = "size", makeTownUpdateCargoNeedsCmd = "needs" }
M.ERRORS_MAX = 5

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
	-- inputs of the command lines (each may raise: the caller guards)
	function acc.updateCount()
		local gt = api.engine.getComponent(api.engine.util.getWorld(), CT.GAME_TIME)
		return gt and gt.updateCount
	end
	function acc.lineUsage(e) return api.engine.util.town.getTownLineUsage(e) end
	-- { {supply, limit} per land use 0..2 }, summed over the cargo types in id order (a fixed order, whatever pairs() gives)
	function acc.supplyDemand(e)
		local out = {}
		for lu = 0, 2 do
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
	-- ids in the order the game script's loops get them
	function acc.townOrder()
		local out = {}
		api.engine.forEachEntityWithComponent(function(e) out[#out + 1] = e end, CT.TOWN)
		return out
	end
	function acc.cargoOrder()
		local out = {}
		for id in pairs(api.res.cargoTypeRep.getAll()) do out[#out + 1] = id end
		return out
	end
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
		function self.noteFactory() end
		function self.flush() end
		self.enabled = false
		return self
	end
	self.enabled = true
	self.cmds = opts.cmds == true
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
		if self.cmds then self.noteOrder(tick, spec.all and { "*" } or spec.list) end
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

	-- ---- command payload trace (opts.cmds)
	local orderPrev = {} -- town -> { text, tick }
	local factoryK, factoryTick = {}, nil -- "town|name" -> calls so far in factoryTick
	local cmdErrors = 0

	local function tickNow()
		if opts.tick then
			local ok, t = pcall(opts.tick)
			if ok and t ~= nil then return t end
		end
		return lastTick or 0
	end

	local function guarded(fn)
		local ok, err = pcall(fn)
		if ok then return true end
		cmdErrors = cmdErrors + 1
		if cmdErrors >= M.ERRORS_MAX then
			self.cmds = false
			emit("# command trace switched off after " .. cmdErrors .. " errors: " .. tostring(err))
		end
		return false
	end

	local function orderHash(list)
		local parts = {}
		for i, v in ipairs(list) do parts[i] = tostring(v) end
		return string.format("%d:%s", #list, tostring(hash(parts)))
	end

	-- the iteration orders of the game script's loops, hashed; one line per traced town when it changed (or every 5 s)
	function self.noteOrder(tick, towns)
		if not self.cmds or not acc.townOrder then return end
		guarded(function()
			local order = acc.townOrder()
			local cargos = {}
			if acc.cargoOrder then
				local ok, c = pcall(acc.cargoOrder)
				if ok and c then cargos = c end
			end
			local th, ch = orderHash(order), orderHash(cargos)
			for _, town in ipairs(towns) do
				local pos = "-"
				for i, e in ipairs(order) do if e == town then pos = tostring(i) end end
				local text = string.format("pos=%s towns=%s cargos=%s", pos, th, ch)
				local op = orderPrev[town]
				if op == nil or op.text ~= text or tick - op.tick >= M.INFO_EVERY then
					emit(string.format("t=%s town=%s order %s", tostring(tick), tostring(town), text))
					orderPrev[town] = { text = text, tick = tick }
				end
			end
		end)
	end

	-- A call of makeTownUpdateSizeCmd / makeTownUpdateCargoNeedsCmd by the game script, seen BEFORE it is passed on.
	-- `args` = { n = select("#", ...), ... } of the call. Reads only; errors are counted (5 switch the command trace off).
	function self.noteFactory(name, args)
		if not self.cmds then return end
		guarded(function()
			local town = args[1]
			if not spec.all and not spec.ids[town] then return end
			local kind = M.CMD_FACTORIES[name]
			if not kind then return end
			local tick = tickNow()
			if tick ~= factoryTick then factoryK, factoryTick = {}, tick end
			local kk = tostring(town) .. "|" .. name
			factoryK[kk] = (factoryK[kk] or 0) + 1
			local k = factoryK[kk]
			local uc = "-"
			if acc.updateCount then
				local ok, v = pcall(acc.updateCount)
				if ok and v ~= nil then uc = tostring(v) end
			end
			local flag = args[3] and "1" or "0"
			if kind == "size" then
				local f32, f64, vs = {}, {}, {}
				for i = 1, 3 do
					local ok, x = pcall(function() return args[2][i] end)
					if not ok then x = nil end
					f32[i], f64[i], vs[i] = M.bits32(x), M.bits64(x), num(x)
				end
				emit(string.format("t=%s uc=%s mk %s town=%s k=%d flag=%s f32=%s f64=%s v=%s", tostring(tick), uc, name, tostring(town), k,
					flag, table.concat(f32, ","), table.concat(f64, ","), table.concat(vs, ",")))
			else
				local needs = seqOf(args[2])
				emit(string.format("t=%s uc=%s mk %s town=%s k=%d flag=%s needs=%s nh=%s", tostring(tick), uc, name, tostring(town), k,
					flag, needs, tostring(hash({ needs }))))
			end
			-- the inputs the script read for this town
			local parts = {}
			local function part(label, fn)
				local ok, v = pcall(fn)
				parts[#parts + 1] = label .. "=" .. (ok and v or "err")
			end
			if acc.lineUsage then part("lu", function() return num(acc.lineUsage(town)) end) end
			if acc.supplyDemand then
				part("sd", function()
					local out = {}
					for i, p in ipairs(acc.supplyDemand(town)) do out[i] = num(p[1]) .. "/" .. num(p[2]) end
					return table.concat(out, ",")
				end)
			end
			local okT, tc = pcall(acc.town, town)
			if okT and tc then
				parts[#parts + 1] = "dev=" .. flat(tc.developmentActive)
				parts[#parts + 1] = "size=" .. seqOf(tc.sizeFactors)
				parts[#parts + 1] = "cap=" .. seqOf(tc.initialLandUseCapacities)
				parts[#parts + 1] = "scale=" .. seqOf(tc.capacityScalingFactor)
				parts[#parts + 1] = "needs=" .. seqOf(tc.cargoNeeds)
			end
			local pv = prev[town]
			parts[#parts + 1] = "bld=" .. (pv and (tostring(pv.n) .. ":" .. tostring(pv.h)) or "?")
			emit(string.format("t=%s town=%s in k=%d %s", tostring(tick), tostring(town), k, table.concat(parts, " ")))
			self.flush()
		end)
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

-- Wraps the two factories on `apiCmd` (api.cmd). `holder` survives a savegame reload (the bridge keeps it in
-- package.loaded): { tracer = <current tracer or nil>, wrappers = {} }. `guard(fn)` (optional) runs fn(tracer) under the
-- bridge's error counter. The wrapper logs FIRST (never raising), then returns exactly what the original returns for the
-- very same arguments. Returns the number of factories newly wrapped.
function M.installCmdWrappers(apiCmd, holder, guard)
	holder.wrappers = holder.wrappers or {}
	local n = 0
	for name in pairs(M.CMD_FACTORIES) do
		local okG, cur = pcall(function() return apiCmd[name] end)
		if okG and type(cur) == "function" and cur ~= holder.wrappers[name] then
			local orig = cur
			local function wrapper(...)
				local t = holder.tracer
				if t and t.cmds then
					local args = { n = select("#", ...), ... }
					local function run(tr) tr.noteFactory(name, args) end
					if guard then pcall(guard, run) else pcall(run, t) end
				end
				return orig(...)
			end
			if pcall(function() apiCmd[name] = wrapper end) then
				holder.wrappers[name] = wrapper
				n = n + 1
			end
		end
	end
	return n
end

return M
