"""The control link between the Lua mod and the native module (docs/NATIVE_LINK.md section 3), from the mod's side.

The module reads the control files (`native_ctl.<half>.w<id>.log`, one per writer) and writes `native_events.log`, in the shape of the
launcher link: `kind<TAB>from<TAB>payload`, no line number, read by position. The mod reads
and writes them with the helpers it already has (`C.newWriter` / `C.writerSend` and `C.newReader` / `C.readerFeed`
/ `C.parse`), so these tests only pin the text on both sides to the same golden lines as the Rust tests
(`native/src/link.rs`: `lines_written_by_the_mods_own_writer_parse`, `golden_lines_shared_with_the_lua_test`). If
either side changes its format, one of the two test suites fails.

Run:  python -I -m unittest discover -s tests/lua -p "test_*.py"
"""
import os
import shutil
import tempfile
import unittest

import lupa.lua52 as lupa

import test_lockstep as T

HERE = os.path.dirname(os.path.abspath(__file__))
CONTENT = os.path.join(HERE, "..", "..", "mod", "feverlink3", "content")
TAB = "\t"


def read_src(name):
    with open(os.path.join(CONTENT, name), encoding="utf-8") as f:
        return f.read()


COMMON_SRC = read_src("feverlink3_common.lua")

# What C.writerSend writes for the control messages, one writer, player id 1 (the Rust test parses these bytes).
CTL_FILE = (
    'hold_on\t1\t{["kinds"]={[1]="build_proposal"}}\n'
    'ping\t1\t{["n"]=3}\n'
    'release\t1\t{["at"]=12400,["id"]=3}\n'
    'cancel\t1\t{["id"]=7}\n'
    'hold_off\t1\t{}\n'
    'terrain_apply\t1\t{["id"]=2,["stamp"]=400}\n'
)

# What the module writes (EventWriter::line in Rust). The payload texts are the Rust golden strings.
EVENTS_FILE = (
    'ready\tnative\t{["build"]="40420",["clock"]=false,["hooks"]={[1]="apply_command",[2]="command_list_add"},'
    '["kinds"]={["build_proposal"]=52},["poolpatch"]=true,["sites"]=12,["version"]="0.1.0"}\n'
    'holding\tnative\t{["kinds"]={[1]="build_proposal"},["on"]=true,["reason"]="hold_on"}\n'
    'audit\tnative\t{["adds"]={["other"]=5,["tool"]=2,["unknown"]=0},["counts"]={[17]=4,[52]=2},'
    '["sites"]={[0]=2},["total"]=6,["unreadable"]=0}\n'
    'error\tnative\t{["detail"]="no held command 9",["where"]="release"}\n'
    'held\tnative\t{["id"]=3,["kind"]=52,["ms"]=120,["site"]=5060975}\n'
    'released\tnative\t{["id"]=3,["ms"]=99,["reason"]="silence"}\n'
    'pong\tnative\t{["n"]=4}\n'
)


class NativeLinkTest(unittest.TestCase):
    def setUp(self):
        os.environ.pop("FEVERLINK3_SESSION_DIR", None)
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.C = T.load_common(self.L)
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-native-link-")
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)

    def table(self, text):
        return self.L.eval(text)

    def test_the_mods_writer_produces_the_control_lines_the_module_parses(self):
        path = os.path.join(self.tmp, "native_ctl.log")
        w = self.C.newWriter(path, 1)
        send = lambda kind, payload: self.assertTrue(self.C.writerSend(w, kind, self.table(payload)))
        send("hold_on", '{ kinds = { "build_proposal" } }')
        send("ping", "{ n = 3 }")
        send("release", "{ id = 3, at = 12400 }")
        send("cancel", "{ id = 7 }")
        send("hold_off", "{}")
        send("terrain_apply", "{ id = 2, stamp = 400 }")
        with open(path, encoding="utf-8", newline="") as f:
            self.assertEqual(f.read(), CTL_FILE)

    def test_a_restarted_writer_goes_on_at_the_end_of_the_file(self):
        # a savegame reload restarts the script: its new writer appends after what the file holds, and the module reads by position
        path = os.path.join(self.tmp, "native_ctl.log")
        w = self.C.newWriter(path, 1)
        self.C.writerSend(w, "ping", self.table("{ n = 1 }"))
        w2 = self.C.newWriter(path, 1)
        self.C.writerSend(w2, "ping", self.table("{ n = 2 }"))
        with open(path, encoding="utf-8") as f:
            self.assertEqual(f.read(), 'ping\t1\t{["n"]=1}\nping\t1\t{["n"]=2}\n')

    def test_the_mod_reads_every_event_the_module_writes(self):
        r = self.C.newReader("unused")
        recs = self.C.readerFeed(r, EVENTS_FILE)
        records = list(recs.values())
        self.assertEqual([x.kind for x in records], ["ready", "holding", "audit", "error", "held", "released", "pong"])
        self.assertTrue(all(x["from"] == "native" for x in records))
        parsed = {x.kind: self.C.parse(x.payload) for x in records}

        ready = parsed["ready"]
        self.assertEqual(ready["build"], "40420")
        self.assertIs(ready["clock"], False)
        self.assertIs(ready["poolpatch"], True)
        self.assertEqual([ready["hooks"][1], ready["hooks"][2]], ["apply_command", "command_list_add"])
        self.assertEqual(ready["kinds"]["build_proposal"], 52)
        self.assertEqual(ready["sites"], 12)

        self.assertIs(parsed["holding"]["on"], True)
        self.assertEqual(parsed["holding"]["kinds"][1], "build_proposal")
        self.assertEqual(parsed["holding"]["reason"], "hold_on")

        audit = parsed["audit"]
        self.assertEqual((audit["counts"][17], audit["counts"][52], audit["total"]), (4, 2, 6))
        self.assertEqual((audit["adds"]["tool"], audit["adds"]["other"], audit["adds"]["unknown"]), (2, 5, 0))
        self.assertEqual(audit["sites"][0], 2)

        self.assertEqual(parsed["error"]["where"], "release")
        self.assertEqual(parsed["error"]["detail"], "no held command 9")
        self.assertEqual((parsed["held"]["id"], parsed["held"]["kind"], parsed["held"]["site"]), (3, 52, 5060975))
        self.assertEqual((parsed["released"]["id"], parsed["released"]["reason"]), (3, "silence"))
        self.assertEqual(parsed["pong"]["n"], 4)

    def test_the_mod_reads_the_events_file_as_it_grows_and_after_a_truncation(self):
        path = os.path.join(self.tmp, "native_events.log")
        r = self.C.newReader(path)
        lines = EVENTS_FILE.splitlines(keepends=True)
        with open(path, "w", encoding="utf-8", newline="") as f:
            f.write("".join(lines[:2]))
        self.assertEqual([x.kind for x in self.C.readerPoll(r).values()], ["ready", "holding"])
        with open(path, "a", encoding="utf-8", newline="") as f:
            f.write("".join(lines[2:4]))
        self.assertEqual([x.kind for x in self.C.readerPoll(r).values()], ["audit", "error"])
        # rewritten from the top (shorter than what was read: another file, the folder was cleaned): read from its first byte
        with open(path, "w", encoding="utf-8", newline="") as f:
            f.write(lines[0])
        self.assertEqual([x.kind for x in self.C.readerPoll(r).values()], ["ready"])
        with open(path, "a", encoding="utf-8", newline="") as f:
            f.write("".join(lines[1:]))
        self.assertEqual([x.kind for x in self.C.readerPoll(r).values()], ["holding", "audit", "error", "held", "released", "pong"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
