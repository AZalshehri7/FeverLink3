"""The Lua string-hash seed probe (feverlink3_common.lua): observation only, inert unless FEVERLINK3_LUA_SEED_LOG=1,
never raises, writes one `luaseed probe order=<n>` line per Lua state.

Run:  python -I tests/lua/test_luaseed_probe.py
"""
import os
import re
import unittest

import lupa.lua52 as lupa

import test_lockstep as T

HERE = os.path.dirname(os.path.abspath(__file__))
CONTENT = os.path.join(HERE, "..", "..", "mod", "feverlink3", "content")
VAR = "FEVERLINK3_LUA_SEED_LOG"


def read(name):
    with open(os.path.join(CONTENT, name), encoding="utf-8") as f:
        return f.read()


class SeedProbe(unittest.TestCase):
    def setUp(self):
        self.saved = {k: os.environ.get(k) for k in (VAR, "FEVERLINK3_SESSION_DIR")}
        os.environ.pop("FEVERLINK3_SESSION_DIR", None)
        os.environ.pop(VAR, None)
        self.lua = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.C = T.load_common(self.lua)
        self.lines = []
        self.log = lambda msg: self.lines.append(msg)

    def tearDown(self):
        for k, v in self.saved.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v

    def test_off_by_default_writes_nothing(self):
        for value in (None, "", "0", "true", "fixed"):
            if value is None:
                os.environ.pop(VAR, None)
            else:
                os.environ[VAR] = value
            self.C.seedProbed = None
            self.assertFalse(self.C.luaSeedProbeOnce(self.log), value)
        self.assertEqual(self.lines, [])

    def test_on_writes_one_line_once(self):
        os.environ[VAR] = "1"
        self.assertTrue(self.C.luaSeedProbeOnce(self.log))
        self.assertFalse(self.C.luaSeedProbeOnce(self.log), "one-shot")
        self.assertEqual(len(self.lines), 1)
        self.assertRegex(self.lines[0], r"^luaseed probe order=\d+$")

    def test_the_order_is_a_stable_number_inside_one_state(self):
        a, b = self.C.luaSeedProbe(), self.C.luaSeedProbe()
        self.assertEqual(a, b)
        self.assertEqual(a, int(a))
        self.assertGreater(a, 0)

    def test_it_never_raises(self):
        os.environ[VAR] = "1"
        # a logger that raises
        self.assertTrue(self.C.luaSeedProbeOnce(lambda m: 1 / 0))
        # an os.getenv that raises
        self.lua.execute("os.getenv = function() error('no env') end")
        self.C.seedProbed = None
        self.assertFalse(self.C.luaSeedProbeOnce(self.log))
        # a probe that fails is reported, not raised
        self.lua.execute("os.getenv = function() return '1' end")
        self.lua.execute("local C = ...; C.luaSeedProbe = function() error('x') end", self.C)
        self.C.seedProbed = None
        self.assertTrue(self.C.luaSeedProbeOnce(self.log))
        self.assertEqual(self.lines[-1], "luaseed probe order=error")

    def test_the_bridge_calls_it_once_under_pcall(self):
        src = read("feverlink3_bridge.script.lua")
        self.assertEqual(len(re.findall(r"pcall\(C\.luaSeedProbeOnce, logSim\)", src)), 1)


if __name__ == "__main__":
    unittest.main()
