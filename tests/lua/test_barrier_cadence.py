"""Bugs 4 and 6 (owner list 2026-10-17): the input barrier's stand-stills and the build lag.

  * the company page reads the lines' results in passes with a budget per frame (a frame that stalls sends no clock line, and every
    peer waits at its barrier for it): never more than the budget's worth of engine reads in one frame, and the page still gets every
    line's result;
  * a game that stands still sends its clock line on a real-time cadence (X.CLOCK_KEEP_S), not every 30 frames: at 2 frames a second
    a line every frame, at a thousand no more than before;
  * a build every game applies at its stamp is stamped at the later of its promise and the fastest clock plus the lead plus the
    measured margin, not at the promise plus the margin, and the extra it reports is the one it really has;
  * the host's adaptive lead (core: Lockstep::lead_for_measured, mirrored here from the round trip and the clock gaps the relay
    measures) converges, and two games over links of 30, 150 and 300 ms round trip with jitter never apply an item late.

The two mocked games are the harness of test_lockstep.py and the jittery relay of test_input_barrier.py (imported, never edited).

Run:  python -I tests/lua/test_barrier_cadence.py
"""
import math
import os
import random
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)  # python -I does not put the script's folder on the path

import test_input_barrier as B  # noqa: E402
import test_lockstep as T  # noqa: E402

STEP = T.STEP

# The bridge keeps its state in chunk locals; the tests reach them through the upvalues of its entry points.
FIND_UP = r'''
function FIND_UP(name)
  local seen = {}
  local function walk(f, d)
    if seen[f] or d > 8 then return nil end
    seen[f] = true
    local i = 1
    while true do
      local n, v = debug.getupvalue(f, i)
      if n == nil then break end
      if n == name then return v end
      if type(v) == "function" then
        local r = walk(v, d + 1)
        if r ~= nil then return r end
      end
      i = i + 1
    end
    return nil
  end
  return walk(GUI.guiUpdate, 0)
end
-- a clock the test moves (os.clock is the bridge's real-time clock)
FAKE_CLOCK = 1000
os.clock = function() return FAKE_CLOCK end
'''


def lead_for_measured(speed, rtt_ms, gap_ms):
    """core/src/session/lockstep.rs Lockstep::lead_for_measured, in Python (the relay plays the host)."""
    per_second = 6.0 * max(speed, 1)
    excess = min(max(gap_ms - 1000.0 / per_second, 0.0), 500.0)
    seconds = rtt_ms / 2000.0 + 0.06 + excess / 1000.0
    steps = math.ceil(seconds * per_second) + 2
    return STEP * min(max(steps, 3), 24)


ROUND_MS = 1000.0 / 6 / 4   # four relay rounds make an engine tick, a tick is a sixth of a second of real time at any speed


class AdaptiveRelay(B.JitterRelay):
    """The jittery relay as the host with the adaptive lead: the round trip is the link's (with its jitter's 90 % quantile), the
    clock gaps are measured from the clock lines it relays (only lines of a running game, as LeadMeter), the lead grows at once and
    shrinks after eight seconds."""

    def __init__(self, games, rng, delay, rtt_ms):
        super().__init__(games, rng, delay)
        self.rtt_ms = rtt_ms
        self.lead_units = lead_for_measured(1, rtt_ms, 0.0)
        self.last = {}
        self.gaps = {g.id: [] for g in games}
        self.lower_since = None
        self.leads = []

    @property
    def lead(self):
        return self.lead_units

    def pump(self):
        before = dict(self.clocks)
        super().pump()
        for gid, t in self.clocks.items():
            prev = self.last.get(gid)
            moved = before.get(gid) is not None and t > before[gid]
            if prev is not None and moved and prev[1]:
                self.gaps[gid].append((self.round, (self.round - prev[0]) * ROUND_MS))
            if before.get(gid) != t or prev is None:
                self.last[gid] = (self.round, moved)
        for gid in self.gaps:
            self.gaps[gid] = [x for x in self.gaps[gid] if self.round - x[0] <= 10000 / ROUND_MS][-512:]

    def adapt(self):
        gap = 0.0
        for xs in self.gaps.values():
            if len(xs) >= 20:
                v = sorted(g for _, g in xs)
                gap = max(gap, v[round((len(v) - 1) * 0.95)])
        want = lead_for_measured(max(self.speed, 1), self.rtt_ms * 1.27, gap)
        changed = False
        if want > self.lead_units:
            self.lead_units, self.lower_since, changed = want, None, True
        elif want < self.lead_units:
            self.lower_since = self.round if self.lower_since is None else self.lower_since
            if (self.round - self.lower_since) * ROUND_MS >= 8000:
                self.lead_units, self.lower_since, changed = want, None, True
        else:
            self.lower_since = None
        if changed:
            self.session()
        self.leads.append(self.lead_units)


def link(rtt_ms):
    """One-way delay in relay rounds for a link of that round trip: half of it, 50 % of jitter, and now and then a spike of four
    times the round trip."""
    one_way = rtt_ms / 2.0 / ROUND_MS

    def delay(rng, dest):
        d = one_way * (1.0 + 0.5 * rng.random())
        if rng.random() < 0.02:
            d += 4 * rtt_ms / ROUND_MS * rng.random()
        return int(round(d))
    return delay


class CadenceTest(B.BarrierTest):
    # the harness of the barrier tests, not their tests (they run in test_input_barrier.py, on the real clock)
    test_two_games_with_jitter_never_apply_late = None
    test_three_games_with_jitter_never_apply_late = None
    test_pauses_and_resumes_with_jitter_never_apply_late = None
    test_without_the_promises_the_same_network_applies_late = None
    test_a_silent_peer_holds_the_barrier_and_its_leaving_releases_it = None

    def make(self, n, seed, delay, lead_steps=None, rtt_ms=None):
        super().make(n, seed, delay, lead_steps)
        if rtt_ms is not None:
            self.relay = AdaptiveRelay(self.games, self.rng, delay, rtt_ms)
        for g in self.games:
            g.L.execute(FIND_UP)

    def wall(self, seconds):
        for g in self.games:
            g.L.execute("FAKE_CLOCK = FAKE_CLOCK + %r" % seconds)

    def tick(self, n=1):
        for _ in range(n):
            self.wall(1.0 / 6)
            super().tick(1)
            if isinstance(self.relay, AdaptiveRelay):
                self.relay.adapt()

    # ---- the company page: a budget per frame

    def test_the_company_page_reads_lines_within_its_frame_budget_and_gets_them_all(self):
        self.make(2, 3, lambda rng, dest: 1)
        self.start(speed=1)
        host = self.games[0]
        n_lines = 300
        # 300 lines of the host's player in the interface's registry mirror, each bound to an entity whose result is its index; every
        # engine read of a line's result costs a millisecond of the frame
        host.L.execute(r'''
          local G = FIND_UP("G")
          for i = 1, %d do
            local key = "0:" .. (9000 + i)
            G.co.made[key] = { kind = "LINE", by = 0 }
            G.bind[key] = 70000 + i
          end
          READS, FRAME_READS = 0, 0
          GUI_API.engine.util.finance = { calculateBalance = function(list, a, b)
            READS = READS + 1
            FAKE_CLOCK = FAKE_CLOCK + 0.001
            return list[1] - 70000
          end }
        ''' % n_lines)
        worst, total = 0, 0
        for _ in range(3000):
            before = int(host.L.eval("READS"))
            host.frame()
            reads = int(host.L.eval("READS")) - before
            worst, total = max(worst, reads), total + reads
            host.L.execute("FAKE_CLOCK = FAKE_CLOCK + 0.016")
            st = host.L.eval("(function() local P = FIND_UP('X').coPage; return P and P.pass or 0 end)()")
            if int(st) >= 1:
                break
        budget = float(host.L.eval("FIND_UP('X').CO_PAGE_BUDGET_S"))
        self.assertLessEqual(worst, int(budget / 0.001) + 1, "never more than the budget's worth of reads in one frame")
        self.assertGreaterEqual(total, n_lines, "every line was read")
        text = T.read_text(os.path.join(host.dir, "companies.txt"))
        self.assertIn('["earned"]=%d' % (n_lines * (n_lines + 1) // 2), text.replace(" ", ""), "the page has the sum of every line's result")
        # the next pass waits for its period: no reads in between
        before = int(host.L.eval("READS"))
        for _ in range(30):
            host.frame()
            host.L.execute("FAKE_CLOCK = FAKE_CLOCK + 0.016")
        self.assertEqual(int(host.L.eval("READS")), before, "no read before the next pass is due")
        host.L.execute("FAKE_CLOCK = FAKE_CLOCK + 6")
        for _ in range(9):
            host.frame()
        self.assertGreater(int(host.L.eval("READS")), before, "a pass every few seconds")

    # ---- the clock line of a game that stands still: a real-time cadence

    def clocks_in(self, g):
        return sum(1 for line in T.read_text(g.from_game).splitlines() if line.startswith("clock\t"))

    def test_a_game_standing_still_sends_its_clock_on_a_real_time_cadence_at_any_frame_rate(self):
        self.make(2, 7, lambda rng, dest: 1)
        self.start(speed=2)
        host, client = self.games
        self.silent.add(host.id)   # the host's game stops: the client stands at the barrier, its clock does not move
        self.tick(30)
        self.assertEqual(client.speed(), 0)
        t = client.time()
        # 2 frames a second: a line every frame (it was one in 30 frames, 15 s)
        before = self.clocks_in(client)
        for _ in range(20):
            client.L.execute("FAKE_CLOCK = FAKE_CLOCK + 0.5")
            client.frame()
        self.assertEqual(client.time(), t, "still standing")
        self.assertGreaterEqual(self.clocks_in(client) - before, 19, "a clock line every 0.25 s of real time")
        # a thousand frames a second: the 30 frames of before still come first, the real-time cadence adds a few lines at most
        before = self.clocks_in(client)
        for _ in range(1000):
            client.L.execute("FAKE_CLOCK = FAKE_CLOCK + 0.001")
            client.frame()
        self.assertLessEqual(self.clocks_in(client) - before, 1000 // 30 + 5)
        self.assertGreaterEqual(self.clocks_in(client) - before, 1000 // 30)

    # ---- a build's stamp: the later of the promise and the measured margin, not both

    def test_a_build_stamp_takes_the_later_of_the_promise_and_the_margin(self):
        self.make(2, 9, lambda rng, dest: 1)
        self.start(speed=1)
        host = self.games[0]
        r = host.L.eval(r'''(function()
          local G, saf = FIND_UP("G"), FIND_UP("stampAfterFastest")
          local now = GAME.time
          local plain = saf(now)
          local at1, xs1 = saf(now, nil, 1)
          local at6, xs6 = saf(now, nil, 9)
          local fastest = now
          for _, pc in pairs(G.peers) do if pc.t > fastest then fastest = pc.t end end
          return now, fastest, plain, at1, xs1, at6, xs6, G.session.lead, G.promiseFloor
        end)()''')
        now, fastest, plain, at1, xs1, at9, xs9, lead, floor = r
        by_lead = -(-(fastest + lead) // STEP) * STEP
        self.assertEqual(plain, max(by_lead, floor), "the promise or the fastest clock plus the lead")
        self.assertEqual(at1, max(by_lead + STEP, floor), "a margin under the promise adds nothing")
        self.assertEqual(xs1, (at1 - by_lead) // STEP, "the extra reported is the one the stamp has")
        self.assertEqual(at9, max(by_lead + 9 * STEP, floor))
        self.assertEqual(xs9, 9)
        self.assertGreaterEqual(at1, floor, "never before the promise (the input barrier)")

    # ---- the pause order after the resume (found with the real-time clock above, 2026-10-18)

    def test_actions_of_a_pause_stamped_after_the_resume_never_land_behind_a_game(self):
        # The barrier runs of test_input_barrier.py with the interface's real-time clock moving with the game: these seeds breached the
        # barrier before (an action of the pause stamped after the resume behind the step every game had run on to; a later one behind
        # the run-on of an earlier one stamped after the resume; seed 29: two actions of one flush read in different frames, the first
        # one's run-on passing the second's stamp, until the flush carried its batch marker, pauseUpto), and a sweep of further seeds.
        for seed in (22, 29, 31, 33, 35) + tuple(range(36, 48)):
            with self.subTest(seed=seed):
                self.random_run(2, seed, 280, lead_choices=[1, 3], speeds=[1, 2, 4], pauses=True)

    # ---- the adaptive lead over three links

    def adaptive_run(self, rtt_ms, seed, speeds, ticks=320):
        self.make(2, seed, link(rtt_ms), rtt_ms=rtt_ms)
        self.start(speed=self.rng.choice(speeds))
        for g in self.games:
            self.lag[g.id] = self.rng.choice([0.0, 0.1])
        issued = 0
        for i in range(ticks):
            if self.rng.random() < 0.15:
                self.rng.choice(self.games).send_gui(self.rng.choice([B.BUY, B.LINE]))
                issued += 1
            if i % 100 == 99:
                self.relay.speed = self.rng.choice(speeds)
                self.relay.session()
            self.tick(1)
        self.settle()
        self.assert_no_late_and_same(issued)
        return self.relay.leads

    def test_the_adaptive_lead_converges_and_never_applies_late_over_30_150_and_300_ms(self):
        for rtt, seed in ((30, 31), (150, 32), (300, 33)):
            with self.subTest(rtt=rtt):
                leads = self.adaptive_run(rtt, seed, speeds=[1, 2, 4])
                for lead in leads:
                    self.assertTrue(3 * STEP <= lead <= 24 * STEP, lead)
                tail = leads[-60:]
                self.assertLessEqual(len(set(tail)), 2, "the lead settles (%s)" % sorted(set(tail)))
                speed = max(self.relay.speed, 1)
                self.assertGreaterEqual(min(tail), lead_for_measured(speed, rtt, 0.0) - STEP)


if __name__ == "__main__":
    unittest.main()
