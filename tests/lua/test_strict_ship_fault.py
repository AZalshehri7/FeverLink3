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

    def open(self):
        super().open()
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

    def assertFailedOnB(self, failed=True):
        """B made the build: never released at the click, nothing applied there, its tool freed, shipped once."""
        self.assertEqual(self.ctl_ids(self.B, "release"), [], "no release of the shipped command was ever written")
        self.assertEqual(self.B.native.released, [])
        if failed:
            self.assertEqual(self.B.native.failed, [1], "failed: callback fired, nothing applied")
        self.assertEqual(self.B.builds_applied(), 0, "nothing applied on the originator")
        self.assertEqual(self.B.L.eval("NATIVE_HELD_COUNT()"), 0, "the command does not stay held (the tool is freed)")
        self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 1), "shipped once")
        self.assertNotIn("native build captured", self.B.log())

    def assertRepairAsked(self):
        self.assertTrue(any(gid == 1 and "was shipped but failed on its originator" in d for gid, d in self.relay.replay_failed),
                        self.relay.replay_failed)

    def assertFailedAfterTheShip(self):
        """The other game applied it at the stamp, a repair is asked for."""
        self.assertFailedOnB(failed=False)
        self.assertEqual(self.A.builds(), ["makeWorldBuildProposalCmd@%d" % self.B.action_records()[0]["stamp"]], "the other game applied it at the stamp")
        self.assertRepairAsked()
        self.assertIn("FAILED after the ship", self.B.log())
        self.assertIn("NOT released, nothing is applied on this game", self.B.log())

    def test_a_fault_after_the_ship_never_releases_the_command_at_the_click(self):
        def plan_fault():
            # a fault in what used to run after the ship (the ship line's plan): now made before it, harmless
            self.start()
            self.B.run("FAIL_PLANOF = true")
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(1)
            stamp = self.B.action_records()[0]["stamp"]
            self.assertNotIn("fault in the verdict on held 1", self.B.log())
            self.assertEqual((self.ctl_ids(self.B, "release"), self.B.native.released), ([], []), "never released at the click")
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

        def route_fault():
            # a fault after the ship in a step that cannot be made before it: failed, never released
            self.start()
            self.B.run("FAIL_NEXT_QUEUE = true")
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(1)
            self.assertIn("fault in the verdict on held 1", self.B.log())
            self.assertEqual(self.ctl_ids(self.B, "fail"), [1], "fail written")
            self.tick(16)
            self.assertEqual(self.B.native.failed, [1], "the module failed the command: its callback fired, nothing applied")
            self.assertFailedAfterTheShip()
            self.assertIn("at step route of the held flow", self.B.log())
            self.assertIn("FAILED without a stamp apply (fail)", self.B.log())
            # and the next build is strict again, held and released at its stamp
            self.B.tool(self.road(500, 0, 600, 0))
            self.tick(16)
            self.assertEqual((self.B.native.released, self.B.builds_applied()), ([2], 1))

        def plan_fault_cancel_flow():
            # cancel-and-replay with a fireable callback: the cancel is written before the route
            self.start()
            self.B.run("FAIL_PLANOF = true")
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(16)
            stamp = self.B.action_records()[0]["stamp"]
            self.assertNotIn("fault in the verdict on held 1", self.B.log())
            self.assertEqual((self.ctl_ids(self.B, "release"), self.B.native.released), ([], []), "never released at the click")
            self.assertEqual(self.B.native.dropped, [1], "cancelled (callback fired), the record replayed")
            self.assertEqual(self.A.builds(), ["makeWorldBuildProposalCmd@%d" % stamp])
            self.assertEqual(self.B.builds(), ["makeWorldBuildProposalCmd@%d" % stamp], "applied at the stamp on the originator too")
            self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 1))
            self.assertSameWorld()

        def route_fault_cancel_flow():
            self.start()
            self.B.run("FAIL_NEXT_QUEUE = true")
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(16)
            self.assertIn("fault in the verdict on held 1", self.B.log())
            self.assertEqual(self.ctl_ids(self.B, "cancel"), [1])
            self.assertEqual(self.ctl_ids(self.B, "fail"), [], "cancelled already: no second order")
            self.assertEqual(self.B.native.dropped, [1])
            self.assertFailedAfterTheShip()
            self.assertIn("at step route of the cancel flow", self.B.log())
            self.assertIn("cancelled already", self.B.log())

        def route_fault_old_module():
            # a module without `fail` and without a fireable callback: the command is cancelled (the tool may stay busy), never released
            self.start()
            self.B.run("FAIL_NEXT_QUEUE = true")
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(16)
            self.assertEqual((self.ctl_ids(self.B, "fail"), self.ctl_ids(self.B, "cancel"), self.B.native.dropped), ([], [1], [1]))
            self.assertFailedAfterTheShip()
            self.assertIn("this module cannot fire the tool's callback", self.B.log())

        cancel_flow = {"strict_cancel": "1", "extra_env": {"FEVERLINK3_STRICT_FIRE_DONE": None}, "fires": True}
        self.cases([("a fault in the ship line's plan", {}, plan_fault), ("a fault while routing", {}, route_fault),
                    ("the cancel flow: a fault in the plan", cancel_flow, plan_fault_cancel_flow),
                    ("the cancel flow: a fault while routing", cancel_flow, route_fault_cancel_flow),
                    ("an old module: a fault while routing", {"fail_capable": False}, route_fault_old_module)])

    def test_a_shipped_command_the_watchdog_veto_or_commit_gives_up_is_failed_never_released(self):
        def stamp_passed():
            self.start()
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(1)
            stamp = self.B.action_records()[0]["stamp"]
            self.hide_release_results(self.B)
            self.tick(24)
            self.assertIn("WATCHDOG: the stamp", self.B.log())
            self.assertIn("FAILED, NOT released: fail written", self.B.log())
            self.assertFailedOnB()
            self.assertEqual(self.A.builds(), ["makeWorldBuildProposalCmd@%d" % stamp], "the other game applied it at the stamp")
            self.assertRepairAsked()

        def held_too_long_in_a_pause():
            # natVeto released such a command (the tool would stay dead after a cancel): the build then existed on this game only
            self.start(speed=2)
            self.tick(6)
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(1)
            self.frames(500)   # the game is stopped (frames go on, the clock does not) for longer than the limit
            self.assertIn("FAILED on the veto, NOT released", self.B.log())
            self.assertFailedOnB()
            self.assertEqual(self.A.builds_applied(), 0, "the other game dropped it on the veto")
            self.assertTrue(any("game stayed paused too long" in c[1] for c in self.relay.chats))

        def vetoed():
            # was a release: the build existed on this game only (the vetoing game dropped it) and a resync followed
            self.start()
            self.A.run("WORLD[110] = nil")   # the host cannot build this split
            self.B.tool(S.SPLIT, make="NATIVE_SPLIT")
            self.tick(20)
            self.assertEqual(len(self.relay.vetoes), 1)
            self.assertIn("FAILED on the veto, NOT released", self.B.log())
            self.assertIn("fail written", self.B.log())
            self.assertFailedOnB()
            self.assertEqual(self.A.builds_applied(), 0, "no game has the build")
            self.assertEqual(self.relay.replay_failed, [], "so no repair")
            self.assertTrue(any("build was cancelled because it could not be built" in c[1] for c in self.relay.chats), self.relay.chats)

        def fault_after_the_commit():
            self.pause()
            self.B.tool(self.road(300, 0, 400, 0))
            self.tick(1)
            self.assertEqual(self.relay.commits, [])
            self.relay.pauseAt = None   # unpaused during the grace: the commit gives a new stamp and routes it to the queue
            self.relay.session()
            self.B.run("FAIL_NEXT_QUEUE = true")
            self.tick(30)
            self.assertEqual(len(self.relay.commits), 1, "the commit was sent")
            self.assertIn("FAILED at its commit (after the commit was sent", self.B.log())
            self.assertFailedOnB()
            self.assertEqual(self.A.builds(), ["makeWorldBuildProposalCmd@%d" % self.relay.commits[0][3]], "the other game applied it at the commit's stamp")
            self.assertRepairAsked()
            self.assertNotIn("WATCHDOG", self.B.log())

        self.cases([("the stamp passed without a release", {}, stamp_passed), ("held too long in a paused game", {}, held_too_long_in_a_pause),
                    ("vetoed", {}, vetoed), ("a fault after the commit of a paused build", {}, fault_after_the_commit)])


if __name__ == "__main__":
    unittest.main()
