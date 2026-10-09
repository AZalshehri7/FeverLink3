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
def("SegmentAndEntity", { "entity", "type", "comp", "streetEdge", "playerOwned" }, function()
  return { entity = -1, type = 0, comp = D("BaseEdge"), streetEdge = D("BaseEdgeStreet"), playerOwned = D("PlayerOwned") }
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
def("Context", { "player", "checkTerrainAlignment", "cleanupStreetGraph", "gatherBuildings", "gatherFields", "refundableEntities" }, function()
  return { player = -1, checkTerrainAlignment = false, cleanupStreetGraph = false, gatherBuildings = false, gatherFields = false, refundableEntities = {} }
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
DEF_TEMPLATE("street/tram_road.street_template", "medium", { { forward = true, modes = { 2, 3 } }, { forward = false, modes = { 2, 3 } }, { forward = true, modes = { 4 } } })
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
      if t == CT.BASE_NODE and OCTREE == "none" then error("octree: component not indexed") end
      local out = {}
      local px, py = pos3(p)
      local name = NAMES[t] or (t == CT.CONSTRUCTION and "CONSTRUCTION") or nil
      if not name then return out end
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

BUILD_MOCK2 = r'''
-- ===== applying a proposal =====
BUILD_LOG, BUILD_REFUSALS, FACTORY_COUNT, HELPER_CALLS = {}, {}, 0, {}
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

  local gone = {}
  for _, e in ipairs(P.consRem) do
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
        if objIds[k] then list[#list + 1] = { objIds[k], o[2] } end
      else
        list[#list + 1] = o
        gone[o[1]] = nil
        if WORLD[o[1]] then WORLD[o[1]].ON = id end
      end
    end
    cs.objects = list
  end
  for o in pairs(gone) do WORLD[o] = nil end
  for _, ce in ipairs(P.consAdd) do
    local id = NEW_ENTITY(nil)
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
  return true, created
end

local BASE_EXECUTE = EXECUTE
function EXECUTE(cmd, cb)
  local name = cmd.__cmd
  if name == "makeWorldReplaceTerrainCmd" then
    GAME.applied[#GAME.applied + 1] = { name = name, time = GAME.time, args = cmd.args }
    if cb then cb({}, true, {}) end
    return
  end
  if name ~= "makeWorldBuildProposalCmd" then return BASE_EXECUTE(cmd, cb) end
  local a = cmd.args
  local ok, res = APPLY_BUILD(a[1])
  if ok then
    local ents = {}
    for i, e in ipairs(res) do ents[i] = { e, 0 } end
    GAME.applied[#GAME.applied + 1] = { name = name, time = GAME.time, args = a }
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
  return D("SegmentAndEntity", { entity = entity, type = opts.track and 1 or 0, comp = c })
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
  local objs = {}
  for i, o in ipairs(cs.objects) do objs[i] = { o[1], o[2] } end
  objs[#objs + 1] = { -400000000, 2 }
  cs.objects = objs
  local P = D("Proposal")
  local sp = P.proposal
  sp.removedSegments = { D("SegmentAndEntity", { entity = spec.edge, comp = COPY(w.BASE_EDGE), type = w.KIND }) }
  sp.addedSegments = { D("SegmentAndEntity", { entity = -1, comp = c, type = w.KIND }) }
  local eo = D("EdgeObjectNative", { segmentEntity = -1, param = spec.param, category = 2, left = spec.left == true })
  STORE(eo)._model = spec.model or "signal/signal_a.mdl"
  sp.edgeObjectsToAdd = { eo }
  P.proposal = sp
  return P
end

local function conEntity(spec)
  local ce = D("ConstructionEntity", { fileName = spec.file, params = spec.params or {}, name = spec.name or "", playerEntity = spec.owner or 7 })
  local m = STORE(STORE(ce).transf)
  m[13], m[14], m[15] = spec.x, spec.y, spec.z or 0
  return ce
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
      lines[#lines + 1] = "E " .. pa .. " " .. pb .. " " .. c.roadTemplate .. " kind" .. tostring(w.KIND) .. " lanes" .. #c.laneConfigs .. " [" .. table.concat(objs, ";") .. "] " .. SER(c.edgeDecorations)
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

function COUNT(comp)
  local n = 0
  for _, w in pairs(WORLD) do if w[comp] then n = n + 1 end end
  return n
end

-- the module under test, loaded alone (no bridge): returns the module and its environment
function LOAD_BUILD()
  local env = setmetatable({}, { __index = _G })
  env.api = MAKE_API()
  env.package = { loaded = {} }
  env.ug_require = function(p)
    if p:find("feverlink3_common") then return assert(load(COMMON_SRC, "=common", "t", env))() end
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
        cmds = self.L.table()
        for stem, src in T.CMDS_SRC.items():
            cmds[stem] = src
        g.CMDS_SRC = cmds
        self.L.execute(T.MOCK % dict(t0=T.T0, first=first_entity))
        self.L.execute(BUILD_MOCK)
        self.L.execute(BUILD_MOCK2)
        self.L.execute(T.HARNESS)
        self.L.execute("SAVE_WORLD()")

    def sig(self):
        return str(self.L.eval("WORLD_SIG()"))

    def lua(self, expr):
        return self.L.eval(expr)

    def issue(self, proposal, ctx="nil", ie="false", pi="true", cb="function() end"):
        """Send a build from the interface state, as a game window or script would."""
        self.send_gui("api.cmd.sendCommand(api.cmd.makeWorldBuildProposalCmd(%s, %s, %s, %s), %s)" % (proposal, ctx, ie, pi, cb))

    def shipped(self):
        return [l for l in self.lines(self.from_game) if l.split(TAB)[1] == "action"]


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
        self.tick(5)
        self.relay.started, self.relay.speed = True, speed
        self.relay.session()
        self.tick(5)
        self.tick(10)

    def both(self, expr):
        return self.A.lua(expr), self.B.lua(expr)

    def assertSameWorld(self, msg=None):
        self.assertEqual(self.A.sig(), self.B.sig(), msg or "both games hold the same world")

    # ---- roads and tracks

    def test_road_built_by_the_client_appears_identically_on_both_games(self):
        self.start()
        self.B.issue("SIMPLE_ROAD{ a = { x = 300, y = 0, z = 0 }, b = { x = 400, y = 0, z = 0 } }", cb="function(d, s, e) ROAD_OK = s end")
        self.assertTrue(self.B.lua("ROAD_OK == nil"), "the window waits for the real result")
        self.tick(30)
        self.assertTrue(self.B.lua("ROAD_OK == true"))
        self.assertSameWorld()
        self.assertIn("E 300.00,0.00,0.00 400.00,0.00,0.00 street/new_medium_road.street_template", self.A.sig())
        na, nb = self.both("FIND_NODE(300, 0, 0)")
        self.assertNotEqual(na, nb, "the new node has another id in each game")
        shipped = self.B.shipped()
        self.assertEqual(len(shipped), 1)
        for ident in (na, nb, self.B.lua("FIND_EDGE(300, 0, 400, 0)"), self.A.lua("FIND_EDGE(300, 0, 400, 0)")):
            self.assertNotIn(str(int(ident)), shipped[0], "no session entity id on the wire")
        self.assertEqual(self.A.lua("BUILD_LOG[1]"), self.B.lua("BUILD_LOG[1]"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
