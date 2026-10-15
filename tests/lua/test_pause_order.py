"""The pause order with native builds: two mocked games, the fake native module with the deferred inject flow (test_inject_fail_closed.py).

A build made while the session is paused is shipped flagged `pause`; the host (the test relay, as host.rs flush_pause) stamps it one step
past where every game stands still, numbers it and sends it to every game, the originator included. Every game runs on to that stamp
during the pause and applies it there through the native inject path (the originator's exact copy), and the originator's deferred held
command completes with that apply.

Run:  python -I -m unittest discover -s tests/lua -p "test_pause_order.py"
"""
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import test_inject_fail_closed as I  # noqa: E402

STEP = 200


class PausedDeferredBuildTest(I.InjectFailClosedTest):
    # the inject tests of the base class are not run again here
    test_the_happy_path_injects_on_both_games_and_completes_the_originators_held_command = None
    test_a_receiver_that_cannot_inject_applies_nothing_asks_for_a_resync_and_the_originators_tool_is_freed = None
    test_the_originators_own_refused_copy_applies_nothing_there_and_fails_its_held_command = None
    test_a_receiver_whose_placeholder_cannot_be_made_refuses_at_the_decode_and_asks_for_a_resync = None

    def pause(self):
        self.start(speed=2)
        self.strict_on(self.A, self.B)
        self.tick(4)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(18)
        self.assertEqual((self.A.time(), self.B.time()), (self.relay.pauseAt, self.relay.pauseAt))

    def test_a_deferred_build_made_in_a_pause_is_injected_on_both_games_in_the_pause_order_and_completes_the_tool(self):
        self.pause()
        paused_at = self.relay.pauseAt
        for g in (self.A, self.B):
            g.native.native_text = I.road_text()
            g.native.input_text = I.road_text()
        self.B.tool(self.road(300, 0, 400, 0), make="NATIVE_ROAD")
        self.tick(10)
        self.assertEqual((self.A.speed(), self.B.speed()), (0, 0), "built during the pause")
        self.assertEqual(self.A.time(), self.B.time())
        self.assertEqual(self.relay.pause_seq, 1, "numbered by the host")
        self.assertEqual((len(self.A.native.injected), len(self.B.native.injected)), (1, 1), self.log())
        self.assertEqual(self.B.native.completed, [(1, True, "stamp_apply")], "the tool's callback took its pause apply's result")
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
        self.assertEqual(self.A.builds(), ["makeWorldBuildProposalCmd@%d" % (paused_at + STEP)], "at the host's stamp")
        self.assertEqual(self.A.builds(), self.B.builds())
        self.assertSameWorld()
        # the originator's own copy went through the inject path, completing its deferred held command
        orders = [p for _, k, p in self.B.native.ctl_records() if k == "inject_next"]
        self.assertEqual(len(orders), 1)
        self.assertEqual(int(orders[0]["held"]), 1)
        self.assertIn("pause order #1", self.B.log())
        self.assertEqual(self.relay.replay_failed, [])

    def test_two_players_building_in_one_pause_get_the_same_world(self):
        self.pause()
        self.A.native.native_text = I.road_text(500.0, 600.0)
        self.A.native.input_text = I.road_text(500.0, 600.0)
        self.B.native.native_text = I.road_text()
        self.B.native.input_text = I.road_text()
        self.B.tool(self.road(300, 0, 400, 0), make="NATIVE_ROAD")
        self.A.tool(self.road(500, 0, 600, 0), make="NATIVE_ROAD")
        self.tick(12)
        self.assertEqual(self.relay.pause_seq, 2)
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (2, 2), self.log())
        self.assertEqual(self.A.builds(), self.B.builds(), "the same order on both games")
        self.assertEqual(self.A.native.completed, [(1, True, "stamp_apply")])
        self.assertEqual(self.B.native.completed, [(1, True, "stamp_apply")])
        self.assertSameWorld()
        # the mod tells its native module that the game time stands still (what it holds does not age during the pause)
        for g in (self.A, self.B):
            pings = [p for _, k, p in g.native.ctl_records() if k == "ping"]
            self.assertTrue(pings and pings[-1].get("paused") is True, "%s: %s" % (g.label, pings[-3:]))
        # and the games go on after the resume, the next ping saying so at once
        self.relay.pauseAt = None
        self.relay.session()
        self.tick(10)
        self.assertGreater(self.A.time(), self.B.time() - 10 * STEP)
        self.assertSameWorld()
        for g in (self.A, self.B):
            pings = [p for _, k, p in g.native.ctl_records() if k == "ping"]
            self.assertNotIn("paused", pings[-1], "%s: %s" % (g.label, pings[-3:]))


class PausedBuildsOvershootTest(PausedDeferredBuildTest):
    """The real engine's speed 0 takes effect late (real run pause3: 3 steps at x3): here the host's game stops on the pause and the
    client's 3 steps past it. Builds of both players in the pause land at one step on both games, injected, never late."""

    test_a_deferred_build_made_in_a_pause_is_injected_on_both_games_in_the_pause_order_and_completes_the_tool = None

    def test_two_players_building_in_one_pause_get_the_same_world(self):
        self.start(speed=2)
        self.strict_on(self.A, self.B)
        self.A.run("OVERSHOOT = 0")
        self.B.run("OVERSHOOT = 3")
        self.tick(4)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(18)
        paused_at = self.relay.pauseAt
        self.assertEqual((self.A.time(), self.B.time()), (paused_at, paused_at + 3 * STEP), "the client overshot the pause")
        self.A.native.native_text = I.road_text(500.0, 600.0)
        self.A.native.input_text = I.road_text(500.0, 600.0)
        self.B.native.native_text = I.road_text()
        self.B.native.input_text = I.road_text()
        self.B.tool(self.road(300, 0, 400, 0), make="NATIVE_ROAD")
        self.A.tool(self.road(500, 0, 600, 0), make="NATIVE_ROAD")
        self.tick(16)
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (2, 2), self.log())
        self.assertEqual(self.A.builds(), self.B.builds(), "the same steps and order on both games")
        self.assertTrue(all(int(b.split("@")[1]) > paused_at + 3 * STEP for b in self.A.builds()), self.A.builds())
        self.assertEqual(self.A.native.completed, [(1, True, "stamp_apply")])
        self.assertEqual(self.B.native.completed, [(1, True, "stamp_apply")])
        self.assertSameWorld()
        self.assertFalse(any("LATE" in g.log() for g in (self.A, self.B)), self.log())
        self.assertEqual(self.relay.replay_failed, [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
