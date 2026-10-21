"""The stall timeout (core host/stall.rs, the mod's L.isStalled / L.catchUp / X.hostStamps): a game that stops answering no longer holds
the others at their input barriers; they run on, it catches up later, and every action is still applied at one stamp on every game.

The real case: the owner's notes of 2026-10-17 (a clone froze one game for 7-10 s and held the other with it) and the friend test of
2026-10-18 (x4 over a 30 ms round trip). The mocked games of test_lockstep.py run the real mod; the relay is the jittery one of
test_input_barrier.py with the host's stall rules on top, as core implements them (a test relay, like the rest of this harness):
  * a game whose clock lines have not come for TIMEOUT rounds while another's still come is stalled: the session lists it, the relay keeps
    a promise for it that follows the others' promises (a peer clock flagged `stalled`), and the others stop pacing themselves to it;
  * its actions marked for the host's stamp (`pause`) get the running stamp past every promise and go to every game, the sender too; one
    stamped before the promise kept for it is refused on every game (`build_veto` with `host`), never relayed;
  * the stall ends once its own promise has reached the one kept for it (which stops following the others once it is within a lead).
Checked: no LATE apply, no breach, no repair; every game applied the same items in the same order at the same game times; the game that
stood still is told it is catching up (its window) and runs batches of up to 4 steps until it has.

Run:  python -I tests/lua/test_stall.py
"""
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)  # python -I does not put the script's folder on the path

import test_barrier_cadence as BC  # noqa: E402
import test_input_barrier as B  # noqa: E402
import test_lockstep as T  # noqa: E402

STEP = T.STEP


class StallRelay(B.JitterRelay):
    """The jittery relay with core's stall rules (host/stall.rs). Time is relay rounds (four make an engine tick)."""

    TIMEOUT = 48   # rounds: twelve engine ticks, two seconds at the mock's six ticks a second (core: 3 s)

    def __init__(self, games, rng, delay, lead_steps=None):
        super().__init__(games, rng, delay, lead_steps)
        self.stall_init()

    def stall_init(self):
        self.stalls, self.heard, self.vetoed, self.events = {}, {}, set(), []
        self.proxy_lines = []

    # ---- the session line carries the stalled games
    def session(self):
        d = {"started": self.started, "speed": self.speed, "lead": self.lead}
        if self.pauseAt is not None:
            d["pauseAt"] = self.pauseAt
        if self.stalls:
            d["stalled"] = sorted(self.stalls)
        if self.pauseAt != self.pause_of:
            if self.pauseAt is not None:
                self.pause_last = None
            self.pause_of = self.pauseAt
        T.Relay.broadcast(self, "session", self.ser(d))
        self.flush_pause()

    def future_stamp(self):
        return max([T.Relay.future_stamp(self)] + [s["proxy"] for s in self.stalls.values()])

    def target(self):
        vals = [v for gid, v in self.safes.items() if gid not in self.stalls]
        return max(vals) if vals else None

    def send_proxy(self, gid):
        s = self.stalls[gid]
        pc = {"from": gid, "t": s["t"], "ahead": s["ahead"], "safe": s["proxy"], "stalled": True}
        self.proxy_lines.append((gid, s["proxy"]))
        T.Relay.broadcast(self, "peerclock", self.ser(pc), exclude=self.game(gid))

    def game(self, gid):
        return next(g for g in self.games if g.id == gid)

    def broadcast(self, kind, payload, exclude=None):
        if kind != "peerclock":
            return T.Relay.broadcast(self, kind, payload, exclude)
        p = self.C.parse(payload)
        p = p[0] if isinstance(p, tuple) else p
        gid = int(p["from"])
        self.heard[gid] = self.round
        if gid not in self.stalls:
            T.Relay.broadcast(self, kind, payload, exclude)
            self.follow()
            return
        # a stalled game's own line: the relay passes its clock on in the proxy, until its own promise reaches the proxy
        s = self.stalls[gid]
        if s["back"] is None and int(p.t) != s["t"]:
            s["back"] = self.round
            self.events.append(("back", gid, int(p.t)))
        s["t"], s["ahead"] = int(p.t), int(p.ahead)
        others = [self.clocks[o] for o in self.clocks if o not in self.stalls]
        if s["back"] is not None and others and max(others) - s["t"] <= self.lead:
            s["caught"] = True
        if p.safe is not None and int(p.safe) >= s["proxy"]:
            del self.stalls[gid]
            self.events.append(("caught", gid, int(p.t)))
            self.session()
            T.Relay.broadcast(self, kind, payload, exclude)
            return
        self.send_proxy(gid)

    def follow(self):
        t = self.target()
        if t is None:
            return
        for gid, s in self.stalls.items():
            if not s["caught"] and t > s["proxy"]:
                s["proxy"] = t
                self.send_proxy(gid)

    def send(self, g, kind, frm, payload):
        if kind == "action" and int(frm) in self.stalls:
            p = self.C.parse(payload)
            p = p[0] if isinstance(p, tuple) else p
            if p.pauseSeq is None and int(p.stamp) < self.stalls[int(frm)]["proxy"]:
                key = (int(frm), int(p.seq))
                if key not in self.vetoed:
                    self.vetoed.add(key)
                    self.events.append(("refused", int(frm), int(p.seq)))
                    veto = self.ser({"seq": int(p.seq), "owner": int(frm), "why": "stamped while its game was stalled", "from": 0,
                                     "name": "FeverLink3", "host": True})
                    T.Relay.broadcast(self, "build_veto", veto)
                return
        B.JitterRelay.send(self, g, kind, frm, payload)

    def deliver(self):
        B.JitterRelay.deliver(self)
        self.watch_stalls()

    def watch_stalls(self):
        """core watch_stalls: `self.round` is the relay's time (rounds here, milliseconds on the measured link)."""
        if not self.started or self.pauseAt is not None:
            return
        heard = {g.id: self.heard.get(g.id, self.round) for g in self.games}
        for g in self.games:
            self.heard.setdefault(g.id, self.round)
        fresh = [gid for gid, r in heard.items() if self.round - r < self.TIMEOUT and gid not in self.stalls]
        for gid, r in heard.items():
            if gid in self.stalls or self.round - r < self.TIMEOUT or not fresh:
                continue
            t = self.clocks.get(gid, 0)
            proxy = max([self.safes.get(gid, t)] + ([self.target()] if self.target() is not None else []))
            self.stalls[gid] = {"t": t, "ahead": self.aheads.get(gid, 3), "proxy": proxy, "back": None, "caught": False}
            self.events.append(("stalled", gid, t))
            self.session()
            self.send_proxy(gid)
        for gid, s in self.stalls.items():
            if self.round - heard[gid] >= self.TIMEOUT:
                s["back"], s["caught"] = None, False


class StallTest(B.BarrierTest):
    def make(self, n, seed, delay, lead_steps=None):
        super().make(n, seed, delay, lead_steps)
        self.relay = StallRelay(self.games, self.rng, delay, lead_steps)
        for g in self.games:
            g.L.execute(BC.FIND_UP)

    def status(self, g):
        return str(g.L.eval("(function() local G, Hud = FIND_UP('G'), FIND_UP('Hud'); return Hud.statusText(G.hud) end)()"))

    def freeze(self, g, ticks, act_every=0, acting=()):
        """`g` stands still for `ticks` engine ticks (no frame, no step: an autosave); the others act every `act_every` ticks."""
        self.silent.add(g.id)
        issued = 0
        statuses = []
        for i in range(ticks):
            if act_every and i % act_every == act_every - 1:
                for o in acting:
                    o.send_gui(B.BUY if i % 2 else B.LINE)
                    issued += 1
            self.tick(1)
            for o in self.games:
                if o.id not in self.silent:
                    statuses.append(self.status(o))
        self.silent.discard(g.id)
        return issued, statuses

    # ---- x2 with a small jitter; the guest freezes for about seven seconds of play. (x4, where only a shorter batch interval can
    # catch up, runs on the measured link of test_pacing_measured.py: this harness has no real time.)

    def test_a_frozen_guest_or_host_no_longer_holds_the_other_which_catches_up_and_an_action_held_past_the_stall_is_refused(self):
        self.make(2, 31, B.jitter(spike_p=0.01, spike=(8, 16), base=(0, 3)))
        self.start(speed=2)
        host, guest = self.games
        issued = 0
        for i in range(30):
            if i % 7 == 3:
                self.rng.choice(self.games).send_gui(B.BUY)
                issued += 1
            self.tick(1)
        t0 = host.time()
        n, statuses = self.freeze(guest, 42, act_every=6, acting=(host,))
        issued += n
        ran = (host.time() - t0) // STEP
        self.assertGreater(ran, 50, "the host ran on (it stood at the barrier only until the timeout): %d steps" % ran)
        self.assertTrue(any(e[0] == "stalled" and e[1] == guest.id for e in self.relay.events), self.relay.events)
        self.assertTrue(any("stood still: running on" in s for s in statuses), statuses[-3:])
        behind = (host.time() - guest.time()) // STEP
        self.assertGreater(behind, 40, "the guest is behind: %d steps" % behind)
        # back: it catches up (batches of up to 4 steps) and its window says so; it acts while catching up (stamped by the host)
        seen_catching = False
        for i in range(120):
            if i in (2, 9):
                guest.send_gui(B.LINE if i == 2 else B.BUY)
                issued += 1
            if i == 5:
                host.send_gui(B.BUY)
                issued += 1
            self.tick(1)
            seen_catching = seen_catching or "catching up" in self.status(guest)
            if not self.relay.stalls and i > 12:
                break
        self.assertTrue(seen_catching, "the stalled game's window said it was catching up")
        self.assertFalse(self.relay.stalls, "caught up: %r" % self.relay.events)
        self.assertIn("stall: the host has this game as stalled", guest.log())
        self.assertIn("stall: caught up", guest.log())
        self.assertIn("made while this game catches up", guest.log(), "its actions went to the host for their stamp")
        self.settle()
        self.assert_no_late_and_same(issued)
        # the host game frozen is the same for the guest
        self.make(2, 33, B.jitter(spike_p=0.0, base=(0, 2)))
        self.start(speed=1)
        host, guest = self.games
        self.tick(10)
        t0 = guest.time()
        n, _ = self.freeze(host, 40, act_every=5, acting=(guest,))
        self.assertGreater((guest.time() - t0) // STEP, 15, "the guest ran on at x1")
        for i in range(150):
            if i == 3:
                host.send_gui(B.BUY)
                n += 1
            self.tick(1)
            if not self.relay.stalls and i > 5:
                break
        self.assertFalse(self.relay.stalls)
        self.settle()
        self.assert_no_late_and_same(n)
        # an action shipped before the stall was known is refused on every game
        # the guest's game ships an action, then stands still; the network holds the action up past the stall (a link spike of the
        # internet): stamped before the promise the host then keeps for the guest, it is refused on every game, its sender's too
        self.make(2, 32, lambda rng, dest: 1)
        self.start(speed=2)
        host, guest = self.games
        self.tick(10)
        guest.send_gui(B.BUY)
        guest.frame()   # shipped to its from_game.log
        # the line sits on the guest's link (its launcher could not send) while the game stands still
        held = self.relay.readers[guest.id]
        self.relay.readers[guest.id] = self.relay.C.newReader(os.path.join(self.tmp, "nothing.log"))
        self.silent.add(guest.id)
        self.tick(30)
        self.assertIn(guest.id, self.relay.stalls)
        self.relay.readers[guest.id] = held
        self.silent.discard(guest.id)
        self.tick(80)
        self.settle()
        self.assertTrue(any(e[0] == "refused" for e in self.relay.events), self.relay.events)
        self.assertIn("refused by the host on every game", guest.log())
        self.assert_no_late_and_same(0)

# the harness's own tests run in their own file
for _name in dir(B.BarrierTest):
    if _name.startswith("test_") and _name not in StallTest.__dict__:
        setattr(StallTest, _name, None)


class PureStallTest(unittest.TestCase):
    def setUp(self):
        os.environ.pop("FEVERLINK3_SESSION_DIR", None)
        self.L = T.lupa.LuaRuntime(unpack_returned_tuples=True)
        T.load_common(self.L)
        self.K = self.L.execute(T.LOCKSTEP_SRC)

    def test_a_stalled_peer_bounds_the_barrier_but_not_the_pacing_and_a_game_far_behind_runs_batches_of_four(self):
        K, L = self.K, self.L
        s = K.newSession()
        K.applySession(s, L.table_from({"started": True, "speed": 1, "lead": 600, "stalled": L.table_from([1])}))
        self.assertTrue(K.isStalled(s, 1))
        self.assertFalse(K.isStalled(s, 0))
        peers = L.table()
        K.applyPeerClock(peers, L.table_from({"from": 1, "t": 1000, "ahead": 3, "safe": 9000, "stalled": True}), 0)
        self.assertIsNone(K.slowestPeer(peers), "a stalled peer is not waited for")
        self.assertEqual(K.barrier(s, peers), 9000 - 3 * STEP, "its promise (the host's) still bounds this game")
        self.assertEqual(K.pace(s, peers, 5000, 3), 1, "this game runs at the session's speed")
        # the other side: this game is far behind the furthest peer that runs
        s2 = K.newSession()
        K.applySession(s2, L.table_from({"started": True, "speed": 1, "lead": 600}))
        self.assertIsNone(K.catchUp(s2, L.table_from({}), 10000))
        s2.self = 1
        K.applySession(s2, L.table_from({"started": True, "speed": 1, "lead": 600, "stalled": L.table_from([1])}))
        peers2 = L.table()
        K.applyPeerClock(peers2, L.table_from({"from": 0, "t": 40000, "ahead": 3, "safe": 41600}), 1)
        self.assertIsNotNone(K.catchUp(s2, peers2, 10000))
        self.assertEqual(K.pace(s2, peers2, 10000, 3), 4, "catching up: batches of 4 steps at x1")
        self.assertEqual(K.pace(s2, peers2, 40400, 3), 1, "near the barrier: the session's speed")
        self.assertIsNone(K.catchUp(s2, peers2, 40200), "ahead of it: no catch-up")
        K.applySession(s2, L.table_from({"started": True, "speed": 1, "lead": 600}))
        self.assertIsNone(K.catchUp(s2, peers2, 10000), "not stalled (two games in step see each other behind): no catch-up")


if __name__ == "__main__":
    unittest.main()
