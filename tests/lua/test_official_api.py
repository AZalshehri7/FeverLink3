"""What of the mod is not the official TF3 scripting API, and where it is (owner decision 2026-10-18): every use of Lua's io and os
libraries is in feverlink3_link.lua, every log and diagnostic file goes through feverlink3_log.lua, and without the link module the mod
has no file access and stays inactive (fail closed).

Run:  python -I -m unittest discover -s tests/lua -p "test_official_api.py"
"""
import os
import re
import shutil
import tempfile
import unittest

import lupa.lua52 as lupa

import test_lockstep as T

CONTENT = T.CONTENT
LINK = "feverlink3_link.lua"
# io.x / os.x, the io module taken from package.loaded, and the io global read as a value
RAW = re.compile(r'\bos\s*\.\s*\w|\bio\s*\.\s*\w|package\.loaded\s*\[\s*"io"\s*\]|return io\b')


def lua_files():
    return sorted(n for n in os.listdir(CONTENT) if n.endswith(".lua"))


class OnlyTheLinkModuleUsesIoAndOs(unittest.TestCase):
    def test_only_the_link_module_names_io_or_os_and_it_documents_why(self):
        found = []
        for name in lua_files():
            if name == LINK:
                continue
            with open(os.path.join(CONTENT, name), encoding="utf-8") as f:
                for i, line in enumerate(f, 1):
                    if RAW.search(line):
                        found.append("%s:%d: %s" % (name, i, line.strip()))
        self.assertEqual([], found, "io/os outside feverlink3_link.lua (route it through C.env / C.clock / C.readFile ...)")
        # the link module documents why each kind of use stays
        with open(os.path.join(CONTENT, LINK), encoding="utf-8") as f:
            head = f.read().split("local Link = {}")[0]
        for word in ("os.getenv", "os.clock", "os.time", "os.date", "os.remove", "package.loaded", "app.saveUserdata"):
            self.assertIn(word, head, word)

class Modules(unittest.TestCase):
    def setUp(self):
        self.saved = {k: os.environ.get(k) for k in ("FEVERLINK3_SESSION_DIR", "FEVERLINK3_PLAYER_ID", "FEVERLINK3_PLAYER_NAME")}
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-official-")
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        os.environ.update(FEVERLINK3_SESSION_DIR=self.tmp, FEVERLINK3_PLAYER_ID="1", FEVERLINK3_PLAYER_NAME="p")

    def tearDown(self):
        for k, v in self.saved.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v

    def bare(self, missing=()):
        """common.lua in a bare state whose ug_require has the link and log modules, minus `missing`."""
        rt = lupa.LuaRuntime(unpack_returned_tuples=True)
        g = rt.globals()
        g.LINK_SRC, g.LOG_SRC = T.LINK_SRC, T.LOG_SRC
        g.MISSING = rt.table_from({m: True for m in missing})
        rt.execute('''
function ug_require(p)
  for m in pairs(MISSING) do if p:find(m, 1, true) then error("not found: " .. p) end end
  if p:find("feverlink3_link.lua", 1, true) then return assert(load(LINK_SRC, "=link", "t"))() end
  if p:find("feverlink3_log.lua", 1, true) then return assert(load(LOG_SRC, "=log", "t"))() end
  error("no module " .. tostring(p))
end''')
        return rt, rt.execute(T.COMMON_SRC)

    def test_without_the_link_module_the_mod_is_inactive_without_the_log_module_it_logs_nowhere_with_both_it_logs(self):
        _rt, C = self.bare(missing=("feverlink3_link",))
        self.assertFalse(C.LINK_MODULE)
        self.assertIsNone(C.IO)
        self.assertFalse(C.ACTIVE)
        self.assertIsNone(C.DIR)   # the session folder comes from the environment, which only the link module reads
        self.assertIsNone(C.env("FEVERLINK3_SESSION_DIR"))
        self.assertFalse(C.appendFile(os.path.join(self.tmp, "x.txt"), "x"))
        self.assertIsNone(C.readFile(os.path.join(self.tmp, "x.txt")))
        w = C.newWriter(os.path.join(self.tmp, "from_game.log"), 1)
        self.assertEqual((False, "no file access"), tuple(C.writerSend(w, "k", {})))
        self.assertEqual(0, len(list(C.readerPoll(C.newReader(os.path.join(self.tmp, "to_game.log"))).values())))
        self.assertFalse(C.poll(True))
        C.logger("t")("no file")   # never raises
        self.assertFalse(C.dump("diag.txt", "a"))
        self.assertEqual([], os.listdir(self.tmp))
        # without the log module the mod still runs and logs nowhere
        _rt, C = self.bare(missing=("feverlink3_log",))
        self.assertTrue(C.LINK_MODULE)
        self.assertFalse(C.LOG_MODULE)
        self.assertTrue(C.ACTIVE)
        C.logger("t")("no file")
        C.tracer("sim")("tag", "text")
        self.assertFalse(C.dump("diag.txt", "a"))
        self.assertFalse(os.path.exists(os.path.join(self.tmp, "mod.log")))
        # with both modules the mod is active and logs to its folder
        _rt, C = self.bare()
        self.assertTrue(C.LINK_MODULE and C.LOG_MODULE)
        self.assertTrue(C.ACTIVE)
        C.logger("t")("hello")
        C.tracer("sim")("tag", "text")
        self.assertTrue(C.dump("diag.txt", "a\n"))
        self.assertTrue(C.dump("diag.txt", "b\n"))
        self.assertTrue(C.dumpReplace("detail.txt", "x\n"))
        with open(os.path.join(self.tmp, "mod.log"), encoding="utf-8") as f:
            self.assertRegex(f.read(), r"^\d\d:\d\d:\d\d \[FeverLink3 t\] hello\n$")
        with open(os.path.join(self.tmp, "trace.log"), encoding="utf-8") as f:
            self.assertRegex(f.read(), r"^\d\d:\d\d:\d\d \+[\d.]+ sim tag text\n$")
        with open(os.path.join(self.tmp, "diag.txt"), encoding="utf-8") as f:
            self.assertEqual("a\nb\n", f.read())
        self.assertEqual(C.env("FEVERLINK3_PLAYER_NAME"), "p")
        self.assertIsInstance(C.clock(), float)

    def test_the_capped_log_the_official_api_probe_and_the_clock(self):
        rt, C = self.bare()
        base = os.path.join(self.tmp, "cap.log")
        for i in range(8):
            self.assertTrue(C.appendCapped(base, "line %d ab0123456789\n" % i, 40, 2))
        files = sorted(n for n in os.listdir(self.tmp) if n.startswith("cap."))
        # 20 bytes a line, 40 a file: two lines a generation, 4 generations, the 2 newest kept
        self.assertEqual(["cap.3.log", "cap.4.log", "cap.gen"], files)
        with open(os.path.join(self.tmp, "cap.4.log"), encoding="utf-8") as f:
            self.assertEqual("line 6 ab0123456789\nline 7 ab0123456789\n", f.read())
        self.assertEqual("full", C.appendCapped(base, "more\n" * 10, 40, None, 4))
        # the official api probe never raises and names what it found
        rt, C = self.bare()
        text = C.officialProbe()
        for word in ("app=no", "saveUserdata=no", "getUserDataFolder=no", "io_global=table", "userdata=no"):
            self.assertIn(word, text)
        rt.execute("app = { saveUserdata = function() end, getUserDataFolder = function() return 'U' end }")
        self.assertIn("userdata=U", C.officialProbe())
        rt.execute("app = { getUserDataFolder = function() error('no') end }")
        self.assertIn("userdata=no", C.officialProbe())
        # the clock is read when used so a swapped os clock is seen
        rt, C = self.bare()
        rt.execute("os.clock = function() return 42 end")
        self.assertEqual(42, C.clock())
        rt.execute("os.clock = nil")
        self.assertRaises(Exception, C.clock)   # where the old direct call raised, this raises too

if __name__ == "__main__":
    unittest.main()
