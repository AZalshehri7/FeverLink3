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
import types
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)  # python -I does not put the script's folder on the path

import test_lockstep as T  # noqa: E402  (the harness; imported, never edited)

lupa = T.lupa

# ---------------------------------------------------------------- the mock

OPS_PRELUDE = r'''
OBJECTS = setmetatable({}, { __mode = "k" })   -- every engine object NEW made
OPS = { realVehicleInfo = false, priceBuy = 10000, loanSign = 1, drop = nil, refuse = nil, replaceKeepsEntity = false, player = 7, strictCmd = false,
  userdataResults = false }
JOURNAL_LOAN = 2
MODEL_OFFSET = 0
MODELS = { "vehicle/bus_red.mdl", "vehicle/bus_blue.mdl", "vehicle/truck.mdl" }
OPS_ENGINE = {}            -- what the harness hands the bridge as FEVERLINK3_ENGINE: nothing, so its own adapters run
FAILED, PENDING, LOANS = {}, {}, { obtained = {} }
TOWNS_STATE, GREEN, MARKETING, SUBS, NOTES = {}, {}, {}, {}, {}

local function modelId(name) for i, n in ipairs(MODELS) do if n == name then return 100 + i + MODEL_OFFSET end end end
local function modelName(id) return MODELS[id - 100 - MODEL_OFFSET] end

-- ---- engine objects
local TYPES
function NEW(typeName)
  local def = TYPES[typeName]
  local data = def.init()
  local member = {}
  for _, k in ipairs(def.members) do member[k] = true end
  local obj
  obj = setmetatable({}, {
    __index = function(_, k)
      if k == "__members" then return def.members end
      return data[k]
    end,
    __newindex = function(_, k, v)
      if not member[k] then error("no member '" .. tostring(k) .. "' in " .. typeName) end
      -- the real engine (seen in every real game): Line.vehicleInfo is typed; a plain table is refused with this message
      if OPS.realVehicleInfo and typeName == "Line" and k == "vehicleInfo" then
        if not OBJECTS[v] then error("bad argument #2 to '?' (const LineVehicleInfo expected, got no value)") end
        data[k] = v
        return
      end
      if def.readonly and def.readonly[k] then error("member '" .. k .. "' is read only") end
      if OPS.drop == k then return end   -- an engine that quietly ignores a member
      if OPS.refuse == k then error("cannot set " .. k) end   -- one that refuses it, not as "read only"
      data[k] = v
    end,
    __tostring = function() return "<" .. typeName .. ">" end,
  })
  OBJECTS[obj] = true
  return obj
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
    init = function() return { stops = {}, customFilters = false, reservationPriority = 1.0,
      vehicleInfo = OPS.realVehicleInfo and NEW("LineVehicleInfo") or { transportModes = { true } } } end },
  LineVehicleInfo = { members = { "transportModes" }, init = function() return { transportModes = {} } end },
  LineStop = { members = { "stationGroup", "station", "terminal", "alternativeTerminals", "stopConfig", "loadMode", "minWaitingTime",
      "maxWaitingTime", "maxAdditionalWaitingTime", "waypoints" },
    init = function() return { stationGroup = -1, station = -1, terminal = -1, alternativeTerminals = {}, stopConfig = NEW("StopConfig"),
      loadMode = 0, minWaitingTime = 0, maxWaitingTime = 0, maxAdditionalWaitingTime = 0, waypoints = {} } end },
  StopConfig = { members = { "load", "maxLoad", "forceUnload", "destroyForConfigChange", "destroyForRefresh" },
    init = function() return { load = {}, maxLoad = {}, forceUnload = false, destroyForConfigChange = false, destroyForRefresh = false } end },
  StationTerminal = { members = { "station", "terminal" }, init = function() return { station = 0, terminal = 0 } end },
  Vec3f = { members = { "x", "y", "z" }, init = function() return { x = 0, y = 0, z = 0 } end },
  Date = { members = { "year", "month", "day" }, init = function() return { year = 1900, month = 1, day = 1 } end },
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
  if spec.modes then line.vehicleInfo.transportModes = spec.modes end
  return line
end

-- ---- the savegame both games start from (identical ids), built once the model offset and the player are known
function BUILD_SAVEGAME()
  WORLD[OPS.player] = WORLD[OPS.player] or { PLAYER = {} }
  WORLD[54].CONSTRUCTION.depots = { 55 }
  WORLD[54].CONSTRUCTION.stations = {}
  WORLD[56].CONSTRUCTION.stations = { 57 }
  WORLD[56].CONSTRUCTION.depots = {}
  WORLD[57] = { STATION = {} }
  WORLD[58] = { STATION_GROUP = { stations = { 57 } }, NAME = { name = "Central" } }
  WORLD[59] = { CONSTRUCTION = { fileName = "station2.con", transf = { [13] = 300, [14] = -20, [15] = 1 }, stations = { 60 }, depots = { 62 } } }
  WORLD[62] = { VEHICLE_DEPOT = {} }
  WORLD[60] = { STATION = {} }
  WORLD[61] = { STATION_GROUP = { stations = { 60 } }, NAME = { name = "North" } }
  local api = MAKE_API()
  WORLD[70] = { TRANSPORT_VEHICLE = { userStopped = false, line = -1, state = "EN_ROUTE",
      config = BUILD_CONFIG(api, { parts = { { model = "vehicle/truck.mdl" } } }) }, NAME = { name = "Truck 1" } }
  WORLD[71] = { LINE = BUILD_LINE(api, { priority = 1.0, stops = { { group = 58 }, { group = 61 } } }), NAME = { name = "Line 1" },
    COLOR = { color = vec(1, 0, 0) } }
  WORLD[80] = { TOWN = {}, NAME = { name = "Springfield" } }
  WORLD[81] = { CONSTRUCTION = { fileName = "industry/coal.con", transf = { [13] = 500, [14] = 600, [15] = 0 } }, INDUSTRY = {},
    NAME = { name = "Coal Mine" } }
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
  api.engine.util.getPlayer = function() return OPS.player end   -- the games' own player entities need not be the same
  for _, n in ipairs({ "makeVehicleSellCmd", "makeVehicleReplaceCmd", "makeVehicleSendToDepotCmd", "makeVehicleReverseCmd",
      "makeVehicleSetStoppedByUserCmd", "makeVehicleTryToDepartCmd", "makeLineUpdateCmd", "makeLineDestroyCmd",
      "makeEntitySetNameCmd", "makeEntitySetColorCmd", "makeGameSetCalendarSpeedCmd", "makeGameSetDateCmd",
      "makeGameSetTimeOfDayCmd", "makeGameSetCloudCoverageCmd", "makeStockListDiscardCargoCmd",
      "makeTownBuildingSetBlockedDevelopmentCmd", "makeTownSetDevelopmentActiveCmd" }) do
    api.cmd[n] = mk(n)
  end
  -- a build whose command table refuses the lookup of a factory it does not have, instead of answering nil
  if OPS.strictCmd then
    setmetatable(api.cmd, { __index = function(_, k) error("no command " .. tostring(k)) end })
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
  t.Date = { new = function(y, m, d) local o = NEW("Date"); o.year, o.month, o.day = y, m, d; return o end }
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
  if a[1] ~= OPS.player then return false, "player entity expected, got " .. tostring(a[1]) end
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
  if a[3] ~= OPS.player then return false, "player entity expected" end
  local comp = a[4]
  for i = 1, #comp.stops do
    local g = comp.stops[i].stationGroup
    if not (WORLD[g] and WORLD[g].STATION_GROUP) then return false, "no station group " .. tostring(g) end
  end
  local created = newId()
  WORLD[created] = { LINE = { stops = comp.stops, customFilters = comp.customFilters, reservationPriority = comp.reservationPriority, vehicleInfo = comp.vehicleInfo },
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
  assert(a[1] == OPS.player, "player entity expected")
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
  if cb then
    local result, entities = type(data) == "table" and data or {}, rec.entity and { { rec.entity, 0 } } or {}
    -- as the real engine does: the result of a purchase is userdata (VehicleBuyCommandData) and lists no entity
    if OPS.userdataResults and rec.entity then result, entities = PYDATA(rec.entity), {} end
    cb(result, true, entities)
  end
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
      local tm = OPS.realVehicleInfo and w.LINE.vehicleInfo and w.LINE.vehicleInfo.transportModes
      if tm then
        local bits = {}
        for i = 1, #tm do bits[i] = tostring(tm[i]) end
        rows[#rows] = rows[#rows] .. " modes=" .. table.concat(bits, ",")
      end
    end
    if w.STATION_GROUP or w.TOWN then rows[#rows + 1] = "named " .. nameOf(e) end
    if w.blocked ~= nil then rows[#rows + 1] = "blocked " .. tostring(w.blocked) end
    if w.TOWN and w.TOWN.developmentActive ~= nil then rows[#rows + 1] = "development " .. nameOf(e) .. "=" .. tostring(w.TOWN.developmentActive) end
  end
  table.sort(rows)
  rows[#rows + 1] = "company " .. nameOf(OPS.player)
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

    def __init__(self, label, player_id, tmp, first_entity, model_offset, player_entity=7, strict_cmd=False):
        saved = T.HARNESS
        # Game.__init__ runs it between the base mock and the mod; restored below
        T.HARNESS = OPS_HARNESS.replace("strictCmd = false", "strictCmd = true", 1) if strict_cmd else OPS_HARNESS
        try:
            super().__init__(label, player_id, tmp, first_entity)
        finally:
            T.HARNESS = saved
        self.L.execute("OPS.player = %d; MODEL_OFFSET = %d; BUILD_SAVEGAME()" % (player_entity, model_offset))

    def userdata_results(self):
        """From now on the engine answers a creating command with userdata and no entity list (a Python object seen from Lua)."""
        self.L.globals().PYDATA = lambda e: types.SimpleNamespace(resultVehicleEntity=int(e), resultEntity=int(e))
        self.L.execute("OPS.userdataResults = true")

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

    def actions_shipped(self):
        return [l for l in self.lines(self.from_game) if l.split(T.TAB)[1] == "action"]


class OpsBase(unittest.TestCase):
    STRICT = False   # True: the games' command table raises on the lookup of a factory it does not have

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-ops-")
        # the client's model ids are 100 higher and its player entity is another one
        self.A = OpsGame("host", 0, self.tmp, 50000, 0, 7, self.STRICT)
        self.B = OpsGame("client", 1, self.tmp, 51000, 100, 17, self.STRICT)
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


class StrictLookupTest(OpsBase):
    STRICT = True

    def test_a_factory_the_build_lacks_does_not_keep_the_others_from_being_wrapped(self):
        self.start()
        self.assertIn("wrapped ", self.B.log())
        self.assertNotIn("frame failed", self.B.log())
        self.B.send_gui(buy("BUY", BUS_RED))
        self.settle()
        self.agree()
        self.assertEqual(self.A.count("TRANSPORT_VEHICLE"), 2)


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

    def test_a_vehicle_bought_in_a_window_and_used_at_once_is_named_by_its_purchase(self):
        # A window that buys a vehicle and puts it on a line in the same breath (a real player did it a second after the
        # purchase) names the vehicle before the bridge's table of keys has it. It used to travel as "s:<id>" - an id the
        # other game does not share (refused there: "vehicle s:106787 does not exist here"). Now it travels as the key of
        # the purchase, and the vehicle is on the line in both games.
        self.start()
        self.B.window(buy("BUY", BUS_RED).replace(
            "function(d, s, e) BUY = { s, d and d.resultVehicleEntity } end",
            "function(d, s, e) BUY = { s, d and d.resultVehicleEntity }; "
            "api.cmd.sendCommand(api.cmd.makeVehicleSetLineCmd(d.resultVehicleEntity, 71, 1)) end"))
        self.settle()
        bought = int(self.B.L.eval("BUY[2]"))
        self.assertNotEqual(bought, 0)
        shipped = self.B.actions_shipped()
        self.assertEqual(len(shipped), 2, "the purchase and the assignment")
        body, _ = self.wire(shipped[1])
        self.assertNotIn("ui:", body, "the placeholder never leaves the game")
        self.assertNotIn('"s:%d"' % bought, body, "the sender's id never travels as a saved entity")
        self.assertIn('"1:1"', body, "the vehicle is named by the key of the purchase")
        self.assertNotIn("refused", self.A.log())
        self.agree()
        self.assertEqual(self.A.count("TRANSPORT_VEHICLE"), 2)
        self.assertIn("line=Line 1 stop=1", self.A.snapshot())

    def test_a_refused_reference_names_what_the_sender_had_not_replicated(self):
        self.start()
        # the sender's simulation half could not replicate a native build a moment ago ...
        self.B.L.execute('local st = STATE:get(); st.notes = { { at = GAME.time, text = "a native build at t=" .. GAME.time .. " was not replicated (test)" } }; STATE:set(st)')
        self.tick(2)
        # ... and now acts on a station group nobody else has
        self.B.L.execute("WORLD[99999] = { STATION_GROUP = { stations = {} } }")
        self.B.send_gui("local line = BUILD_LINE(api, { stops = { { group = 99999 }, { group = 58 } } })\n"
                        'api.cmd.sendCommand(api.cmd.makeLineCreateCmd("Ghost", api.type.Vec3f.new(0, 0, 0), api.engine.util.getPlayer(), line))')
        self.settle()
        text = self.A.log()
        self.assertIn("refused", text)
        self.assertIn("does not exist here", text)
        self.assertIn("the sender had not replicated: ", text)
        self.assertIn("was not replicated (test)", text)

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

    def test_a_purchase_during_a_pause_binds_the_vehicle_and_a_later_command_of_the_pause_names_it(self):
        self.start(speed=2)
        for g in (self.A, self.B):
            g.userdata_results()
        self.tick(10)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(40)
        self.assertEqual((self.A.time(), self.B.time()), (self.relay.pauseAt, self.relay.pauseAt), "both stopped at the pause")
        self.B.send_gui(buy("BUY", BUS_RED))
        self.tick(8)
        self.agree()
        self.assertEqual(self.A.count("TRANSPORT_VEHICLE"), 2)
        self.assertIn("stamped", self.B.log())
        self.assertIn("by the host (pause", self.B.log())
        self.assertEqual((self.A.speed(), self.B.speed()), (0, 0), "still paused")
        v = int(self.B.L.eval("BUY[2]"))
        # a later command names the vehicle by the key the purchase bound, not by an id the host does not have
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleSetStoppedByUserCmd(%d, true))" % v)
        self.tick(8)
        self.agree()
        self.assertEqual(self.A.snapshot().count("stopped=true"), 1)

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

    def test_a_model_the_other_game_does_not_have_is_refused_there(self):
        self.start()
        self.B.L.execute("MODELS[4] = 'vehicle/mod_bus.mdl'")   # installed on the client only
        self.B.send_gui(buy("BUY", '{ model = "vehicle/mod_bus.mdl" }'))
        self.settle()
        self.assertEqual(self.A.count("TRANSPORT_VEHICLE"), 1, "the host does not buy a vehicle it cannot build")
        self.assertIn("model vehicle/mod_bus.mdl is not installed here", self.A.log())
        self.assertTrue(any(l.split(T.TAB)[1] == "action_refused" for l in self.A.lines(self.A.from_game)))

    def test_a_configuration_the_engine_does_not_take_back_equal_stays_native(self):
        self.start()
        # an engine that quietly ignores purchaseTime when it is set on a rebuilt part (after the window built its own)
        self.B.send_gui('local cfg = BUILD_CONFIG(api, { purchaseTime = 18000, parts = { %s } })\n'
                        'OPS.drop = "purchaseTime"\n'
                        'api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(api.engine.util.getPlayer(), 55, cfg))' % BUS_RED)
        self.settle()
        self.assertEqual(self.B.actions_shipped(), [])
        self.assertIn("does not rebuild equal at TransportVehicleConfig.vehicles.1.purchaseTime", self.B.log())
        self.assertEqual(self.A.count("TRANSPORT_VEHICLE"), 1)

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


LINE_2 = """local line = BUILD_LINE(api, { priority = 2.0, customFilters = true, stops = {
    { group = 58, station = 0, terminal = 0, min = 30, max = 90, extra = 10, mode = 1,
      load = { true, false, true }, maxLoad = { 1, 0.5, 1 }, alt = { { 0, 1 } } },
    { group = 61, station = 0, terminal = 0 } } })
api.cmd.sendCommand(api.cmd.makeLineCreateCmd("Line 2", api.type.Vec3f.new(0.2, 0.8, 0.1), api.engine.util.getPlayer(), line),
    function(d, s, e) MADE = { s, e and e[1] and e[1][1] } end)"""


class LineTest(OpsBase):
    def test_two_players_create_edit_rename_colour_and_destroy_a_line(self):
        self.start()
        self.B.send_gui(LINE_2)
        self.settle()
        self.agree("the line has the same stops, waiting times, filters and colour in both games")
        self.assertEqual((self.A.count("LINE"), self.B.count("LINE")), (2, 2))
        snap = self.A.snapshot()
        for needle in ("line Line 2", "prio=2", "filters=true", "minWaitingTime=30", "maxAdditionalWaitingTime=10",
                       "stationGroup=Central", "stationGroup=North", "x=0.2"):
            self.assertIn(needle, snap)
        self.assertTrue(self.B.L.eval("MADE[1] == true and MADE[2] ~= nil"), "the client's window got the line it made")
        self.assertNotEqual(int(self.B.L.eval("MADE[2]")), self.A.find("LINE", "Line 2"), "the two games number it differently")

        # the host acts on the client's line: rename, recolour, give it a third stop and a new priority
        line_a = self.A.find("LINE", "Line 2")
        self.A.send_gui('api.cmd.sendCommand(api.cmd.makeEntitySetNameCmd(%d, "Express"))\n'
                        'api.cmd.sendCommand(api.cmd.makeEntitySetColorCmd(%d, api.type.Vec3f.new(0.5, 0.5, 0.9)))' % (line_a, line_a))
        self.A.send_gui('api.cmd.sendCommand(api.cmd.makeLineUpdateCmd(%d, BUILD_LINE(api, { priority = 3.0,'
                        ' stops = { { group = 61 }, { group = 58 }, { group = 61, min = 5 } } })))' % line_a)
        self.settle()
        self.agree()
        snap = self.A.snapshot()
        self.assertIn("line Express", snap)
        self.assertNotIn("Line 2", snap)
        self.assertIn("prio=3", snap)
        self.assertIn("x=0.5", snap)

        # the client assigns the saved vehicle to it: the line is named by its key, not by an id
        line_b = self.B.find("LINE", "Express")
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleSetLineCmd(70, %d, 2))" % line_b)
        self.settle()
        self.agree()
        self.assertIn("line=Express stop=2", self.A.snapshot())
        # a line with a vehicle on it cannot be destroyed, in either game; without the vehicle it can
        self.A.send_gui("api.cmd.sendCommand(api.cmd.makeLineDestroyCmd(%d))" % line_a)
        self.settle()
        self.agree("the engine refused it in both games")
        self.assertEqual((self.A.count("LINE"), self.B.count("LINE")), (2, 2))
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleSellCmd({ 70 }))\n"
                        "api.cmd.sendCommand(api.cmd.makeLineDestroyCmd(%d))" % line_b)
        self.settle()
        self.agree()
        self.assertEqual((self.A.count("LINE"), self.B.count("LINE")), (1, 1))

    def test_the_company_and_a_station_group_are_renamed_everywhere(self):
        self.start()
        # the client's player entity is 17, the host's 7: the name belongs to the company, whichever id it has
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeEntitySetNameCmd(api.engine.util.getPlayer(), "Rails Inc"))\n'
                        'api.cmd.sendCommand(api.cmd.makeEntitySetNameCmd(58, "Central Station"))')
        self.settle()
        self.agree()
        self.assertEqual(str(self.A.L.eval("WORLD[7].NAME.name")), "Rails Inc")
        self.assertEqual(str(self.B.L.eval("WORLD[17].NAME.name")), "Rails Inc")
        self.assertIn("named Central Station", self.A.snapshot())

    def test_a_line_with_waypoints_is_not_replicated_and_says_why(self):
        self.start()
        self.B.send_gui(LINE_2.replace("{ group = 61, station = 0, terminal = 0 }", "{ group = 61, waypoints = { { tag = 1 } } }"))
        self.settle()
        self.assertEqual((self.A.count("LINE"), self.B.count("LINE")), (1, 2), "native on the sender only: a visible divergence")
        self.assertIn("sent natively: argument 4: stop 2 has waypoints (not replicated yet)", self.B.log())
        self.assertEqual(self.B.actions_shipped(), [])

    def test_an_object_the_engine_does_not_take_back_equal_is_not_replicated(self):
        self.start()
        # an engine that quietly ignores maxWaitingTime when it is set on a rebuilt stop
        self.B.send_gui(LINE_2.replace("local line = BUILD_LINE", "local line = BUILD_LINE", 1).replace(
            "api.cmd.sendCommand(api.cmd.makeLineCreateCmd", "OPS.drop = 'maxWaitingTime'\napi.cmd.sendCommand(api.cmd.makeLineCreateCmd"))
        self.settle()
        self.assertEqual(self.B.actions_shipped(), [])
        self.assertIn("does not rebuild equal at Line.stops.1.maxWaitingTime", self.B.log())
        self.assertEqual(self.A.count("LINE"), 1)

    def test_a_member_the_engine_refuses_is_tolerated_when_it_comes_out_equal_and_named_when_it_does_not(self):
        self.start()
        # refused, but the default is what the stop had anyway: nothing is lost, and the log says it happened
        self.B.send_gui('local line = BUILD_LINE(api, { stops = { { group = 58 }, { group = 61 } } })\n'
                        'OPS.refuse = "loadMode"\n'
                        'api.cmd.sendCommand(api.cmd.makeLineCreateCmd("Line 3", api.type.Vec3f.new(0, 0, 1), api.engine.util.getPlayer(), line))')
        self.settle()
        self.agree()
        self.assertEqual((self.A.count("LINE"), self.B.count("LINE")), (2, 2))
        self.assertIn("a member the engine refused came out equal anyway: Line.stops[1].loadMode", self.B.log())
        # refused, and the stop had another load mode: the object does not come back equal, so it stays native and says why
        self.B.L.execute("OPS.refuse = nil")
        self.B.send_gui(LINE_2.replace("Line 2", "Line 4").replace(
            "api.cmd.sendCommand(api.cmd.makeLineCreateCmd", "OPS.refuse = 'loadMode'\napi.cmd.sendCommand(api.cmd.makeLineCreateCmd"))
        self.settle()
        self.assertEqual(len(self.B.actions_shipped()), 1, "only the first line was shipped")
        self.assertIn("does not rebuild equal at Line.stops.1.loadMode (the engine refused: Line.stops[1].loadMode:", self.B.log())

    def test_a_line_member_that_wants_an_engine_object_is_filled_in_place(self):
        # the real engine takes Line.vehicleInfo only as a LineVehicleInfo; every real game logged the refusal and the
        # transport modes the window had chosen never reached the other game unless they were the defaults
        for g in (self.A, self.B):
            g.L.execute("OPS.realVehicleInfo = true")
        self.start()
        self.B.send_gui(LINE_2.replace("customFilters = true,", "customFilters = true, modes = { true, false, true },"))
        self.settle()
        self.agree("the line carries the transport modes in both games")
        self.assertEqual((self.A.count("LINE"), self.B.count("LINE")), (2, 2), "the line was created on both")
        self.assertIn("modes=true,false,true", self.A.snapshot())
        self.assertNotIn("vehicleInfo", self.A.log() + self.B.log(), "no refused member to report")

    def test_read_only_members_are_left_to_the_engine_and_noted_once(self):
        self.start()
        self.B.send_gui(LINE_2)
        self.settle()
        self.agree()
        self.assertIn("read-only member 'vehicleInfo' of Line is left to the engine", self.B.log())
        # once per Lua state (the window's dry run, the simulation's replay), not once per use
        self.assertLessEqual(self.B.log().count("read-only member 'vehicleInfo'"), 2)
        # the member is read and shipped like any other; the receiver skips what its engine will not let it set
        body, _ = self.wire(self.B.actions_shipped()[0])
        self.assertIn("vehicleInfo", body)


class FinanceTest(OpsBase):
    OFFER = '{ type = "Medium", amount = 20000000, duration = 3000, percentage = 0.06, birthDay = 100 }'
    FRESH = '{ type = "Medium", amount = 24000000, duration = 3000, percentage = 0.07, birthDay = 5000 }'

    def event(self, game, ident, name, param, var="EV"):
        game.send_gui('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "%s", "%s", %s), function(d, s) %s = s end)'
                      % (ident, name, param, var))

    def test_a_loan_is_an_event_every_game_replays_and_money_agrees(self):
        self.start()
        self.event(self.B, "Loan", "Obtain", "{ %s, %s }" % (self.FRESH, self.OFFER))
        self.assertTrue(self.B.L.eval("EV == nil"))
        self.settle()
        self.agree()
        self.assertEqual((self.A.balance(), self.A.loan()), (1000000 + 20000000, 250000 + 20000000))
        self.assertIn("loans Medium:20000000", self.A.snapshot())
        self.assertTrue(self.B.L.eval("EV == true"), "the window is told the event was applied")
        body, p = self.wire(self.B.actions_shipped()[0])
        self.assertEqual(p.category, "spend")
        self.assertNotIn("makeLoan", body)
        # repaying: the parameter has no first element, only the loan; the hole survives the trip
        self.event(self.A, "Loan", "Repay", "{ nil, %s }" % self.OFFER)
        self.settle()
        self.agree()
        self.assertEqual((self.A.balance(), self.A.loan()), (1000000, 250000))
        self.assertEqual(self.A.applied(), self.B.applied())

    def test_marketing_campaign_and_its_cost_are_two_replicated_commands(self):
        self.start()
        # the marketing tool sends the event and, in its callback, books the cost as a journal entry
        self.B.send_gui('local town = 80\n'
                        'api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Companies", "startMarketingCampaign", {\n'
                        '  townEntity = town, companyEntity = api.engine.util.getPlayer(),\n'
                        '  marketingParams = { durationMs = 60000, lineCostFactor = 0.5 }, permitKey = "marketing.res" }),\n'
                        '  function()\n'
                        '    local entry = api.type.JournalEntry.new()\n'
                        '    entry.amount = -12000000; entry.time = -1; entry.category.type = api.type.JournalEntry.Type.OTHER\n'
                        '    api.cmd.sendCommand(api.cmd.makeJournalBookAssetCmd(api.engine.util.getPlayer(), entry, api.type.Vec3f.new(0, 0, 0)))\n'
                        '  end)')
        self.settle(60)
        self.agree("the campaign runs in both games and both paid for it")
        self.assertIn("Springfield:marketing.res", self.A.snapshot())
        self.assertEqual(self.A.balance(), 1000000 - 12000000)
        lines = self.B.actions_shipped()
        self.assertEqual(len(lines), 2)
        self.assertEqual([self.wire(l)[1].category for l in lines], ["spend", "spend"])

    def test_a_balance_edit_from_the_money_widget_reaches_the_other_game(self):
        self.start()
        self.A.send_gui('local entry = api.type.JournalEntry.new()\n'
                        'entry.amount = 5000; entry.time = -1; entry.category.type = api.type.JournalEntry.Type.OTHER\n'
                        'api.cmd.sendCommand(api.cmd.makeJournalBookAssetCmd(api.engine.util.getPlayer(), entry))')
        self.settle()
        self.agree()
        self.assertEqual(self.B.balance(), 1005000)
        # a booking nobody could rebuild stays native: a category this module does not know
        self.A.send_gui('local entry = api.type.JournalEntry.new()\n'
                        'entry.amount = 1; entry.time = -1; entry.category.type = 99\n'
                        'api.cmd.sendCommand(api.cmd.makeJournalBookAssetCmd(api.engine.util.getPlayer(), entry))')
        self.settle()
        self.assertIn("sent natively: argument 2: a journal category this module does not know", self.A.log())
        self.assertEqual(self.B.balance(), 1005000)

    def drift_round(self, n):
        self.relay.checksum_request(n, self.relay.future_stamp(), False)
        self.tick(40)
        self.relay.send_authority(n)
        self.tick(8)

    def test_loan_drift_is_corrected_with_a_loan_entry_and_its_counter_entry(self):
        self.start()
        self.tick(10)
        self.relay.pump()
        self.B.L.execute("ACCOUNT.loan = ACCOUNT.loan - 1000")
        self.drift_round(1)
        self.assertEqual(self.B.loan(), 250000 - 1000, "one round alone may only be an action still on its way")
        self.drift_round(2)
        self.assertEqual(self.B.loan(), 250000, "the same difference twice is a drift: the loan is the host's again")
        self.assertEqual(self.B.balance(), 1000000, "and the balance was left alone")
        self.assertEqual(str(self.B.L.eval("table.concat(JOURNAL, ',')")), "1000,-1000", "a LOAN entry and its counter entry")
        self.assertIn("host authority: loan corrected by 1000", self.B.log())
        self.assertEqual(self.B.applied(), [], "corrections are not replicated actions")

    def test_a_correction_that_moves_the_loan_the_wrong_way_switches_itself_off(self):
        self.start()
        self.tick(10)
        self.relay.pump()
        # an engine whose LOAN entries lower the reported loan: the first correction makes the difference bigger
        self.B.L.execute("OPS.loanSign = -1; ACCOUNT.loan = ACCOUNT.loan - 1000")
        self.drift_round(1)
        self.drift_round(2)
        self.assertEqual(self.B.loan(), 250000 - 2000)
        self.drift_round(3)
        self.drift_round(4)
        self.assertEqual(self.B.loan(), 250000 - 2000, "no second attempt: the error is not doubled again")
        self.assertIn("loan correction is off: the loan moved by -1000 after a correction of 1000", self.B.log())
        self.assertIn("host authority: loan differs but this build cannot correct it", self.B.log())
        self.assertEqual(str(self.B.L.eval("table.concat(JOURNAL, ',')")), "1000,-1000")


class PermissionTest(OpsBase):
    # in an order that keeps the entities they name alive until their turn
    CASES = [
        ("buy", buy("X", BUS_RED), "spend"),
        ("setline", "api.cmd.sendCommand(api.cmd.makeVehicleSetLineCmd(70, 71, 0))", "other"),
        ("reverse", "api.cmd.sendCommand(api.cmd.makeVehicleReverseCmd(70))", "other"),
        ("linecreate", LINE_2, "build"),
        ("lineupdate", "api.cmd.sendCommand(api.cmd.makeLineUpdateCmd(71, BUILD_LINE(api, { stops = { { group = 58 } } })))", "build"),
        ("rename", 'api.cmd.sendCommand(api.cmd.makeEntitySetNameCmd(71, "Renamed"))', "other"),
        ("loan", 'api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Obtain", { nil, { type = "Small", amount = 5000000 } }))', "spend"),
        ("subsidy", 'api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Subvention", "onAccept", { uid = 3 }))', "other"),
        ("timeofday", "api.cmd.sendCommand(api.cmd.makeGameSetTimeOfDayCmd(3600))", "other"),
        ("sell", "api.cmd.sendCommand(api.cmd.makeVehicleSellCmd({ 70 }))", "bulldoze"),
        ("linedestroy", "api.cmd.sendCommand(api.cmd.makeLineDestroyCmd(71))", "bulldoze"),
    ]

    def test_every_replicated_command_ships_the_category_the_host_checks(self):
        self.start()
        for name, code, category in self.CASES:
            before = len(self.B.actions_shipped())
            self.B.send_gui(code)
            self.settle(12)
            shipped = self.B.actions_shipped()
            self.assertEqual(len(shipped), before + 1, name)
            self.assertEqual(self.wire(shipped[-1])[1].category, category, name)

    def test_a_denied_category_cancels_before_stamping_but_other_events_pass(self):
        self.start()
        self.relay.send(self.B, "permissions", 0, self.relay.ser({"spend": False, "bulldoze": False, "other": False}))
        self.tick(3)
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Obtain", { nil, { type = "Small", amount = 5000000 } }),'
                        ' function(d, s) LOAN_OK = s end)\n'
                        'api.cmd.sendCommand(api.cmd.makeVehicleSellCmd({ 70 }), function(d, s) SELL_OK = s end)\n'
                        'api.cmd.sendCommand(api.cmd.makeEntitySetNameCmd(71, "No"), function(d, s) NAME_OK = s end)')
        # an event the mod does not replicate is not a grant's business: it runs, natively, on this game only
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Notifications", "dismiss", { id = 4 }), function(d, s) NOTE_OK = s end)')
        self.settle()
        self.assertTrue(self.B.L.eval("LOAN_OK == false and SELL_OK == false and NAME_OK == false"), "the windows are told no")
        self.assertTrue(self.B.L.eval("NOTE_OK == true"))
        self.assertNotIn("Notifications", self.B.log(), "an event nobody replicates is not worth a log line each")
        self.assertEqual(self.B.actions_shipped(), [])
        self.assertEqual(self.A.applied(), [])
        self.assertEqual(self.B.applied(), ["event:Notifications/dismiss@%d" % int(self.B.L.eval("GAME.applied[1].time"))])
        self.assertEqual([r[2] for r in self.relay.refused], ["permission denied: spend", "permission denied: bulldoze", "permission denied: other"])
        # granted again, the loan goes through
        self.relay.send(self.B, "permissions", 0, self.relay.ser({"spend": True}))
        self.tick(3)
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Obtain", { nil, { type = "Small", amount = 5000000 } }))')
        self.settle()
        self.assertEqual(self.A.loan(), 250000 + 5000000)


class TownTest(OpsBase):
    def test_town_company_and_subsidy_events_reach_every_game(self):
        self.start()
        self.B.send_gui('local c = api.cmd\n'
                        'c.sendCommand(c.makeScriptingSendEventCmd("", "Towns", "setTownRatingSensitivity", { townEntity = 80, rating = "noise", sensitivity = 3 }))\n'
                        'c.sendCommand(c.makeScriptingSendEventCmd("", "Companies", "MakeGreen", { companyEntity = api.engine.util.getPlayer(), constructionEntity = 81, permitKey = "green.res" }))\n'
                        'c.sendCommand(c.makeScriptingSendEventCmd("", "Subvention", "onAccept", { uid = 12 }))\n'
                        'c.sendCommand(c.makeTownSetDevelopmentActiveCmd(80, false))\n'
                        'c.sendCommand(c.makeStockListDiscardCargoCmd(81, { 0, 1 }, 1.0))')
        self.A.send_gui('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Towns", "resetTownRatingSensitivity", { townEntity = 80 }))')
        self.settle()
        self.agree()
        snap = self.A.snapshot()
        for needle in ("Springfield:noise=3", "Springfield:reset", "industry/coal.con", "accept 12", "development Springfield=false",
                       "Coal Mine;"):
            self.assertIn(needle, snap)
        self.assertEqual(self.A.applied(), self.B.applied())

    def test_an_industry_a_game_script_made_is_found_by_file_and_position(self):
        self.start()
        # each game's scripts made the same oil rig in the session; the ids are not the same
        self.A.L.execute("WORLD[90100] = { CONSTRUCTION = { fileName = 'industry/oil.con', transf = { [13] = 900, [14] = 100, [15] = 0 } }, INDUSTRY = {} }")
        self.B.L.execute("WORLD[90200] = { CONSTRUCTION = { fileName = 'industry/oil.con', transf = { [13] = 900.4, [14] = 100, [15] = 0 } }, INDUSTRY = {} }")
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Companies", "MakeGreen", { companyEntity = api.engine.util.getPlayer(), constructionEntity = 90200 }))')
        self.settle()
        self.assertEqual(str(self.A.L.eval("GREEN[1]")), "industry/oil.con")
        self.assertEqual(str(self.B.L.eval("GREEN[1]")), "industry/oil.con")
        # one that exists only in the client's game is refused by the host, not applied to another industry
        self.B.L.execute("WORLD[90300] = { CONSTRUCTION = { fileName = 'industry/oil.con', transf = { [13] = -500, [14] = 100, [15] = 0 } }, INDUSTRY = {} }")
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Companies", "MakeGreen", { companyEntity = api.engine.util.getPlayer(), constructionEntity = 90300 }))')
        self.settle()
        self.assertEqual(int(self.A.L.eval("#GREEN")), 1)
        self.assertIn("unresolved construction reference s:90300", self.A.log())

    def test_a_town_building_the_town_grew_is_found_the_same_way(self):
        self.start()
        self.A.L.execute("WORLD[90100] = { CONSTRUCTION = { fileName = 'house/h1.con', transf = { [13] = 10, [14] = 20, [15] = 0 } }, TOWN_BUILDING = {} }")
        self.B.L.execute("WORLD[90200] = { CONSTRUCTION = { fileName = 'house/h1.con', transf = { [13] = 10, [14] = 20, [15] = 0 } }, TOWN_BUILDING = {} }")
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeTownBuildingSetBlockedDevelopmentCmd(90200, true))")
        self.settle()
        self.assertEqual(self.A.L.eval("WORLD[90100].blocked"), True)


class WorldSettingsTest(OpsBase):
    def test_calendar_weather_and_time_of_day_are_set_on_every_game(self):
        self.start()
        self.B.send_gui('local c = api.cmd\n'
                        'c.sendCommand(c.makeGameSetTimeOfDayCmd(43200))\n'
                        'c.sendCommand(c.makeScriptingSendEventCmd("", "GameTime", "OnManualGameTimeChanged", 43200))\n'
                        'c.sendCommand(c.makeScriptingSendEventCmd("", "GameTime", "SetMode", { mode = "Dynamic" }))\n'
                        'c.sendCommand(c.makeGameSetCalendarSpeedCmd(60000))\n'
                        'c.sendCommand(c.makeGameSetCloudCoverageCmd(0.5))\n'
                        'c.sendCommand(c.makeGameSetDateCmd(api.type.Date.new(1925, 6, 1)))\n'
                        'c.sendCommand(c.makeScriptingSendEventCmd("", "CloudCoverage", "SetMode", { weatherMode = "Constant" }))')
        self.settle()
        self.agree()
        self.assertIn("world 60000 43200 0.5 Dynamic 43200 Constant", self.A.snapshot())
        self.assertEqual(int(self.A.L.eval("GAME.date.year")), 1925)

    def test_the_local_time_of_day_mode_follows_each_machines_clock_and_is_not_replicated(self):
        self.start()
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "GameTime", "SetMode", { mode = "Local" }))')
        self.settle()
        self.assertEqual(self.B.actions_shipped(), [])
        self.assertIn("the Local time-of-day mode follows each machine's clock", self.B.log())
        self.assertEqual(str(self.B.L.eval("GAME.timeMode")), "Local", "it still runs, on the game that asked")
        self.assertEqual(self.A.L.eval("GAME.timeMode"), None)


CTX = "{ keyOf = function(e) return 's:' .. e end, plain = function(v) return v end }"


class ModuleTest(unittest.TestCase):
    """The module and the lockstep additions on their own, inside one mocked game (no session running)."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-ops-")
        self.G = OpsGame("solo", 0, self.tmp, 50000, 0, 7)
        self.L = self.G.L
        self.ops = self.L.eval("GUI_ENV.ug_require('feverlink3_cmds_ops.lua')")
        self.K = self.L.eval("GUI_ENV.ug_require('feverlink3_lockstep.lua')")
        self.K.registerCommands(self.ops.commands)
        self.K.registerEvents(self.ops.events)

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    # ---- reading and rebuilding engine objects

    def test_marshal_reads_plain_values_engine_objects_and_userdata(self):
        marshal = self.ops.internals.marshal
        t = marshal(self.L.eval("{ a = 1, b = { 2, 3 }, c = 'x', d = true }"))
        self.assertEqual((t["a"], t["b"][1], t["b"][2], t["c"], t["d"]), (1, 2, 3, "x", True))
        cfg = self.L.eval("BUILD_CONFIG(GUI_API, { purchaseTime = 7, parts = { { model = 'vehicle/truck.mdl', color = { 1, 0, 0 } } } })")
        t = marshal(cfg)
        self.assertEqual(t["vehicles"][1]["purchaseTime"], 7)
        self.assertEqual(t["vehicles"][1]["part"]["color"]["x"], 1)
        self.assertEqual(t["vehicles"][1]["part"]["modelId"], 103)

        class Vector:   # what the game hands out for a vector is userdata: x, y, z, no member list
            x, y, z = 1.5, 2.5, 3.5

        t = marshal(Vector())
        self.assertEqual((t["x"], t["y"], t["z"]), (1.5, 2.5, 3.5))
        listed = Vector()   # a member list that is empty (or that cannot be counted) does not hide the coordinates
        setattr(listed, "__members", [])
        t = marshal(listed)
        self.assertEqual((t["x"], t["y"], t["z"]), (1.5, 2.5, 3.5))

    def test_marshal_refuses_what_cannot_travel_and_says_what(self):
        marshal = self.ops.internals.marshal

        class Opaque:
            pass

        self.assertEqual(marshal(Opaque())[0], None)
        self.assertIn("cannot read", marshal(Opaque())[1])
        self.assertEqual(marshal(self.L.eval("{ f = print }")), (None, "f: a function cannot be replicated"))
        self.assertIn("keyed by a boolean", marshal(self.L.eval("{ [true] = 1 }"))[1])
        self.assertIn("too deep", marshal(self.L.eval("(function() local t = {} t.t = t return t end)()"))[1])

    def test_rebuild_makes_an_equal_object_skips_read_only_members_and_names_a_failing_one(self):
        I = self.ops.internals
        tree = I.marshal(self.L.eval("BUILD_LINE(GUI_API, { priority = 2.5, stops = { { group = 58, min = 9, alt = { { 1, 2 } } } } })"))
        skipped = self.L.table()
        obj = I.rebuild("Line", tree, "Line", skipped)
        self.assertEqual(str(self.L.eval("tostring")(obj)), "<Line>", "an engine object, not the plain table")
        again = I.marshal(obj)
        self.assertIs(I.contained(tree, again, "Line", skipped), True)
        self.assertTrue(skipped["vehicleInfo"], "the derived member was skipped, not set")
        # a member the object does not have is an error that names the path
        bad = self.L.eval("{ stops = { { stationGroup = 58, nonsense = 1 } } }")
        failed = self.L.table()
        I.rebuild("Line", bad, "Line", self.L.table(), failed)
        self.assertIn("Line.stops[1].nonsense:", failed["nonsense"])
        self.assertIn("no member 'nonsense' in LineStop", failed["nonsense"])

    def test_contained_finds_what_a_rebuild_lost(self):
        contained = self.ops.internals.contained
        lua = self.L.eval
        self.assertIs(contained(lua("{ a = 1, b = { 2, 3 } }"), lua("{ a = 1, b = { 2, 3 }, extra = 9 }"), "t", lua("{}")), True)
        self.assertEqual(contained(lua("{ a = 1 }"), lua("{ a = 2 }"), "t", lua("{}")), (False, "t.a"))
        self.assertEqual(contained(lua("{ a = 1 }"), lua("{}"), "t", lua("{}")), (False, "t.a"))
        self.assertEqual(contained(lua("{ b = { 2, 3 } }"), lua("{ b = { 2 } }"), "t", lua("{}")), (False, "t.b#"))
        self.assertIs(contained(lua("{ a = 1 }"), lua("{ a = 2 }"), "t", lua("{ a = true }")), True, "a skipped member is not compared")
        self.assertIs(contained(lua("{ a = 0.1 }"), lua("{ a = 0.1000000001 }"), "t", lua("{}")), True, "floats compare with a tolerance")

    # ---- permission categories and the event registry

    def test_every_command_and_event_has_the_category_the_host_checks(self):
        want = {
            "spend": ["makeVehicleBuyCmd", "makeVehicleReplaceCmd", "makeJournalBookAssetCmd"],
            "bulldoze": ["makeVehicleSellCmd", "makeLineDestroyCmd"],
            "build": ["makeLineCreateCmd", "makeLineUpdateCmd"],
            "other": ["makeVehicleSetLineCmd", "makeVehicleSendToDepotCmd", "makeVehicleReverseCmd", "makeVehicleSetStoppedByUserCmd",
                      "makeEntitySetNameCmd", "makeEntitySetColorCmd", "makeGameSetCalendarSpeedCmd", "makeGameSetDateCmd",
                      "makeGameSetTimeOfDayCmd", "makeGameSetCloudCoverageCmd", "makeStockListDiscardCargoCmd",
                      "makeTownBuildingSetBlockedDevelopmentCmd", "makeTownSetDevelopmentActiveCmd", "makeVehicleTryToDepartCmd"],
        }
        for category, names in want.items():
            for name in names:
                self.assertEqual(self.K.categoryOf(name), category, name)
        registered = set(str(k) for k in self.ops.commands.keys())
        self.assertTrue(registered >= {n for names in want.values() for n in names})
        # events: by their arguments
        ev = lambda i, n: self.L.eval("{ n = 4, '', '%s', '%s', {} }" % (i, n))   # noqa: E731
        self.assertEqual(self.K.categoryOf("makeScriptingSendEventCmd", ev("Loan", "Obtain")), "spend")
        self.assertEqual(self.K.categoryOf("makeScriptingSendEventCmd", ev("Companies", "MakeGreen")), "spend")
        self.assertEqual(self.K.categoryOf("makeScriptingSendEventCmd", ev("Subvention", "onAccept")), "other")
        self.assertEqual(self.K.categoryOf("makeScriptingSendEventCmd", ev("Notifications", "dismiss")), "local")
        perms = self.K.newPermissions()
        self.K.applyPermissions(perms, self.L.eval("{ build = false, bulldoze = false, spend = false, other = false }"))
        self.assertTrue(self.K.permitted(perms, "local"), "no grant covers an event nobody replicates")
        self.assertFalse(self.K.permitted(perms, "spend"))

    def test_modules_register_events_side_by_side(self):
        self.K.registerEvents(self.L.eval("{ Mine = { One = { category = 'build' } } }"))
        self.K.registerEvents(self.L.eval("{ Loan = { Extra = { category = 'bulldoze' } }, Mine = { Two = { category = 'spend' } } }"))
        cat = lambda i, n: self.K.categoryOf("makeScriptingSendEventCmd", self.L.eval("{ n = 4, '', '%s', '%s', {} }" % (i, n)))   # noqa: E731
        self.assertEqual([cat("Loan", "Obtain"), cat("Loan", "Extra"), cat("Mine", "One"), cat("Mine", "Two")],
                         ["spend", "bulldoze", "build", "spend"], "the later registrations added, none replaced")

    def test_the_event_dispatcher_encodes_registered_events_and_refuses_the_rest(self):
        spec = self.K.COMMANDS.makeScriptingSendEventCmd
        ctx = self.L.eval(CTX)
        args = self.L.eval("{ n = 4, '', 'Companies', 'MakeGreen', { companyEntity = 7, constructionEntity = 81, permitKey = 'k' } }")
        body = spec.encode("makeScriptingSendEventCmd", args, ctx)
        self.assertEqual(body.fn, "makeScriptingSendEventCmd")
        self.assertEqual(body.args[2], "Companies")
        self.assertEqual(body.args[4].companyEntity["__ref"], "player")   # (a bare .__ref would be name-mangled inside a class)
        self.assertEqual(body.args[4].constructionEntity.f, "industry/coal.con")
        out = spec.encode("makeScriptingSendEventCmd", self.L.eval("{ n = 4, '', 'Notifications', 'add', {} }"), ctx)
        self.assertEqual(out[0], None)
        self.assertIn("Notifications/add is not replicated", out[1])
        dctx = self.L.eval("{ player = 7, resolve = function(k) if k == 's:81' then return 81 end end }")
        back = spec.decode(body, dctx)
        self.assertEqual((back[2], back[3], back[4].constructionEntity, back[4].companyEntity), ("Companies", "MakeGreen", 81, 7))
        # an event whose entity this game cannot find is refused with the reason, not applied to something else
        lost = self.L.eval("{ fn = 'makeScriptingSendEventCmd', args = { n = 4, '', 'Companies', 'MakeGreen', { constructionEntity = "
                           "{ __ref = 'entity', key = 's:9999', f = 'industry/none.con', p = { x = 1, y = 1, z = 0 } } } } }")
        refused = spec.decode(lost, self.L.eval("{ player = 7, resolve = function(k) end }"))
        self.assertEqual(refused[0], None)
        self.assertIn("constructionEntity: unresolved construction reference s:9999", refused[1])

    def test_claim_entity_serves_creations_in_order(self):
        claim = self.K.claimEntity
        lua = self.L.eval
        self.assertEqual(claim(lua("{ 1, 2 }"), lua("{ 1, 2, 7, 5, 6 }"), lua("{}")), 5)
        self.assertEqual(claim(lua("{ 1, 2 }"), lua("{ 1, 2, 7, 5, 6 }"), lua("{ [5] = true }")), 6)
        self.assertEqual(claim(lua("{ 1, 2 }"), lua("{ 1, 2, 5 }"), lua("{ [5] = true }")), None)
        self.assertEqual(claim(lua("{ 1, 2 }"), lua("{ 1, 2, 5 }"), None), 5)

    # ---- references

    def test_legacy_bodies_of_the_generic_schema_still_decode(self):
        spec = self.ops.commands.makeVehicleBuyCmd
        body = self.L.eval("{ fn = 'makeVehicleBuyCmd', args = { n = 3, { __ref = 'player' }, { __ref = 'entity', key = 's:55' }, {} } }")
        args = spec.decode(body, self.L.eval("{ player = 7, resolve = function(k) if k == 's:55' then return 55 end end }"))
        self.assertEqual((args[1], args[2], args.n), (7, 55, 3))

    def test_a_hint_about_the_owning_building_beats_a_stale_key_and_a_wrong_one_is_ignored(self):
        spec = self.ops.commands.makeVehicleReverseCmd
        # the key says vehicle 70, the line argument is not a vehicle: kind checks
        out = spec.decode(self.L.eval("{ fn = 'x', args = { n = 1, { __ref = 'entity', key = 's:71' } } }"),
                          self.L.eval("{ player = 7, resolve = function(k) return tonumber(k:match('%d+')) end }"))
        self.assertEqual(out[0], None)
        self.assertIn("vehicle s:71 does not exist here", out[1])
        # depots: the building (key and file) and the place in it name depot 62, whatever the key says
        depot = self.ops.commands.makeVehicleBuyCmd
        resolve = "function(k) return tonumber(k:match('%d+')) end"
        dctx = self.L.eval("{ player = 7, resolve = %s }" % resolve)
        hinted = self.L.eval("{ fn = 'x', args = { n = 3, { __ref = 'player' }, { __ref = 'entity', key = 's:55', at = { con = { key = 's:59', f = 'station2.con' }, i = 1 } }, {} } }")
        self.assertEqual(depot.decode(hinted, dctx)[2], 62)
        wrong_file = self.L.eval("{ fn = 'x', args = { n = 3, { __ref = 'player' }, { __ref = 'entity', key = 's:55', at = { con = { key = 's:59', f = 'other.con' }, i = 1 } }, {} } }")
        self.assertEqual(depot.decode(wrong_file, dctx)[2], 55, "a hint that does not fit is ignored, the key stands")

    def test_a_depot_nobody_bound_is_found_through_the_building_that_has_it(self):
        # each game's player built the same depot in the session: other ids, but the building has a key on both
        a = OpsGame("a", 0, self.tmp, 60000, 0, 7)
        b = OpsGame("b", 1, self.tmp, 61000, 100, 17)
        a.L.execute("WORLD[90001] = { CONSTRUCTION = { fileName = 'depot2.con', transf = { [13] = 5, [14] = 6, [15] = 0 }, depots = { 90010 }, stations = {} } }; "
                    "WORLD[90010] = { VEHICLE_DEPOT = {} }")
        b.L.execute("WORLD[91001] = { CONSTRUCTION = { fileName = 'depot2.con', transf = { [13] = 5, [14] = 6, [15] = 0 }, depots = { 91010 }, stations = {} } }; "
                    "WORLD[91010] = { VEHICLE_DEPOT = {} }")
        enc = b.L.eval("function() local ops = GUI_ENV.ug_require('feverlink3_cmds_ops.lua') "
                       "local C = GUI_ENV.ug_require('feverlink3_common.lua') "
                       "local ctx = { plain = C.plain, keyOf = function(e) if e == 91001 then return '1:7' end return 's:' .. e end } "
                       "local body = ops.commands.makeVehicleBuyCmd.encode('makeVehicleBuyCmd', { n = 3, 17, 91010, BUILD_CONFIG(GUI_API, { parts = { { model = 'vehicle/truck.mdl' } } }) }, ctx) "
                       "return C.serialize(body) end")()
        self.assertIn('1:7', enc)
        self.assertNotIn("91010", enc, "the depot's own id is not offered as a name: it is not one the other game has")
        dec = a.L.eval("function(text) local ops = GUI_ENV.ug_require('feverlink3_cmds_ops.lua') "
                       "local C = GUI_ENV.ug_require('feverlink3_common.lua') "
                       "local body = C.parse(text) "
                       "local args = ops.commands.makeVehicleBuyCmd.decode(body, { player = 7, resolve = function(k) if k == '1:7' then return 90001 end end }) "
                       "return args[2] end")(enc)
        self.assertEqual(dec, 90010, "game A's depot, found through A's building")

    def test_a_depot_of_a_building_whose_key_this_game_never_bound_is_found_by_the_buildings_file_and_place(self):
        # sweep run 20261013-074329 (F2/F3): "unresolved depot reference nil" on the receiving game
        a = OpsGame("a", 0, self.tmp, 60000, 0, 7)
        b = OpsGame("b", 1, self.tmp, 61000, 100, 17)
        a.L.execute("WORLD[90001] = { CONSTRUCTION = { fileName = 'depot2.con', transf = { [13] = 5, [14] = 6, [15] = 0 }, depots = { 90010 }, stations = {} } }; "
                    "WORLD[90010] = { VEHICLE_DEPOT = {} }")
        b.L.execute("WORLD[91001] = { CONSTRUCTION = { fileName = 'depot2.con', transf = { [13] = 5, [14] = 6, [15] = 0 }, depots = { 91010 }, stations = {} } }; "
                    "WORLD[91010] = { VEHICLE_DEPOT = {} }")
        enc = b.L.eval("function() local ops = GUI_ENV.ug_require('feverlink3_cmds_ops.lua') "
                       "local C = GUI_ENV.ug_require('feverlink3_common.lua') "
                       "local ctx = { plain = C.plain, keyOf = function(e) if e == 91001 then return '1:7' end return 's:' .. e end } "
                       "local body = ops.commands.makeVehicleBuyCmd.encode('makeVehicleBuyCmd', { n = 3, 17, 91010, BUILD_CONFIG(GUI_API, { parts = { { model = 'vehicle/truck.mdl' } } }) }, ctx) "
                       "return C.serialize(body) end")()
        dec = a.L.eval("function(text) local ops = GUI_ENV.ug_require('feverlink3_cmds_ops.lua') "
                       "local C = GUI_ENV.ug_require('feverlink3_common.lua') "
                       "local body = C.parse(text) "
                       "local args = ops.commands.makeVehicleBuyCmd.decode(body, { player = 7, resolve = function(k) return nil end }) "
                       "return args and args[2] or 'refused' end")(enc)
        self.assertEqual(dec, 90010, "the key '1:7' is unknown here: the building is found by its file and place")

    def test_garbage_from_another_player_is_refused_and_never_raises(self):
        # a body that makes the codec raise would stop the simulation half and be retried at every step
        dctx = self.L.eval("{ player = 7, resolve = function(k) return tonumber(k:match('%d+')) end }")
        buy = self.ops.commands.makeVehicleBuyCmd
        for text, expect in [
            ("{ fn = 'x', args = { n = 3, 'me', 5, 'config' } }", "malformed player reference"),
            ("{ fn = 'x', args = { n = 3, { __ref = 'player' }, { __ref = 'entity', key = 's:55', at = { con = { key = 's:59', f = 'station2.con',"
             " p = { x = 'far' } }, i = 1 } }, {} } }", "decode failed"),
            ("{ fn = 'x', args = { n = 3, { __ref = 'player' }, { __ref = 'entity', key = 's:55' }, 'config' } }", "malformed TransportVehicleConfig"),
            ("{ fn = 'x', args = { n = 3, { __ref = 'player' }, { __ref = 'entity', key = 's:55' }, { vehicles = 7 } } }", "argument 3"),
        ]:
            out = buy.decode(self.L.eval(text), dctx)
            self.assertEqual(out[0], None, text)
            self.assertIn(expect, out[1], text)
        # the same for an event: a position that is not a position
        spec = self.K.COMMANDS.makeScriptingSendEventCmd
        out = spec.decode(self.L.eval("{ fn = 'x', args = { n = 4, '', 'Companies', 'MakeGreen', { constructionEntity = "
                                      "{ __ref = 'entity', key = 's:81', f = 'industry/coal.con', p = { x = {} } } } } }"), dctx)
        self.assertEqual(out[0], None)
        self.assertIn("failed", out[1])
        # and an encode that raises answers "native" instead of throwing into the window
        weird = self.L.eval("{ n = 3, 7, 55, setmetatable({}, { __pairs = function() error('boom') end }) }")
        out = buy.encode("makeVehicleBuyCmd", weird, self.L.eval(CTX))
        self.assertEqual(out[0], None)
        self.assertIn("encode failed", out[1])

    def test_a_date_is_read_by_its_fields(self):
        class Date:   # userdata with no member list: the game's calendar widget reads year, month and day
            year, month, day = 1925, 6, 1

        spec = self.ops.commands.makeGameSetDateCmd
        body = spec.encode("makeGameSetDateCmd", self.L.eval("function(d) return { n = 1, d } end")(Date()), self.L.eval(CTX))
        self.assertEqual((body.args[1].year, body.args[1].month, body.args[1].day), (1925, 6, 1))
        back = spec.decode(body, self.L.eval("{ player = 7, resolve = function() end }"))
        self.assertEqual((back[1].year, back[1].month, back[1].day), (1925, 6, 1))
        out = spec.encode("makeGameSetDateCmd", self.L.eval("{ n = 1, 'tomorrow' }"), self.L.eval(CTX))
        self.assertEqual(out, (None, "argument 1: not a date"))

    def test_journal_entries_keep_their_sub_category_and_unknown_ones_are_refused(self):
        spec = self.ops.commands.makeJournalBookAssetCmd
        ctx = self.L.eval(CTX)
        entry = self.L.eval("(function() local e = GUI_API.type.JournalEntry.new() e.amount = -300 e.time = -1 "
                            "e.category.type = GUI_API.type.JournalEntry.Type.CONSTRUCTION "
                            "e.category.construction = GUI_API.type.JournalEntry.Construction.STATION return e end)()")
        body = spec.encode("makeJournalBookAssetCmd", self.L.eval("function(e) return { n = 3, 7, e } end")(entry), ctx)
        self.assertEqual((body.args[2].amount, body.args[2].type, body.args[2].sub), (-300, "CONSTRUCTION", "STATION"))
        back = spec.decode(body, self.L.eval("{ player = 7, resolve = function() end }"))
        self.assertEqual((back[2].amount, back[2].category.type, back[2].category.construction), (-300, 4, 4))
        junk = self.L.eval("(function() local e = GUI_API.type.JournalEntry.new() e.amount = 1 e.category.type = 99 return e end)()")
        out = spec.encode("makeJournalBookAssetCmd", self.L.eval("function(e) return { n = 3, 7, e } end")(junk), ctx)
        self.assertEqual(out, (None, "argument 2: a journal category this module does not know"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
