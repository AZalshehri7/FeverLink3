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


if __name__ == "__main__":
    unittest.main()
