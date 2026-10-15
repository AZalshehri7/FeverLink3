"""Fail closed (2026-10-15): a build whose exact copy (the originator's native text, injected by the native module) cannot be applied on a
game is applied NOWHERE on that game, never replaced by that game's own rebuild of the record; the game asks for the repair (a resync), and
the originator's held command is never left busy.

Two mocked games run the real mod with the fake native module of test_strict_build.py, extended here with what the deferred inject flow
needs (docs/NATIVE_LINK.md): the `ready` hooks inject_next / defer / fail, the text of every held command (native_text_<id>.txt), and the
module's side of an inject order at the simulation's apply of the script's build command. An injected command is applied as it is (the
mock's stand-in for the engine Proposal built from the text); a refused one is EMPTIED (applies nothing), as the Rust module does now
(`inject::neutralise`), and the deferred held command of the same build is failed (its tool callback fired with a failed result).

Run:  python -I -m unittest discover -s tests/lua -p "test_inject_fail_closed.py"
"""
import os
import shutil
import struct
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import test_lockstep as T  # noqa: E402
import test_strict_build as S  # noqa: E402


def road_text(x0=300.0, x1=400.0):
    """The originator's portable text of a road (what the native module writes as native_text_<id>.txt): no existing entity named."""
    def f(x):
        return "%08x" % struct.unpack("<I", struct.pack("<f", x))[0]

    def v(x, y, z):
        return "v3:%s,%s,%s" % (f(x), f(y), f(z))
    rows = [
        ("proposal.addedNodes.#", "n:2"), ("proposal.addedNodes[0].entity", "ent:new#1"), ("proposal.addedNodes[0].position", v(x0, 0, 0)),
        ("proposal.addedNodes[1].entity", "ent:new#2"), ("proposal.addedNodes[1].position", v(x1, 0, 0)),
        ("proposal.addedSegments.#", "n:1"), ("proposal.addedSegments[0].entity", "ent:new#3"),
        ("proposal.addedSegments[0].position0", v(x0, 0, 0)), ("proposal.addedSegments[0].position1", v(x1, 0, 0)),
        ("proposal.addedSegments[0].tangent0", v(x1 - x0, 0, 0)), ("proposal.addedSegments[0].tangent1", v(x1 - x0, 0, 0)),
        ("proposal.addedSegments[0].typeIndex", "i32:00000001"), ("proposal.addedSegments[0].roadType", "u32:00000000"),
        ("proposal.addedSegments[0].roadTemplate", 'res:"street/new_medium_road.street_template"'), ("proposal.addedSegments[0].roadStyle", 'res:"medium"'),
        ("toRemove.#", "n:0"), ("toAdd.#", "n:0"), ("kind", "u8:34"),
    ]
    return "\n".join(k + "\t" + val for k, val in rows)


def text_xs(text):
    """The x of every added node of a portable text (the place the fake module matches a command by)."""
    xs = []
    for line in (text or "").splitlines():
        k, _, v = line.partition("\t")
        if k.startswith("proposal.addedNodes[") and k.endswith("].position") and v.startswith("v3:"):
            xs.append(struct.unpack("<f", struct.pack("<I", int(v[3:].split(",")[0], 16)))[0])
    return xs


def meets(a, b):
    return any(abs(x - y) <= 0.25 for x in a for y in b)


class InjectNative(S.FakeNative):
    """The fake module with the deferred inject flow. `refuse_all`: every inject order is refused (a receiver whose copy cannot be made).

    Like the Rust module (native/src/inject.rs, 2026-10-15), the orders wait in a queue in the order the mod wrote them, and a script
    build command takes the first waiting order that identifies it: an inject_next by its text's place (here: the x of its added
    nodes), a complete_next by its held command's place. A command that is no order's passes untouched."""

    def __init__(self, game, clock):
        self.native_text = None   # written as native_text_<id>.txt for every held command
        self.refuse_all = None    # a reason: every inject order is refused with it
        self.orders = []          # the waiting inject_next / complete_next orders, in the order they were written
        self.used = []            # (kind, key, held, xs) of every order a command took, in that order
        self.most_waiting = 0     # the most orders that waited at once when a command came
        self.deferred = {}        # id -> True: held commands kept until this game's own stamp apply
        self.completed = []       # (id, ok, reason) of every completion the module reported
        self.injected, self.refused, self.emptied = [], [], 0
        super().__init__(game, clock, hooks=("apply_command", "command_list_add", "inject_next", "defer", "fail"))

    def handle(self, seq, kind, p):
        if kind == "defer":
            self.last_valid = self.clock.t
            self.heard.append((self.clock.t, seq, kind, p))
            hid = int(p.get("id", -1))
            e = next((e for e in self.ring if e["id"] == hid), None)
            if e is None:
                self.emit("error", {"where": "defer", "detail": "no held command %s" % hid})
                return
            self.ring = [x for x in self.ring if x["id"] != hid]
            self.deferred[hid] = True
        elif kind == "fail":
            self.last_valid = self.clock.t
            self.heard.append((self.clock.t, seq, kind, p))
            self.fail_held(int(p.get("id", -1)), "fail")
        elif kind in ("inject_next", "complete_next"):
            self.last_valid = self.clock.t
            self.heard.append((self.clock.t, seq, kind, p))
            self.orders.append(dict(p, kind=kind))
        else:
            super().handle(seq, kind, p)

    def finish_held(self, hid, ok, reason):
        """A deferred held command leaves: its tool callback is fired (with the stamp apply's result, or failed); it is never applied."""
        if not self.deferred.pop(hid, None):
            return False
        self.game.L.globals().NATIVE_DROP(hid)
        self.completed.append((hid, ok, reason))
        self.emit("completed", {"id": hid, "ok": ok, "fired": True, "ms": int(self.clock.t * 1000), "reason": reason})
        return True

    def fail_held(self, hid, reason):
        if not self.finish_held(hid, False, reason):
            self.emit("error", {"where": "fail", "detail": "no held command %s" % hid})

    def identifies(self, o, xs):
        """Whether order `o` is the command whose added nodes lie at `xs`."""
        if o["kind"] == "complete_next":
            want = list(self.game.L.globals().HELD_XS(int(o.get("held", 0) or 0)).values())
        else:
            path = os.path.join(self.game.dir, "native_inject_%d.txt" % int(o.get("key", 0)))
            want = []
            if os.path.exists(path):
                with open(path, encoding="utf-8") as f:
                    want = text_xs(f.read())
        return meets(want, xs)

    def inject_hook(self, xs=None):
        """The simulation applies a script build command: "emptied" when the module refused the order (the command applies nothing)."""
        self.read_ctl()
        self.most_waiting = max(self.most_waiting, len(self.orders))
        xs = list(xs.values()) if xs is not None else []
        o = next((o for o in self.orders if self.identifies(o, xs)), None)
        if o is None:
            return "pass"   # no waiting order's command: untouched, every order keeps waiting
        self.orders.remove(o)
        held = int(o.get("held", 0) or 0)
        self.used.append((o["kind"], int(o.get("key", 0) or 0), held, xs))
        if o["kind"] == "complete_next":
            self.finish_held(held, True, "stamp_apply")
            return "pass"
        key = int(o.get("key", 0))
        why = self.refuse_all or (o.get("refuse") and "forced refusal (%s)" % o.get("refuse"))
        if why:
            self.refused.append(key)
            self.emptied += 1
            self.emit("inject_refused", {"key": key, "held": held, "outcome": "nothing", "reason": "key %d: %s" % (key, why)})
            if held:
                self.fail_held(held, "fail")
            self.flush_events()
            return "emptied"
        self.injected.append(key)
        self.emit("injected", {"key": key, "nodes": 2, "segments": 1, "constructions": 0})
        if held:
            self.finish_held(held, True, "stamp_apply")
        self.flush_events()
        return "pass"

    def on_add(self, kind, tool):
        hid = super().on_add(kind, tool)
        if hid and self.native_text is not None:
            with open(os.path.join(self.game.dir, "native_text_%d.txt" % hid), "w", encoding="utf-8", newline="") as f:
                f.write(self.native_text)
        return hid


INJECT_LUA = r'''
-- the x of every node a build command adds: the place the fake module identifies a command by
function CMD_XS(c)
  local xs = {}
  local p = c and c.args and c.args[1]
  if not p then return xs end
  local nodes
  pcall(function() nodes = p.proposal.addedNodes end)
  if not nodes or #nodes == 0 then pcall(function() nodes = p.streetProposal.nodesToAdd end) end
  for _, n in ipairs(nodes or {}) do
    pcall(function() xs[#xs + 1] = n.comp.position.x end)
  end
  return xs
end
function HELD_XS(id) local h = NATIVE_HELD[id]; return CMD_XS(h and h[1]) end

-- the module's side of an inject order at the simulation's apply of a script build command (outermost, before the strict hook)
local BASE_MAKE_API_INJECT = MAKE_API
function MAKE_API()
  local api = BASE_MAKE_API_INJECT()
  local base = api.cmd.sendCommand
  local function applyNow(c, cb, progress)
    if INJECT_HOOK and c and c.__cmd == "makeWorldBuildProposalCmd" and INJECT_HOOK(CMD_XS(c)) == "emptied" then
      EMPTIED = (EMPTIED or 0) + 1   -- the module emptied the command: the engine applies an empty proposal, nothing is built
      return
    end
    return base(c, cb, progress)
  end
  api.cmd.sendCommand = function(c, cb, progress)
    -- a test sets LATE_APPLIES = {}: the engine applies build commands only after the script's step (FLUSH_LATE), in the order sent,
    -- so that every order of the step is written before the module sees the first command
    if LATE_APPLIES and c and c.__cmd == "makeWorldBuildProposalCmd" then
      LATE_APPLIES[#LATE_APPLIES + 1] = function() return applyNow(c, cb, progress) end
      return
    end
    return applyNow(c, cb, progress)
  end
  return api
end
function FLUSH_LATE()
  local q = LATE_APPLIES or {}
  if LATE_APPLIES then LATE_APPLIES = {} end
  for _, f in ipairs(q) do f() end
end
'''


class InjectGame(S.StrictGame):
    def __init__(self, label, player_id, tmp, first_entity, clock):
        S.N.NativeGame.__init__(self, label, player_id, tmp, first_entity)
        clock.games.append(self)
        self.native = InjectNative(self, clock)
        g = self.L.globals()
        g.FAKE_T = clock.t
        g.NATIVE_HOOK = self.native.on_add
        g.INJECT_HOOK = self.native.inject_hook
        self.L.execute(S.STRICT_LUA)
        self.L.execute(INJECT_LUA)
        self.reload()


class InjectFailClosedTest(S.StrictTest):
    strict_cancel = None   # the default (removals): with the inject and defer capabilities every build goes the deferred inject flow
    extra_env = {"FEVERLINK3_STRICT_FIRE_DONE": "0", "FEVERLINK3_DEBUG_REFUSE_INJECT": None, "FEVERLINK3_NATIVE_INJECT": None}

    def setUp(self):
        self._saved_env = {k: os.environ.get(k) for k in self.extra_env}
        for k, v in self.extra_env.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
        self._old_cancel = os.environ.get("FEVERLINK3_STRICT_CANCEL")
        os.environ.pop("FEVERLINK3_STRICT_CANCEL", None)
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-inject-")
        self.clock = S.Clock()
        self.A = InjectGame("host", 0, self.tmp, 50000, self.clock)
        self.B = InjectGame("client", 1, self.tmp, 51000, self.clock)
        self.relay = T.Relay([self.A, self.B])

    def tearDown(self):
        super().tearDown()
        shutil.rmtree(self.tmp, ignore_errors=True)

    def build_on_b(self):
        """B's road tool builds; both games run to well past the stamp."""
        self.start()
        self.strict_on(self.A, self.B)
        for g in (self.A, self.B):
            g.native.native_text = road_text()
            g.native.input_text = road_text()
        self.B.tool(self.road(300, 0, 400, 0), make="NATIVE_ROAD")
        self.tick(30)

    def replay_failed_by(self, gid):
        return [d for g, d in self.relay.replay_failed if g == gid]

    def test_the_happy_path_injects_on_both_games_and_completes_the_originators_held_command(self):
        self.build_on_b()
        self.assertIn("defer", self.B.native.ctl_kinds(), "the originator keeps its command until its own stamp apply")
        self.assertEqual((len(self.A.native.injected), len(self.B.native.injected)), (1, 1), self.log())
        self.assertEqual(self.B.native.completed, [(1, True, "stamp_apply")], "the tool's callback took the stamp apply's result")
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
        self.assertSameWorld()
        self.assertEqual(self.relay.replay_failed, [])

    def test_a_receiver_that_cannot_inject_applies_nothing_asks_for_a_resync_and_the_originators_tool_is_freed(self):
        self.A.native.refuse_all = "the proof refused the text"
        self.build_on_b()
        # the receiver: the order was refused, the command emptied; nothing of the build (and no rebuild of the record) is applied there
        self.assertEqual(self.A.native.refused, [1])
        self.assertEqual(self.A.native.emptied, 1)
        self.assertEqual(self.A.builds_applied(), 0, "no build on the refusing game: neither the text nor a guess of it")
        self.assertIsNone(self.A.lua("FIND_EDGE(300, 0, 400, 0)"))
        self.assertNotIn("the record path builds it", self.A.log())
        # the refusal goes through the session: the refusing game asks the host for the repair
        self.assertEqual(len(self.replay_failed_by(0)), 1, self.relay.replay_failed)
        self.assertIn("inject order 1 refused here (nothing)", self.replay_failed_by(0)[0])
        self.assertIn("asking the host for a resync", self.A.log())
        # the originator built it and its held command completed with that apply: the tool is free
        self.assertEqual(self.B.builds_applied(), 1)
        self.assertEqual(self.B.native.completed, [(1, True, "stamp_apply")])
        self.assertEqual(int(self.B.lua("NATIVE_HELD_COUNT()")), 0, "nothing is held any more on the originator")
        self.assertEqual(self.replay_failed_by(1), [], "the originator's world has the build: it asks for nothing")

    def test_the_originators_own_refused_copy_applies_nothing_there_and_fails_its_held_command(self):
        os.environ["FEVERLINK3_DEBUG_REFUSE_INJECT"] = "1"   # read when the bridge loads: reload B's halves
        self.B.reload()
        self.build_on_b()
        orders = [p for _, k, p in self.B.native.ctl_records() if k == "inject_next"]
        self.assertEqual(len(orders), 1)
        self.assertEqual(orders[0].get("refuse"), "FEVERLINK3_DEBUG_REFUSE_INJECT=1")
        self.assertEqual(self.B.builds_applied(), 0, "the originator applies nothing of a copy it could not make")
        self.assertEqual(self.B.native.completed, [(1, False, "fail")], "its held command failed: the tool callback fired, the tool is free")
        self.assertEqual(int(self.B.lua("NATIVE_HELD_COUNT()")), 0)
        self.assertEqual(len(self.replay_failed_by(1)), 1, self.relay.replay_failed)
        # the other game injected it
        self.assertEqual(self.A.builds_applied(), 1)

    def test_a_receiver_whose_placeholder_cannot_be_made_refuses_at_the_decode_and_asks_for_a_resync(self):
        # the factory refuses the receiver's placeholder: before, the record path's other forms built a guess of the road there
        self.start()
        self.strict_on(self.A, self.B)
        for g in (self.A, self.B):
            g.native.native_text = road_text()
            g.native.input_text = road_text()
        self.A.run("FACTORY_REJECT = function() return 'Unknown exception' end")
        self.B.tool(self.road(300, 0, 400, 0), make="NATIVE_ROAD")
        self.tick(30)
        self.assertEqual(self.A.builds_applied(), 0)
        self.assertEqual(self.A.native.injected + self.A.native.refused, [], "no order was written: nothing was sent")
        self.assertIn("inject_refused: the placeholder command cannot be made here", self.A.log())
        self.assertTrue(any("not applied here: inject_refused" in d for d in self.replay_failed_by(0)), self.relay.replay_failed)
        self.assertEqual(self.B.native.completed, [(1, True, "stamp_apply")])

    def test_two_players_builds_stamped_in_the_same_step_are_both_applied_and_each_tool_takes_its_own_builds_result(self):
        """Open bug 7: two builds due in one step write two orders on every game. Each command takes its own order (by its place), the
        originator's held command takes the result of its own build's apply (not the other player's), and both tools are free."""
        self.two_players_in_one_step(late=False)

    def test_with_both_orders_waiting_before_either_command_each_command_still_takes_its_own(self):
        """The same with the engine applying the step's build commands only after the script's step: both orders of the step wait in
        the module at once (one slot kept only the newest before), and each command is matched to its own by its place."""
        self.two_players_in_one_step(late=True)
        for g in (self.A, self.B):
            self.assertEqual(g.native.most_waiting, 2, "%s: both orders waited together" % g.label)

    def test_two_simulation_states_that_take_turns_never_lose_an_order(self):
        """Real run b3real (2026-10-15): the game ran the simulation half in two Lua states that took turns, sharing the script's
        state; each kept its own control writer, so every other order carried a number the other state had already used and the
        module dropped it as old ("1 control line(s) discarded"): the originator's command went by without its order, its held
        command was failed 9 s after the stamp, the other game injected the build (a fork). Here B's simulation half alternates
        between two states every step; one player builds four roads one after the other."""
        self.start()
        self.strict_on(self.A, self.B)
        self.B.run("SIM_OTHER = LOAD_HALF('sim')")
        texts = []
        for i in range(4):
            x0 = 300.0 + 200.0 * i
            text = road_text(x0, x0 + 100.0)
            texts.append(text)
            for g in (self.A, self.B):
                g.native.native_text = g.native.input_text = text
            self.B.tool(self.road(int(x0), 0, int(x0) + 100, 0), make="NATIVE_ROAD")
            for _ in range(13):
                self.tick(1)
                self.B.run("SIM, SIM_OTHER = SIM_OTHER, SIM")   # the next step runs in the other state
        held = [p for _, k, p in self.B.native.ctl_records() if k == "inject_next"]
        self.assertEqual(len(held), 4, held)
        self.assertEqual(self.B.native.completed, [(i, True, "stamp_apply") for i in range(1, 5)], self.log())
        self.assertEqual(getattr(self.B.native, "discarded", 0), 0, "no control line was dropped as old")
        self.assertEqual(len(self.B.native.injected), 4)
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (4, 4))
        self.assertSameWorld()
        self.assertEqual(self.relay.replay_failed, [])

    def two_players_in_one_step(self, late):
        self.start()
        self.strict_on(self.A, self.B)
        self.A.native.native_text = self.A.native.input_text = road_text(600.0, 700.0)
        self.B.native.native_text = self.B.native.input_text = road_text(300.0, 400.0)
        if late:
            for g in (self.A, self.B):
                g.run("LATE_APPLIES = {}")
        # both players confirm their road in the same frame
        self.A.tool(self.road(600, 0, 700, 0), make="NATIVE_ROAD")
        self.B.tool(self.road(300, 0, 400, 0), make="NATIVE_ROAD")
        for _ in range(30):
            self.tick(1)
            for g in (self.A, self.B):
                g.run("FLUSH_LATE()")
        stamps = [r["stamp"] for g in (self.A, self.B) for r in g.action_records()]
        self.assertEqual(len(stamps), 2)
        self.assertEqual(stamps[0], stamps[1], "both builds are due in the same step")
        for g in (self.A, self.B):
            orders = [p for _, k, p in g.native.ctl_records() if k == "inject_next"]
            self.assertEqual(len(orders), 2, "%s: one order per build" % g.label)
            self.assertEqual(len(g.native.injected), 2, "%s: both builds injected\n%s" % (g.label, self.log()))
            self.assertEqual(g.native.orders, [], "%s: no order left waiting" % g.label)
            self.assertEqual(g.builds_applied(), 2)
            self.assertIsNotNone(g.lua("FIND_EDGE(300, 0, 400, 0)"))
            self.assertIsNotNone(g.lua("FIND_EDGE(600, 0, 700, 0)"))
            self.assertEqual(int(g.lua("NATIVE_HELD_COUNT()")), 0, "%s: nothing is held any more: the tool is free" % g.label)
        # the same order of builds on both games (the stamp order: the host's action, then the client's)
        first_x = [[min(u[3]) for u in g.native.used] for g in (self.A, self.B)]
        self.assertEqual(first_x[0], first_x[1])
        self.assertEqual(first_x[0], [600.0, 300.0])
        # each originator's held command took its own build's apply: A's held 1 by the command at x 600, B's held 1 by the one at x 300
        self.assertEqual(self.A.native.completed, [(1, True, "stamp_apply")])
        self.assertEqual(self.B.native.completed, [(1, True, "stamp_apply")])
        self.assertEqual([(u[2], min(u[3])) for u in self.A.native.used if u[2]], [(1, 600.0)])
        self.assertEqual([(u[2], min(u[3])) for u in self.B.native.used if u[2]], [(1, 300.0)])
        self.assertSameWorld()
        self.assertEqual(self.relay.replay_failed, [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
