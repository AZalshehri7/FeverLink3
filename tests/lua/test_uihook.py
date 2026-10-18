"""Tests for the UI-state hook (mod/feverlink3/content/feverlink3_uihook.lua).

The game's windows live in a Lua state of their own; the hook wraps api.cmd there. These tests run the hook in a fresh Lua
state with a mocked api (no second game) and check what it records for the bridge (ui_out.log) and what it passes on to the
game.

Run:  python -I -m unittest discover -s tests/lua -p "test_uihook.py"
"""
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import test_lockstep as T  # noqa: E402  (only for the lupa import and the mod folder)

lupa = T.lupa
CONTENT = os.path.join(HERE, "..", "..", "mod", "feverlink3", "content")

SETUP = r'''
local content = ...
package.loaded["feverlink3_ui_orig_cmds"] = nil
function ug_require(name)
  name = name:gsub("^feverlink3::/", "")
  if package.loaded["ug:" .. name] ~= nil then return package.loaded["ug:" .. name] end
  local f = io.open(content .. "/" .. name, "rb")
  if not f then error("module not found: " .. name) end
  local text = f:read("*a"); f:close()
  local chunk, err = load(text, name, "t")
  if not chunk then error(err) end
  local r = chunk()
  package.loaded["ug:" .. name] = r
  return r
end

SENT, CALLBACKS = {}, {}
api = {
  cmd = {
    sendCommand = function(cmd, cb, progress)
      SENT[#SENT + 1] = cmd
      if cb then cb({ resultEntity = 77 }, true, {}) end
    end,
    makeGameSetSpeedCmd = function(n) return { kind = "speed", n = n } end,
    makeScriptingSendEventCmd = function(a, b, c, d) return { kind = "event", a, b, c, d } end,
    makeLineCreateCmd = function(...) return { kind = "line", ... } end,
    makeWorldBuildProposalCmd = function(...) return { kind = "build", ... } end,
  },
  type = {},
}
'''


class UiHookTest(unittest.TestCase):
    def runtime(self, session=True):
        self.tmp = tempfile.mkdtemp(prefix="fl3-uihook-")
        for key in ("FEVERLINK3_SESSION_DIR", "FEVERLINK3_PLAYER_ID", "FEVERLINK3_LIVE_DIR"):
            os.environ.pop(key, None)
        if session:
            os.environ["FEVERLINK3_SESSION_DIR"] = self.tmp
            os.environ["FEVERLINK3_PLAYER_ID"] = "1"
        else:
            # live mode with nobody hosting or joined: the folder is there, identity.txt is not
            os.environ["FEVERLINK3_LIVE_DIR"] = self.tmp
        self.lua = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.lua.execute(SETUP, CONTENT.replace("\\", "/"))
        self.lua.execute('Hook = ug_require("feverlink3_uihook.lua"); C = ug_require("feverlink3_common.lua")')
        self.addCleanup(lambda: [os.environ.pop(k, None) for k in ("FEVERLINK3_SESSION_DIR", "FEVERLINK3_PLAYER_ID")])

    def records(self):
        path = os.path.join(self.tmp, "ui_out.log")
        if not os.path.exists(path):
            return []
        out = []
        for line in open(path, encoding="utf-8").read().splitlines():
            parts = line.split("\t", 2)
            if len(parts) == 3:
                payload = self.lua.eval("function(s) return C.parse(s) end")(parts[2])
                out.append((parts[0], payload))
        return out

    def sent(self):
        return int(self.lua.eval("#SENT"))

    def issue(self, expr):
        """Builds a command with a factory of the (wrapped) api and sends it, as a window does."""
        self.lua.execute("local cb = ...; local cmd = " + expr + "; api.cmd.sendCommand(cmd, CB)")

    def test_while_nobody_hosts_or_joins_every_command_is_untouched(self):
        self.runtime(session=False)
        self.assertTrue(self.lua.eval("Hook.install()"), "installed in live mode, inert until a session is on")
        self.lua.execute('api.cmd.sendCommand(api.cmd.makeGameSetSpeedCmd(2))')
        self.lua.execute('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Obtain", { 1, {} }))')
        self.assertEqual(2, self.sent(), "both reach the game")
        self.assertEqual([], self.records())

    def answer(self, id, success=True, e=None, key=None):
        """The bridge's answer about a held command (ui_in.log), as uiReply writes it."""
        self.lua.execute("local id, ok, e, key = ...; local w = C.newWriter(C.path('ui_in.log'), 1); "
                         "C.writerSend(w, 'result', { id = id, key = key, success = ok, e = e })", id, success, e, key)

    def test_a_loan_event_is_recorded_for_the_bridge_and_held_until_its_stamp_apply(self):
        # the originator applies a window command at its stamp like every other game: not at the click (that shifted the engine's
        # free-id FIFO against the other games)
        self.runtime()
        self.assertTrue(self.lua.eval("Hook.install()"))
        self.lua.execute('CB = nil; api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Obtain", { 1, { amount = 5000000 } }), function(d, s) CB = { s } end)')
        self.assertEqual(0, self.sent(), "the engine does not get it at the click")
        recs = self.records()
        self.assertEqual(["cmd"], [r[0] for r in recs])
        payload = recs[0][1]
        self.assertEqual(1, payload["id"])
        self.assertEqual("spend", payload["category"])
        body = self.lua.eval("function(s) return C.parse(s) end")(payload["body"])
        self.assertEqual("makeScriptingSendEventCmd", body["fn"])
        self.assertIsNone(body["native"], "nothing of it was applied where it was made")
        self.lua.execute("Hook.poll()")
        self.assertTrue(self.lua.eval("CB == nil"), "the callback waits for the stamp apply")
        self.answer(1, True)
        self.lua.execute("Hook.poll()")
        self.assertTrue(self.lua.eval("CB ~= nil and CB[1] == true"), "the callback ran with the stamp apply's result")
        self.lua.execute("CB = nil; Hook.poll()")
        self.assertTrue(self.lua.eval("CB == nil"), "once")

    def test_the_timing_line_shows_where_the_time_between_two_commands_of_one_handler_went(self):
        # Friend test 2026-10-16: a clone of two vehicles stood the game still for 16 s, 8 s between its two purchases, with no
        # line saying whether the window, the engine or this hook took it. Here the window spends 5 s between its two commands.
        self.runtime()
        self.lua.execute("Hook.install()")
        self.lua.execute("REAL_CLOCK = os.clock; T = 100; os.clock = function() return T end; Hook.poll()")
        self.lua.execute('T = 100.25; api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Obtain", { 1, {} }), function() end)')
        self.lua.execute('T = 105.25; api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Repay", { 1, {} }), function() end)')
        recs = self.records()
        self.assertEqual(len({int(r[1]["grp"]) for r in recs}), 1, "one handler, one group")
        self.assertEqual([float(r[1]["t"]["c"]) for r in recs], [100.25, 105.25], "the click travels with the record")
        for i in (1, 2):
            self.lua.execute("local i = ...; local w = C.newWriter(C.path('ui_in.log'), 1); C.writerSend(w, 'result', { id = i, success = true, "
                             "tm = { r = 105.3, s = 105.31, a = 106.0, at = 2000, now = 1800, i = i, n = 2 } })", i)
        self.lua.execute("T = 106.1; Hook.poll(); os.clock = REAL_CLOCK")
        with open(os.path.join(self.tmp, "mod.log"), encoding="utf-8") as f:
            log = f.read()
        lines = [l for l in log.splitlines() if "timing window #" in l]
        self.assertEqual(len(lines), 2, log)
        self.assertIn("(1 of 2) stamp=2000 now=1800: since_tick=250 gap=-", lines[0])
        self.assertIn("(2 of 2) stamp=2000 now=1800: since_tick=5250 gap=5000", lines[1])
        self.assertIn("read=+50 shipped=+60 applied=+750 answered=+850 ms after the click", lines[1])
        self.assertIn("5000 ms in the window since its previous command", log)

    def test_an_event_nobody_replicates_passes_through_without_a_record(self):
        self.runtime()
        self.lua.execute("Hook.install()")
        self.lua.execute('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Tutorial", "Step", {}))')
        self.assertEqual(1, self.sent())
        self.assertEqual([], self.records())

    def test_a_speed_button_becomes_a_request_and_is_not_applied(self):
        self.runtime()
        self.lua.execute("Hook.install()")
        self.lua.execute("api.cmd.sendCommand(api.cmd.makeGameSetSpeedCmd(2))")
        self.assertEqual(0, self.sent(), "the session speed is the host's to decide")
        recs = self.records()
        self.assertEqual([("speed", 2)], [(r[0], r[1]["n"]) for r in recs])

    def test_a_speed_asked_every_frame_is_sent_once_a_second(self):
        # friend test 2026-10-16, resync 4: the game's speed control asked "speed 4" every frame while the world loaded (2760 records)
        self.runtime()
        self.lua.execute("Hook.install()")
        for _ in range(50):
            self.lua.execute("api.cmd.sendCommand(api.cmd.makeGameSetSpeedCmd(4))")
        self.lua.execute("api.cmd.sendCommand(api.cmd.makeGameSetSpeedCmd(0))")
        self.assertEqual([("speed", 4), ("speed", 0)], [(r[0], r[1]["n"]) for r in self.records()], "another speed goes at once")
        self.assertEqual(0, self.sent())

    def test_the_entity_a_command_created_reaches_its_callback_from_the_bridges_answer(self):
        # a command with the generic schema (a player argument, an entity created, the field the window reads it from)
        self.runtime()
        self.lua.execute("""
          local L = ug_require("feverlink3_lockstep.lua")
          L.COMMANDS.makeFakeCreateCmd = { player = { 1 }, creates = "LINE", result = "resultEntity", category = "build" }
          api.cmd.makeFakeCreateCmd = function(...) return { kind = "fake", ... } end
        """)
        self.lua.execute("Hook.install()")
        self.lua.execute("api.cmd.sendCommand(api.cmd.makeFakeCreateCmd(5, 'x'), function(d, s, e) MADE = { s, d.resultEntity, e[1][1] } end)")
        self.assertEqual(0, self.sent())
        self.assertEqual(["cmd"], [r[0] for r in self.records()], "no bind record: the bridge keys the entity at its own stamp apply")
        self.answer(1, True, 88, "1:4")
        self.lua.execute("Hook.poll()")
        self.assertEqual("true,88,88", self.lua.eval("tostring(MADE[1]) .. ',' .. MADE[2] .. ',' .. MADE[3]"))
        self.assertEqual("1:4", self.lua.eval("Hook.keyOf(88)"), "named by its key at once, before binds.txt has it")

    def test_a_command_refused_at_its_stamp_fires_the_callback_with_a_failure(self):
        # the engine's refusal (not enough money...) is the stamp apply's, the same on every game; the window hears it then.
        # The engine is never called at the click, so a refusal there (seen in a real game: "API is currently restricted")
        # cannot leave the command applied on the other games only.
        self.runtime()
        self.lua.execute("Hook.install()")
        self.lua.execute("""
          local orig = package.loaded["feverlink3_ui_orig_cmds"]
          orig.sendCommand = function(cmd, cb, progress) ATTEMPTS = (ATTEMPTS or 0) + 1; error("API is currently restricted") end
        """)
        self.lua.execute('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Obtain", { 1, {} }), function(d, s, e) REFUSED = { s, #e } end)')
        self.assertTrue(self.lua.eval("ATTEMPTS == nil"), "the engine is not asked at the click")
        self.assertEqual(["cmd"], [r[0] for r in self.records()])
        self.answer(1, False)
        self.lua.execute("Hook.poll()")
        self.assertEqual("false,0", self.lua.eval("tostring(REFUSED[1]) .. ',' .. REFUSED[2]"))

    def test_a_command_the_bridge_never_answers_fails_after_the_limit(self):
        self.runtime()
        self.lua.execute("Hook.install()")
        self.lua.execute('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Obtain", { 1, {} }), function(d, s) LOST = s end)')
        self.lua.execute("local real = os.clock; os.clock = function() return real() + 1000 end; Hook.poll(); os.clock = real")
        self.assertTrue(self.lua.eval("LOST == false"))

    def test_an_answer_of_an_earlier_run_of_the_window_state_is_not_taken_for_this_runs_command(self):
        self.runtime()
        self.answer(1, False)   # in the file before this state starts: record id 1 of an earlier run
        self.lua.execute("Hook.install()")
        self.lua.execute('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Obtain", { 1, {} }), function(d, s) GOT = s end)')
        self.lua.execute("Hook.poll()")
        self.assertTrue(self.lua.eval("GOT == nil"))
        self.answer(1, True)
        self.lua.execute("Hook.poll()")
        self.assertTrue(self.lua.eval("GOT == true"))

    def test_a_command_that_cannot_be_replicated_is_refused_here_too_and_the_player_is_told(self):
        # friend test 2026-10-16: line edits that "do not rebuild equal" went to the engine on the editing game only; the worlds
        # differed and the resync erased the edit. Now nothing reaches the engine, the window hears no, the player reads why.
        self.runtime()
        self.lua.execute("""
          local L = ug_require("feverlink3_lockstep.lua")
          L.COMMANDS.makeFakeEditCmd = { category = "build", encode = function() return nil, "argument 2: does not rebuild equal at X.y.3" end }
          L.COMMANDS.makeFakeFaultCmd = { category = "build", encode = function() error("boom") end }
          api.cmd.makeFakeEditCmd = function(...) return { kind = "edit", ... } end
          api.cmd.makeFakeFaultCmd = function(...) return { kind = "fault", ... } end
        """)
        self.lua.execute("Hook.install()")
        self.lua.execute("api.cmd.sendCommand(api.cmd.makeFakeEditCmd(71, {}), function(d, s) EDIT = { s } end)")
        self.lua.execute("api.cmd.sendCommand(api.cmd.makeFakeFaultCmd(71, {}), function(d, s) FAULT = { s } end)")
        self.assertEqual(0, self.sent(), "the engine gets neither: never applied on this game alone")
        self.assertTrue(self.lua.eval("EDIT ~= nil and EDIT[1] == false and FAULT ~= nil and FAULT[1] == false"), "both windows are told no")
        recs = self.records()
        self.assertEqual(["notice", "notice"], [r[0] for r in recs], "no cmd record: nothing to stamp")
        self.assertIn("Your fake edit was not applied: it cannot be shared with the other players (argument 2: does not rebuild equal at X.y.3)",
                      recs[0][1]["text"])
        self.assertIn("boom", recs[1][1]["text"])

    def test_builds_are_left_to_the_bridge_which_sees_them_where_the_engine_applies_them(self):
        self.runtime()
        self.lua.execute("Hook.install()")
        self.lua.execute("api.cmd.sendCommand(api.cmd.makeWorldBuildProposalCmd({}, nil, false, true))")
        self.assertEqual(1, self.sent())
        self.assertEqual([], self.records())

    def test_installing_twice_does_not_wrap_the_wrapper(self):
        self.runtime()
        self.lua.execute("Hook.install()")
        self.lua.execute("Hook.installed = false; Hook.install()")
        self.lua.execute('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Repay", { nil, {} }))')
        self.assertEqual(0, self.sent(), "held for its stamp")
        self.assertEqual(1, len(self.records()), "one record, not two")

    def test_an_entity_a_window_command_just_made_is_named_by_its_key_not_by_its_id(self):
        # the bridge's table of keys (binds.txt) reaches the hook about a second after a creation; a command that names the
        # new entity before that used to carry "s:<id>", an id the other games do not share
        self.runtime()
        self.lua.execute("""
          local L = ug_require("feverlink3_lockstep.lua")
          L.COMMANDS.makeFakeCreateCmd = { player = { 1 }, creates = "LINE", result = "resultEntity", category = "build" }
          L.COMMANDS.makeFakeUseCmd = { entity = { 1 }, category = "other" }
          api.cmd.makeFakeCreateCmd = function(...) return { kind = "fake", ... } end
          api.cmd.makeFakeUseCmd = function(...) return { kind = "use", ... } end
        """)
        self.lua.execute("Hook.install()")
        self.lua.execute("api.cmd.sendCommand(api.cmd.makeFakeCreateCmd(5, 'x'), function() end)")
        self.answer(1, True, 77, "1:1")   # the bridge's answer at the stamp: entity 77, key 1:1
        self.lua.execute("Hook.poll()")
        self.lua.execute("api.cmd.sendCommand(api.cmd.makeFakeUseCmd(77))")
        self.lua.execute("api.cmd.sendCommand(api.cmd.makeFakeUseCmd(5000))")   # an entity of the savegame
        recs = self.records()
        self.assertEqual(["cmd", "cmd", "cmd"], [r[0] for r in recs])
        parse = self.lua.eval("function(s) return C.parse(s) end")
        used = parse(recs[1][1]["body"])
        self.assertEqual("1:1", used["args"][1]["key"], "named by the key of the command that made it")
        saved = parse(recs[2][1]["body"])
        self.assertEqual("s:5000", saved["args"][1]["key"])
        # once the bridge's table has it, that key wins
        with open(os.path.join(self.tmp, "binds.txt"), "w", encoding="utf-8") as f:
            f.write('{["1:7"]=77}')
        self.lua.execute("api.cmd.sendCommand(api.cmd.makeFakeUseCmd(77))")
        again = parse(self.records()[-1][1]["body"])
        self.assertEqual("1:7", again["args"][1]["key"])


class WindowTickerTest(unittest.TestCase):
    """The window refresh (Hud.newTicker): two drivers (step timer, per-frame onStep) share one 0.5 s throttle, so the
    window refreshes while the game is paused and the guest sees the host's end of the session."""

    def setUp(self):
        self.lua = lupa.LuaRuntime(unpack_returned_tuples=True)
        src = open(os.path.join(CONTENT, "feverlink3_hud.lua"), encoding="utf-8").read()
        self.lua.execute("local src = ...; Hud = assert(load(src, '=hud', 't'))()", src)

    def test_frame_driver_alone_refreshes_at_the_interval_and_reports_the_ended_state(self):
        r = self.lua.execute(r"""
            local now, state, seen = 0, "joined", {}
            local t = Hud.newTicker(0.5, function() return now end, function() seen[#seen + 1] = state; return state end)
            local out = {}
            out[#out + 1] = tostring(t())            -- first frame: runs
            now = 0.1; state = "failed"
            out[#out + 1] = tostring(t())            -- too soon: skipped
            now = 0.6
            out[#out + 1] = tostring(t())            -- due: sees the ended state
            now = 0.7
            out[#out + 1] = tostring(t())            -- throttled again
            return table.concat(out, ",") .. "|" .. #seen
        """)
        self.assertEqual("joined,nil,failed,nil|2", r)

    def test_a_clock_that_goes_back_does_not_stall_the_ticker(self):
        r = self.lua.execute(r"""
            local now, n = 100, 0
            local t = Hud.newTicker(0.5, function() return now end, function() n = n + 1; return true end)
            t(); now = 1; t()
            return n
        """)
        self.assertEqual(2, r)


if __name__ == "__main__":
    unittest.main()
