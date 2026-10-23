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
        for quiet in (line(state=""), line(state="checking"), T.updateLine(None)):
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
        quits only from the save's callback; a refused call says why and leaves the game open."""
        rt = lupa.LuaRuntime(unpack_returned_tuples=True)
        with open(os.path.join(CONTENT, "feverlink3_hud.lua"), encoding="utf-8") as f:
            Hud = rt.execute(f.read())
        offer = lambda state, u: Hud.offerSaveAndQuit(rt.table_from({"state": state, "update": rt.table_from(u)}))
        self.assertTrue(offer("idle", {"state": "ready"}) and offer("failed", {"state": "ready"}))
        for state, u in (("hosting", {"state": "ready"}), ("joined", {"state": "ready"}), ("idle", {"state": "updated"})):
            self.assertFalse(offer(state, u), (state, u))
        run = rt.eval('''function(Hud, refuse)
            local calls, failed = {}, nil
            local app = {
                saveGame = function(name, cb, isMapEditor, skipSetName)
                    if refuse == "save" then error("no world") end
                    calls[#calls + 1] = "save " .. name .. " " .. tostring(isMapEditor) .. " " .. tostring(skipSetName)
                    calls.cb = cb
                end,
                quit = function(instant)
                    if refuse == "quit" then error("no") end
                    calls[#calls + 1] = "quit " .. tostring(instant)
                end,
            }
            local started = Hud.saveAndQuit(app, function() end, function(r) failed = r end)
            local before = #calls
            if calls.cb then calls.cb() end
            return started, before, table.concat(calls, ","), failed
        end''')
        self.assertEqual((True, 1, "save feverlink3-restart false true,quit false", None), run(Hud, None))
        self.assertEqual((False, 0, "", "the world could not be saved"), run(Hud, "save"))
        self.assertEqual((True, 1, "save feverlink3-restart false true", "the game did not close"), run(Hud, "quit"))


if __name__ == "__main__":
    unittest.main()
