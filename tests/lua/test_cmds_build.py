"""Construction replication tests for the FeverLink3 Lua mod (feverlink3_cmds_build.lua).

Two mocked Transport Fever 3 games run the real mod through lupa (Lua 5.2), reusing the harness of test_lockstep.py
(imported, not edited), with the mocked engine extended for construction:
  * proposals, segments, nodes, edge objects and contexts are doubles of the engine's userdata: their members are
    listed in a metatable __members, reading a member that holds a table or another double returns a COPY, and writing
    an unknown member raises (finding from MPFever (MIT): properties return copies, so nested changes must be written
    back);
  * the mocked world keeps a road graph (nodes, segments with objects), constructions with their depots and stations,
    and applies a proposal the way the engine does: the removals, then the nodes, segments, objects and
    constructions it adds, numbering the entities from a per-game offset so that the games' ids differ;
  * the factory throws "Unknown exception" for a segment without lane configurations, as the engine does;
  * the octree, the street system's maps and the street templates answer the way the game's own scripts use them.

Run:  python -I tests/lua/test_cmds_build.py        (or: python -I -m unittest discover -s tests/lua)
Set FEVERLINK3_TEST_VERBOSE=1 to print both games' mod.log on failure.
"""
import os
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import lupa.lua52 as lupa  # noqa: E402

import test_lockstep as T  # noqa: E402  (module, not its test classes: those would be collected again here)

STEP = T.STEP

# ---------------------------------------------------------------- the mocked engine, construction part

BUILD_MOCK = r'''
-- ===== doubles of the engine's userdata =====
local function COPY(v)
  if type(v) ~= "table" then return v end
  local mt = getmetatable(v)
  if mt and mt.__double then return mt.__clone() end
  local out = {}
  for k, x in pairs(v) do out[COPY(k)] = COPY(x) end
  return out
end
_G.COPY = COPY

local DEFS = {}
LANE_SHIFT = 0

local function D(typeName, init)
  local def = DEFS[typeName]
  local store = def.new()
  for k, v in pairs(init or {}) do store[k] = COPY(v) end
  local mt
  mt = {
    __double = typeName, __members = def.members, __store = store,
    __clone = function() return D(typeName, store) end,
    __index = function(_, k)
      local base = type(k) == "string" and k:match("^(.*)_native$")
      if base and def.set[base] then
        local list = store[base]
        return { size = function(self) return #list end, at = function(self, i) return COPY(list[i]) end }
      end
      if def.numeric and type(k) == "number" then return store[k] end
      if def.set and def.set[k] then return COPY(store[k]) end
      return nil
    end,
    __newindex = function(_, k, v)
      if not def.set[k] then error("no writable member '" .. tostring(k) .. "' in " .. typeName, 2) end
      if typeName == "LaneConfig" and k == "transportModes" and LANE_SHIFT ~= 0 then
        -- a write at index k lands at k - shift, index 0 wraps to the end (finding from MPFever (MIT))
        local shifted = {}
        for j = 0, 15 do shifted[(j - LANE_SHIFT) % 16] = v[j] end
        v = shifted
      end
      store[k] = COPY(v)
    end,
  }
  return setmetatable({}, mt)
end
_G.D = D

local function STORE(d) return getmetatable(d).__store end
local function TYPEOF(d) return type(d) == "table" and getmetatable(d) and getmetatable(d).__double or nil end
_G.STORE, _G.TYPEOF = STORE, TYPEOF

local function def(name, members, new, numeric)
  local set = {}
  for _, m in ipairs(members or {}) do set[m] = true end
  DEFS[name] = { members = members, set = set, new = new, numeric = numeric }
end

def("Vec2f", { "x", "y" }, function() return { x = 0, y = 0 } end)
def("Vec3f", { "x", "y", "z" }, function() return { x = 0, y = 0, z = 0 } end)
def("Vec4f", { "x", "y", "z", "w" }, function() return { x = 0, y = 0, z = 0, w = 0 } end)
def("Mat4f", nil, function()
  local c = {}
  for i = 1, 16 do c[i] = (i == 1 or i == 6 or i == 11 or i == 16) and 1 or 0 end
  return c
end, true)
def("BaseNode", { "position" }, function() return { position = D("Vec3f") } end)
def("NodeAndEntity", { "entity", "comp" }, function() return { entity = -1, comp = D("BaseNode") } end)
def("LaneConfig", { "speed", "width", "height", "forward", "offset", "transportModes" }, function()
  local tm = {}
  for j = 0, 15 do tm[j] = false end
  return { speed = 0, width = 0, height = 0, forward = true, offset = 0, transportModes = tm }
end)
def("BaseEdge", { "node0", "node1", "tangent0", "tangent1", "position0", "position1", "type", "typeIndex", "objects", "laneConfigs",
  "roadTemplate", "roadStyle", "roadType", "edgeDecorations", "distance", "roadDevelopmentLocked" }, function()
  return { node0 = -1, node1 = -1, tangent0 = D("Vec3f"), tangent1 = D("Vec3f"), position0 = D("Vec3f"), position1 = D("Vec3f"),
    type = 0, typeIndex = -1, objects = {}, laneConfigs = {}, roadTemplate = "", roadStyle = "", roadType = 0, edgeDecorations = {},
    distance = 0, roadDevelopmentLocked = false }
end)
def("BaseEdgeStreet", { "precedenceNode0", "precedenceNode1" }, function() return { precedenceNode0 = 2, precedenceNode1 = 2 } end)
def("PlayerOwned", { "player" }, function() return { player = -1 } end)
def("BaseEdgeTrack", { "trackType", "catenary" }, function() return { trackType = 0, catenary = false } end)
def("SegmentAndEntity", { "entity", "type", "comp", "streetEdge", "trackEdge", "playerOwned" }, function()
  return { entity = -1, type = 0, comp = D("BaseEdge"), streetEdge = D("BaseEdgeStreet"), trackEdge = D("BaseEdgeTrack"), playerOwned = D("PlayerOwned") }
end)
def("EdgeObjectSimple", { "edgeEntity", "param", "oneWay", "left", "model", "playerEntity", "name" }, function()
  return { edgeEntity = -1, param = 0.5, oneWay = false, left = false, model = "", playerEntity = -1, name = "" }
end)
-- the tools' own edge objects carry no model before the build ran (finding from MPFever (MIT))
def("EdgeObjectNative", { "segmentEntity", "param", "oneWay", "left", "category", "resultEntity", "playerEntity", "name" }, function()
  return { segmentEntity = -1, param = 0, oneWay = false, left = false, category = 0, resultEntity = -1, playerEntity = -1, name = "" }
end)
def("LaneConnection", { "segment0", "lane0", "segment1", "lane1", "withRoad", "withTram" }, function()
  return { segment0 = -1, lane0 = 0, segment1 = -1, lane1 = 0, withRoad = false, withTram = false }
end)
def("BaseNodeConfig", { "laneConnections", "crosswalks", "trafficLightPreference", "doubleSlipSwitch", "userModifiedLaneConnections" }, function()
  return { laneConnections = {}, crosswalks = {}, trafficLightPreference = 0, doubleSlipSwitch = false, userModifiedLaneConnections = false }
end)
def("NodeConfigAndEntity", { "entity", "comp" }, function() return { entity = -1, comp = D("BaseNodeConfig") } end)
def("ConstructionEntity", { "fileName", "params", "transf", "name", "playerEntity" }, function()
  return { fileName = "", params = {}, transf = D("Mat4f"), name = "", playerEntity = -1 }
end)
def("SimpleStreetProposal", { "nodesToAdd", "edgesToAdd", "nodesToRemove", "edgesToRemove", "edgeObjectsToAdd", "edgeObjectsToRemove",
  "nodeConfigsToAdd", "nodeConfigsToRemove" }, function()
  return { nodesToAdd = {}, edgesToAdd = {}, nodesToRemove = {}, edgesToRemove = {}, edgeObjectsToAdd = {}, edgeObjectsToRemove = {},
    nodeConfigsToAdd = {}, nodeConfigsToRemove = {} }
end)
def("SimpleProposal", { "streetProposal", "constructionsToAdd", "constructionsToRemove", "old2new" }, function()
  return { streetProposal = D("SimpleStreetProposal"), constructionsToAdd = {}, constructionsToRemove = {}, old2new = {} }
end)
def("StreetProposal", { "addedNodes", "addedSegments", "removedNodes", "removedSegments", "edgeObjectsToAdd", "edgeObjectsToRemove",
  "nodeConfigsToAdd", "nodeConfigsToRemove" }, function()
  return { addedNodes = {}, addedSegments = {}, removedNodes = {}, removedSegments = {}, edgeObjectsToAdd = {}, edgeObjectsToRemove = {},
    nodeConfigsToAdd = {}, nodeConfigsToRemove = {} }
end)
def("GridVec2f", { "x0", "y0", "width", "height" }, function() return { x0 = 0, y0 = 0, width = 0, height = 0 } end)
def("Terrain", { "baseHeightMod" }, function() return { baseHeightMod = D("GridVec2f") } end)
def("Proposal", { "toAdd", "toRemove", "old2new", "proposal", "terrain" }, function()
  return { toAdd = {}, toRemove = {}, old2new = {}, proposal = D("StreetProposal"), terrain = D("Terrain") }
end)
def("Context", { "player", "checkTerrainAlignment", "cleanupStreetGraph", "gatherBuildings", "gatherFields", "extendProposalRedoPillars", "refundableEntities" }, function()
  return { player = -1, checkTerrainAlignment = false, cleanupStreetGraph = false, gatherBuildings = false, gatherFields = false, extendProposalRedoPillars = false, refundableEntities = {} }
end)
def("EdgeObjectComp", { "edgeObjectConstruction", "param", "params" }, function() return { edgeObjectConstruction = "", param = 0, params = {} } end)
def("ConstructionComp", { "fileName", "transf", "params", "name", "depots", "stations" }, function()
  return { fileName = "", transf = D("Mat4f"), params = {}, name = "", depots = {}, stations = {} }
end)

-- ===== the world =====
-- WORLD entries (the savegame part has the same ids in both games, the session part does not):
--   node        { BASE_NODE = double }          segment { BASE_EDGE = double, KIND = 0 street | 1 track }
--   edge object { EDGE_OBJECT = double, ON = segment id }
--   construction { CONSTRUCTION = double }       depot { VEHICLE_DEPOT = {} }   station { STATION = {} }   group { STATION_GROUP = {} }
local function V(x, y, z) return D("Vec3f", { x = x, y = y, z = z or 0 }) end
_G.V = V

function NEW_ENTITY(comps)
  GAME.nextEntity = GAME.nextEntity + 1
  WORLD[GAME.nextEntity] = comps
  return GAME.nextEntity
end

-- An edge object that is a stop is also a station, and has a station group of its own - unless a test names a group
-- (JOIN_GROUP) for it to join, as a stop beside an existing station does.
JOIN_GROUP = nil
function MAKE_STOP(oid)
  WORLD[oid].STATION = {}
  if JOIN_GROUP then
    WORLD[oid].GROUP = JOIN_GROUP
    local g = WORLD[JOIN_GROUP].STATION_GROUP
    g.stations[#g.stations + 1] = oid
  else
    WORLD[oid].GROUP = NEW_ENTITY({ STATION_GROUP = { stations = { oid } } })
  end
end

TEMPLATES = {}
function DEF_TEMPLATE(name, style, lanes)
  local ls = {}
  for i, l in ipairs(lanes) do
    local lc = D("LaneConfig", { speed = l.speed or 16, width = 3, height = 0.3, forward = l.forward, offset = 0 })
    local tm = {}
    for j = 0, 15 do tm[j] = false end
    for _, m in ipairs(l.modes) do tm[m] = true end
    local stored = STORE(lc)
    stored.transportModes = tm
    ls[i] = lc
  end
  TEMPLATES[#TEMPLATES + 1] = { name = name, style = style, lanes = ls }
end
DEF_TEMPLATE("street/new_medium_road.street_template", "medium", { { forward = true, modes = { 2, 3 } }, { forward = false, modes = { 2, 3 } } })
DEF_TEMPLATE("street/new_small_road.street_template", "small", { { forward = true, modes = { 2 } }, { forward = false, modes = { 2 } } })
DEF_TEMPLATE("track/standard.street_template", "track", { { forward = true, modes = { 8 } } })
DEF_TEMPLATE("track/standard_catenary.street_template", "track", { { forward = true, modes = { 8, 9 } } })
-- the template a real trackBuilder segment carries (real session 2026-10-13); its name contains "simple.street_template"
DEF_TEMPLATE("track/simple/simple.street_template", "track", { { forward = true, modes = { 8 } } })
DEF_TEMPLATE("street/tram_road.street_template", "medium", { { forward = true, modes = { 2, 3 } }, { forward = false, modes = { 2, 3 } }, { forward = true, modes = { 4 } } })
DEF_TEMPLATE("street/airport/taxiway.street_template", "taxiway", { { forward = true, modes = { 5 } }, { forward = false, modes = { 5 } } })
function TEMPLATE_LANES(name)
  for _, t in ipairs(TEMPLATES) do if t.name == name then return COPY(t.lanes), t.style end end
  return nil
end

function ADD_NODE(id, x, y, z) WORLD[id] = { BASE_NODE = D("BaseNode", { position = V(x, y, z) }) } end
function ADD_EDGE(id, n0, n1, kind, tmpl, extra)
  local a, b = STORE(WORLD[n0].BASE_NODE).position, STORE(WORLD[n1].BASE_NODE).position
  local lanes
  for _, t in ipairs(TEMPLATES) do if t.name == tmpl then lanes = t.lanes end end
  local c = D("BaseEdge", { node0 = n0, node1 = n1, position0 = a, position1 = b, roadTemplate = tmpl, type = 0,
    tangent0 = V(b.x - a.x, b.y - a.y, b.z - a.z), tangent1 = V(b.x - a.x, b.y - a.y, b.z - a.z), laneConfigs = lanes })
  for k, v in pairs(extra or {}) do STORE(c)[k] = COPY(v) end
  WORLD[id] = { BASE_EDGE = c, KIND = kind or 0 }
end
function ADD_OBJECT(id, edge, model, param, kind)
  WORLD[id] = { EDGE_OBJECT = D("EdgeObjectComp", { edgeObjectConstruction = model, param = param }), ON = edge }
  local c = WORLD[edge].BASE_EDGE
  local objs = STORE(c).objects
  objs[#objs + 1] = { id, kind }
end

-- the savegame: a road 100-101-102 along y = 0, a track 120-121 along y = 50 with a signal on it
function SAVE_WORLD()
  ADD_NODE(100, 0, 0, 0); ADD_NODE(101, 100, 0, 0); ADD_NODE(102, 200, 0, 0)
  ADD_EDGE(110, 100, 101, 0, "street/new_medium_road.street_template")
  ADD_EDGE(111, 101, 102, 0, "street/new_medium_road.street_template")
  ADD_NODE(120, 0, 50, 0); ADD_NODE(121, 100, 50, 0)
  ADD_EDGE(130, 120, 121, 1, "track/standard.street_template")
  ADD_OBJECT(140, 130, "signal/signal_a.mdl", 0.25, 2)
  -- the lockstep harness's two constructions get full transforms (the engine's Mat4f always has 16 entries)
  WORLD[54].CONSTRUCTION = CON_COMP("depot.con", 100, 200, 5, { depots = { 55 } })
  WORLD[56].CONSTRUCTION = CON_COMP("station.con", -40.5, 12, 0)
end

function CON_COMP(file, x, y, z, extra)
  local m = D("Mat4f")
  local s = STORE(m)
  s[13], s[14], s[15] = x, y, z
  return D("ConstructionComp", { fileName = file, transf = m, depots = extra and extra.depots or {}, stations = extra and extra.stations or {} })
end

local function EDGES_AT(n)
  local out = {}
  for e, w in pairs(WORLD) do
    if w.BASE_EDGE then
      local c = STORE(w.BASE_EDGE)
      if c.node0 == n or c.node1 == n then out[#out + 1] = e end
    end
  end
  table.sort(out)
  return out
end
_G.EDGES_AT = EDGES_AT

-- ===== the engine's answers =====
OCTREE = "node"          -- which component the octree answers for besides CONSTRUCTION: "node", "edge" or "none"
EDGE_REACH_SEEN = nil

local function pos3(d) local s = STORE(d); return s.x, s.y, s.z end

local BASE_MAKE_API = MAKE_API
function MAKE_API()
  local api = BASE_MAKE_API()
  local CT = api.type.ComponentType
  local NAMES = { [9] = "BASE_NODE", [10] = "BASE_EDGE", [11] = "EDGE_OBJECT", [12] = "STATION", [13] = "STATION_GROUP", [14] = "BASE_NODE_CONFIG" }
  for id, n in pairs(NAMES) do CT[n] = id end

  local T = api.type
  local function ctor(name, typeName) return { new = function(...) return D(typeName or name) end } end
  T.Vec3f = { new = function(x, y, z) if x ~= nil and y == nil then y, z = x, x end return D("Vec3f", { x = x or 0, y = y or 0, z = z or 0 }) end }
  T.Vec2f = { new = function(x, y) return D("Vec2f", { x = x or 0, y = y or 0 }) end }
  T.Vec4f = { new = function(x, y, z, w) return D("Vec4f", { x = x, y = y, z = z, w = w }) end }
  T.Mat4f = { new = function(c1, c2, c3, c4)
    local m = D("Mat4f")
    local s = STORE(m)
    local i = 0
    for _, col in ipairs({ c1, c2, c3, c4 }) do
      local cs = STORE(col)
      for _, k in ipairs({ "x", "y", "z", "w" }) do i = i + 1; s[i] = cs[k] end
    end
    return m
  end }
  T.NodeAndEntity = ctor("NodeAndEntity")
  T.SegmentAndEntity = ctor("SegmentAndEntity")
  T.BaseEdgeStreet = ctor("BaseEdgeStreet")
  T.BaseEdgeTrack = ctor("BaseEdgeTrack")
  T.PlayerOwned = ctor("PlayerOwned")
  T.LaneConfig = ctor("LaneConfig")
  T.LaneConnection = ctor("LaneConnection")
  T.Context = ctor("Context")
  T.GridVec2f = { new = function(x0, y0, w, h) return D("GridVec2f", { x0 = x0, y0 = y0, width = w, height = h }) end }
  T.BaseNodeLaneConnectionAndEntity = ctor("NodeConfigAndEntity")
  T.SimpleProposal = { new = function() return D("SimpleProposal") end, ConstructionEntity = ctor("ConstructionEntity") }
  T.SimpleStreetProposal = { EdgeObject = ctor("EdgeObjectSimple") }
  T.Proposal = { new = function() return D("Proposal") end }

  local E = api.engine
  local baseGet, baseEach = E.getComponent, E.forEachEntityWithComponent
  E.getComponent = function(e, t)
    local name = NAMES[t]
    if name then
      local w = WORLD[e]
      local c = w and w[name]
      if c == nil then return nil end
      return COPY(c)
    end
    if t == CT.CONSTRUCTION then
      local w = WORLD[e]
      local c = w and w.CONSTRUCTION
      if c == nil then return nil end
      return COPY(c)
    end
    return baseGet(e, t)
  end
  E.forEachEntityWithComponent = function(fn, t)
    local name = NAMES[t]
    if name == "BASE_NODE" or name == "BASE_EDGE" then error("Cannot loop over this component type") end
    if name and HIDE_ENUM and HIDE_ENUM[name] then return end   -- a component the engine does not list (sweep 2026-10-13, EDGE_OBJECT)
    if name then
      local ids = {}
      for e, w in pairs(WORLD) do if w[name] then ids[#ids + 1] = e end end
      table.sort(ids)
      for _, e in ipairs(ids) do fn(e) end
      return
    end
    return baseEach(fn, t)
  end
  E.system.streetSystem = {
    getNodeStreetSegments = function(n)
      local out = {}
      for _, e in ipairs(EDGES_AT(n)) do if WORLD[e].KIND == 0 then out[#out + 1] = e end end
      return out
    end,
    getNodeTrackSegments = function(n)
      local out = {}
      for _, e in ipairs(EDGES_AT(n)) do if WORLD[e].KIND == 1 then out[#out + 1] = e end end
      return out
    end,
    getNode2SegmentMap = function()
      local m = {}
      for e, w in pairs(WORLD) do
        if w.BASE_NODE then m[e] = EDGES_AT(e) end
      end
      return m
    end,
    getEdgeForEdgeObject = function(o) local w = WORLD[o]; return w and w.ON or -1 end,
  }
  E.system.stationGroupSystem = { getStationGroup = function(st) local w = WORLD[st]; return w and w.GROUP or -1 end }
  E.util.octree = {
    findEntitiesInCircle = function(p, r, t)
      if (t == CT.BASE_NODE or t == CT.BASE_EDGE) and OCTREE == "none" then error("octree: component not indexed") end
      local out = {}
      local px, py = pos3(p)
      local name = NAMES[t] or (t == CT.CONSTRUCTION and "CONSTRUCTION") or nil
      if not name then return out end
      if name == "CONSTRUCTION" and OCTREE_CONS_EMPTY then return out end
      if name == "BASE_NODE" and OCTREE ~= "node" then return out end
      for e, w in pairs(WORLD) do
        local c = w[name]
        if c then
          local x, y
          if name == "BASE_NODE" then local q = STORE(c).position; x, y = STORE(q).x, STORE(q).y
          elseif name == "BASE_EDGE" then
            local s = STORE(c)
            local a, b = STORE(WORLD[s.node0].BASE_NODE).position, STORE(WORLD[s.node1].BASE_NODE).position
            x, y = (STORE(a).x + STORE(b).x) / 2, (STORE(a).y + STORE(b).y) / 2
          elseif name == "CONSTRUCTION" then
            local m = type(c) == "table" and getmetatable(c) and STORE(c).transf or c.transf
            local tr = type(m) == "table" and getmetatable(m) and STORE(m) or m
            x, y = tr[13], tr[14]
          end
          if x and (x - px) ^ 2 + (y - py) ^ 2 <= r * r then out[#out + 1] = e end
        end
      end
      table.sort(out)
      return out
    end,
  }
  api.res = {
    streetTemplateRep = {
      find = function(name) for i, t in ipairs(TEMPLATES) do if t.name == name then return i end end return -1 end,
      get = function(i) local t = TEMPLATES[i]; return t and { laneConfigs = COPY(t.lanes), streetStyle = t.style } or nil end,
    },
  }
  HELPER_CALLS = HELPER_CALLS or {}
  E.util.proposal = {
    -- the engine reads proposal data of a SimpleProposal only (real session 2026-10-13); off unless a test switches it on
    makeProposalData = function(p, ctx)
      if SCRIPT_PIECES then   -- a test lets the construction's script add these street pieces (list of { p0, p1 })
        local segs = {}
        for i, pc in ipairs(SCRIPT_PIECES(p)) do segs[i] = { comp = { position0 = pc.p0, position1 = pc.p1 } } end
        return { errorState = { critical = false, messages = {}, warnings = {} }, proposal = { proposal = { addedNodes = {}, addedSegments = segs } } }
      end
      if not VERDICT_MOCK then error("not available in this mock") end
      local mt = getmetatable(p)
      if mt and mt.__double == "Proposal" then error("bad argument #2 to 'makeProposalData' (SimpleProposal expected, got Proposal)") end
      local crit = VERDICT_CRITICAL and VERDICT_CRITICAL(p) or false
      return { errorState = { critical = crit, messages = crit and { 'Construction Not Possible' } or {}, warnings = {} } }
    end,
    -- the engine's own upgrade: the segment replaced by one with the template's lanes (finding from MPFever (MIT))
    replaceSegment = function(edge, tmpl)
      HELPER_CALLS[#HELPER_CALLS + 1] = "replaceSegment " .. tostring(edge)
      local w = WORLD[edge]
      if not (w and w.BASE_EDGE) then return nil end
      local old = COPY(w.BASE_EDGE)
      local P = D("Proposal")
      local sp = P.proposal
      local rm = D("SegmentAndEntity", { entity = edge, comp = old })
      local c = COPY(old)
      local cs = STORE(c)
      cs.roadTemplate = tmpl or cs.roadTemplate
      for _, t in ipairs(TEMPLATES) do if t.name == cs.roadTemplate then cs.laneConfigs = COPY(t.lanes) end end
      local add = D("SegmentAndEntity", { entity = -1, comp = c, type = w.KIND })
      sp.removedSegments = { rm }
      sp.addedSegments = { add }
      P.proposal = sp
      return P
    end,
    makeSegmentsRemoveProposal = function(list)
      HELPER_CALLS[#HELPER_CALLS + 1] = "makeSegmentsRemoveProposal " .. table.concat(list, ",")
      local P = D("Proposal")
      local sp = P.proposal
      local rms = {}
      for i, e in ipairs(list) do
        local w = WORLD[e]
        if not (w and w.BASE_EDGE) then return nil end
        rms[i] = D("SegmentAndEntity", { entity = e, comp = COPY(w.BASE_EDGE), type = w.KIND })
      end
      sp.removedSegments = rms
      P.proposal = sp
      return P
    end,
  }
  return api
end
'''

BUILD_MOCK3 = r'''
-- after the harness: constructions that appear one step late, and a town building that grows in the same step
local BASE_SIMSTEP = SIMSTEP
function SIMSTEP()
  if GAME.deferredBuilds then
    local list = GAME.deferredBuilds
    GAME.deferredBuilds = nil
    for _, f in ipairs(list) do f() end
  end
  if GAME.deferredCons then
    local list = GAME.deferredCons
    GAME.deferredCons = nil
    for _, f in ipairs(list) do f() end
  end
  if GROW_TOWN then
    GROW_TOWN = nil
    NEW_ENTITY({ CONSTRUCTION = CON_COMP("town/house.con", 5000, 5000, 0) })
  end
  BASE_SIMSTEP()
end
'''

BUILD_MOCK2 = r'''
-- ===== applying a proposal =====
BUILD_LOG, BUILD_REFUSALS, FACTORY_COUNT, HELPER_CALLS, CFG_LOG, BUILD_CTX = {}, {}, 0, {}, {}, {}
OLD2NEW_LOG, CON_OWNERS = {}, {}
REUSE_FREED_IDS, FREED_IDS = false, {}   -- a test sets REUSE_FREED_IDS: the engine gives a removed building's id to the construction that replaces it
BUILD_REFUSE = nil      -- a test sets function(parts) -> message to make the engine refuse a build

DEF_TEMPLATE("street/simple.street_template", "simple", { { forward = true, modes = { 2 } }, { forward = false, modes = { 2 } } })

function PARTS(p)
  local t = TYPEOF(p)
  local s = STORE(p)
  local P = {}
  if t == "SimpleProposal" then
    local ssp = STORE(s.streetProposal)
    P.consAdd, P.consRem, P.old2new = s.constructionsToAdd, s.constructionsToRemove, s.old2new
    P.nodesAdd, P.edgesAdd, P.nodesRem, P.edgesRem = ssp.nodesToAdd, ssp.edgesToAdd, ssp.nodesToRemove, ssp.edgesToRemove
    P.objsAdd, P.objsRem, P.objEdgeKey = ssp.edgeObjectsToAdd, ssp.edgeObjectsToRemove, "edgeEntity"
    P.cfgAdd, P.cfgRem = ssp.nodeConfigsToAdd, ssp.nodeConfigsToRemove
  else
    local sp = STORE(s.proposal)
    P.consAdd, P.consRem, P.old2new = s.toAdd, s.toRemove, s.old2new
    P.nodesAdd, P.edgesAdd = sp.addedNodes, sp.addedSegments
    P.nodesRem, P.edgesRem = {}, {}
    for _, n in ipairs(sp.removedNodes) do P.nodesRem[#P.nodesRem + 1] = n.entity end
    for _, e in ipairs(sp.removedSegments) do P.edgesRem[#P.edgesRem + 1] = e.entity end
    P.objsAdd, P.objsRem, P.objEdgeKey = sp.edgeObjectsToAdd, sp.edgeObjectsToRemove, "segmentEntity"
    P.cfgAdd, P.cfgRem = sp.nodeConfigsToAdd, sp.nodeConfigsToRemove
    P.terrain = STORE(STORE(s.terrain).baseHeightMod)
  end
  return P
end

function SUMMARY(p)
  local P = PARTS(p)
  return string.format("cons=%d rm=%d nodes=%d segs=%d rmsegs=%d objs=%d", #P.consAdd, #P.consRem, #P.nodesAdd, #P.edgesAdd, #P.edgesRem, #P.objsAdd)
end

local function worldPos(id)
  local v = STORE(STORE(WORLD[id].BASE_NODE).position)
  return v.x, v.y, v.z
end

function APPLY_BUILD(p)
  local P = PARTS(p)
  if BUILD_REFUSE then
    local why = BUILD_REFUSE(P)
    if why then return false, why end
  end
  local function exists(id, comp) return WORLD[id] ~= nil and WORLD[id][comp] ~= nil end
  for _, e in ipairs(P.edgesRem) do if not exists(e, "BASE_EDGE") then return false, "segment " .. tostring(e) .. " does not exist" end end
  for _, e in ipairs(P.nodesRem) do if not exists(e, "BASE_NODE") then return false, "node " .. tostring(e) .. " does not exist" end end
  for _, e in ipairs(P.consRem) do if not exists(e, "CONSTRUCTION") then return false, "construction " .. tostring(e) .. " does not exist" end end
  for _, e in ipairs(P.objsRem) do if not exists(e, "EDGE_OBJECT") then return false, "edge object " .. tostring(e) .. " does not exist" end end
  -- the engine does not refuse a removal of a node configuration the node does not have, it aborts (real game: Engine.h:323
  -- ecs::Engine::GetComponentDataIndex "it != components.end()", Component Type Index 30 = BaseNodeConfig, entity unknown)
  for _, e in ipairs(P.cfgRem) do
    if not exists(e, "BASE_NODE_CONFIG") then error("ENGINE ASSERT Engine.h:323 GetComponentDataIndex: Assertion 'it != components.end()' failed (BaseNodeConfig, entity " .. tostring(e) .. ")", 0) end
  end
  -- a node configuration whose lane connection names a segment the proposal removes is a lookup the engine fails ("Unknown exception")
  do
    local goneE = {}
    for _, e in ipairs(P.edgesRem) do goneE[e] = true end
    for _, n in ipairs(P.cfgAdd) do
      for _, l in ipairs(n.comp.laneConnections) do
        if goneE[l.segment0] or goneE[l.segment1] then return false, "lane connection refers to the removed segment " .. tostring(goneE[l.segment0] and l.segment0 or l.segment1) end
      end
    end
  end
  local placeholders = {}
  for _, n in ipairs(P.nodesAdd) do
    local id = n.entity
    if id >= 0 then
      if not exists(id, "BASE_NODE") then return false, "node " .. id .. " does not exist" end
    else placeholders[id] = true end
  end
  local removedNode = {}
  for _, e in ipairs(P.nodesRem) do removedNode[e] = true end
  for _, s in ipairs(P.edgesAdd) do
    local c = s.comp
    for _, nid in ipairs({ c.node0, c.node1 }) do
      if nid < 0 then
        if not placeholders[nid] then return false, "segment refers to the unknown placeholder " .. nid end
      elseif not exists(nid, "BASE_NODE") or removedNode[nid] then
        return false, "segment refers to node " .. nid .. " that does not exist"
      end
    end
    for _, o in ipairs(c.objects) do
      if o[1] >= 0 and not exists(o[1], "EDGE_OBJECT") then return false, "segment keeps object " .. o[1] .. " that does not exist" end
    end
  end
  for _, o in ipairs(P.objsAdd) do
    local model = STORE(o)._model or o.model
    if model == nil or model == "" then return false, "edge object without model" end
  end

  -- a new node that joins three or more of the added segments and has no node configuration entry aborts the engine (the sweep harness's
  -- A3 probe, 2026-10-13: Engine.h:323, BaseNodeConfig on entity 49832 "being added")
  do
    local joins, hasCfg = {}, {}
    for _, n in ipairs(P.cfgAdd) do hasCfg[n.entity] = true end
    for _, s in ipairs(P.edgesAdd) do
      for _, nid in ipairs({ s.comp.node0, s.comp.node1 }) do
        if nid < 0 then joins[nid] = (joins[nid] or 0) + 1 end
      end
    end
    for nid, k in pairs(joins) do
      if k >= 3 and not hasCfg[nid] then error("ENGINE ASSERT Engine.h:323 GetComponentDataIndex: Assertion 'it != components.end()' failed (BaseNodeConfig, new node " .. nid .. " joins " .. k .. " segments)", 0) end
    end
  end

  local gone = {}
  for old, new in pairs(P.old2new) do OLD2NEW_LOG[#OLD2NEW_LOG + 1] = tostring(old) .. "=" .. tostring(new) end
  for _, e in ipairs(P.consRem) do
    FREED_IDS[#FREED_IDS + 1] = e
    local c = STORE(WORLD[e].CONSTRUCTION)
    for _, d in ipairs(c.depots) do WORLD[d] = nil end
    for _, s in ipairs(c.stations) do
      local g = WORLD[s] and WORLD[s].GROUP
      if g then WORLD[g] = nil end
      WORLD[s] = nil
    end
    WORLD[e] = nil
  end
  for _, e in ipairs(P.edgesRem) do
    for _, o in ipairs(STORE(WORLD[e].BASE_EDGE).objects) do gone[o[1]] = true end
    WORLD[e] = nil
  end
  for _, e in ipairs(P.nodesRem) do WORLD[e] = nil end
  for _, o in ipairs(P.objsRem) do gone[o] = true end

  local created, nodeMap, edgeMap, newEdges = {}, {}, {}, {}
  for _, n in ipairs(P.nodesAdd) do
    if n.entity < 0 then nodeMap[n.entity] = NEW_ENTITY({ BASE_NODE = COPY(n.comp) }) end
  end
  for _, s in ipairs(P.edgesAdd) do
    local c = s.comp
    local cs = STORE(c)
    cs.node0 = nodeMap[cs.node0] or cs.node0
    cs.node1 = nodeMap[cs.node1] or cs.node1
    local x0, y0, z0 = worldPos(cs.node0)
    local x1, y1, z1 = worldPos(cs.node1)
    cs.position0, cs.position1 = V(x0, y0, z0), V(x1, y1, z1)
    local id = NEW_ENTITY({ BASE_EDGE = c, KIND = s.type })
    if s.entity < 0 then edgeMap[s.entity] = id end
    newEdges[#newEdges + 1] = id
  end
  local objIds = {}
  for i, o in ipairs(P.objsAdd) do
    local ref = o[P.objEdgeKey]
    local oid = NEW_ENTITY({ EDGE_OBJECT = D("EdgeObjectComp", { edgeObjectConstruction = STORE(o)._model or o.model, param = o.param }), ON = edgeMap[ref] or ref })
    objIds[i] = oid
    created[#created + 1] = oid
  end
  for _, id in ipairs(newEdges) do
    local cs = STORE(WORLD[id].BASE_EDGE)
    local list = {}
    for _, o in ipairs(cs.objects) do
      if o[1] <= -400000000 then
        local k = -400000000 - o[1] + 1
        if objIds[k] then
          list[#list + 1] = { objIds[k], o[2] }
          if o[2] ~= 2 then MAKE_STOP(objIds[k]) end
        end
      else
        list[#list + 1] = o
        gone[o[1]] = nil
        if WORLD[o[1]] then WORLD[o[1]].ON = id end
      end
    end
    cs.objects = list
  end
  for o in pairs(gone) do WORLD[o] = nil end
  for _, n in ipairs(P.cfgAdd) do
    local c = n.comp
    local lcs = {}
    for i, l in ipairs(c.laneConnections) do lcs[i] = l.segment0 .. ">" .. l.segment1 end
    CFG_LOG[#CFG_LOG + 1] = { node = nodeMap[n.entity] or n.entity, lanes = lcs, crosswalks = #c.crosswalks, dss = c.doubleSlipSwitch }
  end
  local function makeConstructions()
    for _, ce in ipairs(P.consAdd) do
      CON_OWNERS[#CON_OWNERS + 1] = tostring(ce.playerEntity)
      local id = (REUSE_FREED_IDS and #FREED_IDS > 0) and table.remove(FREED_IDS, 1) or NEW_ENTITY(nil)
      local comp = D("ConstructionComp", { fileName = ce.fileName, transf = ce.transf, params = ce.params, name = ce.name })
      local cs = STORE(comp)
      if ce.fileName:find("depot") then cs.depots = { NEW_ENTITY({ VEHICLE_DEPOT = {} }) } end
      if ce.fileName:find("station") then
        local st = NEW_ENTITY({ STATION = {} })
        WORLD[st].GROUP = NEW_ENTITY({ STATION_GROUP = {} })
        cs.stations = { st }
      end
      WORLD[id] = { CONSTRUCTION = comp }
      created[#created + 1] = id
    end
  end
  -- a test can have the engine create constructions one step after the command that built them (as it does vehicles)
  if DEFER_CONS and GAME.inUpdate and #P.consAdd > 0 then
    GAME.deferredCons = GAME.deferredCons or {}
    GAME.deferredCons[#GAME.deferredCons + 1] = makeConstructions
  else
    makeConstructions()
  end
  return true, created
end

local BASE_EXECUTE = EXECUTE
local executeBuild
function EXECUTE(cmd, cb)
  local name = cmd.__cmd
  if name == "makeWorldReplaceTerrainCmd" then
    GAME.applied[#GAME.applied + 1] = { name = name, time = GAME.time, args = cmd.args }
    if cb then cb({}, true, {}) end
    return
  end
  if name ~= "makeWorldBuildProposalCmd" then return BASE_EXECUTE(cmd, cb) end
  -- a test can have the engine run the builds of one update together after it, in the order they were sent (it does
  -- not run a command inside the update that sent it)
  if DEFER_BUILDS and GAME.inUpdate then
    GAME.deferredBuilds = GAME.deferredBuilds or {}
    GAME.deferredBuilds[#GAME.deferredBuilds + 1] = function() executeBuild(cmd, cb) end
    return
  end
  return executeBuild(cmd, cb)
end

CTX_REFUSE = nil        -- a test sets function(context) -> true for a Context the engine builds differently (it refuses the build here)
BUILD_NEEDS_IE = nil    -- a test sets function(proposal) -> true for a build the engine only accepts with ignoreErrors = true (non-critical messages)
function executeBuild(cmd, cb)
  local name = cmd.__cmd
  local a = cmd.args
  local ok, res
  if BUILD_NEEDS_IE and BUILD_NEEDS_IE(a[1]) and a[3] ~= true then ok, res = false, "non-critical messages and ignoreErrors = false"
  elseif CTX_REFUSE and a[2] and CTX_REFUSE(a[2]) then ok, res = false, "the engine refuses this Context"
  else ok, res = APPLY_BUILD(a[1]) end
  if ok then
    local ents = {}
    for i, e in ipairs(res) do ents[i] = { e, 0 } end
    GAME.applied[#GAME.applied + 1] = { name = name, time = GAME.time, args = a }
    local c = a[2]
    BUILD_CTX[#BUILD_CTX + 1] = c and (string.format("align=%s cleanup=%s buildings=%s fields=%s player=%s ie=%s pi=%s", tostring(c.checkTerrainAlignment),
      tostring(c.cleanupStreetGraph), tostring(c.gatherBuildings), tostring(c.gatherFields), tostring(c.player), tostring(a[3]), tostring(a[4])))
      or ("nil ie=" .. tostring(a[3]) .. " pi=" .. tostring(a[4]))
    BUILD_LOG[#BUILD_LOG + 1] = SUMMARY(a[1])
    if cb then cb({ resultEntities = ents, resultProposalData = { errorState = { messages = {} } } }, true, ents) end
  else
    BUILD_REFUSALS[#BUILD_REFUSALS + 1] = res
    if cb then cb({ resultProposalData = { errorState = { messages = { res } } } }, false, {}) end
  end
end

local BASE_MAKE_API2 = MAKE_API
function MAKE_API()
  local api = BASE_MAKE_API2()
  api.cmd.makeWorldBuildProposalCmd = function(...)
    local a = { n = select("#", ...), ... }
    local p = a[1]
    if TYPEOF(p) ~= "SimpleProposal" and TYPEOF(p) ~= "Proposal" then error("makeWorldBuildProposalCmd: argument 1 is not a proposal") end
    for _, s in ipairs(PARTS(p).edgesAdd) do
      if #s.comp.laneConfigs == 0 then error("Unknown exception") end   -- finding from MPFever (MIT)
    end
    if FACTORY_REJECT then
      -- (a test makes the factory strict about what the mod rebuilds: the proposal kind tells a rebuild from the original)
      local why = FACTORY_REJECT(PARTS(p), TYPEOF(p))
      if why then error(why) end
    end
    FACTORY_COUNT = FACTORY_COUNT + 1
    return { __cmd = "makeWorldBuildProposalCmd", args = a }
  end
  api.cmd.makeWorldReplaceTerrainCmd = function(...) return { __cmd = "makeWorldReplaceTerrainCmd", args = { n = select("#", ...), ... } } end
  return api
end

-- ===== proposals as scripts and tools make them =====
function NODE_POS(id) local x, y, z = worldPos(id); return { x = x, y = y, z = z } end
local function nodeAndEntity(id, p) return D("NodeAndEntity", { entity = id, comp = D("BaseNode", { position = V(p.x, p.y, p.z or 0) }) }) end

local function segment(entity, n0, n1, pa, pb, opts)
  opts = opts or {}
  local tmpl = opts.template or "street/new_medium_road.street_template"
  local lanes, style = TEMPLATE_LANES(tmpl)
  local d = V(pb.x - pa.x, pb.y - pa.y, (pb.z or 0) - (pa.z or 0))
  local c = D("BaseEdge", { node0 = n0, node1 = n1, tangent0 = d, tangent1 = d, position0 = V(pa.x, pa.y, pa.z or 0), position1 = V(pb.x, pb.y, pb.z or 0),
    type = 0, typeIndex = 1, roadTemplate = tmpl, roadStyle = style, roadType = opts.track and 1 or 0, laneConfigs = lanes,
    objects = opts.objects or {}, edgeDecorations = opts.deco or {} })
  local se = D("SegmentAndEntity", { entity = entity, type = opts.track and 1 or 0, comp = c })
  if opts.trackType then
    local te = se.trackEdge
    te.trackType = opts.trackType
    te.catenary = opts.catenary == true
    se.trackEdge = te
  end
  return se
end

-- a road between two places (or existing nodes), the way a script builds it
function SIMPLE_ROAD(spec)
  local pa = spec.nodeA and NODE_POS(spec.nodeA) or spec.a
  local pb = spec.nodeB and NODE_POS(spec.nodeB) or spec.b
  local nodes, n0, n1 = {}, spec.nodeA, spec.nodeB
  if not n0 then n0 = -1; nodes[#nodes + 1] = nodeAndEntity(-1, pa) end
  if not n1 then n1 = -2; nodes[#nodes + 1] = nodeAndEntity(-2, pb) end
  local sp = D("SimpleProposal")
  local ssp = sp.streetProposal
  ssp.nodesToAdd = nodes
  ssp.edgesToAdd = { segment(-1, n0, n1, pa, pb, spec) }
  sp.streetProposal = ssp
  return sp
end

-- the same road as a tool's proposal: one numbering shared by nodes and segments, a native Proposal
function NATIVE_ROAD(spec)
  local pa = spec.nodeA and NODE_POS(spec.nodeA) or spec.a
  local pb = spec.nodeB and NODE_POS(spec.nodeB) or spec.b
  local nodes, n0, n1 = {}, spec.nodeA, spec.nodeB
  if not n0 then n0 = -1; nodes[#nodes + 1] = nodeAndEntity(-1, pa) end
  if not n1 then n1 = -2; nodes[#nodes + 1] = nodeAndEntity(-2, pb) end
  local P = D("Proposal")
  local sp = P.proposal
  sp.addedNodes = nodes
  sp.addedSegments = { segment(-3, n0, n1, pa, pb, spec) }
  P.proposal = sp
  return P
end

-- a junction on an existing segment with a branch road leaving it
function NATIVE_SPLIT(spec)
  local w = WORLD[spec.edge]
  local c = STORE(w.BASE_EDGE)
  local na, nb = c.node0, c.node1
  local pa, pb = NODE_POS(na), NODE_POS(nb)
  local P = D("Proposal")
  local sp = P.proposal
  sp.removedSegments = { D("SegmentAndEntity", { entity = spec.edge, comp = COPY(w.BASE_EDGE), type = w.KIND }) }
  sp.addedNodes = { nodeAndEntity(-1, spec.at), nodeAndEntity(-2, spec.to) }
  sp.addedSegments = { segment(-3, na, -1, pa, spec.at, spec), segment(-4, -1, nb, spec.at, pb, spec), segment(-5, -1, -2, spec.at, spec.to, spec) }
  -- the real tool always carries an entry for the new junction node (pref 2, no lane connections); the engine aborts without it
  sp.nodeConfigsToAdd = { D("NodeConfigAndEntity", { entity = -1, comp = D("BaseNodeConfig", { laneConnections = {} }) }) }
  P.proposal = sp
  return P
end

-- segments replaced by the same segments with another template (and decorations)
function NATIVE_UPGRADE(spec)
  local P = D("Proposal")
  local sp = P.proposal
  local rm, add = {}, {}
  for i, e in ipairs(spec.edges) do
    local w = WORLD[e]
    local c = COPY(w.BASE_EDGE)
    local cs = STORE(c)
    rm[i] = D("SegmentAndEntity", { entity = e, comp = COPY(w.BASE_EDGE), type = w.KIND })
    cs.roadTemplate = spec.template
    local lanes, style = TEMPLATE_LANES(spec.template)
    cs.laneConfigs, cs.roadStyle = lanes, style
    if spec.deco then cs.edgeDecorations = spec.deco end
    add[i] = D("SegmentAndEntity", { entity = -i, comp = c, type = w.KIND })
  end
  sp.removedSegments, sp.addedSegments = rm, add
  P.proposal = sp
  return P
end

function NATIVE_BULLDOZE(spec)
  local P = D("Proposal")
  local sp = P.proposal
  local rm = {}
  for i, e in ipairs(spec.edges or {}) do
    rm[i] = D("SegmentAndEntity", { entity = e, comp = COPY(WORLD[e].BASE_EDGE), type = WORLD[e].KIND })
  end
  sp.removedSegments = rm
  P.proposal = sp
  P.toRemove = spec.cons or {}
  return P
end

-- a signal on a segment: the segment replaced by one with one more object, the object without a model (it has one
-- only once the engine has built it)
function NATIVE_SIGNAL(spec)
  local w = WORLD[spec.edge]
  local c = COPY(w.BASE_EDGE)
  local cs = STORE(c)
  local kind = spec.kind or 2        -- 0 a stop on the left, 1 on the right, 2 a signal
  local objs = {}
  for i, o in ipairs(cs.objects) do objs[i] = { o[1], o[2] } end
  objs[#objs + 1] = { -400000000, kind }
  cs.objects = objs
  local P = D("Proposal")
  local sp = P.proposal
  sp.removedSegments = { D("SegmentAndEntity", { entity = spec.edge, comp = COPY(w.BASE_EDGE), type = w.KIND }) }
  sp.addedSegments = { D("SegmentAndEntity", { entity = -1, comp = c, type = w.KIND }) }
  local eo = D("EdgeObjectNative", { segmentEntity = -1, param = spec.param, category = kind, left = spec.left == true })
  STORE(eo)._model = spec.model or "signal/signal_a.mdl"
  sp.edgeObjectsToAdd = { eo }
  P.proposal = sp
  return P
end

-- The proposal as it reads after the build when the removed segment is listed by a negative id (the real session
-- 2026-10-13-a: -39433): still as a record with its comp (negative = true), or as a bare number ('bare').
function NEGATE_REMOVED(P, how)
  local sp = P.proposal
  local r = sp.removedSegments[1]
  if how == 'bare' then
    sp.removedSegments = { -r.entity - 1 }
  else
    sp.removedSegments = { D("SegmentAndEntity", { entity = -r.entity - 1, comp = r.comp, type = r.type }) }
  end
  P.proposal = sp
end

-- a stop the way a script places one: the segment replaced by itself with one more object, whose model it names
function SIMPLE_STOP(spec)
  local w = WORLD[spec.edge]
  local c = COPY(w.BASE_EDGE)
  local cs = STORE(c)
  local objs = {}
  for i, o in ipairs(cs.objects) do objs[i] = { o[1], o[2] } end
  objs[#objs + 1] = { -400000000, spec.kind or (spec.left and 0 or 1) }
  cs.objects = objs
  local sp = D("SimpleProposal")
  local ssp = sp.streetProposal
  ssp.edgesToRemove = { spec.edge }
  ssp.edgesToAdd = { D("SegmentAndEntity", { entity = -1, comp = c, type = w.KIND }) }
  ssp.edgeObjectsToAdd = { D("EdgeObjectSimple", { edgeEntity = -1, param = spec.param, left = spec.left == true, model = spec.model or "station/street/bus_stop.mdl" }) }
  sp.streetProposal = ssp
  return sp
end

-- a signal (or stop) bulldozed: the segment replaced by itself without the object, the object listed for removal
function NATIVE_UNSIGNAL(spec)
  local w = WORLD[spec.edge]
  local c = COPY(w.BASE_EDGE)
  local cs = STORE(c)
  local kept = {}
  for _, o in ipairs(cs.objects) do if o[1] ~= spec.object then kept[#kept + 1] = { o[1], o[2] } end end
  cs.objects = kept
  local P = D("Proposal")
  local sp = P.proposal
  sp.removedSegments = { D("SegmentAndEntity", { entity = spec.edge, comp = COPY(w.BASE_EDGE), type = w.KIND }) }
  sp.addedSegments = { D("SegmentAndEntity", { entity = -1, comp = c, type = w.KIND }) }
  sp.edgeObjectsToRemove = { spec.object }
  P.proposal = sp
  return P
end

local function conEntity(spec)
  local ce = D("ConstructionEntity", { fileName = spec.file, params = spec.params or {}, name = spec.name or "", playerEntity = spec.owner or 7 })
  local m = STORE(STORE(ce).transf)
  m[13], m[14], m[15] = spec.x, spec.y, spec.z or 0
  return ce
end

-- a tool's road that takes a town building out of its way and puts it elsewhere (the building is nobody's: owner -1)
function NATIVE_ROAD_MOVING(spec)
  local P = NATIVE_ROAD(spec)
  P.toAdd = { conEntity({ file = spec.moveFile, x = spec.moveTo.x, y = spec.moveTo.y, z = 0, owner = -1 }) }
  P.toRemove = { spec.remove }
  return P
end

-- a construction (and optionally the removal of one it replaces), as the game's own mission scripts build it
function CON_PROPOSAL(spec)
  local sp = D("SimpleProposal")
  sp.constructionsToAdd = { conEntity(spec) }
  if spec.remove then
    sp.constructionsToRemove = { spec.remove }
    sp.old2new = { [spec.remove] = 0 }
  end
  return sp
end

function CON_REMOVE(spec)
  local sp = D("SimpleProposal")
  sp.constructionsToRemove = spec.cons
  return sp
end

-- a depot against a road, as the tool hands it over: the road split at the junction, the depot's entrance segment from the
-- junction to its door, the depot itself
function NATIVE_DEPOT_ON_ROAD(spec)
  local P = NATIVE_SPLIT({ edge = spec.edge, at = spec.at, to = spec.door, template = spec.template })
  local sp = P.proposal
  local added = sp.addedSegments
  added[3] = segment(-5, -1, -2, spec.at, spec.door, { template = "street/simple.street_template" })
  sp.addedSegments = added
  P.proposal = sp
  P.toAdd = { conEntity({ file = spec.file or "depot/road_depot.con", x = spec.door.x, y = spec.door.y, z = spec.door.z, params = spec.params }) }
  return P
end

-- a depot whose entrance reaches a node of the world (real run 2026-10-11, Dunwich: the entrance node at the door and one piece of the
-- construction's own street template from it to a town node): the tool's own build has both, the construction alone does not
function NATIVE_DEPOT_ENTRANCE(spec)
  local pa = NODE_POS(spec.node)
  local P = D("Proposal")
  local sp = P.proposal
  sp.addedNodes = { nodeAndEntity(-1, spec.door) }
  sp.addedSegments = { segment(-2, -1, spec.node, spec.door, pa, { template = "street/simple.street_template" }) }
  P.proposal = sp
  P.toAdd = { conEntity({ file = spec.file or "depot/road_depot.con", x = spec.door.x, y = spec.door.y, z = spec.door.z or 0, params = spec.params }) }
  return P
end

-- an airfield (real run 2026-10-11): the construction with its own taxiway pieces and signs on them; the signs' models are
-- named by the engine only after the build ran, so a tool's preview cannot read them
function NATIVE_AIRFIELD(spec)
  local P = D("Proposal")
  local sp = P.proposal
  sp.addedNodes = { nodeAndEntity(-1, spec.a), nodeAndEntity(-2, spec.b) }
  sp.addedSegments = { segment(-3, -1, -2, spec.a, spec.b, { template = "street/airport/taxiway.street_template" }) }
  local eo = D("EdgeObjectNative", { segmentEntity = -3, param = 0.5, category = 2 })
  STORE(eo)._model = "signal/sign.mdl"
  sp.edgeObjectsToAdd = { eo }
  P.proposal = sp
  P.toAdd = { conEntity({ file = "airport/airfield.con", x = spec.a.x, y = spec.a.y, z = 0, params = spec.params }) }
  return P
end

-- an underground station entrance (real session 2026-10-13): the construction with its own tunnel pieces, four new nodes and two segments of
-- an ordinary tram street template that touch no node of the world
function NATIVE_UNDERGROUND(spec)
  local P = D("Proposal")
  local sp = P.proposal
  local a, b, c, d = { x = spec.x + 30, y = spec.y + 8, z = -13 }, { x = spec.x + 20, y = spec.y + 5, z = -13 }, { x = spec.x - 30, y = spec.y - 8, z = -13 }, { x = spec.x - 20, y = spec.y - 5, z = -13 }
  sp.addedNodes = { nodeAndEntity(-1, a), nodeAndEntity(-2, b), nodeAndEntity(-3, c), nodeAndEntity(-4, d) }
  sp.addedSegments = { segment(-5, -1, -2, a, b, { template = spec.template or "street/new_small_road.street_template" }),
    segment(-6, -3, -4, c, d, { template = spec.template or "street/new_small_road.street_template" }) }
  P.proposal = sp
  P.toAdd = { conEntity({ file = "stations/street/underground_station/underground_station.con", x = spec.x, y = spec.y, z = 5 }) }
  return P
end

-- a node configuration (lane connections between placeholders or segments) added to a proposal that has none
function WITH_CFG(P, node, lanes)
  local sp = P.proposal
  local lcs = {}
  for j, l in ipairs(lanes) do lcs[j] = D("LaneConnection", { segment0 = l[1], lane0 = l[2], segment1 = l[3], lane1 = l[4], withRoad = true }) end
  sp.nodeConfigsToAdd = { D("NodeConfigAndEntity", { entity = node, comp = D("BaseNodeConfig", { laneConnections = lcs }) }) }
  P.proposal = sp
  return P
end

-- lane connections and crosswalks of nodes: spec.nodes = { { node = id, lanes = { { seg0, lane0, seg1, lane1 } }, crosswalks = { seg } } }
function NATIVE_NODECFG(spec)
  local P = D("Proposal")
  local sp = P.proposal
  local cfgs = {}
  for i, n in ipairs(spec.nodes) do
    local lcs = {}
    for j, l in ipairs(n.lanes or {}) do lcs[j] = D("LaneConnection", { segment0 = l[1], lane0 = l[2], segment1 = l[3], lane1 = l[4], withRoad = true }) end
    local comp = D("BaseNodeConfig", { laneConnections = lcs, crosswalks = n.crosswalks or {}, trafficLightPreference = n.pref or 0, doubleSlipSwitch = n.dss == true })
    cfgs[i] = D("NodeConfigAndEntity", { entity = n.node, comp = comp })
  end
  sp.nodeConfigsToAdd = cfgs
  P.proposal = sp
  return P
end

function CONTEXT(align, cleanup, buildings, fields, redo)
  return D("Context", { player = 7, checkTerrainAlignment = align, cleanupStreetGraph = cleanup, gatherBuildings = buildings, gatherFields = fields, extendProposalRedoPillars = redo == true })
end

-- what the module makes of a proposal that a tool applied natively (the body for an action line), as a string
function CAPTURE_BODY(P, created)
  local M, env = LOAD_BUILD()
  local C = env.ug_require("feverlink3_common.lua")
  local body, why = M.capture(P, { result = created })
  if not body then return nil, why end
  return C.serialize(body)
end

function NATIVE_TERRAIN(x0, y0, w, h)
  local P = D("Proposal")
  local t = P.terrain
  t.baseHeightMod = D("GridVec2f", { x0 = x0, y0 = y0, width = w, height = h })
  P.terrain = t
  return P
end

-- ===== looking at the world without ids =====
local function SER(v)
  if type(v) ~= "table" then return tostring(v) end
  local keys = {}
  for k in pairs(v) do keys[#keys + 1] = k end
  table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
  local parts = {}
  for _, k in ipairs(keys) do parts[#parts + 1] = tostring(k) .. "=" .. SER(v[k]) end
  return "{" .. table.concat(parts, ",") .. "}"
end

local function P3(v) return string.format("%.2f,%.2f,%.2f", v.x, v.y, v.z) end

function WORLD_SIG()
  local lines = {}
  for e, w in pairs(WORLD) do
    if w.BASE_NODE then
      lines[#lines + 1] = "N " .. P3(STORE(STORE(w.BASE_NODE).position))
    elseif w.BASE_EDGE then
      local c = STORE(w.BASE_EDGE)
      local pa = P3(STORE(STORE(WORLD[c.node0].BASE_NODE).position))
      local pb = P3(STORE(STORE(WORLD[c.node1].BASE_NODE).position))
      if pa > pb then pa, pb = pb, pa end
      local objs = {}
      for _, o in ipairs(c.objects) do
        local oc = STORE(WORLD[o[1]].EDGE_OBJECT)
        objs[#objs + 1] = oc.edgeObjectConstruction .. "@" .. string.format("%.3f", oc.param) .. "k" .. tostring(o[2])
      end
      table.sort(objs)
      local lanes = {}
      for i, l in ipairs(c.laneConfigs) do
        local ls = STORE(l)
        local m = {}
        for j = 0, 15 do if ls.transportModes[j] then m[#m + 1] = j end end
        lanes[i] = (ls.forward and "f" or "b") .. table.concat(m, ",")
      end
      lines[#lines + 1] = "E " .. pa .. " " .. pb .. " " .. c.roadTemplate .. " kind" .. tostring(w.KIND) .. " t" .. c.type .. "/" .. c.typeIndex .. " lanes[" .. table.concat(lanes, "|") .. "] ["
        .. table.concat(objs, ";") .. "] " .. SER(c.edgeDecorations)
    elseif w.CONSTRUCTION then
      local c = w.CONSTRUCTION
      local isDouble = getmetatable(c) ~= nil
      local m = isDouble and STORE(STORE(c).transf) or c.transf
      local params = isDouble and STORE(c).params or c.params
      local nd, ns = 0, 0
      if isDouble then nd, ns = #STORE(c).depots, #STORE(c).stations end
      lines[#lines + 1] = "C " .. (isDouble and STORE(c).fileName or c.fileName) .. "@" .. string.format("%.2f,%.2f,%.2f", m[13], m[14], m[15]) .. " " .. SER(params or {}) .. " d" .. nd .. "s" .. ns
    end
  end
  table.sort(lines)
  return table.concat(lines, "\n")
end

function FIND_NODE(x, y, z)
  for e, w in pairs(WORLD) do
    if w.BASE_NODE then
      local p = STORE(STORE(w.BASE_NODE).position)
      if math.abs(p.x - x) < 0.01 and math.abs(p.y - y) < 0.01 and math.abs(p.z - (z or 0)) < 0.01 then return e end
    end
  end
end

function FIND_EDGE(x0, y0, x1, y1)
  for e, w in pairs(WORLD) do
    if w.BASE_EDGE then
      local c = STORE(w.BASE_EDGE)
      local a, b = STORE(STORE(WORLD[c.node0].BASE_NODE).position), STORE(STORE(WORLD[c.node1].BASE_NODE).position)
      local function at(p, x, y) return math.abs(p.x - x) < 0.01 and math.abs(p.y - y) < 0.01 end
      if (at(a, x0, y0) and at(b, x1, y1)) or (at(a, x1, y1) and at(b, x0, y0)) then return e end
    end
  end
end

function FIND_CON(file)
  local best
  for e, w in pairs(WORLD) do
    local c = w.CONSTRUCTION
    if c and getmetatable(c) and STORE(c).fileName == file and (best == nil or e > best) then best = e end
  end
  return best
end

-- the newest depot the session built (the savegame's depot is 55)
function FIND_DEPOT()
  local best
  for e, w in pairs(WORLD) do
    if w.VEHICLE_DEPOT and e >= 1000 and (best == nil or e > best) then best = e end
  end
  return best
end

function BOUND_KEYS()
  local ks = {}
  for k in pairs((STATE:get() or {}).bind or {}) do ks[#ks + 1] = k end
  table.sort(ks)
  return table.concat(ks, " ")
end

-- the stop a key names: the ends of its segment, its model, the group it belongs to and whether that group is the key's
function STOP_AT(key)
  local bind = (STATE:get() or {}).bind or {}
  local e = bind[key]
  if not e or not WORLD[e] or not WORLD[e].EDGE_OBJECT then return "none" end
  local c = STORE(WORLD[WORLD[e].ON].BASE_EDGE)
  local a, b = P3(STORE(STORE(WORLD[c.node0].BASE_NODE).position)), P3(STORE(STORE(WORLD[c.node1].BASE_NODE).position))
  if a > b then a, b = b, a end
  local g = bind[key .. "/g"]
  return a .. " " .. b .. " " .. STORE(WORLD[e].EDGE_OBJECT).edgeObjectConstruction .. " group" .. tostring(g == WORLD[e].GROUP) .. " " .. tostring(g ~= nil)
end

function COUNT(comp)
  local n = 0
  for _, w in pairs(WORLD) do if w[comp] then n = n + 1 end end
  return n
end

-- the module under test, loaded alone (no bridge): returns the module and its environment. The environment is strict: a
-- global the module reads without defining raises (a typo in a branch no test reaches would otherwise read nil).
function LOAD_BUILD()
  local env = setmetatable({}, { __index = function(_, k)
    local v = _G[k]
    if v == nil and k ~= "FEVERLINK3_BUILD" and k ~= "FEVERLINK3_ENGINE" then error("undefined global " .. tostring(k), 2) end
    return v
  end, __newindex = function(t, k, v)
    if k ~= "api" and k ~= "package" and k ~= "ug_require" then error("global assigned: " .. tostring(k), 2) end
    rawset(t, k, v)
  end })
  env.api = MAKE_API()
  env.package = { loaded = {} }
  env.ug_require = function(p)
    if p:find("feverlink3_common") then return assert(load(COMMON_SRC, "=common", "t", env))() end
    if p:find("feverlink3_portable") then return assert(load(PORTABLE_SRC, "=portable", "t", env))() end
    local cmds = p:match("(feverlink3_cmds_[%w_]+)")
    if cmds and CMDS_SRC[cmds] then return assert(load(CMDS_SRC[cmds], "=" .. cmds, "t", env))() end
    error("no module " .. tostring(p))
  end
  local M = env.ug_require("feverlink3_cmds_build")
  return M, env
end
'''

# ---------------------------------------------------------------- the games

TAB = T.TAB


class BuildGame(T.Game):
    """A mocked game: the lockstep harness's engine with the construction mock on top."""

    def __init__(self, label, player_id, tmp, first_entity):
        self.label, self.id = label, player_id
        self.dir = os.path.join(tmp, label)
        os.makedirs(self.dir)
        self.to_game = os.path.join(self.dir, "to_game.log")
        self.from_game = os.path.join(self.dir, "from_game.log")
        self.use_env()
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        g = self.L.globals()
        g.COMMON_SRC, g.LOCKSTEP_SRC, g.BRIDGE_SRC, g.HUD_SRC = T.COMMON_SRC, T.LOCKSTEP_SRC, T.BRIDGE_SRC, T.HUD_SRC
        g.PORTABLE_SRC = T.PORTABLE_SRC
        cmds = self.L.table()
        for stem, src in T.CMDS_SRC.items():
            cmds[stem] = src
        g.CMDS_SRC = cmds
        self.L.execute(T.MOCK % dict(t0=T.T0, first=first_entity))
        self.L.execute(BUILD_MOCK)
        self.L.execute(BUILD_MOCK2)
        self.L.execute(T.HARNESS)
        self.L.execute(BUILD_MOCK3)
        self.L.execute("SAVE_WORLD()")

    def sig(self):
        return str(self.L.eval("WORLD_SIG()"))

    def lua(self, expr):
        return self.L.eval(expr)

    def run(self, statements):
        self.L.execute(statements)

    def issue(self, proposal, ctx="nil", ie="false", pi="true", cb="function() end"):
        """Send a build from the interface state, as a game window or script would."""
        self.send_gui("api.cmd.sendCommand(api.cmd.makeWorldBuildProposalCmd(%s, %s, %s, %s), %s)" % (proposal, ctx, ie, pi, cb))

    def shipped(self):
        """The body of every action this game sent, as the text of its serialised table."""
        out = []
        for l in self.lines(self.from_game):
            parts = l.split(TAB, 3)
            if parts[1] == "action":
                p = lupa.LuaRuntime(unpack_returned_tuples=True).execute(T.COMMON_SRC).parse(parts[3])
                p = p[0] if isinstance(p, tuple) else p
                out.append(str(p.body))
        return out


class BuildTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-")
        self.A = BuildGame("host", 0, self.tmp, 50000)
        self.B = BuildGame("client", 1, self.tmp, 51000)
        self.relay = T.Relay([self.A, self.B])

    def tearDown(self):
        if os.environ.get("FEVERLINK3_TEST_VERBOSE"):
            print("\n--- host mod.log ---\n" + self.A.log() + "\n--- client mod.log ---\n" + self.B.log())
        shutil.rmtree(self.tmp, ignore_errors=True)

    def tick(self, n=1, lag_b=False):
        for i in range(n):
            for _ in range(4):
                for g in (self.A, self.B):
                    g.frame()
                self.relay.pump()
            self.A.step()
            if not (lag_b and i % 3 == 0):
                self.B.step()

    def start(self, speed=1):
        self.tick(3)
        self.relay.started, self.relay.speed = True, speed
        self.relay.session()
        self.tick(3)
        self.tick(6)

    def both(self, expr):
        return self.A.lua(expr), self.B.lua(expr)

    def assertSameWorld(self, msg=None):
        self.assertEqual(self.A.sig(), self.B.sig(), msg or "both games hold the same world")

    # ---- roads and tracks

    def test_road_built_by_the_client_appears_identically_on_both_games(self):
        self.start()
        self.B.issue("SIMPLE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }", cb="function(d, s, e) ROAD_OK = s end")
        self.assertTrue(self.B.lua("ROAD_OK == nil"), "the window waits for the real result")
        self.tick(14)
        self.assertTrue(self.B.lua("ROAD_OK == true"))
        self.assertSameWorld()
        self.assertIn("E 300.00,0.00,0.00 400.00,0.00,0.00 street/new_medium_road.street_template", self.A.sig())
        na, nb = self.both("FIND_NODE(300, 0, 0)")
        self.assertNotEqual(na, nb, "the new node has another id in each game")
        shipped = self.B.shipped()
        self.assertEqual(len(shipped), 1)
        for ident in (na, nb, self.B.lua("FIND_EDGE(300, 0, 400, 0)"), self.A.lua("FIND_EDGE(300, 0, 400, 0)")):
            self.assertNotIn(str(int(ident)), shipped[0], "no session entity id on the wire")
        self.assertNotIn("userdata", shipped[0])
        self.assertEqual(self.A.lua("BUILD_LOG[1]"), self.B.lua("BUILD_LOG[1]"))

    def test_a_second_build_attaches_to_a_node_the_first_one_created(self):
        self.start()
        self.B.issue("SIMPLE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }")
        self.tick(14)
        nb = int(self.B.lua("FIND_NODE(400, 0, 0)"))
        na = int(self.A.lua("FIND_NODE(400, 0, 0)"))
        self.assertNotEqual(na, nb)
        self.B.issue("SIMPLE_ROAD{ nodeA = %d, b = { x = 500, y = 100, z = 0 } }" % nb)
        self.tick(14)
        self.assertSameWorld()
        self.assertIsNotNone(self.A.lua("FIND_EDGE(400, 0, 500, 100)"), "the second road starts at the first one's end node on the host too")
        self.assertEqual(self.A.lua("#EDGES_AT(FIND_NODE(400, 0, 0))"), 2, "one node, two roads")
        shipped = self.B.shipped()
        self.assertEqual(len(shipped), 2)
        self.assertIn('["__ref"]="node"', shipped[1])
        self.assertIn('["key"]="s:%d"' % nb, shipped[1], "the id only travels as a key, next to the place it must lie at")
        self.assertIn('["p"]={["x"]=400,["y"]=0,["z"]=0}', shipped[1])
        self.assertEqual(shipped[1].count(str(nb)), 1, "once, inside the key")

    def test_a_key_that_names_another_entity_is_not_trusted(self):
        self.start()
        self.B.issue("SIMPLE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }")
        self.tick(14)
        nb = int(self.B.lua("FIND_NODE(400, 0, 0)"))
        # on the host the same id means a node elsewhere: the key resolves, but not where the reference says
        self.A.run("ADD_NODE(%d, 999, 999, 0)" % nb)
        self.B.issue("SIMPLE_ROAD{ nodeA = %d, b = { x = 500, y = 100, z = 0 } }" % nb)
        self.tick(14)
        self.assertIsNotNone(self.A.lua("FIND_EDGE(400, 0, 500, 100)"), "found by place, not by the id")
        self.assertEqual(self.A.lua("#EDGES_AT(%d)" % nb), 0, "the node that happened to carry the id was left alone")

    def test_an_unresolvable_reference_refuses_the_action_on_the_game_that_lacks_the_place(self):
        self.start()
        self.tick(5)
        self.A.run("WORLD[111] = nil; WORLD[102] = nil")   # the host's copy of the road lost its end
        self.B.issue("SIMPLE_ROAD{ nodeA = 102, b = { x = 300, y = 100, z = 0 } }")
        self.tick(14)
        self.assertEqual(self.A.lua("#BUILD_LOG"), 0, "the host built nothing on a node it does not have")
        self.assertEqual(self.B.lua("#BUILD_LOG"), 1)
        self.assertTrue(any("unresolved node at 200.000,0.000,0.000" in d for _, _, d in self.relay.refused), self.relay.refused)

    def test_two_nodes_equally_near_make_a_reference_ambiguous_and_refuse(self):
        self.start()
        self.tick(5)
        self.A.run("WORLD[102] = nil; WORLD[111] = nil; ADD_NODE(2001, 200.3, 0, 0); ADD_NODE(2002, 199.7, 0, 0)")
        self.B.issue("SIMPLE_ROAD{ nodeA = 102, b = { x = 300, y = 100, z = 0 } }")
        self.tick(14)
        self.assertEqual(self.A.lua("#BUILD_LOG"), 0)
        self.assertTrue(any("ambiguous node" in d for _, _, d in self.relay.refused), self.relay.refused)

    # ---- what the tools hand over

    def test_a_tool_junction_splits_a_segment_and_adds_a_branch(self):
        self.start()
        self.B.issue("NATIVE_SPLIT{ edge = 110, at = { x = 50, y = 0, z = 0 }, to = { x = 50, y = 80, z = 0 } }", ie="false", pi="true")
        self.tick(14)
        self.assertSameWorld()
        self.assertIsNone(self.A.lua("FIND_EDGE(0, 0, 100, 0)"), "the old segment is gone")
        for ends in ("0, 0, 50, 0", "50, 0, 100, 0", "50, 0, 50, 80"):
            self.assertIsNotNone(self.A.lua("FIND_EDGE(%s)" % ends))
        self.assertEqual(self.A.lua("#EDGES_AT(FIND_NODE(50, 0, 0))"), 3)
        self.assertEqual(self.A.lua("BUILD_CTX[1]"), "align=true cleanup=false buildings=true fields=true player=7 ie=false pi=true",
                         "a tool's build is rebuilt with the context the tools use")

    def test_a_script_context_and_flags_are_reproduced(self):
        self.start()
        self.B.issue("SIMPLE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }", ctx="CONTEXT(true, true, false, true)", ie="true", pi="false")
        self.B.issue("SIMPLE_ROAD{ a = { x = 300, y = 100, z = 0 }, b = { x = 400, y = 100, z = 0 } }")
        self.tick(18)
        self.assertEqual(self.A.lua("BUILD_CTX[1]"), "align=true cleanup=true buildings=false fields=true player=7 ie=true pi=false")
        self.assertEqual(self.A.lua("BUILD_CTX[2]"), "nil ie=false pi=true", "no context stays no context")
        self.assertEqual(self.B.lua("BUILD_CTX[1]"), self.A.lua("BUILD_CTX[1]"))

    def test_every_build_kind_is_replayed_with_the_originators_own_context(self):
        # one rule for every plan (the stop tool builds with 0/0/0, the road tools with 1/1/1): the replay never substitutes flags of its own.
        # The mock engine refuses a Context whose cleanupStreetGraph differs from the one the tool used, as the real engine builds another graph.
        cases = [
            ("NATIVE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }", 1),
            ("NATIVE_SPLIT{ edge = 110, at = { x = 50, y = 0, z = 0 }, to = { x = 50, y = 80, z = 0 } }", 1),
            ("NATIVE_BULLDOZE{ edges = { 110 } }", 1),
            ("NATIVE_DEPOT_ON_ROAD{ edge = 110, at = { x = 50, y = 0, z = 0 }, door = { x = 50, y = -30, z = 0 } }", 2),
            ("NATIVE_UPGRADE{ edges = { 110, 111 }, template = 'street/new_small_road.street_template' }", 2),
        ]
        for proposal, commands in cases:
            for flags in ("false, false, false, true, false", "true, true, true, true, true", "false, true, false, true, false"):
                with self.subTest(proposal=proposal[:30], flags=flags):
                    self.setUp()
                    self.start()
                    cleanup = flags.split(", ")[1]
                    for g in (self.A, self.B):
                        g.run("CTX_REFUSE = function(c) return c.cleanupStreetGraph ~= %s end" % cleanup)
                    self.B.issue(proposal, ctx="CONTEXT(%s)" % flags)
                    self.tick(16)
                    self.assertEqual(self.A.lua("#BUILD_CTX"), commands, "the replay was refused: %s" % self.A.lua("BUILD_REFUSALS[1]"))
                    want = "align=%s cleanup=%s buildings=%s fields=%s" % tuple(flags.split(", ")[:4])
                    for i in range(1, commands + 1):
                        self.assertTrue(str(self.A.lua("BUILD_CTX[%d]" % i)).startswith(want), self.A.lua("BUILD_CTX[%d]" % i))
                    self.assertSameWorld()
                    self.tearDown()

    def test_the_redo_pillars_flag_travels_too(self):
        self.start()
        self.B.issue("NATIVE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }", ctx="CONTEXT(true, true, true, true, true)")
        self.tick(14)
        self.assertEqual(self.A.lua("GAME.applied[#GAME.applied].args[2].extendProposalRedoPillars"), True)

    def test_a_record_without_context_flags_is_replayed_with_the_fallback_and_says_so(self):
        self.start()
        self.B.issue("NATIVE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }")   # no Context at all: the record is marked "native"
        self.tick(14)
        self.assertEqual(self.A.lua("BUILD_CTX[1]"), "align=true cleanup=false buildings=true fields=true player=7 ie=false pi=true")
        self.assertIn("carries no Context flags", self.A.log())

    def test_a_removed_node_read_after_the_build_ran_is_the_bitwise_not_of_its_id(self):
        # real run 2026-10-13 (depot over buildings): rmNodes was [-39113] where the held command says 39112, and the other game refused
        # "removed node 1: bad reference". One rule for every removal list: a negative id of a removed entity is -id - 1.
        self.start()
        code = ("(function() local P = NATIVE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }; "
                "local pos = NODE_POS(101); "
                "local sp = P.proposal; sp.removedNodes = { D('NodeAndEntity', { entity = -101 - 1, comp = D('BaseNode', { position = V(pos.x, pos.y, pos.z) }) }) }; P.proposal = sp; "
                "return CAPTURE_BODY(P) end)()")
        text = str(self.A.lua(code))
        self.assertIn('["rmNodes"]={[1]={["__ref"]="node"', text)
        self.assertNotIn('["rmNodes"]={[1]=-', text)
        self.assertIn('["key"]="s:101"', text[text.index('["rmNodes"]'):text.index('["rmNodes"]') + 80])

    def test_upgrade_of_segments_the_session_built(self):
        self.start()
        self.B.issue("SIMPLE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }")
        self.tick(14)
        eb = int(self.B.lua("FIND_EDGE(300, 0, 400, 0)"))
        ea = int(self.A.lua("FIND_EDGE(300, 0, 400, 0)"))
        self.assertNotEqual(ea, eb)
        self.B.issue("NATIVE_UPGRADE{ edges = { %d }, template = 'street/new_small_road.street_template', deco = { { 12, true } } }" % eb)
        self.tick(18)
        self.assertSameWorld()
        sig = self.A.sig()
        self.assertIn("E 300.00,0.00,0.00 400.00,0.00,0.00 street/new_small_road.street_template", sig)
        self.assertIn("{1={1=12,2=true}}", sig, "the decoration came along")
        self.assertEqual(self.A.lua("HELPER_CALLS[#HELPER_CALLS]"), "replaceSegment %d" % ea, "each game upgraded its own segment")
        self.assertEqual(self.B.lua("HELPER_CALLS[#HELPER_CALLS]"), "replaceSegment %d" % eb)

    def test_upgrading_several_segments_chains_one_command_each(self):
        self.start()
        self.B.issue("NATIVE_UPGRADE{ edges = { 110, 111 }, template = 'street/new_small_road.street_template' }")
        self.tick(18)
        self.assertSameWorld()
        self.assertEqual(self.A.lua("#HELPER_CALLS"), 2)
        self.assertEqual(self.A.lua("BUILD_LOG[1]"), "cons=0 rm=0 nodes=0 segs=1 rmsegs=1 objs=0")
        self.assertEqual(self.A.lua("#BUILD_LOG"), 2)
        self.assertEqual(self.A.sig().count("new_small_road"), 2)

    def test_custom_lanes_survive_a_write_shift_of_transport_modes(self):
        self.start()
        self.A.run("LANE_SHIFT = 1")
        self.B.run("LANE_SHIFT = 1")
        self.B.issue("(function() local P = NATIVE_UPGRADE{ edges = { 110 }, template = 'street/new_small_road.street_template' }\n"
                     "  local sp = P.proposal; local added = sp.addedSegments; local s = added[1]; local c = s.comp\n"
                     "  local lanes = c.laneConfigs; lanes[3] = D('LaneConfig', { forward = true, speed = 9, width = 2, height = 0, offset = 0 })\n"
                     "  local tm = {}; for j = 0, 15 do tm[j] = (j == 4 or j == 15) end\n"
                     "  local l = lanes[3]; l.transportModes = tm; lanes[3] = l\n"
                     "  c.laneConfigs = lanes; s.comp = c; added[1] = s; sp.addedSegments = added; P.proposal = sp\n"
                     "  return P end)()")
        self.tick(18)
        self.assertSameWorld("the lanes carry the same modes on both games")
        # the originator's engine stored the shifted write (4 -> 3, 15 -> 14); the replay compensates with the measured shift
        self.assertIn("f3,14", self.A.sig())
        self.assertIn("f3,14", self.B.sig())

    def test_bulldozing_a_session_built_segment_uses_the_engines_removal(self):
        self.start()
        self.B.issue("SIMPLE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }")
        self.tick(14)
        eb = int(self.B.lua("FIND_EDGE(300, 0, 400, 0)"))
        ea = int(self.A.lua("FIND_EDGE(300, 0, 400, 0)"))
        self.B.issue("NATIVE_BULLDOZE{ edges = { %d } }" % eb)
        self.tick(14)
        self.assertSameWorld()
        self.assertIsNone(self.A.lua("FIND_EDGE(300, 0, 400, 0)"))
        self.assertEqual(self.A.lua("HELPER_CALLS[#HELPER_CALLS]"), "makeSegmentsRemoveProposal %d" % ea)
        self.assertEqual(self.B.lua("HELPER_CALLS[#HELPER_CALLS]"), "makeSegmentsRemoveProposal %d" % eb)

    def test_the_replay_verdict_stays_readable_for_simple_proposals_after_a_native_proposal_failed(self):
        self.start()
        for g in (self.A, self.B):
            g.run("VERDICT_MOCK = true")
        self.B.issue("NATIVE_BULLDOZE{ edges = { 110 } }")                      # a native Proposal: the engine cannot read its data
        self.tick(14)
        self.B.issue("SIMPLE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }")
        self.tick(14)
        log = self.A.log()
        self.assertIn("verdict n/a (a native Proposal", log)
        self.assertIn("builds that rebuild as a native Proposal", log)
        self.assertNotIn("from now on", log, "the reader is not switched off for every build")
        self.assertIn("plan=street cons=0 rem=0 nodes=2 segs=1 rmSegs=0 objs=0; verdict critical=false", log)

    def test_a_bridge_in_place_of_a_street_is_rebuilt_as_a_street_proposal_on_both_games(self):
        self.start()
        self.B.issue("(function() local P = NATIVE_UPGRADE{ edges = { 110 }, template = 'street/new_medium_road.street_template' }; local sp = P.proposal; "
                     "local added = sp.addedSegments; local s = added[1]; local c = s.comp; c.type = 1; c.typeIndex = 3; s.comp = c; added[1] = s; "
                     "sp.addedSegments = added; P.proposal = sp; return P end)()")
        self.tick(14)
        self.assertSameWorld()
        self.assertIn("E 0.00,0.00,0.00 100.00,0.00,0.00 street/new_medium_road.street_template kind0 t1/3", self.A.sig())
        self.assertEqual(self.A.lua("#HELPER_CALLS"), 0, "not replaceSegment: that would have kept the street")

    # ---- constructions

    def test_a_station_is_placed_with_a_seed_every_game_agrees_on(self):
        self.start()
        self.B.issue("CON_PROPOSAL{ file = 'station/rail/modular_station.con', x = 300, y = 200, params = { length = 3 } }")
        self.tick(14)
        self.assertSameWorld()
        sig = self.A.sig()
        self.assertIn("C station/rail/modular_station.con@300.00,200.00,0.00", sig)
        self.assertIn("length=3", sig)
        self.assertIn("seed=2700", sig, "7x + 3y + z of where it stands, as the game's own scripts derive it")
        self.assertEqual(self.A.lua("BUILD_LOG[1]"), "cons=1 rm=0 nodes=0 segs=0 rmsegs=0 objs=0")
        self.assertIn("s1", sig.split("station/rail/modular_station.con")[1].splitlines()[0], "the station came with its station")

    def test_a_construction_is_replaced_then_bulldozed_by_the_place_it_stands(self):
        self.start()
        self.B.issue("CON_PROPOSAL{ file = 'station/rail/modular_station.con', x = 300, y = 200, params = { length = 3 } }")
        self.tick(14)
        cb = int(self.B.lua("FIND_CON('station/rail/modular_station.con')"))
        ca = int(self.A.lua("FIND_CON('station/rail/modular_station.con')"))
        self.assertNotEqual(ca, cb)
        self.B.issue("CON_PROPOSAL{ file = 'station/rail/modular_station.con', x = 300, y = 200, params = { length = 5 }, remove = %d }" % cb)
        self.tick(14)
        self.assertSameWorld()
        self.assertIn("length=5", self.A.sig())
        self.assertNotIn("length=3", self.A.sig())
        cb = int(self.B.lua("FIND_CON('station/rail/modular_station.con')"))
        self.B.issue("CON_REMOVE{ cons = { %d } }" % cb)
        self.tick(14)
        self.assertSameWorld()
        self.assertNotIn("modular_station", self.A.sig())

    def test_a_depot_that_removes_buildings_gets_its_keys_when_the_engine_reuses_a_removed_buildings_id(self):
        # real run 2026-10-13: a depot over four buildings was built on the receiving game, but no entity was "new" (the id of a removed
        # building went to the depot), so nothing was keyed and the result was reported as "found new CONSTRUCTION 0"
        self.start()
        for g in (self.A, self.B):
            g.run("REUSE_FREED_IDS = true")
        self.B.issue("(function() local P = NATIVE_DEPOT_ON_ROAD{ edge = 110, at = { x = 50, y = 0, z = 0 }, door = { x = 50, y = -30, z = 0 } }; "
                     "P.toRemove = { 56 }; return P end)()")
        self.tick(24)
        self.assertSameWorld()
        for g in (self.A, self.B):
            self.assertIn("1:1#c1", str(g.lua("BOUND_KEYS()")).split(), g.label)
            self.assertNotIn("RESULT DIFFERS", g.log(), g.label)
        # the junction configuration of the street part is kept when the construction's own pieces are left to its script
        self.assertEqual(self.A.lua("#CFG_LOG"), 1, "the host's replay put the street part's node configuration back")

    def test_a_vehicle_is_bought_at_a_depot_the_session_built(self):
        self.start()
        self.B.issue("CON_PROPOSAL{ file = 'depot/road_depot.con', x = 500, y = 100 }")
        self.tick(14)
        for g in (self.A, self.B):
            keys = str(g.lua("BOUND_KEYS()")).split()
            self.assertIn("1:1#c1", keys)
            self.assertIn("1:1#c1/d1", keys, "the depot has a key of its own")
        depot_b, depot_a = int(self.B.lua("FIND_DEPOT()")), int(self.A.lua("FIND_DEPOT()"))
        self.assertNotEqual(depot_a, depot_b)
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, %d, { vehicles = {} }), function(d, s) BUY_OK = s end)" % depot_b)
        self.tick(18)
        self.assertTrue(self.B.lua("BUY_OK == true"))
        self.assertEqual(self.A.lua("GAME.applied[#GAME.applied].name"), "makeVehicleBuyCmd")
        self.assertEqual(self.A.lua("GAME.applied[#GAME.applied].args[2]"), depot_a, "each game bought at its own depot")
        self.assertEqual(self.B.lua("GAME.applied[#GAME.applied].args[2]"), depot_b)
        self.assertIn('"1:1#c1/d1"', self.B.shipped()[1].replace("\\", ""))

    def test_keys_are_found_when_the_depot_appears_a_step_late_next_to_a_town_building(self):
        self.start()
        for g in (self.A, self.B):
            g.run("DEFER_CONS = true")
        self.B.issue("CON_PROPOSAL{ file = 'depot/road_depot.con', x = 500, y = 100 }")
        for _ in range(60):
            self.tick(1)
            if self.A.lua("#BUILD_LOG") == 1 and self.B.lua("#BUILD_LOG") == 1:
                break
        self.assertEqual(self.A.lua("COUNT('VEHICLE_DEPOT')"), 1, "the engine has not created the depot yet: it comes with the next step")
        for g in (self.A, self.B):
            g.run("GROW_TOWN = true")      # a town building appears in the step the depot does, and has the higher id
        self.tick(6)
        for g in (self.A, self.B):
            self.assertIsNotNone(g.lua("FIND_CON('town/house.con')"))
            self.assertGreater(g.lua("FIND_CON('town/house.con')"), g.lua("FIND_CON('depot/road_depot.con')"))
            self.assertEqual(g.lua("STATE:get().bind['1:1#c1']"), g.lua("FIND_CON('depot/road_depot.con')"), "the depot's construction, not the newest one")
            self.assertIn("1:1#c1/d1", str(g.lua("BOUND_KEYS()")).split())

    def test_a_junction_nodes_configuration_is_kept_in_every_form_so_a_factory_that_rejects_it_refuses_the_build(self):
        self.start()
        split = "WITH_CFG(NATIVE_SPLIT{ edge = 110, at = { x = 50, y = 0, z = 0 }, to = { x = 50, y = 80, z = 0 } }, -1, { { -3, 0, -5, 0 } })"
        self.B.issue(split)
        self.tick(14)
        self.assertEqual(self.A.lua("#CFG_LOG"), 1, "with the configuration when the factory takes it")
        self.assertSameWorld()
        for g in (self.A, self.B):
            g.run("FACTORY_REJECT = function(parts, kind) if kind == 'SimpleProposal' and #parts.cfgAdd > 0 then return 'Unknown exception' end end; CFG_LOG = {}")
        self.B.issue(split.replace("x = 50", "x = 150").replace("edge = 110", "edge = 111") + "")
        self.tick(14)
        # a new junction node (three segments) must keep its configuration entry in every form (the engine aborts without one, sweep
        # 2026-10-13), so the forms that left it out are no way out any more: the build is refused instead of crashing the game
        self.assertIn("refused", self.A.log())
        self.assertIsNone(self.A.lua("FIND_EDGE(150, 0, 150, 80)"))

    def test_placeholders_are_renumbered_as_a_script_does_when_the_factory_insists(self):
        self.start()
        for g in (self.A, self.B):
            g.run("FACTORY_REJECT = function(parts, kind) if kind == 'SimpleProposal' then for _, s in ipairs(parts.edgesAdd) do if s.entity == -3 then return 'numbering' end end end end")
        self.B.issue("NATIVE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }")
        self.tick(14)
        self.assertSameWorld()
        self.assertIsNotNone(self.A.lua("FIND_EDGE(300, 0, 400, 0)"))
        self.assertIn("(form 4)", self.A.log())

    def test_a_proposal_every_form_of_which_the_factory_rejects_refuses_the_action(self):
        self.start()
        for g in (self.A, self.B):
            g.run("FACTORY_REJECT = function(parts, kind) if kind == 'SimpleProposal' then for _, s in ipairs(parts.edgesAdd) do return 'no segment please' end end end")
        self.B.issue("NATIVE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }", cb="function(d, s) NO_OK = s end")
        self.tick(14)
        self.assertEqual(self.A.lua("#BUILD_LOG") + self.B.lua("#BUILD_LOG"), 0)
        self.assertTrue(any("refused the street build" in d and "no segment please" in d for _, _, d in self.relay.refused), self.relay.refused)

    def test_a_road_that_moves_a_town_building_is_one_combined_proposal(self):
        self.start()
        for g in (self.A, self.B):
            g.run("WORLD[57] = { CONSTRUCTION = CON_COMP('town/house_a.con', 350, 5, 0) }")
        self.B.issue("NATIVE_ROAD_MOVING{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 }, moveFile = 'town/house_a.con', moveTo = { x = 350, y = 40 }, remove = 57 }")
        self.tick(14)
        self.assertSameWorld()
        for g in (self.A, self.B):
            self.assertEqual(g.lua("#BUILD_LOG"), 1, "one command")
            self.assertEqual(g.lua("BUILD_LOG[1]"), "cons=1 rm=1 nodes=2 segs=1 rmsegs=0 objs=0")
        sig = self.A.sig()
        self.assertIn("C town/house_a.con@350.00,40.00,0.00", sig)
        self.assertNotIn("350.00,5.00", sig)

    def test_a_road_that_only_demolishes_town_buildings_still_builds_the_road(self):
        self.start()
        for g in (self.A, self.B):
            g.run("WORLD[57] = { CONSTRUCTION = CON_COMP('town/house_a.con', 350, 5, 0) }")
        self.B.issue("(function() local P = NATIVE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }; P.toRemove = { 57 }; return P end)()")
        self.tick(14)
        self.assertSameWorld()
        self.assertIsNotNone(self.A.lua("FIND_EDGE(300, 0, 400, 0)"), "the road is there")
        self.assertNotIn("town/house_a.con", self.A.sig(), "and the house is gone")
        self.assertEqual(self.A.lua("#BUILD_LOG"), 1, "one proposal")

    def test_a_station_gets_keys_for_its_station_and_its_group(self):
        self.start()
        self.B.issue("CON_PROPOSAL{ file = 'station/bus/bus_station.con', x = 100, y = 300 }")
        self.tick(14)
        keys = str(self.A.lua("BOUND_KEYS()")).split()
        self.assertEqual([k for k in keys if k.startswith("1:1#")], ["1:1#c1", "1:1#c1/g1", "1:1#c1/s1"])

    def test_a_station_whose_only_street_is_its_own_entrance_is_built_ignoring_the_collision_with_it(self):
        # a cargo station placed beside the end of a road (real game, 2026-10-11): the tool's proposal holds the station and its
        # entrance piece, nothing foreign, so the plan is `construction`; it was refused on both games with the tool's flags
        self.start()
        self.B.run("ENTRANCE_P = NATIVE_ROAD{ nodeA = 101, b = { x = 100, y = 40, z = 0 }, template = 'street/simple.street_template' }; "
                   "ENTRANCE_P.toAdd = NATIVE_DEPOT_ON_ROAD{ edge = 110, at = { x = 50, y = 0, z = 0 }, door = { x = 100, y = 40, z = 0 }, file = 'station/street/cargo_station.con' }.toAdd")
        self.B.issue("ENTRANCE_P", cb="function(d, s, e) ENTRANCE_OK = s end")
        self.tick(18)
        self.assertTrue(self.B.lua("ENTRANCE_OK == true"))
        self.assertSameWorld()
        for g in (self.A, self.B):
            self.assertEqual(g.lua("BUILD_LOG[1]"), "cons=1 rm=0 nodes=0 segs=0 rmsegs=0 objs=0", "the entrance is left to the station's script")
            self.assertTrue(str(g.lua("BUILD_CTX[1]")).endswith("ie=true pi=false"), g.lua("BUILD_CTX[1]"))

    def test_a_depot_against_a_road_is_built_in_two_commands_street_first(self):
        self.start()
        self.B.issue("NATIVE_DEPOT_ON_ROAD{ edge = 110, at = { x = 50, y = 0, z = 0 }, door = { x = 50, y = -30, z = 0 } }",
                     cb="function(d, s, e) DEPOT_OK = s end")
        self.tick(18)
        self.assertTrue(self.B.lua("DEPOT_OK == true"))
        self.assertSameWorld()
        for g in (self.A, self.B):
            self.assertEqual(g.lua("BUILD_LOG[1]"), "cons=0 rm=0 nodes=1 segs=2 rmsegs=1 objs=0", "the street part first, without the entrance")
            self.assertEqual(g.lua("BUILD_LOG[2]"), "cons=1 rm=0 nodes=0 segs=0 rmsegs=0 objs=0")
            self.assertEqual(g.lua("BUILD_CTX[1]"), "align=true cleanup=false buildings=true fields=true player=7 ie=false pi=true")
            self.assertEqual(g.lua("BUILD_CTX[2]"), "align=true cleanup=false buildings=true fields=true player=7 ie=true pi=false")
        sig = self.A.sig()
        self.assertIn("C depot/road_depot.con@50.00,-30.00,0.00", sig)
        self.assertEqual(sig.count("\nE ") + sig.startswith("E "), 4, "130, 111 and the two halves of 110 - the entrance was left to the depot's script")
        self.assertIn("1:1#c1/d1", str(self.A.lua("BOUND_KEYS()")).split())

    def test_a_depot_built_while_paused_runs_its_commands_through_callbacks(self):
        self.start(speed=2)
        self.tick(10)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(18)
        self.assertEqual((self.A.time(), self.B.time()), (self.relay.pauseAt, self.relay.pauseAt))
        self.B.issue("NATIVE_DEPOT_ON_ROAD{ edge = 110, at = { x = 50, y = 0, z = 0 }, door = { x = 50, y = -30, z = 0 } }",
                     cb="function(d, s, e) PAUSED_OK = s end")
        self.tick(10)
        self.assertTrue(self.B.lua("PAUSED_OK == true"))
        self.assertSameWorld()
        self.assertEqual(self.A.lua("#BUILD_LOG"), 2)
        self.assertIn("1:1#c1/d1", str(self.A.lua("BOUND_KEYS()")).split())
        self.assertIn("1:1#c1/d1", str(self.B.lua("BOUND_KEYS()")).split())

    def test_a_refused_second_command_stops_the_chain_and_tells_the_window(self):
        self.start(speed=2)
        self.tick(10)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(18)
        for g in (self.A, self.B):
            g.run("BUILD_REFUSE = function(parts) if #parts.consAdd > 0 then return 'Collision' end end")
        self.B.issue("NATIVE_DEPOT_ON_ROAD{ edge = 110, at = { x = 50, y = 0, z = 0 }, door = { x = 50, y = -30, z = 0 } }",
                     cb="function(d, s, e) PAUSED_OK = s end")
        self.tick(10)
        self.assertTrue(self.B.lua("PAUSED_OK == false"), "the window learns the build failed")
        self.assertEqual(self.A.lua("#BUILD_LOG"), 1, "only the street part ran")
        self.assertEqual(self.A.lua("BUILD_REFUSALS[1]"), "Collision")
        self.assertIn("REFUSED", self.B.log())

    # ---- signals and stops

    def test_a_signal_captured_before_the_build_ran_has_no_model_and_stays_native(self):
        self.start()
        self.B.issue("NATIVE_SIGNAL{ edge = 130, param = 0.75 }")
        self.tick(14)
        self.assertIn("an edge object has no model", self.B.log())
        self.assertEqual(self.A.lua("#BUILD_LOG"), 0, "not shipped: the model is only known after the build")
        self.assertEqual(self.B.lua("#BUILD_LOG"), 1, "the player's own build went through natively")

    def test_a_signal_captured_after_the_build_ran_is_replayed_on_the_peer(self):
        self.start()
        self.B.run("SIGNAL_P = NATIVE_SIGNAL{ edge = 130, param = 0.75 }; local ok, created = APPLY_BUILD(SIGNAL_P); SIGNAL_CREATED = created")
        body = self.B.lua("CAPTURE_BODY(SIGNAL_P, SIGNAL_CREATED)")
        self.assertIsInstance(body, str, body)
        self.assertIn("rmSegs", body)
        self.assertIn("signal/signal_a.mdl", body, "the model was read from the object the engine created")
        self.relay.send(self.A, "action", 1, self.relay.ser({"stamp": self.A.time() + 6 * STEP, "seq": 1, "body": body, "from": 1}))
        self.tick(12)
        self.assertSameWorld()
        self.assertIn("[signal/signal_a.mdl@0.250k2;signal/signal_a.mdl@0.750k2]", self.A.sig(), "the signal that was there stays, the new one is added")

    def _negative_removed_segment(self, how):
        # real session 2026-10-13-a: the guest's bus stop shipped rmSegs = { -39433 } and the host refused "removed segment 1:
        # bad reference". The mock has no native stop, so this covers the capture and the replay of the record, not the engine.
        self.start()
        self.B.run("SIGNAL_P = NATIVE_SIGNAL{ edge = 130, param = 0.75 }; local ok, created = APPLY_BUILD(SIGNAL_P); SIGNAL_CREATED = created; NEGATE_REMOVED(SIGNAL_P, '%s')" % how)
        body = self.B.lua("CAPTURE_BODY(SIGNAL_P, SIGNAL_CREATED)")
        self.assertIsInstance(body, str, body)
        self.assertNotRegex(body, r'rmSegs\\?"\]=\{\[1\]=-', "the removed segment travels as a reference")
        self.relay.send(self.A, "action", 1, self.relay.ser({"stamp": self.A.time() + 6 * STEP, "seq": 1, "body": body, "from": 1}))
        self.tick(12)
        self.assertNotIn("bad reference", self.A.log())
        self.assertSameWorld()
        self.assertIn("[signal/signal_a.mdl@0.250k2;signal/signal_a.mdl@0.750k2]", self.A.sig())

    def test_a_removed_segment_listed_as_a_negative_id_with_its_record_is_named_by_its_ends(self):
        self._negative_removed_segment("record")

    def test_a_removed_segment_listed_as_a_bare_negative_id_is_named_by_the_added_segment(self):
        self._negative_removed_segment("bare")

    def test_a_signal_is_bulldozed_and_the_one_the_session_added_stays(self):
        self.start()
        self.B.run("SIGNAL_P = NATIVE_SIGNAL{ edge = 130, param = 0.75 }; local ok, created = APPLY_BUILD(SIGNAL_P); SIGNAL_CREATED = created")
        body = self.B.lua("CAPTURE_BODY(SIGNAL_P, SIGNAL_CREATED)")
        self.relay.send(self.A, "action", 1, self.relay.ser({"stamp": self.A.time() + 6 * STEP, "seq": 1, "body": body, "from": 1}))
        self.tick(12)
        self.assertSameWorld()
        self.assertIn("[signal/signal_a.mdl@0.250k2;signal/signal_a.mdl@0.750k2]", self.A.sig())
        # the savegame's signal (rank 1 on the segment) goes, the one just added (rank 2, another id in each game) stays
        for g in (self.A, self.B):
            self.assertNotEqual(g.lua("FIND_EDGE(0, 50, 100, 50)"), 130, "the segment was replaced")
        self.B.issue("NATIVE_UNSIGNAL{ edge = %d, object = 140 }" % int(self.B.lua("FIND_EDGE(0, 50, 100, 50)")))
        self.tick(14)
        self.assertSameWorld()
        self.assertIn("[signal/signal_a.mdl@0.750k2]", self.A.sig())
        self.assertNotIn("0.250", self.A.sig())

    def test_old_to_new_and_owners_map_onto_this_games_entities(self):
        self.start()
        for g in (self.A, self.B):
            g.run("WORLD[57] = { CONSTRUCTION = CON_COMP('town/house_a.con', 350, 5, 0) }")
        self.B.issue("CON_PROPOSAL{ file = 'station/rail/modular_station.con', x = 300, y = 200, params = { length = 3 } }")
        self.tick(14)
        cb, ca = int(self.B.lua("FIND_CON('station/rail/modular_station.con')")), int(self.A.lua("FIND_CON('station/rail/modular_station.con')"))
        self.B.issue("CON_PROPOSAL{ file = 'station/rail/modular_station.con', x = 300, y = 200, params = { length = 5 }, remove = %d }" % cb)
        self.B.issue("NATIVE_ROAD_MOVING{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 }, moveFile = 'town/house_a.con', moveTo = { x = 350, y = 40 }, remove = 57 }")
        self.tick(18)
        self.assertSameWorld()
        self.assertEqual(self.A.lua("OLD2NEW_LOG[1]"), "%d=0" % ca, "the host's own station id as the key")
        self.assertEqual(self.B.lua("OLD2NEW_LOG[1]"), "%d=0" % cb)
        for g in (self.A, self.B):
            owners = [str(g.lua("CON_OWNERS[%d]" % i)) for i in (1, 2, 3)]
            self.assertEqual(owners, ["7", "7", "-1"], "the player's construction is this game's player, a town building stays nobody's")

    def test_a_bus_stop_on_the_left_is_object_kind_zero(self):
        self.start()
        self.B.run("STOP_P = NATIVE_SIGNAL{ edge = 110, param = 0.5, left = true, kind = 0, model = 'station/street/bus_stop.mdl' }; "
                   "local ok, created = APPLY_BUILD(STOP_P); STOP_CREATED = created")
        body = self.B.lua("CAPTURE_BODY(STOP_P, STOP_CREATED)")
        self.relay.send(self.A, "action", 1, self.relay.ser({"stamp": self.A.time() + 6 * STEP, "seq": 1, "body": body, "from": 1}))
        self.tick(12)
        self.assertSameWorld()
        self.assertIn("station/street/bus_stop.mdl@0.500k0", self.A.sig())

    def test_a_road_side_stop_and_its_station_group_get_keys_both_games_agree_on(self):
        self.start()
        self.B.issue("SIMPLE_STOP{ edge = 110, param = 0.5, left = true }", cb="function(d, s, e) STOP_OK = s end")
        self.tick(14)
        self.assertTrue(self.B.lua("STOP_OK == true"))
        self.assertSameWorld()
        self.assertIn("station/street/bus_stop.mdl@0.500k0", self.A.sig())
        sa, sb = self.both("COUNT('STATION')")
        self.assertEqual((sa, sb), (1, 1))
        for g in (self.A, self.B):
            self.assertEqual([k for k in str(g.lua("BOUND_KEYS()")).split() if k.startswith("1:1#")], ["1:1#o1", "1:1#o1/g"])
            # the keys name the stop on the segment it was put on, and the group that stop belongs to, in each game's own ids
            self.assertEqual(g.lua("STOP_AT('1:1#o1')"), "0.00,0.00,0.00 100.00,0.00,0.00 station/street/bus_stop.mdl grouptrue true")
        ga, gb = self.both("STATE:get().bind['1:1#o1/g']")
        self.assertNotEqual(ga, gb, "another id in each game, one key")

    def test_a_stop_is_found_through_its_new_station_group_when_edge_objects_cannot_be_listed(self):
        # sweep run 20261013-053645 (B1): the host listed the new STATION_GROUP but no EDGE_OBJECT, the keys were never bound and the
        # result line read "expected 0 found 0 differs"
        self.start()
        for g in (self.A, self.B):
            g.run("HIDE_ENUM = { EDGE_OBJECT = true }")
        self.B.issue("SIMPLE_STOP{ edge = 110, param = 0.5, left = true }")
        self.tick(14)
        self.assertSameWorld()
        for g in (self.A, self.B):
            self.assertEqual([k for k in str(g.lua("BOUND_KEYS()")).split() if k.startswith("1:1#")], ["1:1#o1", "1:1#o1/g"], g.label)
            self.assertNotIn("RESULT DIFFERS", g.log(), g.label)

    def test_a_second_stop_that_joins_an_existing_group_is_keyed_from_the_engines_result_entities(self):
        # sweep run 20261013-055034 (B2): no new STATION_GROUP, edge objects cannot be listed: only the engine's own result list names the stop
        self.start()
        for g in (self.A, self.B):
            g.run("HIDE_ENUM = { EDGE_OBJECT = true }; WORLD[900] = { STATION_GROUP = { stations = {} } }; JOIN_GROUP = 900")
        self.B.issue("SIMPLE_STOP{ edge = 110, param = 0.5, left = false }")
        self.tick(40)
        self.assertSameWorld()
        for g in (self.A, self.B):
            self.assertEqual([k for k in str(g.lua("BOUND_KEYS()")).split() if k.startswith("1:1#")], ["1:1#o1"], g.label)
            self.assertNotIn("RESULT DIFFERS", g.log(), g.label)

    def test_a_result_that_differs_says_what_was_expected_and_found_for_each_kind(self):
        self.start()
        for g in (self.A, self.B):
            g.run("HIDE_ENUM = { EDGE_OBJECT = true, STATION_GROUP = true }")
        self.B.issue("SIMPLE_STOP{ edge = 110, param = 0.5, left = true }")
        self.tick(40)
        log = self.A.log()
        self.assertIn("RESULT DIFFERS", log)
        self.assertIn("1 road-side stop(s)", log)
        self.assertIn("found new by component: [EDGE_OBJECT 0, STATION_GROUP 0, GROUP_STATION 0]", log)
        self.assertIn("stops keyed 0 of 1", log)

    def test_a_stop_on_a_segment_that_already_has_two_stops_keeps_them_and_logs_both_sides(self):
        self.start()
        for g in (self.A, self.B):
            g.run("ADD_OBJECT(901, 110, 'station/street/bus_stop.mdl', 0.25, 0); MAKE_STOP(901); ADD_OBJECT(902, 110, 'station/street/bus_stop.mdl', 0.75, 1); MAKE_STOP(902)")
        self.B.issue("SIMPLE_STOP{ edge = 110, param = 0.5, left = true }")
        self.tick(14)
        self.assertSameWorld()
        for g in (self.A, self.B):
            self.assertEqual(g.lua("COUNT('STATION')"), 3, g.label)
        self.assertRegex(self.A.log(), r"stop: replaced segment \d+: objects here \[901/0 902/1\]")

    def test_two_stops_in_one_step_are_not_taken_for_each_others(self):
        self.start()
        for g in (self.A, self.B):
            g.run("DEFER_BUILDS = true")
        self.B.issue("SIMPLE_STOP{ edge = 110, param = 0.25, left = true }")
        self.B.issue("SIMPLE_STOP{ edge = 130, param = 0.75, left = false }")
        self.tick(14)
        self.assertSameWorld()
        for g in (self.A, self.B):
            self.assertEqual(g.lua("STOP_AT('1:1#o1')"), "0.00,0.00,0.00 100.00,0.00,0.00 station/street/bus_stop.mdl grouptrue true", "the first build's stop")
            self.assertEqual(g.lua("STOP_AT('1:2#o1')"), "0.00,50.00,0.00 100.00,50.00,0.00 station/street/bus_stop.mdl grouptrue true", "the second build's stop")
            self.assertEqual(g.lua("STOP_AT('1:1#o2')"), "none")
            self.assertEqual(g.lua("STOP_AT('1:2#o2')"), "none")
        self.assertEqual(self.A.lua("STATE:get().bind['1:1#o1/g'] ~= STATE:get().bind['1:2#o1/g']"), True)

    def test_a_stop_that_joins_a_group_that_exists_is_keyed_but_the_group_keeps_its_own_key(self):
        self.start()
        for g in (self.A, self.B):
            g.run("WORLD[900] = { STATION_GROUP = { stations = {} } }; JOIN_GROUP = 900")
        self.B.issue("SIMPLE_STOP{ edge = 110, param = 0.5, left = true }")
        self.tick(14)
        self.assertSameWorld()
        for g in (self.A, self.B):
            self.assertEqual([k for k in str(g.lua("BOUND_KEYS()")).split() if k.startswith("1:1#")], ["1:1#o1"], "no key for a group that was there before")
            self.assertEqual(g.lua("STOP_AT('1:1#o1')"), "0.00,0.00,0.00 100.00,0.00,0.00 station/street/bus_stop.mdl groupfalse false")

    def test_a_scripts_signal_is_no_stop_and_asks_for_no_keys(self):
        self.start()
        # the kind comes from the signal's place in the segment's object list: no station, no group, no keys
        self.B.issue("SIMPLE_STOP{ edge = 130, param = 0.5, kind = 2, model = 'signal/signal_a.mdl' }")
        self.tick(14)
        self.assertSameWorld()
        self.assertIn("signal/signal_a.mdl@0.500k2", self.A.sig())
        self.assertEqual(self.both("COUNT('STATION')"), (0, 0))
        self.assertEqual([k for k in str(self.A.lua("BOUND_KEYS()")).split() if k.startswith("1:1#")], [])

    def test_a_signal_alone_asks_for_no_keys(self):
        self.start()
        self.B.run("SIGNAL_P = NATIVE_SIGNAL{ edge = 130, param = 0.75 }; local ok, created = APPLY_BUILD(SIGNAL_P); SIGNAL_CREATED = created")
        body = self.B.lua("CAPTURE_BODY(SIGNAL_P, SIGNAL_CREATED)")
        self.relay.send(self.A, "action", 1, self.relay.ser({"stamp": self.A.time() + 6 * STEP, "seq": 1, "body": body, "from": 1}))
        self.tick(14)
        self.assertEqual([k for k in str(self.A.lua("BOUND_KEYS()")).split() if k.startswith("1:1")], [])
        self.assertEqual(self.A.lua("#STATE:get().binders"), 0, "nothing left to look for")

    def test_a_stop_built_while_paused_is_keyed_on_both_games(self):
        self.start(speed=2)
        self.tick(10)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(14)
        self.assertEqual((self.A.time(), self.B.time()), (self.relay.pauseAt, self.relay.pauseAt))
        self.B.issue("SIMPLE_STOP{ edge = 110, param = 0.5, left = true }", cb="function(d, s, e) PAUSED_STOP = s end")
        self.tick(10)
        self.assertTrue(self.B.lua("PAUSED_STOP == true"))
        self.assertSameWorld()
        for g in (self.A, self.B):
            self.assertEqual(g.lua("STOP_AT('1:1#o1')"), "0.00,0.00,0.00 100.00,0.00,0.00 station/street/bus_stop.mdl grouptrue true")

    # ---- node configurations

    def test_lane_connections_and_crosswalks_follow_the_segments_the_session_built(self):
        self.start()
        self.B.issue("NATIVE_SPLIT{ edge = 110, at = { x = 50, y = 0, z = 0 }, to = { x = 50, y = 80, z = 0 } }")
        self.tick(14)

        def edge(game, *p):
            return int(game.lua("FIND_EDGE(%s)" % ", ".join(str(x) for x in p)))

        eb1, eb2, eb3 = edge(self.B, 0, 0, 50, 0), edge(self.B, 50, 0, 100, 0), edge(self.B, 50, 0, 50, 80)
        jb = int(self.B.lua("FIND_NODE(50, 0, 0)"))
        self.B.issue("NATIVE_NODECFG{ nodes = { { node = %d, lanes = { { %d, 0, %d, 1 } }, crosswalks = { %d }, dss = true } } }" % (jb, eb1, eb3, eb2))
        self.tick(14)
        ea1, ea3 = edge(self.A, 0, 0, 50, 0), edge(self.A, 50, 0, 50, 80)
        self.assertNotEqual(ea1, eb1)
        self.assertEqual(self.A.lua("CFG_LOG[#CFG_LOG].lanes[1]"), "%d>%d" % (ea1, ea3), "the host's configuration names the host's segments")
        self.assertEqual(self.B.lua("CFG_LOG[#CFG_LOG].lanes[1]"), "%d>%d" % (eb1, eb3))
        self.assertEqual(self.A.lua("CFG_LOG[#CFG_LOG].crosswalks"), 1)
        self.assertTrue(self.A.lua("CFG_LOG[#CFG_LOG].dss"))
        self.assertEqual(self.A.lua("CFG_LOG[#CFG_LOG].node"), int(self.A.lua("FIND_NODE(50, 0, 0)")))

    # ---- terrain

    def test_a_terrain_edit_stays_native(self):
        self.start()
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeWorldReplaceTerrainCmd(1, 2, 'seed', 3, false))")
        self.B.issue("NATIVE_TERRAIN(10, 20, 12, 12)")
        self.tick(14)
        self.assertEqual(self.A.applied(), [])
        self.assertEqual(len(self.B.applied()), 2)
        self.assertEqual(self.B.shipped(), [])
        self.assertIn("terrain cells cannot be read or written from a script", self.B.log())
        self.assertIn("a terrain edit: its cells cannot be read from a script", self.B.log())

    def test_a_terrain_grid_riding_with_a_road_is_not_copied_but_the_road_is(self):
        self.start()
        self.B.issue("(function() local P = NATIVE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }; "
                     "local t = P.terrain; t.baseHeightMod = D('GridVec2f', { x0 = 1, y0 = 2, width = 8, height = 8 }); P.terrain = t; return P end)()")
        self.tick(14)
        self.assertIsNotNone(self.A.lua("FIND_EDGE(300, 0, 400, 0)"))
        self.assertIn("a terrain edit of 8x8 cells comes with this build: the cells are not copied", self.B.log())

    # ---- permissions

    def test_bulldozing_and_building_are_separate_permissions(self):
        self.start()
        self.B.issue("SIMPLE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }")
        self.tick(14)
        eb = int(self.B.lua("FIND_EDGE(300, 0, 400, 0)"))
        self.relay.send(self.B, "permissions", 0, self.relay.ser({"bulldoze": False}))
        self.tick(2)
        self.B.issue("NATIVE_BULLDOZE{ edges = { %d } }" % eb, cb="function(d, s) DOZE_OK = s end")
        self.tick(12)
        self.assertTrue(self.B.lua("DOZE_OK == false"))
        self.assertIn((1, 0, "permission denied: bulldoze"), self.relay.refused)
        self.assertIsNotNone(self.A.lua("FIND_EDGE(300, 0, 400, 0)"), "nothing was bulldozed")
        self.B.issue("SIMPLE_ROAD{ a = { x = 300, y = 100, z = 0 }, b = { x = 400, y = 100, z = 0 } }")
        self.tick(14)
        self.assertIsNotNone(self.A.lua("FIND_EDGE(300, 100, 400, 100)"), "building is still allowed")
        # the other way round: bulldozing allowed, building denied; replacing a segment is building, removing a construction is bulldozing
        self.relay.send(self.B, "permissions", 0, self.relay.ser({"bulldoze": True, "build": False}))
        self.tick(2)
        self.B.issue("SIMPLE_ROAD{ a = { x = 300, y = 200, z = 0 }, b = { x = 400, y = 200, z = 0 } }", cb="function(d, s) BUILD_OK = s end")
        self.B.issue("NATIVE_UPGRADE{ edges = { %d }, template = 'street/new_small_road.street_template' }" % eb, cb="function(d, s) UP_OK = s end")
        self.tick(12)
        self.assertTrue(self.B.lua("BUILD_OK == false"))
        self.assertTrue(self.B.lua("UP_OK == false"), "an upgrade replaces segments: it is building")
        self.B.issue("NATIVE_BULLDOZE{ edges = { %d } }" % eb)
        self.tick(14)
        self.assertIsNone(self.A.lua("FIND_EDGE(300, 0, 400, 0)"))

    # ---- wire safety

    def test_a_hostile_record_is_refused_without_harm(self):
        self.start()
        ident = "{" + ",".join("[%d]=%d" % (i, 1 if i in (1, 6, 11, 16) else 0) for i in range(1, 17)) + "}"
        wrap = '{["fn"]="makeWorldBuildProposalCmd",["args"]={["n"]=5,[1]=%s,[2]=false,[3]=false,[4]=true,[5]=false}}'
        cases = [
            ("version 7", wrap % '{["v"]=7}'),
            ("bad file name", wrap % ('{["v"]=1,["cons"]={[1]={["file"]="evil.lua",["transf"]=%s}}}' % ident)),
            ("too large", wrap % ('{["v"]=1,["nodes"]={' + ",".join('[%d]={["id"]=-1,["pos"]={["x"]=0,["y"]=0,["z"]=0}}' % i for i in range(1, 4100)) + "}}")),
            ("bad entry", wrap % '{["v"]=1,["nodes"]={[1]={["id"]=-1,["pos"]={["x"]=(0/0),["y"]=0,["z"]=0}}}}'),
            ("not a list", wrap % '{["v"]=1,["segs"]=7}'),
            ("nothing to build", wrap % '{["v"]=1}'),
        ]
        for i, (_, body) in enumerate(cases, 1):
            self.relay.send(self.A, "action", 1, self.relay.ser({"stamp": self.A.time() + (6 + i) * STEP, "seq": 10 + i, "body": body, "from": 1}))
        self.tick(14)
        details = [d for _, _, d in self.relay.refused]
        for want in ("build record version 7", "bad file name", "build record too large", "bad entry", "is not a list", "nothing to build"):
            self.assertTrue(any(want in d for d in details), "%s not in %s" % (want, details))
        self.assertEqual(self.A.lua("#BUILD_LOG"), 0)
        self.assertEqual(self.A.speed(), 1, "the game carries on")
        self.assertFalse(self.A.reload_pending())


# ---------------------------------------------------------------- the module without a bridge

PURE_LUA = r'''
function PURE()
  local M, env = LOAD_BUILD()
  local C = env.ug_require("feverlink3_common.lua")
  local ctx = { plain = C.plain, keyOf = function(e) return "s:" .. e end }
  return M, C, ctx, env
end

function CAPTURE(P, flags)
  local M, C, ctx = PURE()
  return M.capture(P, flags or {}, ctx)
end

function PLAN_OF(P)
  local body, why = CAPTURE(P)
  if not body then return "refused: " .. tostring(why) end
  local M = PURE()
  return M.classify(body.args[1])
end

-- a record that went over the wire and back
function WIRE_RECORD(P, flags)
  local M, C, ctx = PURE()
  local body, why = M.capture(P, flags or {}, ctx)
  if not body then return nil, why end
  return C.parse(C.serialize(body)), M, C
end

function DECODE(body, resolve)
  local M = PURE()
  local ctx = { player = 7, resolve = resolve or function(key) local n = tostring(key):match("^s:(%d+)$"); return n and tonumber(n) or nil end }
  return M.commands.makeWorldBuildProposalCmd.decode(body, ctx)
end
'''


class TrackEdgeTest(BuildTest):
    """The segment of a track build carries its track type and catenary (SegmentAndEntity.trackEdge): a replay without it is a street edge."""

    def test_the_track_type_and_catenary_travel_and_a_road_keeps_the_default(self):
        self.start()
        self.B.issue("NATIVE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 }, track = true, trackType = 2, catenary = true }")
        self.tick(14)
        self.B.issue("NATIVE_ROAD{ a = { x = 300, y = 100, z = 0 }, b = { x = 400, y = 100, z = 0 } }")
        self.tick(14)
        for g in (self.A, self.B):
            self.assertEqual(g.lua("tostring(GAME.applied[1].args[1].streetProposal.edgesToAdd[1].trackEdge.trackType) .. ' ' .. tostring(GAME.applied[1].args[1].streetProposal.edgesToAdd[1].trackEdge.catenary)"), "2 true", g.label)
        self.assertEqual(self.A.lua("tostring(GAME.applied[2].args[1].streetProposal.edgesToAdd[1].trackEdge.trackType) .. ' ' .. tostring(GAME.applied[2].args[1].streetProposal.edgesToAdd[1].trackEdge.catenary)"), "0 false", "a road stays default")


class RealTrackTemplateTest(BuildTest):
    """A user's track has the template ::/infrastructure/track/simple/simple.street_template, whose name contains the text of a
    construction's own piece. It must still travel with its geometry and track type (real session 2026-10-13: it was captured
    as a bare piece and refused as "Construction Not Possible" on both games)."""

    def test_a_track_with_the_simple_template_keeps_its_geometry_and_track_type(self):
        self.start()
        self.B.issue("NATIVE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 }, track = true, trackType = 1, catenary = true, template = 'track/simple/simple.street_template' }")
        self.tick(14)
        for g in (self.A, self.B):
            self.assertEqual(g.lua("#GAME.applied"), 1, g.label)
            e = "GAME.applied[1].args[1].streetProposal.edgesToAdd[1]"
            self.assertEqual(g.lua("tostring(%s.trackEdge.trackType) .. ' ' .. tostring(%s.trackEdge.catenary)" % (e, e)), "1 true", g.label)
            self.assertEqual(g.lua("tostring(%s.comp.position1.x) .. ' ' .. tostring(%s.comp.tangent0.x)" % (e, e)), "400 100", g.label)


class ReplayIgnoreErrorsBase:
    """FEVERLINK3_REPLAY_IGNORE_ERRORS: a build that adds and removes constructions is replayed with ignoreErrors on (default off)."""

    switch = None

    def setUp(self):
        self._old = os.environ.get("FEVERLINK3_REPLAY_IGNORE_ERRORS")
        if self.switch is None:
            os.environ.pop("FEVERLINK3_REPLAY_IGNORE_ERRORS", None)
        else:
            os.environ["FEVERLINK3_REPLAY_IGNORE_ERRORS"] = self.switch
        super().setUp()

    def tearDown(self):
        super().tearDown()
        if self._old is None:
            os.environ.pop("FEVERLINK3_REPLAY_IGNORE_ERRORS", None)
        else:
            os.environ["FEVERLINK3_REPLAY_IGNORE_ERRORS"] = self._old

    def replayed_ctx(self):
        self.start()
        for g in (self.A, self.B):
            g.run("WORLD[57] = { CONSTRUCTION = CON_COMP('town/house_a.con', 350, 5, 0) }")
        self.B.issue("NATIVE_ROAD_MOVING{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 }, moveFile = 'town/house_a.con', moveTo = { x = 350, y = 40 }, remove = 57 }")
        self.tick(14)
        return str(self.A.lua("BUILD_CTX[1]"))

class ReplayIgnoreErrorsOffTest(ReplayIgnoreErrorsBase, BuildTest):
    def test_off_by_default(self):
        self.assertIn("ie=false", self.replayed_ctx())


class ReplayIgnoreErrorsOnTest(ReplayIgnoreErrorsBase, BuildTest):
    switch = "1"

    def test_on_with_the_switch(self):
        self.assertIn("ie=true", self.replayed_ctx())


class PureBuildTest(unittest.TestCase):
    """The build schema alone, in a mocked world, without the bridge."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-")
        # a session folder of its own: the module logs, and must not touch anything outside this test
        self.saved_env = {k: os.environ.get(k) for k in ("FEVERLINK3_SESSION_DIR", "FEVERLINK3_PLAYER_ID", "FEVERLINK3_PLAYER_NAME")}
        os.environ.update(FEVERLINK3_SESSION_DIR=self.tmp, FEVERLINK3_PLAYER_ID="1", FEVERLINK3_PLAYER_NAME="pure")
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        g = self.L.globals()
        g.COMMON_SRC, g.PORTABLE_SRC = T.COMMON_SRC, T.PORTABLE_SRC
        cmds = self.L.table()
        for stem, src in T.CMDS_SRC.items():
            cmds[stem] = src
        g.CMDS_SRC = cmds
        self.L.execute(T.MOCK % dict(t0=T.T0, first=70000))
        self.L.execute(BUILD_MOCK)
        self.L.execute(BUILD_MOCK2)
        self.L.execute("SAVE_WORLD()")
        self.L.execute(PURE_LUA)

    def tearDown(self):
        for k, v in self.saved_env.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
        shutil.rmtree(self.tmp, ignore_errors=True)

    def ev(self, code):
        return self.L.eval(code)

    def test_the_plan_chosen_for_each_shape_of_build(self):
        road = "a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 }"
        cases = [
            ("SIMPLE_ROAD{ %s }" % road, "street"),
            ("NATIVE_ROAD{ %s }" % road, "street"),
            ("NATIVE_SPLIT{ edge = 110, at = { x = 50, y = 0, z = 0 }, to = { x = 50, y = 80, z = 0 } }", "street"),
            ("NATIVE_UPGRADE{ edges = { 110, 111 }, template = 'street/new_small_road.street_template' }", "upgrade"),
            ("NATIVE_BULLDOZE{ edges = { 110 } }", "removal"),
            ("NATIVE_BULLDOZE{ cons = { 54 } }", "construction"),
            ("NATIVE_BULLDOZE{ edges = { 110 }, cons = { 54 } }", "bulldoze"),
            ("CON_PROPOSAL{ file = 'depot/road_depot.con', x = 1, y = 2 }", "construction"),
            ("CON_PROPOSAL{ file = 'depot/road_depot.con', x = 1, y = 2, remove = 54 }", "construction"),
            ("NATIVE_DEPOT_ON_ROAD{ edge = 110, at = { x = 50, y = 0, z = 0 }, door = { x = 50, y = -30, z = 0 } }", "withStreet"),
            ("NATIVE_ROAD_MOVING{ %s, moveFile = 'town/house_a.con', moveTo = { x = 350, y = 40 }, remove = 54 }" % road, "combined"),
            ("(function() local P = NATIVE_ROAD{ %s }; P.toRemove = { 54 }; return P end)()" % road, "combined"),
            ("NATIVE_NODECFG{ nodes = { { node = 101, lanes = { { 110, 0, 111, 0 } } } } }", "nodecfg"),
            ("NATIVE_TERRAIN(1, 2, 3, 3)", "refused: a terrain edit: its cells cannot be read from a script, only the native module can copy them"),
            ("D('SimpleProposal')", "refused: nothing to build"),
            ("NATIVE_SIGNAL{ edge = 130, param = 0.5 }", "refused: an edge object has no model (captured before the build ran?)"),
            ("5", "refused: not a proposal (neither streetProposal nor proposal / toAdd found)"),
        ]
        for code, want in cases:
            self.assertEqual(self.ev("PLAN_OF(%s)" % code), want, code)
        # a segment replaced by one of another kind (a bridge) is no upgrade the engine's replaceSegment can make
        bridge = ("(function() local P = NATIVE_UPGRADE{ edges = { 110 }, template = 'street/new_medium_road.street_template' }; local sp = P.proposal; "
                  "local added = sp.addedSegments; local s = added[1]; local c = s.comp; c.type = 1; c.typeIndex = 3; s.comp = c; added[1] = s; "
                  "sp.addedSegments = added; P.proposal = sp; return P end)()")
        self.assertEqual(self.ev("PLAN_OF(%s)" % bridge), "street")
        # what cannot be read about the street pieces a construction brings with it does not stop it from being copied
        odd = ("(function() local P = NATIVE_DEPOT_ON_ROAD{ edge = 110, at = { x = 50, y = 0, z = 0 }, door = { x = 50, y = -30, z = 0 } }; local sp = P.proposal; "
               "local added = sp.addedSegments; local s = added[3]; local c = s.comp; c.node1 = 99999; c.laneConfigs = {}; c.objects = { { 123456, 1 } }; s.comp = c; "
               "added[3] = s; sp.addedSegments = added; P.proposal = sp; return P end)()")
        self.assertEqual(self.ev("PLAN_OF(%s)" % odd), "withStreet")

    def test_the_tunnel_pieces_of_an_underground_station_entrance_are_left_to_its_script(self):
        # real session 2026-10-13: planned "combined", the segments the station's script makes itself were added twice and the engine answered
        # "Construction Not Possible" on the other game; a free road next to a moved town building stays a street build
        road = "a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 }"
        self.assertEqual(self.ev("PLAN_OF(NATIVE_UNDERGROUND{ x = 1195, y = 513 })"), "construction")
        # one piece of it joined to a node of the world is a street
        joined = ("(function() local P = NATIVE_UNDERGROUND{ x = 100, y = 0 }; local sp = P.proposal; local added = sp.addedSegments; "
                  "local s = added[1]; local c = s.comp; c.node1 = 101; s.comp = c; added[1] = s; sp.addedSegments = added; P.proposal = sp; return P end)()")
        self.assertEqual(self.ev("PLAN_OF(%s)" % joined), "withStreet")
        self.assertEqual(self.ev("PLAN_OF(NATIVE_ROAD_MOVING{ %s, moveFile = 'town/house_a.con', moveTo = { x = 350, y = 40 }, remove = 54 })" % road), "combined")

    def test_the_pieces_a_construction_brings_are_the_ones_its_dry_run_adds_not_a_guess_by_shape(self):
        # sweep run 20261013-064236 (B5): the shape guess ("both ends new nodes within 200 m of the construction") decided which tunnel pieces are the
        # construction's own; the engine's own dry run of the construction now decides, the guess is the fallback
        joined = ("(function() local P = NATIVE_UNDERGROUND{ x = 100, y = 0 }; local sp = P.proposal; local added = sp.addedSegments; "
                  "local s = added[1]; local c = s.comp; c.node1 = 101; s.comp = c; added[1] = s; sp.addedSegments = added; P.proposal = sp; return P end)()")
        pieces = ("SCRIPT_PIECES = function(p) return { { p0 = { x = 130, y = 8, z = -13 }, p1 = { x = 120, y = 5, z = -13 } }, "
                  "{ p0 = { x = 80, y = -5, z = -13 }, p1 = { x = 70, y = -8, z = -13 } } } end")
        self.assertEqual(self.ev("PLAN_OF(%s)" % joined), "withStreet", "the guess alone takes a piece joined to the world for a street")
        self.ev("(function() %s end)()" % pieces)
        self.assertEqual(self.ev("PLAN_OF(%s)" % joined), "construction", "the engine's dry run says both pieces are the construction's own (either way round)")
        # a dry run that adds none of the record's pieces leaves all of them street (nothing is the construction's own)
        self.ev("(function() SCRIPT_PIECES = function(p) return { { p0 = { x = 5000, y = 5000, z = 0 }, p1 = { x = 5010, y = 5000, z = 0 } } } end end)()")
        self.assertEqual(self.ev("PLAN_OF(%s)" % joined), "combined")
        # a dry run that answers with no piece at all falls back to the shape guess
        self.ev("(function() SCRIPT_PIECES = function(p) return {} end end)()")
        self.assertEqual(self.ev("PLAN_OF(NATIVE_UNDERGROUND{ x = 1195, y = 513 })"), "construction")

    def test_the_world_readback_of_a_records_segments_names_the_edge_or_says_there_is_none(self):
        # the trace line both games write after a build (build.pieces), to compare what each ended up with
        code = """(function()
            local M, C, ctx = PURE()
            local P = SIMPLE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }
            local body = M.capture(P, {}, ctx)
            local before = table.concat(M.readPieces(body), '|')
            APPLY_BUILD(P)
            return before .. '##' .. table.concat(M.readPieces(body), '|')
        end)()"""
        before, after = self.ev(code).split("##")
        self.assertIn("NO edge with these ends in this game", before)
        self.assertRegex(after, r"edge \d+ type=\S+ typeIndex=\S+ roadType=\S+ p0=300\.000,0\.000,0\.000 p1=400\.000,0\.000,0\.000")

    def test_a_removed_segment_that_arrives_as_the_bitwise_not_of_its_id_is_the_edge_it_names_not_the_next_one(self):
        # sweep run 20261013-074329 (B1): read after the build, the stop tool's removed segment 39879 came as -39880; taken as -id it was the edge
        # 39880 elsewhere and the replay removed that one
        code = """(function()
            ADD_EDGE(5000, 100, 101, 0, 'street/new_medium_road.street_template')
            ADD_EDGE(5001, 101, 102, 0, 'street/new_medium_road.street_template')
            local M, C, ctx = PURE()
            local P = NATIVE_BULLDOZE{ edges = { 5000 } }
            local sp = P.proposal
            local rm = sp.removedSegments
            local r = rm[1]
            r.entity = -5001
            rm[1] = r
            sp.removedSegments = rm
            P.proposal = sp
            local body = M.capture(P, {}, ctx)
            return body.args[1].rmSegs[1].key
        end)()"""
        self.assertEqual(self.ev(code), "s:5000")

    def test_a_stop_or_signal_takes_its_removed_segment_from_the_added_segments_ends_not_from_the_engines_ids(self):
        # sweep runs 20261013-074329 / -075916: the removed id read after the build was 1 or 3 off, or a neighbour sharing the end node
        code = """(function(reported, drop)
            ADD_EDGE(5003, 120, 100, 0, 'street/new_medium_road.street_template')
            local M, C, ctx = PURE()
            local P = NATIVE_SIGNAL{ edge = 110, param = 0.5 }
            local ok, created = APPLY_BUILD(P)
            local sp = P.proposal
            local rm = sp.removedSegments
            local r = rm[1]
            r.entity = reported
            rm[1] = r
            sp.removedSegments = rm
            P.proposal = sp
            if drop then local e = FIND_EDGE(0, 0, 100, 0); if e then WORLD[e] = nil end end
            local body, why = M.capture(P, { result = created }, ctx)
            if not body then return 'refused: ' .. tostring(why) end
            local ref = body.args[1].rmSegs[1]
            return tostring(ref.key) .. '|' .. tostring(ref.p0.x) .. ',' .. tostring(ref.p1.x)
        end)"""
        for wrong in (110, 111, 5003, 113, 107, -111, -5003):
            self.setUp()   # a world of its own for each
            self.assertEqual(self.L.eval(code)(wrong, False), "nil|0,100", "reported %s: the ends of the added segment decide" % wrong)
        self.setUp()
        self.assertTrue(self.L.eval(code)(111, True).startswith("refused: the segment this build replaces"), "no edge with those ends: the record is refused")

    def test_a_depot_with_its_street_pieces_and_removals_is_rebuilt_as_one_proposal_unless_the_engine_says_it_is_not_possible(self):
        # real run 2026-10-13 (road depot over buildings): construction alone was refused ("Construction Not Possible"), the two-step plan
        # ended with other edges and node configurations than the originator's; the originator applied ONE proposal with the record's flags
        code = """(function(setup)
            local rec = WIRE_RECORD((function() local P = NATIVE_DEPOT_ON_ROAD{ edge = 110, at = { x = 50, y = 0, z = 0 }, door = { x = 50, y = -30, z = 0 } }; P.toRemove = { 56 }; return P end)()).args[1]
            setup()
            local args = DECODE({ fn = 'makeWorldBuildProposalCmd', args = { n = 5, rec, { native = true }, false, true, false } })
            if not args then return 'refused' end
            local sp = STORE(args[1])
            local ssp = STORE(sp.streetProposal)
            return tostring(args.plan) .. '|' .. #sp.constructionsToAdd .. '|' .. #sp.constructionsToRemove .. '|' .. #ssp.nodesToAdd .. '|' .. #ssp.edgesToAdd .. '|' .. #ssp.nodeConfigsToAdd .. '|' .. tostring(args[3]) .. tostring(args[4]) .. '|' .. tostring(args.chain ~= nil)
        end)"""
        f = self.L.eval(code)
        # the engine's verdict is fine: one proposal, everything in it, the record's flags (ignoreErrors false, playerInitiated true), no chain
        got = f(self.L.eval("function() VERDICT_MOCK = true; VERDICT_CRITICAL = nil end")).split("|")
        self.assertEqual(got[0], "combined")
        self.assertEqual((got[1], got[2]), ("1", "1"))
        self.assertGreaterEqual(int(got[3]), 2)
        self.assertGreaterEqual(int(got[4]), 3)
        self.assertGreaterEqual(int(got[5]), 1)
        self.assertEqual((got[6], got[7]), ("falsetrue", "false"))
        # the engine calls the one proposal not possible: the older plans decide
        self.setUp()
        f = self.L.eval(code)
        got = f(self.L.eval("function() VERDICT_MOCK = true; VERDICT_CRITICAL = function(p) local P = PARTS(p); return #P.consAdd > 0 and #P.edgesAdd > 0 end end")).split("|")
        self.assertIn(got[0], ("construction", "withStreet"))
        # no verdict can be read: the older plans decide as before
        self.setUp()
        f = self.L.eval(code)
        got = f(self.L.eval("function() VERDICT_MOCK = nil end")).split("|")
        self.assertIn(got[0], ("construction", "withStreet"))

    def test_a_removed_segment_read_as_a_bare_id_after_the_build_does_not_crash_the_shape_test(self):
        # real session 2026-10-13: "attempt to index local 'rm' (a number value)" while the upgrade tool's applied build was classified
        code = ("(function() local M, C, ctx = PURE(); local body = M.capture(NATIVE_UPGRADE{ edges = { 110 }, template = 'street/new_small_road.street_template' }, {}, ctx); "
                "local rec = body.args[1]; rec.rmSegs[1] = -5; local ok, plan = pcall(M.classify, rec); return ok and 'ok:' .. tostring(plan) or 'raised: ' .. tostring(plan) end)()")
        self.assertEqual(self.ev(code), "ok:street")
        code = ("(function() local M, C, ctx = PURE(); local body = M.capture(NATIVE_BULLDOZE{ edges = { 110 } }, {}, ctx); "
                "local rec = body.args[1]; rec.rmSegs[1] = -5; local ok, plan = pcall(M.classify, rec); return ok and 'ok' or 'raised: ' .. tostring(plan) end)()")
        self.assertEqual(self.ev(code), "ok")

    def test_the_guard_drops_node_configuration_and_object_removals_this_game_has_nothing_to_remove(self):
        # the sweep crash 2026-10-13 (Engine.h:323, BaseNodeConfig): a removal of a node configuration on a node without one aborts the engine.
        # The mock engine aborts the same way; the guard in front of the factory drops what does not exist, in every plan and form.
        code = """(function()
            local rec = WIRE_RECORD(SIMPLE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }).args[1]
            local pos = STORE(STORE(WORLD[101].BASE_NODE).position)
            rec.rmNcfg = { { __ref = 'node', key = 's:101', p = { x = pos.x, y = pos.y, z = pos.z } } }
            local args = DECODE({ fn = 'makeWorldBuildProposalCmd', args = { n = 5, rec, false, false, true, false } })
            local sp = STORE(STORE(args[1]).streetProposal)
            local left = #sp.nodeConfigsToRemove
            local ok, err = pcall(APPLY_BUILD, args[1])
            return tostring(left) .. '|' .. tostring(ok) .. '|' .. tostring(err)
        end)()"""
        self.assertEqual(self.ev(code), "0|true|true")
        # a node that has a configuration keeps its removal
        code = code.replace("local pos =", "WORLD[101].BASE_NODE_CONFIG = D('BaseNodeConfig', { laneConnections = {} }); local pos =")
        self.assertEqual(self.ev(code), "1|true|true")

    def test_the_guard_gives_a_new_junction_node_its_configuration_entry_and_keeps_one_the_record_has(self):
        # the A3 probe of the sweep (2026-10-13): a split with a new node joining three segments and no node configuration entry aborts the engine
        base = """(function()
            local rec = WIRE_RECORD(NATIVE_SPLIT{ edge = 110, at = { x = 50, y = 0, z = 0 }, to = { x = 50, y = 80, z = 0 } }).args[1]
            %s
            local args = DECODE({ fn = 'makeWorldBuildProposalCmd', args = { n = 5, rec, false, false, true, false } })
            local sp = STORE(STORE(args[1]).streetProposal)
            local ok, err = pcall(APPLY_BUILD, args[1])
            return tostring(#sp.nodeConfigsToAdd) .. '|' .. tostring(ok) .. '|' .. tostring(err):sub(1, 20)
        end)()"""
        got = self.ev(base % "rec.ncfg = {}").split("|")
        self.assertEqual(got[0], "1")           # the guard added the default entry
        self.assertEqual(got[1], "true")        # and the engine did not abort

    def test_the_guard_keeps_the_entry_a_record_has_for_its_new_junction_node(self):
        base = """(function()
            local rec = WIRE_RECORD(NATIVE_SPLIT{ edge = 110, at = { x = 50, y = 0, z = 0 }, to = { x = 50, y = 80, z = 0 } }).args[1]
            %s
            local args = DECODE({ fn = 'makeWorldBuildProposalCmd', args = { n = 5, rec, false, false, true, false } })
            local sp = STORE(STORE(args[1]).streetProposal)
            local ok, err = pcall(APPLY_BUILD, args[1])
            return tostring(#sp.nodeConfigsToAdd) .. '|' .. tostring(ok) .. '|' .. tostring(err):sub(1, 20)
        end)()"""
        own = "rec.ncfg = { { node = rec.nodes[1].id, lc = {}, cw = {}, pref = 2 } }"
        got = self.ev(base % own).split("|")
        self.assertEqual(got[:2], ["1", "true"])

    def _stop_with_lane_on_the_replaced_segment(self, extra=""):
        return """(function()
            %s
            local rec = WIRE_RECORD(SIMPLE_STOP{ edge = 110, param = 0.5, left = true }).args[1]
            local pos = STORE(STORE(WORLD[101].BASE_NODE).position)
            local node = { __ref = 'node', key = 's:101', p = { x = pos.x, y = pos.y, z = pos.z } }
            WORLD[101].BASE_NODE_CONFIG = D('BaseNodeConfig', { laneConnections = {} })
            rec.rmNcfg = { node }
            rec.ncfg = { { node = node, lc = { { s0 = rec.rmSegs[1], l0 = 0, s1 = -1, l1 = 0, road = true } }, cw = { rec.rmSegs[1] }, pref = 2 } }
            local args, why = DECODE({ fn = 'makeWorldBuildProposalCmd', args = { n = 5, rec, false, false, true, false } })
            if not args then return 'refused: ' .. tostring(why) end
            local ok, err = APPLY_BUILD(args[1])
            return tostring(ok) .. '|' .. tostring(err) .. '|' .. (CFG_LOG[1] and CFG_LOG[1].lanes[1] or 'none')
        end)()""" % extra

    def test_a_stop_maps_the_lane_connections_of_its_replaced_segment_to_the_replacement(self):
        # sweep run 20261013-060028 (B3): lane connections naming the removed segment by its real key went to the engine and the factory threw
        got = self.ev(self._stop_with_lane_on_the_replaced_segment())
        self.assertTrue(got.startswith("true|"), got)
        self.assertEqual(got.split("|")[2].split(">")[0], "-1")

    def test_the_node_configurations_that_changed_since_a_snapshot_are_a_nodecfg_record(self):
        code = ("(function() local M, C, ctx = PURE(); "
                "WORLD[101].BASE_NODE_CONFIG = D('BaseNodeConfig', { laneConnections = {} }); "
                "local before = M.nodeConfigSnapshot(); "
                "local none, why = M.nodeConfigDelta(before, ctx); "
                "local lc = D('LaneConnection', { segment0 = 110, lane0 = 0, segment1 = 111, lane1 = 0, withRoad = true }); "
                "WORLD[101].BASE_NODE_CONFIG = D('BaseNodeConfig', { laneConnections = { lc } }); "
                "local body = M.nodeConfigDelta(before, ctx); "
                "return tostring(none) .. '/' .. tostring(why) .. '/' .. tostring(body and M.classify(body.args[1])) .. '/' .. tostring(body and #body.args[1].ncfg) end)()")
        self.assertEqual(self.ev(code), "nil/no node configuration changed/nodecfg/1")

    def test_permission_category_follows_what_the_proposal_takes_and_gives(self):
        def cat(code):
            return self.ev("(function() local M = PURE(); return M.commands.makeWorldBuildProposalCmd.categoryFor({ n = 4, %s }) end)()" % code)

        for code in ("SIMPLE_ROAD{ a = { x = 1, y = 2, z = 0 }, b = { x = 5, y = 6, z = 0 } }", "NATIVE_UPGRADE{ edges = { 110 }, template = 'street/new_small_road.street_template' }",
                     "NATIVE_SIGNAL{ edge = 130, param = 0.5 }", "CON_PROPOSAL{ file = 'depot/road_depot.con', x = 1, y = 2 }",
                     "CON_PROPOSAL{ file = 'depot/road_depot.con', x = 1, y = 2, remove = 54 }", "NATIVE_SPLIT{ edge = 110, at = { x = 50, y = 0, z = 0 }, to = { x = 50, y = 80, z = 0 } }",
                     "7"):
            self.assertEqual(cat(code), "build", code)
        for code in ("NATIVE_BULLDOZE{ edges = { 110 } }", "CON_REMOVE{ cons = { 54 } }", "NATIVE_BULLDOZE{ cons = { 54 } }", "NATIVE_BULLDOZE{ edges = { 110 }, cons = { 54 } }"):
            self.assertEqual(cat(code), "bulldoze", code)
        self.assertEqual(self.ev("PURE().commands.makeWorldReplaceTerrainCmd.category"), "build")

    def test_a_record_is_plain_data_that_survives_the_wire(self):
        for code in ("SIMPLE_ROAD{ a = { x = 300.123456789, y = 0, z = 0.5 }, b = { x = 400, y = 0, z = 0 } }", "NATIVE_UPGRADE{ edges = { 110 }, template = 'street/new_small_road.street_template', deco = { { 12, true } } }",
                     "CON_PROPOSAL{ file = 'depot/road_depot.con', x = 1.25, y = 2, params = { a = 1, b = { c = 'x' } } }"):
            self.assertTrue(self.ev("(function() local M, C, ctx = PURE(); local body = CAPTURE(%s); local text = C.serialize(body); "
                                    "return text == C.serialize(C.parse(text)) and not text:find('userdata') and #text end)()" % code), code)
        # a position keeps every digit
        self.assertEqual(self.ev("(function() local rec = WIRE_RECORD(SIMPLE_ROAD{ a = { x = 300.123456789, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }); "
                                 "return rec.args[1].nodes[1].pos.x end)()"), 300.123456789)

    def test_a_proposal_that_is_not_plain_data_cannot_go_on_the_wire(self):
        self.assertEqual(self.ev("(function() local P = CON_PROPOSAL{ file = 'depot/road_depot.con', x = 1, y = 2, params = { f = print } }; "
                                 "local body, why = CAPTURE(P); return tostring(body) .. ':' .. tostring(why):sub(1, 60) end)()").split(":")[0], "nil")

    def test_the_kind_of_a_scripts_edge_object_comes_from_its_placeholder(self):
        got = self.ev("(function() local out = {}; for _, k in ipairs({ 0, 1, 2 }) do "
                      "local rec = WIRE_RECORD(SIMPLE_STOP{ edge = 130, param = 0.5, kind = k }); out[#out + 1] = rec.args[1].objs[1].kind end; "
                      "return table.concat(out, ',') end)()")
        self.assertEqual(got, "0,1,2")
        bad = self.ev("(function() local rec, M = WIRE_RECORD(SIMPLE_STOP{ edge = 130, param = 0.5, kind = 2 }); rec = rec.args[1]; rec.objs[1].kind = 'x'; "
                      "local ok, why = M.normalise(rec); return tostring(ok) .. '|' .. tostring(why) end)()")
        self.assertEqual(bad, "nil|edge object 1: bad kind")

    def test_owners_follow_the_acting_player_and_another_owner_travels_as_a_key(self):
        # docs/COMPANIES.md 3.7: a decode hook takes every owner from ctx.player, never from the local player
        got = self.ev("""(function()
          local M, C, ctx = PURE()
          local function resolve(k) local n = tostring(k):match('^s:(%d+)$'); return n and tonumber(n) or nil end
          local function wire(P) return C.parse(C.serialize(M.capture(P, { ctx = CONTEXT(true, false, true, true) }, ctx))) end
          local function decodeWith(body, player)
            local args, why = M.commands.makeWorldBuildProposalCmd.decode(body, { player = player, resolve = resolve })
            if not args then return 'refused:' .. tostring(why) end
            return tostring(args[2].player) .. '/' .. tostring(args[1].constructionsToAdd[1].playerEntity)
          end
          local own = wire(CON_PROPOSAL{ file = 'depot/road_depot.con', x = 1, y = 2 })
          local foreign = wire(CON_PROPOSAL{ file = 'depot/road_depot.con', x = 1, y = 2, owner = 8 })
          local nobody = wire(CON_PROPOSAL{ file = 'depot/road_depot.con', x = 1, y = 2, owner = -1 })
          local lost = wire(CON_PROPOSAL{ file = 'depot/road_depot.con', x = 1, y = 2, owner = 8 })
          lost.args[1].cons[1].owner = { __ref = 'owner', key = '1:5#c1' }
          local bad = wire(CON_PROPOSAL{ file = 'depot/road_depot.con', x = 1, y = 2 })
          bad.args[1].cons[1].owner = { __ref = 'owner', key = 5 }
          local ok, why = M.normalise(bad.args[1])
          return table.concat({ decodeWith(own, 7), decodeWith(own, 99), decodeWith(foreign, 99), decodeWith(nobody, 99), decodeWith(lost, 99),
            tostring(ok) .. ':' .. tostring(why), tostring(own.args[1].cons[1].owner), tostring(foreign.args[1].cons[1].owner.key) }, ',')
        end)()""")
        self.assertEqual(got, "7/7,99/99,99/8,99/-1,refused:unresolved owner 1:5#c1,nil:construction 1: bad owner,player,s:8")

    def test_normalise_refuses_what_it_cannot_trust(self):
        def refuse(mutation):
            return self.ev("(function() local rec, M = WIRE_RECORD(SIMPLE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }); rec = rec.args[1]; "
                           "%s; local ok, why = M.normalise(rec); return tostring(ok) .. '|' .. tostring(why) end)()" % mutation)

        self.assertTrue(refuse("rec.v = 2").startswith("nil|build record version 2"))
        self.assertTrue(refuse("rec.segs[1].c.roadTemplate = 7").startswith("nil|segment 1: bad roadTemplate"))
        self.assertTrue(refuse("rec.segs[1].n0 = 5").startswith("nil|segment 1: bad entry"), "an existing node must be a reference")
        self.assertTrue(refuse("rec.nodes[1].pos.x = 1/0").startswith("nil|node 1: bad entry"))
        self.assertTrue(refuse("rec.segs[1].objects = { { 7, 1 } }").startswith("nil|segment 1: bad object"))
        self.assertTrue(refuse("rec.segs[1].lanes = 'x'").startswith("nil|segment 1: bad lanes"))
        self.assertTrue(refuse("rec.segs[1].owner = 'root'").startswith("nil|segment 1: bad owner"))
        self.assertTrue(refuse("rec.cons = { { file = 'a.con', transf = {1} } }").startswith("nil|construction 1: bad transform"))
        self.assertTrue(refuse("rec.rem = { 54 }").startswith("nil|removed construction 1: bad reference"))
        self.assertTrue(refuse("rec.segs[1].c.deco = { f = print }").startswith("nil|segment 1: bad decorations"))
        self.assertTrue(refuse("local deep = {}; local t = deep; for i = 1, 20 do t.x = {}; t = t.x end; rec.cons = { { file = 'a.con', transf = { 1,0,0,0,0,1,0,0,0,0,1,0,0,0,0,1 }, params = deep } }")
                        .startswith("nil|construction 1: bad parameters"))
        self.assertTrue(refuse("rec.v = 1").startswith("table: "), "an untouched record is accepted")

    def test_encode_and_decode_never_raise(self):
        got = self.ev("(function() local M = PURE(); local hostile = setmetatable({}, { __index = function() error('boom') end }); "
                      "local spec = M.commands.makeWorldBuildProposalCmd; "
                      "local a, why = spec.decode({ fn = 'makeWorldBuildProposalCmd', args = { n = 5, hostile } }, { player = 7, resolve = function() end }); "
                      "local b, why2 = spec.encode('makeWorldBuildProposalCmd', { n = 1, hostile }, { plain = function(v) return v end, keyOf = function() return 's:1' end }); "
                      "local c, why3 = spec.encode('makeWorldBuildProposalCmd', { n = 1, setmetatable({}, { __index = function() error('boom') end, __len = function() error('boom') end }) }, "
                      "{ plain = error, keyOf = error }); "
                      "return tostring(a) .. '|' .. tostring(why):sub(1, 40) .. '|' .. tostring(b) .. '|' .. tostring(c) end)()")
        self.assertTrue(got.startswith("nil|the build record could not be read: "), got)
        self.assertTrue(got.endswith("|nil|nil"), got)

    def test_the_model_of_an_edge_object_is_named_without_the_resource_prefix_unless_the_engine_wants_it(self):
        def model(reject):
            return self.ev("(function() FACTORY_REJECT = %s; local P = NATIVE_SIGNAL{ edge = FIND_EDGE(0, 50, 100, 50), param = 0.6, model = '::/signal/signal_b.mdl' }; "
                           "local ok, created = APPLY_BUILD(P); local body = CAPTURE(P, { result = created }); "
                           "local args, why = DECODE(body); FACTORY_REJECT = nil; "
                           "if not args then return 'refused: ' .. tostring(why) end "
                           "return STORE(STORE(STORE(args[1]).streetProposal).edgeObjectsToAdd[1]).model end)()" % reject)

        self.assertEqual(model("nil"), "signal/signal_b.mdl")
        self.assertEqual(model("function(parts, kind) if kind == 'SimpleProposal' then for _, o in ipairs(parts.objsAdd) do if not o.model:find('^::/') then return 'prefix wanted' end end end end"),
                         "::/signal/signal_b.mdl", "the second form keeps the name as read")
        self.assertTrue(model("function(parts, kind) if kind == 'SimpleProposal' then return 'no' end end").startswith("refused: the engine's command factory refused the stop build"))

    def test_default_seed_and_name_of_a_construction(self):
        got = self.ev("(function() local body = CAPTURE(CON_PROPOSAL{ file = 'station/rail/modular_station.con', x = 300, y = 200, z = 5, params = {}, name = '' }); "
                      "body.args[1].cons[1].name = nil; local args = DECODE(body); local ce = STORE(args[1]).constructionsToAdd[1]; "
                      "return STORE(ce).params.seed .. '|' .. STORE(ce).name .. '|' .. STORE(ce).playerEntity end)()")
        self.assertEqual(got, "2705|modular_station|7", "7x + 3y + z; the file's base name; the local player")

    def test_the_captured_context_and_flags_travel(self):
        self.assertEqual(self.ev("(function() local body = CAPTURE(SIMPLE_ROAD{ a = { x = 1, y = 2, z = 0 }, b = { x = 5, y = 6, z = 0 } }, "
                                 "{ ctx = CONTEXT(true, false, true, false), ignoreErrors = true, playerInitiated = false }); local a = body.args; "
                                 "return tostring(a[2].align) .. tostring(a[2].cleanup) .. tostring(a[2].buildings) .. tostring(a[2].fields) .. '|' .. tostring(a[3]) .. tostring(a[4]) end)()"),
                         "truefalsetruefalse|truefalse")
        self.assertEqual(self.ev("(function() local body = CAPTURE(NATIVE_ROAD{ a = { x = 1, y = 2, z = 0 }, b = { x = 5, y = 6, z = 0 } }); "
                                 "return tostring(body.args[2].native) .. tostring(body.args[3]) .. tostring(body.args[4]) end)()"), "truefalsetrue",
                         "a tool's build has no context a script can see: the plan's default is used, flags as the tools build")
        self.assertEqual(self.ev("tostring(CAPTURE(SIMPLE_ROAD{ a = { x = 1, y = 2, z = 0 }, b = { x = 5, y = 6, z = 0 } }).args[2])"), "false")

    def test_custom_lanes_travel_only_when_they_are_not_the_templates(self):
        base = "NATIVE_UPGRADE{ edges = { 110 }, template = 'street/tram_road.street_template' }"
        self.assertEqual(self.ev("tostring(WIRE_RECORD(%s).args[1].segs[1].lanes)" % base), "nil")
        custom = ("(function() local P = NATIVE_UPGRADE{ edges = { 110 }, template = 'street/new_small_road.street_template' }; local sp = P.proposal; "
                  "local added = sp.addedSegments; local s = added[1]; local c = s.comp; local lanes = c.laneConfigs; lanes[3] = D('LaneConfig', { forward = true }); "
                  "c.laneConfigs = lanes; s.comp = c; added[1] = s; sp.addedSegments = added; P.proposal = sp; return P end)()")
        self.assertEqual(self.ev("#WIRE_RECORD(%s).args[1].segs[1].lanes" % custom), 3)

    def test_nodes_are_found_whichever_way_the_octree_answers(self):
        modes = {"node": "octree-node", "edge": "octree-edge", "none": "scan"}
        for i, (octree, want) in enumerate(modes.items()):
            x = 1000 + 100 * i
            got = self.ev("(function() OCTREE = '%s'; local M = PURE(); APPLY_BUILD(SIMPLE_ROAD{ a = { x = %d, y = 500, z = 0 }, b = { x = %d, y = 500, z = 0 } }); "
                          "local R = M.resolver({ resolve = function() return nil end }); local n = R.node({ __ref = 'node', p = { x = %d, y = 500, z = 0 } }); "
                          "local e = R.edge({ __ref = 'edge', p0 = { x = %d, y = 500, z = 0 }, p1 = { x = %d, y = 500, z = 0 } }); "
                          "return M.adapter.nodeMode .. ':' .. tostring(n == FIND_NODE(%d, 500, 0)) .. ':' .. tostring(e == FIND_EDGE(%d, 500, %d, 500)) end)()"
                          % (octree, x, x + 50, x + 50, x + 50, x, x + 50, x, x + 50))
            self.assertEqual(got, "%s:true:true" % want, octree)

    def test_a_segment_is_found_whichever_way_round_it_is_described(self):
        got = self.ev("(function() local M = PURE(); local R = M.resolver({ resolve = function() return nil end }); "
                      "local e = R.edge({ __ref = 'edge', p0 = { x = 100, y = 0, z = 0 }, p1 = { x = 0, y = 0, z = 0 }, t0 = { x = -100, y = 0, z = 0 }, t1 = { x = -100, y = 0, z = 0 } }); "
                      "return e end)()")
        self.assertEqual(got, 110)

    def test_two_segments_between_the_same_nodes_are_told_apart_by_their_tangents_or_refused(self):
        setup = ("ADD_NODE(3000, 3000, 0, 0); ADD_NODE(3001, 3100, 0, 0); "
                 "ADD_EDGE(3010, 3000, 3001, 0, 'street/new_medium_road.street_template', { tangent0 = V(100, 0, 0), tangent1 = V(100, 0, 0) }); "
                 "ADD_EDGE(3011, 3000, 3001, 0, 'street/new_medium_road.street_template', { tangent0 = V(70, 70, 0), tangent1 = V(70, -70, 0) })")
        self.L.execute(setup)
        res = self.ev("(function() local M = PURE(); local R = M.resolver({ resolve = function() return nil end }); "
                      "local function ref(t0, t1) return { __ref = 'edge', p0 = { x = 3000, y = 0, z = 0 }, p1 = { x = 3100, y = 0, z = 0 }, t0 = t0, t1 = t1 } end; "
                      "local a = R.edge(ref({ x = 100, y = 0, z = 0 }, { x = 100, y = 0, z = 0 })); "
                      "local b = R.edge(ref({ x = 70, y = 70, z = 0 }, { x = 70, y = -70, z = 0 })); "
                      "local ok, why = pcall(R.edge, { __ref = 'edge', p0 = { x = 3000, y = 0, z = 0 }, p1 = { x = 3100, y = 0, z = 0 } }); "
                      "return a .. ',' .. b .. ',' .. tostring(ok) .. ',' .. tostring(why) end)()")
        self.assertEqual(res.split(",")[:3], ["3010", "3011", "false"])
        self.assertIn("ambiguous segment", res)

    def test_a_construction_is_found_by_file_and_place_and_not_by_a_wrong_key(self):
        got = self.ev("(function() local M = PURE(); "
                      "local R = M.resolver({ resolve = function(k) return 54 end }); "
                      "local a = R.con({ __ref = 'con', key = 's:54', file = 'depot.con', p = { x = 100, y = 200, z = 5 } }); "
                      "local R2 = M.resolver({ resolve = function(k) return 56 end }); "
                      "local b = R2.con({ __ref = 'con', key = 's:56', file = 'depot.con', p = { x = 100.4, y = 200, z = 5 } }); "
                      "local okFile, whyFile = pcall(R.con, { __ref = 'con', file = 'depot.con', p = { x = 500, y = 200, z = 5 } }); "
                      "return a .. ',' .. b .. ',' .. tostring(okFile) end)()")
        self.assertEqual(got, "54,54,false")

    def test_constructions_are_found_by_place_when_the_octree_has_nothing_for_them(self):
        got = self.ev("(function() OCTREE_CONS_EMPTY = true; local M = PURE(); local R = M.resolver({ resolve = function() return nil end }); "
                      "local e = R.con({ __ref = 'con', file = 'station.con', p = { x = -40.5, y = 12, z = 0 } }); OCTREE_CONS_EMPTY = nil; return e end)()")
        self.assertEqual(got, 56)

    def test_an_object_is_named_by_its_rank_on_its_segment(self):
        got = self.ev("(function() local M = PURE(); local R = M.resolver({ resolve = function() return nil end }); "
                      "local edge = { __ref = 'edge', p0 = { x = 0, y = 50, z = 0 }, p1 = { x = 100, y = 50, z = 0 } }; "
                      "local o = R.obj({ __ref = 'obj', edge = edge, i = 1 }); local ok, why = pcall(R.obj, { __ref = 'obj', edge = edge, i = 2 }); "
                      "return o .. ',' .. tostring(ok) .. ',' .. tostring(why) end)()")
        self.assertTrue(got.startswith("140,false,"), got)
        self.assertIn("unresolved object 2 of segment", got)

    def test_keys_go_to_the_constructions_that_match_not_to_decoys(self):
        self.L.execute("APPLY_BUILD(CON_PROPOSAL{ file = 'depot/road_depot.con', x = 500, y = 100 }); APPLY_BUILD(CON_PROPOSAL{ file = 'depot/road_depot.con', x = 900, y = 900 })")
        got = self.ev("(function() local M, C, ctx = PURE(); local body = CAPTURE(CON_PROPOSAL{ file = 'depot/road_depot.con', x = 500, y = 100 }); "
                      "local made = {}; for e, w in pairs(WORLD) do if w.CONSTRUCTION and e >= 1000 then made[#made + 1] = e end end; table.sort(made); "
                      "local map, complete = M.commands.makeWorldBuildProposalCmd.bind(body, '1:7', made); "
                      "local keys = {}; for k in pairs(map) do keys[#keys + 1] = k end; table.sort(keys); "
                      "local none, completeNone = M.commands.makeWorldBuildProposalCmd.bind(body, '1:8', {}); "
                      "return table.concat(keys, ' ') .. '|' .. tostring(complete) .. '|' .. tostring(next(none)) .. '|' .. tostring(completeNone) .. '|' .. "
                      "tostring(map['1:7#c1'] == FIND_CON('depot/road_depot.con') - 1 or map['1:7#c1']) end)()")
        keys, complete, none, complete_none, _ = got.split("|")
        self.assertEqual(keys, "1:7#c1 1:7#c1/d1")
        self.assertEqual((complete, none, complete_none), ("true", "nil", "false"))

    def test_tool_events_are_logged_once_per_kind_and_a_capture_is_summarised(self):
        said = self.ev("(function() local M = PURE(); local said = {}; local function say(m) said[#said + 1] = m end; "
                       "local P = SIMPLE_ROAD{ a = { x = 1, y = 2, z = 0 }, b = { x = 5, y = 6, z = 0 } }; "
                       "M.onToolEvent('streetBuilder', 'builder.proposalCreate', { P }, say); M.onToolEvent('streetBuilder', 'builder.proposalCreate', { P }, say); "
                       "M.onToolEvent('streetBuilder', 'builder.proposalApply', { P, nil, {} }, say); "
                       "M.onToolEvent('streetBuilder', 'builder.proposalApply', { D('SimpleProposal') }, say); "
                       "return table.concat(said, '#') end)()")
        parts = said.split("#")
        self.assertEqual(parts.count("tool event streetBuilder/builder.proposalCreate"), 1, "each kind of event once")
        self.assertEqual(parts.count("tool event streetBuilder/builder.proposalApply"), 1)
        self.assertTrue(any(p.startswith("tool build streetBuilder/builder.proposalApply: street, 0+0 constructions, 2+0 nodes, 1+0 segments, 0+0 objects") for p in parts), parts)
        self.assertTrue(any(p.startswith("segment members: node0,node1") for p in parts), parts)
        self.assertTrue(any(p.startswith("node members: position") for p in parts))
        self.assertTrue(any("not capturable: nothing to build" in p for p in parts))

    def test_previews_of_a_tool_are_rebuilt_as_a_dry_run_and_the_result_logged(self):
        said = self.ev("(function() local M = PURE(); local said = {}; local function say(m) said[#said + 1] = m end; "
                       "for i = 1, 12 do M.onToolEvent('streetBuilder', 'builder.proposalCreate', { NATIVE_SPLIT{ edge = 110, at = { x = 50, y = 0, z = 0 }, to = { x = 50, y = 80, z = 0 } }, {} }, say) end; "
                       "M.onToolEvent('streetTerminalBuilder', 'builder.proposalCreate', { NATIVE_SIGNAL{ edge = 130, param = 0.5 }, {} }, say); "
                       "M.onToolEvent('bulldozer', 'builder.proposalCreate', { NATIVE_BULLDOZE{ edges = { 110 } }, {} }, say); "
                       "M.onToolEvent('constructionBuilder', 'builder.proposalCreate', { NATIVE_DEPOT_ON_ROAD{ edge = 110, at = { x = 50, y = 0, z = 0 }, door = { x = 50, y = -30, z = 0 } }, {} }, say); "
                       "return table.concat(said, '#') end)()")
        lines = said.split("#")
        previews = [l for l in lines if l.startswith("tool preview streetBuilder: street rebuilds (")]
        self.assertEqual(len(previews), 2, "two per tool and plan, not one per frame")
        self.assertTrue(previews[0].endswith(" bytes)"), previews[0])
        self.assertIn("tool preview streetTerminalBuilder: not capturable: an edge object has no model (captured before the build ran?)", lines)
        self.assertTrue(any(l.startswith("tool preview bulldozer: removal rebuilds (") for l in lines), lines)
        self.assertTrue(any(l.startswith("tool preview constructionBuilder: withStreet rebuilds (") for l in lines), lines)

    def test_engine_values_dump_to_plain_data(self):
        got = self.ev("(function() local M = PURE(); local A = M.adapter; local m = D('Mat4f'); STORE(m)[13] = 5; "
                      "local v = A.dump(V(1, 2, 3)); local mm = A.dump(m); "
                      "local P = NATIVE_BULLDOZE{ cons = { 54, 56 } }; local l = A.list(P, 'toRemove'); "
                      "return v.x .. v.y .. v.z .. '|' .. #mm .. '|' .. mm[13] .. '|' .. #l .. '|' .. tostring(A.isObject(V(1, 2, 3))) .. tostring(A.isObject({})) end)()")
        self.assertEqual(got, "123|16|5|2|truefalse")

    def test_the_lane_write_shift_is_measured_and_compensated(self):
        for shift in (0, 1):
            got = self.ev("(function() LANE_SHIFT = %d; local M = PURE(); local A = M.adapter; local measured = A.laneShift(); "
                          "local lanes = A.rebuildLanes({ { forward = true, speed = 5, width = 2, height = 0, offset = 0, modes = { 0, 3, 15 } } }); "
                          "local back = A.readLane(lanes[1]); LANE_SHIFT = 0; return measured .. '|' .. table.concat(back.modes, ',') end)()" % shift)
            self.assertEqual(got, "%d|0,3,15" % shift, "shift %d" % shift)


# ---------------------------------------------------------------- the additions to the lockstep module


class LockstepAdditionsTest(unittest.TestCase):
    def setUp(self):
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.K = self.L.execute(T.LOCKSTEP_SRC)

    def test_a_schema_can_decide_the_category_per_command(self):
        # categoryFor is the lockstep core's own hook (main); the build schema uses it for "a proposal that only removes"
        got = self.L.eval("""function(K)
          K.registerCommands({
            byContent = { category = "build", categoryFor = function(args) return args[1] and "bulldoze" or nil end },
            fixed = { category = "spend" },
          })
          return K.categoryOf("byContent", { true }) .. K.categoryOf("byContent", { false }) .. K.categoryOf("byContent") ..
                 K.categoryOf("fixed") .. K.categoryOf("makeSomethingBuyCmd")
        end""")(self.K)
        self.assertEqual(got, "bulldozebuildbuild" + "spend" + "spend")

    def test_commands_of_one_action_are_sent_in_order(self):
        got = self.L.eval("""function(K)
          local sent = {}
          local args = { n = 1, "first", chain = { { n = 1, "second" }, function() return { n = 1, "third" } end } }
          K.sendNow(args, function(a) sent[#sent + 1] = a[1] end)
          local ok, err = pcall(K.sendNow, { n = 1, "x", chain = { function() return nil end } }, function(a) sent[#sent + 1] = a[1] end)
          return table.concat(sent, ",") .. "|" .. tostring(ok) .. "|x" .. (#sent == 4 and "sent" or "")
        end""")(self.K)
        self.assertTrue(got.startswith("first,second,third,x|false|"), got)

    def test_chained_commands_wait_for_each_result_and_stop_at_a_failure(self):
        got = self.L.eval("""function(K)
          local log = {}
          local function run(args, outcomes)
            local i = 0
            local result
            K.runChained(args, function(a, cb) i = i + 1; log[#log + 1] = a[1]; cb({ id = a[1] }, outcomes[i] ~= false, { { i, 0 } }) end,
              function(data, success, entities) result = tostring(data and data.id) .. ":" .. tostring(success) end)
            return result
          end
          local r1 = run({ n = 1, "a", chain = { { n = 1, "b" }, function() return { n = 1, "c" } end } }, {})
          local r2 = run({ n = 1, "d", chain = { { n = 1, "e" }, { n = 1, "f" } } }, { true, false })
          local r3 = run({ n = 1, "g", chain = { function() return nil end } }, {})
          local r4 = run({ n = 1, "h", chain = { function() error("no") end } }, {})
          local r5 = run({ n = 1, "j" }, {})
          return table.concat(log, "") .. "|" .. r1 .. "|" .. r2 .. "|" .. r3 .. "|" .. r4 .. "|" .. r5
        end""")(self.K)
        self.assertEqual(got, "abcdegh" + "j" + "|c:true|e:false|g:false|h:false|j:true")

    def test_new_entities_are_the_ones_that_were_not_there_before_in_order(self):
        got = self.L.eval("function(K) return table.concat(K.newEntities({ 5, 3 }, { 9, 3, 5, 7, 4 }), ',') end")(self.K)
        self.assertEqual(got, "4,7,9")

    def test_binding_hooks_are_optional_and_cannot_break_the_apply(self):
        got = self.L.eval("""function(K)
          local spec = { bindsComp = function(body) return body.cons and "CONSTRUCTION" or nil end,
                         bind = function(body, key, made) return { [key .. "#c1"] = made[1] }, #made > 0 end }
          local function names(c) return c and table.concat(c, "+") or "nil" end
          local a = names(K.bindsComp(spec, { cons = {} })) .. "," .. names(K.bindsComp(spec, {})) .. "," .. names(K.bindsComp({}, {})) .. "," ..
                    names(K.bindsComp({ bindsComp = "LINE" }, {})) .. "," .. names(K.bindsComp({ bindsComp = { "EDGE_OBJECT", 7, "STATION_GROUP" } }, {})) .. "," ..
                    names(K.bindsComp({ bindsComp = {} }, {})) .. "," .. names(K.bindsComp({ bindsComp = function() error("boom") end }, {}))
          local map, complete = K.bindMap(spec, {}, "1:2", { 77 })
          local none, incomplete = K.bindMap(spec, {}, "1:2", {})
          local bad, badComplete = K.bindMap({ bind = function() error("boom") end }, {}, "1:2", {})
          local plain, plainComplete = K.bindMap({}, {}, "1:2", {})
          return a .. "|" .. map["1:2#c1"] .. tostring(complete) .. "|" .. tostring(next(none)) .. tostring(incomplete) .. "|" .. tostring(next(bad)) .. tostring(badComplete) .. "|" .. tostring(next(plain)) .. tostring(plainComplete)
        end""")(self.K)
        self.assertEqual(got, "CONSTRUCTION,nil,nil,LINE,EDGE_OBJECT+STATION_GROUP,nil,nil|77true|nilfalse|nilfalse|niltrue")


def portable_road_text(ax=300.0, bx=400.0, template="street/new_medium_road.street_template", segs=1, extra=None, tangent=None):
    """The input-level text of the road NATIVE_ROAD{ a = (ax, 0), b = (bx, 0) } as the native module writes it (placeholders as the mock numbers them)."""
    import struct as _s

    def f(x):
        return "%08x" % _s.unpack("<I", _s.pack("<f", x))[0]

    def v(x, y, z):
        return "v3:%s,%s,%s" % (f(x), f(y), f(z))

    rows = [
        ("proposal.addedNodes.#", "n:2"), ("proposal.addedNodes[0].entity", "ent:new#1"), ("proposal.addedNodes[0].position", v(ax, 0, 0)),
        ("proposal.addedNodes[1].entity", "ent:new#2"), ("proposal.addedNodes[1].position", v(bx, 0, 0)),
        ("proposal.addedSegments.#", "n:%d" % segs), ("proposal.addedSegments[0].entity", "ent:new#3"),
        ("proposal.addedSegments[0].node0", "ent:new#1"), ("proposal.addedSegments[0].node1", "ent:new#2"),
        ("proposal.addedSegments[0].position0", v(ax, 0, 0)), ("proposal.addedSegments[0].position1", v(bx, 0, 0)),
        ("proposal.addedSegments[0].tangent0", v(tangent or bx - ax, 0, 0)), ("proposal.addedSegments[0].tangent1", v(tangent or bx - ax, 0, 0)),
        ("proposal.addedSegments[0].baseType", "u32:00000000"), ("proposal.addedSegments[0].typeIndex", "i32:00000001"),
        ("proposal.addedSegments[0].roadType", "u32:00000000"), ("proposal.addedSegments[0].type", "u32:00000000"),
        ("proposal.addedSegments[0].roadTemplate", 'res:"%s"' % template), ("proposal.addedSegments[0].roadStyle", 'res:"medium"'),
        ("toRemove.#", "n:0"), ("toAdd.#", "n:0"), ("kind", "u8:34"),
    ] + (extra or [])
    return chr(10).join(k + chr(9) + val for k, val in rows)


class PortableReplayBase:
    """FEVERLINK3_PORTABLE_REPLAY: a build that comes with the native module's text is replayed through it (default off)."""

    switch = None

    def setUp(self):
        self._old = os.environ.get("FEVERLINK3_PORTABLE_REPLAY")
        if self.switch is None:
            os.environ.pop("FEVERLINK3_PORTABLE_REPLAY", None)
        else:
            os.environ["FEVERLINK3_PORTABLE_REPLAY"] = self.switch
        super().setUp()

    def tearDown(self):
        super().tearDown()
        if self._old is None:
            os.environ.pop("FEVERLINK3_PORTABLE_REPLAY", None)
        else:
            os.environ["FEVERLINK3_PORTABLE_REPLAY"] = self._old

    def decoded(self, text, proposal="NATIVE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }"):
        code = """(function(txt)
            local body = WIRE_RECORD(%s)
            body.input = txt
            local args = DECODE(body)
            if not args then return 'refused' end
            local sp = STORE(args[1])
            local ssp = STORE(sp.streetProposal)
            return tostring(args.plan) .. '|' .. #ssp.nodesToAdd .. '|' .. #ssp.edgesToAdd
        end)""" % proposal
        return str(self.L.eval(code)(text))


class PortableReplayOffTest(PortableReplayBase, PureBuildTest):
    def test_off_by_default_the_record_path_builds_it(self):
        self.assertEqual(self.decoded(portable_road_text()), "street|2|1")


class PortableReplayOnTest(PortableReplayBase, PureBuildTest):
    switch = "1"

    def test_a_road_is_built_from_the_text_as_one_proposal(self):
        self.assertEqual(self.decoded(portable_road_text()), "portable|2|1")

    def test_the_text_decides_the_geometry(self):
        # the text says the tangent is 123: the engine is given the text's geometry, not the record's
        code = """(function(txt)
            local body = WIRE_RECORD(NATIVE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } })
            body.input = txt
            local args = DECODE(body)
            local ssp = STORE(STORE(args[1]).streetProposal)
            local c = STORE(STORE(ssp.edgesToAdd[1]).comp)
            return tostring(args.plan) .. '|' .. c.tangent0.x
        end)"""
        got = str(self.L.eval(code)(portable_road_text(tangent=123.0)))
        self.assertEqual(got, "portable|123")

    def test_a_node_the_text_does_not_have_where_the_record_has_it_falls_back(self):
        self.assertEqual(self.decoded(portable_road_text(bx=450.0)), "street|2|1")

    def test_the_depot_text_with_other_numbering_than_the_record_is_matched_by_identity(self):
        # real run 2026-10-13 (b22): the engine numbers ONE sequence across nodes, segments and the construction (the third node is -6), the
        # record numbers its own. The numbers are labels: nodes are matched by position, segments by the ends they join, the construction by
        # file and placement; the new proposal numbers nodes and segments from -1 each, and what refers to them follows.
        text = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures", "depot_input.txt"), encoding="utf-8").read()
        code = """(function(txt)
            local M = PURE()
            local P = M.portable
            local tree = P.parse(txt)
            local pt = tree.proposal
            local function pos(e) return { x = e.x, y = e.y, z = e.z } end
            local oldPos = {}
            for _, s in ipairs(P.list(pt.addedSegments)) do
                for _, k in ipairs({ 'node0', 'node1' }) do if s[k].old then oldPos[s[k].old] = pos(s[k].pos) end end
            end
            local rec = { v = 1, src = 'native', cons = {}, rem = {}, old2new = {}, nodes = {}, segs = {}, rmNodes = {}, rmSegs = {}, objs = {}, rmObjs = {}, ncfg = {}, rmNcfg = {} }
            local nodeId = {}
            for i, tn in ipairs(P.list(pt.addedNodes)) do
                nodeId[tn.entity.new] = -100 - i
                rec.nodes[i] = { id = -100 - i, pos = pos(tn.position) }
            end
            local function end_(e) if e.new then return nodeId[e.new] end return { __ref = 'node', p = pos(e.pos) } end
            for i, ts in ipairs(P.list(pt.addedSegments)) do
                local own = ts.roadTemplate:find('/constructions/', 1, true)
                rec.segs[i] = { id = -200 - i, n0 = end_(ts.node0), n1 = end_(ts.node1), type = 0, owner = 'player',
                    c = own and { roadTemplate = '::/' .. ts.roadTemplate } or { roadTemplate = '::/' .. ts.roadTemplate, roadStyle = '::/' .. ts.roadStyle, typeIndex = -1, roadType = 1,
                        p0 = pos(ts.position0), p1 = pos(ts.position1) } }
            end
            for i = 1, pt.removedNodes.n do rec.rmNodes[i] = { __ref = 'node', p = pos(pt.removedNodes[i].position) } end
            for i = 1, pt.removedSegments.n do rec.rmSegs[i] = { __ref = 'edge', p0 = pos(pt.removedSegments[i].position0), p1 = pos(pt.removedSegments[i].position1) } end
            for i = 1, pt.nodeConfigsToAdd.n do
                local e = pt.nodeConfigsToAdd[i].entity
                rec.ncfg[i] = { node = e.new and nodeId[e.new] or { __ref = 'node', p = oldPos[e.old] }, lc = {}, cw = {}, pref = 2 }
            end
            rec.ncfg[1].lc[1] = { s0 = rec.segs[1].id, l0 = 1, s1 = rec.segs[2].id, l1 = 2, road = true }
            rec.ncfg[1].cw[1] = rec.segs[3].id
            for i = 1, pt.edgeObjectsToAdd.n do rec.objs[i] = { edge = rec.segs[3].id, model = 'station/street/bus_stop.mdl', param = 0.5 } end
            for i = 1, pt.edgeObjectsToRemove.n do rec.rmObjs[i] = { __ref = 'obj', i = i, edge = rec.rmSegs[1] } end
            for i = 1, pt.nodeConfigsToRemove.n do rec.rmNcfg[i] = { __ref = 'node', p = oldPos[pt.nodeConfigsToRemove[i].old] or { x = 0, y = 0, z = 0 } } end
            for i = 1, tree.toRemove.n do rec.rem[i] = { __ref = 'con', file = '::/buildings/x.con', p = { x = 1, y = 2, z = 3 } } end
            local tc = tree.toAdd[1]
            rec.cons[1] = { file = '::/' .. tc.fileName, transf = tc.transf, owner = 'player', params = {} }
            local out, why = M.portableRecord(rec, tree)
            if not out then return 'NO: ' .. why end
            local ids = {}
            for i, n in ipairs(out.nodes) do ids[#ids + 1] = 'n' .. n.id end
            for i, s in ipairs(out.segs) do ids[#ids + 1] = 's' .. s.id .. '(' .. tostring(type(s.n0) == 'number' and s.n0 or 'ref') .. ',' .. tostring(type(s.n1) == 'number' and s.n1 or 'ref') .. ')' end
            local nc = out.ncfg[1]
            ids[#ids + 1] = 'cfg' .. tostring(nc.node) .. ':' .. nc.lc[1].s0 .. ',' .. nc.lc[1].s1 .. ':' .. nc.cw[1]
            ids[#ids + 1] = 'obj' .. out.objs[1].edge
            ids[#ids + 1] = 'con' .. #out.cons
            return table.concat(ids, ' ')
        end)"""
        got = str(self.L.eval(code)(text))
        self.assertEqual(got, "n-1 n-2 n-3 s-1(-1,-2) s-2(ref,-1) s-3(-1,-3) s-4(-3,ref) cfg-1:-1,-2:-3 obj-3 con1")

    def test_a_text_that_counts_differently_falls_back_to_the_record_path(self):
        self.assertEqual(self.decoded(portable_road_text(segs=2)), "street|2|1")

    def test_a_text_that_cannot_be_read_falls_back_to_the_record_path(self):
        self.assertEqual(self.decoded("this is not a portable text"), "street|2|1")

    def test_a_build_without_a_text_is_built_by_the_record_path(self):
        code = """(function()
            local body = WIRE_RECORD(NATIVE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } })
            return tostring(DECODE(body).plan)
        end)"""
        self.assertEqual(str(self.L.eval(code)()), "street")


if __name__ == "__main__":
    unittest.main(verbosity=2)
