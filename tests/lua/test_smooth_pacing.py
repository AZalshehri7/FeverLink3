"""Bug 4 (owner list 2026-10-17): the input barrier's stand-stills over the internet, and smooth pacing.

Without it a game runs at full speed into its stop (the input barrier, or the pacing limit past the slowest peer) and stands there until
a peer's next clock moves the stop on. With it (feverlink3_lockstep.lua "smooth pacing", native pace.rs) the game asks the native module
to stretch the engine's batch interval by a factor from 1 to 2 as the room before the stop shrinks, and runs a little slower instead of
standing still.

The two mocked games are the harness of test_lockstep.py over the adaptive host and the jittery links of test_barrier_cadence.py
(imported, never edited). The native module is played by the test: each game's batches come every 1/6 s of real time times the factor
the mod last wrote to its control file (`pace {permille}`), the way the module stretches the engine's interval after each batch. A game
"stands still" in a tick in which its mod holds the engine at speed 0 while the session runs. Checked over links of 150 and 300 ms
round trip with jitter: the stand-still time drops sharply against the same network without the factor, no item is applied late,
and both games applied the same items in the same order at the same game times.

Run:  python -I tests/lua/test_smooth_pacing.py
"""
import glob
import os
import re
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)  # python -I does not put the script's folder on the path

import test_barrier_cadence as BC  # noqa: E402
import test_input_barrier as B  # noqa: E402
import test_lockstep as T  # noqa: E402

STEP = T.STEP
TICK_S = 1.0 / 6   # an engine batch of the harness: a sixth of a second of real time at factor 1

# what the native module writes into native_events.log when it installed the redirect of CGame::Sync (pace.rs HOOK_NAME)
READY = 'ready\tnative\t{["build"]="40420",["clock"]=false,["hooks"]={[1]="batch_pace"},["poolpatch"]=false,["sites"]=0,["version"]="test"}\n'
PACE_LINE = re.compile(r'^pace\t[^\t]*\t.*\["permille"\]=(\d+)', re.M)


class PacingTest(BC.CadenceTest):
    def make(self, n, seed, delay, lead_steps=None, rtt_ms=None, paced=True):
        super().make(n, seed, delay, lead_steps, rtt_ms)
        self.paced = paced
        self.credit = {g.id: 0.0 for g in self.games}
        self.stood = {g.id: 0 for g in self.games}
        self.ran = {g.id: 0 for g in self.games}
        self.factors = {g.id: [] for g in self.games}
        self.measuring = False
        self.ctl_seen = {}
        self.factor_now = {g.id: 1.0 for g in self.games}
        if paced:
            for g in self.games:
                with open(os.path.join(g.dir, "native_events.log"), "w", encoding="utf-8", newline="") as f:
                    f.write(READY)

    def factor(self, g):
        """The factor the mod last wrote to any of its control files (the module reads them all), 1 before any."""
        for path in glob.glob(os.path.join(g.dir, "native_ctl.*.log")):
            at = self.ctl_seen.get(path, 0)
            with open(path, "r", encoding="utf-8", newline="") as f:
                f.seek(at)
                text = f.read()
                self.ctl_seen[path] = f.tell()
            found = PACE_LINE.findall(text)
            if found:
                p = int(found[-1])
                assert 500 <= p <= 2000, p
                self.factor_now[g.id] = p / 1000.0
        return self.factor_now[g.id]

    def held(self, g):
        return int(g.L.eval("FIND_UP('G').lastSpeedSet or -1")) == 0

    def tick(self, n=1):
        """A sixth of a second of real time in four frames, each followed by a relay pump and a network round. A game's engine runs a
        batch once TICK_S times its factor of real time has passed since its last one (the native module stretching the interval), at
        the end of a frame; a lagging game skips some (a slow PC). A frame in which a game's mod holds its engine at speed 0 while the
        session runs is a quarter tick of stand-still."""
        for _ in range(n):
            for _ in range(4):
                self.wall(TICK_S / 4)
                for g in self.games:
                    if g.id not in self.silent:
                        g.frame()
                self.relay.pump()
                self.relay.deliver()
                running = self.relay.started and self.relay.speed > 0 and self.relay.pauseAt is None
                for g in self.games:
                    if g.id in self.silent:
                        continue
                    f = self.factor(g) if self.paced else 1.0
                    if self.measuring and running:
                        self.ran[g.id] += 0.25
                        self.factors[g.id].append(f)
                        if self.held(g):
                            self.stood[g.id] += 0.25
                    self.credit[g.id] += 0.25
                    if self.credit[g.id] >= f - 1e-9:
                        self.credit[g.id] -= f
                        if self.rng.random() >= self.lag[g.id]:
                            g.step()
            if isinstance(self.relay, BC.AdaptiveRelay):
                self.relay.adapt()

    def run_link(self, rtt_ms, seed, speed, paced, minutes=1.5, lag=(0.0, 0.1), delay=None):
        self.make(2, seed, delay or BC.link(rtt_ms), rtt_ms=rtt_ms, paced=paced)
        self.start(speed=speed)
        for g, l in zip(self.games, lag):
            self.lag[g.id] = l
        t0 = [g.time() for g in self.games]
        self.measuring = True
        issued = 0
        for _ in range(int(minutes * 60 / TICK_S)):
            if self.rng.random() < 0.05:
                self.rng.choice(self.games).send_gui(self.rng.choice([B.BUY, B.LINE]))
                issued += 1
            self.tick(1)
        self.measuring = False
        progressed = [g.time() - t for g, t in zip(self.games, t0)]
        self.settle()
        self.assert_no_late_and_same(issued)
        stood_s = {g.label: self.stood[g.id] * TICK_S / minutes for g in self.games}   # per minute
        worst = max(max(v) if v else 1.0 for v in self.factors.values())
        return stood_s, progressed, worst, issued

    def compare(self, rtt_ms, seed, speed, **kw):
        before, prog0, f0, n0 = self.run_link(rtt_ms, seed, speed, paced=False, **kw)
        after, prog1, f1, n1 = self.run_link(rtt_ms, seed, speed, paced=True, **kw)
        line = "rtt %d ms x%d seed %d: stand-still s/min before %s after %s; game time advanced before %s after %s; largest factor %.2f; items %d/%d" % (
            rtt_ms, speed, seed, {k: round(v, 1) for k, v in before.items()}, {k: round(v, 1) for k, v in after.items()},
            prog0, prog1, f1, n0, n1)
        print("\n  " + line)
        self.assertEqual(f0, 1.0, "without the module's ready, no factor is ever written")
        self.assertLessEqual(f1, 2.0)
        return before, after, prog0, prog1, line

    def check_drop(self, before, after, prog0, prog1, line, keep, pace_cost):
        total0, total1 = sum(before.values()), sum(after.values())
        self.assertGreater(total0, 5.0, "the network makes the games stand still without smooth pacing: " + line)
        self.assertLessEqual(total1, keep * total0, "the stand-still time drops sharply: " + line)
        for p0, p1 in zip(prog0, prog1):
            self.assertGreaterEqual(p1, (1 - pace_cost) * p0, "the session is not much slower than it was: " + line)

    # Measured on this harness (2026-10-18, 90 s per run, the client's PC 10 % slower; stand-still s/min of both games together, and the
    # game time advanced against the run without the factor): 150 ms x1 24.0 -> 1.5 (-1 %) and 29.5 -> 1.5 (-5 %), x2 34.0 -> 4.5 (-9 %);
    # 300 ms x1 19.4 -> 7.1 (-3 %) and 18.3 -> 5.1 (-3 %), x2 30.2 -> 14.1 (-20 %). Over 300 ms at x2 the room is a single batch most of
    # the time: both games wait for each other's clock whatever they do, and the pacing spreads the waits at a cost in speed.

    @T.full
    def test_smooth_pacing_cuts_the_stand_still_over_150_and_300_ms(self):
        runs = [(150, seed, speed, 0.3, 0.15) for seed, speed in ((51, 1), (52, 2), (53, 1))]
        runs += [(300, seed, speed, 0.6, 0.25) for seed, speed in ((61, 1), (62, 2), (63, 1))]
        for rtt, seed, speed, keep, cost in runs:
            with self.subTest(rtt=rtt, seed=seed, speed=speed):
                self.check_drop(*self.compare(rtt, seed, speed), keep=keep, pace_cost=cost)

    def test_a_game_with_ample_room_runs_at_full_speed(self):
        # a fast link with a generous lead: the room always outlasts the stop's next move, so the factor stays 1
        self.make(2, 71, lambda rng, dest: 1, lead_steps=12)
        self.start(speed=1)
        self.measuring = True
        self.tick(300)
        self.assertTrue(all(f == 1.0 for v in self.factors.values() for f in v), "factor 1 throughout")
        self.assertEqual(sum(self.stood.values()), 0)

# the harness's own tests run in their own files
for _name in dir(BC.CadenceTest):
    if _name.startswith("test_") and _name not in PacingTest.__dict__:
        setattr(PacingTest, _name, None)


class PureSmoothPacingTest(unittest.TestCase):
    def setUp(self):
        os.environ.pop("FEVERLINK3_SESSION_DIR", None)
        self.L = T.lupa.LuaRuntime(unpack_returned_tuples=True)
        T.load_common(self.L)
        self.K = self.L.execute(T.LOCKSTEP_SRC)

    def test_the_pure_pacing_room_stop_batch_time_and_factor(self):
        K = self.K
        bias = K.PACE_BIAS
        # 1x, a batch every 200 ms, the last one just now: one step of room means the next batch empties it
        self.assertAlmostEqual(K.paceTarget(1 * STEP, 1, 300, 200, 0)[0], 300 * bias / 200)
        self.assertEqual(K.paceTarget(1 * STEP, 1, 100, 200, 0)[0], 1, "the stop moves before the batch is due")
        # two steps: one safe batch, the second empties it: 2 batches to cover the wait
        self.assertEqual(K.paceTarget(2 * STEP, 1, 300, 200, 0)[0], 1)
        self.assertAlmostEqual(K.paceTarget(2 * STEP, 1, 500, 200, 0)[0], 500 * bias / 400)
        # half a batch already gone: the same moment of the next batch
        self.assertAlmostEqual(K.paceTarget(1 * STEP, 1, 200, 200, 100)[0], (100 + 200 * bias) / 200)
        # at 2x a batch is two steps: two steps of room are one batch
        self.assertAlmostEqual(K.paceTarget(2 * STEP, 2, 300, 200, 0)[0], 300 * bias / 200)
        self.assertEqual(K.paceTarget(4 * STEP, 2, 300, 200, 0)[0], 1)
        # never more than half speed; standing still already: the batches after the release come stretched
        self.assertEqual(K.paceTarget(1 * STEP, 1, 3000, 200, 0)[0], K.PACE_MAX)
        self.assertEqual(K.paceTarget(0, 1, 100, 200, 0)[0], K.PACE_MAX)
        # nothing known: 1
        self.assertEqual(K.paceTarget(None, 1, 300, 200, 0)[0], 1)
        self.assertEqual(K.paceTarget(1 * STEP, 1, None, 200, 0)[0], 1)
        # more room, less factor
        prev = None
        for i in range(1, 41):
            f = K.paceTarget(i * STEP / 4, 1, 600, 200, 0)[0]
            self.assertTrue(1 <= f <= K.PACE_MAX)
            if prev is not None:
                self.assertLessEqual(f, prev + 1e-9)
            prev = f
        # the stops next move is expected after its usual gap or as late again
        K = self.K
        P = K.paceNew()
        self.assertIsNone(K.paceNext(P, 10.0), "nothing measured: no expectation")
        clock, stop = 10.0, 1000
        K.paceObserve(P, stop, clock)
        for i in range(20):
            clock += 0.5 if i == 7 else 0.2
            stop += STEP
            K.paceObserve(P, stop, clock)
        self.assertAlmostEqual(K.paceGap(P, 50), 200, delta=1)
        self.assertAlmostEqual(K.paceNext(P, clock + 0.05), 150, delta=1)
        # later than usual: within its jitter at first, then as late again as it is
        self.assertAlmostEqual(K.paceNext(P, clock + 0.2), K.paceJitter(P), delta=1)
        self.assertAlmostEqual(K.paceNext(P, clock + 1.2), 1000, delta=1)
        # a stop that does not move is no gap; one that went back (a peer left) starts the count again; frames of one moment are one
        K.paceObserve(P, stop, clock + 5)
        K.paceObserve(P, stop - STEP, clock + 6)
        K.paceObserve(P, stop, clock + 6.2)
        self.assertEqual(len(list(P.gaps.values())), 21)
        K.paceObserve(P, stop + STEP, clock + 60)
        self.assertEqual(max(P.gaps.values()), K.PACE_WAIT_MAX_MS, "an outage counts as the longest wait the pacing cares about")
        for _ in range(100):
            clock, stop = clock + 0.3, stop + STEP
            K.paceObserve(P, stop, clock)
        self.assertEqual(len(list(P.gaps.values())), K.PACE_GAPS)
        # the batch time is measured with the factor taken out
        K = self.K
        P = K.paceNew()
        self.assertEqual(K.paceBatch(P), STEP, "nothing measured yet")
        clock, now = 10.0, 1000
        K.paceOwn(P, now, clock, True, 1)
        for i in range(10):
            f = 1.5 if i % 2 else 1.0
            clock += 0.25 * f
            now += STEP
            K.paceOwn(P, now, clock, True, f)
        self.assertAlmostEqual(K.paceBatch(P), 250, delta=1)
        # a batch after a hold is not a batch time
        clock += 3.0
        K.paceOwn(P, now, clock, False, 1)
        now += STEP
        K.paceOwn(P, now, clock + 0.01, True, 1)
        self.assertAlmostEqual(K.paceBatch(P), 250, delta=1)
        # the stop is the nearer of the barrier and the pacing limit and none when not running
        K, L = self.K, self.L
        s = K.newSession()
        peers = L.table()
        K.applyPeerClock(peers, L.table_from({"from": 1, "t": 1000, "ahead": 3, "safe": 4000}), 0)
        self.assertIsNone(K.paceStop(s, peers, 3), "not started")
        K.applySession(s, L.table_from({"started": True, "speed": 1, "lead": 600}))
        self.assertEqual(K.paceStop(s, peers, 3), 1000 + 3 * STEP, "the pacing limit is nearer than the barrier")
        K.applyPeerClock(peers, L.table_from({"from": 1, "t": 3000, "ahead": 3, "safe": 4000}), 0)
        self.assertEqual(K.paceStop(s, peers, 3), 4000 - 3 * STEP, "the barrier (promise less the receiver's keep)")
        K.applySession(s, L.table_from({"started": True, "speed": 0, "lead": 600}))
        self.assertIsNone(K.paceStop(s, peers, 3), "speed 0")
        # the factor follows its target and stays in range
        K = self.K
        P = K.paceNew()
        K.paceSmooth(P, 2, 0.0)
        self.assertEqual(P.trim, 1)
        K.paceSmooth(P, 2, 0.1)
        self.assertGreater(P.trim, 1.9, "up at once: the next batch needs it")
        K.paceSmooth(P, 1, 0.2)
        self.assertGreater(P.trim, 1.2, "down over a few tenths of a second")
        K.paceSmooth(P, 1, 3.0)
        self.assertEqual(P.trim, 1, "exactly 1 again")
        K.paceSmooth(P, 5, 9.0)
        self.assertLessEqual(P.trim, 2)

if __name__ == "__main__":
    unittest.main()
