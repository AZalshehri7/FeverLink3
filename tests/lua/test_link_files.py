"""The link files from the mod's side (docs/NATIVE_LINK.md section 3, the launcher's gamelink "Generations"):

- a line carries no number: every reader reads by position (the generation and the bytes it consumed), complete lines only, and
  drops nothing for what a line says. The bug this replaces (friend test 2026-10-16, "could not buy cars"): after a resync reload the
  window's new writer read only the last 4 KB of ui_out.log, found no number in a 4.7 KB line, started again at 1, and the bridge
  dropped every later window command as old;
- the C runtime's append is a seek and a write, two steps, so two Lua states appending to one file from two threads overwrite each
  other's lines; a file several Lua states write (the native control files) is one file per writer: each writer names its own file
  <family>.w<id>.log, unique without coordination (the game's Lua has no rename and no exclusive open); the module lists the folder;
- every link file is a generation log: a writer ends a full generation with a `rotate` line and goes on in the next file, a reader
  follows it without losing or splitting a line, old generations are deleted; a file shorter than what was read, or one truncated and
  written again past its old length, is read again from its first byte;
- the events file is read every frame without an open per frame; mod.log is capped across both halves.

Run:  python -I -m unittest discover -s tests/lua -p "test_link_files.py"
"""
import os
import re
import shutil
import sys
import tempfile
import threading
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
    """(kind, payload) of every line of a file, [] when it is missing."""
    if not os.path.exists(path):
        return []
    out = []
    for line in T.read_text(path).splitlines():
        parts = line.split(TAB, 2)
        if len(parts) == 3:
            out.append((parts[0], parts[2]))
    return out


def slot_files(d, half):
    """The control files of a half's family (native_ctl.<half>.w<id>.log), as the module finds them by listing the folder."""
    return [os.path.join(d, n) for n in sorted(os.listdir(d)) if re.match(r"^native_ctl\.%s\.w[0-9a-f]+\.log$" % half, n)]


class CommonLinkTest(unittest.TestCase):
    """The helpers of feverlink3_common.lua on their own."""

    def setUp(self):
        os.environ.pop("FEVERLINK3_SESSION_DIR", None)
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.L.execute(COUNT_OPENS)
        self.C = T.load_common(self.L)
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-link-files-")
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)

    def path(self, name):
        return os.path.join(self.tmp, name)

    def opens(self, name, mode):
        return int(self.L.eval("OPENS_OF(%s, %s)" % (T.lua_str(name), T.lua_str(mode))))

    def send(self, w, kind, n, C=None):
        C = C or self.C
        self.assertTrue(C.writerSend(w, kind, C.parse("{ n = %d }" % n)))

    def poll(self, r):
        return [(x.kind, int(self.C.parse(x.payload).n)) for x in self.C.readerPoll(r).values()]

    def test_a_writer_never_reads_its_log_and_opens_it_once_per_line(self):
        base = self.path("native_ctl.sim.w1.log")
        with open(base, "w", encoding="utf-8", newline="") as f:
            f.write("ping\t1\t{}\nping\t1\t{}\n")
        w = self.C.newWriter(base, 1)
        self.L.execute("OPENS_RESET()")
        for n in range(1, 21):
            self.send(w, "inject_next", n)
        self.assertEqual(self.opens("native_ctl.sim.w1.log", "rb"), 0, "the file is not read for an order")
        self.assertEqual(self.opens("native_ctl.sim.w1.log", "ab"), 20)
        self.assertEqual([k for k, _ in records(base)], ["ping", "ping"] + ["inject_next"] * 20)
        self.assertNotIn("lastSeqInFile", T.COMMON_SRC, "the numbering machinery is gone")

    def test_a_5000_byte_line_followed_by_a_reload_does_not_lose_later_commands(self):
        # friend test 2026-10-16: the window's last record was a 4728-byte line edit; after the resync reload the window's new writer
        # looked for the last number in the last 4 KB, found none, numbered from 1 again, and the bridge dropped every later command
        base = self.path("ui_out.log")
        before = self.C.newWriter(base, 0)
        self.assertTrue(self.C.writerSend(before, "cmd", self.C.parse('{ id = 41, body = "%s" }' % ("x" * 5000))))
        bridge = self.C.newReader(base)
        self.assertEqual([x.kind for x in self.C.readerPoll(bridge).values()], ["cmd"])
        cursor = self.C.posText(self.C.readerPos(bridge))   # what the resync cursor keeps for the window's records
        self.assertGreater(os.path.getsize(base), 5000)
        # the reload: a new Lua state with a new writer, and a new reader at the cursor
        L2 = lupa.LuaRuntime(unpack_returned_tuples=True)
        C2 = T.load_common(L2)
        after = C2.newWriter(base, 0)
        for n in range(1, 6):
            self.send(after, "cmd", n, C2)
        resumed = self.C.newReader(base, self.L.eval("{ at = %s }" % T.lua_str(cursor)))
        self.assertEqual(self.poll(resumed), [("cmd", n) for n in range(1, 6)], "every command after the reload arrives")
        self.assertEqual(self.poll(bridge), [("cmd", n) for n in range(1, 6)], "and a reader that went on reading gets them too")
        self.assertEqual(int(resumed.dropped) + int(bridge.dropped), 0)

    def test_two_writers_interleaving_lose_nothing(self):
        # two Lua states (two runtimes) make a control writer each in one family: two files, each line once in its writer's file,
        # in its writer's order, also with 5000-byte lines between
        family = self.path("native_ctl.sim")
        L2 = lupa.LuaRuntime(unpack_returned_tuples=True)
        C2 = T.load_common(L2)
        a = self.C.newOwnWriter(family, "1@a")
        b = C2.newOwnWriter(family, "1@b")
        self.assertNotEqual(str(a.path), str(b.path), "two writers never share a file")
        readers = [self.C.newReader(str(w.path)) for w in (a, b)]
        got = {0: [], 1: []}
        for n in range(1, 41):
            for i, (w, C) in enumerate(((a, self.C), (b, C2))):
                payload = '{ n = %d, pad = "%s" }' % (n, "y" * (5000 if n % 7 == 0 else 10))
                self.assertTrue(C.writerSend(w, "inject_next", C.parse(payload)))
            if n % 3 == 0:
                for i, r in enumerate(readers):
                    got[i] += self.poll(r)
        for i, r in enumerate(readers):
            got[i] += self.poll(r)
        for i in (0, 1):
            self.assertEqual(got[i], [("inject_next", n) for n in range(1, 41)], "writer %d: every line once, in order" % i)
        self.assertEqual(len(slot_files(self.tmp, "sim")), 2)

    def test_two_writers_on_two_threads_lose_nothing_with_one_file_each(self):
        # the real case: two Lua states on two threads append at the same moment, each to its own file
        family = self.path("native_ctl.sim")
        runtimes = [lupa.LuaRuntime(unpack_returned_tuples=True) for _ in range(2)]
        mods = [T.load_common(rt) for rt in runtimes]
        writers = [C.newOwnWriter(family, "1@%d" % i) for i, C in enumerate(mods)]
        code = "return function(C, w, n) for i = 1, n do C.writerSend(w, 'inject_next', { n = i, pad = string.rep('z', i % 5 == 0 and 5000 or 200) }) end end"
        fns = [rt.execute(code) for rt in runtimes]
        threads = [threading.Thread(target=fns[i], args=(mods[i], writers[i], 300)) for i in range(2)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        files = slot_files(self.tmp, "sim")
        self.assertEqual(len(files), 2)
        for p in files:
            ns = [int(self.C.parse(payload).n) for kind, payload in records(p)]
            self.assertEqual(ns, list(range(1, 301)), p)

    def test_a_writer_names_its_own_file_without_rename_or_any_os_function_but_the_clocks(self):
        # the game's Lua has io.open, os.time and os.clock, and no os.rename (real run 2026-10-18 02:33): the writer needs nothing else
        self.L.execute("os.rename, os.remove, os.date, os.getenv, os.tmpname = nil, nil, nil, nil, nil")
        family = self.path("native_ctl.gui")
        ws = [self.C.newOwnWriter(family, "1") for _ in range(50)]   # 50 writers made in the same instant, all alive
        paths = {str(w.path) for w in ws}
        self.assertEqual(len(paths), 50, "every writer names a file of its own")
        for p in paths:
            self.assertRegex(os.path.basename(p), r"^native_ctl\.gui\.w[0-9a-f]+\.log$")
        self.assertTrue(self.C.writerSend(ws[0], "ping", self.C.parse("{ n = 1 }")))
        self.assertEqual(len(slot_files(self.tmp, "gui")), 1, "the file appears with its first line")
        # a name whose file is there already is not taken again
        w = self.C.newOwnWriter(family, "1")
        self.assertNotIn(str(w.path), paths)
        self.assertNotIn("os.rename", T.LINK_SRC.split("function C.newOwnWriter")[1].split("\n\tend")[0])
    @unittest.skipUnless(os.environ.get("FEVERLINK3_APPEND_RACE") == "1", "a measurement, not a check (FEVERLINK3_APPEND_RACE=1)")
    def test_measure_two_lua_states_appending_to_one_file(self):
        # why the control files are one per writer: io.open(path, "ab") + one write per line from two Lua states on two threads. On
        # Windows (2026-10-18, lupa, 2 x 5000 lines of 200 bytes) about 7 % of the lines were lost (overwritten); with 5000-byte lines
        # most were lost or torn. Prints the counts; asserts nothing (a machine may not show the race on a given run).
        path = self.path("race.log").replace("\\", "/")
        code = "return function(path, tag, n, size) local pad = string.rep(tag, size) for i = 1, n do local f = io.open(path, 'ab') f:write(tag .. ' ' .. i .. ' ' .. pad .. '\\n') f:close() end end"
        runtimes = [lupa.LuaRuntime() for _ in range(2)]
        fns = [rt.execute(code) for rt in runtimes]
        threads = [threading.Thread(target=fns[i], args=(path, tag, 5000, 200)) for i, tag in enumerate("AB")]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        whole = {"A": 0, "B": 0}
        lines = open(path, "rb").read().decode("latin-1").split("\n")[:-1]
        for line in lines:
            parts = line.split(" ", 2)
            if len(parts) == 3 and parts[0] in whole and parts[2] == parts[0] * 200:
                whole[parts[0]] += 1
        print("two Lua states, one file: %d of 10000 lines whole, %d lines in the file" % (whole["A"] + whole["B"], len(lines)))

    def test_truncation_resets_the_reader(self):
        base = self.path("to_game.log")
        w = self.C.newWriter(base, 0)
        r = self.C.newReader(base)
        for n in range(1, 6):
            self.send(w, "action", n)
        self.assertEqual(self.poll(r), [("action", n) for n in range(1, 6)])
        # shorter: another file now (a cleaned folder), read from its first byte
        with open(base, "w", encoding="utf-8", newline="") as f:
            f.write('action\t0\t{["n"]=10}\n')
        self.assertEqual(self.poll(r), [("action", 10)])
        # truncated and written again past the old length between two polls: the first bytes differ, read from the top again
        with open(base, "w", encoding="utf-8", newline="") as f:
            f.write("".join('session\t0\t{["n"]=%d}\n' % n for n in range(20, 26)))
        self.assertEqual(self.poll(r), [("session", n) for n in range(20, 26)])
        # a reader started at a place beyond the file's end: the file was replaced, it is read from its first byte
        beyond = self.C.newReader(base, self.L.eval('{ at = "1:999999" }'))
        self.assertEqual(len(self.poll(beyond)), 6)

    def test_a_partial_last_line_waits_and_is_history_for_a_reader_started_at_the_end(self):
        base = self.path("from_game.log")
        with open(base, "w", encoding="utf-8", newline="") as f:
            f.write('clock\t1\t{["n"]=1}\nclock\t1\t{["n"]')
        r = self.C.newReader(base)
        self.assertEqual(self.poll(r), [("clock", 1)])
        self.assertEqual(self.C.posText(self.C.readerPos(r)), "1:%d" % len('clock\t1\t{["n"]=1}\n'))
        late = self.C.newReader(base, self.L.eval('{ start = "end" }'))
        with open(base, "a", encoding="utf-8", newline="") as f:
            f.write('=2}\nclock\t1\t{["n"]=3}\n')
        self.assertEqual(self.poll(r), [("clock", 2), ("clock", 3)], "the line's end came: it is read whole")
        self.assertEqual(self.poll(late), [("clock", 3)], "a line that was being written when the reader started is history")

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
        self.assertEqual(got, [("clock", n) for n in range(1, 81)], "every line exactly once, in order")
        self.assertEqual(int(r.dropped), 0, "a rotate line is not a dropped record")
        newest = int(self.C.newestGen(base))
        self.assertGreater(newest, 3)
        self.assertEqual(newest, int(w.gen))
        self.assertFalse(os.path.exists(base), "older generations are deleted")
        self.assertTrue(os.path.exists(self.C.genPath(base, newest - 2)), "the newest three are kept")
        # the rotate lines name the next generation (the mod's writers write no header: skip 0)
        ended = T.read_text(self.C.genPath(base, newest - 1)).splitlines()[-1].split(TAB)
        self.assertEqual((ended[0], ended[2]), ("rotate", '{["gen"]=%d,["skip"]=0}' % newest))
        # a writer made again (a reload) goes on in the newest generation
        w2 = self.C.newWriter(base, 1, self.L.eval("{ rotate = 200, keep = 3 }"))
        self.assertEqual(int(w2.gen), newest)
        # a reader made now: from the oldest file still there, or the newest from its top, or at the end
        oldest = self.C.newReader(base)
        self.assertEqual(int(oldest.gen), newest - 2)
        top = self.C.newReader(base, self.L.eval('{ start = "newest" }'))
        first_of_newest = int(self.C.parse(T.read_text(self.C.genPath(base, newest)).splitlines()[0].split(TAB)[2]).n)
        self.assertEqual(self.poll(top)[0], ("clock", first_of_newest))
        at_end = self.C.newReader(base, self.L.eval('{ start = "end" }'))
        self.assertEqual(self.poll(at_end), [])
        self.send(w2, "clock", 81)
        self.assertEqual(self.poll(at_end), [("clock", 81)])
        # a second writer object of the same log on the same thread (the script loaded twice into one state) follows a rotation
        # the other made, so no line lands after a rotate line where no reader looks
        rest = []
        for n in range(82, 120):
            self.send(w if n % 2 else w2, "clock", n)
            rest += self.poll(at_end)
        self.assertEqual(rest, [("clock", n) for n in range(82, 120)])

    def test_a_reader_opened_right_after_a_rotate_line_starts_in_the_next_generation(self):
        base = self.path("ui_in.log")
        w = self.C.newWriter(base, 1, self.L.eval("{ rotate = 1 }"))
        self.send(w, "result", 1)
        self.assertTrue(self.C.writerRotate(w))
        r = self.C.newReader(base, self.L.eval('{ start = "end" }'))
        self.assertEqual(int(r.gen), 2)
        self.send(w, "result", 2)
        self.assertEqual(self.poll(r), [("result", 2)])

    def test_a_reader_that_follows_a_rotation_skips_the_header_it_has_read(self):
        # the native module repeats `ready` at the top of each new generation and says how many bytes that is (`skip`)
        base = self.path("native_events.log")
        ready = 'ready\tnative\t{["n"]=0}\n'
        with open(base, "w", encoding="utf-8", newline="") as f:
            f.write(ready + 'pong\tnative\t{["n"]=1}\n')
        r = self.C.newReader(base, self.L.eval('{ start = "newest", keep = true }'))
        self.assertEqual(self.poll(r), [("ready", 0), ("pong", 1)])
        with open(self.C.genPath(base, 2), "w", encoding="utf-8", newline="") as f:
            f.write(ready + 'pong\tnative\t{["n"]=2}\n')
        with open(base, "a", encoding="utf-8", newline="") as f:
            f.write('rotate\tnative\t{["gen"]=2,["skip"]=%d}\n' % len(ready))
        self.assertEqual(self.poll(r), [("pong", 2)], "the copy of ready is not read twice")
        fresh = self.C.newReader(base, self.L.eval('{ start = "newest" }'))
        self.assertEqual(self.poll(fresh)[0], ("ready", 0), "a reader that starts in the new generation reads it")

    def test_a_kept_reader_opens_the_file_once_and_sees_a_truncate_and_regrow_between_two_polls(self):
        base = self.path("native_events.log")
        with open(base, "w", encoding="utf-8", newline="") as f:
            f.write('ready\tnative\t{["n"]=1}\npong\tnative\t{["n"]=2}\n')
        r = self.C.newReader(base, self.L.eval('{ start = "newest", keep = true }'))
        self.L.execute("OPENS_RESET()")
        self.assertEqual(self.poll(r), [("ready", 1), ("pong", 2)])
        for _ in range(30):
            self.assertEqual(self.poll(r), [])
        with open(base, "a", encoding="utf-8", newline="") as f:
            f.write('held\tnative\t{["n"]=3}\n')
        self.assertEqual(self.poll(r), [("held", 3)])
        self.assertEqual(self.opens("native_events.log", "rb"), 1, "one open for 33 polls")
        # truncated and written again past the old length between two polls: another file, read from its top
        with open(base, "w", encoding="utf-8", newline="") as f:
            f.write('ready\tnative\t{["n"]=4,["build"]="again"}\npong\tnative\t{["n"]=5}\nheld\tnative\t{["n"]=6}\npong\tnative\t{["n"]=7}\n')
        self.assertEqual(self.poll(r), [("ready", 4), ("pong", 5), ("held", 6), ("pong", 7)])

    def test_mod_log_is_capped_across_both_halves(self):
        os.environ["FEVERLINK3_SESSION_DIR"] = self.tmp
        try:
            halves = [T.load_common(self.L) for _ in range(2)]   # two Lua states' copies of the module
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


class WindowAfterReloadTest(unittest.TestCase):
    """Bug 3 of the friend test (2026-10-16) end to end: two games run the real mod; the window's last record is 5000 bytes, the game is
    reloaded for a resync, and the window's commands after the reload still reach the other game."""

    setUp = T.LockstepTest.setUp
    tearDown = T.LockstepTest.tearDown
    tick = T.LockstepTest.tick
    start = T.LockstepTest.start

    def test_a_5000_byte_window_record_and_a_reload_do_not_lose_the_window_commands_after_it(self):
        self.start()
        self.tick(10)
        # the window's last record before the reload: 5000 bytes (a record of an earlier session, which the bridge ignores)
        out = os.path.join(self.B.dir, "ui_out.log")
        with open(out, "a", encoding="utf-8", newline="") as f:
            f.write('cmd\t0\t{["body"]="%s",["id"]=41,["sid"]="earlier"}\n' % ("x" * 5000))
        self.assertTrue(self.B.ui_send("before the reload"))
        self.tick(4)
        self.assertEqual(self.relay.chats, [(1, "before the reload")])
        # the resync reload: the bridge's halves and the window state start again (a new window writer)
        self.relay.send(self.B, "resync_load", 0, self.relay.ser({"id": 1, "name": "hostsave", "sha256": "00"}))
        self.B.frame()
        self.B.reload()
        self.B.L.execute("UI_OUTBOX = nil; UI_HOOK = nil")
        self.relay.session()
        self.tick(6)
        self.assertIn("reloaded after a resync", self.B.log())
        # window commands after the reload: a chat line and a purchase from the window (UI hook) reach the host and the other game
        self.assertTrue(self.B.ui_send("after the reload"))
        self.B.window('api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function(d, s, e) WINDOW_CB = { s } end)')
        self.tick(40)
        self.assertEqual(self.relay.chats, [(1, "before the reload"), (1, "after the reload")])
        bought = lambda g: len([a for a in g.applied() if a.startswith("makeVehicleBuyCmd@")])
        self.assertEqual((bought(self.A), bought(self.B)), (1, 1), "the purchase after the reload landed on both games")
        self.assertTrue(self.B.L.eval("WINDOW_CB ~= nil and WINDOW_CB[1] == true"), "the window's callback ran with success")
        self.assertNotIn("had no answer from the bridge", self.B.log())


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
            gui = [r for p in slot_files(g.dir, "gui") for r in records(p)]
            sim = [r for p in slot_files(g.dir, "sim") for r in records(p)]
            gui_kinds, sim_kinds = {k for k, _ in gui}, {k for k, _ in sim}
            self.assertIn("hold_on", gui_kinds, g.label)
            self.assertIn("inject_next", sim_kinds, "%s: the stamp apply's order is the simulation half's" % g.label)
            self.assertFalse(sim_kinds & {"hold_on", "ping", "defer"}, "%s: the interface's lines are not in the simulation's file" % g.label)
            heard = [k for _, _, k, _ in g.native.heard]
            self.assertEqual(sorted(heard), sorted(k for k, _ in gui + sim), "%s: the module heard every line, none dropped" % g.label)
            # every line names the Lua state and the load of the script that wrote it (the `from` column)
            for p in slot_files(g.dir, "gui") + slot_files(g.dir, "sim"):
                froms = {line.split(TAB)[1] for line in T.read_text(p).splitlines()}
                self.assertEqual(len(froms), 1, p)
                self.assertRegex(froms.pop(), r"^\d+@.+/[0-9a-f]{8}/[0-9a-f]{8}/.+$")
        self.assertIn("defer", {k for p in slot_files(self.B.dir, "gui") for k, _ in records(p)}, "the originator's verdict is the interface half's")
        self.assertRegex(self.trace(self.B), r"written to native_ctl\.sim\.w[0-9a-f]+\.log as its line 1")
        self.assertRegex(self.B.log(), r"control writer of the sim half: native_ctl\.sim\.w[0-9a-f]+\.log made by \S+ \(state=")
        self.assertIn("script load: bridge", self.B.log())

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
        ctl = [os.path.basename(p) for half in ("gui", "sim") for p in slot_files(self.B.dir, half)]
        self.assertEqual(sum(opens(n, "rb") for n in ctl), 0, "no control file is read")
        self.assertGreater(sum(opens(n, "ab") for n in ctl), 2)
        self.assertEqual(opens("native_events.log", "rb"), 0, "the events file stays open: no open in 120 frames")

    def trace(self, g):
        return T.read_text(os.path.join(g.dir, "trace.log"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
