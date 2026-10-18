"""Tests for the town trace (feverlink3_towntrace.lua): writes only when a traced town changed, the diff content, no
write for an unchanged town, buffering, the `all` sampling, and the real-api accessors against a mocked engine.

Run:  python -m unittest discover -s tests/lua -p "test_towntrace.py"
"""
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import test_lockstep as T  # noqa: E402  (only for the lupa import)

lupa = T.lupa
CONTENT = os.path.join(HERE, "..", "..", "mod", "feverlink3", "content")


def read(name):
    with open(os.path.join(CONTENT, name), encoding="utf-8") as f:
        return f.read()


# A mocked world: BUILDINGS[entity] = {town, construction, level, depth, parcels, timeBuilt}, CONS[c] = {file,x,y,z},
# TOWNS[id] = component. WRITES collects every append, CLOCK is the wall clock.
SETUP = r'''
BUILDINGS, CONS, TOWNS, WRITES, NOW = {}, {}, {}, {}, 0
function ids(t) local out = {} for k in pairs(t) do out[#out + 1] = k end table.sort(out) return out end
ACC = {
  buildings = function() return ids(BUILDINGS) end,
  building = function(e) return BUILDINGS[e] end,
  construction = function(c) return CONS[c] end,
  town = function(t) return TOWNS[t] end,
}
function hash(list)
  local h = 5381
  for _, l in ipairs(list) do for i = 1, #l do h = (h * 33 + l:byte(i)) % 4294967296 end end
  return string.format("%08x", h)
end
function add(e, town, file, x, level, depth, parcels, built)
  BUILDINGS[e] = { town = town, construction = e + 1000, level = level or 1, depth = depth or 1, parcels = parcels or 2, timeBuilt = built or 0 }
  CONS[e + 1000] = { file = file, x = x, y = 5, z = 0 }
end
function new(spec)
  return TT.new({ spec = spec, acc = ACC, hash = hash, clock = function() return NOW end,
                  write = function(text) WRITES[#WRITES + 1] = text return true end })
end
function written() return table.concat(WRITES) end
'''


class TraceTest(unittest.TestCase):
    def setUp(self):
        self.lua = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.lua.execute("function ug_require(n) error('no ' .. n) end")
        self.lua.globals().SRC = read("feverlink3_towntrace.lua")
        self.lua.execute("TT = assert(load(SRC, 'towntrace', 't'))()")
        self.lua.execute(SETUP)

    def run_lua(self, code):
        return self.lua.execute(code)

    def text(self):
        return self.lua.eval("written()")

    def lines(self):
        return [l for l in self.text().splitlines() if l]

    # ---- the switch

    def test_spec_parsing(self):
        r = self.lua.eval("function(s) local p = TT.parseSpec(s) return p.enabled, p.all, #p.list, p.ids[60479] end")
        self.assertEqual((False, False, 0, None), tuple(r(None)))
        self.assertEqual((False, False, 0, None), tuple(r("")))
        self.assertEqual((False, False, 0, None), tuple(r("nonsense, x1")))
        self.assertEqual((True, False, 2, True), tuple(r(" 60479 , 60487,60479 ")))
        self.assertEqual((True, True, 0, None), tuple(r("ALL")))

    def test_off_means_inert_and_never_writes(self):
        self.run_lua("t = new(nil); add(1, 7, 'a.mdl', 0); t.step(1); t.noteCommand(1, 'x'); t.flush(true)")
        self.assertFalse(self.lua.eval("t.enabled"))
        self.assertEqual("", self.text())

    # ---- changes only

    def test_the_baseline_lists_the_buildings_once_and_an_unchanged_town_is_not_written_again(self):
        self.run_lua("add(1, 7, 'h.mdl', 10, 2, 3, 4, 99); add(2, 7, 'g.mdl', 20); add(3, 8, 'other.mdl', 5)")
        self.run_lua("t = new('7'); t.step(100); t.flush(true)")
        out = self.lines()
        self.assertTrue(out[0].startswith("# town_trace start tick=100"), out)
        self.assertRegex(out[1], r"^t=100 town=7 n=2 h=\S+ base$")
        self.assertEqual(["+ g.mdl@20.0,5.0,0.0 L=1 D=1 P=2 T=0", "+ h.mdl@10.0,5.0,0.0 L=2 D=3 P=4 T=99"], out[2:4])
        self.assertNotIn("town=8", self.text(), "an untraced town is never written")
        size = len(WRITES_LEN(self))
        for tick in range(101, 160):
            self.run_lua("t.step(%d)" % tick)
        self.run_lua("t.flush(true)")
        later = [l for l in self.lines() if "info" not in l]
        self.assertEqual(size, len(later), "nothing changed, nothing written: %s" % later[size:])

    def test_the_same_tick_twice_does_nothing(self):
        self.run_lua("add(1, 7, 'h.mdl', 10); t = new('7'); t.step(5); add(2, 7, 'n.mdl', 11); t.step(5); t.flush(true)")
        self.assertIn("n=1", self.text())
        self.assertNotIn("n=2", self.text())

    def test_an_added_building_is_a_plus_line_after_a_header_with_the_new_count(self):
        self.run_lua("add(1, 7, 'h.mdl', 10); t = new('7'); t.step(1); add(2, 7, 'n.mdl', 30, 1, 2, 3, 555); t.step(2); t.flush(true)")
        out = self.lines()
        i = [k for k, l in enumerate(out) if l.startswith("t=2 town=7 n=2")][0]
        self.assertEqual("+ n.mdl@30.0,5.0,0.0 L=1 D=2 P=3 T=555", out[i + 1])
        self.assertNotIn("base", out[i])

    def test_a_removed_building_is_a_minus_line(self):
        self.run_lua("add(1, 7, 'h.mdl', 10); add(2, 7, 'n.mdl', 30); t = new('7'); t.step(1); BUILDINGS[2] = nil; t.step(2); t.flush(true)")
        out = self.lines()
        i = [k for k, l in enumerate(out) if l.startswith("t=2 town=7 n=1")][0]
        self.assertEqual("- n.mdl@30.0,5.0,0.0 L=1 D=1 P=2 T=0", out[i + 1])

    def test_an_upgrade_in_place_shows_only_the_changed_fields_as_old_greater_new(self):
        self.run_lua("add(1, 7, 'h.mdl', 10, 1, 2, 3, 100); t = new('7'); t.step(1); BUILDINGS[1].level = 2; BUILDINGS[1].timeBuilt = 250; t.step(2); t.flush(true)")
        out = self.lines()
        i = [k for k, l in enumerate(out) if l.startswith("t=2 town=7 n=1")][0]
        self.assertEqual("~ h.mdl@10.0,5.0,0.0 L=1>2 D=2 P=3 T=100>250", out[i + 1])

    def test_a_building_that_is_replaced_by_another_at_the_same_place_is_one_changed_line(self):
        self.run_lua("add(1, 7, 'h.mdl', 10); t = new('7'); t.step(1); BUILDINGS[1] = nil; add(9, 7, 'h.mdl', 10, 1, 1, 2, 300); t.step(2); t.flush(true)")
        out = self.lines()
        i = [k for k, l in enumerate(out) if l.startswith("t=2 town=7")][0]
        self.assertEqual("~ h.mdl@10.0,5.0,0.0 L=1 D=1 P=2 T=0>300", out[i + 1])

    def test_entity_ids_never_enter_the_text_or_the_hash(self):
        # two worlds with the same buildings under other entity ids give the same hash
        self.run_lua("add(1, 7, 'h.mdl', 10); add(2, 7, 'g.mdl', 20); a = new('7'); a.step(1); a.flush(true)")
        first = self.text()
        self.run_lua("BUILDINGS, CONS, WRITES = {}, {}, {}; add(501, 7, 'g.mdl', 20); add(77, 7, 'h.mdl', 10); b = new('7'); b.step(1); b.flush(true)")
        self.assertEqual(first, self.text())

    def test_two_identical_buildings_at_one_place_are_both_counted(self):
        self.run_lua("add(1, 7, 'h.mdl', 10); add(2, 7, 'h.mdl', 10); t = new('7'); t.step(1); t.flush(true)")
        out = self.lines()
        self.assertTrue(any(l.startswith("t=1 town=7 n=2") for l in out), out)
        self.assertEqual(1, len([l for l in out if l.startswith("+ h.mdl@10.0,5.0,0.0#2")]), out)

    def test_a_town_without_buildings_is_traced_too(self):
        self.run_lua("t = new('7'); t.step(1); add(1, 7, 'h.mdl', 10); t.step(2); t.flush(true)")
        out = self.lines()
        self.assertRegex(out[1], r"^t=1 town=7 n=0 h=\S+ base$")
        self.assertTrue(any(l.startswith("t=2 town=7 n=1") for l in out), out)

    # ---- the town component summary

    def test_info_is_written_on_change_and_every_five_seconds(self):
        self.run_lua("add(1, 7, 'h.mdl', 10); TOWNS[7] = {developmentActive = true, sizeFactors = {1, 0.5, 0.25}, cargoNeeds = {{{1, 2}}, {}, {}}, noise = 3.14159265}")
        self.run_lua("t = new('7'); t.step(10); t.flush(true)")
        info = [l for l in self.lines() if " info " in l]
        self.assertEqual(1, len(info))
        self.assertIn("dev=1", info[0])
        self.assertIn("size=[1,0.5,0.25]", info[0])
        self.assertIn("needs=[[[1,2]],[],[]]", info[0])
        self.assertIn("noise=3.142", info[0])
        for tick in range(11, 34):
            self.run_lua("t.step(%d)" % tick)
        self.run_lua("t.flush(true)")
        self.assertEqual(1, len([l for l in self.lines() if " info " in l]), "unchanged and under 25 ticks")
        self.run_lua("t.step(35); t.flush(true)")
        self.assertEqual(2, len([l for l in self.lines() if " info " in l]), "refreshed after 25 ticks")
        self.run_lua("TOWNS[7].developmentActive = false; t.step(36); t.flush(true)")
        info = [l for l in self.lines() if " info " in l]
        self.assertEqual(3, len(info))
        self.assertIn("dev=0", info[-1])

    # ---- commands

    def test_commands_are_listed_with_tick_name_town_and_player_flag(self):
        self.run_lua("t = new('7'); t.noteCommand(12, 'townUpdateSize', 7, false); t.noteCommand(13, 'onPreBuildProposal'); t.flush(true)")
        out = self.lines()
        self.assertIn("t=12 cmd townUpdateSize town=7 pi=0", out)
        self.assertIn("t=13 cmd onPreBuildProposal", out)

    # ---- cost and buffering

    def test_writes_are_buffered_until_a_second_or_a_few_hundred_lines(self):
        self.run_lua("add(1, 7, 'h.mdl', 10); t = new('7'); t.step(1)")
        self.assertEqual(0, int(self.lua.eval("#WRITES")), "nothing written yet")
        self.run_lua("NOW = 0.5; t.step(2)")
        self.assertEqual(0, int(self.lua.eval("#WRITES")))
        self.run_lua("NOW = 1.2; add(2, 7, 'n.mdl', 3); t.step(3)")
        self.assertEqual(1, int(self.lua.eval("#WRITES")), "one append for everything buffered")
        self.assertIn("n=2", self.text())

    def test_a_failing_write_never_raises(self):
        self.run_lua("add(1, 7, 'h.mdl', 10); t = TT.new({spec='7', acc=ACC, hash=hash, clock=function() return NOW end, write=function() error('disk full') end}); t.step(1); t.flush(true); t.step(2)")

    # ---- all

    def test_all_mode_samples_every_fifth_tick_and_lists_no_baseline_buildings(self):
        self.run_lua("add(1, 7, 'h.mdl', 10); add(2, 8, 'g.mdl', 20); t = new('all'); for k = 1, 4 do t.step(k) end; t.flush(true)")
        self.assertEqual("", "".join(l for l in self.lines() if l.startswith("t=")), "ticks 1-4 are skipped")
        self.run_lua("t.step(5); t.flush(true)")
        out = self.lines()
        self.assertTrue(any(l.startswith("t=5 town=7 n=1") for l in out), out)
        self.assertTrue(any(l.startswith("t=5 town=8 n=1") for l in out), out)
        self.assertFalse(any(l.startswith("+ ") for l in out), "the baseline of `all` names no buildings")
        self.run_lua("add(3, 8, 'n.mdl', 40); t.step(7); t.step(10); t.flush(true)")
        out = self.lines()
        self.assertTrue(any(l.startswith("t=10 town=8 n=2") for l in out), out)
        self.assertIn("+ n.mdl@40.0,5.0,0.0 L=1 D=1 P=2 T=0", out)
        self.assertFalse(any(l.startswith("t=10 town=7") and " info " not in l for l in out), "town 7 did not change")

    # ---- the real api accessors, against a mocked engine

    def test_the_api_accessors_read_the_components_and_leave_entity_ids_out(self):
        self.run_lua(r'''
          CT = { TOWN_BUILDING = 1, CONSTRUCTION = 2, TOWN = 3 }
          local comps = {
            [10] = { [1] = { town = 7, construction = 20, level = 3, depth = 2, parcels = { 1, 2, 3 }, timeBuilt = 42 } },
            [20] = { [2] = { fileName = 'res/x.mdl', transf = { [13] = 1.25, [14] = 2.5, [15] = 3 } } },
            [7] = { [3] = { developmentActive = true, sizeFactors = { 1 }, noise = 2 } },
          }
          API = { type = { ComponentType = CT },
                  engine = { getComponent = function(e, t) return comps[e] and comps[e][t] end,
                             util = { town = { getTownEmissionDB = function(e) return 55 end } } } }
          A = TT.apiAccessors(API, function(name) if name == 'TOWN_BUILDING' then return { 10 } end return {} end)
          t = TT.new({ spec = '7', acc = A, hash = hash, clock = function() return NOW end, write = function(x) WRITES[#WRITES + 1] = x return true end })
          t.step(1); t.flush(true)
        ''')
        out = self.lines()
        self.assertTrue(any(l.startswith("t=1 town=7 n=1") for l in out), out)
        self.assertIn("+ res/x.mdl@1.2,2.5,3.0 L=3 D=2 P=3 T=42", out)
        info = [l for l in out if " info " in l][0]
        self.assertIn("dev=1", info)
        self.assertIn("emis=55", info, "read through api.engine.util.town.getTownEmissionDB, where the API has it")
        self.assertNotIn("10", "".join(l for l in out if l.startswith("+ ")), "no entity id in a building line")


CMD_SETUP = r"""
TICK = 100
ACC.updateCount = function() return TICK * 2 end
ACC.lineUsage = function(t) return 0.1 + t / 1000 end
ACC.supplyDemand = function(t) return { {1.5, 3}, {2, 4}, {0, 1} } end
ORDER_T, ORDER_C = { 7, 8, 9 }, { 1, 2, 3 }
ACC.townOrder = function() return ORDER_T end
ACC.cargoOrder = function() return ORDER_C end
function newc(spec)
  return TT.new({ spec = spec, cmds = true, acc = ACC, hash = hash, tick = function() return TICK end,
                  clock = function() return NOW end, write = function(text) WRITES[#WRITES + 1] = text return true end })
end
CALLS = {}
APICMD = {
  makeTownUpdateSizeCmd = function(...) CALLS[#CALLS + 1] = table.pack(...) return "cmdA", nil, 42 end,
  makeTownUpdateCargoNeedsCmd = function(...) CALLS[#CALLS + 1] = table.pack(...) return "cmdB" end,
  makeOther = function() return "o" end,
}
"""


class CmdTraceTest(unittest.TestCase):
    def setUp(self):
        self.lua = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.lua.execute("function ug_require(n) error('no ' .. n) end")
        self.lua.globals().SRC = read("feverlink3_towntrace.lua")
        self.lua.execute("TT = assert(load(SRC, 'towntrace', 't'))()")
        self.lua.execute(SETUP)
        self.lua.execute(CMD_SETUP)
        self.lua.execute("TOWNS[7] = {developmentActive = true, sizeFactors = {0.5, 1, 0.25}, initialLandUseCapacities = {10, 20, 30}, capacityScalingFactor = {1, 1, 1}, cargoNeeds = {{1}, {}, {}}}")
        self.lua.execute("add(1, 7, 'h.mdl', 10)")

    def lines(self):
        return [l for l in self.lua.eval("written()").splitlines() if l]

    def mk(self):
        return [l for l in self.lines() if " mk " in l]

    def wrap(self, spec="7"):
        self.lua.execute("t = newc('%s'); H = { tracer = t }; N = TT.installCmdWrappers(APICMD, H)" % spec)

    def test_bits_are_exact(self):
        r = self.lua.eval("function(x) return TT.bits32(x), TT.bits64(x) end")
        self.assertEqual(("3f800000", "3ff0000000000000"), tuple(r(1.0)))
        self.assertEqual(("3e800000", "3fd0000000000000"), tuple(r(0.25)))
        self.assertEqual(("bf000000", "bfe0000000000000"), tuple(r(-0.5)))
        # 0.1 is not a float32 number: the two widths differ
        self.assertEqual(("3dcccccd", "3fb999999999999a"), tuple(r(0.1)))
        self.assertEqual(("-", "-"), tuple(r(None)))

    def test_soft_bits_match_ieee_for_many_values(self):
        import random
        import struct

        rnd = random.Random(7)
        values = [0.0, 1.0, -1.0, 0.1, 1e-45, 1e-40, 3.4e38, 3.5e38, 1e300, 5e-324, 2.2250738585072014e-308, 16777217.0, 0.30000000000000004]
        values += [rnd.uniform(-2, 2) for _ in range(300)] + [rnd.random() * 10 ** rnd.randint(-30, 30) for _ in range(300)]
        soft = self.lua.eval("function(x, e, m) return TT.softBits(x, e, m) end")
        for x in values:
            self.assertEqual(struct.pack(">d", x).hex(), soft(x, 11, 52), x)
            try:
                want32 = struct.pack(">f", x).hex()
            except OverflowError:
                want32 = "7f800000" if x > 0 else "ff800000"
            self.assertEqual(want32, soft(x, 8, 23), x)
        self.assertEqual("80000000", soft(-0.0, 8, 23))

    def test_pass_through_is_exact_arguments_and_all_results(self):
        self.wrap()
        self.lua.execute("SIZE = {0.5, 1, 0.25}; R = table.pack(APICMD.makeTownUpdateSizeCmd(7, SIZE, false))")
        self.assertEqual("cmdA", self.lua.eval("R[1]"))
        self.assertIsNone(self.lua.eval("R[2]"))
        self.assertEqual(42, self.lua.eval("R[3]"))
        self.assertEqual(3, self.lua.eval("R.n"))
        self.assertEqual(1, self.lua.eval("#CALLS"))
        self.assertTrue(self.lua.eval("CALLS[1][2] == SIZE"), "the very same table object")
        self.assertEqual((3, 7, False), (self.lua.eval("CALLS[1].n"), self.lua.eval("CALLS[1][1]"), self.lua.eval("CALLS[1][3]")))
        # the other factory of the table is untouched, the two traced ones are wrapped
        self.assertEqual(2, self.lua.eval("N"))
        self.assertEqual("o", self.lua.eval("APICMD.makeOther()"))

    def test_the_call_is_logged_before_the_original_runs(self):
        self.lua.execute("t = newc('7'); H = { tracer = t }")
        self.lua.execute("APICMD.makeTownUpdateSizeCmd = function(...) t.flush(true); SEEN = #written() return 1 end")
        self.lua.execute("TT.installCmdWrappers(APICMD, H); APICMD.makeTownUpdateSizeCmd(7, {0.5, 1, 0.25}, false)")
        self.assertGreater(self.lua.eval("SEEN"), 0, "the line was already buffered when the original ran")
        self.assertEqual(1, len(self.mk()))

    def test_size_line_content_and_inputs(self):
        self.wrap()
        self.lua.execute("APICMD.makeTownUpdateSizeCmd(7, {0.5, 1, 0.1}, false); t.flush(true)")
        mk = self.mk()[0]
        self.assertEqual("t=100 uc=200 mk makeTownUpdateSizeCmd town=7 k=1 flag=0 "
                         "f32=3f000000,3f800000,3dcccccd f64=3fe0000000000000,3ff0000000000000,3fb999999999999a "
                         "v=0.5,1,0.10000000000000001", mk)
        inp = [l for l in self.lines() if " in k=1 " in l][0]
        self.assertTrue(inp.startswith("t=100 town=7 in k=1 lu=0.10700000000000001 sd=1.5/3,2/4,0/1 dev=1 size=[0.5,1,0.25] cap=[10,20,30] scale=[1,1,1] needs=[[1],[],[]] bld="), inp)

    def test_cargo_needs_line(self):
        self.wrap()
        self.lua.execute("APICMD.makeTownUpdateCargoNeedsCmd(7, {{1, 2}, {}, {3}}, true); t.flush(true)")
        mk = self.mk()[0]
        self.assertRegex(mk, r"^t=100 uc=200 mk makeTownUpdateCargoNeedsCmd town=7 k=1 flag=1 needs=\[\[1,2\],\[\],\[3\]\] nh=\S+$")

    def test_k_counts_calls_of_one_town_in_one_tick_and_restarts(self):
        self.wrap()
        self.lua.execute("for i = 1, 2 do APICMD.makeTownUpdateSizeCmd(7, {1, 1, 1}, false) end; TICK = 101; APICMD.makeTownUpdateSizeCmd(7, {1, 1, 1}, false); t.flush(true)")
        ks = [l.split(" k=")[1].split()[0] for l in self.mk()]
        self.assertEqual(["1", "2", "1"], ks)

    def test_untraced_town_not_logged_but_passed_through(self):
        self.wrap()
        self.lua.execute("R = APICMD.makeTownUpdateSizeCmd(8, {1, 1, 1}, false); t.flush(true)")
        self.assertEqual("cmdA", self.lua.eval("R"))
        self.assertEqual([], self.mk())

    def test_all_mode_logs_every_town(self):
        self.wrap("all")
        self.lua.execute("APICMD.makeTownUpdateSizeCmd(8, {1, 1, 1}, false); t.flush(true)")
        self.assertEqual(1, len(self.mk()))

    def test_userdata_like_sequences_are_read_by_index(self):
        self.wrap()
        self.lua.execute("""
          VEC = setmetatable({}, { __index = function(_, i) return ({0.5, 1, 0.25})[i] end, __len = function() return 3 end })
          APICMD.makeTownUpdateSizeCmd(7, VEC, false); t.flush(true)""")
        self.assertIn("v=0.5,1,0.25", self.mk()[0])

    def test_order_line_hash_changes_with_the_order_and_is_throttled(self):
        self.lua.execute("t = newc('7'); t.step(100); t.step(101); t.flush(true)")
        order = [l for l in self.lines() if " order " in l]
        self.assertEqual(1, len(order), order)
        self.assertRegex(order[0], r"^t=100 town=7 order pos=1 towns=3:\S+ cargos=3:\S+$")
        first = order[0].split("towns=")[1]
        self.lua.execute("ORDER_T = { 8, 7, 9 }; t.step(102); t.flush(true)")
        order = [l for l in self.lines() if " order " in l]
        self.assertEqual(2, len(order))
        self.assertIn("pos=2", order[1])
        self.assertNotEqual(first, order[1].split("towns=")[1])
        self.lua.execute("t.step(130); t.flush(true)")
        self.assertEqual(3, len([l for l in self.lines() if " order " in l]), "repeated after 25 ticks")

    def test_buffered_not_written_before_a_second(self):
        self.wrap()
        self.lua.execute("APICMD.makeTownUpdateSizeCmd(7, {1, 1, 1}, false)")
        self.assertEqual(0, int(self.lua.eval("#WRITES")))
        self.lua.execute("NOW = 1.5; APICMD.makeTownUpdateSizeCmd(7, {1, 1, 1}, false)")
        self.assertEqual(1, int(self.lua.eval("#WRITES")))

    def test_errors_switch_the_command_trace_off_but_never_break_the_call(self):
        self.wrap()
        # make the formatting itself fail: a hash that raises
        self.lua.execute("t = TT.new({ spec = '7', cmds = true, acc = ACC, hash = function() error('boom') end, tick = function() return TICK end, "
                         "clock = function() return NOW end, write = function(x) WRITES[#WRITES + 1] = x return true end }); H.tracer = t")
        self.lua.execute("for i = 1, 8 do R = APICMD.makeTownUpdateCargoNeedsCmd(7, {{1}}, false) end; t.flush(true)")
        self.assertEqual("cmdB", self.lua.eval("R"))
        self.assertEqual(8, self.lua.eval("#CALLS"), "the original ran every time")
        self.assertFalse(self.lua.eval("t.cmds"))
        self.assertTrue(any("switched off after 5 errors" in l for l in self.lines()), self.lines())
        before = len(self.lines())
        self.lua.execute("APICMD.makeTownUpdateCargoNeedsCmd(7, {{1}}, false); t.flush(true)")
        self.assertEqual(before, len(self.lines()))

    def test_a_raising_input_accessor_is_tolerated(self):
        self.wrap()
        self.lua.execute("ACC.lineUsage = function() error('no usage') end; APICMD.makeTownUpdateSizeCmd(7, {1, 1, 1}, false); t.flush(true)")
        inp = [l for l in self.lines() if " in k=1 " in l][0]
        self.assertIn("lu=err", inp)
        self.assertTrue(self.lua.eval("t.cmds"))

    def test_noop_when_the_env_is_off(self):
        # cmds not requested: wrappers pass through and write nothing
        self.lua.execute("t = new('7'); H = { tracer = t }; TT.installCmdWrappers(APICMD, H); R = APICMD.makeTownUpdateSizeCmd(7, {1, 1, 1}, false); t.step(1); t.flush(true)")
        self.assertEqual("cmdA", self.lua.eval("R"))
        self.assertEqual([], self.mk())
        self.assertEqual([], [l for l in self.lines() if " order " in l])
        # and without a tracer at all
        self.lua.execute("H.tracer = nil; R = APICMD.makeTownUpdateSizeCmd(7, {1, 1, 1}, false)")
        self.assertEqual("cmdA", self.lua.eval("R"))

    def test_installing_twice_does_not_wrap_the_wrapper(self):
        self.wrap()
        self.assertEqual(0, self.lua.eval("TT.installCmdWrappers(APICMD, H)"))
        self.lua.execute("APICMD.makeTownUpdateSizeCmd(7, {1, 1, 1}, false); t.flush(true)")
        self.assertEqual(1, len(self.mk()))
        self.assertEqual(1, self.lua.eval("#CALLS"))

    def test_the_api_accessors_for_the_inputs(self):
        self.lua.execute(r"""
          CT = { TOWN_BUILDING = 1, CONSTRUCTION = 2, TOWN = 3, GAME_TIME = 4 }
          LUS = {}
          API = { type = { ComponentType = CT, enum = { LandUseType = { RESIDENTIAL = 5, COMMERCIAL = 6, INDUSTRIAL = 7 } } },
                  engine = { getComponent = function(e, t) if t == CT.GAME_TIME then return { updateCount = 77 } end end,
                             util = { getWorld = function() return 1 end, town = { getTownLineUsage = function(e) return 0.5 end } },
                             forEachEntityWithComponent = function(fn, t) for _, e in ipairs({ 9, 3, 5 }) do fn(e) end end,
                             system = { townBuildingSystem = { getCargoSupplyAndLimit = function(e, lu) LUS[#LUS + 1] = lu return { [12] = { 1, 2 }, [3] = { 0.5, 1 } } end } } },
                  res = { cargoTypeRep = { getAll = function() return { [4] = 'x' } end } } }
          A = TT.apiAccessors(API, function() return {} end)
        """)
        self.assertEqual(3, self.lua.eval("#A.supplyDemand(1)"))
        self.assertEqual("5,6,7", self.lua.eval("table.concat(LUS, ',')"), "the land uses are the enum's values, in its order")
        self.lua.execute("API.type.enum.LandUseType.COMMERCIAL = nil")
        self.assertFalse(self.lua.eval("(pcall(A.supplyDemand, 1))"), "a missing land use raises, it is not guessed")
        self.lua.execute("API.type.enum.LandUseType.COMMERCIAL = 6")
        self.assertEqual(77, self.lua.eval("A.updateCount()"))
        self.assertEqual(0.5, self.lua.eval("A.lineUsage(1)"))
        self.assertEqual("1.5/3", self.lua.eval("(function() local s = A.supplyDemand(1)[2] return s[1] .. '/' .. s[2] end)()"))
        self.assertEqual("9,3,5", self.lua.eval("table.concat(A.townOrder(), ',')"))
        self.assertEqual("4", self.lua.eval("table.concat(A.cargoOrder(), ',')"))


class InBridgeTest(unittest.TestCase):
    """The real bridge in the lockstep harness (two mocked games): the switch, the file in the game's own folder."""

    def setUp(self):
        import tempfile

        self.tmp = tempfile.mkdtemp(prefix="fl3-trace-")
        self.old = os.environ.get("FEVERLINK3_TOWN_TRACE")

    def tearDown(self):
        import shutil

        if self.old is None:
            os.environ.pop("FEVERLINK3_TOWN_TRACE", None)
        else:
            os.environ["FEVERLINK3_TOWN_TRACE"] = self.old
        shutil.rmtree(self.tmp, ignore_errors=True)

    def game(self, label="trace"):
        g = T.Game(label, 0, self.tmp, 50000)
        g.use_env()
        return g

    def test_off_by_default_no_file(self):
        os.environ.pop("FEVERLINK3_TOWN_TRACE", None)
        g = self.game()
        for _ in range(5):
            g.step()
        self.assertFalse(os.path.exists(os.path.join(g.dir, "town_trace.txt")))

    def test_a_traced_town_gets_its_baseline_and_changes_in_the_games_own_folder(self):
        os.environ["FEVERLINK3_TOWN_TRACE"] = "90"
        g = self.game()
        g.step()
        g.L.execute("WORLD[64].TOWN_BUILDING.level = 3")  # an upgrade between two ticks
        g.step()
        g.L.execute("CLOCK_FLUSH = true")
        # the file is flushed by wall time (a second) or 200 lines: force it by waiting
        import time

        time.sleep(1.1)
        g.step()
        text = T.read_text(os.path.join(g.dir, "town_trace.txt"))
        lines = text.splitlines()
        self.assertTrue(lines[0].startswith("# town_trace start tick=94"), lines[:3])
        self.assertTrue(lines[1].startswith("t=94 town=90 n=2 h="), lines[:4])
        self.assertIn("+ house_a.con@10.0,20.0,1.0 L=1 D=1 P=1 T=100", lines)
        self.assertIn("+ house_b.con@30.0,40.0,2.0 L=2 D=1 P=2 T=200", lines)
        self.assertIn("~ house_b.con@30.0,40.0,2.0 L=2>3 D=1 P=2 T=200", lines)
        self.assertNotIn("town=91", text, "the other town is not traced")

    def test_commands_seen_by_the_sim_script_are_listed(self):
        os.environ["FEVERLINK3_TOWN_TRACE"] = "90"
        g = self.game()
        g.step()
        g.L.execute("SIM_EVENT(0, 'apply_command', 'townUpdateSize', { { townEntity = 90 }, nil, nil, false })")
        import time

        time.sleep(1.1)
        g.step()
        text = T.read_text(os.path.join(g.dir, "town_trace.txt"))
        self.assertIn("cmd townUpdateSize town=90 pi=0", text)


class InBridgeCmdsTest(InBridgeTest):
    """FEVERLINK3_TOWN_TRACE_CMDS in the real bridge: the game script's factory calls reach the file, unchanged."""

    def setUp(self):
        super().setUp()
        self.old_cmds = os.environ.get("FEVERLINK3_TOWN_TRACE_CMDS")

    def tearDown(self):
        if self.old_cmds is None:
            os.environ.pop("FEVERLINK3_TOWN_TRACE_CMDS", None)
        else:
            os.environ["FEVERLINK3_TOWN_TRACE_CMDS"] = self.old_cmds
        super().tearDown()

    def test_factories_are_wrapped_only_when_the_switch_is_on(self):
        import time

        os.environ["FEVERLINK3_TOWN_TRACE"] = "90"
        os.environ.pop("FEVERLINK3_TOWN_TRACE_CMDS", None)
        g = self.game("off")
        g.step()
        g.L.execute("R = SIM_ENV.api.cmd.makeTownUpdateSizeCmd(90, {0.5, 1, 0.25}, false)")
        self.assertEqual("makeTownUpdateSizeCmd", g.L.eval("R.__cmd"))
        time.sleep(1.1)
        g.step()
        self.assertNotIn(" mk ", T.read_text(os.path.join(g.dir, "town_trace.txt")))

    def test_the_game_scripts_call_is_logged_and_returned_unchanged(self):
        import time

        os.environ["FEVERLINK3_TOWN_TRACE"] = "90"
        os.environ["FEVERLINK3_TOWN_TRACE_CMDS"] = "1"
        g = self.game("on")
        g.step()
        g.L.execute("SIZE = {0.5, 1, 0.25}; R = SIM_ENV.api.cmd.makeTownUpdateSizeCmd(90, SIZE, false)")
        self.assertEqual("makeTownUpdateSizeCmd", g.L.eval("R.__cmd"))
        self.assertTrue(g.L.eval("R.args[2] == SIZE"))
        time.sleep(1.1)
        g.step()
        text = T.read_text(os.path.join(g.dir, "town_trace.txt"))
        self.assertIn(" mk makeTownUpdateSizeCmd town=90 k=1 flag=0 f32=3f000000,3f800000,3e800000 ", text)
        self.assertIn(" town=90 in k=1 ", text)
        self.assertIn(" town=90 order pos=", text)


def WRITES_LEN(test):
    return [l for l in test.lines() if " info " not in l]


class BridgeWiringTest(unittest.TestCase):
    """The bridge reads the switch and loads the module only when asked; the module is listed for the game."""

    def test_the_bridge_names_the_module_the_switch_and_the_file(self):
        bridge = read("feverlink3_bridge.script.lua")
        self.assertIn("FEVERLINK3_TOWN_TRACE", bridge)
        self.assertIn("feverlink3_towntrace.lua", bridge)
        self.assertIn("townTraceGuard", bridge)


if __name__ == "__main__":
    unittest.main()
