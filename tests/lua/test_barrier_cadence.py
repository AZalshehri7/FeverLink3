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


def cover_ms(rtt_ms, gap_ms):
    """core Lockstep::cover_ms: the one-way time and the clock gap's excess over one engine batch's real time (at most 500 ms)."""
    excess = min(max(gap_ms - 1000.0 / 6.0, 0.0), 500.0)
    return max(rtt_ms, 0.0) / 2.0 + excess


def lead_for_cover(speed, cover):
    """core Lockstep::lead_for_cover, in Python (the relay plays the host)."""
    per_second = 6.0 * max(speed, 1)
    seconds = max(cover, 0.0) / 1000.0 + 0.06
    steps = math.ceil(seconds * per_second) + 1 + min(max(speed, 1), 4)   # the hand-off step and one batch of `speed` steps
    return STEP * min(max(steps, 3), 24)


def lead_for_measured(speed, rtt_ms, gap_ms):
    """core/src/session/lockstep.rs Lockstep::lead_for_measured, in Python (the relay plays the host)."""
    return lead_for_cover(speed, cover_ms(rtt_ms, gap_ms))


class JitterBuffer:
    """core JitterBuffer: the latest arrival plus 20 ms, at once when it is later, shrinking by 50 ms a second otherwise."""
    SAFETY_MS, SHRINK_MS_PER_SECOND = 20.0, 50.0

    def __init__(self):
        self.ms, self.at = 0.0, None

    def update(self, cover, now_s):
        target = max(cover, 0.0) + self.SAFETY_MS
        dt = 0.0 if self.at is None else max(0.0, now_s - self.at)
        self.at = now_s
        self.ms = target if target >= self.ms else max(target, self.ms - self.SHRINK_MS_PER_SECOND * dt)
        return self.ms


ROUND_MS = 1000.0 / 6 / 4   # four relay rounds make an engine tick, a tick is a sixth of a second of real time at any speed


class AdaptiveRelay(B.JitterRelay):
    """The jittery relay as the host with the adaptive lead (core's jitter buffer): the round trip is the link's largest of eight
    (this link model's 50 % of jitter: 1.5 times it), the clock gaps are measured from the clock lines it relays (only lines of a
    running game, as LeadMeter; the largest in ten seconds), the lead covers them at once and shrinks by 50 ms a second."""

    def __init__(self, games, rng, delay, rtt_ms):
        super().__init__(games, rng, delay)
        self.rtt_ms = rtt_ms
        self.lead_units = lead_for_measured(1, rtt_ms, 0.0)
        self.last = {}
        self.gaps = {g.id: [] for g in games}
        self.buffer = JitterBuffer()
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
                gap = max(gap, max(g for _, g in xs))
        ms = self.buffer.update(cover_ms(self.rtt_ms * 1.5, gap), self.round * ROUND_MS / 1000.0)
        want = lead_for_cover(max(self.speed, 1), ms)
        if want != self.lead_units:
            self.lead_units = want
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


@T.own_tests_only   # the harness of the barrier tests, not their tests (they run in test_input_barrier.py, on the real clock)
class CadenceTest(B.BarrierTest):
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

    def test_the_company_page_keeps_its_frame_budget_gets_every_line_and_never_stalls_on_a_deleted_or_slow_one(self):
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
            WORLD[70000 + i] = { LINE = { stops = {} } }
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
        # The pre5 friend test (2026-10-18): companies= 450-610 ms in one frame, once per pass for every deleted line. A late-game
        # registry here: 40 live lines (0.2 ms a read), 6 deleted ones whose keys stay bound to their old entity (2 of those ids reused by
        # a vehicle), every engine read of an entity that is no line costs 550 ms like in the log, and one live line whose read is slow
        # by itself (500 ms). Over many passes no frame spends more than a few ms on the page, apart from the one read of the slow line.
        self.make(2, 3, lambda rng, dest: 1)
        self.start(speed=1)
        host = self.games[0]
        host.L.execute(r'''
          local G = FIND_UP("G")
          LIVE_SUM = 0
          for i = 1, 40 do
            local key = "0:" .. (8000 + i)
            G.co.made[key] = { kind = "LINE", by = 0 }
            G.bind[key] = 60000 + i
            WORLD[60000 + i] = { LINE = { stops = {} } }
            if i ~= 7 then LIVE_SUM = LIVE_SUM + i end
          end
          for i = 1, 6 do
            local key = "1:" .. (8100 + i)
            G.co.made[key] = { kind = "LINE", by = 0 }
            G.bind[key] = 61000 + i
            if i <= 2 then WORLD[61000 + i] = { TRANSPORT_VEHICLE = {} } end
          end
          SLOW_LINE, PAGE_COST, BAD_READS, SLOW_READS = 60007, 0, 0, 0
          GUI_API.engine.util.finance = { calculateBalance = function(list, a, b)
            local e = list[1]
            local cost = 0.0002
            if not (WORLD[e] and WORLD[e].LINE) then cost, BAD_READS = 0.55, BAD_READS + 1
            elseif e == SLOW_LINE then cost, SLOW_READS = 0.5, SLOW_READS + 1 end
            FAKE_CLOCK = FAKE_CLOCK + cost
            PAGE_COST = PAGE_COST + cost
            return e - 60000
          end }
        ''')
        costs = []
        for _ in range(4000):
            host.L.execute("PAGE_COST = 0")
            host.frame()
            costs.append(float(host.L.eval("PAGE_COST")))
            host.L.execute("FAKE_CLOCK = FAKE_CLOCK + 0.016")
            if int(host.L.eval("FIND_UP('X').coPage and FIND_UP('X').coPage.pass or 0")) >= 6:
                break
        self.assertGreaterEqual(int(host.L.eval("FIND_UP('X').coPage.pass")), 6, "six passes ran")
        self.assertEqual(int(host.L.eval("BAD_READS")), 0, "the entity of a deleted line is never read")
        self.assertEqual(int(host.L.eval("SLOW_READS")), 1, "a line whose one read is slow is read once, not every pass")
        over = [c for c in costs if c > 0.003]
        self.assertEqual(len(over), 1, "one frame paid the slow line's read, no other frame more than 3 ms: %s" % over)
        self.assertLessEqual(max(c for c in costs if c != over[0]), 0.003)
        self.assertIn("(entity 60007) took 500 ms in one engine call", host.log())
        text = T.read_text(os.path.join(host.dir, "companies.txt")).replace(" ", "")
        self.assertIn('["earned"]=%d' % (int(host.L.eval("LIVE_SUM")) + 7), text, "every live line counted (the slow one with the result it had)")

    # ---- the clock line of a game that stands still: a real-time cadence

    def clocks_in(self, g):
        return sum(1 for line in T.read_text(g.from_game).splitlines() if line.startswith("clock\t"))

    def test_a_standing_game_sends_its_clock_on_a_real_time_cadence_and_a_build_stamp_takes_the_later_of_promise_and_margin(self):
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
        # a build's stamp: the later of the promise and the measured margin, not both
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
        # one's run-on passing the second's stamp, until the flush carried its batch marker, pauseUpto). Seeds 22 and 29 by default (the
        # first breach and the pauseUpto one); 31, 33, 35 and a sweep of further seeds in the full set.
        seeds = (22, 29) + ((31, 33, 35) + tuple(range(36, 48)) if T.FULL else ())
        for seed in seeds:
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

    def test_the_adaptive_lead_converges_and_never_applies_late_over_150_ms_and_in_the_full_set_30_and_300_ms(self):
        for rtt, seed in ((150, 32),) + (((30, 31), (300, 33)) if T.FULL else ()):
            with self.subTest(rtt=rtt):
                leads = self.adaptive_run(rtt, seed, speeds=[1, 2, 4])
                for lead in leads:
                    self.assertTrue(3 * STEP <= lead <= 24 * STEP, lead)
                tail = leads[-60:]
                speed = max(self.relay.speed, 1)
                # a jitter buffer (core JitterBuffer): it grows at once on a late arrival and shrinks gradually, a step at a time, within
                # what the link's latest arrivals ask for
                for a, b in zip(tail, tail[1:]):
                    self.assertLessEqual(a - b, STEP, "the lead shrinks a step at a time (%s)" % tail)
                most = lead_for_cover(speed, cover_ms(rtt * 1.5, 1000.0 / 6 + 500.0) + JitterBuffer.SAFETY_MS)
                self.assertLessEqual(max(tail), most, "bounded by the largest arrival it covers (%s)" % tail)
                self.assertGreaterEqual(min(tail), lead_for_measured(speed, rtt, 0.0) - STEP)


if __name__ == "__main__":
    unittest.main()
