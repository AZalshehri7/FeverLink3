"""The mod's diagnostic switches, each inert unless asked for and never able to break the game:
  * the town trace (feverlink3_towntrace.lua, FEVERLINK3_TOWN_TRACE / _CMDS): writes only when a traced town changed, the diff content,
    buffering, the `all` sampling, the factory wrappers of the town commands, the real-api accessors against a mocked engine;
  * the live-vs-loaded log (feverlink3_livevsloaded.lua, FEVERLINK3_LIVE_VS_LOADED): L1 lines, the L2 hashes (ordered vs sorted), the
    resume detection and the sampling window, the work cap, switching off after errors, L4;
  * the Lua string-hash seed probe (feverlink3_common.lua, FEVERLINK3_LUA_SEED_LOG);
  * all three in the real bridge of a mocked game (test_lockstep.py's harness): off by default, their files in the game's own folder.

Run:  python -I -m unittest discover -s tests/lua -p "test_diagnostics.py"
"""
import os
import re
import shutil
import sys
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import test_lockstep as T  # noqa: E402

lupa = T.lupa


def env(**values):
    """Sets environment variables (None removes one) and returns a function that puts the old values back."""
    old = {k: os.environ.get(k) for k in values}

    def put(vals):
        for k, v in vals.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
    put(values)
    return lambda: put(old)


# ---------------------------------------------------------------- the town trace

# A mocked world: BUILDINGS[entity] = {town, construction, level, depth, parcels, timeBuilt}, CONS[c] = {file,x,y,z},
# TOWNS[id] = component. WRITES collects every append, NOW is the wall clock.
TRACE_SETUP = r'''
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
TOWNS[7] = {developmentActive = true, sizeFactors = {0.5, 1, 0.25}, initialLandUseCapacities = {10, 20, 30}, capacityScalingFactor = {1, 1, 1}, cargoNeeds = {{1}, {}, {}}}
"""


class TownTraceTest(unittest.TestCase):
    def setUp(self):
        self.fresh()

    def fresh(self, cmds=False):
        self.lua = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.lua.execute("function ug_require(n) error('no ' .. n) end")
        self.lua.execute("TT = assert(load(..., 'towntrace', 't'))()", T.TRACE_SRC)
        self.lua.execute(TRACE_SETUP)
        if cmds:
            self.lua.execute(CMD_SETUP)
            self.lua.execute("add(1, 7, 'h.mdl', 10)")

    def run_lua(self, code):
        return self.lua.execute(code)

    def lines(self):
        return [l for l in self.lua.eval("written()").splitlines() if l]

    def after(self, header):
        """The line after the first one that starts with `header`."""
        out = self.lines()
        return out[[k for k, l in enumerate(out) if l.startswith(header)][0] + 1]

    def writes(self):
        return int(self.lua.eval("#WRITES"))

    def test_the_trace_writes_a_traced_towns_baseline_once_and_then_only_what_changed(self):
        r = self.lua.eval("function(s) local p = TT.parseSpec(s) return p.enabled, p.all, #p.list, p.ids[60479] end")
        for spec, want in ((None, (False, False, 0, None)), ("", (False, False, 0, None)), ("nonsense, x1", (False, False, 0, None)),
                           (" 60479 , 60487,60479 ", (True, False, 2, True)), ("ALL", (True, True, 0, None))):
            self.assertEqual(want, tuple(r(spec)), spec)
        with self.subTest("off: inert"):
            self.run_lua("t = new(nil); add(1, 7, 'a.mdl', 0); t.step(1); t.noteCommand(1, 'x'); t.flush(true)")
            self.assertFalse(self.lua.eval("t.enabled"))
            self.assertEqual([], self.lines())
        with self.subTest("the baseline, then nothing while nothing changes"):
            self.fresh()
            self.run_lua("add(1, 7, 'h.mdl', 10, 2, 3, 4, 99); add(2, 7, 'g.mdl', 20); add(3, 8, 'other.mdl', 5); t = new('7'); t.step(100); t.flush(true)")
            out = self.lines()
            self.assertTrue(out[0].startswith("# town_trace start tick=100"), out)
            self.assertRegex(out[1], r"^t=100 town=7 n=2 h=\S+ base$")
            self.assertEqual(["+ g.mdl@20.0,5.0,0.0 L=1 D=1 P=2 T=0", "+ h.mdl@10.0,5.0,0.0 L=2 D=3 P=4 T=99"], out[2:4])
            self.assertFalse([l for l in out if "town=8" in l], "an untraced town is never written")
            size = len([l for l in out if " info " not in l])
            # (the same tick twice does nothing, even with a change in between)
            self.run_lua("for tick = 101, 159 do t.step(tick) end; add(5, 7, 'z.mdl', 1); t.step(159); t.flush(true)")
            later = [l for l in self.lines() if " info " not in l]
            self.assertEqual(size, len(later), "nothing changed, nothing written: %s" % later[size:])
        cases = [
            ("an added building", "add(1, 7, 'h.mdl', 10); t = new('7'); t.step(1); add(2, 7, 'n.mdl', 30, 1, 2, 3, 555); t.step(2)",
             "t=2 town=7 n=2", "+ n.mdl@30.0,5.0,0.0 L=1 D=2 P=3 T=555"),
            ("a removed building", "add(1, 7, 'h.mdl', 10); add(2, 7, 'n.mdl', 30); t = new('7'); t.step(1); BUILDINGS[2] = nil; t.step(2)",
             "t=2 town=7 n=1", "- n.mdl@30.0,5.0,0.0 L=1 D=1 P=2 T=0"),
            ("an upgrade in place: only the changed fields, old>new",
             "add(1, 7, 'h.mdl', 10, 1, 2, 3, 100); t = new('7'); t.step(1); BUILDINGS[1].level = 2; BUILDINGS[1].timeBuilt = 250; t.step(2)",
             "t=2 town=7 n=1", "~ h.mdl@10.0,5.0,0.0 L=1>2 D=2 P=3 T=100>250"),
            ("replaced by another at the same place: one changed line",
             "add(1, 7, 'h.mdl', 10); t = new('7'); t.step(1); BUILDINGS[1] = nil; add(9, 7, 'h.mdl', 10, 1, 1, 2, 300); t.step(2)",
             "t=2 town=7", "~ h.mdl@10.0,5.0,0.0 L=1 D=1 P=2 T=0>300"),
            ("two identical buildings at one place", "add(1, 7, 'h.mdl', 10); add(2, 7, 'h.mdl', 10); t = new('7'); t.step(1)",
             "t=1 town=7 n=2", "+ h.mdl@10.0,5.0,0.0#2 L=1 D=1 P=2 T=0"),
            ("a town without buildings", "t = new('7'); t.step(1); add(1, 7, 'h.mdl', 10); t.step(2)",
             "t=2 town=7 n=1", "+ h.mdl@10.0,5.0,0.0 L=1 D=1 P=2 T=0"),
        ]
        for what, lua, header, want in cases:
            with self.subTest(what):
                self.fresh()
                self.run_lua(lua + "; t.flush(true)")
                out = self.lines()
                i = [k for k, l in enumerate(out) if l.startswith(header)][0]
                self.assertIn(want, out[i + 1:i + 3])
        with self.subTest("entity ids never enter the text or the hash"):
            self.fresh()
            self.run_lua("add(1, 7, 'h.mdl', 10); add(2, 7, 'g.mdl', 20); a = new('7'); a.step(1); a.flush(true)")
            first = self.lines()
            self.run_lua("BUILDINGS, CONS, WRITES = {}, {}, {}; add(501, 7, 'g.mdl', 20); add(77, 7, 'h.mdl', 10); b = new('7'); b.step(1); b.flush(true)")
            self.assertEqual(first, self.lines())
        with self.subTest("commands are listed with tick, name, town and player flag"):
            self.fresh()
            self.run_lua("t = new('7'); t.noteCommand(12, 'townUpdateSize', 7, false); t.noteCommand(13, 'onPreBuildProposal'); t.flush(true)")
            self.assertEqual(["t=12 cmd townUpdateSize town=7 pi=0", "t=13 cmd onPreBuildProposal"], [l for l in self.lines() if " cmd " in l])
        with self.subTest("buffered until a second has passed; a failing write never raises"):
            self.fresh()
            self.run_lua("add(1, 7, 'h.mdl', 10); t = new('7'); t.step(1); NOW = 0.5; t.step(2)")
            self.assertEqual(0, self.writes(), "nothing written yet")
            self.run_lua("NOW = 1.2; add(2, 7, 'n.mdl', 3); t.step(3)")
            self.assertEqual(1, self.writes(), "one append for everything buffered")
            self.assertIn("n=2", self.lua.eval("written()"))
            self.run_lua("t = TT.new({spec='7', acc=ACC, hash=hash, clock=function() return NOW end, write=function() error('disk full') end}); t.step(1); t.flush(true); t.step(2)")
        with self.subTest("all: every fifth tick, no baseline buildings"):
            self.fresh()
            self.run_lua("add(1, 7, 'h.mdl', 10); add(2, 8, 'g.mdl', 20); t = new('all'); for k = 1, 4 do t.step(k) end; t.flush(true)")
            self.assertEqual([], [l for l in self.lines() if l.startswith("t=")], "ticks 1-4 are skipped")
            self.run_lua("t.step(5); t.flush(true)")
            out = self.lines()
            self.assertTrue(any(l.startswith("t=5 town=7 n=1") for l in out) and any(l.startswith("t=5 town=8 n=1") for l in out), out)
            self.assertFalse(any(l.startswith("+ ") for l in out), "the baseline of `all` names no buildings")
            self.run_lua("add(3, 8, 'n.mdl', 40); t.step(7); t.step(10); t.flush(true)")
            out = self.lines()
            self.assertEqual("+ n.mdl@40.0,5.0,0.0 L=1 D=1 P=2 T=0", self.after("t=10 town=8 n=2"))
            self.assertFalse(any(l.startswith("t=10 town=7") and " info " not in l for l in out), "town 7 did not change")

    def test_the_town_info_line_and_the_api_accessors(self):
        self.run_lua("add(1, 7, 'h.mdl', 10); TOWNS[7] = {developmentActive = true, sizeFactors = {1, 0.5, 0.25}, cargoNeeds = {{{1, 2}}, {}, {}}, noise = 3.14159265}")
        self.run_lua("t = new('7'); t.step(10); t.flush(true)")
        info = lambda: [l for l in self.lines() if " info " in l]
        self.assertEqual(1, len(info()))
        for part in ("dev=1", "size=[1,0.5,0.25]", "needs=[[[1,2]],[],[]]", "noise=3.142"):
            self.assertIn(part, info()[0])
        self.run_lua("for tick = 11, 33 do t.step(tick) end; t.flush(true)")
        self.assertEqual(1, len(info()), "unchanged and under 25 ticks")
        self.run_lua("t.step(35); t.flush(true)")
        self.assertEqual(2, len(info()), "refreshed after 25 ticks")
        self.run_lua("TOWNS[7].developmentActive = false; t.step(36); t.flush(true)")
        self.assertEqual(3, len(info()))
        self.assertIn("dev=0", info()[-1])
        # the real api accessors against a mocked engine
        self.fresh()
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
        self.assertEqual("+ res/x.mdl@1.2,2.5,3.0 L=3 D=2 P=3 T=42", self.after("t=1 town=7 n=1"))
        self.assertIn("dev=1", info()[0])
        self.assertIn("emis=55", info()[0], "read through api.engine.util.town.getTownEmissionDB, where the API has it")
        self.assertNotIn("10", "".join(l for l in out if l.startswith("+ ")), "no entity id in a building line")

    # ---- the command trace (FEVERLINK3_TOWN_TRACE_CMDS)

    def wrap(self, spec="7"):
        self.lua.execute("t = newc('%s'); H = { tracer = t }; N = TT.installCmdWrappers(APICMD, H)" % spec)

    def mk(self):
        return [l for l in self.lines() if " mk " in l]

    def test_float_bits_are_exact_and_the_soft_path_matches_ieee(self):
        import random
        import struct
        r = self.lua.eval("function(x) return TT.bits32(x), TT.bits64(x) end")
        for x, want in ((1.0, ("3f800000", "3ff0000000000000")), (0.25, ("3e800000", "3fd0000000000000")),
                        (-0.5, ("bf000000", "bfe0000000000000")), (0.1, ("3dcccccd", "3fb999999999999a")), (None, ("-", "-"))):
            self.assertEqual(want, tuple(r(x)), x)   # (0.1 is not a float32 number: the two widths differ)
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

    def test_the_factory_wrappers_pass_everything_through_and_log_the_call_before_it_runs(self):
        self.fresh(cmds=True)
        self.wrap()
        self.lua.execute("SIZE = {0.5, 1, 0.1}; R = table.pack(APICMD.makeTownUpdateSizeCmd(7, SIZE, false))")
        self.assertEqual(("cmdA", None, 42, 3), tuple(self.lua.eval("R[%d]" % i) if i else self.lua.eval("R.n") for i in (1, 2, 3, 0)))
        self.assertTrue(self.lua.eval("#CALLS == 1 and CALLS[1][2] == SIZE"), "the very same table object")
        self.assertEqual((3, 7, False), (self.lua.eval("CALLS[1].n"), self.lua.eval("CALLS[1][1]"), self.lua.eval("CALLS[1][3]")))
        self.assertEqual((2, "o"), (self.lua.eval("N"), self.lua.eval("APICMD.makeOther()")), "only the two traced factories are wrapped")
        self.assertEqual(0, self.writes(), "buffered for a second")
        self.lua.execute("t.flush(true)")
        self.assertEqual(["t=100 uc=200 mk makeTownUpdateSizeCmd town=7 k=1 flag=0 "
                          "f32=3f000000,3f800000,3dcccccd f64=3fe0000000000000,3ff0000000000000,3fb999999999999a v=0.5,1,0.10000000000000001"], self.mk())
        inp = [l for l in self.lines() if " in k=1 " in l][0]
        self.assertTrue(inp.startswith("t=100 town=7 in k=1 lu=0.10700000000000001 sd=1.5/3,2/4,0/1 dev=1 size=[0.5,1,0.25] cap=[10,20,30] scale=[1,1,1] needs=[[1],[],[]] bld="), inp)
        # k counts the calls of one town in one tick; another town is passed through unlogged; a userdata-like vector is read by index
        self.lua.execute("""APICMD.makeTownUpdateSizeCmd(7, {1, 1, 1}, false); TICK = 101; APICMD.makeTownUpdateSizeCmd(7, {1, 1, 1}, false)
          R8 = APICMD.makeTownUpdateSizeCmd(8, {1, 1, 1}, false)
          VEC = setmetatable({}, { __index = function(_, i) return ({0.5, 1, 0.25})[i] end, __len = function() return 3 end })
          APICMD.makeTownUpdateSizeCmd(7, VEC, false); APICMD.makeTownUpdateCargoNeedsCmd(7, {{1, 2}, {}, {3}}, true); t.flush(true)""")
        self.assertEqual("cmdA", self.lua.eval("R8"))
        mk = self.mk()
        self.assertEqual(["1", "2", "1", "2", "1"], [l.split(" k=")[1].split()[0] for l in mk], "per factory, town and tick")
        self.assertFalse([l for l in mk if "town=8" in l])
        self.assertIn("v=0.5,1,0.25", mk[3])
        self.assertRegex(mk[4], r"^t=101 uc=202 mk makeTownUpdateCargoNeedsCmd town=7 k=1 flag=1 needs=\[\[1,2\],\[\],\[3\]\] nh=\S+$")
        # installing twice does not wrap the wrapper
        self.assertEqual(0, self.lua.eval("TT.installCmdWrappers(APICMD, H)"))
        # `all` logs every town; the line is buffered before the original runs
        self.fresh(cmds=True)
        self.lua.execute("t = newc('all'); H = { tracer = t }")
        self.lua.execute("local orig = APICMD.makeTownUpdateSizeCmd; APICMD.makeTownUpdateSizeCmd = function(...) t.flush(true); SEEN = #written(); return orig(...) end")
        self.lua.execute("TT.installCmdWrappers(APICMD, H); APICMD.makeTownUpdateSizeCmd(8, {0.5, 1, 0.25}, false)")
        self.assertGreater(self.lua.eval("SEEN"), 0, "the line was already buffered when the original ran")
        self.assertEqual(1, len(self.mk()))

    def test_the_command_trace_never_breaks_a_call(self):
        self.fresh(cmds=True)
        # cmds not requested, and no tracer at all: the wrappers pass through and write nothing
        self.lua.execute("t = new('7'); H = { tracer = t }; TT.installCmdWrappers(APICMD, H); R = APICMD.makeTownUpdateSizeCmd(7, {1, 1, 1}, false); t.step(1); t.flush(true)")
        self.assertEqual("cmdA", self.lua.eval("R"))
        self.assertEqual([], [l for l in self.lines() if " mk " in l or " order " in l])
        self.lua.execute("H.tracer = nil; R = APICMD.makeTownUpdateSizeCmd(7, {1, 1, 1}, false)")
        self.assertEqual("cmdA", self.lua.eval("R"))
        # a raising input accessor is tolerated
        self.fresh(cmds=True)
        self.wrap()
        self.lua.execute("ACC.lineUsage = function() error('no usage') end; APICMD.makeTownUpdateSizeCmd(7, {1, 1, 1}, false); t.flush(true)")
        self.assertIn("lu=err", [l for l in self.lines() if " in k=1 " in l][0])
        self.assertTrue(self.lua.eval("t.cmds"))
        # errors in the trace itself (a hash that raises) switch it off after five, and the original runs every time
        self.lua.execute("t = TT.new({ spec = '7', cmds = true, acc = ACC, hash = function() error('boom') end, tick = function() return TICK end, "
                         "clock = function() return NOW end, write = function(x) WRITES[#WRITES + 1] = x return true end }); H.tracer = t; CALLS = {}")
        self.lua.execute("for i = 1, 8 do R = APICMD.makeTownUpdateCargoNeedsCmd(7, {{1}}, false) end; t.flush(true)")
        self.assertEqual(("cmdB", 8), (self.lua.eval("R"), self.lua.eval("#CALLS")))
        self.assertFalse(self.lua.eval("t.cmds"))
        self.assertTrue(any("switched off after 5 errors" in l for l in self.lines()), self.lines())
        before = len(self.lines())
        self.lua.execute("APICMD.makeTownUpdateCargoNeedsCmd(7, {{1}}, false); t.flush(true)")
        self.assertEqual(before, len(self.lines()))

    def test_the_order_line_and_the_api_accessors_of_the_inputs(self):
        self.fresh(cmds=True)
        self.lua.execute("t = newc('7'); t.step(100); t.step(101); t.flush(true)")
        order = lambda: [l for l in self.lines() if " order " in l]
        self.assertEqual(1, len(order()), order())
        self.assertRegex(order()[0], r"^t=100 town=7 order pos=1 towns=3:\S+ cargos=3:\S+$")
        first = order()[0].split("towns=")[1]
        self.lua.execute("ORDER_T = { 8, 7, 9 }; t.step(102); t.flush(true)")
        self.assertEqual(2, len(order()))
        self.assertIn("pos=2", order()[1])
        self.assertNotEqual(first, order()[1].split("towns=")[1])
        self.lua.execute("t.step(130); t.flush(true)")
        self.assertEqual(3, len(order()), "repeated after 25 ticks")
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
        self.assertEqual((77, 0.5), (self.lua.eval("A.updateCount()"), self.lua.eval("A.lineUsage(1)")))
        self.assertEqual("1.5/3", self.lua.eval("(function() local s = A.supplyDemand(1)[2] return s[1] .. '/' .. s[2] end)()"))
        self.assertEqual(("9,3,5", "4"), (self.lua.eval("table.concat(A.townOrder(), ',')"), self.lua.eval("table.concat(A.cargoOrder(), ',')")))


# ---------------------------------------------------------------- the live-vs-loaded log

LVL_SETUP = r'''
WRITES, NOW, WALL, CALLS = {}, 0, 100, 0
TOWNS = { 5, 3, 9 }
USAGE = { [3] = 0.25, [5] = 0.5, [9] = 0.125 }
DEST = { [40] = { 7, 3, 5 }, [10] = { 1, 2 }, [20] = {} }
NETS = { 8, 6 }
LINES = { 30, 31, 32, 33 }
FAIL = {}
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
  forDestination = function(d) may("dest") CALLS = CALLS + 1 return DEST[d] end,
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


class LiveVsLoadedTest(unittest.TestCase):
    def setUp(self):
        self.fresh()

    def fresh(self):
        self.lua = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.lua.execute("LVL = assert(load(..., 'lvl', 't'))()", T.LVL_SRC)
        self.lua.execute(LVL_SETUP)

    def run_lua(self, code):
        self.fresh()
        return self.lua.execute(code)

    def grep(self, text):
        return [l for l in self.lua.eval("written()").splitlines() if l and text in l]

    def test_the_lines_the_hashes_and_the_sampling_window(self):
        r = self.lua.eval("function(s) local p = LVL.parseTowns(s) return p.all, p.ids[12] end")
        self.assertEqual([(True, None), (True, None), (False, True)], [tuple(r(s)) for s in (None, "all", "12, 14")])
        self.lua.execute("o1, s1 = LVL.hashes({ 3, 1, 2 }); o2, s2 = LVL.hashes({ 1, 2, 3 }); o3, s3 = LVL.hashes({ 1, 2, 4 })")
        o1, s1, o2, s2, o3, s3 = (self.lua.eval(n) for n in ("o1", "s1", "o2", "s2", "o3", "s3"))
        self.assertEqual(s1, s2, "the same set sorts to the same hash")
        self.assertNotEqual(o1, o2, "another order changes the ordered hash")
        self.assertNotEqual(s2, s3, "another set changes the sorted hash")
        self.assertEqual(16, len(o1))
        self.assertEqual([1, 2], list(self.lua.eval("LVL.lowest({ 9, 1, 'x', 4, 2 }, 2)").values()))
        with self.subTest("L1 every 12 ticks per town, traced towns only, a dash for a missing xp"):
            self.run_lua("t = new(); for tick = 24, 30 do t.step(tick) end t.flush(true)")
            l1 = self.grep(" L1 ")
            self.assertEqual(3, len(l1), l1)
            self.assertTrue(all(re.match(r"t=2[456] ", l) for l in l1), l1)
            self.assertIn("t=24 L1 town=3 lu=b0.25 lun=0.25 sd=3/10,2/3,0/1.5 xp=1003 lvl=4", l1)
            self.run_lua("t = new({ towns = '5' }); for tick = 24, 30 do t.step(tick) end t.flush(true)")
            self.assertEqual(["town=5"], [re.search(r"town=\d+", l).group(0) for l in self.grep(" L1 ")])
            self.run_lua("FAIL.xp = true; t = new(); for tick = 24, 30 do t.step(tick) end t.flush(true)")
            l1 = self.grep(" L1 ")
            self.assertTrue(len(l1) == 3 and all(" xp=- lvl=-" in l for l in l1), l1)
            self.assertTrue(self.grep("source xp disabled"))
        with self.subTest("L2 at the resume and every 60 ticks for five minutes, the lowest ids, a wall-clock gap is a resume"):
            self.run_lua("t = new(); for tick = 100, 1700 do t.step(tick) end t.flush(true)")
            self.assertEqual([100] + list(range(120, 1601, 60)), [int(l.split()[0][2:]) for l in self.grep(" L2 sample")])
            self.assertEqual(1, len(self.grep("resume why=start")))
            self.run_lua("t = new(); for tick = 100, 112 do t.step(tick) end t.flush(true)")
            dest = self.grep(" L2 dest ")
            self.assertEqual(["10", "20", "40"], [l.split("id=")[1].split()[0] for l in dest], "the lowest ids in ascending order, whatever pairs() gave")
            self.assertIn(" n=3 ", [l for l in dest if "id=40 " in l][0])
            self.assertEqual(2, len(self.grep(" L2 tn ")))
            self.assertEqual(["30", "31", "32"], [l.split("id=")[1].split()[0] for l in self.grep(" L2 line ")], "3 lines of 4")
            self.assertTrue(self.grep(" L2 count n=77"))
            self.run_lua("t = new(); t.step(100); WALL = WALL + 1; t.step(101); WALL = WALL + 30; t.step(102); t.flush(true)")
            self.assertEqual(1, len(self.grep("resume why=gap")))
            self.assertTrue(self.grep("t=102 resume why=gap"))
        with self.subTest("another order shows in the ordered hash only"):
            self.run_lua("t = new(); t.step(100); t.step(101); t.flush(true); A = written(); WRITES = {}")
            self.lua.execute("DEST[40] = { 3, 5, 7 }; t2 = new(); t2.step(100); t2.step(101); t2.flush(true); B = written()")
            a, b = ([l for l in self.lua.eval(v).splitlines() if "id=40 " in l and "L2 dest" in l][0] for v in ("A", "B"))
            fa, fb = dict(p.split("=") for p in a.split()[3:]), dict(p.split("=") for p in b.split()[3:])
            self.assertEqual(fa["s"], fb["s"])
            self.assertNotEqual(fa["o"], fb["o"])
        with self.subTest("the work of a step is capped; the same tick twice does nothing twice"):
            self.run_lua("t = new(); local worst = 0; for tick = 120, 135 do local c0 = CALLS; t.step(tick); worst = math.max(worst, CALLS - c0) end WORST = worst")
            self.assertLessEqual(self.lua.eval("WORST"), 2 * int(self.lua.eval("LVL.JOBS_PER_STEP")), "one L1 job calls the API twice")
            self.lua.execute("n = CALLS; t.step(135)")
            self.assertEqual(self.lua.eval("n"), self.lua.eval("CALLS"))
        with self.subTest("L4 waits for the override and logs once; without a probe it says so"):
            self.run_lua("READY = false; t = new({ randomProbe = function() if not READY then return nil end return { installed = 1, rng_n = 3 } end })")
            self.lua.execute("t.step(100); t.step(101); READY = true; t.step(102); t.step(103); t.flush(true)")
            self.assertEqual(["t=102 L4 installed=1 rng_n=3"], self.grep(" L4 "))
            self.run_lua("t = new(); t.step(100); t.flush(true)")
            self.assertEqual(["t=100 L4 probe=none"], self.grep(" L4 "))

    def test_it_is_inert_when_off_never_raises_and_switches_itself_off_after_five_errors(self):
        self.run_lua("t = LVL.new({ enabled = false, acc = ACC, write = function(x) WRITES[#WRITES + 1] = x end }); "
                     "t.step(1); t.step(2); t.noteResume(2, 'x'); t.flush(true)")
        self.assertEqual(([], 0, False), (self.grep(""), self.lua.eval("CALLS"), self.lua.eval("t.enabled")))
        # a failing source is disabled without counting as an error; the other sources keep running
        self.run_lua("FAIL.dest = true; FAIL.line = true; t = new(); for tick = 100, 400 do t.step(tick) end t.flush(true)")
        self.assertTrue(self.lua.eval("t.enabled"))
        self.assertTrue(self.grep("source L2dest disabled") and self.grep(" L2 dest id=10 err") and self.grep(" L1 "))
        # every source failing, a failing write: never raises
        self.run_lua("for _, k in ipairs({ 'lineUsage', 'supplyDemand', 'xp', 'count', 'destinations', 'dest', 'tn', 'line' }) do FAIL[k] = true end; "
                     "ACC.towns = function() error('x') end; t = new(); for tick = 100, 400 do t.step(tick) end t.flush(true)")
        self.assertTrue(self.lua.eval("t.enabled"))
        self.run_lua("t = new({ write = function() error('disk') end }); for tick = 100, 400 do t.step(tick) end t.flush(true)")
        self.assertTrue(self.lua.eval("t.enabled"))
        # internal errors (a wall clock that raises: the step itself errors) switch it off after five, then nothing is written
        self.run_lua("BAD = false; t = new({ wall = function() if BAD then error('wall') end return 100 end }); t.step(1); BAD = true; for tick = 2, 20 do t.step(tick) end")
        self.assertEqual((False, 5), (self.lua.eval("t.enabled"), self.lua.eval("t.errors")))
        self.lua.execute("n0 = #WRITES; t.step(99); t.flush(true)")
        self.assertEqual(self.lua.eval("n0"), self.lua.eval("#WRITES"), "nothing after the switch-off")
        self.assertTrue(self.grep("switched off after 5 errors"))
        # the real accessors against a mocked engine
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
        self.assertEqual((12, 3, 3, 9), tuple(self.lua.eval(v) for v in ("XP", "LV", "SD[1][1]", "SD[1][2]")))
        # the land uses are the enum's values (getCargoSupplyAndLimit's typed LandUseType), in the documented order
        self.assertEqual("res,com,ind", self.lua.eval("table.concat(LUS, ',')"))
        # a game without the town game script: no entity, no component read of it
        self.lua.execute("API.engine.system.gameScriptSystem.getEntityForGameScript = function() return -1 end; "
                         "API.engine.getComponent = function() error('read of a missing entity') end")
        self.assertIsNone(self.lua.eval("A.xp(2)"))
        # no LandUseType enum: the accessor raises (the caller guards), it does not guess integers
        self.lua.execute("API.type.enum = nil")
        self.assertFalse(self.lua.eval("(pcall(A.supplyDemand, 2))"))
        self.assertEqual((5, 2, 3, 1, 2), tuple(self.lua.eval(e) for e in ("A.count()", "#A.destinations()", "#A.forLine(1)",
                                                                           "#A.forNetwork(1)", "#A.forDestination(1)")))


# ---------------------------------------------------------------- the Lua seed probe

class SeedProbeTest(unittest.TestCase):
    """Observation only: inert unless FEVERLINK3_LUA_SEED_LOG=1, never raises, one `luaseed probe order=<n>` line per Lua state."""

    def test_off_by_default_one_stable_line_when_on_and_never_raises(self):
        self.addCleanup(env(FEVERLINK3_LUA_SEED_LOG=None, FEVERLINK3_SESSION_DIR=None))
        lua = lupa.LuaRuntime(unpack_returned_tuples=True)
        C = T.load_common(lua)
        lines = []
        for value in (None, "", "0", "true", "fixed"):
            env(FEVERLINK3_LUA_SEED_LOG=value)
            C.seedProbed = None
            self.assertFalse(C.luaSeedProbeOnce(lines.append), value)
        self.assertEqual(lines, [])
        a, b = C.luaSeedProbe(), C.luaSeedProbe()
        self.assertTrue(a == b == int(a) and a > 0, "a stable number inside one state")
        os.environ["FEVERLINK3_LUA_SEED_LOG"] = "1"
        self.assertTrue(C.luaSeedProbeOnce(lines.append))
        self.assertFalse(C.luaSeedProbeOnce(lines.append), "one-shot")
        self.assertEqual(len(lines), 1)
        self.assertRegex(lines[0], r"^luaseed probe order=\d+$")
        # a logger that raises, an os.getenv that raises, a probe that fails: reported, never raised
        C.seedProbed = None
        self.assertTrue(C.luaSeedProbeOnce(lambda m: 1 / 0))
        lua.execute("os.getenv = function() error('no env') end")
        C.seedProbed = None
        self.assertFalse(C.luaSeedProbeOnce(lines.append))
        lua.execute("os.getenv = function() return '1' end")
        lua.execute("local C = ...; C.luaSeedProbe = function() error('x') end", C)
        C.seedProbed = None
        self.assertTrue(C.luaSeedProbeOnce(lines.append))
        self.assertEqual(lines[-1], "luaseed probe order=error")
        self.assertEqual(len(re.findall(r"pcall\(C\.luaSeedProbeOnce, logSim\)", T.BRIDGE_SRC)), 1, "the bridge calls it once under pcall")


# ---------------------------------------------------------------- in the real bridge

class InBridgeTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="fl3-diag-")
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)

    def test_each_switch_is_off_by_default_and_writes_its_file_in_the_games_own_folder_when_on(self):
        names = ("FEVERLINK3_TOWN_TRACE", "FEVERLINK3_TOWN_TRACE_CMDS", "FEVERLINK3_LIVE_VS_LOADED")
        self.addCleanup(env(**{k: None for k in names}))
        off = T.Game("off", 0, self.tmp, 50000)
        for _ in range(5):
            off.step()
        for f in ("town_trace.txt", "live_vs_loaded.txt"):
            self.assertFalse(os.path.exists(os.path.join(off.dir, f)), f)
        env(FEVERLINK3_TOWN_TRACE="90", FEVERLINK3_TOWN_TRACE_CMDS="1", FEVERLINK3_LIVE_VS_LOADED="1")
        g = T.Game("on", 0, self.tmp, 50000)
        g.step()
        g.L.execute("WORLD[64].TOWN_BUILDING.level = 3")  # an upgrade between two ticks
        g.step()
        g.L.execute("SIM_EVENT(0, 'apply_command', 'townUpdateSize', { { townEntity = 90 }, nil, nil, false })")
        # the game script's factory call is logged and returned unchanged
        g.L.execute("SIZE = {0.5, 1, 0.25}; R = SIM_ENV.api.cmd.makeTownUpdateSizeCmd(90, SIZE, false)")
        self.assertEqual("makeTownUpdateSizeCmd", g.L.eval("R.__cmd"))
        self.assertTrue(g.L.eval("R.args[2] == SIZE"))
        time.sleep(1.1)   # the files are flushed by wall time (a second) or 200 lines
        g.step()   # (the mocked engine lacks the sim person api: those sources are disabled, nothing raises)
        text = T.read_text(os.path.join(g.dir, "town_trace.txt"))
        lines = text.splitlines()
        self.assertTrue(lines[0].startswith("# town_trace start tick=94"), lines[:3])
        self.assertTrue([l for l in lines[1:3] if l.startswith("t=94 town=90 n=2 h=")], lines[:4])
        for want in ("+ house_a.con@10.0,20.0,1.0 L=1 D=1 P=1 T=100", "+ house_b.con@30.0,40.0,2.0 L=2 D=1 P=2 T=200",
                     "~ house_b.con@30.0,40.0,2.0 L=2>3 D=1 P=2 T=200"):
            self.assertIn(want, lines)
        self.assertNotIn("town=91", text, "the other town is not traced")
        for want in ("cmd townUpdateSize town=90 pi=0", " mk makeTownUpdateSizeCmd town=90 k=1 flag=0 f32=3f000000,3f800000,3e800000 ",
                     " town=90 in k=1 ", " town=90 order pos="):
            self.assertIn(want, text)
        lvl = T.read_text(os.path.join(g.dir, "live_vs_loaded.txt"))
        for want in ("# lvl start", " resume why=start ", " L4 "):
            self.assertIn(want, lvl)


if __name__ == "__main__":
    unittest.main()
