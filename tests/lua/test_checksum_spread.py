"""A full checksum round spread over the simulation (bug list 5, owner 2026-10-17: "spread the checksum over several frames").

Real runs 2026-10-15/16: a full round read about 6500 entities in 370-480 ms of one simulation step (1.0 s at worst), about once a
minute. The host now asks for a full round with `spread` (ChecksumRequestMessage.spread, one engine batch): at the stamp every game takes
the light lanes and the entity lists of the heavy lanes, then reads L.CHECKSUM_CHUNK entities every `spread` steps, each chunk at the
same step on every game (a landing mark). Two mocked games (test_lockstep.py's harness), each splitting its steps between two simulation
Lua states as the real game does, check that:
  * every chunk is read at its step whichever state runs it, both games reach the parts of the round hashed whole, and no step costs more
    than the budget in a mock where every component read costs a fixed time, while the same world hashed whole does;
  * a difference is detected before the stamp and in the middle of the round, and a change made on both games mid-round is none;
  * an action stamped while a round is being read is applied at its stamp on both games (never held back, never dropped);
  * games that lag behind each other at x2 still read every chunk at its step; a fork is seen within 10 s and the dump is complete.

Run:  python -I -m unittest discover -s tests/lua -p "test_checksum_spread.py"
"""
import os
import re
import shutil
import sys
import tempfile
import unittest

import lupa.lua52 as lupa

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import test_lockstep as T  # noqa: E402

STEP = T.STEP
CHUNK = 200            # L.CHECKSUM_CHUNK
BUDGET_MS = 30         # L.CHECKSUM_FRAME_BUDGET_MS
HEAVY = {"constructions", "edges", "nodeConfigs", "townBuildings", "ids"}

# n town buildings in three towns, each with its own construction (the same ids on both games: they come from the savegame)
POPULATE = """
for i = 1, %(n)d do
  WORLD[1000 + 2 * i] = { CONSTRUCTION = { fileName = "house_" .. (i %% 7) .. ".con", transf = { [13] = i * 3.5, [14] = -i, [15] = 1 } } }
  WORLD[1001 + 2 * i] = { TOWN_BUILDING = { town = 90 + (i %% 3), construction = 1000 + 2 * i, level = 1 + (i %% 4), depth = 1, timeBuilt = i, parcels = { i } } }
end
"""

# every component read costs 0.05 ms of a fake clock: the round's cost then depends on what it reads, never on the machine
FAKE_CLOCK = """
FAKE = 0
os.clock = function() return FAKE end
for _, env in ipairs({ SIM_ENV, GUI_ENV, SIM2_ENV }) do   -- (SIM2_ENV: a second simulation state, when the test made one)
  local gc = env.api.engine.getComponent
  env.api.engine.getComponent = function(e, t) FAKE = FAKE + 0.00005; return gc(e, t) end
end
"""


def no_time(parts):
    return {k: v for k, v in parts.items() if k != "time"}


class TwoGames(unittest.TestCase):
    """Two mocked games with a populated world (the harness of the tests below)."""
    N = 600

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-spread-")
        self.A = T.Game("host", 0, self.tmp, 50000)
        self.B = T.Game("client", 1, self.tmp, 51000)
        self.relay = T.Relay([self.A, self.B])
        self.populate()

    def populate(self, n=None):
        self.N = n or self.N
        for g in (self.A, self.B):
            g.L.execute(POPULATE % {"n": self.N})

    def tearDown(self):
        if os.environ.get("FEVERLINK3_TEST_VERBOSE"):
            print("\n--- host mod.log ---\n" + self.A.log() + "\n--- client mod.log ---\n" + self.B.log())
        shutil.rmtree(self.tmp, ignore_errors=True)

    tick = T.LockstepTest.tick
    start = T.LockstepTest.start

    def round(self, n, spread, ticks=40, lag_b=False, full=None):
        self.relay.pump()
        at = self.relay.future_stamp()
        self.relay.checksum_request(n, at, False, full=full, spread=spread)
        self.tick(ticks, lag_b=lag_b)
        self.assertEqual(sorted(self.relay.reports.get(n, {})), [0, 1], "round %d reached the launcher from both games" % n)
        return at, self.relay.reports[n][0], self.relay.reports[n][1]

    def spread_line(self, g, n):
        m = re.search(r"checksum round %d spread: (\d+) entities in (\d+) chunk\(s\) of (\d+), every (\d+) step\(s\) from t=(\d+) to t=(\d+); "
                      r"start ([\d.]+) ms, step max ([\d.]+) ms \(budget (\d+) ms, (\d+) chunk\(s\) over\)" % n, g.log())
        self.assertIsNotNone(m, g.log())
        keys = ("entities", "chunks", "chunk", "every", "first", "last", "start", "step_max", "budget", "over")
        return {k: float(v) for k, v in zip(keys, m.groups())}


class TwoSimulationStatesTest(TwoGames):
    """The real game shares the steps of one game between two simulation Lua states (owner's live test of 2026-10-18,
    22:08:51: round 20 read its chunks at t=13300200, 13300800, 13301000, 13301600, ... two steps in four, 37 of 38 late by 144400 ms in
    all; an action was applied at its stamp at a step between them, by the other state). Before the fix the round lived in the module
    table of the state that began it. Each game here splits its steps the way the live log shows (two steps in one state, two in the
    other), the guest one step out of phase with the host: a round kept in one state was late by a different amount on each game."""

    # step k = t / 200: the host's state 1 runs k = 0, 1 mod 4, the guest's k = 1, 2 mod 4 (live log: 13300200 / 200 = 66501 = 1 mod 4)
    HOST_SPLIT = "function(t) local k = math.floor(t / 200) % 4; return (k == 0 or k == 1) and 1 or 2 end"
    GUEST_SPLIT = "function(t) local k = math.floor(t / 200) % 4; return (k == 1 or k == 2) and 1 or 2 end"

    def setUp(self):
        super().setUp()
        self.A.split_sim(self.HOST_SPLIT)
        self.B.split_sim(self.GUEST_SPLIT)

    def test_every_chunk_is_read_at_its_step_within_the_budget_and_the_parts_are_the_whole_rounds(self):
        for g in (self.A, self.B):
            g.L.execute(FAKE_CLOCK)
        self.start()
        self.tick(5)
        _at, (_t, whole), (_tb, whole_b) = self.round(1, spread=None)
        self.assertEqual(whole, whole_b)
        slow = re.search(r"sim\] checksum slow: total (\d+) ms", self.A.log())
        self.assertIsNotNone(slow, "the whole round costs more than the budget of the fake clock: %s" % self.A.log())
        self.assertGreater(int(slow.group(1)), BUDGET_MS)
        for n in (2, 3):
            at, (ta, pa), (tb, pb) = self.round(n, spread=1)
            self.assertEqual((ta, tb), (at, at), "both report the round's stamp: every chunk on time")
            self.assertEqual(pa, pb)
            self.assertTrue(HEAVY <= set(pa), pa)
            self.assertTrue(pa["townBuildings"].startswith("%d/" % (self.N + 3)), pa["townBuildings"])
            self.assertEqual(no_time(pa), no_time(whole), "the same static world hashed whole gives the same parts")
            lines = [self.spread_line(g, n) for g in (self.A, self.B)]
            self.assertEqual(lines[0]["entities"], lines[1]["entities"])
            for s in lines:
                self.assertEqual((s["chunk"], s["every"], s["over"], s["budget"]), (CHUNK, 1, 0, BUDGET_MS))
                self.assertEqual(s["chunks"], -(-s["entities"] // CHUNK), "the chunks are cut by count: the same on both games")
                self.assertGreater(s["chunks"], 4)
                self.assertEqual((s["first"], s["last"]), (at, at + s["chunks"] * STEP), "one chunk a step after the stamp")
                self.assertLessEqual(s["step_max"], BUDGET_MS, s)
                self.assertLessEqual(s["start"], BUDGET_MS, s)
        for g in (self.A, self.B):
            self.assertEqual(len(re.findall(r"sim\] checksum slow", g.log())), 1, "only the whole round was slow")
            runs = g.sim_runs()
            self.assertGreater(min(runs[1], runs[2]), 20, "both states ran steps: %s" % runs)
        log = self.A.log() + self.B.log()
        for bad in ("read after their step", "failed and is dropped"):
            self.assertNotIn(bad, log)
        # a light round and a paused round are hashed whole, never spread
        _at, (ta, pa), (tb, pb) = self.round(4, spread=0, full=False)
        self.assertEqual(pa, pb)
        self.assertFalse(set(pa) & HEAVY)
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(30)
        self.relay.checksum_request(5, self.relay.pauseAt, True, spread=0)
        self.tick(3)
        self.assertEqual(self.relay.reports[5][0], self.relay.reports[5][1])
        self.assertTrue(HEAVY <= set(self.relay.reports[5][0][1]))
        self.assertNotIn("checksum round 5 spread", self.A.log())

    def test_a_difference_before_or_during_a_round_is_seen_and_the_same_change_on_both_games_is_none(self):
        b_only = (False, True)
        cases = [
            # (what, building i, on host/guest, at step after the stamp (None: before it), change, the parts that differ)
            ("grown on the guest before the stamp", 7, b_only, None, "WORLD[1001 + 2 * I].TOWN_BUILDING.level = 9",
             {"townBuildings", "townBuildings:91"}),
            ("grown on both one step in (as the engine's town growth)", 11, (True, True), 1,
             "WORLD[1001 + 2 * I].TOWN_BUILDING.level = 9; WORLD[1000 + 2 * I].CONSTRUCTION.transf[13] = 7", set()),
            ("changed on the guest in the middle of the round", 12, b_only, 2, "WORLD[1001 + 2 * I].TOWN_BUILDING.depth = 3",
             {"townBuildings", "townBuildings:90"}),
            ("replaced by its town on both: gone when its chunk comes", 21, (True, True), 1,
             "WORLD[1000 + 2 * I] = nil; WORLD[1001 + 2 * I] = nil", set()),
            ("replaced on the guest only", 22, b_only, 1, "WORLD[1000 + 2 * I] = nil; WORLD[1001 + 2 * I] = nil",
             {"constructions", "townBuildings", "townBuildings:91"}),
        ]
        self.start()
        self.tick(5)
        for n, (what, i, on, after, change, want) in enumerate(cases, 1):
            with self.subTest(what):
                self.populate()
                self.relay.pump()
                at = self.relay.future_stamp()
                for g, yes in zip((self.A, self.B), on):
                    g.L.execute("I = %d; STEP_HOOK = nil" % i)
                    if yes and after is None:
                        g.L.execute(change)
                    elif yes:
                        g.L.execute("STEP_HOOK = function(t) if t == %d then %s end end" % (at + after * STEP, change))
                self.relay.checksum_request(n, at, False, spread=1)
                self.tick(40)
                (ta, pa), (tb, pb) = self.relay.reports[n][0], self.relay.reports[n][1]
                self.assertEqual((ta, tb), (at, at), "the same time: the round is compared")
                self.assertEqual({k for k in pa if pa[k] != pb[k]}, want)
                if i == 21:
                    self.assertTrue(pa["townBuildings"].startswith("%d/" % (self.N + 2)), "the list had it, the read found nothing")
        self.assertNotIn("checksum lane", self.A.log() + self.B.log(), "no lane failed")

    def test_an_action_stamped_while_a_round_is_read_is_applied_at_its_stamp_on_both_games(self):
        self.populate(1200)
        self.start()
        self.tick(5)
        self.relay.pump()
        self.relay.checksum_request(1, self.relay.future_stamp(), False, spread=1)
        self.tick(2)
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function(d, s, e) BUY_CB = s end)')
        self.tick(50)
        a, b = self.A.applied(), self.B.applied()
        self.assertEqual(len(a), 1, a)
        self.assertEqual(a, b, "applied at the same stamp on both games")
        stamp = int(a[0].split("@")[1])
        s = self.spread_line(self.A, 1)
        self.assertTrue(s["first"] < stamp <= s["last"], "the action's stamp lies inside the round (%s..%s): %d" % (s["first"], s["last"], stamp))
        self.assertTrue(self.B.L.eval("BUY_CB == true"), "the client's own build reported its result")
        self.assertEqual(self.relay.reports[1][0], self.relay.reports[1][1])
        self.assertFalse(re.search(r"LATE|dropped", self.A.log() + self.B.log()), "nothing late, nothing dropped")
        # the next round sees the vehicle on both games alike
        _at, (ta, pa), (tb, pb) = self.round(2, spread=1)
        self.assertEqual((ta, pa), (tb, pb))

    def test_games_lagging_behind_each_other_at_x2_read_every_chunk_at_its_step(self):
        self.start(speed=2)
        self.tick(6, lag_b=True)
        for n in (1, 2):
            at, (ta, pa), (tb, pb) = self.round(n, spread=2, ticks=60, lag_b=True)
            self.assertEqual((ta, tb), (at, at))
            self.assertEqual(pa, pb)
            for g in (self.A, self.B):
                s = self.spread_line(g, n)
                self.assertEqual((s["every"], s["last"]), (2, at + s["chunks"] * 2 * STEP))
        self.assertNotIn("read after their step", self.A.log() + self.B.log())

    def test_a_fork_is_seen_within_10_s_and_twice_within_15_s_and_the_detail_dump_is_complete_on_both_games(self):
        """The host's cadence since 2026-10-19 (host.rs): a round every 5 s (25 steps at x1), never two at once, chunks sized from the
        games' reported cost (ChecksumPolicy::chunk_for, mirrored below), and `detail` after a round that differed. Each component read
        costs 0.01 ms of a fake clock (the real game: 17-30 us an entity, labs H/I/K and the friend test). A town building forks on the
        guest only: the first round that differs is complete within 10 s of game time and the second (the repair) within 15 s; no round
        before differs; no step costs more than the budget; the dump at the repair is complete and of the same chunks on both games."""
        self.populate(1500)
        for g in (self.A, self.B):
            g.L.execute(FAKE_CLOCK.replace("0.00005", "0.00001"))

        def chunk_for(costs, previous):   # core ChecksumPolicy::chunk_for (its own tests: core/tests/policy_tests.rs)
            ns = max([c for per in costs.values() for c in per[-3:]] or [0])
            if not ns:
                return previous
            return min(max(12_000_000 // ns, 50), 4000, previous * 2)

        self.start()
        self.tick(5)
        costs, chunk, detail, n, asked, fork, differing = {0: [], 1: [], }, 200, False, 0, -10**9, None, []
        ticks = 0
        while len(differing) < 2 and ticks < 600:
            if n == 0 or (len(self.relay.reports.get(n, {})) == 2 and ticks - asked >= 25):
                if n:
                    (ta, pa), (tb, pb) = self.relay.reports[n][0], self.relay.reports[n][1]
                    self.assertEqual(ta, tb, "every round compared (round %d)" % n)
                    if pa != pb:
                        self.assertIsNotNone(fork, "no difference before the fork (round %d): %s" % (n, {k for k in pa if pa[k] != pb[k]}))
                        differing.append((n, ticks - fork))
                    detail = pa != pb
                    for gid, c in self.relay.costs.get(n, {}).items():
                        costs[gid].append(c["entityNs"])
                    chunk = chunk_for(costs, chunk)
                if len(differing) == 2:
                    break
                n += 1
                self.relay.pump()
                self.relay.checksum_request(n, self.relay.future_stamp(), False, spread=1, chunk=chunk, detail=detail)
                asked = ticks
            if fork is None and n >= 3 and len(self.relay.reports.get(3, {})) == 2:
                # the fork comes just after round 3 read that building (close to the worst phase): round 4, asked 5 s after round 3, sees it
                fork = ticks
                self.B.L.execute("WORLD[1001 + 2 * 33].TOWN_BUILDING.level = 9")   # town growth on the guest only: a fork
            self.tick(1)
            ticks += 1
        (n1, first), (n2, second) = differing
        self.assertEqual(n2, n1 + 1, "two rounds in a row")
        self.assertLessEqual(first, 50, "seen within 10 s at x1 (5 steps a second): %d steps" % first)
        self.assertLessEqual(second, 75, "seen twice (the repair) within 15 s: %d steps" % second)
        for g in (self.A, self.B):
            lines = [self.spread_line(g, k) for k in range(1, n2 + 1)]
            self.assertTrue(all(s["step_max"] <= BUDGET_MS and s["over"] == 0 for s in lines), lines)
            self.assertGreater(lines[-1]["chunk"], CHUNK, "the chunk grew from the measured cost: %s" % lines[-1])
        self.assertNotIn("read after their step", self.A.log() + self.B.log())
        # the repair's dump: the keys of detail round n2 from the game-script state, every chunk whichever state read it
        for g in (self.A, self.B):
            self.relay.send(g, "resync_save", 0, self.relay.ser({"id": 1, "name": "resync_1"}))
        self.tick(3)
        t = self.relay.reports[n2][0][0]
        files = [T.read_text(os.path.join(g.dir, "detail_%d_%d.txt" % (t, g.id))).splitlines() for g in (self.A, self.B)]
        for f in files:
            self.assertIn("complete: detail round %d" % n2, f[0])
            self.assertEqual(sum(1 for x in f if x.startswith("townBuildings\t")), self.N + 3)
        self.assertEqual(len(files[0]), len(files[1]))
        diff = sorted(set(files[0][1:]) ^ set(files[1][1:]))
        self.assertEqual(len(diff), 2, "only the forked building differs: %s" % diff[:6])
        self.assertTrue(all(x.startswith("townBuildings\t") for x in diff), diff)


class LateRoundsAreLoudTest(TwoGames):
    """Rounds read late again and again are desync detection lost (the host skips a round whose games report different times): the
    simulation half says so in a WARNING line and a line every game minute, never only in a clause of the round's own line."""

    def test_rounds_late_in_a_row_give_a_warning_and_a_minute_line_and_on_time_again_is_said(self):
        # the host's game runs no script at one step in four (a step the script misses: the only way left to make a chunk late)
        self.A.split_sim("function(t) return math.floor(t / 200) % 4 == 3 and 0 or 1 end")
        self.start()
        self.tick(5)
        for n in (1, 2, 3):
            self.round(n, spread=1)
        log = self.A.log()
        self.assertIn("read after their step", log)
        self.assertRegex(log, r"WARNING: 3 checksum rounds in a row read chunks after their steps \(round 3: \d+ chunk\(s\), \+[\d,]+ ms\)")
        self.assertNotIn("WARNING", self.B.log(), "the guest read every chunk on time")
        # a game minute later: the minute's line
        self.tick(300)
        self.round(4, spread=1)
        self.assertRegex(self.A.log(), r"checksum minute: \d+ of \d+ spread round\(s\) since t=[\d,]+ read chunks after their steps")
        # every step runs a script again: the next round is on time, and that is said
        self.A.L.execute("SIM_SPLIT = nil")
        _at, (ta, pa), (tb, pb) = self.round(5, spread=1)
        self.assertEqual((ta, pa), (tb, pb))
        self.assertRegex(self.A.log(), r"checksum rounds read on time again after \d+ late round\(s\) in a row \(round 5\)")


class SpreadRoundPureTest(unittest.TestCase):
    """L.newChecksumRound / L.checksumRoundStep over a plain engine: no game, no bridge."""

    ENGINE = """(function(n)
      local reads = 0
      local e = {
        gameTime = function() return 5000 end,
        account = function() return 100, 10 end,
        lines = function() return {} end,
        constructionIds = function() local t = {} for i = 1, n do t[i] = i end return t end,
        construction = function(i) reads = reads + 1; return { file = "c" .. (i % 5) .. ".con", x = i, y = 2 * i, z = 0 } end,
        streetIds = function() local ed, no = {}, {} for i = 1, n do ed[i] = 10000 + i; no[i] = 20000 + i end return { edges = ed, nodes = no } end,
        edge = function(i, ctx) reads = reads + 1; return { x0 = i, y0 = 0, x1 = i + 1, y1 = 0, type = "0", objects = {} } end,
        nodeConfig = function(i) reads = reads + 1; return { x = i, y = 1, lanes = 2, crosswalks = 0 } end,
        townBuildingIds = function() local t = {} for i = 1, n do t[i] = 30000 + i end return t end,
        townBuilding = function(i) reads = reads + 1; return { town = 1 + i % 4, file = "h.con", x = i, y = i, z = 0, level = LEVEL[i] or 1, depth = 1, parcels = 1, timeBuilt = i } end,
      }
      LEVEL = LEVEL or {}
      return e, function() local r = reads; reads = 0; return r end
    end)"""

    def setUp(self):
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.C = T.load_common(self.L)
        self.K = self.L.execute(T.LOCKSTEP_SRC)
        self.mk = self.L.eval(self.ENGINE)

    def spread(self, n, chunk, max_chunks=None):
        e, reads = self.mk(n)
        reads()
        opts = self.L.table_from({"chunk": chunk, "maxChunks": max_chunks or 1000, "keyHash": self.C.fnv1a})
        r = self.K.newChecksumRound(e, self.C.hashValue, False, opts)
        self.assertEqual(reads(), 0, "the stamp reads no entity: only the lists")
        per_step = []
        while not r.done:
            self.K.checksumRoundStep(r)
            per_step.append(reads())
        return r, per_step

    def whole(self, n):
        e, _reads = self.mk(n)
        parts, t = self.K.checksumParts(e, self.C.hashValue, None, False, None, self.C.fnv1a)
        return {k: v for k, v in parts.items()}, t

    def test_a_spread_round_gives_the_whole_rounds_parts_a_chunk_a_step_and_its_marks(self):
        r, per_step = self.spread(250, 64)
        self.assertEqual(sum(per_step), 4 * 250)
        self.assertTrue(all(k <= 64 for k in per_step), per_step)
        self.assertEqual(len(per_step), -(-1000 // 64))
        self.assertEqual(r.chunks, len(per_step))
        whole, t = self.whole(250)
        a = {k: v for k, v in r.parts.items()}
        self.assertEqual(a, whole)
        self.assertEqual(t, 5000)
        self.assertTrue(r.parts["townBuildings"].startswith("250/"))
        self.assertEqual(len([k for k in r.parts.keys() if str(k).startswith("townBuildings:")]), 4)
        # one entity that differs is detected in its lane and its town
        self.L.execute("LEVEL[30007] = 3")
        r2, _ = self.spread(250, 64)
        b = {k: v for k, v in r2.parts.items()}
        self.assertEqual({k for k in a if a[k] != b[k]}, {"townBuildings", "townBuildings:4"})
        # in a large world the chunk grows instead of the round
        r3, per_step = self.spread(1000, 10, max_chunks=20)
        self.assertEqual((r3.chunk, len(per_step)), (200, 20))
        # a failing read makes its lane an error part and the round goes on
        e, _reads = self.mk(50)
        e.edge = self.L.eval("function(i) if i == 10020 then error('boom') end return { x0 = i, y0 = 0, x1 = 1, y1 = 0 } end")
        r4 = self.K.newChecksumRound(e, self.C.hashValue, False, self.L.table_from({"chunk": 16}))
        while not r4.done:
            self.K.checksumRoundStep(r4)
        self.assertEqual(r4.parts["edges"], "error")
        self.assertNotEqual(r4.parts["townBuildings"], "error")
        self.assertIn("boom", r4.errors["edges"])
        # the landing marks of a spread round are its chunk steps; a whole, paused or light round has none
        marks = list(self.K.checksumMarks(self.K.checksumItem(self.L.table_from({"n": 3, "at": 4000, "paused": False, "spread": 2}))).values())
        self.assertEqual((marks[:3], len(marks)), ([4400, 4800, 5200], 48))
        for p in ({"n": 3, "at": 4000, "paused": False}, {"n": 3, "at": 4000, "paused": True, "spread": 2},
                  {"n": 3, "at": 4000, "paused": False, "full": False, "spread": 2}):
            self.assertEqual(len(self.K.checksumMarks(self.K.checksumItem(self.L.table_from(p)))), 0, p)

    def test_a_round_saved_and_restored_after_every_chunk_gives_the_same_parts(self):
        # the shared form (the game-script state): every chunk read by a fresh copy restored from the saved record, as when each step
        # runs in another Lua state; the state's copy goes through a deep copy as the mock's game-script state does
        e, _reads = self.mk(250)
        opts = self.L.table_from({"chunk": 64, "maxChunks": 1000, "keyHash": self.C.fnv1a})
        r = self.K.newChecksumRound(e, self.C.hashValue, False, opts)
        rec = self.K.checksumRoundSave(r, None)
        ids = rec.lanes[1].ids
        self.assertEqual(self.L.eval("type")(ids), "string", "an entity list travels as one text")
        copy = self.L.eval("""function(v) local function dc(x) if type(x) ~= 'table' then return x end local o = {} for k, y in pairs(x) do o[dc(k)] = dc(y) end return o end return dc(v) end""")
        steps = 0
        while not rec.done:
            r2 = self.K.checksumRoundRestore(e, self.C.hashValue, copy(rec), opts)
            self.K.checksumRoundStep(r2)
            rec = self.K.checksumRoundSave(r2, copy(rec))
            steps += 1
        self.assertEqual({k: v for k, v in rec.parts.items()}, self.whole(250)[0])
        self.assertEqual(steps, -(-1000 // 64))
        self.assertEqual(list(self.K.unpackIds("5,6,70").values()), [5, 6, 70])
        self.assertEqual(self.K.packIds(self.L.table_from([3, 4])), "3,4")


class FnvNativeXorTest(unittest.TestCase):
    """C.fnv1a with the state's native xor (bit32.bxor in Lua 5.2, LuaJIT's bit.bxor) gives the pure-Lua values bit for bit."""

    def load(self, pre=""):
        L = lupa.LuaRuntime(unpack_returned_tuples=True)
        L.execute(pre)
        return T.load_common(L)

    def test_the_native_and_the_pure_paths_agree_and_a_library_that_disagrees_is_not_used(self):
        import random
        rnd = random.Random(20261018)
        strings = [b"", bytes(range(256)), bytes(range(255, -1, -1)) * 40, b"\x00" * 1000, b"\xff" * 999]
        strings += [bytes(rnd.randrange(256) for _ in range(rnd.randrange(0, 300))) for _ in range(400)]
        strings.append(bytes(rnd.randrange(256) for _ in range(200000)))
        Cn, Cp = self.load(), self.load("bit32 = nil; bit = nil")
        self.assertEqual(Cp.FNV_XOR, "lua")
        self.assertIn(Cn.FNV_XOR, ("bit32", "bit"), "lupa's Lua 5.2 has bit32")
        for s in strings:
            self.assertEqual(Cn.fnv1a(s), Cp.fnv1a(s), len(s))
        self.assertEqual(Cn.fnv1a(b"a"), 0xE40C292C)
        self.assertEqual(Cp.fnv1a(b"foobar"), 0xBF9CF968)
        bad = self.load("bit32 = { bxor = function(a, b) return a end }")
        self.assertEqual((bad.FNV_XOR, bad.fnv1a(b"a")), ("lua", 0xE40C292C))


if __name__ == "__main__":
    unittest.main()
