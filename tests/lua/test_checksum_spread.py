"""A full checksum round spread over the simulation (bug list 5, owner 2026-10-17: "spread the checksum over several frames").

Real runs 2026-10-15/16: a full round read about 6500 entities in 370-480 ms of one simulation step (1.0 s at worst), about once a
minute. The host now asks for a full round with `spread` (ChecksumRequestMessage.spread, one engine batch): at the stamp every game takes
the light lanes and the entity lists of the heavy lanes, then reads L.CHECKSUM_CHUNK entities every `spread` steps, each chunk at the
same step on every game (a landing mark). Two mocked games (test_lockstep.py's harness) check that:
  * both games reach the same parts at the same reported time, with the work cut into chunks of at most L.CHECKSUM_CHUNK entities;
  * no step of a spread round costs more than the budget in a mock where every component read costs a fixed time, while the same
    world hashed whole does;
  * a difference is still detected, before the stamp and in the middle of the round, and a change on both games mid-round is not one;
  * an action stamped while a round is being read is applied at its stamp on both games (never held back, never dropped);
  * games that lag behind each other at x2 still read every chunk at its step.

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


class TwoGames(unittest.TestCase):
    """Two mocked games with a populated world (the harness of the tests below)."""
    N = 600

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-spread-")
        self.A = T.Game("host", 0, self.tmp, 50000)
        self.B = T.Game("client", 1, self.tmp, 51000)
        self.relay = T.Relay([self.A, self.B])
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


class SpreadChecksumTest(TwoGames):

    def test_both_games_reach_the_same_parts_reading_one_chunk_a_step(self):
        self.start()
        self.tick(5)
        at, (ta, pa), (tb, pb) = self.round(1, spread=1)
        self.assertEqual((ta, tb), (at, at), "both report the round's stamp")
        self.assertEqual(pa, pb)
        self.assertTrue(HEAVY <= set(pa), pa)
        self.assertTrue(pa["townBuildings"].startswith("%d/" % (self.N + 3)), pa["townBuildings"])
        for g in (self.A, self.B):
            s = self.spread_line(g, 1)
            self.assertEqual(s["chunk"], CHUNK)
            self.assertEqual(s["chunks"], -(-s["entities"] // CHUNK), "the chunks are cut by count: the same on both games")
            self.assertGreater(s["chunks"], 4)
            self.assertEqual((s["first"], s["last"]), (at, at + s["chunks"] * STEP), "one chunk a step after the stamp")
            self.assertEqual(s["every"], 1)
        self.assertEqual(self.spread_line(self.A, 1)["entities"], self.spread_line(self.B, 1)["entities"])
        # the same static world hashed whole gives the same parts (only the time differs)
        _at2, (_t, whole), _b = self.round(2, spread=None)
        self.assertEqual({k: v for k, v in pa.items() if k != "time"}, {k: v for k, v in whole.items() if k != "time"})
        self.assertNotIn("read after their step", self.A.log() + self.B.log())

    def test_no_step_of_a_spread_round_costs_more_than_the_budget_while_the_whole_round_does(self):
        for g in (self.A, self.B):
            g.L.execute(FAKE_CLOCK)
        self.start()
        self.tick(5)
        self.round(1, spread=None)
        whole = re.search(r"sim\] checksum slow: total (\d+) ms", self.A.log())
        self.assertIsNotNone(whole, "the whole round costs more than 50 ms of the fake clock: %s" % self.A.log())
        self.assertGreater(int(whole.group(1)), BUDGET_MS)
        _at, (ta, pa), (tb, pb) = self.round(2, spread=1)
        self.assertEqual((ta, pa), (tb, pb))
        for g in (self.A, self.B):
            s = self.spread_line(g, 2)
            self.assertLessEqual(s["step_max"], BUDGET_MS, s)
            self.assertLessEqual(s["start"], BUDGET_MS, s)
            self.assertEqual(s["over"], 0)
            self.assertEqual(s["budget"], BUDGET_MS)
            self.assertEqual(len(re.findall(r"sim\] checksum slow", g.log())), 1, "only the whole round was slow")

    def test_a_difference_before_the_stamp_is_detected(self):
        self.start()
        self.tick(5)
        self.B.L.execute("WORLD[1001 + 2 * 7].TOWN_BUILDING.level = 9")   # a building of town 91 grown on one game only
        _at, (ta, pa), (tb, pb) = self.round(1, spread=1)
        self.assertEqual(ta, tb)
        self.assertEqual({k for k in pa if pa[k] != pb[k]}, {"townBuildings", "townBuildings:91"})

    def test_a_change_in_the_middle_of_the_round_is_seen_on_one_game_and_not_when_both_make_it(self):
        self.start()
        self.tick(5)
        self.relay.pump()
        # both games grow the same building one step after the stamp (as the engine's own town growth): the round, which reads the town
        # buildings last, sees it on both, at the same step
        at = self.relay.future_stamp()
        hook = ("STEP_HOOK = function(t) if t == %d then WORLD[1001 + 2 * 11].TOWN_BUILDING.level = 9; WORLD[1000 + 2 * 11].CONSTRUCTION.transf[13] = 7 end end"
                % (at + STEP))
        for g in (self.A, self.B):
            g.L.execute(hook)
        self.relay.checksum_request(1, at, False, spread=1)
        self.tick(40)
        (ta, pa), (tb, pb) = self.relay.reports[1][0], self.relay.reports[1][1]
        self.assertEqual((ta, pa), (tb, pb), "the same change at the same step on both games is no difference")
        # now on the client only, in the middle of the round
        self.relay.pump()
        at = self.relay.future_stamp()
        self.B.L.execute("STEP_HOOK = function(t) if t == %d then WORLD[1001 + 2 * 12].TOWN_BUILDING.depth = 3 end end" % (at + 2 * STEP))
        self.A.L.execute("STEP_HOOK = nil")
        self.relay.checksum_request(2, at, False, spread=1)
        self.tick(40)
        (ta, pa), (tb, pb) = self.relay.reports[2][0], self.relay.reports[2][1]
        self.assertEqual(ta, tb, "the same time: the round is compared")
        self.assertEqual({k for k in pa if pa[k] != pb[k]}, {"townBuildings", "townBuildings:90"})

    def test_an_entity_removed_while_the_round_is_read_is_nothing_on_both_games_and_a_difference_on_one(self):
        self.start()
        self.tick(5)
        for n, games in ((1, (self.A, self.B)), (2, (self.B,))):
            self.relay.pump()
            at = self.relay.future_stamp()
            # a town replaces a building one step after the stamp: the TOWN_BUILDING and its construction are gone when their chunk comes
            i = 20 + n
            for g in (self.A, self.B):
                g.L.execute("STEP_HOOK = nil")
            for g in games:
                g.L.execute("STEP_HOOK = function(t) if t == %d then WORLD[1000 + 2 * %d] = nil; WORLD[1001 + 2 * %d] = nil end end" % (at + STEP, i, i))
            self.relay.checksum_request(n, at, False, spread=1)
            self.tick(40)
            (ta, pa), (tb, pb) = self.relay.reports[n][0], self.relay.reports[n][1]
            self.assertEqual(ta, tb)
            differing = {k for k in pa if pa[k] != pb[k]}
            if n == 1:
                self.assertEqual(differing, set(), "removed on both games at the same step")
                self.assertTrue(pa["townBuildings"].startswith("%d/" % (self.N + 2)), "the list had it, the read found nothing: %s" % pa["townBuildings"])
            else:
                self.assertEqual(differing, {"constructions", "townBuildings", "townBuildings:%d" % (90 + i % 3)})
        self.assertNotIn("checksum lane", self.A.log() + self.B.log(), "no lane failed")

    def test_an_action_stamped_while_a_round_is_read_is_applied_at_its_stamp_on_both_games(self):
        self.N = 1200
        for g in (self.A, self.B):
            g.L.execute(POPULATE % {"n": self.N})
        self.start()
        self.tick(5)
        self.relay.pump()
        at = self.relay.future_stamp()
        self.relay.checksum_request(1, at, False, spread=1)
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
            at, (ta, pa), (tb, pb) = self.round(n, spread=2, ticks=50, lag_b=True)
            self.assertEqual((ta, tb), (at, at))
            self.assertEqual(pa, pb)
            for g in (self.A, self.B):
                s = self.spread_line(g, n)
                self.assertEqual(s["every"], 2)
                self.assertEqual(s["last"], at + s["chunks"] * 2 * STEP)
        self.assertNotIn("read after their step", self.A.log() + self.B.log())

    def test_a_light_round_and_a_paused_round_are_hashed_whole(self):
        self.start()
        self.tick(5)
        _at, (ta, pa), (tb, pb) = self.round(1, spread=0, full=False)
        self.assertEqual(pa, pb)
        self.assertFalse(set(pa) & HEAVY)
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(30)
        self.relay.checksum_request(2, self.relay.pauseAt, True, spread=0)
        self.tick(3)
        self.assertEqual(self.relay.reports[2][0], self.relay.reports[2][1])
        self.assertTrue(HEAVY <= set(self.relay.reports[2][0][1]))
        self.assertNotIn("checksum round 2 spread", self.A.log())


class TwoSimulationStatesTest(SpreadChecksumTest):
    """The real game shares the steps of one game between two simulation Lua states (owner's live test of 0.2.0-pre5, 2026-10-18,
    22:08:51: round 20 read its chunks at t=13300200, 13300800, 13301000, 13301600, ... two steps in four, 37 of 38 late by 144400 ms in
    all; an action was applied at its stamp at a step between them, by the other state). Before the fix the round lived in the module
    table of the state that began it. Each game here splits its steps the way the live log shows (two steps in one state, two in the
    other), the guest one step out of phase with the host: a round kept in one state was late by a different amount on each game."""

    # step k = t / 200: the host's state 1 runs k = 0, 1 mod 4, the guest's k = 1, 2 mod 4 (live log: 13300200 / 200 = 66501 = 1 mod 4)
    HOST_SPLIT = "function(t) local k = math.floor(t / 200) %% 4; return (k == 0 or k == 1) and 1 or 2 end"
    GUEST_SPLIT = "function(t) local k = math.floor(t / 200) %% 4; return (k == 1 or k == 2) and 1 or 2 end"

    def setUp(self):
        super().setUp()
        self.A.split_sim(self.HOST_SPLIT.replace("%%", "%"))
        self.B.split_sim(self.GUEST_SPLIT.replace("%%", "%"))

    def test_every_chunk_is_read_at_its_step_whichever_state_runs_it(self):
        self.start()
        self.tick(5)
        for n in (1, 2, 3):
            at, (ta, pa), (tb, pb) = self.round(n, spread=1)
            self.assertEqual((ta, tb), (at, at), "both report the round's stamp: every chunk on time")
            self.assertEqual(pa, pb)
            self.assertTrue(HEAVY <= set(pa), pa)
            for g in (self.A, self.B):
                s = self.spread_line(g, n)
                self.assertEqual((s["first"], s["last"]), (at, at + s["chunks"] * STEP))
                self.assertLessEqual(s["step_max"], BUDGET_MS)
        log = self.A.log() + self.B.log()
        self.assertNotIn("read after their step", log)
        self.assertNotIn("WARNING", log)
        self.assertNotIn("failed and is dropped", log)
        for g in (self.A, self.B):
            runs = g.sim_runs()
            self.assertGreater(runs[1], 20, runs)
            self.assertGreater(runs[2], 20, "the second state ran steps too: %s" % runs)
        # a split world is the same as one Lua state's: the parts equal those of a round hashed whole
        _at, (_t, whole), (_tb, whole_b) = self.round(4, spread=None)
        self.assertEqual({k: v for k, v in pa.items() if k != "time"}, {k: v for k, v in whole.items() if k != "time"})
        self.assertEqual(whole, whole_b)

    def test_a_difference_in_the_middle_of_the_round_is_still_seen(self):
        self.start()
        self.tick(5)
        self.relay.pump()
        at = self.relay.future_stamp()
        self.B.L.execute("STEP_HOOK = function(t) if t == %d then WORLD[1001 + 2 * 12].TOWN_BUILDING.depth = 3 end end" % (at + 2 * STEP))
        self.relay.checksum_request(1, at, False, spread=1)
        self.tick(40)
        (ta, pa), (tb, pb) = self.relay.reports[1][0], self.relay.reports[1][1]
        self.assertEqual((ta, tb), (at, at), "compared")
        self.assertEqual({k for k in pa if pa[k] != pb[k]}, {"townBuildings", "townBuildings:90"})

    def test_at_x2_with_lag_every_chunk_is_on_time(self):
        self.start(speed=2)
        self.tick(6, lag_b=True)
        for n in (1, 2):
            at, (ta, pa), (tb, pb) = self.round(n, spread=2, ticks=60, lag_b=True)
            self.assertEqual((ta, tb), (at, at))
            self.assertEqual(pa, pb)
        self.assertNotIn("read after their step", self.A.log() + self.B.log())


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

    def parts(self, r):
        return {k: v for k, v in r.parts.items()}

    def test_a_spread_round_gives_the_whole_rounds_parts_and_reads_at_most_a_chunk_a_step(self):
        r, per_step = self.spread(250, 64)
        self.assertEqual(sum(per_step), 4 * 250)
        self.assertTrue(all(k <= 64 for k in per_step), per_step)
        self.assertEqual(len(per_step), -(-1000 // 64))
        self.assertEqual(r.chunks, len(per_step))
        e, _reads = self.mk(250)
        whole, t = self.K.checksumParts(e, self.C.hashValue, None, False, None, self.C.fnv1a)
        self.assertEqual(self.parts(r), {k: v for k, v in whole.items()})
        self.assertEqual(t, 5000)
        self.assertTrue(r.parts["townBuildings"].startswith("250/"))
        self.assertEqual(len([k for k in r.parts.keys() if str(k).startswith("townBuildings:")]), 4)

    def test_the_chunk_grows_instead_of_the_round_in_a_large_world(self):
        r, per_step = self.spread(1000, 10, max_chunks=20)
        self.assertEqual(r.chunk, 200)
        self.assertEqual(len(per_step), 20)

    def test_one_entity_that_differs_is_detected_in_its_lane_and_its_town(self):
        r, _ = self.spread(250, 64)
        self.L.execute("LEVEL[30007] = 3")
        r2, _ = self.spread(250, 64)
        a, b = self.parts(r), self.parts(r2)
        self.assertEqual({k for k in a if a[k] != b[k]}, {"townBuildings", "townBuildings:4"})

    def test_a_failing_read_makes_its_lane_an_error_part_and_the_round_goes_on(self):
        e, _reads = self.mk(50)
        e.edge = self.L.eval("function(i) if i == 10020 then error('boom') end return { x0 = i, y0 = 0, x1 = 1, y1 = 0 } end")
        r = self.K.newChecksumRound(e, self.C.hashValue, False, self.L.table_from({"chunk": 16}))
        while not r.done:
            self.K.checksumRoundStep(r)
        self.assertEqual(r.parts["edges"], "error")
        self.assertNotEqual(r.parts["townBuildings"], "error")
        self.assertIn("boom", r.errors["edges"])

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
        whole, _t = self.K.checksumParts(e, self.C.hashValue, None, False, None, self.C.fnv1a)
        self.assertEqual({k: v for k, v in rec.parts.items()}, {k: v for k, v in whole.items()})
        self.assertEqual(steps, -(-1000 // 64))
        self.assertEqual(list(self.K.unpackIds("5,6,70").values()), [5, 6, 70])
        self.assertEqual(self.K.packIds(self.L.table_from([3, 4])), "3,4")

    def test_the_marks_of_a_spread_round_are_its_chunk_steps(self):
        it = self.K.checksumItem(self.L.table_from({"n": 3, "at": 4000, "paused": False, "spread": 2}))
        marks = list(self.K.checksumMarks(it).values())
        self.assertEqual(marks[:3], [4400, 4800, 5200])
        self.assertEqual(len(marks), 48)
        for p in ({"n": 3, "at": 4000, "paused": False}, {"n": 3, "at": 4000, "paused": True, "spread": 2},
                  {"n": 3, "at": 4000, "paused": False, "full": False, "spread": 2}):
            self.assertEqual(len(self.K.checksumMarks(self.K.checksumItem(self.L.table_from(p)))), 0, p)


class FnvNativeXorTest(unittest.TestCase):
    """C.fnv1a with the state's native xor (bit32.bxor in Lua 5.2, LuaJIT's bit.bxor) gives the pure-Lua values bit for bit."""

    def load(self, hide=()):
        L = lupa.LuaRuntime(unpack_returned_tuples=True)
        for name in hide:
            L.execute("%s = nil" % name)
        return L, T.load_common(L)

    def strings(self):
        import random
        rnd = random.Random(20261018)
        out = [b"", bytes(range(256)), bytes(range(255, -1, -1)) * 40, b"\x00" * 1000, b"\xff" * 999]
        for _ in range(400):
            out.append(bytes(rnd.randrange(256) for _ in range(rnd.randrange(0, 300))))
        out.append(bytes(rnd.randrange(256) for _ in range(200000)))
        return out

    def test_the_native_and_the_pure_paths_agree_on_every_byte_and_long_strings(self):
        Ln, Cn = self.load()
        Lp, Cp = self.load(hide=("bit32", "bit"))
        self.assertEqual(Cp.FNV_XOR, "lua")
        self.assertIn(Cn.FNV_XOR, ("bit32", "bit"), "lupa's Lua 5.2 has bit32")
        for s in self.strings():
            self.assertEqual(Cn.fnv1a(s), Cp.fnv1a(s), len(s))
        self.assertEqual(Cn.fnv1a(b"a"), 0xE40C292C)
        self.assertEqual(Cp.fnv1a(b"foobar"), 0xBF9CF968)

    def test_a_library_that_disagrees_is_not_used(self):
        L = lupa.LuaRuntime(unpack_returned_tuples=True)
        L.execute("bit32 = { bxor = function(a, b) return a end }")
        C = T.load_common(L)
        self.assertEqual(C.FNV_XOR, "lua")
        self.assertEqual(C.fnv1a(b"a"), 0xE40C292C)

    def test_speed(self):
        import time
        s = bytes(range(256)) * 400   # 100 KB, about one heavy lane of a big world serialised
        res = {}
        for label, hide in (("native", ()), ("pure", ("bit32", "bit"))):
            _L, C = self.load(hide)
            t0 = time.perf_counter()
            C.fnv1a(s)
            res[label] = time.perf_counter() - t0
        if os.environ.get("FEVERLINK3_TEST_VERBOSE"):
            print("\nfnv1a 100 KB: native %.1f ms, pure %.1f ms (x%.1f)" % (res["native"] * 1000, res["pure"] * 1000, res["pure"] / res["native"]))
        self.assertLess(res["native"], res["pure"])


if __name__ == "__main__":
    unittest.main()
