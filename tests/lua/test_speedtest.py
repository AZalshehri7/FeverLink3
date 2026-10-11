"""Tests for the text and parameters of the window's Steam speed test (feverlink3_common.lua), and a syntax check of the
window script that uses them (the window itself needs the game's UI library and cannot run here).

Run:  python -m unittest discover -s tests/lua -p "test_speedtest.py"
"""
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import test_lockstep as T  # noqa: E402  (only for the lupa import)

lupa = T.lupa
CONTENT = os.path.join(HERE, "..", "..", "mod", "feverlink3_1", "content")


def read(name):
    with open(os.path.join(CONTENT, name), encoding="utf-8") as f:
        return f.read()


class SpeedTextTest(unittest.TestCase):
    def setUp(self):
        self.lua = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.lua.execute("function ug_require(n) error('no ' .. n) end")
        self.C = self.lua.execute("local src = ...; return assert(load(src, 'common', 't'))()", read("feverlink3_common.lua"))

    def lines(self, speed_lua):
        fn = self.lua.eval("function(s, C) return C.speedLines(s) end")
        out = fn(self.lua.eval(speed_lua) if speed_lua is not None else None, self.C)
        return [out[i] for i in range(1, len(out) + 1)]

    def test_no_test_started_shows_nothing(self):
        self.assertEqual([], self.lines(None))
        self.assertEqual([], self.lines("{}"))

    def test_progress_shows_the_status_line(self):
        self.assertEqual(
            ["Speed test, sending to your friend: 4.2 of 32 MB, 1.50 MB/s"],
            self.lines('{running=true, status="Speed test, sending to your friend: 4.2 of 32 MB, 1.50 MB/s", lines={}}'),
        )

    def test_a_failure_shows_the_reason(self):
        self.assertEqual(
            ["Speed test failed: no data for 60 s"],
            self.lines('{running=false, status="Speed test failed: no data for 60 s", lines={}}'),
        )

    def test_a_result_shows_all_its_lines_not_the_status(self):
        speed = '{running=false, status="a", lines={"a","b","c","d"}}'
        self.assertEqual(["a", "b", "c", "d"], self.lines(speed))

    def test_the_size_is_one_of_the_offered_choices(self):
        choice = self.C.speedChoice
        self.assertEqual([8, 32, 80], [self.C.SPEED_CHOICES[i] for i in (1, 2, 3)])
        self.assertEqual(80, choice(80))
        self.assertEqual(8, choice(8))
        self.assertEqual(32, choice(32))
        for odd in (None, 0, 5, "huge", 1e9):
            self.assertEqual(32, choice(odd), odd)

    def test_the_command_carries_the_id_and_the_size(self):
        params = self.C.speedParams("76561199000000042", 80)
        self.assertEqual("76561199000000042", params["id"])
        self.assertEqual(80, params["mb"])
        self.assertEqual(32, self.C.speedParams("1", 7)["mb"])

    def test_the_window_script_still_compiles(self):
        check = self.lua.eval("function(src) local f, err = load(src, 'ui', 't'); return f ~= nil, err end")
        ok, err = check(read("feverlink3_ui.script.lua"))
        self.assertTrue(ok, err)


if __name__ == "__main__":
    unittest.main()
