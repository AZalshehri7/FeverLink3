"""Builds made with the game's native tools (roads, tracks...): captured where the engine announces them, rebuilt on the other
games at a stamp, never applied twice and never shipped back.

The mock engine announces every build it applies to the simulation state's handleEvent ("apply_command" /
"onPreBuildProposal", just before applying: finding from MPFever, MIT) exactly as the game does, replays of other players'
actions included. A native tool's command does not pass the mod's wrapped sendCommand, so it is put in the engine's own queue.

Run:  python -I -m unittest discover -s tests/lua -p "test_native_capture.py"
"""
import os
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import test_cmds_build as B  # noqa: E402  (the construction harness; imported, not edited)
import test_lockstep as T  # noqa: E402

ANNOUNCE = r'''
-- the engine tells the game script about a build just before it applies it
ANNOUNCE_BUILDS = true
ANNOUNCED = 0
local BASE_EXECUTE_BUILD = EXECUTE
function EXECUTE(cmd, cb)
  if ANNOUNCE_BUILDS and cmd.__cmd == "makeWorldBuildProposalCmd" then
    ANNOUNCED = ANNOUNCED + 1
    local a = cmd.args
    SIM_EVENT(0, "apply_command", "onPreBuildProposal", { a[1], a[2], a[3], a[4] })
  end
  -- and again once it has run (onPostBuildProposal, with the entities the build made)
  local a = cmd.args
  return BASE_EXECUTE_BUILD(cmd, function(res, ok, ents)
    if cb then cb(res, ok, ents) end
    if ANNOUNCE_BUILDS and cmd.__cmd == "makeWorldBuildProposalCmd" and ok then
      SIM_EVENT(0, "apply_command", "onPostBuildProposal", { a[1], nil, ents, a[4] })   -- (the real post event carries no Context)
    end
  end)
end

-- what a native tool does: its command goes straight to the engine, past the mod's wrapped sendCommand
function NATIVE_BUILD(spec)
  local p = SIMPLE_ROAD(spec)
  local cmd = SIM_ENV.api.cmd.makeWorldBuildProposalCmd(p, nil, false, true)
  GAME.guiQueue[#GAME.guiQueue + 1] = { cmd, nil }
end
'''


class NativeGame(B.BuildGame):
    def __init__(self, label, player_id, tmp, first_entity):
        super().__init__(label, player_id, tmp, first_entity)
        self.L.execute(ANNOUNCE)

    def native(self, spec):
        self.L.execute("NATIVE_BUILD(%s)" % spec)

    def announced(self):
        return int(self.L.eval("ANNOUNCED"))

    def builds_applied(self):
        return int(self.L.eval("(function() local n = 0; for _, a in ipairs(GAME.applied) do if a.name == 'makeWorldBuildProposalCmd' then n = n + 1 end end; return n end)()"))

    def shipped_count(self):
        return len(self.shipped())


class NativeCaptureTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-native-")
        self.A = NativeGame("host", 0, self.tmp, 50000)
        self.B = NativeGame("client", 1, self.tmp, 51000)
        self.relay = T.Relay([self.A, self.B])

    def tearDown(self):
        if os.environ.get("FEVERLINK3_TEST_VERBOSE"):
            print("\n--- host mod.log ---\n" + self.A.log() + "\n--- client mod.log ---\n" + self.B.log())
        shutil.rmtree(self.tmp, ignore_errors=True)

    def tick(self, n=1):
        for _ in range(n):
            for _ in range(4):
                for g in (self.A, self.B):
                    g.frame()
                self.relay.pump()
            self.A.step()
            self.B.step()

    def start(self, speed=1):
        self.tick(3)
        self.relay.started, self.relay.speed = True, speed
        self.relay.session()
        self.tick(9)

    def road(self, x0, y0, x1, y1):
        return "{ a = { x = %d, y = %d, z = 0 }, b = { x = %d, y = %d, z = 0 } }" % (x0, y0, x1, y1)

    def test_a_road_made_with_a_native_tool_appears_on_the_other_game_once(self):
        self.start()
        self.B.native(self.road(300, 0, 400, 0))
        self.tick(16)
        self.assertEqual(self.A.sig(), self.B.sig(), "both games hold the same world")
        self.assertIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.A.sig())
        self.assertEqual(self.B.builds_applied(), 1, "the originator applied its own build only once (natively)")
        self.assertEqual(self.A.builds_applied(), 1, "the other game applied it once, at the stamp")
        self.assertEqual(self.B.shipped_count(), 1)
        self.assertEqual(self.A.shipped_count(), 0, "a replay is not shipped again (it would come back as the host's own build)")
        self.assertIn("native build captured", self.B.log())
        self.assertNotIn("refused", self.A.log() + self.B.log())

    def test_a_build_the_originators_engine_accepted_with_ignore_errors_is_replayed_with_ignore_errors(self):
        # sweep run 20261013-061744 (B5): the guest applied a construction with ignoreErrors = true after "Empty Construction"; the host replayed it
        # with false, the engine refused it silently and the games drifted
        self.start()
        for g in (self.A, self.B):
            g.run("BUILD_NEEDS_IE = function(p) return true end")
        self.B.run("GAME.guiQueue[#GAME.guiQueue + 1] = { SIM_ENV.api.cmd.makeWorldBuildProposalCmd(SIMPLE_ROAD(%s), nil, true, true), nil }" % self.road(300, 0, 400, 0))
        self.tick(20)
        self.assertEqual(self.A.sig(), self.B.sig(), "both games hold the same world")
        self.assertIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.A.sig())
        self.assertEqual(self.A.builds_applied(), 1)

    def test_the_originators_own_native_build_gets_its_keys_like_a_replay(self):
        # a depot the engine built at the click and the capture shipped: the originator keys it too, so that a vehicle bought at it can be
        # named for the other game (real session 2026-10-12: "stationGroup s:45494 does not exist here")
        self.start()
        self.B.run("DEPOT_P = NATIVE_DEPOT_ON_ROAD{ edge = 110, at = { x = 50, y = 0, z = 0 }, door = { x = 50, y = -30, z = 0 }, params = { seed = 260 } }; "
                   "GAME.guiQueue[#GAME.guiQueue + 1] = { SIM_ENV.api.cmd.makeWorldBuildProposalCmd(DEPOT_P, nil, false, true), nil }")
        self.tick(20)
        for g in (self.A, self.B):
            self.assertIn("1:1#c1", str(g.lua("BOUND_KEYS()")).split(), "%s keyed the depot" % g.label)
        self.assertIn("this game's own native build is keyed here like a replay", self.B.log())

    def test_a_stop_or_signal_without_a_model_before_the_build_is_read_after_it_and_reaches_the_other_game(self):
        # the engine names the model of an edge object only when the build has run (a real game, 2026-10-11: "an edge object has no
        # model" for every stop built with the stop tool, and the other game never got it)
        self.start()
        self.B.run("SIGNAL_P = NATIVE_SIGNAL{ edge = 130, param = 0.75 }; "
                   "GAME.guiQueue[#GAME.guiQueue + 1] = { SIM_ENV.api.cmd.makeWorldBuildProposalCmd(SIGNAL_P, nil, false, true), nil }")
        self.tick(16)
        self.assertIn("has an edge object that the engine has not made yet", self.B.log())
        self.assertIn("native build captured", self.B.log())
        self.assertNotIn("is not replicated", self.B.log())
        self.assertNotIn("refused", self.A.log() + self.B.log())
        self.assertEqual(self.A.sig(), self.B.sig(), "both games hold the same world")
        self.assertIn("signal/signal_a.mdl@0.750k2", self.A.sig())
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied(), self.A.shipped_count(), self.B.shipped_count()), (1, 1, 0, 1))

    def test_a_build_read_after_it_ran_keeps_the_context_of_the_pre_build_event_and_the_constructions_it_removes(self):
        # real run 2026-10-13: the stop tool's deferred record was replayed with the fallback flags (the post event has no Context), and a depot
        # over buildings could not be read after the build ("construction ... is not in the world")
        self.start()
        self.B.run("SIGNAL_P = NATIVE_SIGNAL{ edge = 130, param = 0.75 }; "
                   "GAME.guiQueue[#GAME.guiQueue + 1] = { SIM_ENV.api.cmd.makeWorldBuildProposalCmd(SIGNAL_P, CONTEXT(false, false, false, true), false, true), nil }")
        self.tick(16)
        self.assertNotIn("is not replicated", self.B.log())
        self.assertEqual(self.A.lua("BUILD_CTX[1]"), "align=false cleanup=false buildings=false fields=true player=7 ie=false pi=true")
        self.assertNotIn("carries no Context flags", self.A.log())

    def test_the_originator_keys_its_own_native_stop_and_a_line_by_the_other_game_names_it(self):
        # sweep run 20261013-081155 (F1): the guest built two stops natively, never keyed them itself ("native.bind: no snapshot stored"), and
        # the host's line over them was refused on the guest, "unresolved stationGroup reference 1:12#o1/g"
        self.start()
        self.B.run("STOP_P = SIMPLE_STOP{ edge = 110, param = 0.5, left = true }; "
                   "GAME.guiQueue[#GAME.guiQueue + 1] = { SIM_ENV.api.cmd.makeWorldBuildProposalCmd(STOP_P, nil, false, true), nil }")
        self.tick(24)
        for g in (self.A, self.B):
            keys = str(g.lua("BOUND_KEYS()")).split()
            self.assertIn("1:1#o1", keys, "%s keyed the stop" % g.label)
            self.assertIn("1:1#o1/g", keys, "%s keyed the stop's group" % g.label)
        self.assertEqual(self.B.sig(), self.A.sig())

    def test_the_same_in_the_other_direction(self):
        self.start()
        self.A.native(self.road(0, 300, 0, 400))
        self.tick(16)
        self.assertEqual(self.A.sig(), self.B.sig())
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (1, 1))
        self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (1, 0))

    def test_several_builds_in_a_row_each_land_once_and_the_marks_do_not_leak(self):
        self.start()
        for i in range(4):
            self.B.native(self.road(300 + 150 * i, 0, 400 + 150 * i, 0))
            self.tick(3)
        for _ in range(40):
            if self.A.builds_applied() == 4:
                break
            self.tick(1)
        self.assertEqual(self.A.builds_applied(), 4)
        self.assertEqual((self.A.shipped_count(), self.B.shipped_count()), (0, 4), "the replays were not shipped back")
        # a real build the very moment after the last replay is still captured: a mark left behind must not swallow it
        self.A.native(self.road(0, 700, 0, 800))
        self.tick(20)
        self.assertEqual(self.A.sig(), self.B.sig())
        self.assertEqual(self.A.shipped_count(), 1, "the host's own build was captured")
        self.assertEqual((self.A.builds_applied(), self.B.builds_applied()), (5, 5))

    def test_both_games_building_at_once_end_up_with_both_roads(self):
        self.start()
        self.A.native(self.road(0, 300, 0, 400))
        self.B.native(self.road(300, 0, 400, 0))
        self.tick(20)
        self.assertEqual(self.A.sig(), self.B.sig())
        self.assertIn("E 300.00,0.00,0.00 400.00,0.00,0.00", self.A.sig())
        self.assertIn("E 0.00,300.00,0.00 0.00,400.00,0.00", self.B.sig())

    def test_the_company_ledger_counts_the_build_on_both_games_so_the_checksum_agrees(self):
        # Seen in a real game: only the games that replayed an action recorded it in the company ledger, so every build made
        # the games "differ in companies" until a resync copied everything.
        self.start()
        self.B.native(self.road(300, 0, 400, 0))
        self.tick(16)
        self.relay.checksum_request(1, self.relay.future_stamp(), False)
        self.tick(40)
        ta, pa = self.relay.reports[1][0]
        tb, pb = self.relay.reports[1][1]
        self.assertEqual(pa, pb, "every checksum part agrees")
        self.assertIn("companies", pa, "the ledger is part of the checksum once an action was recorded")


if __name__ == "__main__":
    unittest.main()
