"""The automatic update in the window: the words come from feverlink3_text.lua (T.UPDATE, T.updateLine) and match what the
module writes in ctl_status.txt (`update` of launcher/src/autoupdate.rs); the switch sends the controller's `autoupdate`
command; Host and Join are not offered while the game must restart.

Run:  python -I -m unittest discover -s tests/lua -p "test_update_ui.py"
"""
import os
import unittest

import lupa.lua52 as lupa

HERE = os.path.dirname(os.path.abspath(__file__))
CONTENT = os.path.join(HERE, "..", "..", "mod", "feverlink3", "content")


class UpdateLine(unittest.TestCase):
    def test_the_update_line_and_the_switch(self):
        rt = lupa.LuaRuntime(unpack_returned_tuples=True)
        with open(os.path.join(CONTENT, "feverlink3_text.lua"), encoding="utf-8") as f:
            T = rt.execute(f.read())
        line = lambda **u: T.updateLine(rt.table_from(u))
        self.assertEqual(("Downloading FeverLink3 0.2.1.", "info"), line(state="downloading", version="0.2.1"))
        self.assertEqual(("Updated to 0.2.1. Restart the game to finish.", "info"),
                         line(state="ready", version="0.2.1", current="0.2.0"))
        self.assertEqual(("Update to 0.2.1 failed: GitHub could not be reached. You keep 0.2.0.", "warn"),
                         line(state="failed", version="0.2.1", current="0.2.0", reason="GitHub could not be reached"))
        text, level = line(state="failed", version="0.2.1", current="0.2.0", reason="x", restart=True)
        self.assertTrue(text.endswith("Restart the game before you play together.") and level == "warn", text)
        self.assertEqual(("Your FeverLink3 is older than the host's (0.2.8). Checking for an update now...", "info"),
                         line(state="checking", version="0.2.8"))
        self.assertEqual(("Your FeverLink3 is older than the host's (0.2.8). Download the new version from GitHub or Nexus.",
                          "warn"), line(state="behind", version="0.2.8"))
        for quiet in (line(state=""), line(state="other"), T.updateLine(None)):
            self.assertIsNone(quiet)
        self.assertEqual("Automatic updates: on", T.UPDATE.on)
        self.assertEqual("Automatic updates: off", T.UPDATE.off)

        # the window and the pop-up draw long texts as lines broken between words (TextView itself does not wrap)
        with open(os.path.join(CONTENT, "feverlink3_hud.lua"), encoding="utf-8") as f:
            Hud = rt.execute(f.read())
        wrap = lambda s, w: list(Hud.wrapLines(s, w).values())
        failed, _ = line(state="failed", version="0.2.1", current="0.2.0", reason="it did not start properly", restart=True)
        lines = wrap(failed, 34)
        self.assertEqual(failed, " ".join(lines))
        self.assertTrue(len(lines) > 1 and all(len(x) <= 34 for x in lines), lines)
        self.assertEqual(["  ab cd", "  ef"], wrap("  ab cd ef", 8), "the indent stays on every line")
        self.assertEqual(["abcdefgh", "ij", "x"], wrap("abcdefghij\nx", 8), "a long word is cut, a newline breaks")
        self.assertEqual([""], wrap("", 40))

        with open(os.path.join(CONTENT, "feverlink3_ui.script.lua"), encoding="utf-8") as f:
            ui = f.read()
        self.assertEqual(1, ui.count('command("autoupdate"'), "one switch")
        self.assertIn('command("autoupdate", { on = not on })', ui)
        idle = ui[ui.index('add(heading("Play together"))'):ui.index('elseif state == "joining" then')]
        self.assertLess(idle.index("if mustRestart(ctl) then"), idle.index('button("Host a game"'))

        # one margin for every text: the window and the pop-up draw their rows in the one padded column
        self.assertIn("content = fixedColumn(WIDTH, rows)", ui)
        self.assertIn("return box(V, { fixedColumn(POP_W, rows) })", ui)
        self.assertEqual(1, ui.count("s.padding = "), "the padding is set in one place")

    def test_save_and_quit(self):
        """Offered only for a staged update outside a session; saves as feverlink3-restart (not the player's default name),
        quits only from the save's callback and only once the save is found and read back (review 2026-10-31):
        only a save new or changed since the click counts (an older feverlink3-restart, here in two folders, is no proof),
        and a file the game reports as broken (errorMsg) is unreadable; a refused call or a missing save says why and leaves
        the game open."""
        rt = lupa.LuaRuntime(unpack_returned_tuples=True)
        with open(os.path.join(CONTENT, "feverlink3_hud.lua"), encoding="utf-8") as f:
            Hud = rt.execute(f.read())
        offer = lambda state, u: Hud.offerSaveAndQuit(rt.table_from({"state": state, "update": rt.table_from(u)}))
        self.assertTrue(offer("idle", {"state": "ready"}) and offer("failed", {"state": "ready"}))
        for state, u in (("hosting", {"state": "ready"}), ("joined", {"state": "ready"}), ("idle", {"state": "updated"})):
            self.assertFalse(offer(state, u), (state, u))
        rt.execute("api = { type = { SavegameId = { new = function() return {} end } } }")
        run = rt.eval('''function(Hud, refuse)
            local calls, failed, logs = {}, nil, {}
            -- an older save of the name from an earlier update in this folder ("p", unless "first") and one in another folder
            local saved = { { saveName = "feverlink3-restart", path = "q", timestamp = 50, size = 7 } }
            if refuse ~= "first" then saved[2] = { saveName = "feverlink3-restart", path = "p", timestamp = 100, size = 7 } end
            local app = {
                saveGame = function(name, cb, isMapEditor, skipSetName)
                    if refuse == "save" then error("no world") end
                    calls[#calls + 1] = "save " .. name .. " " .. tostring(isMapEditor) .. " " .. tostring(skipSetName)
                    calls.cb = cb
                    -- "written": the save failed and left the older file as it was; the same size, only the time tells
                    if refuse ~= "written" then saved[2] = { saveName = name, path = "p", timestamp = 200, size = 7 } end
                end,
                SaveGameNamespace = { getSavegame = function() return 1 end },
                findAllSavegames = function() if refuse == "list" then error("no list") end return saved end,
                getSavegameInfo = function(id)
                    assert(id.path == "p", "the save written since the click is the one read")
                    local msg = refuse == "broken" and "bad file" or ""
                    return { get = function() return { errorMsg = msg, info = { metadata = { gameModDescs = {} } } } end }
                end,
                quit = function(instant)
                    if refuse == "quit" then error("no") end
                    calls[#calls + 1] = "quit " .. tostring(instant)
                end,
            }
            local started = Hud.saveAndQuit(app, function(l) logs[#logs + 1] = l end, function(r) failed = r end)
            local before = #calls
            if calls.cb then calls.cb() end
            return started, before, table.concat(calls, ","), failed, table.concat(logs, "|")
        end''')
        quit = "save feverlink3-restart false true,quit false"
        started, n, calls, failed, logs = run(Hud, None)
        self.assertEqual((True, 1, quit, None), (started, n, calls, failed))
        self.assertIn("(saves of that name before: 100/7, 50/7; now: 200/7, 50/7), quitting", logs, "both stamps, no path")
        self.assertEqual((True, 1, quit, None), run(Hud, "first")[:4], "the first save of the name")
        self.assertEqual((False, 0, "", "the world could not be saved"), run(Hud, "save")[:4])
        self.assertEqual((False, 0, "", "the world could not be saved"), run(Hud, "list")[:4], "no listing: no proof, no save")
        self.assertEqual((True, 1, "save feverlink3-restart false true", "the game did not close"), run(Hud, "quit")[:4])
        for refuse, why in (("written", "no savegame 'feverlink3-restart' written since the click"),
                            ("broken", "savegame 'feverlink3-restart' cannot be read: bad file")):
            started, n, calls, failed, logs = run(Hud, refuse)
            self.assertEqual((True, 1, "save feverlink3-restart false true", "the world could not be saved"),
                             (started, n, calls, failed), refuse)
            self.assertIn(why, logs)


if __name__ == "__main__":
    unittest.main()
