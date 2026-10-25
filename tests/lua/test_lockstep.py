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

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)  # python -I does not put the script's folder on the path

import fastio  # noqa: E402  (the read cache of the link logs: tests/lua/fastio.py says why)

# The build module only refuses remote constructions with FEVERLINK3_REMOTE_CONSTRUCTIONS=0 (a developer switch);
# tests of the block set it themselves.
os.environ.pop("FEVERLINK3_REMOTE_CONSTRUCTIONS", None)

# The default run (and CI, and the release workflow) is the essential set; FEVERLINK3_FULL_TESTS=1 adds the long simulations and the
# wider variants (`@full`), run locally on demand (docs/RELEASING.md).
FULL = os.environ.get("FEVERLINK3_FULL_TESTS") == "1"
full = unittest.skipUnless(FULL, "full set only (FEVERLINK3_FULL_TESTS=1)")


def own_tests_only(cls):
    """Class decorator: a subclass made for its parent's harness (a switch, another engine shape) runs only the tests it defines
    itself, not every inherited test a second time with the same path and the same assertions."""
    for name in dir(cls):
        if name.startswith("test") and name not in cls.__dict__ and callable(getattr(cls, name, None)):
            setattr(cls, name, None)
    return cls
CONTENT = os.path.join(HERE, "..", "..", "mod", "feverlink3", "content")
TAB = "\t"
STEP = 200
T0 = 18600


def lua_str(text):
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"') + '"'


def read_text(path):
    if not os.path.exists(path):
        return ""
    if path.endswith(".log"):
        return fastio.read_text(path)
    with open(path, encoding="utf-8") as f:
        return f.read()


def read_src(name):
    return read_text(os.path.join(CONTENT, name))


COMMON_SRC = read_src("feverlink3_common.lua")
LOCKSTEP_SRC = read_src("feverlink3_lockstep.lua")
BRIDGE_SRC = read_src("feverlink3_bridge.script.lua")
HUD_SRC = read_src("feverlink3_hud.lua")
TEXT_SRC = read_src("feverlink3_text.lua")
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
    -- FREE_IDS (a test's): ids the engine hands out again before new ones, first freed first (the real engine reuses freed ids, so
    -- a later command's entity can have the smaller id). SYNC_CREATE (a test's): the vehicle exists as the command returns, as a
    -- build's entities do in the real simulation state
    if FREE_IDS and #FREE_IDS > 0 then
      created = table.remove(FREE_IDS, 1)
    else
      GAME.nextEntity = GAME.nextEntity + 1
      created = GAME.nextEntity
    end
    if GAME.inUpdate and not SYNC_CREATE then
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
      -- GUI_THROWS (a test's): the interface's sendCommand throws for that command after it took it (D, the pause path)
      if GUI_THROWS and not GAME.inUpdate and c and c.__cmd == GUI_THROWS then error("Unknown exception") end
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
-- like the game's interface state (owner and friend tests 2026-10-18): no subscribe method at all; guiHandleEvent receives what the
-- script's one subscription (made in the simulation state, SUBS) covers
GSTATE = {}

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
    if p:find("feverlink3_text") then return assert(load(TEXT_SRC, "=text", "t", env))() end
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

-- More than one simulation Lua state (a test's, SIM_SPLIT): the real game runs the game scripts in several Lua states and shares the
-- steps of one game between them (owner's live test 2026-10-18: two states installed the session's math.random at the same game time;
-- a spread round begun in one of them read its chunks only at two steps in four, t = 0 and 1 mod 4 steps, while an action was applied
-- at its stamp at t = 3 mod 4 by the other). SIM_SPLIT(t) names the state that runs the step at game time t (1: SIM, 2: SIM2, 0: none
-- runs the script at that step). Each state has its own module tables; they share only the game-script state (STATE). SIM_RUNS counts
-- the steps each state ran.
-- The events (handleEvent) are shared out the same way: every simulation state receives events (real game: both states that ran
-- steps logged their first event "at handleEvent", lab K 2026-10-19 00:17:33 and friend test 2026-10-18 22:01:02 / 22:01:29), and each event
-- reaches ONE of them (friend test 2026-10-18, host: 136 stamped actions came in "queue" events, none applied twice). Which one the game picks
-- is not documented; the model: the state that runs the engine step in which the command is executed. An interface state's command
-- "is executed in the next simulation step" (tf3-docs api/cmd.html), so an event sent from there goes to the state of the next step; an
-- engine event during a step (a build's apply_command) to the state running it. Without SIM_SPLIT every event goes to SIM, as before.
SIM2, SIM2_ENV, SIM_SPLIT, SIM_RUNS, SIM_CUR = nil, nil, nil, { [0] = 0, 0, 0 }, nil
function SPLIT_SIM(fn)
  if not SIM2 then SIM2, SIM2_ENV = LOAD_HALF("sim2") end
  SIM_SPLIT = fn
end
function SIM_FOR_STEP(t)
  local k = SIM_SPLIT and SIM_SPLIT(t) or 1
  SIM_RUNS[k] = (SIM_RUNS[k] or 0) + 1
  if k == 2 then return SIM2 end
  if k == 0 then return nil end
  return SIM
end
-- the state an event reaches (see above): the one running the current step, else the one that runs the next step
function SIM_FOR_EVENT()
  if SIM_CUR then return SIM_CUR end
  local k = SIM_SPLIT and SIM_SPLIT(GAME.time + 200) or 1
  if k == 2 and SIM2 then return SIM2 end
  return SIM
end

-- a savegame reload: the scripts start again (fresh Lua states), the world and the session folder stay
function RELOAD()
  RELOAD_PENDING = false
  GAME.guiQueue = {}
  -- the loaded game waits for app.startReadyGame when the wait was asked for before (or at) the load
  GAME.waitReady, GAME.waitNext = GAME.waitNext == true, nil
  GAME.held = GAME.waitReady
  SIM, SIM_ENV = LOAD_HALF("sim")
  if SIM2 then SIM2, SIM2_ENV = LOAD_HALF("sim2") end
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
    if p:find("feverlink3_text") then return assert(load(TEXT_SRC, "=text", "t", env))() end
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
    if p:find("feverlink3_text") then return assert(load(TEXT_SRC, "=text", "t", env))() end
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
  SIM_FOR_EVENT().handleEvent(nil, STATE, src, id, name, param)
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
    local S = SIM_FOR_STEP(GAME.time)
    SIM_CUR = S
    local res = S and S.update(nil, STATE, 0.2)
    if #GAME.applied ~= applied0 then UPDATE_WRITES[#UPDATE_WRITES + 1] = "engine command applied in update" end
    if OTHER_UPDATE then OTHER_UPDATE(GAME.time) end   -- another game script's update in the same step (a test's own)
    -- POST_MODE (a test's): "never" calls no postUpdate, "alternate" every other step (the bridge's fallback applies the step in update)
    GAME.stepN = (GAME.stepN or 0) + 1
    if S and not (POST_MODE == "never" or (POST_MODE == "alternate" and GAME.stepN % 2 == 0)) then
      GAME.phase = "postUpdate"
      S.postUpdate(nil, STATE, 0.2, res)
    end
    GAME.phase = nil
    GAME.inUpdate = false
    SIM_CUR = nil
  end
end
function APPLIED()
  local r = {}
  for i, a in ipairs(GAME.applied) do r[i] = a.name .. "@" .. a.time end
  return table.concat(r, ",")
end
'''
# The mod's files are compiled once per test run, not once per mocked game (fastio.bytecode): the same functions, loaded from
# their bytecode into each game's environment.
HARNESS = fastio.LOAD_SRC_LUA + re.sub(r"\bload\((\w+_SRC\b|CMDS_SRC\[cmds\])", r"LOAD_SRC(\1", HARNESS)


class Game:
    def __init__(self, label, player_id, tmp, first_entity):
        self.label, self.id = label, player_id
        self.dir = os.path.join(tmp, label)
        os.makedirs(self.dir)
        self.to_game = os.path.join(self.dir, "to_game.log")
        self.from_game = os.path.join(self.dir, "from_game.log")
        self.use_env()
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        fastio.install(self.L)
        g = self.L.globals()
        g.COMMON_SRC, g.LOCKSTEP_SRC, g.BRIDGE_SRC, g.HUD_SRC = COMMON_SRC, LOCKSTEP_SRC, BRIDGE_SRC, HUD_SRC
        g.HOOK_SRC, g.TEXT_SRC = HOOK_SRC, TEXT_SRC
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

    def split_sim(self, lua_fn):
        """Runs the steps in two simulation Lua states: `lua_fn` (Lua source, t -> 1, 2 or 0) names the state of each step (SIM_SPLIT)."""
        self.use_env()
        self.L.execute("SPLIT_SIM(%s)" % lua_fn)

    def sim_runs(self):
        return {k: int(self.L.eval("SIM_RUNS[%d] or 0" % k)) for k in (0, 1, 2)}

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

    def frames_to_load(self):
        """After a resync_load line: the frame that reads it asks the simulation half for this world's detail dump (X.syncDetail), the
        engine runs that event (without a step: the game stands still), the next frame loads."""
        self.frame()
        self.L.execute("local q = GAME.guiQueue; GAME.guiQueue = {}; for _, c in ipairs(q) do EXECUTE(c[1], c[2]) end")
        self.frame()

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
        fastio.install(self.L)
        self.C = load_common(self.L)
        self.readers = {g.id: self.C.newReader(g.from_game) for g in games}
        self.seq = {g.id: 0 for g in games}
        self.history = {g.id: [] for g in games}
        self.clocks, self.aheads, self.hello, self.reports, self.speed_requests = {}, {}, {}, {}, []
        self.costs = {}
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
        fastio.CACHE.append(g.to_game, line.encode("utf-8"))

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
            if not (kind == "action" and p.applied):   # host.rs: one its sender's engine applied already keeps its stamp
                p["stamp"] = stamp if self.pauseAt is not None else max(stamp, int(p.stamp))
            p["pauseSeq"] = self.pause_seq
            if kind == "build_commit":
                self.commits.append((g.id, int(p.owner), int(p.seq), int(p.stamp)))
            payload = self.C.serialize(p)
            for o in self.games:
                self.send(o, kind, g.id if kind == "action" else 0, payload)
            if self.pauseAt is not None:
                self.pause_last = max(self.pause_last or int(p.stamp), int(p.stamp))

    def future_stamp(self):
        t = max(self.clocks.values())
        # as host.rs: never before a promise a game has made, so no game has passed the stamp (the input barrier)
        return max([-(-(t + self.lead) // STEP) * STEP] + list(self.safes.values()))

    def checksum_request(self, n, at, paused, full=None, spread=None, chunk=None, detail=False):
        body = {"n": n, "at": at, "paused": paused}
        if full is not None:
            body["full"] = full   # absent means a full round
        if spread is not None:
            body["spread"] = spread   # absent: the round is hashed whole at its stamp
        if chunk:
            body["chunk"] = chunk   # host.rs: sized from the games' cost (ChecksumPolicy::chunk_for); absent: the game's 200
        if detail:
            body["detail"] = True   # host.rs: after a round that differed, the round keeps its itemised keys in the game-script state
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
        fastio.CACHE.write(g.to_game, "".join(self.history[g.id][drop:]).encode("utf-8"))

    def verdict(self, g, p, applied):
        """core VerdictBook / host on_action_verdict: verdicts of one action that differ between games ask for a resync at once;
        verdicts that agree (all refused alike, all applied) ask for nothing."""
        key = (int(p["from"]) if p["from"] is not None else 0, int(p.seq))
        by = self.verdicts.setdefault(key, {})
        by[g.id] = (applied, str(p.detail or ""))
        n_applied = sum(1 for a, _ in by.values() if a)
        results = set(d for a, d in by.values() if a)   # an applied verdict's result (a terrain edit's height proof): two differ = a fork
        if 0 < n_applied < len(by) or len(results) > 1:
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
                    # the host fills in the sender and relays to everyone but the origin, as a line from 0; a notice keeps its level
                    msg = {"text": str(p.text), "from": g.id, "name": g.label}
                    if p.level in ("info", "warn", "error"):
                        msg["level"] = str(p.level)
                    out = self.ser(msg)
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
                    if p.cost is not None:   # core ChecksumCost: what a spread round cost this game
                        self.costs.setdefault(int(p.n), {})[g.id] = {k: int(v) for k, v in p.cost.items()}
                    # the parts the game could not hash this round (core ChecksumReportMessage.not_ready)
                    self.not_ready.setdefault(int(p.n), {})[g.id] = sorted(p.notReady.values()) if p.notReady is not None else []
                    if g.id == 0 and p.authority is not None:
                        self.authority[int(p.n)] = {k: v for k, v in p.authority.items()}
                elif rec.kind == "speed_request":
                    self.speed_requests.append(int(p.speed))
            self.flush_pause()


# The bridge keeps its state in chunk locals: the tests reach them through the upvalues of its entry points (FIND_UP("G"), "X", ...).
FIND_UP = r'''
function FIND_UP(name)
  local seen = {}
  local function walk(f, d)
    if seen[f] or d > 8 then return nil end
    seen[f] = true
    local i = 1
    while true do
      local n, v = debug.getupvalue(f, i)
      if n == nil then break end
      if n == name then return v end
      if type(v) == "function" then
        local r = walk(v, d + 1)
        if r ~= nil then return r end
      end
      i = i + 1
    end
    return nil
  end
  return walk(GUI.guiUpdate, 0)
end
'''

BUY = 'api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function(d, s, e) %s = { s, d and d.resultVehicleEntity } end)'
LINE = 'api.cmd.sendCommand(api.cmd.makeLineCreateCmd("%s", { x = 0, y = 0, z = 0 }, 7, { stops = {} }), function(d, s, e) %s = e and e[1] and e[1][1] end)'
BUY_BODY_TEXT = '{["fn"]="makeVehicleBuyCmd",["args"]={["n"]=3,[1]={["__ref"]="player"},[2]={["__ref"]="entity",["key"]="s:55"},[3]={}}}'


class LockstepTest(unittest.TestCase):
    """Two games whose session entity ids differ (the client allocates 1000 higher). setUp/tearDown/tick/start/log_has are the harness
    of other test files too."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-")
        self.A = Game("host", 0, self.tmp, 50000)
        self.B = Game("client", 1, self.tmp, 51000)
        self.relay = Relay([self.A, self.B])

    def tearDown(self):
        if os.environ.get("FEVERLINK3_TEST_VERBOSE"):
            print("\n--- host mod.log ---\n" + self.A.log() + "\n--- client mod.log ---\n" + self.B.log())
        shutil.rmtree(self.tmp, ignore_errors=True)

    def again(self):
        """Fresh games for the next case of a table-driven test."""
        self.tearDown()
        self.setUp()

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

    def shipped(self, g, kind="action"):
        """g's own lines of that kind to the launcher (from_game.log), parsed."""
        out = []
        for line in g.lines(g.from_game):
            parts = line.split(TAB, 2)
            if len(parts) == 3 and parts[0] == kind:
                p = self.relay.C.parse(parts[2])
                out.append(p[0] if isinstance(p, tuple) else p)
        return out

    def update_writes(self, g):
        return list(g.L.eval("table.concat(UPDATE_WRITES, ';')").split(";")) if int(g.L.eval("#UPDATE_WRITES")) else []

    # ---- the session's core path

    def test_two_games_hold_run_within_ahead_and_apply_a_clients_actions_at_one_stamp(self):
        self.tick(15)
        self.assertEqual((self.A.time(), self.B.time()), (T0, T0), "both games hold at the save time before the start")
        self.assertEqual((self.A.speed(), self.B.speed()), (0, 0))
        self.assertEqual(self.relay.hello, {0: T0, 1: T0}, "both games said hello with the save time")
        self.start()
        self.tick(30, lag_b=True)
        gaps = []
        for _ in range(30):
            self.tick(1, lag_b=True)
            gaps.append((self.A.time() - self.B.time()) // STEP)
        ahead = self.relay.aheads[0]
        self.assertTrue(all(-ahead <= g <= ahead for g in gaps), "gaps in steps %s exceed ahead=%d" % (gaps, ahead))
        self.assertGreater(max(gaps), 0, "the client did lag behind at some point: %s" % gaps)
        # a vehicle, a line, the vehicle put on the line: each reference translated to the host's own ids
        self.B.send_gui(BUY % "BUY_CB")
        self.assertTrue(self.B.L.eval("BUY_CB == nil"), "the depot window waits for the real result")
        self.tick(40, lag_b=True)
        a, b = self.A.applied(), self.B.applied()
        self.assertEqual(len(a), 1, a)
        self.assertEqual(a, b, "same action at the same stamp on both games")
        va, vb = self.A.L.eval("GAME.applied[1].entity"), self.B.L.eval("GAME.applied[1].entity")
        self.assertNotEqual(va, vb, "the vehicle has different ids in the two games")
        self.assertTrue(self.B.L.eval("BUY_CB[1] == true and BUY_CB[2] == %d" % vb), "the client's callback got its local vehicle id")
        shipped = [l for l in self.B.lines(self.B.from_game) if l.split(TAB)[0] == "action"]
        self.assertEqual(len(shipped), 1)
        self.assertIn('s:55', shipped[0], "the depot travels as a savegame key")
        self.assertNotIn(str(vb), shipped[0])
        self.assertEqual(int(a[0].split("@")[1]), int(self.shipped(self.B)[0].stamp), "applied in postUpdate at exactly its stamp")
        self.B.send_gui(LINE % ("Line 1", "LINE_CB"))
        self.tick(40, lag_b=True)
        lb, la = self.B.L.eval("LINE_CB"), self.A.L.eval("GAME.applied[2].entity")
        self.assertTrue(lb and la and lb != la)
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleSetLineCmd(%d, %d, 0), function(d, s) SET_CB = s end)" % (vb, lb))
        self.tick(40, lag_b=True)
        self.assertEqual(self.A.L.eval("GAME.applied[3].args[1] .. ',' .. GAME.applied[3].args[2]"), "%d,%d" % (va, la), "host ids")
        self.assertEqual(self.B.L.eval("GAME.applied[3].args[1] .. ',' .. GAME.applied[3].args[2]"), "%d,%d" % (vb, lb), "client ids")
        self.assertEqual(self.A.applied(), self.B.applied())
        self.assertTrue(self.B.L.eval("SET_CB == true"))
        self.assertFalse(self.log_has("LATE"), "no late action")
        for g in (self.A, self.B):
            # the wiki (modding:scripting:gamescripts): update "may not write the state itself"; postUpdate runs after every update
            self.assertEqual(self.update_writes(g), [], "%s: nothing written or sent in update()" % g.label)
            for bad in ("postUpdate was not called", "postUpdate missing", "update fallback failed", "postUpdate received no result",
                        "postUpdate runs at t=", "postUpdate failed"):
                self.assertNotIn(bad, g.log())
            # the game has ONE event subscription per script, made in the simulation state; its interface state has no subscribe
            # method (owner and friend tests 2026-10-18: a subscription by name cut the interface half off the build tools' events)
            self.assertTrue(g.L.eval("SUBS.all"), "the script's one subscription covers every event")
            self.assertFalse(g.L.eval("SUBS.any"), "no subscription by name")
            self.assertIn("events (sim, at", g.log())
            self.assertIn("events (gui, at the session's first frame): no subscription here", g.log())
            self.assertNotIn("SUBSCRIBING FAILED", g.log())
            self.assertEqual(g.L.eval("#SUBS.dropped"), 0)
        with open(os.path.join(CONTENT, "feverlink3_bridge.script.lua"), encoding="utf-8") as f:
            src = f.read()
        sent = set(re.findall(r'toSim\(\s*"(\w+)"', src)) | set(re.findall(r'toSim,\s*"(\w+)"', src))
        listed = set(re.search(r"X\.SIM_EVENTS = \{(.*?)\}", src, re.S).group(1).replace('"', " ").replace(",", " ").split())
        self.assertGreater(len(sent), 10)
        self.assertEqual(sorted(sent - listed), ["nop"], "every message to the simulation half is listed (but the strict mode's trigger)")
        # an action that arrives after its stamp is applied, and it is a fork, never silent: named, and the repair asked for
        payload = self.relay.ser({"stamp": self.A.time() - 2 * STEP, "seq": 7, "body": self.relay.C.serialize(self.relay.C.parse(BUY_BODY_TEXT)), "from": 1})
        self.relay.send(self.A, "action", 1, payload)
        self.tick(5)
        self.assertEqual(len(self.A.applied()), 4)
        self.assertIn("LATE", self.A.log())
        self.assertIn("INPUT BARRIER BREACHED: action 1:7", self.A.log())
        self.relay.pump()
        self.assertTrue(any("late apply of 1:7" in d for g, d in self.relay.replay_failed if g == 0), self.relay.replay_failed)
        # the detector itself: the step's work run inside the update phase (the fallback when no postUpdate comes) is caught
        self.B.L.execute('POST_MODE = "never"')
        self.B.step()
        self.B.step()
        self.assertIn("state:set", self.update_writes(self.B))

    def test_a_game_whose_engine_skips_postupdate_still_applies_every_step_once_at_its_stamp(self):
        for i, mode in enumerate(("never", "alternate")):
            with self.subTest(mode=mode):
                if i:
                    self.again()
                for g in (self.A, self.B):
                    g.L.execute('POST_MODE = "%s"' % mode)
                self.start()
                self.tick(10)
                self.B.send_gui(BUY % "BUY_CB")
                self.tick(40)
                self.B.send_gui(LINE % ("Line 1", "LINE_CB"))
                self.tick(40)
                vb, lb = self.B.L.eval("BUY_CB and BUY_CB[2]"), self.B.L.eval("LINE_CB")
                self.assertTrue(vb and lb, "the client's callbacks got their entities")
                self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleSetLineCmd(%d, %d, 0), function(d, s) SET_CB = s end)" % (vb, lb))
                self.tick(40)
                a, b = self.A.applied(), self.B.applied()
                stamps = [int(p.stamp) for p in self.shipped(self.B)]
                self.assertEqual([x.split("@")[0] for x in a], ["makeVehicleBuyCmd", "makeLineCreateCmd", "makeVehicleSetLineCmd"], "each once")
                self.assertEqual(a, b, "same actions, same steps, same order on both games")
                self.assertTrue(self.B.L.eval("SET_CB == true"))
                for got, stamp in zip([int(x.split("@")[1]) for x in a], stamps):
                    # never: applied in update at its stamp; alternate: a step caught up in the next update lands one step late, never twice
                    self.assertIn(got - stamp, (0,) if mode == "never" else (0, STEP))
                for g in (self.A, self.B):
                    self.assertNotIn("update fallback failed", g.log())
                    if mode == "never":
                        self.assertIn("postUpdate was not called after the update at t=", g.log())
                        self.assertIn("step(s) of this session applied in update instead", g.log())

    def test_an_action_a_game_may_not_apply_is_refused_and_a_denied_one_is_never_stamped(self):
        self.start()
        self.tick(10)
        # permissions: a denied category is refused before stamping, not even applied locally
        self.relay.send(self.B, "permissions", 0, self.relay.ser({"build": False, "spend": True}))
        self.tick(2)
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeLineCreateCmd("L", { x = 0, y = 0, z = 0 }, 7, { stops = {} }), function(d, s) LINE_OK = s end)')
        self.tick(30)
        self.assertTrue(self.B.L.eval("LINE_OK == false"), "the window got a failed result")
        self.assertEqual((self.A.applied(), self.B.applied()), ([], []))
        self.assertEqual(self.shipped(self.B), [], "nothing stamped or shipped")
        self.assertEqual(self.relay.refused, [(1, 0, "permission denied: build")])
        self.B.send_gui(BUY % "X")   # spending is still allowed
        self.tick(40)
        self.assertEqual(len(self.A.applied()), 1)
        self.assertEqual(self.A.applied(), self.B.applied())
        # revoked later, granted again later
        self.relay.send(self.B, "permissions", 0, self.relay.ser({"spend": False}))
        self.relay.send(self.B, "permissions", 0, self.relay.ser({"build": True}))
        self.tick(3)
        self.B.send_gui(BUY % "X")
        self.tick(30)
        self.assertEqual(len(self.A.applied()), 1, "the second purchase was refused")
        self.assertEqual(self.relay.refused[-1][2], "permission denied: spend")
        self.B.send_gui(LINE % ("L", "X"))
        self.tick(40)
        self.assertEqual(len(self.A.applied()), 2, "building works again")
        self.relay.send(self.B, "permissions", 0, self.relay.ser({"spend": True}))
        self.tick(3)
        # a reference the host cannot resolve: refused there, not applied
        self.B.L.execute("WORLD[9999] = { VEHICLE_DEPOT = {} }")   # a depot that exists in the client game only
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 9999, { vehicles = {} }), function(d, s) BAD_CB = s end)')
        self.tick(40)
        self.assertEqual(len(self.A.applied()), 2, "the host did not buy in a depot it does not have")
        self.assertEqual(len(self.B.applied()), 3)
        self.assertIn("refused", self.A.log())
        self.assertTrue(self.shipped(self.A, "action_refused"))
        # a relayed action its sender was not allowed to make is not applied and asks for a repair
        failed = len(self.relay.replay_failed)
        body = self.relay.C.serialize(self.relay.C.parse(BUY_BODY_TEXT))
        self.relay.send(self.A, "action", 1, self.relay.ser({"stamp": self.A.time() + 5 * STEP, "seq": 40, "body": body, "from": 1, "allowed": False}))
        self.tick(15)
        self.assertEqual(len(self.A.applied()), 2)
        self.assertEqual(len(self.relay.replay_failed), failed + 1)
        self.assertEqual(self.relay.replay_failed[-1][0], 0)
        self.assertIn("1:40", self.relay.replay_failed[-1][1])
        self.assertIn("allowed=false", self.A.log())

    def test_pause_stamp_stops_both_exactly_and_action_during_pause(self):
        self.start(speed=2)
        self.tick(10, lag_b=True)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(40, lag_b=True)
        self.assertEqual((self.A.time(), self.B.time()), (self.relay.pauseAt, self.relay.pauseAt), "both stopped at the pause stamp")
        self.assertEqual((self.A.speed(), self.B.speed()), (0, 0))
        self.A.send_gui(BUY % "BUY_CB")
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
        paused_at = self.relay.pauseAt
        self.relay.pauseAt, self.relay.speed = None, 2
        self.relay.session()
        self.tick(20)
        self.assertGreater(self.A.time(), paused_at)
        self.assertEqual(self.A.speed(), 2)
        # the vehicle bought during the pause is known to the simulation half too: assign it to a new line
        self.A.send_gui(LINE % ("L", "LINE_CB"))
        self.tick(30)
        self.A.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleSetLineCmd(%d, %d, 0), function() end)" % (va, self.A.L.eval("LINE_CB")))
        self.tick(30)
        self.assertEqual(self.A.applied(), self.B.applied())
        self.assertEqual(self.B.L.eval("GAME.applied[3].args[1]"), self.B.L.eval("GAME.applied[1].entity"))

    # ---- checksums

    def test_checksum_rounds_through_a_big_balance_a_stale_saved_state_and_a_pause(self):
        # Real sweep 2026-10-13: a balance of 5,035,445,200 (the game's Lua raises on "%d" beyond 32 bits, emulated in the harness): no
        # round reached the launcher and nothing said why. The same sweep's base save carried the earlier session's 40 stored reports:
        # the new session read them as its own, marked them sent and never sent the real ones.
        for g in (self.A, self.B):
            g.L.execute("""
              ACCOUNT.balance = 5035445200
              local reports, results, refused = {}, {}, {}
              for n = 1, 40 do reports[n] = { n = n, time = 1000 + n, parts = { time = "old" .. n, balance = "old" }, account = { balance = 38000000, loan = 0 } } end
              results[1] = { key = "1:1", at = 5, success = true }
              refused[1] = { key = "1:2", from = 1, seq = 2, detail = "stale refusal" }
              STATE:set({ reports = reports, results = results, refused = refused, late = 7, queue = {}, bind = {}, watches = {}, binders = {} })
            """)
        self.start()
        self.tick(10, lag_b=True)
        self.relay.pump()
        heavy = {"constructions", "edges", "nodeConfigs", "townBuildings"}
        light = {"time", "balance", "loan", "lines", "stations", "names", "vehicles"}
        at = self.relay.future_stamp()
        self.relay.checksum_request(1, at, False)   # no flag: a full round
        self.tick(40, lag_b=True)
        self.relay.checksum_request(2, self.relay.future_stamp(), False, full=False)
        self.tick(40, lag_b=True)
        self.A.L.execute("REAL_CLOCK = os.clock; local t = 0; os.clock = function() t = t + 0.03; return t end")   # every hash > 50 ms on A
        self.relay.checksum_request(3, self.relay.future_stamp(), False, full=True)
        self.tick(40, lag_b=True)
        self.A.L.execute("os.clock = function() error('no clock') end")   # a clock that raises is harmless
        self.relay.checksum_request(4, self.relay.future_stamp(), False)
        self.tick(40, lag_b=True)
        self.A.L.execute("os.clock = REAL_CLOCK")
        self.tick(40, lag_b=True)   # the lagging client's spread round needs more steps than the host's
        for n in (1, 2, 3, 4):
            self.assertEqual(sorted(self.relay.reports.get(n, {})), [0, 1], "round %d reached the launcher from both games" % n)
            pa, pb = self.relay.reports[n][0][1], self.relay.reports[n][1][1]
            self.assertEqual(pa, pb, "identical worlds give identical parts (round %d); timing never enters them" % n)
            self.assertNotIn("old", str(pa["time"]), "a stale part reached the launcher")
            names = set(pa)
            if n == 2:
                self.assertFalse(names & heavy or any(k.startswith("townBuildings:") for k in names), names)
                self.assertTrue(light <= names, names)
            else:
                self.assertTrue(heavy <= names and any(k.startswith("townBuildings:") for k in names), (n, names))
        (ta, pa), (tb, _) = self.relay.reports[1][0], self.relay.reports[1][1]
        self.assertEqual((ta, tb), (at, at), "both hashed at the requested stamp")
        self.assertEqual(set(pa) - {"stations", "edges", "nodeConfigs", "vehicles", "names", "ids"},
                         {"time", "balance", "loan", "constructions", "lines", "townBuildings", "townBuildings:90", "townBuildings:91"})
        self.assertTrue(pa["townBuildings"].startswith("3/") and pa["townBuildings:90"].startswith("2/") and pa["townBuildings:91"].startswith("1/"), pa)
        self.assertEqual(self.relay.authority[2]["balance"], 5035445200)
        self.assertNotIn("checksum slow", self.B.log(), "a fast checksum says nothing")
        self.assertRegex(self.A.log(), r"checksum slow: total \d+ ms: \w+ \d+")
        self.assertNotIn("stale refusal", "".join(self.A.lines(self.A.from_game)) + "".join(self.B.lines(self.B.from_game)))
        self.assertIn("ignoring", self.A.log() + self.B.log(), "the stale outputs are logged once")
        self.assertNotIn("applied an action late", self.A.log())
        self.assertNotIn("failed", self.B.log())
        # paused rounds: the games are stopped, the interface half answers
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(40, lag_b=True)
        self.assertEqual(self.A.time(), self.relay.pauseAt)
        diffs = [
            (None, set()),
            ("ACCOUNT.balance = ACCOUNT.balance + 1000", {"balance"}),   # one account differs: only the balance part
            ("WORLD[65].TOWN_BUILDING.level = 2", {"balance", "townBuildings", "townBuildings:91"}),   # one building of town 91 grown
        ]
        for n, (change, want) in enumerate(diffs, start=5):
            if change:
                self.B.L.execute(change)
            self.relay.checksum_request(n, self.relay.pauseAt, True)
            self.tick(5)
            (ta, pa), (tb, pb) = self.relay.reports[n][0], self.relay.reports[n][1]
            self.assertEqual((ta, tb), (self.relay.pauseAt, self.relay.pauseAt))
            self.assertEqual({k for k in pa if pa[k] != pb[k]}, want, n)
        # a building in town 90 on one game only: the counts in the part values name the town
        self.B.L.execute("WORLD[66] = { TOWN_BUILDING = { town = 90, construction = 62, level = 1, depth = 1, timeBuilt = 400, parcels = {} } }")
        self.relay.checksum_request(8, self.relay.pauseAt, True)
        self.tick(5)
        pa, pb = self.relay.reports[8][0][1], self.relay.reports[8][1][1]
        self.assertTrue(pa["townBuildings:90"].startswith("2/") and pb["townBuildings:90"].startswith("3/"), (pa, pb))
        self.assertTrue(pa["townBuildings"].startswith("3/") and pb["townBuildings"].startswith("4/"))
        # a game-script state the interface half cannot read is logged with the reason
        self.A.L.execute("STATE.get = function(self) error('cannot convert') end")
        self.tick(5)
        self.assertIn("the game-script state cannot be read: ", self.A.log())
        self.assertIn("cannot convert", self.A.log())

    # ---- window commands, speed, roster, chat

    def test_window_commands_reach_the_other_game_once_and_leave_every_checksum_part_equal(self):
        # The windows live in a Lua state of their own (real game: a loan taken in the window never reached the bridge's wrap). The UI
        # hook records the command for the bridge; the originator applies it natively, once, and records it in its company ledger with
        # the stamp the other game uses (real game: else the games "differ in companies" after every loan, rename or purchase).
        self.start()
        self.tick(10)
        self.A.window('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Obtain", { 1, { amount = 5 } }))')
        self.B.window('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Repay", { nil, { amount = 5 } }))')
        self.tick(60)
        self.assertEqual((self.A.events_seen(), self.B.events_seen()), (2, 2), "each command once on each game, natively on its originator")
        self.assertIn("window command", self.A.log())
        self.relay.checksum_request(1, self.relay.future_stamp(), False)
        self.tick(40)
        pa, pb = self.relay.reports[1][0][1], self.relay.reports[1][1][1]
        self.assertEqual({k for k in pa if pa[k] != pb[k]}, set(), "no part differs: " + str({k: (pa[k], pb[k]) for k in pa if pa[k] != pb[k]}))
        self.assertIn("companies", pa)

    def test_the_window_speed_buttons_are_requests_and_the_roster_status_and_chat_show(self):
        self.start(speed=2)
        # a speed button, from the bridge's state or a window's, is a request to the host and not applied there
        self.A.send_gui("api.cmd.sendCommand(api.cmd.makeGameSetSpeedCmd(3))")
        self.tick(3)
        self.A.window("api.cmd.sendCommand(api.cmd.makeGameSetSpeedCmd(4))")
        self.tick(5)
        self.assertEqual(self.relay.speed_requests, [3, 4])
        self.assertEqual(self.A.speed(), 2, "the session's speed, not the button's")
        # the FeverLink3 window's own buttons: a record; the bridge refuses a speed it does not know
        self.assertTrue(self.B.ui_speed(3))
        self.assertTrue(self.A.ui_speed(0))
        self.assertTrue(self.B.ui_speed(9), "the record is written")
        self.tick(4)
        b = [l for l in self.B.lines(self.B.from_game) if l.split(TAB)[0] == "speed_request"]
        self.assertEqual(len(b), 1, b)
        self.assertIn('["speed"]=3', b[0])
        self.assertEqual(sum('["speed"]=0' in l for l in self.A.lines(self.A.from_game) if l.split(TAB)[0] == "speed_request"), 1)
        # roster and status
        self.relay.send(self.B, "welcome", 0, self.relay.ser({"you": 1, "hostName": "host", "version": "0.1", "players": [{"id": 1, "name": "client", "ready": True}, {"id": 2, "name": "carol", "ready": False}], "session": {}}))
        self.tick(4)
        status, roster, chat = str(self.B.ui_view()).split("|")
        self.assertEqual(status, "Running x2 - 3 players")
        self.assertEqual(roster, "host [host], client (you), carol")
        self.relay.send(self.B, "player_joined", 0, self.relay.ser({"player": {"id": 3, "name": "dave", "ready": False}}))
        self.relay.send(self.B, "player_left", 0, self.relay.ser({"id": 2, "name": "carol"}))
        self.tick(4)
        self.assertEqual(str(self.B.ui_view()).split("|")[1], "host [host], client (you), dave")
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
        self.B.frames_to_load()
        self.B.reload()
        self.tick(4)
        view = str(self.B.ui_view()).split("|")
        self.assertEqual(view[1:], ["host [host], client (you), dave", "client: hello team !"])

    # ---- host authority over the money

    def test_the_host_corrects_money_only_for_a_difference_seen_two_rounds_in_a_row(self):
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
        self.relay.checksum_request(2, self.relay.future_stamp(), False)
        self.tick(40, lag_b=True)
        # the world moves on before the correction arrives: it is measured against the round, not against now
        self.B.L.execute("ACCOUNT.balance = ACCOUNT.balance - 300")
        self.relay.send_authority(2)
        self.tick(6)
        self.assertEqual(self.B.L.eval("ACCOUNT.balance"), 1000000 - 300, "balance moved by the round's difference (-5000) only")
        self.assertEqual(self.B.L.eval("ACCOUNT.loan"), 250000)
        self.assertEqual((self.B.L.eval("JOURNAL[1]"), self.B.L.eval("#JOURNAL")), (-5000, 1), "booked as one journal entry for the delta")
        self.assertEqual(self.A.L.eval("#JOURNAL"), 0, "the host's own game is never corrected")
        self.assertEqual(self.B.applied(), [], "a journal entry is not a replicated action")
        self.B.L.execute("ACCOUNT.balance = ACCOUNT.balance + 300")
        self.relay.checksum_request(3, self.relay.future_stamp(), False)
        self.tick(40, lag_b=True)
        self.assertEqual(self.relay.reports[3][0][1], self.relay.reports[3][1][1], "worlds identical after the correction")
        self.assertIn("balance corrected by -5000", self.B.log())
        # Real game: the host sends its values as it reaches a round; a game a little behind had no account of its own for it yet, the
        # values were thrown away every round, and money never followed. They wait for the round.
        balance, loan = int(self.B.L.eval("ACCOUNT.balance")), int(self.B.L.eval("ACCOUNT.loan"))
        for n in (9, 10):
            self.relay.authority[n] = {"balance": balance + 700, "loan": loan}
            self.relay.send_authority(n)
            self.tick(5)
            self.assertEqual(self.B.L.eval("#JOURNAL"), 1, "nothing to measure against yet, and nothing thrown away")
            self.assertNotIn("skipped", self.B.log())
            self.relay.checksum_request(n, self.relay.future_stamp(), False)
            self.tick(40)
        self.assertEqual((self.B.L.eval("JOURNAL[2]"), self.B.L.eval("#JOURNAL")), (700, 2), "applied once the game reached the second round")
        self.assertIn("balance corrected by 700", self.B.log())
        # an action replayed on the other game reaches it a lead later: a round in between sees it on one side only, the next round sees
        # none (real game: correcting the first counted it twice)
        balance, loan = int(self.B.L.eval("ACCOUNT.balance")), int(self.B.L.eval("ACCOUNT.loan"))
        for n, extra in ((11, 17000), (12, 0)):
            self.relay.authority[n] = {"balance": balance + extra, "loan": loan + extra}
            self.relay.send_authority(n)
            self.tick(5)
            self.relay.checksum_request(n, self.relay.future_stamp(), False)
            self.tick(40)
        self.assertEqual(self.B.L.eval("#JOURNAL"), 2)
        self.assertNotIn("corrected by 17000", self.B.log())

    # ---- resync: save, load, the start gate

    def test_resync_save_and_load_and_a_rewritten_link_file_dispatch_nothing_twice(self):
        self.start()
        self.tick(10)
        self.B.send_gui(BUY % "X")
        self.tick(40)
        before = self.B.applied()
        self.assertEqual(len(before), 1, "the client's action applied once")
        self.assertEqual(self.A.applied(), before)
        # a launcher compacting the host's to_game.log: the file gets shorter, its lines are not dispatched again, new lines still arrive
        self.relay.rewrite_history(self.A)
        self.assertTrue(any(l.split(TAB)[0] == "action" for l in self.A.lines(self.A.to_game)), "the old action line is still in the file")
        self.tick(20)
        self.assertEqual(self.A.applied(), before, "re-written history was not dispatched again")
        # a load of an unknown save fails without reloading; the game keeps reading its link: the line read with the load line (skipped
        # while the load waited for the detail dump) is dispatched once after the failure
        self.relay.send(self.B, "resync_load", 0, self.relay.ser({"id": 4, "name": "nope", "sha256": "00"}))
        self.relay.send(self.B, "chat", 0, self.relay.ser({"text": "after the load line", "name": "Host"}))
        self.tick(3)
        self.assertEqual(self.B.log().count("after the load line"), 1)
        self.assertFalse(self.B.reload_pending())
        self.assertEqual(len(self.relay.replay_failed), 1)
        self.assertEqual(read_text(os.path.join(self.B.dir, "resync_cursor.txt")), "")
        self.tick(3)
        self.assertEqual(self.B.speed(), 1)
        # a save, then a save that fails with its reason
        self.relay.send(self.A, "resync_save", 0, self.relay.ser({"id": 7, "name": "resync_7"}))
        self.tick(4)
        self.assertEqual(self.A.L.eval("SAVED[1]"), "resync_7")
        self.assertEqual(self.relay.saves_done, [(0, 7, True, None)])
        self.A.L.execute("SAVE_FAIL = true")
        self.relay.send(self.A, "resync_save", 0, self.relay.ser({"id": 8, "name": "resync_8"}))
        self.tick(4)
        self.assertEqual(self.relay.saves_done[1][:3], (0, 8, False))
        self.assertIn("disk full", self.relay.saves_done[1][3])
        # the load: the old scripts start it, the reloaded mod says hello and dispatches nothing the file already held
        self.assertEqual(self.relay.hellos.count(1), 1)
        self.relay.send(self.B, "resync_load", 0, self.relay.ser({"id": 3, "name": "hostsave", "sha256": "00"}))
        after_load_line = os.path.getsize(self.B.to_game)
        self.B.frames_to_load()
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
        self.assertIn("speed=3", self.B.log(), "the line behind the load was kept for the reloaded game")
        self.relay.speed, self.relay.started = 2, True
        self.relay.session()
        self.tick(5)
        self.assertEqual((self.A.speed(), self.B.speed()), (2, 2))
        # the reloaded writer went on at the end of from_game.log: the launcher reads both hellos, every line whole
        kinds = [l.split(TAB)[0] for l in self.B.lines(self.B.from_game)]
        self.assertEqual(kinds.count("hello"), 2)
        self.assertTrue(all(len(l.split(TAB)) == 3 for l in self.B.lines(self.B.from_game)))

    def test_the_start_gate_after_a_resync_load(self):
        # app.setWaitForStartReadyGame / startReadyGame (script-doc app.html). Off by default since the lab run of 2026-10-18 (H-new: while
        # the loaded game waited at its "Start Game" screen this script did not run); FEVERLINK3_READY_WAIT=1 turns it on.
        cases = ("off by default", "waits for the session", "functions missing", "time limit", "bound functions are tables")
        for i, case in enumerate(cases):
            with self.subTest(case=case):
                if i:
                    self.again()
                if case == "off by default":
                    os.environ.pop("FEVERLINK3_READY_WAIT", None)
                else:
                    os.environ["FEVERLINK3_READY_WAIT"] = "1"
                try:
                    self.start_gate_case(case)
                finally:
                    os.environ.pop("FEVERLINK3_READY_WAIT", None)
                    os.environ.pop("FEVERLINK3_READY_WAIT_SECONDS", None)

    def start_gate_case(self, case):
        B = self.B
        if case == "functions missing":
            B.L.execute("app.setWaitForStartReadyGame, app.isWaitForStartReadyGame, app.startReadyGame = nil, nil, nil")
        if case == "bound functions are tables":
            # the lab run of 2026-10-18 logged the interface state's app functions as tables ("saveUserdata=table"): bound engine functions
            B.L.execute("for _, n in ipairs({ 'setWaitForStartReadyGame', 'isWaitForStartReadyGame', 'startReadyGame' }) do"
                        " local f = app[n]; app[n] = setmetatable({}, { __call = function(_, ...) return f(...) end }) end")
        self.start()
        self.tick(10)
        self.relay.send(B, "resync_load", 0, self.relay.ser({"id": 3, "name": "hostsave", "sha256": "00"}))
        B.frames_to_load()
        self.assertTrue(B.reload_pending())
        B.reload()
        if case == "off by default":
            self.tick(3)
            self.assertIn("ready: the start gate is off", B.log())
            self.assertFalse(B.L.eval("GAME.held == true"))
        elif case == "functions missing":
            self.assertIn("ready: app.setWaitForStartReadyGame / app.startReadyGame missing", B.log())
            self.tick(4)
            self.assertFalse(B.L.eval("GAME.held == true"))
            self.assertEqual(B.speed(), 0, "the first frame's speed 0 holds the game until the session runs")
            self.relay.session()
            self.tick(6)
            self.assertEqual(B.speed(), 1)
        elif case == "time limit":
            os.environ["FEVERLINK3_READY_WAIT_SECONDS"] = "0"
            self.tick(3)
            self.assertEqual(int(B.L.eval("READY_STARTS")), 1)
            self.assertIn("not released within 0 s", B.log())
            self.assertEqual(B.speed(), 0, "started and held by speed 0, the old path")
        else:
            self.assertIn("ready: app.setWaitForStartReadyGame() before the load called", B.log())
            held_at = B.time()
            self.tick(8)
            self.assertEqual(B.time(), held_at, "the loaded game ran no step while it waited")
            self.assertTrue(B.L.eval("GAME.held == true and READY_STARTS == nil"))
            self.assertIn("ready: this loaded game waits", B.log())
            self.assertEqual(self.relay.hellos.count(1), 2, "the waiting game's interface half said hello")
            self.relay.session()   # the host's session line after the load releases it: started once, then it runs with the session
            self.tick(6)
            self.assertEqual(int(B.L.eval("READY_STARTS")), 1, "app.startReadyGame was called once")
            self.assertIn("ready: app.startReadyGame() after", B.log())
            self.assertIn("(the session runs, speed", B.log())
            self.assertGreater(B.time(), held_at)
            self.tick(6)
            self.assertEqual(int(B.L.eval("READY_STARTS")), 1, "and never again")
            self.assertIsNone(self.A.L.eval("READY_STARTS"), "a game that did not load is not touched")
            self.assertFalse(self.log_has("LATE"))
            if case == "bound functions are tables":
                self.assertIn("app keys (gui): app ", B.log())
                self.assertIn("by name: setWaitForStartReadyGame=table isWaitForStartReadyGame=table startReadyGame=table", B.log())
                self.assertIn("app keys (sim): app ", B.log())


@own_tests_only
class StampedEverywhereTest(LockstepTest):
    """Every game, the originator included, applies a command at its stamp, in stamp order: then the engine hands out the same
    entity ids on every game. Both mocked games here allocate ids from the SAME base (in LockstepTest they differ on purpose), so
    an action applied at another step, or in another order, on one game shows as different ids."""

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

    def test_window_commands_are_applied_at_their_stamp_on_the_originator_too(self):
        self.start()
        self.tick(10, lag_b=True)
        # makeScriptingSendEventCmd(src, ...): src names a game script as the pretended origin (tf3-docs api/cmd); the bridge's messages
        # to its simulation half send "" as the base game does
        srcs = self.A.L.eval("(function() local out = {} for _, e in ipairs(EVENTS_SEEN) do if e[1] == 'feverlink3' then out[#out + 1] = e[3] end end return table.concat(out, ',') end)()")
        self.assertTrue(self.A.L.eval("(function() for _, e in ipairs(EVENTS_SEEN) do if e[1] == 'feverlink3' then return true end end return false end)()"),
                        "the session message went to the simulation half")
        self.assertEqual(set(srcs.split(",")), {""}, srcs)
        self.A.window(BUY % "BUY")
        # the other player buys from the bridge's own state a moment later: the two purchases interleave by stamp
        self.B.send_gui(BUY % "X")
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
        self.B.window(LINE % ("L", "LINE"))
        self.tick(40, lag_b=True)
        self.assertSameApplies(3)
        line = int(self.B.L.eval("LINE"))
        self.assertEqual(line, int(self.B.L.eval("GAME.applied[3].entity")))
        self.A.window("api.cmd.sendCommand(api.cmd.makeVehicleSetLineCmd(%d, %d, 0), function(d, s) SET = s end)" % (int(self.A.L.eval("BUY[2]")), line))
        self.tick(40, lag_b=True)
        self.assertSameApplies(4)
        self.assertTrue(self.A.L.eval("SET == true"))
        # sendCommand's callback: (data, success, resultEntities : {{Engine.Entity, Engine.Revision}}) (tf3-docs api/cmd): a stamp
        # apply's result names the entity only; the callback gets this game's revision of it, from the bridge and from a window alike
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), '
                        'function(d, s, e) BUY_E = { e[1][1], e[1][2] and e[1][2].num and e[1][2].num[1] } end)')
        self.A.window('api.cmd.sendCommand(api.cmd.makeLineCreateCmd("M", { x = 0, y = 0, z = 0 }, 7, { stops = {} }), '
                      'function(d, s, e) LINE_E = { e[1][1], e[1][2] and e[1][2].num and e[1][2].num[1] } end)')
        self.tick(40, lag_b=True)
        self.assertSameApplies(6)
        vb, la = int(self.B.L.eval("BUY_E[1]")), int(self.A.L.eval("LINE_E[1]"))
        self.assertEqual(int(self.B.L.eval("BUY_E[2]")), 1000 + vb, "the bridge's answer carries the vehicle's revision")
        self.assertEqual(int(self.A.L.eval("LINE_E[2]")), 1000 + la, "the window's answer carries the line's revision")
        C = self.A.L.eval("GUI_ENV.ug_require('feverlink3_common.lua')")
        out = C.resultEntities(self.A.L.eval("{ { %d, { num = { 5, 0, 0 } } }, { 424242 }, 'x' }" % la))
        self.assertEqual(out[1][2]["num"][1], 5, "a pair that came with a revision keeps it")
        self.assertEqual((out[2][1], out[2][2]), (424242, None), "an entity that is gone keeps the pair as it came")
        self.assertIsNone(out[3])
        self.assertFalse(self.log_has("LATE"))
        # a purchase the engine refuses at its stamp is refused on every game, and the window hears it
        for g in (self.A, self.B):
            g.L.execute("NO_MONEY = true")
        self.A.window(BUY % "NOBUY")
        self.tick(40)
        self.assertSameApplies(6)
        self.assertTrue(self.A.L.eval("NOBUY ~= nil and NOBUY[1] == false"), "the callback ran with the refusal")
        for g in (self.A, self.B):
            self.assertIn("refused by the engine: ", g.log())
        self.assertIn("REFUSED at its stamp", self.A.log())

    def make_line(self):
        self.A.window(LINE % ("L", "LINE"))
        self.tick(40)
        return int(self.A.L.eval("LINE"))

    def actions_shipped(self, g, fn):
        """The stamps of g's own actions of command `fn`, as shipped to the host (from_game.log)."""
        return [int(p.stamp) for p in self.shipped(g) if ('"%s"' % fn) in str(p.body)]

    def test_a_window_order_is_stamped_once_or_shipped_nowhere(self):
        # Friend test 2026-10-16: cloning vehicles froze the game. The game's clone (vehicle_react_util.HandleVehicleChanges) buys every copy
        # in one loop and puts each on the line from the purchase's callback: the window's sendCommand returns at once, the purchases are
        # one order with one stamp, the line assignments another, the same on both games.
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
        self.assertFalse(self.log_has("LATE"))
        # an order that cannot be shipped whole (its second record names a window entity that was never shipped) is shipped nowhere
        shipped_before = self.actions_shipped(self.A, "makeVehicleBuyCmd")
        self.A.L.execute(r'''
          local C = UI_HOOK.ug_require("feverlink3_common.lua")
          local send = UI_HOOK.HookModule.send
          send("cmd", { id = 901, grp = 4242, category = "spend", sid = C.SESSION, body = BUY_BODY_TEXT })
          send("cmd", { id = 902, grp = 4242, category = "other", sid = C.SESSION,
            body = '{["fn"]="makeVehicleSetLineCmd",["args"]={["n"]=3,[1]={["__ref"]="entity",["key"]="ui:999"},[2]={["__ref"]="entity",["key"]="s:55"},[3]=0}}' })
        '''.replace("BUY_BODY_TEXT", "'%s'" % BUY_BODY_TEXT))
        self.tick(40)
        self.assertSameApplies(1 + 2 * n)
        self.assertEqual(self.actions_shipped(self.A, "makeVehicleBuyCmd"), shipped_before, "the valid purchase did not leave either")
        self.assertIn("a window order of 2 command(s) was not shipped, none of it", self.A.log())
        answers = [l for l in self.A.lines(os.path.join(self.A.dir, "ui_in.log")) if l.split(TAB)[0] == "result"]
        self.assertTrue(all('["success"]=false' in a for a in answers[-2:]), answers[-2:])

    def test_each_command_takes_the_entity_its_own_send_created_whatever_id_the_engine_reuses(self):
        # Run F 2026-10-18 (X1B): two buses bought in one window order, one stamp; the simulation state creates each as its command is
        # sent and the engine reuses freed ids. The first watch took the smaller new id (the second bus), the second found nothing:
        # "action 1:9: nothing created after 10 steps" on both games, a third vehicle in the depot for two purchases bound.
        self.start()
        self.tick(5)
        line = self.make_line()
        for g in (self.A, self.B):
            g.L.execute("SYNC_CREATE = true; FREE_IDS = { 49990, 49980 }")
        self.A.window(('BUYS, SETS = {}, {}; for i = 1, 2 do api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }),'
                       ' function(d, s) BUYS[i] = s and d.resultVehicleEntity or false; if s then api.cmd.sendCommand('
                       'api.cmd.makeVehicleSetLineCmd(d.resultVehicleEntity, %d, 0), function(_, s2) SETS[i] = s2 end) end end) end') % line)
        self.tick(60)
        self.assertSameApplies(5)
        bought = [int(self.A.L.eval("GAME.applied[%d].entity" % (i + 1))) for i, a in enumerate(self.A.applied()) if a.startswith("makeVehicleBuyCmd@")]
        self.assertEqual(bought, [49990, 49980], "the engine gave the second purchase the smaller id")
        self.assertEqual([int(self.A.L.eval("BUYS[%d]" % i)) for i in (1, 2)], [49990, 49980], "each callback got its own command's vehicle")
        self.assertEqual([self.A.L.eval("SETS[%d]" % i) for i in (1, 2)], [True, True])
        for g in (self.A, self.B):
            sets = [int(g.L.eval("GAME.applied[%d].args[1]" % (i + 1))) for i, a in enumerate(g.applied()) if a.startswith("makeVehicleSetLineCmd@")]
            self.assertEqual(sets, [49990, 49980], g.label)
        # a key still bound to an entity that is gone is no claim on a new entity with the same id (before, the watch skipped it:
        # nothing created, the purchase's callback failed on every game)
        for g in (self.A, self.B):
            g.L.execute("SYNC_CREATE = nil; WORLD[49990] = nil; FREE_IDS = { 49990 }")   # sold: the id is free again
        self.A.window(BUY % "SECOND")
        self.tick(40)
        self.assertTrue(self.A.L.eval("SECOND ~= nil and SECOND[1] == true"), self.A.log())
        self.assertEqual(int(self.A.L.eval("SECOND[2]")), 49990)
        for g in (self.A, self.B):
            self.assertNotIn("nothing created", g.log())
            self.assertNotIn("refused", g.log())

    def test_a_key_made_before_a_resync_load_still_names_its_entity_after_it(self):
        # Friend test 2026-10-21 21:51: the numbers of a player's actions began at 1 again after a resync load while the loaded world still
        # bound 1:4 to a line made before it; the vehicle bought as the new 1:4 took the key over and the set-line commands that named the
        # line by 1:4 were refused on both games ("line 1:4 does not exist here").
        self.start()
        self.tick(5)
        self.B.window(LINE % ("L", "LINE"))
        self.tick(40)
        line = int(self.B.L.eval("LINE"))
        for g in (self.A, self.B):
            self.relay.send(g, "resync_load", 0, self.relay.ser({"id": 21, "name": "hostsave", "sha256": "00"}))
            g.frames_to_load()
            self.assertTrue(g.reload_pending())
            g.reload()
        self.relay.session()
        self.tick(10)
        self.B.window(('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function(d, s) if s then '
                       'api.cmd.sendCommand(api.cmd.makeVehicleSetLineCmd(d.resultVehicleEntity, %d, 0), function(_, s2) SET = s2 end) end end)') % line)
        self.tick(60)
        self.assertSameApplies(3)
        self.assertTrue(self.B.L.eval("SET"), self.B.log())
        self.assertIn("action numbers go on from 1:2", self.B.log())
        for g in (self.A, self.B):
            sets = [int(g.L.eval("GAME.applied[%d].args[2]" % (i + 1))) for i, a in enumerate(g.applied()) if a.startswith("makeVehicleSetLineCmd@")]
            self.assertEqual(sets, [line], g.label)
            self.assertNotIn("refused", g.log())

    def pause_both(self):
        self.start(speed=2)
        self.tick(10, lag_b=True)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(40, lag_b=True)
        return self.relay.pauseAt

    def test_actions_of_a_pause_are_applied_in_the_hosts_order_on_every_game(self):
        cases = ("two players", "read after the resume", "assignment in the purchase's frame", "assignment a step later",
                 "the engine throws during a paused apply")
        for i, case in enumerate(cases):
            with self.subTest(case=case):
                if i:
                    self.again()
                getattr(self, "pause_case_" + str(i))(self.pause_both())

    def pause_case_0(self, paused_at):
        # each player acts during the pause: before, each game applied its own first (arrival order) and the ids crossed. Now the host
        # stamps them past where every game stands still and every game, the originator too, applies them at that stamp
        self.assertEqual((self.A.time(), self.B.time()), (paused_at, paused_at))
        self.A.window(BUY % "A_BUY")
        self.B.send_gui(BUY % "B_BUY")
        self.B.window(LINE % ("L", "X"))
        self.A.send_gui(LINE % ("M", "X"))
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
        self.assertFalse(self.log_has("LATE") or self.log_has("PAUSE ORDER GAP"))

    def pause_case_1(self, paused_at):
        # stamped at the pause, still in the outboxes when the host resumes: the resume line goes out first, then a normal stamp
        self.B.send_gui(BUY % "B_BUY")
        self.A.send_gui(BUY % "A_BUY")
        self.relay.pauseAt, self.relay.speed = None, 2
        self.relay.session()
        self.tick(20)
        for g in (self.A, self.B):
            lines = self.relay.history[g.id]
            kinds = [ln.split(TAB)[0] for ln in lines]
            last_pause = max(i for i, ln in enumerate(lines) if "pauseAt" in ln)
            self.assertGreater(kinds.index("action", last_pause + 1), kinds.index("session", last_pause + 1), "the actions came after the resume")
        self.assertSameApplies(2)
        self.assertGreater(min(self.A.time(), self.B.time()), paused_at, "and then both went on")
        self.assertTrue(self.B.L.eval("B_BUY ~= nil and B_BUY[1] == true"))
        self.assertTrue(self.A.L.eval("A_BUY ~= nil and A_BUY[1] == true"))
        self.assertFalse(self.log_has("LATE"))

    PAUSE_ITEMS = FIND_UP + r'''
    LINE_E = 49000
    WORLD[LINE_E] = { LINE = { stops = {} } }
    BUY_BODY = { fn = "makeVehicleBuyCmd", args = { n = 3, { __ref = "player" }, { __ref = "entity", key = "s:55" }, {} } }
    function SET_BODY(key) return { fn = "makeVehicleSetLineCmd", args = { n = 3, { __ref = "entity", key = key }, { __ref = "entity", key = "s:" .. LINE_E }, 0 } } end
    function ITEM(seq, at, body) return { kind = "action", at = at, from = 0, seq = seq, key = "0:" .. seq, body = body } end
    XX, GG = FIND_UP('X'), FIND_UP('G')
    ROUTE = function(it) XX.pauseArrived(it, (GG.pauseLast or 0) + 1) end
    NOW = GAME.time
    '''

    def pause_case_2(self, paused_at):
        # MPFever issue #36: in a pause, actions applied from the interface get their engine answer a frame later, and only that answer
        # binds the new vehicle. A game standing at the stamp reads the purchase and the assignment its callback made in one frame (a
        # game that stalled): the assignment waits for the purchase's answer instead of being refused on this game alone.
        g = self.B
        g.L.execute(self.PAUSE_ITEMS + "ROUTE(ITEM(91, NOW, BUY_BODY)); ROUTE(ITEM(92, NOW, SET_BODY('0:91')))")
        self.assertIn("action 0:92 waits for the engine's answer to the action that makes 0:91", g.log())
        self.tick(4)
        self.assertEqual([a.split("@")[0] for a in g.applied()[-2:]], ["makeVehicleBuyCmd", "makeVehicleSetLineCmd"], g.applied())
        self.assertNotIn("action 0:92 refused", g.log())
        self.assertIn("action 0:92 makeVehicleSetLineCmd applied during the pause", g.log())

    def pause_case_3(self, paused_at):
        # the assignment stamped a step later (applied by the simulation half as the game runs on): the game stays at speed 0 until the
        # purchase's answer bound the vehicle and reached the simulation half (before, the run-on stepped first and the simulation refused)
        g = self.B
        g.L.execute(self.PAUSE_ITEMS + "ROUTE(ITEM(91, NOW, BUY_BODY)); ROUTE(ITEM(92, NOW + 200, SET_BODY('0:91'))); GG.session.pauseRun = NOW + 400")
        now = g.time()
        self.tick(8)
        self.assertGreater(g.time(), now, "the game ran on to the assignment's stamp")
        self.assertEqual([a.split("@")[0] for a in g.applied()[-2:]], ["makeVehicleBuyCmd", "makeVehicleSetLineCmd"], g.applied())
        self.assertEqual(g.applied()[-1], "makeVehicleSetLineCmd@%d" % (now + STEP), "at its stamp")
        self.assertNotIn("action 0:92 refused", g.log())

    def pause_case_4(self, paused_at):
        # D on the pause path: the interface's send throws after the engine took the command. As at a stamp apply (simSendCommand), that
        # is "the world may be PARTLY built": said, the verdict carries it (the host never repairs from this game), the repair is asked
        g = self.B
        g.L.execute(self.PAUSE_ITEMS + "GUI_THROWS = 'makeVehicleBuyCmd'; ROUTE(ITEM(91, NOW, BUY_BODY)); GUI_THROWS = nil")
        self.assertIn("action 0:91 (applied during the pause): the engine threw DURING its apply: this game's world may be PARTLY built", g.log())
        self.assertTrue(any("partly built" in str(p.detail) for p in self.shipped(g, "replay_failed")))
        verdict = [p for p in self.shipped(g, "action_refused") if int(p.seq) == 91]
        self.assertTrue(verdict and verdict[-1].partial is True, verdict)


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

    def assertSameEverywhere(self, n):
        a = self.A.applied()
        self.assertEqual(len(a), n, a)
        for g in (self.B, self.C):
            self.assertEqual(g.applied(), a, "the same steps and order on every game")
            self.assertEqual(self.entities(g), self.entities(self.A), "the same entity ids")
        self.assertFalse(any("LATE" in g.log() for g in self.games), [g.log() for g in self.games if "LATE" in g.log()])

    def test_games_that_overshoot_the_pause_by_different_amounts_apply_the_pauses_actions_at_one_step(self):
        for g, k in zip(self.games, (0, 2, 3)):
            g.L.execute("OVERSHOOT = %d" % k)
        self.tick(5)
        self.relay.started, self.relay.speed = True, 3
        self.relay.session()
        self.tick(10)
        self.relay.pump()
        paused_at = self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(30)
        self.assertEqual([g.time() for g in self.games], [paused_at, paused_at + 2 * STEP, paused_at + 3 * STEP], "the mock overshoots as the real engine")
        self.B.send_gui(BUY % "B1")
        self.A.window(LINE % ("A", "X"))
        self.C.send_gui(BUY % "C1")
        self.tick(30)
        self.assertSameEverywhere(3)
        first = int(self.A.applied()[0].split("@")[1])
        self.assertEqual(first, paused_at + 4 * STEP, "one step past the furthest game")
        # a second batch: the games ran on to the first stamp and overshot it again, each by its own amount
        self.A.send_gui(BUY % "A2")
        self.B.window(LINE % ("B", "X"))
        self.tick(30)
        self.assertSameEverywhere(5)
        for name, owner in (("B1", self.B), ("C1", self.C), ("A2", self.A)):
            self.assertTrue(owner.L.eval("%s ~= nil and %s[1] == true" % (name, name)), name)
        self.relay.pauseAt, self.relay.speed = None, 2
        self.relay.session()
        self.tick(20)
        self.assertSameEverywhere(5)
        self.assertTrue(all(g.time() > first for g in self.games))


class PureTest(unittest.TestCase):
    """The common, lockstep and hud modules, without any game."""

    def setUp(self):
        os.environ.pop("FEVERLINK3_SESSION_DIR", None)
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.C = load_common(self.L)
        self.K = self.L.execute(LOCKSTEP_SRC)
        self.H = self.L.execute(HUD_SRC)

    def parsed(self, text):
        p = self.C.parse(text)
        return p[0] if isinstance(p, tuple) else p

    def test_values_serialize_and_link_lines_read_and_write_as_the_launcher_does(self):
        # money beyond 32 bits is text the game's Lua can make (real sweep 2026-10-13: a balance of 5,035,445,200)
        f = self.C.integerText
        for v, text in ((5035445200, "5035445200"), (-5035445200, "-5035445200"), (0, "0"), (-0.0, "0"), (37000000.0, "37000000"),
                        (2 ** 60, "1.152921504606847e+18")):
            self.assertEqual(f(v), text)
        self.assertEqual(float(f(1234.5678)), 1234.5678, "a fraction round-trips")
        self.assertEqual(self.C.serialize(5035445200), "5035445200")
        a = self.L.eval('(function() local t = {} t.b = 1 t.a = { 3, 2, x = "q\\t\\"z" } t[1] = 0.1 t[2] = true return t end)()')
        b = self.L.eval('(function() local t = { [2] = true, [1] = 0.1 } t.a = { x = "q\\t\\"z", 3, 2 } t.b = 1 return t end)()')
        sa = self.C.serialize(a)
        self.assertEqual(sa, self.C.serialize(b), "order independent")
        self.assertEqual(sa, '{[1]=0.10000000000000001,[2]=true,["a"]={[1]=3,[2]=2,["x"]="q\\t\\"z"},["b"]=1}')
        self.assertEqual(self.C.serialize(self.C.parse(sa)), sa)
        self.assertEqual(self.C.serialize(self.C.parse(self.C.serialize(-0.0))), "0")
        r = self.C.parse("os.exit()")   # no globals in the payload environment: nil plus a reason
        self.assertIsNone(r[0] if isinstance(r, tuple) else r)
        with self.assertRaises(lupa.LuaError):
            self.C.serialize(self.L.eval("print"))
        for text, h in (("", 0x811C9DC5), ("a", 0xE40C292C), ("foobar", 0xBF9CF968)):
            self.assertEqual(int(self.C.fnv1a(text)), h)
        self.assertEqual(self.C.hashValue(self.L.table_from({"x": 1})), self.C.hashValue(self.L.table_from({"x": 1})))
        # the reader: by position, a partial line waits, the same lines again are new lines, garbage is counted
        r = self.C.newReader("unused")
        recs = self.C.readerFeed(r, "session\t0\t{}\naction\t5\t{[\"stamp\"]=1}\nclo")
        self.assertEqual([x.kind for x in recs.values()], ["session", "action"])
        self.assertEqual(recs[2]["from"], 5)
        self.assertEqual((int(recs[1].start), int(recs[1].after), int(recs[2].start)), (0, 13, 13), "each record knows its place")
        self.assertEqual(self.C.posText(self.C.readerPos(r)), "1:%d" % len("session\t0\t{}\naction\t5\t{[\"stamp\"]=1}\n"), "the partial line is not consumed")
        self.assertEqual([x.kind for x in self.C.readerFeed(r, "ck\t0\t{}\r\n").values()], ["clock"])
        self.assertEqual([x.kind for x in self.C.readerFeed(r, "session\t0\t{}\nsession\t0\t{}\n").values()], ["session", "session"])
        self.assertEqual(int(r.dropped), 0)
        recs = self.C.readerFeed(r, "garbage without tabs\n")
        self.assertEqual((len(recs), int(r.dropped)), (0, 1))
        # a writer made again (a reload) goes on at the end of the file
        tmp = tempfile.mkdtemp(prefix="feverlink3-")
        try:
            p = os.path.join(tmp, "from_game.log")
            w = self.C.newWriter(p, 3)
            self.assertTrue(self.C.writerSend(w, "hello", self.L.table_from({"time": 1})))
            self.assertTrue(self.C.writerSend(w, "clock", self.L.table_from({"t": 1})))
            self.C.writerSend(self.C.newWriter(p, 3), "clock", self.L.table_from({"t": 2}))
            lines = read_text(p).splitlines()
            self.assertEqual([l.split(TAB)[0] for l in lines], ["hello", "clock", "clock"])
            self.assertEqual(lines[2], 'clock\t3\t{["t"]=2}')
        finally:
            shutil.rmtree(tmp, ignore_errors=True)

    WIDE_ENGINE = """(function(v)
      local e = {
        gameTime = function() return 5 end,
        account = function() return 100, 10 end,
        constructions = function() return {} end,
        lines = function() return {} end,
      }
      for _, k in ipairs({ "stations", "edges", "nodeConfigs", "vehicles", "names" }) do
        if v[k] then e[k] = function() return v[k] end end
      end
      return e
    end)"""

    def test_the_wide_checksum_lanes_see_what_differs_and_ignore_order(self):
        L = self.L

        def wide(**kw):
            parts, _t = self.K.checksumParts(L.eval(self.WIDE_ENGINE)(L.table_from(kw, recursive=True)), self.C.hashValue)
            return {k: parts[k] for k in parts}
        parts = wide()
        for name in ("stations", "edges", "nodeConfigs", "vehicles", "names"):
            self.assertNotIn(name, parts, "a lane is absent without its accessor")
        self.assertIn("balance", parts)
        st = [{"stations": 1, "terminals": 2, "at": [{"x": 1.04, "y": 2}, {"x": 5, "y": 6}]}, {"stations": 1, "terminals": 1, "at": [{"x": 9, "y": 9}]}]
        edge = {"x0": 0, "y0": 0, "x1": 50, "y1": 0, "type": "NORMAL", "typeIndex": -1, "roadType": "STREET", "template": "t", "style": "s",
                "decorations": 0, "objects": ["signal.mdl", "stop.mdl"]}
        node = {"x": 1, "y": 2, "lanes": 4, "crosswalks": 1, "preference": "AUTO", "light": 0, "slip": False}
        v = lambda line, state="EN_ROUTE", stopped=False: {"line": line, "state": state, "stopped": stopped, "sell": False}  # noqa: E731
        veh = [v("A#2"), v("A#2", "AT_TERMINAL"), v("B#3")]
        nm = {"stations": ["Alpha", "Beta"], "lines": ["L1"], "towns": ["T1", "T2"], "players": ["P"]}
        # lane, a value, values hashing the same (order, rounding, flipped endpoints), values that differ
        cases = [
            ("stations", st, [[st[1], {"stations": 1, "terminals": 2, "at": [{"x": 5, "y": 6.01}, {"x": 1.0, "y": 2}]}]],
             [[st[0], {"stations": 1, "terminals": 1, "at": [{"x": 9, "y": 12}]}]]),
            ("edges", [edge], [[dict(edge, x0=50, x1=0, objects=["stop.mdl", "signal.mdl"])]],
             [[dict(edge, objects=["stop.mdl"])], [dict(edge, template="u")]]),
            ("nodeConfigs", [node], [[dict(node)]], [[dict(node, light=1)]]),
            ("vehicles", veh, [[veh[2], veh[1], veh[0]]], [veh[:2], [v("A#2"), v("A#2"), v("B#3", stopped=True)]]),
            ("names", nm, [dict(nm, stations=["Beta", "Alpha"])],
             [dict(nm, stations=["Alpha", "Gamma"]), dict(nm, towns=["T1", "T3"]), dict(nm, players=["Q"])]),
        ]
        for lane, base, same, other in cases:
            with self.subTest(lane=lane):
                h = wide(**{lane: base})[lane]
                for x in same:
                    self.assertEqual(h, wide(**{lane: x})[lane])
                for x in other:
                    self.assertNotEqual(h, wide(**{lane: x})[lane])
        # a names difference says which entity (friend test 2026-10-16 could only say "names")
        e = L.eval(self.WIDE_ENGINE)(L.table_from({"names": {"stations": ["Beta", "Alpha"], "lines": ["L1"], "towns": ["T1"], "players": ["P"]}}, recursive=True))
        detail = L.table()
        self.K.checksumParts(e, self.C.hashValue, None, False, detail)
        self.assertEqual([detail["names"][i] for i in range(1, len(detail["names"]) + 1)],
                         ["lines|L1", "players|P", "stations|Alpha", "stations|Beta", "towns|T1"])
        # issue #3: an industry's station group (no owner) counts, its name (each game's language) does not; a detail round's
        # sample gives each name by entity, cut whole UTF-8 characters
        self.assertNotEqual(wide(names=dict(nm, unownedStations=1))["names"], wide(names=dict(nm, unownedStations=2))["names"])
        s = self.K.namesSample(L.table_from({"stations": ["Crop Farm", "a" * 99 + "ü"], "towns": ["T1"], "ids": {"stations": [7, 8]}}, recursive=True))
        self.assertEqual((s["stations|7"], s["stations|8"], s["towns|1"]), ("Crop Farm", "a" * 99, None))
        # a failing accessor yields an error part, not a crash
        e = L.eval(self.WIDE_ENGINE)(L.table_from({"stations": [{"stations": 1}]}, recursive=True))
        e.stations = L.eval("function() error('boom') end")
        self.assertEqual(self.K.checksumParts(e, self.C.hashValue)[0]["stations"], "error")

    def test_commands_encode_decode_and_entity_references(self):
        K, L, C = self.K, self.L, self.C
        ctx = L.table_from({"plain": C.plain, "keyOf": L.eval("function(e) return ({[50001]='1:1'})[e] or ('s:' .. e) end")})
        body = K.encodeCommand("makeVehicleBuyCmd", L.eval("{ n = 3, 7, 55, { vehicles = {} } }"), ctx)
        self.assertEqual(C.serialize(body), '{["args"]={[1]={["__ref"]="player"},[2]={["__ref"]="entity",["key"]="s:55"},[3]={["vehicles"]={}},["n"]=3},["fn"]="makeVehicleBuyCmd"}')
        body2 = K.encodeCommand("makeVehicleSetLineCmd", L.eval("{ n = 3, 50001, 60, 0 }"), ctx)
        self.assertIn('"1:1"', C.serialize(body2))
        dctx = L.table_from({"player": 7, "resolve": L.eval("function(k) if k == '1:1' then return 70001 end if k == 's:60' then return 60 end end")})
        out = K.decodeCommand(body2, dctx)
        self.assertEqual([int(out[i]) for i in (1, 2, 3)], [70001, 60, 0])
        out, err = K.decodeCommand(K.encodeCommand("makeVehicleSetLineCmd", L.eval("{ n = 3, 99, 60, 0 }"), ctx), dctx)
        self.assertIsNone(out)
        self.assertIn("unresolved", err)
        res, err = K.encodeCommand("makeVehicleBuyCmd", L.eval("{ n = 3, 7, 55, { f = print } }"), ctx)
        self.assertIsNone(res)
        self.assertIn("cannot be replicated", err)
        self.assertIsNone(K.encodeCommand("makeTerrainCmd", L.eval("{ n = 0 }"), ctx)[0])
        # registered commands may bring their own codec; one without hooks goes through the generic path
        fn, tag, back, category = L.eval("""function(K)
          K.registerCommands({ makeCustomCmd = { category = "build",
            encode = function(name, args, ctx) return { fn = name, args = { n = 1, args[1].tag } } end,
            decode = function(body, ctx) return { n = 1, { tag = body.args[1] } } end } })
          local body = K.encodeCommand("makeCustomCmd", { n = 1, { tag = "road" } }, {})
          return body.fn, body.args[1], K.decodeCommand(body, {})[1].tag, K.categoryOf("makeCustomCmd")
        end""")(K)
        self.assertEqual((fn, tag, back, category), ("makeCustomCmd", "road", "road", "build"))
        self.assertEqual(L.eval("""function(K)
          K.registerCommands({ makeGenericCmd = { category = "other" } })
          local body, why = K.encodeCommand("makeGenericCmd", { n = 1, 42 }, { plain = function(v) return v end })
          return body and body.args[1] or why
        end""")(K), 42)
        # {TransportMode : boolean} (script-doc api/type.html) travels by the enum's names and is keyed by this game's own values again
        # (wiki audit 2026-10-18 A5); fail closed on anything the enum does not name
        both = L.eval("function(f, x) local a, b = f(x); return { a, b } end")
        call = lambda f, code: both(f, L.eval(code))  # noqa: E731
        L.execute("FL3_SAVED_API = api; api = { type = { enum = { TransportMode = { PERSON = 0, CARGO = 1, BUS = 3, SHIP = 10 } } } }")
        try:
            r = call(C.modeNames, "{ [0] = true, [1] = false, [3] = true }")
            self.assertIsNone(r[2])
            self.assertEqual(dict(r[1].items()), {"PERSON": True, "CARGO": False, "BUS": True})
            L.execute("api.type.enum.TransportMode = { PERSON = 20, CARGO = 21, BUS = 23 }")
            back = both(C.modeMap, r[1])
            self.assertIsNone(back[2])
            self.assertEqual(dict(back[1].items()), {20: True, 21: False, 23: True})
            self.assertIn("99 is not a value of api.type.enum.TransportMode", call(C.modeNames, "{ [99] = true }")[2])
            self.assertIn("SHIP is not in api.type.enum.TransportMode here", call(C.modeMap, "{ SHIP = true }")[2])
            self.assertIn("not a boolean", call(C.modeNames, "{ [20] = 1 }")[2])
            L.execute("api = {}")
            self.assertIn("no api.type.enum.TransportMode", call(C.modeMap, "{ BUS = true }")[2])
            self.assertIsNone(C["writeEngineArray"], "the shift guess is gone")
        finally:
            L.execute("api = FL3_SAVED_API; FL3_SAVED_API = nil")
        # window placeholders become the keys of the records that made them; one never shipped cannot be named
        body = self.parsed('{["args"]={[1]={["__ref"]="entity",["key"]="ui:3"},[2]={["list"]={{["__ref"]="entity",["key"]="s:9"},'
                           '{["__ref"]="entity",["key"]="ui:4"}}}},["fn"]="x"}')
        seq = L.eval("function(id) return ({ [3] = 12, [4] = 13 })[id] end")
        self.assertTrue(K.rewriteUiKeys(body, seq, 1))
        text = C.serialize(body)
        self.assertTrue('"1:12"' in text and '"1:13"' in text and '"s:9"' in text and "ui:" not in text, text)
        ok2, missing = K.rewriteUiKeys(self.parsed('{["args"]={[1]={["__ref"]="entity",["key"]="ui:7"}}}'), seq, 1)
        self.assertEqual((ok2, missing), (False, 7))
        # an action that waits for a pending entity is not a refusal
        pend = lambda k: K.keyIsPending(k, L.eval("{ { key = '1:5' } }"), L.eval("{ { key = '0:2' } }"))  # noqa: E731
        self.assertEqual([bool(pend(k)) for k in ("1:5", "0:2", "0:2#stop1", "0:20", "s:8")], [True, True, True, False, False])
        body = self.parsed('{["args"]={[1]={["__ref"]="entity",["key"]="1:5"},[2]={["__ref"]="entity",["key"]="s:8"}}}')
        self.assertEqual("1:5", K.pendingRef(body, L.eval("function(k) return k == '1:5' end")))
        self.assertIsNone(K.pendingRef(body, L.eval("function(k) return false end")))
        # notes of what a game did not replicate ride with its actions (bounded, recent) and explain a refusal
        notes = L.eval("{}")
        K.addNote(notes, 1000, "a native build at t=1000 was not replicated")
        for i in range(10):
            K.addNote(notes, 6000 + i, "n" + str(i), 4)
        self.assertEqual(4, len(notes), "the list is bounded")
        fresh = L.eval("{ { at = 5000, text = 'a' }, { at = 5100, text = 'b' }, { at = 5200, text = 'c' }, { at = 5300, text = 'd' }, { at = 5400, text = 'e' } }")
        got = K.recentNotes(fresh, 5500)
        self.assertEqual((len(got), got[3]), (3, "t=5400 e"), "the newest three")
        self.assertEqual(0, len(K.recentNotes(fresh, 5000 + 700000)), "old notes are not relevant to later actions")
        body = L.eval("{ diverged = { 't=997800 a native build was not replicated' } }")
        why = K.refusalDetail("argument 2: stop 2: stationGroup s:1 does not exist here", body)
        self.assertIn("[the sender had not replicated: t=997800 a native build was not replicated]", why)
        self.assertEqual("not enough money", K.refusalDetail("not enough money", body), "only a reference problem is explained by it")

    def test_pace_stamps_margins_queue_and_random(self):
        K, L = self.K, self.L
        s = K.newSession()
        peers = L.table()
        self.assertEqual(K.pace(s, peers, 1000, 3), 0, "not started: hold")
        K.applySession(s, L.table_from({"started": True, "speed": 2, "lead": 1200}))
        self.assertEqual(K.pace(s, peers, 1000, 3), 2)
        K.applyPeerClock(peers, L.table_from({"from": 1, "t": 1000, "ahead": 3}), 0)
        self.assertEqual([K.pace(s, peers, t, 3) for t in (1400, 1600, 1800)], [1, 0, 0], "one step of room, at, past the barrier")
        K.applyPeerClock(peers, L.table_from({"from": 0, "t": 0, "ahead": 3}), 0)
        self.assertEqual(K.pace(s, peers, 1400, 3), 1, "own clock relayed back is ignored")
        K.applySession(s, L.table_from({"started": True, "speed": 4, "pauseAt": 1400, "lead": 1200}))
        self.assertEqual(K.pace(s, peers, 1000, 3), 2, "batch shortened to land on the pause")
        self.assertEqual(K.pace(s, peers, 1400, 3), 0)
        self.assertEqual(K.stampFor(s, 1400), 1400, "stopped at the pause: the stamp is the pause time")
        self.assertEqual([K.stampFor(s, 1000), K.stampFor(s, 1050)], [2200, 2400], "rounded up to a step")
        self.assertEqual(K.stampFor(s, 1000, 2000), 3000, "a game may raise the lead")
        self.assertEqual(K.stampFor(s, 1000, 400), 2200, "never lower it")
        # batches are shortened to land exactly on a checksum stamp (every game hashes at the same time); a mark never holds the game
        s = K.newSession()
        peers = L.table()
        K.applySession(s, L.table_from({"started": True, "speed": 4, "lead": 1200}))
        marks = L.table()
        self.assertEqual(K.pace(s, peers, 1000, 6, marks), 4, "no mark: full batch")
        K.addMark(marks, 2000, 1000)
        self.assertEqual([K.pace(s, peers, t, 6, marks) for t in (1000, 1400, 1800, 2000)], [4, 3, 1, 4])
        K.addMark(marks, 3000, 2000)
        self.assertEqual(len(marks), 1, "passed marks are dropped")
        K.applySession(s, L.table_from({"started": True, "speed": 4, "pauseAt": 2200, "lead": 1200}))
        self.assertEqual([K.pace(s, peers, t, 6, marks) for t in (2000, 2200)], [1, 0], "a pause still wins")
        K.applySession(s, L.table_from({"started": True, "speed": 4, "lead": 1200}))
        for start in (1000, 1200, 1600):   # games at different phases reach the same time at the stamp
            now, m = start, L.table()
            K.addMark(m, 2400, now)
            while now < 2400:
                now += int(K.pace(s, peers, now, 6, m)) * 200
            self.assertEqual(now, 2400)
        # the measured stamp margin: grows at once, shrinks after a full window; each speed its own; the largest need of the peers
        m = K.newMargin()
        self.assertEqual((K.marginNeed(m, 1), K.marginNeed(m, 3), K.marginTarget(1), K.marginTarget(3)), (4, 6, 3, 5))
        for i, wall in enumerate([0.0, 2.0, 4.0]):
            self.assertEqual(tuple(K.marginSample(m, 1, 4, 10000, 10000 - 7 * 200, wall)), (0, 7, 4, "keep"), i)
        sample, slack, need, how = K.marginSample(m, 1, 4, 10000, 10000 - 6 * 200, 6.0)
        self.assertEqual((sample, need, how), (1, 4, "keep"), "four builds but only 6 s: not yet")
        self.assertEqual(tuple(K.marginSample(m, 1, 4, 10000, 10000 - 7 * 200, 9.0))[2:], (2, "shrink"), "the window's largest sample plus one")
        self.assertEqual(tuple(K.marginSample(m, 1, 1, 10000, 10000 - 1 * 200, 10.0)), (3, 1, 4, "grow"), "too little slack: at once")
        self.assertEqual(K.marginLate(m, 1, 2), 8, "a late apply grows it by the lateness plus two")
        self.assertEqual(K.marginNeed(m, 3), 6, "each speed has its own need")
        self.assertEqual(K.marginSample(m, 3, 0, 10000, 10000 - 40 * 200, 0.0)[0], 0, "never negative")
        sample, _, need, _ = K.marginSample(m, 3, 24, 10000, 10000 + 10 * 200, 0.0)
        self.assertEqual((sample, need), (K.MARGIN_MAX, K.MARGIN_MAX))
        peers = L.table()
        self.assertEqual(K.marginExtra(peers, 1), 0, "no peer: nobody to wait for")
        for frm, t, need, want in ((1, 1000, None, 4), (1, 1200, 1, 1), (2, 1200, 2, 2), (2, 1400, 99, K.MARGIN_MAX), (2, 1400, -3, 1)):
            pc = {"from": frm, "t": t, "ahead": 3}
            if need is not None:
                pc["need"] = need
            K.applyPeerClock(peers, L.table_from(pc), 0)
            self.assertEqual(K.marginExtra(peers, 1), want, pc)
        # the test switches (FEVERLINK3_TEST_NO_MARGIN / NO_LEAD) fix the extra and change nothing unset
        self.assertIsNone(K.testMargin(None, None))
        self.assertIsNone(K.testMargin("0", "0"))
        self.assertIsNone(K.testMargin("extra:x", None))
        t = K.testMargin("1", None)
        self.assertEqual((t.extra, t.noLead), (0, None))
        self.assertEqual((K.testMargin("extra:3", None).extra, K.testMargin("extra:99", None).extra), (3, K.MARGIN_MAX))
        t = K.testMargin(None, "1")
        self.assertEqual((t.extra, t.noLead), (0, True), "no lead alone: no extra either")
        peers = L.table()
        self.assertEqual(K.marginExtra(peers, 3, K.testMargin("extra:2", None)), 0, "no peer: nobody to wait for")
        K.applyPeerClock(peers, L.table_from({"from": 1, "t": 1000, "ahead": 3, "need": 9}), 0)
        self.assertEqual([K.marginExtra(peers, 3, K.testMargin("1", None)), K.marginExtra(peers, 3, K.testMargin("extra:2", None)),
                          K.marginExtra(peers, 3, None)], [0, 2, 9])
        # the queue: by stamp, then checksum first, then origin and number; a passed stamp is late by its steps
        q = L.table()
        for at, frm, seq, kind in [(600, 2, 1, "action"), (600, 1, 9, "action"), (400, 3, 1, "action"), (600, 0, 0, "checksum"), (800, 1, 1, "action"), (600, 1, 2, "action")]:
            K.enqueue(q, L.table_from({"kind": kind, "at": at, "from": frm, "seq": seq}))
        due = K.takeDue(q, 600)
        self.assertEqual([(x.kind, int(x.at), int(x["from"]), int(x.seq), int(x.lateSteps)) for x in due.values()],
                         [("action", 400, 3, 1, 1), ("checksum", 600, 0, 0, 0), ("action", 600, 1, 2, 0), ("action", 600, 1, 9, 0), ("action", 600, 2, 1, 0)])
        self.assertEqual((len(q), int(q[1].at)), (1, 800))
        # the session's random numbers are the same on every game at a step, whatever a game drew before
        draws = lambda rng: [K.randomDraw(rng) for _ in range(5)] + [K.randomDraw(rng, 6) for _ in range(5)] + [K.randomDraw(rng, 10, 20) for _ in range(5)]  # noqa: E731
        seq1 = draws(K.newRandom(L.eval("function() return 1000 end")))
        rng2 = K.newRandom(L.eval("function() return 1000 end"))
        for _ in range(3):
            K.randomDraw(rng2)
        rng2.t = None   # reseeded by the step change
        self.assertEqual(seq1, draws(rng2))
        self.assertTrue(all(0 <= x < 1 for x in seq1[:5]) and len(set(seq1[:5])) > 3)
        self.assertTrue(all(1 <= x <= 6 and x == int(x) for x in seq1[5:10]))
        self.assertTrue(all(10 <= x <= 20 for x in seq1[10:]))
        self.assertNotEqual(K.randomDraw(K.newRandom(L.eval("function() return 1200 end"))), seq1[0], "another step draws other numbers")

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

    def test_the_bar_icon_colour_the_status_line_and_the_notices_follow_the_session(self):
        H, P = self.H, self.parsed
        m = H.new(1, "me")
        H.setSession(m, self.L.table_from({"started": True, "speed": 2}))
        icon = lambda ctl, now=1000: H.iconState(P(ctl) if ctl else None, m, now)
        line = lambda ctl: H.statusLine(P(ctl) if ctl else None, m)
        ok = '{state = "joined", started = true, me = 1, players = {{id = 0, name = "Ann", state = "connected", rtt = 15}, {id = 1, name = "me"}}}'
        # white without a session, red for a refused join or a failed sync, yellow while anything is pending, green otherwise
        self.assertEqual((icon(None), icon('{state = "idle"}'), icon('{state = "failed", message = "refused"}')), ("white", "white", "red"))
        self.assertEqual((icon(ok), line(ok)), ("green", "Running x2 - in sync"))
        self.assertEqual(icon('{state = "joined", started = true, resync = "failed: disk full"}'), "red")
        self.assertEqual(line('{state = "hosting", started = true, resync = "failed: disk full"}'), "Sync failed: disk full")
        sync = '{state = "hosting", started = true, sync = {phase = "sending", done = 45, total = 100}}'
        self.assertEqual((icon(sync), line(sync)), ("yellow", "Syncing: sending world 45%"))
        info = H.syncInfo(P(sync).sync, False)
        self.assertEqual((info.step, info.fraction, info.detail), ("receiving world", 0.45, "Receiving the host's world: 45%"))
        # the load names the games it still waits for, on every game (friend test 2026-10-21: ~52 s with only the step shown)
        loading = '{state = "joined", started = true, sync = {phase = "loading", loading = {"Arishi", "Ben"}}}'
        self.assertEqual(line(loading), "Syncing: waiting for Arishi and Ben to load the world")
        self.assertEqual(H.syncInfo(P(loading).sync, True).detail, "Waiting for Arishi and Ben to load the world...")
        away = ok.replace('state = "connected"', 'state = "away"')
        self.assertEqual((icon(away), line(away)), ("yellow", "Running x2 - Ann reconnecting"))
        self.assertEqual(H.playerState(P(away).players[1], 1, m), ("yellow", "reconnecting"))
        self.assertEqual(H.playerState(P(ok).players[1], 1, m), ("green", "15 ms"))
        self.assertEqual(icon('{state = "joined", started = false}'), "yellow")
        self.assertEqual(line('{state = "joining", reconnecting = true, reconnectingS = 7}'), "Reconnecting (7 s)")
        # notices: read defensively, newest last; an error notice turns the icon red for a while
        self.assertEqual(len(H.notices(m)), 0)
        m.notices = P('{{text = "a", level = "info", t = 990}, {text = 5}, {text = "b", level = "odd", t = 995}}')
        listed = H.notices(m)
        self.assertEqual([(listed[i].text, listed[i].level) for i in (1, 2)], [("a", "info"), ("b", "info")])
        self.assertEqual(H.latestNotice(m, 1000, 10)[0].text, "b")
        self.assertIsNone(H.latestNotice(m, 1010, 10))
        m.notices = P('{{text = "repair failed", level = "error", t = 995}}')
        self.assertEqual((icon(ok, 1000), icon(ok, 1100)), ("red", "green"))

    def test_the_window_model_roster_chat_history_and_mod_lists(self):
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
        # the chat history is bounded and the snapshot round-trips
        h = H.new(2, "me")
        for i in range(1, 251):
            H.addChat(h, "bob", "m%d" % i, 3)
        self.assertEqual((len(h.history), h.history[1].text, h.history[200].text), (200, "m51", "m250"))
        self.assertEqual(H.chatLines(h, 2)[2], "bob: m250")
        self.assertFalse(H.addChat(h, "bob", "  ", 3))
        # system notices stay out of the chat: at most 20, a repeat moves to the end with a count
        for i in range(1, 26):
            H.addNotice(h, "n%d" % i, "info", 100 + i)
        H.addNotice(h, "n10", "warn", 200)
        H.addNotice(h, "n10", "bogus", 201)
        self.assertEqual((len(h.notices), h.notices[1].text, h.notices[20].text, h.notices[20].level, h.notices[20].t),
                         (20, "n6", "n10 (x3)", "info", 201))
        self.assertEqual(len(h.history), 200, "no notice went into the chat")
        back = H.fromSnapshot(self.C.parse(self.C.serialize(H.snapshot(h))))
        self.assertEqual((back.version, len(back.history), back.history[200].text), (h.version, 200, "m250"))
        H.addNotice(back, "n10", "error", 300)
        self.assertEqual((len(back.notices), back.notices[20].text, back.notices[20].level), (20, "n10 (x4)", "error"))
        # the catalogue (feverlink3_text.lua): plain words only, "Your ..." for the player, "<name>'s ..." for the others
        T = self.L.execute(TEXT_SRC)
        for key in list(T.MSG.keys()):
            for owner in (None, "Pat"):
                text, level = T.say(key, self.L.table_from({"who": "Bob", "cause": T.CAUSES.missing, "thing": "build"}), owner)
                self.assertIn(level, ("info", "warn", "error"), key)
                self.assertNotRegex(text.lower(), r"checksum|lane|\bids?\b|stamp|inject|verdict|resync|engine|\{|\d:\d", key)
        self.assertEqual(T.say("build_cancelled", None)[0][:11], "Your build ")
        self.assertEqual(T.say("build_cancelled", None, "Pat")[0][:12], "Pat's build ")
        self.assertEqual(H.rosterText(back), "me (you)")
        self.assertIsNone(H.fromSnapshot(L.table_from({"version": 1})))
        # the mod list from the documented repository (app.getUserProfile():getModRep(), getInstalledMods() of Mod.ModId records with
        # `name`, getModDesc(id).revision / .cosmetic), and the lines of a join refused over mods. The mock is the game's shape: the
        # window's app functions are callable tables ("getUserProfile:table", lab L3), the repository is called as the base game does
        # (gui/menu/main_menu.tl, mod_selector_page.tl)
        app = self.L.execute("""
          local descs = { trains = { revision = 3, cosmetic = false }, ui = { revision = 2, cosmetic = true } }
          local rep = {}
          function rep:getInstalledMods() return { { name = "trains" }, { name = "ui" } } end
          function rep:getModDesc(id) return descs[id.name] end
          local profile = {}
          function profile:getModRep() return rep end
          return { getUserProfile = setmetatable({}, { __call = function() return profile end }) }
        """)
        mods = H.modList(app)
        self.assertEqual([(mods[i].id, mods[i].revision, mods[i].cosmetic) for i in (1, 2)], [("trains", 3, False), ("ui", 2, True)])
        self.assertIn("2 mods; getUserProfile=table", H.modShape(app))
        # the window sends its list again only when this text changes (shareMods in feverlink3_ui.script.lua)
        key = H.modsKey(self.L.table_from({"game": "40423", "mods": mods}))
        self.assertEqual(key, "40423|trains:3|ui:2c")
        self.assertNotEqual(key, H.modsKey(self.L.table_from({"game": "40423"})))
        self.assertIn("id1.name=string:trains", H.modShape(app))
        broken = self.L.execute("local p = {} function p:getModRep() error('no repository in this state') end"
                                " return { getUserProfile = function() return p end }")
        # a refused read (the game restricts the repository in a running world) answers with the last list read
        cached = H.modList(broken)
        self.assertEqual(cached[1].id, "trains")
        self.assertIn("FAILED at profile:getModRep()", H.modShape(broken))
        # a mod's player-facing name from its own information: the folder is its Mod Hub id (mod.io) or its mod id (user mod)
        named = self.L.execute("""
          local rep = {}
          function rep:getInstalledMods() return { { name = "zhenya_auto_color_lines" }, { name = "trains" } } end
          function rep:getGameModDesc(id) return ({ zhenya_auto_color_lines = { modhubModId = "6416672", name = "Auto Color Lines" },
            trains = { modhubModId = "", name = "Trains" } })[id.name] end
          local profile = {}
          function profile:getModRep() return rep end
          return { getUserProfile = function() return profile end }
        """)
        self.assertEqual([H.modName(named, "6416672"), H.modName(named, "trains"), H.modName(named, "999")], ["Auto Color Lines", "Trains", None])
        self.assertIsNone(H.modName(broken, "6416672"))
        # a record keeps the sign of zero (a roundabout's transform has -0.0; the proof compares bit for bit), logs do not
        text = self.C.serialize(self.L.execute("return { -0.0, 0.0, 1.5 }"))
        self.assertIn("-0.0", text)
        # the sign is checked in Lua: lupa hands Lua 5.2's integral -0.0 to Python as the int 0
        self.assertEqual(self.L.execute("return function(t) return 1 / t[1] end")(self.C.parse(text)), float("-inf"))
        self.assertEqual(self.C.integerText(self.L.execute("return -0.0")), "0")
        ctl = self.C.parse('{ mods = { {block = true, text = "missing maps (host has revision 1)"}, {block = false, text = "only you have x"} },'
                           ' modWarnings = { "the host could not list its mods" } }')
        lines = H.modLines(ctl)
        self.assertEqual([lines[1], lines[2], lines[3]], ["Must match: missing maps (host has revision 1)", "Warning: only you have x",
                                                          "Warning: the host could not list its mods"])
        self.assertEqual(len(H.modLines(None)), 0)
        # the world's mods from the save just made: app.findAllSavegames, app.getSavegameInfo(id):get().info.metadata.gameModDescs
        self.L.execute("api = { type = { SavegameId = { new = function() return {} end } } }")
        saves = self.L.execute("""
          local info = { info = { metadata = { gameModDescs = { { modId = { name = "trains" } } } } } }
          return {
            SaveGameNamespace = { getSavegame = function() return "ns" end },
            findAllSavegames = function(ns) return { { saveName = "feverlink3-resync-3", path = "p" } } end,
            getSavegameInfo = function(id) assert(id.saveGameName == "feverlink3-resync-3"); return { get = function() return info end } end,
          }
        """)
        self.assertEqual(H.worldMods(saves, "feverlink3-resync-3")[1], "trains")
        self.assertIsNone(H.worldMods(saves, "other")[0])


if __name__ == "__main__":
    unittest.main(verbosity=2)
