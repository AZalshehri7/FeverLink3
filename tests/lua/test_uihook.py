"""Tests for the UI-state hook (mod/feverlink3_1/content/feverlink3_uihook.lua).

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
CONTENT = os.path.join(HERE, "..", "..", "mod", "feverlink3_1", "content")

SETUP = r'''
local content = ...
package.loaded["feverlink3_ui_orig_cmds"] = nil
function ug_require(name)
  name = name:gsub("^feverlink3_1::/", "")
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
            parts = line.split("\t", 3)
            if len(parts) == 4:
                payload = self.lua.eval("function(s) return C.parse(s) end")(parts[3])
                out.append((parts[1], payload))
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

    def test_a_loan_event_is_recorded_for_the_bridge_and_applied_natively(self):
        self.runtime()
        self.assertTrue(self.lua.eval("Hook.install()"))
        self.lua.execute('CB = nil; api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Obtain", { 1, { amount = 5000000 } }))')
        self.assertEqual(1, self.sent(), "the game still applies it here")
        recs = self.records()
        self.assertEqual(["cmd"], [r[0] for r in recs])
        payload = recs[0][1]
        self.assertEqual(1, payload["id"])
        self.assertEqual("spend", payload["category"])
        body = self.lua.eval("function(s) return C.parse(s) end")(payload["body"])
        self.assertEqual("makeScriptingSendEventCmd", body["fn"])
        self.assertTrue(body["native"], "marked as applied where it was made")

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

    def test_a_created_entity_is_reported_back_so_later_commands_can_name_it(self):
        self.runtime()
        # a command with the generic schema (a player argument, an entity created, the field the window reads it from)
        self.lua.execute("""
          local L = ug_require("feverlink3_lockstep.lua")
          L.COMMANDS.makeFakeCreateCmd = { player = { 1 }, creates = "LINE", result = "resultEntity", category = "build" }
          api.cmd.makeFakeCreateCmd = function(...) return { kind = "fake", ... } end
        """)
        self.lua.execute("Hook.install()")
        self.lua.execute("api.cmd.sendCommand(api.cmd.makeFakeCreateCmd(5, 'x'), function() end)")
        self.assertEqual(1, self.sent())
        recs = self.records()
        self.assertEqual(["cmd", "bind"], [r[0] for r in recs])
        self.assertEqual(1, recs[1][1]["id"])
        self.assertEqual(77, recs[1][1]["e"])

    def test_a_command_the_engine_refuses_outright_is_not_recorded_and_not_sent_twice(self):
        # seen in a real game: sendCommand raised "API is currently restricted" when called from a context that may not send;
        # the command had been recorded already, so only the OTHER game applied it
        self.runtime()
        self.lua.execute("Hook.install()")
        self.lua.execute("RAISE = true; local real = api.cmd.sendCommand; ")
        self.lua.execute("""
          -- the engine's own sendCommand, replaced by one that refuses (the hook keeps the original it found at install)
          local orig = package.loaded["feverlink3_ui_orig_cmds"]
          orig.sendCommand = function(cmd, cb, progress) ATTEMPTS = (ATTEMPTS or 0) + 1; error("API is currently restricted") end
        """)
        ok, err = self.lua.eval('(function() return pcall(function() api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Obtain", { 1, {} })) end) end)()')
        self.assertFalse(ok, "the caller sees the engine's error, as without the mod")
        self.assertIn("API is currently restricted", str(err))
        self.assertEqual(1, int(self.lua.eval("ATTEMPTS")), "sent once, not retried by the hook")
        self.assertEqual([], self.records(), "nothing reached the other games")

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
        self.assertEqual(1, self.sent())
        self.assertEqual(1, len(self.records()), "one record, not two")


if __name__ == "__main__":
    unittest.main()
