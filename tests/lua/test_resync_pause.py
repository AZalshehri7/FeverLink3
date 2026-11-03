"""The pause after a resync load: every game loaded the host's save, which was taken where the host's game stood still, a few steps
past the round's pause. The session lines a reloaded game reads after its load still carry the pause of the world it left; a stamp
there lies before the time every game stands at, and the other game applies it late (friend test 2026-10-16, bug 9: pauseAt
10341800, both games loaded at 10342400, a native build stamped 10341800 was applied 3 steps late, a fork and another resync).

Run:  python -I -m unittest discover -s tests/lua -p "test_resync_pause.py"
"""
import os
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import test_lockstep as T  # noqa: E402
import test_native_capture as N  # noqa: E402

STEP = T.STEP
BUILD_AT = "(function() for _, a in ipairs(GAME.applied) do if a.name == 'makeWorldBuildProposalCmd' then return a.time end end end)()"


class ResyncPauseTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-resyncpause-")
        self.A = N.NativeGame("host", 0, self.tmp, 50000)
        self.B = N.NativeGame("client", 1, self.tmp, 51000)
        self.relay = T.Relay([self.A, self.B])

    def tearDown(self):
        if os.environ.get("FEVERLINK3_TEST_VERBOSE"):
            print("\n--- host mod.log ---\n" + self.A.log() + "\n--- client mod.log ---\n" + self.B.log())
        shutil.rmtree(self.tmp, ignore_errors=True)

    def tick(self, n=1):
        for _ in range(n):
            for _ in range(4):
                for g in (self.A, self.B):
                    g.frame()
                self.relay.pump()
            self.A.step()
            self.B.step()

    def paused_then_loaded(self, overshoot, at_load=None):
        """A started session paused at P, then both games reloaded with the host's save taken `overshoot` steps past P. The relay keeps
        saying pauseAt P after the load (the lines held for the load, and a host core that did not move the pause). `at_load(g)`: what
        runs in a reloaded game before its first frame. Returns (P, loaded)."""
        self.tick(3)
        self.relay.started, self.relay.speed = True, 2
        self.relay.session()
        self.tick(9)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(30)
        p = self.relay.pauseAt
        self.assertEqual((self.A.time(), self.B.time()), (p, p), "both stopped at the pause")
        loaded = p + overshoot * STEP
        for g in (self.A, self.B):
            self.relay.send(g, "resync_load", 0, self.relay.ser({"id": 12, "name": "hostsave", "sha256": "00"}))
            g.frames_to_load()
            self.assertTrue(g.reload_pending())
            g.run("GAME.time = %d" % loaded)   # the save's time: where the host's game stood still
            g.reload()
            if at_load:
                at_load(g)
        self.relay.stops = {}
        self.relay.session()   # still pauseAt P
        self.tick(6)
        self.assertEqual((self.A.time(), self.B.time()), (loaded, loaded), "both reloaded games stand at the save's time")
        self.assertEqual((self.A.speed(), self.B.speed()), (0, 0), "and stay paused")
        return p, loaded

    def test_after_a_resync_load_in_a_pause_every_stamp_lies_at_or_past_the_loaded_time(self):
        def native_build_after_the_load(p, loaded):
            self.A.native(N.NativeCaptureTest.road(None, 300, 0, 400, 0))
            self.tick(16)
            self.assertNotIn("LATE", self.A.log() + self.B.log())
            self.assertNotIn("BREACHED", self.A.log() + self.B.log())
            self.assertIn("stamped %d" % loaded, self.A.log(), "the stamp is where every game stands, not the stale pause %d" % p)
            self.assertEqual(self.A.sig(), self.B.sig(), "both games hold the same world")
            self.assertEqual(self.A.lua(BUILD_AT), self.B.lua(BUILD_AT), "applied in the same step on both games")
            self.assertEqual(int(self.B.lua(BUILD_AT)), loaded)
            for g in (self.A, self.B):
                self.assertIn("is before this world's load at %d" % loaded, g.log())

        def action_of_the_pause(p, loaded):
            self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function() end)')
            self.tick(10)
            expected = ["makeVehicleBuyCmd@%d" % (loaded + STEP)]
            for g in (self.A, self.B):
                self.assertEqual([a for a in g.applied() if a.startswith("makeVehicleBuyCmd")], expected, g.label)
            self.assertNotIn("LATE", self.A.log() + self.B.log())

        def resume(p, loaded):
            self.relay.pauseAt, self.relay.speed = None, 2
            self.relay.session()
            self.tick(20)
            self.assertGreater(min(self.A.time(), self.B.time()), loaded, "the games run on from the loaded time")
            self.A.native(N.NativeCaptureTest.road(None, 300, 0, 400, 0))
            self.tick(20)
            self.assertEqual(self.A.sig(), self.B.sig())
            self.assertNotIn("LATE", self.A.log() + self.B.log())

        def a_later_pause(p, loaded):
            self.relay.pauseAt = loaded + 4 * STEP
            self.relay.session()
            self.tick(3)
            for g in (self.A, self.B):
                self.assertIn("pauseAt=%d" % (loaded + 4 * STEP), g.log(), "a pause at or after the load is kept as the host sends it")

        for i, case in enumerate((native_build_after_the_load, action_of_the_pause, resume, a_later_pause)):
            with self.subTest(case.__name__):
                if i:
                    self.tearDown()
                    self.setUp()
                case(*self.paused_then_loaded(3))


    def test_another_mods_send_before_the_first_frame_after_the_load_waits_for_the_session(self):
        # seen in play (0.2.14-beta.2 lab, 0.2.14-beta.1 two-PC session): after the resync load Pay Your Tolls booked the vehicles at
        # its tolls again on each game before the bridge's first frame; each copy went to its engine alone and the paused checksum
        # after the reload differed in balance. The game's interface sends its events (idAdded) before the first frame.
        def at_load(g):
            g.run("GUI.guiHandleEvent(nil, STATE, GSTATE, '', 'menu.construction.roads.roads_tools.tab', 'idAdded', {})")
            g.run("assert(load(%s, %s, 't', setmetatable({ api = GUI_API }, { __index = _G })))()"
                  % (T.lua_str('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "pyt", "pyt.addCounts", { counts = { { 1, 2, 0, 40 } } }))'),
                     T.lua_str("epod_pay_your_tolls_tf3_1::/pyt/pyt_toll.script.tl")))
        p, loaded = self.paused_then_loaded(3, at_load)
        self.tick(10)
        self.assertEqual((self.A.events_seen("pyt"), self.B.events_seen("pyt")), (0, 0), "no game applied its own copy in the pause")
        self.assertIn("is made before this game's session began (a reload): it waits for the session", self.A.log())
        self.assertIn("is dropped here: the host's copy of the mod sends it for every game", self.B.log())
        self.relay.pauseAt, self.relay.speed = None, 2
        self.relay.session()
        self.tick(20)
        self.assertEqual((self.A.events_seen("pyt"), self.B.events_seen("pyt")), (1, 1), self.A.log())
        self.assertEqual(self.A.sig(), self.B.sig())


class LoadedAtTest(unittest.TestCase):
    """The pure rule (L.loadedAt, L.applySession)."""

    def setUp(self):
        import lupa
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.M = self.L.execute(T.LOCKSTEP_SRC)

    def session(self, **p):
        return self.L.table_from(p)

    def test_a_pause_before_the_load_becomes_the_load_time_and_a_later_one_is_kept(self):
        s = self.M.newSession()
        self.M.applySession(s, self.session(started=True, speed=0, pauseAt=200, lead=1000))
        self.assertEqual(s.pauseAt, 200, "a session never loaded by a resync takes the pause as sent")
        s = self.M.newSession()
        self.M.applySession(s, self.session(started=True, speed=0, pauseAt=10341800, lead=1000))
        self.assertEqual(self.M.loadedAt(s, 10342400), 10341800, "the pause it replaced")
        self.assertEqual(s.pauseAt, 10342400)
        self.M.applySession(s, self.session(started=True, speed=4, pauseAt=10341800, lead=1000))
        self.assertEqual(s.pauseAt, 10342400, "a stale line read after the load")
        self.assertEqual(self.M.stampFor(s, 10342400), 10342400, "a stamp at the pause is where every game stands")
        self.M.applySession(s, self.session(started=True, speed=4, pauseAt=10343000, lead=1000))
        self.assertEqual(s.pauseAt, 10343000)
        self.M.applySession(s, self.session(started=True, speed=4, lead=1000))
        self.assertIsNone(s.pauseAt)
        self.assertEqual(s.lastPauseAt, 10343000)


if __name__ == "__main__":
    unittest.main()
