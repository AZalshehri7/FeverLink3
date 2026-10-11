"""Strict mode for builds made with the game's native tools: the native module holds the tool's command, the mod ships the build
with a stamp, cancels the held command and EVERY game, the originator's included, applies the build at the stamp.

Two mocked games run the real mod (the harness of test_native_capture.py / test_cmds_build.py, imported, not edited). What is new
here is a FAKE NATIVE MODULE (`FakeNative`) that does what native/ does through the two files of docs/NATIVE_LINK.md section 3:
it reads `native_ctl.log` (hold_on, hold_off, release, cancel, ping), writes `native_events.log` (ready with hooks, holding,
held, released, dropped, pong, error), holds a build command added by a tool at CommandList::Add, hands held commands back or
destroys them at the NEXT Add (any command), and switches holding off after 10 s without a valid line from the mod. A mock
native TOOL fires the engine's tool event (builder.proposalApply, into the interface half's guiHandleEvent) and then adds its
command, which the fake module holds instead of the engine when holding is on.

Time: the mod reads os.clock(); the games get a fake one (`Clock`) that moves 50 ms per interface frame, so seconds-long rules
(a ping every 2 s, a capture dropped after 2 s, the module's 10 s silence) run in a few hundred frames.

Run:  python -I -m unittest discover -s tests/lua -p "test_strict_build.py"
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
        self.ctl = os.path.join(game.dir, "native_ctl.log")
        self.events = os.path.join(game.dir, "native_events.log")
        self.R = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.C = self.R.execute(T.COMMON_SRC)
        self.ev_seq = 0
        self.outbox = []
        self.ctl_off, self.ctl_seq, self.partial = 0, 0, ""
        self.holding, self.hold_kinds = False, []
        self.ring, self.next_id = [], 1
        self.last_valid = None
        self.heard = []          # (time, seq, kind, payload) of every control line it accepted
        self.dropped, self.released, self.held = [], [], []
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
            self.ev_seq += 1
            text += self.C.formatLine(self.ev_seq, kind, "native", self.C.serialize(self.lua(payload)))
        self.outbox = []
        with open(self.events, "a", encoding="utf-8", newline="") as f:
            f.write(text)

    def open_link(self):
        """The link opens: what native_ctl.log holds is history; `ready` is the first event."""
        self.is_open = True
        if os.path.exists(self.ctl):
            data = open(self.ctl, "rb").read()
            self.ctl_off = len(data)
            for line in data.decode("utf-8").splitlines():
                if line.split(TAB)[0].isdigit():
                    self.ctl_seq = max(self.ctl_seq, int(line.split(TAB)[0]))
        self.emit("ready", {"build": "40420", "version": "0.1.0", "hooks": self.hooks, "poolpatch": False, "clock": False,
                            "kinds": {"build_proposal": 52}, "sites": 12})
        self.flush_events()

    def read_ctl(self):
        if not os.path.exists(self.ctl):
            return
        with open(self.ctl, "rb") as f:
            f.seek(self.ctl_off)
            chunk = f.read()
        self.ctl_off += len(chunk)
        text = self.partial + chunk.decode("utf-8")
        *lines, self.partial = text.split("\n")
        for line in lines:
            parts = line.split(TAB, 3)
            if len(parts) < 4 or not parts[0].isdigit():
                continue
            seq, kind, payload = int(parts[0]), parts[1], parts[3]
            if seq <= self.ctl_seq:
                continue
            self.ctl_seq = seq
            p = to_py(self.C.parse(payload))
            self.handle(seq, kind, p if isinstance(p, dict) else {})

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
            self.emit("held", {"id": hid, "kind": 52, "site": TOOL_SITE, "ms": int(self.clock.t * 1000)})
            return hid
        return 0

    # ---- what a test reads
    def ctl_records(self):
        out = []
        if os.path.exists(self.ctl):
            for line in T.read_text(self.ctl).splitlines():
                parts = line.split(TAB, 3)
                p = to_py(self.C.parse(parts[3]))
                out.append((int(parts[0]), parts[1], p))
        return out

    def ctl_kinds(self):
        return [k for _, k, _ in self.ctl_records()]

    def event_kinds(self):
        return [line.split(TAB)[1] for line in T.read_text(self.events).splitlines()] if os.path.exists(self.events) else []


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
            parts = line.split(TAB, 3)
            if len(parts) == 4 and parts[1] == "action":
                out.append(to_py(self.native_C().parse(parts[3])))
        return out

    def native_C(self):
        if not hasattr(self, "_C"):
            self._C = lupa.LuaRuntime(unpack_returned_tuples=True).execute(T.COMMON_SRC)
        return self._C


class StrictTest(unittest.TestCase):
    """Two games that both have the native module."""

    natives = (True, True)

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-strict-")
        self.clock = Clock()
        self.A = StrictGame("host", 0, self.tmp, 50000, self.clock, native=self.natives[0])
        self.B = StrictGame("client", 1, self.tmp, 51000, self.clock, native=self.natives[1])
        self.relay = T.Relay([self.A, self.B])

    def tearDown(self):
        if os.environ.get("FEVERLINK3_TEST_VERBOSE"):
            print("\n--- host mod.log ---\n" + self.A.log() + "\n--- client mod.log ---\n" + self.B.log())
        shutil.rmtree(self.tmp, ignore_errors=True)

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

    def road(self, x0, y0, x1, y1):
        return "{ a = { x = %d, y = %d, z = 0 }, b = { x = %d, y = %d, z = 0 } }" % (x0, y0, x1, y1)

    def log(self):
        return self.A.log() + self.B.log()

    def assertSameWorld(self):
        self.assertEqual(self.A.sig(), self.B.sig(), "both games hold the same world")

    def strict_on(self, *games):
        for g in games:
            self.assertTrue(g.native.holding, "%s: the module is holding build commands" % g.label)


class StrictOnTest(StrictTest):
    # ---- (a) the build is applied by every game at the stamp, the originator included

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
        self.assertEqual(self.B.native.released, [1], "released at the stamp")
        self.assertEqual(self.B.native.dropped, [])
        self.assertEqual(self.A.builds(), ["makeWorldBuildProposalCmd@%d" % stamp], "the other game replayed it at the stamp")
        self.assertEqual(self.B.builds_applied(), 1, "the originator's engine applied its own command once")
        self.assertEqual(self.A.builds_applied(), 1)
        self.assertSameWorld()
        self.assertIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.B.sig())
        self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 1), "shipped once; neither the replay nor the release was shipped back")
        self.assertNotIn("native build captured", self.log(), "the released build was not captured as a new build")
        self.assertNotIn("refused", self.log())
        self.assertIn("matched the capture of streetBuilder", self.B.log())
        self.assertIn("held until the stamp", self.B.log())
        self.assertIn("released at game time", self.B.log())
        self.assertIn("mode ON", self.B.log())
        self.assertIn("applied by the engine (accepted true)", self.B.log())
        self.assertIn("due at t=%d: the native module holds this game's own copy" % stamp, self.B.log())
        body = self.B.action_records()[0]
        self.assertEqual(body["category"], "build")
        self.assertNotIn("native=true", str(body["body"]).replace(" ", "").lower(), "the shipped record is not marked as already applied")
        self.assertNotIn("heldid", str(body).lower(), "the local release mark never goes on the wire")

    def test_the_originators_tool_is_alive_after_each_build(self):
        # the tool's own callback runs when its command is released: the fake tool is "alive" when its held command went back
        self.start()
        for i in range(3):
            self.B.tool(self.road(300 + 150 * i, 0, 400 + 150 * i, 0))
            self.tick(12)
            self.assertEqual(self.B.native.released, list(range(1, i + 2)), "the tool's command %d was handed back to the engine" % (i + 1))
            self.assertEqual(self.B.L.eval("NATIVE_HELD_COUNT()"), 0, "no command stays held")
        self.assertEqual(self.B.native.dropped, [])
        self.assertSameWorld()

    def test_the_company_ledger_and_every_checksum_part_agree_after_a_strict_build(self):
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(16)
        self.relay.checksum_request(1, self.relay.future_stamp(), False)
        self.tick(40)
        (_, pa), (_, pb) = self.relay.reports[1][0], self.relay.reports[1][1]
        self.assertEqual(pa, pb, "every checksum part agrees")
        self.assertIn("companies", pa, "the originator booked the build in the company ledger through the normal apply path")

    def test_several_builds_in_a_row_each_land_once_in_order(self):
        self.start()
        for i in range(3):
            self.B.tool(self.road(300 + 150 * i, 0, 400 + 150 * i, 0))
            self.tick(2)
        self.tick(20)
        self.assertSameWorld()
        self.assertEqual(len(self.A.builds()), 3)
        self.assertEqual(self.A.builds(), self.B.builds())
        self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 3))
        self.assertEqual([r["seq"] for r in self.B.action_records()], [1, 2, 3])
        self.assertEqual(self.B.native.released, [1, 2, 3])

    def test_two_commands_held_at_once_are_all_given_back_rather_than_paired_with_one_preview(self):
        # a real tool is blocked after its click, so this cannot happen; if it ever does, no record may be shipped for the
        # wrong command
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))
        self.B.tool(self.road(500, 0, 600, 0))
        self.tick(20)
        self.assertSameWorld()
        self.assertEqual(sorted(self.B.native.released), [1, 2])
        self.assertEqual(self.B.native.dropped, [])
        self.assertIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.A.sig())
        self.assertIn("E 500.00,0.00,0.00 600.00,0.00,0.00", self.A.sig())
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (2, 2))

    def test_the_held_event_read_before_the_tool_event_still_pairs(self):
        self.start()
        i = self.B.tool(self.road(300, 0, 400, 0), event=False)   # the Add first
        self.frames(1)                                             # the mod reads `held` and has no capture yet
        self.assertEqual(self.B.native.dropped, [], "the held command waits for its capture")
        self.B.tool_event(i)                                       # the event follows within the grace
        self.tick(20)
        self.assertEqual(self.B.native.released, [1])
        self.assertEqual(self.B.native.dropped, [])
        self.assertSameWorld()
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
        self.assertEqual(self.B.shipped_count(), 1)

    def test_a_depot_the_originator_built_with_the_tool_is_keyed_on_the_originator_too(self):
        # the released command is applied by the engine; the binders registered at the stamp key what it made, so that a later
        # action (a vehicle bought at the depot, a line through the station) can name it on every game
        self.start()
        self.B.tool("{ edge = 110, at = { x = 50, y = 0, z = 0 }, door = { x = 50, y = -30, z = 0 }, params = { seed = 260 } }", make="NATIVE_DEPOT_ON_ROAD")
        self.tick(16)
        self.assertEqual(self.B.native.released, [1])
        # (the worlds are not compared: the mock engine adds the depot's entrance segment to the tool's own build, while the
        # replay leaves it to the depot's script, which the mock does not run)
        self.assertIn("C depot/road_depot.con@50.00,-30.00,0.00", self.B.sig())
        self.assertIn("C depot/road_depot.con@50.00,-30.00,0.00", self.A.sig())
        for g in (self.A, self.B):
            self.assertIn("1:1#c1/d1", str(g.lua("BOUND_KEYS()")).split(), "%s keyed the depot" % g.label)
        self.assertIn("action 1:1 created 1 CONSTRUCTION entities, keyed", self.B.log())
        self.assertNotIn("native build captured", self.log())
        self.assertEqual(self.B.shipped_count(), 1)

    def test_a_vehicle_bought_at_the_originators_depot_is_found_on_both_games(self):
        self.start()
        self.B.tool("{ edge = 110, at = { x = 50, y = 0, z = 0 }, door = { x = 50, y = -30, z = 0 }, params = { seed = 260 } }", make="NATIVE_DEPOT_ON_ROAD")
        self.tick(16)
        for g in (self.A, self.B):
            self.assertIn("1:1#c1/d1", str(g.lua("BOUND_KEYS()")).split())
        self.assertEqual(self.B.native.released, [1])
        self.assertNotIn("does not exist here", self.log())

    def test_the_late_stamp_still_releases_and_the_command_is_not_held_forever(self):
        # the game of the originator stands still for a while before the stamp (a stall): the command stays held (the module's
        # own 30 s age and the 10 s silence rule are the bounds, and pings keep the silence rule quiet), and goes back when
        # the stamp is reached
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))
        self.frames(60)   # 3 s of interface frames, the simulation does not advance
        self.assertEqual(self.B.native.released, [], "still held at the stamp's door")
        self.assertTrue(self.B.native.holding)
        self.assertEqual(self.B.native.heard[-1][2] in ("ping", "hold_on", "release"), True)
        self.tick(10)
        self.assertEqual(self.B.native.released, [1])
        self.assertSameWorld()

    def test_the_module_giving_the_command_back_early_does_not_ship_the_build_twice(self):
        # silence or age: the module hands the command back before the stamp. The build is in the originator's world early, the
        # replay mark keeps the simulation half from capturing it as a second build
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
        self.assertEqual(self.A.builds_applied(), 1)
        self.assertEqual(self.B.builds_applied(), 1)

    def test_strict_mode_stopping_while_a_command_is_held_gives_it_back_without_a_second_ship(self):
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))
        self.frames(2)
        self.assertEqual(self.B.native.held, [1])
        with open(os.path.join(self.B.dir, "identity.txt"), "w", encoding="utf-8") as f:
            f.write("id=1\nname=client\nrole=idle\n")   # the session ended while the command is held: strict mode stops
        self.frames(30)
        self.assertIn("goes back to the engine before its stamp (the session ended)", self.B.log())
        self.tick(16)
        self.assertEqual(self.B.native.released, [1])
        self.assertEqual(self.B.shipped_count(), 1)
        self.assertNotIn("native build captured", self.B.log())
        self.assertSameWorld()

    def test_a_build_the_originator_refuses_at_the_stamp_is_cancelled_not_left_held(self):
        # the same rule that refuses it on every other game refuses it here: the held command is destroyed, nothing is built
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))
        self.frames(2)
        self.assertEqual(self.B.native.held, [1])
        self.B.run("FACTORY_REJECT = function(parts, kind) if kind == 'SimpleProposal' then for _ in ipairs(parts.edgesAdd) do return 'no segment please' end end end")
        self.tick(14)
        self.assertEqual(self.B.native.dropped, [1])
        self.assertEqual(self.B.native.released, [])
        self.assertIn("was refused at its stamp: cancelled", self.B.log())
        self.assertEqual(self.B.builds_applied(), 0)
        self.assertEqual(self.B.L.eval("NATIVE_HELD_COUNT()"), 0, "nothing stays held")

    # ---- (g) both players building at once

    def test_both_players_building_at_once_end_with_identical_worlds(self):
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

    # ---- (c) a held command without a usable capture goes the old way

    def test_a_held_command_with_no_capture_is_released_and_the_old_path_ships_it(self):
        self.start()
        self.B.tool(self.road(300, 0, 400, 0), event=False)
        self.tick(14)
        self.assertEqual(self.B.native.released, [1], "released")
        self.assertEqual(self.B.native.dropped, [])
        kinds = self.B.native.ctl_kinds()
        self.assertIn("release", kinds)
        self.assertNotIn("cancel", kinds)
        self.assertIn("no capture of a tool build; release written", self.B.log())
        self.assertSameWorld()
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1), "the originator built natively, the other game at the stamp")
        self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 1), "the simulation half shipped it, once")
        self.assertIn("native build captured", self.B.log())

    def test_a_capture_the_module_could_not_make_releases_the_held_command(self):
        self.start()
        self.B.run("FACTORY_REJECT = nil")
        i = int(self.B.L.eval("TOOL_MAKE(%s)" % self.road(300, 0, 400, 0)))
        # the tool event arrives with something that is no proposal: the capture fails, the entry keeps its place in the order
        self.B.L.execute("GUI.guiHandleEvent(nil, STATE, GSTATE, 0, 'streetBuilder', 'builder.proposalCreate', { 'junk' })")
        self.B.L.execute("TOOL_ADD(%d, true)" % i)
        self.tick(14)
        self.assertEqual(self.B.native.released, [1])
        self.assertIn("the capture of streetBuilder failed", self.B.log())
        self.assertSameWorld()
        self.assertEqual(self.B.shipped_count(), 1, "the old path shipped it")

    def test_a_record_this_game_cannot_rebuild_is_released_never_cancelled(self):
        self.start()
        i = int(self.B.L.eval("TOOL_MAKE(%s)" % self.road(300, 0, 400, 0)))   # the command exists; now the factory turns strict
        self.B.run("FACTORY_REJECT = function(parts, kind) if kind == 'SimpleProposal' then for _ in ipairs(parts.edgesAdd) do return 'no segment please' end end end")
        self.B.tool_event(i)
        self.B.L.execute("TOOL_ADD(%d, true)" % i)
        self.tick(14)
        self.assertEqual(self.B.native.dropped, [], "a build that cannot be rebuilt is never cancelled")
        self.assertEqual(self.B.native.released, [1])
        self.assertIn("it cannot be rebuilt here", self.B.log())
        self.assertEqual(self.B.builds_applied(), 1, "the tool's own build went through")
        self.assertIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.B.sig())
        self.assertIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.A.sig(), "and the old path took it to the other game")

    def test_a_fault_while_deciding_gives_the_command_back_and_does_not_break_the_bridge(self):
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))     # held; the verdict is made in the next frame
        self.B.run("FAIL_NEXT_PLAYER = true")
        self.tick(14)
        self.assertEqual(self.B.native.released, [1], "the held command was given back, not left in the ring")
        self.assertEqual(self.B.native.dropped, [])
        self.assertIn("fault in the verdict on held 1", self.B.log())
        self.assertSameWorld()
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
        self.assertEqual(self.B.shipped_count(), 1, "the old path shipped it")
        # and the next build is strict again
        self.B.tool(self.road(500, 0, 600, 0))
        self.tick(16)
        self.assertEqual(self.B.native.released, [1, 2])
        self.assertSameWorld()

    # ---- (d) a capture without a held command

    def test_a_preview_that_gets_no_held_command_ships_nothing(self):
        self.start()
        self.B.tool(self.road(300, 0, 400, 0), add=False)   # hovering: the preview only, nothing is held
        self.tick(14)
        self.assertEqual(self.B.shipped_count(), 0)
        self.assertEqual(self.B.native.ctl_kinds().count("cancel"), 0)
        self.assertEqual(self.B.native.ctl_kinds().count("release"), 0)

    def test_a_tool_build_the_module_did_not_hold_is_shipped_once_by_the_simulation_half(self):
        self.start()
        self.B.tool(self.road(300, 0, 400, 0), site=False)   # an Add from a call site that is not a tool's: not held
        self.tick(16)
        self.assertEqual(self.B.native.held, [])
        self.assertSameWorld()
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
        self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 1), "only the old path shipped it")
        self.tick(12)
        self.assertEqual(self.B.shipped_count(), 1, "the preview is not shipped on top of it")

    # ---- (f) pings

    def test_pings_keep_coming_and_the_module_never_goes_silent_on_us(self):
        self.start()
        self.strict_on(self.A, self.B)
        t0 = self.clock.t
        self.tick(160)   # 32 s without any build
        self.assertGreater(self.clock.t - t0, 30)
        self.strict_on(self.A, self.B)
        times = [t for t, _, kind, _ in self.B.native.heard if kind in ("ping", "hold_on")]
        gaps = [b - a for a, b in zip(times, times[1:])]
        self.assertLessEqual(max(gaps), 2.2, gaps)
        self.assertGreaterEqual(len(times), 15)
        ns = [p["n"] for _, _, kind, p in self.B.native.heard if kind == "ping"]
        self.assertEqual(ns, sorted(set(ns)), "ping numbers grow")
        seqs = [s for s, _, _ in self.B.native.ctl_records()]
        self.assertEqual(seqs, list(range(1, len(seqs) + 1)), "the control file's sequence numbers grow by one")
        self.assertEqual(self.B.native.event_kinds().count("holding"), 1, "the module never switched holding off")
        self.assertNotIn("switched holding off", self.B.log())

    def test_a_game_that_stands_still_past_the_silence_limit_is_switched_off_by_the_module_and_comes_back(self):
        self.start()
        self.strict_on(self.B)
        # the interface stops for 11 s (a long load, a hitch): the module's silence rule switches holding off
        self.clock.advance(11.0)
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

    # ---- (e) hold_off before a resync load and when the session ends

    def test_hold_off_is_written_before_a_resync_load_and_strict_mode_comes_back_after_the_reload(self):
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
        recs = self.B.native.ctl_records()[n_ctl:]
        self.assertIn("hold_off", [k for _, k, _ in recs], "hold_off was written before the load")
        self.B.native.service()
        self.assertFalse(self.B.native.holding, "the module stopped holding before the scripts were replaced")
        self.B.reload()
        self.relay.session()   # the launcher tells the reloaded game the session state again (as in the resync test)
        self.tick(6)
        self.assertTrue(self.B.native.holding, "the reloaded mod switched strict mode on again by itself")
        kinds = self.B.native.ctl_kinds()
        self.assertEqual(kinds.count("hold_on"), 2)
        self.assertLess(kinds.index("hold_off"), len(kinds) - 1 - kinds[::-1].index("hold_on"), "hold_off came before the second hold_on")
        seqs = [s for s, _, _ in self.B.native.ctl_records()]
        self.assertEqual(seqs, list(range(1, len(seqs) + 1)), "the reloaded mod continued the numbering")
        # and a build after the reload is strict again
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(20)
        self.assertEqual(self.B.native.released, [1])
        self.assertSameWorld()
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
        self.assertEqual(self.B.shipped_count(), 1)

    def test_a_mod_that_starts_after_the_module_asks_and_does_not_answer_the_events_it_finds(self):
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
        # and it works: a build after the restart is strict
        self.B.tool(self.road(500, 0, 600, 0))
        self.tick(20)
        self.assertEqual(self.B.native.released, [1, 2])
        self.assertSameWorld()
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (2, 2))

    def test_hold_off_is_written_when_the_session_ends_and_hold_on_when_a_new_one_starts(self):
        self.start()
        self.strict_on(self.B)
        ident = os.path.join(self.B.dir, "identity.txt")
        with open(ident, "w", encoding="utf-8") as f:
            f.write("id=1\nname=client\nrole=idle\n")
        self.tick(8)   # the mod looks once a second
        self.assertFalse(self.B.native.holding)
        self.assertIn("hold_off", self.B.native.ctl_kinds())
        self.assertIn("the session ended", self.B.log())
        before = len(self.B.native.ctl_records())
        self.tick(30)   # nothing more is written while there is no session (no pings needed: nothing is held)
        self.assertEqual(len(self.B.native.ctl_records()), before)
        # a build now is a plain native build: nothing is held, nothing is captured for strict mode
        with open(ident, "w", encoding="utf-8") as f:
            f.write("id=1\nname=client\nrole=player\n")
        self.tick(12)
        self.strict_on(self.B)
        self.assertEqual(self.B.native.ctl_kinds().count("hold_on"), 2)

    # ---- pause, a late capture, a game without the module

    def test_a_build_made_while_the_session_is_paused_is_applied_by_both_games_at_the_pause_time(self):
        self.start(speed=2)
        self.tick(10)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(18)
        self.assertEqual((self.A.time(), self.B.time()), (self.relay.pauseAt, self.relay.pauseAt))
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(6)
        self.assertEqual(self.B.native.released, [1])
        expected = ["makeWorldBuildProposalCmd@%d" % self.relay.pauseAt]
        self.assertEqual(self.A.builds(), expected)
        self.assertEqual(self.B.builds(), expected, "the originator applied it during the pause too, and only once")
        self.assertSameWorld()
        self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 1))

    def test_a_newer_preview_that_cannot_be_captured_is_never_replaced_by_an_older_good_one(self):
        # the newest preview is the proposal on screen: if it could not be encoded, the held command must go back, not be
        # paired with the record of an earlier preview
        self.start()
        self.B.tool(self.road(300, 0, 400, 0), add=False)          # a good preview
        self.B.L.execute("GUI.guiHandleEvent(nil, STATE, GSTATE, 0, 'streetBuilder', 'builder.proposalCreate', { 'junk' })")
        self.B.tool(self.road(500, 0, 600, 0), event=False)        # the click: its command is held, its preview is the bad one
        self.tick(14)
        self.assertEqual(self.B.native.released, [1])
        self.assertEqual(self.B.native.dropped, [])
        self.assertSameWorld()
        self.assertIn("E 500.00,0.00,0.00 600.00,0.00,0.00", self.A.sig(), "the clicked road, not the earlier preview's")
        self.assertNotIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.A.sig())
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))

    def test_a_preview_is_used_once(self):
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))                     # preview and click
        self.tick(12)
        self.assertEqual(self.B.native.released, [1])
        self.B.tool(self.road(500, 0, 600, 0), event=False)        # a click with no preview of its own: the old one is spent
        self.tick(14)
        self.assertEqual(self.B.native.released, [1, 2])
        self.assertSameWorld()
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (2, 2))

    def test_a_game_without_the_module_and_one_with_it_keep_the_same_world(self):
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

    # ---- refusals

    def test_a_build_the_player_may_not_make_is_cancelled_before_it_is_stamped(self):
        self.start()
        self.relay.send(self.B, "permissions", 0, self.relay.ser({"build": False}))
        self.tick(2)
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(14)
        self.assertEqual(self.B.native.dropped, [1], "held and destroyed: nothing was built")
        self.assertEqual(self.B.builds_applied() + self.A.builds_applied(), 0)
        self.assertEqual(self.B.shipped_count(), 0)
        self.assertTrue(any("permission denied: build" in d for _, _, d in self.relay.refused), self.relay.refused)
        self.assertNotIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.B.sig())


class StrictOffTest(StrictTest):
    """Strict mode is simply not there when the module is not."""

    natives = (False, False)

    def test_without_the_module_nothing_is_written_or_logged_and_the_old_path_works(self):
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))   # the tool event reaches the mod, the command goes to the engine
        self.tick(16)
        for g in (self.A, self.B):
            self.assertFalse(os.path.exists(os.path.join(g.dir, "native_ctl.log")), "no control file without a module")
            self.assertNotIn("strict:", g.log())
        self.assertSameWorld()
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
        self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 1))


class NoHoldHookTest(StrictTest):
    """The module is there but did not install the CommandList::Add hook."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-strict-")
        self.clock = Clock()
        self.A = StrictGame("host", 0, self.tmp, 50000, self.clock, native=True, hooks=("apply_command",))
        self.B = StrictGame("client", 1, self.tmp, 51000, self.clock, native=True, hooks=("apply_command",))
        self.relay = T.Relay([self.A, self.B])

    def test_strict_mode_stays_off_and_says_why_once(self):
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(16)
        self.assertEqual(self.B.native.ctl_records(), [], "nothing is written to a module that cannot hold")
        self.assertEqual(self.B.log().count("no command_list_add hook"), 1)
        self.assertSameWorld()
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
        self.assertEqual(self.B.shipped_count(), 1, "the old path")


class ModuleSilentTest(StrictTest):
    """native_events.log exists but the module never answers (an old file of a crashed run)."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-strict-")
        self.clock = Clock()
        self.A = StrictGame("host", 0, self.tmp, 50000, self.clock, native=True)
        self.B = StrictGame("client", 1, self.tmp, 51000, self.clock, native=True)
        self.relay = T.Relay([self.A, self.B])
        self.B.native.is_open = False   # the file stays, the module reads nothing

    def test_a_module_that_does_not_answer_is_pinged_a_few_times_and_never_asked_to_hold(self):
        self.start()
        self.tick(80)
        kinds = self.B.native.ctl_kinds()
        self.assertNotIn("hold_on", kinds)
        self.assertLessEqual(len(kinds), 6, "a few pings, then it waits")
        self.assertIn("does not answer pings", self.B.log())
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(16)
        self.assertSameWorld()
        self.assertEqual(self.B.shipped_count(), 1, "the old path")


if __name__ == "__main__":
    unittest.main()
