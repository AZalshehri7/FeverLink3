"""Lockstep test for the FeverLink3 Lua mod: two mocked Transport Fever 3 games run the real mod files through
lupa (Lua 5.2), with a mocked host relay standing in for FeverLink3.exe.

Each game has two Lua environments, as the engine has two Lua states: the simulation half (update, handleEvent)
and the interface half (guiUpdate), sharing one mocked world but not their module locals nor their `api.cmd`
table. The mocked engine reproduces what matters for lockstep:
  * entity ids allocated during the session differ between the games (the client allocates 1000 higher), ids
    loaded from the savegame do not;
  * commands sent from the interface state run on the engine's thread at the next simulation tick, with their
    callback; commands sent from inside the simulation update run at once and may not carry a callback;
  * a bought vehicle appears one step after its command.

Run:  python -I tests/lua/test_lockstep.py        (or: python -m unittest discover -s tests/lua)
Set FEVERLINK3_TEST_VERBOSE=1 to print both games' mod.log on failure.
"""
import os
import re
import shutil
import sys
import tempfile
import unittest

import lupa.lua52 as lupa

# The build module only refuses remote constructions with FEVERLINK3_REMOTE_CONSTRUCTIONS=0 (a developer switch);
# tests of the block set it themselves.
os.environ.pop("FEVERLINK3_REMOTE_CONSTRUCTIONS", None)

HERE = os.path.dirname(os.path.abspath(__file__))
CONTENT = os.path.join(HERE, "..", "..", "mod", "feverlink3", "content")
TAB = "\t"
STEP = 200
T0 = 18600


def lua_str(text):
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"') + '"'


def read_text(path):
    if not os.path.exists(path):
        return ""
    with open(path, encoding="utf-8") as f:
        return f.read()


def read_src(name):
    return read_text(os.path.join(CONTENT, name))


COMMON_SRC = read_src("feverlink3_common.lua")
LOCKSTEP_SRC = read_src("feverlink3_lockstep.lua")
BRIDGE_SRC = read_src("feverlink3_bridge.script.lua")
HUD_SRC = read_src("feverlink3_hud.lua")
HOOK_SRC = read_src("feverlink3_uihook.lua")
TRACE_SRC = read_src("feverlink3_towntrace.lua")
PORTABLE_SRC = read_src("feverlink3_portable.lua")
LVL_SRC = read_src("feverlink3_livevsloaded.lua")
LINKSTATS_SRC = read_src("feverlink3_linkstats.lua")
# every io/os use of the mod (feverlink3_link.lua) and its logging (feverlink3_log.lua): the common module loads both
LINK_SRC = read_src("feverlink3_link.lua")
LOG_SRC = read_src("feverlink3_log.lua")
# the command-schema modules of the other areas (feverlink3_cmds_*.lua), by module name
CMDS_SRC = {
    os.path.splitext(f)[0]: read_src(f)
    for f in sorted(os.listdir(CONTENT))
    if f.startswith("feverlink3_cmds_") and f.endswith(".lua")
}

# feverlink3_common.lua in a bare Lua state (no harness): ug_require finds the two modules it loads, the link and the log module
STANDALONE_REQUIRE = r'''
function ug_require(p)
  if p:find("feverlink3_link.lua", 1, true) then return assert(load(LINK_SRC, "=link", "t"))() end
  if p:find("feverlink3_log.lua", 1, true) then return assert(load(LOG_SRC, "=log", "t"))() end
  error("no module " .. tostring(p))
end
'''


def load_common(rt):
    """Runs feverlink3_common.lua in the runtime `rt` and returns the module (a ug_require of its own unless rt has one)."""
    g = rt.globals()
    g.LINK_SRC, g.LOG_SRC = LINK_SRC, LOG_SRC
    if g.ug_require is None:
        rt.execute(STANDALONE_REQUIRE)
    return rt.execute(COMMON_SRC)


# ---------------------------------------------------------------- mocked engine (one per game)

MOCK = r'''
GAME = { time = %(t0)d, speed = 1, applied = {}, guiQueue = {}, inUpdate = false, nextEntity = %(first)d, deferred = {} }
WORLD = {
  [7]  = { PLAYER = {} },
  [54] = { CONSTRUCTION = { fileName = "depot.con", transf = { [13] = 100, [14] = 200, [15] = 5 } } },
  [55] = { VEHICLE_DEPOT = {} },
  [56] = { CONSTRUCTION = { fileName = "station.con", transf = { [13] = -40.5, [14] = 12, [15] = 0 } } },
  -- two towns' buildings: each TOWN_BUILDING names its town and its CONSTRUCTION entity
  [60] = { CONSTRUCTION = { fileName = "house_a.con", transf = { [13] = 10.04, [14] = 20, [15] = 1 } } },
  [61] = { CONSTRUCTION = { fileName = "house_b.con", transf = { [13] = 30, [14] = 40, [15] = 2 } } },
  [62] = { CONSTRUCTION = { fileName = "shop.con", transf = { [13] = 500, [14] = 600, [15] = 3 } } },
  [63] = { TOWN_BUILDING = { town = 90, construction = 60, level = 1, depth = 1, timeBuilt = 100, parcels = { 1 } } },
  [64] = { TOWN_BUILDING = { town = 90, construction = 61, level = 2, depth = 1, timeBuilt = 200, parcels = { 2, 3 } } },
  [65] = { TOWN_BUILDING = { town = 91, construction = 62, level = 1, depth = 2, timeBuilt = 300, parcels = { 4 } } },
}
ACCOUNT = { balance = 1000000, loan = 250000 }
JOURNAL = {}
-- the application object: saving calls back one tick later, loading is a reload the test performs (RELOAD)
SAVES = { { saveName = "hostsave", path = "/saves/hostsave.sav" } }
SAVED, LOADED, SAVE_FAIL, SAVE_CBS, RELOAD_PENDING = {}, nil, false, {}, false
app = {
  SaveGameNamespace = { getSavegame = function() return 1 end },
  saveGame = function(name, cb, a, b)
    assert(a == false and b == true, "save flags as in MPFever")
    if SAVE_FAIL then error("disk full") end
    SAVED[#SAVED + 1] = name
    SAVES[#SAVES + 1] = { saveName = name, path = "/saves/" .. name .. ".sav" }
    SAVE_CBS[#SAVE_CBS + 1] = cb
  end,
  findAllSavegames = function(ns) assert(ns == 1); return SAVES end,
  loadGame = function(id, flag) assert(id.saveGameNamespace == 1 and id.path); LOADED = id.saveGameName; RELOAD_PENDING = true end,
  -- the start gate (script-doc app.html): a game loaded after setWaitForStartReadyGame does not start by itself. Modelled (not seen in
  -- the game): while it waits the interface frames run, the engine applies no command and runs no step, until startReadyGame.
  setWaitForStartReadyGame = function() GAME.waitNext = true end,
  isWaitForStartReadyGame = function() return GAME.waitReady == true or GAME.waitNext == true end,
  startReadyGame = function() READY_STARTS = (READY_STARTS or 0) + 1; GAME.held = false end,
}
local CT = { GAME_TIME = 1, GAME_SPEED = 2, ACCOUNT = 3, CONSTRUCTION = 4, VEHICLE_DEPOT = 5, TRANSPORT_VEHICLE = 6, LINE = 7, PLAYER = 8, TOWN_BUILDING = 9 }
local NAMEOF = {}
for k, v in pairs(CT) do NAMEOF[v] = k end

local function newEntity(comps)
  GAME.nextEntity = GAME.nextEntity + 1
  WORLD[GAME.nextEntity] = comps
  return GAME.nextEntity
end

local function idsWith(name)
  local ids = {}
  for e, w in pairs(WORLD) do if w[name] then ids[#ids + 1] = e end end
  table.sort(ids)
  return ids
end

local function mk(name)
  return function(...) return { __cmd = name, args = { n = select("#", ...), ... } } end
end

EVENTS_SEEN = {}

-- the engine applying a command (on its own thread: between frames, before the steps of a tick)
function EXECUTE(cmd, cb)
  local name, a = cmd.__cmd, cmd.args
  if name == "makeGameSetSpeedCmd" then
    if a[1] == 0 and GAME.speed > 0 and (OVERSHOOT or 0) > 0 then GAME.coast = OVERSHOOT end
    if a[1] > 0 then GAME.coast = 0 end
    GAME.speed = a[1]
    if cb then cb({}, true, {}) end
    return
  end
  if name == "makeScriptingSendEventCmd" then
    EVENTS_SEEN[#EVENTS_SEEN + 1] = { a[2], a[3], a[1] }
    SIM_EVENT(a[1], a[2], a[3], a[4])
    if cb then cb({}, true, {}) end
    return
  end
  if name == "makeJournalBookAssetCmd" then
    assert(a[1] == 7, "player entity expected")
    ACCOUNT.balance = ACCOUNT.balance + a[2].amount
    JOURNAL[#JOURNAL + 1] = a[2].amount
    if cb then cb({}, true, {}) end
    return
  end
  local rec = { name = name, time = GAME.time, args = a }
  local created = nil
  if name == "makeVehicleBuyCmd" then
    if NO_MONEY then error("not enough money") end   -- a test sets it on every game: the engine refuses the purchase
    assert(a[1] == 7, "player entity expected, got " .. tostring(a[1]))
    assert(WORLD[a[2]] and WORLD[a[2]].VEHICLE_DEPOT, "no depot " .. tostring(a[2]))
    GAME.nextEntity = GAME.nextEntity + 1
    created = GAME.nextEntity
    if GAME.inUpdate then
      GAME.deferred[#GAME.deferred + 1] = created   -- the vehicle entity appears at the next step
    else
      WORLD[created] = { TRANSPORT_VEHICLE = {} }
    end
  elseif name == "makeLineCreateCmd" then
    assert(a[3] == 7, "player entity expected, got " .. tostring(a[3]))
    created = newEntity({ LINE = { stops = {} } })
  elseif name == "makeVehicleSetLineCmd" then
    assert(WORLD[a[1]] and WORLD[a[1]].TRANSPORT_VEHICLE, "no vehicle " .. tostring(a[1]))
    assert(WORLD[a[2]] and WORLD[a[2]].LINE, "no line " .. tostring(a[2]))
  else
    error("unknown command " .. tostring(name))
  end
  rec.entity = created
  GAME.applied[#GAME.applied + 1] = rec
  if cb then cb(created and { resultVehicleEntity = created, resultEntity = created } or {}, true, created and { { created, 0 } } or {}) end
end

function MAKE_API()
  local cmd = {
    sendCommand = function(c, cb, progress)
      if GAME.phase == "update" then UPDATE_WRITES[#UPDATE_WRITES + 1] = "sendCommand " .. tostring(c and c.__cmd) end
      if GAME.inUpdate then
        -- finding from MPFever (MIT): callbacks are refused inside the simulation update
        if cb then error("Callbacks are currently disallowed") end
        EXECUTE(c, nil)
        return
      end
      GAME.guiQueue[#GAME.guiQueue + 1] = { c, cb }
    end,
  }
  for _, n in ipairs({ "makeGameSetSpeedCmd", "makeScriptingSendEventCmd", "makeVehicleBuyCmd", "makeLineCreateCmd", "makeVehicleSetLineCmd", "makeJournalBookAssetCmd", "makeTownUpdateSizeCmd", "makeTownUpdateCargoNeedsCmd" }) do
    cmd[n] = mk(n)
  end
  return {
    cmd = cmd,
    type = {
      ComponentType = CT,
      JournalEntry = { new = function() return { category = {} } end, Type = { OTHER = 1 } },
      SavegameId = { new = function() return {} end },
      -- the TransportMode enum (api.type.enum.TransportMode); its values here as the game's maps are keyed (PERSON 0 ... 15)
      enum = { TransportMode = { PERSON = 0, CARGO = 1, CAR = 2, BUS = 3, TRUCK = 4, TRAM = 5, ELECTRIC_TRAM = 6, TRAIN = 7,
        ELECTRIC_TRAIN = 8, AIRCRAFT = 9, SHIP = 10, SMALL_AIRCRAFT = 11, SMALL_SHIP = 12, HELICOPTER = 13, TRAM_TRACK = 14,
        ELECTRIC_TRAM_TRACK = 15 },
        -- land uses (getCargoSupplyAndLimit's LandUseType) and street vs track (BaseEdge.roadType), as tf3-docs api/type lists them
        LandUseType = { RESIDENTIAL = 0, COMMERCIAL = 1, INDUSTRIAL = 2 }, RoadType = { STREET = 0, TRACK = 1 } },
    },
    engine = {
      getComponent = function(e, t)
        if t == CT.GAME_TIME then return { gameTime = GAME.time } end
        if t == CT.GAME_SPEED then return { speedup = GAME.speed } end
        if t == CT.ACCOUNT then return ACCOUNT end
        local w = WORLD[e]
        return w and w[NAMEOF[t]] or nil
      end,
      entityExists = function(e) return WORLD[e] ~= nil end,
      -- Engine.Revision of an entity ({ num = { a, b, c } }, tf3-docs api/engine getRevision); an entity that is gone raises
      getRevision = function(e) if WORLD[e] == nil then error("no entity " .. tostring(e)) end return { num = { 1000 + e, 0, 0 } } end,
      util = { getWorld = function() return 1 end, getPlayer = function() return 7 end },
      forEachEntityWithComponent = function(fn, t) for _, e in ipairs(idsWith(NAMEOF[t])) do fn(e) end end,
      system = { lineSystem = { getLines = function() return idsWith("LINE") end } },
    },
  }
end

debugPrint = function() end
'''

# ---------------------------------------------------------------- the mod, loaded twice (two Lua states)

HARNESS = r'''
-- The game's Lua 5.2.2 raises on "%d" (and "%i") for a number beyond the 32-bit range ("bad argument #2 to 'format' (not a number
-- in proper range)", found by the 2026-10-13 sweep: a balance of 5 billion); lupa's Lua does not, so the limit is emulated here.
do
  local real = string.format
  string.format = function(f, ...)
    if type(f) == "string" and f:find("%%[%-%+ #0-9.]*[di]") then
      local args, n = { ... }, 0
      for spec in f:gsub("%%%%", ""):gmatch("%%[%-%+ #0-9.]*([%a])") do
        n = n + 1
        if (spec == "d" or spec == "i") and type(args[n]) == "number" and math.abs(args[n]) >= 2147483648 then
          error("bad argument #" .. (n + 1) .. " to 'format' (not a number in proper range)", 2)
        end
      end
    end
    return real(f, ...)
  end
end
local function deepcopy(v)
  if type(v) ~= "table" then return v end
  local out = {}
  for k, x in pairs(v) do out[deepcopy(k)] = deepcopy(x) end
  return out
end

-- the game-script state: written by the simulation half, readable from both (copies: no shared references). The wiki's update
-- "may not write the state itself": what a script writes or sends during the update phase is recorded (UPDATE_WRITES).
local STORED = nil
UPDATE_WRITES = {}
STATE = {
  get = function(self) return deepcopy(STORED) end,
  set = function(self, v)
    if GAME.phase == "update" then UPDATE_WRITES[#UPDATE_WRITES + 1] = "state:set" end
    STORED = deepcopy(v)
  end,
  -- subscriptions (script-doc gamescript.html): handleEvent receives the events of the names subscribed, or all. Until the script's first
  -- subscription the mock delivers everything (the docs do not say what an unsubscribed script receives; the base scripts subscribe on the
  -- game's init events, which therefore arrive without one); after it, an event of another name is dropped (SUBS.dropped).
  subscribeToAllEvents = function()
    if GAME.phase == "update" then UPDATE_WRITES[#UPDATE_WRITES + 1] = "state:subscribeToAllEvents" end
    SUBS.all = true
  end,
  subscribeToEvent = function(self, name)
    if GAME.phase == "update" then UPDATE_WRITES[#UPDATE_WRITES + 1] = "state:subscribeToEvent" end
    assert(type(name) == "string", "subscribeToEvent(name : string)")
    SUBS.names[name] = true
    SUBS.any = true
  end,
  hasEventSubscriptions = function() return SUBS.all or SUBS.any end,
}
SUBS = { all = false, any = false, names = {}, dropped = {} }
GSUBS = { all = false, names = {} }
GSTATE = {
  subscribeToAllEvents = function() GSUBS.all = true end,
  subscribeToEvent = function(self, name) assert(type(name) == "string"); GSUBS.names[name] = true end,
}

function LOAD_HALF(label)
  local env = setmetatable({}, { __index = _G })
  env.api = MAKE_API()
  env.package = { loaded = {} }
  env.FEVERLINK3_ENGINE = { adjustLoan = function(d) ACCOUNT.loan = ACCOUNT.loan + d end }   -- the injected loan accessor
  env.ug_require = function(p)
    if p:find("feverlink3_link.lua", 1, true) then return assert(load(LINK_SRC, "=link", "t", env))() end
    if p:find("feverlink3_log.lua", 1, true) then return assert(load(LOG_SRC, "=log", "t", env))() end
    if p:find("feverlink3_common") then return assert(load(COMMON_SRC, "=common", "t", env))() end
    if p:find("feverlink3_lockstep") then return assert(load(LOCKSTEP_SRC, "=lockstep", "t", env))() end
    if p:find("feverlink3_portable") then return assert(load(PORTABLE_SRC, "=portable", "t", env))() end
    if p:find("feverlink3_hud") then return assert(load(HUD_SRC, "=hud", "t", env))() end
    if p:find("feverlink3_towntrace") then return assert(load(TRACE_SRC, "=towntrace", "t", env))() end
    if p:find("feverlink3_livevsloaded") then return assert(load(LVL_SRC, "=livevsloaded", "t", env))() end
    if p:find("feverlink3_linkstats") then return assert(load(LINKSTATS_SRC, "=linkstats", "t", env))() end
    local cmds = p:match("(feverlink3_cmds_[%w_]+)")
    if cmds and CMDS_SRC[cmds] then return assert(load(CMDS_SRC[cmds], "=" .. cmds, "t", env))() end
    error("no module " .. tostring(p))
  end
  assert(load(BRIDGE_SRC, "=bridge-" .. label, "t", env))()
  return env.data(), env
end

SIM, SIM_ENV = LOAD_HALF("sim")
GUI, GUI_ENV = LOAD_HALF("gui")
GUI_API = GUI_ENV.api

-- a savegame reload: the scripts start again (fresh Lua states), the world and the session folder stay
function RELOAD()
  RELOAD_PENDING = false
  GAME.guiQueue = {}
  -- the loaded game waits for app.startReadyGame when the wait was asked for before (or at) the load
  GAME.waitReady, GAME.waitNext = GAME.waitNext == true, nil
  GAME.held = GAME.waitReady
  SIM, SIM_ENV = LOAD_HALF("sim")
  GUI, GUI_ENV = LOAD_HALF("gui")
  GUI_API = GUI_ENV.api
end

function UI_HOOK_NEW()
  local env = setmetatable({}, { __index = _G })
  env.api = MAKE_API()
  env.package = { loaded = {} }
  env.ug_require = function(p)
    if p:find("feverlink3_link.lua", 1, true) then return assert(load(LINK_SRC, "=link", "t", env))() end
    if p:find("feverlink3_log.lua", 1, true) then return assert(load(LOG_SRC, "=log", "t", env))() end
    if p:find("feverlink3_common") then return assert(load(COMMON_SRC, "=common", "t", env))() end
    if p:find("feverlink3_lockstep") then return assert(load(LOCKSTEP_SRC, "=lockstep", "t", env))() end
    if p:find("feverlink3_portable") then return assert(load(PORTABLE_SRC, "=portable", "t", env))() end
    if p:find("feverlink3_hud") then return assert(load(HUD_SRC, "=hud", "t", env))() end
    if p:find("feverlink3_uihook") then return assert(load(HOOK_SRC, "=uihook", "t", env))() end
    local cmds = p:match("(feverlink3_cmds_[%w_]+)")
    if cmds and CMDS_SRC[cmds] then return assert(load(CMDS_SRC[cmds], "=" .. cmds, "t", env))() end
    error("no module " .. tostring(p))
  end
  env.HookModule = env.ug_require("feverlink3_uihook.lua")
  env.HookModule.install()
  UI_HOOK = env
  return env
end

-- the window's side (feverlink3_ui.script.lua minus the drawing): read the snapshot, queue a chat line
function UI_ENV_NEW()
  local env = setmetatable({}, { __index = _G })
  env.package = { loaded = {} }
  env.ug_require = function(p)
    if p:find("feverlink3_link.lua", 1, true) then return assert(load(LINK_SRC, "=link", "t", env))() end
    if p:find("feverlink3_log.lua", 1, true) then return assert(load(LOG_SRC, "=log", "t", env))() end
    if p:find("feverlink3_common") then return assert(load(COMMON_SRC, "=common", "t", env))() end
    if p:find("feverlink3_portable") then return assert(load(PORTABLE_SRC, "=portable", "t", env))() end
    if p:find("feverlink3_hud") then return assert(load(HUD_SRC, "=hud", "t", env))() end
    error("no module " .. tostring(p))
  end
  env.C, env.Hud = env.ug_require("feverlink3_common"), env.ug_require("feverlink3_hud")
  return env
end
function UI_SPEED(n)
  local e = UI_ENV_NEW()
  UI_OUTBOX = UI_OUTBOX or e.Hud.newOutbox(e.C)
  return e.C.writerSend(UI_OUTBOX, "speed", { n = n })
end
function UI_SEND(text)
  local e = UI_ENV_NEW()
  UI_OUTBOX = UI_OUTBOX or e.Hud.newOutbox(e.C)
  return e.Hud.sendChat(e.C, UI_OUTBOX, text)
end
function UI_VIEW()
  local e = UI_ENV_NEW()
  local m = e.Hud.readSnapshot(e.C, nil)
  if not m then return nil end
  return e.Hud.statusText(m) .. "|" .. e.Hud.rosterText(m) .. "|" .. table.concat(e.Hud.chatLines(m, 15), ";")
end

function SIM_EVENT(src, id, name, param)
  if SUBS.any and not SUBS.all and not SUBS.names[name] then
    SUBS.dropped[#SUBS.dropped + 1] = tostring(id) .. "/" .. tostring(name)
    return
  end
  SIM.handleEvent(nil, STATE, src, id, name, param)
end
-- a frame: the bridge's interface half, then the window state's refresh (the UI hook's held commands get their stamp applies)
function FRAME()
  GUI.guiUpdate(nil, STATE, GSTATE)
  if UI_HOOK and UI_HOOK.HookModule.poll then UI_HOOK.HookModule.poll() end
end
function SIMSTEP()
  if GAME.held then HELD_TICKS = (HELD_TICKS or 0) + 1; return end   -- a loaded game waiting for app.startReadyGame
  local cbs = SAVE_CBS
  SAVE_CBS = {}
  for _, cb in ipairs(cbs) do cb() end
  local q = GAME.guiQueue
  GAME.guiQueue = {}
  for _, c in ipairs(q) do EXECUTE(c[1], c[2]) end
  local steps = GAME.speed
  if steps == 0 and (GAME.coast or 0) > 0 then steps, GAME.coast = 1, GAME.coast - 1 end   -- speed 0 not in effect yet
  for _ = 1, steps do
    for _, e in ipairs(GAME.deferred) do WORLD[e] = { TRANSPORT_VEHICLE = {} } end
    GAME.deferred = {}
    GAME.time = GAME.time + 200
    if STEP_HOOK then STEP_HOOK(GAME.time) end   -- a test's own world change at a step (as the engine's town growth), before the script
    -- one simulation step as the wiki describes it (modding:scripting:gamescripts): every game script's update, which "may not write
    -- the state itself" and whose result "is forwarded to the next script", then postUpdate, "executed after all the update scripts are
    -- done", at the same game time. Commands sent in either run at once (the simulation state) and may not carry a callback.
    GAME.inUpdate = true
    GAME.phase = "update"
    local applied0 = #GAME.applied
    local res = SIM.update(nil, STATE, 0.2)
    if #GAME.applied ~= applied0 then UPDATE_WRITES[#UPDATE_WRITES + 1] = "engine command applied in update" end
    if OTHER_UPDATE then OTHER_UPDATE(GAME.time) end   -- another game script's update in the same step (a test's own)
    -- POST_MODE (a test's): "never" calls no postUpdate, "alternate" every other step (the bridge's fallback applies the step in update)
    GAME.stepN = (GAME.stepN or 0) + 1
    if not (POST_MODE == "never" or (POST_MODE == "alternate" and GAME.stepN % 2 == 0)) then
      GAME.phase = "postUpdate"
      SIM.postUpdate(nil, STATE, 0.2, res)
    end
    GAME.phase = nil
    GAME.inUpdate = false
  end
end
function APPLIED()
  local r = {}
  for i, a in ipairs(GAME.applied) do r[i] = a.name .. "@" .. a.time end
  return table.concat(r, ",")
end
'''


class Game:
    def __init__(self, label, player_id, tmp, first_entity):
        self.label, self.id = label, player_id
        self.dir = os.path.join(tmp, label)
        os.makedirs(self.dir)
        self.to_game = os.path.join(self.dir, "to_game.log")
        self.from_game = os.path.join(self.dir, "from_game.log")
        self.use_env()
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        g = self.L.globals()
        g.COMMON_SRC, g.LOCKSTEP_SRC, g.BRIDGE_SRC, g.HUD_SRC = COMMON_SRC, LOCKSTEP_SRC, BRIDGE_SRC, HUD_SRC
        g.HOOK_SRC = HOOK_SRC
        g.TRACE_SRC = TRACE_SRC
        g.PORTABLE_SRC = PORTABLE_SRC
        g.LVL_SRC = LVL_SRC
        g.LINKSTATS_SRC = LINKSTATS_SRC
        g.LINK_SRC, g.LOG_SRC = LINK_SRC, LOG_SRC
        cmds = self.L.table()
        for stem, src in CMDS_SRC.items():
            cmds[stem] = src
        g.CMDS_SRC = cmds
        self.L.execute(MOCK % dict(t0=T0, first=first_entity))
        self.L.execute(HARNESS)

    def use_env(self):
        """The mod reads its environment when its files load: make it this game's."""
        os.environ.update(FEVERLINK3_SESSION_DIR=self.dir, FEVERLINK3_PLAYER_ID=str(self.id), FEVERLINK3_PLAYER_NAME=self.label,
                          FEVERLINK3_TEST_STOP_SECONDS="0")

    def reload(self):
        self.use_env()
        self.L.execute("RELOAD()")

    def reload_pending(self):
        return bool(self.L.eval("RELOAD_PENDING"))

    def ui_send(self, text):
        self.use_env()
        return bool(self.L.eval("UI_SEND(%s)" % lua_str(text)))

    def ui_speed(self, n):
        self.use_env()
        return bool(self.L.eval("UI_SPEED(%d)" % n))

    def window(self, code):
        """Runs `code` as a game window would: in the window state, whose api the UI hook wrapped (not the bridge's)."""
        self.use_env()
        self.L.execute("if not UI_HOOK then UI_HOOK_NEW() end")
        self.L.execute("local api = UI_HOOK.api; " + code)

    def events_seen(self, event_id="Loan"):
        """How many events of that id the engine applied here (the bridge's own plumbing events do not count)."""
        return int(self.L.eval("(function() local n = 0; for _, e in ipairs(EVENTS_SEEN) do if e[1] == %s then n = n + 1 end end; return n end)()" % lua_str(event_id)))

    def ui_view(self):
        self.use_env()
        return self.L.eval("UI_VIEW()")

    def frame(self):
        self.L.execute("FRAME()")

    def step(self):
        self.L.execute("SIMSTEP()")

    def time(self):
        return int(self.L.eval("GAME.time"))

    def speed(self):
        return int(self.L.eval("GAME.speed"))

    def applied(self):
        s = self.L.eval("APPLIED()")
        return s.split(",") if s else []

    def send_gui(self, lua_call):
        """Issue a command from the interface state, as a game window would."""
        self.L.execute("local api = GUI_API; " + lua_call)

    def log(self):
        return read_text(os.path.join(self.dir, "mod.log"))

    def lines(self, path):
        return read_text(path).splitlines()


class Relay:
    """Stand-in for FeverLink3.exe: owns the session, relays actions with the origin filled in, broadcasts
    session state and peer clocks, collects checksum reports. Uses the mod's own common module to read and
    write link lines."""

    def __init__(self, games):
        self.games = games
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.C = load_common(self.L)
        self.readers = {g.id: self.C.newReader(g.from_game) for g in games}
        self.seq = {g.id: 0 for g in games}
        self.history = {g.id: [] for g in games}
        self.clocks, self.aheads, self.hello, self.reports, self.speed_requests = {}, {}, {}, {}, []
        self.hellos, self.authority, self.saves_done, self.replay_failed, self.refused, self.chats = [], {}, [], [], [], []
        self.vetoes = []
        # every game's verdict of an action at its stamp, as the host keeps them (core VerdictBook): (owner, seq) -> {game: (applied, detail)}
        self.verdicts, self.verdict_resyncs = {}, []
        self.not_ready = {}
        self.commits = []
        self.pause_seq = 0   # the host's pause order
        self.stops, self.pause_held, self.pause_last, self.pause_of = {}, [], None, None
        self.safes, self.drop_promises = {}, False
        self.started, self.speed, self.pauseAt = False, 0, None

    # launcher -> mod
    def send(self, g, kind, frm, payload):
        self.seq[g.id] += 1
        line = self.C.formatLine(kind, frm, payload)
        self.history[g.id].append(line)
        with open(g.to_game, "a", encoding="utf-8", newline="") as f:
            f.write(line)

    def lua(self, v):
        if isinstance(v, dict):
            return self.L.table_from({k: self.lua(x) for k, x in v.items()})
        if isinstance(v, list):
            return self.L.table_from([self.lua(x) for x in v])
        return v

    def ser(self, d):
        return self.C.serialize(self.lua(d))

    def broadcast(self, kind, payload, exclude=None):
        for g in self.games:
            if g is not exclude:
                self.send(g, kind, 0, payload)

    @property
    def lead(self):
        return 200 * (4 + 2 * max(self.speed, 0))

    def session(self):
        d = {"started": self.started, "speed": self.speed, "lead": self.lead}
        if self.pauseAt is not None:
            d["pauseAt"] = self.pauseAt
        if self.pauseAt != self.pause_of:
            # a new pause starts its own order; the resume keeps the last stamp of the pause (host.rs: the actions still held then are
            # stamped past it)
            if self.pauseAt is not None:
                self.pause_last = None
            self.pause_of = self.pauseAt
        self.broadcast("session", self.ser(d))
        self.flush_pause()

    def flush_pause(self):
        """host.rs flush_pause: the actions of the pause are stamped one step past the furthest place a game stands still (once every
        game said where, at or past the last such stamp) and sent to every game, the sender included, numbered; after the resume, a
        normal stamp."""
        if not self.pause_held:
            return
        if self.pauseAt is None:
            stamp = self.future_stamp()
            if self.pause_last is not None:
                stamp = max(stamp, self.pause_last + 2 * STEP)   # host.rs: every game may stand at the pause's last stamp plus a step
        else:
            floor = max(self.pauseAt, self.pause_last or self.pauseAt)
            stops = [self.stops.get(g.id) for g in self.games]
            if any(st is None or st < floor for st in stops):
                return
            stamp = -(-max(stops) // STEP) * STEP + STEP
        held, self.pause_held = self.pause_held, []
        upto = self.pause_seq + len(held)   # host.rs: every item of a flush names the number of its last one (pause_upto)
        for kind, g, p in held:
            self.pause_seq += 1
            p["pauseUpto"] = upto
            p["stamp"] = stamp if self.pauseAt is not None else max(stamp, int(p.stamp))
            p["pauseSeq"] = self.pause_seq
            if kind == "build_commit":
                self.commits.append((g.id, int(p.owner), int(p.seq), int(p.stamp)))
            payload = self.C.serialize(p)
            for o in self.games:
                self.send(o, kind, g.id if kind == "action" else 0, payload)
            if self.pauseAt is not None:
                self.pause_last = stamp

    def future_stamp(self):
        t = max(self.clocks.values())
        # as host.rs: never before a promise a game has made, so no game has passed the stamp (the input barrier)
        return max([-(-(t + self.lead) // STEP) * STEP] + list(self.safes.values()))

    def checksum_request(self, n, at, paused, full=None, spread=None):
        body = {"n": n, "at": at, "paused": paused}
        if full is not None:
            body["full"] = full   # absent means a full round
        if spread is not None:
            body["spread"] = spread   # absent: the round is hashed whole at its stamp
        self.broadcast("checksum_request", self.ser(body))

    def send_authority(self, n):
        """The host's authority values of round n, to every other game (what the core does after a round)."""
        payload = self.ser({"n": n, "values": self.authority[n]})
        for g in self.games:
            if g.id != 0:
                self.send(g, "authority", 0, payload)

    def rewrite_history(self, g, drop=1):
        """Truncate the game's to_game.log and write its history again without the first `drop` lines (a
        launcher compacting the file): the file gets shorter, the remaining lines keep their sequence numbers."""
        with open(g.to_game, "w", encoding="utf-8", newline="") as f:
            f.write("".join(self.history[g.id][drop:]))

    def verdict(self, g, p, applied):
        """core VerdictBook / host on_action_verdict: verdicts of one action that differ between games ask for a resync at once;
        verdicts that agree (all refused alike, all applied) ask for nothing."""
        key = (int(p["from"]) if p["from"] is not None else 0, int(p.seq))
        by = self.verdicts.setdefault(key, {})
        by[g.id] = (applied, str(p.detail or ""))
        n_applied = sum(1 for a, _ in by.values() if a)
        if 0 < n_applied < len(by):
            self.verdict_resyncs.append("verdicts of %d:%d differ" % key)
            del self.verdicts[key]
        elif len(by) == len(self.games):
            del self.verdicts[key]

    # mod -> launcher
    def pump(self):
        for g in self.games:
            for rec in self.C.readerPoll(self.readers[g.id]).values():
                p = self.C.parse(rec.payload)
                if isinstance(p, tuple):
                    p = p[0]
                if rec.kind == "clock":
                    self.clocks[g.id], self.aheads[g.id] = int(p.t), int(p.ahead)
                    # where the game stands still at a pause (host.rs: a game without a pause promise counts where its speed is 0)
                    if p.pause:
                        self.stops[g.id] = int(p.stop) if p.stop is not None and int(p.stop) == int(p.t) else None
                    else:
                        self.stops[g.id] = int(p.t) if p.speed is not None and int(p.speed) == 0 else None
                    pc = {"from": g.id, "t": int(p.t), "ahead": int(p.ahead)}
                    if p.need is not None:
                        pc["need"] = max(0, min(24, int(p.need)))   # the host relays the measured margin clamped (host.rs)
                    if p.safe is not None and not self.drop_promises:
                        # the input barrier's promise, relayed as it came (host.rs drops an implausible one)
                        pc["safe"] = int(p.safe)
                        self.safes[g.id] = int(p.safe)
                        if p.pause:
                            pc["pause"] = True
                    self.broadcast("peerclock", self.ser(pc), exclude=g)
                elif rec.kind == "action":
                    p["from"] = g.id
                    p["pauseSeq"] = None
                    if p.pause:
                        self.pause_held.append(("action", g, p))   # the pause order: stamped by the host (flush_pause)
                    else:
                        payload = self.C.serialize(p)
                        for o in self.games:
                            if o is not g:
                                self.send(o, "action", g.id, payload)
                elif rec.kind == "hello":
                    self.hello[g.id] = int(p.time)
                    self.hellos.append(g.id)
                elif rec.kind == "save_done":
                    self.saves_done.append((g.id, int(p.id), bool(p.ok), p.detail))
                elif rec.kind == "replay_failed":
                    self.replay_failed.append((g.id, str(p.detail)))
                elif rec.kind == "action_refused":
                    self.refused.append((g.id, int(p.seq), str(p.detail)))
                    self.verdict(g, p, False)
                elif rec.kind == "action_applied":
                    self.verdict(g, p, True)
                elif rec.kind == "chat":
                    self.chats.append((g.id, str(p.text)))
                    # the host fills in the sender and relays to everyone but the origin, as a line from 0
                    out = self.ser({"text": str(p.text), "from": g.id, "name": g.label})
                    for o in self.games:
                        if o is not g:
                            self.send(o, "chat", 0, out)
                elif rec.kind == "build_veto":
                    # the host fills in the sender and relays to everyone but the origin (the payload carries the sender)
                    self.vetoes.append((g.id, int(p.owner), int(p.seq), str(p.why)))
                    out = self.ser({"seq": int(p.seq), "owner": int(p.owner), "why": str(p.why), "from": g.id, "name": g.label})
                    for o in self.games:
                        if o is not g:
                            self.send(o, "build_veto", 0, out)
                elif rec.kind == "build_commit":
                    c = {"seq": int(p.seq), "owner": int(p.owner), "stamp": int(p.stamp), "from": g.id}
                    if p.pause:
                        c["pause"] = True
                        self.pause_held.append(("build_commit", g, self.lua(c)))
                    else:
                        self.commits.append((g.id, int(p.owner), int(p.seq), int(p.stamp)))
                        out = self.ser(c)
                        for o in self.games:
                            if o is not g:
                                self.send(o, "build_commit", 0, out)
                elif rec.kind == "checksum_report":
                    self.reports.setdefault(int(p.n), {})[g.id] = (int(p.time), {k: v for k, v in p.parts.items()})
                    # the parts the game could not hash this round (core ChecksumReportMessage.not_ready)
                    self.not_ready.setdefault(int(p.n), {})[g.id] = sorted(p.notReady.values()) if p.notReady is not None else []
                    if g.id == 0 and p.authority is not None:
                        self.authority[int(p.n)] = {k: v for k, v in p.authority.items()}
                elif rec.kind == "speed_request":
                    self.speed_requests.append(int(p.speed))
            self.flush_pause()


class LockstepTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-")
        self.A = Game("host", 0, self.tmp, 50000)
        self.B = Game("client", 1, self.tmp, 51000)
        self.relay = Relay([self.A, self.B])

    def tearDown(self):
        if os.environ.get("FEVERLINK3_TEST_VERBOSE"):
            print("\n--- host mod.log ---\n" + self.A.log() + "\n--- client mod.log ---\n" + self.B.log())
        shutil.rmtree(self.tmp, ignore_errors=True)

    def tick(self, n=1, lag_b=False):
        """One tick: a few interface frames with relay pumps, then one engine tick per game (the client skips
        every third tick when lagging)."""
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

    def log_has(self, text):
        return text in self.A.log() or text in self.B.log()

    # ---- scenarios

    def test_hold_at_save_time_before_start(self):
        self.tick(15)
        self.assertEqual((self.A.time(), self.B.time()), (T0, T0), "both games hold at the save time")
        self.assertEqual((self.A.speed(), self.B.speed()), (0, 0))
        self.assertEqual(self.relay.hello, {0: T0, 1: T0}, "both games said hello with the save time")

    def test_barrier_keeps_games_within_ahead(self):
        self.start()
        self.tick(30, lag_b=True)
        self.assertGreater(self.A.time(), T0)
        self.assertGreater(self.B.time(), T0)
        gaps = []
        for _ in range(30):
            self.tick(1, lag_b=True)
            gaps.append((self.A.time() - self.B.time()) // STEP)
        ahead = self.relay.aheads[0]
        self.assertTrue(all(-ahead <= g <= ahead for g in gaps), "gaps in steps %s exceed ahead=%d" % (gaps, ahead))
        self.assertGreater(max(gaps), 0, "the client did lag behind at some point: %s" % gaps)

    def test_client_action_applied_on_both_at_the_same_stamp(self):
        self.start()
        self.tick(10, lag_b=True)
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function(d, s, e) BUY_CB = { s, d and d.resultVehicleEntity } end)')
        self.assertTrue(self.B.L.eval("BUY_CB == nil"), "the depot window waits for the real result")
        self.tick(40, lag_b=True)
        a, b = self.A.applied(), self.B.applied()
        self.assertEqual(len(a), 1, a)
        self.assertEqual(a, b, "same action at the same stamp on both games")
        stamp = int(a[0].split("@")[1])
        self.assertEqual(stamp % STEP, 0)
        va, vb = self.A.L.eval("GAME.applied[1].entity"), self.B.L.eval("GAME.applied[1].entity")
        self.assertNotEqual(va, vb, "the vehicle has different ids in the two games")
        self.assertTrue(self.B.L.eval("BUY_CB ~= nil and BUY_CB[1] == true and BUY_CB[2] == %d" % vb), "the client's callback got its local vehicle id")
        shipped = [l for l in self.B.lines(self.B.from_game) if l.split(TAB)[0] == "action"]
        self.assertEqual(len(shipped), 1)
        self.assertIn('s:55', shipped[0], "the depot travels as a savegame key")
        self.assertNotIn(str(vb), shipped[0])

        # a line, then the vehicle assigned to it: both references are translated on the host
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeLineCreateCmd("Line 1", { x = 1, y = 0, z = 0 }, 7, { stops = {} }), function(d, s, e) LINE_CB = { s, e and e[1] and e[1][1] } end)')
        self.tick(40, lag_b=True)
        lb = self.B.L.eval("LINE_CB and LINE_CB[2]")
        la = self.A.L.eval("GAME.applied[2].entity")
        self.assertTrue(lb and la and lb != la)
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleSetLineCmd(%d, %d, 0), function(d, s) SET_CB = s end)" % (vb, lb))
        self.tick(40, lag_b=True)
        sa = self.A.L.eval("GAME.applied[3].args[1] .. ',' .. GAME.applied[3].args[2]")
        sb = self.B.L.eval("GAME.applied[3].args[1] .. ',' .. GAME.applied[3].args[2]")
        self.assertEqual(sa, "%d,%d" % (va, la), "host used its own vehicle and line ids")
        self.assertEqual(sb, "%d,%d" % (vb, lb), "client used its own vehicle and line ids")
        self.assertEqual(self.A.applied(), self.B.applied())
        self.assertTrue(self.B.L.eval("SET_CB == true"))
        self.assertFalse(self.log_has("LATE"), "no late action")

    def update_writes(self, g):
        return list(g.L.eval("table.concat(UPDATE_WRITES, ';')").split(";")) if int(g.L.eval("#UPDATE_WRITES")) else []

    def test_update_itself_writes_no_state_and_postupdate_applies_at_the_stamp(self):
        # The wiki (modding:scripting:gamescripts): update "may not write the state itself"; postUpdate "is executed after all the update
        # scripts are done". The mock records every state write, subscription and engine command made in the update phase.
        self.start()
        self.tick(10, lag_b=True)
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function(d, s, e) BUY_CB = s end)')
        self.tick(40, lag_b=True)
        a, b = self.A.applied(), self.B.applied()
        self.assertEqual(len(a), 1, a)
        self.assertEqual(a, b, "same action at the same step on both games")
        shipped = [l for l in self.B.lines(self.B.from_game) if l.split(TAB)[0] == "action"]
        self.assertEqual(len(shipped), 1)
        p = self.relay.C.parse(shipped[0].split(TAB)[-1])
        if isinstance(p, tuple):
            p = p[0]
        self.assertEqual(int(a[0].split("@")[1]), int(p.stamp), "applied in postUpdate at exactly its stamp")
        self.assertTrue(self.B.L.eval("BUY_CB == true"))
        for g in (self.A, self.B):
            self.assertEqual(self.update_writes(g), [], "%s: nothing written or sent in update()" % g.label)
            for bad in ("postUpdate was not called", "postUpdate missing", "update fallback failed", "postUpdate received no result", "postUpdate runs at t=", "postUpdate failed"):
                self.assertNotIn(bad, g.log())
        # the detector itself: the step's work run inside the update phase (the fallback when no postUpdate comes) is caught
        self.B.L.execute('POST_MODE = "never"')
        self.B.step()
        self.B.step()
        self.assertIn("state:set", self.update_writes(self.B))

    def buy_and_line(self):
        """A vehicle bought, a line made and the vehicle set on it, from the client's window; returns what each game applied."""
        self.start()
        self.tick(10)
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function(d, s, e) BUY_CB = { s, d and d.resultVehicleEntity } end)')
        self.tick(40)
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeLineCreateCmd("Line 1", { x = 1, y = 0, z = 0 }, 7, { stops = {} }), function(d, s, e) LINE_CB = { s, e and e[1] and e[1][1] } end)')
        self.tick(40)
        vb, lb = self.B.L.eval("BUY_CB and BUY_CB[2]"), self.B.L.eval("LINE_CB and LINE_CB[2]")
        self.assertTrue(vb and lb, "the client's callbacks got their entities")
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleSetLineCmd(%d, %d, 0), function(d, s) SET_CB = s end)" % (vb, lb))
        self.tick(40)
        stamps = []
        for l in self.B.lines(self.B.from_game):
            if l.split(TAB)[0] == "action":
                p = self.relay.C.parse(l.split(TAB)[-1])
                stamps.append(int((p[0] if isinstance(p, tuple) else p).stamp))
        return self.A.applied(), self.B.applied(), stamps

    def test_a_game_whose_engine_never_calls_postupdate_still_applies_every_action_at_its_stamp(self):
        for g in (self.A, self.B):
            g.L.execute('POST_MODE = "never"')
        a, b, stamps = self.buy_and_line()
        self.assertEqual(len(a), 3, a)
        self.assertEqual(a, b, "same actions, same steps, same order on both games")
        self.assertEqual([int(x.split("@")[1]) for x in a], stamps, "each applied at exactly its stamp")
        self.assertTrue(self.B.L.eval("SET_CB == true"))
        for g in (self.A, self.B):
            self.assertIn("postUpdate was not called after the update at t=", g.log())
            self.assertIn("step(s) of this session applied in update instead", g.log())
            self.assertNotIn("update fallback failed", g.log())

    def test_a_postupdate_on_every_other_step_never_applies_a_step_twice(self):
        for g in (self.A, self.B):
            g.L.execute('POST_MODE = "alternate"')
        a, b, stamps = self.buy_and_line()
        self.assertEqual(len(a), 3, "each action applied once: %s" % a)
        self.assertEqual(a, b, "same actions, same steps, same order on both games")
        self.assertEqual([x.split("@")[0] for x in a], ["makeVehicleBuyCmd", "makeLineCreateCmd", "makeVehicleSetLineCmd"])
        for got, stamp in zip([int(x.split("@")[1]) for x in a], stamps):
            # a step caught up in the next update lands one engine step late in world time; never earlier, never twice
            self.assertIn(got - stamp, (0, STEP))
        self.assertTrue(self.B.L.eval("SET_CB == true"))
        for g in (self.A, self.B):
            self.assertNotIn("update fallback failed", g.log())

    def test_unresolvable_reference_is_refused_not_applied(self):
        self.start()
        self.tick(10)
        self.B.L.execute("WORLD[9999] = { VEHICLE_DEPOT = {} }")   # a depot that exists in the client game only
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 9999, { vehicles = {} }), function(d, s) BAD_CB = s end)')
        self.tick(40)
        self.assertEqual(len(self.A.applied()), 0, "the host did not buy in a depot it does not have")
        self.assertEqual(len(self.B.applied()), 1)
        self.assertIn("refused", self.A.log())
        self.assertTrue(any(l.split(TAB)[0] == "action_refused" for l in self.A.lines(self.A.from_game)))

    def test_pause_stamp_stops_both_exactly_and_action_during_pause(self):
        self.start(speed=2)
        self.tick(10, lag_b=True)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(40, lag_b=True)
        self.assertEqual((self.A.time(), self.B.time()), (self.relay.pauseAt, self.relay.pauseAt), "both stopped at the pause stamp")
        self.assertEqual((self.A.speed(), self.B.speed()), (0, 0))
        self.A.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function(d, s, e) BUY_CB = { s, d and d.resultVehicleEntity } end)')
        self.tick(8)
        # the host stamped it one step past where the games stand still; both ran on that one step and applied it there
        expected = "makeVehicleBuyCmd@%d" % (self.relay.pauseAt + STEP)
        self.assertEqual(self.A.applied(), [expected])
        self.assertEqual(self.B.applied(), [expected], "the same step on the client too")
        va = self.A.L.eval("GAME.applied[1].entity")
        self.assertTrue(self.A.L.eval("BUY_CB ~= nil and BUY_CB[1] == true and BUY_CB[2] == %d" % va))
        # (and one step more, where the simulation finds the vehicle and the window's callback gets it)
        self.assertEqual((self.A.time(), self.B.time()), (self.relay.pauseAt + 2 * STEP, self.relay.pauseAt + 2 * STEP), "still paused, two steps on")
        self.assertEqual((self.A.speed(), self.B.speed()), (0, 0))
        # resume at x2
        paused_at = self.relay.pauseAt
        self.relay.pauseAt, self.relay.speed = None, 2
        self.relay.session()
        self.tick(20)
        self.assertGreater(self.A.time(), paused_at)
        self.assertEqual(self.A.speed(), 2)
        # the vehicle bought during the pause is known to the simulation half too: assign it to a new line
        self.A.send_gui('api.cmd.sendCommand(api.cmd.makeLineCreateCmd("L", { x = 0, y = 0, z = 0 }, 7, { stops = {} }), function(d, s, e) LINE_CB = e[1][1] end)')
        self.tick(30)
        la = self.A.L.eval("LINE_CB")
        self.A.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleSetLineCmd(%d, %d, 0), function() end)" % (va, la))
        self.tick(30)
        self.assertEqual(self.A.applied(), self.B.applied())
        self.assertEqual(self.B.L.eval("GAME.applied[3].args[1]"), self.B.L.eval("GAME.applied[1].entity"))

    def test_the_checksum_cost_is_logged_only_above_the_threshold_and_changes_nothing(self):
        self.start()
        self.tick(10, lag_b=True)
        self.relay.pump()
        self.relay.checksum_request(1, self.relay.future_stamp(), False)
        self.tick(40, lag_b=True)
        self.assertNotIn("checksum slow", self.A.log() + self.B.log(), "a fast checksum says nothing")
        # a clock that advances 30 ms per reading makes every checksum cost more than 50 ms on game A
        self.A.L.execute("local t = 0; os.clock = function() t = t + 0.03; return t end")
        self.relay.checksum_request(2, self.relay.future_stamp(), False)
        self.tick(40, lag_b=True)
        self.assertIn("checksum slow: total ", self.A.log())
        self.assertRegex(self.A.log(), r"checksum slow: total \d+ ms: \w+ \d+")
        self.assertNotIn("checksum slow", self.B.log())
        self.assertEqual(self.relay.reports[2][0][1], self.relay.reports[2][1][1], "timing never enters the parts")
        # a clock that raises is harmless
        self.A.L.execute("os.clock = function() error('no clock') end")
        self.relay.checksum_request(3, self.relay.future_stamp(), False)
        self.tick(40, lag_b=True)
        self.assertIn(0, self.relay.reports.get(3, {}))

    def test_a_light_round_leaves_out_the_heavy_lanes_and_a_full_round_has_everything(self):
        self.start()
        self.tick(10, lag_b=True)
        self.relay.pump()
        heavy = {"constructions", "edges", "nodeConfigs", "townBuildings"}
        self.relay.checksum_request(1, self.relay.future_stamp(), False)             # no flag: full
        self.tick(40, lag_b=True)
        self.relay.checksum_request(2, self.relay.future_stamp(), False, full=True)
        self.tick(40, lag_b=True)
        self.relay.checksum_request(3, self.relay.future_stamp(), False, full=False)
        self.tick(40, lag_b=True)
        for n in (1, 2):
            names = set(self.relay.reports[n][0][1])
            self.assertTrue(heavy <= names and any(k.startswith("townBuildings:") for k in names), (n, names))
        for g in (0, 1):
            names = set(self.relay.reports[3][g][1])
            self.assertFalse(names & heavy or any(k.startswith("townBuildings:") for k in names), names)
            self.assertTrue({"time", "balance", "loan", "lines", "stations", "names", "vehicles"} <= names, names)
        self.assertEqual(self.relay.reports[3][0][1], self.relay.reports[3][1][1])

    def test_checksum_rounds_flow_with_a_balance_above_2_pow_31(self):
        # Real sweep 2026-10-13: a base save with a balance of 5,035,445,200. The game's Lua raises on "%d" beyond 32 bits (emulated in
        # the harness); no checksum round after the paused one reached the launcher, and nothing in mod.log said why.
        for g in (self.A, self.B):
            g.L.execute("ACCOUNT.balance = 5035445200")
        self.start()
        self.tick(10, lag_b=True)
        for n in (1, 2, 3):
            self.relay.checksum_request(n, self.relay.future_stamp(), False, full=(n == 1))
            self.tick(30, lag_b=True)
            self.assertEqual(sorted(self.relay.reports.get(n, {})), [0, 1], "round %d reached the launcher from both games; host log: %s" % (n, self.A.log()))
        self.assertEqual(self.relay.authority[3]["balance"], 5035445200)
        self.assertNotIn("failed", self.A.log() + self.B.log())

    def test_a_state_saved_mid_session_does_not_mask_the_new_sessions_rounds(self):
        # Sweep 2026-10-13: the base save carried the earlier session's 40 stored reports (rounds 1..40, old accounts); the new session's
        # interface half read rounds 2..40 from them as its own, marked them sent and never sent the real ones.
        for g in (self.A, self.B):
            g.L.execute("""
              local reports, results, refused = {}, {}, {}
              for n = 1, 40 do reports[n] = { n = n, time = 1000 + n, parts = { time = "old" .. n, balance = "old" }, account = { balance = 38000000, loan = 0 } } end
              results[1] = { key = "1:1", at = 5, success = true }
              refused[1] = { key = "1:2", from = 1, seq = 2, detail = "stale refusal" }
              STATE:set({ reports = reports, results = results, refused = refused, late = 7, queue = {}, bind = {}, watches = {}, binders = {} })
            """)
        self.start()
        self.tick(10, lag_b=True)
        for n in (1, 2, 3):
            self.relay.checksum_request(n, self.relay.future_stamp(), False, full=(n == 1))
            self.tick(30, lag_b=True)
            self.assertEqual(sorted(self.relay.reports.get(n, {})), [0, 1], "round %d" % n)
            for gid in (0, 1):
                self.assertNotIn("old", str(self.relay.reports[n][gid][1]["time"]), "a stale part reached the launcher")
            self.assertEqual(self.relay.reports[n][0][1], self.relay.reports[n][1][1])
        self.assertNotIn("stale refusal", "".join(self.A.lines(self.A.from_game)) + "".join(self.B.lines(self.B.from_game)))
        self.assertIn("ignoring", self.A.log() + self.B.log(), "the stale outputs are logged once")
        self.assertNotIn("applied an action late", self.A.log())

    def test_an_unreadable_state_and_a_lost_round_are_logged(self):
        self.A.L.execute("STATE.get = function(self) error('cannot convert') end")
        self.tick(5)
        self.assertIn("the game-script state cannot be read: ", self.A.log())
        self.assertIn("cannot convert", self.A.log())

    def test_checksum_rounds(self):
        self.start()
        self.tick(10, lag_b=True)
        self.relay.pump()
        at = self.relay.future_stamp()
        self.relay.checksum_request(1, at, False)
        self.tick(40, lag_b=True)
        self.assertIn(0, self.relay.reports.get(1, {}))
        self.assertIn(1, self.relay.reports.get(1, {}))
        (ta, pa), (tb, pb) = self.relay.reports[1][0], self.relay.reports[1][1]
        self.assertEqual((ta, tb), (at, at), "both hashed at the requested stamp")
        self.assertEqual(pa, pb, "identical worlds give identical parts")
        self.assertEqual(set(pa) - {"stations", "edges", "nodeConfigs", "vehicles", "names", "ids"}, {"time", "balance", "loan", "constructions", "lines", "townBuildings", "townBuildings:90", "townBuildings:91"})
        self.assertTrue(pa["townBuildings"].startswith("3/") and pa["townBuildings:90"].startswith("2/") and pa["townBuildings:91"].startswith("1/"), pa)
        # paused round: the games are stopped, the interface half answers
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(40, lag_b=True)
        self.assertEqual(self.A.time(), self.relay.pauseAt)
        self.relay.checksum_request(2, self.relay.pauseAt, True)
        self.tick(5)
        self.assertEqual(self.relay.reports[2][0], self.relay.reports[2][1])
        self.assertEqual(self.relay.reports[2][0][0], self.relay.pauseAt)
        # one account differs: only the balance part differs
        self.B.L.execute("ACCOUNT.balance = ACCOUNT.balance + 1000")
        self.relay.checksum_request(3, self.relay.pauseAt, True)
        self.tick(5)
        pa, pb = self.relay.reports[3][0][1], self.relay.reports[3][1][1]
        self.assertEqual({k for k in pa if pa[k] != pb[k]}, {"balance"})
        self.assertEqual(pa["loan"], pb["loan"])
        # one building of town 91 grown on one game only: the aggregate and that town's part differ, nothing else
        self.B.L.execute("WORLD[65].TOWN_BUILDING.level = 2")
        self.relay.checksum_request(4, self.relay.pauseAt, True)
        self.tick(5)
        pa, pb = self.relay.reports[4][0][1], self.relay.reports[4][1][1]
        self.assertEqual({k for k in pa if pa[k] != pb[k]}, {"balance", "townBuildings", "townBuildings:91"})
        self.assertEqual(pa["townBuildings:90"], pb["townBuildings:90"])
        # a building in town 90 on one game only: the counts in the part values name the town
        self.B.L.execute("WORLD[66] = { TOWN_BUILDING = { town = 90, construction = 62, level = 1, depth = 1, timeBuilt = 400, parcels = {} } }")
        self.relay.checksum_request(5, self.relay.pauseAt, True)
        self.tick(5)
        pa, pb = self.relay.reports[5][0][1], self.relay.reports[5][1][1]
        self.assertTrue(pa["townBuildings:90"].startswith("2/") and pb["townBuildings:90"].startswith("3/"), (pa, pb))
        self.assertTrue(pa["townBuildings"].startswith("3/") and pb["townBuildings"].startswith("4/"))

    def test_truncated_to_game_log_does_not_reapply(self):
        self.start()
        self.tick(10)
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function() end)')
        self.tick(40)
        self.assertEqual(len(self.A.applied()), 1)
        before = self.A.applied()
        self.relay.rewrite_history(self.A)
        self.assertTrue(any(l.split(TAB)[0] == "action" for l in self.A.lines(self.A.to_game)), "the old action line is still in the file")
        self.tick(20)
        self.assertEqual(self.A.applied(), before, "re-written history was not dispatched again")
        # new lines after the truncation still arrive
        self.relay.speed = 2
        self.relay.session()
        self.tick(10)
        self.assertEqual(self.A.speed(), 2)

    def test_late_action_is_applied_and_logged(self):
        self.start()
        self.tick(10)
        body = self.relay.C.serialize(self.relay.C.parse(
            '{["fn"]="makeVehicleBuyCmd",["args"]={["n"]=3,[1]={["__ref"]="player"},[2]={["__ref"]="entity",["key"]="s:55"},[3]={}}}'))
        stamp = self.A.time() - 2 * STEP
        payload = self.relay.ser({"stamp": stamp, "seq": 7, "body": body, "from": 1})
        self.relay.send(self.A, "action", 1, payload)
        self.tick(5)
        self.assertEqual(len(self.A.applied()), 1)
        self.assertIn("LATE", self.A.log())
        # a late apply is a fork, never silent: the arrival is named and the host is asked for the repair
        self.assertIn("INPUT BARRIER BREACHED: action 1:7", self.A.log())
        self.relay.pump()
        self.assertTrue(any("late apply of 1:7" in d for g, d in self.relay.replay_failed if g == 0), self.relay.replay_failed)

    def test_a_command_from_a_window_state_reaches_the_other_game_once(self):
        # The windows live in a Lua state of their own (seen in a real game: a loan taken in the window never reached the
        # bridge's wrap). The UI hook records the command for the bridge; the game applies it natively as ever, and the
        # other game applies it at the stamp. The originator must not apply it a second time.
        self.start()
        self.tick(10)
        self.A.window('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Obtain", { 1, { amount = 5 } }))')
        self.tick(60)
        self.assertEqual(self.A.events_seen(), 1, "applied natively on the originator, once")
        self.assertEqual(self.B.events_seen(), 1, "applied on the other game at the stamp")
        self.assertIn("window command", self.A.log())

    def test_a_command_from_a_window_state_of_the_guest_reaches_the_host(self):
        self.start()
        self.tick(10)
        self.B.window('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Obtain", { 1, { amount = 5 } }))')
        self.tick(60)
        self.assertEqual(self.B.events_seen(), 1)
        self.assertEqual(self.A.events_seen(), 1)

    def test_a_window_command_leaves_every_checksum_part_equal_on_both_games(self):
        # the originating game applies the command natively, so it must also record it in its company ledger, with the stamp the
        # other game uses, or the games "differ in companies" after every loan, rename or purchase (seen in a real game)
        self.start()
        self.tick(10)
        self.A.window('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Obtain", { 1, { amount = 5 } }))')
        self.B.window('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Repay", { nil, { amount = 5 } }))')
        self.tick(60)
        self.relay.checksum_request(1, self.relay.future_stamp(), False)
        self.tick(40)
        pa, pb = self.relay.reports[1][0][1], self.relay.reports[1][1][1]
        self.assertEqual({k for k in pa if pa[k] != pb[k]}, set(), "no part differs: " + str({k: (pa[k], pb[k]) for k in pa if pa[k] != pb[k]}))
        self.assertIn("companies", pa)

    def test_a_speed_button_in_a_window_state_is_a_request_and_not_applied_there(self):
        self.start()
        self.tick(5)
        self.A.window("api.cmd.sendCommand(api.cmd.makeGameSetSpeedCmd(3))")
        self.tick(5)
        self.assertEqual(self.relay.speed_requests, [3])
        self.assertEqual(self.A.speed(), 1, "the session's speed, not the button's")

    def test_speed_button_becomes_a_request(self):
        self.start()
        self.A.send_gui("api.cmd.sendCommand(api.cmd.makeGameSetSpeedCmd(3))")
        self.tick(3)
        self.assertEqual(self.relay.speed_requests, [3])
        self.assertEqual(self.A.speed(), 1, "the local engine speed stays the session's")


    # ---- host authority

    def test_authority_corrects_balance_and_loan_at_the_checkpoint(self):
        self.start()
        self.tick(10, lag_b=True)
        self.relay.pump()
        self.B.L.execute("ACCOUNT.balance = ACCOUNT.balance + 5000; ACCOUNT.loan = ACCOUNT.loan - 1000")
        self.relay.checksum_request(1, self.relay.future_stamp(), False)
        self.tick(40, lag_b=True)
        pa, pb = self.relay.reports[1][0][1], self.relay.reports[1][1][1]
        self.assertEqual({k for k in pa if pa[k] != pb[k]}, {"balance", "loan"})
        self.assertEqual(self.relay.authority[1], {"balance": 1000000, "loan": 250000}, "the host's report carries its account")
        self.relay.send_authority(1)
        self.tick(6)
        self.assertEqual(self.B.L.eval("#JOURNAL"), 0, "one round alone may be an action still on its way (replay on peers)")
        # the same difference one round later is a drift: now it is corrected
        self.relay.checksum_request(2, self.relay.future_stamp(), False)
        self.tick(40, lag_b=True)
        # the world moves on before the correction arrives: it is measured against the round, not against now
        self.B.L.execute("ACCOUNT.balance = ACCOUNT.balance - 300")
        self.relay.send_authority(2)
        self.tick(6)
        self.assertEqual(self.B.L.eval("ACCOUNT.balance"), 1000000 - 300, "balance moved by the round's difference (-5000) only")
        self.assertEqual(self.B.L.eval("ACCOUNT.loan"), 250000)
        self.assertEqual(self.B.L.eval("JOURNAL[1]"), -5000, "booked as one journal entry for the delta")
        self.assertEqual(self.B.L.eval("#JOURNAL"), 1)
        self.assertEqual(self.A.L.eval("#JOURNAL"), 0, "the host's own game is never corrected")
        self.assertEqual(self.B.applied(), [], "a journal entry is not a replicated action")
        # the next round is identical
        self.B.L.execute("ACCOUNT.balance = ACCOUNT.balance + 300")
        self.relay.checksum_request(3, self.relay.future_stamp(), False)
        self.tick(40, lag_b=True)
        self.assertEqual(self.relay.reports[3][0][1], self.relay.reports[3][1][1], "worlds identical after the correction")
        self.assertIn("balance corrected by -5000", self.B.log())

    def test_authority_that_arrives_before_the_game_reaches_the_round_waits_for_it(self):
        # Seen in a real game: the host reaches a round and sends its values at once; a game a little behind had no account
        # of its own for that round yet, the values were thrown away every round, and money and loans never followed.
        self.start()
        self.tick(5)
        balance = int(self.B.L.eval("ACCOUNT.balance"))
        loan = int(self.B.L.eval("ACCOUNT.loan"))
        for n in (9, 10):   # the same difference in two rounds in a row: a drift, corrected
            self.relay.authority[n] = {"balance": balance + 700, "loan": loan}
            self.relay.send_authority(n)
            self.tick(5)
            self.assertEqual(self.B.L.eval("#JOURNAL"), 0, "nothing to measure against yet, and nothing thrown away")
            self.assertNotIn("skipped", self.B.log())
            self.relay.checksum_request(n, self.relay.future_stamp(), False)
            self.tick(40)
        self.assertEqual(self.B.L.eval("JOURNAL[1]"), 700, "applied as the difference once the game reached the second round")
        self.assertEqual(self.B.L.eval("#JOURNAL"), 1)
        self.assertIn("balance corrected by 700", self.B.log())

    def test_a_difference_that_is_gone_one_round_later_is_never_corrected(self):
        # An action replayed on the other game reaches it a stamp's lead after the game it came from: a checksum round in
        # between sees the effect on one side only, and the next round sees none. Correcting the first would count it twice.
        self.start()
        self.tick(5)
        balance = int(self.B.L.eval("ACCOUNT.balance"))
        loan = int(self.B.L.eval("ACCOUNT.loan"))
        self.relay.authority[11] = {"balance": balance + 17000, "loan": loan + 17000}
        self.relay.checksum_request(11, self.relay.future_stamp(), False)
        self.tick(40)
        self.relay.send_authority(11)
        self.tick(5)
        self.relay.authority[12] = {"balance": balance, "loan": loan}   # the action has reached this game: no difference
        self.relay.checksum_request(12, self.relay.future_stamp(), False)
        self.tick(40)
        self.relay.send_authority(12)
        self.tick(5)
        self.assertEqual(self.B.L.eval("#JOURNAL"), 0)
        self.assertNotIn("corrected by", self.B.log())

    # ---- resync

    def test_resync_save_then_save_done(self):
        self.start()
        self.tick(5)
        self.relay.send(self.A, "resync_save", 0, self.relay.ser({"id": 7, "name": "resync_7"}))
        self.tick(4)
        self.assertEqual(self.A.L.eval("SAVED[1]"), "resync_7")
        self.assertEqual(self.relay.saves_done, [(0, 7, True, None)])
        # a failing save reports ok = false with the reason
        self.A.L.execute("SAVE_FAIL = true")
        self.relay.send(self.A, "resync_save", 0, self.relay.ser({"id": 8, "name": "resync_8"}))
        self.tick(4)
        self.assertEqual(self.relay.saves_done[1][:3], (0, 8, False))
        self.assertIn("disk full", self.relay.saves_done[1][3])

    def test_resync_load_reloads_says_hello_and_dispatches_nothing_again(self):
        self.start()
        self.tick(10)
        self.A.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function() end)')
        self.tick(40)
        before = self.B.applied()
        self.assertEqual(len(before), 1, "the client applied the host's action once")
        self.assertEqual(self.relay.hellos.count(1), 1)
        self.relay.send(self.B, "resync_load", 0, self.relay.ser({"id": 3, "name": "hostsave", "sha256": "00"}))
        after_load_line = os.path.getsize(self.B.to_game)
        self.B.frame()
        self.assertEqual(self.B.L.eval("LOADED"), "hostsave")
        self.assertTrue(self.B.reload_pending())
        # the places right after the load line in to_game.log and where the window's records were read to (none yet)
        self.assertEqual(read_text(os.path.join(self.B.dir, "resync_cursor.txt")), "1:%d 1:0" % after_load_line)
        # a line queued behind the load in the same file is not dispatched by the old scripts
        self.relay.send(self.B, "session", 0, self.relay.ser({"started": True, "speed": 3, "lead": 2000}))
        self.B.frame()
        self.assertEqual(self.B.speed(), 1)
        self.B.reload()
        self.tick(10)
        self.assertEqual(self.relay.hellos.count(1), 2, "the reloaded mod reports hello again")
        self.assertEqual(self.B.applied(), before, "nothing the file already held was dispatched again")
        self.assertEqual(read_text(os.path.join(self.B.dir, "resync_cursor.txt")), "", "the cursor is used once")
        self.assertIn("reloaded after a resync", self.B.log())
        # the line behind the load was kept for the reloaded game, and new lines still arrive
        self.assertIn("speed=3", self.B.log())
        self.relay.speed = 2
        self.relay.started = True
        self.relay.session()
        self.tick(5)
        self.assertEqual(self.B.speed(), 2)
        # the reloaded writer went on at the end of from_game.log: the launcher reads both hellos, every line whole
        kinds = [l.split(TAB)[0] for l in self.B.lines(self.B.from_game)]
        self.assertEqual(kinds.count("hello"), 2)
        self.assertTrue(all(len(l.split(TAB)) == 3 for l in self.B.lines(self.B.from_game)))

    def resync_reload_b(self):
        """B loads the host's save on the relay's word and comes back as a fresh script (the old one's next frame starts the load)."""
        self.relay.send(self.B, "resync_load", 0, self.relay.ser({"id": 3, "name": "hostsave", "sha256": "00"}))
        self.B.frame()
        self.assertTrue(self.B.reload_pending())
        self.B.reload()

    def test_a_resync_load_waits_for_the_start_until_the_session_runs(self):
        self.start()
        self.tick(10)
        self.resync_reload_b()
        self.assertIn("ready: app.setWaitForStartReadyGame() before the load called", self.B.log())
        held_at = self.B.time()
        self.tick(8)
        self.assertEqual(self.B.time(), held_at, "the loaded game ran no step while it waited")
        self.assertTrue(self.B.L.eval("GAME.held == true and READY_STARTS == nil"))
        self.assertIn("ready: this loaded game waits", self.B.log())
        self.assertEqual(self.relay.hellos.count(1), 2, "the waiting game's interface half said hello")
        # the host's session line after the load releases it: started once, then it runs with the session
        self.relay.session()
        self.tick(6)
        self.assertEqual(int(self.B.L.eval("READY_STARTS")), 1, "app.startReadyGame was called once")
        self.assertIn("ready: app.startReadyGame() after", self.B.log())
        self.assertIn("(the session runs, speed", self.B.log())
        self.assertGreater(self.B.time(), held_at)
        self.tick(6)
        self.assertEqual(int(self.B.L.eval("READY_STARTS")), 1, "and never again")
        self.assertIsNone(self.A.L.eval("READY_STARTS"), "a game that did not load is not touched")
        self.assertFalse(self.log_has("LATE"))

    def test_a_resync_load_without_the_start_functions_keeps_the_speed_zero_hold(self):
        self.B.L.execute("app.setWaitForStartReadyGame, app.isWaitForStartReadyGame, app.startReadyGame = nil, nil, nil")
        self.start()
        self.tick(10)
        self.resync_reload_b()
        self.assertIn("ready: app.setWaitForStartReadyGame / app.startReadyGame missing", self.B.log())
        self.tick(4)
        self.assertFalse(self.B.L.eval("GAME.held == true"))
        self.assertEqual(self.B.speed(), 0, "the first frame's speed 0 holds the game until the session runs")
        self.relay.session()
        self.tick(6)
        self.assertEqual(self.B.speed(), 1)

    def test_a_game_the_session_never_releases_is_started_after_the_time_limit(self):
        self.start()
        self.tick(10)
        self.resync_reload_b()
        os.environ["FEVERLINK3_READY_WAIT_SECONDS"] = "0"
        try:
            self.tick(3)
        finally:
            os.environ.pop("FEVERLINK3_READY_WAIT_SECONDS", None)
        self.assertEqual(int(self.B.L.eval("READY_STARTS")), 1)
        self.assertIn("not released within 0 s", self.B.log())
        self.assertEqual(self.B.speed(), 0, "started and held by speed 0, the old path")

    def test_the_start_functions_as_the_game_binds_them_callable_tables_are_used(self):
        # the lab run of 2026-10-18 logged the interface state's app functions as tables ("saveUserdata=table"): bound engine functions
        self.B.L.execute("for _, n in ipairs({ 'setWaitForStartReadyGame', 'isWaitForStartReadyGame', 'startReadyGame' }) do"
                         " local f = app[n]; app[n] = setmetatable({}, { __call = function(_, ...) return f(...) end }) end")
        self.start()
        self.tick(10)
        self.resync_reload_b()
        self.assertIn("ready: app.setWaitForStartReadyGame() before the load called", self.B.log())
        self.tick(4)
        self.assertTrue(self.B.L.eval("GAME.held == true"))
        self.relay.session()
        self.tick(4)
        self.assertEqual(int(self.B.L.eval("READY_STARTS")), 1)
        self.assertIn("app keys (gui): app ", self.B.log())
        self.assertIn("by name: setWaitForStartReadyGame=table isWaitForStartReadyGame=table startReadyGame=table", self.B.log())
        self.assertIn("app keys (sim): app ", self.B.log())

    def test_the_start_wait_can_be_switched_off(self):
        os.environ["FEVERLINK3_READY_WAIT"] = "0"
        try:
            self.start()
            self.tick(10)
            self.resync_reload_b()
            self.tick(3)
        finally:
            os.environ.pop("FEVERLINK3_READY_WAIT", None)
        self.assertIn("ready: FEVERLINK3_READY_WAIT=0", self.B.log())
        self.assertFalse(self.B.L.eval("GAME.held == true"))

    def test_resync_load_of_an_unknown_save_fails_without_reloading(self):
        self.start()
        self.tick(5)
        self.relay.send(self.B, "resync_load", 0, self.relay.ser({"id": 4, "name": "nope", "sha256": "00"}))
        self.tick(3)
        self.assertFalse(self.B.reload_pending())
        self.assertEqual(len(self.relay.replay_failed), 1)
        self.assertEqual(read_text(os.path.join(self.B.dir, "resync_cursor.txt")), "")
        self.tick(3)
        self.assertEqual(self.B.speed(), 1, "the game keeps reading its link")

    # ---- permissions

    def test_denied_category_is_refused_before_stamping(self):
        self.start()
        self.tick(10)
        self.relay.send(self.B, "permissions", 0, self.relay.ser({"build": False, "spend": True}))
        self.tick(2)
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeLineCreateCmd("L", { x = 0, y = 0, z = 0 }, 7, { stops = {} }), function(d, s) LINE_OK = s end)')
        self.tick(30)
        self.assertTrue(self.B.L.eval("LINE_OK == false"), "the window got a failed result")
        self.assertEqual(self.A.applied(), [])
        self.assertEqual(self.B.applied(), [], "not even applied locally")
        self.assertFalse(any(l.split(TAB)[0] == "action" for l in self.B.lines(self.B.from_game)), "nothing stamped or shipped")
        self.assertEqual(self.relay.refused, [(1, 0, "permission denied: build")])
        # spending is still allowed
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function() end)')
        self.tick(40)
        self.assertEqual(len(self.A.applied()), 1)
        self.assertEqual(self.A.applied(), self.B.applied())
        # revoked later, granted again later
        self.relay.send(self.B, "permissions", 0, self.relay.ser({"spend": False}))
        self.relay.send(self.B, "permissions", 0, self.relay.ser({"build": True}))
        self.tick(3)
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function() end)')
        self.tick(30)
        self.assertEqual(len(self.A.applied()), 1, "the second purchase was refused")
        self.assertEqual(self.relay.refused[-1][2], "permission denied: spend")
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeLineCreateCmd("L", { x = 0, y = 0, z = 0 }, 7, { stops = {} }), function() end)')
        self.tick(40)
        self.assertEqual(len(self.A.applied()), 2, "building works again")

    def test_relayed_action_not_allowed_requests_a_resync(self):
        self.start()
        self.tick(10)
        body = self.relay.C.serialize(self.relay.C.parse(
            '{["fn"]="makeVehicleBuyCmd",["args"]={["n"]=3,[1]={["__ref"]="player"},[2]={["__ref"]="entity",["key"]="s:55"},[3]={}}}'))
        stamp = self.A.time() + 5 * STEP
        self.relay.send(self.A, "action", 1, self.relay.ser({"stamp": stamp, "seq": 4, "body": body, "from": 1, "allowed": False}))
        self.tick(15)
        self.assertEqual(self.A.applied(), [], "an action the sender was not allowed to make is not applied")
        self.assertEqual(len(self.relay.replay_failed), 1)
        self.assertEqual(self.relay.replay_failed[0][0], 0)
        self.assertIn("1:4", self.relay.replay_failed[0][1])
        self.assertIn("allowed=false", self.A.log())

    # ---- window

    def test_roster_status_and_chat(self):
        self.start(speed=2)
        self.relay.send(self.B, "welcome", 0, self.relay.ser({"you": 1, "hostName": "host", "version": "0.1", "players": [{"id": 1, "name": "client", "ready": True}, {"id": 2, "name": "carol", "ready": False}], "session": {}}))
        self.tick(4)
        view = self.B.ui_view()
        self.assertIsNotNone(view)
        status, roster, chat = str(view).split("|")
        self.assertEqual(status, "Running x2 - 3 players")
        self.assertEqual(roster, "host [host], client (you), carol")
        self.relay.send(self.B, "player_joined", 0, self.relay.ser({"player": {"id": 3, "name": "dave", "ready": False}}))
        self.relay.send(self.B, "player_left", 0, self.relay.ser({"id": 2, "name": "carol"}))
        self.tick(4)
        self.assertEqual(str(self.B.ui_view()).split("|")[1], "host [host], client (you), dave")
        # pause shows in the status
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(4)
        self.assertTrue(str(self.B.ui_view()).startswith("Paused"))
        # typed in the window of B: a chat line to the host, relayed to A, shown in both
        self.assertTrue(self.B.ui_send("hello team\t!"))
        self.assertFalse(self.B.ui_send("   "), "empty text is not sent")
        self.tick(4)
        self.assertEqual(self.relay.chats, [(1, "hello team !")])
        self.assertEqual(str(self.B.ui_view()).split("|")[2], "client: hello team !", "own message shown without an echo from the host")
        self.assertEqual(str(self.A.ui_view()).split("|")[2], "client: hello team !")
        # a reload keeps the roster and the chat of the window
        self.relay.send(self.B, "resync_load", 0, self.relay.ser({"id": 1, "name": "hostsave", "sha256": "00"}))
        self.B.frame()
        self.B.reload()
        self.tick(4)
        view = str(self.B.ui_view()).split("|")
        self.assertEqual(view[1], "host [host], client (you), dave")
        self.assertEqual(view[2], "client: hello team !")


    def test_speed_pressed_in_the_window_is_a_request_to_the_host(self):
        self.start(speed=1)
        self.assertTrue(self.B.ui_speed(3))
        self.assertTrue(self.A.ui_speed(0))
        self.assertTrue(self.B.ui_speed(9), "the record is written; the bridge refuses a speed it does not know")
        self.tick(4)
        b = [l for l in self.B.lines(self.B.from_game) if l.split(TAB)[0] == "speed_request"]
        a = [l for l in self.A.lines(self.A.from_game) if l.split(TAB)[0] == "speed_request"]
        self.assertEqual(len(b), 1, b)
        self.assertIn('["speed"]=3', b[0])
        self.assertEqual(len(a), 1, a)
        self.assertIn('["speed"]=0', a[0])


BUY_IN_WINDOW = ('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }),'
                 ' function(d, s, e) %s = { s, d and d.resultVehicleEntity } end)')


class StampedEverywhereTest(unittest.TestCase):
    """Every game, the originator included, applies a command at its stamp, in stamp order: then the engine hands out the same
    entity ids on every game. Both mocked games here allocate ids from the SAME base (in LockstepTest they differ on purpose), so
    an action applied at another step, or in another order, on one game shows as different ids."""

    tearDown = LockstepTest.tearDown
    tick = LockstepTest.tick
    start = LockstepTest.start

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-")
        self.A = Game("host", 0, self.tmp, 50000)
        self.B = Game("client", 1, self.tmp, 50000)
        self.relay = Relay([self.A, self.B])

    def entities(self, g):
        return g.L.eval("(function() local r = {} for i, a in ipairs(GAME.applied) do r[i] = tostring(a.entity) end return table.concat(r, ',') end)()")

    def assertSameApplies(self, n):
        a, b = self.A.applied(), self.B.applied()
        self.assertEqual(len(a), n, a)
        self.assertEqual(a, b, "the same commands at the same steps in the same order")
        self.assertEqual(self.entities(self.A), self.entities(self.B), "the same entity ids")

    def test_a_vehicle_bought_and_a_line_made_in_a_window_are_applied_at_their_stamp_on_the_originator_too(self):
        self.start()
        self.tick(10, lag_b=True)
        self.A.window(BUY_IN_WINDOW % "BUY")
        # the other player buys from the bridge's own state a moment later: the two purchases interleave by stamp
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function() end)')
        self.tick(1)
        self.assertEqual(self.A.applied(), [], "not applied at the click")
        self.assertTrue(self.A.L.eval("BUY == nil"), "the window's callback waits for the stamp apply")
        self.tick(40, lag_b=True)
        self.assertSameApplies(2)
        own = [i for i in (1, 2) if int(self.A.L.eval("GAME.applied[%d].args[2]" % i)) == 55]
        self.assertTrue(self.A.L.eval("BUY ~= nil and BUY[1] == true"))
        self.assertIn(int(self.A.L.eval("BUY[2]")), [int(self.A.L.eval("GAME.applied[%d].entity" % i)) for i in own])
        self.assertIn("held until its stamp", self.A.log())
        self.assertIn("applied at its stamp", self.A.log())
        # a line from a window of the client, then the vehicle the host bought put on it from the host's window
        self.B.window('api.cmd.sendCommand(api.cmd.makeLineCreateCmd("L", { x = 0, y = 0, z = 0 }, 7, { stops = {} }), function(d, s, e) LINE = e[1][1] end)')
        self.tick(40, lag_b=True)
        self.assertSameApplies(3)
        line = int(self.B.L.eval("LINE"))
        self.assertEqual(line, int(self.B.L.eval("GAME.applied[3].entity")))
        self.A.window("api.cmd.sendCommand(api.cmd.makeVehicleSetLineCmd(%d, %d, 0), function(d, s) SET = s end)" % (int(self.A.L.eval("BUY[2]")), line))
        self.tick(40, lag_b=True)
        self.assertSameApplies(4)
        self.assertTrue(self.A.L.eval("SET == true"))
        self.assertFalse(any("LATE" in g.log() for g in (self.A, self.B)))

    def test_the_interface_half_talks_to_the_simulation_half_with_an_empty_event_source(self):
        # makeScriptingSendEventCmd(src, id, name, param): src is "the filename of the game_script that should be the pretended origin"
        # (tf3-docs api/cmd); the bridge's messages have none and send "" as the base game does; handleEvent tells them by id
        self.start()
        self.tick(5)
        srcs = self.A.L.eval("(function() local out = {} for _, e in ipairs(EVENTS_SEEN) do if e[1] == 'feverlink3' then out[#out + 1] = e[3] end end return table.concat(out, ',') end)()")
        self.assertTrue(self.A.L.eval("(function() for _, e in ipairs(EVENTS_SEEN) do if e[1] == 'feverlink3' then return true end end return false end)()"),
                        "the session message went to the simulation half")
        self.assertEqual(set(srcs.split(",")), {""}, srcs)

    def test_a_callback_gets_entity_and_revision_pairs_as_send_command_documents(self):
        # sendCommand's callback: (data, success, resultEntities : {{Engine.Entity, Engine.Revision}}) (tf3-docs api/cmd). The result of
        # a stamp apply names the entity only; the callback gets this game's revision of it, from the bridge and from a window alike
        self.start()
        self.tick(10, lag_b=True)
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), '
                        'function(d, s, e) BUY_E = { e[1][1], e[1][2] and e[1][2].num and e[1][2].num[1] } end)')
        self.A.window('api.cmd.sendCommand(api.cmd.makeLineCreateCmd("L", { x = 0, y = 0, z = 0 }, 7, { stops = {} }), '
                      'function(d, s, e) LINE_E = { e[1][1], e[1][2] and e[1][2].num and e[1][2].num[1] } end)')
        self.tick(40, lag_b=True)
        vb = int(self.B.L.eval("BUY_E[1]"))
        self.assertEqual(int(self.B.L.eval("BUY_E[2]")), 1000 + vb, "the bridge's answer carries the vehicle's revision")
        la = int(self.A.L.eval("LINE_E[1]"))
        self.assertEqual(int(self.A.L.eval("LINE_E[2]")), 1000 + la, "the window's answer carries the line's revision")
        # pairs that came with a revision keep it; an entity that is gone keeps the pair as it came
        C = self.A.L.eval("GUI_ENV.ug_require('feverlink3_common.lua')")
        out = C.resultEntities(self.A.L.eval("{ { %d, { num = { 5, 0, 0 } } }, { 424242 }, 'x' }" % la))
        self.assertEqual(out[1][2]["num"][1], 5)
        self.assertEqual((out[2][1], out[2][2]), (424242, None))
        self.assertIsNone(out[3])

    def test_a_purchase_refused_at_its_stamp_is_refused_on_every_game_and_the_window_hears_it(self):
        self.start()
        self.tick(5)
        for g in (self.A, self.B):
            g.L.execute("NO_MONEY = true")
        self.A.window(BUY_IN_WINDOW % "BUY")
        self.tick(40)
        self.assertEqual((self.A.applied(), self.B.applied()), ([], []))
        self.assertTrue(self.A.L.eval("BUY ~= nil and BUY[1] == false"), "the callback ran with the refusal")
        for g in (self.A, self.B):
            self.assertIn("refused by the engine: ", g.log())
        self.assertIn("REFUSED at its stamp", self.A.log())

    def make_line(self):
        self.A.window('api.cmd.sendCommand(api.cmd.makeLineCreateCmd("L", { x = 0, y = 0, z = 0 }, 7, { stops = {} }), function(d, s, e) LINE = e[1][1] end)')
        self.tick(40)
        return int(self.A.L.eval("LINE"))

    def actions_shipped(self, g, fn):
        """The stamps of g's own actions of command `fn`, as shipped to the host (from_game.log)."""
        parse = g.L.eval("function(s) return UI_ENV_NEW().C.parse(s) end")
        out = []
        for line in g.lines(g.from_game):
            parts = line.split(TAB, 2)
            if len(parts) == 3 and parts[0] == "action":
                p = parse(parts[2])
                if ('"%s"' % fn) in str(p.body):
                    out.append(int(p.stamp))
        return out

    def test_a_clone_of_n_vehicles_is_one_order_stamped_once_with_no_wait_and_the_same_on_both_games(self):
        # Friend test 2026-10-16: cloning vehicles froze the game. The game's clone (vehicle_react_util.HandleVehicleChanges) buys
        # every copy in one loop and puts each on the line from the purchase's callback. Here: the window's sendCommand returns at
        # once (nothing waits for the network), the purchases are one order with one stamp, the line assignments another, and
        # both games apply the same commands at the same steps with the same entity ids.
        self.start()
        self.tick(5)
        line = self.make_line()
        n = 3
        self.A.window(('CLONED = {}; for i = 1, %d do api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }),'
                       ' function(d, s) if s then api.cmd.sendCommand(api.cmd.makeVehicleSetLineCmd(d.resultVehicleEntity, %d, 0),'
                       ' function(_, s2) CLONED[#CLONED + 1] = s2 end) end end) end') % (n, line))
        self.assertEqual(len(self.A.applied()), 1, "nothing of the clone applied at the click")
        self.assertEqual(int(self.A.L.eval("#CLONED")), 0, "sendCommand returned without waiting for any answer")
        parse = self.A.L.eval("function(s) return UI_ENV_NEW().C.parse(s) end")
        recs = [parse(l.split(TAB, 2)[2]) for l in self.A.lines(os.path.join(self.A.dir, "ui_out.log")) if l.split(TAB)[0] == "cmd"]
        buys = [r for r in recs if "makeVehicleBuyCmd" in str(r.body)]
        self.assertEqual(len(buys), n)
        self.assertEqual(len({int(r.grp) for r in buys}), 1, "the purchases of one handler are one group")
        self.tick(60)
        self.assertSameApplies(1 + 2 * n)
        self.assertEqual([str(v) for v in self.A.L.eval("CLONED").values()], ["True"] * n)
        applied = self.A.applied()
        self.assertEqual(len({a for a in applied if a.startswith("makeVehicleBuyCmd@")}), 1, "every purchase in one step")
        self.assertEqual(len({a for a in applied if a.startswith("makeVehicleSetLineCmd@")}), 1, "every line assignment in one step")
        self.assertEqual(len(set(self.actions_shipped(self.A, "makeVehicleBuyCmd"))), 1, "one stamp for the order")
        log = self.A.log()
        self.assertEqual(log.count("3 command(s) stamped once"), 2, "the purchases one order, the line assignments one order")
        self.assertEqual(log.count("timing window #"), 1 + 2 * n, "one timing line per window command on the originator's window")
        for g in (self.A, self.B):
            self.assertEqual(g.log().count("timing window command "), 1 + 2 * n, "one apply timing line per command on every game")
        self.assertFalse(any("LATE" in g.log() for g in (self.A, self.B)))

    def test_a_window_order_that_cannot_be_shipped_whole_is_shipped_nowhere_and_every_command_of_it_fails(self):
        self.start()
        self.tick(5)
        # two records of one group, the second naming the entity of a window command that was never shipped
        self.A.window("local _ = 0")
        self.A.L.execute(r'''
          local C = UI_HOOK.ug_require("feverlink3_common.lua")
          local send = UI_HOOK.HookModule.send
          send("cmd", { id = 901, grp = 4242, category = "spend", sid = C.SESSION,
            body = '{["fn"]="makeVehicleBuyCmd",["args"]={["n"]=3,[1]={["__ref"]="player"},[2]={["__ref"]="entity",["key"]="s:55"},[3]={}}}' })
          send("cmd", { id = 902, grp = 4242, category = "other", sid = C.SESSION,
            body = '{["fn"]="makeVehicleSetLineCmd",["args"]={["n"]=3,[1]={["__ref"]="entity",["key"]="ui:999"},[2]={["__ref"]="entity",["key"]="s:55"},[3]=0}}' })
        ''')
        self.tick(40)
        self.assertEqual((self.A.applied(), self.B.applied()), ([], []), "nothing of the order is applied anywhere")
        self.assertEqual(self.actions_shipped(self.A, "makeVehicleBuyCmd"), [], "the valid purchase did not leave either")
        self.assertIn("a window order of 2 command(s) was not shipped, none of it", self.A.log())
        answers = [l for l in self.A.lines(os.path.join(self.A.dir, "ui_in.log")) if l.split(TAB)[0] == "result"]
        self.assertEqual(len(answers), 2, answers)
        self.assertTrue(all('["success"]=false' in a for a in answers))

    def pause_both(self):
        self.start(speed=2)
        self.tick(10, lag_b=True)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(40, lag_b=True)
        return self.relay.pauseAt

    def test_commands_of_two_players_during_a_pause_are_applied_at_one_step_in_one_order_on_both_games(self):
        paused_at = self.pause_both()
        self.assertEqual((self.A.time(), self.B.time()), (paused_at, paused_at))
        # each player acts during the pause: before, each game applied its own first (arrival order) and the ids crossed. Now the host
        # stamps them past where every game stands still and every game, the originator too, applies them at that stamp
        self.A.window(BUY_IN_WINDOW % "A_BUY")
        self.B.send_gui(BUY_IN_WINDOW % "B_BUY")
        self.B.window('api.cmd.sendCommand(api.cmd.makeLineCreateCmd("L", { x = 0, y = 0, z = 0 }, 7, { stops = {} }), function() end)')
        self.A.send_gui('api.cmd.sendCommand(api.cmd.makeLineCreateCmd("M", { x = 0, y = 0, z = 0 }, 7, { stops = {} }), function() end)')
        self.tick(16)
        self.assertSameApplies(4)
        self.assertTrue(all(int(x.split("@")[1]) > paused_at for x in self.A.applied()), self.A.applied())
        self.assertEqual(self.A.time(), self.B.time(), "both stand still again at one step")
        self.assertLessEqual(self.A.time(), paused_at + 4 * STEP, "the pause's world moved only a few steps")
        self.assertEqual((self.A.speed(), self.B.speed()), (0, 0), "still paused")
        self.assertEqual(self.relay.pause_seq, 4)
        self.assertTrue(self.A.L.eval("A_BUY ~= nil and A_BUY[1] == true"), "the window's callback ran during the pause")
        self.assertTrue(self.B.L.eval("B_BUY ~= nil and B_BUY[1] == true"))
        self.relay.pauseAt, self.relay.speed = None, 2
        self.relay.session()
        self.tick(20)
        self.assertGreater(self.A.time(), paused_at + 4 * STEP)
        self.assertSameApplies(4)
        self.assertFalse(any("LATE" in g.log() or "PAUSE ORDER GAP" in g.log() for g in (self.A, self.B)))

    def test_a_paused_action_that_reaches_the_host_after_its_resume_gets_a_normal_stamp_on_every_game(self):
        paused_at = self.pause_both()
        self.B.send_gui(BUY_IN_WINDOW % "B_BUY")   # stamped at the pause, in B's outbox, not yet at the host
        self.A.send_gui(BUY_IN_WINDOW % "A_BUY")
        # the host resumes before it reads them: the resume line goes out first, the paused actions get a normal stamp after it
        self.relay.pauseAt, self.relay.speed = None, 2
        self.relay.session()
        self.tick(20)
        for g in (self.A, self.B):
            lines = self.relay.history[g.id]
            kinds = [ln.split(TAB)[0] for ln in lines]
            last_pause = max(i for i, ln in enumerate(lines) if "pauseAt" in ln)
            resume = kinds.index("session", last_pause + 1)
            first_action = kinds.index("action", last_pause + 1)
            self.assertGreater(first_action, resume, "the paused actions reached this game after the resume line")
        self.assertSameApplies(2)
        self.assertGreater(min(self.A.time(), self.B.time()), paused_at, "and then both went on")
        self.assertTrue(self.B.L.eval("B_BUY ~= nil and B_BUY[1] == true"))
        self.assertTrue(self.A.L.eval("A_BUY ~= nil and A_BUY[1] == true"))
        self.assertFalse(any("LATE" in g.log() for g in (self.A, self.B)))


class ThreeGamesPauseOrderTest(unittest.TestCase):
    """Three games, one id base. The real engine's speed 0 takes effect a few steps late, by a different amount on each game (real run
    pause3, 2026-10-15: 3 steps at x3): here the games overshoot the pause by 0, 2 and 3 steps. Every player buys and makes lines in the
    pause, interleaved; every game applies them at the same step, in the same order, with the same entity ids, never late."""

    tearDown = LockstepTest.tearDown
    entities = StampedEverywhereTest.entities

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-")
        self.A = Game("host", 0, self.tmp, 50000)
        self.B = Game("client", 1, self.tmp, 50000)
        self.C = Game("third", 2, self.tmp, 50000)
        self.games = (self.A, self.B, self.C)
        self.relay = Relay(list(self.games))

    def tick(self, n=1):
        for _ in range(n):
            for _ in range(4):
                for g in self.games:
                    g.frame()
                self.relay.pump()
            for g in self.games:
                g.step()

    def pause(self, overshoot=(0, 0, 0)):
        for g, k in zip(self.games, overshoot):
            g.L.execute("OVERSHOOT = %d" % k)
        self.tick(5)
        self.relay.started, self.relay.speed = True, 3
        self.relay.session()
        self.tick(10)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(30)
        return self.relay.pauseAt

    def assertSameEverywhere(self, n):
        a = self.A.applied()
        self.assertEqual(len(a), n, a)
        for g in (self.B, self.C):
            self.assertEqual(g.applied(), a, "the same steps and order on every game")
            self.assertEqual(self.entities(g), self.entities(self.A), "the same entity ids")
        self.assertFalse(any("LATE" in g.log() for g in self.games), [g.log() for g in self.games if "LATE" in g.log()])

    def test_three_players_in_one_pause(self):
        paused_at = self.pause()
        self.assertEqual([g.time() for g in self.games], [paused_at] * 3)
        line = 'api.cmd.sendCommand(api.cmd.makeLineCreateCmd("%s", { x = 0, y = 0, z = 0 }, 7, { stops = {} }), function() end)'
        self.C.send_gui(BUY_IN_WINDOW % "C1")
        self.A.window(line % "A")
        self.B.send_gui(BUY_IN_WINDOW % "B1")
        self.C.window(line % "C")
        self.A.send_gui(BUY_IN_WINDOW % "A1")
        self.tick(16)
        self.assertSameEverywhere(5)
        self.relay.pauseAt, self.relay.speed = None, 2
        self.relay.session()
        self.tick(20)
        self.assertTrue(all(g.time() > paused_at for g in self.games))

    def test_games_that_overshoot_the_pause_by_different_amounts_apply_the_pauses_actions_at_one_step(self):
        paused_at = self.pause(overshoot=(0, 2, 3))
        stood = [g.time() for g in self.games]
        self.assertEqual(stood, [paused_at, paused_at + 2 * STEP, paused_at + 3 * STEP], "the mock overshoots as the real engine")
        line = 'api.cmd.sendCommand(api.cmd.makeLineCreateCmd("%s", { x = 0, y = 0, z = 0 }, 7, { stops = {} }), function() end)'
        self.B.send_gui(BUY_IN_WINDOW % "B1")
        self.A.window(line % "A")
        self.C.send_gui(BUY_IN_WINDOW % "C1")
        self.tick(30)
        self.assertSameEverywhere(3)
        first = int(self.A.applied()[0].split("@")[1])
        self.assertEqual(first, paused_at + 4 * STEP, "one step past the furthest game")
        # a second batch: the games ran on to the first stamp and overshot it again, each by its own amount
        self.A.send_gui(BUY_IN_WINDOW % "A2")
        self.B.window(line % "B")
        self.tick(30)
        self.assertSameEverywhere(5)
        for name in ("B1", "C1", "A2"):
            owner = {"A": self.A, "B": self.B, "C": self.C}[name[0]]
            self.assertTrue(owner.L.eval("%s ~= nil and %s[1] == true" % (name, name)), name)
        self.relay.pauseAt, self.relay.speed = None, 2
        self.relay.session()
        self.tick(20)
        self.assertSameEverywhere(5)
        self.assertTrue(all(g.time() > first for g in self.games))


class EventSubscriptionTest(unittest.TestCase):
    """The scripts subscribe to the event names they handle (subscribeToEvent), not to all events; the mock drops the others."""

    setUp, tearDown, tick, start = LockstepTest.setUp, LockstepTest.tearDown, LockstepTest.tick, LockstepTest.start

    def sim_names(self, g):
        return sorted(g.L.eval("(function() local t = {}; for k in pairs(SUBS.names) do t[#t + 1] = k end; return table.concat(t, ' ') end)()").split())

    def test_every_message_the_interface_half_sends_is_subscribed(self):
        with open(os.path.join(CONTENT, "feverlink3_bridge.script.lua"), encoding="utf-8") as f:
            src = f.read()
        sent = set(re.findall(r'toSim\(\s*"(\w+)"', src)) | set(re.findall(r'toSim,\s*"(\w+)"', src))
        listed = set(re.search(r"X\.SIM_EVENTS = \{(.*?)\}", src, re.S).group(1).replace('"', " ").replace(",", " ").split())
        self.assertGreater(len(sent), 10)
        self.assertEqual(sorted(sent - listed), ["nop"], "only the strict mode's trigger is not subscribed")
        for n in ("onPreBuildProposal", "onPostBuildProposal"):
            self.assertIn(n, listed)

    def test_the_session_runs_with_the_named_subscriptions_and_an_unknown_name_is_not_received(self):
        self.start()
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function() end)')
        self.tick(40)
        self.assertEqual(len(self.A.applied()), 1)
        self.assertEqual(self.A.applied(), self.B.applied(), "the stamped action reached both simulation halves")
        for g in (self.A, self.B):
            self.assertFalse(g.L.eval("SUBS.all"), "not every event")
            self.assertIn("queue", self.sim_names(g))
            self.assertIn("session", self.sim_names(g))
            self.assertNotIn("nop", self.sim_names(g))
            self.assertTrue(g.L.eval("GSUBS.all == false and GSUBS.names['builder.proposalCreate'] and GSUBS.names['builder.proposalApply']"))
            self.assertEqual(g.L.eval("#SUBS.dropped"), 0, "no event the mod sends was dropped")
            self.assertIn("events (sim, at", g.log())
        self.A.L.execute("SIM_EVENT('', 'feverlink3', 'somethingElse', {})")
        self.assertEqual(self.A.L.eval("SUBS.dropped[1]"), "feverlink3/somethingElse")

    def test_the_lab_switch_subscribes_to_all_events(self):
        os.environ["FEVERLINK3_EVENTS"] = "all"
        try:
            self.A.reload()
            self.tick(3)
        finally:
            os.environ.pop("FEVERLINK3_EVENTS", None)
        self.assertTrue(self.A.L.eval("SUBS.all and GSUBS.all"))
        self.assertIn("all (FEVERLINK3_EVENTS=all)", self.A.log())


class PureTest(unittest.TestCase):
    """The common module and the core, without any game."""

    def setUp(self):
        os.environ.pop("FEVERLINK3_SESSION_DIR", None)
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.C = load_common(self.L)
        self.K = self.L.execute(LOCKSTEP_SRC)
        self.H = self.L.execute(HUD_SRC)

    def test_integer_text_of_money_beyond_32_bits(self):
        f = self.C.integerText
        self.assertEqual(f(5035445200), "5035445200")
        self.assertEqual(f(-5035445200), "-5035445200")
        self.assertEqual(f(0), "0")
        self.assertEqual(f(-0.0), "0")
        self.assertEqual(f(37000000.0), "37000000")
        self.assertEqual(f(2 ** 60), "1.152921504606847e+18")
        self.assertEqual(float(f(1234.5678)), 1234.5678, "a fraction round-trips")
        self.assertEqual(self.C.serialize(5035445200), "5035445200")

    def test_serialize_is_order_independent_and_round_trips(self):
        a = self.L.eval('(function() local t = {} t.b = 1 t.a = { 3, 2, x = "q\\t\\"z" } t[1] = 0.1 t[2] = true return t end)()')
        b = self.L.eval('(function() local t = { [2] = true, [1] = 0.1 } t.a = { x = "q\\t\\"z", 3, 2 } t.b = 1 return t end)()')
        sa, sb = self.C.serialize(a), self.C.serialize(b)
        self.assertEqual(sa, sb)
        self.assertEqual(sa, '{[1]=0.10000000000000001,[2]=true,["a"]={[1]=3,[2]=2,["x"]="q\\t\\"z"},["b"]=1}')
        self.assertNotIn("\n", sa)
        back = self.C.parse(sa)
        self.assertEqual(self.C.serialize(back), sa)
        self.assertEqual(self.C.serialize(self.C.parse(self.C.serialize(-0.0))), "0")
        self.assertEqual(self.C.serialize(2 ** 60), "1.152921504606847e+18")
        r = self.C.parse("os.exit()")   # no globals in the payload environment: nil plus a reason
        self.assertIsNone(r[0] if isinstance(r, tuple) else r)
        with self.assertRaises(lupa.LuaError):
            self.C.serialize(self.L.eval("print"))

    WIDE_ENGINE = """(function(v)
      local e = {
        gameTime = function() return 5 end,
        account = function() return 100, 10 end,
        constructions = function() return {} end,
        lines = function() return {} end,
      }
      if v.stations then e.stations = function() return v.stations end end
      if v.edges then e.edges = function() return v.edges end end
      if v.nodeConfigs then e.nodeConfigs = function() return v.nodeConfigs end end
      if v.vehicles then e.vehicles = function() return v.vehicles end end
      if v.names then e.names = function() return v.names end end
      return e
    end)"""

    def _wide(self, **kw):
        L = self.L
        mk = L.eval(self.WIDE_ENGINE)
        tbl = L.table_from(kw, recursive=True)
        parts, _t = self.K.checksumParts(mk(tbl), self.C.hashValue)
        return {k: parts[k] for k in parts}

    def test_wide_lanes_are_absent_without_their_accessor(self):
        parts = self._wide()
        for name in ("stations", "edges", "nodeConfigs", "vehicles", "names"):
            self.assertNotIn(name, parts)
        self.assertIn("balance", parts)

    def test_stations_lane_ignores_order_and_sees_a_moved_stop(self):
        a = [{"stations": 1, "terminals": 2, "at": [{"x": 1.04, "y": 2}, {"x": 5, "y": 6}]}, {"stations": 1, "terminals": 1, "at": [{"x": 9, "y": 9}]}]
        b = [a[1], {"stations": 1, "terminals": 2, "at": [{"x": 5, "y": 6.01}, {"x": 1.0, "y": 2}]}]
        c = [a[0], {"stations": 1, "terminals": 1, "at": [{"x": 9, "y": 12}]}]
        pa, pb, pc = (self._wide(stations=x)["stations"] for x in (a, b, c))
        self.assertEqual(pa, pb)
        self.assertNotEqual(pa, pc)

    def test_edges_lane_canonical_endpoints_and_edge_objects_count(self):
        base = {"x0": 0, "y0": 0, "x1": 50, "y1": 0, "type": "NORMAL", "typeIndex": -1, "roadType": "STREET", "template": "t", "style": "s", "decorations": 0, "objects": ["signal.mdl", "stop.mdl"]}
        flipped = dict(base, x0=50, x1=0, objects=["stop.mdl", "signal.mdl"])
        fewer = dict(base, objects=["stop.mdl"])
        other = dict(base, template="u")
        h = lambda e: self._wide(edges=[e])["edges"]
        self.assertEqual(h(base), h(flipped))
        self.assertNotEqual(h(base), h(fewer))
        self.assertNotEqual(h(base), h(other))

    def test_node_configs_lane_sees_a_traffic_light(self):
        n = {"x": 1, "y": 2, "lanes": 4, "crosswalks": 1, "preference": "AUTO", "light": 0, "slip": False}
        h = lambda x: self._wide(nodeConfigs=[x])["nodeConfigs"]
        self.assertEqual(h(n), h(dict(n)))
        self.assertNotEqual(h(n), h(dict(n, light=1)))

    def test_vehicles_lane_counts_per_line_and_ignores_order(self):
        v = lambda line, state="EN_ROUTE", stopped=False: {"line": line, "state": state, "stopped": stopped, "sell": False}
        a = [v("A#2"), v("A#2", "AT_TERMINAL"), v("B#3")]
        b = [v("B#3"), v("A#2", "AT_TERMINAL"), v("A#2")]
        h = lambda x: self._wide(vehicles=x)["vehicles"]
        self.assertEqual(h(a), h(b))
        self.assertNotEqual(h(a), h(a[:2]))                       # a vehicle fewer
        self.assertNotEqual(h(a), h([v("A#2"), v("A#2"), v("B#3", stopped=True)]))

    def test_names_lane_sees_a_renamed_station_and_a_town_but_not_order(self):
        n = {"stations": ["Alpha", "Beta"], "lines": ["L1"], "towns": ["T1", "T2"], "players": ["P"]}
        h = lambda x: self._wide(names=x)["names"]
        self.assertEqual(h(n), h(dict(n, stations=["Beta", "Alpha"])))
        self.assertNotEqual(h(n), h(dict(n, stations=["Alpha", "Gamma"])))
        self.assertNotEqual(h(n), h(dict(n, towns=["T1", "T3"])))
        self.assertNotEqual(h(n), h(dict(n, players=["Q"])))

    def test_names_lane_is_itemised_per_entity_for_the_detail_dump(self):
        # a names difference must say which entity (friend test 2026-10-16 could only say "names")
        L = self.L
        e = L.eval(self.WIDE_ENGINE)(L.table_from({"names": {"stations": ["Beta", "Alpha"], "lines": ["L1"], "towns": ["T1"], "players": ["P"]}},
                                                  recursive=True))
        detail = L.table()
        self.K.checksumParts(e, self.C.hashValue, None, False, detail)
        items = [detail["names"][i] for i in range(1, len(detail["names"]) + 1)]
        self.assertEqual(items, ["lines|L1", "players|P", "stations|Alpha", "stations|Beta", "towns|T1"])

    def test_a_transport_mode_map_travels_by_the_enums_names(self):
        # {TransportMode : boolean} (script-doc api/type.html): keyed by api.type.enum.TransportMode values; it travels by name and
        # is keyed by this game's own enum values again, whatever numbers they are (wiki audit 2026-10-18 A5)
        L = self.L
        both = L.eval("function(f, x) local a, b = f(x); return { a, b } end")

        def call(f, code):
            return both(f, L.eval(code))
        L.execute("FL3_SAVED_API = api; api = { type = { enum = { TransportMode = { PERSON = 0, CARGO = 1, BUS = 3, SHIP = 10 } } } }")
        try:
            r = call(self.C.modeNames, "{ [0] = true, [1] = false, [3] = true }")
            self.assertIsNone(r[2])
            self.assertEqual(dict(r[1].items()), {"PERSON": True, "CARGO": False, "BUS": True})
            # a game whose enum gives the names other numbers keys the map by its own
            L.execute("api.type.enum.TransportMode = { PERSON = 20, CARGO = 21, BUS = 23 }")
            back = both(self.C.modeMap, r[1])
            self.assertIsNone(back[2])
            self.assertEqual(dict(back[1].items()), {20: True, 21: False, 23: True})
            # fail closed: a key the enum does not name, a name this game lacks, a value that is not a boolean, no enum at all
            r = call(self.C.modeNames, "{ [99] = true }")
            self.assertIsNone(r[1])
            self.assertIn("99 is not a value of api.type.enum.TransportMode", r[2])
            self.assertIn("SHIP is not in api.type.enum.TransportMode here", call(self.C.modeMap, "{ SHIP = true }")[2])
            self.assertIn("not a boolean", call(self.C.modeNames, "{ [20] = 1 }")[2])
            L.execute("api = {}")
            self.assertIn("no api.type.enum.TransportMode", call(self.C.modeMap, "{ BUS = true }")[2])
            self.assertIsNone(self.C["writeEngineArray"], "the shift guess is gone")
        finally:
            L.execute("api = FL3_SAVED_API; FL3_SAVED_API = nil")

    def test_a_failing_accessor_yields_an_error_part_not_a_crash(self):
        L = self.L
        e = L.eval(self.WIDE_ENGINE)(L.table_from({"stations": [{"stations": 1}]}, recursive=True))
        e.stations = L.eval("function() error('boom') end")
        parts, _t = self.K.checksumParts(e, self.C.hashValue)
        self.assertEqual(parts["stations"], "error")

    def test_window_placeholders_become_the_keys_of_the_records_that_made_them(self):
        body = self.C.parse('{["args"]={[1]={["__ref"]="entity",["key"]="ui:3"},[2]={["list"]={{["__ref"]="entity",["key"]="s:9"},'
                            '{["__ref"]="entity",["key"]="ui:4"}}}},["fn"]="x"}')
        body = body[0] if isinstance(body, tuple) else body
        seq = self.L.eval("function(id) return ({ [3] = 12, [4] = 13 })[id] end")
        ok = self.K.rewriteUiKeys(body, seq, 1)
        self.assertTrue(ok)
        text = self.C.serialize(body)
        self.assertIn('"1:12"', text)
        self.assertIn('"1:13"', text)
        self.assertIn('"s:9"', text, "a saved entity is left alone")
        self.assertNotIn("ui:", text)
        body2 = self.C.parse('{["args"]={[1]={["__ref"]="entity",["key"]="ui:7"}}}')
        body2 = body2[0] if isinstance(body2, tuple) else body2
        ok2, missing = self.K.rewriteUiKeys(body2, seq, 1)
        self.assertFalse(ok2)
        self.assertEqual(7, missing, "a command that was never shipped cannot be named")

    def test_an_action_that_waits_for_a_pending_entity_is_not_a_refusal(self):
        body = self.C.parse('{["args"]={[1]={["__ref"]="entity",["key"]="1:5"},[2]={["__ref"]="entity",["key"]="s:8"}}}')
        body = body[0] if isinstance(body, tuple) else body
        pend = lambda k: self.K.keyIsPending(k, self.L.eval("{ { key = '1:5' } }"), self.L.eval("{ { key = '0:2' } }"))  # noqa: E731
        self.assertTrue(pend("1:5"))
        self.assertTrue(pend("0:2"))
        self.assertTrue(pend("0:2#stop1"), "a sub-key of an action still looking for its entities")
        self.assertFalse(pend("0:20"))
        self.assertFalse(pend("s:8"))
        isp = self.L.eval("function(k) return k == '1:5' end")
        self.assertEqual("1:5", self.K.pendingRef(body, isp))
        self.assertIsNone(self.K.pendingRef(body, self.L.eval("function(k) return false end")))

    def test_notes_of_what_a_game_did_not_replicate_ride_with_its_actions_and_explain_a_refusal(self):
        notes = self.L.eval("{}")
        self.K.addNote(notes, 1000, "a native build at t=1000 was not replicated")
        self.K.addNote(notes, 5000, "action 1:5 did not find every entity it created")
        for i in range(10):
            self.K.addNote(notes, 6000 + i, "n" + str(i), 4)
        self.assertEqual(4, len(notes), "the list is bounded")
        fresh = self.L.eval("{ { at = 5000, text = 'a' }, { at = 5100, text = 'b' }, { at = 5200, text = 'c' }, { at = 5300, text = 'd' }, { at = 5400, text = 'e' } }")
        got = self.K.recentNotes(fresh, 5500)
        self.assertEqual(3, len(got), "the newest three")
        self.assertEqual("t=5400 e", got[3])
        self.assertEqual(0, len(self.K.recentNotes(fresh, 5000 + 700000)), "old notes are not relevant to later actions")
        body = self.L.eval("{ diverged = { 't=997800 a native build was not replicated' } }")
        why = self.K.refusalDetail("argument 2: stop 2: stationGroup s:1 does not exist here", body)
        self.assertIn("[the sender had not replicated: t=997800 a native build was not replicated]", why)
        plain = self.K.refusalDetail("not enough money", body)
        self.assertEqual("not enough money", plain, "only a reference problem is explained by it")

    def test_registered_commands_may_bring_their_own_codec(self):
        lua = self.L.eval("""function(K)
          K.registerCommands({
            makeCustomCmd = {
              category = "build",
              encode = function(name, args, ctx) return { fn = name, args = { n = 1, args[1].tag } } end,
              decode = function(body, ctx) return { n = 1, { tag = body.args[1] } } end,
            },
          })
          local body = K.encodeCommand("makeCustomCmd", { n = 1, { tag = "road" } }, {})
          local args = K.decodeCommand(body, {})
          return body.fn, body.args[1], args[1].tag, K.categoryOf("makeCustomCmd")
        end""")
        fn, tag, back, category = lua(self.K)
        self.assertEqual((fn, tag, back, category), ("makeCustomCmd", "road", "road", "build"))
        # a registered command without hooks still goes through the generic path
        generic = self.L.eval("""function(K)
          K.registerCommands({ makeGenericCmd = { category = "other" } })
          local body, why = K.encodeCommand("makeGenericCmd", { n = 1, 42 }, { plain = function(v) return v end })
          return body and body.args[1] or why
        end""")
        self.assertEqual(generic(self.K), 42)

    def test_fnv1a_known_vectors(self):
        self.assertEqual(int(self.C.fnv1a("")), 0x811C9DC5)
        self.assertEqual(int(self.C.fnv1a("a")), 0xE40C292C)
        self.assertEqual(int(self.C.fnv1a("foobar")), 0xBF9CF968)
        self.assertEqual(self.C.hashValue(self.L.table_from({"x": 1})), self.C.hashValue(self.L.table_from({"x": 1})))

    def test_reader_reads_by_position_and_waits_for_partial_lines(self):
        r = self.C.newReader("unused")
        recs = self.C.readerFeed(r, "session\t0\t{}\naction\t5\t{[\"stamp\"]=1}\nclo")
        self.assertEqual([x.kind for x in recs.values()], ["session", "action"])
        self.assertEqual(recs[2]["from"], 5)
        self.assertEqual((int(recs[1].start), int(recs[1].after), int(recs[2].start)), (0, 13, 13), "each record knows its place")
        self.assertEqual(self.C.posText(self.C.readerPos(r)), "1:%d" % len("session\t0\t{}\naction\t5\t{[\"stamp\"]=1}\n"), "the partial line is not consumed")
        recs = self.C.readerFeed(r, "ck\t0\t{}\r\n")
        self.assertEqual([x.kind for x in recs.values()], ["clock"])
        # the same lines again are new lines: nothing is dropped for what it says
        recs = self.C.readerFeed(r, "session\t0\t{}\nsession\t0\t{}\n")
        self.assertEqual([x.kind for x in recs.values()], ["session", "session"])
        self.assertEqual(int(r.dropped), 0)
        recs = self.C.readerFeed(r, "garbage without tabs\n")
        self.assertEqual((len(recs), int(r.dropped)), (0, 1))

    def test_a_writer_made_again_goes_on_at_the_end_of_the_file(self):
        tmp = tempfile.mkdtemp(prefix="feverlink3-")
        try:
            p = os.path.join(tmp, "from_game.log")
            w = self.C.newWriter(p, 3)
            self.assertTrue(self.C.writerSend(w, "hello", self.L.table_from({"time": 1})))
            self.assertTrue(self.C.writerSend(w, "clock", self.L.table_from({"t": 1})))
            w2 = self.C.newWriter(p, 3)
            self.C.writerSend(w2, "clock", self.L.table_from({"t": 2}))
            lines = read_text(p).splitlines()
            self.assertEqual([l.split(TAB)[0] for l in lines], ["hello", "clock", "clock"])
            self.assertEqual(lines[2], 'clock\t3\t{["t"]=2}')
        finally:
            shutil.rmtree(tmp, ignore_errors=True)
    def test_pace_and_stamps(self):
        K, L = self.K, self.L
        s = K.newSession()
        peers = L.table()
        self.assertEqual(K.pace(s, peers, 1000, 3), 0, "not started: hold")
        K.applySession(s, L.table_from({"started": True, "speed": 2, "lead": 1200}))
        self.assertEqual(K.pace(s, peers, 1000, 3), 2)
        K.applyPeerClock(peers, L.table_from({"from": 1, "t": 1000, "ahead": 3}), 0)
        self.assertEqual(K.pace(s, peers, 1400, 3), 1, "one step of room left: run one step")
        self.assertEqual(K.pace(s, peers, 1600, 3), 0, "at the barrier: hold")
        self.assertEqual(K.pace(s, peers, 1800, 3), 0, "past the barrier: hold")
        K.applyPeerClock(peers, L.table_from({"from": 0, "t": 0, "ahead": 3}), 0)
        self.assertEqual(K.pace(s, peers, 1400, 3), 1, "own clock relayed back is ignored")
        K.applySession(s, L.table_from({"started": True, "speed": 4, "pauseAt": 1400, "lead": 1200}))
        self.assertEqual(K.pace(s, peers, 1000, 3), 2, "batch shortened to land on the pause")
        self.assertEqual(K.pace(s, peers, 1400, 3), 0)
        self.assertEqual(K.stampFor(s, 1400), 1400, "stopped at the pause: the stamp is the pause time")
        self.assertEqual(K.stampFor(s, 1000), 2200)
        self.assertEqual(K.stampFor(s, 1050), 2400, "rounded up to a step")
        self.assertEqual(K.stampFor(s, 1000, 2000), 3000, "a game may raise the lead")
        self.assertEqual(K.stampFor(s, 1000, 400), 2200, "never lower it")

    def test_measured_stamp_margin_grows_at_once_and_shrinks_after_a_window(self):
        K, L = self.K, self.L
        m = K.newMargin()
        self.assertEqual(K.marginNeed(m, 1), 4, "nothing measured at x1: aheadFor(1) + 1")
        self.assertEqual(K.marginNeed(m, 3), 6, "x3: aheadFor(3) + 1")
        self.assertEqual(K.marginTarget(1), 3)
        self.assertEqual(K.marginTarget(3), 5)
        # x1, stamped with 4 extra, arrived 7 steps early: 4 + 3 - 7 = 0 would have done; kept until the window is full and old
        for i, wall in enumerate([0.0, 2.0, 4.0]):
            sample, slack, need, how = K.marginSample(m, 1, 4, 10000, 10000 - 7 * 200, wall)
            self.assertEqual((sample, slack, need, how), (0, 7, 4, "keep"), i)
        sample, slack, need, how = K.marginSample(m, 1, 4, 10000, 10000 - 6 * 200, 6.0)
        self.assertEqual((sample, need, how), (1, 4, "keep"), "four builds but only 6 s: not yet")
        sample, slack, need, how = K.marginSample(m, 1, 4, 10000, 10000 - 7 * 200, 9.0)
        self.assertEqual((need, how), (2, "shrink"), "five builds over 9 s: the largest sample of the window plus one step of headroom")
        # a build that arrives with too little slack: the need grows at once
        sample, slack, need, how = K.marginSample(m, 1, 1, 10000, 10000 - 1 * 200, 10.0)
        self.assertEqual((sample, slack, need, how), (3, 1, 4, "grow"), "the sample plus the headroom, at once")
        # a late apply grows it by the lateness plus two at once
        self.assertEqual(K.marginLate(m, 1, 2), 8)
        self.assertEqual(K.marginNeed(m, 3), 6, "each speed has its own need")
        # never negative, never above the maximum
        sample, _, _, _ = K.marginSample(m, 3, 0, 10000, 10000 - 40 * 200, 0.0)
        self.assertEqual(sample, 0)
        sample, _, need, _ = K.marginSample(m, 3, 24, 10000, 10000 + 10 * 200, 0.0)
        self.assertEqual((sample, need), (K.MARGIN_MAX, K.MARGIN_MAX))

    def test_measured_stamp_margin_uses_the_largest_need_of_the_peers(self):
        K, L = self.K, self.L
        peers = L.table()
        self.assertEqual(K.marginExtra(peers, 1), 0, "no peer: nobody to wait for")
        K.applyPeerClock(peers, L.table_from({"from": 1, "t": 1000, "ahead": 3}), 0)
        self.assertEqual(K.marginExtra(peers, 1), 4, "a peer without a measurement counts as the default")
        K.applyPeerClock(peers, L.table_from({"from": 1, "t": 1200, "ahead": 3, "need": 1}), 0)
        self.assertEqual(K.marginExtra(peers, 1), 1)
        K.applyPeerClock(peers, L.table_from({"from": 2, "t": 1200, "ahead": 3, "need": 2}), 0)
        self.assertEqual(K.marginExtra(peers, 1), 2)
        K.applyPeerClock(peers, L.table_from({"from": 2, "t": 1400, "ahead": 3, "need": 99}), 0)
        self.assertEqual(K.marginExtra(peers, 1), K.MARGIN_MAX, "clamped")
        K.applyPeerClock(peers, L.table_from({"from": 2, "t": 1400, "ahead": 3, "need": -3}), 0)
        self.assertEqual(K.marginExtra(peers, 1), 1)

    def test_the_test_margin_switches_fix_the_extra_and_change_nothing_unset(self):
        K, L = self.K, self.L
        self.assertIsNone(K.testMargin(None, None), "unset: nothing changes")
        self.assertIsNone(K.testMargin("0", "0"))
        self.assertIsNone(K.testMargin("extra:x", None))
        t = K.testMargin("1", None)
        self.assertEqual((t.extra, t.noLead), (0, None))
        self.assertEqual(K.testMargin("extra:3", None).extra, 3)
        self.assertEqual(K.testMargin("extra:99", None).extra, K.MARGIN_MAX)
        t = K.testMargin(None, "1")
        self.assertEqual((t.extra, t.noLead), (0, True), "no lead alone: no extra either")
        peers = L.table()
        self.assertEqual(K.marginExtra(peers, 3, K.testMargin("extra:2", None)), 0, "no peer: nobody to wait for")
        K.applyPeerClock(peers, L.table_from({"from": 1, "t": 1000, "ahead": 3, "need": 9}), 0)
        self.assertEqual(K.marginExtra(peers, 3, K.testMargin("1", None)), 0, "the measured need is ignored")
        self.assertEqual(K.marginExtra(peers, 3, K.testMargin("extra:2", None)), 2)
        self.assertEqual(K.marginExtra(peers, 3, None), 9, "without the switch: the measured need")

    def test_pace_lands_batches_on_checksum_stamps(self):
        K, L = self.K, self.L
        s = K.newSession()
        peers = L.table()
        K.applySession(s, L.table_from({"started": True, "speed": 4, "lead": 1200}))
        marks = L.table()
        self.assertEqual(K.pace(s, peers, 1000, 6, marks), 4, "no mark: full batch")
        K.addMark(marks, 2000, 1000)
        self.assertEqual(K.pace(s, peers, 1000, 6, marks), 4, "5 steps of room: full batch")
        self.assertEqual(K.pace(s, peers, 1400, 6, marks), 3, "batch shortened to land exactly on the stamp")
        self.assertEqual(K.pace(s, peers, 1800, 6, marks), 1)
        self.assertEqual(K.pace(s, peers, 2000, 6, marks), 4, "at the stamp: the mark no longer limits")
        K.addMark(marks, 3000, 2000)
        self.assertEqual(len(marks), 1, "passed marks are dropped")
        # a mark never holds the game, and a pause still wins
        K.applySession(s, L.table_from({"started": True, "speed": 4, "pauseAt": 2200, "lead": 1200}))
        self.assertEqual(K.pace(s, peers, 2000, 6, marks), 1)
        self.assertEqual(K.pace(s, peers, 2200, 6, marks), 0)
        # two games at different phases reach the same time at the stamp
        for start in (1000, 1200, 1600):
            now, sp = start, 4
            m = L.table()
            K.addMark(m, 2400, now)
            K.applySession(s, L.table_from({"started": True, "speed": sp, "lead": 1200}))
            while now < 2400:
                now += int(K.pace(s, peers, now, 6, m)) * 200
            self.assertEqual(now, 2400)

    def test_queue_order_and_late(self):
        K, L = self.K, self.L
        q = L.table()
        for at, frm, seq, kind in [(600, 2, 1, "action"), (600, 1, 9, "action"), (400, 3, 1, "action"), (600, 0, 0, "checksum"), (800, 1, 1, "action"), (600, 1, 2, "action")]:
            K.enqueue(q, L.table_from({"kind": kind, "at": at, "from": frm, "seq": seq}))
        due = K.takeDue(q, 600)
        order = [(x.kind, int(x.at), int(x["from"]), int(x.seq), int(x.lateSteps)) for x in due.values()]
        self.assertEqual(order, [("action", 400, 3, 1, 1), ("checksum", 600, 0, 0, 0), ("action", 600, 1, 2, 0), ("action", 600, 1, 9, 0), ("action", 600, 2, 1, 0)])
        self.assertEqual(len(q), 1)
        self.assertEqual(int(q[1].at), 800)

    def test_random_is_deterministic_per_step(self):
        K, L = self.K, self.L
        rng1 = K.newRandom(L.eval("function() return %d end" % 1000))
        rng2 = K.newRandom(L.eval("function() return %d end" % 1000))
        seq1 = [K.randomDraw(rng1) for _ in range(5)] + [K.randomDraw(rng1, 6) for _ in range(5)] + [K.randomDraw(rng1, 10, 20) for _ in range(5)]
        for _ in range(3):
            K.randomDraw(rng2)   # a game that drew more earlier...
        rng2.t = None            # ...is reseeded by the step change
        seq2 = [K.randomDraw(rng2) for _ in range(5)] + [K.randomDraw(rng2, 6) for _ in range(5)] + [K.randomDraw(rng2, 10, 20) for _ in range(5)]
        self.assertEqual(seq1, seq2)
        self.assertTrue(all(0 <= x < 1 for x in seq1[:5]))
        self.assertTrue(all(1 <= x <= 6 and x == int(x) for x in seq1[5:10]))
        self.assertTrue(all(10 <= x <= 20 for x in seq1[10:]))
        self.assertGreater(len(set(seq1[:5])), 3)
        rng3 = K.newRandom(L.eval("function() return 1200 end"))
        self.assertNotEqual(K.randomDraw(rng3), seq1[0], "another step draws other numbers")

    def test_permissions_categories_and_authority_pure(self):
        K, L = self.K, self.L
        self.assertEqual([K.categoryOf(n) for n in ("makeVehicleBuyCmd", "makeLineCreateCmd", "makeVehicleSetLineCmd")], ["spend", "build", "other"])
        self.assertEqual([K.categoryOf(n) for n in ("makeBulldozeProposalCmd", "makeRemoveEntityCmd", "makeConstructionCmd", "makeBuyVehicle", "makeSetNameCmd")],
                         ["bulldoze", "bulldoze", "build", "spend", "other"])
        perms = K.newPermissions()
        self.assertTrue(all(K.permitted(perms, c) for c in ("build", "bulldoze", "spend", "other")))
        self.assertTrue(K.applyPermissions(perms, L.table_from({"build": False, "other": "no", "junk": False})))
        self.assertFalse(K.permitted(perms, "build"))
        self.assertTrue(K.permitted(perms, "other"), "only an explicit boolean changes a grant")
        self.assertFalse(K.applyPermissions(perms, L.table_from({"build": False})), "no change reported when nothing changed")
        self.assertTrue(K.applyPermissions(perms, L.table_from({"build": True})))
        self.assertTrue(K.permitted(perms, "build"))
        d = K.authorityDeltas(L.table_from({"balance": 100.4, "loan": 50}), L.table_from({"balance": 200, "loan": 50}))
        self.assertEqual((d.balance, d.loan), (100, None))
        booked = []
        eng = L.table_from({"bookJournal": L.eval("function(a) BOOKED = a end"), "adjustLoan": None})
        own, host = L.table_from({"balance": 1, "loan": 5}), L.table_from({"balance": 11, "loan": 9})
        applied, failed, measured = K.applyAuthority(eng, own, host)
        self.assertEqual((applied.balance, applied.loan, len(list(failed.values()))), (None, None, 0), "a first round only measures")
        self.assertEqual((measured.balance, measured.loan), (10, 4))
        applied, failed, measured = K.applyAuthority(eng, own, host, measured)
        self.assertEqual(applied.balance, 10)
        self.assertEqual(list(failed.values()), ["loan"], "a build without a loan accessor reports it cannot correct the loan")
        applied, failed, _ = K.applyAuthority(eng, own, host, L.table_from({"balance": 200, "loan": 4}))
        self.assertEqual(applied.balance, None, "a difference that changed since the round before is not the same one")

    def test_hud_model(self):
        H, L = self.H, self.L
        h = H.new(1, "me")
        self.assertEqual(H.statusText(h), "Waiting for the host to start - 1 player")
        H.setSession(h, L.table_from({"started": True, "speed": 1}))
        H.playerJoined(h, L.table_from({"player": L.table_from({"id": 0, "name": "boss", "ready": True})}))
        self.assertEqual(H.statusText(h), "Running x1 - 2 players")
        self.assertEqual(H.rosterText(h), "boss [host], me (you)")
        H.setSession(h, L.table_from({"started": True, "speed": 2, "pauseAt": 4000}))
        self.assertTrue(H.statusText(h).startswith("Paused"))
        v = h.version
        H.setSession(h, L.table_from({"started": True, "speed": 2, "pauseAt": 4000}))
        self.assertEqual(h.version, v, "an unchanged session does not bump the version")
        H.playerLeft(h, L.table_from({"id": 1, "name": "me"}))
        self.assertEqual(H.rosterText(h), "boss [host], me (you)", "a game never removes itself")
        H.playerLeft(h, L.table_from({"id": 0, "name": "boss"}))
        self.assertEqual(H.rosterText(h), "me (you)")
        self.assertEqual(H.cleanText("  a\tb\nc  "), "a b c")
        self.assertIsNone(H.cleanText("   "))
        self.assertIsNone(H.cleanText(5))
        self.assertEqual(len(H.cleanText("x" * 900)), 500)

    def test_hud_history_is_bounded_and_snapshot_round_trips(self):
        H, L = self.H, self.L
        h = H.new(2, "me")
        for i in range(1, 251):
            H.addChat(h, "bob", "m%d" % i, 3)
        self.assertEqual(len(h.history), 200)
        self.assertEqual(h.history[1].text, "m51")
        self.assertEqual(h.history[200].text, "m250")
        self.assertEqual(H.chatLines(h, 2)[2], "bob: m250")
        self.assertFalse(H.addChat(h, "bob", "  ", 3))
        snap = H.snapshot(h)
        back = H.fromSnapshot(self.C.parse(self.C.serialize(snap)))
        self.assertEqual(back.version, h.version)
        self.assertEqual(len(back.history), 200)
        self.assertEqual(back.history[200].text, "m250")
        self.assertEqual(H.rosterText(back), "me (you)")
        self.assertIsNone(H.fromSnapshot(L.table_from({"version": 1})))

    def test_encode_decode_commands(self):
        K, L = self.K, self.L
        rev = L.table_from({50001: "1:1"})
        ctx = L.table_from({"plain": self.C.plain, "keyOf": L.eval("function(e) return (%s)[e] or ('s:' .. e) end" % "({[50001]='1:1'})")})
        args = L.eval("{ n = 3, 7, 55, { vehicles = {} } }")
        body = K.encodeCommand("makeVehicleBuyCmd", args, ctx)
        self.assertEqual(self.C.serialize(body), '{["args"]={[1]={["__ref"]="player"},[2]={["__ref"]="entity",["key"]="s:55"},[3]={["vehicles"]={}},["n"]=3},["fn"]="makeVehicleBuyCmd"}')
        body2 = K.encodeCommand("makeVehicleSetLineCmd", L.eval("{ n = 3, 50001, 60, 0 }"), ctx)
        self.assertIn('"1:1"', self.C.serialize(body2))
        dctx = L.table_from({"player": 7, "resolve": L.eval("function(k) if k == '1:1' then return 70001 end if k == 's:60' then return 60 end end")})
        out = K.decodeCommand(body2, dctx)
        self.assertEqual([int(out[i]) for i in (1, 2, 3)], [70001, 60, 0])
        out, err = K.decodeCommand(K.encodeCommand("makeVehicleSetLineCmd", L.eval("{ n = 3, 99, 60, 0 }"), ctx), dctx)
        self.assertIsNone(out)
        self.assertIn("unresolved", err)
        res, err = K.encodeCommand("makeVehicleBuyCmd", L.eval("{ n = 3, 7, 55, { f = print } }"), ctx)
        self.assertIsNone(res)
        self.assertIn("cannot be replicated", err)
        res, err = K.encodeCommand("makeTerrainCmd", L.eval("{ n = 0 }"), ctx)
        self.assertIsNone(res)


if __name__ == "__main__":
    unittest.main(verbosity=2)
