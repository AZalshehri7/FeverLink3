"""The games autosave together (friend session 2026-10-30; native autosave.rs, FEVERLINK3_SYNC_AUTOSAVE).

Each game's own autosave stopped its simulation ~3.3 s at its own interval (guest 2 min, host 10) and the other game waited at the
input barrier. With the native module's `sync_autosave`, every game holds its own autosave timer off while the session runs, the host
stamps an `autosave` action at its own interval, and every game asks its module to run the game's own autosave when it reaches the
stamp. The two mocked games are the harness of test_input_barrier.py (imported, never edited); the native module is played by the test
(its `ready` and `autosave` events written into native_events.log, the mod's control lines read back).

Run:  python -I tests/lua/test_sync_autosave.py
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

READY = 'ready\tnative\t{["build"]="40425",["clock"]=false,["hooks"]={[1]="sync_autosave"},["poolpatch"]=false,["sites"]=0,["version"]="test"}\n'
EVENT = 'autosave\tnative\t{["hold"]=false,["minutes"]=%d,["runs"]=0,["why"]=""}\n'


class SyncAutosaveTest(unittest.TestCase):
    setUp, tearDown, start = B.BarrierTest.setUp, B.BarrierTest.tearDown, B.BarrierTest.start

    def make(self, n, seed, delay):
        B.BarrierTest.make(self, n, seed, delay)
        for g in self.games:
            g.L.execute(BC.FIND_UP)   # the fake wall clock

    def tick(self, n=1):
        for _ in range(n):
            for g in self.games:
                g.L.execute("FAKE_CLOCK = FAKE_CLOCK + %r" % (1.0 / 6))
            B.BarrierTest.tick(self, 1)

    def ctl(self, g):
        text = ""
        for path in glob.glob(os.path.join(g.dir, "native_ctl.*.log")):
            with open(path, "r", encoding="utf-8", newline="") as f:
                text += f.read()
        return [line for line in text.splitlines() if line.startswith("autosave\t")]

    def test_the_host_stamps_at_its_own_interval_and_every_game_runs_its_autosave_at_that_step(self):
        self.make(2, 7, BC.link(60))
        host, guest = self.games
        for g, minutes in ((host, 1), (guest, 2)):
            with open(os.path.join(g.dir, "native_events.log"), "w", encoding="utf-8", newline="") as f:
                f.write(READY + EVENT % minutes)
        self.start()
        for g in self.games:
            self.assertTrue(self.ctl(g) and all('["hold"]=true' in line for line in self.ctl(g)), "the own timer is held off: %s" % self.ctl(g))
        self.tick(6 * 30)
        self.assertNotIn("stamped", host.log(), "not before the host's own interval")
        self.tick(6 * 35)
        self.assertEqual(len(re.findall(r"autosave \S+ stamped", host.log())), 1, host.log())
        self.assertNotIn("stamped", guest.log(), "the guest's own (2 min) interval stamps nothing")
        reached = []
        for g in self.games:
            self.assertEqual(sum('["now"]=true' in line for line in self.ctl(g)), 1, "%s: %s" % (g.label, self.ctl(g)))
            m = re.findall(r"autosave \S+ reached at t=(\d+) \(stamp (\d+)\)", g.log())
            self.assertEqual(len(m), 1, g.log())
            reached.append(m[0])
        self.assertTrue(all(t == s for t, s in reached) and reached[0] == reached[1], "the same step on every game: %s" % reached)


if __name__ == "__main__":
    unittest.main()
