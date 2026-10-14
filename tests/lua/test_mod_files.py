"""The game loads only the files a mod lists in _content.json. A file that is in content/ but not listed there is "not found"
by ug_require (seen in a real game on 2026-10-10: the UI hook never loaded for that reason, silently)."""
import json
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
MOD = os.path.join(HERE, "..", "..", "mod", "feverlink3_1")


class ContentIndexTest(unittest.TestCase):
    def test_every_file_in_content_is_listed_in_the_games_index(self):
        listed = set(json.load(open(os.path.join(MOD, "_content.json"), encoding="utf-8"))["files"])
        present = set(os.listdir(os.path.join(MOD, "content")))
        self.assertEqual(set(), present - listed, "files the game would not find")
        self.assertEqual(set(), listed - present, "files listed but missing")

    def test_modules_that_carry_values_never_format_with_percent_d(self):
        # The game's Lua raises on "%d" beyond 32 bits (a balance of 5 billion, sweep 2026-10-13): numbers that may be money or another
        # large value go through C.integerText (feverlink3_common.lua), never string.format("%d").
        import re
        for name in ("feverlink3_common.lua", "feverlink3_cmds_company.lua", "feverlink3_lockstep.lua"):
            text = open(os.path.join(MOD, "content", name), encoding="utf-8").read()
            bad = [l.strip() for l in text.splitlines() if re.search(r'format\(\s*"[^"]*%[-+ #0-9.]*[di]', l)]
            # common.lua keeps one two-digit escape ("%03d" of a byte) and one lane key ("%02d" of a counter): both are small by construction
            bad = [l for l in bad if not re.search(r'"%0[23]d"', l)]
            self.assertEqual([], bad, name)
        # the two trace modules count things with "%d" (small by construction) but prints its sampled values through the helper
        for name in ("feverlink3_towntrace.lua", "feverlink3_livevsloaded.lua"):
            self.assertNotIn('format("%d", v)', open(os.path.join(MOD, "content", name), encoding="utf-8").read(), name)

    def test_the_bridge_keeps_room_below_the_games_limit_of_200_locals(self):
        # The game's Lua refused the bridge with "too many local variables (limit is 200) in main function" when six top-level locals were
        # added (real run rc1, 2026-10-14; lupa compiled it): 191 declared names loaded, 197 did not (the game counts some more of its own).
        # New module-level state goes into the X table instead.
        import re
        n = 0
        with open(os.path.join(MOD, "content", "feverlink3_bridge.script.lua"), encoding="utf-8") as f:
            for line in f.read().splitlines():
                if re.match(r"^local\s+function\s+\w+", line):
                    n += 1
                    continue
                m = re.match(r"^local\s+([\w\s,]+?)(=|$|--)", line)
                if m:
                    n += len([x for x in m.group(1).split(",") if x.strip()])
        self.assertLessEqual(n, 191, "top-level local names of the bridge")


if __name__ == "__main__":
    unittest.main()
