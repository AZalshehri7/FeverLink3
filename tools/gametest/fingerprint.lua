-- Fingerprint of the world, id-independent, for comparing two games at the same game time. Runs in the simulation state
-- through the developer probe (dev_gui.lua or dev_sim.lua). Every section is guarded: a section that cannot be read says why.
local A = Build.adapter
local CT = api.type.ComponentType
local out = {}

local function r1(x) return string.format("%.1f", x or 0) end
local function sorted(list) table.sort(list); return list end
local function digest(list)
	return { n = #list, hash = C.hashValue(list), first = { list[1], list[2], list[3] } }
end
local function section(name, fn)
	local ok, res = pcall(fn)
	out[name] = ok and res or ("ERROR: " .. tostring(res))
end
local function nameOf(e)
	local okc, c = pcall(function() return api.engine.getComponent(e, CT.NAME) end)
	if okc and c then
		local okn, n = pcall(function() return c.name end)
		if okn and type(n) == "string" then return n end
	end
	return ""
end
local function entitiesWith(compName)
	local ids = {}
	api.engine.forEachEntityWithComponent(function(e) ids[#ids + 1] = e end, CT[compName])
	return ids
end

section("time", function()
	local gt = api.engine.getComponent(api.engine.util.getWorld(), CT.GAME_TIME)
	return gt.gameTime
end)

section("money", function()
	local a = api.engine.getComponent(api.engine.util.getPlayer(), CT.ACCOUNT)
	return { balance = a.balance, loan = a.loan }
end)

section("constructions", function()
	local keys = {}
	for _, e in ipairs(entitiesWith("CONSTRUCTION")) do
		local c = A.comp(e, "CONSTRUCTION")
		local m = c and c.transf
		local x, y = m and m[13], m and m[14]
		keys[#keys + 1] = string.format("%s@%s,%s %s", tostring(c and c.fileName):match("[^/]+$") or "?", r1(x), r1(y), nameOf(e))
	end
	return digest(sorted(keys))
end)

local function edgeKeys(mapName)
	local keys = {}
	local seen = {}
	local map = api.engine.system.streetSystem[mapName]()
	local n = 0
	for node, edges in pairs(map) do
		for _, e in ipairs(edges) do
			if not seen[e] then
				seen[e] = true
				local info = A.edgeInfo(e)
				if info then
					local a = string.format("%s,%s,%s", r1(info.p0.x), r1(info.p0.y), r1(info.p0.z))
					local b = string.format("%s,%s,%s", r1(info.p1.x), r1(info.p1.y), r1(info.p1.z))
					if b < a then a, b = b, a end
					keys[#keys + 1] = a .. ">" .. b
				end
			end
		end
		n = n + 1
	end
	return keys, n
end
section("streetEdges", function() local k, nodes = edgeKeys("getNode2StreetEdgeMap"); local d = digest(sorted(k)); d.nodes = nodes; return d end)
section("trackEdges", function() local k, nodes = edgeKeys("getNode2TrackEdgeMap"); local d = digest(sorted(k)); d.nodes = nodes; return d end)

section("lines", function()
	local keys = {}
	for _, e in ipairs(api.engine.system.lineSystem.getLines()) do
		local c = A.comp(e, "LINE")
		local stops = c and c.stops and #c.stops or 0
		keys[#keys + 1] = string.format("%s|stops=%d", nameOf(e), stops)
	end
	return digest(sorted(keys))
end)

section("vehicles", function()
	local keys = {}
	for _, e in ipairs(entitiesWith("TRANSPORT_VEHICLE")) do
		keys[#keys + 1] = nameOf(e)
	end
	return digest(sorted(keys))
end)

section("towns", function()
	local keys = {}
	for _, e in ipairs(entitiesWith("TOWN")) do keys[#keys + 1] = nameOf(e) end
	return digest(sorted(keys))
end)

section("stationGroups", function()
	local keys = {}
	for _, e in ipairs(entitiesWith("STATION_GROUP")) do keys[#keys + 1] = nameOf(e) end
	return digest(sorted(keys))
end)

section("companyName", function()
	return nameOf(api.engine.util.getPlayer())
end)

return out
