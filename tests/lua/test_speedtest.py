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
CONTENT = os.path.join(HERE, "..", "..", "mod", "feverlink3", "content")


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

    def test_the_speed_test_lines_size_mode_and_command(self):
        self.assertEqual([], self.lines(None))
        self.assertEqual([], self.lines("{}"))
        # progress shows the status line
        self.assertEqual(
            ["Speed test, sending to your friend: 4.2 of 32 MB, 1.50 MB/s"],
            self.lines('{running=true, status="Speed test, sending to your friend: 4.2 of 32 MB, 1.50 MB/s", lines={}}'),
        )
        # a failure shows the reason
        self.assertEqual(
            ["Speed test failed: no data for 60 s"],
            self.lines('{running=false, status="Speed test failed: no data for 60 s", lines={}}'),
        )
        # a result shows all its lines not the status
        speed = '{running=false, status="a", lines={"a","b","c","d"}}'
        self.assertEqual(["a", "b", "c", "d"], self.lines(speed))
        # the size is one of the offered choices
        choice = self.C.speedChoice
        self.assertEqual([8, 32, 80], [self.C.SPEED_CHOICES[i] for i in (1, 2, 3)])
        self.assertEqual(80, choice(80))
        self.assertEqual(8, choice(8))
        self.assertEqual(32, choice(32))
        for odd in (None, 0, 5, "huge", 1e9):
            self.assertEqual(32, choice(odd), odd)
        # the command carries the id and the size
        params = self.C.speedParams("76561190000000001", 80)
        self.assertEqual("76561190000000001", params["id"])
        self.assertEqual(80, params["mb"])
        self.assertEqual(32, self.C.speedParams("1", 7)["mb"])
        # the mode is one of the offered words
        mode = self.C.speedMode
        self.assertEqual(
            ["reliable", "dgram", "both", "parallel", "auto"], [self.C.SPEED_MODES[i] for i in (1, 2, 3, 4, 5)]
        )
        self.assertEqual("Auto (scaled)", self.C.SPEED_MODE_LABELS["auto"])
        for word in ("reliable", "dgram", "both", "parallel", "auto"):
            self.assertEqual(word, mode(word))
        for odd in (None, "", "all", "DGRAM", 5):
            self.assertEqual("reliable", mode(odd), odd)
        params = self.C.speedParams("76561190000000001", 8, "dgram")
        self.assertEqual(("76561190000000001", 8, "dgram"), (params["id"], params["mb"], params["mode"]))
        self.assertEqual("reliable", self.C.speedParams("1", 8)["mode"])

    def test_the_window_script_compiles_and_says_how_a_transfer_goes_and_who_wants_to_join(self):
        check = self.lua.eval("function(src) local f, err = load(src, 'ui', 't'); return f ~= nil, err end")
        ok, err = check(read("feverlink3_ui.script.lua"))
        self.assertTrue(ok, err)
        # the window says how a transfer over several connections is going
        import re

        src = read("feverlink3_ui.script.lua")
        found = re.search(r"local function syncText\(sync, host\).*?\nend\n", src, re.S)
        self.assertIsNotNone(found, "syncText is in the window script")
        sync_text = self.lua.eval("function(src) return load(src .. ' return syncText', 'sync', 't')() end")(found.group(0))
        # a chunk transfer: the percentage only
        self.assertEqual(
            "Syncing: Receiving the host's world: 63%",
            sync_text(self.lua.table_from({"phase": "sending", "done": 63, "total": 100}), False),
        )
        # over parallel connections: the note follows
        line = sync_text(
            self.lua.table_from(
                {"phase": "sending", "done": 63, "total": 100, "note": "7 connections, 6.9 MB/s"}
            ),
            False,
        )
        self.assertEqual("Syncing: Receiving the host's world: 63% (7 connections, 6.9 MB/s)", line)
        line = sync_text(
            self.lua.table_from({"phase": "sending", "done": 5, "total": 100, "note": "1 connection, 1.0 MB/s"}), True
        )
        self.assertEqual("Syncing: Sending your world to the players: 5% (1 connection, 1.0 MB/s)", line)
        # no note, an empty one, a number: nothing added
        for odd in (None, "", 5):
            sync = {"phase": "sending", "done": 1, "total": 2}
            if odd is not None:
                sync["note"] = odd
            self.assertEqual(
                "Syncing: Receiving the host's world: 50%", sync_text(self.lua.table_from(sync), False), odd
            )
        # the window names join requests and counts what waits for an answer
        import re

        src = read("feverlink3_ui.script.lua")
        found = re.search(r"local function requestLabel\(r\).*?\nend\n.*?local function askCount\(ctl\).*?\nend\n", src, re.S)
        self.assertIsNotNone(found, "requestLabel and askCount are in the window script")
        fns = self.lua.eval("function(src) return load(src .. ' return requestLabel, askCount', 'inv', 't')() end")
        label, count = fns(found.group(0))
        t = self.lua.table_from
        # Steam's name for the account first; the name its game sent only when it differs
        self.assertEqual("Ben (Benny) wants to join", label(t({"id": "1", "name": "Benny", "steam": "Ben"})))
        self.assertEqual("Ben wants to join", label(t({"id": "1", "name": "Ben", "steam": "Ben"})))
        self.assertEqual("Benny wants to join", label(t({"id": "1", "name": "Benny"})))
        self.assertEqual("A Steam friend wants to join", label(t({"id": "1", "name": ""})))
        reqs = t({"requests": t({1: t({"id": "1"}), 2: t({"id": "2"})})})
        self.assertEqual(3, count(t({"invites": reqs, "invite": t({"host": "Ann"})})))
        self.assertEqual(0, count(t({"state": "idle"})))
        self.assertEqual(0, count(None))


if __name__ == "__main__":
    unittest.main()
