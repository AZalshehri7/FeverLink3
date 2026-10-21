"""Native builds made while the session is paused: two mocked games, the fake native module with the deferred inject flow
(test_inject_fail_closed.py); the games may stop at different steps past the pause (OVERSHOOT: the real engine's speed 0 takes effect late).

A build made in a pause is shipped flagged `pause`; the host (the test relay, as host.rs flush_pause) stamps it one step past where every
game stands still, numbers it and sends it to every game, the originator included. Every game runs on to that stamp during the pause and
applies it there through the native inject path (the originator's exact copy), and the originator's deferred held command completes with
that apply.

A build WITHOUT a record (owner decision 2026-10-18: its tool sent no preview, here a site the strict table does not know) has no record
before the engine applies it. Released at the click in a pause it was applied at its originator's own stop and shipped stamped at the pause,
so a game that had stopped further on applied it late (PAUSE LATE). Now only its place goes to the host's pause order (`pauseSlot`); every
game holds there, the originator releases its command once every game stands still at or past it, and its record, stamped where it was
applied, is applied there by every other game.

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
UNKNOWN_SITE = 0x4ABCDE   # no entry in the bridge's SITES table
GUI_TIME = "GUI_ENV.api.engine.getComponent(1, GUI_ENV.api.type.ComponentType.GAME_TIME).gameTime"

# The interface's world lags the simulation (the engine copies the simulation's changes to it a little later): with GUI_LAG set, the
# interface half sees the game time of the frame before (one frame behind after every step; a game standing still catches up).
# Installed on the interface half's api after every load.
GUI_LAG_LUA = r'''
local api = GUI_ENV.api
local base = api.engine.getComponent
api.engine.getComponent = function(e, t)
  local c = base(e, t)
  if t == api.type.ComponentType.GAME_TIME and GUI_LAG and GUI_SEEN then return { gameTime = GUI_SEEN } end
  return c
end
if not FRAME_LAGGED then
  FRAME_LAGGED = true
  local baseFrame = FRAME
  FRAME = function()
    GUI_SEEN, PREV_T = PREV_T or GAME.time, GAME.time
    return baseFrame()
  end
end
'''


class ReleaseNative(I.InjectNative):
    """The module's real rule for a deferred held command (docs/NATIVE_LINK.md, commit 712c55b): its completion with this game's stamp
    apply waits for the mod's `release` of the id, which says the INTERFACE's world has that apply; the tool's callback fires then and
    reads the new entities there. The fake notes the interface's game time at the release."""

    def __init__(self, game, clock):
        self.waiting, self.release_gui_time = {}, {}
        super().__init__(game, clock)

    def handle(self, seq, kind, p):
        hid = int(p.get("id", -1)) if kind == "release" else None
        if hid is not None and (hid in self.deferred or hid in self.waiting):
            self.last_valid = self.clock.t
            self.heard.append((self.clock.t, seq, kind, p))
            self.release_gui_time[hid] = int(self.game.L.eval(GUI_TIME))
            if self.waiting.pop(hid, None):
                I.InjectNative.finish_held(self, hid, True, "stamp_apply")
            return
        super().handle(seq, kind, p)

    def finish_held(self, hid, ok, reason):
        if ok and reason == "stamp_apply" and hid not in self.release_gui_time:
            if hid in self.deferred:
                self.waiting[hid] = True   # the simulation applied it: the completion waits for the release
                return True
            return False
        return I.InjectNative.finish_held(self, hid, ok, reason)


class ReleaseGame(I.InjectGame):
    def __init__(self, label, player_id, tmp, first_entity, clock):
        I.S.N.NativeGame.__init__(self, label, player_id, tmp, first_entity)
        clock.games.append(self)
        self.native = ReleaseNative(self, clock)
        g = self.L.globals()
        g.FAKE_T = clock.t
        g.NATIVE_HOOK = self.native.on_add
        g.INJECT_HOOK = self.native.inject_hook
        self.L.execute(I.S.STRICT_LUA)
        self.L.execute(I.INJECT_LUA)
        self.reload()

    def reload(self):
        super().reload()
        self.L.execute(GUI_LAG_LUA)


class PausedBuildTest(I.InjectTest):
    def paused(self, over_host=0, over_client=0, gui_lag=False):
        """A session paused with the host stopping `over_host` steps past the pause and the client `over_client` steps."""
        self.start(speed=2)
        self.strict_on(self.A, self.B)
        self.A.run("OVERSHOOT = %d" % over_host)
        self.B.run("OVERSHOOT = %d" % over_client)
        if gui_lag:
            for g in (self.A, self.B):
                g.run("GUI_LAG = true")   # every interface world is a frame behind its simulation
        self.tick(4)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(18)
        p = self.relay.pauseAt
        self.assertEqual((self.A.time(), self.B.time()), (p + over_host * STEP, p + over_client * STEP), "the games stopped where they stop")
        return p

    def assertSameAndNotLate(self):
        self.assertSameWorld()
        self.assertFalse(any("LATE" in g.log() for g in (self.A, self.B)), self.log())
        self.assertEqual(self.relay.replay_failed, [])

    def test_builds_made_in_a_pause_are_injected_on_every_game_at_the_hosts_stamp_and_complete_the_tool(self):
        def one_build():
            paused_at = self.paused()
            self.texts(I.road_text())
            self.B.tool(self.road(300, 0, 400, 0), make="NATIVE_ROAD")
            self.tick(10)
            self.assertEqual((self.A.speed(), self.B.speed()), (0, 0), "built during the pause")
            self.assertEqual(self.A.time(), self.B.time())
            self.assertEqual(self.relay.pause_seq, 1, "numbered by the host")
            self.assertEqual((len(self.A.native.injected), len(self.B.native.injected)), (1, 1), self.log())
            self.assertEqual(self.B.native.completed, [(1, True, "stamp_apply")], "the tool's callback took its pause apply's result")
            self.assertEqual(self.A.builds(), ["makeWorldBuildProposalCmd@%d" % (paused_at + STEP)], "at the host's stamp")
            self.assertEqual(self.A.builds(), self.B.builds())
            # the originator's own copy went through the inject path, completing its deferred held command
            orders = [p for _, k, p in self.B.native.ctl_records() if k == "inject_next"]
            self.assertEqual([int(o["held"]) for o in orders], [1])
            self.assertIn("pause order #1", self.B.log())
            self.assertSameAndNotLate()

        def two_players(over_client):
            # real run pause3: 3 steps at x3 past the pause on one game. Builds of both players in the pause land at one step on both games,
            # injected, never late; the mod tells its native module that the game time stands still, and that it runs again after the resume
            paused_at = self.paused(0, over_client)
            self.texts(I.road_text(500.0, 600.0), games=(self.A,))
            self.texts(I.road_text(), games=(self.B,))
            self.B.tool(self.road(300, 0, 400, 0), make="NATIVE_ROAD")
            self.A.tool(self.road(500, 0, 600, 0), make="NATIVE_ROAD")
            self.tick(16)
            self.assertEqual(self.relay.pause_seq, 2)
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (2, 2), self.log())
            self.assertEqual(self.A.builds(), self.B.builds(), "the same steps and order on both games")
            self.assertTrue(all(int(b.split("@")[1]) > paused_at + over_client * STEP for b in self.A.builds()), self.A.builds())
            self.assertEqual((self.A.native.completed, self.B.native.completed), ([(1, True, "stamp_apply")], [(1, True, "stamp_apply")]))
            self.assertSameAndNotLate()
            for g in (self.A, self.B):
                pings = [p for _, k, p in g.native.ctl_records() if k == "ping"]
                self.assertTrue(pings and pings[-1].get("paused") is True, "%s: %s" % (g.label, pings[-3:]))
            self.relay.pauseAt = None
            self.relay.session()
            self.tick(10)
            self.assertGreater(self.A.time(), self.B.time() - 10 * STEP)
            self.assertSameWorld()
            for g in (self.A, self.B):
                pings = [p for _, k, p in g.native.ctl_records() if k == "ping"]
                self.assertNotIn("paused", pings[-1], "%s: %s" % (g.label, pings[-3:]))

        def callback_waits_for_the_interface_world():
            # b3pause2 (2026-10-15): the host's own road made in a pause crashed the host. The host stood 6 steps past the pause (speed 0
            # takes effect late), the mod took the pause stamp, a placeholder until the host stamps the build, for passed and wrote `release`
            # at once; the module completed the street tool's command at the simulation's apply of the host's stamp, before the interface's
            # world had the road, and the tool's callback stopped the game on an edge it did not know. The release must wait for the host's
            # stamp and for the interface's world to have it, while paused too.
            self.paused(3, 0, gui_lag=True)
            self.texts(I.road_text())
            stood = self.A.time()
            self.A.tool(self.road(300, 0, 400, 0), make="NATIVE_ROAD")
            self.tick(16)
            stamp = int(self.A.builds()[0].split("@")[1])
            self.assertEqual(self.A.builds(), self.B.builds())
            self.assertGreater(stamp, stood, "stamped by the host past where the games stood")
            self.assertIn(1, self.A.native.release_gui_time, "released: " + self.log())
            self.assertGreater(self.A.native.release_gui_time[1], stamp, "released only once the interface's world is past the stamp")
            self.assertEqual(self.A.native.completed, [(1, True, "stamp_apply")], "the tool's callback took the stamp apply")
            self.assertEqual((self.A.speed(), self.B.speed()), (0, 0), "while paused")
            self.assertSameAndNotLate()

        self.cases([("one build", {}, one_build), ("two players", {}, lambda: two_players(0)),
                    ("two players, the client 3 steps past the pause", {}, lambda: two_players(3)),
                    ("the callback waits for the interface's world", {"game": ReleaseGame}, callback_waits_for_the_interface_world)])

    def test_a_build_without_a_record_made_in_a_pause_is_applied_in_the_pause_at_its_place_in_the_hosts_order(self):
        def build_without_record(g):
            """A tool build of game `g` that sends no preview: no capture before the engine applies it."""
            g.tool(self.road(300, 0, 400, 0), event=False, make="NATIVE_ROAD")

        def paused(over_host, over_client):
            p = self.paused(over_host, over_client)
            self.texts(I.road_text())
            for g in (self.A, self.B):
                g.native.site = UNKNOWN_SITE
            return p

        def wait_for_the_hold(g):
            for _ in range(40):
                if "made during the pause: HELD" in g.log():
                    break
                self.frames(1)
            self.assertIn("made during the pause: HELD", g.log())

        def assertAppliedInThePauseOnBoth(origin, other, stood):
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1), "applied once on each game, the originator not twice\n" + self.log())
            self.assertEqual(self.A.builds(), self.B.builds(), "at the same step on both games")
            at = int(self.A.builds()[0].split("@")[1])
            self.assertGreaterEqual(at, stood, "where every game stands, none past it")
            self.assertEqual((self.A.speed(), self.B.speed()), (0, 0), "in the pause")
            self.assertSameAndNotLate()
            self.assertEqual(self.relay.pause_seq, 1, "its place was numbered by the host")
            self.assertIn("held 1 (site 0x4abcde): no capture of a tool build; made during the pause: HELD", origin.log())
            self.assertIn("released at its place", origin.log())
            self.assertIn("at its place %d:1 in the pause order: stamped %d" % (origin.id, at), origin.log())
            self.assertIn("this game holds at t=", other.log())
            self.assertIn("closed: its record", other.log())
            return at

        def hosts_build():
            p = paused(0, 3)
            build_without_record(self.A)
            self.tick(3)
            self.assertEqual(self.A.builds_applied(), 0, "not applied at the click: held until its place in the pause order")
            self.tick(20)
            at = assertAppliedInThePauseOnBoth(self.A, self.B, p + 3 * STEP)
            self.assertGreater(at, p + 3 * STEP, "past the client's stop: the host ran on to where the client stood")

        def clients_build():
            p = paused(3, 0)
            build_without_record(self.B)
            self.tick(23)
            assertAppliedInThePauseOnBoth(self.B, self.A, p + 3 * STEP)

        def a_later_build_waits_for_the_slot():
            p = paused(0, 3)
            # the host holds both until it flushes them together (one flush: the same stamp, numbered in arrival order)
            flush = self.relay.flush_pause
            self.relay.flush_pause = lambda: None
            build_without_record(self.A)
            wait_for_the_hold(self.A)
            # the client's ordinary road (its tool sends a preview: the deferred inject flow) comes after it in the pause order
            self.texts(I.road_text(500.0, 600.0), games=(self.B,))
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
            self.assertSameAndNotLate()

        def the_resume_waits():
            p = paused(0, 3)
            build_without_record(self.A)
            wait_for_the_hold(self.A)
            self.relay.pauseAt = None   # the host resumes while the place is on its way and the games hold at it
            self.relay.session()
            self.tick(30)
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1), self.log())
            self.assertEqual(self.A.builds(), self.B.builds())
            self.assertGreater(int(self.A.builds()[0].split("@")[1]), p)
            self.assertSameAndNotLate()
            self.assertGreater(self.A.time(), p + 10 * STEP, "and the games run on afterwards")

        def outside_a_pause():
            self.start()
            self.strict_on(self.A, self.B)
            self.texts(I.road_text())
            for g in (self.A, self.B):
                g.native.site = UNKNOWN_SITE
            build_without_record(self.B)
            self.tick(30)
            # outside a pause no game stands where the originator's engine would apply it: refused (pre7 id forks), never released
            self.assertIn("REFUSED: no record before the engine applies it (no capture of a tool build)", self.B.log())
            self.assertNotIn("pauseSlot", self.B.log())
            self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (0, 0))

        self.cases([("the host's build, the client further on", {}, hosts_build), ("the client's build, the host further on", {}, clients_build),
                    ("a later build of the pause waits for the slot", {}, a_later_build_waits_for_the_slot),
                    ("the resume waits for the record of an open slot", {}, the_resume_waits),
                    ("outside a pause: released at the click", {}, outside_a_pause)])


if __name__ == "__main__":
    unittest.main(verbosity=2)
