"""The pause order with native builds: two mocked games, the fake native module with the deferred inject flow (test_inject_fail_closed.py).

A build made while the session is paused is shipped flagged `pause`; the host (the test relay, as host.rs pause_order) numbers it and sends it
to every game, the originator included. Every game applies it during the pause, in that order, through the native inject path (the
originator's exact copy), and the originator's deferred held command completes with that apply.

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
        self.assertEqual((self.A.time(), self.B.time()), (paused_at, paused_at), "built during the pause")
        self.assertEqual(self.relay.pause_seq, 1, "numbered by the host")
        self.assertEqual((len(self.A.native.injected), len(self.B.native.injected)), (1, 1), self.log())
        self.assertEqual(self.B.native.completed, [(1, True, "stamp_apply")], "the tool's callback took its pause apply's result")
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
        self.assertSameWorld()
        # the originator's own copy went through the inject path from the interface state, under its own file numbers
        orders = [p for _, k, p in self.B.native.ctl_records() if k == "inject_next"]
        self.assertEqual(len(orders), 1)
        self.assertGreaterEqual(int(orders[0]["key"]), 1000000000)
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
        # and the games go on after the resume
        self.relay.pauseAt = None
        self.relay.session()
        self.tick(10)
        self.assertGreater(self.A.time(), self.B.time() - 10 * STEP)
        self.assertSameWorld()


if __name__ == "__main__":
    unittest.main(verbosity=2)
