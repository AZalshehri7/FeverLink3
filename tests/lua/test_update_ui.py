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


if __name__ == "__main__":
    unittest.main()
