"""The end of a session, and the next one (problems #15 and #16, 2026-10-15).

#15: nothing switched the mod off when a session ended (identity.txt `idle`): the wrap went on stamping and shipping the player's commands
to nobody, the pacing kept forcing the engine speed, a gone peer's promise could freeze the game at the input barrier, and held, deferred,
parked and pause-order items lingered. Now one transition (the bridge's X.sessionOff, the simulation half's X.simSessionOff, the UI hook's
M.sessionOver) makes the game single-player again: held native commands go back to the engine (hold_off), a deferred one is failed by the
module, commands stamped and not applied fail their callbacks, the queue goes, the speed is given back and the player's commands and
speed buttons reach the engine directly.

#16: the live folder keeps to_game.log across sessions, and a new interface state read it from its first line: an earlier session's
`session`, `action` and resync lines were fed into the next world. Now identity.txt names the session (`session=`) and where
to_game.log ended before it (`start=<gen>:<offset>`, launcher gamelink::Session::open), and a state that saw a session never dispatches a line
of it again.

Two mocked games run the real mod with the fake native module of test_strict_build.py / test_inject_fail_closed.py (imported, not edited);
the one used here also does what the Rust module does at hold_off with a deferred command (hold.rs release_all: failed, never applied).

Run:  python -I -m unittest discover -s tests/lua -p "test_session_end.py"
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
import test_strict_build as S  # noqa: E402
import test_inject_fail_closed as I  # noqa: E402

BUY = 'api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function(d, s, e) %s = { s } end)'


def identity(g, session, start, role):
    """What the session controller writes (launcher gamelink::Session::write_identity): session and start first, role last. `start`:
    where to_game.log ends (bytes of its first generation)."""
    with open(os.path.join(g.dir, "identity.txt"), "w", encoding="utf-8", newline="") as f:
        f.write("session=%s\nstart=1:%d\nid=%d\nname=%s\nrole=%s\n" % (session, start, g.id, g.label, role))


def log_end(g):
    """Where the game's to_game.log ends now (the controller's Session::open)."""
    return os.path.getsize(g.to_game) if os.path.exists(g.to_game) else 0


class EndNative(I.InjectNative):
    """hold_off as the Rust module does it (hold.rs release_all): a waiting command goes back to the engine, a deferred one is FAILED
    (its tool callback fired with a failed result) and never applied."""

    def handle(self, seq, kind, p):
        super().handle(seq, kind, p)
        if kind == "hold_off":
            for hid in list(self.deferred):
                self.finish_held(hid, False, "hold_off")


class EndGame(S.StrictGame):
    def __init__(self, label, player_id, tmp, first_entity, clock):
        S.N.NativeGame.__init__(self, label, player_id, tmp, first_entity)
        clock.games.append(self)
        self.native = EndNative(self, clock)
        g = self.L.globals()
        g.FAKE_T = clock.t
        g.NATIVE_HOOK = self.native.on_add
        g.INJECT_HOOK = self.native.inject_hook
        g.HOOK_SRC = T.HOOK_SRC   # the window state's UI hook (Game.window)
        self.L.execute(S.STRICT_LUA)
        self.L.execute(I.INJECT_LUA)
        identity(self, "s1", 0, "host" if player_id == 0 else "player")
        self.reload()

    def vehicles(self):
        return len([a for a in self.applied() if a.startswith("makeVehicleBuyCmd@")])

    def records(self, kind):
        return [line for line in self.lines(self.from_game) if line.split(T.TAB)[0:1] == [kind]]


class SessionEndTest(S.StrictTest):
    strict_cancel = None   # the default: with the inject and defer capabilities a tool build goes the deferred inject flow
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
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-end-")
        self.clock = S.Clock()
        self.A = EndGame("host", 0, self.tmp, 50000, self.clock)
        self.B = EndGame("client", 1, self.tmp, 51000, self.clock)
        self.relay = T.Relay([self.A, self.B])

    def tearDown(self):
        super().tearDown()
        shutil.rmtree(self.tmp, ignore_errors=True)

    def look(self):
        """Long enough for every half to read identity.txt again (at most once a second): frames only, the game time stands."""
        self.frames(25)

    def end_session(self, *games):
        for g in games:
            identity(g, "s1", 0, "idle")
        self.look()

    # ---- #15: the end of a session

    def test_a_session_that_ends_mid_build_leaves_the_game_single_player_with_its_speed_back(self):
        self.start()
        self.strict_on(self.A, self.B)
        for g in (self.A, self.B):
            g.native.native_text = I.road_text()
            g.native.input_text = I.road_text()
        # a tool build whose command the module defers until this game's stamp apply, a window command the UI hook holds and a command
        # of the bridge's own wrap: all stamped, none applied yet
        self.B.tool(self.road(300, 0, 400, 0), make="NATIVE_ROAD")
        self.frames(3)
        self.assertIn("defer", self.B.native.ctl_kinds(), self.log())
        self.assertEqual(list(self.B.native.deferred), [1])
        self.B.window(BUY % "WIN_CB")
        self.B.send_gui(BUY % "GUI_CB")
        self.frames(4)
        actions_before = len(self.B.records("action"))
        self.assertEqual(actions_before, 3, "the build, the window's command and the wrapped command were shipped")
        self.assertTrue(self.B.lua("WIN_CB == nil and GUI_CB == nil"), "both wait for their stamps")

        # the host ends the session (identity.txt says idle): the transition runs before any stamp is reached
        self.end_session(self.B)
        log = self.B.log()
        self.assertIn("session over (the session ended; identity.txt role idle): single-player again", log)
        self.assertIn("hold_off", self.B.native.ctl_kinds())
        self.assertFalse(self.B.native.holding)
        self.assertEqual(self.B.native.completed, [(1, False, "hold_off")], "the deferred command is failed: its tool is free, nothing applied")
        self.assertEqual(int(self.B.lua("NATIVE_HELD_COUNT()")), 0, "nothing is held any more")
        self.assertTrue(self.B.lua("WIN_CB ~= nil and WIN_CB[1] == false"), "the window's held command failed: its callback ran")
        self.assertTrue(self.B.lua("GUI_CB ~= nil and GUI_CB[1] == false"), "the wrapped command failed: its callback ran")
        self.assertIn("1 deferred one(s) failed by the module", log)
        self.assertIn("2 stamped command(s) not applied (callbacks failed)", log)
        self.assertIn("dropped 1 peer(s)", log)

        # well past every stamp: nothing of the ended session is applied on this game (the simulation's queue went with it)
        self.tick(30)
        self.assertIn("session over (the interface half ended the session)", self.B.log())
        self.assertEqual((self.B.builds_applied(), self.B.vehicles()), (0, 0), self.B.log())
        self.assertEqual(self.B.native.completed, [(1, False, "hold_off")])
        # no frozen speed: the session's speed is back and the game runs on its own, whatever the gone host does
        self.assertEqual(self.B.speed(), 1)
        t = self.B.time()
        self.tick(10)
        self.assertEqual(self.B.time(), t + 10 * S.STEP, "the game runs: no pacing, no barrier, no peer")
        # nothing more goes to the launcher: no clock lines, no actions
        sent = len(self.B.lines(self.B.from_game))
        # a command now goes straight to the engine, as in single-player; the speed buttons work
        self.B.send_gui(BUY % "GUI_CB2")
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeGameSetSpeedCmd(3))")
        self.tick(2)
        self.assertTrue(self.B.lua("GUI_CB2 ~= nil and GUI_CB2[1] == true"))
        self.assertEqual(self.B.vehicles(), 1)
        self.assertEqual(self.B.speed(), 3, "the player's speed button reached the engine")
        self.assertNotIn(3, self.relay.speed_requests)
        self.assertEqual(len(self.B.lines(self.B.from_game)), sent, "no line to the launcher outside a session")
        # a native tool build now is a plain build: the module does not hold it, nothing is shipped
        self.B.tool(self.road(500, 0, 600, 0), make="NATIVE_ROAD")
        self.tick(3)
        self.assertEqual(self.B.builds_applied(), 1)
        self.assertEqual(len(self.B.records("action")), actions_before)

    def again(self):
        """A fresh pair of games for the next variant of one test."""
        self.tearDown()
        self.setUp()

    def test_a_paused_command_of_the_ended_session_fails_and_its_pause_holds_nothing(self):
        # (1) waiting for the host's pause order, which never comes: the host is gone. (2) the pause order (pause-align) stamped it one
        # step past where the games stand still; the session ends after the stamp came back and before this game ran on to it.
        for waits_for_order in (True, False):
            with self.subTest(waits_for_order=waits_for_order):
                if not waits_for_order:
                    self.again()
                self.start(speed=2)
                if waits_for_order:
                    self.strict_on(self.A, self.B)
                self.relay.pauseAt = self.relay.future_stamp()
                self.relay.session()
                self.tick(20)
                self.assertEqual(self.B.time(), self.relay.pauseAt)
                pump = self.relay.pump
                if waits_for_order:
                    self.relay.pump = lambda: None
                self.B.window(BUY % "WIN_CB")
                if waits_for_order:
                    self.frames(4)
                    self.assertIn("applied when the host's pause order brings it back", self.B.log())
                else:
                    self.frames(30)   # frames only: the stamp comes back, the simulation does not step
                    self.assertIn("pause order #1: 1:1 stamped", self.B.log())
                    self.assertTrue(self.B.lua("WIN_CB == nil"), "it waits for its stamp")
                self.end_session(self.B)
                self.relay.pump = pump
                self.assertIn("1 stamped command(s) not applied (callbacks failed)" +
                              ("; dropped 1 peer(s), 1 pause-order item(s)" if waits_for_order else ""), self.B.log())
                self.assertTrue(self.B.lua("WIN_CB ~= nil and WIN_CB[1] == false"))
                # the session's speed is back: no run-to-stamp or pause of the gone host holds this game any more
                self.tick(10)
                self.assertEqual(self.B.speed(), 2)
                self.assertGreater(self.B.time(), self.relay.pauseAt)
                self.assertEqual(self.B.vehicles(), 0, "the paused action of the ended session is applied nowhere here")

    # ---- #16: the next session

    def second_session(self, reload):
        """Session 1 with a bought vehicle, its end, then (with `reload`: in fresh Lua states, another save loaded) session 2."""
        self.start()
        self.B.send_gui(BUY % "GUI_CB")
        self.tick(15)
        self.assertEqual((self.A.vehicles(), self.B.vehicles()), (1, 1))
        self.end_session(self.A, self.B)
        self.tick(2)
        for g in (self.A, self.B):
            self.assertIn("session over", g.log())
        if reload:
            for g in (self.A, self.B):
                g.reload()
        # the controller opens session 2 on the same folder: a new id, and the end of session 1's lines as its start
        self.relay.started, self.relay.speed, self.relay.pauseAt, self.relay.safes = False, 0, None, {}
        self.starts = {g.id: log_end(g) for g in (self.A, self.B)}
        for g in (self.A, self.B):
            identity(g, "s2", self.starts[g.id], "host" if g.id == 0 else "player")
        self.look()
        for g in (self.A, self.B):
            self.assertIn("session s2 begins here", g.log())

    def check_nothing_replayed(self):
        t = (self.A.time(), self.B.time())
        self.tick(10)
        self.assertEqual((self.A.time(), self.B.time()), t, "both hold until the host starts session 2 (session 1's start line is not taken)")
        self.assertEqual((self.A.vehicles(), self.B.vehicles()), (1, 1), "session 1's action is not applied again")
        self.relay.started, self.relay.speed = True, 1
        self.relay.session()
        self.tick(5)
        self.B.send_gui(BUY % "GUI_CB3")
        self.tick(15)
        self.assertEqual((self.A.vehicles(), self.B.vehicles()), (2, 2), "session 2 works: its own action lands once on both")
        self.assertTrue(self.B.lua("GUI_CB3 ~= nil and GUI_CB3[1] == true"))
        self.assertEqual(self.relay.replay_failed, [])

    def test_the_next_session_never_replays_or_applies_an_ended_sessions_lines_or_saved_queue(self):
        for reload in (False, True):
            with self.subTest(reload=reload):
                if reload:
                    self.again()
                self.second_session(reload=reload)
                if reload:
                    self.assertIn("session s2 begins here (fresh): to_game.log from 1:%d" % self.starts[self.B.id], self.B.log())
                self.check_nothing_replayed()
        with self.subTest("a save made mid-session"):
            self.again()
            self.start()
            self.B.send_gui(BUY % "GUI_CB")
            self.frames(2)
            self.tick(1)
            # the world's game-script state holds the stamped action now: a save made at this moment carries it
            self.assertEqual(int(self.B.lua("#STATE:get().queue")), 1)
            self.B.run("SAVED_QUEUE = STATE:get().queue")
            self.end_session(self.A, self.B)
            self.tick(2)
            self.assertEqual(int(self.B.lua("#STATE:get().queue")), 0, "the ended session's queue is gone")
            # that save is loaded later and a new session is hosted on it
            self.B.run("local st = STATE:get(); st.queue = SAVED_QUEUE; STATE:set(st)")
            for g in (self.A, self.B):
                g.reload()
            self.relay.started, self.relay.speed, self.relay.pauseAt, self.relay.safes = False, 0, None, {}
            for g in (self.A, self.B):
                identity(g, "s2", log_end(g), "host" if g.id == 0 else "player")
            self.look()
            self.tick(2)
            self.assertIn("a new session: dropped 1 queued action(s) and binder(s) of an earlier session", self.B.log())
            self.relay.started, self.relay.speed = True, 1
            self.relay.session()
            self.tick(15)
            self.assertEqual((self.A.vehicles(), self.B.vehicles()), (0, 0), "the saved queue's action is applied nowhere")


@T.own_tests_only
class TwoSimulationStatesSessionTest(SessionEndTest):
    """The real game shares one game's steps between two simulation Lua states (the mock's SIM_SPLIT); each sees a session end on its own
    (identity.txt, read at most once a second, or the interface half's `sessionoff`, which reaches one of them). The clean-up of the
    game-script state ran in each state that saw the end: a state that saw it after the next session had begun emptied that session's
    queue, outputs and epoch (an action applied on the other games only)."""

    def test_a_state_that_sees_the_end_late_leaves_the_next_sessions_queue_alone(self):
        self.A.split_sim("function(t) return ONLY_STATE or (math.floor(t / 200) % 2 + 1) end")
        self.start()
        self.B.send_gui(BUY % "GUI_CB")
        self.tick(15)
        self.assertEqual((self.A.vehicles(), self.B.vehicles()), (1, 1))
        runs = self.A.sim_runs()
        self.assertGreater(min(runs[1], runs[2]), 5, "both states took part in session 1: %s" % runs)
        # session 1 ends and session 2 begins while the host's first state runs every step
        self.A.run("ONLY_STATE = 1")
        self.end_session(self.A, self.B)
        self.tick(2)
        self.relay.started, self.relay.speed, self.relay.pauseAt, self.relay.safes = False, 0, None, {}
        for g in (self.A, self.B):
            identity(g, "s2", log_end(g), "host" if g.id == 0 else "player")
        self.look()
        self.relay.started, self.relay.speed = True, 1
        self.relay.session()
        self.tick(5)
        self.B.send_gui(BUY % "GUI_CB2")
        for _ in range(10):
            self.tick(1)
            if int(self.A.lua("#STATE:get().queue")) > 0:
                break
        self.assertEqual(int(self.A.lua("#STATE:get().queue")), 1, "session 2's action is queued on the host")
        # the host's other state runs again: it sees session 1's end only now
        self.A.run("ONLY_STATE = nil")
        self.tick(15)
        self.assertEqual((self.A.vehicles(), self.B.vehicles()), (2, 2), "session 2's action lands on both games\n" + self.A.log())
        self.assertTrue(self.B.lua("GUI_CB2 ~= nil and GUI_CB2[1] == true"))
        self.assertIn("cleared by another simulation state", self.A.log())
        self.assertEqual(self.relay.replay_failed, [])


class IdentityTest(unittest.TestCase):
    """common.lua C.poll: the session switch every half reads."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-ident-")
        self._env = {k: os.environ.get(k) for k in ("FEVERLINK3_SESSION_DIR", "FEVERLINK3_LIVE_DIR", "FEVERLINK3_PLAYER_ID")}
        os.environ.pop("FEVERLINK3_SESSION_DIR", None)
        os.environ.pop("FEVERLINK3_PLAYER_ID", None)
        os.environ["FEVERLINK3_LIVE_DIR"] = self.tmp
        import lupa.lua52 as lupa
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.C = T.load_common(self.L)

    def tearDown(self):
        for k, v in self._env.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
        shutil.rmtree(self.tmp, ignore_errors=True)

    def write(self, text):
        with open(os.path.join(self.tmp, "identity.txt"), "w", encoding="utf-8", newline="") as f:
            f.write(text)

    def poll(self):
        return self.C.poll(True)

    def test_on_off_another_session_and_incomplete_files(self):
        self.assertFalse(self.poll(), "no identity: idle")
        self.write("session=aa\nstart=1:7\nid=2\nname=Pat\nrole=player\n")
        self.assertTrue(self.poll())
        self.assertEqual((self.C.SESSION, self.C.posText(self.C.SESSION_START), self.C.PLAYER_ID, self.C.GEN), ("aa", "1:7", 2, 1))
        self.write("session=aa\nstart=1:7\nid=2\nname=Pat\nrole=pla")   # a read racing the write: no complete role line
        self.assertTrue(self.poll(), "an incomplete file changes nothing")
        self.write("")
        self.assertTrue(self.poll())
        self.write("session=bb\nstart=2:40\nid=0\nname=Pat\nrole=host\n")
        self.assertTrue(self.poll())
        self.assertEqual((self.C.SESSION, self.C.posText(self.C.SESSION_START), self.C.PLAYER_ID, self.C.GEN), ("bb", "2:40", 0, 2), "another session began")
        self.write("session=cc\nstart=2:41\nid=0\nname=Pat\nrole=idle\n")
        self.assertFalse(self.poll(), "idle: the session is over")
        self.assertEqual(self.C.ROLE, "idle")
        self.write("id=1\nname=Old\nrole=player\n")
        self.assertTrue(self.poll(), "an identity without a session line (an older writer) still starts one")
        self.assertEqual((self.C.SESSION, self.C.SESSION_START, self.C.GEN), ("", None, 3))


if __name__ == "__main__":
    unittest.main(verbosity=2)
