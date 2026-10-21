"""Strict mode for builds made with the game's native tools: the native module holds the tool's command, the mod ships the build
with a stamp, cancels the held command and EVERY game, the originator's included, applies the build at the stamp.

Two mocked games run the real mod (the harness of test_native_capture.py / test_cmds_build.py, imported, not edited). What is new
here is a FAKE NATIVE MODULE (`FakeNative`) that does what native/ does through the two files of docs/NATIVE_LINK.md section 3:
it reads the control files `native_ctl.gui.w<id>.log` and `native_ctl.sim.w<id>.log` (one per writer, read by position: hold_on, hold_off,
release, cancel, ping, the orders), writes `native_events.log` (ready with hooks, holding,
held, released, dropped, pong, error), holds a build command added by a tool at CommandList::Add, hands held commands back or
destroys them at the NEXT Add (any command), and switches holding off after 10 s without a valid line from the mod. A mock
native TOOL fires the engine's tool event (builder.proposalApply, into the interface half's guiHandleEvent) and then adds its
command, which the fake module holds instead of the engine when holding is on.

Time: the mod reads os.clock(); the games get a fake one (`Clock`) that moves 50 ms per interface frame, so seconds-long rules
(a ping every 2 s, a capture dropped after 2 s, the module's 10 s silence) run in a few hundred frames.

Run:  python -I -m unittest discover -s tests/lua -p "test_strict_build.py"
"""
import os
import re
import shutil
import struct
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import lupa.lua52 as lupa  # noqa: E402

import test_lockstep as T  # noqa: E402
import test_native_capture as N  # noqa: E402

TAB = "\t"
STEP = T.STEP
TOOL_SITE = 0x5891B4   # the RVA the fake module reports as the tool's call site


def to_py(v):
    """A Lua table from lupa as dicts / lists."""
    if isinstance(v, tuple):
        v = v[0]
    if hasattr(v, "keys"):
        keys = list(v.keys())
        if keys and all(isinstance(k, (int, float)) for k in keys) and sorted(keys) == list(range(1, len(keys) + 1)):
            return [to_py(v[k]) for k in sorted(keys)]
        return {k: to_py(v[k]) for k in keys}
    return v


class Clock:
    """The one clock of a test: the mod's os.clock() (per game) and the fake module's time."""

    def __init__(self):
        self.t = 100.0
        self.games = []

    def advance(self, dt):
        self.t += dt
        for g in self.games:
            g.L.globals().FAKE_T = self.t


class FakeNative:
    """A stand-in for the native module, in the shape of docs/NATIVE_LINK.md (sections 3 and 4). Not the Rust code: only what
    the mod's contract needs, honoured roughly the way the module honours it."""

    SILENCE = 10.0

    def __init__(self, game, clock, hooks=("apply_command", "command_list_add"), link=True):
        self.game, self.clock, self.hooks = game, clock, list(hooks)
        self.events = os.path.join(game.dir, "native_events.log")
        self.R = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.C = T.load_common(self.R)
        self.outbox = []
        self.ctl_pos = {}   # control file -> [offset, partial line]: each writer is read on its own, by position
        self.holding, self.hold_kinds = False, []
        self.ring, self.next_id = [], 1
        self.last_valid = None
        self.heard = []          # (time, n, kind, payload) of every control line it read (n counts them)
        self.dropped, self.released, self.held = [], [], []
        self.site = TOOL_SITE   # the call site the module reports for the next held command
        self.input_text = None   # the input-level text the module writes for every held command (native_input_<id>.txt); None = an old module
        self.patch_cap = False   # the module reports the patch_next capability in `ready`
        self.patch_refuse = None  # a reason: the module answers patch_next with patch_refused
        self.patches = []        # the patch_next orders it accepted
        self.held_flags = None   # the flags the module read out of the held command (the `ctx` of the held event); None = an old module
        self.fires = False   # the module would fire the tool's completion callback at a cancel (the held event says `done`)
        self.is_open = False
        if link:
            self.open_link()

    # ---- the files
    def lua(self, v):
        if isinstance(v, dict):
            return self.R.table_from({k: self.lua(x) for k, x in v.items()})
        if isinstance(v, (list, tuple)):
            return self.R.table_from([self.lua(x) for x in v])
        return v

    def emit(self, kind, payload):
        self.outbox.append((kind, payload))

    def flush_events(self):
        if not self.outbox:
            return
        text = ""
        for kind, payload in self.outbox:
            text += self.C.formatLine(kind, "native", self.C.serialize(self.lua(payload)))
        self.outbox = []
        T.fastio.CACHE.append(self.events, text.encode("utf-8"))

    def ctl_slots(self):
        """The control files there now as (writer, path), as the module finds them (docs/NATIVE_LINK.md section 3): every writer
        names its own file native_ctl.<half>.w<id>.log; the interface half's first, each half's in the order they were found."""
        found = getattr(self, "_writers", None)
        if found is None:
            found = self._writers = []
        names = sorted(os.listdir(self.game.dir)) if os.path.isdir(self.game.dir) else []
        for n in names:
            m = re.match(r"^native_ctl\.(gui|sim)\.w([0-9a-f]+)\.log$", n)
            if m and n not in [x[2] for x in found]:
                found.append((m.group(1), "%s.w%s" % (m.group(1), m.group(2)), n))
        return [(w, os.path.join(self.game.dir, n)) for half in ("gui", "sim") for h, w, n in found if h == half]
    def newest_slot(self, half):
        slots = [p for w, p in self.ctl_slots() if w.startswith(half + ".")]
        return slots[-1] if slots else os.path.join(self.game.dir, "native_ctl.%s.w0.log" % half)

    @property
    def ctl(self):
        """The interface half's newest control file."""
        return self.newest_slot("gui")

    @property
    def ctl_sim(self):
        """The simulation half's newest control file."""
        return self.newest_slot("sim")

    def open_link(self):
        """The link opens: what the control files hold is history; `ready` is the first event."""
        self.is_open = True
        for _, path in self.ctl_slots():
            pos = self.ctl_pos.setdefault(path, [0, ""])
            if os.path.exists(path):
                pos[0], pos[1] = os.path.getsize(path), ""
        self.emit("ready", {"build": "40420", "version": "0.1.0", "hooks": self.hooks, "poolpatch": False, "clock": False,
                            "kinds": {"build_proposal": 52}, "sites": 12, "patch": self.patch_cap})
        self.flush_events()

    def read_ctl(self):
        for _, path in self.ctl_slots():
            self.read_ctl_file(path)

    def read_ctl_file(self, path):
        """One writer's file, by position: complete lines only, nothing dropped for what it says."""
        data = T.fastio.CACHE.read(path)
        if data is None:
            return
        pos = self.ctl_pos.setdefault(path, [0, ""])
        if len(data) < pos[0]:
            pos[0], pos[1] = 0, ""   # shorter: another file, read from its first byte
        chunk = data[pos[0]:]
        pos[0] += len(chunk)
        text = pos[1] + chunk.decode("utf-8")
        *lines, pos[1] = text.split("\n")
        for line in lines:
            parts = line.split(TAB, 2)
            if len(parts) < 3:
                self.discarded = getattr(self, "discarded", 0) + 1
                continue
            kind, payload = parts[0], parts[2]
            self.lines_read = getattr(self, "lines_read", 0) + 1
            p = to_py(self.C.parse(payload))
            self.handle(self.lines_read, kind, p if isinstance(p, dict) else {})
    # ---- what the module does with a control line
    def handle(self, seq, kind, p):
        now = self.clock.t
        self.last_valid = now
        self.heard.append((now, seq, kind, p))
        if kind == "ping":
            self.emit("pong", {"n": int(p.get("n", 0))})
        elif kind == "hold_on":
            if "command_list_add" not in self.hooks:
                self.emit("error", {"where": "hold_on", "detail": "the command list hook is not installed"})
                return
            kinds = p.get("kinds") or []
            self.holding, self.hold_kinds = True, [k for k in kinds] if isinstance(kinds, list) else []
            self.emit("holding", {"on": True, "kinds": self.hold_kinds, "reason": "hold_on"})
        elif kind == "hold_off":
            self.holding = False
            for e in self.ring:
                if e["phase"] == "waiting":
                    e["phase"], e["reason"] = "release", "hold_off"
            self.emit("holding", {"on": False, "kinds": [], "reason": "hold_off"})
        elif kind == "patch_next":
            if self.patch_refuse:
                self.emit("patch_refused", {"reason": self.patch_refuse})
            else:
                self.patches.append(p)
                self.emit("patched", {"adds": int(p.get("adds", -1))})
        elif kind in ("release", "cancel"):
            e = next((e for e in self.ring if e["id"] == int(p.get("id", -1))), None)
            if e is None:
                self.emit("error", {"where": kind, "detail": "no held command %s" % p.get("id")})
            elif kind == "cancel":
                e["phase"] = "cancel"
            elif e["phase"] != "cancel":
                e["phase"], e["reason"] = "release", "release"
        else:
            self.emit("error", {"where": "link", "detail": "unknown kind " + kind})

    def flush_due(self):
        """At an Add: held commands the mod gave a verdict on go back to the engine or are destroyed, in id order."""
        for e in sorted(self.ring, key=lambda x: x["id"]):
            if e["phase"] == "release":
                self.game.L.globals().NATIVE_RELEASE(e["id"])
                self.released.append(e["id"])
                self.emit("released", {"id": e["id"], "reason": e.get("reason", "release"), "ms": int(self.clock.t * 1000)})
                self.emit("applied", {"id": e["id"], "ok": True})
            elif e["phase"] == "cancel":
                self.game.L.globals().NATIVE_DROP(e["id"])
                self.dropped.append(e["id"])
                self.emit("dropped", {"id": e["id"], "reason": "cancel"})
        self.ring = [e for e in self.ring if e["phase"] == "waiting"]

    def service(self):
        """The service thread: reads the control file, applies the silence rule, writes the events."""
        if not self.is_open:
            return
        self.read_ctl()
        if self.holding and self.last_valid is not None and self.clock.t - self.last_valid > self.SILENCE:
            self.holding = False
            self.emit("error", {"where": "hold", "detail": "silence: holding is off, %d held command(s) go back" % len(self.ring)})
            self.emit("holding", {"on": False, "kinds": [], "reason": "silence"})
            for e in self.ring:
                if e["phase"] == "waiting":
                    e["phase"], e["reason"] = "release", "silence"
        self.flush_events()

    def on_add(self, kind, tool):
        """CommandList::Add hook. Returns the id of the held command, or 0 (the engine takes the command)."""
        if not self.is_open:
            return 0
        self.read_ctl()
        self.flush_due()
        if self.holding and int(kind) == 52 and int(tool) and "build_proposal" in self.hold_kinds and len(self.ring) < 64:
            hid = self.next_id
            self.next_id += 1
            self.ring.append({"id": hid, "phase": "waiting"})
            self.held.append(hid)
            ev = {"id": hid, "kind": 52, "site": self.site, "ms": int(self.clock.t * 1000)}
            if self.fires:
                ev["done"] = True
            if self.held_flags:
                ev["ctx"] = dict(self.held_flags)
            if self.input_text:
                with open(os.path.join(self.game.dir, "native_input_%d.txt" % hid), "w", encoding="utf-8", newline="") as f:
                    f.write(self.input_text)
            self.emit("held", ev)
            return hid
        return 0

    # ---- what a test reads
    def ctl_records(self):
        """Every control line written so far as (writer, kind, payload): the interface half's slots, then the simulation half's."""
        out = []
        for writer, path in self.ctl_slots():
            if os.path.exists(path):
                for line in T.read_text(path).splitlines():
                    parts = line.split(TAB, 2)
                    p = to_py(self.C.parse(parts[2]))
                    out.append((writer, parts[0], p))
        return out

    def ctl_kinds(self):
        return [k for _, k, _ in self.ctl_records()]

    def event_kinds(self):
        return [line.split(TAB)[0] for line in T.read_text(self.events).splitlines()] if os.path.exists(self.events) else []


STRICT_LUA = r'''
os.clock = function() return FAKE_T end

-- the fake module's hooks around the engine's queue: every command added from the interface state passes NATIVE_HOOK, which
-- hands back due held commands first (they enter the queue before the command that arrived) and may hold a tool's command
NATIVE_HELD = {}
local function kindOf(c)
  if c.__cmd == "makeWorldBuildProposalCmd" then return 52 end
  if c.__cmd == "makeScriptingSendEventCmd" then return 27 end
  return 0
end
function NATIVE_ADD(c, cb, tool)
  local id = NATIVE_HOOK and NATIVE_HOOK(kindOf(c), tool and 1 or 0) or 0
  if id > 0 then
    NATIVE_HELD[id] = { c, cb }
    return
  end
  GAME.guiQueue[#GAME.guiQueue + 1] = { c, cb }
end
function NATIVE_RELEASE(id)
  local h = NATIVE_HELD[id]
  NATIVE_HELD[id] = nil
  if h then GAME.guiQueue[#GAME.guiQueue + 1] = h end
end
function NATIVE_DROP(id) NATIVE_HELD[id] = nil end
function NATIVE_HELD_COUNT() local n = 0; for _ in pairs(NATIVE_HELD) do n = n + 1 end; return n end

local BASE_MAKE_API_STRICT = MAKE_API
function MAKE_API()
  local api = BASE_MAKE_API_STRICT()
  local base = api.cmd.sendCommand
  api.cmd.sendCommand = function(c, cb, progress)
    if NATIVE_HOOK then NATIVE_HOOK(kindOf(c), 0) end   -- an Add that is not a tool's: never held, but it runs the module's queue
    return base(c, cb, progress)
  end
  -- a test can make the next read of the player entity fail (a fault inside strict mode's verdict)
  local basePlayer = api.engine.util.getPlayer
  api.engine.util.getPlayer = function()
    if FAIL_NEXT_PLAYER then FAIL_NEXT_PLAYER = nil; error("injected fault") end
    return basePlayer()
  end
  return api
end

-- a native tool: builds the proposal, tells the interface half what it previews (builder.proposalCreate: the real tool is
-- blocked while its command is held, and its apply event only comes after the engine applied the build) and adds its command
TOOLS = {}
function TOOL_MAKE(spec, make)
  local p = (make or SIMPLE_ROAD)(spec)
  TOOLS[#TOOLS + 1] = { p = p, cmd = SIM_ENV.api.cmd.makeWorldBuildProposalCmd(p, nil, false, true) }
  return #TOOLS
end
function TOOL_EVENT(i)
  -- like the game: an interface state that subscribed to nothing gets no tool event (owner test 2026-10-18: the failed subscription by
  -- name left the interface half without builder events, and every build went the release-at-the-click path)
  -- the script's one subscription is made in the simulation state (SUBS): once it subscribed by name, an event outside the names is not
  -- delivered to guiHandleEvent either (0.2.0-pre3 to pre5 subscribed the simulation half to its own names: 0 builds captured in real play)
  if (SUBS.all or SUBS.any) and not (SUBS.all or SUBS.names["builder.proposalCreate"]) then return end
  GUI.guiHandleEvent(nil, STATE, GSTATE, 0, "streetBuilder", "builder.proposalCreate", { TOOLS[i].p, {}, nil })
end
function TOOL_ADD(i, site)
  NATIVE_ADD(TOOLS[i].cmd, nil, site)
end
'''


class StrictGame(N.NativeGame):
    def __init__(self, label, player_id, tmp, first_entity, clock, native=True, **native_args):
        super().__init__(label, player_id, tmp, first_entity)
        clock.games.append(self)
        self.native = FakeNative(self, clock, **native_args) if native else None
        g = self.L.globals()
        g.FAKE_T = clock.t
        if self.native:
            g.NATIVE_HOOK = self.native.on_add
        self.L.execute(STRICT_LUA)
        self.reload()   # fresh halves, so that the interface half's sendCommand sits on the module's hook

    def tool(self, road, event=True, add=True, event_first=True, site=True, make=None):
        """A native tool's confirm click: the tool event and the command's Add, in either order."""
        i = int(self.L.eval("TOOL_MAKE(%s%s)" % (road, (", " + make) if make else "")))
        steps = [("TOOL_EVENT(%d)" % i) if event else None, ("TOOL_ADD(%d, %s)" % (i, "true" if site else "false")) if add else None]
        if not event_first:
            steps.reverse()
        for s in steps:
            if s:
                self.L.execute(s)
        return i

    def tool_event(self, i):
        self.L.execute("TOOL_EVENT(%d)" % i)

    def builds(self):
        return [a for a in self.applied() if a.startswith("makeWorldBuildProposalCmd@")]

    def action_records(self):
        out = []
        for line in self.lines(self.from_game):
            parts = line.split(TAB, 2)
            if len(parts) == 3 and parts[0] == "action":
                out.append(to_py(self.native_C().parse(parts[2])))
        return out

    def native_C(self):
        if not hasattr(self, "_C"):
            self._C = T.load_common(lupa.LuaRuntime(unpack_returned_tuples=True))
        return self._C


class StrictTest(unittest.TestCase):
    """Two games that both have the native module. A table-driven test runs each case on a new pair of games (`cases`, `fresh`)."""

    natives = (True, True)
    # FEVERLINK3_STRICT_CANCEL for the test: "0" = hold-and-release for every build (what most tests here are about), "1" = cancel
    # every kind, None = unset (the default: cancel and replay on every game for street, combined and bulldoze builds)
    strict_cancel = "0"
    # more environment variables for the test (None removes one). The firing of the tool's callback is ON by default since 2026-10-13;
    # most tests here are about the older flows and switch it off (cases that set extra_env themselves say what they need).
    extra_env = {"FEVERLINK3_STRICT_FIRE_DONE": "0"}
    fires = False       # the fake module reports `done` in its held events
    hooks = None        # the hooks the fake module reports in `ready` (None: its default, with the CommandList::Add hook)

    def setUp(self):
        self.open()

    def tearDown(self):
        self.close()

    def open(self):
        self._open = True
        self._saved_env = {k: os.environ.get(k) for k in self.extra_env}
        for k, v in self.extra_env.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
        self._old_cancel = os.environ.get("FEVERLINK3_STRICT_CANCEL")
        if self.strict_cancel is None:
            os.environ.pop("FEVERLINK3_STRICT_CANCEL", None)
        else:
            os.environ["FEVERLINK3_STRICT_CANCEL"] = self.strict_cancel
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-strict-")
        self.clock = Clock()
        self.make_games()
        for g in (self.A, self.B):
            if g.native:
                g.native.fires = self.fires

    def make_games(self):
        args = {} if self.hooks is None else {"hooks": self.hooks}
        self.A = StrictGame("host", 0, self.tmp, 50000, self.clock, native=self.natives[0], **args)
        self.B = StrictGame("client", 1, self.tmp, 51000, self.clock, native=self.natives[1], **args)
        self.relay = T.Relay([self.A, self.B])

    def close(self):
        if getattr(self, "_open", True) is False:
            return
        self._open = False
        for k, v in getattr(self, "_saved_env", {}).items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
        if os.environ.get("FEVERLINK3_TEST_VERBOSE"):
            print("\n--- host mod.log ---\n" + self.A.log() + "\n--- client mod.log ---\n" + self.B.log())
        shutil.rmtree(self.tmp, ignore_errors=True)
        if getattr(self, "_old_cancel", None) is None:
            os.environ.pop("FEVERLINK3_STRICT_CANCEL", None)
        else:
            os.environ["FEVERLINK3_STRICT_CANCEL"] = self._old_cancel

    def fresh(self, **cfg):
        """A new pair of games; cfg overrides the class's settings (strict_cancel, extra_env, fires, natives, hooks...) for it."""
        self.close()
        for k in getattr(self, "_cfg", ()):
            self.__dict__.pop(k, None)
        self._cfg = list(cfg)
        self.__dict__.update(cfg)
        self.open()

    def cases(self, table):
        """Runs every (name, cfg, fn) of a table as a subTest on a new pair of games (the first on the games setUp made when cfg is {})."""
        for i, (name, cfg, fn) in enumerate(table):
            with self.subTest(name):
                if i or cfg:
                    self.fresh(**cfg)
                fn()

    def frames(self, n=1):
        """n interface frames of 50 ms each: both games, the relay and the fake modules' service threads."""
        for _ in range(n):
            self.clock.advance(0.05)
            for g in (self.A, self.B):
                g.frame()
            self.relay.pump()
            for g in (self.A, self.B):
                if g.native:
                    g.native.service()

    def tick(self, n=1):
        for _ in range(n):
            self.frames(4)
            self.A.step()
            self.B.step()

    def start(self, speed=1):
        self.tick(3)
        self.relay.started, self.relay.speed = True, speed
        self.relay.session()
        self.tick(9)

    def pause(self):
        """A started session at x2, paused at a stamp both games reached."""
        self.start(speed=2)
        self.tick(10)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(18)
        self.assertEqual((self.A.time(), self.B.time()), (self.relay.pauseAt, self.relay.pauseAt))

    def road(self, x0, y0, x1, y1):
        return "{ a = { x = %d, y = %d, z = 0 }, b = { x = %d, y = %d, z = 0 } }" % (x0, y0, x1, y1)

    def log(self):
        return self.A.log() + self.B.log()

    def trace(self, g):
        path = os.path.join(g.dir, "trace.log")
        return T.read_text(path)

    def assertSameWorld(self):
        self.assertEqual(self.A.sig(), self.B.sig(), "both games hold the same world")

    def strict_on(self, *games):
        for g in games:
            self.assertTrue(g.native.holding, "%s: the module is holding build commands" % g.label)

    def end_session_of_b(self, role="idle"):
        with open(os.path.join(self.B.dir, "identity.txt"), "w", encoding="utf-8") as f:
            f.write("id=1\nname=client\nrole=%s\n" % role)

    def hide_release_results(self, g):
        """The interface half of `g` never sees the simulation half's release result."""
        g.L.execute("local g = STATE.get; STATE.get = function(self) local v = g(self); if v and v.results then local k = {}; for _, r in ipairs(v.results) do if not r.release then k[#k + 1] = r end end; v.results = k end; return v end")


SPLIT = "{ edge = 110, at = { x = 50, y = 0, z = 0 }, to = { x = 50, y = 80, z = 0 } }"
DEPOT = "{ edge = 110, at = { x = 50, y = 0, z = 0 }, door = { x = 50, y = -30, z = 0 }, params = { seed = 260 } }"
BULLDOZE = "{ cons = { 54 } }"
FIRE = {"extra_env": {"FEVERLINK3_STRICT_FIRE_DONE": "1"}, "fires": True}   # the cancel path needs a tool callback the module can fire
DEFAULTS = {"strict_cancel": None, "extra_env": {"FEVERLINK3_STRICT_FIRE_DONE": None}, "fires": True}   # nothing set


def road_text(template="street/new_medium_road.street_template"):
    """The input-level text of the 300-400 road, with a frozen node the record has no member for."""
    def f(x):
        return "%08x" % struct.unpack("<I", struct.pack("<f", x))[0]

    def v(x, y, z):
        return "v3:%s,%s,%s" % (f(x), f(y), f(z))
    rows = [
        ("proposal.addedNodes.#", "n:2"), ("proposal.addedNodes[0].entity", "ent:new#1"), ("proposal.addedNodes[0].position", v(300, 0, 0)),
        ("proposal.addedNodes[1].entity", "ent:new#2"), ("proposal.addedNodes[1].position", v(400, 0, 0)),
        ("proposal.addedSegments.#", "n:1"), ("proposal.addedSegments[0].entity", "ent:new#3"),
        ("proposal.addedSegments[0].position0", v(300, 0, 0)), ("proposal.addedSegments[0].position1", v(400, 0, 0)),
        ("proposal.addedSegments[0].tangent0", v(100, 0, 0)), ("proposal.addedSegments[0].tangent1", v(100, 0, 0)),
        ("proposal.addedSegments[0].typeIndex", "i32:00000001"), ("proposal.addedSegments[0].roadType", "u32:00000000"),
        ("proposal.addedSegments[0].roadTemplate", 'res:"%s"' % template), ("proposal.addedSegments[0].roadStyle", 'res:"medium"'),
        ("proposal.frozenNodes.#", "n:1"), ("proposal.frozenNodes[0]", "ent:old#5"),
        ("toRemove.#", "n:0"), ("toAdd.#", "n:0"), ("kind", "u8:34"),
    ]
    return chr(10).join(k + chr(9) + val for k, val in rows)


class StrictOnTest(StrictTest):
    def test_a_tool_build_is_held_until_the_stamp_then_released_on_the_originator_and_replayed_by_the_others(self):
        self.start()
        self.strict_on(self.A, self.B)
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(1)
        stamp = self.B.action_records()[0]["stamp"]
        self.assertGreater(stamp, self.B.time(), "the stamp lies ahead of the originator's game")
        self.assertEqual(self.B.native.held, [1])
        self.assertEqual(self.B.native.released, [], "the command is still held: the originator's world does not run ahead")
        self.assertEqual(self.B.native.dropped, [], "the command is never cancelled: a cancelled command kills the tool")
        self.assertNotIn("cancel", self.B.native.ctl_kinds())
        self.assertEqual(self.B.builds_applied(), 0)
        while self.B.time() < stamp - STEP:
            self.tick(1)
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 0), "nobody applies before the stamp")
            self.assertEqual(self.B.native.released, [], "still held")
        self.tick(8)
        self.assertEqual(self.B.native.released, [1], "released at the stamp: the tool's callback runs, the tool is alive")
        self.assertEqual(self.B.native.dropped, [])
        self.assertEqual(self.B.L.eval("NATIVE_HELD_COUNT()"), 0, "no command stays held")
        self.assertEqual(self.A.builds(), ["makeWorldBuildProposalCmd@%d" % stamp], "the other game replayed it at the stamp")
        self.assertEqual(self.B.builds_applied(), 1, "the originator's engine applied its own command once")
        self.assertEqual(self.A.builds_applied(), 1)
        self.assertSameWorld()
        self.assertIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.B.sig())
        self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 1), "shipped once; neither the replay nor the release was shipped back")
        self.assertNotIn("native build captured", self.log(), "the released build was not captured as a new build")
        self.assertNotIn("refused", self.log())
        for line in ("matched the capture of streetBuilder", "held until the stamp", "released at game time", "mode ON",
                     "applied by the engine (accepted true)", "due at t=%d: the native module holds this game's own copy" % stamp):
            self.assertIn(line, self.B.log())
        body = self.B.action_records()[0]
        self.assertEqual(body["category"], "build")
        self.assertNotIn("native=true", str(body["body"]).replace(" ", "").lower(), "the shipped record is not marked as already applied")
        self.assertNotIn("heldid", str(body).lower(), "the local release mark never goes on the wire")
        # the build's timing text travels with the record (owner 2026-10-14: one timing line per build on every game)
        self.assertRegex(str(body["body"]), r"tm.{0,8}held=1 click=", str(body["body"])[:400])
        # several builds in a row, each made while the one before is still held: each lands once, in order
        for i in range(3):
            self.B.tool(self.road(500 + 200 * i, 0, 600 + 200 * i, 0))
            self.tick(2)
        self.tick(20)
        self.assertSameWorld()
        self.assertEqual(len(self.A.builds()), 4)
        self.assertEqual(self.A.builds(), self.B.builds())
        self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 4))
        self.assertEqual([r["seq"] for r in self.B.action_records()], [1, 2, 3, 4])
        self.assertEqual(self.B.native.released, [1, 2, 3, 4])
        self.assertEqual(self.B.L.eval("NATIVE_HELD_COUNT()"), 0)
        # a preview is used once: a click with no preview of its own is not paired with the spent one (no record: refused, pre7 id forks)
        self.B.tool(self.road(1100, 0, 1200, 0), event=False)
        self.tick(14)
        self.assertEqual((self.B.native.released, self.B.native.dropped), ([1, 2, 3, 4], [5]))
        self.assertSameWorld()
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (4, 4))
        # the company ledger (booked through the normal apply path) and every checksum part agree
        self.relay.checksum_request(1, self.relay.future_stamp(), False)
        self.tick(40)
        (_, pa), (_, pb) = self.relay.reports[1][0], self.relay.reports[1][1]
        self.assertEqual(pa, pb, "every checksum part agrees")
        self.assertIn("companies", pa)

    def test_terrain_tools_are_switched_off_during_a_session_and_on_again_at_its_end(self):
        """Owner decision 2026-10-21 (A3): a terrain edit runs on its maker's game alone (pre7: 2 of 8 free-id forks), so the terrain
        menu's tools are disabled while a session runs (api.gui.byId.setEnabled) and the rule is removed when it ends."""
        self.start()
        self.B.L.execute("GUI_API.gui = GUI_API.gui or {}; ENABLED = {}; GUI_API.gui.byId = { setEnabled = function(id, on, only) "
                         "ENABLED[#ENABLED + 1] = tostring(id) .. '=' .. tostring(on) .. '/' .. tostring(only) end }")
        raise_ = "menu.construction.landscaping.landscaping_terrain.::/gui/construction/tools/terrain_raise_tool.res"
        for name in ("idAdded", "visibilityChange"):
            self.B.L.execute("GUI.guiHandleEvent(nil, STATE, GSTATE, 0, '%s', '%s', {})" % (raise_, name))
        self.B.L.execute("GUI.guiHandleEvent(nil, STATE, GSTATE, 0, 'menu.construction.road', 'idAdded', {})")
        calls = lambda: list(self.B.L.eval("ENABLED").values())
        self.assertEqual(calls(), [raise_ + "=false/true"], "switched off once; other tools untouched")
        self.assertIn("terrain tools: " + raise_ + " switched off for this session", self.B.log())
        self.end_session_of_b()
        self.frames(30)
        self.assertEqual(calls(), [raise_ + "=false/true", raise_ + "=nil/true"], "the rule is removed at the session's end")

    def test_a_held_command_is_paired_with_the_preview_of_its_own_tool_only(self):
        def held_event_first():
            self.start()
            i = self.B.tool(self.road(300, 0, 400, 0), event=False)   # the Add first
            self.frames(1)                                             # the mod reads `held` and has no capture yet
            self.assertEqual(self.B.native.dropped, [], "the held command waits for its capture")
            self.B.tool_event(i)                                       # the event follows within the grace
            self.tick(20)
            self.assertEqual((self.B.native.released, self.B.native.dropped), ([1], []))
            self.assertSameWorld()
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
            self.assertEqual(self.B.shipped_count(), 1)

        def two_held_at_once():
            # a real tool is blocked after its click, so this cannot happen; if it ever does, no record may be shipped for the wrong command
            self.start()
            self.B.tool(self.road(300, 0, 400, 0))
            self.B.tool(self.road(500, 0, 600, 0))
            self.tick(20)
            self.assertSameWorld()
            self.assertEqual((self.B.native.released, sorted(self.B.native.dropped)), ([], [1, 2]), "no record for either: both refused")
            self.assertNotIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.A.sig() + self.B.sig())
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 0))

        def preview_without_command():
            self.start()
            self.B.tool(self.road(300, 0, 400, 0), add=False)   # hovering: the preview only, nothing is held
            self.tick(14)
            self.assertEqual(self.B.shipped_count(), 0)
            self.assertEqual([k for k in self.B.native.ctl_kinds() if k in ("cancel", "release")], [])

        def not_held():
            self.start()
            self.B.tool(self.road(300, 0, 400, 0), site=False)   # an Add from a call site that is not a tool's: not held
            self.tick(16)
            self.assertEqual(self.B.native.held, [])
            self.assertSameWorld()
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
            self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 1), "only the old path shipped it")
            self.tick(12)
            self.assertEqual(self.B.shipped_count(), 1, "the preview is not shipped on top of it")

        def site_without_preview():
            # real session 2026-10-13: the lane tool (UI::LaneModifier::Apply) sends no preview; its command was shipped as the upgrade
            # tool's preview of 12 s before
            self.start()
            self.B.tool(self.road(300, 0, 400, 0), add=False)             # a preview of the street tool, never clicked
            self.B.native.site = 0x537EE5
            self.B.L.execute("TOOL_ADD(%d, true)" % 1)
            self.tick(14)
            self.assertEqual((self.B.native.released, self.B.native.dropped), ([], [1]), "no record: refused, never given back")
            self.assertNotIn("matched the capture", self.B.log())
            self.assertIn("held 1 (site 0x537ee5) REFUSED", self.B.log(), "named by its site")
            self.assertSameWorld()

        def other_tools_preview():
            self.start()
            self.B.native.site = 0x4D396F                                  # the bulldozer's site; the preview is the street tool's
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(14)
            self.assertEqual((self.B.native.released, self.B.native.dropped), ([], [1]))
            self.assertIn("which does not issue commands there; not paired", self.B.log())
            self.assertNotIn("matched the capture", self.B.log())

        def newer_preview_uncapturable():
            # the newest preview is the proposal on screen: if it could not be encoded, the held command is refused (bug 8: fail closed),
            # never paired with the record of an earlier preview
            self.start()
            self.B.tool(self.road(300, 0, 400, 0), add=False)          # a good preview
            self.B.L.execute("GUI.guiHandleEvent(nil, STATE, GSTATE, 0, 'streetBuilder', 'builder.proposalCreate', { 'junk' })")
            self.B.tool(self.road(500, 0, 600, 0), event=False)        # the click: its command is held, its preview is the bad one
            self.tick(14)
            self.assertEqual((self.B.native.released, self.B.native.dropped), ([], [1]))
            self.assertSameWorld()
            self.assertNotIn("E 500.00,0.00,0.00 600.00,0.00,0.00", self.A.sig())
            self.assertNotIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.A.sig(), "never the earlier preview's road")
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 0))

        def no_capture():
            self.start()
            self.B.tool(self.road(300, 0, 400, 0), event=False)
            self.tick(14)
            # friend test pre7 2026-10-21: released, it ran on the originator ~16 steps before the others (a free-id fork); now refused
            self.assertEqual((self.B.native.released, self.B.native.dropped), ([], [1]), "refused, never released")
            self.assertNotIn("release", self.B.native.ctl_kinds())
            self.assertIn("REFUSED: no record before the engine applies it (no capture of a tool build)", self.B.log())
            self.assertSameWorld()
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 0), "no game has it")
            self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 0))

        def both_players_at_once():
            self.start()
            self.A.tool(self.road(0, 300, 0, 400))
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(24)
            self.assertSameWorld()
            self.assertIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.A.sig())
            self.assertIn("E 0.00,300.00,0.00 0.00,400.00,0.00", self.B.sig())
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (2, 2))
            self.assertEqual(self.A.builds(), self.B.builds(), "the same builds at the same game times on both games")
            self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (1, 1))
            self.assertEqual((self.A.native.released, self.B.native.released), ([1], [1]))
            self.relay.checksum_request(1, self.relay.future_stamp(), False)
            self.tick(40)
            self.assertEqual(self.relay.reports[1][0][1], self.relay.reports[1][1][1])

        self.cases([("the held event before the tool event", {}, held_event_first), ("two commands held at once", {}, two_held_at_once),
                    ("a preview that gets no held command", {}, preview_without_command), ("a tool build the module did not hold", {}, not_held),
                    ("a site without a preview", {}, site_without_preview), ("a preview of another tool", {}, other_tools_preview),
                    ("a newer preview that cannot be captured", {}, newer_preview_uncapturable), ("no capture", {}, no_capture),
                    ("both players at once", {}, both_players_at_once)])

    def assertRefusedEverywhere(self, chat=None):
        """Bug 8 (fail closed): B's held build failed a check on B. It is never given back to the engine: nothing is shipped, nothing is
        built on either game, the player is told, nothing stays held."""
        self.assertEqual(self.B.native.released, [], "never given back to the engine (fail closed)")
        self.assertEqual(self.B.native.dropped, [1], "failed (this old module has no `fail`: a cancel)")
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 0), "no game has the build")
        self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 0), "nothing was shipped")
        self.assertNotIn("native build captured", self.B.log(), "the simulation half never saw the build either")
        self.assertIn("REFUSED: ", self.B.log())
        self.assertIn("NOT released, nothing is shipped or applied on any game", self.B.log())
        self.assertEqual(self.B.L.eval("NATIVE_HELD_COUNT()"), 0, "nothing stays held")
        self.assertSameWorld()
        self.assertEqual(len(self.relay.chats), 1, "the player is told once")
        self.assertIn("build was cancelled: ", self.relay.chats[0][1])
        if chat:
            self.assertIn(chat, self.relay.chats[0][1])

    def test_a_build_that_fails_a_check_on_its_originator_is_refused_never_released(self):
        reject = "FACTORY_REJECT = function(parts, kind) if kind == 'SimpleProposal' then for _ in ipairs(parts.edgesAdd) do return 'no segment please' end end end"

        def capture_failed():
            self.start()
            self.B.run("FACTORY_REJECT = nil")
            i = int(self.B.L.eval("TOOL_MAKE(%s)" % self.road(300, 0, 400, 0)))
            # a preview FeverLink3 can carry gets no refusal hint (X.refusalHint: guiHandleEvent returns nothing)
            hint = "local r = GUI.guiHandleEvent(nil, STATE, GSTATE, 0, 'streetBuilder', 'builder.proposalCreate', %s); " \
                   "if r == nil then return 'nil' end for t in pairs(r.errorMessages) do return t end"
            self.assertEqual(self.B.L.execute(hint % ("{ TOOLS[%d].p, {}, nil }" % i)), "nil")
            # the tool event arrives with something that is no proposal: the capture fails, the entry keeps its place in the order; the tool
            # shows why in red before the click (errorMessages), and the click (the mock clicks anyway) is refused exactly as without the hint
            self.assertIn("FeverLink3 cannot carry this build to the other players: not a proposal", self.B.L.execute(hint % "{ 'junk' }"))
            self.B.L.execute("TOOL_ADD(%d, true)" % i)
            self.tick(14)
            self.assertIn("REFUSED: the capture of streetBuilder failed", self.B.log())
            self.assertRefusedEverywhere("cannot be recorded yet")

        def cannot_rebuild():
            # friend test 2026-10-16, host street 0:2: "it cannot be rebuilt here (...); release written, the native build goes through"
            self.start()
            i = int(self.B.L.eval("TOOL_MAKE(%s)" % self.road(300, 0, 400, 0)))   # the command exists; now the factory turns strict
            self.B.run(reject)
            self.B.tool_event(i)
            self.B.L.execute("TOOL_ADD(%d, true)" % i)
            self.tick(14)
            self.assertIn("REFUSED: it cannot be rebuilt here", self.B.log())
            self.assertNotIn("release written", self.B.log())
            self.assertNotIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.A.sig() + self.B.sig())
            self.assertRefusedEverywhere("cannot be rebuilt the same way on every game")

        def fault_deciding():
            self.start()
            self.B.tool(self.road(300, 0, 400, 0))     # held; the verdict is made in the next frame
            self.B.run("FAIL_NEXT_PLAYER = true")
            self.tick(14)
            self.assertIn("fault in the verdict on held 1", self.B.log())
            self.assertIn("REFUSED: a fault in the verdict", self.B.log())
            self.assertRefusedEverywhere()
            self.B.tool(self.road(500, 0, 600, 0))     # and the next build is strict again
            self.tick(16)
            self.assertEqual(self.B.native.released, [2])
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
            self.assertSameWorld()

        def refused_at_the_stamp():
            # bug 8 (fail closed): the rule that refuses it at the stamp refused it here; released, the build was applied on this game alone.
            # Failed now as on a veto (this old module has no `fail`: a cancel), never given back to the engine.
            self.start()
            self.B.tool(self.road(300, 0, 400, 0))
            self.frames(2)
            self.assertEqual(self.B.native.held, [1])
            self.B.run(reject)
            self.tick(14)
            self.assertEqual((self.B.native.released, self.B.native.dropped), ([], [1]), "never given back to the engine")
            self.assertIn("was refused at its stamp: FAILED, NOT released", self.B.log())
            self.assertEqual(self.B.builds_applied(), 0, "the refused build is not applied on its originator")
            self.assertEqual(self.B.L.eval("NATIVE_HELD_COUNT()"), 0, "nothing stays held")

        def not_permitted():
            self.start()
            self.relay.send(self.B, "permissions", 0, self.relay.ser({"build": False}))
            self.tick(2)
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(14)
            self.assertEqual(self.B.native.dropped, [1], "held and destroyed: nothing was built")
            self.assertEqual(self.B.builds_applied() + self.A.builds_applied() + self.B.shipped_count(), 0)
            self.assertTrue(any("permission denied: build" in d for _, _, d in self.relay.refused), self.relay.refused)

        def native_fault():
            # native/src/failsafe.rs: a hook that could not do its part says so instead of letting this game run apart
            self.start()
            self.B.native.emit("fault", {"where": "add hook", "action": "repair", "detail": "a tool's command went in at the click"})
            self.B.native.emit("fault", {"where": "add hook", "action": "repair", "detail": "again"})
            self.tick(3)
            self.assertEqual([g for g, d in self.relay.replay_failed], [1], "one repair request for a burst")
            self.assertIn("native module fault [add hook] (repair)", self.relay.replay_failed[0][1])
            self.assertIn("internal fault (add hook)", self.relay.chats[-1][1])
            self.B.native.emit("fault", {"where": "step seed patch", "action": "stop", "detail": "not in place"})
            self.tick(3)
            self.assertIn("leave the session and restart the game", self.relay.chats[-1][1])
            self.assertIn("native module FAULT [step seed patch] (stop)", self.B.log())

        self.cases([("a capture the module could not make", {}, capture_failed), ("a record this game cannot rebuild", {}, cannot_rebuild),
                    ("a fault while deciding", {}, fault_deciding), ("refused at the stamp", {}, refused_at_the_stamp),
                    ("not permitted", {}, not_permitted), ("a native fault", {}, native_fault)])

    def test_a_held_command_given_back_early_or_late_is_never_shipped_or_applied_twice(self):
        def late_stamp():
            # the originator's game stands still before the stamp (a stall): the command stays held (the module's 30 s age and 10 s silence
            # rule are the bounds, and pings keep the silence rule quiet), and goes back when the stamp is reached
            self.start()
            self.B.tool(self.road(300, 0, 400, 0))
            self.frames(60)   # 3 s of interface frames, the simulation does not advance
            self.assertEqual(self.B.native.released, [], "still held at the stamp's door")
            self.assertTrue(self.B.native.holding)
            self.assertIn(self.B.native.heard[-1][2], ("ping", "hold_on", "release"))
            self.tick(10)
            self.assertEqual(self.B.native.released, [1])
            self.assertSameWorld()

        def given_back_early():
            # silence or age: the module hands the command back before the stamp; the replay mark keeps the simulation half from capturing it
            self.start()
            self.B.tool(self.road(300, 0, 400, 0))
            self.frames(2)
            self.assertEqual(self.B.native.held, [1])
            self.B.native.handle(999, "hold_off", {})   # what the module does on its own (silence): holding off, the command goes back
            self.B.native.on_add(0, 0)
            self.tick(16)
            self.assertEqual(self.B.native.released, [1])
            self.assertIn("released by the module (hold_off) before its stamp", self.B.log())
            self.assertEqual(self.B.shipped_count(), 1, "shipped once")
            self.assertNotIn("native build captured", self.B.log())
            self.assertIn("is the early release of the held command of 1:1", self.B.log())
            self.assertSameWorld()
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))

        # real run 2026-10-11: a held host build was never released at its stamp, a resync saved a world without it and the module's 30 s
        # limit put it into the world later, shipped a second time
        def town_build():
            self.start()
            self.A.tool(self.road(300, 0, 400, 0))
            self.tick(1)
            self.assertEqual(self.A.native.held, [1])
            # the engine itself builds something (playerInitiated false: a town growing a road)
            self.A.L.execute("SIM_EVENT(0, 'apply_command', 'onPreBuildProposal', { SIMPLE_ROAD{ a = { x = 700, y = 0, z = 0 }, b = { x = 800, y = 0, z = 0 } }, nil, nil, false })")
            self.assertNotIn("is the early release of the held command", self.A.log())
            self.A.native.ring[0]["phase"], self.A.native.ring[0]["reason"] = "release", "timeout"   # the module's age limit
            self.A.native.on_add(0, 0)
            self.tick(16)
            self.assertIn("is the early release of the held command of 0:1", self.A.log())
            self.assertEqual(self.A.shipped_count(), 1, "shipped once: the late release is not a new build")
            self.assertNotIn("native build captured", self.A.log())

        def resync_while_held():
            self.start()
            self.A.tool(self.road(300, 0, 400, 0))
            self.tick(1)
            self.assertEqual((self.A.native.held, self.A.native.released), ([1], []))
            self.relay.send(self.A, "resync_save", 0, self.relay.ser({"id": 1, "name": "feverlink3-resync-1"}))
            self.tick(3)
            self.assertEqual(self.A.native.released, [1], "released before the save")
            self.assertIn("released before the resync save", self.A.log())
            self.assertEqual([x[:3] for x in self.relay.saves_done], [(0, 1, True)], "the save happened after the engine applied it")
            self.assertIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.A.sig(), "the saved world contains the build")
            self.tick(16)
            self.assertEqual((self.A.builds_applied(), self.A.shipped_count()), (1, 1), "applied and shipped once on the originator")
            self.assertNotIn("native build captured", self.A.log())

        def stamp_passed_without_a_release():
            # 2026-10-15: a release here applied the build steps after the stamp the other game used (other entity ids); the command is
            # failed instead (here a cancel with the callback fired: this fake module has no `fail`) and the games are repaired
            self.start()
            self.A.tool(self.road(300, 0, 400, 0))
            self.tick(1)
            self.hide_release_results(self.A)
            self.tick(24)
            self.assertEqual((self.A.native.released, self.A.native.dropped), ([], [1]), "never released late")
            self.assertIn("WATCHDOG: the stamp", self.A.log())
            self.assertIn("FAILED, NOT released: cancel written (the tool's callback is fired)", self.A.log())
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 1))
            self.assertTrue(any(gid == 0 and "was shipped but failed on its originator" in d for gid, d in self.relay.replay_failed), self.relay.replay_failed)
            self.assertEqual(self.A.shipped_count(), 1)
            self.assertNotIn("native build captured", self.A.log())

        def paused_past_the_limit():
            self.start(speed=2)
            self.tick(6)
            self.A.tool(self.road(300, 0, 400, 0))
            self.tick(1)
            self.assertEqual(self.A.native.held, [1])
            self.frames(500)   # the game is stopped (frames go on, the clock does not) for longer than the limit
            self.assertEqual((self.A.native.released, self.A.native.dropped), ([], [1]), "never given back early")
            self.assertTrue(any("cancelled on every game" in c[1] for c in self.relay.chats))
            self.assertEqual(self.B.builds_applied(), 0)

        self.cases([("a late stamp", {}, late_stamp), ("given back early by the module", {}, given_back_early),
                    ("a town build while held", FIRE, town_build), ("a resync request while held", FIRE, resync_while_held),
                    ("the stamp passed without a release", FIRE, stamp_passed_without_a_release),
                    ("held in a game that stays paused", FIRE, paused_past_the_limit)])

    def test_strict_mode_stops_before_a_reload_or_the_session_end_and_comes_back_by_itself(self):
        def resync_load():
            self.start()
            self.strict_on(self.B)
            n_ctl = len(self.B.native.ctl_records())
            # what native_ctl.log holds at the very moment the game is told to load
            self.B.run("local base = app.loadGame; app.loadGame = function(id, flag) local f = io.open(%s, 'rb'); CTL_AT_LOAD = f and f:read('*a') or ''; if f then f:close() end; return base(id, flag) end"
                       % T.lua_str(self.B.native.ctl))
            self.relay.send(self.B, "resync_load", 0, self.relay.ser({"id": 3, "name": "hostsave", "sha256": "00"}))
            self.B.frame()
            self.assertTrue(self.B.reload_pending())
            self.assertIn("hold_off", str(self.B.L.eval("CTL_AT_LOAD")), "hold_off was already written when the game was told to load")
            self.assertIn("hold_off", [k for _, k, _ in self.B.native.ctl_records()[n_ctl:]], "hold_off was written before the load")
            self.B.native.service()
            self.assertFalse(self.B.native.holding, "the module stopped holding before the scripts were replaced")
            self.B.reload()
            self.relay.session()   # the launcher tells the reloaded game the session state again (as in the resync test)
            self.tick(6)
            self.assertTrue(self.B.native.holding, "the reloaded mod switched strict mode on again by itself")
            kinds = self.B.native.ctl_kinds()
            self.assertEqual(kinds.count("hold_on"), 2)
            self.assertLess(kinds.index("hold_off"), len(kinds) - 1 - kinds[::-1].index("hold_on"), "hold_off came before the second hold_on")
            recs = self.B.native.ctl_records()
            before = {w for w, k, _ in recs if k == "hold_off"}
            after = {w for w, k, _ in recs if k == "hold_on"} - before
            self.assertTrue(before and after and all(w.startswith("gui.") for w in before | after),
                            "the reloaded script writes a control file of its own: %r %r" % (before, after))
            self.B.tool(self.road(300, 0, 400, 0))   # and a build after the reload is strict again
            self.tick(20)
            self.assertEqual(self.B.native.released, [1])
            self.assertSameWorld()
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied(), self.B.shipped_count()), (1, 1, 1))

        def scripts_restarted():
            self.start()
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(16)
            self.assertIn("held", self.B.native.event_kinds())
            self.assertEqual(self.B.native.released, [1])
            before = self.B.native.ctl_kinds()
            self.B.reload()   # no hold_off first (a crash of the scripts, say): the module still holds, and the file holds the history
            self.relay.session()
            self.tick(8)
            after = self.B.native.ctl_kinds()[len(before):]
            self.assertNotIn("release", after, "no verdict for a `held` event of the history")
            self.assertNotIn("cancel", after)
            self.assertIn("ping", after, "it asked whether the module answers")
            self.assertTrue(self.B.native.holding)
            self.assertEqual(self.B.native.ctl_kinds().count("hold_on"), 2, "and switched holding on again by itself")
            self.B.tool(self.road(500, 0, 600, 0))   # and it works: a build after the restart is strict
            self.tick(20)
            self.assertEqual(self.B.native.released, [1, 2])
            self.assertSameWorld()
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (2, 2))

        def session_end():
            self.start()
            self.strict_on(self.B)
            self.B.tool(self.road(300, 0, 400, 0))
            self.frames(2)
            self.assertEqual(self.B.native.held, [1])
            self.end_session_of_b()   # the session ended while the command is held: strict mode stops
            self.frames(30)
            self.assertIn("goes back to the engine before its stamp (the session ended)", self.B.log())
            # B's game is single-player now and sends no more clock lines; the host tells A that B left (its promise no longer holds A)
            self.relay.broadcast("player_left", self.relay.ser({"id": 1}), exclude=self.B)
            self.tick(16)
            self.assertEqual((self.B.native.released, self.B.shipped_count()), ([1], 1), "given back without a second ship")
            self.assertNotIn("native build captured", self.B.log())
            self.assertSameWorld()
            self.assertFalse(self.B.native.holding)
            self.assertIn("hold_off", self.B.native.ctl_kinds())
            self.assertIn("the session ended", self.B.log())
            before = len(self.B.native.ctl_records())
            self.tick(30)   # nothing more is written while there is no session (no pings needed: nothing is held)
            self.assertEqual(len(self.B.native.ctl_records()), before)
            self.end_session_of_b("player")   # a new session: hold_on again
            self.tick(12)
            self.strict_on(self.B)
            self.assertEqual(self.B.native.ctl_kinds().count("hold_on"), 2)

        self.cases([("a resync load", {}, resync_load), ("the scripts restarted", {}, scripts_restarted), ("the session ends", {}, session_end)])

    def test_the_watchdog_keeps_strict_mode_on_while_the_module_answers_and_re_arms_it(self):
        def fast_building():
            # real run 2026-10-14 (rail-crash): the host built every 2-3 s; every build wrote defer / inject_next / release lines, so the
            # mod never pinged (it pinged only after NAT_PING s without any line), no pong came, the watchdog switched strict mode off after
            # 8 s with the module alive, and gave up for good. The next station was applied at the click, the worlds forked, and the
            # guest's next track crashed the host.
            self.start()
            self.strict_on(self.A, self.B)
            t0 = self.clock.t
            for i in range(14):   # ~20 s, a release line every 1.4 s
                self.B.tool(self.road(300 + 200 * i, 0, 400 + 200 * i, 0))
                self.tick(7)
            self.tick(20)
            self.assertGreater(self.clock.t - t0, 20)
            self.strict_on(self.A, self.B)
            self.assertNotIn("stopped answering", self.log())
            self.assertNotIn("mode is off", self.log())
            self.assertNotIn("switched holding off", self.B.log())
            self.assertEqual(self.B.native.ctl_kinds().count("hold_on"), 1, "never switched off and on again")
            self.assertEqual(self.B.native.event_kinds().count("holding"), 1, "the module never switched holding off")
            times = [t for t, _, kind, _ in self.B.native.heard if kind == "ping" and t >= t0]
            gaps = [b - a for a, b in zip(times, times[1:])]
            self.assertGreaterEqual(len(times), 9, "pings went on while builds wrote the file")
            self.assertLessEqual(max(gaps), 2.2, gaps)
            ns = [p["n"] for _, _, kind, p in self.B.native.heard if kind == "ping"]
            self.assertEqual(ns, sorted(set(ns)), "ping numbers grow")
            self.assertEqual(len(self.B.native.heard), len(self.B.native.ctl_records()), "every control line written was read, none dropped")
            self.assertEqual((self.B.native.held, self.B.native.released), (list(range(1, 15)), list(range(1, 15))), "every build held and released")
            self.assertSameWorld()

        def events_without_pongs():
            self.start()
            self.strict_on(self.B)
            native, orig = self.B.native, self.B.native.handle

            def no_pong(seq, kind, p):   # the module stops answering pings (a lost pong) but keeps writing events: no switch-off
                if kind == "ping":
                    native.last_valid = self.clock.t
                    native.heard.append((self.clock.t, seq, kind, p))
                    native.emit("timing", {"id": 0, "what": "alive", "detail": "n=%s" % p.get("n")})
                    return
                orig(seq, kind, p)
            native.handle = no_pong
            self.tick(80)   # 16 s
            self.assertNotIn("stopped answering", self.B.log())
            self.strict_on(self.B)

        def silent_module():
            self.start()
            self.strict_on(self.B)
            self.B.native.is_open = False   # the module reads nothing and writes nothing (but it keeps its state)
            self.tick(50)   # 10 s
            self.assertIn("stopped answering", self.B.log())
            self.assertIn("hold_off", self.B.native.ctl_kinds(), "hold_off written when it gave up")
            self.B.native.is_open = True    # it reads the file again: hold_off, then the pings, each answered
            self.tick(40)   # 8 s: the back-off (5 s) is over and a pong came
            self.assertIn("mode re-armed", self.B.log())
            self.strict_on(self.B)
            self.assertEqual(self.B.native.ctl_kinds().count("hold_on"), 2)
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(20)
            self.assertEqual((self.B.native.held, self.B.native.released), ([1], [1]), "a build after the re-arm is held")
            self.assertSameWorld()
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))

        def game_stands_still():
            self.start()
            self.strict_on(self.B)
            self.clock.advance(11.0)   # the interface stops for 11 s (a long load, a hitch): the module's silence rule switches holding off
            self.B.native.service()
            self.assertFalse(self.B.native.holding)
            self.frames(1)
            self.assertIn("switched holding off (silence)", self.B.log())
            self.assertFalse(self.B.native.holding)
            self.tick(20)   # 4 s: the mod asks again after its back-off
            self.strict_on(self.B)
            self.assertEqual(self.B.native.ctl_kinds().count("hold_on"), 2)
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(20)
            self.assertSameWorld()
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))

        def three_faults():
            self.start()
            for i in range(3):
                self.B.tool(self.road(300 + 200 * i, 0, 400 + 200 * i, 0))
                self.B.run("FAIL_NEXT_PLAYER = true")
                self.tick(14)
            self.assertIn("three faults", self.B.log())
            self.assertFalse(self.B.native.holding)
            self.tick(320)   # 64 s
            self.assertIn("mode re-armed after three faults", self.B.log())
            self.strict_on(self.B)
            n = len(self.B.native.held)
            self.B.tool(self.road(1100, 0, 1200, 0))
            self.tick(20)
            self.assertEqual(len(self.B.native.held), n + 1, "held again")
            self.assertSameWorld()

        self.cases([("fast building", {}, fast_building), ("events without pongs", {}, events_without_pongs),
                    ("a silent module", {}, silent_module), ("a game that stands still past the silence limit", {}, game_stands_still),
                    ("three faults", {}, three_faults)])

    def test_without_a_module_that_can_hold_the_old_path_keeps_the_worlds_equal(self):
        def no_module():
            self.start()
            self.B.tool(self.road(300, 0, 400, 0))   # the tool event reaches the mod, the command goes to the engine
            self.tick(16)
            for g in (self.A, self.B):
                self.assertFalse([n for n in os.listdir(g.dir) if n.startswith("native_ctl.")], "no control file without a module")
                self.assertNotIn("strict:", g.log())
            self.assertSameWorld()
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied(), self.A.shipped_count(), self.B.shipped_count()), (1, 1, 0, 1))

        def no_hold_hook():
            self.start()
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(16)
            self.assertEqual(self.B.native.ctl_records(), [], "nothing is written to a module that cannot hold")
            self.assertEqual(self.B.log().count("no command_list_add hook"), 1, "strict mode stays off and says why once")
            self.assertSameWorld()
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied(), self.B.shipped_count()), (1, 1, 1))

        def module_never_answers():
            self.B.native.is_open = False   # native_events.log exists but nothing answers (an old file of a crashed run)
            self.start()
            self.tick(80)
            kinds = self.B.native.ctl_kinds()
            self.assertEqual(set(kinds), {"ping"}, "never asked to hold")
            # six pings 2.5 s apart, then one every 5 s while it waits for an answer (16 s here)
            self.assertLessEqual(len(kinds), 8, "a few pings, then it only asks now and then")
            self.assertIn("does not answer pings", self.B.log())
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(16)
            self.assertSameWorld()
            self.assertEqual(self.B.shipped_count(), 1, "the old path")
            self.B.native.is_open = True   # the module starts reading late: the pings in the file are answered
            self.tick(40)
            self.assertIn("mode re-armed", self.B.log())
            self.strict_on(self.B)
            self.B.tool(self.road(500, 0, 600, 0))
            self.tick(20)
            self.assertEqual(self.B.native.held, [1], "the build is held now")
            self.assertSameWorld()

        def one_game_without():
            self.A.native.is_open = False   # the host has no module (the file stays, nothing answers); the client holds
            self.A.native.holding = False
            self.start()
            self.B.tool(self.road(300, 0, 400, 0))
            self.A.tool(self.road(0, 300, 0, 400), site=False)
            self.tick(24)
            self.assertSameWorld()
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (2, 2))
            self.assertEqual(self.B.native.released, [1])
            self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (1, 1))

        self.cases([("no module", {"natives": (False, False)}, no_module), ("no hold hook", {"hooks": ("apply_command",)}, no_hold_hook),
                    ("a module that never answers, then answers late", {}, module_never_answers),
                    ("one game without the module", {}, one_game_without)])


class NativeVersusReplayTest(StrictTest):
    """The originator's world gets the tool's NATIVE result (its own command, released at the stamp); every other game gets the
    record REBUILT. Both must end with the same world: the hard cases of a real session (2026-10-11, the worlds diverged from the
    first road that touched an existing one)."""

    def test_the_native_result_and_the_replay_end_the_same(self):
        def split():
            self.start()
            self.B.tool(SPLIT, make="NATIVE_SPLIT")
            self.tick(16)
            self.assertEqual(self.B.native.released, [1])
            self.assertSameWorld()
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
            self.assertIn("E 0.00,0.00,0.00 50.00,0.00,0.00", self.A.sig(), "the old segment was split at the junction on the replay too")
            self.assertNotIn("E 0.00,0.00,0.00 100.00,0.00,0.00", self.A.sig(), "and the segment that was split is gone")
            self.assertEqual(self.relay.vetoes, [])

        def connect_to_the_previous_build():
            # the second record refers to a node the first creates: the peer's dry run waits for the first to land instead of vetoing
            self.start()
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(6)
            self.assertEqual(self.B.native.released, [], "the first is still held (its stamp lies ahead)")
            self.tick(4)   # two seconds of game time: the first build has landed on both games
            self.assertSameWorld()
            nb = int(self.B.lua("FIND_NODE(400, 0, 0)"))
            self.assertNotEqual(nb, int(self.A.lua("FIND_NODE(400, 0, 0)")), "the games number their new nodes differently")
            self.B.tool("{ nodeA = %d, b = { x = 500, y = 100, z = 0 } }" % nb)
            self.tick(16)
            self.assertSameWorld()
            self.assertEqual((self.A.lua("#EDGES_AT(FIND_NODE(400, 0, 0))"), self.B.lua("#EDGES_AT(FIND_NODE(400, 0, 0))")), (2, 2),
                             "one node, two roads, on the replay too")
            self.assertEqual(self.relay.vetoes, [])
            self.assertNotIn("refused", self.log())

        def depot():
            # the released command is applied by the engine; the binders registered at the stamp key what it made, so that a later action
            # (a vehicle bought at the depot, a line through the station) can name it on every game
            self.start()
            self.B.tool(DEPOT, make="NATIVE_DEPOT_ON_ROAD")
            self.tick(48)
            self.assertEqual(self.B.native.released, [1])
            lines = lambda g, p: sorted(x for x in g.sig().splitlines() if x.startswith(p))
            self.assertEqual(lines(self.A, "C "), lines(self.B, "C "), "the same construction in the same place")
            self.assertIn("C depot/road_depot.con@50.00,-30.00,0.00", self.B.sig())
            # (the mock engine adds the depot's entrance node to the tool's own build only; the replay leaves it to the depot's script)
            self.assertTrue(set(lines(self.A, "N ")) <= set(lines(self.B, "N ")), "every street node of the replay is on the originator too")
            self.assertIn("N 50.00,0.00,0.00", lines(self.A, "N "), "the junction on the road exists on the replay")
            for g in (self.A, self.B):
                self.assertIn("1:1#c1/d1", str(g.lua("BOUND_KEYS()")).split(), "%s keyed the depot" % g.label)
            self.assertIn("action 1:1 created 1 CONSTRUCTION entities, keyed", self.B.log())
            self.assertNotIn("native build captured", self.log())
            self.assertNotIn("does not exist here", self.log())
            self.assertEqual((self.B.shipped_count(), self.relay.vetoes), (1, []))

        def depot_entrance_to_an_existing_node():
            # real run 2026-10-11 (Dunwich): the originator's result has the depot's entrance piece to a town node; a road built from that
            # piece was vetoed by the other game ("unresolved segment ...") because its replay of the depot had not made the piece
            self.start()
            n = int(self.B.lua("FIND_NODE(100, 0, 0)"))
            self.B.tool("{ node = %d, door = { x = 150, y = -40, z = 0 }, params = { seed = 261 } }" % n, make="NATIVE_DEPOT_ENTRANCE")
            self.tick(48)
            self.assertEqual(self.B.native.released, [1])
            self.assertSameWorld()
            self.assertIn("E 100.00,0.00,0.00 150.00,-40.00,0.00", self.A.sig(), "the entrance piece exists on the replay too")
            self.assertIn("did not make the street connection", self.A.log(), "the log names the first cause on the replaying game")
            nb = int(self.B.lua("FIND_NODE(150, -40, 0)"))
            self.B.tool("{ nodeA = %d, b = { x = 250, y = -80, z = 0 } }" % nb)
            self.tick(48)
            self.assertSameWorld()
            self.assertEqual((self.relay.vetoes, self.B.native.released), ([], [1, 2]))

        def airfield():
            # real run 2026-10-11: the tool's preview could not be read (the signs' models), the build went through un-vetted and was
            # shipped afterwards as 21 references to nodes nobody else had ("unresolved node")
            self.start()
            self.B.tool("{ a = { x = 600, y = 600, z = 0 }, b = { x = 700, y = 650, z = 0 }, params = { seed = 5 } }", make="NATIVE_AIRFIELD")
            self.tick(1)
            self.assertEqual(self.B.native.released, [], "still held: the record was shipped first, so a veto can still stop it")
            self.assertEqual(len(self.B.action_records()), 1)
            self.assertNotIn("capture of streetBuilder failed", self.B.log())
            self.tick(48)
            self.assertEqual((self.B.native.released, self.relay.vetoes), ([1], []))
            self.assertNotIn("refused", self.log())
            self.assertNotIn("native build captured", self.log())
            lines = lambda g: sorted(x for x in g.sig().splitlines() if x.startswith("C "))
            self.assertEqual(lines(self.A), lines(self.B), "the same airfield on both games")
            self.assertEqual(self.B.shipped_count(), 1)

        def missing_node():
            self.start()
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(10)
            nb = int(self.B.lua("FIND_NODE(400, 0, 0)"))
            # this game lost the first road (and with it the node the next road starts at)
            na_edge = int(self.A.lua("FIND_EDGE(300, 0, 400, 0)"))
            self.A.run("WORLD[%d] = nil; WORLD[FIND_NODE(400, 0, 0)] = nil; WORLD[FIND_NODE(300, 0, 0)] = nil" % na_edge)
            self.B.tool("{ nodeA = %d, b = { x = 500, y = 100, z = 0 } }" % nb, make="NATIVE_ROAD")
            self.tick(16)
            self.assertEqual(self.relay.vetoes, [], "a node that can simply be created is no reason to refuse")
            self.assertIn("lenient: no node at 400.000,0.000,0.000", self.A.log())
            self.assertEqual(self.A.builds_applied(), 2)
            self.assertIsNotNone(self.A.lua("FIND_EDGE(400, 0, 500, 100)"))

        self.cases([("a road that splits a segment", {}, split), ("a road from the node the previous build made", {}, connect_to_the_previous_build),
                    ("a depot with its street", {}, depot), ("a depot entrance to an existing node", {}, depot_entrance_to_an_existing_node),
                    ("an airfield with signs", {}, airfield), ("a missing node", {}, missing_node)])


class VetoTest(StrictTest):
    """Veto-only validation: a game that cannot build the record says so; silence means yes. A refused build exists on NO game."""

    extra_env = FIRE["extra_env"]
    fires = True

    def test_a_game_that_cannot_build_the_record_vetoes_it_and_no_game_builds_it(self):
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))   # the happy path: no veto, no extra message
        self.tick(16)
        self.assertEqual((self.relay.vetoes, self.B.native.released, self.relay.chats), ([], [1], []))
        self.assertSameWorld()
        self.assertIn("held", str(self.B.action_records()[0]["body"]), "the record says its originator holds the command (peers dry-run it)")
        before_b, applied_lines = self.B.sig(), self.A.log().count("makeWorldBuildProposalCmd applied")
        self.A.run("WORLD[110] = nil")   # this game has no such segment: the split cannot be rebuilt here
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(2)
        self.assertEqual(len(self.relay.vetoes), 1, "the veto is out within a tick or two of the record, long before the stamp")
        self.assertLess(self.B.time(), self.B.action_records()[1]["stamp"])
        self.tick(18)
        who, owner, seq, why = self.relay.vetoes[0]
        self.assertEqual((who, owner, seq), (0, 1, 2))
        self.assertIn("unresolved segment", why, "the first cause is named")
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1), "nobody built it")
        self.assertEqual(self.A.log().count("makeWorldBuildProposalCmd applied"), applied_lines, "the replay was dropped on the vetoing game")
        self.assertEqual((self.B.native.released, self.B.native.dropped), ([1], [2]), "cancelled: the build exists on no game")
        self.assertEqual(self.B.sig(), before_b, "the originator's world did not run ahead")
        self.assertIn("VETO of 1:2: this game cannot build it (unresolved segment", self.A.log())
        self.assertIn("cancelled: host could not build it (unresolved segment", self.B.log(), "the originator's log says who and why")
        self.assertIn("VETO of 1:2 by host: unresolved segment", self.B.log())
        self.assertNotIn("vetoed too late: it was applied already", self.A.log())
        self.assertIn("never entered this game's queue: this game vetoed it", self.A.log())
        self.assertNotIn("Player 1", self.log())
        chats = len(self.relay.chats)
        self.relay.send(self.B, "build_veto", 0, self.relay.ser({"seq": 2, "owner": 1, "why": "unresolved node 1,2,3", "from": 0, "name": "host"}))
        self.tick(2)
        self.assertEqual(len(self.relay.chats), chats, "a second veto of the same build adds no second line")
        # the tool of a vetoed build is freed by its fired callback, and the session goes on
        self.B.tool(self.road(500, 0, 600, 0))
        self.tick(20)
        self.assertIn("E 500.00,0.00,0.00 600.00,0.00,0.00", self.A.sig())
        self.assertIn("E 500.00,0.00,0.00 600.00,0.00,0.00", self.B.sig())
        self.assertEqual(self.B.native.released, [1, 3])
        resyncs = len(self.relay.replay_failed)
        # a veto that comes after the release says plainly that the build exists on one game, and asks for a resync
        self.relay.send(self.B, "build_veto", 0, self.relay.ser({"seq": 3, "owner": 1, "why": "unresolved segment 1.000,2.000,0.000 - 3.000,4.000,0.000", "from": 0, "name": "host"}))
        self.tick(2)
        self.assertEqual(len(self.relay.chats), chats + 1)
        text = self.relay.chats[-1][1]
        self.assertIn("client's build exists on their game only", text)
        self.assertIn("the games will be synchronised", text)
        self.assertNotIn("cancelled on every game", text)
        self.assertEqual(len(self.relay.replay_failed), resyncs + 1, "a resync is asked for")
        self.assertIn("came too late", self.B.log())


class CancelFlowTest(StrictTest):
    """Which builds are cancelled and replayed at the stamp by every game, and which are held and released (FEVERLINK3_STRICT_CANCEL,
    FEVERLINK3_STRICT_FIRE_DONE, and whether the module can fire the tool's callback). A held command is cancelled only when the module
    can fire that callback: otherwise a cancel leaves the tool dead."""

    def flow(self, game, spec, make, dropped, released, ticks=24, same=True):
        game.tool(spec, make=make)
        self.tick(ticks)
        self.assertEqual((game.native.dropped, game.native.released), (dropped, released))
        if same:   # (not for a depot: the mock engine adds its entrance segment to the tool's own build, the replay leaves it to its script)
            self.assertSameWorld()

    def test_each_switch_cancels_or_holds_the_builds_it_names(self):
        def defaults():
            # nothing set: the defaults since 2026-10-13 (removals, fire-done on). A demolition is cancelled and replayed at the stamp by every
            # game (real two-window run: 11 of 11 left the checksums identical and the bulldozer usable); roads are held and released
            self.start()
            self.flow(self.B, BULLDOZE, "NATIVE_BULLDOZE", [1], [])
            self.flow(self.B, SPLIT, "NATIVE_SPLIT", [1], [2])

        def refund():
            # the held command's Context names what the engine refunds (refundableEntities, read by the native module and given in the held
            # event's ctx.refund). Friend test 2026-10-16: a bulldozing was cancelled and replayed with a Context that had none, and no game
            # gave money back. The cancelled demolition's replay on every game, the originator's too, carries that game's own ids.
            self.start()
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(24)
            eb, ea = int(self.B.lua("FIND_EDGE(300, 0, 400, 0)")), int(self.A.lua("FIND_EDGE(300, 0, 400, 0)"))
            self.assertNotEqual(ea, eb)
            self.B.native.held_flags = {"align": False, "cleanup": True, "buildings": True, "fields": True, "redo": False, "ie": False, "pi": True,
                                        "refund": [{"e": eb, "k": -1, "r": [1, 11, 0], "n": 339}, {"e": 99999, "k": -1, "r": [1, 11, 0], "n": 339}]}
            self.flow(self.B, "{ edges = { %d } }" % eb, "NATIVE_BULLDOZE", [2], [1])
            self.assertIsNone(self.A.lua("FIND_EDGE(300, 0, 400, 0)"))
            for g, e in ((self.A, ea), (self.B, eb)):
                self.assertEqual(g.lua("REFUND_TEXT(GAME.applied[#GAME.applied].args[2])"), "%d{-1}rev1,11,0/339" % e, g.label)
            self.assertIn("left out on every game", self.B.log(), "99999 names nothing: left out before the record travels")
            self.assertIn("side=originator source=held command ctx{align=false cleanup=true buildings=true fields=true redo=false refund=1}", self.trace(self.B))

        def opt_out():
            self.start()   # FEVERLINK3_STRICT_FIRE_DONE=0: back to hold-and-release for everything
            self.flow(self.B, BULLDOZE, "NATIVE_BULLDOZE", [], [1])

        def plans():
            self.start()   # cancel-and-replay for roads and bulldozing, for every originator
            self.flow(self.B, SPLIT, "NATIVE_SPLIT", [1], [])
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
            self.assertIn("cancel flow", self.B.log())

        def host_only():
            self.start()
            self.flow(self.A, SPLIT, "NATIVE_SPLIT", [1], [])
            self.flow(self.B, self.road(300, 0, 400, 0), None, [], [1])

        def plans_firing():
            # a build is cancelled only when the module says it fires the tool's callback (`done` in the held event)
            self.start()
            self.flow(self.B, SPLIT, "NATIVE_SPLIT", [1], [])
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
            self.B.native.fires = False
            self.flow(self.B, self.road(300, 0, 400, 0), None, [1], [2])
            self.assertIn("cannot fire this tool's callback", self.B.log())

        def removals():
            # real run 2026-10-13: the originator's engine applied its held bulldozer command 1000 ms after the stamp, a house regrew
            # differently. A build that only takes things away is cancelled and replayed at the stamp on every game; anything that adds
            # stays hold-and-release; and a demolition stays hold-and-release when the module cannot fire the callback
            self.start()
            self.flow(self.B, BULLDOZE, "NATIVE_BULLDOZE", [1], [])
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
            self.flow(self.B, SPLIT, "NATIVE_SPLIT", [1], [2])
            self.B.native.fires = False
            self.flow(self.B, "{ cons = { 56 } }", "NATIVE_BULLDOZE", [1], [2, 3])

        def cancel_all():
            # FEVERLINK3_STRICT_CANCEL=1: the first strict flow, for testing sync independent of the tool
            self.start()
            self.tick(4)   # clocks (and the receiver's default need) reach the other game
            self.flow(self.B, SPLIT, "NATIVE_SPLIT", [1], [], ticks=16)
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1), "both games applied the same rebuilt record")
            self.assertIn("cancel flow", self.B.log())
            self.assertEqual(self.relay.vetoes, [])
            body = self.B.action_records()[0]["body"]
            self.assertNotIn("held=true", str(body).replace(" ", ""), "no held marker in this mode")
            # the stamp margin is measured by the receiver and reported back
            self.assertIn('["xs"]=', body, "the record carries the extra it was stamped with")
            self.assertRegex(self.trace(self.A), r"build\.margin 1:1 recv g\d+ stamp \d+ slack=-?\d+ steps extra_used=\d+ target=\d+ sample=\d+ need=\d+ \((grow|keep|shrink)\)")

        def cancel_all_no_veto():
            self.start()
            self.A.run("WORLD[110] = nil")
            self.B.tool(SPLIT, make="NATIVE_SPLIT")
            self.tick(20)
            self.assertEqual(self.relay.vetoes, [], "the originator replays it itself: nothing to cancel, the refusal at the stamp is the old one")

        def veto_without_callback():
            # a veto of a command whose callback cannot be fired was a release until 2026-10-15 (the build then existed on this game only,
            # every other game dropped it, a resync followed); now it is failed, and a module without `fail` (this fake one) gets a cancel
            self.start()
            self.A.run("WORLD[110] = nil")
            self.B.tool(SPLIT, make="NATIVE_SPLIT")
            self.tick(20)
            self.assertEqual(len(self.relay.vetoes), 1)
            self.assertEqual((self.B.native.released, self.B.native.dropped), ([], [1]), "never given back to the engine")
            self.assertIn("FAILED on the veto, NOT released", self.B.log())
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 0), "no game has the build")
            self.assertEqual(self.relay.replay_failed, [], "so no repair")

        fire_plans = dict(FIRE, strict_cancel="plans")
        self.cases([("defaults", DEFAULTS, defaults), ("defaults: a demolition's refund", DEFAULTS, refund),
                    ("opt-out", dict(DEFAULTS, extra_env={"FEVERLINK3_STRICT_FIRE_DONE": "0"}), opt_out),
                    ("plans", {"strict_cancel": "plans"}, plans), ("host", {"strict_cancel": "host"}, host_only),
                    ("plans with the callback fired", fire_plans, plans_firing), ("removals", dict(FIRE, strict_cancel="removals"), removals),
                    ("cancel all", {"strict_cancel": "1"}, cancel_all), ("cancel all: no dry-run veto", {"strict_cancel": "1"}, cancel_all_no_veto),
                    ("a veto without a fireable callback", {}, veto_without_callback)])

    def test_constructions_and_builds_that_cannot_be_captured(self):
        def construction_default():
            # remote constructions are replicated as before (hold-and-release); the player gets an informational chat line
            self.start()
            self.flow(self.B, DEPOT, "NATIVE_DEPOT_ON_ROAD", [], [1], ticks=20, same=False)
            self.assertEqual(self.B.shipped_count(), 1)
            self.assertEqual(len(self.relay.chats), 1)
            self.assertIn("experimental in this test build", self.relay.chats[0][1])

        def constructions_blocked():
            # FEVERLINK3_REMOTE_CONSTRUCTIONS=0 (developers): a build that adds a construction is cancelled at the originator (with a chat
            # line) and replayed by nobody; a road still goes through
            self.start()
            self.flow(self.B, DEPOT, "NATIVE_DEPOT_ON_ROAD", [1], [], ticks=20)
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied(), self.B.shipped_count()), (0, 0, 0), "nothing is shipped")
            self.assertNotIn("C depot/road_depot.con", self.A.sig() + self.B.sig())
            self.assertIn("not supported yet in this test build", self.relay.chats[0][1])
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(24)
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
            self.assertSameWorld()

        def constructions_blocked_without_fire():
            # the record of a build with constructions cannot be rebuilt in this test mode: a failed self-check, refused (bug 8: fail closed);
            # it was released before, the build then existed on this game alone
            self.start()
            self.flow(self.B, DEPOT, "NATIVE_DEPOT_ON_ROAD", [1], [], ticks=20)
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 0))

        def broken_capture(fires):
            # a build whose capture fails cannot be shipped: it is refused with a chat line (bug 8: fail closed, also where the module cannot
            # fire the tool's callback; it was released then, applied on this game alone)
            self.start()
            self.B.native.fires = fires
            self.B.run("FACTORY_REJECT = nil")
            i = int(self.B.L.eval("TOOL_MAKE(%s)" % self.road(300, 0, 400, 0)))
            self.B.L.execute("GUI.guiHandleEvent(nil, STATE, GSTATE, 0, 'streetBuilder', 'builder.proposalCreate', { 'junk' })")
            self.B.L.execute("TOOL_ADD(%d, true)" % i)
            self.tick(14)
            self.assertEqual((self.B.native.dropped, self.B.native.released), ([1], []))
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied(), self.B.shipped_count()), (0, 0, 0))
            self.assertSameWorld()
            if fires:
                self.assertEqual(len(self.relay.chats), 1)
                self.assertIn("cannot be recorded yet in this test build", self.relay.chats[0][1])
                self.assertIn("(the tool's callback is fired)", self.B.log())
            else:
                self.assertIn("this module cannot fire the tool's callback", self.B.log(), "said, not hidden")

        def edge_object():
            # a stop or signal gets its model when the build runs and its record could not be completed from the held command's text: no
            # record before the build, so refused like a build without a capture (pre7 id forks); the player is told once
            self.start()
            self.flow(self.B, "{ edge = 130, param = 0.75 }", "NATIVE_SIGNAL", [1], [])
            self.assertIn("has no record before the build", self.B.log())
            self.assertEqual(len(self.relay.chats), 1)
            self.assertIn("could not be carried to the other players", self.relay.chats[0][1])

        blocked = {"extra_env": {"FEVERLINK3_REMOTE_CONSTRUCTIONS": "0"}}
        self.cases([("constructions by default", {}, construction_default), ("constructions blocked", dict(blocked, fires=True), constructions_blocked),
                    ("constructions blocked, no fireable callback", blocked, constructions_blocked_without_fire),
                    ("a broken capture", FIRE, lambda: broken_capture(True)), ("a broken capture, no fireable callback", FIRE, lambda: broken_capture(False)),
                    ("an edge object", FIRE, edge_object)])


class PausedGraceTest(StrictTest):
    """A build made while the session is PAUSED: the originator holds its command for a real-time grace, the peers dry-run the record
    and park it, and only the originator's commit makes every game apply it (real run 2026-10-11: a veto came too late and the
    guest's game built a road the host could not)."""

    extra_env = FIRE["extra_env"]
    fires = True

    def test_a_build_made_in_a_pause_is_applied_on_every_game_only_after_its_commit(self):
        def grace_then_commit():
            self.pause()
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(1)
            self.assertEqual(self.B.native.released, [], "the originator holds its command through the grace")
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 0), "the peer has parked the replay: nothing applied yet")
            self.assertEqual(self.relay.commits, [])
            self.assertIn("parked until its originator commits it", self.A.log())
            self.assertIn("for a veto before it is committed", self.B.log())
            self.tick(14)
            stamp = self.relay.pauseAt + STEP
            self.assertEqual(self.relay.commits, [(1, 1, 1, stamp)], "one commit, stamped by the host one step past where the games stand")
            self.assertEqual(self.B.native.released, [1])
            self.assertEqual(self.A.builds(), ["makeWorldBuildProposalCmd@%d" % stamp])
            # hold-and-release: the originator's own command is released at the stamp and the engine applies it at its next tick, once
            self.assertEqual(self.B.builds_applied(), 1)
            self.assertEqual((self.A.speed(), self.B.speed()), (0, 0), "still paused")
            self.assertSameWorld()
            self.assertEqual(self.relay.vetoes, [], "a peer that never answers is no veto")
            self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 1))
            self.assertIn("committed by its originator", self.A.log())
            m = re.search(r"waits (\d+) ms \(round trip about (\d+) ms\)", self.B.log())
            self.assertIsNotNone(m, "the grace is chosen from the round trip and logged")
            grace, rtt = int(m.group(1)), int(m.group(2))
            self.assertTrue(250 <= grace <= 1500 and grace >= min(1500, 2 * rtt), (grace, rtt))

        def veto_in_the_grace():
            self.pause()
            before = self.B.sig()
            self.A.run("WORLD[110] = nil")   # the host cannot build this split
            self.B.tool(SPLIT, make="NATIVE_SPLIT")
            self.tick(20)
            self.assertEqual(len(self.relay.vetoes), 1)
            self.assertEqual(self.relay.commits, [], "no commit after a veto")
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 0))
            self.assertEqual((self.B.native.released, self.B.native.dropped), ([], [1]), "cancelled: the build exists on no game")
            self.assertEqual(self.B.sig(), before)
            # one chat line for this vetoed build, from the originator: cancelled everywhere, who and why in words
            self.assertEqual(len(self.relay.chats), 1)
            who, text = self.relay.chats[0]
            self.assertEqual(who, 1)
            self.assertIn("client's build was cancelled on every game", text)
            self.assertIn("missing on host's game", text)
            self.assertNotIn("unresolved", text, "the technical text stays in the log")
            self.assertNotIn("Player 1", text)
            self.assertIn("[FeverLink3]", text)
            self.assertIn("unresolved segment", self.B.log())
            self.assertEqual(self.A.log().count("chat client: [FeverLink3]"), 1, "the other game gets that one line")

        def unpaused_in_the_grace():
            self.pause()
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(1)
            self.assertEqual(self.relay.commits, [])
            self.relay.pauseAt = None
            self.relay.session()
            self.tick(30)
            self.assertEqual(len(self.relay.commits), 1)
            stamp = self.relay.commits[0][3]
            self.assertEqual(self.A.builds(), ["makeWorldBuildProposalCmd@%d" % stamp], "the peer applied it at the commit's stamp")
            self.assertEqual(self.B.builds(), ["makeWorldBuildProposalCmd@%d" % stamp], "so did the originator, once")
            self.assertSameWorld()
            self.assertIn("no longer paused", self.B.log())

        def commit_never_comes():
            self.pause()
            orig = self.relay.send
            self.relay.send = lambda g, kind, frm, payload: None if kind == "build_commit" else orig(g, kind, frm, payload)
            self.B.tool(self.road(300, 0, 400, 0))
            self.frames(260)   # 13 s of interface frames
            self.assertIn("originator never committed it: dropped", self.A.log())
            self.assertEqual(self.A.builds_applied(), 0)

        def second_build_needs_the_first():
            self.pause()
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(14)
            nb = int(self.B.lua("FIND_NODE(400, 0, 0)"))
            self.B.tool("{ nodeA = %d, b = { x = 500, y = 100, z = 0 } }" % nb)
            self.tick(14)
            self.assertSameWorld()
            self.assertEqual((self.relay.vetoes, self.B.native.released, len(self.relay.commits)), ([], [1, 2], 2))

        def depot_then_road_from_its_street_end():
            self.pause()
            n = int(self.B.lua("FIND_NODE(100, 0, 0)"))
            self.B.tool("{ node = %d, door = { x = 150, y = -40, z = 0 }, params = { seed = 261 } }" % n, make="NATIVE_DEPOT_ENTRANCE")
            self.tick(48)
            self.assertEqual(self.B.native.released, [1])
            self.assertSameWorld()
            nb = int(self.B.lua("FIND_NODE(150, -40, 0)"))
            self.B.tool("{ nodeA = %d, b = { x = 250, y = -80, z = 0 } }" % nb)
            self.tick(48)
            self.assertSameWorld()
            self.assertEqual((self.relay.vetoes, self.B.native.released), ([], [1, 2]))

        self.cases([("the grace, then the commit", {}, grace_then_commit), ("a veto in the grace", {}, veto_in_the_grace),
                    ("unpaused in the grace", {}, unpaused_in_the_grace), ("the commit never comes", {}, commit_never_comes),
                    ("a second build that needs the first", {}, second_build_needs_the_first),
                    ("a depot, then a road from its street end", {}, depot_then_road_from_its_street_end)])


class TraceLogTest(StrictTest):
    """trace.log (FEVERLINK3_TRACE, on by default): one self-contained block per build and a line per checksum round, in the session folder;
    every applied build also has one `build <key> (replay|originator): ...` line in mod.log of every run."""

    def external_ctl_line(self, g, kind, payload_text):
        """A control line written by another simulation-half writer (patch_next), in a file of its own."""
        n = 1
        while os.path.exists(os.path.join(g.dir, "native_ctl.sim.wfeed%d.log" % n)):
            n += 1
        with open(os.path.join(g.dir, "native_ctl.sim.wfeed%d.log" % n), "a", encoding="utf-8", newline="") as f:
            f.write(TAB.join([kind, str(g.player_id if hasattr(g, "player_id") else 1), payload_text]) + chr(10))

    def test_every_build_leaves_its_trace(self):
        def plain_road():
            self.start()
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(20)
            self.assertEqual(self.B.builds_applied(), 1)
            self.assertIn("build.input held=1 NOT AVAILABLE (missing)", self.trace(self.B), "a held command without an input text says so")
            self.assertIn("source=none (old native module", self.trace(self.B), "an old module without flags keeps the capture's guess")
            self.assertRegex(self.A.log(), r"build 1:1 \(replay\): plan=street cons=0 rem=0 ")
            self.assertNotIn("removes [", self.A.log())

        def flags_and_input_text():
            # the preview a build is captured from has no Context; the native module reads the held command's own flags (a stop tool builds
            # with cleanup = false, a road tool with true) and the shipped record, and so every game's replay, uses them. The input-level
            # text of the held command is read, counted, compared with the record and shipped with it.
            self.start()
            self.strict_on(self.A, self.B)
            self.B.native.held_flags = {"align": False, "cleanup": True, "buildings": False, "fields": True, "redo": True, "ie": True, "pi": True}
            self.B.native.input_text = road_text()
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(24)
            body = self.B.action_records()[0]["body"]
            self.assertIn('[2]={["align"]=false,["buildings"]=false,["cleanup"]=true,["fields"]=true,["redo"]=true},[3]=true,[4]=true', str(body))
            tb, ta = self.trace(self.B), self.trace(self.A)
            self.assertIn("side=originator source=held command ctx{align=false cleanup=true buildings=false fields=true redo=true} ie=true pi=true", tb)
            self.assertIn("side=replay source=record ctx{align=false cleanup=true buildings=false fields=true redo=true}", ta)
            self.assertIn("build.input held=1 bytes=", tb)
            self.assertIn("counts[cons=0 rem=0 nodes=2 segs=1 rmNodes=0", tb)
            self.assertIn("build.portable held=1 side=originator diffs=1 [frozenNodes in the text: 1 (the record has no such member)]", tb)
            self.assertIn("build.portable 1:1 side=replay diffs=1 [frozenNodes in the text", ta)
            self.assertIn("this game's first proposal counts[", ta)

        def a_difference_is_named():
            self.start()
            self.B.native.input_text = road_text("street/other.street_template")
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(24)
            self.assertIn("segment 1: template text street/other.street_template, record street/new_medium_road.street_template", self.trace(self.B))

        def depot_record():
            self.start()
            self.B.tool(DEPOT, make="NATIVE_DEPOT_ON_ROAD")
            self.tick(40)
            self.assertRegex(self.A.log(), r"build 1:1 \(replay\): plan=\w+ cons=1 rem=0 nodes=\d+ segs=\d+ rmSegs=\d+ objs=\d+; adds \[[^\]]+\]; verdict .*; ie=\w+ pi=\w+")
            self.assertRegex(self.B.log(), r"build 1:1 \(originator\): plan=\w+ cons=1 ")
            self.relay.checksum_request(1, self.relay.future_stamp(), False)
            self.tick(40)
            a, b = self.trace(self.A), self.trace(self.B)
            self.assertRegex(b, r"gui build\.ship 1:1 held=1 site=\S+ done=\w+ tool=\S+ plan=\w+ stamp=\d+ now=\d+ lead=\d+ extra=\w+ bytes=\d+ capture_ms=[\d.]+ balance=\S+ record=\{")
            for pattern in (r"sim build\.record 1:1 role=replay from=1 stamp=\d+ now=\d+ late=0 balance=\S+ record=\{", r"sim build\.decode 1:1 ok=true ms=[\d.]+",
                            r"build\.plan 1:1 plan=\w+ form=1 built ms=[\d.]+", r"build\.factory 1:1 plan=\w+ form=1 ok ms=[\d.]+ ie=\w+ pi=\w+ dust=\w+ counts\[",
                            r"sim build\.send 1:1 ie=\w+ pi=\w+ dust=\w+ ctx\{", r"sim build\.bind 1:1 ok=true steps=\d+ made=\d+ \[\d+\{[A-Z_,]+\}",
                            r"sim build\.result 1:1 role=replay kind=build plan=\w+ verdict=applied announced=yes entities\[.*\+CONSTRUCTION\[1:\d+\]",
                            r"sim checksum t=\d+ light=false lanes=true items\{[^}]*\} parts=\{"):
                self.assertRegex(a, pattern)
            self.assertIn("native ", b)

        def patches():
            # patch_next is written from the simulation half to its own file: the interface half's orders keep their own numbering
            self.start()
            self.strict_on(self.A, self.B)
            self.external_ctl_line(self.B, "patch_next", '{["adds"]=3,["frozen"]={[1]=1}}')
            self.tick(2)
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(24)
            self.assertEqual(self.B.native.patches, [{"adds": 3, "frozen": [1]}])
            self.assertEqual(self.B.native.released, [1], "the release written after it was heard")
            self.assertIn("build.patch patched", self.trace(self.B), "the module's answers to a patch are traced")
            self.B.native.patch_refuse = "the node count differs"
            self.external_ctl_line(self.B, "patch_next", '{["adds"]=5,["frozen"]={[1]=1}}')
            self.tick(4)
            self.assertIn("build.patch patch_refused", self.trace(self.B))
            self.assertIn("the node count differs", self.trace(self.B))

        def trace_off():
            self.start()
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(30)
            for g in (self.A, self.B):
                self.assertFalse(os.path.exists(os.path.join(g.dir, "trace.log")))

        self.cases([("a plain road", {}, plain_road), ("the held command's flags and input text", {}, flags_and_input_text),
                    ("a difference between the text and the record", {}, a_difference_is_named), ("a depot's full record", {}, depot_record),
                    ("patches", {}, patches), ("FEVERLINK3_TRACE=0", {"extra_env": {"FEVERLINK3_TRACE": "0"}}, trace_off)])


if __name__ == "__main__":
    unittest.main()
