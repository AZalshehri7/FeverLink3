"""Tests for tools/gametest/compare_town_trace.py with synthetic town_trace.txt / growth.log files of two games.

Run:  python -m unittest discover -s tests/lua -p "test_compare_town_trace.py"
"""
import contextlib
import io
import os
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "tools", "gametest"))

import compare_town_trace as C  # noqa: E402

BASE = [
    "# town_trace start tick=90 spec=60479",
    "t=90 town=60479 n=2 h=aaaa base",
    "+ a.con@1.0,2.0,0.0 L=1 D=1 P=1 T=10",
    "+ b.con@3.0,4.0,0.0 L=1 D=1 P=2 T=20",
    "t=90 town=60479 info dev=1 size=[1,1,1] needs=[[],[],[]]",
]


def call(n, tick, seed="0x00000064", town=60479, ret="0x1", a6="0x0000000000000006", pid=111, used=None):
    u = " used=%s" % used if used else ""
    return ("growth n=%d pid=%d town=%d seed=%s t=%d%s r9b=1 a6=%s a6f=0 a6b=0 a7=0x0 ret=%s caller=0x9e1470"
            % (n, pid, town, seed, tick, u, a6, ret))


class Run:
    """Two live folders written from lists of lines."""

    def __init__(self, test):
        self.root = tempfile.mkdtemp(prefix="fl3-cmp-")
        test.addCleanup(shutil.rmtree, self.root, True)
        self.a, self.b = os.path.join(self.root, "live1"), os.path.join(self.root, "live2")
        os.makedirs(self.a)
        os.makedirs(self.b)

    def write(self, which, trace=None, growth=None):
        d = self.a if which == "A" else self.b
        if trace is not None:
            with open(os.path.join(d, "town_trace.txt"), "w", encoding="utf-8") as f:
                f.write("\n".join(trace) + "\n")
        if growth is not None:
            with open(os.path.join(d, "growth.log"), "w", encoding="utf-8") as f:
                f.write("\n".join(growth) + "\n")

    def compare(self, *extra):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            verdicts = C.main([self.a, self.b, *extra])
        return verdicts, buf.getvalue()


class CompareTest(unittest.TestCase):
    def test_identical_games_have_no_divergence(self):
        r = Run(self)
        tr = BASE + ["t=100 town=60479 n=3 h=bbbb", "+ c.con@5.0,6.0,0.0 L=1 D=1 P=1 T=500"]
        g = [call(1, 100), call(2, 105)]
        for w in "AB":
            r.write(w, tr, g)
        verdicts, text = r.compare()
        self.assertEqual({60479: C.V_NONE}, verdicts)
        self.assertIn("no divergence", text)

    def test_different_seed_before_the_change_is_inputs_differed(self):
        r = Run(self)
        r.write("A", BASE + ["t=100 town=60479 n=3 h=bbbb", "+ c.con@5.0,6.0,0.0 L=1 D=1 P=1 T=500"], [call(1, 100, seed="0x00000064")])
        r.write("B", BASE + ["t=100 town=60479 n=3 h=cccc", "+ d.con@7.0,8.0,0.0 L=1 D=1 P=1 T=500"], [call(1, 100, seed="0x00000099", pid=222)])
        verdicts, text = r.compare()
        self.assertEqual(C.V_INPUTS, verdicts[60479], text)
        self.assertIn("first tick at which the state differs: 100", text)
        self.assertIn("fields seed", text)
        self.assertIn("only A: c.con@5.0,6.0,0.0", text)
        self.assertIn("only B: d.con@7.0,8.0,0.0", text)

    def test_different_arguments_count_as_inputs(self):
        r = Run(self)
        r.write("A", BASE + ["t=100 town=60479 n=3 h=bbbb", "+ c.con@5.0,6.0,0.0 L=1 D=1 P=1 T=500"], [call(1, 100, a6="0x0000000000000006")])
        r.write("B", BASE + ["t=100 town=60479 n=3 h=cccc", "+ d.con@7.0,8.0,0.0 L=1 D=1 P=1 T=500"], [call(1, 100, a6="0x0000000100000006")])
        verdicts, text = r.compare()
        self.assertEqual(C.V_INPUTS, verdicts[60479])
        self.assertIn("a6", text)

    def test_the_state_differing_before_the_call_is_inputs(self):
        # the component values differ at tick 95 although the buildings are the same: the later difference is downstream
        r = Run(self)
        r.write("A", BASE + ["t=95 town=60479 info dev=1 size=[1,1,2] needs=[[],[],[]]", "t=100 town=60479 n=3 h=bbbb", "+ c.con@5.0,6.0,0.0 L=1 D=1 P=1 T=500"], [call(1, 100)])
        r.write("B", BASE + ["t=100 town=60479 n=3 h=cccc", "+ d.con@7.0,8.0,0.0 L=1 D=1 P=1 T=500"], [call(1, 100)])
        verdicts, text = r.compare()
        self.assertEqual(C.V_INPUTS, verdicts[60479])
        self.assertIn("first tick at which the state differs: 95", text)
        self.assertIn("(info)", text)

    def test_identical_inputs_and_calls_but_different_buildings_is_nondeterminism(self):
        r = Run(self)
        r.write("A", BASE + ["t=100 town=60479 n=3 h=bbbb", "+ c.con@5.0,6.0,0.0 L=1 D=1 P=1 T=500"], [call(1, 99), call(2, 100, ret="0x1")])
        r.write("B", BASE + ["t=100 town=60479 n=3 h=cccc", "+ d.con@7.0,8.0,0.0 L=1 D=1 P=1 T=500"], [call(1, 99, pid=9), call(2, 100, ret="0x1", pid=9)])
        verdicts, text = r.compare()
        self.assertEqual(C.V_SAME, verdicts[60479], text)
        self.assertIn("identical seed, flag, arguments 6/7", text)

    def test_a_change_with_no_develop_call_is_a_non_growth_writer_and_commands_are_listed(self):
        r = Run(self)
        a = BASE + ["t=100 town=60479 n=2 h=aaaa", "t=130 town=60479 info dev=1 size=[1,1,1] needs=[[],[],[]]"]
        b = BASE + ["t=120 town=60479 n=2 h=dddd", "~ a.con@1.0,2.0,0.0 L=1>2 D=1 P=1 T=10>1200", "t=118 cmd townUpdateCargoNeeds town=60479 pi=0", "t=119 cmd onPreBuildProposal pi=1"]
        r.write("A", a, [call(1, 100)])
        r.write("B", b, [call(1, 100)])
        verdicts, text = r.compare()
        self.assertEqual(C.V_WRITER, verdicts[60479], text)
        self.assertIn("~ a.con@1.0,2.0,0.0 L=1>2", text)
        self.assertIn("townUpdateCargoNeeds town=60479 pi=0", text)
        self.assertIn("onPreBuildProposal pi=1", text)

    def test_a_one_tick_offset_is_reported_as_a_timing_offset(self):
        r = Run(self)
        r.write("A", BASE + ["t=100 town=60479 n=3 h=bbbb", "+ c.con@5.0,6.0,0.0 L=1 D=1 P=1 T=500"], [call(1, 100)])
        r.write("B", BASE + ["t=101 town=60479 n=3 h=bbbb", "+ c.con@5.0,6.0,0.0 L=1 D=1 P=1 T=500"], [call(1, 100)])
        verdicts, text = r.compare()
        # a constant sampling offset is not a divergence
        self.assertEqual(C.V_NONE, verdicts[60479], text)

    def test_environment_float_jitter_is_not_a_divergence(self):
        r = Run(self)
        r.write("A", BASE + ["t=100 town=60479 info dev=1 size=[1,1,1] needs=[[],[],[]] noise=63 poll=3082"], [call(1, 100)])
        r.write("B", BASE + ["t=100 town=60479 info dev=1 size=[1,1,1] needs=[[],[],[]] noise=63 poll=3075"], [call(1, 100)])
        verdicts, text = r.compare()
        self.assertEqual(C.V_NONE, verdicts[60479], text)

    def test_all_mode_baseline_without_building_lines_still_compares(self):
        r = Run(self)
        base = ["# town_trace start tick=90 spec=all", "t=90 town=7 n=40 h=aaaa base"]
        r.write("A", base + ["t=100 town=7 n=41 h=bbbb", "+ c.con@5.0,6.0,0.0 L=1 D=1 P=1 T=500"], [call(1, 100, town=7)])
        r.write("B", base + ["t=100 town=7 n=41 h=cccc", "+ d.con@7.0,8.0,0.0 L=1 D=1 P=1 T=500"], [call(1, 100, town=7)])
        verdicts, text = r.compare()
        self.assertEqual(C.V_SAME, verdicts[7])
        self.assertIn("a baseline did not list the buildings", text)

    def test_town_filter_quiet_and_missing_files(self):
        r = Run(self)
        verdicts, text = r.compare()
        self.assertEqual({}, verdicts)
        self.assertIn("was FEVERLINK3_TOWN_TRACE set", text)
        both = BASE + ["t=90 town=5 n=0 h=0 base"]
        for w in "AB":
            r.write(w, both, [])
        verdicts, text = r.compare("--town", "5", "--quiet")
        self.assertEqual({5: C.V_NONE}, verdicts)
        self.assertIn("town 5: " + C.V_NONE, text)

    def test_the_parsers_read_the_lua_and_native_formats(self):
        r = Run(self)
        r.write("A", BASE + ["t=100 cmd townUpdateSize town=60479 pi=0", "t=100 cmd x"], [call(1, 100, used="0x00001234")])
        tr = C.parse_trace(os.path.join(r.a, "town_trace.txt"))
        self.assertEqual([(100, "townUpdateSize", 60479, False), (100, "x", None, None)], tr.cmds)
        t = tr.towns[60479]
        self.assertEqual(1, len(t.events))
        self.assertEqual((90, 2, "aaaa", True), t.events[0][:4])
        self.assertEqual(2, len(t.events[0][4]))
        self.assertEqual(1, len(t.info))
        c = C.parse_growth(os.path.join(r.a, "growth.log"))[0]
        self.assertEqual((100, 60479, "111", "0x00001234"), (c.tick, c.town, c.f["pid"], c.f["used"]))


if __name__ == "__main__":
    unittest.main()
