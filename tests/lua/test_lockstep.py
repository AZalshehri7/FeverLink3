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
import shutil
import sys
import tempfile
import unittest

import lupa.lua52 as lupa

HERE = os.path.dirname(os.path.abspath(__file__))
CONTENT = os.path.join(HERE, "..", "..", "mod", "feverlink3_1", "content")
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
# the command-schema modules of the other areas (feverlink3_cmds_*.lua), by module name
CMDS_SRC = {
    os.path.splitext(f)[0]: read_src(f)
    for f in sorted(os.listdir(CONTENT))
    if f.startswith("feverlink3_cmds_") and f.endswith(".lua")
}

# ---------------------------------------------------------------- mocked engine (one per game)

MOCK = r'''
GAME = { time = %(t0)d, speed = 1, applied = {}, guiQueue = {}, inUpdate = false, nextEntity = %(first)d, deferred = {} }
WORLD = {
  [7]  = { PLAYER = {} },
  [54] = { CONSTRUCTION = { fileName = "depot.con", transf = { [13] = 100, [14] = 200, [15] = 5 } } },
  [55] = { VEHICLE_DEPOT = {} },
  [56] = { CONSTRUCTION = { fileName = "station.con", transf = { [13] = -40.5, [14] = 12, [15] = 0 } } },
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
}
local CT = { GAME_TIME = 1, GAME_SPEED = 2, ACCOUNT = 3, CONSTRUCTION = 4, VEHICLE_DEPOT = 5, TRANSPORT_VEHICLE = 6, LINE = 7, PLAYER = 8 }
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
    GAME.speed = a[1]
    if cb then cb({}, true, {}) end
    return
  end
  if name == "makeScriptingSendEventCmd" then
    EVENTS_SEEN[#EVENTS_SEEN + 1] = { a[2], a[3] }
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
      if GAME.inUpdate then
        -- finding from MPFever (MIT): callbacks are refused inside the simulation update
        if cb then error("Callbacks are currently disallowed") end
        EXECUTE(c, nil)
        return
      end
      GAME.guiQueue[#GAME.guiQueue + 1] = { c, cb }
    end,
  }
  for _, n in ipairs({ "makeGameSetSpeedCmd", "makeScriptingSendEventCmd", "makeVehicleBuyCmd", "makeLineCreateCmd", "makeVehicleSetLineCmd", "makeJournalBookAssetCmd" }) do
    cmd[n] = mk(n)
  end
  return {
    cmd = cmd,
    type = {
      ComponentType = CT,
      JournalEntry = { new = function() return { category = {} } end, Type = { OTHER = 1 } },
      SavegameId = { new = function() return {} end },
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
local function deepcopy(v)
  if type(v) ~= "table" then return v end
  local out = {}
  for k, x in pairs(v) do out[deepcopy(k)] = deepcopy(x) end
  return out
end

-- the game-script state: written by the simulation half, readable from both (copies: no shared references)
local STORED = nil
STATE = {
  get = function(self) return deepcopy(STORED) end,
  set = function(self, v) STORED = deepcopy(v) end,
  subscribeToAllEvents = function() end,
}
GSTATE = { subscribeToAllEvents = function() end }

function LOAD_HALF(label)
  local env = setmetatable({}, { __index = _G })
  env.api = MAKE_API()
  env.package = { loaded = {} }
  env.FEVERLINK3_ENGINE = { adjustLoan = function(d) ACCOUNT.loan = ACCOUNT.loan + d end }   -- the injected loan accessor
  env.ug_require = function(p)
    if p:find("feverlink3_common") then return assert(load(COMMON_SRC, "=common", "t", env))() end
    if p:find("feverlink3_lockstep") then return assert(load(LOCKSTEP_SRC, "=lockstep", "t", env))() end
    if p:find("feverlink3_hud") then return assert(load(HUD_SRC, "=hud", "t", env))() end
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
  SIM, SIM_ENV = LOAD_HALF("sim")
  GUI, GUI_ENV = LOAD_HALF("gui")
  GUI_API = GUI_ENV.api
end

function UI_HOOK_NEW()
  local env = setmetatable({}, { __index = _G })
  env.api = MAKE_API()
  env.package = { loaded = {} }
  env.ug_require = function(p)
    if p:find("feverlink3_common") then return assert(load(COMMON_SRC, "=common", "t", env))() end
    if p:find("feverlink3_lockstep") then return assert(load(LOCKSTEP_SRC, "=lockstep", "t", env))() end
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
    if p:find("feverlink3_common") then return assert(load(COMMON_SRC, "=common", "t", env))() end
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

function SIM_EVENT(src, id, name, param) SIM.handleEvent(nil, STATE, src, id, name, param) end
function FRAME() GUI.guiUpdate(nil, STATE, GSTATE) end
function SIMSTEP()
  local cbs = SAVE_CBS
  SAVE_CBS = {}
  for _, cb in ipairs(cbs) do cb() end
  local q = GAME.guiQueue
  GAME.guiQueue = {}
  for _, c in ipairs(q) do EXECUTE(c[1], c[2]) end
  for _ = 1, GAME.speed do
    for _, e in ipairs(GAME.deferred) do WORLD[e] = { TRANSPORT_VEHICLE = {} } end
    GAME.deferred = {}
    GAME.time = GAME.time + 200
    GAME.inUpdate = true
    SIM.update(nil, STATE, 0.2)
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
        cmds = self.L.table()
        for stem, src in CMDS_SRC.items():
            cmds[stem] = src
        g.CMDS_SRC = cmds
        self.L.execute(MOCK % dict(t0=T0, first=first_entity))
        self.L.execute(HARNESS)

    def use_env(self):
        """The mod reads its environment when its files load: make it this game's."""
        os.environ.update(FEVERLINK3_SESSION_DIR=self.dir, FEVERLINK3_PLAYER_ID=str(self.id), FEVERLINK3_PLAYER_NAME=self.label)

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
        self.C = self.L.execute(COMMON_SRC)
        self.readers = {g.id: self.C.newReader(g.from_game) for g in games}
        self.seq = {g.id: 0 for g in games}
        self.history = {g.id: [] for g in games}
        self.clocks, self.aheads, self.hello, self.reports, self.speed_requests = {}, {}, {}, {}, []
        self.hellos, self.authority, self.saves_done, self.replay_failed, self.refused, self.chats = [], {}, [], [], [], []
        self.started, self.speed, self.pauseAt = False, 0, None

    # launcher -> mod
    def send(self, g, kind, frm, payload):
        self.seq[g.id] += 1
        line = self.C.formatLine(self.seq[g.id], kind, frm, payload)
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
        self.broadcast("session", self.ser(d))

    def future_stamp(self):
        t = max(self.clocks.values())
        return -(-(t + self.lead) // STEP) * STEP

    def checksum_request(self, n, at, paused):
        self.broadcast("checksum_request", self.ser({"n": n, "at": at, "paused": paused}))

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

    # mod -> launcher
    def pump(self):
        for g in self.games:
            for rec in self.C.readerPoll(self.readers[g.id]).values():
                p = self.C.parse(rec.payload)
                if isinstance(p, tuple):
                    p = p[0]
                if rec.kind == "clock":
                    self.clocks[g.id], self.aheads[g.id] = int(p.t), int(p.ahead)
                    self.broadcast("peerclock", self.ser({"from": g.id, "t": int(p.t), "ahead": int(p.ahead)}), exclude=g)
                elif rec.kind == "action":
                    p["from"] = g.id
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
                elif rec.kind == "chat":
                    self.chats.append((g.id, str(p.text)))
                    # the host fills in the sender and relays to everyone but the origin, as a line from 0
                    out = self.ser({"text": str(p.text), "from": g.id, "name": g.label})
                    for o in self.games:
                        if o is not g:
                            self.send(o, "chat", 0, out)
                elif rec.kind == "checksum_report":
                    self.reports.setdefault(int(p.n), {})[g.id] = (int(p.time), {k: v for k, v in p.parts.items()})
                    if g.id == 0 and p.authority is not None:
                        self.authority[int(p.n)] = {k: v for k, v in p.authority.items()}
                elif rec.kind == "speed_request":
                    self.speed_requests.append(int(p.speed))


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
        shipped = [l for l in self.B.lines(self.B.from_game) if l.split(TAB)[1] == "action"]
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

    def test_unresolvable_reference_is_refused_not_applied(self):
        self.start()
        self.tick(10)
        self.B.L.execute("WORLD[9999] = { VEHICLE_DEPOT = {} }")   # a depot that exists in the client game only
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 9999, { vehicles = {} }), function(d, s) BAD_CB = s end)')
        self.tick(40)
        self.assertEqual(len(self.A.applied()), 0, "the host did not buy in a depot it does not have")
        self.assertEqual(len(self.B.applied()), 1)
        self.assertIn("refused", self.A.log())
        self.assertTrue(any(l.split(TAB)[1] == "action_refused" for l in self.A.lines(self.A.from_game)))

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
        expected = "makeVehicleBuyCmd@%d" % self.relay.pauseAt
        self.assertEqual(self.A.applied(), [expected])
        self.assertEqual(self.B.applied(), [expected], "applied at the pause time on the client too")
        va = self.A.L.eval("GAME.applied[1].entity")
        self.assertTrue(self.A.L.eval("BUY_CB ~= nil and BUY_CB[1] == true and BUY_CB[2] == %d" % va))
        self.assertEqual((self.A.time(), self.B.time()), (self.relay.pauseAt, self.relay.pauseAt), "still paused")
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
        self.assertEqual(set(pa) - {"stations", "edges", "nodeConfigs", "vehicles", "names"}, {"time", "balance", "loan", "constructions", "lines"})
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

    def test_truncated_to_game_log_does_not_reapply(self):
        self.start()
        self.tick(10)
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function() end)')
        self.tick(40)
        self.assertEqual(len(self.A.applied()), 1)
        before = self.A.applied()
        self.relay.rewrite_history(self.A)
        self.assertTrue(any(l.split(TAB)[1] == "action" for l in self.A.lines(self.A.to_game)), "the old action line is still in the file")
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
        lines_before = len(self.B.lines(self.B.to_game))
        self.relay.send(self.B, "resync_load", 0, self.relay.ser({"id": 3, "name": "hostsave", "sha256": "00"}))
        self.B.frame()
        self.assertEqual(self.B.L.eval("LOADED"), "hostsave")
        self.assertTrue(self.B.reload_pending())
        self.assertEqual(read_text(os.path.join(self.B.dir, "resync_cursor.txt")), "%d 0" % (lines_before + 1))
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
        # the writer continued its sequence: the launcher saw no restart
        seqs = [int(l.split(TAB)[0]) for l in self.B.lines(self.B.from_game)]
        self.assertEqual(seqs, sorted(set(seqs)))

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
        self.assertFalse(any(l.split(TAB)[1] == "action" for l in self.B.lines(self.B.from_game)), "nothing stamped or shipped")
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
        b = [l for l in self.B.lines(self.B.from_game) if l.split(TAB)[1] == "speed_request"]
        a = [l for l in self.A.lines(self.A.from_game) if l.split(TAB)[1] == "speed_request"]
        self.assertEqual(len(b), 1, b)
        self.assertIn('["speed"]=3', b[0])
        self.assertEqual(len(a), 1, a)
        self.assertIn('["speed"]=0', a[0])


class PureTest(unittest.TestCase):
    """The common module and the core, without any game."""

    def setUp(self):
        os.environ.pop("FEVERLINK3_SESSION_DIR", None)
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.C = self.L.execute(COMMON_SRC)
        self.K = self.L.execute(LOCKSTEP_SRC)
        self.H = self.L.execute(HUD_SRC)

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

    def test_a_failing_accessor_yields_an_error_part_not_a_crash(self):
        L = self.L
        e = L.eval(self.WIDE_ENGINE)(L.table_from({"stations": [{"stations": 1}]}, recursive=True))
        e.stations = L.eval("function() error('boom') end")
        parts, _t = self.K.checksumParts(e, self.C.hashValue)
        self.assertEqual(parts["stations"], "error")

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

    def test_reader_tracks_sequence_and_partial_lines(self):
        r = self.C.newReader("unused")
        recs = self.C.readerFeed(r, "1\tsession\t0\t{}\n2\taction\t5\t{[\"stamp\"]=1}\n3\tclo")
        self.assertEqual([x.kind for x in recs.values()], ["session", "action"])
        self.assertEqual(recs[2]["from"], 5)
        self.assertEqual(int(r.lastSeq), 2)
        recs = self.C.readerFeed(r, "ck\t0\t{}\r\n")
        self.assertEqual([x.kind for x in recs.values()], ["clock"])
        # history re-written after a truncation: nothing below lastSeq is dispatched
        recs = self.C.readerFeed(r, "1\tsession\t0\t{}\n2\taction\t5\t{}\n3\tclock\t0\t{}\n4\tsession\t0\t{}\n")
        self.assertEqual([int(x.seq) for x in recs.values()], [4])
        self.assertEqual(int(r.dropped), 3)

    def test_writer_continues_sequence_from_file(self):
        tmp = tempfile.mkdtemp(prefix="feverlink3-")
        try:
            p = os.path.join(tmp, "from_game.log")
            w = self.C.newWriter(p, 3)
            self.assertTrue(self.C.writerSend(w, "hello", self.L.table_from({"time": 1})))
            self.assertTrue(self.C.writerSend(w, "clock", self.L.table_from({"t": 1})))
            w2 = self.C.newWriter(p, 3)
            self.C.writerSend(w2, "clock", self.L.table_from({"t": 2}))
            lines = read_text(p).splitlines()
            self.assertEqual([l.split(TAB)[0] for l in lines], ["1", "2", "3"])
            self.assertEqual(lines[2], '3\tclock\t3\t{["t"]=2}')
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
