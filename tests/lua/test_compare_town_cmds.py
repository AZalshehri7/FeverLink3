"""Tests for tools/gametest/compare_town_cmds.py with synthetic town_trace.txt files of two games.

Run:  python -m unittest discover -s tests/lua -p "test_compare_town_cmds.py"
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

import compare_town_cmds as C  # noqa: E402


def mk(tick, f32="3f000000,3f800000,3e800000", town=60479, k=1, name="makeTownUpdateSizeCmd", lu="0.1", sd="1/2,3/4,0/1", bld="12:ab", size="[0.5,1,0.25]"):
    return [
        "t=%d uc=%d mk %s town=%d k=%d flag=0 f32=%s f64=0,0,0 v=0.5,1,0.25" % (tick, tick * 2, name, town, k, f32),
        "t=%d town=%d in k=%d lu=%s sd=%s dev=1 size=%s cap=[1,2,3] scale=[1,1,1] needs=[[],[],[]] bld=%s" % (tick, town, k, lu, sd, size, bld),
    ]


def order(tick, h="3:aa", town=60479):
    return ["t=%d town=%d order pos=1 towns=%s cargos=5:bb" % (tick, town, h)]


class CmpTest(unittest.TestCase):
    def run_cmp(self, a, b, argv=()):
        root = tempfile.mkdtemp(prefix="fl3-cmds-")
        self.addCleanup(shutil.rmtree, root, True)
        dirs = []
        for name, lines in (("a", a), ("b", b)):
            d = os.path.join(root, name)
            os.makedirs(d)
            with open(os.path.join(d, "town_trace.txt"), "w", encoding="utf-8") as f:
                f.write("\n".join(lines) + "\n")
            dirs.append(d)
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            v = C.main(dirs + list(argv))
        return v, buf.getvalue()

    def test_all_equal(self):
        lines = mk(100) + order(100) + mk(220)
        v, out = self.run_cmp(lines, list(lines))
        self.assertEqual(C.V_EQUAL, v)
        self.assertIn("verdict: all equal", out)

    def test_payload_differs_reports_first_call_float_and_inputs(self):
        a = mk(100) + mk(220) + mk(340)
        b = mk(100) + mk(220, f32="3f000000,3f800001,3e800000", lu="0.2") + mk(340, f32="0,0,0")
        v, out = self.run_cmp(a, b)
        self.assertEqual(C.V_PAYLOAD, v)
        self.assertIn("at tick 220", out)
        self.assertIn("float 2 differs", out)
        self.assertIn("!= lu", out)
        self.assertNotIn("tick 340", out)

    def test_inputs_differ_payload_equal(self):
        a = mk(100) + mk(220)
        b = mk(100) + mk(220, sd="1/2,3/5,0/1")
        v, out = self.run_cmp(a, b)
        self.assertEqual(C.V_INPUTS, v)
        self.assertIn("input sd", out)

    def test_building_list_difference_is_an_input_difference(self):
        v, out = self.run_cmp(mk(100), mk(100, bld="13:cd"))
        self.assertEqual(C.V_INPUTS, v)

    def test_cargo_needs_payload(self):
        def needs(h):
            return ["t=100 uc=200 mk makeTownUpdateCargoNeedsCmd town=60479 k=1 flag=1 needs=[[1],[],[]] nh=%s" % h]
        v, out = self.run_cmp(needs("aa"), needs("bb"))
        self.assertEqual(C.V_PAYLOAD, v)
        self.assertIn("field nh", out)

    def test_order_difference_is_reported_but_not_the_verdict(self):
        a = mk(100) + order(100)
        b = mk(100) + order(100, h="3:zz")
        v, out = self.run_cmp(a, b)
        self.assertEqual(C.V_EQUAL, v)
        self.assertIn("first iteration-order difference: town 60479 from tick 100", out)

    def test_order_lines_are_carried_forward(self):
        a = order(100) + order(130, h="3:xx")
        b = order(100) + order(110, h="3:xx") + order(130, h="3:xx")
        v, out = self.run_cmp(mk(100) + a, mk(100) + b)
        self.assertIn("first iteration-order difference: town 60479 from tick 110", out)

    def test_unpaired_calls_are_counted_and_no_data(self):
        v, out = self.run_cmp(mk(100), mk(200))
        self.assertEqual(C.V_NODATA, v)
        self.assertIn("only in A: 1, only in B: 1", out)
        v, out = self.run_cmp(["# town_trace start"], ["# town_trace start"])
        self.assertEqual(C.V_NODATA, v)

    def test_town_and_from_tick_filters(self):
        a = mk(100, town=1) + mk(220, town=2)
        b = mk(100, town=1, f32="0,0,0") + mk(220, town=2)
        self.assertEqual(C.V_PAYLOAD, self.run_cmp(a, b)[0])
        self.assertEqual(C.V_EQUAL, self.run_cmp(a, b, ["--town", "2"])[0])
        self.assertEqual(C.V_EQUAL, self.run_cmp(a, b, ["--from-tick", "200"])[0])

    def test_missing_files_do_not_raise(self):
        root = tempfile.mkdtemp(prefix="fl3-cmds-")
        self.addCleanup(shutil.rmtree, root, True)
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(C.V_NODATA, C.main([root, root]))


if __name__ == "__main__":
    unittest.main()
