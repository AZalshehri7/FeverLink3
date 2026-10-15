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

    def test_every_mod_file_keeps_room_below_the_games_limit_of_200_locals(self):
        # Lua allows 200 active locals per function, and a file's main chunk is one function. The game's Lua refused the bridge with "too
        # many local variables (limit is 200) in main function" when six top-level locals were added (real run rc1, 2026-10-14): 191 names
        # loaded, 197 did not although lupa compiled them (the game counts a few more of its own), and every entry point of the bridge then
        # failed. The bridge was cut down to sections in do ... end blocks (SECTIONS in the file); these caps keep real headroom: a file
        # that reaches its cap needs a section (or a table such as the bridge's X) for its new names, not a higher cap.
        caps = {"feverlink3_bridge.script.lua": 120, "feverlink3_cmds_build.lua": 145}
        content = os.path.join(MOD, "content")
        names = sorted(n for n in os.listdir(content) if n.endswith(".lua"))
        self.assertIn("feverlink3_bridge.script.lua", names)
        for name in names:
            with open(os.path.join(content, name), encoding="utf-8") as f:
                n = peak_locals(f.read(), name)
            self.assertLessEqual(n, caps.get(name, 100), "%s: %d active locals at its peak (game limit 200, minus a few)" % (name, n))


def peak_locals(text, name):
    """The most locals active at once at any column-0 statement of the file (its main chunk and its top-level functions), as Lua 5.2's
    compiler counts them: a probe `do local p0_, ... end` is put before every such line and the largest probe that still compiles is
    200 - peak. A file that does not compile at all fails here too."""
    import re
    import lupa.lua52 as lupa
    load = lupa.LuaRuntime().eval("function(s) local f, e = load(s, '=probe'); return f ~= nil, e end")
    ok, err = load(text)
    if not ok:
        raise AssertionError("%s does not compile: %s" % (name, err))
    lines = text.splitlines(True)
    at = {i for i, l in enumerate(lines) if re.match(r"(local|function|do)\b|end -- section", l)}

    def fits(k):
        probe = "do local " + ", ".join("p%d_" % j for j in range(k)) + " end\n"
        ok, err = load("".join(lines[i] if i not in at else probe + lines[i] for i in range(len(lines))))
        if not ok and "local variables" not in str(err):
            raise AssertionError("%s: probe did not compile: %s" % (name, err))
        return ok

    lo, hi = 0, 200
    while lo < hi:
        m = (lo + hi + 1) // 2
        if fits(m):
            lo = m
        else:
            hi = m - 1
    return 200 - lo


if __name__ == "__main__":
    unittest.main()
