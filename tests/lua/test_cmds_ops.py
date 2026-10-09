"""Tests for the operations schemas (mod/feverlink3_1/content/feverlink3_cmds_ops.lua): the everyday actions of a
session, replicated between two mocked Transport Fever 3 games through the real mod files (see test_lockstep.py for
the harness this builds on, imported and not edited).

What the mock adds to the lockstep harness (OPS_PRELUDE, run between the base mock and the mod's harness):
  * engine objects as the game shows them (finding from MPFever, MIT): a vehicle configuration, a line, a stop, a
    colour are objects that list their members under __members, refuse members they do not have and raise
    "read only" for the ones the engine derives;
  * model resources whose ids differ between the two games (the client's are 100 higher) but whose names do not;
  * station groups, depots, a vehicle, a line, a town and an industry in the "savegame", with ids both games share;
  * the commands of the operations schema, with the engine's rules (money for a purchase, a line with vehicles on it
    cannot be destroyed...) and a loan game script that reacts to the Loan events as the real one does;
  * an id-independent SNAPSHOT of everything the operations touch, which is what two games must agree on.

Run:  python -I tests/lua/test_cmds_ops.py        (or: python -m unittest discover -s tests/lua)
Set FEVERLINK3_TEST_VERBOSE=1 to print both games' mod.log on failure.
"""
import os
import re
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)  # python -I does not put the script's folder on the path

import test_lockstep as T  # noqa: E402  (the harness; imported, never edited)

lupa = T.lupa
OPS_SRC = T.CMDS_SRC["feverlink3_cmds_ops"]

# ---------------------------------------------------------------- the mock

OPS_PRELUDE = r'''
OPS = { priceBuy = 10000, loanSign = 1, drop = nil, replaceKeepsEntity = false }
JOURNAL_LOAN = 2
MODEL_OFFSET = 0
MODELS = { "vehicle/bus_red.mdl", "vehicle/bus_blue.mdl", "vehicle/truck.mdl" }
OPS_ENGINE = {}            -- what the harness hands the bridge as FEVERLINK3_ENGINE: nothing, so its own adapters run
FAILED, PENDING, LOANS = {}, {}, { obtained = {} }
EVENTS_SEEN, TOWNS_STATE, GREEN, MARKETING, SUBS, NOTES = {}, {}, {}, {}, {}, {}

local function modelId(name) for i, n in ipairs(MODELS) do if n == name then return 100 + i + MODEL_OFFSET end end end
local function modelName(id) return MODELS[id - 100 - MODEL_OFFSET] end
MODEL_ID = modelId

-- ---- engine objects
local TYPES
function NEW(typeName)
  local def = TYPES[typeName]
  local data = def.init()
  local member = {}
  for _, k in ipairs(def.members) do member[k] = true end
  return setmetatable({}, {
    __index = function(_, k)
      if k == "__members" then return def.members end
      return data[k]
    end,
    __newindex = function(_, k, v)
      if not member[k] then error("no member '" .. tostring(k) .. "' in " .. typeName) end
      if def.readonly and def.readonly[k] then error("member '" .. k .. "' is read only") end
      if OPS.drop == k then return end   -- an engine that quietly ignores a member
      data[k] = v
    end,
    __tostring = function() return "<" .. typeName .. ">" end,
  })
end
TYPES = {
  TransportVehicleConfig = { members = { "vehicles", "vehicleGroups", "muFileNames" },
    init = function() return { vehicles = {}, vehicleGroups = {}, muFileNames = {} } end },
  TransportVehiclePart = { members = { "part", "purchaseTime", "maintenanceState", "autoLoadConfig" },
    init = function() return { part = NEW("VehiclePart"), purchaseTime = 0, maintenanceState = 1, autoLoadConfig = {} } end },
  VehiclePart = { members = { "modelId", "reversed", "compartment2loadConfig", "color" },
    init = function() return { modelId = 0, reversed = false, compartment2loadConfig = {} } end },
  LoadConfig = { members = { "loadConfigIndex" }, init = function() return { loadConfigIndex = 0 } end },
  Line = { members = { "stops", "customFilters", "reservationPriority", "vehicleInfo" }, readonly = { vehicleInfo = true },
    init = function() return { stops = {}, customFilters = false, reservationPriority = 1.0, vehicleInfo = { transportModes = { true } } } end },
  LineStop = { members = { "stationGroup", "station", "terminal", "alternativeTerminals", "stopConfig", "loadMode", "minWaitingTime",
      "maxWaitingTime", "maxAdditionalWaitingTime", "waypoints" },
    init = function() return { stationGroup = -1, station = -1, terminal = -1, alternativeTerminals = {}, stopConfig = NEW("StopConfig"),
      loadMode = 0, minWaitingTime = 0, maxWaitingTime = 0, maxAdditionalWaitingTime = 0, waypoints = {} } end },
  StopConfig = { members = { "load", "maxLoad", "forceUnload", "destroyForConfigChange", "destroyForRefresh" },
    init = function() return { load = {}, maxLoad = {}, forceUnload = false, destroyForConfigChange = false, destroyForRefresh = false } end },
  StationTerminal = { members = { "station", "terminal" }, init = function() return { station = 0, terminal = 0 } end },
  Vec3f = { members = { "x", "y", "z" }, init = function() return { x = 0, y = 0, z = 0 } end },
}

local function vec(x, y, z) local o = NEW("Vec3f"); o.x, o.y, o.z = x, y, z; return o end

-- compact builders, called from a window's code with that game's api (so its model ids are used)
function BUILD_CONFIG(api, spec)
  local cfg = api.type.TransportVehicleConfig.new()
  local parts, groups = {}, {}
  for i, p in ipairs(spec.parts) do
    local part = api.type.TransportVehiclePart.new()
    part.part.modelId = api.res.modelRep.find(p.model)
    part.part.reversed = p.reversed or false
    local loads = {}
    for j = 1, p.compartments or 1 do local lc = api.type.LoadConfig.new(); lc.loadConfigIndex = j - 1; loads[j] = lc end
    part.part.compartment2loadConfig = loads
    if p.color then part.part.color = api.type.Vec3f.new(p.color[1], p.color[2], p.color[3]) end
    part.purchaseTime = spec.purchaseTime or 0
    local auto = {}
    for j = 1, p.compartments or 1 do auto[j] = true end
    part.autoLoadConfig = auto
    parts[i] = part
    groups[i] = 1
  end
  cfg.vehicles = parts
  cfg.vehicleGroups = groups
  cfg.muFileNames = {}
  return cfg
end

-- spec = { priority =, customFilters =, stops = { { group =, station =, terminal =, min =, max =, extra =, load = { ... }, alt = { {s, t} } } } }
function BUILD_LINE(api, spec)
  local line = api.type.Line.new()
  local stops = {}
  for i, s in ipairs(spec.stops) do
    local stop = api.type.Line.Stop.new()
    stop.stationGroup = s.group
    stop.station = s.station or 0
    stop.terminal = s.terminal or 0
    stop.minWaitingTime = s.min or 0
    stop.maxWaitingTime = s.max or 0
    stop.maxAdditionalWaitingTime = s.extra or 0
    stop.loadMode = s.mode or 0
    local cfg = api.type.Line.StopConfig.new()
    cfg.load = s.load or {}
    cfg.maxLoad = s.maxLoad or {}
    cfg.forceUnload = s.forceUnload or false
    stop.stopConfig = cfg
    local alt = {}
    for j, a in ipairs(s.alt or {}) do alt[j] = api.type.StationTerminal.new(a[1], a[2]) end
    stop.alternativeTerminals = alt
    if s.waypoints then stop.waypoints = s.waypoints end
    stops[i] = stop
  end
  line.stops = stops
  line.customFilters = spec.customFilters or false
  line.reservationPriority = spec.priority or 1.0
  return line
end

-- ---- the savegame both games start from (identical ids), built once the model offset is known
function BUILD_SAVEGAME()
  WORLD[54].CONSTRUCTION.depots = { 55 }
  WORLD[54].CONSTRUCTION.stations = {}
  WORLD[56].CONSTRUCTION.stations = { 57 }
  WORLD[56].CONSTRUCTION.depots = {}
  WORLD[57] = { STATION = {} }
  WORLD[58] = { STATION_GROUP = { stations = { 57 } }, NAME = { name = "Central" } }
  WORLD[59] = { CONSTRUCTION = { fileName = "station2.con", transf = { [13] = 300, [14] = -20, [15] = 1 }, stations = { 60 }, depots = {} } }
  WORLD[60] = { STATION = {} }
  WORLD[61] = { STATION_GROUP = { stations = { 60 } }, NAME = { name = "North" } }
  local api = MAKE_API()
  WORLD[70] = { TRANSPORT_VEHICLE = { userStopped = false, line = -1, state = "EN_ROUTE",
      config = BUILD_CONFIG(api, { parts = { { model = "vehicle/truck.mdl" } } }) }, NAME = { name = "Truck 1" } }
  WORLD[71] = { LINE = BUILD_LINE(api, { priority = 1.0, stops = { { group = 58 }, { group = 61 } } }), NAME = { name = "Line 1" },
    COLOR = { color = vec(1, 0, 0) } }
  WORLD[80] = { TOWN = {}, NAME = { name = "Springfield" } }
  WORLD[81] = { CONSTRUCTION = { fileName = "industry/coal.con", transf = { [13] = 500, [14] = 600, [15] = 0 } }, INDUSTRY = {} }
  setmetatable(WORLD, { __newindex = function(t, k, v)
    local p = PENDING[k]
    if p then
      PENDING[k] = nil
      for pk, pv in pairs(p) do v.TRANSPORT_VEHICLE[pk] = pv end
    end
    rawset(t, k, v)
  end })
end

-- ---- the api the games see, on top of the base mock's
local EXTRA_CT = { STATION_GROUP = 9, STATION = 10, TOWN = 11, NAME = 12, COLOR = 13, INDUSTRY = 14 }
local EXTRA_NAME = {}
for k, v in pairs(EXTRA_CT) do EXTRA_NAME[v] = k end
local function mk(name) return function(...) return { __cmd = name, args = { n = select("#", ...), ... } } end end
local base_make_api = MAKE_API
function MAKE_API()
  local api = base_make_api()
  for k, v in pairs(EXTRA_CT) do api.type.ComponentType[k] = v end
  local base_get = api.engine.getComponent
  api.engine.getComponent = function(e, t)
    local name = EXTRA_NAME[t]
    if name then local w = WORLD[e]; return w and w[name] or nil end
    return base_get(e, t)
  end
  for _, n in ipairs({ "makeVehicleSellCmd", "makeVehicleReplaceCmd", "makeVehicleSendToDepotCmd", "makeVehicleReverseCmd",
      "makeVehicleSetStoppedByUserCmd", "makeVehicleTryToDepartCmd", "makeLineUpdateCmd", "makeLineDestroyCmd",
      "makeEntitySetNameCmd", "makeEntitySetColorCmd", "makeGameSetCalendarSpeedCmd", "makeGameSetDateCmd",
      "makeGameSetTimeOfDayCmd", "makeGameSetCloudCoverageCmd", "makeStockListDiscardCargoCmd",
      "makeTownBuildingSetBlockedDevelopmentCmd", "makeTownSetDevelopmentActiveCmd" }) do
    api.cmd[n] = mk(n)
  end
  local t = api.type
  t.TransportVehicleConfig = { new = function() return NEW("TransportVehicleConfig") end }
  t.TransportVehiclePart = { new = function() return NEW("TransportVehiclePart") end }
  t.VehiclePart = { new = function() return NEW("VehiclePart") end }
  t.LoadConfig = { new = function() return NEW("LoadConfig") end }
  t.Line = { new = function() return NEW("Line") end, Stop = { new = function() return NEW("LineStop") end },
    StopConfig = { new = function() return NEW("StopConfig") end } }
  t.StationTerminal = { new = function(s, tm) local o = NEW("StationTerminal"); o.station = s or 0; o.terminal = tm or 0; return o end }
  t.Vec3f = { new = function(x, y, z) return vec(x or 0, y or 0, z or 0) end }
  t.JournalEntry = {
    new = function() return { category = {} } end,
    Type = { OTHER = 1, LOAN = JOURNAL_LOAN, INTEREST = 3, CONSTRUCTION = 4, ACQUISITION = 5, MAINTENANCE = 6, INCOME = 7, SUBSIDY = 8 },
    Construction = { STREET = 1, TRACK = 2, SIGNAL = 3, STATION = 4, DEPOT = 5, BULLDOZER = 6, WAREHOUSE = 7, OTHER = 8 },
    Maintenance = { VEHICLE = 1, INFRASTRUCTURE = 2, OTHER = 3, VEHICLE_MAINTENANCE = 4 },
    Other = { OTHER = 1 },
    Carrier = { ROAD = 1, RAIL = 2, TRAM = 3, OTHER = 4, AIR = 5, WATER = 6 },
  }
  api.res = { modelRep = {
    getName = function(id) return modelName(id) end,
    find = function(name) return modelId(name) or -1 end,
    get = function(id) return { name = modelName(id) } end,
  } }
  local function ownerOf(e)
    for c, w in pairs(WORLD) do
      local con = w.CONSTRUCTION
      if con then
        for _, list in ipairs({ con.depots or {}, con.stations or {} }) do
          for _, x in ipairs(list) do if x == e then return c end end
        end
      end
    end
    return -1
  end
  api.engine.system.streetConnectorSystem = {
    getConstructionEntityForSubconstruction = ownerOf,
    getConstructionEntityForDepot = ownerOf,
    getConstructionEntityForStation = ownerOf,
  }
  api.engine.system.stationGroupSystem = { getStationGroup = function(st)
    for g, w in pairs(WORLD) do
      if w.STATION_GROUP then for _, x in ipairs(w.STATION_GROUP.stations) do if x == st then return g end end end
    end
    return -1
  end }
  return api
end

-- ---- the engine's side of the commands
local function isVehicle(e) return WORLD[e] and WORLD[e].TRANSPORT_VEHICLE end
local function isLine(e) return WORLD[e] and WORLD[e].LINE end
local function newId() GAME.nextEntity = GAME.nextEntity + 1; return GAME.nextEntity end

local function charge(config)
  local cost = OPS.priceBuy * math.max(1, config and config.vehicles and #config.vehicles or 1)
  if ACCOUNT.balance < cost then return false end
  ACCOUNT.balance = ACCOUNT.balance - cost
  return true
end

local function createVehicle(extra)
  local created = newId()
  PENDING[created] = extra
  if GAME.inUpdate then GAME.deferred[#GAME.deferred + 1] = created else WORLD[created] = { TRANSPORT_VEHICLE = {} } end
  return created
end

OPS_EXEC = {}
OPS_EXEC.makeVehicleBuyCmd = function(a)
  if a[1] ~= 7 then return false, "player entity expected, got " .. tostring(a[1]) end
  if not (WORLD[a[2]] and WORLD[a[2]].VEHICLE_DEPOT) then return false, "no depot " .. tostring(a[2]) end
  if not charge(a[3]) then return false, "not enough money" end
  local created = createVehicle({ config = a[3], line = -1, userStopped = false, state = "IN_DEPOT" })
  return true, created, { resultVehicleEntity = created }
end
OPS_EXEC.makeVehicleReplaceCmd = function(a)
  if not isVehicle(a[1]) then return false, "no vehicle " .. tostring(a[1]) end
  if not charge(a[2]) then return false, "not enough money" end
  local old = WORLD[a[1]].TRANSPORT_VEHICLE
  if OPS.replaceKeepsEntity then old.config = a[2]; return true end
  local created = createVehicle({ config = a[2], line = old.line, stopIndex = old.stopIndex, userStopped = old.userStopped, state = old.state })
  WORLD[a[1]] = nil
  return true, created, { resultVehicleEntity = created }
end
OPS_EXEC.makeVehicleSellCmd = function(a)
  local list = a[1]
  if type(list) ~= "table" or #list == 0 then return false, "nothing to sell" end
  for i = 1, #list do if not isVehicle(list[i]) then return false, "no vehicle " .. tostring(list[i]) end end
  for i = 1, #list do ACCOUNT.balance = ACCOUNT.balance + OPS.priceBuy / 2; WORLD[list[i]] = nil end
  return true
end
OPS_EXEC.makeVehicleSetLineCmd = function(a)
  if not isVehicle(a[1]) then return false, "no vehicle " .. tostring(a[1]) end
  if not isLine(a[2]) then return false, "no line " .. tostring(a[2]) end
  WORLD[a[1]].TRANSPORT_VEHICLE.line, WORLD[a[1]].TRANSPORT_VEHICLE.stopIndex = a[2], a[3]
  return true
end
OPS_EXEC.makeVehicleSendToDepotCmd = function(a)
  if not isVehicle(a[1]) then return false, "no vehicle " .. tostring(a[1]) end
  WORLD[a[1]].TRANSPORT_VEHICLE.state = "GOING_TO_DEPOT"
  return true
end
OPS_EXEC.makeVehicleReverseCmd = function(a)
  if not isVehicle(a[1]) then return false, "no vehicle " .. tostring(a[1]) end
  local v = WORLD[a[1]].TRANSPORT_VEHICLE
  v.reversed = not v.reversed
  return true
end
OPS_EXEC.makeVehicleSetStoppedByUserCmd = function(a)
  if not isVehicle(a[1]) then return false, "no vehicle " .. tostring(a[1]) end
  WORLD[a[1]].TRANSPORT_VEHICLE.userStopped = a[2]
  return true
end
OPS_EXEC.makeVehicleTryToDepartCmd = function(a)
  if not isVehicle(a[1]) then return false, "no vehicle " .. tostring(a[1]) end
  return true
end
OPS_EXEC.makeLineCreateCmd = function(a)
  if a[3] ~= 7 then return false, "player entity expected" end
  local comp = a[4]
  for i = 1, #comp.stops do
    local g = comp.stops[i].stationGroup
    if not (WORLD[g] and WORLD[g].STATION_GROUP) then return false, "no station group " .. tostring(g) end
  end
  local created = newId()
  WORLD[created] = { LINE = { stops = comp.stops, customFilters = comp.customFilters, reservationPriority = comp.reservationPriority },
    NAME = { name = a[1] }, COLOR = { color = a[2] } }
  return true, created, { resultEntity = created }
end
OPS_EXEC.makeLineUpdateCmd = function(a)
  if not isLine(a[1]) then return false, "no line " .. tostring(a[1]) end
  local comp = a[2]
  for i = 1, #comp.stops do
    local g = comp.stops[i].stationGroup
    if not (WORLD[g] and WORLD[g].STATION_GROUP) then return false, "no station group " .. tostring(g) end
  end
  local l = WORLD[a[1]].LINE
  l.stops, l.customFilters, l.reservationPriority = comp.stops, comp.customFilters, comp.reservationPriority
  return true
end
OPS_EXEC.makeLineDestroyCmd = function(a)
  if not isLine(a[1]) then return false, "no line " .. tostring(a[1]) end
  for _, w in pairs(WORLD) do
    if w.TRANSPORT_VEHICLE and w.TRANSPORT_VEHICLE.line == a[1] then return false, "vehicles are assigned to the line" end
  end
  WORLD[a[1]] = nil
  return true
end
OPS_EXEC.makeEntitySetNameCmd = function(a)
  if not WORLD[a[1]] then return false, "no entity " .. tostring(a[1]) end
  WORLD[a[1]].NAME = { name = a[2] }
  return true
end
OPS_EXEC.makeEntitySetColorCmd = function(a)
  if not WORLD[a[1]] then return false, "no entity " .. tostring(a[1]) end
  WORLD[a[1]].COLOR = { color = a[2] }
  return true
end
OPS_EXEC.makeJournalBookAssetCmd = function(a, rec)
  assert(a[1] == 7, "player entity expected")
  local e = a[2]
  ACCOUNT.balance = ACCOUNT.balance + e.amount
  if e.category.type == JOURNAL_LOAN then ACCOUNT.loan = ACCOUNT.loan + OPS.loanSign * e.amount end
  JOURNAL[#JOURNAL + 1] = e.amount
  rec.silent = true   -- not listed among the applied commands (the bridge's own corrections are journal entries too)
  return true
end
OPS_EXEC.makeGameSetCalendarSpeedCmd = function(a) GAME.calendarSpeed = a[1]; return true end
OPS_EXEC.makeGameSetTimeOfDayCmd = function(a) GAME.timeOfDay = a[1]; return true end
OPS_EXEC.makeGameSetCloudCoverageCmd = function(a) GAME.clouds = a[1]; return true end
OPS_EXEC.makeGameSetDateCmd = function(a) GAME.date = a[1]; return true end
OPS_EXEC.makeStockListDiscardCargoCmd = function(a)
  if not WORLD[a[1]] then return false, "no entity" end
  GAME.discarded = (GAME.discarded or "") .. tostring(WORLD[a[1]].NAME and WORLD[a[1]].NAME.name or a[1]) .. ";"
  return true
end
OPS_EXEC.makeTownBuildingSetBlockedDevelopmentCmd = function(a)
  if not (WORLD[a[1]] and WORLD[a[1]].CONSTRUCTION) then return false, "no building" end
  WORLD[a[1]].blocked = a[2]
  return true
end
OPS_EXEC.makeTownSetDevelopmentActiveCmd = function(a)
  if not (WORLD[a[1]] and WORLD[a[1]].TOWN) then return false, "no town" end
  WORLD[a[1]].TOWN.developmentActive = a[2]
  return true
end

-- events for game scripts other than the mod's own: what the real scripts do, reduced to their effect on the account
OPS_EVENTS = {
  Loan = {
    Obtain = function(p)
      if #LOANS.obtained >= 4 then return end
      local loan = p[2]
      ACCOUNT.balance = ACCOUNT.balance + loan.amount
      ACCOUNT.loan = ACCOUNT.loan + OPS.loanSign * loan.amount
      LOANS.obtained[#LOANS.obtained + 1] = loan.type .. ":" .. loan.amount
    end,
    Repay = function(p)
      local loan = p[2]
      for i, l in ipairs(LOANS.obtained) do
        if l == loan.type .. ":" .. loan.amount then
          table.remove(LOANS.obtained, i)
          ACCOUNT.balance = ACCOUNT.balance - loan.amount
          ACCOUNT.loan = ACCOUNT.loan - OPS.loanSign * loan.amount
          return
        end
      end
    end,
  },
  Subvention = { onAccept = function(p) SUBS[#SUBS + 1] = "accept " .. tostring(p.uid) end, onDecline = function(p) SUBS[#SUBS + 1] = "decline " .. tostring(p.uid) end },
  Companies = {
    startMarketingCampaign = function(p) MARKETING[#MARKETING + 1] = tostring(WORLD[p.townEntity] and WORLD[p.townEntity].NAME.name) .. ":" .. tostring(p.permitKey) end,
    MakeGreen = function(p) GREEN[#GREEN + 1] = tostring(WORLD[p.constructionEntity] and WORLD[p.constructionEntity].CONSTRUCTION.fileName) end,
    spawnIndustry = function(p) MARKETING[#MARKETING + 1] = "prospect:" .. tostring(p.cargoType) end,
  },
  Towns = {
    setTownRatingSensitivity = function(p) TOWNS_STATE[#TOWNS_STATE + 1] = tostring(WORLD[p.townEntity].NAME.name) .. ":" .. p.rating .. "=" .. tostring(p.sensitivity) end,
    resetTownRatingSensitivity = function(p) TOWNS_STATE[#TOWNS_STATE + 1] = tostring(WORLD[p.townEntity].NAME.name) .. ":reset" end,
  },
  GameTime = {
    SetMode = function(p) GAME.timeMode = p.mode end,
    OnManualGameTimeChanged = function(p) GAME.manualTime = p end,
    SkipPhase = function() GAME.skips = (GAME.skips or 0) + 1 end,
  },
  CloudCoverage = { SetMode = function(p) GAME.weatherMode = p.weatherMode end },
  Notifications = { dismiss = function(p) NOTES[#NOTES + 1] = "dismiss " .. tostring(p.id) end },
}

local base_execute = EXECUTE
function EXECUTE(cmd, cb)
  local name, a = cmd.__cmd, cmd.args
  local label, run = name, OPS_EXEC[name]
  if name == "makeScriptingSendEventCmd" and a[2] ~= "feverlink3" then
    local byName = OPS_EVENTS[a[2]]
    label = "event:" .. tostring(a[2]) .. "/" .. tostring(a[3])
    run = function() local h = byName and byName[a[3]]; if h then h(a[4]) end; return true end
  end
  if not run then return base_execute(cmd, cb) end
  local rec = { name = label, time = GAME.time, args = a }
  -- a handler answers (true, created entity, callback data) or (false, reason); an error is a refusal too
  local ran, ok, created, data = pcall(run, a, rec)
  if not ran then ok, created = false, ok end
  if not ok then
    FAILED[#FAILED + 1] = label .. ": " .. tostring(created)
    if cb then cb({}, false, {}) end
    return
  end
  rec.entity = created
  if not rec.silent then GAME.applied[#GAME.applied + 1] = rec end
  if cb then cb(type(data) == "table" and data or {}, true, rec.entity and { { rec.entity, 0 } } or {}) end
end

-- ---- what two games must agree on, without any entity id in it
local function sig(v, depth)
  depth = depth or 0
  if type(v) ~= "table" then return tostring(v) end
  if depth > 8 then return "..." end
  local keys, parts = {}, {}
  local m = v.__members
  if m then for _, k in ipairs(m) do keys[#keys + 1] = k end else for k in pairs(v) do keys[#keys + 1] = k end end
  table.sort(keys, function(x, y) return tostring(x) < tostring(y) end)
  for _, k in ipairs(keys) do
    local x = v[k]
    if k == "modelId" then x = modelName(x) or ("?" .. tostring(x)) end
    if k == "stationGroup" then x = WORLD[x] and WORLD[x].NAME and WORLD[x].NAME.name or ("?" .. tostring(x)) end
    parts[#parts + 1] = tostring(k) .. "=" .. sig(x, depth + 1)
  end
  return "{" .. table.concat(parts, ",") .. "}"
end
local function nameOf(e) return WORLD[e] and WORLD[e].NAME and WORLD[e].NAME.name or "-" end

function SNAPSHOT()
  local rows = {}
  for e, w in pairs(WORLD) do
    local v = w.TRANSPORT_VEHICLE
    if v then
      rows[#rows + 1] = "vehicle " .. sig(v.config) .. " line=" .. nameOf(v.line) .. " stop=" .. tostring(v.stopIndex) .. " stopped=" ..
        tostring(v.userStopped) .. " state=" .. tostring(v.state) .. " reversed=" .. tostring(v.reversed) .. " name=" .. nameOf(e) ..
        " color=" .. sig(w.COLOR and w.COLOR.color)
    end
    if w.LINE then
      rows[#rows + 1] = "line " .. nameOf(e) .. " color=" .. sig(w.COLOR and w.COLOR.color) .. " prio=" .. tostring(w.LINE.reservationPriority) ..
        " filters=" .. tostring(w.LINE.customFilters) .. " stops=" .. sig(w.LINE.stops)
    end
    if w.STATION_GROUP or w.TOWN then rows[#rows + 1] = "named " .. nameOf(e) end
    if w.blocked ~= nil then rows[#rows + 1] = "blocked " .. tostring(w.blocked) end
    if w.TOWN and w.TOWN.developmentActive ~= nil then rows[#rows + 1] = "development " .. nameOf(e) .. "=" .. tostring(w.TOWN.developmentActive) end
  end
  table.sort(rows)
  rows[#rows + 1] = "company " .. nameOf(7)
  rows[#rows + 1] = "account " .. tostring(ACCOUNT.balance) .. "/" .. tostring(ACCOUNT.loan)
  rows[#rows + 1] = "loans " .. table.concat(LOANS.obtained, ",")
  rows[#rows + 1] = "events " .. table.concat(TOWNS_STATE, ",") .. "|" .. table.concat(MARKETING, ",") .. "|" .. table.concat(GREEN, ",") ..
    "|" .. table.concat(SUBS, ",") .. "|" .. table.concat(NOTES, ",")
  rows[#rows + 1] = "world " .. tostring(GAME.calendarSpeed) .. " " .. tostring(GAME.timeOfDay) .. " " .. tostring(GAME.clouds) .. " " ..
    tostring(GAME.timeMode) .. " " .. tostring(GAME.manualTime) .. " " .. tostring(GAME.weatherMode) .. " " .. tostring(GAME.skips) .. " " .. tostring(GAME.discarded)
  return table.concat(rows, "\n")
end

-- the world entity ids by name, for a window that wants to act on what another player made
function FIND(kind, name)
  for e, w in pairs(WORLD) do
    if w[kind] and nameOf(e) == name then return e end
  end
  return -1
end
function COUNT(kind)
  local n = 0
  for _, w in pairs(WORLD) do if w[kind] then n = n + 1 end end
  return n
end
'''

# The base harness hands the bridge a FEVERLINK3_ENGINE with a mocked adjustLoan; these tests want the bridge's own.
_ENGINE_LINE = re.compile(r"env\.FEVERLINK3_ENGINE = \{[^\n]*\}[^\n]*")
assert len(_ENGINE_LINE.findall(T.HARNESS)) == 1, "test_lockstep.HARNESS changed: update the patch in test_cmds_ops.py"
OPS_HARNESS = OPS_PRELUDE + _ENGINE_LINE.sub("env.FEVERLINK3_ENGINE = OPS_ENGINE", T.HARNESS)


class OpsGame(T.Game):
    """A game of the lockstep harness with the operations mock added; the client's model ids are 100 higher."""

    def __init__(self, label, player_id, tmp, first_entity, model_offset):
        saved = T.HARNESS
        T.HARNESS = OPS_HARNESS  # Game.__init__ runs it between the base mock and the mod; restored below
        try:
            super().__init__(label, player_id, tmp, first_entity)
        finally:
            T.HARNESS = saved
        self.L.execute("MODEL_OFFSET = %d; BUILD_SAVEGAME()" % model_offset)

    def snapshot(self):
        return str(self.L.eval("SNAPSHOT()"))

    def find(self, kind, name):
        return int(self.L.eval("FIND(%s, %s)" % (T.lua_str(kind), T.lua_str(name))))

    def count(self, kind):
        return int(self.L.eval("COUNT(%s)" % T.lua_str(kind)))

    def balance(self):
        return int(self.L.eval("ACCOUNT.balance"))

    def loan(self):
        return int(self.L.eval("ACCOUNT.loan"))

    def failed(self):
        return str(self.L.eval("table.concat(FAILED, ' | ')"))

    def actions_shipped(self):
        return [l for l in self.lines(self.from_game) if l.split(T.TAB)[1] == "action"]


class OpsBase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-ops-")
        self.A = OpsGame("host", 0, self.tmp, 50000, 0)
        self.B = OpsGame("client", 1, self.tmp, 51000, 100)
        self.relay = T.Relay([self.A, self.B])

    def tearDown(self):
        if os.environ.get("FEVERLINK3_TEST_VERBOSE"):
            print("\n--- host mod.log ---\n" + self.A.log() + "\n--- client mod.log ---\n" + self.B.log())
        shutil.rmtree(self.tmp, ignore_errors=True)

    def tick(self, n=1):
        for _ in range(n):
            for _ in range(4):
                for g in (self.A, self.B):
                    g.frame()
                self.relay.pump()
            self.A.step()
            self.B.step()

    def start(self, speed=1):
        self.tick(5)
        self.relay.started, self.relay.speed = True, speed
        self.relay.session()
        self.tick(10)

    def settle(self, n=40):
        self.tick(n)

    def agree(self, msg="the two games hold the same world"):
        self.assertEqual(self.A.snapshot(), self.B.snapshot(), msg)

    def wire(self, line):
        """An `action` line of from_game.log as (the serialised command body, the payload table)."""
        p = self.relay.C.parse(line.split(T.TAB)[3])
        if isinstance(p, tuple):
            p = p[0]
        return str(p.body), p

    def both_logs(self):
        return self.A.log() + self.B.log()


BUS_RED = '{ model = "vehicle/bus_red.mdl", color = { 0.9, 0.1, 0.1 }, compartments = 2 }'
BUS_BLUE = '{ model = "vehicle/bus_blue.mdl", reversed = true }'


def buy(var, parts, depot=55, purchase_time=18000):
    """Lua for a window buying a vehicle with the callback result kept in the global `var`."""
    return ('local cfg = BUILD_CONFIG(api, { purchaseTime = %d, parts = { %s } })\n'
            'api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(api.engine.util.getPlayer(), %d, cfg),'
            ' function(d, s, e) %s = { s, d and d.resultVehicleEntity } end)' % (purchase_time, parts, depot, var))


class VehicleTest(OpsBase):
    def test_baseline_world_is_identical(self):
        self.start()
        self.agree("the savegame is the same in both games")
        self.assertEqual((self.A.count("TRANSPORT_VEHICLE"), self.A.count("LINE")), (1, 1))

    def test_purchase_carries_model_names_and_the_new_vehicle_gets_a_key(self):
        self.start()
        self.B.send_gui(buy("BUY", BUS_RED + ", " + BUS_BLUE))
        self.assertTrue(self.B.L.eval("BUY == nil"), "the window waits for the real result")
        self.settle()
        self.agree("same vehicle, same configuration, same money")
        self.assertEqual((self.A.count("TRANSPORT_VEHICLE"), self.B.count("TRANSPORT_VEHICLE")), (2, 2))
        self.assertEqual(self.A.balance(), 1000000 - 2 * 10000, "two parts cost two parts")
        # the client's window got its own vehicle id back, and the vehicles' ids differ between the games
        vb = int(self.B.L.eval("BUY[2]"))
        self.assertTrue(self.B.L.eval("BUY[1] == true"))
        self.assertTrue(self.B.L.eval("WORLD[%d].TRANSPORT_VEHICLE ~= nil" % vb))
        self.assertNotEqual(vb, int(self.A.L.eval("GAME.applied[1].entity")))
        # on the wire: the depot as a saved entity with its building, model names beside the ids, the category
        shipped = self.B.actions_shipped()
        self.assertEqual(len(shipped), 1)
        body, p = self.wire(shipped[0])
        for needle in ('"s:55"', '"s:54"', '"vehicle/bus_red.mdl"', '"vehicle/bus_blue.mdl"'):
            self.assertIn(needle, body)
        self.assertEqual(p.category, "spend")
        self.assertNotIn(str(vb), body, "no entity id of the client's game travels")
        self.assertIn("vehicle/bus_red.mdl is 101 here and 201 on the sending game", self.A.log(),
                      "a model id that differs between the games is noted for whoever checks the build")

    def test_two_purchases_in_one_step_are_two_vehicles_with_two_keys(self):
        self.start()
        self.B.send_gui(buy("BUY1", BUS_RED) + "\n" + buy("BUY2", BUS_BLUE))
        self.settle()
        self.agree()
        self.assertEqual(self.A.count("TRANSPORT_VEHICLE"), 3)
        v1, v2 = int(self.B.L.eval("BUY1[2]")), int(self.B.L.eval("BUY2[2]"))
        self.assertNotEqual(v1, v2, "the window got two different vehicles")
        # selling the FIRST purchase must remove the red bus everywhere: the keys 1:1 and 1:2 name different vehicles
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleSellCmd({ %d }))" % v1)
        self.settle()
        self.agree()
        snap = self.A.snapshot()
        self.assertNotIn("bus_red", snap)
        self.assertIn("bus_blue", snap)
        self.assertEqual(self.A.balance(), 1000000 - 2 * 10000 + 5000, "a sale returns half")

    def test_a_purchase_the_account_cannot_pay_fails_for_that_vehicle_only(self):
        self.start()
        for g in (self.A, self.B):
            g.L.execute("ACCOUNT.balance = 15000")
        self.B.send_gui(buy("BUY1", BUS_RED) + "\n" + buy("BUY2", BUS_BLUE))
        self.settle(60)
        self.agree("both games ran out of money at the same purchase")
        self.assertEqual(self.A.count("TRANSPORT_VEHICLE"), 2)
        self.assertTrue(self.B.L.eval("BUY1[1] == true and BUY1[2] ~= nil"), "the first purchase succeeded")
        self.assertTrue(self.B.L.eval("BUY2[1] == false"), "the second one is told it did not happen")
        self.assertIn("bus_red", self.A.snapshot())

    def test_orders_reach_the_same_vehicle_and_line(self):
        self.start()
        self.B.send_gui("local c = api.cmd\n"
                        "c.sendCommand(c.makeVehicleSetLineCmd(70, 71, 1))\n"
                        "c.sendCommand(c.makeVehicleReverseCmd(70))\n"
                        "c.sendCommand(c.makeVehicleSetStoppedByUserCmd(70, true))")
        self.A.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleSendToDepotCmd(70, false))")
        self.settle()
        self.agree()
        snap = self.A.snapshot()
        for needle in ("line=Line 1", "stop=1", "stopped=true", "reversed=true", "state=GOING_TO_DEPOT"):
            self.assertIn(needle, snap)
        self.assertEqual(self.A.applied(), self.B.applied(), "the same commands at the same steps")
        self.assertEqual(len(self.A.applied()), 4)

    def test_replacing_a_vehicle_binds_the_new_one_and_later_commands_find_it(self):
        self.start()
        self.B.send_gui(buy("BUY", BUS_RED))
        self.settle()
        old = int(self.B.L.eval("BUY[2]"))
        self.B.send_gui('local cfg = BUILD_CONFIG(api, { purchaseTime = 19000, parts = { { model = "vehicle/truck.mdl" } } })\n'
                        'api.cmd.sendCommand(api.cmd.makeVehicleReplaceCmd(%d, cfg))' % old)
        self.settle()
        self.agree("the old vehicle is gone, a truck took its place")
        self.assertEqual(self.A.count("TRANSPORT_VEHICLE"), 2)
        self.assertNotIn("bus_red", self.A.snapshot())
        # the window never saw the new vehicle's id; it finds the vehicle in the world, as the game's windows do
        new = int(self.B.L.eval("(function() for e, w in pairs(WORLD) do if w.TRANSPORT_VEHICLE and e ~= 70 then return e end end end)()"))
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleSetStoppedByUserCmd(%d, true))" % new)
        self.settle()
        self.agree("the command found the replacement on the other game")
        self.assertEqual(self.A.snapshot().count("stopped=true"), 1)

    def test_a_replacement_that_keeps_the_entity_is_noted_and_changes_nothing_else(self):
        self.start()
        for g in (self.A, self.B):
            g.L.execute("OPS.replaceKeepsEntity = true")
        self.B.send_gui(buy("BUY", BUS_RED))
        self.settle()
        old = int(self.B.L.eval("BUY[2]"))
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleReplaceCmd(%d, BUILD_CONFIG(api, { parts = { { model = "vehicle/truck.mdl" } } })))' % old)
        self.settle(60)
        self.agree()
        self.assertEqual(self.A.count("TRANSPORT_VEHICLE"), 2)
        self.assertIn("action 1:2: nothing created after 10 steps", self.A.log(), "the watch gave up and said so")
        # the vehicle is still reachable through the key of the purchase
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleSetStoppedByUserCmd(%d, true))" % old)
        self.settle()
        self.agree()
        self.assertEqual(self.A.snapshot().count("stopped=true"), 1)

    def test_selling_a_list_of_vehicles_and_a_stale_reference(self):
        self.start()
        self.B.send_gui(buy("BUY", BUS_RED))
        self.settle()
        v = int(self.B.L.eval("BUY[2]"))
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleSellCmd({ %d, 70 }))" % v)
        self.settle()
        self.agree()
        self.assertEqual(self.A.count("TRANSPORT_VEHICLE"), 0)
        # a command about a vehicle that no longer exists is refused on every game that has the key, never applied to another entity
        before = len(self.A.applied())
        self.A.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleReverseCmd(70))")
        self.settle()
        self.assertEqual(len(self.A.applied()), before)
        self.assertEqual(len(self.B.applied()), before)

    def test_unknown_or_wrong_kind_references_are_refused_and_stay_native(self):
        self.start()
        # a vehicle that only the client has: the sender applies it, the host refuses it, nobody applies it to a stranger
        self.B.L.execute("WORLD[9999] = { TRANSPORT_VEHICLE = { userStopped = false, line = -1, config = {} } }")
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleReverseCmd(9999))")
        self.settle()
        self.assertEqual(self.A.applied(), [])
        self.assertIn("vehicle s:9999 does not exist here", self.A.log())
        self.assertTrue(any(l.split(T.TAB)[1] == "action_refused" for l in self.A.lines(self.A.from_game)))
        # a vehicle given where a line belongs never leaves the sender: the dry run catches it
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleSetLineCmd(70, 70, 0))")
        self.settle()
        self.assertIn("sent natively: argument 2: cannot refer to entity 70 as a line", self.B.log())
        self.assertEqual(len(self.B.actions_shipped()), 1, "only the first command was shipped")


if __name__ == "__main__":
    unittest.main(verbosity=2)
