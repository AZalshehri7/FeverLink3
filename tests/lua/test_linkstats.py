"""The mod's link statistics (feverlink3_linkstats.lua): hop latencies on the host's clock, the gaps between a peer's clocks at every
stamp, the barrier's stand-still per minute, and the minute's lines. Pure module; plus one two-game run through the bridge that shows
the new lines change nothing and say what they should.

Run:  python -I tests/lua/test_linkstats.py
"""
import os
import re
import sys
import unittest

import lupa.lua52 as lupa

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
CONTENT = os.path.join(HERE, "..", "..", "mod", "feverlink3", "content")

import test_lockstep as T  # noqa: E402

W = 1760000000000


class LinkStatsTest(unittest.TestCase):
    def setUp(self):
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        with open(os.path.join(CONTENT, "feverlink3_linkstats.lua"), encoding="utf-8") as f:
            self.S = self.L.execute(f.read())

    def hop(self, **kw):
        return self.L.table_from(kw)

    def test_every_hop_is_measured_on_the_hosts_clock_and_a_minute_has_its_lines_and_starts_over(self):
        # sender 500 ms ahead of the host, receiver 300 behind
        x = self.S.hops(self.hop(g=W + 500, l=W + 510, h=W + 40, so=500, sr=20, w=W - 210, ro=-300, rr=30), W - 205)
        self.assertEqual((x.g2l, x.l2h, x.h2w, x.w2r, x.e2e, x.rtt), (10, 30, 50, 5, 95, 50))
        # without offsets only the one-machine hops
        x = self.S.hops(self.hop(g=W, l=W + 3, h=W + 9, w=W + 20), W + 21)
        self.assertEqual((x.g2l, x.l2h, x.h2w, x.w2r, x.e2e), (3, None, None, 1, None))
        # a minute has its lines and starts over
        st = self.S.new(0)
        for i in range(5):
            self.S.peerClock(st, 1, self.hop(g=W + 250 * i, l=W + 250 * i + 2, h=W + 250 * i + 20, so=0, w=W + 250 * i + 40, ro=0), 0.25 * i, W + 250 * i + 41)
        self.S.frame(st, 0.016)
        self.S.holdStart(st, 10)
        self.S.holdEnd(st, 12.5, 2.5)
        self.S.holdStart(st, 59)
        self.S.build(st, 900)
        self.S.build(st, 1500)
        self.assertIsNone(self.S.minute(st, 30))
        lines = list(self.S.minute(st, 60).values())
        self.assertTrue(lines[0].startswith("link.minute secs=60 stood_s=3.5 holds=1 longest_s=2.5 frame_ms_n=1 frame_ms=16/16/16"), lines[0])
        self.assertIn(" clock_gap_ms_n=4 clock_gap_ms=250/250/250", lines[0])
        self.assertIn(" e2e_ms_n=5 e2e_ms=41/41/41", lines[0])
        self.assertIn(" build_ms_n=2 build_ms=900/1500/1500 build_p90_ms=1500", lines[0])
        self.assertTrue(lines[1].startswith("link.peer peer=1 clock_gap_ms_n=4"), lines[1])
        self.assertIn(" gap_g_ms_n=4 gap_g_ms=250/250/250", lines[1])
        # the hold still open at the minute's end: its rest counts in the next minute, the first second not twice
        self.S.holdEnd(st, 61, 2.0)
        nxt = list(self.S.minute(st, 120).values())
        self.assertIn("stood_s=1.0 holds=1", nxt[0])
        self.assertIn("clock_gap_ms_n=0", nxt[0])

class BridgeLinesTest(unittest.TestCase):
    def test_a_two_game_run_writes_barrier_details_and_changes_nothing_else(self):
        import shutil
        import tempfile
        tmp = tempfile.mkdtemp(prefix="feverlink3-linkstats-")
        try:
            a, b = T.Game("host", 0, tmp, 50000), T.Game("client", 1, tmp, 51000)
            class TightRelay(T.Relay):
                """Relays every promise cut down to the sender's own clock: the receiver's barrier lies behind its clock, so it holds
                until the next clock, every time (a peer whose clock lines come late, in effect)."""

                def broadcast(self, kind, payload, exclude=None):
                    if kind == "peerclock":
                        p = self.C.parse(payload)
                        p = p[0] if isinstance(p, tuple) else p
                        if p.safe is not None:
                            p["safe"] = int(p.t)
                        payload = self.C.serialize(p)
                    T.Relay.broadcast(self, kind, payload, exclude)

            relay = TightRelay([a, b])

            def tick(n):
                for _ in range(n):
                    for _ in range(4):
                        for g in (a, b):
                            g.frame()
                        relay.pump()
                    a.step()
                    b.step()
            tick(5)
            relay.started, relay.speed = True, 1
            relay.session()
            tick(40)
            trace = T.read_text(os.path.join(a.dir, "trace.log")) + T.read_text(os.path.join(b.dir, "trace.log"))
            holds = [l for l in trace.splitlines() if " barrier hold at " in l]
            self.assertTrue(holds, "the lagging game holds the other one")
            self.assertRegex(holds[0], r"peer_t=\d+ behind_steps=-?[\d.]+ room_steps=-?[\d.]+ clock_age_ms=\d+ rtt_ms=\? frame_ms=\S+ engine_speed=\d")
            rel = [l for l in trace.splitlines() if " barrier released at " in l]
            self.assertTrue(all(re.search(r"released at t=-?\d+ after [\d.]+ s \(player \d+\) peer_t=", l) for l in rel), rel[:3])
            self.assertNotIn("frame failed", a.log() + b.log())
            self.assertNotIn("detail=?", trace)
        finally:
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    unittest.main()
