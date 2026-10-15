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


class PausedCallbackWaitsForTheInterfaceWorldTest(PausedDeferredBuildTest):
    """b3pause2 (2026-10-15): the host's own road made in a pause crashed the host. The host stood 6 steps past the pause (speed 0 takes
    effect late), the mod took the pause stamp, a placeholder until the host stamps the build, for passed and wrote `release` at once;
    the module completed the street tool's command at the simulation's apply of the host's stamp, before the interface's world had the
    road, and the tool's callback stopped the game on an edge it did not know. The release must wait for the host's stamp and for the
    interface's world to have it, while paused too."""

    test_a_deferred_build_made_in_a_pause_is_injected_on_both_games_in_the_pause_order_and_completes_the_tool = None
    test_two_players_building_in_one_pause_get_the_same_world = None

    def setUp(self):
        super().setUp()
        self.A = ReleaseGame("host", 0, self.tmp + "x", 50000, self.clock)
        self.B = ReleaseGame("client", 1, self.tmp + "x", 51000, self.clock)
        self.clock.games[:] = [self.A, self.B]
        self.relay = I.T.Relay([self.A, self.B])

    def test_the_originators_paused_build_completes_only_once_the_interface_world_is_past_the_hosts_stamp(self):
        self.start(speed=2)
        self.strict_on(self.A, self.B)
        self.A.run("OVERSHOOT = 3")      # the host stands 3 steps past the pause, as in the real run
        for g in (self.A, self.B):
            g.run("GUI_LAG = true")       # and every interface world is a frame behind its simulation
        self.tick(4)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(18)
        paused_at = self.relay.pauseAt
        self.assertGreater(self.A.time(), paused_at, "the host overshot the pause")
        for g in (self.A, self.B):
            g.native.native_text = I.road_text()
            g.native.input_text = I.road_text()
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
        self.assertSameWorld()
        self.assertEqual(self.relay.replay_failed, [])


# only its own test: the inherited ones run with their own module elsewhere
for _name in dir(PausedCallbackWaitsForTheInterfaceWorldTest):
    if _name.startswith("test_") and _name != "test_the_originators_paused_build_completes_only_once_the_interface_world_is_past_the_hosts_stamp":
        setattr(PausedCallbackWaitsForTheInterfaceWorldTest, _name, None)
# likewise for the pause tests' base: the inject tests run in test_inject_fail_closed.py
for _cls in (PausedDeferredBuildTest, PausedBuildsOvershootTest):
    for _name in dir(I.InjectFailClosedTest):
        if _name.startswith("test_") and _name not in vars(_cls):
            setattr(_cls, _name, None)


if __name__ == "__main__":
    unittest.main(verbosity=2)
