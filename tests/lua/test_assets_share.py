"""Tree, plant and rock ticks shared at the stamp (owner decision 2026-10-26, option A; native assets.rs).

The fake native module of test_terrain_share.py, extended with the asset contract: `assets_apply` in its `ready` hooks, the originator's
`assets` event with its text (native_assets_out_<id>.txt, the module's format), and at the simulation's apply of the mod's carrier (a
Proposal that only removes) the groups of native_assets_in_<key>.txt built into the mocked world in place of the removed ones, answered
`injected`. The mocked world has asset groups (ASSET_GROUP + MODEL_INSTANCE_LIST, no CONSTRUCTION) and api.res.modelRep.getName.

Run:  python -I -m unittest discover -s tests/lua -p "test_assets_share.py"
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
import test_terrain_share as TS  # noqa: E402

ASSETS_LUA = r'''
MODEL_NAMES = { [3] = "::/assets/vegetation/trees/brleaf_05/brleaf_05.mdl", [4] = "::/assets/vegetation/rocks/rock_02.mdl" }
MODEL_ID = {}
for id, n in pairs(MODEL_NAMES) do MODEL_ID[n] = id end
function ADD_GROUP(e, models)
  local thin = {}
  for i, m in ipairs(models) do thin[i] = { modelId = MODEL_ID[m[1]], pos = { x = m[2], y = m[3], z = m[4] }, rot = 0, scale = 1 } end
  WORLD[e] = { ASSET_GROUP = {}, MODEL_INSTANCE_LIST = { thinInstances = thin, fatInstances = {} } }
end
-- the module's build at the carrier: the removed groups go, the text's groups come (new ids from 70000, per game)
NEXT_GROUP = 70000
function APPLY_TICK(rm, groups)
  for _, e in ipairs(rm) do WORLD[e] = nil end
  for _, g in ipairs(groups) do NEXT_GROUP = NEXT_GROUP + 1; ADD_GROUP(NEXT_GROUP, g) end
end
function GROUPS()
  local out = {}
  for e, w in pairs(WORLD) do
    if w.ASSET_GROUP then
      local t = {}
      for _, m in ipairs(w.MODEL_INSTANCE_LIST.thinInstances) do t[#t + 1] = MODEL_NAMES[m.modelId] .. "@" .. m.pos.x .. "," .. m.pos.y end
      table.sort(t)
      out[#out + 1] = table.concat(t, ";")
    end
  end
  table.sort(out)
  return table.concat(out, " | ")
end
local BASE_MAKE_API_ASSETS = MAKE_API
function MAKE_API()
  local api = BASE_MAKE_API_ASSETS()
  api.type.ComponentType.ASSET_GROUP, api.type.ComponentType.MODEL_INSTANCE_LIST = 41, 42
  local get = api.engine.getComponent
  api.engine.getComponent = function(e, t)
    local w = WORLD[e]
    if t == 41 then return w and w.ASSET_GROUP end
    if t == 42 then return w and COPY(w.MODEL_INSTANCE_LIST) end
    return get(e, t)
  end
  api.res = api.res or {}
  api.res.modelRep = { getName = function(id) return MODEL_NAMES[id] end }
  local base = api.cmd.sendCommand
  api.cmd.sendCommand = function(c, cb, progress)
    local rm = nil
    pcall(function() local p = c.args[1]; if TYPEOF(p) == "Proposal" and #p.toAdd == 0 and #p.toRemove > 0 then rm = p.toRemove end end)
    if rm and ASSETS_HOOK and ASSETS_HOOK(rm) == "built" then return end
    return base(c, cb, progress)
  end
  return api
end
'''


def tick_text(remove, old2new, groups):
    """What assets.rs writes for a tick: flags, removals, old2new, groups of models (name, x, y, z) as hex, its end line."""
    s = "feverlink3-assets 1\nflags 1 1 1\n"
    s += "remove %d%s\n" % (len(remove), "".join(" %d" % e for e in remove))
    s += "old2new %d%s\n" % (len(old2new), "".join(" %d:%d" % kv for kv in old2new))
    for g in groups:
        s += "group 0 00000000 3f400000 40200000 0 %d\n" % len(g)
        for name, x, y, z in g:
            transf = struct.pack("<16f", 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, x, y, z, 1)
            s += "m - %s - %s 1\n" % (name.encode().hex(), transf.hex())
    return s + "end 0\n"


def text_groups(text):
    groups = []
    for line in text.split("\n"):
        f = line.split(" ")
        if f[0] == "group":
            groups.append([])
        elif f[0] == "m":
            x, y, z = struct.unpack_from("<3f", bytes.fromhex(f[4]), 48)
            groups[-1].append([bytes.fromhex(f[2]).decode(), x, y, z])
    return groups


class AssetsNative(TS.TerrainNative):
    def __init__(self, game, clock):
        self.asset_orders = []
        TS.TerrainNative.__init__(self, game, clock)

    def open_link(self):
        if "assets_apply" not in self.hooks:
            self.hooks.append("assets_apply")
        super().open_link()

    def handle(self, seq, kind, p):
        if kind == "assets_apply":
            self.last_valid = self.clock.t
            self.heard.append((self.clock.t, seq, kind, p))
            self.asset_orders.append(dict(p))
        else:
            super().handle(seq, kind, p)

    def tick(self, oid, text):
        """The originator: an asset tool's command emptied at its apply, its tick written for the mod."""
        with open(os.path.join(self.game.dir, "native_assets_out_%d.txt" % oid), "w", encoding="utf-8", newline="") as f:
            f.write(text)
        groups = text_groups(text)
        self.emit("assets", {"id": oid, "remove": 0, "groups": len(groups), "models": sum(map(len, groups)), "bytes": len(text)})
        self.flush_events()

    def assets_hook(self, rm):
        self.read_ctl()
        rm = list(rm.values())
        o = next((o for o in self.asset_orders if [int(v) for v in (o.get("ids") or [])] == [int(v) for v in rm]), None)
        if o is None:
            return "pass"
        self.asset_orders.remove(o)
        key = int(o["key"])
        with open(os.path.join(self.game.dir, "native_assets_in_%d.txt" % key), encoding="utf-8") as f:
            groups = text_groups(f.read())
        L = self.game.L
        L.globals().APPLY_TICK(L.table_from(rm), L.table_from([L.table_from([L.table_from(m) for m in g]) for g in groups]))
        self.emit("injected", {"key": key, "nodes": 0, "segments": 0, "constructions": len(groups)})
        self.flush_events()
        return "built"


class AssetsGame(TS.TerrainGame):
    def __init__(self, label, player_id, tmp, first_entity, clock):
        S.N.NativeGame.__init__(self, label, player_id, tmp, first_entity)
        clock.games.append(self)
        self.native = AssetsNative(self, clock)
        g = self.L.globals()
        g.FAKE_T = clock.t
        g.NATIVE_HOOK = self.native.on_add
        g.INJECT_HOOK = self.native.inject_hook
        g.TERRAIN_HOOK = self.native.terrain_hook
        g.ASSETS_HOOK = self.native.assets_hook
        self.L.execute(S.STRICT_LUA)
        self.L.execute(I.INJECT_LUA)
        self.L.execute(TS.TERRAIN_LUA)
        self.L.execute(ASSETS_LUA)
        self.reload()


class AssetsShareTest(I.InjectTest):
    game = AssetsGame

    def test_a_tick_replaces_its_groups_at_the_stamp_on_every_game_and_a_group_changed_since_the_click_refuses_everywhere(self):
        tree, rock = "::/assets/vegetation/trees/brleaf_05/brleaf_05.mdl", "::/assets/vegetation/rocks/rock_02.mdl"
        # the brush merged two groups of the savegame (same ids on every game) into one group, and planted a second one
        new = [[[tree, 10.0, 20.0, 1.0], [tree, 12.0, 21.0, 1.0], [rock, 11.0, 23.0, 1.0]], [[tree, 40.0, 20.0, 1.0]]]
        text = tick_text([9001, 9002], [(9002, 0)], new)

        def world(*games):
            for g in games:
                g.L.execute('ADD_GROUP(9001, { { "%s", 10, 20, 1 } }); ADD_GROUP(9002, { { "%s", 11, 23, 1 } })' % (tree, rock))

        def round_trip():
            self.start()
            world(self.A, self.B)
            self.strict_on(self.A, self.B)
            self.B.native.tick(1, text)
            self.frames(2)
            self.assertNotIn("@40", self.B.lua("GROUPS()"), "never applied at the click, the originator's own game included")
            self.tick(30)
            want = self.A.lua("GROUPS()")
            self.assertEqual(self.B.lua("GROUPS()"), want, "the originator builds the same groups at the stamp")
            self.assertIn("@40", want)
            self.assertEqual(self.A.lua("WORLD[9001] == nil and WORLD[9002] == nil"), True, "the removed groups are gone")
            self.assertEqual((self.A.native.asset_orders, self.B.native.asset_orders), ([], []), "every order taken by its carrier")
            self.assertIn("asset tick 1: removes 2 group(s) (1 old2new), adds 2 group(s) of 4 model(s)", self.B.log())
            self.assertEqual(self.relay.verdict_resyncs, [], "both games applied it: no repair")

        def changed():
            self.start()
            world(self.A, self.B)
            self.strict_on(self.A, self.B)
            self.B.native.tick(1, text)
            self.frames(2)
            # before the stamp, something else changed group 9002 on every game alike (another tick, the same mechanic everywhere)
            for g in (self.A, self.B):
                g.L.execute('ADD_GROUP(9002, { { "%s", 11, 24, 1 } })' % rock)
            self.tick(30)
            for g in (self.A, self.B):
                self.assertIn("changed since the click", g.log())
                self.assertNotIn("@40", g.lua("GROUPS()"), "refused on every game: nothing applied")
            self.assertEqual(self.relay.verdict_resyncs, [], "the same refusal everywhere: no repair")

        self.cases([("round trip", {}, round_trip), ("changed since the click", {}, changed)])

    def test_the_vegetation_menu_stays_usable_while_ticks_are_shared(self):
        """Lab v028-trees-drag 2026-10-26: the menu's model tiles (``landscaping_vegetation.::/assets/.../x.mdl``: each tree, plant and rock
        the brushes place) were switched off while the share was on, so the plant and rock brushes could not be picked."""
        self.start()
        self.strict_on(self.A, self.B)
        self.frames(70)
        self.B.L.execute("GUI_API.gui = GUI_API.gui or {}; ENABLED = {}; GUI_API.gui.byId = { setEnabled = function(id, on, only) "
                         "ENABLED[#ENABLED + 1] = tostring(id) .. '=' .. tostring(on) .. '/' .. tostring(only) end }")
        veg = "menu.construction.landscaping.landscaping_vegetation."
        ids = [veg + "::/gui/construction/tools/mixed_plants_tool.res", veg + "::/gui/construction/tools/mixed_rocks_tool.res",
               veg + "::/assets/vegetation/plants/flower_01/flower_01.mdl", veg + "::/assets/rocks/gr_rck_01/gr_rck_01.mdl",
               veg + "::/assets/vegetation/trees/brleaf_05/brleaf_05.mdl", veg + "tab"]
        for i in ids:
            self.B.L.execute("GUI.guiHandleEvent(nil, STATE, GSTATE, 0, '%s', 'idAdded', {})" % i)
        off = [c for c in self.B.L.eval("ENABLED").values() if c.endswith("=false/true")]
        self.assertEqual(off, [], "no tile or tool of the vegetation menu is switched off while ticks are shared")


if __name__ == "__main__":
    unittest.main()
