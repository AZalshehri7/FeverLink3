"""The lockstep input barrier (feverlink3_lockstep.lua: L.promise, L.barrier, L.clampStamp, L.pace): no game simulates a step
it may still get a peer's input for, so no item is ever applied late, whatever the network does.

Two or three mocked games (the harness of test_lockstep.py, imported and not edited) talk through a relay that delays every
line it writes to a game by a random number of relay rounds (seeded), keeping each link in order as the real path does (one
reliable connection per game, the host relaying in the order it handles lines). Delays include spikes far beyond the lead.
The games issue actions at random moments, at random speeds and leads, through pauses and resumes. Checked: no LATE apply,
no breach, no repair request, and every game applied the same items in the same order at the same game times. A control
run without the promises (the pacing alone) shows that the same network does make late applies. A peer that falls silent
holds the others at the barrier until the session says it left; then they run on.

Run:  python -I tests/lua/test_input_barrier.py        (or: python -m unittest discover -s tests/lua)
Set FEVERLINK3_TEST_VERBOSE=1 to print the games' mod.log on failure.
"""
import os
import random
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)  # python -I does not put the script's folder on the path

import test_lockstep as T  # noqa: E402  (the harness; imported, never edited)

STEP = T.STEP

BUY = 'api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }))'
LINE = 'api.cmd.sendCommand(api.cmd.makeLineCreateCmd("L", { x = 0, y = 0, z = 0 }, 7, { stops = {} }))'


class JitterRelay(T.Relay):
    """The test relay with a network: every line to a game is delivered `delay(rng)` relay rounds later, never before the
    line written to the same game before it (each link stays in order)."""

    def __init__(self, games, rng, delay, lead_steps=None):
        super().__init__(games)
        self.rng, self.delay, self.lead_steps = rng, delay, lead_steps
        self.round, self.old_stamps = 0, False
        self.out = {g.id: [] for g in games}
        self.last_due = {g.id: 0 for g in games}

    @property
    def lead(self):
        if self.lead_steps is not None:
            return STEP * self.lead_steps
        return STEP * (4 + 2 * max(self.speed, 0))

    def send(self, g, kind, frm, payload):
        if kind == "action" and self.old_stamps:
            # the control: the sender's stamp as the rule before the barrier made it (its clock plus the lead, no promise)
            p = self.C.parse(payload)
            p = p[0] if isinstance(p, tuple) else p
            p.stamp = -(-(self.clocks[int(frm)] + self.lead) // STEP) * STEP
            payload = self.C.serialize(p)
        due = max(self.last_due[g.id], self.round + self.delay(self.rng, g.id))
        self.last_due[g.id] = due
        self.out[g.id].append((due, g, kind, frm, payload))

    def deliver(self):
        self.round += 1
        for q in self.out.values():
            while q and q[0][0] <= self.round:
                _, g, kind, frm, payload = q.pop(0)
                T.Relay.send(self, g, kind, frm, payload)

    def in_flight(self):
        return sum(len(q) for q in self.out.values())


def jitter(spike_p=0.04, spike=(20, 70), base=(0, 6)):
    """A delay in relay rounds: mostly a few rounds, sometimes a spike far beyond the lead (4 rounds make an engine tick)."""
    def delay(rng, dest):
        if rng.random() < spike_p:
            return rng.randint(*spike)
        return rng.randint(*base)
    return delay


class BarrierTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-barrier-")
        self.games = []

    def tearDown(self):
        if os.environ.get("FEVERLINK3_TEST_VERBOSE"):
            for g in self.games:
                print("\n--- %s mod.log ---\n%s" % (g.label, g.log()))
        shutil.rmtree(self.tmp, ignore_errors=True)

    def make(self, n, seed, delay, lead_steps=None):
        labels = ["host", "client", "third"]
        run = tempfile.mkdtemp(dir=self.tmp)
        self.games = [T.Game(labels[i], i, run, 50000 + 1000 * i) for i in range(n)]
        self.rng = random.Random(seed)
        self.relay = JitterRelay(self.games, self.rng, delay, lead_steps)
        self.lag = {g.id: 0.0 for g in self.games}
        self.silent = set()

    def tick(self, n=1):
        """One engine tick: four interface frames, each followed by a relay pump and a network round; then each game's engine
        tick (a lagging game skips some)."""
        for _ in range(n):
            for _ in range(4):
                for g in self.games:
                    if g.id not in self.silent:
                        g.frame()
                self.relay.pump()
                self.relay.deliver()
            for g in self.games:
                if g.id not in self.silent and self.rng.random() >= self.lag[g.id]:
                    g.step()

    def start(self, speed=1):
        self.tick(5)
        while self.relay.in_flight():
            self.tick(1)
        self.relay.started, self.relay.speed = True, speed
        self.relay.session()
        self.tick(10)

    def settle(self):
        """Every line delivered and every stamp passed: what was shipped has landed everywhere."""
        for _ in range(400):
            self.tick(1)
            if not self.relay.in_flight():
                break
        self.tick(60)

    def logs(self):
        return "".join(g.log() for g in self.games)

    def assert_no_late_and_same(self, issued):
        logs = self.logs()
        self.assertNotIn("LATE", logs, "an item was applied late")
        self.assertNotIn("BREACHED", logs, "an item arrived after its stamp")
        self.assertEqual(self.relay.replay_failed, [], "no repair was asked for")
        first = self.games[0].applied()
        self.assertEqual(len(first), issued, first)
        for g in self.games[1:]:
            self.assertEqual(g.applied(), first, "%s applied other items, in another order or at other times" % g.label)

    def random_run(self, n, seed, ticks, lead_choices, speeds, p_action=0.15, pauses=False):
        self.make(n, seed, jitter())
        self.relay.lead_steps = self.rng.choice(lead_choices)
        self.start(speed=self.rng.choice(speeds))
        for g in self.games:
            self.lag[g.id] = self.rng.choice([0.0, 0.1, 0.3])
        issued = 0
        times = [g.time() for g in self.games]
        for i in range(ticks):
            if self.rng.random() < p_action:
                g = self.rng.choice(self.games)
                g.send_gui(self.rng.choice([BUY, LINE]))
                issued += 1
            if i % 40 == 39:
                # the host changes the speed or the lead now and then (the lead also shrinks: the promise may not)
                self.relay.speed = self.rng.choice(speeds)
                self.relay.lead_steps = self.rng.choice(lead_choices)
                self.relay.session()
            if pauses and i % 90 == 45:
                self.relay.pump()
                self.relay.pauseAt = self.relay.future_stamp()
                self.relay.session()
                for _ in range(self.rng.randint(5, 40)):
                    if self.rng.random() < 0.2:
                        self.rng.choice(self.games).send_gui(BUY)
                        issued += 1
                    self.tick(1)
                self.relay.pauseAt = None
                self.relay.session()
            self.tick(1)
        self.settle()
        for g, t0 in zip(self.games, times):
            self.assertGreater(g.time(), t0 + 40 * STEP, "%s ran (no deadlock)" % g.label)
        self.assert_no_late_and_same(issued)
        return issued

    # ---- the barrier under a network that misbehaves

    # The pauses under jitter run on the test's own clock in test_barrier_cadence.py (the seeds that breached the barrier before), so
    # the real-clock runs below are one seed by default and the rest in the full set.
    def test_two_games_with_jitter_never_apply_late(self):
        self.random_run(2, 1, 260, lead_choices=[1, 2, 3, 6], speeds=[1, 2, 3, 4])

    @T.full
    def test_more_seeds_three_games_pauses_and_a_control_without_the_promises(self):
        runs = [("two games", 2, s, 260, [1, 2, 3, 6], [1, 2, 3, 4], False) for s in (2, 3)]
        runs += [("three games", 3, 11, 260, [1, 3, 5], [1, 3, 4], False)]
        runs += [("pauses and resumes", 2, s, 280, [1, 3], [1, 2, 4], True) for s in (21, 22)]
        for name, n, seed, ticks, leads, speeds, pauses in runs:
            with self.subTest(name, seed=seed):
                self.random_run(n, seed, ticks, lead_choices=leads, speeds=speeds, pauses=pauses)
        # The control: stamps by the rule before the barrier (the sender's clock plus the lead) and no promises, so only the pacing
        # holds the client (at most `ahead` steps past the slowest clock it knows). The same kind of network then makes the client
        # pass stamps before their items arrive. So the runs above do test the barrier.
        self.make(2, 5, jitter(spike_p=0.08))
        self.relay.drop_promises = True
        self.relay.old_stamps = True
        self.relay.lead_steps = 1
        self.start(speed=3)
        for i in range(200):
            if i % 5 == 0:
                self.games[0].send_gui(BUY)
            self.tick(1)
        self.settle()
        self.assertIn("LATE", self.logs(), "the control run had no late apply: the jitter is too gentle to test anything")
        self.assertTrue(any("late apply" in d for _, d in self.relay.replay_failed), "a late apply asks for the repair")

    # ---- a peer that falls silent, then leaves

    def test_a_silent_peer_holds_the_barrier_and_its_leaving_releases_it(self):
        self.make(2, 7, lambda rng, dest: 1)
        self.start(speed=2)
        self.tick(20)
        host, client = self.games
        self.silent.add(host.id)   # the host's game stops (frozen, or its connection is gone and nobody said so yet)
        self.tick(40)
        held = client.time()
        self.tick(40)
        self.assertEqual(client.time(), held, "the client waits for the silent game's inputs")
        self.assertEqual(client.speed(), 0)
        safe = self.relay.safes[host.id]
        self.assertLessEqual(held, safe, "never past the silent game's promise")
        # the session's disconnect path: the player is gone (or away, its slot held)
        self.relay.broadcast("player_left", self.relay.ser({"id": host.id, "name": host.label, "away": True}))
        self.tick(30)
        self.assertGreater(client.time(), held + 10 * STEP, "the barrier is released when the peer leaves")
        self.assertGreater(client.speed(), 0)


class PureBarrierTest(unittest.TestCase):
    def setUp(self):
        os.environ.pop("FEVERLINK3_SESSION_DIR", None)
        self.L = T.lupa.LuaRuntime(unpack_returned_tuples=True)
        T.load_common(self.L)
        self.K = self.L.execute(T.LOCKSTEP_SRC)

    def session(self, **kw):
        s = self.K.newSession()
        self.K.applySession(s, self.L.table_from(dict({"started": True, "speed": 1, "lead": 600}, **kw)))
        return s

    def test_promise_floor_clamp_barrier_pace_and_the_pause_stop(self):
        K = self.K
        s = self.session(speed=1, lead=600)
        safe, floor, pause = K.promise(s, 1000, None)
        self.assertEqual((safe, floor, pause), (1600 + 1 * STEP, 1800, False), "now + lead rounded up + one batch (1 step at x1)")
        # the lead shrinks: the promise never goes back
        K.applySession(s, self.L.table_from({"started": True, "speed": 1, "lead": 200}))
        safe, floor, _ = K.promise(s, 1200, floor)
        self.assertEqual((safe, floor), (1800, 1800))
        self.assertEqual(K.clampStamp(s, 1200, 1400, floor), 1800, "a stamp is held to the promise")
        self.assertEqual(K.clampStamp(s, 1200, 3000, floor), 3000)
        # stopped at a pause: the pause stop is promised, flagged, and the pause stamps stay
        K.applySession(s, self.L.table_from({"started": True, "speed": 1, "lead": 200, "pauseAt": 1400}))
        safe, floor2, pause = K.promise(s, 1400, floor)
        self.assertEqual((safe, floor2, pause), (1400, 1800, True))
        self.assertEqual(K.clampStamp(s, 1400, 1400, floor2), 1400, "at the pause the stamp is the pause time")
        self.assertEqual(K.clampStamp(s, 1200, 1400, floor2), 1800, "before reaching the pause it is held to the promise")
        # the barrier: the smallest promise of the peers less the receiver's margin, a batch ends on it, a pause stop exactly
        L = self.L
        s = self.session(speed=4, lead=800)
        peers = L.table()
        self.assertIsNone(K.barrier(s, peers), "no peer: no barrier")
        K.applyPeerClock(peers, L.table_from({"from": 1, "t": 1000, "ahead": 6}), 0)
        self.assertIsNone(K.barrier(s, peers), "a peer without a promise is paced as before")
        K.applyPeerClock(peers, L.table_from({"from": 1, "t": 1000, "ahead": 6, "safe": 3000}), 0)
        self.assertEqual(K.barrier(s, peers), 3000 - 6 * STEP, "the receiver keeps marginTarget(4) = 6 steps")
        K.applyPeerClock(peers, L.table_from({"from": 2, "t": 1000, "ahead": 6, "safe": 2800}), 0)
        self.assertEqual(K.barrier(s, peers), 1600, "the smallest promise")
        self.assertEqual(K.pace(s, peers, 1000, 99), 3, "the batch ends on the barrier")
        self.assertEqual(K.pace(s, peers, 1500, 99), 0, "no partial step past it")
        self.assertEqual(K.pace(s, peers, 1600, 99), 0)
        K.removePeer(peers, 2)
        self.assertEqual(K.barrier(s, peers), 1800, "a peer that left no longer holds anyone")
        K.applyPeerClock(peers, L.table_from({"from": 1, "t": 2000, "ahead": 6, "safe": 2000, "pause": True}), 0)
        self.assertEqual(K.barrier(s, peers), 2000, "a pause stop is reached exactly")
        self.assertEqual(K.pace(s, peers, 1800, 99), 1)
        self.assertEqual(K.pace(s, peers, 2000, 99), 0)
        # an item stamped at the pause that ended applies while the game still sits there
        s = self.session(speed=2, pauseAt=4000)
        it = L.table_from({"at": 4000})
        self.assertTrue(K.applyNow(s, 4000, it))
        K.applySession(s, L.table_from({"started": True, "speed": 2, "lead": 600}))
        self.assertEqual(s.lastPauseAt, 4000)
        self.assertTrue(K.applyNow(s, 4000, it), "resumed but still at the pause time: applied as during the pause")
        self.assertFalse(K.applyNow(s, 4200, it), "past it: late, to the simulation (logged and repaired)")
        self.assertFalse(K.applyNow(s, 4000, L.table_from({"at": 4200})), "a later stamp goes to the simulation")


if __name__ == "__main__":
    unittest.main()
