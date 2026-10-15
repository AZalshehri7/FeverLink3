"""A fault on the originator AFTER its held build was shipped (natDecideUnsafe writes the action, then more code runs).

Every other game applies a shipped build at the stamp, so the originator may only apply it at the stamp too, or fail it (the tool's
callback fired with a failed result, nothing applied there) and ask for a repair. It must never give the held command back at the click:
that applied the build here before the stamp (other entity ids than everywhere else) and the simulation half captured and shipped it a
second time. Before the fix a fault in the ship's trace line (Build.planOf, a string.format) reached natDecide's handler, which released.

The harness of test_strict_build.py (two mocked games running the real mod, a fake native module), imported, not edited. The fake module
here also knows `fail` (the real one has it together with `defer`): the command is failed at the next Add and `completed` is reported with
the reason, as native/src/runtime.rs does.

Run:  python -I -m unittest discover -s tests/lua -p "test_strict_ship_fault.py"
"""
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import test_strict_build as S  # noqa: E402

STEP = S.STEP


class FailingNative(S.FakeNative):
    """The fake module plus the `fail` order of the real one (a waiting or deferred command fails, its callback fired as failed)."""

    def handle(self, seq, kind, p):
        if kind != "fail":
            return super().handle(seq, kind, p)
        self.last_valid = self.clock.t
        self.heard.append((self.clock.t, seq, kind, p))
        e = next((e for e in self.ring if e["id"] == int(p.get("id", -1))), None)
        if e is None:
            self.emit("error", {"where": kind, "detail": "no held command %s" % p.get("id")})
        elif e["phase"] == "waiting":
            e["phase"] = "fail"

    def flush_due(self):
        for e in sorted(self.ring, key=lambda x: x["id"]):
            if e["phase"] == "fail":
                self.game.L.globals().NATIVE_DROP(e["id"])
                self.failed.append(e["id"])
                self.emit("completed", {"id": e["id"], "ok": False, "fired": True, "reason": "fail"})
        self.ring = [e for e in self.ring if e["phase"] != "fail"]
        super().flush_due()


# a fault the test can switch on: the next "queue" event of the interface half (route -> toSim) raises, after the action was written
FAULT_LUA = r'''
local BASE_MAKE_API_FAULT = MAKE_API
function MAKE_API()
  local api = BASE_MAKE_API_FAULT()
  local base = api.cmd.sendCommand
  api.cmd.sendCommand = function(c, cb, progress)
    if FAIL_NEXT_QUEUE and type(c) == "table" and c.__cmd == "makeScriptingSendEventCmd" and c.args and c.args[3] == "queue" then
      FAIL_NEXT_QUEUE = nil
      error("injected fault after the ship")
    end
    return base(c, cb, progress)
  end
  return api
end
-- Build.planOf raises while FAIL_PLANOF is set (the module is wrapped, its own source unchanged)
local src = CMDS_SRC.feverlink3_cmds_build
CMDS_SRC.feverlink3_cmds_build = "local M = (function(...)\n" .. src .. "\nend)(...)\n" ..
  "local base = M.planOf\nM.planOf = function(...) if FAIL_PLANOF then error('injected planOf fault') end return base(...) end\nreturn M"
'''


class ShipFaultTest(S.StrictTest):
    fail_capable = True   # the module reports `defer` (and with it `fail`) in its ready hooks

    def setUp(self):
        super().setUp()
        for g in (self.A, self.B):
            g.native.__class__ = FailingNative
            g.native.failed = []
            if self.fail_capable:
                g.native.hooks.append("defer")
                g.native.open_link()   # a second `ready` with the hook
            g.L.execute(FAULT_LUA)
            g.reload()

    def ctl_ids(self, g, kind):
        return [int(p.get("id", -1)) for _, k, p in g.native.ctl_records() if k == kind]

    def assertNeverReleasedAtTheClick(self, g):
        self.assertEqual(self.ctl_ids(g, "release"), [], "no release of the shipped command was ever written")
        self.assertEqual(g.native.released, [])

    def assertFailedNotApplied(self):
        """B made the build: nothing applied there, its tool freed, A applied it at the stamp, shipped once, a repair asked for."""
        stamp = self.B.action_records()[0]["stamp"]
        self.assertNeverReleasedAtTheClick(self.B)
        self.assertEqual(self.B.builds_applied(), 0, "nothing applied on the originator")
        self.assertEqual(self.A.builds(), ["makeWorldBuildProposalCmd@%d" % stamp], "the other game applied it at the stamp")
        self.assertEqual(self.B.L.eval("NATIVE_HELD_COUNT()"), 0, "the command does not stay held (the tool is freed)")
        self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 1), "shipped once")
        self.assertNotIn("native build captured", self.B.log())
        self.assertTrue(any(gid == 1 and "was shipped but failed on its originator" in d for gid, d in self.relay.replay_failed),
                        self.relay.replay_failed)
        self.assertIn("FAILED after the ship", self.B.log())
        self.assertIn("NOT released, nothing is applied on this game", self.B.log())

    # ---- a fault in what used to run after the ship: now made before it, harmless

    def test_a_fault_in_the_plan_of_the_ship_line_does_not_release_the_command_at_the_click(self):
        self.start()
        self.B.run("FAIL_PLANOF = true")
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(1)
        stamp = self.B.action_records()[0]["stamp"]
        self.assertNotIn("fault in the verdict on held 1", self.B.log())
        self.assertNeverReleasedAtTheClick(self.B)
        while self.B.time() < stamp - STEP:
            self.tick(1)
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 0), "nobody applies before the stamp")
            self.assertEqual(self.B.native.released, [])
        self.tick(8)
        self.assertEqual(self.B.native.released, [1], "released at the stamp")
        self.assertEqual(self.A.builds(), ["makeWorldBuildProposalCmd@%d" % stamp])
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
        self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 1), "shipped once")
        self.assertNotIn("native build captured", self.log())
        self.assertSameWorld()

    # ---- a fault after the ship in a step that cannot be made before it: failed, never released

    def test_a_fault_while_routing_a_held_build_fails_it_and_asks_for_a_repair(self):
        self.start()
        self.B.run("FAIL_NEXT_QUEUE = true")
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(1)
        self.assertIn("fault in the verdict on held 1", self.B.log())
        self.assertEqual(self.ctl_ids(self.B, "fail"), [1], "fail written")
        self.tick(16)
        self.assertEqual(self.B.native.failed, [1], "the module failed the command: its callback fired, nothing applied")
        self.assertFailedNotApplied()
        self.assertIn("at step route of the held flow", self.B.log())
        self.assertIn("FAILED without a stamp apply (fail)", self.B.log())
        # and the next build is strict again, held and released at its stamp
        self.B.tool(self.road(500, 0, 600, 0))
        self.tick(16)
        self.assertEqual(self.B.native.released, [2])
        self.assertEqual(self.B.builds_applied(), 1)


class ShipFaultCancelFlowTest(ShipFaultTest):
    """Cancel-and-replay with a fireable callback: the cancel is written before the route, so the fault leaves it cancelled."""

    strict_cancel = "1"
    extra_env = {"FEVERLINK3_STRICT_FIRE_DONE": None}
    fires = True

    def test_a_fault_in_the_plan_of_the_ship_line_does_not_release_the_command_at_the_click(self):
        self.start()
        self.B.run("FAIL_PLANOF = true")
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(16)
        stamp = self.B.action_records()[0]["stamp"]
        self.assertNotIn("fault in the verdict on held 1", self.B.log())
        self.assertNeverReleasedAtTheClick(self.B)
        self.assertEqual(self.B.native.dropped, [1], "cancelled (callback fired), the record replayed")
        self.assertEqual(self.A.builds(), ["makeWorldBuildProposalCmd@%d" % stamp])
        self.assertEqual(self.B.builds(), ["makeWorldBuildProposalCmd@%d" % stamp], "applied at the stamp on the originator too")
        self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 1))
        self.assertSameWorld()

    def test_a_fault_while_routing_a_held_build_fails_it_and_asks_for_a_repair(self):
        self.start()
        self.B.run("FAIL_NEXT_QUEUE = true")
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(16)
        self.assertIn("fault in the verdict on held 1", self.B.log())
        self.assertEqual(self.ctl_ids(self.B, "cancel"), [1])
        self.assertEqual(self.ctl_ids(self.B, "fail"), [], "cancelled already: no second order")
        self.assertEqual(self.B.native.dropped, [1])
        self.assertFailedNotApplied()
        self.assertIn("at step route of the cancel flow", self.B.log())
        self.assertIn("cancelled already", self.B.log())


class ShipFaultOldModuleTest(ShipFaultTest):
    """A module without `fail` and without a fireable callback: the command is cancelled (the tool may stay busy), never released."""

    fail_capable = False
    test_a_fault_in_the_plan_of_the_ship_line_does_not_release_the_command_at_the_click = None

    def test_a_fault_while_routing_a_held_build_fails_it_and_asks_for_a_repair(self):
        self.start()
        self.B.run("FAIL_NEXT_QUEUE = true")
        self.B.tool(self.road(300, 0, 400, 0))
        self.tick(16)
        self.assertEqual(self.ctl_ids(self.B, "fail"), [])
        self.assertEqual(self.ctl_ids(self.B, "cancel"), [1])
        self.assertEqual(self.B.native.dropped, [1])
        self.assertFailedNotApplied()
        self.assertIn("this module cannot fire the tool's callback", self.B.log())


if __name__ == "__main__":
    unittest.main()
