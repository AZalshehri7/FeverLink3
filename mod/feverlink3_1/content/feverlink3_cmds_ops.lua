-- FeverLink3 command schemas: operations (vehicles, lines, depots, finance, towns and industries).
--
-- Returns { commands = { <api.cmd function name> = spec }, events = { <id> = { <name> = handler } } }; the bridge
-- registers both (feverlink3_lockstep.lua documents the spec fields, the encode/decode hooks and the event registry).
-- docs/COVERAGE_OPS.md lists every command maker of the engine, what becomes of it and why.
--
-- Every command here goes through the encode/decode hooks, for two reasons the generic path cannot meet: the game
-- builds its arguments as engine objects (a vehicle configuration, a line, a colour) and most arguments are entities
-- that must be checked on the receiving game before the command is rebuilt there.
--
-- Argument shapes are read from the call sites in the game's own scripts (base/content/*.zip). The handling of engine
-- objects is a finding from MPFever (MIT), which ran it in the game: they are userdata that list their members under
-- __members, vectors read as x/y/z, lists by # and [i]; a new object comes from api.type.<Name>.new() and is filled
-- member by member, and some properties hand out copies. Everything that touches the engine sits in the adapters (A):
-- one place to change when a build differs, one place for the tests to mock.
--
-- Two rules keep a wrong guess from becoming a silent divergence. A command is rebuilt on the sending game before it
-- is shipped (a dry run), and an engine object must come back from that rebuild equal to what was read; if not, the
-- command stays native and the log says where. A reference to an entity is checked on every game that applies it
-- (it exists, it is of the expected kind): an action that fails the check is refused, never applied to whatever
-- happens to share the id.

local M = { commands = {}, events = {} }

local function try(f, ...)
	local ok, v = pcall(f, ...)
	if ok then return v end
	return nil
end

-- What a game run needs to see to judge the guesses below (an id that differs from its model's name, a member the
-- engine would not take) goes to mod.log, once per message. Silent where the log module is not there.
local log = false
do
	local ok, C = pcall(ug_require, "feverlink3_common.lua")
	if not ok then ok, C = pcall(ug_require, "feverlink3_1::/feverlink3_common.lua") end
	if ok and type(C) == "table" and C.logger then log = C.logger("ops") end
end
local noted = {}
local function note(msg)
	if noted[msg] or not log then return end
	noted[msg] = true
	log(msg)
end

-- ================================================================== adapters

local A = {}

-- the component each kind of entity carries (names as the game's own scripts write them)
local COMPONENT = {
	vehicle = "TRANSPORT_VEHICLE", line = "LINE", depot = "VEHICLE_DEPOT", stationGroup = "STATION_GROUP",
	station = "STATION", construction = "CONSTRUCTION", town = "TOWN",
}
-- the order a generic entity is classified in
local KIND_ORDER = { "vehicle", "line", "stationGroup", "depot", "station", "town", "construction" }

-- how the owner of a depot or a station is found: the lookup the game's own scripts use for a station
-- (transfer_ownership_util.tl), then the two specific ones MPFever used (MIT)
local OWNER_LOOKUPS = { "getConstructionEntityForSubconstruction", "getConstructionEntityForDepot", "getConstructionEntityForStation" }

function A.player()
	return try(function() return api.engine.util.getPlayer() end)
end

function A.exists(e)
	if type(e) ~= "number" or e < 0 then return false end
	return try(function() return api.engine.entityExists(e) end) == true
end

function A.component(e, name)
	return try(function() return api.engine.getComponent(e, api.type.ComponentType[name]) end)
end

-- true when `e` exists and, for a kind that has a component, carries it
function A.isKind(e, kind)
	if not A.exists(e) then return false end
	local name = COMPONENT[kind]
	return name == nil or A.component(e, name) ~= nil
end

function A.kindOf(e)
	if not A.exists(e) then return nil end
	for _, kind in ipairs(KIND_ORDER) do
		if A.component(e, COMPONENT[kind]) ~= nil then return kind end
	end
	return nil
end

local function indexOf(list, e)
	local n = list ~= nil and try(function() return #list end) or 0
	for i = 1, n do
		if try(function() return list[i] end) == e then return i end
	end
	return nil
end

-- The construction that owns a depot or a station (`holder` is "depots" or "stations") and the 1-based place of the
-- entity in that construction's list: a name that stays true when the entity's own id is not bound to a key.
function A.ownerOf(e, holder)
	local sys = try(function() return api.engine.system.streetConnectorSystem end)
	if sys == nil then return nil end
	for _, lookup in ipairs(OWNER_LOOKUPS) do
		local con = try(function() return sys[lookup](e) end)
		if type(con) == "number" and con >= 0 then
			local c = A.component(con, "CONSTRUCTION")
			local i = c and indexOf(c[holder], e)
			if i then return con, i end
		end
	end
	return nil
end

function A.holderEntity(con, holder, i)
	local c = A.component(con, "CONSTRUCTION")
	return c and try(function() return c[holder][i] end)
end

function A.groupOf(station)
	return try(function() return api.engine.system.stationGroupSystem.getStationGroup(station) end)
end

function A.stationsOf(group)
	local g = A.component(group, "STATION_GROUP")
	return g and g.stations
end

function A.fileOf(con)
	local c = A.component(con, "CONSTRUCTION")
	return c and c.fileName
end

-- translation column (indices 13..15) of a construction's matrix, as the bridge reads it
function A.positionOf(con)
	local c = A.component(con, "CONSTRUCTION")
	local m = c and c.transf
	if m == nil then return nil end
	local x, y, z = try(function() return m[13] end), try(function() return m[14] end), try(function() return m[15] end)
	if type(x) ~= "number" or type(y) ~= "number" then return nil end
	return { x = x, y = y, z = z or 0 }
end

-- every entity with a component (forEachEntityWithComponent refuses some component types, hence the second way:
-- finding from MPFever (MIT))
function A.entitiesWith(name)
	local ids = {}
	local ok = pcall(function()
		api.engine.forEachEntityWithComponent(function(e) ids[#ids + 1] = e end, api.type.ComponentType[name])
	end)
	if not ok then
		ids = {}
		local list = try(function() return api.engine.getEntitiesWithComponent(api.type.ComponentType[name]) end)
		for i = 1, list ~= nil and #list or 0 do ids[#ids + 1] = list[i] end
	end
	table.sort(ids)
	return ids
end

-- model ids are resource indices; the resource name is what is the same on every machine (modelRep.getName and find
-- are what the game's achievement script uses)
function A.modelName(id)
	return try(function() return api.res.modelRep.getName(id) end)
end

function A.modelFind(name)
	local id = try(function() return api.res.modelRep.find(name) end)
	if type(id) == "number" and id > 0 then return id end
	return nil
end

-- A new engine object of a named type, or nil where the type does not exist (the test harness, an older build).
local TYPES = {
	TransportVehicleConfig = function() return api.type.TransportVehicleConfig end,
	TransportVehiclePart = function() return api.type.TransportVehiclePart end,
	VehiclePart = function() return api.type.VehiclePart end,
	LoadConfig = function() return api.type.LoadConfig end,
	Line = function() return api.type.Line end,
	LineStop = function() return api.type.Line.Stop end,
	StopConfig = function() return api.type.Line.StopConfig end,
	StationTerminal = function() return api.type.StationTerminal end,
	Vec3f = function() return api.type.Vec3f end,
	Date = function() return api.type.Date end,
}
-- constructors that take their members as arguments
local CTOR_ARGS = {
	Vec3f = function(t) return t.x, t.y, t.z end,
	StationTerminal = function(t) return t.station, t.terminal end,
	Date = function(t) return t.year, t.month, t.day end,
}

function A.new(typeName, tree)
	local get = TYPES[typeName]
	local ctor = get and try(get)
	if ctor == nil then return nil end
	local obj = nil
	if CTOR_ARGS[typeName] and tree then
		obj = try(function() return ctor.new(CTOR_ARGS[typeName](tree)) end)
	end
	if obj == nil then obj = try(function() return ctor.new() end) end
	return obj
end

function A.members(o)
	local ok, m = pcall(function() return o.__members end)
	if ok and m ~= nil then return m end
	local mt = getmetatable(o)
	if type(mt) == "table" then
		ok, m = pcall(function() return mt.__members end)
		if ok and m ~= nil then return m end
	end
	return nil
end

-- ================================================================== engine objects <-> plain tables

local function copy(v)
	if type(v) ~= "table" then return v end
	local out = {}
	for k, x in pairs(v) do out[k] = copy(x) end
	return out
end

-- An engine object (or any plain value) as a tree of numbers, strings, booleans and tables. nil and a reason for
-- what cannot travel: a function, an object this module cannot read.
local function marshal(v, depth)
	local t = type(v)
	if t == "nil" or t == "boolean" or t == "number" or t == "string" then return v end
	if t ~= "table" and t ~= "userdata" then return nil, "a " .. t .. " cannot be replicated" end
	depth = depth or 0
	if depth > 24 then return nil, "too deep" end
	local m = A.members(v)
	local count = m ~= nil and try(function() return #m end) or 0
	if count > 0 then -- (an empty member list counts as none, as in MPFever's reader (MIT): a vector then reads as x, y, z)
		local out = {}
		for i = 1, count do
			local k = m[i]
			local ok, x = pcall(function() return v[k] end)
			if ok and type(x) ~= "function" then
				local w, why = marshal(x, depth + 1)
				if w == nil and why then return nil, tostring(k) .. ": " .. why end
				out[k] = w
			end
		end
		return out
	end
	if t == "userdata" then
		local okx, x = pcall(function() return v.x end)
		if okx and type(x) == "number" then
			local out = { x = x }
			for _, k in ipairs({ "y", "z", "w" }) do
				local c = try(function() return v[k] end)
				if type(c) == "number" then out[k] = c end
			end
			return out
		end
		local okl, n = pcall(function() return #v end)
		if okl and type(n) == "number" then
			local out = {}
			for i = 1, n do
				local oki, x2 = pcall(function() return v[i] end)
				if not oki then return nil, "item " .. i .. " cannot be read" end
				local w, why = marshal(x2, depth + 1)
				if w == nil and why then return nil, "item " .. i .. ": " .. why end
				out[i] = w
			end
			return out
		end
		return nil, "an engine object this module cannot read (" .. tostring(v) .. ")"
	end
	local out = {}
	for k, x in pairs(v) do
		if type(k) ~= "number" and type(k) ~= "string" then return nil, "a table keyed by a " .. type(k) end
		local w, why = marshal(x, depth + 1)
		if w == nil and why then return nil, tostring(k) .. ": " .. why end
		out[k] = w
	end
	return out
end

-- Which members need an object of a type of their own (a list of them, or one), and which are derived by the engine
-- and never copied (false). Anything else is copied as the plain value it was read as.
local SCHEMA = {
	TransportVehicleConfig = { vehicles = { list = "TransportVehiclePart" } },
	TransportVehiclePart = { part = { inplace = "VehiclePart" } },
	VehiclePart = { compartment2loadConfig = { list = "LoadConfig" }, color = "Vec3f" },
	Line = { stops = { list = "LineStop" } },
	LineStop = { alternativeTerminals = { list = "StationTerminal" }, stopConfig = "StopConfig", waypoints = false },
}

local rebuild

-- Writes a table into member k of an engine object. Some members only take an engine object of their own type (the real
-- engine: Line.vehicleInfo is a LineVehicleInfo, and a plain table is refused with "bad argument #2 ... (const LineVehicleInfo
-- expected, got no value)" - seen in every real game that created a line). When the table is refused and the member holds an
-- engine object, it is filled in place, member by member; if that fails too, the first refusal stands.
local function setMember(obj, k, val)
	local ok, err = pcall(function() obj[k] = val end)
	if ok then return end
	if type(val) ~= "table" then error(err, 0) end
	local cur = try(function() return obj[k] end)
	if cur == nil or type(cur) == "number" or type(cur) == "string" or type(cur) == "boolean" or A.members(cur) == nil then error(err, 0) end
	local okAll = pcall(function()
		for kk, vv in pairs(val) do setMember(cur, kk, vv) end
	end)
	if not okAll then error(err, 0) end
end

-- Sets the members of `obj` from `tree`. A member the engine calls read-only is left to it (`skipped`, and not
-- compared afterwards). Any other refusal does not stop the rebuild: it is kept in `failed` (member name -> reason),
-- and the comparison with what was read decides whether it mattered, so a member the engine will not take but which
-- comes out the same costs nothing, and one that does not names itself with the engine's own words.
local function fill(obj, tree, typeName, where, skipped, failed)
	local schema = SCHEMA[typeName] or {}
	for k, mv in pairs(tree) do
		local ft = schema[k]
		if ft == false then
			skipped[k] = true -- derived by the engine
		else
			local at = where .. "." .. tostring(k)
			local ok, err = pcall(function()
				if type(ft) == "table" and ft.inplace then
					local cur = obj[k]
					if cur == nil then error("no such member", 0) end
					fill(cur, mv, ft.inplace, at, skipped, failed)
					-- some properties hand out copies (finding from MPFever (MIT)): the filled one is written back; where it is the object itself
					-- (the game's own makePart fills `part.part` in place) the write changes nothing, and a refusal is harmless
					pcall(function() obj[k] = cur end)
					return
				end
				local val = mv
				if type(ft) == "string" then
					val = rebuild(ft, mv, at, skipped, failed)
				elseif type(ft) == "table" and ft.list then
					val = {}
					for i = 1, #mv do val[i] = rebuild(ft.list, mv[i], at .. "[" .. i .. "]", skipped, failed) end
				end
				setMember(obj, k, val)
			end)
			if not ok then
				local msg = tostring(err)
				if msg:lower():find("read[ %-]?only") then
					skipped[k] = true
					note("read-only member '" .. tostring(k) .. "' of " .. typeName .. " is left to the engine")
				elseif failed[k] == nil then
					failed[k] = at .. ": " .. msg
				end
			end
		end
	end
	return obj
end

-- Builds the engine object of `typeName` from a tree. Where the engine has no such type the tree itself stands in.
rebuild = function(typeName, tree, where, skipped, failed)
	if type(tree) ~= "table" then return tree end
	local obj = A.new(typeName, tree)
	if obj == nil then return tree end
	if CTOR_ARGS[typeName] then return obj end
	return fill(obj, tree, typeName, where or typeName, skipped or {}, failed or {})
end

-- Every member of `a` is in `b` with the same value (b may have more: defaults, derived members). Members the
-- rebuild had to skip as read-only are not compared. Returns true, or false and the path of the first difference.
local function contained(a, b, path, skipped)
	if type(a) == "number" and type(b) == "number" then
		return math.abs(a - b) <= 1e-6 * math.max(1, math.abs(a)), path
	end
	if type(a) ~= "table" or type(b) ~= "table" then return a == b, path end
	if #a > 0 and #a ~= #b then return false, path .. "#" end -- a list that came back longer or shorter
	for k, x in pairs(a) do
		if not skipped[k] then
			local ok, where = contained(x, b[k], path .. "." .. tostring(k), skipped)
			if not ok then return false, where end
		end
	end
	return true
end

-- ================================================================== references

-- A reference travels as { __ref = "player" } for the player's own company and { __ref = "entity", key = K } for
-- anything else, K being the key the lockstep core gives the entity: "s:<id>" from the savegame, "<sender>:<seq>" for
-- what a replicated command created. Depots and station groups also carry the construction that owns them and their
-- place in it (`at`), which stays true when their own id was never bound; a construction carries its file and
-- position (`f`, `p`) and is searched for by them when its key does not fit. Whatever a game resolves is checked.

local function newState()
	return { seen = {} }
end

local function dryCtx(st)
	return { player = A.player(), resolve = function(key) return st.seen[key] end }
end

local HOLDER = { depot = "depots", station = "stations" }

local function conRef(ctx, st, con)
	local key = ctx.keyOf(con)
	st.seen[key] = con
	return { key = key, f = A.fileOf(con) }
end

local function describe(ctx, st, e, kind)
	local con, i = nil, nil
	if kind == "depot" or kind == "station" then
		con, i = A.ownerOf(e, HOLDER[kind])
	elseif kind == "stationGroup" then
		local stations = A.stationsOf(e)
		local first = stations ~= nil and try(function() return stations[1] end)
		if first then con, i = A.ownerOf(first, "stations") end
	end
	if con then return { con = conRef(ctx, st, con), i = i } end
	return nil
end

local function near(p, q)
	if p == nil or q == nil then return false end
	local dx, dy, dz = p.x - q.x, p.y - q.y, (p.z or 0) - (q.z or 0)
	return dx * dx + dy * dy + dz * dz < 2.25
end

local function matches(con, r)
	if r.f ~= nil and A.fileOf(con) ~= r.f then return false end
	if r.p ~= nil and not near(A.positionOf(con), r.p) then return false end
	return true
end

local function resolveConstruction(ctx, r)
	local e = r.key ~= nil and ctx.resolve(r.key) or nil
	if e ~= nil and A.isKind(e, "construction") and matches(e, r) then return e end
	if r.f == nil or r.p == nil then return nil end
	for _, c in ipairs(A.entitiesWith("CONSTRUCTION")) do
		if matches(c, r) then return c end
	end
	return nil
end

local function resolveAt(ctx, r, kind)
	local at = r.at
	if type(at) ~= "table" or type(at.con) ~= "table" then return nil end
	local con = ctx.resolve(at.con.key)
	if con == nil or not A.isKind(con, "construction") or not matches(con, at.con) then return nil end
	if kind == "depot" then return A.holderEntity(con, "depots", at.i) end
	local station = A.holderEntity(con, "stations", at.i)
	if kind == "station" then return station end
	return station ~= nil and A.groupOf(station) or nil
end

local function decodeRef(ctx, r, kind)
	if type(r) ~= "table" then return nil, "malformed " .. kind .. " reference" end
	if r.__ref == "player" then return ctx.player end
	if kind == "entity" and type(r.kind) == "string" then kind = r.kind end
	local e
	if kind == "construction" then
		e = resolveConstruction(ctx, r)
	else
		e = resolveAt(ctx, r, kind)
		if e == nil and r.key ~= nil then e = ctx.resolve(r.key) end
	end
	if e == nil then return nil, "unresolved " .. kind .. " reference " .. tostring(r.key) end
	if not A.isKind(e, kind) then
		local why = kind .. " " .. tostring(r.key) .. " does not exist here"
		-- an "s:" key is the id the sending game gave the entity: right for what the savegame holds, wrong for anything its
		-- player made since without a replicated command (a native build that was not replicated, a purchase not yet keyed)
		if tostring(r.key):match("^s:") then why = why .. " (an s: key names an entity of the savegame or one the sender made without a replicated command)" end
		return nil, why
	end
	return e
end

local function encodeRef(ctx, st, e, kind)
	if type(e) ~= "number" then return nil, "not an entity id" end
	if e == A.player() then return { __ref = "player" } end
	local generic = kind == "entity"
	if generic then kind = A.kindOf(e) or "entity" end
	local key = ctx.keyOf(e)
	st.seen[key] = e
	local r = { __ref = "entity", key = key }
	if generic then r.kind = kind end
	if kind == "construction" then
		r.f, r.p = A.fileOf(e), A.positionOf(e)
	else
		r.at = describe(ctx, st, e, kind)
		-- the entity belongs to a building made in the session: its own id is then not one the other games share, and
		-- the building's key and the entity's place in it are its name
		if r.at and not tostring(r.at.con.key):match("^s:") then r.key = nil end
	end
	-- the dry run: the reference must lead back to this very entity; a hint that misleads is dropped
	if decodeRef(dryCtx(st), r, kind) ~= e and r.at then
		r.at = nil
	end
	if decodeRef(dryCtx(st), r, kind) ~= e then return nil, "cannot refer to entity " .. e .. " as a " .. kind end
	return r
end

-- ================================================================== argument codecs

-- A codec turns one argument into its wire form and back. enc(ctx, v, st) -> wire | nil, reason (nil without a
-- reason is a nil argument); dec(ctx, w) -> value | nil, reason.

local PLAIN = {
	enc = function(_, v) return marshal(v) end,
	dec = function(_, w) return w end,
}

local function entityArg(kind)
	return {
		enc = function(ctx, v, st)
			if v == nil then return nil end
			return encodeRef(ctx, st, v, kind)
		end,
		dec = function(ctx, w)
			if w == nil then return nil end
			return decodeRef(ctx, w, kind)
		end,
	}
end

local PLAYER, VEHICLE, LINE_ENTITY, DEPOT, TOWN, ANY =
	entityArg("player"), entityArg("vehicle"), entityArg("line"), entityArg("depot"), entityArg("town"), entityArg("entity")

-- a list of entities of one kind (a sale names several vehicles at once)
local function listArg(kind)
	return {
		enc = function(ctx, v, st)
			local n = (type(v) == "table" or type(v) == "userdata") and try(function() return #v end)
			if not n then return nil, "not a list" end
			local out = {}
			for i = 1, n do
				local r, why = encodeRef(ctx, st, v[i], kind)
				if not r then return nil, "item " .. i .. ": " .. tostring(why) end
				out[i] = r
			end
			return out
		end,
		dec = function(ctx, w)
			if type(w) ~= "table" then return nil, "malformed list" end
			local out = {}
			for i = 1, #w do
				local e, why = decodeRef(ctx, w[i], kind)
				if e == nil then return nil, "item " .. i .. ": " .. tostring(why) end
				out[i] = e
			end
			return out
		end,
	}
end
local VEHICLES = listArg("vehicle")

-- An engine object with its own type. `wireOf(ctx, st, tree)` turns the tree read from the object into the wire
-- form (entity ids become references), `treeOf(ctx, wire)` is its inverse; both may fail with a reason. `read`
-- replaces the member-list reading for an object whose fields are known (a date).
local function objectArg(typeName, wireOf, treeOf, read)
	read = read or marshal
	local codec = {}
	-- The receiving side: the object is rebuilt and read back, and what it holds must be what was shipped, so a body
	-- that does not fit the engine's object (garbage, or a member this build refuses) is refused instead of applied
	-- half built. Returns the object, then (for the dry run) what was read back and the notes of the rebuild.
	codec.dec = function(ctx, w)
		if w == nil then return nil end -- an argument the caller left out
		if type(w) ~= "table" then return nil, "malformed " .. typeName end
		local tree, why = treeOf(ctx, copy(w))
		if tree == nil then return nil, why end
		local skipped, failed = {}, {}
		local ok, obj = pcall(rebuild, typeName, tree, typeName, skipped, failed)
		if not ok then return nil, tostring(obj) end
		local again, why2 = read(obj)
		if again == nil then return nil, "does not read back: " .. tostring(why2) end
		local same, path = contained(tree, again, typeName, skipped)
		if not same then
			local first = next(failed)
			return nil, "does not rebuild equal at " .. path .. (first and (" (the engine refused: " .. failed[first] .. ")") or "")
		end
		for _, msg in pairs(failed) do note("a member the engine refused came out equal anyway: " .. msg) end
		return obj, nil, again, skipped
	end
	-- The sending side: the dry run is the receiving side's work on the wire form, plus a comparison of what comes out
	-- with what the window's own object held (the translations of ids and names must lead back to the same values).
	codec.enc = function(ctx, v, st)
		if v == nil then return nil end
		local tree, why = read(v)
		if tree == nil then return nil, why or "empty " .. typeName end
		local wire = copy(tree)
		wire, why = wireOf(ctx, st, wire)
		if wire == nil then return nil, why end
		local obj, why2, again, skipped = codec.dec(dryCtx(st), wire)
		if obj == nil then return nil, why2 end
		local same, path = contained(tree, again, typeName, skipped)
		if not same then return nil, "does not rebuild equal at " .. path end
		return wire
	end
	return codec
end

local function eachPart(tree, fn)
	local vs = tree.vehicles
	for i = 1, type(vs) == "table" and #vs or 0 do
		local p = type(vs[i]) == "table" and vs[i].part
		if type(p) == "table" then fn(p) end
	end
end

-- A vehicle configuration carries the model resource names next to the ids: the name is what is the same on every
-- machine, the id is checked against it when both are known.
local CONFIG = objectArg("TransportVehicleConfig",
	function(_, _, wire)
		eachPart(wire, function(p)
			local name = A.modelName(p.modelId)
			if name ~= nil then p.modelName = name end
		end)
		return wire
	end,
	function(_, tree)
		local why = nil
		eachPart(tree, function(p)
			if p.modelName ~= nil then
				local id = A.modelFind(p.modelName)
				if id == nil then why = "model " .. tostring(p.modelName) .. " is not installed here" end
				if id ~= nil and p.modelId ~= nil and id ~= p.modelId then
					note("model " .. tostring(p.modelName) .. " is " .. id .. " here and " .. tostring(p.modelId) .. " on the sending game")
				end
				p.modelId = id or p.modelId
				p.modelName = nil
			end
		end)
		if why then return nil, why end
		return tree
	end)

-- A line: the station group of each stop is an entity; waypoints name network edges, whose ids no game shares, so a
-- line that has any is not replicated yet.
local LINE = objectArg("Line",
	function(ctx, st, wire)
		local stops = wire.stops
		for i = 1, type(stops) == "table" and #stops or 0 do
			local s = stops[i]
			if type(s.waypoints) == "table" and next(s.waypoints) ~= nil then return nil, "stop " .. i .. " has waypoints (not replicated yet)" end
			s.waypoints = nil
			local r, why = encodeRef(ctx, st, s.stationGroup, "stationGroup")
			if not r then return nil, "stop " .. i .. ": " .. tostring(why) end
			s.stationGroup = r
		end
		return wire
	end,
	function(ctx, tree)
		local stops = tree.stops
		for i = 1, type(stops) == "table" and #stops or 0 do
			local e, why = decodeRef(ctx, stops[i].stationGroup, "stationGroup")
			if e == nil then return nil, "stop " .. i .. ": " .. tostring(why) end
			stops[i].stationGroup = e
		end
		return tree
	end)

local function plainObject(typeName, read)
	return objectArg(typeName, function(_, _, wire) return wire end, function(_, tree) return tree end, read)
end
local VEC3 = plainObject("Vec3f")
-- a date has year, month and day (the game's calendar widget reads and builds it that way)
local DATE = plainObject("Date", function(v)
	local y, m, d = try(function() return v.year end), try(function() return v.month end), try(function() return v.day end)
	if type(y) ~= "number" or type(m) ~= "number" or type(d) ~= "number" then return nil, "not a date" end
	return { year = y, month = m, day = d }
end)

-- A journal entry (amount, time, category). The category's enum values are told apart by name, from the lists in
-- tealdef/scripts/journal.d.tl; the sub-category is only read for the types that have one.
local JOURNAL_NAMES = {
	Type = { "LOAN", "INTEREST", "CONSTRUCTION", "ACQUISITION", "MAINTENANCE", "INCOME", "OTHER", "SUBSIDY" },
	Construction = { "STREET", "TRACK", "SIGNAL", "STATION", "DEPOT", "BULLDOZER", "WAREHOUSE", "OTHER" },
	Maintenance = { "VEHICLE", "INFRASTRUCTURE", "OTHER", "VEHICLE_MAINTENANCE" },
	Other = { "OTHER" },
	Carrier = { "ROAD", "RAIL", "TRAM", "OTHER", "AIR", "WATER" },
}
local JOURNAL_SUB = {
	CONSTRUCTION = { "construction", "Construction" }, MAINTENANCE = { "maintenance", "Maintenance" },
	OTHER = { "other", "Other" }, INCOME = { "carrier", "Carrier" },
}

local function journalEnum(group)
	return try(function() return api.type.JournalEntry[group] end)
end

local function enumName(group, value)
	local enum = journalEnum(group)
	if enum == nil or value == nil then return nil end
	for _, name in ipairs(JOURNAL_NAMES[group]) do
		if try(function() return enum[name] end) == value then return name end
	end
	return nil
end

local function journalWire(entry)
	local cat = try(function() return entry.category end)
	if cat == nil or type(entry.amount) ~= "number" then return nil, "not a journal entry" end
	local kind = enumName("Type", cat.type)
	if kind == nil then return nil, "a journal category this module does not know" end
	local out = { amount = entry.amount, time = entry.time, type = kind }
	local sub = JOURNAL_SUB[kind]
	if sub then
		local name = enumName(sub[2], try(function() return cat[sub[1]] end))
		if name then out.sub = name end
	end
	return out
end

local function journalEntry(w)
	if w == nil then return nil end
	if type(w) ~= "table" then return nil, "malformed journal entry" end
	local entry = try(function() return api.type.JournalEntry.new() end)
	if entry == nil then return w end -- no engine type here: the plain table stands in
	local ok, err = pcall(function()
		entry.amount = w.amount
		entry.time = w.time
		entry.category.type = api.type.JournalEntry.Type[w.type]
		local sub = JOURNAL_SUB[w.type]
		if sub and w.sub then entry.category[sub[1]] = api.type.JournalEntry[sub[2]][w.sub] end
	end)
	if not ok then return nil, tostring(err) end
	return entry
end

local JOURNAL = {
	enc = function(_, v)
		if v == nil then return nil end
		local wire, why = journalWire(v)
		if wire == nil then return nil, why end
		local back, why2 = journalEntry(wire)
		local again = back and journalWire(back)
		if not again or again.amount ~= wire.amount or again.type ~= wire.type or again.sub ~= wire.sub then
			return nil, "does not rebuild: " .. tostring(why2)
		end
		return wire
	end,
	dec = function(_, w) return journalEntry(w) end,
}

-- ================================================================== commands

-- A codec or handler that fails with an error answers "nil, reason" instead: an encode that raised would throw into
-- the window that sent the command, a decode that raised on a body another player sent would stop the simulation half
-- and make it try the same action again at every step.
local function guarded(what, f)
	return function(...)
		local r = { pcall(f, ...) }
		if not r[1] then return nil, what .. " failed: " .. tostring(r[2]) end
		if r[3] ~= nil then return r[2], r[3] end
		return r[2]
	end
end

-- command{ name, category, args = { codec, ... }, creates, result }: argument i without a codec travels as a plain
-- value. `creates` and `result` are the lockstep core's: the component of the entity the command creates and the
-- field of the callback data that names it.
local function command(def)
	local codecs = def.args or {}
	local spec = { category = def.category, creates = def.creates, result = def.result }
	spec.encode = guarded("encode", function(name, args, ctx)
		local n = args.n or #args
		local st = newState()
		local out = { n = n }
		for i = 1, n do
			local w, why = (codecs[i] or PLAIN).enc(ctx, args[i], st)
			if w == nil and why then return nil, "argument " .. i .. ": " .. tostring(why) end
			out[i] = w
		end
		return { fn = name, args = out }
	end)
	spec.decode = guarded("decode", function(body, ctx)
		local a = body.args
		if type(a) ~= "table" then return nil, "malformed body" end
		local n = a.n or #a
		local out = { n = n }
		for i = 1, n do
			local v, why = (codecs[i] or PLAIN).dec(ctx, a[i])
			if v == nil and why then return nil, "argument " .. i .. ": " .. tostring(why) end
			out[i] = v
		end
		return out
	end)
	M.commands[def.name] = spec
	return spec
end

-- vehicles. Buying and replacing create the vehicle: the lockstep core binds it to the key of the action that made
-- it, on every game, so the next command can name it. (Whether a replacement is a new entity or the old one changed
-- in place is not known: docs/COVERAGE_OPS.md.)
command{ name = "makeVehicleBuyCmd", category = "spend", args = { PLAYER, DEPOT, CONFIG }, creates = "TRANSPORT_VEHICLE", result = "resultVehicleEntity" }
command{ name = "makeVehicleReplaceCmd", category = "spend", args = { VEHICLE, CONFIG }, creates = "TRANSPORT_VEHICLE", result = "resultVehicleEntity" }
command{ name = "makeVehicleSellCmd", category = "bulldoze", args = { VEHICLES } }
command{ name = "makeVehicleSetLineCmd", category = "other", args = { VEHICLE, LINE_ENTITY } }
command{ name = "makeVehicleSendToDepotCmd", category = "other", args = { VEHICLE } }
command{ name = "makeVehicleReverseCmd", category = "other", args = { VEHICLE } }
command{ name = "makeVehicleSetStoppedByUserCmd", category = "other", args = { VEHICLE } }
-- not used by the base game's windows; registered with a vehicle first, as a mod may call them
command{ name = "makeVehicleTryToDepartCmd", category = "other", args = { VEHICLE } }
command{ name = "makeVehicleSetManualDepartureCmd", category = "other", args = { VEHICLE } }
command{ name = "makeVehicleSetModifiersCmd", category = "other", args = { VEHICLE } }

-- lines. Creating one yields the entity (its key is the action's); stops, waiting times, load modes, cargo filters
-- and the reservation priority all travel inside the Line object of create and update.
command{ name = "makeLineCreateCmd", category = "build", args = { PLAIN, VEC3, PLAYER, LINE }, creates = "LINE", result = "resultEntity" }
command{ name = "makeLineUpdateCmd", category = "build", args = { LINE_ENTITY, LINE } }
command{ name = "makeLineDestroyCmd", category = "bulldoze", args = { LINE_ENTITY } }

-- names and colours: of lines, vehicles, station groups, depots, towns and the company itself (the player entity)
command{ name = "makeEntitySetNameCmd", category = "other", args = { ANY } }
command{ name = "makeEntitySetColorCmd", category = "other", args = { ANY, VEC3 } }

-- money booked from a window: the balance edit of the sandbox and the cost of a marketing campaign. Loans are events
-- (below). The bridge's own corrections go through the game's sendCommand and never reach this.
command{ name = "makeJournalBookAssetCmd", category = "spend", args = { PLAYER, JOURNAL, VEC3 } }

-- the world's clock and weather, set from the calendar widget of the game bar
command{ name = "makeGameSetCalendarSpeedCmd", category = "other" }
command{ name = "makeGameSetDateCmd", category = "other", args = { DATE } }
command{ name = "makeGameSetTimeOfDayCmd", category = "other" }
command{ name = "makeGameSetCloudCoverageCmd", category = "other" }

-- towns, industries and cargo
command{ name = "makeStockListDiscardCargoCmd", category = "other", args = { ANY } }
command{ name = "makeTownBuildingSetBlockedDevelopmentCmd", category = "other", args = { entityArg("construction") } }
command{ name = "makeTownSetDevelopmentActiveCmd", category = "other", args = { TOWN } }
command{ name = "makeTownSetInitialLandUseCapacitiesCmd", category = "other", args = { TOWN } }
command{ name = "makeTownCustomDistributionWeightsCmd", category = "other", args = { TOWN } }
command{ name = "makeTownUpdateCargoNeedsCmd", category = "other", args = { TOWN } }

-- ================================================================== events

-- An event whose parameter is a plain table with entity fields: fields = { <field> = <kind> }.
local function fieldEvent(category, fields)
	return {
		category = category,
		encode = guarded("encode", function(param, ctx)
			local tree, why = marshal(param)
			if type(tree) ~= "table" then return nil, why or "no parameters" end
			local st = newState()
			for field, kind in pairs(fields) do
				if tree[field] ~= nil then
					local r, why2 = encodeRef(ctx, st, tree[field], kind)
					if not r then return nil, field .. ": " .. tostring(why2) end
					tree[field] = r
				end
			end
			return tree
		end),
		decode = guarded("decode", function(param, ctx)
			if type(param) ~= "table" then return nil, "malformed parameters" end
			local tree = copy(param)
			for field, kind in pairs(fields) do
				if tree[field] ~= nil then
					local e, why = decodeRef(ctx, tree[field], kind)
					if e == nil then return nil, field .. ": " .. tostring(why) end
					tree[field] = e
				end
			end
			return tree
		end),
	}
end

-- A plain event with a plain parameter (the dispatcher copies it). `check(param)` may veto it with a reason.
local function plainEvent(category, check)
	local h = { category = category }
	if check then
		h.encode = guarded("encode", function(param)
			local tree, why = marshal(param)
			if tree == nil and why then return nil, why end
			local veto = check(tree)
			if veto then return nil, veto end
			return tree
		end)
	end
	return h
end

-- the time of day follows this machine's clock in the "Local" mode: no two games would agree
local function notLocal(param)
	if type(param) == "table" and (param.mode == "Local") then return "the Local time-of-day mode follows each machine's clock" end
	return nil
end

M.events = {
	-- loans live in a game script (game_mechanics/finance/loan.script.tl): replaying the event on every game makes
	-- each one pay the loan out and book its repayments, with the same offers; the parameter is {offer, loan}
	Loan = { Obtain = plainEvent("spend"), Repay = plainEvent("spend") },
	Subvention = { onAccept = plainEvent("other"), onDecline = plainEvent("other") },
	Companies = {
		startMarketingCampaign = fieldEvent("spend", { townEntity = "town", companyEntity = "player" }),
		spawnIndustry = fieldEvent("spend", { townEntity = "town", companyEntity = "player" }),
		MakeGreen = fieldEvent("spend", { constructionEntity = "construction", companyEntity = "player" }),
	},
	Towns = {
		setTownRatingSensitivity = fieldEvent("other", { townEntity = "town" }),
		resetTownRatingSensitivity = fieldEvent("other", { townEntity = "town" }),
	},
	GameTime = { SetMode = plainEvent("other", notLocal), OnManualGameTimeChanged = plainEvent("other"), SkipPhase = plainEvent("other") },
	CloudCoverage = { SetMode = plainEvent("other"), OnManualCloudCoverageChanged = plainEvent("other"), SkipPhase = plainEvent("other") },
}

-- for the tests
M.internals = { A = A, marshal = marshal, rebuild = rebuild, contained = contained, SCHEMA = SCHEMA }

return M
