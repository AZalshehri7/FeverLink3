"""Bug 4 again: smooth pacing over the link of the friend test of 0.2.0-pre5 (2026-10-18, x4, 30 ms round trip).

That test's link.minute lines showed the pacing slowing both games almost all the time on a fast link (host 23:02: pace_mean=1.58
pace_max=2.00 paced_s=60.0 rtt_ms=26/30/71 clock_gap_ms=0/125/825 stood_s=1.9 holds=14; guest 23:06-23:08: stood_s=6.1-7.3,
holds=23-24, pace_mean=1.58). The barrier hold lines said why: `room_steps=0.0`, `behind_steps=4.0`, the barrier exactly the peer's
clock plus 4 steps, which at x4 is ONE engine batch (the engine runs `speed` steps at once, a batch every ~200 ms). The host's lead
was its floor: its clock-cadence meter kept a gap only when the line before a move had moved too, and a running game sends a line
with the same time between two batches (the 30-frame and real-time keepalives), so nearly no gap was kept (link.lead gap_ms=0), and
the floor counted the clock as moving one step a line. With one batch of room every game stood at its barrier after almost every
batch, the pacing then asked for its full factor 2 ("standing still already") for the batches after the release, which made the
peer late in turn: the two games slowed each other down.

The harness runs the two mocked games of test_lockstep.py in real time at a millisecond's resolution, as the real engine does: each
game's frames come every few milliseconds (the two PCs differ: 5-12 ms and 8-18 ms, now and then a stalled frame), its engine runs a
batch of `speed` steps once 200 ms times the factor the mod last wrote to its control file (`pace {permille}`, native pace.rs) have
passed, and the host's lead is made the way core/src/session/lockstep.rs makes it (LeadMeter over every clock line it relays, with
the round trip, lead_for_measured, longer at once, shorter after 8 s). The network delays every line by the measured one-way time and
its jitter. Measured per game: the stand-still seconds a minute (its mod holds the engine at speed 0 while the session runs), the
time-weighted mean of the factor (link.minute's pace_mean), the game time it advanced, and the gaps between the peer clocks it read.

Measured on this harness (2026-10-18; the code before the fix against after it; speed is the game time advanced against an engine that
is never held nor stretched):
  * x4, 30 ms: lead 4 -> 9 steps; stood_s/min host 7.2 -> 0.0, guest 6.4 -> 0.0; pace_mean 1.71/1.69 -> 1.01/1.00; speed 0.59 -> 0.97
    (the field's numbers: pace_mean 1.58, stood_s 2-7, a peer clock gap median of 140 ms; the harness: 137-145 ms);
  * the same with the friend test's frame stalls (full set): stood_s/min 5.5/17.9 -> 0.4/0.5, pace_mean 1.69/1.81 -> 1.02/1.06,
    speed 0.52 -> 0.89;
  * a guest whose engine batches every 260 ms: the host stands 57 s/min without pacing (it waits after nearly every batch); with it,
    before 9.6 s/min at pace_mean 1.87 and the slow guest slowed too (1.40), speed 0.54; after 1.6 s/min at 1.31, the guest at 1.00,
    speed 0.75 (its own pace);
  * x2, 30 ms (full set): unchanged, already fine before (lead 5).

Run:  python -I tests/lua/test_pacing_measured.py
"""
import os
import random
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)  # python -I does not put the script's folder on the path

import test_barrier_cadence as BC  # noqa: E402
import test_input_barrier as B  # noqa: E402
import test_lockstep as T  # noqa: E402
import test_smooth_pacing as SP  # noqa: E402

STEP = T.STEP
BATCH_MS = 200.0   # the engine's batch interval at factor 1 (pace_batch_ms 190-218 in the friend test)


def one_way(rtt_ms):
    """The delay of a line from one game to the other (ms): half the round trip and the hand-offs of both ends (e2e_ms 25-41 at a
    30 ms round trip in the friend test), jitter, and now and then a spike."""
    def delay(rng, dest):
        d = rtt_ms / 2.0 + 8.0 + 12.0 * rng.random()
        if rng.random() < 0.02:
            d += 2.0 * rtt_ms + 60.0 * rng.random()
        return d
    return delay


class MeasuredRelay(B.JitterRelay):
    """The relay as the host with core's adaptive lead, on a clock in milliseconds (`round` is the time in ms)."""

    def __init__(self, games, rng, delay, rtt_ms):
        super().__init__(games, rng, delay)
        self.rtt_ms = rtt_ms
        self.lead_units = BC.lead_for_measured(1, rtt_ms, 0.0)
        self.meter = {g.id: {"last": None, "gaps": []} for g in games}
        self.lower_since = None
        self.leads = []
        self.peer_reads = {g.id: [] for g in games}

    @property
    def lead(self):
        return self.lead_units

    def clock_line(self, gid, t, speed, at_ms):
        """core LeadMeter::clock: the gaps between two moves of a running game's clock; a keepalive of a running engine between two
        batches is skipped, a line of a standing engine ends the run."""
        c = self.meter[gid]
        last, running = c["last"], speed > 0
        if last is not None and t > last[1]:
            if last[2] and at_ms >= last[0]:
                c["gaps"] = (c["gaps"] + [(at_ms, at_ms - last[0])])[-512:]
            c["last"] = (at_ms, t, running)
        elif last is not None and t == last[1]:
            c["last"] = (last[0], last[1], last[2] and running)
        else:
            c["last"] = (at_ms, t, False)

    def gap_ms(self, now_ms):
        gap = 0.0
        for c in self.meter.values():
            xs = sorted(v for at, v in c["gaps"] if at >= now_ms - 10000)
            if len(xs) >= 20:
                gap = max(gap, xs[int(round((len(xs) - 1) * 0.95))])
        return gap

    def adapt(self, now_ms):
        """host adapt_lead: the round trip's high quantile (30 % over the median) and the clock gaps; longer at once, shorter after 8 s."""
        want = BC.lead_for_measured(max(self.speed, 1), self.rtt_ms * 1.3, self.gap_ms(now_ms))
        if want > self.lead_units:
            self.lead_units, self.lower_since = want, None
            self.session()
        elif want < self.lead_units:
            self.lower_since = now_ms if self.lower_since is None else self.lower_since
            if now_ms - self.lower_since >= 8000:
                self.lead_units, self.lower_since = want, None
                self.session()
        else:
            self.lower_since = None
        self.leads.append(self.lead_units)

    def deliver_until(self, ms):
        self.round = ms
        for gid, q in self.out.items():
            while q and q[0][0] <= ms:
                _, g, kind, frm, payload = q.pop(0)
                if kind == "peerclock":
                    self.peer_reads[gid].append(ms)
                T.Relay.send(self, g, kind, frm, payload)

    def deliver(self):
        # the base harness's settle() ticks: a relay round is a millisecond here
        self.deliver_until(self.round + 1)


class MeasuredLinkTest(B.BarrierTest):
    def setup_link(self, seed, rtt_ms, frames, batch_ms=(BATCH_MS, BATCH_MS), stalls=((0, 0, 0), (0, 0, 0)), paced=True):
        """`frames`: per game (lo, hi) ms of a frame; `stalls`: per game (per second, lo ms, hi ms) of a stalled frame."""
        self.make(2, seed, lambda rng, dest: 1)
        self.relay = MeasuredRelay(self.games, self.rng, one_way(rtt_ms), rtt_ms)
        self.frames, self.batch_ms, self.stalls, self.paced = frames, batch_ms, stalls, paced
        self.now = 0.0
        ids = [g.id for g in self.games]
        self.next_frame = {gid: float(i) for i, gid in enumerate(ids)}
        self.last_frame = {gid: 0.0 for gid in ids}
        self.last_batch = {gid: 0.0 for gid in ids}
        self.held_prev = {gid: False for gid in ids}
        self.next_adapt = 250.0
        self.ctl_seen, self.factor_now = {}, {gid: 1.0 for gid in ids}
        self.measuring, self.issued = False, 0
        self.ran = {gid: 0.0 for gid in ids}
        self.stood = {gid: 0.0 for gid in ids}
        self.fsum = {gid: 0.0 for gid in ids}
        for g in self.games:
            g.L.execute(BC.FIND_UP)
            if paced:
                with open(os.path.join(g.dir, "native_events.log"), "w", encoding="utf-8", newline="") as f:
                    f.write(SP.READY)

    factor = SP.PacingTest.factor
    held = SP.PacingTest.held

    def frame_len(self, i):
        per_s, lo, hi = self.stalls[i]
        if per_s and self.rng.random() < per_s * self.frames[i][1] / 1000.0:
            return lo + (hi - lo) * self.rng.random()
        lo, hi = self.frames[i]
        return lo + (hi - lo) * self.rng.random()

    def frame(self, i, g):
        now = self.now
        g.L.execute("FAKE_CLOCK = %r" % (1000.0 + now / 1000.0))
        dt = now - self.last_frame[g.id]
        self.last_frame[g.id] = now
        running = self.relay.started and self.relay.speed > 0 and self.relay.pauseAt is None
        if self.measuring and running:
            self.ran[g.id] += dt
            self.fsum[g.id] += self.factor_now[g.id] * dt
            if self.held_prev[g.id]:
                self.stood[g.id] += dt
        g.frame()
        if float(g.L.eval("FIND_UP('X').clockSentAt or -1")) == 1000.0 + now / 1000.0:
            self.relay.clock_line(g.id, g.time(), g.speed(), now)
        # the engine: a batch once the interval (times the factor of the native module) has passed since the last one
        if now - self.last_batch[g.id] >= self.batch_ms[i]:
            f = self.factor(g) if self.paced else 1.0
            if now - self.last_batch[g.id] >= self.batch_ms[i] * f:
                t0 = g.time()
                g.step()
                if g.time() != t0:
                    self.last_batch[g.id] = now
        self.held_prev[g.id] = self.held(g)
        self.relay.round = now
        self.relay.pump()
        self.next_frame[g.id] = now + self.frame_len(i)

    def run_ms(self, ms):
        end = self.now + ms
        while True:
            i, g = min(enumerate(self.games), key=lambda x: self.next_frame[x[1].id])
            t = self.next_frame[g.id]
            if t > end:
                break
            self.relay.deliver_until(t)
            self.now = t
            if self.now >= self.next_adapt:
                self.relay.adapt(self.now)
                self.next_adapt += 250.0
            self.frame(i, g)
        self.now = end

    def start_link(self, speed):
        self.run_ms(1500)
        self.relay.started, self.relay.speed = True, speed
        self.relay.session()
        self.run_ms(3000)

    def measure(self, seconds, act_every_s=3.0):
        """`seconds` of play, an action from either game every `act_every_s` (the input barrier is tested along)."""
        t0 = {g.id: g.time() for g in self.games}
        self.measuring = True
        left = seconds * 1000.0
        while left > 0:
            chunk = min(left, act_every_s * 1000.0)
            self.run_ms(chunk)
            left -= chunk
            self.rng.choice(self.games).send_gui(self.rng.choice([B.BUY, B.LINE]))
            self.issued += 1
        self.measuring = False
        out = {}
        for g in self.games:
            ran = max(self.ran[g.id], 1.0)
            reads = self.relay.peer_reads[g.id]
            gaps = sorted(b - a for a, b in zip(reads, reads[1:]))
            out[g.label] = {
                "stood_s": self.stood[g.id] / ran * 60.0,
                "pace_mean": self.fsum[g.id] / ran,
                # against a game whose engine runs a batch of `speed` steps every BATCH_MS, never held nor stretched
                "speed": (g.time() - t0[g.id]) / STEP / (seconds * 1000.0 / BATCH_MS * self.relay.speed),
                "clock_gap_ms": (round(gaps[len(gaps) // 2]) if gaps else None, round(gaps[-1]) if gaps else None),
            }
        return out

    def line(self, what, r):
        text = "%s: lead %d steps; %s" % (what, self.relay.lead_units // STEP, "; ".join(
            "%s stood_s/min %.1f pace_mean %.2f speed %.2f clock_gap p50/max %s" % (k, v["stood_s"], v["pace_mean"], v["speed"], v["clock_gap_ms"])
            for k, v in r.items()))
        print("\n  " + text)
        return text

    def assert_same_world(self):
        self.relay.pump()
        for _ in range(400):
            self.run_ms(50)
            if not self.relay.in_flight():
                break
        self.run_ms(3000)
        self.assert_no_late_and_same(self.issued)

    # ---- the friend test's link: x4, 30 ms round trip, two PCs with different frame times

    FRIEND = dict(rtt_ms=30.0, frames=((5.0, 12.0), (8.0, 18.0)))

    def test_a_fast_link_runs_at_full_speed_and_does_not_stand(self):
        self.setup_link(81, **self.FRIEND)
        self.start_link(speed=4)
        r = self.measure(30)
        text = self.line("rtt 30 ms x4", r)
        for k, v in r.items():
            self.assertLess(v["pace_mean"], 1.06, "%s runs at full speed nearly always: %s" % (k, text))
            self.assertLess(v["stood_s"], 0.5, "%s does not stand: %s" % (k, text))
            self.assertGreater(v["speed"], 0.85, text)
        self.assertGreaterEqual(self.relay.lead_units, BC.lead_for_measured(4, 30.0, 0.0), "the lead holds a batch: " + text)
        self.assert_same_world()

    @T.full
    def test_a_fast_link_with_the_friend_tests_frame_stalls_and_actions(self):
        # the host's company page stalled a frame for 450-800 ms every few seconds in that test (frame.slow companies=440-760, the clock
        # gaps' maximum of 700-825 ms), the guest's for about 240 ms
        self.setup_link(82, stalls=((0.2, 450.0, 800.0), (0.2, 230.0, 260.0)), **self.FRIEND)
        self.start_link(speed=4)
        r = self.measure(60)
        text = self.line("rtt 30 ms x4 with stalls", r)
        for k, v in r.items():
            self.assertLess(v["pace_mean"], 1.15, "%s: %s" % (k, text))
            self.assertLess(v["stood_s"], 3.0, "%s: %s" % (k, text))
        self.assert_same_world()

    @T.full
    def test_a_fast_link_at_x2_runs_at_full_speed(self):
        self.setup_link(83, **self.FRIEND)
        self.start_link(speed=2)
        r = self.measure(30)
        text = self.line("rtt 30 ms x2", r)
        for k, v in r.items():
            self.assertLess(v["pace_mean"], 1.06, "%s: %s" % (k, text))
            self.assertLess(v["stood_s"], 0.5, "%s: %s" % (k, text))

    # ---- a genuinely slow peer: its engine's batches come every 260 ms (it cannot keep x4); the pacing still helps

    def slow_peer(self, paced):
        self.setup_link(84, rtt_ms=30.0, frames=((5.0, 12.0), (8.0, 18.0)), batch_ms=(BATCH_MS, 260.0), paced=paced)
        self.start_link(speed=4)
        return self.measure(30)

    def test_a_genuinely_slow_peer_is_followed_by_slowing_down_not_by_standing_still(self):
        before = self.slow_peer(paced=False)
        t_before = self.line("slow peer, no pacing", before)
        self.tearDown()
        self.setUp()
        after = self.slow_peer(paced=True)
        t_after = self.line("slow peer, smooth pacing", after)
        fast_before, fast_after = before["host"], after["host"]
        self.assertGreater(fast_before["stood_s"], 5.0, "the fast game stands without pacing: " + t_before)
        self.assertLess(fast_after["stood_s"], 0.5 * fast_before["stood_s"], "and much less with it: " + t_after)
        self.assertGreater(fast_after["pace_mean"], 1.1, "the fast game is slowed to its peer's pace: " + t_after)
        self.assertLess(after["client"]["pace_mean"], 1.06, "the slow game is not slowed further: " + t_after)
        self.assertGreater(fast_after["speed"], 0.9 * fast_before["speed"], "the session is not slower for it: %s / %s" % (t_before, t_after))
        self.assert_same_world()


# the harness's own tests run in their own file
for _name in dir(B.BarrierTest):
    if _name.startswith("test_") and _name not in MeasuredLinkTest.__dict__:
        setattr(MeasuredLinkTest, _name, None)


if __name__ == "__main__":
    unittest.main()
