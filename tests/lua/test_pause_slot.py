"""A build without a record made during a pause (owner decision 2026-10-18): applied IN the pause, in the host's pause order, on every game.

A held command whose tool sent no preview (here: a site the strict table does not know, no capture) has no record before the engine applies
it. Released at the click in a pause it was applied at its originator's own stop and shipped stamped at the pause, so a game that had
stopped further on applied it late (PAUSE LATE) and the originator's world had it one step or more before the others. Now only its place
goes to the host's pause order (`pauseSlot`); every game holds there, the originator releases its command once every game stands still at
or past it (itself at or past every other), and its record, stamped where it was applied, is applied there by every other game. Two mocked
games with the fake native module of test_inject_fail_closed.py; the games stop at different steps past the pause (OVERSHOOT).

Run:  python -I -m unittest discover -s tests/lua -p "test_pause_slot.py"
"""
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import test_inject_fail_closed as I  # noqa: E402

STEP = 200
UNKNOWN_SITE = 0x4ABCDE   # no entry in the bridge's SITES table


class PausedBuildWithoutRecordTest(I.InjectFailClosedTest):
    def paused(self, over_host, over_client):
        """A session paused with the host stopping `over_host` steps past the pause and the client `over_client` steps."""
        self.start(speed=2)
        self.strict_on(self.A, self.B)
        self.A.run("OVERSHOOT = %d" % over_host)
        self.B.run("OVERSHOOT = %d" % over_client)
        self.tick(4)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(18)
        p = self.relay.pauseAt
        self.assertEqual((self.A.time(), self.B.time()), (p + over_host * STEP, p + over_client * STEP), "the games stopped at different steps")
        for g in (self.A, self.B):
            g.native.native_text = I.road_text()
            g.native.input_text = I.road_text()
            g.native.site = UNKNOWN_SITE
        return p

    def build_without_record(self, g):
        """A tool build of game `g` that sends no preview: no capture before the engine applies it."""
        g.tool(self.road(300, 0, 400, 0), event=False, make="NATIVE_ROAD")

    def assertAppliedInThePauseOnBoth(self, origin, other, stood):
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1), "applied once on each game, the originator not twice\n" + self.log())
        self.assertEqual(self.A.builds(), self.B.builds(), "at the same step on both games")
        at = int(self.A.builds()[0].split("@")[1])
        self.assertGreaterEqual(at, stood, "where every game stands, none past it")
        self.assertEqual((self.A.speed(), self.B.speed()), (0, 0), "in the pause")
        self.assertSameWorld()
        self.assertFalse(any("LATE" in g.log() for g in (self.A, self.B)), self.log())
        self.assertEqual(self.relay.replay_failed, [])
        self.assertEqual(self.relay.pause_seq, 1, "its place was numbered by the host")
        self.assertIn("held 1 (site 0x4abcde): no capture of a tool build; made during the pause: HELD", origin.log())
        self.assertIn("released at its place", origin.log())
        self.assertIn("at its place %d:1 in the pause order: stamped %d" % (origin.id, at), origin.log())
        self.assertIn("this game holds at t=", other.log())
        self.assertIn("closed: its record", other.log())
        return at

    def test_the_hosts_build_without_a_record_is_applied_in_the_pause_where_the_client_stands_further_on(self):
        p = self.paused(0, 3)
        self.build_without_record(self.A)
        self.tick(3)
        self.assertEqual(self.A.builds_applied(), 0, "not applied at the click: held until its place in the pause order")
        self.tick(20)
        at = self.assertAppliedInThePauseOnBoth(self.A, self.B, p + 3 * STEP)
        self.assertGreater(at, p + 3 * STEP, "past the client's stop: the host ran on to where the client stood")

    def test_the_clients_build_without_a_record_is_applied_in_the_pause_where_the_host_stands_further_on(self):
        p = self.paused(3, 0)
        self.build_without_record(self.B)
        self.tick(23)
        self.assertAppliedInThePauseOnBoth(self.B, self.A, p + 3 * STEP)

    def test_a_later_build_of_the_pause_waits_for_the_slot_and_follows_it_on_both_games(self):
        p = self.paused(0, 3)
        # the host holds both until it flushes them together (one flush: the same stamp, numbered in arrival order)
        flush = self.relay.flush_pause
        self.relay.flush_pause = lambda: None
        self.build_without_record(self.A)
        for _ in range(40):
            if "made during the pause: HELD" in self.A.log():
                break
            self.frames(1)
        # the client's ordinary road (its tool sends a preview: the deferred inject flow) comes after it in the pause order
        self.B.native.native_text = I.road_text(500.0, 600.0)
        self.B.native.input_text = I.road_text(500.0, 600.0)
        self.B.native.site = I.S.TOOL_SITE
        self.B.tool(self.road(500, 0, 600, 0), make="NATIVE_ROAD")
        for _ in range(40):
            if len(self.relay.pause_held) == 2:
                break
            self.frames(1)
        self.assertEqual([g.id for _, g, _ in self.relay.pause_held], [0, 1], "the host's slot first, then the client's road")
        self.relay.flush_pause = flush
        self.relay.flush_pause()
        self.tick(30)
        self.assertEqual(self.relay.pause_seq, 2)
        for g in (self.A, self.B):
            self.assertIn("pause order #2 (action) waits for the record of 0:1 (pause order #1)", g.log(), g.label)
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (2, 2), self.log())
        self.assertEqual(self.A.builds(), self.B.builds(), "the same steps and order on both games")
        first, second = (int(b.split("@")[1]) for b in self.A.builds())
        self.assertLessEqual(first, second, "the build of the slot first, the later number after it")
        self.assertGreater(first, p + 3 * STEP)
        self.assertEqual(self.B.native.completed, [(1, True, "stamp_apply")], "the client's deferred command completed with its pause apply")
        self.assertSameWorld()
        self.assertFalse(any("LATE" in g.log() for g in (self.A, self.B)), self.log())
        self.assertEqual(self.relay.replay_failed, [])

    def test_the_resume_waits_for_the_record_of_an_open_slot(self):
        p = self.paused(0, 3)
        self.build_without_record(self.A)
        for _ in range(40):
            if "made during the pause: HELD" in self.A.log():
                break
            self.frames(1)
        self.assertIn("made during the pause: HELD", self.A.log())
        self.relay.pauseAt = None   # the host resumes while the place is on its way and the games hold at it
        self.relay.session()
        self.tick(30)
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1), self.log())
        self.assertEqual(self.A.builds(), self.B.builds())
        self.assertGreater(int(self.A.builds()[0].split("@")[1]), p)
        self.assertSameWorld()
        self.assertFalse(any("LATE" in g.log() for g in (self.A, self.B)), self.log())
        self.assertGreater(self.A.time(), p + 10 * STEP, "and the games run on afterwards")

    def test_outside_a_pause_a_build_without_a_record_is_still_released_at_the_click(self):
        self.start()
        self.strict_on(self.A, self.B)
        for g in (self.A, self.B):
            g.native.native_text = I.road_text()
            g.native.input_text = I.road_text()
            g.native.site = UNKNOWN_SITE
        self.build_without_record(self.B)
        self.tick(30)
        self.assertIn("no capture of a tool build; release written", self.B.log())
        self.assertNotIn("pauseSlot", self.B.log())
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))


# only the tests of this file: the inherited inject tests run in test_inject_fail_closed.py
for _name in dir(I.InjectFailClosedTest):
    if _name.startswith("test_") and _name not in vars(PausedBuildWithoutRecordTest):
        setattr(PausedBuildWithoutRecordTest, _name, None)


if __name__ == "__main__":
    unittest.main(verbosity=2)
