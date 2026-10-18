"""Tests for the live-vs-loaded log (feverlink3_livevsloaded.lua): the switch, L1 lines, the L2 hashes (ordered vs sorted),
the resume detection and the sampling window, the work cap, never raising, switching off after errors, L4, and the
bridge wiring.

Run:  python -m unittest discover -s tests/lua -p "test_livevsloaded.py"
"""
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import test_lockstep as T  # noqa: E402  (only for the lupa import and the Game harness)

lupa = T.lupa
CONTENT = os.path.join(HERE, "..", "..", "mod", "feverlink3", "content")


def read(name):
    with open(os.path.join(CONTENT, name), encoding="utf-8") as f:
        return f.read()


SETUP = r'''
WRITES, NOW, WALL, CALLS = {}, 0, 100, 0
TOWNS = { 5, 3, 9 }
USAGE = { [3] = 0.25, [5] = 0.5, [9] = 0.125 }
DEST = { [40] = { 7, 3, 5 }, [10] = { 1, 2 }, [20] = {} }
NETS = { 8, 6 }
LINES = { 30, 31, 32, 33 }
FAIL = {}
CALLLOG = {}
local function may(name) if FAIL[name] then error("boom " .. name) end end
ACC = {
  towns = function() return TOWNS end,
  networks = function() return NETS end,
  lines = function() return LINES end,
  lineUsage = function(e) may("lineUsage") CALLS = CALLS + 1 return USAGE[e] end,
  supplyDemand = function(e) may("supplyDemand") CALLS = CALLS + 1 return { { e, 10 }, { 2, 3 }, { 0, 1.5 } } end,
  xp = function(e) may("xp") return 1000 + e, 4 end,
  count = function() may("count") return 77 end,
  destinations = function() may("destinations") local out = {} for k in pairs(DEST) do out[#out + 1] = k end return out end,
  forDestination = function(d) may("dest") CALLS = CALLS + 1 CALLLOG[#CALLLOG + 1] = d return DEST[d] end,
  forNetwork = function(n) may("tn") CALLS = CALLS + 1 return { n, n + 1 } end,
  forLine = function(l) may("line") CALLS = CALLS + 1 return { l } end,
}
function new(extra)
  local o = { enabled = true, acc = ACC, towns = nil, clock = function() return NOW end, wall = function() return WALL end,
              bits32 = function(x) return string.format("b%s", tostring(x)) end,
              write = function(text) WRITES[#WRITES + 1] = text return true end }
  for k, v in pairs(extra or {}) do o[k] = v end
  return LVL.new(o)
end
function written() return table.concat(WRITES) end
'''


class LvlTest(unittest.TestCase):
    def setUp(self):
        self.lua = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.lua.globals().SRC = read("feverlink3_livevsloaded.lua")
        self.lua.execute("LVL = assert(load(SRC, 'lvl', 't'))()")
        self.lua.execute(SETUP)

    def run_lua(self, code):
        return self.lua.execute(code)

    def lines(self):
        return [l for l in self.lua.eval("written()").splitlines() if l]

    def grep(self, text):
        return [l for l in self.lines() if text in l]

    # ---- the switch

    def test_off_is_inert(self):
        self.run_lua("t = LVL.new({ enabled = false, acc = ACC, write = function(x) WRITES[#WRITES + 1] = x end })")
        self.run_lua("t.step(1); t.step(2); t.noteResume(2, 'x'); t.flush(true)")
        self.assertEqual([], self.lines())
        self.assertEqual(0, self.lua.eval("CALLS"))
        self.assertFalse(self.lua.eval("t.enabled"))

    def test_parse_towns(self):
        r = self.lua.eval("function(s) local p = LVL.parseTowns(s) return p.all, p.ids[12] end")
        self.assertEqual((True, None), tuple(r(None)))
        self.assertEqual((True, None), tuple(r("all")))
        self.assertEqual((False, True), tuple(r("12, 14")))

    # ---- hashes

    def test_ordered_vs_sorted_hash(self):
        self.run_lua("o1, s1 = LVL.hashes({ 3, 1, 2 }); o2, s2 = LVL.hashes({ 1, 2, 3 }); o3, s3 = LVL.hashes({ 1, 2, 4 })")
        o1, s1, o2, s2, o3, s3 = (self.lua.eval(n) for n in ("o1", "s1", "o2", "s2", "o3", "s3"))
        self.assertEqual(s1, s2, "the same set sorts to the same hash")
        self.assertNotEqual(o1, o2, "another order changes the ordered hash")
        self.assertNotEqual(s2, s3, "another set changes the sorted hash")
        self.assertEqual(16, len(o1))

    def test_lowest_ids(self):
        self.assertEqual([1, 2], list(self.lua.eval("LVL.lowest({ 9, 1, 'x', 4, 2 }, 2)").values()))

    # ---- L1

    def test_l1_lines_every_12_ticks_per_town(self):
        self.run_lua("t = new(); for tick = 24, 30 do t.step(tick) end t.flush(true)")
        l1 = self.grep(" L1 ")
        self.assertEqual(3, len(l1), l1)
        self.assertTrue(all(l.startswith("t=24 ") or l.startswith("t=25 ") or l.startswith("t=26 ") for l in l1), l1)
        self.assertIn("t=24 L1 town=3 lu=b0.25 lun=0.25 sd=3/10,2/3,0/1.5 xp=1003 lvl=4", l1)

    def test_l1_only_traced_towns(self):
        self.run_lua("t = new({ towns = '5' }); for tick = 24, 30 do t.step(tick) end t.flush(true)")
        l1 = self.grep(" L1 ")
        self.assertEqual(1, len(l1))
        self.assertIn("town=5", l1[0])

    def test_l1_missing_xp_prints_dash(self):
        self.run_lua("FAIL.xp = true; t = new(); for tick = 24, 30 do t.step(tick) end t.flush(true)")
        l1 = self.grep(" L1 ")
        self.assertTrue(all(" xp=- lvl=-" in l for l in l1), l1)
        self.assertEqual(3, len(l1))
        self.assertTrue(self.grep("source xp disabled"))

    # ---- L2

    def test_l2_at_the_resume_and_every_60_ticks_for_five_minutes(self):
        self.run_lua("t = new(); for tick = 100, 1700 do t.step(tick) end t.flush(true)")
        samples = [int(l.split()[0][2:]) for l in self.grep(" L2 sample")]
        self.assertEqual([100] + [x for x in range(120, 1601, 60)], samples)  # window = 100 + 1500
        self.assertEqual(1, len(self.grep("resume why=start")))

    def test_l2_lines_and_choice_of_ids(self):
        self.run_lua("t = new(); for tick = 100, 112 do t.step(tick) end t.flush(true)")
        dest = self.grep(" L2 dest ")
        self.assertEqual(3, len(dest))
        ids = [l.split("id=")[1].split()[0] for l in dest]
        self.assertEqual(["10", "20", "40"], ids)  # the lowest ids in ascending order, whatever pairs() gave
        self.assertEqual(2, len(self.grep(" L2 tn ")))
        line_ids = [l.split("id=")[1].split()[0] for l in self.grep(" L2 line ")]
        self.assertEqual(["30", "31", "32"], line_ids)  # 3 lines of 4
        self.assertTrue(self.grep(" L2 count n=77"))
        d40 = [l for l in dest if "id=40 " in l][0]
        self.assertIn(" n=3 ", d40)

    def test_order_difference_shows_in_ordered_hash_only(self):
        self.run_lua("t = new(); t.step(100); t.step(101); t.flush(true); A = written(); WRITES = {}")
        self.run_lua("DEST[40] = { 3, 5, 7 }; t2 = new(); t2.step(100); t2.step(101); t2.flush(true); B = written()")
        a = [l for l in self.lua.eval("A").splitlines() if "id=40 " in l and "L2 dest" in l][0]
        b = [l for l in self.lua.eval("B").splitlines() if "id=40 " in l and "L2 dest" in l][0]
        fa, fb = dict(p.split("=") for p in a.split()[3:]), dict(p.split("=") for p in b.split()[3:])
        self.assertEqual(fa["s"], fb["s"])
        self.assertNotEqual(fa["o"], fb["o"])

    def test_work_per_step_is_capped(self):
        self.run_lua("t = new(); local worst = 0; for tick = 120, 135 do local c0 = CALLS; t.step(tick); worst = math.max(worst, CALLS - c0) end WORST = worst")
        # a step does at most JOBS_PER_STEP jobs; one L1 job calls the API twice
        self.assertLessEqual(self.lua.eval("WORST"), 2 * int(self.lua.eval("LVL.JOBS_PER_STEP")))

    def test_a_wall_clock_gap_is_a_resume(self):
        self.run_lua("t = new(); t.step(100); WALL = WALL + 1; t.step(101); WALL = WALL + 30; t.step(102); t.flush(true)")
        self.assertEqual(1, len(self.grep("resume why=gap")))
        self.assertTrue(self.grep("t=102 resume why=gap"))

    # ---- never raises, switches off

    def test_a_failing_source_is_disabled_without_counting_as_an_error(self):
        self.run_lua("FAIL.dest = true; FAIL.line = true; t = new(); for tick = 100, 400 do t.step(tick) end t.flush(true)")
        self.assertTrue(self.lua.eval("t.enabled"))
        self.assertTrue(self.grep("source L2dest disabled"))
        self.assertTrue(self.grep(" L2 dest id=10 err"))
        self.assertTrue(self.grep(" L1 "), "the other sources keep running")

    def test_everything_failing_never_raises(self):
        self.run_lua("for _, k in ipairs({ 'lineUsage', 'supplyDemand', 'xp', 'count', 'destinations', 'dest', 'tn', 'line' }) do FAIL[k] = true end")
        self.run_lua("ACC.towns = function() error('x') end; t = new(); for tick = 100, 400 do t.step(tick) end t.flush(true)")
        self.assertTrue(self.lua.eval("t.enabled"))

    def test_internal_errors_switch_it_off_after_five(self):
        # a write function that is fine, but a clock that raises: the step itself errors
        self.run_lua("BAD = false; t = new({ wall = function() if BAD then error('wall') end return 100 end })")
        self.run_lua("t.step(1); BAD = true; for tick = 2, 20 do t.step(tick) end")
        self.assertFalse(self.lua.eval("t.enabled"))
        self.assertEqual(5, self.lua.eval("t.errors"))
        self.run_lua("n0 = #WRITES; t.step(99); t.flush(true)")
        self.assertEqual(self.lua.eval("n0"), self.lua.eval("#WRITES"), "nothing after the switch-off")
        self.assertTrue(self.grep("switched off after 5 errors"))

    def test_a_failing_write_never_raises(self):
        self.run_lua("t = new({ write = function() error('disk') end }); for tick = 100, 400 do t.step(tick) end t.flush(true)")
        self.assertTrue(self.lua.eval("t.enabled"))

    def test_same_tick_twice_does_nothing_twice(self):
        self.run_lua("t = new(); t.step(120); n = CALLS; t.step(120); t.flush(true)")
        self.assertEqual(self.lua.eval("n"), self.lua.eval("CALLS"))

    # ---- L4

    def test_l4_waits_for_the_override_and_logs_once(self):
        self.run_lua("READY = false; t = new({ randomProbe = function() if not READY then return nil end return { installed = 1, rng_n = 3 } end })")
        self.run_lua("t.step(100); t.step(101); READY = true; t.step(102); t.step(103); t.flush(true)")
        l4 = self.grep(" L4 ")
        self.assertEqual(["t=102 L4 installed=1 rng_n=3"], l4)

    def test_l4_without_a_probe(self):
        self.run_lua("t = new(); t.step(100); t.flush(true)")
        self.assertEqual(["t=100 L4 probe=none"], self.grep(" L4 "))

    # ---- the real accessors against a mocked engine

    def test_the_api_accessors(self):
        self.run_lua(r'''
          ids = { TOWN = { 4, 2 }, LINE = { 9 }, TRANSPORT_NETWORK = { 6 } }
          LUS = {}
          API = { type = { ComponentType = { GAME_SCRIPT = 'GS' },
                           enum = { LandUseType = { RESIDENTIAL = 'res', COMMERCIAL = 'com', INDUSTRIAL = 'ind' } } }, engine = {
            getComponent = function(e, c) return { state = { townState = { [2] = { experience = 12, level = 3 } } } } end,
            entityExists = function(e) return e == 1 end,
            util = { town = { getTownLineUsage = function() return 0.5 end } },
            system = {
              gameScriptSystem = { getEntityForGameScript = function() return 1 end },
              townBuildingSystem = { getCargoSupplyAndLimit = function(e, lu) LUS[#LUS + 1] = lu return { [2] = { 1, 4 }, [1] = { 2, 5 } } end },
              simPersonSystem = { getCount = function() return 5 end, getDestination2SpMap = function() return { [8] = {}, [3] = {} } end,
                getSimPersonsForDestination = function() return { 1, 2 } end, getSimPersonsAtTerminalForTransportNetwork = function() return { 3 } end,
                getSimPersonsForLine = function() return { 4, 5, 6 } end } } } }
          A = LVL.apiAccessors(API, function(n) return ids[n] end)
          XP, LV = A.xp(2)
          SD = A.supplyDemand(2)
        ''')
        self.assertEqual((12, 3), (self.lua.eval("XP"), self.lua.eval("LV")))
        self.assertEqual((3, 9), (self.lua.eval("SD[1][1]"), self.lua.eval("SD[1][2]")))
        # the land uses are the enum's values (getCargoSupplyAndLimit's typed LandUseType), in the documented order
        self.assertEqual("res,com,ind", self.lua.eval("table.concat(LUS, ',')"))
        # a game without the town game script: no entity, no component read of it
        self.lua.execute("API.engine.system.gameScriptSystem.getEntityForGameScript = function() return -1 end; "
                         "API.engine.getComponent = function() error('read of a missing entity') end")
        self.assertIsNone(self.lua.eval("A.xp(2)"))
        # no LandUseType enum: the accessor raises (the caller guards), it does not guess integers
        self.lua.execute("API.type.enum = nil")
        self.assertFalse(self.lua.eval("(pcall(A.supplyDemand, 2))"))
        self.assertEqual(5, self.lua.eval("A.count()"))
        self.assertEqual(2, self.lua.eval("#A.destinations()"))
        self.assertEqual(3, self.lua.eval("#A.forLine(1)"))
        self.assertEqual(1, self.lua.eval("#A.forNetwork(1)"))
        self.assertEqual(2, self.lua.eval("#A.forDestination(1)"))


class InBridgeTest(unittest.TestCase):
    """The real bridge in the lockstep harness: the switch and the file in the game's own folder."""

    def setUp(self):
        import tempfile

        self.tmp = tempfile.mkdtemp(prefix="fl3-lvl-")
        self.old = os.environ.get("FEVERLINK3_LIVE_VS_LOADED")

    def tearDown(self):
        import shutil

        if self.old is None:
            os.environ.pop("FEVERLINK3_LIVE_VS_LOADED", None)
        else:
            os.environ["FEVERLINK3_LIVE_VS_LOADED"] = self.old
        shutil.rmtree(self.tmp, ignore_errors=True)

    def game(self):
        g = T.Game("lvl", 0, self.tmp, 50000)
        g.use_env()
        return g

    def test_off_by_default_no_file(self):
        os.environ.pop("FEVERLINK3_LIVE_VS_LOADED", None)
        g = self.game()
        for _ in range(5):
            g.step()
        self.assertFalse(os.path.exists(os.path.join(g.dir, "live_vs_loaded.txt")))

    @T.full
    def test_on_writes_the_start_and_l4_and_never_breaks_the_game(self):
        import time

        os.environ["FEVERLINK3_LIVE_VS_LOADED"] = "1"
        g = self.game()
        for _ in range(4):
            g.step()  # the mocked engine lacks the sim person api: the sources are disabled, nothing raises
        time.sleep(1.1)
        g.step()
        text = T.read_text(os.path.join(g.dir, "live_vs_loaded.txt"))
        self.assertIn("# lvl start", text)
        self.assertIn(" resume why=start ", text)
        self.assertIn(" L4 ", text)


class BridgeWiringTest(unittest.TestCase):
    def test_the_bridge_names_the_module_the_switch_and_the_file(self):
        bridge = read("feverlink3_bridge.script.lua")
        self.assertIn("FEVERLINK3_LIVE_VS_LOADED", bridge)
        self.assertIn("feverlink3_livevsloaded.lua", bridge)
        self.assertIn("livevsloadedGuard", bridge)
        self.assertIn("feverlink3_livevsloaded.lua", read("../_content.json"))


if __name__ == "__main__":
    unittest.main()
