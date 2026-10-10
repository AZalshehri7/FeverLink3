"""Town growth is held still while a session runs and given back when it is over.

The engine grows a town with choices that differ from one game to the next (seen in the real game: the same save, 90 s later,
other building variants and levels), so a build through a town fails on the other games and the worlds drift. The simulation half
switches growth off for every town (the switch of the town window) and keeps the towns it switched in the savegame's script
state, to give them back when the session is over or when a plain game loads the save.

The games here run in live mode (the native module hosts the session): identity.txt says whether a session exists.

Run:  python -I -m unittest discover -s tests/lua -p "test_towns.py"
"""
import os
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import test_lockstep as T  # noqa: E402

PATCH = r'''
TOWNS = TOWNS or { [301] = { developmentActive = true }, [302] = { developmentActive = true }, [303] = { developmentActive = false } }
TOWN_CMDS = TOWN_CMDS or {}

local function patch(api)
  api.type.ComponentType.TOWN = 99
  local each, get = api.engine.forEachEntityWithComponent, api.engine.getComponent
  api.engine.forEachEntityWithComponent = function(fn, t)
    if t ~= 99 then return each(fn, t) end
    local ids = {}
    for e in pairs(TOWNS) do ids[#ids + 1] = e end
    table.sort(ids)
    for _, e in ipairs(ids) do fn(e) end
  end
  api.engine.getComponent = function(e, t)
    if t == 99 then return TOWNS[e] end
    return get(e, t)
  end
  api.cmd.makeTownSetDevelopmentActiveCmd = function(e, on)
    return { __cmd = "makeTownSetDevelopmentActiveCmd", args = { n = 2, e, on } }
  end
end
patch(SIM_ENV.api)

if not EXECUTE_WRAPPED then
  EXECUTE_WRAPPED = true
  local BASE_EXECUTE = EXECUTE
  function EXECUTE(cmd, cb)
    if cmd.__cmd == "makeTownSetDevelopmentActiveCmd" then
      local a = cmd.args
      TOWN_CMDS[#TOWN_CMDS + 1] = a[1] .. ":" .. tostring(a[2])
      TOWNS[a[1]].developmentActive = a[2]
      if cb then cb({}, true, {}) end
      return
    end
    return BASE_EXECUTE(cmd, cb)
  end
end

function HELD()
  local st = STATE:get()
  local out = {}
  for e in pairs((st and st.townsHeld) or {}) do out[#out + 1] = e end
  table.sort(out)
  return table.concat(out, ",")
end
function GROWING()
  local out = {}
  for e, t in pairs(TOWNS) do if t.developmentActive then out[#out + 1] = e end end
  table.sort(out)
  return table.concat(out, ",")
end
'''


class TownGame(T.Game):
    """A game in live mode: the mod is active while identity.txt names a role of host or player."""

    def use_env(self):
        for k in ("FEVERLINK3_SESSION_DIR", "FEVERLINK3_PLAYER_ID", "FEVERLINK3_PLAYER_NAME"):
            os.environ.pop(k, None)
        os.environ["FEVERLINK3_LIVE_DIR"] = self.dir
        if not os.path.exists(os.path.join(self.dir, "identity.txt")):
            self.set_role("host")

    def set_role(self, role):
        with open(os.path.join(self.dir, "identity.txt"), "w", encoding="utf-8") as f:
            f.write("id=0\nname=host\nrole=%s\n" % role)

    def patch(self):
        self.L.execute(PATCH)

    def steps(self, n):
        for _ in range(n):
            self.step()

    def growing(self):
        return self.L.eval("GROWING()")

    def held(self):
        return self.L.eval("HELD()")

    def town_cmds(self):
        return list(self.L.eval("table.concat(TOWN_CMDS, ',')").split(",")) if self.L.eval("#TOWN_CMDS") else []


class TownHoldTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-towns-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.addCleanup(os.environ.pop, "FEVERLINK3_TOWN_GROWTH", None)
        self.addCleanup(os.environ.pop, "FEVERLINK3_LIVE_DIR", None)

    def game(self):
        g = TownGame("host", 0, self.tmp, 50000)
        g.patch()
        return g

    def test_a_session_switches_growth_off_for_the_towns_that_grow(self):
        g = self.game()
        self.assertEqual(g.growing(), "301,302")
        g.steps(30)
        self.assertEqual(g.growing(), "", "no town grows while the session runs")
        self.assertEqual(g.held(), "301,302", "the mod remembers the towns it switched off")
        self.assertIn("towns held still: 2", g.log())

    def test_a_town_the_player_had_switched_off_is_left_alone_and_never_given_back(self):
        g = self.game()
        g.steps(30)
        g.set_role("idle")
        g.steps(30)
        self.assertEqual(g.growing(), "301,302", "the two the mod switched off grow again")
        self.assertNotIn("303:true", g.town_cmds(), "the town the player switched off stays off")
        self.assertNotIn("303:false", g.town_cmds())

    def test_a_town_switched_on_again_during_the_session_is_held_again(self):
        g = self.game()
        g.steps(30)
        g.L.execute("TOWNS[301].developmentActive = true")
        g.steps(14)
        self.assertEqual(g.growing(), "")

    def test_nothing_is_touched_when_growth_is_allowed_by_the_environment(self):
        os.environ["FEVERLINK3_TOWN_GROWTH"] = "1"
        g = self.game()
        g.steps(40)
        self.assertEqual(g.growing(), "301,302")
        self.assertEqual(g.held(), "")
        self.assertEqual(g.town_cmds(), [])

    def test_the_towns_are_given_back_when_the_session_is_over_and_the_world_stays(self):
        g = self.game()
        g.steps(30)
        self.assertEqual(g.growing(), "")
        g.set_role("idle")                       # the host stopped hosting; the world stays open
        g.steps(14)
        self.assertEqual(g.growing(), "301,302")
        self.assertEqual(g.held(), "")
        self.assertIn("towns grow again: 2 given back", g.log())

    def test_a_save_made_during_a_session_grows_again_when_a_plain_game_loads_it(self):
        g = self.game()
        g.steps(30)
        self.assertEqual(g.held(), "301,302")
        # the savegame carries the script state and the switched-off towns; the loading game has no session
        g.set_role("idle")
        g.reload()                               # fresh Lua states, the world and its script state stay
        g.patch()
        self.assertEqual(g.growing(), "", "still off after the load")
        g.steps(70)
        self.assertEqual(g.growing(), "301,302")
        self.assertEqual(g.held(), "")
        self.assertIn("towns grow again: no session", g.log())

    def test_a_session_that_starts_again_in_the_same_world_holds_the_towns_again(self):
        g = self.game()
        g.steps(30)
        g.set_role("idle")
        g.steps(30)
        self.assertEqual(g.growing(), "301,302")
        g.set_role("host")
        g.steps(30)
        self.assertEqual(g.growing(), "")


if __name__ == "__main__":
    unittest.main()
