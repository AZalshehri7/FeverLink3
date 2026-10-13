-- FeverLink3 command schemas: construction (roads, tracks, stations, depots, signals, bulldozing, upgrades).
--
-- Returns { commands = { <api.cmd function name> = spec, ... } } for the bridge, and the entry points the native
-- capture and the tests use (capture, adapter, classify, ...). docs/COVERAGE_BUILD.md lists every construction
-- command and proposal call of the engine and what is done with each.
--
-- What travels is a "build record": plain data describing a proposal the way the engine's own helpers need it
--   constructions to add (file, parameters, transform, name, owner) and to remove,
--   street nodes and segments to add and to remove, edge objects (stops, signals), node configurations,
--   a marker for terrain edits (their cells cannot be read from a script and are not copied).
-- Entity ids never travel. An entity that exists already is described by a reference that carries its key
-- (s:<id> from the savegame, <sender>:<seq>#... for what a replicated build created) and where it is: a node by
-- its position, a segment by its end positions, a construction by file and position. The applying game uses the
-- key when the entity it names lies where the reference says (so a savegame entity costs one component read) and
-- otherwise looks for the entity at that place; a reference it cannot resolve, or resolves to two entities, refuses
-- the whole action (nothing is applied with a wrong id). Placeholders for entities the proposal itself creates
-- stay negative numbers, as in the engine.
--
-- Every call into the engine is in the adapter table A below, so a game build that behaves differently from what
-- is assumed here is fixed in one place (or from outside, through the global FEVERLINK3_BUILD, like the bridge's
-- FEVERLINK3_ENGINE). The assumptions are listed in docs/COVERAGE_BUILD.md under "needs game verification".
--
-- Findings used from MPFever (MIT): proposals are rebuilt as SimpleProposals (the form the game's own scripts use);
-- properties of engine objects return copies, so every nested change is read, changed and written back; a segment
-- needs lane configurations or the factory throws; transportModes of a lane may be written shifted by one; the
-- engine's own replaceSegment and makeSegmentsRemoveProposal rebuild upgrades and bulldozing exactly as the tools
-- do; a stop or signal replaces its segment and is marked by a placeholder in the segment's object list; the
-- entrance of a depot or street station cannot be built in the same proposal as its street part (street first,
-- construction second); the native tools build with ignoreErrors = false, playerInitiated = true; edge objects get
-- their model and position only when the build has run; entity lists of a core type cannot be walked
-- (BASE_NODE, BASE_EDGE), the street system's maps are used instead.

local okC, C = pcall(ug_require, "feverlink3_common.lua")
if not okC then C = ug_require("feverlink3_1::/feverlink3_common.lua") end
local log = C.logger("build")
local unpack = table.unpack or unpack
local floor, sqrt, abs = math.floor, math.sqrt, math.abs

local M = { commands = {}, VERSION = 1 }

-- metres. TOL_SAME: the entity a key names must lie this close to the place a reference describes. TOL_NODE and
-- TOL_CON: how far a search for the place goes. CON_REACH and EDGE_REACH: circle radius of the octree query (the
-- centre the octree compares is not necessarily the origin a construction is placed by, so the query is generous
-- and the candidates are filtered here).
local TOL_SAME, TOL_NODE, TOL_CON = 0.5, 1.0, 3.0
local CON_REACH, EDGE_REACH = 200, 250
local LIMIT = { cons = 200, nodes = 4000, segs = 4000, objs = 1000, ncfg = 2000, rem = 4000, bytes = 600000, text = 400 }

-- contexts the native tools build with (finding from MPFever (MIT); "terrain" and "plain" are its names)
local CTX_NATIVE = { align = true, cleanup = false, buildings = true, fields = true } -- terrainNoCleanup
local CTX_TERRAIN = { align = true, cleanup = true, buildings = true, fields = true }
local CTX_PLAIN = { align = false, cleanup = true, buildings = false, fields = true }

-- ================================================================== adapter (the only code that calls the engine)

local A = {}
M.adapter = A

local function walk(root, dotted)
	local cur = root
	for part in dotted:gmatch("[^.]+") do
		if cur == nil then return nil end
		local ok, nxt = pcall(function() return cur[part] end)
		if not ok then return nil end
		cur = nxt
	end
	return cur
end

local function get(o, k)
	local ok, v = pcall(function() return o[k] end)
	if ok then return v end
	return nil
end
A.get = get

-- Where the constructors of the engine's types live below `api`. A name with several paths is tried in order.
A.TYPES = {
	SimpleProposal = { "type.SimpleProposal" },
	ConstructionEntity = { "type.SimpleProposal.ConstructionEntity" },
	NodeAndEntity = { "type.NodeAndEntity", "type.Proposal.NodeAndEntity" },
	SegmentAndEntity = { "type.SegmentAndEntity", "type.Proposal.SegmentAndEntity" },
	BaseEdgeStreet = { "type.BaseEdgeStreet", "engine.Component.BaseEdgeStreet" },
	BaseEdgeTrack = { "type.BaseEdgeTrack", "engine.Component.BaseEdgeTrack" },
	PlayerOwned = { "type.PlayerOwned", "engine.Component.PlayerOwned" },
	LaneConfig = { "type.LaneConfig" },
	EdgeObject = { "type.SimpleStreetProposal.EdgeObject" },
	NodeConfig = { "type.BaseNodeLaneConnectionAndEntity" },
	LaneConnection = { "type.LaneConnection" },
	Context = { "type.Context" },
}

function A.new(name)
	for _, p in ipairs(A.TYPES[name] or {}) do
		local c = walk(api, p)
		if c ~= nil then
			local ok, obj = pcall(function() return c.new() end)
			if ok and obj ~= nil then return obj end
			ok, obj = pcall(function() return c:new() end)
			if ok and obj ~= nil then return obj end
		end
	end
	error("no constructor for " .. tostring(name), 0)
end

function A.vec3(x, y, z) return api.type.Vec3f.new(x, y, z) end
function A.vec2(x, y) return api.type.Vec2f.new(x, y) end

-- Mat4f from 16 numbers, column by column (translation is entries 13 to 15)
function A.mat4(m)
	local V = api.type.Vec4f.new
	return api.type.Mat4f.new(V(m[1], m[2], m[3], m[4]), V(m[5], m[6], m[7], m[8]), V(m[9], m[10], m[11], m[12]), V(m[13], m[14], m[15], m[16]))
end

-- The names of an engine object's members: its __members, on the object or on its metatable. Nil for plain tables.
function A.members(o)
	local ok, m = pcall(function() return o.__members end)
	if ok and type(m) == "table" then return m end
	local mt = getmetatable(o)
	if type(mt) == "table" then
		ok, m = pcall(function() return mt.__members end)
		if ok and type(m) == "table" then return m end
	end
	return nil
end

function A.isObject(v)
	local t = type(v)
	if t == "userdata" then return true end
	return t == "table" and getmetatable(v) ~= nil and A.members(v) ~= nil
end

-- x, y, z of a Vec3f / Vec2f, or of a plain table or array; nil when it is none of them
function A.xyz(v)
	if v == nil then return nil end
	local x, y, z = get(v, "x"), get(v, "y"), get(v, "z")
	if type(x) == "number" and type(y) == "number" then return { x = x, y = y, z = type(z) == "number" and z or 0 } end
	x, y, z = get(v, 1), get(v, 2), get(v, 3)
	if type(x) == "number" and type(y) == "number" and type(z) == "number" then return { x = x, y = y, z = z } end
	return nil
end

-- the 16 numbers of a Mat4f (indexable 1..16) or of an array; nil when it is neither
function A.mat16(m)
	if m == nil then return nil end
	local out = {}
	for i = 1, 16 do
		local v = get(m, i)
		if type(v) ~= "number" then return nil end
		out[i] = v
	end
	return out
end

-- The elements of a list value (a Lua table or an engine vector) as a Lua array.
function A.items(l)
	local out = {}
	if l == nil then return out end
	local okN, n = pcall(function() return #l end)
	if okN and type(n) == "number" then
		for i = 1, n do
			local okI, v = pcall(function() return l[i] end)
			if not okI then error("cannot read element " .. i, 0) end
			out[i] = v
		end
	end
	return out
end

-- The elements of a list member as a Lua array. Properties return copies, so big lists are read through the
-- `<name>_native` view (size(), at(i), 1-based) when the engine has one (used that way by the game's own scripts).
function A.list(o, k)
	if o == nil then return {} end
	local nat = get(o, k .. "_native")
	if nat ~= nil then
		local ok, n = pcall(function() return nat:size() end)
		if ok and type(n) == "number" then
			local out = {}
			local fine = true
			for i = 1, n do
				local okI, v = pcall(function() return nat:at(i) end)
				if not okI or v == nil then fine = false; break end
				out[i] = v
			end
			-- the view's indexing was not what this assumed (seen in the real game: crosswalks[1] of a node
			-- configuration): the plain copy of the list is slower but is what the game's own scripts read
			if fine then return out end
		end
	end
	return A.items(get(o, k))
end

-- Any engine value as plain data (numbers, strings, booleans, tables). Vectors become {x, y, z}, other objects the
-- table of their members. For diagnostics and for small values such as edge decorations; the replication itself
-- reads the members it knows by name.
function A.dump(v, depth)
	depth = depth or 0
	local t = type(v)
	if t == "nil" or t == "boolean" or t == "number" or t == "string" then return v end
	if depth > 12 then return nil end
	if t == "table" and getmetatable(v) == nil then
		local out = {}
		for k, x in pairs(v) do
			if type(k) == "number" or type(k) == "string" then out[k] = A.dump(x, depth + 1) end
		end
		return out
	end
	local m = A.members(v)
	if m ~= nil and #m > 0 then
		local vec = A.xyz(v)
		if vec and #m <= 4 then return vec end
		local out = {}
		for _, k in ipairs(m) do
			local x = get(v, k)
			if type(x) ~= "function" then out[k] = A.dump(x, depth + 1) end
		end
		return out
	end
	local m16 = A.mat16(v)
	if m16 then return m16 end
	local okN, n = pcall(function() return #v end)
	if okN and type(n) == "number" then
		local out = {}
		local v0 = get(v, 0)
		if v0 ~= nil then out[0] = A.dump(v0, depth + 1) end
		for i = 1, n do out[i] = A.dump(get(v, i), depth + 1) end
		return out
	end
	return { __unknown = tostring(v) }
end

-- ---- the world

function A.player() return api.engine.util.getPlayer() end

function A.exists(e)
	if type(e) ~= "number" or e < 0 then return false end
	local ok, v = pcall(function() return api.engine.entityExists(e) end)
	return ok and v and true or false
end

function A.comp(e, name)
	if not A.exists(e) then return nil end
	local ok, c = pcall(function() return api.engine.getComponent(e, api.type.ComponentType[name]) end)
	if ok then return c end
	return nil
end

function A.nodePos(e)
	local c = A.comp(e, "BASE_NODE")
	return c and A.xyz(get(c, "position")) or nil
end

-- A segment as the world has it: its end nodes, end positions and end tangents (tangent1 as the engine stores it).
function A.edgeInfo(e)
	local c = A.comp(e, "BASE_EDGE")
	if not c then return nil end
	local n0, n1 = get(c, "node0"), get(c, "node1")
	local p0 = A.nodePos(n0) or A.xyz(get(c, "position0"))
	local p1 = A.nodePos(n1) or A.xyz(get(c, "position1"))
	if not p0 or not p1 then return nil end
	return { n0 = n0, n1 = n1, p0 = p0, p1 = p1, t0 = A.xyz(get(c, "tangent0")), t1 = A.xyz(get(c, "tangent1")) }
end

function A.conInfo(e)
	local c = A.comp(e, "CONSTRUCTION")
	if not c then return nil end
	local m = A.mat16(get(c, "transf"))
	if not m then return nil end
	return { file = get(c, "fileName"), p = { x = m[13], y = m[14], z = m[15] } }
end

-- the {object entity, kind} pairs a segment carries, in the engine's order
function A.objectsOf(e)
	local c = A.comp(e, "BASE_EDGE")
	local out = {}
	if not c then return out end
	for _, o in ipairs(A.list(c, "objects")) do
		local id, kind = get(o, 1), get(o, 2)
		if type(id) == "number" then out[#out + 1] = { id, kind } end
	end
	return out
end

-- depots and stations of a construction, in the engine's order
function A.conParts(e)
	local c = A.comp(e, "CONSTRUCTION")
	if not c then return {}, {} end
	return A.list(c, "depots"), A.list(c, "stations")
end

function A.stationGroup(station)
	local ok, g = pcall(function() return api.engine.system.stationGroupSystem.getStationGroup(station) end)
	if ok and type(g) == "number" and g >= 0 then return g end
	return nil
end

-- an edge object that is also a station: a road-side stop (a signal is an edge object and nothing else)
function A.isStop(e)
	return A.comp(e, "EDGE_OBJECT") ~= nil and A.comp(e, "STATION") ~= nil
end

function A.edgeOfObject(o)
	local ok, e = pcall(function() return api.engine.system.streetSystem.getEdgeForEdgeObject(o) end)
	if ok and type(e) == "number" and e >= 0 then return e end
	return nil
end

-- ---- looking for the entity at a place

local function octree(p, radius, compName)
	local ok, l = pcall(function()
		return api.engine.util.octree.findEntitiesInCircle(api.type.Vec2f.new(p.x, p.y), radius, api.type.ComponentType[compName])
	end)
	if not ok or l == nil then return nil end
	local out = {}
	local okN, n = pcall(function() return #l end)
	if not okN or type(n) ~= "number" then return nil end
	for i = 1, n do out[i] = l[i] end
	return out
end

local function nodesFromOctreeNodes(p, tol) return octree(p, tol + 0.5, "BASE_NODE") end

-- the nodes at the ends of the segments the octree finds near p
local function nodesFromOctreeEdges(p, tol)
	local edges = octree(p, EDGE_REACH, "BASE_EDGE")
	if not edges then return nil end
	local out, seen = {}, {}
	for _, e in ipairs(edges) do
		local c = A.comp(e, "BASE_EDGE")
		for _, n in ipairs({ c and get(c, "node0"), c and get(c, "node1") }) do
			if type(n) == "number" and not seen[n] then seen[n] = true; out[#out + 1] = n end
		end
	end
	return out
end

-- every node of the street graph (BASE_NODE cannot be walked with forEachEntityWithComponent: finding from MPFever (MIT))
local function nodesFromScan(p, tol)
	local ok, map = pcall(function() return api.engine.system.streetSystem.getNode2SegmentMap() end)
	local out = {}
	if not ok or map == nil then return out end
	pcall(function()
		for n, _ in pairs(map) do
			local q = A.nodePos(n)
			if q and abs(q.x - p.x) <= tol and abs(q.y - p.y) <= tol then out[#out + 1] = n end
		end
	end)
	return out
end

function A.nodesScan(p, tol) return nodesFromScan(p, tol) end

-- Which of the three ways above finds a node. Probed once against a node that exists: the octree is used by the
-- game's own scripts for BASE_EDGE, CONSTRUCTION and STATION, not seen for BASE_NODE.
function A.probeNodeLookup()
	local sample, p
	pcall(function()
		for n, _ in pairs(api.engine.system.streetSystem.getNode2SegmentMap()) do sample = n; break end
	end)
	if sample == nil then return nil end
	p = A.nodePos(sample)
	if not p then return nil end
	local function has(list)
		for _, e in ipairs(list or {}) do if e == sample then return true end end
		return false
	end
	if has(nodesFromOctreeNodes(p, TOL_NODE)) then return "octree-node" end
	if has(nodesFromOctreeEdges(p, TOL_NODE)) then return "octree-edge" end
	return "scan"
end

-- candidate nodes within reach of p (the caller measures the distance)
function A.nodesNear(p, tol)
	if A.nodeMode == nil then
		A.nodeMode = A.probeNodeLookup()
		if A.nodeMode then log("references: nodes are looked up by " .. A.nodeMode) end
	end
	local list
	if A.nodeMode == "octree-node" then list = nodesFromOctreeNodes(p, tol)
	elseif A.nodeMode == "octree-edge" then list = nodesFromOctreeEdges(p, tol) end
	if list == nil then list = nodesFromScan(p, tol) end
	return list
end

-- true when a segment of the world passes within `tol` metres of p (its ends are looked up, then the line between its ends)
function A.edgeNear(p, tol)
	local list = octree(p, EDGE_REACH, "BASE_EDGE")
	for _, e in ipairs(list or {}) do
		local info = A.edgeInfo(e)
		if info then
			local ax, ay, bx, by = info.p0.x, info.p0.y, info.p1.x, info.p1.y
			local dx, dy = bx - ax, by - ay
			local l2 = dx * dx + dy * dy
			local t = l2 > 0 and ((p.x - ax) * dx + (p.y - ay) * dy) / l2 or 0
			t = math.max(0, math.min(1, t))
			local qx, qy = ax + t * dx, ay + t * dy
			if sqrt((p.x - qx) ^ 2 + (p.y - qy) ^ 2) <= tol then return true end
		end
	end
	return false
end

-- the segments attached to a node, streets then tracks (accessors used by the game's own entity windows)
function A.edgesAt(n)
	local out, seen = {}, {}
	for _, fn in ipairs({ "getNodeStreetSegments", "getNodeTrackSegments" }) do
		local ok, l = pcall(function() return api.engine.system.streetSystem[fn](n) end)
		if ok and l ~= nil then
			local okN, cnt = pcall(function() return #l end)
			for i = 1, (okN and cnt or 0) do
				local e = l[i]
				if type(e) == "number" and not seen[e] then seen[e] = true; out[#out + 1] = e end
			end
		end
	end
	return out
end

-- candidate constructions around p; the octree answers for CONSTRUCTION (game scripts use it that way); when it
-- does not, every construction is a candidate
function A.consNear(p)
	local list = octree(p, CON_REACH, "CONSTRUCTION")
	if list ~= nil and #list > 0 then return list end
	local out = {}
	pcall(function()
		api.engine.forEachEntityWithComponent(function(e) out[#out + 1] = e end, api.type.ComponentType.CONSTRUCTION)
	end)
	return out
end

-- ---- street templates and lanes

-- the lane configurations of a street template (engine objects, assignable to a segment as they are)
function A.templateLanes(template)
	if type(template) ~= "string" or template == "" then return nil end
	local lanes
	pcall(function()
		local rep = api.res.streetTemplateRep
		local idx = rep.find(template)
		if idx == nil or idx < 0 then idx = rep.find((template:gsub("^::/", ""))) end
		local t = rep.get(idx)
		if t and t.laneConfigs and #t.laneConfigs > 0 then lanes = t.laneConfigs end
	end)
	return lanes
end

-- transportModes reads from index 0, but its writes may land shifted by one (tm[j] = x ends at j - 1): measured once
-- on a new lane (finding from MPFever (MIT))
function A.laneShift()
	if A.shift ~= nil then return A.shift end
	A.shift = 0
	pcall(function()
		local lc = A.new("LaneConfig")
		local tm = lc.transportModes
		for j = 0, 15 do tm[j] = false end
		tm[3] = true
		lc.transportModes = tm
		local back = lc.transportModes
		if back[3] then A.shift = 0 elseif back[2] then A.shift = 1 end
	end)
	return A.shift
end

-- one lane as plain data: speed, width, height, forward, offset and the transport modes that may use it
function A.readLane(l)
	local modes = {}
	local tm = get(l, "transportModes")
	for j = 0, 15 do
		if get(tm, j) == true then modes[#modes + 1] = j end
	end
	return { speed = get(l, "speed"), width = get(l, "width"), height = get(l, "height"), forward = get(l, "forward"),
		offset = get(l, "offset"), modes = modes }
end

function A.readLanes(list)
	local out = {}
	for i, l in ipairs(list) do out[i] = A.readLane(l) end
	return out
end

-- lanes compare by direction and the vehicles each takes (tram tracks on a road change this, not the template)
local function laneSignature(plainLanes)
	local parts = {}
	for i, l in ipairs(plainLanes) do parts[i] = tostring(l.forward) .. ":" .. table.concat(l.modes or {}, ",") end
	return table.concat(parts, "|")
end

function A.rebuildLanes(plainLanes)
	local shift = A.laneShift()
	local out = {}
	for i, m in ipairs(plainLanes) do
		local lc = A.new("LaneConfig")
		for _, k in ipairs({ "speed", "width", "height", "forward", "offset" }) do
			if m[k] ~= nil then pcall(function() lc[k] = m[k] end) end
		end
		local on = {}
		for _, j in ipairs(m.modes or {}) do on[j] = true end
		pcall(function()
			local tm = lc.transportModes
			-- a write at k lands at k - shift, and index 0 wraps to the end: so mode j is written at (j + shift) mod 16
			for j = 0, 15 do tm[(j + shift) % 16] = on[j] == true end
			lc.transportModes = tm
		end)
		out[i] = lc
	end
	return out
end

-- ---- engine helpers that build what the tools build

function A.replaceSegment(edge, template) return api.engine.util.proposal.replaceSegment(edge, template) end
function A.removeSegmentsProposal(edges) return api.engine.util.proposal.makeSegmentsRemoveProposal(edges) end

-- A test, or a game build that behaves differently, replaces any of the above through a global table.
do
	local okE, override = pcall(function() return FEVERLINK3_BUILD end)
	if okE and type(override) == "table" then
		for k, v in pairs(override) do A[k] = v end
	end
end

-- ================================================================== small helpers

local function isNum(v) return type(v) == "number" and v == v and v ~= math.huge and v ~= -math.huge end
local function d3(a, b)
	local dx, dy, dz = a.x - b.x, a.y - b.y, (a.z or 0) - (b.z or 0)
	return sqrt(dx * dx + dy * dy + dz * dz)
end
local function pkey(p) return string.format("%.3f,%.3f,%.3f", p.x, p.y, p.z or 0) end
local function copyXYZ(p) return { x = p.x, y = p.y, z = p.z or 0 } end
local function fail(msg) error(msg, 0) end

local function describe(ref)
	if type(ref) ~= "table" then return tostring(ref) end
	if ref.__ref == "node" then return "node at " .. (ref.p and pkey(ref.p) or "?") end
	if ref.__ref == "edge" then return "segment " .. (ref.p0 and pkey(ref.p0) or "?") .. " - " .. (ref.p1 and pkey(ref.p1) or "?") end
	if ref.__ref == "con" then return "construction " .. tostring(ref.file) .. " at " .. (ref.p and pkey(ref.p) or "?") end
	if ref.__ref == "obj" then return "object " .. tostring(ref.i) .. " of " .. describe(ref.edge) end
	return tostring(ref.__ref) .. " " .. tostring(ref.key)
end

-- a required member: failing to set it refuses the build; an optional one is only noted
local function req(obj, field, value)
	local ok, err = pcall(function() obj[field] = value end)
	if not ok then fail("cannot set " .. field .. ": " .. tostring(err)) end
end
local function opt(notes, obj, field, value)
	local ok, err = pcall(function() obj[field] = value end)
	if not ok then notes[#notes + 1] = field .. ": " .. tostring(err):sub(1, 80) end
end

-- ================================================================== references

-- Picks the best of {e, s (score), k (tie-break text)}: nil when none is within `limit`, nil and "ambiguous" when two
-- different entities are equally good.
local function pick(cands, limit)
	table.sort(cands, function(a, b)
		if a.s ~= b.s then return a.s < b.s end
		return a.k < b.k
	end)
	local best, second = cands[1], cands[2]
	if not best or best.s > limit then return nil end
	if second and second.e ~= best.e and second.s - best.s < 1e-6 then return nil, "ambiguous" end
	return best.e
end

local function dot(a, b) return a.x * b.x + a.y * b.y + (a.z or 0) * (b.z or 0) end
local function len(a) return sqrt(dot(a, a)) end
local function cosine(a, b)
	local la, lb = len(a), len(b)
	if la < 1e-9 or lb < 1e-9 then return 1 end
	return dot(a, b) / (la * lb)
end

-- How well a segment of the world matches a reference: the larger end distance (either direction), plus a small
-- term for the tangents so that two segments between the same nodes are told apart.
local function edgeScore(info, ref)
	local straight = math.max(d3(info.p0, ref.p0), d3(info.p1, ref.p1))
	local flipped = math.max(d3(info.p0, ref.p1), d3(info.p1, ref.p0))
	local s = math.min(straight, flipped)
	if info.t0 and ref.t0 then
		local c
		if flipped < straight and ref.t1 then
			c = cosine(info.t0, { x = -ref.t1.x, y = -ref.t1.y, z = -(ref.t1.z or 0) })
		else
			c = cosine(info.t0, ref.t0)
		end
		s = s + 0.01 * (1 - c)
	end
	return s
end

local function resolveNode(ref, ctx)
	local e = ref.key and ctx.resolve(ref.key)
	if e then
		local p = A.nodePos(e)
		if p and d3(p, ref.p) <= TOL_SAME then return e end
	end
	local function search(list)
		local cands = {}
		for _, n in ipairs(list) do
			local p = A.nodePos(n)
			if p then
				local d = d3(p, ref.p)
				if d <= TOL_NODE then cands[#cands + 1] = { e = n, s = d, k = pkey(p) } end
			end
		end
		return pick(cands, TOL_NODE)
	end
	local found, why = search(A.nodesNear(ref.p, TOL_NODE))
	if found == nil and why == nil and A.nodeMode ~= "scan" then
		-- the octree may not know a node built a moment ago (a paused game never ticks its spatial index): the street graph's own
		-- map does
		found, why = search(A.nodesScan(ref.p, TOL_NODE))
		if found ~= nil then log("references: the node at " .. pkey(ref.p) .. " was not in the octree but is in the street graph") end
	end
	return found, why
end

local function resolveEdge(ref, ctx)
	local e = ref.key and ctx.resolve(ref.key)
	if e then
		local info = A.edgeInfo(e)
		if info and edgeScore(info, ref) <= TOL_SAME then return e end
	end
	-- Looked for at the nodes near either end of the reference (the other end helps when the octree lacks the node at the first),
	-- then, if the octree found nothing, in the street graph's own map.
	local function search(scan)
		local cands, seen = {}, {}
		for _, at in ipairs({ ref.p0, ref.p1 }) do
			for _, n in ipairs(scan and A.nodesScan(at, TOL_NODE) or A.nodesNear(at, TOL_NODE)) do
				local p = A.nodePos(n)
				if p and d3(p, at) <= TOL_NODE then
					for _, ed in ipairs(A.edgesAt(n)) do
						if not seen[ed] then
							seen[ed] = true
							local info = A.edgeInfo(ed)
							if info then
								local s = edgeScore(info, ref)
								if s <= TOL_NODE then cands[#cands + 1] = { e = ed, s = s, k = pkey(info.p0) .. ";" .. pkey(info.p1) } end
							end
						end
					end
				end
			end
		end
		return pick(cands, TOL_NODE)
	end
	local found, why = search(false)
	if found == nil and why == nil and A.nodeMode ~= "scan" then
		found, why = search(true)
		if found ~= nil then log("references: " .. describe(ref) .. " was not found through the octree but is in the street graph") end
	end
	return found, why
end

local function resolveCon(ref, ctx)
	local e = ref.key and ctx.resolve(ref.key)
	if e then
		local info = A.conInfo(e)
		if info and info.file == ref.file and d3(info.p, ref.p) <= TOL_SAME + 0.5 then return e end
	end
	local cands = {}
	for _, c in ipairs(A.consNear(ref.p)) do
		local info = A.conInfo(c)
		if info and info.file == ref.file then
			local d = d3(info.p, ref.p)
			if d <= TOL_CON then cands[#cands + 1] = { e = c, s = d, k = pkey(info.p) } end
		end
	end
	return pick(cands, TOL_CON)
end

-- The refs table: how references are resolved on the applying game. Each function returns the local entity or raises
-- a message that says what could not be found.
local function newResolver(ctx)
	local R = {}
	local function must(kind, ref, fn)
		if type(ref) ~= "table" then fail("malformed " .. kind .. " reference") end
		local e, why = fn(ref, ctx)
		if e == nil then fail((why == "ambiguous" and "ambiguous " or "unresolved ") .. describe(ref)) end
		return e
	end
	function R.node(ref) return must("node", ref, resolveNode) end
	function R.edge(ref) return must("segment", ref, resolveEdge) end
	function R.con(ref) return must("construction", ref, resolveCon) end
	function R.obj(ref)
		local edge = R.edge(ref.edge)
		local o = A.objectsOf(edge)[ref.i]
		if not o then fail("unresolved " .. describe(ref)) end
		return o[1]
	end
	-- a number is a placeholder (negative) or means "none"; a table is a reference
	function R.nodeOr(v) if type(v) == "table" then return R.node(v) end return v end
	function R.edgeOr(v) if type(v) == "table" then return R.edge(v) end return v end
	function R.objOr(v) if type(v) == "table" then return R.obj(v) end return v end
	-- The owner of a construction, segment or edge object. "player" is the entity the acting player's commands apply to on
	-- this game (ctx.player: in a company session the sender's company, docs/COMPANIES.md 3.7), never the local player;
	-- another owner travels as a key; a number is "nobody" (a town building).
	function R.owner(v)
		if v == "player" then return ctx.player or A.player() end
		if type(v) == "table" then
			local e = ctx.resolve and ctx.resolve(v.key)
			if e == nil then fail("unresolved owner " .. tostring(v.key)) end
			return e
		end
		return v
	end
	return R
end

-- ================================================================== capture (proposal -> build record)

-- Names of the street part's lists in the two proposal types: SimpleProposal.streetProposal (what scripts build) and
-- Proposal.proposal (what the tools produce).
local NAMES = {
	simple = { nodes = "nodesToAdd", segs = "edgesToAdd", rmNodes = "nodesToRemove", rmSegs = "edgesToRemove",
		objs = "edgeObjectsToAdd", rmObjs = "edgeObjectsToRemove", ncfg = "nodeConfigsToAdd", rmNcfg = "nodeConfigsToRemove" },
	native = { nodes = "addedNodes", segs = "addedSegments", rmNodes = "removedNodes", rmSegs = "removedSegments",
		objs = "edgeObjectsToAdd", rmObjs = "edgeObjectsToRemove", ncfg = "nodeConfigsToAdd", rmNcfg = "nodeConfigsToRemove" },
}

local function idOf(x)
	if type(x) == "number" then return x end
	local e = get(x, "entity")
	if type(e) == "number" then return e end
	fail("a list entry is neither an entity id nor an entity record")
end

local function flavorOf(p)
	if get(p, "streetProposal") ~= nil or get(p, "constructionsToAdd") ~= nil then return "simple" end
	if get(p, "proposal") ~= nil or get(p, "toAdd") ~= nil then return "native" end
	return nil
end

-- the state of one capture: how entities are keyed, what the proposal itself knows about entities it removes, and
-- the references already made
local function newCapture(ctx, opts)
	local cap = { keyOf = ctx.keyOf, plain = ctx.plain, known = { nodes = {}, edges = {}, edgeTypes = {}, objIndex = {} }, memo = {}, result = opts.result, alias = {} }

	function cap.node(id)
		if type(id) ~= "number" then fail("a node reference is not an entity id") end
		if id < 0 then return id end
		if cap.alias["n" .. id] then return cap.alias["n" .. id] end
		local m = cap.memo["n" .. id]
		if m then return m end
		local p = A.nodePos(id) or cap.known.nodes[id]
		if not p then fail("node " .. id .. " has no position") end
		m = { __ref = "node", key = cap.keyOf and cap.keyOf(id) or nil, p = copyXYZ(p) }
		cap.memo["n" .. id] = m
		return m
	end

	function cap.edge(id)
		if type(id) ~= "number" then fail("a segment reference is not an entity id") end
		if cap.alias["e" .. id] then return cap.alias["e" .. id] end
		local m = cap.memo["e" .. id]
		if m then return m end
		local info = A.edgeInfo(id) or cap.known.edges[id]
		-- A negative id is a placeholder of the proposal, except where the proposal's own record of a segment it removes
		-- (learnRemoved) carries it: the real session 2026-10-13-a listed a bus stop's removed segment as -39433 and the
		-- peer got a bare number where a reference by ends belongs ("removed segment 1: bad reference"). Why the id is
		-- negative: unproven.
		if id < 0 and not info then return id end
		if not info then fail("segment " .. id .. " has no ends") end
		m = { __ref = "edge", key = id >= 0 and cap.keyOf and cap.keyOf(id) or nil, p0 = copyXYZ(info.p0), p1 = copyXYZ(info.p1),
			t0 = info.t0 and copyXYZ(info.t0) or nil, t1 = info.t1 and copyXYZ(info.t1) or nil }
		cap.memo["e" .. id] = m
		return m
	end

	function cap.con(id)
		if type(id) ~= "number" or id < 0 then fail("a construction reference is not an entity id") end
		local m = cap.memo["c" .. id]
		if m then return m end
		local info = A.conInfo(id)
		if not info then fail("construction " .. id .. " is not in the world") end
		m = { __ref = "con", key = cap.keyOf and cap.keyOf(id) or nil, file = info.file, p = copyXYZ(info.p) }
		cap.memo["c" .. id] = m
		return m
	end

	-- an existing edge object is named by the segment that carries it and its rank there (object ids differ
	-- between games, the rank does not)
	function cap.obj(id)
		if type(id) ~= "number" then fail("an object reference is not an entity id") end
		if id < 0 then return id end
		local m = cap.memo["o" .. id]
		if m then return m end
		local at = cap.known.objIndex[id]
		if not at then
			local edge = A.edgeOfObject(id)
			if edge then
				for i, o in ipairs(A.objectsOf(edge)) do
					if o[1] == id then at = { edge = edge, i = i } end
				end
			end
		end
		if not at then fail("object " .. id .. " is on no known segment") end
		-- An object read after the build ran, on a segment this build made (its real id aliased to a placeholder): it is a NEW object of
		-- this build, so it travels as a placeholder like the ones of a tool's own proposal (the n-th placeholder in the segments' object
		-- lists is the n-th edge object; PLAN.stop). Real run 2026-10-12: a bus stop was shipped as { __ref = "obj", edge = -1, i = 1 }
		-- and every other game refused the record ("segment 1: bad object").
		if type(cap.alias["e" .. at.edge]) == "number" then
			cap.newObjs = (cap.newObjs or 0) + 1
			m = -400000000 - (cap.newObjs - 1)
		else
			m = { __ref = "obj", edge = cap.edge(at.edge), i = at.i }
		end
		cap.memo["o" .. id] = m
		return m
	end

	function cap.owner(v)
		if v == nil then return nil end
		if v == A.player() then return "player" end
		-- another company's entity has no id the other games share; nobody (a town building) is -1
		if type(v) == "number" and v >= 0 and cap.keyOf then return { __ref = "owner", key = cap.keyOf(v) } end
		return v
	end

	return cap
end

local function containsUnknown(t, depth)
	if type(t) ~= "table" or depth > 12 then return false end
	for k, v in pairs(t) do
		if k == "__unknown" or containsUnknown(v, depth + 1) then return true end
	end
	return false
end

-- A construction's parameters as a plain copy. A table is copied as it is (functions and userdata in it raise); a value
-- the engine hands over as an object is read out member by member first.
local function plainParams(v, cap)
	if v == nil then return {} end
	local plain = v
	if type(v) ~= "table" or getmetatable(v) ~= nil then plain = A.dump(v) end
	if type(plain) ~= "table" or containsUnknown(plain, 0) then fail("construction parameters cannot be replicated: they hold a value that is not plain data") end
	local ok, copy = pcall(cap.plain or C.plain, plain)
	if not ok then fail("construction parameters cannot be replicated: " .. tostring(copy)) end
	return copy
end

local function readConstruction(ce, cap)
	local params = get(ce, "params")
	if params == nil then
		local con = get(ce, "construction")
		if con ~= nil then params = get(con, "params") end
	end
	local m = A.mat16(get(ce, "transf"))
	if not m then fail("a construction has no transform") end
	local file = get(ce, "fileName")
	if type(file) ~= "string" then fail("a construction has no file name") end
	return { file = file, params = plainParams(params, cap), transf = m, name = get(ce, "name"), owner = cap.owner(get(ce, "playerEntity")) }
end

local function readNode(n, cap)
	local id = get(n, "entity")
	local comp = get(n, "comp")
	local pos = A.xyz(get(comp, "position"))
	if not pos then fail("a node to add has no position") end
	return { id = cap.node(id), pos = copyXYZ(pos) }
end

-- The street segments a construction brings with it (the entrance of a depot or street station, the paths of a rail
-- station) have templates of their own; the construction's script makes them, so a record does not need their detail
-- (finding from MPFever (MIT)).
-- `withCons` false: the proposal brings no construction, so a "simple.street_template" segment is not a construction's piece.
-- The template of a user's track (/infrastructure/track/simple/simple.street_template) contains that text too: real session
-- 2026-10-13 (trace.log 03:02:37, trackBuilder) captured both track segments as pieces "of a construction" with no tangents,
-- positions or track type, and both games refused the replay ("Construction Not Possible").
local function ownTemplate(template, withCons)
	local tpl = tostring(template or "")
	return tpl:find("/constructions/", 1, true) ~= nil or (withCons ~= false and tpl:find("simple.street_template", 1, true) ~= nil)
		or tpl:find("/airport/", 1, true) ~= nil   -- an airfield's runway and taxiways (real run 2026-10-11: 21 segments of an airfield)
end

-- one segment to add (SegmentAndEntity): everything the rebuild needs, from members read by name
local function readSegment(s, cap)
	local c = get(s, "comp")
	if c == nil then fail("a segment to add has no component") end
	local template = get(c, "roadTemplate")
	if ownTemplate(template, cap.hasCons) then
		-- noted, not described: what cannot be read about it must not stop the construction from being copied
		local function nodeRef(k)
			local ok, r = pcall(cap.node, get(c, k))
			if ok then return r end
			return -2147483647
		end
		local seg = {
			id = cap.edge(get(s, "entity")), type = get(s, "type"), n0 = nodeRef("node0"), n1 = nodeRef("node1"), objects = {},
			c = { roadTemplate = template, type = get(c, "type"), typeIndex = get(c, "typeIndex"), roadType = get(c, "roadType"), roadStyle = get(c, "roadStyle") },
		}
		-- A piece that ends at a node the world already had is a CONNECTION to the street (a depot's entrance to a town road), made
		-- by the tool, not by the construction's own script: its geometry travels so that a game whose replay did not make it can.
		if (type(seg.n0) == "table") ~= (type(seg.n1) == "table") then
			local okG, g = pcall(function()
				return { t0 = A.xyz(get(c, "tangent0")), t1 = A.xyz(get(c, "tangent1")), p0 = A.xyz(get(c, "position0")), p1 = A.xyz(get(c, "position1")) }
			end)
			if okG and g.t0 and g.t1 and g.p0 and g.p1 then
				seg.c.t0, seg.c.t1, seg.c.p0, seg.c.p1 = g.t0, g.t1, g.p0, g.p1
				seg.connector = true
			end
		end
		return seg
	end
	local lanes = A.list(c, "laneConfigs")
	-- lanes travel only when they are not the lanes of the street template (tram tracks on a road, platform lanes of a
	-- stop): the template's own are used otherwise
	local custom
	if #lanes > 0 then
		local plain = A.readLanes(lanes)
		local tl = A.templateLanes(template)
		if not tl or laneSignature(plain) ~= laneSignature(A.readLanes(A.items(tl))) then custom = plain end
	end
	local objects = {}
	for _, o in ipairs(A.list(c, "objects")) do
		local oid, kind = get(o, 1), get(o, 2)
		if type(oid) ~= "number" then fail("a segment object is not an entity id") end
		objects[#objects + 1] = { cap.obj(oid), kind }
	end
	local se = get(s, "streetEdge")
	local po = get(s, "playerOwned")
	-- A track segment says what track it is in the SegmentAndEntity's `trackEdge` (track type, catenary), not in its BaseEdge: without
	-- it a replayed track is a plain street edge and the engine refuses it ("Construction Not Possible"; real session 2026-10-12, every
	-- track build was refused on both games). Read as it is; the replay sets it only when it differs from a fresh BaseEdgeTrack.
	local track = nil
	do
		local te = get(s, "trackEdge")
		if te ~= nil then
			local tt, cat = get(te, "trackType"), get(te, "catenary")
			if type(tt) == "number" then track = { tt = tt, cat = (cat == true or cat == 1) and 1 or 0 } end
		end
	end
	local seg = {
		id = cap.edge(get(s, "entity")), type = get(s, "type"),
		n0 = cap.node(get(c, "node0")), n1 = cap.node(get(c, "node1")),
		c = {
			type = get(c, "type"), typeIndex = get(c, "typeIndex"), roadType = get(c, "roadType"),
			roadTemplate = template, roadStyle = get(c, "roadStyle"), dev = get(c, "roadDevelopmentLocked"),
			distance = get(c, "distance"),
			t0 = A.xyz(get(c, "tangent0")), t1 = A.xyz(get(c, "tangent1")),
			p0 = A.xyz(get(c, "position0")), p1 = A.xyz(get(c, "position1")),
			deco = A.dump(get(c, "edgeDecorations")),
		},
		lanes = custom, objects = objects, track = track,
		prec = se and { get(se, "precedenceNode0"), get(se, "precedenceNode1") } or nil,
		owner = po and cap.owner(get(po, "player")) or nil,
	}
	return seg
end

local function readEdgeObject(o, i, cap)
	local edge = get(o, "edgeEntity")
	if edge == nil then edge = get(o, "segmentEntity") end
	local out = {
		edge = type(edge) == "number" and cap.edge(edge) or nil,
		model = get(o, "model") or get(o, "edgeObjectConstruction"),
		param = get(o, "param"), left = get(o, "left") == true, oneWay = get(o, "oneWay") == true,
		name = get(o, "name"), kind = get(o, "category"), owner = cap.owner(get(o, "playerEntity")),
	}
	if out.edge == nil then fail("an edge object has no segment") end
	-- the engine fills in model and position only when the build has run (finding from MPFever (MIT)): read them from the
	-- object that was created
	if out.model == nil or out.param == nil then
		local e = get(o, "resultEntity")
		if not A.comp(e, "EDGE_OBJECT") then
			e = nil
			local n = 0
			for _, r in ipairs(cap.result or {}) do
				local re = type(r) == "number" and r or get(r, 1)
				if A.comp(re, "EDGE_OBJECT") then
					n = n + 1
					if n == i then e = re end
				end
			end
		end
		local ec = e and A.comp(e, "EDGE_OBJECT")
		if ec then
			out.model = out.model or get(ec, "edgeObjectConstruction")
			out.param = out.param or get(ec, "param")
			out.params = A.dump(get(ec, "params"))
		end
	end
	if type(out.model) ~= "string" then fail("an edge object has no model (captured before the build ran?)") end
	return out
end

local function readNodeConfig(n, cap)
	local comp = get(n, "comp")
	local lcs = {}
	for _, l in ipairs(A.list(comp, "laneConnections")) do
		lcs[#lcs + 1] = { s0 = cap.edge(get(l, "segment0")), l0 = get(l, "lane0"), s1 = cap.edge(get(l, "segment1")), l1 = get(l, "lane1"),
			road = get(l, "withRoad") == true, tram = get(l, "withTram") == true }
	end
	local cw = {}
	for _, x in ipairs(A.list(comp, "crosswalks")) do cw[#cw + 1] = cap.edge(x) end
	return { node = cap.node(get(n, "entity")), lc = lcs, cw = cw, pref = get(comp, "trafficLightPreference"),
		dss = get(comp, "doubleSlipSwitch") == true, user = get(comp, "userModifiedLaneConnections") == true }
end

-- the size of a terrain edit's grid; its cells cannot be read from a script (finding from MPFever (MIT))
local function readTerrain(p)
	local t = get(p, "terrain")
	local g = t and get(t, "baseHeightMod")
	if not g then return nil end
	local w, h = get(g, "width"), get(g, "height")
	if type(w) ~= "number" or type(h) ~= "number" or w * h <= 0 then return nil end
	return { x0 = get(g, "x0"), y0 = get(g, "y0"), w = w, h = h }
end

local function emptyRecord(flavor)
	return { v = M.VERSION, src = flavor, cons = {}, rem = {}, old2new = {}, nodes = {}, segs = {}, rmNodes = {}, rmSegs = {},
		objs = {}, rmObjs = {}, ncfg = {}, rmNcfg = {} }
end

-- What the proposal knows about the entities it removes. A tool's proposal carries them as full records, and after the
-- build ran the world no longer has them.
local function learnRemoved(st, names, cap)
	for _, n in ipairs(A.list(st, names.rmNodes)) do
		if type(n) ~= "number" then
			local id = get(n, "entity")
			local p = A.xyz(get(get(n, "comp"), "position"))
			if type(id) == "number" and p then cap.known.nodes[id] = p end
		end
	end
	for _, s in ipairs(A.list(st, names.rmSegs)) do
		if type(s) ~= "number" then
			local id, c = get(s, "entity"), get(s, "comp")
			if type(id) == "number" and c then
				local p0 = A.xyz(get(c, "position0")) or A.nodePos(get(c, "node0"))
				local p1 = A.xyz(get(c, "position1")) or A.nodePos(get(c, "node1"))
				if p0 and p1 then
					cap.known.edges[id] = { p0 = p0, p1 = p1, t0 = A.xyz(get(c, "tangent0")), t1 = A.xyz(get(c, "tangent1")) }
				end
				cap.known.edgeTypes[id] = { ty = get(c, "type"), ti = get(c, "typeIndex"), rt = get(c, "roadType"), st = get(s, "type") }
				for i, o in ipairs(A.list(c, "objects")) do
					local oid = get(o, 1)
					if type(oid) == "number" and oid >= 0 then cap.known.objIndex[oid] = { edge = id, i = i } end
				end
			end
		end
	end
end

-- The segments in the world, whose objects an unchanged segment keeps: indexed before the lists are read.
local function indexWorldObjects(ids, cap)
	for _, id in ipairs(ids) do
		if type(id) == "number" and id >= 0 then
			for i, o in ipairs(A.objectsOf(id)) do
				if not cap.known.objIndex[o[1]] then cap.known.objIndex[o[1]] = { edge = id, i = i } end
			end
		end
	end
end

-- Last resort for a removed segment that is still a bare negative id: a stop (or signal) replaces its segment by one
-- with the same ends and tangents, so the single added segment names the single removed one.
local function removedEdgeFromAdded(rec)
	if #rec.rmSegs ~= 1 or #rec.segs ~= 1 or type(rec.rmSegs[1]) ~= "number" or rec.rmSegs[1] >= 0 then return end
	local c = rec.segs[1].c
	if type(c) ~= "table" or not c.p0 or not c.p1 then return end
	rec.rmSegs[1] = { __ref = "edge", p0 = copyXYZ(c.p0), p1 = copyXYZ(c.p1),
		t0 = c.t0 and copyXYZ(c.t0) or nil, t1 = c.t1 and copyXYZ(c.t1) or nil }
end

local function captureRecord(proposal, cap)
	local flavor = flavorOf(proposal)
	if not flavor then fail("not a proposal (neither streetProposal nor proposal / toAdd found)") end
	local rec = emptyRecord(flavor)
	local names = NAMES[flavor]
	local consKey, remKey = "constructionsToAdd", "constructionsToRemove"
	if flavor == "native" then consKey, remKey = "toAdd", "toRemove" end
	local st = get(proposal, flavor == "simple" and "streetProposal" or "proposal")

	for i, ce in ipairs(A.list(proposal, consKey)) do rec.cons[i] = readConstruction(ce, cap) end
	local remIds = A.list(proposal, remKey)
	for i, e in ipairs(remIds) do rec.rem[i] = cap.con(e) end
	cap.hasCons = #rec.cons + #rec.rem > 0
	-- old2new maps a removed construction to what replaces it; as a table keyed by entity ids it cannot travel, so it
	-- travels as pairs of ranks in the remove list
	local o2n = get(proposal, "old2new")
	if o2n ~= nil then
		for i, e in ipairs(remIds) do
			local v = get(o2n, e)
			if v ~= nil then rec.old2new[#rec.old2new + 1] = { i, A.dump(v) } end
		end
	end

	if st ~= nil then
		learnRemoved(st, names, cap)
		-- Read after the build ran (cap.result): the proposal's own nodes and segments carry the entity ids the engine gave them.
		-- They are NEW entities of this build, not references to ones the world had; placeholders again, so that another game
		-- creates them (the airfield of real run 2026-10-11 was shipped as 21 references to nodes nobody else had).
		if cap.result ~= nil then
			for _, kind in ipairs({ { "n", names.nodes }, { "e", names.segs } }) do
				local k = 0
				for _, x in ipairs(A.list(st, kind[2])) do
					local id = get(x, "entity")
					if type(id) == "number" and id >= 0 then
						k = k + 1
						cap.alias[kind[1] .. id] = -k
					end
				end
			end
		end
		local rmSegIds = {}
		for i, s in ipairs(A.list(st, names.rmSegs)) do rmSegIds[i] = idOf(s) end
		indexWorldObjects(rmSegIds, cap)
		for i, n in ipairs(A.list(st, names.nodes)) do rec.nodes[i] = readNode(n, cap) end
		for i, id in ipairs(rmSegIds) do
			-- (read after the build, a removed segment of the stop tool arrived as the NEGATIVE of its entity id: -33244 in a real record,
			-- which no other game can resolve; it is the existing segment)
			if type(id) == "number" and id < -1000 and (A.edgeInfo(-id) or cap.known.edges[-id]) then id = -id; rmSegIds[i] = id end
			-- a removed segment also says what kind of segment it was (normal, bridge, tunnel ...): a replacement by another
			-- kind is not an upgrade the engine's replaceSegment can make
			local ref = cap.edge(id)
			local t = cap.known.edgeTypes[id]
			if not t then
				local c = A.comp(id, "BASE_EDGE")
				if c then t = { ty = get(c, "type"), ti = get(c, "typeIndex"), rt = get(c, "roadType") } end
			end
			if t and type(ref) == "table" then ref.ty, ref.ti, ref.rt, ref.st = t.ty, t.ti, t.rt, t.st end
			rec.rmSegs[i] = ref
		end
		for i, n in ipairs(A.list(st, names.rmNodes)) do rec.rmNodes[i] = cap.node(idOf(n)) end
		for i, s in ipairs(A.list(st, names.segs)) do rec.segs[i] = readSegment(s, cap) end
		removedEdgeFromAdded(rec)
		local ownEdge = {}
		for _, sg in ipairs(rec.segs) do
			if ownTemplate(sg.c and sg.c.roadTemplate) and type(sg.id) == "number" then ownEdge[sg.id] = true end
		end
		for _, o in ipairs(A.list(st, names.objs)) do
			-- An edge object on a segment of a construction's own (the signs of an airfield's taxiways) is made by the construction's
			-- script; the engine names its model only after the build, which is why the tool's preview cannot read it. It is not
			-- described: the record is complete without it and can be shipped before the build.
			local okO, eo = pcall(readEdgeObject, o, #rec.objs + 1, cap)
			if okO then
				rec.objs[#rec.objs + 1] = eo
			else
				local edgeId = get(o, "edgeEntity")
				if edgeId == nil then edgeId = get(o, "segmentEntity") end
				local mine = #rec.cons > 0 and tostring(eo):find("has no model", 1, true)
					and type(edgeId) == "number" and ownEdge[cap.edge(edgeId)]
				if not mine then error(eo, 0) end
				rec.ownObjs = (rec.ownObjs or 0) + 1
			end
		end
		-- A script's edge object does not say what it is; the placeholder that stands for it in its segment's object list
		-- does (0 and 1 a stop on the left or right, 2 a signal), the n-th placeholder for the n-th object.
		local kinds = {}
		for _, s in ipairs(rec.segs) do
			for _, o in ipairs(s.objects or {}) do
				if type(o[1]) == "number" and o[1] < 0 then kinds[#kinds + 1] = o[2] end
			end
		end
		if #kinds == #rec.objs then
			for i, o in ipairs(rec.objs) do
				if o.kind == nil then o.kind = kinds[i] end
			end
		end
		for i, id in ipairs(A.list(st, names.rmObjs)) do rec.rmObjs[i] = cap.obj(id) end
		for i, n in ipairs(A.list(st, names.ncfg)) do rec.ncfg[i] = readNodeConfig(n, cap) end
		for i, id in ipairs(A.list(st, names.rmNcfg)) do rec.rmNcfg[i] = cap.node(id) end
	end
	rec.terrain = readTerrain(proposal)
	return rec
end

local function captureContext(c)
	if c == nil or c == false then return false end
	return {
		align = get(c, "checkTerrainAlignment") == true, cleanup = get(c, "cleanupStreetGraph") == true,
		buildings = get(c, "gatherBuildings") == true, fields = get(c, "gatherFields") == true,
	}
end

-- ================================================================== plans

-- A record is rebuilt in one of a few ways, chosen by its shape. Each way is a function below that returns the
-- arguments of the engine command (and, for plans that need more than one command, a chain of further ones).

-- Segments that are a construction's own although their template is an ordinary street one: the tunnel tram pieces of an underground
-- station entrance (real session 2026-10-13: tunnel type=2, tram_new.street_template, four new nodes, no existing node, 31 m from the
-- station). Rebuilt as street ("combined"), the segments the station's script makes itself were added twice and the engine
-- answered "Construction Not Possible" (critical) on the other game. Kept in a weak table keyed by the segment so the record that goes
-- on the wire stays as the capture made it.
local OWN_BY_SHAPE = setmetatable({}, { __mode = "k" })

local function markOwnByShape(rec)
	if rec.src ~= "native" or #rec.cons == 0 or #rec.rem > 0 or #rec.rmSegs > 0 or #rec.rmNodes > 0 then return end
	-- groups of new nodes joined by the segments to add; a group with an end at a node of the world is a street, not the construction's
	local parent = {}
	local function find(x)
		while parent[x] ~= x do parent[x] = parent[parent[x]]; x = parent[x] end
		return x
	end
	local attached = {}
	for _, sg in ipairs(rec.segs) do
		for _, n in ipairs({ sg.n0, sg.n1 }) do
			if type(n) == "number" and parent[n] == nil then parent[n] = n end
		end
	end
	for _, sg in ipairs(rec.segs) do
		if type(sg.n0) == "number" and type(sg.n1) == "number" then parent[find(sg.n0)] = find(sg.n1) end
	end
	for _, sg in ipairs(rec.segs) do
		if type(sg.n0) ~= "number" or type(sg.n1) ~= "number" then
			for _, n in ipairs({ sg.n0, sg.n1 }) do if type(n) == "number" then attached[find(n)] = true end end
		end
	end
	local function nearCon(p)
		if type(p) ~= "table" then return false end
		for _, c in ipairs(rec.cons) do
			local t = c.transf
			if math.abs(p.x - t[13]) <= 200 and math.abs(p.y - t[14]) <= 200 then return true end
		end
		return false
	end
	for _, sg in ipairs(rec.segs) do
		if type(sg.n0) == "number" and type(sg.n1) == "number" and not attached[find(sg.n0)] and nearCon(sg.c and sg.c.p0) and nearCon(sg.c and sg.c.p1) then
			OWN_BY_SHAPE[sg] = true
		end
	end
end

local function isConstructionSegment(s)
	return OWN_BY_SHAPE[s] == true or ownTemplate(s.c and s.c.roadTemplate)
end

-- the edge objects of a record that sit on a construction's own segments (made by its script) and the others
local function foreignObjCount(rec)
	local own = {}
	for _, sg in ipairs(rec.segs) do
		if isConstructionSegment(sg) and type(sg.id) == "number" then own[sg.id] = true end
	end
	local n = 0
	for _, o in ipairs(rec.objs) do
		if not (type(o.edge) == "number" and own[o.edge]) then n = n + 1 end
	end
	return n
end

local function counts(rec)
	return #rec.cons, #rec.rem, #rec.nodes, #rec.segs, #rec.rmNodes, #rec.rmSegs, #rec.objs, #rec.rmObjs, #rec.ncfg, #rec.rmNcfg
end

-- node references of a segment compared by place: the same pair of nodes in either order
local function nodePair(seg)
	local a, b = seg.n0, seg.n1
	if type(a) ~= "table" or type(b) ~= "table" then return nil end
	return a.p, b.p
end

local function samePlace(a, b) return a and b and d3(a, b) <= TOL_NODE end

-- the removed segment and the added one are of the same kind (a bridge in place of a street is no upgrade)
local function sameKind(rm, s)
	-- (a capture read AFTER the build ran can hold a removed segment as a bare number, an id with no place: real session
	-- 2026-10-13, "attempt to index local 'rm' (a number value)"; such a record is no upgrade the shape test can vouch for)
	if type(rm) ~= "table" then return false end
	local c = s.c or {}
	local function same(a, b) return a == nil or b == nil or a == b end
	return same(rm.ty, c.type) and same(rm.ti, c.typeIndex) and same(rm.rt, c.roadType) and same(rm.st, s.type)
end

-- the added segments that each replace one removed segment between the same nodes, nothing else changed
local function upgradeSteps(rec)
	local nCons, nRem, nNodes, nSegs, nRmNodes, nRmSegs, nObjs, nRmObjs = counts(rec)
	if nCons + nRem + nNodes + nRmNodes + nObjs + nRmObjs > 0 or nSegs == 0 or nSegs ~= nRmSegs then return nil end
	local steps = {}
	local used = {}
	for _, s in ipairs(rec.segs) do
		local a, b = nodePair(s)
		if not a then return nil end
		local match
		for j, rm in ipairs(rec.rmSegs) do
			if not used[j] and type(rm) == "table" and sameKind(rm, s) and ((samePlace(rm.p0, a) and samePlace(rm.p1, b)) or (samePlace(rm.p0, b) and samePlace(rm.p1, a))) then
				match = j
				break
			end
		end
		if not match then return nil end
		used[match] = true
		steps[#steps + 1] = { edge = rec.rmSegs[match], seg = s }
	end
	return steps
end

-- A pure bulldozing: the removed segments that touch none of the ends of the segments the engine re-added when it
-- merged what was left (finding from MPFever (MIT): every game's engine redoes the same merge).
local function removalList(rec)
	local nCons, nRem, nNodes, nSegs, nRmNodes, nRmSegs, nObjs = counts(rec)
	if nCons > 0 or nNodes > 0 or nObjs > 0 or nRmSegs == 0 then return nil end
	local ends = {}
	for _, s in ipairs(rec.segs) do
		if type(s.n0) ~= "table" or type(s.n1) ~= "table" then return nil end
		ends[#ends + 1] = s.n0.p
		ends[#ends + 1] = s.n1.p
	end
	local list = {}
	for _, rm in ipairs(rec.rmSegs) do
		local touches = type(rm) ~= "table"   -- a bare id has no place to compare: not listed
		for _, p in ipairs(ends) do
			if type(rm) == "table" and (samePlace(rm.p0, p) or samePlace(rm.p1, p)) then touches = true end
		end
		if not touches then list[#list + 1] = rm end
	end
	if #list == 0 then return nil end
	return list
end

-- A stop or signal on an existing segment: the segment replaced by itself with one more object.
local function isStopShape(rec)
	local nCons, nRem, nNodes, nSegs, nRmNodes, nRmSegs, nObjs, nRmObjs = counts(rec)
	return nCons + nRem + nNodes + nRmNodes == 0 and nSegs == 1 and nRmSegs == 1 and nObjs + nRmObjs > 0
end

-- The shape of a record: construction, withStreet, combined, bulldoze, upgrade, removal, stop, street, nodecfg, or nil
-- and why not. (Every case that has something to add to the street is a plan that adds it.)
local function classify(rec)
	local nCons, nRem, nNodes, nSegs, nRmNodes, nRmSegs, nObjs, nRmObjs, nCfg, nRmCfg = counts(rec)
	local street = nNodes + nSegs + nRmNodes + nRmSegs + nObjs + nRmObjs + nCfg + nRmCfg
	if nCons + nRem == 0 and street == 0 then
		if rec.terrain then return nil, "a terrain edit: its cells cannot be read from a script, only the native module can copy them" end
		return nil, "nothing to build"
	end
	if nCons + nRem > 0 then
		if street == 0 then return "construction" end
		markOwnByShape(rec)
		-- the segments a construction brings with it belong to it; its own script makes them
		local own, foreign = 0, 0
		for _, s in ipairs(rec.segs) do
			if isConstructionSegment(s) then own = own + 1 else foreign = foreign + 1 end
		end
		if foreign == 0 and nRmSegs == 0 and foreignObjCount(rec) + nRmObjs == 0 then return "construction" end
		-- constructions taken away with segments, and nothing new but what the engine re-adds when it merges: a bulldozing
		if nCons == 0 and nNodes == 0 and nObjs == 0 and nRmSegs > 0 then return "bulldoze" end
		-- a depot or station against a street: street first, construction second; any other mixture (a road that takes
		-- town buildings out of its way, with or without putting them elsewhere) is one proposal
		if nCons > 0 and own > 0 then return "withStreet" end
		return "combined"
	end
	if upgradeSteps(rec) then return "upgrade" end
	if removalList(rec) then return "removal" end
	if isStopShape(rec) then return "stop" end
	if nNodes + nSegs + nRmNodes + nRmSegs + nObjs + nRmObjs > 0 then return "street" end
	return "nodecfg"
end

-- ---- pieces the plans share

-- The Context of the command: none when the original had none; the plan's default when the build comes from a native
-- tool (whose context a script cannot see); otherwise the original's flags. Always for this game's player.
local function newContext(c, default, R)
	if type(c) ~= "table" then return nil end
	local f = c.native and default or c
	if f == nil then return nil end
	local obj = A.new("Context")
	req(obj, "player", R.owner("player"))
	pcall(function() obj.checkTerrainAlignment = f.align == true end)
	pcall(function() obj.cleanupStreetGraph = f.cleanup == true end)
	pcall(function() obj.gatherBuildings = f.buildings == true end)
	pcall(function() obj.gatherFields = f.fields == true end)
	return obj
end

local guardProposal   -- defined after newNodeConfig

local function commandArgs(proposal, ctxObj, ie, pi, dust)
	guardProposal(proposal)
	return { n = 5, proposal, ctxObj, ie, pi, dust }
end

-- A seed for a construction that has none, from where it stands (the game's own scripts do the same), so that every
-- game's construction script makes the same choices.
local function seedFor(m)
	return 7 * m[13] + 3 * m[14] + m[15]
end

local function baseName(file)
	local b = tostring(file or "construction"):match("([^/]+)$") or "construction"
	return (b:gsub("%.con$", ""))
end

local function newConstructionEntity(c, R)
	local ce = A.new("ConstructionEntity")
	local params = C.plain(c.params or {})
	if params.seed == nil then params.seed = seedFor(c.transf) end
	req(ce, "fileName", c.file)
	req(ce, "params", params)
	req(ce, "transf", A.mat4(c.transf))
	req(ce, "playerEntity", R.owner(c.owner == nil and "player" or c.owner))
	pcall(function() ce.name = (type(c.name) == "string" and c.name ~= "") and c.name or baseName(c.file) end)
	return ce
end

local function newNodeAndEntity(n, R)
	local ne = A.new("NodeAndEntity")
	req(ne, "entity", R.nodeOr(n.id))
	local comp = ne.comp
	req(comp, "position", A.vec3(n.pos.x, n.pos.y, n.pos.z))
	req(ne, "comp", comp)
	return ne
end

-- One segment to add. `placeholder` is the id the segment gets (a negative number); objects the segment already carries
-- are named by rank on a removed segment and mapped to this game's objects.
local function newSegmentAndEntity(s, R, notes, placeholder, form)
	form = form or {}
	local se = A.new("SegmentAndEntity")
	req(se, "entity", placeholder or R.edgeOr(s.id))
	if s.type ~= nil then req(se, "type", s.type) end
	local c = se.comp
	req(c, "node0", R.nodeOr(s.n0))
	req(c, "node1", R.nodeOr(s.n1))
	local sc = s.c or {}
	for _, k in ipairs({ "type", "typeIndex", "roadType", "roadTemplate", "roadStyle" }) do
		if sc[k] ~= nil then req(c, k, sc[k]) end
	end
	if sc.dev ~= nil then opt(notes, c, "roadDevelopmentLocked", sc.dev) end
	for k, name in pairs({ t0 = "tangent0", t1 = "tangent1", p0 = "position0", p1 = "position1" }) do
		local v = sc[k]
		if v then req(c, name, A.vec3(v.x, v.y, v.z)) end
	end
	if type(sc.distance) == "number" and sc.distance ~= 0 then opt(notes, c, "distance", sc.distance) end
	if type(sc.deco) == "table" and #sc.deco > 0 then opt(notes, c, "edgeDecorations", C.plain(sc.deco)) end
	local lanes
	if s.lanes then
		lanes = A.rebuildLanes(s.lanes)
	else
		lanes = A.templateLanes(sc.roadTemplate)
	end
	if not lanes then fail("no lanes for street template " .. tostring(sc.roadTemplate)) end
	req(c, "laneConfigs", lanes)
	local objects = {}
	for i, o in ipairs(s.objects or {}) do objects[i] = { R.objOr(o[1]), o[2] } end
	if #objects > 0 then req(c, "objects", objects) end
	req(se, "comp", c)
	if s.track then
		-- only what differs from a fresh BaseEdgeTrack is set: a road segment (the default) stays exactly as it was
		local okT, te = pcall(A.new, "BaseEdgeTrack")
		if okT then
			local d = A.new("BaseEdgeTrack")
			local dTT, dCat = get(d, "trackType"), get(d, "catenary")
			local want, wantCat = s.track.tt, (s.track.cat or 0) == 1
			if want ~= dTT or wantCat ~= (dCat == true) then
				opt(notes, te, "trackType", want)
				opt(notes, te, "catenary", wantCat)
				opt(notes, se, "trackEdge", te)
			end
		end
	end
	if form.owner ~= false then
		if s.prec then
			local st = se.streetEdge
			opt(notes, st, "precedenceNode0", s.prec[1] or 2)
			opt(notes, st, "precedenceNode1", s.prec[2] or 2)
			opt(notes, se, "streetEdge", st)
		end
		if s.owner ~= nil then
			local po = A.new("PlayerOwned")
			req(po, "player", R.owner(s.owner))
			opt(notes, se, "playerOwned", po)
		end
	end
	return se
end

-- The model of an edge object is named without the resource prefix "::/" in a proposal (finding from MPFever (MIT)); the
-- name read from a created object may carry it. The raw name is the second form.
local function newEdgeObject(o, R, edgeId, notes, form)
	form = form or {}
	local eo = A.new("EdgeObject")
	req(eo, "edgeEntity", edgeId)
	req(eo, "model", form.rawModel and o.model or (o.model:gsub("^::/", "")))
	req(eo, "param", o.param or 0.5)
	req(eo, "left", o.left == true)
	req(eo, "oneWay", o.oneWay == true)
	local owner = R.owner(o.owner == nil and "player" or o.owner)
	if form.owner == false then opt(notes, eo, "playerEntity", owner) else req(eo, "playerEntity", owner) end
	opt(notes, eo, "name", o.name or "")
	return eo
end

local function newNodeConfig(n, R)
	local nc = A.new("NodeConfig")
	req(nc, "entity", R.nodeOr(n.node))
	local cfg = nc.comp
	local lcs = {}
	for i, l in ipairs(n.lc or {}) do
		local lc = A.new("LaneConnection")
		req(lc, "segment0", R.edgeOr(l.s0)); req(lc, "lane0", l.l0)
		req(lc, "segment1", R.edgeOr(l.s1)); req(lc, "lane1", l.l1)
		req(lc, "withRoad", l.road == true); req(lc, "withTram", l.tram == true)
		lcs[i] = lc
	end
	req(cfg, "laneConnections", lcs)
	local cw = {}
	for i, x in ipairs(n.cw or {}) do cw[i] = R.edgeOr(x) end
	req(cfg, "crosswalks", cw)
	pcall(function() cfg.trafficLightPreference = n.pref end)
	pcall(function() cfg.doubleSlipSwitch = n.dss == true end)
	pcall(function() cfg.userModifiedLaneConnections = n.user == true end)
	req(nc, "comp", cfg)
	return nc
end

-- The last check before a proposal is handed to the engine, for every plan and every form (commandArgs below): the lists of
-- node configurations and edge objects it removes or adds must name things THIS game has. The engine does not refuse a removal
-- of a node configuration that is not there, it aborts the whole game (Engine.h:323 ecs::Engine::GetComponentDataIndex
-- "it != components.end()", Component Type Index 30 = BaseNodeConfig; sweep run 20261013-044920, guest, entity 49832, and the
-- host of the real session 2026-10-12/13 after a replayed withStreet build). MPFever does the same existence test before each
-- removal. A dropped item is logged (trace category build, event build.guard); the rest of the proposal is unchanged.
-- The forms that leave node configurations out (cfg = false) are covered by the same check: a new node of three or more segments
-- gets the default entry (preference 2, no lane connections, not user-modified, the real tool's own entry for a new node).
-- Returns the number of items dropped or added. Never raises.
guardProposal = function(proposal)
	local dropped = 0
	pcall(function()
		local flavor = flavorOf(proposal)
		if not flavor then return end
		local names = NAMES[flavor]
		local key = flavor == "simple" and "streetProposal" or "proposal"
		local st = get(proposal, key)
		if st == nil then return end
		local trace = C.tracer("build")
		local changed = false
		local function entityOf(x)
			if type(x) == "number" then return x end
			local e = get(x, "entity")
			if type(e) == "number" then return e end
			return nil
		end
		local function ids(list)
			local set = {}
			for _, x in ipairs(A.list(st, list)) do
				local e = entityOf(x)
				if e then set[e] = true end
			end
			return set
		end
		local newNode, goneNode = ids(names.nodes), ids(names.rmNodes)
		-- keeps the entries that pass `ok`; the others are logged with `what`
		local function filter(field, ok, what)
			local list = A.list(st, field)
			local keep = {}
			for _, x in ipairs(list) do
				local e = entityOf(x)
				local pass, why = ok(e)
				if pass then keep[#keep + 1] = x
				else
					dropped = dropped + 1
					trace("build.guard", string.format("dropped %s %s=%s (%s)", field, what, tostring(e), why))
				end
			end
			if #keep < #list then
				st[field] = keep
				changed = true
			end
		end
		filter(names.rmNcfg, function(e)
			if e and A.comp(e, "BASE_NODE_CONFIG") then return true end
			return false, "no BASE_NODE_CONFIG"
		end, "node")
		filter(names.ncfg, function(e)
			if e and e < 0 and newNode[e] then return true end
			if e and e >= 0 and not goneNode[e] and A.comp(e, "BASE_NODE") then return true end
			return false, "no such node in this game or in the proposal"
		end, "node")
		filter(names.rmObjs, function(e)
			if e and A.comp(e, "EDGE_OBJECT") then return true end
			return false, "no EDGE_OBJECT"
		end, "object")
		-- a lane connection or crosswalk that names a segment this proposal removes is dangling (MPFever: "the engine fails a lookup, map_util Get
		-- assert"): dropped. The stop plan maps its own replaced segment to the replacement before it gets here.
		do
			local goneEdge = ids(names.rmSegs)
			if next(goneEdge) then
				local list = A.list(st, names.ncfg)
				local touched = false
				for i, nc in ipairs(list) do
					local comp = get(nc, "comp")
					if comp ~= nil then
						local lcs, keepL, cws, keepC = A.list(comp, "laneConnections"), {}, A.list(comp, "crosswalks"), {}
						for _, l in ipairs(lcs) do
							local a, b = get(l, "segment0"), get(l, "segment1")
							if goneEdge[a] or goneEdge[b] then
								trace("build.guard", string.format("dropped lane connection of node %s (names the removed segment %s)", tostring(get(nc, "entity")), tostring(goneEdge[a] and a or b)))
								dropped = dropped + 1
							else keepL[#keepL + 1] = l end
						end
						for _, x in ipairs(cws) do
							if goneEdge[x] then
								trace("build.guard", string.format("dropped crosswalk of node %s (names the removed segment %s)", tostring(get(nc, "entity")), tostring(x)))
								dropped = dropped + 1
							else keepC[#keepC + 1] = x end
						end
						if #keepL < #lcs or #keepC < #cws then
							comp.laneConnections = keepL
							comp.crosswalks = keepC
							nc.comp = comp
							list[i] = nc
							touched = true
						end
					end
				end
				if touched then
					st[names.ncfg] = list
					changed = true
				end
			end
		end
		-- a NEW node that joins three or more of the segments the proposal adds needs an entry (real records: a new node of one or two
		-- segments has one only sometimes, so the threshold of three is the harness finding, not yet seen in a real record)
		local joins = {}
		for _, sg in ipairs(A.list(st, names.segs)) do
			local c = get(sg, "comp")
			for _, k in ipairs({ "node0", "node1" }) do
				local n = c and get(c, k)
				if type(n) == "number" and n < 0 and newNode[n] then joins[n] = (joins[n] or 0) + 1 end
			end
		end
		local have = ids(names.ncfg)
		local extra = {}
		for n, cnt in pairs(joins) do
			if cnt >= 3 and not have[n] then extra[#extra + 1] = n end
		end
		table.sort(extra, function(x, y) return x > y end)
		if #extra > 0 then
			local list = A.list(st, names.ncfg)
			local stub = { nodeOr = function(v) return v end, edgeOr = function(v) return v end }
			for _, n in ipairs(extra) do
				list[#list + 1] = newNodeConfig({ node = n, lc = {}, cw = {}, pref = 2 }, stub)
				dropped = dropped + 1
				trace("build.guard", string.format("added nodeConfig for new node %d (joins %d segments, no entry in the proposal)", n, joins[n]))
			end
			st[names.ncfg] = list
			changed = true
		end
		if changed then proposal[key] = st end
	end)
	if dropped > 0 then log("guard: repaired " .. dropped .. " node configuration / edge object entr" .. (dropped == 1 and "y" or "ies") .. " (see build.guard in the trace)") end
	return dropped
end

-- A SimpleProposal's street part, read, changed and written back (properties return copies).
local function withStreet(sp, fn)
	local ssp = sp.streetProposal
	fn(ssp)
	sp.streetProposal = ssp
end

-- The record with its placeholders renumbered the way a script proposal numbers them: nodes and segments each from -1 in
-- list order (a tool numbers all new entities in one sequence).
local function renumbered(rec)
	local nodeMap, segMap, kn, ks = {}, {}, 0, 0
	for _, n in ipairs(rec.nodes) do
		if type(n.id) == "number" and n.id < 0 then kn = kn + 1; nodeMap[n.id] = -kn end
	end
	for _, s in ipairs(rec.segs) do
		if type(s.id) == "number" and s.id < 0 then ks = ks + 1; segMap[s.id] = -ks end
	end
	local function mapN(v) if type(v) == "number" and nodeMap[v] then return nodeMap[v] end return v end
	local function mapS(v) if type(v) == "number" and segMap[v] then return segMap[v] end return v end
	local function copy(t, fn) local o = {}; for k, v in pairs(t) do o[k] = v end; fn(o); return o end
	local out = {}
	for k, v in pairs(rec) do out[k] = v end
	out.nodes, out.segs, out.objs, out.ncfg = {}, {}, {}, {}
	for i, n in ipairs(rec.nodes) do out.nodes[i] = copy(n, function(o) o.id = mapN(n.id) end) end
	for i, s in ipairs(rec.segs) do out.segs[i] = copy(s, function(o) o.id, o.n0, o.n1 = mapS(s.id), mapN(s.n0), mapN(s.n1) end) end
	for i, e in ipairs(rec.objs) do out.objs[i] = copy(e, function(o) o.edge = mapS(e.edge) end) end
	for i, c in ipairs(rec.ncfg) do
		out.ncfg[i] = copy(c, function(o)
			o.node = mapN(c.node)
			o.lc, o.cw = {}, {}
			for j, l in ipairs(c.lc or {}) do o.lc[j] = copy(l, function(x) x.s0, x.s1 = mapS(l.s0), mapS(l.s1) end) end
			for j, x in ipairs(c.cw or {}) do o.cw[j] = mapS(x) end
		end)
	end
	return out
end

-- The street half of a record as a SimpleProposal. opts (a form, see FORMS): drop leaves out the segments a construction
-- brings with it; cfg = false leaves out the node configurations; owner = false the owner and precedence members;
-- renumber numbers the placeholders as a script does.
local function streetProposal(rec, R, notes, opts)
	opts = opts or {}
	if opts.renumber then rec = renumbered(rec) end
	local sp = A.new("SimpleProposal")
	local keepSeg, usedNode, dropped = {}, {}, {}
	for _, s in ipairs(rec.segs) do
		if opts.drop and isConstructionSegment(s) then
			if type(s.id) == "number" then dropped[s.id] = true end
		else
			keepSeg[#keepSeg + 1] = s
			if type(s.n0) == "number" then usedNode[s.n0] = true end
			if type(s.n1) == "number" then usedNode[s.n1] = true end
		end
	end
	local nodes, edges, objs = {}, {}, {}
	for _, n in ipairs(rec.nodes) do
		if not opts.drop or type(n.id) ~= "number" or usedNode[n.id] then nodes[#nodes + 1] = newNodeAndEntity(n, R) end
	end
	-- Lenient ends: a segment to add that ends at a node of the world which this game does not have (the shipped result says where
	-- it must be) gets a new node there instead of refusing the whole build. Only for the ends of segments the record adds, and only
	-- when no segment of this world passes through the place (that would be a crossing, not a missing node); the record is
	-- a tool's result (src native), so the position is the originator's real one. Before this, such a build was refused outright.
	local RS = R
	if rec.src == "native" and not opts.strict then
		local lowest = 0
		for _, x in ipairs(rec.nodes) do if type(x.id) == "number" and x.id < lowest then lowest = x.id end end
		for _, x in ipairs(rec.segs) do if type(x.id) == "number" and x.id < lowest then lowest = x.id end end
		local made = {}
		local function lenient(ref)
			local okR, e = pcall(R.node, ref)
			if okR then return e end
			if not tostring(e):find("^unresolved node") or type(ref) ~= "table" or not ref.p then error(e, 0) end
			local k = pkey(ref.p)
			if made[k] then return made[k] end
			if A.edgeNear and A.edgeNear(ref.p, 0.5) then error(e, 0) end
			lowest = lowest - 1
			made[k] = lowest
			nodes[#nodes + 1] = newNodeAndEntity({ id = lowest, pos = ref.p }, R)
			log("lenient: no node at " .. k .. " on this game, a new node is made there (the shipped result says it exists)")
			return lowest
		end
		RS = setmetatable({ node = lenient, nodeOr = function(v) if type(v) == "table" then return lenient(v) end return v end }, { __index = R })
	end
	for i, s in ipairs(keepSeg) do
		edges[i] = newSegmentAndEntity(s, RS, notes, type(s.id) == "number" and s.id or nil, opts)
	end
	local rmNodes, rmEdges, rmObjs = {}, {}, {}
	for i, ref in ipairs(rec.rmNodes) do rmNodes[i] = R.node(ref) end
	for i, ref in ipairs(rec.rmSegs) do rmEdges[i] = R.edge(ref) end
	for i, ref in ipairs(rec.rmObjs) do rmObjs[i] = R.obj(ref) end
	for _, o in ipairs(rec.objs) do
		-- (with `drop`, the objects on the dropped segments of a construction's own go with them)
		if not (opts.drop and type(o.edge) == "number" and dropped[o.edge]) then objs[#objs + 1] = newEdgeObject(o, R, R.edgeOr(o.edge), notes, opts) end
	end
	local cfgs, rmCfgs = {}, {}
	if not opts.drop and opts.cfg ~= false then
		for i, n in ipairs(rec.ncfg) do cfgs[i] = newNodeConfig(n, R) end
	end
	for i, ref in ipairs(rec.rmNcfg) do rmCfgs[i] = R.node(ref) end
	withStreet(sp, function(ssp)
		req(ssp, "nodesToAdd", nodes)
		req(ssp, "edgesToAdd", edges)
		req(ssp, "nodesToRemove", rmNodes)
		req(ssp, "edgesToRemove", rmEdges)
		if #objs > 0 then req(ssp, "edgeObjectsToAdd", objs) end
		if #rmObjs > 0 then req(ssp, "edgeObjectsToRemove", rmObjs) end
		if #cfgs > 0 then opt(notes, ssp, "nodeConfigsToAdd", cfgs) end
		if #rmCfgs > 0 then opt(notes, ssp, "nodeConfigsToRemove", rmCfgs) end
	end)
	return sp
end

-- The constructions to add, the entities to remove, and the map from a removed one to what replaces it.
local function constructionLists(rec, R)
	local cons, rem, map = {}, {}, {}
	for i, c in ipairs(rec.cons) do cons[i] = newConstructionEntity(c, R) end
	for i, ref in ipairs(rec.rem) do rem[i] = R.con(ref) end
	for _, pair in ipairs(rec.old2new) do
		if rem[pair[1]] then map[rem[pair[1]]] = pair[2] end
	end
	return cons, rem, map
end

local function setConstructions(sp, rec, R)
	local cons, rem, map = constructionLists(rec, R)
	req(sp, "constructionsToAdd", cons)
	req(sp, "constructionsToRemove", rem)
	req(sp, "old2new", map)
end

-- The construction half of a record as a SimpleProposal (the form of the game's own mission scripts).
local function constructionProposal(rec, R)
	local sp = A.new("SimpleProposal")
	setConstructions(sp, rec, R)
	return sp
end

-- ---- the plans

local PLAN = {}

-- a record with only some of its parts (the steps of a plan that needs more than one command)
local function part(rec, keep)
	local out = { v = rec.v, src = rec.src, cons = {}, rem = {}, old2new = {}, nodes = {}, segs = {}, rmNodes = {}, rmSegs = {},
		objs = {}, rmObjs = {}, ncfg = {}, rmNcfg = {} }
	for _, k in ipairs(keep) do out[k] = rec[k] end
	return out
end

-- Ways to build the proposal of a plan, tried in order until the engine's factory accepts one (a factory that rejects has
-- no side effect, so every game goes through the same attempts: finding from MPFever (MIT), whose forms these follow). A
-- form names what to leave out or change. A form that drops content (cfg = false) is only listed where the engine makes
-- that content itself.
local FORMS = {
	street = { {}, { cfg = false }, { cfg = false, owner = false }, { cfg = false, owner = false, renumber = true } },
	withStreet = { {}, { owner = false }, { owner = false, renumber = true } },
	stop = { {}, { rawModel = true }, { owner = false } },
}
FORMS.combined = FORMS.street

local function with(form, extra)
	local o = {}
	for k, v in pairs(form or {}) do o[k] = v end
	for k, v in pairs(extra) do o[k] = v end
	return o
end

-- The connection of a construction to the street (a depot's entrance piece that ends at a node the world already had) is part of
-- the originator's native result, but a replay that hands the engine the construction alone may not reproduce it: a road that is
-- then built from that piece cannot be resolved on the other game (real run 2026-10-11: "unresolved segment 923.952,6856.501 -
-- 919.444,6840.109" for the entrance of a depot, the same place as the previous run's refused 0:5). After the construction,
-- this step looks whether each such piece exists and builds the missing ones where the shipped result says they are. It says
-- which it was (the first-cause note of the log); `false` when nothing is missing, so a replay that already has it does nothing.
local function connectorStep(rec, R, f, notes)
	return function()
		local pos = {}
		for _, n in ipairs(rec.nodes) do if type(n.id) == "number" then pos[n.id] = n.pos end end
		local mini = part(rec, {})
		mini.src = rec.src
		local lowest = 0
		for _, s in ipairs(rec.segs) do
			if s.connector then
				local first = type(s.n0) == "table"
				local refEnd, newEnd = first and s.n0 or s.n1, first and s.n1 or s.n0
				local newPos = pos[newEnd]
				local label = pkey(s.c.p0) .. " - " .. pkey(s.c.p1)
				local okE, en = pcall(R.node, refEnd)
				if not newPos or not okE then
					log("connector " .. label .. ": cannot be checked here (" .. (okE and "no position for its new end" or tostring(en)) .. ")")
				else
					local exists = false
					for _, ed in ipairs(A.edgesAt(en)) do
						local info = A.edgeInfo(ed)
						if info then
							local other = d3(info.p0, refEnd.p) <= TOL_NODE and info.p1 or info.p0
							if d3(other, newPos) <= 1.5 then exists = true end
						end
					end
					if exists then
						log("connector " .. label .. ": this game's construction made it itself")
					else
						log("connector " .. label .. ": this game's replay did not make the street connection the originator's result has; it is built here")
						local newRef = { __ref = "node", p = newPos }
						local okN = pcall(R.node, newRef)
						local endRef = newEnd
						if okN then
							endRef = newRef
						else
							lowest = lowest - 1
							mini.nodes[#mini.nodes + 1] = { id = lowest, pos = newPos }
							endRef = lowest
						end
						lowest = lowest - 1
						local seg = {}
						for k, v in pairs(s) do seg[k] = v end
						seg.id = lowest
						if first then seg.n1 = endRef else seg.n0 = endRef end
						mini.segs[#mini.segs + 1] = seg
					end
				end
			end
		end
		if #mini.segs == 0 then return false end
		local okP, args = pcall(function()
			local sp = streetProposal(mini, R, notes, { cfg = false })
			return commandArgs(sp, newContext(f.ctx, CTX_NATIVE, R), true, false, f.dust)
		end)
		if okP then
			local okF, why = pcall(function() return api.cmd.makeWorldBuildProposalCmd(unpack(args, 1, args.n)) end)
			if not okF then okP, args = false, why end
		end
		if not okP then
			log("connector: cannot be built here (" .. tostring(args) .. "); the games will differ at this place until a resync")
			return false
		end
		return args
	end
end

PLAN.construction = function(rec, R, f, notes)
	-- A construction that brings its own street piece (a station or depot entrance, even one that ends at a node already
	-- there) collides with that piece in the engine's check, a non-critical error that stops the build unless errors are
	-- ignored. The plan `withStreet` already builds such a construction that way (its second command); a tool's build
	-- that has no street of its own to put first was refused on both games in a real session (2026-10-11, a cargo station)
	-- when it was rebuilt with the flags of the tool's capture.
	local ie, pi = f.ie, f.pi
	if rec.src == "native" then
		for _, s in ipairs(rec.segs) do
			if isConstructionSegment(s) then ie, pi = true, false break end
		end
	end
	local first = commandArgs(constructionProposal(rec, R), newContext(f.ctx, CTX_NATIVE, R), ie, pi, f.dust)
	if rec.src == "native" then
		for _, s in ipairs(rec.segs) do
			if s.connector then
				first.chain = { connectorStep(rec, R, f, notes) }
				break
			end
		end
	end
	return first
end

-- Street part first, then the construction on the street that is now cut (finding from MPFever (MIT): the engine
-- refuses both in one proposal). The construction's own paths are left to its script; the second command ignores the
-- non-critical collision with its own street connection.
PLAN.withStreet = function(rec, R, f, notes, form)
	local first = commandArgs(streetProposal(rec, R, notes, with(form, { drop = true })), newContext(f.ctx, CTX_NATIVE, R), f.ie, f.pi, f.dust)
	local conOnly = part(rec, { "cons", "rem", "old2new" })
	first.chain = { function()
		return commandArgs(constructionProposal(conOnly, R), newContext(f.ctx, CTX_NATIVE, R), true, false, f.dust)
	end }
	return first
end

-- Street work that also moves or replaces constructions (a road that takes town buildings out of its way and puts them
-- elsewhere): one proposal with both parts, as MPFever rebuilt them (MIT).
PLAN.combined = function(rec, R, f, notes, form)
	local sp = streetProposal(rec, R, notes, form)
	setConstructions(sp, rec, R)
	return commandArgs(sp, newContext(f.ctx, CTX_NATIVE, R), f.ie, f.pi, f.dust)
end

-- A bulldozing that takes constructions and street pieces: the constructions first, then the segments (the order the
-- game's own industry removal uses).
PLAN.bulldoze = function(rec, R, f, notes)
	local first = commandArgs(constructionProposal(part(rec, { "rem", "old2new" }), R), newContext(f.ctx, CTX_PLAIN, R), f.ie, f.pi, f.dust)
	local list = removalList(rec)
	if list then
		first.chain = { function()
			local edges = {}
			for i, ref in ipairs(list) do edges[i] = R.edge(ref) end
			return commandArgs(A.removeSegmentsProposal(edges), newContext(f.ctx, CTX_TERRAIN, R), f.ie, f.pi, f.dust)
		end }
	end
	return first
end

PLAN.street = function(rec, R, f, notes, form)
	return commandArgs(streetProposal(rec, R, notes, form), newContext(f.ctx, CTX_NATIVE, R), f.ie, f.pi, f.dust)
end

PLAN.nodecfg = PLAN.street

PLAN.removal = function(rec, R, f, notes)
	local edges = {}
	for i, ref in ipairs(removalList(rec)) do edges[i] = R.edge(ref) end
	local P = A.removeSegmentsProposal(edges)
	if P == nil then fail("the engine made no removal proposal") end
	return commandArgs(P, newContext(f.ctx, CTX_TERRAIN, R), f.ie, f.pi, f.dust)
end

-- Upgrades one segment per command with the engine's replaceSegment, as the upgrade tool does (finding from MPFever
-- (MIT)); decorations and custom lanes the template does not give are put on the engine's proposal.
PLAN.upgrade = function(rec, R, f, notes)
	local steps = upgradeSteps(rec)
	local edges = {}
	for i, st in ipairs(steps) do edges[i] = R.edge(st.edge) end
	local function stepArgs(i)
		local st, edge = steps[i], edges[i]
		local P = A.replaceSegment(edge, st.seg.c.roadTemplate)
		if P == nil then fail("the engine made no upgrade proposal for " .. describe(st.edge)) end
		local deco, lanes = st.seg.c.deco, st.seg.lanes
		if (type(deco) == "table" and #deco > 0) or lanes then
			local sp = P.proposal
			local list = sp.addedSegments
			local seg = list[1]
			local cmp = seg.comp
			if type(deco) == "table" and #deco > 0 then
				local d = C.plain(deco)
				-- a decoration has a side relative to the segment's direction: swapped when this game's segment runs the
				-- other way round than the originator's
				local info = A.edgeInfo(edge)
				if info and not samePlace(info.p0, st.seg.n0.p) then
					for _, x in ipairs(d) do if type(x) == "table" then x[2] = not x[2] end end
				end
				cmp.edgeDecorations = d
			end
			if lanes then cmp.laneConfigs = A.rebuildLanes(lanes) end
			seg.comp = cmp
			list[1] = seg
			sp.addedSegments = list
			P.proposal = sp
		end
		return commandArgs(P, newContext(f.ctx, CTX_TERRAIN, R), f.ie, f.pi, f.dust)
	end
	local first = stepArgs(1)
	if #steps > 1 then
		first.chain = {}
		for i = 2, #steps do first.chain[i - 1] = function() return stepArgs(i) end end
	end
	return first
end

-- A stop or signal: the segment is replaced by one carrying the new object; placeholders mark the new objects in its
-- object list (finding from MPFever (MIT)).
PLAN.stop = function(rec, R, f, notes, form)
	form = form or {}
	local s, rm = rec.segs[1], rec.rmSegs[1]
	local edge = R.edge(rm)
	local info = A.edgeInfo(edge)
	if not info then fail("the segment for a stop has vanished") end
	local sp = A.new("SimpleProposal")
	-- the objects already on the segment stay, then one placeholder per new object (-400000000 - k)
	local keep = {}
	local gone = {}
	for _, ref in ipairs(rec.rmObjs) do gone[R.obj(ref)] = true end
	for _, o in ipairs(A.objectsOf(edge)) do
		if not gone[o[1]] then keep[#keep + 1] = { o[1], o[2] } end
	end
	for i, o in ipairs(rec.objs) do
		local kind = (o.kind == 2) and 2 or (o.left and 0 or 1)
		keep[#keep + 1] = { -400000000 - (i - 1), kind }
	end
	local seg = { id = -1, type = s.type, n0 = info.n0, n1 = info.n1, c = s.c, lanes = s.lanes, objects = {}, prec = s.prec or { 2, 2 }, owner = s.owner }
	local e = newSegmentAndEntity(seg, R, notes, -1, form)
	-- the segment keeps this game's own ends, tangents and positions, and carries the object list built above
	local c = e.comp
	req(c, "node0", info.n0); req(c, "node1", info.n1)
	if info.t0 then req(c, "tangent0", A.vec3(info.t0.x, info.t0.y, info.t0.z)) end
	if info.t1 then req(c, "tangent1", A.vec3(info.t1.x, info.t1.y, info.t1.z)) end
	req(c, "position0", A.vec3(info.p0.x, info.p0.y, info.p0.z))
	req(c, "position1", A.vec3(info.p1.x, info.p1.y, info.p1.z))
	req(c, "objects", keep)
	req(e, "comp", c)
	local eos, removeIds, cfgs = {}, {}, {}
	-- What the two games see on the replaced segment, both sides in the log: the objects this game's segment carries (kept), and the objects the
	-- record's segment carried (placeholders -4e8-k are the new ones). Sweep run 20261013-060028 (B3): a record whose removed segment
	-- (737,-31 -> 694,-110) was not the segment it added (737,-31 -> 676,2.5) was refused by the factory ("Unknown exception").
	pcall(function()
		local mine, theirs = {}, {}
		for _, o in ipairs(A.objectsOf(edge)) do mine[#mine + 1] = tostring(o[1]) .. "/" .. tostring(o[2]) end
		for _, o in ipairs(s.objects or {}) do
			local id = type(o) == "table" and o[1] or o
			theirs[#theirs + 1] = (type(id) == "table" and tostring(id.key or "ref") or tostring(id)) .. "/" .. tostring(type(o) == "table" and o[2] or "?")
		end
		local msg = string.format("replaced segment %d: objects here [%s], in the record [%s]", edge, table.concat(mine, " "), table.concat(theirs, " "))
		local sc = s.c or {}
		local function far(a, b) return a and b and (abs(a.x - b.x) > 1 or abs(a.y - b.y) > 1) end
		if far(sc.p0, info.p0) or far(sc.p1, info.p1) then
			msg = msg .. string.format("; WARNING the record's new segment (%.0f,%.0f -> %.0f,%.0f) is not the segment it removes (%.0f,%.0f -> %.0f,%.0f)",
				(sc.p0 or {}).x or 0, (sc.p0 or {}).y or 0, (sc.p1 or {}).x or 0, (sc.p1 or {}).y or 0, info.p0.x, info.p0.y, info.p1.x, info.p1.y)
		end
		log("stop: " .. msg)
		C.tracer("build")("build.stop", msg)
	end)
	-- every lane connection and crosswalk of the node configurations that names the replaced segment names its replacement (-1): the removed
	-- segment is gone once the proposal runs, a configuration that still names it is the dangling lookup the engine fails on
	local RC = setmetatable({ edgeOr = function(v)
		local e2 = R.edgeOr(v)
		if e2 == edge then return -1 end
		return e2
	end }, { __index = R })
	for i, n in ipairs(rec.ncfg) do cfgs[i] = newNodeConfig(n, RC) end
	for i, o in ipairs(rec.objs) do eos[i] = newEdgeObject(o, R, -1, notes, form) end
	for id in pairs(gone) do removeIds[#removeIds + 1] = id end
	table.sort(removeIds)
	local nodeCfg = {}
	for _, n in ipairs({ info.n0, info.n1 }) do
		if A.comp(n, "BASE_NODE_CONFIG") then nodeCfg[#nodeCfg + 1] = n end
	end
	withStreet(sp, function(ssp)
		req(ssp, "edgesToRemove", { edge })
		req(ssp, "edgesToAdd", { e })
		req(ssp, "edgeObjectsToAdd", eos)
		if #removeIds > 0 then req(ssp, "edgeObjectsToRemove", removeIds) end
		if #nodeCfg > 0 then opt(notes, ssp, "nodeConfigsToRemove", nodeCfg) end
		-- the stop build removes the junction configurations of the segment's two ends and adds new ones; a replay that
		-- does not add them leaves the junctions short (finding from MPFever (MIT)). The record's own placeholder for the
		-- segment is -1, the same as here.
		if #cfgs > 0 then opt(notes, ssp, "nodeConfigsToAdd", cfgs) end
	end)
	return commandArgs(sp, newContext(f.ctx, CTX_PLAIN, R), f.ie, f.pi, f.dust)
end

-- ================================================================== validation (nothing from the wire is trusted)

local function okText(v, max) return type(v) == "string" and #v <= (max or LIMIT.text) and not v:find("%c") end
local function okPos(p) return type(p) == "table" and isNum(p.x) and isNum(p.y) and isNum(p.z) end
local function okRef(r, kind)
	if type(r) ~= "table" or r.__ref ~= kind then return false end
	if r.key ~= nil and not okText(r.key, 120) then return false end
	if kind == "node" then return okPos(r.p) end
	if kind == "edge" then
		for _, k in ipairs({ "ty", "ti", "rt", "st" }) do
			if r[k] ~= nil and not isNum(r[k]) then return false end
		end
		return okPos(r.p0) and okPos(r.p1) and (r.t0 == nil or okPos(r.t0)) and (r.t1 == nil or okPos(r.t1))
	end
	if kind == "con" then return okText(r.file) and okPos(r.p) end
	if kind == "obj" then return okRef(r.edge, "edge") and isNum(r.i) and r.i >= 1 end
	return false
end
local function okId(v, kind) return (isNum(v) and v < 0) or okRef(v, kind) end
local function okList(l, max) return type(l) == "table" and #l <= max end
-- an owner: nobody or a number from the savegame, the acting player, or another entity by key
local function okOwner(v) return v == nil or v == "player" or isNum(v) or (type(v) == "table" and v.__ref == "owner" and okText(v.key)) end

local function plainDepth(v, depth, budget)
	local t = type(v)
	if t == "number" then return isNum(v) end
	if t == "string" or t == "boolean" then return true end
	if t ~= "table" or depth > 8 then return false end
	for k, x in pairs(v) do
		budget.n = budget.n + 1
		if budget.n > 20000 or (type(k) ~= "number" and type(k) ~= "string") or not plainDepth(x, depth + 1, budget) then return false end
	end
	return true
end

-- Returns the record with every list present, or nil and why not.
local function normalise(rec)
	if type(rec) ~= "table" then return nil, "no build record" end
	if rec.v ~= M.VERSION then return nil, "build record version " .. tostring(rec.v) .. " (this mod reads " .. M.VERSION .. ")" end
	for _, k in ipairs({ "cons", "rem", "old2new", "nodes", "segs", "rmNodes", "rmSegs", "objs", "rmObjs", "ncfg", "rmNcfg" }) do
		if rec[k] == nil then rec[k] = {} end
		if type(rec[k]) ~= "table" then return nil, "build record: " .. k .. " is not a list" end
	end
	if not (okList(rec.cons, LIMIT.cons) and okList(rec.rem, LIMIT.rem) and okList(rec.nodes, LIMIT.nodes) and okList(rec.segs, LIMIT.segs)
		and okList(rec.rmNodes, LIMIT.rem) and okList(rec.rmSegs, LIMIT.rem) and okList(rec.objs, LIMIT.objs) and okList(rec.rmObjs, LIMIT.objs)
		and okList(rec.ncfg, LIMIT.ncfg) and okList(rec.rmNcfg, LIMIT.ncfg) and okList(rec.old2new, LIMIT.rem)) then
		return nil, "build record too large"
	end
	local budget = { n = 0 }
	for i, c in ipairs(rec.cons) do
		if type(c) ~= "table" or not okText(c.file) or not c.file:find("%.con$") then return nil, "construction " .. i .. ": bad file name" end
		if c.name ~= nil and not okText(c.name) then return nil, "construction " .. i .. ": bad name" end
		if type(c.transf) ~= "table" or #c.transf ~= 16 then return nil, "construction " .. i .. ": bad transform" end
		for j = 1, 16 do if not isNum(c.transf[j]) then return nil, "construction " .. i .. ": bad transform" end end
		if c.params ~= nil and not plainDepth(c.params, 0, budget) then return nil, "construction " .. i .. ": bad parameters" end
		if not okOwner(c.owner) then return nil, "construction " .. i .. ": bad owner" end
	end
	for i, r in ipairs(rec.rem) do if not okRef(r, "con") then return nil, "removed construction " .. i .. ": bad reference" end end
	for i, p in ipairs(rec.old2new) do
		if type(p) ~= "table" or not isNum(p[1]) or p[1] < 1 or p[1] > #rec.rem or not plainDepth(p[2], 0, budget) then return nil, "old2new " .. i .. ": bad entry" end
	end
	for i, n in ipairs(rec.nodes) do
		if type(n) ~= "table" or not okId(n.id, "node") or not okPos(n.pos) then return nil, "node " .. i .. ": bad entry" end
	end
	for i, s in ipairs(rec.segs) do
		if type(s) ~= "table" or not okId(s.id, "edge") or not okId(s.n0, "node") or not okId(s.n1, "node") or type(s.c) ~= "table" then return nil, "segment " .. i .. ": bad entry" end
		for _, k in ipairs({ "roadTemplate", "roadStyle" }) do
			if s.c[k] ~= nil and not okText(s.c[k]) then return nil, "segment " .. i .. ": bad " .. k end
		end
		for _, k in ipairs({ "type", "typeIndex", "roadType", "distance" }) do
			if s.c[k] ~= nil and not isNum(s.c[k]) then return nil, "segment " .. i .. ": bad " .. k end
		end
		for _, k in ipairs({ "t0", "t1", "p0", "p1" }) do
			if s.c[k] ~= nil and not okPos(s.c[k]) then return nil, "segment " .. i .. ": bad " .. k end
		end
		if s.c.deco ~= nil and not plainDepth(s.c.deco, 0, budget) then return nil, "segment " .. i .. ": bad decorations" end
		if s.lanes ~= nil then
			if type(s.lanes) ~= "table" or #s.lanes > 16 or not plainDepth(s.lanes, 0, budget) then return nil, "segment " .. i .. ": bad lanes" end
		end
		if type(s.objects) ~= "table" or #s.objects > 64 then return nil, "segment " .. i .. ": bad objects" end
		for _, o in ipairs(s.objects) do
			if type(o) ~= "table" or not okId(o[1], "obj") or (o[2] ~= nil and not isNum(o[2])) then return nil, "segment " .. i .. ": bad object" end
		end
		if s.type ~= nil and not isNum(s.type) then return nil, "segment " .. i .. ": bad type" end
		if s.track ~= nil and (type(s.track) ~= "table" or not isNum(s.track.tt) or (s.track.cat ~= nil and not isNum(s.track.cat))) then return nil, "segment " .. i .. ": bad track" end
		if s.prec ~= nil and (type(s.prec) ~= "table" or (s.prec[1] ~= nil and not isNum(s.prec[1])) or (s.prec[2] ~= nil and not isNum(s.prec[2]))) then
			return nil, "segment " .. i .. ": bad precedence"
		end
		if not okOwner(s.owner) then return nil, "segment " .. i .. ": bad owner" end
	end
	for i, r in ipairs(rec.rmNodes) do if not okRef(r, "node") then return nil, "removed node " .. i .. ": bad reference" end end
	for i, r in ipairs(rec.rmSegs) do if not okRef(r, "edge") then return nil, "removed segment " .. i .. ": bad reference" end end
	for i, o in ipairs(rec.objs) do
		if type(o) ~= "table" or not okId(o.edge, "edge") or not okText(o.model) or not isNum(o.param) then return nil, "edge object " .. i .. ": bad entry" end
		if o.params ~= nil and not plainDepth(o.params, 0, budget) then return nil, "edge object " .. i .. ": bad parameters" end
		if o.kind ~= nil and not isNum(o.kind) then return nil, "edge object " .. i .. ": bad kind" end
		if not okOwner(o.owner) then return nil, "edge object " .. i .. ": bad owner" end
	end
	for i, r in ipairs(rec.rmObjs) do if not okRef(r, "obj") then return nil, "removed edge object " .. i .. ": bad reference" end end
	for i, n in ipairs(rec.ncfg) do
		if type(n) ~= "table" or not okId(n.node, "node") then return nil, "node configuration " .. i .. ": bad entry" end
		if type(n.lc) ~= "table" or #n.lc > 256 or type(n.cw) ~= "table" or #n.cw > 64 then return nil, "node configuration " .. i .. ": bad lists" end
		for _, l in ipairs(n.lc) do
			if type(l) ~= "table" or not okId(l.s0, "edge") or not okId(l.s1, "edge") or not isNum(l.l0) or not isNum(l.l1) then return nil, "node configuration " .. i .. ": bad lane connection" end
		end
		for _, x in ipairs(n.cw) do if not okId(x, "edge") then return nil, "node configuration " .. i .. ": bad crosswalk" end end
	end
	for i, r in ipairs(rec.rmNcfg) do if not okRef(r, "node") then return nil, "removed node configuration " .. i .. ": bad reference" end end
	return rec
end

-- ================================================================== the command makeWorldBuildProposalCmd

local function logNotes(notes, what)
	if #notes > 0 then log(what .. ": members not set: " .. table.concat(notes, "; "):sub(1, 300)) end
end

local function encodeBuildUnsafe(name, args, ctx)
	local cap = newCapture(ctx, {})
	local ok, rec = pcall(captureRecord, args[1], cap)
	if not ok then return nil, tostring(rec) end
	local plan, why = classify(rec)
	if not plan then return nil, why end
	if rec.terrain then log("a terrain edit of " .. rec.terrain.w .. "x" .. rec.terrain.h .. " cells comes with this build: the cells are not copied") end
	local ctxRec = captureContext(args[2])
	local body = { fn = name, args = { n = 5, rec, ctxRec or (rec.src == "native" and { native = true } or false),
		args[3] == true, args[4] == true, args[5] == true } }
	local okS, text = pcall(C.serialize, body)
	if not okS then return nil, "the build record is not plain data: " .. tostring(text) end
	if #text > LIMIT.bytes then return nil, "the build is too large to send (" .. #text .. " bytes)" end
	return body
end

-- encode and decode never raise: the interface half calls encode from inside a game window's command, and the
-- simulation half calls decode between two actions of one step, where an error would drop the rest of the step.
local function encodeBuild(name, args, ctx)
	local ok, body, why = pcall(encodeBuildUnsafe, name, args, ctx)
	if ok then return body, why end
	return nil, "the build could not be read: " .. tostring(body)
end

-- The same capture for a proposal that does not come through a command factory: the native tools' builds, as the
-- native module or the tool events deliver them. flags = { ctx = Context or nil, ignoreErrors, playerInitiated, doDust,
-- result = created entities }. Returns a body for the action line, or nil and why not.
function M.capture(proposal, flags, ctx)
	flags = flags or {}
	ctx = ctx or { plain = C.plain, keyOf = function(e) return "s:" .. tostring(e) end }
	local cap = newCapture(ctx, { result = flags.result })
	local ok, rec = pcall(captureRecord, proposal, cap)
	if not ok then return nil, tostring(rec) end
	return M.bodyOf(rec, flags)
end

-- A captured record as the body of an action line: the body, nil and its wire text, or nil and why not.
function M.bodyOf(rec, flags)
	flags = flags or {}
	local plan, why = classify(rec)
	if not plan then return nil, why end
	local body = { fn = "makeWorldBuildProposalCmd", args = { n = 5, rec, captureContext(flags.ctx) or (rec.src == "native" and { native = true } or false),
		flags.ignoreErrors == true, flags.playerInitiated ~= false, flags.doDust == true } }
	local okS, text = pcall(C.serialize, body)
	if not okS then return nil, "the build record is not plain data: " .. tostring(text) end
	if #text > LIMIT.bytes then return nil, "the build is too large to send (" .. #text .. " bytes)" end
	return body, nil, text
end

-- ---- node configurations read from the world
-- Some tools (the lane / crosswalk / traffic-light tools: UI::LaneModifier::Apply) send no preview event to the interface, so
-- the proposal they apply cannot be captured before the build. What they change is node configurations only; the world says
-- what it holds before and after, and the difference is the record.
local function nodeConfigSig(nc)
	local lcs, cw = {}, {}
	for _, l in ipairs(A.list(nc, "laneConnections")) do
		lcs[#lcs + 1] = table.concat({ tostring(get(l, "segment0")), tostring(get(l, "lane0")), tostring(get(l, "segment1")), tostring(get(l, "lane1")),
			tostring(get(l, "withRoad")), tostring(get(l, "withTram")) }, ",")
	end
	for _, x in ipairs(A.list(nc, "crosswalks")) do cw[#cw + 1] = tostring(x) end
	table.sort(lcs)
	table.sort(cw)
	return table.concat(lcs, ";") .. "|" .. table.concat(cw, ",") .. "|" .. tostring(get(nc, "trafficLightPreference")) .. "|"
		.. tostring(get(nc, "doubleSlipSwitch")) .. "|" .. tostring(get(nc, "userModifiedLaneConnections"))
end

-- node entity -> signature of its configuration, for every node of the street graph; or nil and why not
function M.nodeConfigSnapshot()
	local ok, map = pcall(function() return api.engine.system.streetSystem.getNode2SegmentMap() end)
	if not ok or map == nil then return nil, "the street graph cannot be listed" end
	local snap = {}
	pcall(function()
		for node in pairs(map) do
			local nc = A.comp(node, "BASE_NODE_CONFIG")
			if nc then snap[node] = nodeConfigSig(nc) end
		end
	end)
	return snap
end

-- The build that turned `before` (a snapshot) into the world as it is now: the configurations of the nodes whose signature
-- changed. Returns the body, nil and its wire text; or nil and why not (nothing changed included).
function M.nodeConfigDelta(before, ctx)
	local after, why = M.nodeConfigSnapshot()
	if not after then return nil, why end
	ctx = ctx or { plain = C.plain, keyOf = function(e) return "s:" .. tostring(e) end }
	local cap = newCapture(ctx, {})
	local rec = emptyRecord("native")
	local nodes = {}
	for node, sig in pairs(after) do
		if before[node] ~= sig then nodes[#nodes + 1] = node end
	end
	table.sort(nodes)
	for _, node in ipairs(nodes) do
		local ok, cfg = pcall(readNodeConfig, { entity = node, comp = A.comp(node, "BASE_NODE_CONFIG") }, cap)
		if not ok then return nil, "node configuration of " .. tostring(node) .. ": " .. tostring(cfg) end
		rec.ncfg[#rec.ncfg + 1] = cfg
	end
	if #rec.ncfg == 0 then return nil, "no node configuration changed" end
	return M.bodyOf(rec, { playerInitiated = true })
end

-- The capture of one applied tool event (builder.proposalApply) for the bridge's strict mode: the native module has held
-- the tool's command, so the world is still as the tool saw it and the references resolve. `ctx` names entities the way the
-- bridge keys them (see M.capture). Returns the body, nil and its serialised text (the wire form), or nil and why not. Never
-- raises. The logging of the same events stays in M.onToolEvent.
function M.captureToolBuild(id, name, param, ctx)
	if name ~= "builder.proposalApply" and name ~= "builder.proposalCreate" then return nil, "not a build event: " .. tostring(name) end
	if type(param) ~= "table" then return nil, "the event carries no data" end
	local okP, proposal = pcall(function() return param[1] or param.proposal end)
	if not okP or proposal == nil then return nil, "the event carries no proposal" end
	local ok, body, why, text = pcall(M.capture, proposal, { playerInitiated = true }, ctx)
	if not ok then return nil, "the build could not be read: " .. tostring(body) end
	return body, why, text
end

-- The plan (street, combined, bulldoze, construction, ...) a captured build body falls under; nil when it cannot be told.
-- The bridge's strict mode cancels and replays only the plans that real games proved (see STRICT_CANCEL_PLANS there).
function M.planOf(body)
	local ok, plan = pcall(function()
		local rec = normalise(body.args[1])
		return rec and classify(rec) or nil
	end)
	return ok and plan or nil
end

-- Remote constructions (a build that ADDS constructions: depots, stations, assets, constructions with a street). Real session
-- 2026-10-12: the host's game died with a UI assertion `it != components.end()` in UI::CGameUi 8 s after it replayed the guest's depot
-- build (cause unproven). They are replicated as before (hold-and-release) and the player gets an informational chat line. With
-- FEVERLINK3_REMOTE_CONSTRUCTIONS=0 (developers) they are refused: not shipped, not replayed. The originator can only cancel such a
-- build when the native module can fire the tool's completion callback (a cancel without it leaves the tool dead: real session).
local REPLAY_IGNORE_ERRORS = false
pcall(function() REPLAY_IGNORE_ERRORS = os.getenv("FEVERLINK3_REPLAY_IGNORE_ERRORS") == "1" end)
M.REPLAY_IGNORE_ERRORS = REPLAY_IGNORE_ERRORS
local BLOCK_CONSTRUCTIONS = false
pcall(function() BLOCK_CONSTRUCTIONS = os.getenv("FEVERLINK3_REMOTE_CONSTRUCTIONS") == "0" end)
M.CONSTRUCTIONS_BLOCKED_TEXT = "station/depot builds are not supported yet in this test build"
M.CONSTRUCTIONS_INFO_TEXT = "station/depot builds are experimental in this test build"

local function constructionsBlocked(rec)
	return BLOCK_CONSTRUCTIONS and type(rec) == "table" and type(rec.cons) == "table" and #rec.cons > 0
end

-- true when this build record (a body of makeWorldBuildProposalCmd, as captured) adds constructions and the block is on
function M.constructionsBlocked(body)
	local ok, blocked = pcall(function()
		local rec = normalise(body.args[1])
		return constructionsBlocked(rec)
	end)
	return ok and blocked == true
end

-- What a build record holds, for the log: the plan, the counts, the constructions it adds ("file@x,y") and removes ("file@x,y[key]"),
-- each list cut at 4. Never raises; nil when the body is not a build record.
-- the stops a record puts on a street: every edge object but a signal (kind 2), as in PLAN.stop
local function stopCount(rec)
	local n = 0
	for _, o in ipairs(type(rec.objs) == "table" and rec.objs or {}) do
		if type(o) == "table" and o.kind ~= 2 then n = n + 1 end
	end
	return n
end

function M.recSummary(body)
	local ok, s = pcall(function()
		local rec = normalise(body.args[1])
		if type(rec) ~= "table" then return nil end
		local function p3(x, y) return string.format("%.1f,%.1f", x or 0, y or 0) end
		local add, rem = {}, {}
		for i, c in ipairs(rec.cons or {}) do
			if i > 4 then add[#add + 1] = "..." break end
			local t = c.transf or {}
			add[#add + 1] = tostring(c.file) .. "@" .. p3(t[13], t[14])
		end
		for i, r in ipairs(rec.rem or {}) do
			if i > 4 then rem[#rem + 1] = "..." break end
			local p = r.p or {}
			rem[#rem + 1] = tostring(r.file) .. "@" .. p3(p.x, p.y) .. (r.key and ("[" .. tostring(r.key) .. "]") or "")
		end
		local plan = M.planOf(body)
		return { plan = plan or "?", cons = #(rec.cons or {}), rem = #(rec.rem or {}), nodes = #(rec.nodes or {}), segs = #(rec.segs or {}),
			rmSegs = #(rec.rmSegs or {}), objs = #(rec.objs or {}), stops = stopCount(rec), add = add, remove = rem }
	end)
	return ok and s or nil
end

-- true when the record adds or removes constructions (a station that takes town buildings away counts)
function M.addsOrRemovesConstructions(body)
	local ok, yes = pcall(function()
		local rec = normalise(body.args[1])
		return type(rec) == "table" and ((type(rec.cons) == "table" and #rec.cons > 0) or (type(rec.rem) == "table" and #rec.rem > 0))
	end)
	return ok and yes == true
end

-- true when the record adds constructions, whatever the block says
function M.addsConstructions(body)
	local ok, adds = pcall(function()
		local rec = normalise(body.args[1])
		return type(rec) == "table" and type(rec.cons) == "table" and #rec.cons > 0
	end)
	return ok and adds == true
end

-- What was passed to the engine's command factory, for the log when it throws: the lengths of the proposal's lists (constructions and
-- street parts to add and to remove). Never raises.
local function proposalCounts(proposal)
	local parts = {}
	pcall(function()
		local function n(obj, key)
			local ok, l = pcall(function() return obj[key] end)
			return ok and l ~= nil and #l or nil
		end
		for _, k in ipairs({ "constructionsToAdd", "constructionsToRemove" }) do
			local c = n(proposal, k)
			if c then parts[#parts + 1] = k .. "=" .. c end
		end
		local ok, st = pcall(function() return proposal.streetProposal end)
		if ok and st ~= nil then
			for _, k in ipairs({ "nodesToAdd", "edgesToAdd", "nodesToRemove", "edgesToRemove", "edgeObjectsToAdd", "edgeObjectsToRemove", "nodeConfigsToAdd" }) do
				local c = n(st, k)
				if c then parts[#parts + 1] = k .. "=" .. c end
			end
		end
	end)
	return table.concat(parts, " ")
end

local function decodeBuildUnsafe(body, ctx)
	local a = body.args
	local rec, why = normalise(a[1])
	if not rec then return nil, why end
	if constructionsBlocked(rec) then return nil, M.CONSTRUCTIONS_BLOCKED_TEXT end
	local plan, whyNot = classify(rec)
	if not plan then return nil, whyNot end
	local R = newResolver(ctx)
	local f = { ctx = a[2] or nil, ie = a[3] == true, pi = a[4] == true, dust = a[5] == true, src = rec.src }
	-- Developer switch (default off): a build that adds constructions AND removes some (a station that takes town buildings away) is
	-- replayed with ignoreErrors = true. Real session 2026-10-12: such builds created nothing on the receiving game; to A/B whether the
	-- engine's non-critical errors (a collision with a building this game has and the originator did not) are the reason.
	if REPLAY_IGNORE_ERRORS and type(rec.cons) == "table" and #rec.cons > 0 and type(rec.rem) == "table" and #rec.rem > 0 then
		f.ie = true
	end
	-- The forms of the plan are tried in order and the engine's factory is the judge, here, before anything is applied: a
	-- proposal it rejects in every form refuses the action on every game alike.
	local args, used, lastErr
	local tkey = ctx and ctx.traceKey
	local trace = C.tracer("build")
	for i, form in ipairs(FORMS[plan] or { {} }) do
		local notes = {}
		local t0 = os.clock()
		local ok, built = pcall(PLAN[plan], rec, R, f, notes, form)
		if tkey then trace("build.plan", string.format("%s plan=%s form=%d %s ms=%.1f%s", tkey, plan, i, ok and "built" or "failed", (os.clock() - t0) * 1000, ok and "" or (" error(" .. type(built) .. ")=" .. tostring(built))) ) end
		if not ok then
			lastErr = tostring(built)
			-- what cannot be found is not found in any form
			if lastErr:find("^unresolved ") or lastErr:find("^ambiguous ") then return nil, lastErr end
		else
			local tF = os.clock()
			local okF, errF = pcall(function() return api.cmd.makeWorldBuildProposalCmd(unpack(built, 1, built.n)) end)
			if tkey then
				trace("build.factory", string.format("%s plan=%s form=%d %s ms=%.1f ie=%s pi=%s dust=%s counts[%s]%s", tkey, plan, i, okF and "ok" or "THREW", (os.clock() - tF) * 1000,
					tostring(built[3]), tostring(built[4]), tostring(built[5]), proposalCounts(built[1]),
					okF and "" or (" error(" .. type(errF) .. ")=" .. tostring(errF))))
			end
			if okF then
				args, used = built, i
				logNotes(notes, "build " .. plan)
				break
			end
			local first = tostring(errF):gsub("\nstack traceback:.*", ""):sub(1, 160)
			lastErr = "the engine's command factory refused the " .. plan .. " build: " .. first
			-- say what it was given: the next real run names the part that makes it throw
			log(string.format("factory threw for %s (form %d of %d): %s; passed [%s]; record cons=%d rem=%d nodes=%d+%d segs=%d+%d objs=%d+%d; first command ie=%s pi=%s",
				plan, i, #(FORMS[plan] or { {} }), first, proposalCounts(built[1]), #rec.cons, #rec.rem, #rec.nodes, #rec.rmNodes, #rec.segs, #rec.rmSegs,
				#rec.objs, #rec.rmObjs, tostring(built[3]), tostring(built[4])))
		end
	end
	if not args then return nil, lastErr or "no way to build it" end
	args.plan, args.form = plan, used   -- for the engine-verdict lines of the bridge (which form of which plan was judged)
	log("build " .. plan .. (used > 1 and (" (form " .. used .. ")") or "") .. " (" .. tostring(rec.src) .. "): " .. #rec.cons .. "+" .. #rec.rem .. " constructions, "
		.. #rec.nodes .. "+" .. #rec.rmNodes .. " nodes, " .. #rec.segs .. "+" .. #rec.rmSegs .. " segments, " .. #rec.objs .. "+" .. #rec.rmObjs .. " objects"
		.. (args.chain and (", " .. #args.chain + 1 .. " commands") or ""))
	return args
end

local function decodeBuild(body, ctx)
	local ok, args, why = pcall(decodeBuildUnsafe, body, ctx)
	if ok then return args, why end
	return nil, "the build record could not be read: " .. tostring(args)
end

-- Permissions: a proposal that only takes things away is bulldozing, anything else is building. Counts only. Segments
-- the engine re-adds when it merges what a bulldozing leaves are fewer than the ones removed; an upgrade replaces as
-- many as it removes.
local function categoryFor(args)
	local ok, c = pcall(function()
		local p = args and args[1]
		local flavor = flavorOf(p)
		if not flavor then return "build" end
		local names = NAMES[flavor]
		local st = get(p, flavor == "simple" and "streetProposal" or "proposal")
		local consKey, remKey = "constructionsToAdd", "constructionsToRemove"
		if flavor == "native" then consKey, remKey = "toAdd", "toRemove" end
		local adds, removes = #A.list(p, consKey), #A.list(p, remKey)
		local segAdd, segRm, objRm = 0, 0, 0
		if st ~= nil then
			adds = adds + #A.list(st, names.nodes) + #A.list(st, names.objs)
			segAdd, segRm, objRm = #A.list(st, names.segs), #A.list(st, names.rmSegs), #A.list(st, names.rmObjs)
		end
		if adds == 0 and (removes > 0 or segAdd < segRm or objRm > 0) then return "bulldoze" end
		return "build"
	end)
	return ok and c or "build"
end

-- Keys for what the build created, so that later actions (a vehicle bought at a depot, a line through a station) can
-- name it: <key>#c<i> for the i-th construction of the record, #c<i>/d<j> for its depots, /s<j> its stations,
-- /g<j> their station groups. Matched by file and place, not by id, so a town building that grew in the same step
-- is not taken for one of ours. Road-side stops have no building to be named by: <key>#o<n> is the n-th stop the engine
-- made and <key>#o<n>/g its station group (see below). Returns the map and whether everything expected was found.

local function bindsComp(body)
	local rec = body.args and body.args[1]
	if type(rec) ~= "table" then return nil end
	local comps = {}
	if type(rec.cons) == "table" and #rec.cons > 0 then comps[#comps + 1] = "CONSTRUCTION" end
	if stopCount(rec) > 0 then
		comps[#comps + 1] = "EDGE_OBJECT"
		comps[#comps + 1] = "STATION_GROUP"
		comps[#comps + 1] = "GROUP_STATION"   -- the stations the groups list (new ones, also in a group that was there before)
	end
	return comps
end

local function bindCreated(body, key, made)
	local rec = body.args and body.args[1]
	local map, complete = {}, true
	if type(rec) ~= "table" or type(rec.cons) ~= "table" then return map, true end
	local used = {}
	for i, c in ipairs(rec.cons) do
		local p = { x = c.transf[13], y = c.transf[14], z = c.transf[15] }
		local pick1
		local bestD
		for _, e in ipairs(made) do
			if not used[e] then
				local info = A.conInfo(e)
				if info and info.file == c.file then
					local d = d3(info.p, p)
					if d <= TOL_CON and (bestD == nil or d < bestD) then pick1, bestD = e, d end
				end
			end
		end
		if pick1 then
			used[pick1] = true
			local base = key .. "#c" .. i
			map[base] = pick1
			local depots, stations = A.conParts(pick1)
			for j, d in ipairs(depots) do map[base .. "/d" .. j] = d end
			for j, s in ipairs(stations) do
				map[base .. "/s" .. j] = s
				local g = A.stationGroup(s)
				if g then map[base .. "/g" .. j] = g end
			end
		else
			complete = false
		end
	end
	-- A stop on a road is an edge object that is a station. Nothing in the record or the world says which of two games'
	-- entities is the same stop except the order the engine made them in: the same proposal on the same world makes the
	-- same stops in the same order on every game (ids rise with creation), whatever number each game gives them. So the
	-- n-th new stop is #o<n>, and its station group #o<n>/g when the stop made one (a stop beside a station joins that
	-- station's group, which keeps its own key). All or none: a count that is short may mean the engine is not done.
	local want = stopCount(rec)
	if want > 0 then
		local stops, fresh, have = {}, {}, {}
		for _, e in ipairs(made) do
			if A.isStop(e) then
				stops[#stops + 1] = e
				have[e] = true
			elseif A.comp(e, "STATION_GROUP") ~= nil then
				fresh[e] = true
			end
		end
		-- Sweep run 20261013-053645 (B1, host replay of a bus stop): the new STATION_GROUP was listed but no new EDGE_OBJECT, so the stop
		-- itself may not be enumerable by component. The new group names its stations: a stop is found there too.
		for g in pairs(fresh) do
			for _, st in ipairs(A.list(A.comp(g, "STATION_GROUP"), "stations")) do
				if type(st) == "number" and not have[st] and A.isStop(st) then
					have[st] = true
					stops[#stops + 1] = st
				end
			end
		end
		table.sort(stops)
		if #stops >= want then
			for n = 1, want do
				local s = stops[n]
				map[key .. "#o" .. n] = s
				local g = A.stationGroup(s)
				if g and fresh[g] then map[key .. "#o" .. n .. "/g"] = g end
			end
		else
			complete = false
		end
	end
	return map, complete
end

M.commands.makeWorldBuildProposalCmd = {
	category = "build",
	categoryFor = categoryFor,
	encode = encodeBuild,
	decode = decodeBuild,
	bindsComp = bindsComp,
	bind = bindCreated,
}

-- A terrain edit rides in the proposal's terrain grid; a script sees the grid's size but not its cells, so it cannot be
-- copied from here. Stays native; the next checkpoint finds the difference.
M.commands.makeWorldReplaceTerrainCmd = {
	category = "build",
	encode = function() return nil, "terrain cells cannot be read or written from a script" end,
}

-- ================================================================== tool events (diagnostics for the first game run)

local seenEvents = {}
local probes = 0
local previews = {}

-- A preview of a tool's build, captured, sent through the wire form and rebuilt as a peer would (the engine's factory is
-- called, nothing is sent): says whether it rebuilds and how long that took. A few per tool and plan; the world is still
-- as the tool saw it, so references resolve.
local function previewRebuild(id, proposal, say)
	local started = os.clock()
	local body, why = M.capture(proposal, {})
	if not body then
		local key = tostring(id) .. " refused " .. tostring(why)
		if not previews[key] then
			previews[key] = 1
			say("tool preview " .. tostring(id) .. ": not capturable: " .. tostring(why))
		end
		return
	end
	local plan = classify(body.args[1])
	local key = tostring(id) .. " " .. tostring(plan)
	previews[key] = (previews[key] or 0) + 1
	if previews[key] > 2 then return end
	local wire = C.parse(C.serialize(body))
	local ctx = { player = A.player(), resolve = function(k) local n = tostring(k):match("^s:(%d+)$"); return n and tonumber(n) or nil end }
	local args, err = decodeBuild(wire, ctx)
	say("tool preview " .. tostring(id) .. ": " .. tostring(plan) .. (args and " rebuilds" or (" does NOT rebuild: " .. tostring(err)))
		.. string.format(" (%.1f ms, %d bytes)", (os.clock() - started) * 1000, #C.serialize(body)))
end

-- Called by the bridge for the engine's tool events (id = the tool: streetBuilder, trackBuilder, constructionBuilder,
-- streetTerminalBuilder, bulldozer ..., name = builder.proposalCreate / builder.proposalApply, param = { proposal, data,
-- result }). Logs each kind once; for the first previews of each tool whether they rebuild; for the first few applied
-- builds what a capture of them is, so that the adapter can be checked against real proposals.
function M.onToolEvent(id, name, param, say)
	say = say or log
	local key = tostring(id) .. "/" .. tostring(name)
	if not seenEvents[key] then
		seenEvents[key] = true
		say("tool event " .. key)
	end
	if type(param) ~= "table" then return end
	if name == "builder.proposalCreate" then
		local n = (previews[tostring(id)] or 0) + 1
		previews[tostring(id)] = n
		if n <= 8 then previewRebuild(id, param[1] or param.proposal, say) end
		return
	end
	if name ~= "builder.proposalApply" or probes >= 6 then return end
	probes = probes + 1
	local proposal, result = param[1] or param.proposal, param[3] or param.result
	local body, why = M.capture(proposal, { result = result })
	if body then
		local rec = body.args[1]
		say("tool build " .. key .. ": " .. (classify(rec) or "?") .. ", " .. #rec.cons .. "+" .. #rec.rem .. " constructions, " .. #rec.nodes .. "+" .. #rec.rmNodes
			.. " nodes, " .. #rec.segs .. "+" .. #rec.rmSegs .. " segments, " .. #rec.objs .. "+" .. #rec.rmObjs .. " objects, "
			.. #C.serialize(body) .. " bytes")
	else
		say("tool build " .. key .. " not capturable: " .. tostring(why))
	end
	-- the members of the first segment, node and edge object of the proposal, to compare with the ones this module reads
	pcall(function()
		local st = get(proposal, "proposal") or get(proposal, "streetProposal") or {}
		local seg = A.list(st, "addedSegments")[1] or A.list(st, "edgesToAdd")[1]
		local c = seg and get(seg, "comp")
		local m = c and A.members(c)
		if m then say("segment members: " .. table.concat(m, ",")) end
		-- the SegmentAndEntity itself and its trackEdge (track type, catenary): an unknown member of a track shows here
		if seg then
			say("segment entity members: " .. table.concat(A.members(seg) or {}, ",") .. " type " .. tostring(get(seg, "type")))
			local te = get(seg, "trackEdge")
			if te ~= nil then
				say("trackEdge: members " .. table.concat(A.members(te) or {}, ",") .. " value " .. C.serialize(A.dump(te)):sub(1, 300))
			end
		end
		local node = A.list(st, "addedNodes")[1] or A.list(st, "nodesToAdd")[1]
		m = node and A.members(get(node, "comp"))
		if m then say("node members: " .. table.concat(m, ",")) end
		local eo = A.list(st, "edgeObjectsToAdd")[1]
		if eo then
			say("edge object: members " .. table.concat(A.members(eo) or {}, ",") .. " value " .. C.serialize(A.dump(eo)):sub(1, 300))
		end
	end)
end

M.classify = classify
M.normalise = normalise
M.plans = PLAN
M.resolver = newResolver
M.CTX = { native = CTX_NATIVE, terrain = CTX_TERRAIN, plain = CTX_PLAIN }

return M
