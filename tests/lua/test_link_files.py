"""The link files from the mod's side (docs/NATIVE_LINK.md section 3, the launcher's gamelink "Generations"):

- every control line has one writer: the interface half writes native_ctl.gui.log, the simulation half native_ctl.sim.log, each numbered
  on its own by a writer made once per Lua state (no tail of the file is read again per order), so the module, which keeps a last
  sequence per file, never drops a line for a number the other half used;
- every link file is a generation log: a writer ends a full generation with a `rotate` line and goes on in the next file, a reader
  follows it without losing or splitting a line, old generations are deleted; a reader sees a file truncated and written again
  past its old length between two polls;
- the events file is read every frame without an open per frame; mod.log is capped across both halves.

Run:  python -I -m unittest discover -s tests/lua -p "test_link_files.py"
"""
import os
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import lupa.lua52 as lupa  # noqa: E402

import test_inject_fail_closed as F  # noqa: E402
import test_lockstep as T  # noqa: E402
import test_strict_build as S  # noqa: E402

TAB = "\t"

# counts every io.open by file name and mode (the mod's C.IO is the io table)
COUNT_OPENS = r'''
OPENS = {}
local real = io.open
io.open = function(path, mode)
  local k = tostring(path):match("[^\\/]+$") .. " " .. tostring(mode)
  OPENS[k] = (OPENS[k] or 0) + 1
  return real(path, mode)
end
function OPENS_OF(name, mode) return OPENS[name .. " " .. mode] or 0 end
function OPENS_RESET() OPENS = {} end
'''


def records(path):
    """(seq, kind) of every line of a file, [] when it is missing."""
    if not os.path.exists(path):
        return []
    out = []
    for line in T.read_text(path).splitlines():
        parts = line.split(TAB, 3)
        if len(parts) == 4 and parts[0].isdigit():
            out.append((int(parts[0]), parts[1]))
    return out


class CommonLinkTest(unittest.TestCase):
    """The helpers of feverlink3_common.lua on their own."""

    def setUp(self):
        os.environ.pop("FEVERLINK3_SESSION_DIR", None)
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.L.execute(COUNT_OPENS)
        self.C = self.L.execute(T.COMMON_SRC)
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-link-files-")
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)

    def path(self, name):
        return os.path.join(self.tmp, name)

    def opens(self, name, mode):
        return int(self.L.eval("OPENS_OF(%s, %s)" % (T.lua_str(name), T.lua_str(mode))))

    def send(self, w, kind, n):
        self.assertTrue(self.C.writerSend(w, kind, self.L.eval("{ n = %d }" % n)))

    def poll(self, r):
        return [(int(x.seq), x.kind) for x in self.C.readerPoll(r).values()]

    def test_a_writer_reads_the_logs_tail_once_when_it_is_made_and_never_per_line(self):
        base = self.path("native_ctl.sim.log")
        with open(base, "w", encoding="utf-8", newline="") as f:
            f.write("1\tping\t1\t{}\n2\tping\t1\t{}\n")
        w = self.C.newWriter(base, 1)
        self.assertEqual(int(w.seq), 2, "the numbering goes on after the log's last line")
        self.L.execute("OPENS_RESET()")
        for n in range(1, 21):
            self.send(w, "inject_next", n)
        self.assertEqual(self.opens("native_ctl.sim.log", "rb"), 0, "the file is not read again for an order")
        self.assertEqual(self.opens("native_ctl.sim.log", "ab"), 20)
        self.assertEqual([s for s, _ in records(base)], list(range(1, 23)))

    def test_a_reader_follows_the_writers_generations_without_losing_or_splitting_a_line(self):
        base = self.path("from_game.log")
        w = self.C.newWriter(base, 1, self.L.eval("{ rotate = 200, keep = 3 }"))
        r = self.C.newReader(base)
        got = []
        for n in range(1, 81):
            self.send(w, "clock", n)
            if n % 3 == 0:
                got += self.poll(r)
        for _ in range(5):
            got += self.poll(r)
        self.assertEqual(got, [(n, "clock") for n in range(1, 81)], "every line exactly once, in order")
        self.assertEqual(int(r.dropped), 0, "a rotate line is not a dropped record")
        newest = int(self.C.newestGen(base))
        self.assertGreater(newest, 3)
        self.assertEqual(newest, int(w.gen))
        self.assertFalse(os.path.exists(base), "older generations are deleted")
        self.assertTrue(os.path.exists(self.C.genPath(base, newest - 2)), "the newest three are kept")
        # the rotate lines: seq 0, named generation
        ended = T.read_text(self.C.genPath(base, newest - 1)).splitlines()[-1].split(TAB)
        self.assertEqual((ended[0], ended[1], ended[3]), ("0", "rotate", '{["gen"]=%d}' % newest))
        # a writer made again (a reload) goes on in the newest generation with the next number
        w2 = self.C.newWriter(base, 1, self.L.eval("{ rotate = 200, keep = 3 }"))
        self.assertEqual((int(w2.gen), int(w2.seq)), (newest, 80))
        # a reader made now: from the oldest file still there, or the newest from its top, or at the end
        oldest = self.C.newReader(base)
        self.assertEqual(int(oldest.gen), newest - 2)
        top = self.C.newReader(base, self.L.eval('{ start = "newest" }'))
        first_of_newest = int(T.read_text(self.C.genPath(base, newest)).split(TAB)[0])
        self.assertEqual(self.poll(top)[0][0], first_of_newest)
        at_end = self.C.newReader(base, self.L.eval('{ start = "end" }'))
        self.assertEqual(self.poll(at_end), [])
        self.send(w2, "clock", 81)
        self.assertEqual(self.poll(at_end), [(81, "clock")])

    def test_a_reader_opened_right_after_a_rotate_line_starts_in_the_next_generation(self):
        base = self.path("ui_in.log")
        w = self.C.newWriter(base, 1, self.L.eval("{ rotate = 1 }"))
        self.send(w, "result", 1)
        self.assertTrue(self.C.writerRotate(w))
        r = self.C.newReader(base, self.L.eval('{ start = "end" }'))
        self.assertEqual(int(r.gen), 2)
        self.send(w, "result", 2)
        self.assertEqual(self.poll(r), [(2, "result")])

    def test_a_kept_reader_opens_the_file_once_and_sees_a_truncate_and_regrow_between_two_polls(self):
        base = self.path("native_events.log")
        with open(base, "w", encoding="utf-8", newline="") as f:
            f.write("1\tready\tnative\t{}\n2\tpong\tnative\t{}\n")
        r = self.C.newReader(base, self.L.eval('{ start = "newest", keep = true }'))
        self.L.execute("OPENS_RESET()")
        self.assertEqual(self.poll(r), [(1, "ready"), (2, "pong")])
        for _ in range(30):
            self.assertEqual(self.poll(r), [])
        with open(base, "a", encoding="utf-8", newline="") as f:
            f.write("3\theld\tnative\t{}\n")
        self.assertEqual(self.poll(r), [(3, "held")])
        self.assertEqual(self.opens("native_events.log", "rb"), 1, "one open for 33 polls")
        # truncated and written again past the old length between two polls: read from the top, history dropped by number
        with open(base, "w", encoding="utf-8", newline="") as f:
            f.write("4\tready\tnative\t{[\"build\"]=\"again\"}\n5\tpong\tnative\t{}\n6\theld\tnative\t{}\n7\tpong\tnative\t{}\n")
        self.assertEqual(self.poll(r), [(4, "ready"), (5, "pong"), (6, "held"), (7, "pong")])

    def test_mod_log_is_capped_across_both_halves(self):
        os.environ["FEVERLINK3_SESSION_DIR"] = self.tmp
        try:
            halves = [self.L.execute(T.COMMON_SRC) for _ in range(2)]   # two Lua states' copies of the module
        finally:
            os.environ.pop("FEVERLINK3_SESSION_DIR", None)
        for C in halves:
            C.LOG_BYTES, C.LOG_KEEP = 2000, 3
        logs = [halves[0].logger("sim"), halves[1].logger("gui")]
        for i in range(400):
            logs[i % 2]("line %d %s" % (i, "x" * 40))
        base = self.path("mod.log")
        newest = int(halves[0].newestGen(base))
        self.assertGreater(newest, 3)
        files = [n for n in os.listdir(self.tmp) if n.startswith("mod.") and n.endswith(".log")]
        self.assertEqual(len(files), 3, files)
        for n in files:
            self.assertLess(os.path.getsize(self.path(n)), 2000 + 200, n)
        self.assertIn("line 399", T.read_text(halves[0].genPath(base, newest)))


class WriterIdentityTest(S.StrictTest):
    """The real mod, two games, the fake module with the deferred inject flow (test_inject_fail_closed.py)."""

    setUp = F.InjectFailClosedTest.setUp
    build_on_b = F.InjectFailClosedTest.build_on_b
    strict_cancel = F.InjectFailClosedTest.strict_cancel
    extra_env = F.InjectFailClosedTest.extra_env

    def test_each_half_writes_its_own_control_file_and_the_module_hears_every_line(self):
        self.build_on_b()
        self.assertEqual((len(self.A.native.injected), len(self.B.native.injected)), (1, 1), self.log())
        self.assertEqual(self.B.native.completed, [(1, True, "stamp_apply")])
        for g in (self.A, self.B):
            gui, sim = records(g.native.ctl), records(g.native.ctl_sim)
            gui_kinds, sim_kinds = {k for _, k in gui}, {k for _, k in sim}
            self.assertIn("hold_on", gui_kinds, g.label)
            self.assertIn("inject_next", sim_kinds, "%s: the stamp apply's order is the simulation half's" % g.label)
            self.assertFalse(sim_kinds & {"hold_on", "ping", "defer"}, "%s: the interface's lines are not in the simulation's file" % g.label)
            for lines in (gui, sim):
                self.assertEqual([s for s, _ in lines], list(range(1, len(lines) + 1)), "%s: each file is numbered on its own" % g.label)
            heard = [(s, k) for _, s, k, _ in g.native.heard]
            for rec in gui + sim:
                self.assertIn(rec, heard, "%s: the module heard every line" % g.label)
        self.assertIn("defer", {k for _, k in records(self.B.native.ctl)}, "the originator's verdict is the interface half's")
        self.assertIn("written to native_ctl.sim.log seq 1", self.trace(self.B))

    def test_no_control_file_is_read_per_order_and_the_events_file_is_not_opened_per_frame(self):
        self.start()
        self.strict_on(self.A, self.B)
        for g in (self.A, self.B):
            g.native.native_text = F.road_text()
            g.native.input_text = F.road_text()
        self.B.tool(self.road(300, 0, 400, 0), make="NATIVE_ROAD")   # the first build makes the simulation half's writer
        self.tick(30)
        self.B.L.execute(COUNT_OPENS)
        self.B.native.native_text = F.road_text(500.0, 600.0)
        self.B.native.input_text = F.road_text(500.0, 600.0)
        self.B.tool(self.road(500, 0, 600, 0), make="NATIVE_ROAD")
        self.tick(30)
        self.assertEqual(len(self.B.native.injected), 2, self.log())
        opens = lambda name, mode: int(self.B.L.eval("OPENS_OF(%s, %s)" % (T.lua_str(name), T.lua_str(mode))))
        self.assertEqual(opens("native_ctl.gui.log", "rb") + opens("native_ctl.sim.log", "rb"), 0, "no tail re-read")
        self.assertGreater(opens("native_ctl.gui.log", "ab") + opens("native_ctl.sim.log", "ab"), 2)
        self.assertEqual(opens("native_events.log", "rb"), 0, "the events file stays open: no open in 120 frames")

    def trace(self, g):
        return T.read_text(os.path.join(g.dir, "trace.log"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
