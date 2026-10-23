""""Report a problem" in the window: the words come from feverlink3_text.lua (plain, no internal terms), and the UI only sends the
controller's `report` command with the note (launcher/src/live/report.rs does the work), from one function used in two places.

Run:  python -I -m unittest discover -s tests/lua -p "test_report_ui.py"
"""
import os
import re
import unittest

import lupa.lua52 as lupa

HERE = os.path.dirname(os.path.abspath(__file__))
CONTENT = os.path.join(HERE, "..", "..", "mod", "feverlink3", "content")


class ReportButton(unittest.TestCase):
    def test_the_words_are_plain_and_the_window_sends_only_the_report_command_from_one_function(self):
        rt = lupa.LuaRuntime(unpack_returned_tuples=True)
        with open(os.path.join(CONTENT, "feverlink3_text.lua"), encoding="utf-8") as f:
            T = rt.execute(f.read())
        report = T.REPORT
        for key in ("button", "note", "explain", "contents"):
            text = report[key]
            self.assertTrue(isinstance(text, str) and text, key)
            for word in ("checksum", "lane", "resync", "engine", "salt", "bundle"):
                self.assertNotIn(word, text.lower(), "%s says %r" % (key, word))
        self.assertEqual("Report a problem", report.button)
        self.assertIn("Nothing is sent anywhere by itself", report.explain)

        with open(os.path.join(CONTENT, "feverlink3_ui.script.lua"), encoding="utf-8") as f:
            ui = f.read()
        self.assertEqual(1, len(re.findall(r'command\("report"', ui)), "one place sends it")
        self.assertIn('command("report", withMods({ note = U.reportNote or "" }))', ui)
        # the mods and build reach the controller without a click too (a Steam-invite join has no window command)
        self.assertEqual(1, len(re.findall(r'command\("mods"', ui)))
        self.assertIn("pcall(shareMods)", ui)
        # the refresh closure must see the local: declared before the call (two windows 2026-10-23: it called a global nil)
        self.assertLess(ui.index("local shareMods"), ui.index("pcall(shareMods)"))
        # the pop-up (only while the bar icon is red), the Troubleshooting section and the Players tab (host and guest, any
        # icon colour) call the same function
        calls = re.findall(r"(?<!function )reportRows\(add, ctl\)", ui)
        self.assertEqual(2, len(calls))
        self.assertIn('if Hud.iconState(ctl, m, now) == "red" then add(gap()); reportRows(add, ctl) end', ui)
        players_tab = ui[ui.index("local function playersTab(host)"):ui.index("local function chatTab()")]
        self.assertIn("reportRows(function(node) list[#list + 1] = node end, ctl)", players_tab)


if __name__ == "__main__":
    unittest.main()
