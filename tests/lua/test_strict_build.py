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
                            "kinds": {"build_proposal": 52}, "sites": 12, "patch": self.patch_cap})
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
    # FEVERLINK3_STRICT_CANCEL for the test: "0" = hold-and-release for every build (what most tests here are about), "1" = cancel
    # every kind, None = unset (the default: cancel and replay on every game for street, combined and bulldoze builds)
    strict_cancel = "0"
    # more environment variables for the test (None removes one). The firing of the tool's callback is ON by default since 2026-10-13;
    # most tests here are about the older flows and switch it off (classes that set extra_env themselves say what they need).
    extra_env = {"FEVERLINK3_STRICT_FIRE_DONE": "0"}
    fires = False       # the fake module reports `done` in its held events

    def setUp(self):
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
        self.A = StrictGame("host", 0, self.tmp, 50000, self.clock, native=self.natives[0])
        self.B = StrictGame("client", 1, self.tmp, 51000, self.clock, native=self.natives[1])
        self.relay = T.Relay([self.A, self.B])
        for g in (self.A, self.B):
            if g.native:
                g.native.fires = self.fires

    def tearDown(self):
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
        # the build's timing text travels with the record (owner 2026-10-14: one timing line per build on every game)
        self.assertRegex(str(body["body"]), r"tm.{0,8}held=1 click=", str(body["body"])[:400])

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
        self.tick(48)
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
        self.tick(48)
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
        # B's game is single-player now and sends no more clock lines; the host tells A that B left (its promise no longer holds A)
        self.relay.broadcast("player_left", self.relay.ser({"id": 1}), exclude=self.B)
        self.tick(16)
        self.assertEqual(self.B.native.released, [1])
        self.assertEqual(self.B.shipped_count(), 1)
        self.assertNotIn("native build captured", self.B.log())
        self.assertSameWorld()

    def test_a_build_the_originator_refuses_at_the_stamp_is_released_not_cancelled_when_the_callback_cannot_be_fired(self):
        # the rule that refuses it on every other game refuses it here too; a cancel would leave the tool dead (the module cannot fire
        # the callback here), so the command goes back to the engine and the games are repaired
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))
        self.frames(2)
        self.assertEqual(self.B.native.held, [1])
        self.B.run("FACTORY_REJECT = function(parts, kind) if kind == 'SimpleProposal' then for _ in ipairs(parts.edgesAdd) do return 'no segment please' end end end")
        self.tick(14)
        self.assertEqual(self.B.native.dropped, [])
        self.assertEqual(self.B.native.released, [1])
        self.assertIn("was refused at its stamp but is NOT cancelled", self.B.log())
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
        self.tick(14)   # the grace for vetoes (real time), then the commit
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

    # ---- (e) a command is paired with the preview of its own tool only; nothing given back is unaccounted for

    def test_a_command_from_a_site_without_a_preview_is_never_paired_with_an_older_preview(self):
        # real session 2026-10-13: the lane tool (UI::LaneModifier::Apply) sends no preview; its command was shipped as the upgrade tool's
        # preview of 12 s before
        self.start()
        self.B.tool(self.road(300, 0, 400, 0), add=False)             # a preview of the street tool, never clicked
        self.B.native.site = 0x537EE5
        self.B.L.execute("TOOL_ADD(%d, true)" % 1)
        self.tick(14)
        self.assertEqual(self.B.native.released, [1], "given back, not cancelled")
        self.assertEqual(self.B.native.dropped, [])
        self.assertNotIn("matched the capture", self.B.log())
        self.assertIn("(site 0x537ee5, LaneModifier::Apply)", self.B.log(), "named by its site")
        self.assertIn("accounted for, the simulation half shipped a native build", self.B.log(), "and checked that something shipped it")
        self.assertSameWorld()

    def test_a_preview_of_another_tool_is_not_paired_with_a_command_of_a_known_site(self):
        self.start()
        self.B.native.site = 0x4D396F                                  # the bulldozer's site; the preview is the street tool's
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(14)
        self.assertEqual(self.B.native.released, [1])
        self.assertIn("which does not issue commands there; not paired", self.B.log())
        self.assertNotIn("matched the capture", self.B.log())

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


SPLIT = "{ edge = 110, at = { x = 50, y = 0, z = 0 }, to = { x = 50, y = 80, z = 0 } }"


class NativeVersusReplayTest(StrictTest):
    """The originator's world gets the tool's NATIVE result (its own command, released at the stamp); every other game gets the
    record REBUILT. Both must end with the same world: the hard cases of a real session (2026-10-11, the worlds diverged from the
    first road that touched an existing one)."""

    def test_a_road_that_splits_an_existing_segment_ends_the_same_on_both_games(self):
        self.start()
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(16)
        self.assertEqual(self.B.native.released, [1])
        self.assertSameWorld()
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
        self.assertIn("E 0.00,0.00,0.00 50.00,0.00,0.00", self.A.sig(), "the old segment was split at the junction on the replay too")
        self.assertNotIn("E 0.00,0.00,0.00 100.00,0.00,0.00", self.A.sig(), "and the segment that was split is gone")
        self.assertEqual(self.relay.vetoes, [])

    def test_a_road_that_connects_to_a_node_the_previous_build_made_two_seconds_earlier(self):
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(10)   # two seconds of game time: the first build has landed on both games
        self.assertSameWorld()
        nb = int(self.B.lua("FIND_NODE(400, 0, 0)"))
        self.assertNotEqual(nb, int(self.A.lua("FIND_NODE(400, 0, 0)")), "the games number their new nodes differently")
        self.B.tool("{ nodeA = %d, b = { x = 500, y = 100, z = 0 } }" % nb)
        self.tick(16)
        self.assertSameWorld()
        self.assertEqual(self.A.lua("#EDGES_AT(FIND_NODE(400, 0, 0))"), 2, "one node, two roads, on the replay too")
        self.assertEqual(self.B.lua("#EDGES_AT(FIND_NODE(400, 0, 0))"), 2)
        self.assertEqual(self.relay.vetoes, [])
        self.assertNotIn("refused", self.log())

    def test_two_builds_in_quick_succession_where_the_second_needs_what_the_first_makes(self):
        # the second record refers to a node the first creates: the peer's dry run waits for the first to land instead of vetoing
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(6)   # the first is still held (its stamp lies ahead)
        self.assertEqual(self.B.native.released, [])
        self.tick(8)
        nb = int(self.B.lua("FIND_NODE(400, 0, 0)"))
        self.B.tool("{ nodeA = %d, b = { x = 500, y = 100, z = 0 } }" % nb)
        self.tick(16)
        self.assertSameWorld()
        self.assertEqual(self.relay.vetoes, [])

    def test_a_depot_with_its_street_is_keyed_and_placed_the_same_on_both_games(self):
        self.start()
        self.B.tool("{ edge = 110, at = { x = 50, y = 0, z = 0 }, door = { x = 50, y = -30, z = 0 }, params = { seed = 260 } }", make="NATIVE_DEPOT_ON_ROAD")
        self.tick(48)
        self.assertEqual(self.B.native.released, [1])
        lines = lambda g, p: sorted(x for x in g.sig().splitlines() if x.startswith(p))
        self.assertEqual(lines(self.A, "C "), lines(self.B, "C "), "the same construction in the same place")
        # (the mock engine adds the depot's entrance node to the tool's own build only; the replay leaves it to the depot's script)
        self.assertTrue(set(lines(self.A, "N ")) <= set(lines(self.B, "N ")), "every street node of the replay is on the originator too")
        self.assertIn("N 50.00,0.00,0.00", lines(self.A, "N "), "the junction on the road exists on the replay")
        self.assertEqual(self.relay.vetoes, [])

    def test_a_depot_whose_entrance_reaches_an_existing_node_gets_that_connection_on_the_other_game_and_a_road_from_it_resolves(self):
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
        self.assertEqual(self.relay.vetoes, [])
        self.assertEqual(self.B.native.released, [1, 2])

    def test_an_airfield_with_signs_on_its_own_taxiways_is_shipped_before_it_is_built_and_replayed_without_a_veto(self):
        # real run 2026-10-11: the tool's preview could not be read (the signs' models), the build went through un-vetted and was
        # shipped afterwards as 21 references to nodes nobody else had ("unresolved node")
        self.start()
        self.B.tool("{ a = { x = 600, y = 600, z = 0 }, b = { x = 700, y = 650, z = 0 }, params = { seed = 5 } }", make="NATIVE_AIRFIELD")
        self.tick(1)
        self.assertEqual(self.B.native.released, [], "still held: the record was shipped first, so a veto can still stop it")
        self.assertEqual(len(self.B.action_records()), 1)
        self.assertNotIn("capture of streetBuilder failed", self.B.log())
        self.tick(48)
        self.assertEqual(self.B.native.released, [1])
        self.assertEqual(self.relay.vetoes, [])
        self.assertNotIn("refused", self.log())
        self.assertNotIn("native build captured", self.log())
        lines = lambda g: sorted(x for x in g.sig().splitlines() if x.startswith("C "))
        self.assertEqual(lines(self.A), lines(self.B), "the same airfield on both games")
        self.assertEqual(self.B.shipped_count(), 1)

    def test_a_missing_node_is_made_where_the_result_says_instead_of_refusing_the_build(self):
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


class VetoTest(StrictTest):
    """Veto-only validation: a game that cannot build the record says so; silence means yes. A refused build exists on NO game."""

    # the cancel path needs a tool callback the module can fire (otherwise a cancel leaves the tool dead: see NoCancelWithoutCallbackTest)
    extra_env = {"FEVERLINK3_STRICT_FIRE_DONE": "1"}
    fires = True

    def test_the_happy_path_has_no_veto_and_no_extra_message(self):
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(16)
        self.assertEqual(self.relay.vetoes, [])
        self.assertEqual(self.B.native.released, [1])
        self.assertSameWorld()
        self.assertIn("held", str(self.B.action_records()[0]["body"]), "the record says its originator holds the command (peers dry-run it)")

    def test_a_game_that_cannot_resolve_a_segment_vetoes_and_neither_game_builds(self):
        self.start()
        before_a, before_b = self.A.sig(), self.B.sig()
        self.A.run("WORLD[110] = nil")   # this game has no such segment: the split cannot be rebuilt here
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(20)
        self.assertEqual(len(self.relay.vetoes), 1)
        who, owner, seq, why = self.relay.vetoes[0]
        self.assertEqual((who, owner, seq), (0, 1, 1))
        self.assertIn("unresolved segment", why, "the first cause is named")
        # nobody built it
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 0))
        self.assertEqual(self.B.native.released, [], "the originator's held command was not released")
        self.assertEqual(self.B.native.dropped, [1], "it was cancelled: the build exists on no game")
        self.assertEqual(self.B.sig(), before_b, "the originator's world did not run ahead")
        self.assertNotEqual(self.A.sig(), "")
        self.assertIn("VETO of 1:1: this game cannot build it (unresolved segment", self.A.log())
        self.assertIn("cancelled: host could not build it (unresolved segment", self.B.log(), "the originator's log says who and why")
        self.assertIn("VETO of 1:1 by host: unresolved segment", self.B.log())

    def test_the_veto_is_cast_before_the_stamp_and_the_replay_is_dropped_on_the_vetoing_game(self):
        self.start()
        self.A.run("WORLD[110] = nil")
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(2)
        self.assertEqual(len(self.relay.vetoes), 1, "the veto is out within a tick or two of the record, long before the stamp")
        stamp = self.B.action_records()[0]["stamp"]
        self.assertLess(self.B.time(), stamp)
        self.tick(14)
        self.assertEqual(self.A.builds_applied(), 0)
        self.assertNotIn("makeWorldBuildProposalCmd applied", self.A.log())

    def test_the_games_still_work_after_a_vetoed_build(self):
        # the tool of a vetoed build is dead (a cancelled command gives it no callback), but the session goes on
        self.start()
        self.A.run("WORLD[110] = nil")
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(20)
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(20)
        self.assertIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.A.sig())
        self.assertIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.B.sig())
        self.assertEqual(self.B.native.released, [2])


class NoCancelWithoutCallbackTest(StrictTest):
    """A held command is cancelled only when the module can fire the tool's callback. A veto (a game that cannot rebuild the build) of
    one whose callback cannot be fired was a release until 2026-10-15 (the build then existed on this game only, every other game dropped
    it, a resync followed); now it is failed (`fail`, test_strict_ship_fault.py), and a module without `fail` (this fake one) gets a
    cancel: nothing is applied on any game, never a release."""

    fires = False

    def test_a_veto_never_releases_the_build_on_this_game_only(self):
        self.start()
        self.A.run("WORLD[110] = nil")
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(20)
        self.assertEqual(len(self.relay.vetoes), 1)
        self.assertEqual(self.B.native.released, [], "never given back to the engine")
        self.assertEqual(self.B.native.dropped, [1], "a module without `fail`: cancelled")
        self.assertIn("FAILED on the veto, NOT released", self.B.log())
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 0), "no game has the build")
        self.assertEqual(self.relay.replay_failed, [], "so no repair")


class StrictCancelFallbackTest(StrictTest):
    """FEVERLINK3_STRICT_CANCEL=1: the first strict flow, for testing sync independent of the tool."""

    strict_cancel = "1"

    def test_the_command_is_cancelled_and_every_game_replays_the_record_so_the_worlds_are_identical(self):
        self.start()
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(16)
        self.assertEqual(self.B.native.dropped, [1], "cancelled, as in the first design")
        self.assertEqual(self.B.native.released, [])
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1), "both games applied the same rebuilt record")
        self.assertSameWorld()
        self.assertIn("cancel flow", self.B.log())
        self.assertEqual(self.relay.vetoes, [])
        self.assertNotIn("held=true", str(self.B.action_records()[0]["body"]).replace(" ", ""), "no held marker in this mode")

    def test_the_stamp_margin_is_measured_by_the_receiver_and_reported_back(self):
        self.start()
        self.tick(4)   # clocks (and the receiver's default need) reach the other game
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(16)
        body = self.B.action_records()[0]["body"]
        self.assertIn('["xs"]=', body, "the record carries the extra it was stamped with")
        a_trace = self.trace(self.A)
        self.assertRegex(a_trace, r"build\.margin 1:1 recv g\d+ stamp \d+ slack=-?\d+ steps extra_used=\d+ target=\d+ sample=\d+ need=\d+ \((grow|keep|shrink)\)")
        self.assertSameWorld()

    def trace(self, g):
        path = os.path.join(g.dir, "trace.log")
        return open(path, encoding="utf-8").read() if os.path.exists(path) else ""

    def test_no_dry_run_veto_in_this_mode(self):
        self.start()
        self.A.run("WORLD[110] = nil")
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(20)
        self.assertEqual(self.relay.vetoes, [], "the originator replays it itself: nothing to cancel, the refusal at the stamp is the old one")


class DefaultHoldAndReleaseTest(StrictTest):
    """FEVERLINK3_STRICT_CANCEL unset: every build, host's or guest's, is held and released (the tool gets its callback and keeps its
    ghost); nothing is cancelled."""

    strict_cancel = None

    def test_a_road_build_of_either_game_is_held_and_released_by_default(self):
        self.start()
        self.A.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(24)
        self.assertEqual((self.A.native.released, self.A.native.dropped), ([1], []))
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(24)
        self.assertEqual((self.B.native.released, self.B.native.dropped), ([1], []))
        self.assertNotIn("cancel flow", self.log())


class DefaultDemolitionTest(StrictTest):
    """Nothing set: the defaults since 2026-10-13 (FEVERLINK3_STRICT_CANCEL=removals, FEVERLINK3_STRICT_FIRE_DONE on). A demolition is cancelled
    and replayed at the stamp by every game (real two-window run: 11 of 11 left the checksums identical and the bulldozer usable); roads are
    held and released as before. =0 on either variable is the opt-out."""

    strict_cancel = None
    extra_env = {"FEVERLINK3_STRICT_FIRE_DONE": None}
    fires = True

    def test_a_demolition_is_cancelled_and_a_road_is_held_and_released(self):
        self.start()
        self.B.tool("{ cons = { 54 } }", make="NATIVE_BULLDOZE")
        self.tick(24)
        self.assertEqual((self.B.native.dropped, self.B.native.released), ([1], []))
        self.assertSameWorld()
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(24)
        self.assertEqual((self.B.native.dropped, self.B.native.released), ([1], [2]))
        self.assertSameWorld()


class OptOutTest(DefaultDemolitionTest):
    """FEVERLINK3_STRICT_FIRE_DONE=0 or FEVERLINK3_STRICT_CANCEL=0: back to hold-and-release for everything."""

    extra_env = {"FEVERLINK3_STRICT_FIRE_DONE": "0"}

    def test_a_demolition_is_cancelled_and_a_road_is_held_and_released(self):
        self.start()
        self.B.tool("{ cons = { 54 } }", make="NATIVE_BULLDOZE")
        self.tick(24)
        self.assertEqual((self.B.native.released, self.B.native.dropped), ([1], []))
        self.assertSameWorld()


class CancelFlowChoiceTest(StrictTest):
    """FEVERLINK3_STRICT_CANCEL=plans / host: cancel-and-replay for roads and bulldozing, for every originator or the host only."""

    strict_cancel = "plans"

    def test_plans_cancels_the_roads_of_the_guest_too(self):
        self.start()
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(24)
        self.assertEqual((self.B.native.dropped, self.B.native.released), ([1], []))
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
        self.assertSameWorld()
        self.assertIn("cancel flow", self.B.log())


class CancelFlowHostOnlyTest(StrictTest):
    strict_cancel = "host"

    def test_host_cancels_and_a_guest_keeps_hold_and_release(self):
        self.start()
        self.A.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(24)
        self.assertEqual((self.A.native.dropped, self.A.native.released), ([1], []))
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(24)
        self.assertEqual((self.B.native.released, self.B.native.dropped), ([1], []))
        self.assertSameWorld()


class HoldAndReleaseSwitchTest(StrictTest):
    """FEVERLINK3_STRICT_CANCEL=0: the developer switch back to hold-and-release for every kind."""

    def test_the_originator_releases_its_own_command(self):
        self.start()
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(16)
        self.assertEqual(self.B.native.released, [1])
        self.assertEqual(self.B.native.dropped, [])
        self.assertSameWorld()


class ConstructionDefaultTest(StrictTest):
    """Remote constructions are replicated as before (hold-and-release); the player gets an informational chat line."""

    DEPOT = "{ edge = 110, at = { x = 50, y = 0, z = 0 }, door = { x = 50, y = -30, z = 0 }, params = { seed = 260 } }"

    def test_a_depot_build_is_held_and_released_and_the_player_is_told_it_is_experimental(self):
        self.start()
        self.B.tool(self.DEPOT, make="NATIVE_DEPOT_ON_ROAD")
        self.tick(20)
        self.assertEqual((self.B.native.released, self.B.native.dropped), ([1], []))
        self.assertEqual(self.B.shipped_count(), 1)
        self.assertEqual(len(self.relay.chats), 1)
        self.assertIn("experimental in this test build", self.relay.chats[0][1])


class BuildLogLinesTest(StrictTest):
    """Every build that is applied has one `build <key> (replay|originator): ...` line before it is applied, with the plan, the counts and
    the engine's verdict, in mod.log of every run (not only when paused)."""

    DEPOT = ConstructionDefaultTest.DEPOT

    def test_the_receiver_logs_a_replay_line_and_the_originator_its_own(self):
        self.start()
        self.B.tool(self.DEPOT, make="NATIVE_DEPOT_ON_ROAD")
        self.tick(20)
        import re
        self.assertRegex(self.A.log(), r"build 1:1 \(replay\): plan=\w+ cons=1 rem=0 nodes=\d+ segs=\d+ rmSegs=\d+ objs=\d+; adds \[[^\]]+\]; verdict .*; ie=\w+ pi=\w+")
        self.assertRegex(self.B.log(), r"build 1:1 \(originator\): plan=\w+ cons=1 ")

    def test_a_road_has_the_line_too_without_the_removal_lists(self):
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(20)
        self.assertRegex(self.A.log(), r"build 1:1 \(replay\): plan=street cons=0 rem=0 ")
        self.assertNotIn("removes [", self.A.log())


class TraceLogTest(StrictTest):
    """trace.log (FEVERLINK3_TRACE, on by default): one self-contained block per build and a line per checksum round, in the session folder."""

    DEPOT = ConstructionDefaultTest.DEPOT

    def trace(self, g):
        import os as _os
        path = _os.path.join(g.dir, "trace.log")
        return open(path, encoding="utf-8").read() if _os.path.exists(path) else ""

    def test_the_record_carries_the_flags_of_the_held_command_not_the_previews_guess(self):
        # the preview a build is captured from has no Context; the native module reads the held command's own flags (a stop tool builds with
        # cleanup = false, a road tool with true) and the shipped record, and so every game's replay, uses them
        self.start()
        self.strict_on(self.A, self.B)
        self.B.native.held_flags = {"align": False, "cleanup": True, "buildings": False, "fields": True, "redo": True, "ie": True, "pi": True}
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(20)
        body = self.B.action_records()[0]["body"]
        self.assertIn('[2]={["align"]=false,["buildings"]=false,["cleanup"]=true,["fields"]=true,["redo"]=true},[3]=true,[4]=true', str(body))
        self.assertIn("side=originator source=held command ctx{align=false cleanup=true buildings=false fields=true redo=true} ie=true pi=true", self.trace(self.B))
        self.assertIn("side=replay source=record ctx{align=false cleanup=true buildings=false fields=true redo=true}", self.trace(self.A))

    def test_the_input_level_text_of_a_held_command_is_read_and_counted_in_the_trace(self):
        self.start()
        self.strict_on(self.A, self.B)
        self.B.native.input_text = chr(10).join(["proposal.addedNodes.#" + chr(9) + "n:2", "proposal.addedSegments.#" + chr(9) + "n:1", "toRemove.#" + chr(9) + "n:0", "kind" + chr(9) + "u8:34"])
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(20)
        self.assertIn("build.input held=1 bytes=", self.trace(self.B))
        self.assertIn("counts[cons=0 rem=0 nodes=2 segs=1 rmNodes=0", self.trace(self.B))

    @staticmethod
    def road_text(template="street/new_medium_road.street_template"):
        import struct as _s
        def f(x):
            return "%08x" % _s.unpack("<I", _s.pack("<f", x))[0]
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

    def test_the_text_is_compared_with_the_record_and_shipped_with_it(self):
        self.start()
        self.strict_on(self.A, self.B)
        self.B.native.input_text = self.road_text()
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(24)
        tb, ta = self.trace(self.B), self.trace(self.A)
        self.assertIn("build.portable held=1 side=originator diffs=1 [frozenNodes in the text: 1 (the record has no such member)]", tb)
        self.assertIn("build.portable 1:1 side=replay diffs=1 [frozenNodes in the text", ta)
        self.assertIn("this game's first proposal counts[", ta)

    def test_a_difference_between_the_text_and_the_record_is_named(self):
        self.start()
        self.strict_on(self.A, self.B)
        self.B.native.input_text = self.road_text("street/other.street_template")
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(24)
        self.assertIn("segment 1: template text street/other.street_template, record street/new_medium_road.street_template", self.trace(self.B))

    def test_a_held_command_without_an_input_text_says_so_and_nothing_else_changes(self):
        self.start()
        self.strict_on(self.A, self.B)
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(20)
        self.assertIn("build.input held=1 NOT AVAILABLE (missing)", self.trace(self.B))
        self.assertEqual(self.B.builds_applied(), 1)

    def external_ctl_line(self, g, kind, payload_text):
        """A control line written by someone else than the interface half (the simulation half writes patch_next), in the file's own shape."""
        path = g.native.ctl
        last = 0
        if os.path.exists(path):
            for line in open(path, encoding="utf-8").read().splitlines():
                head = line.split(TAB)[0]
                if head.isdigit():
                    last = max(last, int(head))
        with open(path, "a", encoding="utf-8", newline="") as f:
            f.write(TAB.join([str(last + 1), kind, str(g.player_id if hasattr(g, "player_id") else 1), payload_text]) + chr(10))

    def test_a_control_line_written_by_the_simulation_half_does_not_make_the_interface_halfs_next_order_stale(self):
        # patch_next is written from the simulation half with a writer of its own; the interface half's writer continues after it
        self.start()
        self.strict_on(self.A, self.B)
        self.external_ctl_line(self.B, "patch_next", '{["adds"]=3,["frozen"]={[1]=1}}')
        self.tick(2)
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(24)
        self.assertEqual(self.B.native.patches, [{"adds": 3, "frozen": [1]}])
        self.assertEqual(self.B.native.released, [1], "the release written after it was heard")

    def test_the_modules_answers_to_a_patch_are_traced(self):
        self.start()
        self.strict_on(self.A, self.B)
        self.external_ctl_line(self.B, "patch_next", '{["adds"]=2,["frozen"]={[1]=1}}')
        self.tick(4)
        self.assertIn('build.patch patched', self.trace(self.B))
        self.B.native.patch_refuse = "the node count differs"
        self.external_ctl_line(self.B, "patch_next", '{["adds"]=5,["frozen"]={[1]=1}}')
        self.tick(4)
        self.assertIn('build.patch patch_refused', self.trace(self.B))
        self.assertIn("the node count differs", self.trace(self.B))

    def test_an_old_native_module_without_flags_keeps_the_captures_guess_and_says_so(self):
        self.start()
        self.strict_on(self.A, self.B)
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(20)
        self.assertIn("source=none (old native module", self.trace(self.B))

    def test_a_build_leaves_its_record_decode_send_result_and_the_originators_ship_line_on_both_games(self):
        self.start()
        self.B.tool(self.DEPOT, make="NATIVE_DEPOT_ON_ROAD")
        self.tick(40)
        self.relay.checksum_request(1, self.relay.future_stamp(), False)
        self.tick(40)
        a, b = self.trace(self.A), self.trace(self.B)
        self.assertRegex(b, r"gui build\.ship 1:1 held=1 site=\S+ done=\w+ tool=\S+ plan=\w+ stamp=\d+ now=\d+ lead=\d+ extra=\w+ bytes=\d+ capture_ms=[\d.]+ balance=\S+ record=\{")
        self.assertRegex(a, r"sim build\.record 1:1 role=replay from=1 stamp=\d+ now=\d+ late=0 balance=\S+ record=\{")
        self.assertRegex(a, r"sim build\.decode 1:1 ok=true ms=[\d.]+")
        self.assertRegex(a, r"build\.plan 1:1 plan=\w+ form=1 built ms=[\d.]+")
        self.assertRegex(a, r"build\.factory 1:1 plan=\w+ form=1 ok ms=[\d.]+ ie=\w+ pi=\w+ dust=\w+ counts\[")
        self.assertRegex(a, r"sim build\.send 1:1 ie=\w+ pi=\w+ dust=\w+ ctx\{")
        self.assertRegex(a, r"sim build\.result 1:1 ok=true steps=\d+ made=\d+ \[\d+\{[A-Z_,]+\}")
        self.assertRegex(a, r"sim checksum t=\d+ light=false lanes=true items\{[^}]*\} parts=\{")
        self.assertIn("native ", b)

class TraceOffTest(StrictTest):
    extra_env = {"FEVERLINK3_TRACE": "0"}

    def test_no_trace_file(self):
        import os as _os
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(30)
        self.assertFalse(_os.path.exists(_os.path.join(self.A.dir, "trace.log")))
        self.assertFalse(_os.path.exists(_os.path.join(self.B.dir, "trace.log")))


class ConstructionBlockTest(StrictTest):
    """FEVERLINK3_REMOTE_CONSTRUCTIONS=0 (developers): a build that adds a construction is cancelled at the originator (with a chat line)
    and replayed by nobody, but only when the native module can fire the tool's callback; otherwise it stays hold-and-release."""

    extra_env = {"FEVERLINK3_REMOTE_CONSTRUCTIONS": "0"}
    fires = True
    DEPOT = ConstructionDefaultTest.DEPOT

    def test_a_depot_build_is_cancelled_with_a_message_and_no_game_builds_it(self):
        self.start()
        self.B.tool(self.DEPOT, make="NATIVE_DEPOT_ON_ROAD")
        self.tick(20)
        self.assertEqual(self.B.native.dropped, [1], "the originator's command is cancelled")
        self.assertEqual(self.B.native.released, [])
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 0))
        self.assertEqual(self.B.shipped_count(), 0, "nothing is shipped")
        self.assertNotIn("C depot/road_depot.con", self.A.sig() + self.B.sig())
        self.assertIn("not supported yet in this test build", self.relay.chats[0][1])
        self.assertSameWorld()

    def test_a_road_still_goes_through(self):
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(24)
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
        self.assertSameWorld()


class UncapturableBuildTest(StrictTest):
    """A build whose capture fails cannot be shipped: with the tool's callback fireable it is cancelled with a chat line, otherwise it is
    released as before (fail open)."""

    extra_env = {"FEVERLINK3_STRICT_FIRE_DONE": "1"}
    fires = True

    def broken_capture(self):
        self.start()
        self.B.run("FACTORY_REJECT = nil")
        i = int(self.B.L.eval("TOOL_MAKE(%s)" % self.road(300, 0, 400, 0)))
        self.B.L.execute("GUI.guiHandleEvent(nil, STATE, GSTATE, 0, 'streetBuilder', 'builder.proposalCreate', { 'junk' })")
        self.B.L.execute("TOOL_ADD(%d, true)" % i)
        self.tick(14)

    def test_the_build_is_cancelled_and_the_player_told(self):
        self.broken_capture()
        self.assertEqual((self.B.native.dropped, self.B.native.released), ([1], []))
        self.assertEqual(self.B.shipped_count(), 0)
        self.assertEqual(len(self.relay.chats), 1)
        self.assertIn("not supported yet in this test build", self.relay.chats[0][1])
        self.assertSameWorld()

    def test_a_capture_that_only_lacks_the_model_of_an_edge_object_is_released_and_read_after_the_build(self):
        # a stop or signal gets its model when the build runs: no cancel, the record is read afterwards by the simulation half
        self.start()
        self.B.tool("{ edge = 130, param = 0.75 }", make="NATIVE_SIGNAL")
        self.tick(24)
        self.assertEqual((self.B.native.released, self.B.native.dropped), ([1], []))
        self.assertIn("waits for the build", self.B.log())
        self.assertEqual(self.relay.chats, [])

    def test_without_a_fireable_callback_it_is_released_as_before(self):
        self.fires_off = True
        self.start()
        self.B.native.fires = False
        self.B.run("FACTORY_REJECT = nil")
        i = int(self.B.L.eval("TOOL_MAKE(%s)" % self.road(300, 0, 400, 0)))
        self.B.L.execute("GUI.guiHandleEvent(nil, STATE, GSTATE, 0, 'streetBuilder', 'builder.proposalCreate', { 'junk' })")
        self.B.L.execute("TOOL_ADD(%d, true)" % i)
        self.tick(14)
        self.assertEqual((self.B.native.released, self.B.native.dropped), ([1], []))


class ConstructionBlockWithoutFireTest(StrictTest):
    extra_env = {"FEVERLINK3_REMOTE_CONSTRUCTIONS": "0"}
    fires = False

    def test_without_a_fireable_callback_the_build_is_not_cancelled(self):
        self.start()
        self.B.tool(ConstructionDefaultTest.DEPOT, make="NATIVE_DEPOT_ON_ROAD")
        self.tick(20)
        self.assertEqual((self.B.native.released, self.B.native.dropped), ([1], []), "a cancel would leave the tool dead")


class FireDoneChoiceTest(StrictTest):
    """FEVERLINK3_STRICT_FIRE_DONE=1 with FEVERLINK3_STRICT_CANCEL=plans: a build is cancelled only when the module says it fires the tool's
    callback (`done` in the held event); otherwise the build is held and released."""

    strict_cancel = "plans"
    extra_env = {"FEVERLINK3_STRICT_FIRE_DONE": "1"}
    fires = True

    def test_a_road_is_cancelled_when_the_module_fires_the_callback(self):
        self.start()
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(24)
        self.assertEqual((self.B.native.dropped, self.B.native.released), ([1], []))
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
        self.assertSameWorld()

    def test_a_road_stays_hold_and_release_when_it_cannot(self):
        self.start()
        self.B.native.fires = False
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(24)
        self.assertEqual((self.B.native.released, self.B.native.dropped), ([1], []))
        self.assertIn("cannot fire this tool's callback", self.B.log())
        self.assertSameWorld()


class RemovalsCancelTest(StrictTest):
    """FEVERLINK3_STRICT_CANCEL=removals with FEVERLINK3_STRICT_FIRE_DONE=1: a build that only takes things away (the bulldozer removing a town
    building) is cancelled and replayed at the stamp on every game, the originator included; anything that adds stays hold-and-release.
    (Real run 2026-10-13: the originator's engine applied its held bulldozer command 1000 ms after the stamp, a house regrew differently.)"""

    strict_cancel = "removals"
    extra_env = {"FEVERLINK3_STRICT_FIRE_DONE": "1"}
    fires = True

    def test_a_demolition_is_cancelled_and_applied_at_the_stamp_by_every_game(self):
        self.start()
        self.B.tool("{ cons = { 54 } }", make="NATIVE_BULLDOZE")
        self.tick(24)
        self.assertEqual((self.B.native.dropped, self.B.native.released), ([1], []))
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
        self.assertSameWorld()

    def test_a_road_is_still_held_and_released(self):
        self.start()
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(24)
        self.assertEqual((self.B.native.released, self.B.native.dropped), ([1], []))
        self.assertSameWorld()

    def test_a_demolition_stays_hold_and_release_when_the_module_cannot_fire_the_callback(self):
        self.start()
        self.B.native.fires = False
        self.B.tool("{ cons = { 54 } }", make="NATIVE_BULLDOZE")
        self.tick(24)
        self.assertEqual((self.B.native.released, self.B.native.dropped), ([1], []))
        self.assertSameWorld()


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
        self.assertEqual(set(kinds), {"ping"})
        # six pings 2.5 s apart, then one every 5 s while it waits for an answer (16 s here)
        self.assertLessEqual(len(kinds), 8, "a few pings, then it only asks now and then")
        self.assertIn("does not answer pings", self.B.log())
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(16)
        self.assertSameWorld()
        self.assertEqual(self.B.shipped_count(), 1, "the old path")

    def test_a_module_that_answers_late_is_armed_when_it_answers(self):
        self.start()
        self.tick(80)
        self.assertIn("does not answer pings", self.B.log())
        self.assertFalse(self.B.native.holding)
        self.B.native.is_open = True   # the module starts reading: the pings in the file are answered
        self.tick(40)
        self.assertIn("mode re-armed", self.B.log())
        self.strict_on(self.B)
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(20)
        self.assertEqual(self.B.native.held, [1], "the build is held again")
        self.assertSameWorld()


class WatchdogTest(StrictTest):
    """Real run 2026-10-14 (rail-crash): the host built every 2-3 s; every build wrote defer / inject_next / release lines, so the mod never
    pinged (it pinged only after NAT_PING s without any line), no pong came, the watchdog switched strict mode off after 8 s with the module
    alive, and gave up for good. The next station was applied at the click, the worlds forked, and the guest's next track crashed the host."""

    def build_every(self, game, seconds, count, x0=300):
        for i in range(count):
            game.tool(self.road(x0 + 200 * i, 0, x0 + 200 * i + 100, 0))
            self.tick(int(round(seconds / 0.2)))

    def test_fast_building_keeps_the_pings_and_strict_mode_on(self):
        self.start()
        self.strict_on(self.A, self.B)
        t0 = self.clock.t
        self.build_every(self.B, 1.4, 14)   # ~20 s, a release line every 1.4 s
        self.tick(20)
        self.assertGreater(self.clock.t - t0, 20)
        self.strict_on(self.A, self.B)
        self.assertNotIn("stopped answering", self.log())
        self.assertNotIn("mode is off", self.log())
        self.assertEqual(self.B.native.ctl_kinds().count("hold_on"), 1, "never switched off and on again")
        times = [t for t, _, kind, _ in self.B.native.heard if kind == "ping" and t >= t0]
        gaps = [b - a for a, b in zip(times, times[1:])]
        self.assertGreaterEqual(len(times), 9, "pings went on while builds wrote the file")
        self.assertLessEqual(max(gaps), 2.2, gaps)
        self.assertEqual(self.B.native.held, list(range(1, 15)), "every build held")
        self.assertEqual(self.B.native.released, list(range(1, 15)), "and released at its stamp")
        self.assertSameWorld()

    def test_any_event_of_the_module_keeps_the_watchdog_alive(self):
        self.start()
        self.strict_on(self.B)
        # the module stops answering pings (a lost pong) but keeps writing events: no switch-off
        native = self.B.native
        orig = native.handle

        def no_pong(seq, kind, p):
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

    def test_a_silent_module_is_switched_off_and_re_armed_when_it_answers_again(self):
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
        self.assertEqual(self.B.native.held, [1], "a build after the re-arm is held")
        self.assertEqual(self.B.native.released, [1])
        self.assertSameWorld()
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))

    def test_three_faults_switch_strict_mode_off_for_a_while_not_for_good(self):
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


class PausedGraceTest(StrictTest):
    """A build made while the session is PAUSED: the originator holds its command for a real-time grace, the peers dry-run the record
    and park it, and only the originator's commit makes every game apply it (real run 2026-10-11: a veto came too late and the
    guest's game built a road the host could not)."""

    # the cancel path needs a tool callback the module can fire (otherwise a cancel leaves the tool dead: see NoCancelWithoutCallbackTest)
    extra_env = {"FEVERLINK3_STRICT_FIRE_DONE": "1"}
    fires = True

    def pause(self):
        self.start(speed=2)
        self.tick(10)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(18)
        self.assertEqual((self.A.time(), self.B.time()), (self.relay.pauseAt, self.relay.pauseAt))

    def test_nobody_applies_the_build_before_the_grace_is_over_and_the_commit_is_sent(self):
        self.pause()
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(1)
        self.assertEqual(self.B.native.released, [], "the originator holds its command through the grace")
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 0), "the peer has parked the replay: nothing applied yet")
        self.assertEqual(self.relay.commits, [])
        self.assertIn("parked until its originator commits it", self.A.log())
        self.assertIn("for a veto before it is committed", self.B.log())
        self.tick(14)
        self.assertEqual(self.relay.commits, [(1, 1, 1, self.relay.pauseAt)], "one commit, at the pause time")
        self.assertEqual(self.B.native.released, [1])
        self.assertEqual(self.A.builds(), ["makeWorldBuildProposalCmd@%d" % self.relay.pauseAt])
        self.assertEqual(self.B.builds(), ["makeWorldBuildProposalCmd@%d" % self.relay.pauseAt])
        self.assertSameWorld()
        self.assertEqual(self.relay.vetoes, [])
        self.assertEqual(self.B.shipped_count(), 1)
        self.assertIn("committed by its originator", self.A.log())

    def test_the_grace_is_chosen_from_the_round_trip_and_logged(self):
        self.pause()
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(2)
        import re
        m = re.search(r"waits (\d+) ms \(round trip about (\d+) ms\)", self.B.log())
        self.assertIsNotNone(m, self.B.log())
        grace, rtt = int(m.group(1)), int(m.group(2))
        self.assertGreaterEqual(grace, 250)
        self.assertLessEqual(grace, 1500)
        self.assertGreaterEqual(grace, min(1500, 2 * rtt))

    def test_a_veto_during_the_grace_drops_the_build_everywhere_and_says_so_once(self):
        self.pause()
        before = (self.A.sig(), self.B.sig())
        self.A.run("WORLD[110] = nil")   # the host cannot build this split
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(20)
        self.assertEqual(len(self.relay.vetoes), 1)
        self.assertEqual(self.relay.commits, [], "no commit after a veto")
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 0))
        self.assertEqual(self.B.native.released, [])
        self.assertEqual(self.B.native.dropped, [1], "cancelled: the build exists on no game")
        self.assertEqual(self.B.sig(), before[1])
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

    def test_a_game_that_is_unpaused_during_the_grace_gives_the_build_a_normal_stamp(self):
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

    def test_a_parked_build_whose_commit_never_comes_is_dropped_by_the_peer(self):
        self.pause()
        orig = self.relay.send

        def send(g, kind, frm, payload):
            if kind == "build_commit":
                return None
            return orig(g, kind, frm, payload)
        self.relay.send = send
        self.B.tool(self.road(300, 0, 400, 0))
        self.frames(260)   # 13 s of interface frames
        self.assertIn("originator never committed it: dropped", self.A.log())
        self.assertEqual(self.A.builds_applied(), 0)

    def test_a_peer_that_never_answers_is_no_veto(self):
        self.pause()
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(16)
        self.assertEqual(self.relay.vetoes, [])
        self.assertEqual(self.B.native.released, [1])
        self.assertSameWorld()

    def test_a_second_build_in_the_pause_that_needs_the_first_one_works(self):
        self.pause()
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(14)
        nb = int(self.B.lua("FIND_NODE(400, 0, 0)"))
        self.B.tool("{ nodeA = %d, b = { x = 500, y = 100, z = 0 } }" % nb)
        self.tick(14)
        self.assertSameWorld()
        self.assertEqual(self.relay.vetoes, [])
        self.assertEqual(self.B.native.released, [1, 2])
        self.assertEqual(len(self.relay.commits), 2)

    def test_a_depot_with_its_street_then_a_road_from_its_street_end_in_a_paused_game(self):
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
        self.assertEqual(self.relay.vetoes, [])
        self.assertEqual(self.B.native.released, [1, 2])


class VetoWordsTest(StrictTest):
    def test_a_veto_that_comes_after_the_release_says_plainly_that_the_build_exists_on_one_game_and_asks_for_a_resync(self):
        self.start()
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(16)
        self.assertEqual(self.B.native.released, [1])
        # a veto from the host arrives late
        self.relay.send(self.B, "build_veto", 0, self.relay.ser({"seq": 1, "owner": 1, "why": "unresolved segment 1.000,2.000,0.000 - 3.000,4.000,0.000", "from": 0, "name": "host"}))
        self.tick(2)
        self.assertEqual(len(self.relay.chats), 1)
        text = self.relay.chats[0][1]
        self.assertIn("client's build exists on their game only", text)
        self.assertIn("the games will be synchronised", text)
        self.assertNotIn("cancelled on every game", text)
        self.assertEqual(len(self.relay.replay_failed), 1, "a resync is asked for")
        self.assertIn("came too late", self.B.log())

    def test_a_second_veto_of_the_same_build_adds_no_second_line(self):
        self.start()
        self.A.run("WORLD[110] = nil")
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(10)
        self.relay.send(self.B, "build_veto", 0, self.relay.ser({"seq": 1, "owner": 1, "why": "unresolved node 1,2,3", "from": 0, "name": "host"}))
        self.tick(2)
        self.assertEqual(len(self.relay.chats), 1)

    def test_the_log_of_the_vetoing_game_does_not_claim_the_build_was_applied_there(self):
        self.start()
        self.A.run("WORLD[110] = nil")
        self.B.tool(SPLIT, make="NATIVE_SPLIT")
        self.tick(10)
        self.assertNotIn("vetoed too late: it was applied already", self.A.log())
        self.assertIn("never entered this game's queue: this game vetoed it", self.A.log())
        self.assertNotIn("Player 1", self.A.log() + self.B.log())


class HeldAcrossResyncTest(StrictTest):
    """Real run 2026-10-11: a held host build was never released at its stamp, a resync saved a world without it and the module's
    30 s limit put it into the world later, shipped a second time."""

    # the cancel path needs a tool callback the module can fire (otherwise a cancel leaves the tool dead: see NoCancelWithoutCallbackTest)
    extra_env = {"FEVERLINK3_STRICT_FIRE_DONE": "1"}
    fires = True

    def test_a_town_build_is_not_taken_for_the_early_release_of_a_held_command(self):
        self.start()
        self.A.tool(self.road(300, 0, 400, 0))
        self.tick(1)
        self.assertEqual(self.A.native.held, [1])
        # the engine itself builds something (playerInitiated false: a town growing a road)
        self.A.L.execute("SIM_EVENT(0, 'apply_command', 'onPreBuildProposal', { SIMPLE_ROAD{ a = { x = 700, y = 0, z = 0 }, b = { x = 800, y = 0, z = 0 } }, nil, nil, false })")
        self.assertNotIn("is the early release of the held command", self.A.log())
        # the module gives the command back by its age limit
        self.A.native.ring[0]["phase"], self.A.native.ring[0]["reason"] = "release", "timeout"
        self.A.native.on_add(0, 0)
        self.tick(16)
        self.assertIn("is the early release of the held command of 0:1", self.A.log())
        self.assertEqual(self.A.shipped_count(), 1, "shipped once: the late release is not a new build")
        self.assertNotIn("native build captured", self.A.log())

    def test_a_resync_request_while_a_build_is_held_releases_it_before_the_save_and_no_game_applies_it_twice(self):
        self.start()
        self.A.tool(self.road(300, 0, 400, 0))
        self.tick(1)
        self.assertEqual(self.A.native.held, [1])
        self.assertEqual(self.A.native.released, [])
        self.relay.send(self.A, "resync_save", 0, self.relay.ser({"id": 1, "name": "feverlink3-resync-1"}))
        self.tick(3)
        self.assertEqual(self.A.native.released, [1], "released before the save")
        self.assertIn("released before the resync save", self.A.log())
        self.assertEqual([x[:3] for x in self.relay.saves_done], [(0, 1, True)], "the save happened after the engine applied it")
        self.assertIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.A.sig(), "the saved world contains the build")
        self.tick(16)
        self.assertEqual(self.A.builds_applied(), 1, "applied once on the originator")
        self.assertEqual(self.A.shipped_count(), 1)
        self.assertNotIn("native build captured", self.A.log())

    def test_a_held_build_whose_stamp_passed_without_a_release_is_failed_by_the_watchdog_never_released_late(self):
        # 2026-10-15: a release here applied the build steps after the stamp the other game used (other entity ids); the command is
        # failed instead (here a cancel with the callback fired: this fake module has no `fail`) and the games are repaired
        self.start()
        self.A.tool(self.road(300, 0, 400, 0))
        self.tick(1)
        # the interface half never sees the simulation half's release result
        self.A.L.execute("local g = STATE.get; STATE.get = function(self) local v = g(self); if v and v.results then local k = {}; for _, r in ipairs(v.results) do if not r.release then k[#k + 1] = r end end; v.results = k end; return v end")
        self.tick(24)
        self.assertEqual(self.A.native.released, [], "never released late")
        self.assertEqual(self.A.native.dropped, [1])
        self.assertIn("WATCHDOG: the stamp", self.A.log())
        self.assertIn("FAILED, NOT released: cancel written (the tool's callback is fired)", self.A.log())
        self.assertEqual(self.A.builds_applied(), 0)
        self.assertEqual(self.B.builds_applied(), 1)
        self.assertTrue(any(gid == 0 and "was shipped but failed on its originator" in d for gid, d in self.relay.replay_failed), self.relay.replay_failed)
        self.assertEqual(self.A.shipped_count(), 1)
        self.assertNotIn("native build captured", self.A.log())

    def test_a_build_held_in_a_game_that_stays_paused_before_its_stamp_is_cancelled_everywhere_before_the_module_gives_it_back(self):
        self.start(speed=2)
        self.tick(6)
        self.A.tool(self.road(300, 0, 400, 0))
        self.tick(1)
        self.assertEqual(self.A.native.held, [1])
        # the game is stopped (frames go on, the clock does not) for longer than the limit
        self.frames(500)
        self.assertEqual(self.A.native.released, [], "never given back early")
        self.assertEqual(self.A.native.dropped, [1])
        self.assertTrue(any("cancelled on every game" in c[1] for c in self.relay.chats))
        self.assertEqual(self.B.builds_applied(), 0)


if __name__ == "__main__":
    unittest.main()
