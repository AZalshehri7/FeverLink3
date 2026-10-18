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
-- Argument shapes are read from the call sites in the game's own scripts (base/content/*.zip). An engine object is read
-- member by member by the names its type has in the game's own typed definitions (api/tealdef/api/type.d.tl and
-- engine.d.tl, the tf3-docs API reference): DOC below, one entry per type, never a name tried to see whether it is
-- there. A new object comes from api.type.<Name>.new() and is filled member by member, and some properties hand out
-- copies (finding from MPFever, MIT). Everything that touches the engine sits in the adapters (A): one place to change
-- when a build differs, one place for the tests to mock.
--
-- Why no trial reads (bug #10, the clone freeze): MPFever issue #21 found that every failed access on an engine object
-- (a member a type does not have, e.g. `.w` or `__members` on a Vec3f colour) makes the game write a crash-dump file to
-- Steam\userdata\<id>\3493540\local\crash_dump, inside pcall too, about 0.1 s each (11 dumps per vehicle part). This
-- encoder used to probe that way (`__members` on the object and its metatable, then `.x`, `.y`, `.z`, `.w`, then `#v`).
-- Friend test 2026-10-17: this PC's game wrote 3571 dumps from 01:00 to 03:10, and the two busiest minutes (66 each,
-- 02:08 and 02:20) are the two clone freezes of 7-10 s. Whatever still fails on an engine object is counted
-- (M.counters.engineFailed) and logged as "engine read failed: <Type>.<member>" (or write / call), so a lab run shows
-- whether any remain.
--
-- Two rules keep a wrong guess from becoming a silent divergence. A command is rebuilt on the sending game before it
-- is shipped (a dry run), and an engine object must come back from that rebuild equal to what was read; if not, the
-- command stays native and the log says where. A reference to an entity is checked on every game that applies it
-- (it exists, it is of the expected kind): an action that fails the check is refused, never applied to whatever
-- happens to share the id.

-- counters: the world searches the reference codec made (resolveConstruction) and their time, read by the UI hook's timing line;
-- engineFailed: the reads, writes and calls on engine objects that raised (each may have cost a crash dump, see above)
local M = { commands = {}, events = {}, counters = { scans = 0, scanMs = 0, engineFailed = 0 } }

-- What a game run needs to see to judge the guesses below (an id that differs from its model's name, a member the
-- engine would not take) goes to mod.log, once per message. Silent where the log module is not there.
local log = false
local Common = nil   -- the common module: the logger and the clock
do
	local ok, C = pcall(ug_require, "feverlink3_common.lua")
	if not ok then ok, C = pcall(ug_require, "feverlink3::/feverlink3_common.lua") end
	if ok and type(C) == "table" then Common = C end
	if Common and C.logger then log = C.logger("ops") end
end
-- the real clock of the timing notes (the link module's, feverlink3_link.lua); 0 without the common module
local function clock() return Common and Common.clock and Common.clock() or 0 end
local noted = {}
local function note(msg)
	if noted[msg] or not log then return end
	noted[msg] = true
	log(msg)
end

-- An access on an engine object that raised: counted, and logged with the command it belongs to (the first 5 of each kind, then
-- every 100th, so a failure in a loop cannot flood the log). `kind` is read, write or call; `what` is <Type>.<member> or the call.
local current = nil   -- the command being encoded or decoded
local failures = {}
local function engineFailed(kind, what, err)
	M.counters.engineFailed = M.counters.engineFailed + 1
	local key = kind .. " " .. what
	local n = (failures[key] or 0) + 1
	failures[key] = n
	if log and (n <= 5 or n % 100 == 0) then
		log(string.format("engine %s failed: %s (%s, %d so far): %s", kind, what, tostring(current or "-"), n, tostring(err):match("^[^\n]*")))
	end
end

-- an engine call (documented function, documented arguments) whose error must not reach the window; counted when it raises
local function try(f, what)
	local ok, v = pcall(f)
	if ok then return v end
	engineFailed("call", what or "an engine call", v)
	return nil
end

-- One documented member of an engine object (typeName names the object's type for the log). true and the value, or false and
-- the error, counted.
local function index(o, k) return o[k] end
local function field(obj, typeName, k)
	local ok, v = pcall(index, obj, k)
	if ok then return true, v end
	engineFailed("read", tostring(typeName) .. "." .. tostring(k), v)
	return false, v
end

-- ================================================================== adapters

local A = {}

-- the component each kind of entity carries (names as the game's own scripts write them)
local COMPONENT = {
	vehicle = "TRANSPORT_VEHICLE", line = "LINE", depot = "VEHICLE_DEPOT", stationGroup = "STATION_GROUP",
	station = "STATION", construction = "CONSTRUCTION", town = "TOWN", townBuilding = "TOWN_BUILDING",
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
	local ok, err = pcall(function()
		api.engine.forEachEntityWithComponent(function(e) ids[#ids + 1] = e end, api.type.ComponentType[name])
	end)
	if not ok then
		engineFailed("call", "forEachEntityWithComponent(" .. tostring(name) .. ")", err)
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

-- find answers "the id of the resource, -1 if the resource doesn't exist" (tf3-docs api/res ResTypeRep.find, res.d.tl:28): 0 is a
-- model like any other (until 2026-10-18 this took ids > 0 only, which refused a vehicle of the model with id 0 on every game)
function A.modelFind(name)
	local id = try(function() return api.res.modelRep.find(name) end)
	if type(id) == "number" and id >= 0 then return id end
	return nil
end

-- A new engine object of a named type, or nil where the type does not exist (the test harness, an older build). Only the
-- constructors api.type lists (type.d.tl's api.type record): VehiclePart has none there and is filled in place (DOC below).
local TYPES = {
	TransportVehicleConfig = function() return api.type.TransportVehicleConfig end,
	TransportVehiclePart = function() return api.type.TransportVehiclePart end,
	LoadConfig = function() return api.type.LoadConfig end,
	Line = function() return api.type.Line end,
	LineStop = function() return api.type.Line.Stop end,
	StopConfig = function() return api.type.Line.StopConfig end,
	StationTerminal = function() return api.type.StationTerminal end,
	Vec3f = function() return api.type.Vec3f end,
	Date = function() return api.type.Date end,
	Modifiers = function() return api.type.TransportVehicle.Modifiers end,
	-- a line stop's waypoints (api/tealdef/api/type.d.tl: Waypoint { edgePos : EdgePos, pos : Vec3f, tag : integer }, EdgePos
	-- { edgeId : EdgeId, param : number }, EdgeId.new(entity, index)); the game's line manager makes them with api.type.Waypoint.new()
	-- and sets edgePos to the snap of the network edge clicked, or pos for a ship or aircraft (gui/line_vehicle_mgmt/manager_window.tl)
	Waypoint = function() return api.type.Waypoint end,
	EdgePos = function() return api.type.EdgePos end,
	EdgeId = function() return api.type.EdgeId end,
}
-- constructors that take their members as arguments
local CTOR_ARGS = {
	Vec3f = function(t) return t.x, t.y, t.z end,
	StationTerminal = function(t) return t.station, t.terminal end,
	Date = function(t) return t.year, t.month, t.day end,
	EdgeId = function(t) return t.entity, t.index end,
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

-- ================================================================== engine objects <-> plain tables

local function copy(v)
	if type(v) ~= "table" then return v end
	local out = {}
	for k, x in pairs(v) do out[k] = copy(x) end
	return out
end

-- The members of every engine type a window command carries, by the game's own typed definitions (api/tealdef/api/type.d.tl:
-- TransportVehicleConfig, TransportVehiclePart, VehiclePart, LoadConfig, Vec3f, StationTerminal, Waypoint, EdgePos, EdgeId, Date;
-- api/engine.d.tl: Engine.Component.Line with its Stop and StopConfig, Engine.Component.TransportVehicle.Modifiers). Data members
-- only (no functions), and none the engine derives (DERIVED below). What a member holds:
--   true            a plain value (number, boolean, string; an enum value such as Line.LoadMode is a number)
--   "list"          a list of plain values ({integer}, {boolean}, {string})
--   "<Type>"        an engine object of that type: rebuilt from its constructor (TYPES), or filled in place where it has none
--   { list = T }    a list of engine objects of type T
--   { inplace = T } an object of type T that is filled where it is (the game's own makePart fills `part.part` in place)
-- The shipped bodies of the friend test 2026-10-16 (read then through the binding's member lists) hold exactly these members for
-- the configuration, its parts, the colour, the load configurations, the line, its stops and stop configurations.
local DOC = {
	TransportVehicleConfig = { vehicles = { list = "TransportVehiclePart" }, vehicleGroups = "list", muFileNames = "list" },
	TransportVehiclePart = { part = { inplace = "VehiclePart" }, purchaseTime = true, maintenanceChange = true, maintenanceState = true,
		autoLoadConfig = "list" },
	VehiclePart = { modelId = true, reversed = true, compartment2loadConfig = { list = "LoadConfig" }, color = "Vec3f" },
	LoadConfig = { loadConfigIndex = true, cargoTypeId = true },
	Vec3f = { x = true, y = true, z = true },
	Line = { stops = { list = "LineStop" }, customFilters = true, reservationPriority = true },
	LineStop = { stationGroup = true, station = true, terminal = true, alternativeTerminals = { list = "StationTerminal" }, loadMode = true,
		minWaitingTime = true, maxWaitingTime = true, maxAdditionalWaitingTime = true, waypoints = { list = "Waypoint" }, stopConfig = "StopConfig" },
	StopConfig = { load = "list", maxLoad = "list", forceUnload = true, destroyForConfigChange = true, destroyForRefresh = true },
	StationTerminal = { station = true, terminal = true },
	Waypoint = { edgePos = "EdgePos", pos = "Vec3f", tag = true },
	EdgePos = { edgeId = "EdgeId", param = true },
	EdgeId = { entity = true, index = true },
	Date = { year = true, month = true, day = true },
	Modifiers = { topSpeedScale = true, noiseScale = true, pollutionScale = true, comfortScale = true },
}
-- The documented members of a type that the engine derives itself: never read, never shipped, never written, never compared. A
-- command's object goes out with the engine's own value of such a member (what api.type.<Name>.new() holds, which is what the
-- game's own windows send), and the engine fills it in as it applies the command. A member belongs here only on evidence that
-- the game's own scripts never write it; the reason is kept with it.
--   Line.vehicleInfo (LineVehicleInfo { transportModes }): the line manager builds api.type.Line.new() and sets only stops,
--   customFilters and reservationPriority (gui/line_vehicle_mgmt/manager_window.tl), the cargo filter window edits only the stops
--   (cargofilter_window.tl), line.tl copies the component and sets reservationPriority; no base script writes vehicleInfo (they
--   read it: achievements), and the engine has api.engine.util.getLineTransportModesUnion(line) (util.d.tl). Shipping it made every
--   edit of a line with a mode on fail its own check: the owner's live test of 0.2.0-pre5 (2026-10-18 22:11) refused each one with
--   "does not rebuild equal at Line.vehicleInfo.transportModes.9" (also .3, .2), the rebuilt Line keeping the engine's own value.
--   (owner's decision, 2026-10-18)
local DERIVED = {
	Line = { vehicleInfo = "the engine derives a line's transport modes (no game script writes Line.vehicleInfo)" },
}

-- each type's member names in one fixed order (a reason names the same first member on every run)
local ORDER = {}
for typeName, members in pairs(DOC) do
	local names = {}
	for k in pairs(members) do names[#names + 1] = k end
	table.sort(names)
	ORDER[typeName] = names
end

-- an engine object, as opposed to a plain value or a plain Lua table: userdata (the game's), or a table with a metatable (the tests')
local function isObject(v)
	return type(v) == "userdata" or (type(v) == "table" and getmetatable(v) ~= nil)
end

-- A plain value as a tree of numbers, strings, booleans and tables: what a window passes as a plain argument (a name, a flag, a list
-- of ids) and the parameters of the game scripts' events (plain tables, gui/*.tl). An engine object has no documented type at such a
-- place and is refused, never probed. nil and a reason for what cannot travel.
local function plain(v, depth)
	local t = type(v)
	if t == "nil" or t == "boolean" or t == "number" or t == "string" then return v end
	if t ~= "table" and t ~= "userdata" then return nil, "a " .. t .. " cannot be replicated" end
	if t == "userdata" then return nil, "an engine object this module cannot read without its documented type (" .. tostring(v) .. ")" end
	depth = depth or 0
	if depth > 24 then return nil, "too deep" end
	local out = {}
	for k, x in pairs(v) do
		if type(k) ~= "number" and type(k) ~= "string" then return nil, "a table keyed by a " .. type(k) end
		local w, why = plain(x, depth + 1)
		if w == nil and why then return nil, tostring(k) .. ": " .. why end
		out[k] = w
	end
	return out
end

local readAs

-- The elements of a list member: a Lua table ({T} in the typed definitions) or an engine vector (its documented # and [i]).
local function items(x, where)
	if type(x) == "table" then
		local out = {}
		for i = 1, #x do out[i] = x[i] end
		return out
	end
	local ok, n = pcall(function() return #x end)
	if not ok or type(n) ~= "number" then
		if not ok then engineFailed("read", where .. "#", n) end
		return nil, where .. ": not a list"
	end
	local out = {}
	for i = 1, n do
		local okI, e = field(x, where, i)
		if not okI then return nil, where .. "[" .. i .. "]: the engine refused the read" end
		out[i] = e
	end
	return out
end

-- one member's value, read as what DOC says it holds
local function readMember(x, kind, where, depth)
	if kind == true then
		if type(x) == "userdata" then return nil, "an engine object where the documented type is a plain value" end
		return plain(x, depth)
	end
	if kind == "list" then
		if type(x) == "table" then return plain(x, depth) end
		local list, why = items(x, where)
		if list == nil then return nil, why end
		return plain(list, depth)
	end
	if type(kind) == "string" then return readAs(x, kind, depth) end
	if kind.inplace then return readAs(x, kind.inplace, depth) end
	local list, why = items(x, where)
	if list == nil then return nil, why end
	local out = {}
	for i = 1, #list do
		local w, whyI = readAs(list[i], kind.list, depth)
		if w == nil and whyI then return nil, tostring(i) .. ": " .. whyI end
		out[i] = w
	end
	return out
end

-- An engine object of a documented type as a plain tree: exactly its documented members, each read once. A plain value comes back
-- as it is (a member that is not set, a number where an object was expected: the rebuild's comparison judges it). nil and a reason
-- for what cannot travel: a function, a member the engine refused to hand out (counted and logged: the typed definitions and the
-- game disagree there, and the command stays unreplicated rather than travelling without it), an engine object without any of its
-- members.
readAs = function(v, typeName, depth)
	local t = type(v)
	if t == "nil" or t == "boolean" or t == "number" or t == "string" then return v end
	if t ~= "table" and t ~= "userdata" then return nil, "a " .. t .. " cannot be replicated" end
	depth = (depth or 0) + 1
	if depth > 24 then return nil, "too deep" end
	local doc = DOC[typeName]
	if doc == nil then return nil, "no documented type " .. tostring(typeName) end
	local out, any = {}, false
	for _, k in ipairs(ORDER[typeName]) do
		local ok, x = field(v, typeName, k)
		if not ok then return nil, k .. ": the engine refused the read (" .. tostring(x):match("^[^\n]*") .. ")" end
		if x ~= nil then
			any = true
			local w, why = readMember(x, doc[k], typeName .. "." .. k, depth)
			if w == nil and why then return nil, k .. ": " .. why end
			out[k] = w
		end
	end
	-- (a plain table may hold none: the receiving side reads back what an engine without the type stood in with, e.g. an old body's {})
	if not any and isObject(v) then return nil, "not a " .. typeName .. " (none of its members is there: " .. tostring(v) .. ")" end
	return out
end

-- An engine object of a documented type (typeName), or a plain value (no typeName), as a tree. nil and a reason for what cannot travel.
local function marshal(v, typeName)
	if typeName then return readAs(v, typeName) end
	return plain(v)
end

local rebuild

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

-- the engine type a DOC entry names for a member, nil for a plain value, a list or a map
local function objectType(kind)
	if type(kind) == "string" and DOC[kind] then return kind end
	return nil
end

local function assign(o, k, v) o[k] = v end

-- Writes a value into member k of an engine object of type `owner` (`memberType`: the member's documented engine type, if it has
-- one). A member typed as an object that has no constructor only takes an object of its own type: the real engine refused a plain
-- table for Line.vehicleInfo (LineVehicleInfo, no new(); a derived member now, DERIVED above) with "bad argument #2 ... (const
-- LineVehicleInfo expected, got no value)". The object the member holds is filled member by member and written back: a property of
-- the engine hands out a copy (finding from MPFever (MIT)), and a filled copy that stays unwritten loses every member put into it
-- (friend test 2026-10-16: 115 line edits lost their writes that way). No member DOC carries today needs this; it stays for the
-- next documented type without a constructor. Such a member is filled
-- in place at once when it holds an engine object (the plain table is never offered: a refused write is a raised error, which may
-- cost a crash dump, header); otherwise the plain write is tried, and when it is refused and the member holds an engine object
-- after all, that object is filled. If the filling fails too, the first refusal stands.
local function setMember(obj, k, val, owner, memberType)
	local cur, err = nil, nil
	if type(val) == "table" and memberType ~= nil and TYPES[memberType] == nil then
		local okC, c = field(obj, owner, k)
		if okC and isObject(c) then cur = c end
	end
	if cur == nil then
		local ok, e = pcall(assign, obj, k, val)
		if ok then return end
		engineFailed("write", tostring(owner) .. "." .. tostring(k), e)
		err = e
		if type(val) ~= "table" then error(err, 0) end
		local okC, c = field(obj, owner, k)
		if not (okC and isObject(c)) then error(err, 0) end
		cur = c
	end
	local sub = memberType and DOC[memberType] or nil
	local okAll, errAll = pcall(function()
		for kk, vv in pairs(val) do
			if sub and sub[kk] == nil then error("no member '" .. tostring(kk) .. "' in " .. memberType .. " (the typed definitions have none)", 0) end
			setMember(cur, kk, vv, memberType or (tostring(owner) .. "." .. tostring(k)), sub and objectType(sub[kk]) or nil)
		end
	end)
	if not okAll then error(err or errAll, 0) end
	-- the object itself when the member handed it out, a copy otherwise: writing it back is harmless in the one case and needed in
	-- the other (the member takes an object of its own type, which `cur` is)
	local okW, errW = pcall(assign, obj, k, cur)
	if not okW then engineFailed("write", tostring(owner) .. "." .. tostring(k), errW) end
end

-- Sets the members of `obj` (an engine object of type `typeName`) from `tree`. Only the documented members of the type are offered
-- to the engine: any other key (a body another player made up) is kept in `failed` and never written. A member the engine calls
-- read-only is left to it (`skipped`, and not compared afterwards). Any other refusal does not stop the rebuild: it is kept in
-- `failed` (member name -> reason), and the comparison with what was read decides whether it mattered, so a member the engine will
-- not take but which comes out the same costs nothing, and one that does not names itself with the engine's own words.
local function fill(obj, tree, typeName, where, skipped, failed)
	local doc = DOC[typeName] or {}
	for k, mv in pairs(tree) do
		local ft = doc[k]
		local at = where .. "." .. tostring(k)
		local derived = DERIVED[typeName] and DERIVED[typeName][k]
		if derived then
			-- (a body from a sender that still shipped it) never written, never compared: the engine's own value stays
			skipped[k] = true
			note("derived member '" .. tostring(k) .. "' of " .. typeName .. " is left to the engine: " .. derived)
		elseif ft == nil then
			if failed[k] == nil then failed[k] = at .. ": no member '" .. tostring(k) .. "' in " .. typeName .. " (the typed definitions have none)" end
		else
			local ok, err = pcall(function()
				if type(ft) == "table" and ft.inplace then
					local okC, cur = field(obj, typeName, k)
					if not okC or cur == nil then error("no such member", 0) end
					fill(cur, mv, ft.inplace, at, skipped, failed)
					-- some properties hand out copies (finding from MPFever (MIT)): the filled one is written back unless the member already
					-- reads back filled (the game's own makePart fills `part.part` in place, so the write is normally not needed, and a write
					-- the engine refused would be a raised error)
					local okA, now = field(obj, typeName, k)
					local back = okA and readAs(now, ft.inplace) or nil
					if not (type(back) == "table" and contained(mv, back, at, skipped)) then
						local okW, errW = pcall(assign, obj, k, cur)
						if not okW then engineFailed("write", typeName .. "." .. tostring(k), errW) end
					end
					return
				end
				local val = mv
				if objectType(ft) then
					val = rebuild(ft, mv, at, skipped, failed)
				elseif type(ft) == "table" and ft.list then
					val = {}
					for i = 1, #mv do val[i] = rebuild(ft.list, mv[i], at .. "[" .. i .. "]", skipped, failed) end
				end
				setMember(obj, k, val, typeName, objectType(ft))
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

local HOLDER = { depot = "depots", station = "stations", townBuilding = "townBuildings" }

-- The construction of a town building (Engine.Component.TownBuilding.construction) and the building's 1-based place in that
-- construction's townBuildings list (Engine.Component.Construction.townBuildings; tf3-docs api/engine Engine.Component).
function A.townBuildingOwner(e)
	local tb = A.component(e, "TOWN_BUILDING")
	local con = tb and try(function() return tb.construction end)
	if type(con) ~= "number" or not A.isKind(con, "construction") then return nil end
	local c = A.component(con, "CONSTRUCTION")
	local i = c and indexOf(try(function() return c.townBuildings end), e)
	if i then return con, i end
	return nil
end

local function conRef(ctx, st, con)
	local key = ctx.keyOf(con)
	st.seen[key] = con
	return { key = key, f = A.fileOf(con), p = A.positionOf(con) }
end

local function describe(ctx, st, e, kind)
	local con, i = nil, nil
	if kind == "depot" or kind == "station" then
		con, i = A.ownerOf(e, HOLDER[kind])
	elseif kind == "townBuilding" then
		con, i = A.townBuildingOwner(e)
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
	-- the one search over the whole world on this path (every construction, town buildings included, two component reads each):
	-- counted and timed (M.counters, and a log line when it is slow), so a run shows whether a command paid for it
	local t0 = clock()
	local all = A.entitiesWith("CONSTRUCTION")
	local found = nil
	for _, c in ipairs(all) do
		if matches(c, r) then found = c break end
	end
	local ms = (clock() - t0) * 1000
	M.counters.scans, M.counters.scanMs = M.counters.scans + 1, M.counters.scanMs + ms
	if ms >= 50 and log then log(string.format("construction search by file and place (%s): %d constructions in %.0f ms, %s", tostring(r.f), #all, ms, found and "found" or "not found")) end
	return found
end

local function resolveAt(ctx, r, kind)
	local at = r.at
	if type(at) ~= "table" or type(at.con) ~= "table" then return nil end
	-- The building by its key, or, when this game has not bound that key (a build of the sender's that was never keyed here, or keyed
	-- under another name), by its file and place: the same search a construction reference makes. Sweep run 20261013-074329 (F2/F3): the
	-- purchase at a depot the sender built natively was refused with "unresolved depot reference nil".
	local con = resolveConstruction(ctx, at.con)
	if con == nil then return nil end
	if kind == "depot" then return A.holderEntity(con, "depots", at.i) end
	if kind == "townBuilding" then return A.holderEntity(con, "townBuildings", at.i) end
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
	if e == nil then
		local why = "unresolved " .. kind .. " reference " .. tostring(r.key)
		if type(r.at) == "table" and type(r.at.con) == "table" then
			-- which step of the way through the owning building failed
			local con = resolveConstruction(ctx, r.at.con)
			why = why .. string.format(" (building key %s, file %s: %s", tostring(r.at.con.key), tostring(r.at.con.f),
				con == nil and "no such building here by key or by file and place" or ("found as " .. tostring(con) .. " but it has no " .. kind .. " number " .. tostring(r.at.i)))
			why = why .. ")"
		end
		return nil, why
	end
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
	-- the dry run: the reference must lead back to this very entity; a hint that misleads is dropped (and the reference tried again
	-- without it; when the first try led back, a second one would only repeat its engine reads)
	if decodeRef(dryCtx(st), r, kind) == e then return r end
	if not r.at then return nil, "cannot refer to entity " .. e .. " as a " .. kind end
	r.at = nil
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

-- makeTownBuildingSetBlockedDevelopmentCmd(townBuildingEntity, blockedDevelopment): "the entity Id of the town building" (tf3-docs
-- api/cmd, cmd.d.tl:883). Until 2026-10-18 the argument had to be a construction. An entity that is a construction is still referred
-- to as one (its file and place); a town building entity of its own (TOWN_BUILDING, whose `construction` names its building) is
-- referred to through that construction and its place in the construction's townBuildings list, the way a depot is. Anything else
-- refuses the command.
local TOWN_BUILDING_ARG = {
	enc = function(ctx, v, st)
		if v == nil then return nil end
		if A.isKind(v, "construction") then return encodeRef(ctx, st, v, "construction") end
		if not A.isKind(v, "townBuilding") then return nil, "entity " .. tostring(v) .. " is neither a town building nor a construction" end
		local r, why = encodeRef(ctx, st, v, "townBuilding")
		if r then r.tb = true end
		return r, why
	end,
	dec = function(ctx, w)
		if w == nil then return nil end
		return decodeRef(ctx, w, type(w) == "table" and w.tb and "townBuilding" or "construction")
	end,
}

-- An engine object with its own type. `wireOf(ctx, st, tree)` turns the tree read from the object into the wire
-- form (entity ids become references), `treeOf(ctx, wire)` is its inverse; both may fail with a reason. `read`
-- replaces the typed reading (DOC) where what was read must be checked further (a date).
local function objectArg(typeName, wireOf, treeOf, read)
	read = read or function(v) return marshal(v, typeName) end
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
		-- ctx.objMs (the UI hook's timing line): read = the window's object read member by member, wire = ids to names (model names),
		-- dry = the rebuild into a new engine object and its read-back on this game
		local t = type(ctx.objMs) == "table" and { clock() } or nil
		local tree, why = read(v)
		if tree == nil then return nil, why or "empty " .. typeName end
		if t then t[2] = clock() end
		local wire = copy(tree)
		wire, why = wireOf(ctx, st, wire)
		if wire == nil then return nil, why end
		if t then t[3] = clock() end
		local obj, why2, again, skipped = codec.dec(dryCtx(st), wire)
		if t then
			ctx.objMs[#ctx.objMs + 1] = string.format("%s read=%.1f wire=%.1f dry=%.1f", typeName, (t[2] - t[1]) * 1000, (t[3] - t[2]) * 1000, (clock() - t[3]) * 1000)
		end
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

-- ---- the network entity a waypoint names
--
-- A waypoint on a street or track is an EdgePos: the edge `index` (zero-based) of the TransportNetwork entity `entity`, and `param`, the
-- position along that edge (api/tealdef/api/type.d.tl EdgeId, EdgePos; the game's line manager reads it as
-- getComponent(edgeId.entity, TRANSPORT_NETWORK).edges[index + 1], gui/line_vehicle_mgmt/line_manager_panel.tl). The entity is a segment
-- ("A BaseEdge usually also has a BaseEdgeStreet and a TransportNetwork", api/engine.d.tl), a node or a construction, and its id is not
-- one the other games share. It travels as the reference the build records make for the same entity (feverlink3_cmds_build.lua M.refOf:
-- a segment by key, end positions and tangents, a node by key and position, a construction by key, file and position), which every game
-- resolves to its own entity or refuses the command (unresolved, ambiguous). The index and the param are the same on every game where
-- the entity is the same; the number of edges of its network travels too (`n`), and a game whose entity has another number refuses.
-- A ship's or an aircraft's waypoint is a position (Waypoint.pos) and travels as it is.
--
-- A segment found the other way round (its node0 at the reference's p1: the same street or track, built from the other end on that
-- game; the segment reference matches either direction, feverlink3_cmds_build.lua edgeScore) has its network edges the other way round
-- too, so the same index and param would name another place, or another lane. The place is found again from where the waypoint is:
-- the sending game adds the waypoint's world position and direction on its edge (api.engine.util.transport.calcPositionAndDirection,
-- "Calc position along a transport edge ... @return position on the edge and direction", api/tealdef/api/engine/util.d.tl), and the
-- receiving game takes, among the edges of its segment at param and at 1 - param (EdgePos.param: "The position on the edge", in
-- [0, 1] along the edge's geometry, calcPosition), the one at that position, the one in the same direction where two lie on each
-- other (the two directions of a track, if a track has two). Without those (an older sender, or no geometry readable here) a segment
-- of one edge takes the opposite place, 1 - param (the finding MPFever (MIT) made for reversed tracks, issue #27), and one of several
-- edges is refused: guessing the lane would put the waypoint on another one.
A.WAYPOINT_TOL = 1.0   -- metres between the sent position and the place found here

-- the world position and direction of `param` on edge `index` of network entity e, or nil
function A.edgePlace(e, index, param)
	if type(index) ~= "number" or type(param) ~= "number" then return nil end
	local tn = A.component(e, "TRANSPORT_NETWORK")
	local geom = tn ~= nil and try(function() return tn.edges[index + 1].geometry end) or nil
	local calc = try(function() return api.engine.util.transport.calcPositionAndDirection end)
	if geom == nil or type(calc) ~= "function" then return nil end
	local r = try(function() return calc(geom, param) end)
	local p = r ~= nil and try(function() return r[1] end) or nil
	local d = r ~= nil and try(function() return r[2] end) or nil
	local out = p ~= nil and d ~= nil and try(function() return { x = p.x, y = p.y, z = p.z or 0, dx = d.x, dy = d.y, dz = d.z or 0 } end) or nil
	if out == nil or type(out.x) ~= "number" or type(out.dx) ~= "number" then return nil end
	return out
end

-- whether segment e here runs from the reference's p1 to its p0 (BASE_EDGE node0 nearer p1)
function A.segmentReversed(e, ref)
	if type(ref) ~= "table" or type(ref.p0) ~= "table" or type(ref.p1) ~= "table" then return false end
	local be = A.component(e, "BASE_EDGE")
	local n0 = be ~= nil and try(function() return be.node0 end) or nil
	local nd = n0 ~= nil and A.component(n0, "BASE_NODE") or nil
	local p = nd ~= nil and try(function() return nd.position end) or nil
	if p == nil then return false end
	local function d2(q) return (p.x - q.x) ^ 2 + (p.y - q.y) ^ 2 + ((p.z or 0) - (q.z or 0)) ^ 2 end
	return d2(ref.p1) < d2(ref.p0)
end

-- The index and param on segment e (found reversed here, n edges) of the place `at` (A.edgePlace on the sending game), or nil and why.
function A.reversedPlace(e, n, index, param, at)
	if type(at) == "table" and type(at.x) == "number" then
		local best = nil
		for i = 0, (n or 0) - 1 do
			for _, p in ipairs({ param, 1 - param }) do
				local q = A.edgePlace(e, i, p)
				if q ~= nil then
					local dist = math.sqrt((q.x - at.x) ^ 2 + (q.y - at.y) ^ 2 + (q.z - (at.z or 0)) ^ 2)
					local same = q.dx * (at.dx or 0) + q.dy * (at.dy or 0) + q.dz * (at.dz or 0) >= 0
					-- the position decides; the direction only between places that lie on each other
					local score = dist + (same and 0 or A.WAYPOINT_TOL / 2)
					if dist <= A.WAYPOINT_TOL and (best == nil or score < best.score) then best = { index = i, param = p, score = score } end
				end
			end
		end
		if best ~= nil then return best.index, best.param end
		if A.edgePlace(e, index, param) ~= nil then
			return nil, "segment " .. e .. " runs the other way here and none of its " .. tostring(n) .. " edges passes the waypoint's place ("
				.. string.format("%.1f,%.1f,%.1f", at.x, at.y, at.z or 0) .. ")"
		end
	end
	if n == 1 then return index, 1 - param end
	return nil, "segment " .. e .. " runs the other way here and its " .. tostring(n) .. " edges cannot be told apart without the waypoint's place"
end

local Build = nil   -- the build module, loaded the first time a waypoint needs it (false: it cannot be loaded here)
local function buildModule()
	if Build == nil then
		local ok, B = pcall(ug_require, "feverlink3_cmds_build.lua")
		if not ok then ok, B = pcall(ug_require, "feverlink3::/feverlink3_cmds_build.lua") end
		Build = (ok and type(B) == "table" and type(B.refOf) == "function" and type(B.resolver) == "function") and B or false
	end
	return Build or nil
end

-- the number of edges in an entity's TransportNetwork component, nil when it cannot be read
function A.networkEdges(e)
	local tn = A.component(e, "TRANSPORT_NETWORK")
	local edges = tn ~= nil and try(function() return tn.edges end) or nil
	local n = edges ~= nil and try(function() return #edges end) or nil
	return type(n) == "number" and n or nil
end

-- what kind of network entity e is, as the build module refers to it
local NETWORK_KINDS = { { "edge", "BASE_EDGE" }, { "node", "BASE_NODE" }, { "con", "CONSTRUCTION" } }

local function networkRef(ctx, st, e, index, param)
	local B = buildModule()
	if B == nil then return nil, "the build module is not loaded here (it names network entities)" end
	local kind = nil
	for _, k in ipairs(NETWORK_KINDS) do
		if A.component(e, k[2]) ~= nil then kind = k[1] break end
	end
	if kind == nil then return nil, "network entity " .. tostring(e) .. " is neither a segment, a node nor a construction" end
	local r, why = B.refOf(kind, e, ctx)
	if r == nil then return nil, tostring(why) end
	if r.key ~= nil then st.seen[r.key] = e end   -- (the dry run resolves the key to this very entity)
	local n = A.networkEdges(e)
	if n ~= nil and type(index) == "number" and index >= n then return nil, "edge " .. index .. " of network entity " .. e .. " which has " .. n end
	-- (a segment: where the waypoint is, for a game that has the segment the other way round, A.reversedPlace)
	local at = kind == "edge" and A.edgePlace(e, index, param) or nil
	return { __ref = "network", [kind] = r, n = n, at = at }
end

-- The local entity of a network reference; for a segment found the other way round here also the index and param of the same place
-- (A.reversedPlace). Returns entity, index, param, or nil and why.
local function networkEntity(ctx, r, index, param)
	local B = buildModule()
	if B == nil then return nil, "the build module is not loaded here (it names network entities)" end
	local kind = nil
	for _, k in ipairs(NETWORK_KINDS) do
		if type(r[k[1]]) == "table" then kind = k[1] break end
	end
	if r.__ref ~= "network" or kind == nil then return nil, "malformed network reference" end
	local ok, e = pcall(function() return B.resolver(ctx)[kind](r[kind]) end)
	if not ok then return nil, tostring(e) end
	if type(e) ~= "number" then return nil, "unresolved network entity" end
	local n = A.networkEdges(e)
	if r.n ~= nil and n ~= nil and n ~= r.n then
		return nil, "network entity " .. e .. " has " .. n .. " edges here and " .. tostring(r.n) .. " on the sending game"
	end
	if n ~= nil and type(index) == "number" and index >= n then return nil, "edge " .. index .. " of network entity " .. e .. " which has " .. n end
	if kind == "edge" and type(index) == "number" and type(param) == "number" and A.segmentReversed(e, r.edge) then
		local i, p = A.reversedPlace(e, n, index, param, r.at)
		if i == nil then return nil, p end
		return e, i, p
	end
	return e, index, param
end

-- calls fn(stop i, waypoint j, edgeId, edgePos) for each waypoint of the tree's stops that names a network edge
local function eachWaypointEdge(tree, fn)
	local stops = tree.stops
	for i = 1, type(stops) == "table" and #stops or 0 do
		local wps = type(stops[i]) == "table" and stops[i].waypoints or nil
		for j = 1, type(wps) == "table" and #wps or 0 do
			local ep = type(wps[j]) == "table" and wps[j].edgePos or nil
			local id = type(ep) == "table" and ep.edgeId or nil
			if type(id) == "table" then
				local why = fn(i, j, id, ep)
				if why then return why end
			end
		end
	end
	return nil
end

-- A line: the station group of each stop is an entity, and so is the network entity of each waypoint on a street or track (above).
local LINE = objectArg("Line",
	function(ctx, st, wire)
		local stops = wire.stops
		for i = 1, type(stops) == "table" and #stops or 0 do
			local s = stops[i]
			local r, why = encodeRef(ctx, st, s.stationGroup, "stationGroup")
			if not r then return nil, "stop " .. i .. ": " .. tostring(why) end
			s.stationGroup = r
		end
		local why = eachWaypointEdge(wire, function(i, j, id, ep)
			-- (a waypoint by position has no edge: its EdgeId names no entity, -1, and travels as it is)
			if type(id.entity) ~= "number" or id.entity < 0 then return nil end
			local r, whyR = networkRef(ctx, st, id.entity, id.index, ep.param)
			if r == nil then return "stop " .. i .. " waypoint " .. j .. ": " .. tostring(whyR) end
			id.entity = r
			return nil
		end)
		if why then return nil, why end
		return wire
	end,
	function(ctx, tree)
		local stops = tree.stops
		for i = 1, type(stops) == "table" and #stops or 0 do
			local e, why = decodeRef(ctx, stops[i].stationGroup, "stationGroup")
			if e == nil then return nil, "stop " .. i .. ": " .. tostring(why) end
			stops[i].stationGroup = e
		end
		local why = eachWaypointEdge(tree, function(i, j, id, ep)
			-- a raw id of the sending game would name another entity here, or none: only "none" (-1) travels as a number
			if type(id.entity) == "number" and id.entity >= 0 then return "stop " .. i .. " waypoint " .. j .. ": a raw entity id, not a reference" end
			if type(id.entity) ~= "table" then return nil end
			-- (networkEntity: entity, index, param, or nil and why)
			local e, index, param = networkEntity(ctx, id.entity, id.index, ep.param)
			if e == nil then return "stop " .. i .. " waypoint " .. j .. ": " .. tostring(index) end
			id.entity = e
			-- (the same place on a segment found the other way round here, A.reversedPlace)
			if index ~= id.index then id.index = index end
			if param ~= ep.param then ep.param = param end
			return nil
		end)
		if why then return nil, why end
		return tree
	end)

local function plainObject(typeName, read)
	return objectArg(typeName, function(_, _, wire) return wire end, function(_, tree) return tree end, read)
end
local VEC3 = plainObject("Vec3f")
-- a vehicle's modifiers (Engine.Component.TransportVehicle.Modifiers: topSpeedScale, noiseScale, pollutionScale, comfortScale, tf3-docs; the game's vehicle
-- window copies one with api.type.TransportVehicle.Modifiers.new(tv.modifiers))
local MODIFIERS = plainObject("Modifiers")
-- a date has year, month and day (the game's calendar widget reads and builds it that way)
local DATE = plainObject("Date", function(v)
	local t = (type(v) == "table" or type(v) == "userdata") and readAs(v, "Date") or nil
	if type(t) ~= "table" or type(t.year) ~= "number" or type(t.month) ~= "number" or type(t.day) ~= "number" then return nil, "not a date" end
	return { year = t.year, month = t.month, day = t.day }
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
-- `name(...)` names the command or event for the "engine ... failed" lines; after a run that had any, one line says how many.
local function guarded(what, f, name)
	return function(...)
		local prev, before = current, M.counters.engineFailed
		current = name and name(...) or what
		local r = { pcall(f, ...) }
		local n = M.counters.engineFailed - before
		if n > 0 and log then log(string.format("%s of %s: %d engine access(es) failed (the \"engine ... failed\" lines name them)", what, tostring(current), n)) end
		current = prev
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
		-- ctx.argMs (the UI hook's timing line): the ms each argument's codec took, with the dry run of its reference or object
		local argMs = type(ctx.argMs) == "table" and ctx.argMs or nil
		for i = 1, n do
			local t0 = argMs and clock()
			local w, why = (codecs[i] or PLAIN).enc(ctx, args[i], st)
			if argMs then argMs[i] = (clock() - t0) * 1000 end
			if w == nil and why then return nil, "argument " .. i .. ": " .. tostring(why) end
			out[i] = w
		end
		return { fn = name, args = out }
	end, function() return def.name end)
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
	end, function() return def.name end)
	M.commands[def.name] = spec
	return spec
end

-- vehicles. Buying creates the vehicle (VehicleBuyCommandData.resultVehicleEntity): the lockstep core binds it to the key of the
-- action that made it, on every game, so the next command can name it. Replacing does not: the game's typed definition gives
-- VehicleReplaceCommandData only vehicleEntity, config and applyPurchaseCost, no result entity (tf3-docs api/cmd, cmd.d.tl:898), so
-- the vehicle is changed in place and keeps its key. Until 2026-10-18 the replacement was modelled as creating a vehicle
-- (`resultVehicleEntity`, a field that data does not have): the watch for it found nothing and answered the window's callback
-- with a failure 10 steps after the replacement had applied. docs/COVERAGE_OPS.md V6 says what a real run checks.
command{ name = "makeVehicleBuyCmd", category = "spend", args = { PLAYER, DEPOT, CONFIG }, creates = "TRANSPORT_VEHICLE", result = "resultVehicleEntity" }
command{ name = "makeVehicleReplaceCmd", category = "spend", args = { VEHICLE, CONFIG } }
command{ name = "makeVehicleSellCmd", category = "bulldoze", args = { VEHICLES } }
command{ name = "makeVehicleSetLineCmd", category = "other", args = { VEHICLE, LINE_ENTITY } }
-- (vehicleEntity, sellOnArrival, jumpToDepoEntity: "optional. If provided, then vehicle gets teleported to depot", script-doc
-- api/cmd.html): the depot is an entity like any other and travels as a reference (a raw id would name another entity, or none,
-- on the other games); one that cannot be referred to refuses the command
command{ name = "makeVehicleSendToDepotCmd", category = "other", args = { VEHICLE, PLAIN, DEPOT } }
command{ name = "makeVehicleReverseCmd", category = "other", args = { VEHICLE } }
command{ name = "makeVehicleSetStoppedByUserCmd", category = "other", args = { VEHICLE } }
-- not used by the base game's windows; registered with a vehicle first, as a mod may call them
command{ name = "makeVehicleTryToDepartCmd", category = "other", args = { VEHICLE } }
command{ name = "makeVehicleSetManualDepartureCmd", category = "other", args = { VEHICLE } }
-- (vehicleEntity, modifiers : Engine.Component.TransportVehicle.Modifiers): an engine object, read and rebuilt like the others
command{ name = "makeVehicleSetModifiersCmd", category = "other", args = { VEHICLE, MODIFIERS } }

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
command{ name = "makeTownBuildingSetBlockedDevelopmentCmd", category = "other", args = { TOWN_BUILDING_ARG } }
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

-- A plain event with a plain parameter (the dispatcher copies it). `check(param)` may veto it with a reason (it is then refused
-- on every game, the sender's included). `localFor(param)` true: this one is the sender's own setting, never replicated
-- (category "local", L.EVENT_SPEC).
local function plainEvent(category, check, localFor)
	local h = { category = category, localFor = localFor }
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

-- the time of day follows this machine's clock in the "Local" mode: no two games would agree, so it is each player's own setting
local function localTimeMode(param)
	local ok, mode = pcall(function() return param.mode end)
	return ok and mode == "Local"
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
	GameTime = { SetMode = plainEvent("other", nil, localTimeMode), OnManualGameTimeChanged = plainEvent("other"), SkipPhase = plainEvent("other") },
	CloudCoverage = { SetMode = plainEvent("other"), OnManualCloudCoverageChanged = plainEvent("other"), SkipPhase = plainEvent("other") },
}

-- for the tests
M.internals = { A = A, marshal = marshal, rebuild = rebuild, contained = contained, DOC = DOC }

return M
