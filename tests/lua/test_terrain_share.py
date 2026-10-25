"""Terrain height edits shared at the stamp (owner decisions 2026-10-25, native terrain.rs).

The fake native module of test_inject_fail_closed.py, extended with the terrain contract: `terrain_apply` in its `ready` hooks, the
`terrain_share` switch, the originator's `terrain` event with its cells file (native_terrain_out_<id>.txt), and at the simulation's
apply of the mod's carrier (a build command whose terrain grid has the order's header) the cells of native_terrain_in_<key>.txt written
into the mocked ground, answered `injected` (or the carrier emptied and `inject_refused`). The mocked ground answers
api.engine.terrain.getBaseHeightAt per 4 m cell.

Run:  python -I -m unittest discover -s tests/lua -p "test_terrain_share.py"
"""
import os
import struct
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import test_inject_fail_closed as I  # noqa: E402
import test_strict_build as S  # noqa: E402

TERRAIN_LUA = r'''
HEIGHTS = {}
local BASE_MAKE_API_TERRAIN = MAKE_API
function MAKE_API()
  local api = BASE_MAKE_API_TERRAIN()
  api.engine.terrain = { getBaseHeightAt = function(v) return HEIGHTS[math.floor(v.x / 4) .. "," .. math.floor(v.y / 4)] or 0 end }
  local base = api.cmd.sendCommand
  api.cmd.sendCommand = function(c, cb, progress)
    local g = nil
    pcall(function() local t = c.args[1].terrain.baseHeightMod; if t.width > 0 then g = { t.x0, t.y0, t.width, t.height } end end)
    if g and TERRAIN_HOOK and TERRAIN_HOOK(g[1], g[2], g[3], g[4]) == "emptied" then return end
    return base(c, cb, progress)
  end
  return api
end
'''


def cells_text(x0, y0, w, h, heights):
    """What terrain.rs writes: a header line, then the {height, base} float cells as hex (the fake reads only its own format)."""
    raw = b"".join(struct.pack("<ff", z, z) for z in heights)
    return "feverlink3-terrain 1 %d %d %d %d 0\n%s\n" % (x0, y0, w, h, raw.hex())


class TerrainNative(I.InjectNative):
    def __init__(self, game, clock):
        self.terrain_orders, self.shares, self.skew = [], [], 0.0
        I.InjectNative.__init__(self, game, clock)

    def open_link(self):
        if "terrain_apply" not in self.hooks:
            self.hooks.append("terrain_apply")
        super().open_link()

    def handle(self, seq, kind, p):
        if kind in ("terrain_share", "terrain_apply"):
            self.last_valid = self.clock.t
            self.heard.append((self.clock.t, seq, kind, p))
            (self.shares if kind == "terrain_share" else self.terrain_orders).append(dict(p))
        else:
            super().handle(seq, kind, p)

    def edit(self, oid, x0, y0, w, h, heights):
        """The originator: a terrain tool's command emptied at its apply, its cells written for the mod."""
        text = cells_text(x0, y0, w, h, heights)
        with open(os.path.join(self.game.dir, "native_terrain_out_%d.txt" % oid), "w", encoding="utf-8", newline="") as f:
            f.write(text)
        self.emit("terrain", {"id": oid, "x0": x0, "y0": y0, "w": w, "h": h, "bytes": len(text)})
        self.flush_events()

    def terrain_hook(self, x0, y0, w, h):
        self.read_ctl()
        o = next((o for o in self.terrain_orders if (o["x0"], o["y0"], o["w"], o["h"]) == (x0, y0, w, h)), None)
        if o is None:
            self.emit("terrain_refused", {"reason": "a script's terrain edit came without its order"})
            self.flush_events()
            return "emptied"
        self.terrain_orders.remove(o)
        key = int(o["key"])
        with open(os.path.join(self.game.dir, "native_terrain_in_%d.txt" % key), encoding="utf-8") as f:
            raw = bytes.fromhex(f.read().split("\n")[1])
        hs = self.game.L.globals().HEIGHTS
        for k in range(w * h):
            hs["%d,%d" % (x0 + k % w, y0 + k // w)] = struct.unpack_from("<f", raw, 8 * k)[0] + self.skew
        self.emit("injected", {"key": key, "nodes": 0, "segments": 0, "constructions": 0})
        self.flush_events()
        return "pass"


class TerrainGame(I.InjectGame):
    def __init__(self, label, player_id, tmp, first_entity, clock):
        S.N.NativeGame.__init__(self, label, player_id, tmp, first_entity)
        clock.games.append(self)
        self.native = TerrainNative(self, clock)
        g = self.L.globals()
        g.FAKE_T = clock.t
        g.NATIVE_HOOK = self.native.on_add
        g.INJECT_HOOK = self.native.inject_hook
        g.TERRAIN_HOOK = self.native.terrain_hook
        self.L.execute(S.STRICT_LUA)
        self.L.execute(I.INJECT_LUA)
        self.L.execute(TERRAIN_LUA)
        self.reload()


class TerrainShareTest(I.InjectTest):
    game = TerrainGame

    def heights(self, g, x0, y0, w, h):
        hs = g.L.globals().HEIGHTS
        return [hs["%d,%d" % (x0 + k % w, y0 + k // w)] for k in range(w * h)]

    def test_a_height_edit_is_applied_at_its_stamp_on_every_game_and_differing_heights_are_a_fork(self):
        heights = [1.5 + 0.25 * k for k in range(6)]

        def round_trip():
            self.start()
            self.strict_on(self.A, self.B)
            self.assertEqual((self.A.native.shares, self.B.native.shares), ([{"on": True}], [{"on": True}]), "sharing is switched on with holding")
            self.B.native.edit(1, -3, 7, 3, 2, heights)
            self.frames(2)
            self.assertEqual(self.B.lua("next(HEIGHTS) == nil"), True, "never applied at the click, the originator's own game included")
            self.tick(30)
            self.assertEqual(self.heights(self.A, -3, 7, 3, 2), heights)
            self.assertEqual(self.heights(self.B, -3, 7, 3, 2), heights, "the originator applies the same cells at the stamp")
            self.assertEqual((self.A.native.terrain_orders, self.B.native.terrain_orders), ([], []), "every order taken by its carrier")
            self.assertIn("terrain edit 1: 3x2 = 6 height cells at -3,7", self.B.log())
            self.assertEqual((self.relay.replay_failed, self.relay.verdict_resyncs), ([], []), "the proofs agree: no repair")

        def heights_differ():
            self.start()
            self.A.native.skew = 0.5   # A's ground ends up other than B's after the same cells
            self.B.native.edit(1, 0, 0, 2, 2, heights[:4])
            self.tick(30)
            self.assertNotEqual(self.heights(self.A, 0, 0, 2, 2), self.heights(self.B, 0, 0, 2, 2))
            self.assertTrue(any("differ" in r for r in self.relay.verdict_resyncs), "the differing proofs ask for the repair at once")

        self.cases([("round trip", {}, round_trip), ("heights differ", {}, heights_differ)])


if __name__ == "__main__":
    unittest.main()
