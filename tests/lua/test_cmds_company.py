"""Tests for the company layer of the FeverLink3 Lua mod (feverlink3_cmds_company.lua, docs/COMPANIES.md).

Two parts, both through lupa (Lua 5.2):
  * CompanyUnit: the module on its own. Registry operations and who may do them, attribution, the ledger and the
    spending limit, payments, the checksum digest, the window's status table, the command codecs, the adapters.
  * CompanyGameTest: the real mod in two mocked Transport Fever 3 games (the harness of test_lockstep.py, extended
    with more than one player entity, per-player accounts and ownership), a mocked host relay in between. Checks that
    the registry and the ledger come out identical on both games, that `player` arguments become the sender's company
    entity on every game (whose ids differ), that limits stop spending, that actions applied during a pause end up in
    the same state, and that a reload keeps the registry.

Everything the engine would have to confirm is mocked here; the experiments that confirm it are in
docs/COMPANIES.md section 8.

Run:  python -I tests/lua/test_cmds_company.py        (or: python -I -m unittest discover -s tests/lua -p "test_*.py")
"""
import glob
import os
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import lupa.lua52 as lupa

import test_lockstep as tl  # helpers only: the module's own test cases are not imported by name, so they run once

COMPANY_SRC = tl.read_src("feverlink3_cmds_company.lua")
TAB = tl.TAB
STEP = tl.STEP

# ---------------------------------------------------------------- helpers


def to_py(v):
    """A Lua value as plain Python: tables become dicts keyed by int/str (numbers first, in order: the order Lua
    iterates a table in is not the same on every Lua), everything else is unchanged."""
    if lupa.lua_type(v) == "table":
        items = []
        for k, x in v.items():
            if isinstance(k, float) and k == int(k):
                k = int(k)
            items.append((k, to_py(x)))
        items.sort(key=lambda kv: (0, kv[0], "") if isinstance(kv[0], (int, float)) and not isinstance(kv[0], bool) else (1, 0, str(kv[0])))
        return dict(items)
    return v


def pair(r):
    """A Lua call's results as (first, second): lupa returns a bare value when there is only one."""
    return r if isinstance(r, tuple) else (r, None)


def row(status, player_id):
    """The status row of one player (the table lists the players the registry, the ledger or the roster know)."""
    for r in status["players"].values():
        if r["id"] == player_id:
            return r
    raise KeyError(player_id)


def allowed(result):
    """Whether a Co.gate result says yes. lupa returns a bare true for yes and a tuple (false, why) for no, which is truthy."""
    return pair(result)[0] is True


def lit(v):
    """A Python value as a Lua source literal."""
    if v is None:
        return "nil"
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return repr(v)
    if isinstance(v, str):
        return tl.lua_str(v)
    if isinstance(v, (list, tuple)):
        return "{" + ",".join(lit(x) for x in v) + "}"
    if isinstance(v, dict):
        return "{" + ",".join("[%s]=%s" % (lit(k), lit(x)) for k, x in v.items()) + "}"
    raise TypeError(v)


# ---------------------------------------------------------------- the unit part


class CompanyUnit(unittest.TestCase):
    """The company module in a bare Lua state."""

    def setUp(self):
        os.environ.pop("FEVERLINK3_SESSION_DIR", None)
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        self.C = tl.load_common(self.L)
        self.K = self.L.execute(tl.LOCKSTEP_SRC)
        self.M = self.L.execute(COMPANY_SRC)
        self.Co = self.M.company
        self.co = self.Co.new()

    def lua(self, v):
        if isinstance(v, dict):
            return self.L.table_from({k: self.lua(x) for k, x in v.items()})
        if isinstance(v, list):
            return self.L.table_from([self.lua(x) for x in v])
        return v

    def ctx(self, sender, at=0, key=None, **extra):
        t = self.L.table_from({"from": sender, "at": at})
        if key:
            t["key"] = key
        for k, v in extra.items():
            t[k] = v
        return t

    def op(self, sender, **fields):
        """One registry operation by `sender`; returns (ok, effects-or-reason)."""
        r = self.Co.apply(self.co, self.ctx(sender, fields.pop("at", 0)), self.lua(fields))
        ok, fx = (r if isinstance(r, tuple) else (r, None))
        return bool(ok), (to_py(fx) if lupa.lua_type(fx) == "table" else fx)

    def reg(self):
        return to_py(self.co)

    def fresh(self):
        """A new, empty registry: the next part of a test starts from nothing."""
        self.co = self.Co.new()

    def must(self, sender, **fields):
        ok, why = self.op(sender, **fields)
        self.assertTrue(ok, "%s refused: %s" % (fields, why))
        return why

    def refused(self, sender, text, **fields):
        before = self.C.serialize(self.co)
        ok, why = self.op(sender, **fields)
        self.assertFalse(ok, "%s was accepted" % fields)
        self.assertIn(text, str(why))
        self.assertEqual(self.C.serialize(self.co), before, "a refused operation changed the registry: %s" % fields)

    def seat(self, *pairs):
        """Players say who they are (OPS.me): seat(0, "Boss", 1, "Alice")."""
        for i in range(0, len(pairs), 2):
            self.must(pairs[i], op="me", name=pairs[i + 1])

    def companies(self):
        """Boss (0) in c1 "One", bound to the savegame's player s:7; Alice (1) in c2 "Two", bound to the player entity 0:2."""
        self.seat(0, "Boss", 1, "Alice")
        self.must(0, op="found", name="One")
        self.must(1, op="found", name="Two")
        self.must(0, op="bind", company="c1", entity="s:7")
        self.must(0, op="bind", company="c2", entity="0:2")

    def test_the_games_loan_and_subsidy_scripts_run_for_every_company(self):
        # stand-ins of the game's scripts (loan.script.tl, subventions.script.tl, subvention_util.tl): they book to getPlayer()
        out = self.L.execute("""
            local Co = ...
            local util = { getPlayer = function() return 7 end }
            local booked = {}
            local loanUtil = { createAvailableLoans = function() return { { type = "Small" } } end }
            local loan = {
                update = function(_, state, dt)
                    local t = state:get(); t.months = (t.months or 0) + 1; state:set(t)
                    booked[#booked + 1] = "repay:" .. util.getPlayer()
                end,
                handleEvent = function(_, state, _, id, name, loans)
                    local t = state:get(); table.insert(t.obtainedLoans, loans[2].amount); state:set(t)
                    booked[#booked + 1] = name .. ":" .. util.getPlayer()
                end,
            }
            local subUtil = { applyBonusMalus = function(b) booked[#booked + 1] = b.type .. ":" .. util.getPlayer() end }
            local subState = { activeSubventions = {}, proposedSubventions = {
                { uid = 1, data = { upfront = { { type = "up1" } }, complete = { { type = "done1" } } } },
                { uid = 2, data = { upfront = { { type = "up2" } }, complete = { { type = "done2" } } } } } }
            local stateOf = function(t) return { get = function() return t end, set = function() end } end
            local sub = {
                update = function(_, state)
                    for _, v in ipairs(state:get().activeSubventions) do subUtil.applyBonusMalus(v.data.complete[1], true) end
                end,
                handleEvent = function(_, state, _, id, name, p)
                    local st = state:get()
                    for i, v in ipairs(st.proposedSubventions) do
                        if v.uid == p.uid then
                            table.insert(st.activeSubventions, v); table.remove(st.proposedSubventions, i)
                            subUtil.applyBonusMalus(v.data.upfront[1], true)
                            break
                        end
                    end
                    state:set(st)   -- the game's script saves its table at the end
                end,
            }
            local loaded = { [Co.LOAN_SCRIPT] = loan, [Co.LOAN_UTIL] = loanUtil, [Co.SUBSIDY_SCRIPT] = sub, [Co.SUBSIDY_UTIL] = subUtil }
            local others = {}
            local deps = { loaded = loaded, util = util, others = function() return others end }
            local done = Co.scriptCompanies(deps)
            local again = Co.scriptCompanies(deps)
            local real = { obtainedLoans = {} }
            loan.update(nil, stateOf(real), 1)        -- shared mode: the script as it is
            others = { 9 }
            loan.update(nil, stateOf(real), 1)
            loan.handleEvent(nil, stateOf(real), "", "Loan", "Obtain", { nil, { amount = 5, fl3player = 9 } })
            loan.handleEvent(nil, stateOf(real), "", "Loan", "Obtain", { nil, { amount = 3 } })
            sub.handleEvent(nil, stateOf(subState), "", "Subvention", "onAccept", { uid = 1, fl3player = 9 })
            sub.handleEvent(nil, stateOf(subState), "", "Subvention", "onAccept", { uid = 2 })
            sub.update(nil, stateOf(subState), 1)
            return { done = table.concat(done, ","), again = #again, booked = table.concat(booked, " "),
                main = real.months, mainLoans = table.concat(real.obtainedLoans, ","), theirs = real.fl3["p9"].months,
                theirLoans = table.concat(real.fl3["p9"].obtainedLoans, ","), offers = #real.fl3["p9"].availableLoans,
                tag = tostring(subState.activeSubventions[1].fl3player), untagged = tostring(subState.activeSubventions[2].fl3player),
                restored = util.getPlayer() }
        """, self.Co)
        self.assertEqual(out["done"], "loans,subsidies")
        self.assertEqual(out["again"], 0, "wrapped once")
        self.assertEqual(out["booked"], "repay:7 repay:7 repay:9 Obtain:9 Obtain:7 up1:9 up2:7 done1:9 done2:7")
        self.assertEqual((out["main"], out["mainLoans"]), (2, "3"))
        self.assertEqual((out["theirs"], out["theirLoans"], out["offers"]), (1, "5", 1))
        self.assertEqual((out["tag"], out["untagged"], out["restored"]), ("9", "nil", 7))

    def test_registry_operations_and_who_may_do_them(self):
        """me, found, join, leave, rename, remove, bind, mode, addPlayer: each by whom it may, each refusal leaving the registry as it was."""
        self.refused(1, "has not said who they are", op="found", name="Mine")
        self.seat(0, "Boss", 1, "  Alice ", 2, "Carol")
        reg = self.reg()
        self.assertEqual(reg["seats"], {0: "boss", 1: "alice", 2: "carol"}, "keyed by the name, trimmed, in lower case")
        self.assertEqual(reg["people"]["alice"], {"name": "Alice"})
        # any player founds a company, owns it and moves into it; names are unique whatever their case
        fx = self.must(1, op="found", name="  Ironworks \t", color=[0.9, 0.2, 0.2])
        self.assertEqual(fx["company"], "c1")
        c = self.reg()["companies"]["c1"]
        self.assertEqual((c["name"], c["owner"], c.get("entity")), ("Ironworks", "alice", None))
        self.assertEqual([c["color"][1], c["color"][2], c["color"][3]], [0.9, 0.2, 0.2])
        self.assertEqual(self.reg()["people"]["alice"]["company"], "c1")
        self.refused(2, "exists already", op="found", name="IRONWORKS")
        self.refused(0, "needs a name", op="found", name="   ")
        self.assertEqual(self.must(2, op="found", name="Bakery")["company"], "c2", "keys are never reused")
        # anyone joins any company; a company holds several players
        self.must(0, op="join", company="c1")
        self.refused(0, "no such company", op="join", company="c9")
        self.assertEqual({w: p.get("company") for w, p in self.reg()["people"].items()}, {"alice": "c1", "boss": "c1", "carol": "c2"})
        self.assertEqual(self.Co.companyOf(self.co, 0), "c1")
        self.must(2, op="leave")
        self.assertIsNone(self.Co.companyOf(self.co, 2))
        # the owner and the host rename and remove; a member who is not the owner does not
        self.refused(0, "exists already", op="rename", company="c2", name="ironworks")
        self.must(2, op="join", company="c1")
        self.refused(2, "host or the owner", op="rename", company="c1", name="Mine")
        self.must(1, op="rename", company="c1", name="Uno")
        self.must(0, op="rename", company="c2", name="Dos")
        self.assertEqual((self.reg()["companies"]["c1"]["name"], self.reg()["companies"]["c2"]["name"]), ("Uno", "Dos"))
        self.refused(1, "host or the owner", op="remove", company="c2")
        self.must(1, op="remove", company="c1")
        self.assertNotIn("c1", self.reg()["companies"])
        self.assertEqual([p.get("company") for p in self.reg()["people"].values()], [None, None, None], "its players are taken out")
        # the same person on another session id (a rejoin, the next session) takes their seat and their company along
        self.must(0, op="join", company="c2")
        self.must(5, op="me", name="BOSS")
        reg = self.reg()
        self.assertEqual((reg["seats"].get(0), reg["seats"][5], reg["people"]["boss"]["name"]), (None, "boss", "BOSS"))
        self.assertEqual(self.Co.companyOf(self.co, 5), "c2")
        # an operation carrying `as` seats its sender first (the window's records do)
        self.must(7, op="found", name="Mill", **{"as": "Dave"})
        self.assertEqual((self.reg()["seats"][7], self.Co.companyOf(self.co, 7)), ("dave", "c3"))
        self.fresh()
        self.assertTrue(self.Co.pristine(self.co))
        self.assertIsNone(self.Co.digest(self.co))
        self.seat(0, "Boss")
        self.assertFalse(self.Co.pristine(self.co))
        rev = self.reg()["rev"]
        self.must(0, op="me", name="Boss")       # no change: the window does not redraw
        self.assertEqual(self.reg()["rev"], rev)
        self.refused(0, "unknown company action", op="dance")
        ok, why = self.Co.apply(self.co, self.ctx(0), 5)
        self.assertFalse(ok)
        for i in range(16):
            self.must(0, op="found", name="C%d" % i)
        self.refused(0, "16 companies already", op="found", name="One more")
        self.must(0, op="remove", company="c3")
        self.assertEqual(self.must(0, op="found", name="x" * 200)["company"], "c17")
        self.assertEqual(len(self.reg()["companies"]["c17"]["name"]), 40)
        self.fresh()
        self.seat(0, "Boss")
        # Arabic, two bytes a character: 20 characters make 40 bytes. Euro signs, three bytes: 13 characters make 39.
        self.must(0, op="found", name="شركة" * 12)
        self.must(0, op="found", name="€" * 20)
        self.must(0, op="found", name="x" * 39 + "ش")           # the second byte of the last character would not fit
        names = [self.reg()["companies"]["c%d" % i]["name"] for i in (1, 2, 3)]     # raises on bytes that are not UTF-8
        self.assertEqual(names, ["شركة" * 5, "€" * 13, "x" * 39])
        self.assertTrue(all(len(n.encode("utf-8")) <= 40 for n in names))
        # the gameplay type: a company for each player only with the native capability; the host's roster is seated and each
        # player without a company gets one named after them
        self.fresh()
        self.seat(1, "Alice")
        self.must(1, op="found", name="Alice")
        self.refused(1, "only the host", op="mode", mode="separate", acting=True)
        self.refused(0, "is not available in this game", op="mode", mode="separate")
        self.refused(0, "unknown mode", op="mode", mode="chaos")
        self.must(0, op="mode", mode="separate", acting=True, players=[{"id": 0, "name": "Boss"}, {"id": 1, "name": "Alice"}, {"id": 2, "name": "Carol"}])
        reg = self.reg()
        self.assertEqual(reg["mode"], "separate")
        self.assertEqual({k: (c["name"], c["owner"]) for k, c in reg["companies"].items()}, {"c1": ("Alice", "alice"), "c2": ("Boss", "boss"), "c3": ("Carol", "carol")})
        self.must(3, op="me", name="Alice")                       # someone else called Alice: their own company, another name
        self.must(4, op="me", name="Dave")
        self.assertEqual((self.Co.companyOf(self.co, 3), self.Co.companyOf(self.co, 4)), ("c1", "c4"), "the same name is the same person")
        self.refused(4, "join another one", op="leave")
        self.refused(4, "cannot be deleted", op="remove", company="c4")
        self.must(4, op="join", company="c2")
        self.must(4, op="remove", company="c4")                    # empty now
        self.must(0, op="mode", mode="shared")
        self.assertEqual(self.reg()["mode"], "shared")
        # the host's game binds the companies to player entities: makeGameAddPlayerCmd binds the company of that name
        self.must(0, op="bind", company="c2", entity="s:7")
        self.refused(0, "another company", op="bind", company="c1", entity="s:7")
        self.refused(0, "bad entity key", op="bind", company="c1", entity="seven")
        self.refused(1, "only the host", op="bind", company="c1", entity="0:4")
        r = self.Co.addPlayer(self.co, self.ctx(0, key="0:5"), "ALICE", self.lua({"x": 0.1, "y": 0.2, "z": 0.3}))
        self.assertTrue(r[0])
        self.assertEqual(self.reg()["companies"]["c1"]["entity"], "0:5")
        r = self.Co.addPlayer(self.co, self.ctx(0, key="0:6"), "Ironworks", self.lua({"x": 0.1, "y": 0.2, "z": 0.3}))
        c = self.reg()["companies"][r[1]["company"]]
        self.assertEqual((c["name"], c["entity"]), ("Ironworks", "0:6"))
        self.assertEqual([c["color"][1], c["color"][2], c["color"][3]], [0.1, 0.2, 0.3])
        before = self.C.serialize(self.co)
        for sender, key, name, text in [(1, "1:5", "Mine", "only the host"), (0, None, "Mine", "no action key"), (0, "0:7", " ", "needs a name")]:
            ok, why = self.Co.addPlayer(self.co, self.ctx(sender, key=key), name, None)
            self.assertFalse(ok)
            self.assertIn(text, why)
        self.assertEqual(self.C.serialize(self.co), before)
        # a registry of an older save: membership by session id is dropped, the rest kept
        old = self.lua({"v": 1, "rev": 3, "mode": "shared", "serial": 1, "companies": {"c1": {"key": "c1", "name": "One", "color": [0.5, 0.5, 0.5], "owner": 0, "open": True}},
                        "members": {1: "c1"}, "limits": {}, "denied": {}, "ledger": {}, "made": {}, "nmade": 0})
        self.Co.upgrade(old)
        up = to_py(old)
        self.assertEqual((up["people"], up["seats"], up.get("members"), up["companies"]["c1"]["name"], up["companies"]["c1"].get("owner")), ({}, {}, None, "One", None))

    def resolver(self, table):
        return self.L.eval("function(t) return function(k) return t[k] end end")(self.lua(table))

    def test_actor_rewrites_the_player_per_sender_and_postings_land_on_the_companys_entity(self):
        self.companies()
        resolve = self.resolver({"s:7": 7, "0:2": 52001, "s:55": 55})
        # shared mode: every sender keeps this game's own player entity
        for sender in (0, 1, 2):
            c = self.L.table_from({"player": 7, "from": sender, "co": self.co, "resolve": resolve})
            self.Co.actor(c)
            self.assertEqual(c.player, 7)
            self.assertIsNone(c.refuse)
        self.must(0, op="mode", mode="separate", acting=True)
        got = {}
        for sender in (0, 1, 2):
            c = self.L.table_from({"player": 7, "from": sender, "co": self.co, "resolve": resolve})
            self.Co.actor(c)
            got[sender] = c.player
        self.assertEqual(got, {0: 7, 1: 52001, 2: 7}, "members act as their company; players outside any company keep the game's own player")
        # a company whose entity this game does not know: refused, never a wrong id
        c = self.L.table_from({"player": 7, "from": 1, "co": self.co, "resolve": self.resolver({"s:7": 7, "s:55": 55})})
        self.Co.actor(c)
        self.assertIn("not known on this game", c.refuse)
        # and decodeCommand honours it
        body = self.C.parse('{["fn"]="makeVehicleBuyCmd",["args"]={["n"]=3,[1]={["__ref"]="player"},[2]={["__ref"]="entity",["key"]="s:55"},[3]={}}}')
        out = self.K.decodeCommand(body, c)
        self.assertEqual(out, (None, c.refuse))
        c2 = self.L.table_from({"player": 7, "from": 1, "co": self.co, "resolve": resolve})
        self.Co.actor(c2)
        out = self.K.decodeCommand(body, c2)
        args = out[0] if isinstance(out, tuple) else out
        self.assertEqual(int(args[1]), 52001, "the {__ref=player} argument became the sender's company entity")

    def test_the_ledger_limits_permissions_payments_and_digest(self):
        co, Co = self.co, self.Co
        self.refused(1, "only the host", op="limit", player=1, amount=1000)
        self.refused(0, "bad amount", op="limit", player=1, amount=-5)
        self.refused(0, "bad period", op="limit", player=1, amount=1000, per=0)
        self.must(0, op="limit", player=1, amount=50000)
        self.assertEqual(Co.gate(co, 1, "build", 0), True)
        Co.record(co, 1, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 30000}))
        self.assertEqual(Co.gate(co, 1, "spend", 0), True)
        Co.record(co, 1, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 30000}))
        ok, why = Co.gate(co, 1, "spend", 0)
        self.assertFalse(ok)
        self.assertIn("60,000 of 50,000", why)
        self.assertTrue(allowed(Co.gate(co, 1, "bulldoze", 0)), "bulldozing is never stopped")
        self.assertTrue(allowed(Co.gate(co, 1, "other", 0)))
        self.assertTrue(allowed(Co.gate(co, 0, "spend", 0)), "a player without a limit is free")
        # money that comes back is not counted against the limit, and is kept apart
        Co.record(co, 1, 0, self.lua({"fn": "makeWorldBuildProposalCmd", "category": "bulldoze", "cost": -4000}))
        led = self.reg()["ledger"][1]
        self.assertEqual((led["refund"], led["gross"]["spend"], led["n"]["bulldoze"], led["per"]["gross"]), (4000, 60000, 1, 60000))
        # the host resets the counter, or lifts the limit
        self.must(0, op="reset", player=1)
        self.assertEqual(Co.gate(co, 1, "spend", 0), True)
        self.assertEqual(self.reg()["ledger"][1]["gross"]["spend"], 60000, "the reset clears the period, not the history")
        Co.record(co, 1, 0, self.lua({"fn": "x", "category": "build", "cost": 70000}))
        self.assertFalse(Co.gate(co, 1, "build", 0)[0])
        self.must(0, op="limit", player=1)
        self.assertTrue(allowed(Co.gate(co, 1, "build", 0)))
        self.assertNotIn(1, self.reg()["limits"])
        # a limit per period: the count restarts when the period changes, by the stamp and not by the clock of a game
        self.must(0, op="limit", player=1, amount=50000, per=1000, at=2500)
        Co.record(co, 1, 2600, self.lua({"fn": "x", "category": "build", "cost": 50000}))
        self.assertFalse(Co.gate(co, 1, "build", 2900)[0])
        self.assertTrue(allowed(Co.gate(co, 1, "build", 3000)), "the next period starts at 3000")
        Co.record(co, 1, 3100, self.lua({"fn": "x", "category": "build", "cost": 10000}))
        self.assertEqual(self.reg()["ledger"][1]["per"], {"idx": 3, "gross": 10000})
        self.fresh()
        co, Co = self.co, self.Co
        self.refused(1, "only the host", op="perm", player=1, category="build", allowed=False)
        self.refused(0, "unknown category", op="perm", player=1, category="fly", allowed=False)
        self.refused(0, "true or false", op="perm", player=1, category="build")
        self.refused(0, "bad player", op="perm", player=-1, category="build", allowed=False)
        self.must(0, op="perm", player=1, category="build", allowed=False)
        self.must(0, op="perm", player=1, category="spend", allowed=False)
        ok, why = pair(Co.gate(co, 1, "build", 0))
        self.assertFalse(ok)
        self.assertEqual(why, "not allowed to build")
        self.assertFalse(pair(Co.gate(co, 1, "spend", 0))[0])
        self.assertTrue(allowed(Co.gate(co, 1, "bulldoze", 0)))
        self.assertTrue(allowed(Co.gate(co, 1, "other", 0)))
        self.assertTrue(allowed(Co.gate(co, 2, "build", 0)), "other players are not affected")
        self.assertEqual(self.reg()["denied"], {1: {"build": True, "spend": True}})
        rev = self.reg()["rev"]
        self.must(0, op="perm", player=1, category="build", allowed=False)       # already so: nothing changes
        self.must(0, op="perm", player=2, category="build", allowed=True)        # never forbidden: nothing changes
        self.assertEqual(self.reg()["rev"], rev)
        # it shows in the page, and a limit does not undo it
        st = to_py(self.Co.status(self.co, self.lua({"self": 0, "names": {0: "Boss", 1: "Alice"}})))
        alice = [p for p in st["players"].values() if p["id"] == 1][0]
        self.assertEqual(alice["denied"], {1: "build", 2: "spend"})
        self.assertIn("not allowed to build, spend", [l for l in st["lines"].values() if l.startswith("Alice")][0])
        boss = [p for p in st["players"].values() if p["id"] == 0][0]
        self.assertNotIn("denied", boss)
        self.must(0, op="limit", player=1, amount=1000000)
        self.assertFalse(pair(Co.gate(co, 1, "build", 0))[0])
        self.must(0, op="perm", player=1, category="build", allowed=True)
        self.assertTrue(allowed(Co.gate(co, 1, "build", 0)))
        self.must(0, op="perm", player=1, category="spend", allowed=True)
        self.assertEqual(self.reg()["denied"], {}, "nothing left of the denial")
        self.fresh()
        co, Co = self.co, self.Co
        obtain = self.lua(["", "Loan", "Obtain", {}])
        repay = self.lua(["", "Loan", "Repay", {}])
        self.assertEqual(Co.nameOf("makeVehicleBuyCmd", None), "makeVehicleBuyCmd")
        self.assertEqual(Co.nameOf("makeScriptingSendEventCmd", obtain), "event Loan/Obtain")
        self.assertTrue(Co.freeOfLimit("makeScriptingSendEventCmd", repay))
        self.assertFalse(Co.freeOfLimit("makeScriptingSendEventCmd", obtain))
        self.assertFalse(Co.freeOfLimit("makeVehicleBuyCmd", repay), "only the event of that name")
        self.must(0, op="limit", player=1, amount=100)
        Co.record(co, 1, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 500}))
        self.assertFalse(pair(Co.gate(co, 1, "spend", 0, Co.freeOfLimit("makeScriptingSendEventCmd", obtain)))[0])
        self.assertTrue(allowed(Co.gate(co, 1, "spend", 0, Co.freeOfLimit("makeScriptingSendEventCmd", repay))))
        # a category the host forbade still stops it
        self.must(0, op="perm", player=1, category="spend", allowed=False)
        self.assertFalse(pair(Co.gate(co, 1, "spend", 0, True))[0])
        # the ledger names an event and books no money for it
        it = self.lua({"key": "1:3", "body": {"fn": "makeScriptingSendEventCmd", "args": ["", "Loan", "Obtain", {}]}})
        info = to_py(Co.info(it, self.lua({}), "spend", 5000))
        self.assertEqual((info["fn"], info["category"], info.get("cost")), ("event Loan/Obtain", "spend", None))
        it = self.lua({"key": "1:4", "body": {"fn": "makeVehicleBuyCmd", "args": [1, 2, {}]}})
        info = to_py(Co.info(it, self.lua({"creates": "TRANSPORT_VEHICLE"}), "spend", 5000))
        self.assertEqual((info["fn"], info["cost"], info["created"]), ("makeVehicleBuyCmd", 5000, {"key": "1:4", "kind": "TRANSPORT_VEHICLE"}))
        # the report says it in words
        Co.record(co, 1, 0, self.lua({"fn": "event Loan/Obtain", "category": "spend"}))
        Co.record(co, 1, 0, self.lua({"fn": "event GameTime/SetMode", "category": "other"}))
        st = to_py(Co.status(co, self.lua({"self": 1})))
        labels = [m["label"] for m in [p for p in st["players"].values() if p["id"] == 1][0]["made"].values()]
        self.assertEqual(sorted(labels), ["GameTime SetMode", "loans taken", "vehicles bought"])
        self.assertEqual(to_py(self.M.events), {"Companies": {"applyLevel": {"category": "other"}}})
        self.fresh()
        self.Co.record(self.co, 1, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend"}))
        self.Co.record(self.co, 1, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": float("nan")}))
        led = self.reg()["ledger"][1]
        self.assertEqual((led["n"]["spend"], led["made"]["makeVehicleBuyCmd"], led["gross"], led["refund"]), (2, 2, {}, 0))

        self.companies()
        self.must(2, op="me", name="Carol")
        self.must(2, op="join", company="c2")
        fx = self.must(1, op="pay", to=0, amount=10000, memo="the station")
        self.assertNotIn("postings", fx, "shared mode: nothing moves in the game")
        led = self.reg()["ledger"]
        self.assertEqual((led["alice"]["paid"], led["boss"]["received"]), (10000, 10000), "a seated player's ledger is kept by identity")
        self.must(0, op="mode", mode="separate", acting=True)
        fx = self.must(1, op="pay", to=0, amount=2500, memo="fee")
        self.assertEqual([(p["company"], p["amount"]) for p in fx["postings"].values()], [("c2", -2500), ("c1", 2500)])
        fx = self.must(1, op="pay", to=2, amount=100)
        self.assertNotIn("postings", fx, "inside one company the ledger is enough")
        # the host may charge anyone (a fee), nobody else may
        fx = self.must(0, op="pay", **{"from": 1, "to": 0, "amount": 700})
        self.assertEqual(fx["postings"][1]["amount"], -700)
        self.refused(1, "only the host", op="pay", **{"from": 2, "to": 0, "amount": 5})
        self.refused(1, "bad amount", op="pay", to=0, amount=0)
        self.refused(1, "bad amount", op="pay", to=0, amount=-3)
        self.refused(1, "pays themselves", op="pay", to=1, amount=5)
        self.refused(1, "bad payee", op="pay", to="x", amount=5)
        led = self.reg()["ledger"]
        self.assertEqual((led["alice"]["paid"], led["boss"]["received"]), (10000 + 2500 + 100 + 700, 10000 + 2500 + 700))
        # the window's Pay: to a company, from the sender's own; the receiving company's owner counts it as received
        fx = self.must(1, op="pay", toCompany="c1", amount=900)
        self.assertEqual([(p["company"], p["amount"]) for p in fx["postings"].values()], [("c2", -900), ("c1", 900)])
        self.assertEqual((self.reg()["ledger"]["alice"]["paid"], self.reg()["ledger"]["boss"]["received"]), (14200, 14100))
        self.refused(1, "nobody pays themselves", op="pay", toCompany="c2", amount=5)
        self.refused(1, "no such company", op="pay", toCompany="c9", amount=5)
        self.refused(0, "paid by its own players", op="pay", toCompany="c2", amount=5, **{"from": 1})
        self.must(0, op="mode", mode="shared")
        self.refused(1, "nothing to pay between companies", op="pay", toCompany="c1", amount=5)
        self.fresh()
        self.companies()
        booked = self.L.eval("{}")
        engine = self.L.eval("function(booked) return { bookJournalFor = function(e, n, kind) booked[#booked + 1] = e .. ':' .. n .. ':' .. tostring(kind) end } end")(booked)
        ctx = self.L.table_from({"co": self.co, "resolve": self.resolver({"s:7": 7, "0:2": 52001})})
        fx = self.lua({"postings": [{"company": "c2", "amount": -2500, "memo": "fee", "kind": "OTHER"}, {"company": "c1", "amount": 2500}]})
        done, failed = self.Co.runEffects(ctx, fx, engine)
        self.assertEqual(int(done), 2)
        self.assertEqual([booked[i] for i in (1, 2)], ["52001:-2500:OTHER", "7:2500:nil"], "the entry type goes along, the adapter's default is OTHER")
        # an entity this game does not know, or an engine that refuses: counted as failed, the rest still goes in
        ctx2 = self.L.table_from({"co": self.co, "resolve": self.resolver({"s:7": 7})})
        done, failed = self.Co.runEffects(ctx2, fx, engine)
        self.assertEqual(int(done), 1)
        self.assertEqual(len(failed), 1)
        self.assertIn("2,500", failed[1] if lupa.lua_type(failed) == "table" else failed[0])
        nobook = self.L.table_from({})
        done, failed = self.Co.runEffects(ctx, fx, nobook)
        self.assertEqual((int(done), len(failed)), (0, 2))
        self.fresh()
        self.must(0, op="limit", player=1, amount=5)
        self.Co.record(self.co, 1, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 30000}))
        a = self.C.hashValue(self.Co.digest(self.co))
        self.Co.record(self.co, 2, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 100}))
        b = self.C.hashValue(self.Co.digest(self.co))
        self.assertNotEqual(a, b, "another player's purchase changes the digest")
        # same actions, another measured cost: same digest
        other = self.Co.new()
        self.Co.apply(other, self.ctx(0), self.lua({"op": "limit", "player": 1, "amount": 5}))
        self.Co.record(other, 1, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 29990}))
        self.Co.record(other, 2, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 5}))
        self.assertEqual(self.C.hashValue(self.Co.digest(other)), b)
        self.Co.record(other, 2, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 5}))
        self.assertNotEqual(self.C.hashValue(self.Co.digest(other)), b, "one more action does")
        # a ledger kept by session id before the player said who they are goes into their identity's
        self.must(1, op="me", name="Alice")
        led = self.reg()["ledger"]
        self.assertNotIn(1, led)
        self.assertEqual((led["alice"]["n"]["spend"], led["alice"]["gross"]["spend"]), (1, 30000))
        self.fresh()
        self.companies()
        self.must(0, op="limit", player=1, amount=1000, per=5000)
        self.must(1, op="pay", to=0, amount=5, memo="x")
        self.Co.record(self.co, 1, 100, self.lua({"fn": "makeLineCreateCmd", "category": "build", "cost": 12, "created": {"key": "1:2", "kind": "LINE"}}))
        text = self.C.serialize(self.co)          # raises on functions, userdata, cycles
        back = self.C.parse(text)
        self.assertEqual(self.C.serialize(back), text)
        self.assertEqual(to_py(back), self.reg())
        self.fresh()
        for i in range(1, 421):
            self.Co.record(self.co, 1, i, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "created": {"key": "1:%d" % i, "kind": "TRANSPORT_VEHICLE"}}))
        made = self.reg()["made"]
        self.assertLessEqual(len(made), 400)
        self.assertIn("1:420", made)
        self.assertNotIn("1:1", made)
        self.assertEqual(self.reg()["nmade"], len(made))
        self.fresh()
        for key, by in (("1:2", 1), ("1:3", 1), ("0:4", 0)):
            self.Co.record(self.co, by, 0, self.lua({"fn": "makeLineCreateCmd", "category": "build", "created": {"key": key, "kind": "LINE"}}))
        self.Co.record(self.co, 0, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "created": {"key": "0:5", "kind": "TRANSPORT_VEHICLE"}}))
        results = {"1:2": 1200.4, "1:3": -300, "0:4": None}
        got = to_py(self.Co.earned(self.co, self.L.eval("function(t) return function(k) return t[k] end end")(self.lua({k: v for k, v in results.items() if v is not None}))))
        self.assertEqual(got, {1: 900}, "vehicles are not lines, unreadable lines count for nothing")

    def test_the_status_table_the_command_codecs_and_the_window_records(self):
        self.seat(0, "Boss", 1, "Alice", 4, "Erin")
        self.must(1, op="found", name="Ironworks")
        self.must(0, op="join", company="c1")
        self.must(4, op="found", name="Mill")
        self.must(0, op="limit", player=1, amount=50000)
        self.Co.record(self.co, 1, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 45000}))
        self.Co.record(self.co, 1, 0, self.lua({"fn": "makeLineCreateCmd", "category": "build", "cost": 3000}))
        self.Co.record(self.co, 0, 0, self.lua({"fn": "makeWorldBuildProposalCmd", "category": "build", "cost": 20000}))
        self.Co.record(self.co, 0, 0, self.lua({"fn": "makeWorldBuildProposalCmd", "category": "bulldoze", "cost": -1500}))
        self.Co.record(self.co, 5, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 10}))     # left the session
        view = self.lua({"self": 1, "names": {0: "Boss", 1: "Alice", 2: "Carol"}, "now": 0, "earned": {"alice": 800}, "hostEntityKey": "s:7",
                         "yearLength": 96000, "monthLength": 8000, "perms": {"build": True, "bulldoze": False, "spend": True, "other": True}})
        st = to_py(self.Co.status(self.co, view))
        self.assertEqual((st["yearLength"], st["monthLength"], st["perms"]["bulldoze"], st["perms"]["build"]), (96000, 8000, False, True))
        self.assertEqual((st["mode"], st["acting"], st["self"], st["isHost"], st["hostEntityKey"], st["identity"], st["company"]), ("shared", False, 1, False, "s:7", "alice", "c1"))
        self.assertEqual([p["id"] for p in st["players"].values()], [0, 1, 2, 4, 5])
        self.assertEqual([p["present"] for p in st["players"].values()], [True, True, True, False, False], "a player who left stays in the report")
        boss, alice, carol = st["players"][1], st["players"][2], st["players"][3]
        self.assertEqual((alice["name"], alice["you"], alice["company"], alice["bought"], alice["built"], alice["total"]), ("Alice", True, "c1", 1, 1, 48000))
        self.assertEqual((alice["limit"]["amount"], alice["used"], alice["remaining"], alice["blocked"]), (50000, 48000, 2000, False))
        self.assertEqual((alice["earned"], alice["net"]), (800, 800 - 48000))
        self.assertEqual([m["label"] for m in alice["made"].values()], ["lines created", "vehicles bought"])
        self.assertEqual((boss["refunded"], boss["bulldozed"], boss["total"]), (1500, 1, 20000))
        self.assertEqual((carol["name"], carol["total"], carol.get("company")), ("Carol", 0, None))
        c1, mill = st["companies"][1], st["companies"][2]
        self.assertEqual((c1["key"], c1["name"], c1["spent"], c1["spentText"], c1["bound"], c1["you"], c1["mine"], c1["mayEdit"], c1["ownerName"]),
                         ("c1", "Ironworks", 68000, "68,000", False, True, True, True, "Alice"))
        self.assertEqual([(m["name"], m["you"], m["online"]) for m in c1["members"].values()], [("Alice", True, True), ("Boss", False, True)])
        self.assertEqual([(m["name"], m["online"]) for m in mill["members"].values()], [("Erin", False)], "a seat the roster does not list is away")
        self.assertFalse(mill["mayEdit"])
        self.assertTrue(any("used 96% of the spending limit" in w for w in st["warnings"].values()), st["warnings"])
        lines = list(st["lines"].values())
        self.assertEqual(lines[1], "Alice (you): 1 built, 1 bought; spent 48,000; lines 800; limit 50,000, used 48,000")
        self.assertEqual(lines[2], "Carol: nothing yet")
        # the window's Companies tab draws this (feverlink3_hud.lua H.companyView, the words of feverlink3_text.lua)
        H = self.L.execute(tl.HUD_SRC)
        W = self.L.execute(tl.TEXT_SRC).COMPANY
        v = to_py(H.companyView(self.Co.status(self.co, view), W))
        self.assertEqual((v["intro"], v["mine"]), (W.modeShared, "Your company: Ironworks"))
        self.assertEqual([(r["title"], r["detail"], r["join"], r["leave"], r["remove"]) for r in v["rows"].values()],
                         [("Ironworks", "Players: Alice (you), Boss;  spent 68,000", False, True, True), ("Mill", "Players: Erin (away)", True, False, False)])
        # at the limit
        self.Co.record(self.co, 1, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 5000}))
        st = to_py(self.Co.status(self.co, view))
        self.assertTrue(st["players"][2]["blocked"])
        self.assertTrue(any("reached the spending limit" in w for w in st["warnings"].values()))
        # a refusal reaches the window; company for each player shows each company's money and offers no Leave
        self.refused(1, "exists already", op="found", name="mill")
        self.Co.refuse(self.co, 1, "a company called mill exists already")
        self.must(0, op="mode", mode="separate", acting=True)
        sview = self.lua({"self": 1, "names": {0: "Boss", 1: "Alice"}, "acting": True, "balances": {"c1": 1234567}})
        v = to_py(H.companyView(self.Co.status(self.co, sview), W))
        self.assertEqual((v["intro"], v["refused"]), (W.modeSeparate, "Not done: a company called mill exists already"))
        self.assertEqual([(r["detail"], r["leave"], r["remove"]) for r in v["rows"].values()],
                         [("Players: Alice (you), Boss;  money 1,234,567", False, False), ("Players: Erin (away)", False, False)])
        self.assertEqual(([r["pay"] for r in v["rows"].values()], v.get("handOver")), ([False, False], None), "no account yet: no Pay, no Give line")
        # Pay on every other company with an account; Give line only for the line selected in the game (the page's `selected`): a hint
        # without one, the companies it can go to when one's own company owns it, whose it is otherwise
        self.must(0, op="bind", company="c1", entity="s:7")
        self.must(0, op="bind", company="c2", entity="0:2")
        st = self.Co.status(self.co, sview)
        v = to_py(H.companyView(st, W))
        self.assertEqual(([r["pay"] for r in v["rows"].values()], v["handOver"], v["hint"], v.get("selected")), ([False, True], True, W.selectHint, None))
        st.selected = self.lua({"id": 301, "name": "Bus 1", "vehicles": 2, "stops": 5, "mine": True, "company": "c1"})
        v = to_py(H.companyView(st, W))
        self.assertEqual((v["selected"], v["give"], [t["key"] for t in v["targets"].values()], v.get("owner")),
                         ("Selected line: Bus 1 (2 vehicles, 5 stops)", 301, ["c2"], None))
        st.selected = self.lua({"id": 302, "name": "Tram", "vehicles": 1, "stops": 2, "mine": False, "company": "c2", "companyName": "Ironworks"})
        v = to_py(H.companyView(st, W))
        self.assertEqual((v.get("give"), v["owner"]), (None, "This line belongs to Ironworks: only its own company can give it away."))
        self.assertEqual([H.amountOf(t) for t in ("250,000", " 1 000 ", "0", "-5", "x", None)], [250000, 1000, None, None, None, None])
        # without a registry or a view the table is still whole
        st = to_py(self.Co.status(None, None))
        self.assertEqual((st["mode"], len(st["players"]), len(st["companies"])), ("shared", 0, 0))
        self.fresh()
        self.assertEqual([self.Co.money(n) for n in (0, 5, 999, 1000, 1234567, -45000.4, 3e9)], ["0", "5", "999", "1,000", "1,234,567", "-45,000", "3,000,000,000"])

    # ---- command specs
        self.fresh()
        L = self.L
        L.execute("api = { type = { Vec3f = { new = function(x, y, z) return { x = x, y = y, z = z } end } } }")
        self.K.registerCommands(self.M.commands)
        keyOf = L.eval("function(e) if e == 50001 then return '0:2' end return 's:' .. e end")
        ctx = L.table_from({"plain": self.C.plain, "keyOf": keyOf})
        # makeGameAddPlayerCmd: the colour is engine userdata; it travels as numbers and is rebuilt on every game
        args = L.eval("{ n = 2, ' Ironworks ', api.type.Vec3f.new(0.25, 0.5, 2) }")
        body, why = pair(self.K.encodeCommand("makeGameAddPlayerCmd", args, ctx))
        self.assertEqual(self.C.serialize(body), '{["args"]={[1]="Ironworks",[2]={[1]=0.25,[2]=0.5,[3]=1},["n"]=2},["fn"]="makeGameAddPlayerCmd"}')
        dctx = L.table_from({"player": 7, "resolve": L.eval("function(k) end")})
        out = pair(self.K.decodeCommand(body, dctx))[0]
        self.assertEqual((out[1], out.n, out[2].x, out[2].y, out[2].z), ("Ironworks", 2, 0.25, 0.5, 1))
        res = pair(self.K.encodeCommand("makeGameAddPlayerCmd", L.eval("{ n = 2, '   ', nil }"), ctx))
        self.assertEqual(res[0], None)
        self.assertEqual(self.K.categoryOf("makeGameAddPlayerCmd"), "other")
        self.assertEqual(self.M.commands.makeGameAddPlayerCmd.creates, "PLAYER")
        # makeEntitySetPlayerCmd: both arguments are entities (keys), -1 is the owner "nobody"
        body, why = pair(self.K.encodeCommand("makeEntitySetPlayerCmd", L.eval("{ n = 2, 77, 50001 }"), ctx))
        self.assertEqual(self.C.serialize(body), '{["args"]={[1]={["__ref"]="entity",["key"]="s:77"},[2]={["__ref"]="entity",["key"]="0:2"},["n"]=2},["fn"]="makeEntitySetPlayerCmd"}')
        dctx = L.table_from({"player": 7, "resolve": L.eval("function(k) if k == 's:77' then return 77 end if k == '0:2' then return 61001 end end")})
        out = pair(self.K.decodeCommand(body, dctx))[0]
        self.assertEqual((out[1], out[2]), (77, 61001))
        body, why = pair(self.K.encodeCommand("makeEntitySetPlayerCmd", L.eval("{ n = 2, 77, -1 }"), ctx))
        out = pair(self.K.decodeCommand(body, dctx))[0]
        self.assertEqual((out[1], out[2]), (77, -1))
        self.assertIsNone(pair(self.K.encodeCommand("makeEntitySetPlayerCmd", L.eval("{ n = 2, 'x', 7 }"), ctx))[0])
        self.assertIsNone(pair(self.K.encodeCommand("makeEntitySetPlayerCmd", L.eval("{ n = 2, 77, 'x' }"), ctx))[0])
        known = pair(self.K.encodeCommand("makeEntitySetPlayerCmd", L.eval("{ n = 2, 77, 50001 }"), ctx))[0]
        out = pair(self.K.decodeCommand(known, L.table_from({"player": 7, "resolve": L.eval("function(k) end")})))
        self.assertIsNone(out[0], "an owner this game does not know refuses the action")
        # the registry operation is a table with an op; anything else never leaves the originator
        spec = self.M.commands.feverlink3Company
        self.assertTrue(spec.virtual)
        body, why = pair(self.K.encodeCommand("feverlink3Company", L.eval("{ n = 1, { op = 'limit', player = 1, amount = 5 } }"), ctx))
        self.assertEqual(self.C.serialize(body), '{["args"]={[1]={["amount"]=5,["op"]="limit",["player"]=1},["n"]=1},["fn"]="feverlink3Company"}')
        self.assertIsNone(pair(self.K.encodeCommand("feverlink3Company", L.eval("{ n = 1, 'limit' }"), ctx))[0])
        self.assertIsNone(pair(self.K.encodeCommand("feverlink3Company", L.eval("{ n = 1, { op = 'x', f = print } }"), ctx))[0])
        self.fresh()
        ctx = self.ctx(1, key="1:2")
        ctx["co"] = self.co
        ok, why = pair(self.M.commands.makeGameAddPlayerCmd.apply(ctx, self.lua({"key": "1:2"}), self.lua({"n": 2, 1: "Mine"})))
        self.assertFalse(ok)
        self.assertIn("only the host", why)
        ok, why = pair(self.M.commands.makeEntitySetPlayerCmd.apply(ctx, self.lua({}), self.lua({})))
        self.assertFalse(ok)
        ctx0 = self.ctx(0, key="0:2")
        ctx0["co"] = self.co
        ok, why = pair(self.M.commands.makeEntitySetPlayerCmd.apply(ctx0, self.lua({}), self.lua({})))
        self.assertTrue(ok)
        # separate mode: a company hands over what it owns there (Give to company), to another company or to nobody; nothing else
        self.companies()
        self.must(0, op="mode", mode="separate", acting=True)
        owners = self.lua({100: 52001, 101: 7})
        hand = lambda sender, entity, to, player=52001: pair(self.M.commands.makeEntitySetPlayerCmd.apply(
            self.ctx(sender, co=self.co, player=player, resolve=self.resolver({"s:7": 7, "0:2": 52001}), ownerOf=L.eval("function(t) return function(e) return t[e] end end")(owners)),
            self.lua({}), self.lua({"n": 2, 1: entity, 2: to})))[0]
        self.assertEqual([bool(hand(1, 100, 7)), bool(hand(1, 100, -1)), bool(hand(1, 101, 52001)), bool(hand(1, 100, 99)), bool(hand(1, 100, 7, player=7))],
                         [True, True, False, False, False], "own -> a company, own -> nobody; not another's, not to a stranger, not as someone else")
        # what Give line moves: the base game's recipe, only what the giver owns; a station group another line of the giver uses goes to nobody
        eng = L.eval("""{ ownerOf = function(e) return ({ [1] = 5, [2] = 5, [3] = 8, [10] = 5, [11] = 5, [12] = 5, [20] = 5, [21] = 5, [22] = 5, [30] = 8 })[e] end,
            lineVehicles = function(l) return ({ [1] = { 2, 3 } })[l] or {} end,
            lineGroups = function(l) return ({ [1] = { 10, 20, 30 }, [40] = { 20 } })[l] or {} end,
            groupStations = function(g) return ({ [10] = { 11 }, [20] = { 21 } })[g] or {} end,
            stationConstruction = function(s) return ({ [11] = 12, [21] = 22 })[s] or -1 end,
            linesOf = function(p) return { 1, 40 } end }""")
        plan = [(s["entity"], s["to"]) for s in to_py(self.Co.transferPlan(1, 5, 6, eng)).values()]
        self.assertEqual(plan, [(2, 6), (1, 6), (10, 6), (11, 6), (12, 6), (20, -1), (21, -1), (22, -1)])
        self.fresh()
        api = self.L.eval("""{ cmd = { makeGameAddPlayerCmd = function(n, c) return { add = n, r = c.x, g = c.y, b = c.z } end,
                                       feverlink3Company = function(p) return { company = p.op } end },
                               type = { Vec3f = { new = function(x, y, z) return { x = x, y = y, z = z } end } } }""")
        self.assertIsNone(self.Co.windowCommand(self.lua({"op": "add", "name": "Bakery"}), api), "player entities are the host game's business")
        for op in self.Co.OPS.values():
            self.assertEqual(self.Co.windowCommand(self.lua({"op": op}), api).company, op)
        self.assertIsNone(self.Co.windowCommand(self.lua({"op": "format"}), api))
        self.assertIsNone(self.Co.windowCommand("add", api))

    def test_engine_adapters(self):
        L = self.L
        calls = L.eval("{}")
        L.execute("""
            REAL = function() return 7 end
            api = {
              engine = { entityExists = function(e) return e == 5 or e == 6 end,
                         getComponent = function(e, t) if t == 7 and e == 5 then return { stops = {} } end end,
                         util = { getPlayer = REAL, finance = {
                getPlayersBalance = function(e) if e == 99 then return nil end return e * 1000 end,
                calculateBalance = function(list, from, to, flag) return list[1] + from + to end } } },
              util = { getDefaultYearDuration = function() return 96000 end, getDefaultMonthDuration = function() return 8000 end },
              type = { ComponentType = { LINE = 7 }, JournalEntry = { new = function() return { category = {} } end, Type = { OTHER = 5 } } },
              cmd = { makeJournalBookAssetCmd = function(e, entry) return { to = e, amount = entry.amount, time = entry.time, kind = entry.category.type } end,
                      sendCommand = function(c) NATIVE_SENT = c end },
            }
        """)
        engine = L.eval("""{ player = function() return api.engine.util.getPlayer() end,
                             account = function() return api.engine.util.getPlayer() * 10, 3 end,
                             bookJournal = function(n) BOOKED_ON = api.engine.util.getPlayer() end }""")
        sent = L.eval("{}")
        deps = L.eval("function(sent) return { sender = function() return function(c) sent[#sent + 1] = c end end } end")(sent)
        self.Co.extendEngine(engine, deps)
        self.assertEqual(engine.balanceOf(7), 7000)
        self.assertIsNone(engine.balanceOf(99), "nil is infinite money")
        self.assertEqual(engine.lineResult(5, 10, 20), 35)
        # a deleted line (its entity gone, or its id reused by something that is no line) is not read: nil, no balance call
        self.assertIsNone(engine.lineResult(4, 10, 20))
        self.assertIsNone(engine.lineResult(6, 10, 20))
        self.assertEqual((engine.yearLength(), engine.monthLength()), (96000, 8000))
        engine.bookJournalFor(52001, -2500)
        self.assertEqual((sent[1].to, sent[1].amount, sent[1].time, sent[1].kind), (52001, -2500, -1, 5))
        self.assertIsNone(L.eval("NATIVE_SENT"), "the booking uses the original sender, not the wrapped one")
        # the local company: getPlayer says so, the canonical player stays the game's own, and nil puts it back
        self.assertEqual(engine.canonicalPlayer(), 7)
        self.assertEqual(pair(engine.account()), (70, 3))
        self.assertTrue(engine.setLocalCompany(52001))
        self.assertEqual(L.eval("api.engine.util.getPlayer()"), 52001)
        self.assertEqual(engine.canonicalPlayer(), 7)
        self.assertEqual(pair(engine.account()), (70, 3), "the money the checksum compares is the savegame's player's")
        engine.bookJournal(5)
        self.assertEqual(L.eval("BOOKED_ON"), 7)
        self.assertEqual(L.eval("api.engine.util.getPlayer()"), 52001, "and the windows' player is put back")
        # even when the money code fails: the error goes on, the windows' player is put back first
        failing = L.eval("{ account = function() error('boom') end }")
        self.Co.extendEngine(failing, None)
        failing.localCompanyActive = True
        with self.assertRaises(lupa.LuaError):
            failing.account()
        self.assertEqual(L.eval("api.engine.util.getPlayer()"), 52001)
        self.assertTrue(engine.setLocalCompany(None))
        self.assertEqual(L.eval("api.engine.util.getPlayer()"), 7)
        self.assertEqual(engine.canonicalPlayer(), 7)
        # a savegame reload runs the mod again in the same Lua state: the override of the previous run is not taken for the
        # game's own function, and the first thing the new run does is put the game's own back
        self.assertTrue(engine.setLocalCompany(52001))
        again = L.eval("{ player = function() return api.engine.util.getPlayer() end }")
        self.Co.extendEngine(again, None)
        self.assertEqual(L.eval("api.engine.util.getPlayer()"), 52001, "still overridden until the new run says otherwise")
        self.assertTrue(again.setLocalCompany(None))
        self.assertEqual(L.eval("api.engine.util.getPlayer()"), 7)
        self.assertEqual(again.canonicalPlayer(), 7)
        self.assertTrue(engine.setLocalCompany(None))
        # an engine that already has an accessor keeps it
        mine = L.eval("{ balanceOf = function() return 1 end }")
        self.Co.extendEngine(mine, None)
        self.assertEqual(mine.balanceOf(5), 1)
        # an api that refuses assignment: reported, not raised
        L.execute("api.engine.util = setmetatable({}, { __newindex = function() error('read-only') end, __index = { getPlayer = REAL } })")
        frozen = L.eval("{}")
        self.Co.extendEngine(frozen, None)
        self.assertFalse(frozen.setLocalCompany(52001))


# ---------------------------------------------------------------- the mocked games

# Added to the harness' engine mock: several player entities, an account for each, ownership, prices. Player 7 is the
# savegame's; makeGameAddPlayerCmd adds more, allocated like any entity of the session (ids differ between the games).
CO_MOCK = r'''
ACCOUNTS = { [7] = ACCOUNT }
OWNER = {}
PRICE = { vehicle = 30000, line = 5000 }
START_BALANCE = 0
INFINITE = false
LINE_RESULT = {}
JOURNALS = {}
EVENTS = {}      -- "<id>/<name>@<time>" of every event sent to the game scripts (not the mod's own)
WORLD[7].PLAYER = { headquarters = -1 }

local function allocate(comps)
  GAME.nextEntity = GAME.nextEntity + 1
  WORLD[GAME.nextEntity] = comps
  return GAME.nextEntity
end

local function applied(name, a, entity)
  GAME.applied[#GAME.applied + 1] = { name = name, time = GAME.time, args = a, entity = entity }
end

local baseExecute = EXECUTE
function EXECUTE(cmd, cb)
  local name, a = cmd.__cmd, cmd.args
  if name == "makeScriptingSendEventCmd" and a[2] ~= "feverlink3" then EVENTS[#EVENTS + 1] = a[2] .. "/" .. a[3] .. "@" .. GAME.time end
  if name == "makeGameAddPlayerCmd" then
    local e = allocate({ PLAYER = { headquarters = -1 }, NAME = a[1], COLOR = a[2] })
    ACCOUNTS[e] = { balance = START_BALANCE, loan = 0 }
    applied(name, a, e)
    if cb then cb({ resultEntity = e }, true, { { e, 0 } }) end
    return
  end
  if name == "makeEntitySetPlayerCmd" then
    assert(WORLD[a[1]], "no entity " .. tostring(a[1]))
    if a[2] == -1 then OWNER[a[1]] = nil else
      assert(WORLD[a[2]] and WORLD[a[2]].PLAYER, "no player " .. tostring(a[2]))
      OWNER[a[1]] = a[2]
    end
    applied(name, a, nil)
    if cb then cb({}, true, {}) end
    return
  end
  if name == "makeEntitySetNameCmd" then
    assert(WORLD[a[1]], "no entity " .. tostring(a[1]))
    WORLD[a[1]].NAME = a[2]
    applied(name, a, nil)
    if cb then cb({}, true, {}) end
    return
  end
  if name == "makeJournalBookAssetCmd" then
    local acc = ACCOUNTS[a[1]]
    assert(acc, "no account for " .. tostring(a[1]))
    acc.balance = acc.balance + a[2].amount
    JOURNALS[a[1]] = JOURNALS[a[1]] or {}
    JOURNALS[a[1]][#JOURNALS[a[1]] + 1] = a[2].amount
    if a[1] == 7 then JOURNAL[#JOURNAL + 1] = a[2].amount end
    if cb then cb({}, true, {}) end
    return
  end
  if name == "makeVehicleBuyCmd" then
    assert(ACCOUNTS[a[1]], "player entity expected, got " .. tostring(a[1]))
    assert(WORLD[a[2]] and WORLD[a[2]].VEHICLE_DEPOT, "no depot " .. tostring(a[2]))
    GAME.nextEntity = GAME.nextEntity + 1
    local created = GAME.nextEntity
    if GAME.inUpdate then GAME.deferred[#GAME.deferred + 1] = created else WORLD[created] = { TRANSPORT_VEHICLE = {} } end
    OWNER[created] = a[1]
    ACCOUNTS[a[1]].balance = ACCOUNTS[a[1]].balance - PRICE.vehicle
    applied(name, a, created)
    if cb then cb({ resultVehicleEntity = created, resultEntity = created }, true, { { created, 0 } }) end
    return
  end
  if name == "makeLineCreateCmd" then
    assert(ACCOUNTS[a[3]], "player entity expected, got " .. tostring(a[3]))
    local created = allocate({ LINE = { stops = {} } })
    OWNER[created] = a[3]
    ACCOUNTS[a[3]].balance = ACCOUNTS[a[3]].balance - PRICE.line
    applied(name, a, created)
    if cb then cb({ resultVehicleEntity = created, resultEntity = created }, true, { { created, 0 } }) end
    return
  end
  return baseExecute(cmd, cb)
end

local baseMakeApi = MAKE_API
function MAKE_API()
  local api = baseMakeApi()
  local function mk(name) return function(...) return { __cmd = name, args = { n = select("#", ...), ... } } end end
  api.cmd.makeGameAddPlayerCmd = mk("makeGameAddPlayerCmd")
  api.cmd.makeEntitySetPlayerCmd = mk("makeEntitySetPlayerCmd")
  api.cmd.makeEntitySetNameCmd = mk("makeEntitySetNameCmd")
  api.type.Vec3f = { new = function(x, y, z) return { x = x, y = y, z = z } end }
  api.type.ComponentType.PLAYER_OWNED = 9
  local baseGet = api.engine.getComponent
  api.engine.getComponent = function(e, t)
    if t == api.type.ComponentType.ACCOUNT and ACCOUNTS[e] then return ACCOUNTS[e] end
    if t == 9 then return OWNER[e] and { player = OWNER[e] } or nil end
    return baseGet(e, t)
  end
  api.engine.util.finance = {
    getPlayersBalance = function(e) if INFINITE then return nil end local acc = ACCOUNTS[e]; return acc and acc.balance end,
    calculateBalance = function(list, from, to, flag) return LINE_RESULT[list[1]] or 0 end,
  }
  api.util = { getDefaultYearDuration = function() return 12000 end }
  api.engine.system.transportVehicleSystem = { getLineVehicles = function(l) return {} end }
  return api
end

-- the window's side: a record in ui_out.log
function UI_COMPANY(op)
  local e = UI_ENV_NEW()
  UI_OUTBOX = UI_OUTBOX or e.Hud.newOutbox(e.C)
  return e.C.writerSend(UI_OUTBOX, "company", op)
end
'''


class CoGame(tl.Game):
    """A mocked game with the company mock added between the engine mock and the mod harness."""

    def __init__(self, label, player_id, tmp, first_entity):
        self.label, self.id = label, player_id
        self.dir = os.path.join(tmp, label)
        os.makedirs(self.dir)
        self.to_game = os.path.join(self.dir, "to_game.log")
        self.from_game = os.path.join(self.dir, "from_game.log")
        self.use_env()
        self.L = lupa.LuaRuntime(unpack_returned_tuples=True)
        tl.fastio.install(self.L)
        g = self.L.globals()
        g.COMMON_SRC, g.LOCKSTEP_SRC, g.BRIDGE_SRC, g.HUD_SRC = tl.COMMON_SRC, tl.LOCKSTEP_SRC, tl.BRIDGE_SRC, tl.HUD_SRC
        g.LINK_SRC, g.LOG_SRC, g.TEXT_SRC = tl.LINK_SRC, tl.LOG_SRC, tl.TEXT_SRC
        cmds = self.L.table()
        for stem, src in tl.CMDS_SRC.items():
            cmds[stem] = src
        g.CMDS_SRC = cmds
        self.L.execute(tl.MOCK % dict(t0=tl.T0, first=first_entity))
        self.L.execute(CO_MOCK)
        self.L.execute(tl.HARNESS)

    def registry(self):
        """The registry in the simulation state (what the savegame carries), as Python."""
        return to_py(self.L.eval("STATE:get().co"))

    def status(self):
        text = tl.read_text(os.path.join(self.dir, "companies.txt"))
        return to_py(self.parse(text)) if text else None

    def parse(self, text):
        self.use_env()
        return self.L.eval("(function(s) local f = load('return ' .. s, '=x', 't', {}); return f() end)")(text)

    def ev(self, expr):
        return self.L.eval(expr)

    def ui_company(self, **op):
        self.use_env()
        return bool(self.L.eval("UI_COMPANY(%s)" % lit(op)))


class CompanyGameTest(unittest.TestCase):
    tick = tl.LockstepTest.tick
    start = tl.LockstepTest.start
    log_has = tl.LockstepTest.log_has

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="feverlink3-")
        # ids differ between the games, and stay within the gap the scan for new player entities allows (entitiesScan: a real world's ids
        # are dense, the engine cannot loop over PLAYER)
        self.A = CoGame("host", 0, self.tmp, 2000)
        self.B = CoGame("client", 1, self.tmp, 3000)
        self.relay = tl.Relay([self.A, self.B])

    def tearDown(self):
        if os.environ.get("FEVERLINK3_TEST_VERBOSE"):
            print("\n--- host mod.log ---\n" + self.A.log() + "\n--- client mod.log ---\n" + self.B.log())
        shutil.rmtree(self.tmp, ignore_errors=True)

    # ---- scenario helpers

    def op(self, game, ticks=15, **fields):
        """A registry operation issued from `game`'s interface, as the company page does (it says who sends it: `as`, the
        player's name); returns its callback result."""
        fields.setdefault("as", game.label)
        game.send_gui("LAST_OP = nil; api.cmd.sendCommand(api.cmd.feverlink3Company(%s), function(d, s) LAST_OP = s end)" % lit(fields))
        self.tick(ticks)
        return game.ev("LAST_OP")

    def buy(self, game, ticks=15):
        game.send_gui("LAST_BUY = nil; api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(api.engine.util.getPlayer(), 55, { vehicles = {} }), function(d, s) LAST_BUY = s end)")
        self.tick(ticks)
        return game.ev("LAST_BUY")

    def line(self, game, ticks=15):
        game.send_gui('LAST_LINE = nil; api.cmd.sendCommand(api.cmd.makeLineCreateCmd("L", { x = 0, y = 0, z = 0 }, api.engine.util.getPlayer(), { stops = {} }), function(d, s) LAST_LINE = s end)')
        self.tick(ticks)
        return game.ev("LAST_LINE")

    def add_company(self, name="Ironworks", ticks=15):
        self.A.send_gui('api.cmd.sendCommand(api.cmd.makeGameAddPlayerCmd(%s, api.type.Vec3f.new(0.9, 0.2, 0.2)))' % tl.lua_str(name))
        self.tick(ticks)

    def both(self, expr):
        return self.A.ev(expr), self.B.ev(expr)

    def assertSameRegistry(self):
        self.assertEqual(self.A.registry(), self.B.registry(), "the two games' registries differ")


    def fresh_games(self):
        """Two new games and a new relay: the next case of a table-driven test starts from the savegame."""
        self.tearDown()
        self.setUp()

    def forge(self, body, sender=1, seq=9, steps=5):
        """An action of `sender` the relay hands to both games as it is (the originator's own copy is not relayed back in a
        session; both get it here for the test)."""
        payload = self.relay.ser({"stamp": self.A.time() + steps * STEP, "seq": seq, "body": self.relay.C.serialize(self.relay.C.parse(body)),
                                  "from": sender})
        self.relay.send(self.A, "action", sender, payload)
        self.relay.send(self.B, "action", sender, payload)
        self.tick(20)

    def shipped(self, game):
        return len([l for l in game.lines(game.from_game) if l.split(TAB)[0] == "action"])

    # ---- shared company: the ledger and its checksum part

    def test_the_ledger_and_its_checksum_part_agree_on_both_games(self):
        # the real game: a command sent in the simulation update runs after the update returns, so the account read right after the send
        # has not moved (friend test 2026-10-16: 82 builds and 15 vehicles bought, every ledger said "spent 0")
        later = ("SEND_LATER = {}; local BASE = EXECUTE; "
                 "function EXECUTE(cmd, cb) if GAME.inUpdate and cmd.__cmd ~= 'makeScriptingSendEventCmd' then SEND_LATER[#SEND_LATER + 1] = { cmd, cb }; return end; return BASE(cmd, cb) end; "
                 "local U0 = SIM.update; SIM.update = function(...) U0(...); local l = SEND_LATER; SEND_LATER = {}; for _, x in ipairs(l) do BASE(x[1], x[2]) end end")
        self.A.L.execute(later)
        self.B.L.execute(later)
        self.start()
        self.tick(10)
        self.relay.pump()
        self.relay.checksum_request(1, self.relay.future_stamp(), False)
        self.tick(20)
        self.assertEqual(set(self.relay.reports[1][0][1]) - {"stations", "edges", "nodeConfigs", "vehicles", "names", "ids"}, {"time", "balance", "loan", "constructions", "lines", "townBuildings"}, "pristine: no part")
        self.assertTrue(self.buy(self.B))
        self.assertTrue(self.line(self.B))
        self.assertTrue(self.buy(self.A))
        self.assertEqual(self.A.applied(), self.B.applied())
        self.assertSameRegistry()
        reg = self.A.registry()
        self.assertEqual(reg["mode"], "shared")
        b, a = reg["ledger"][1], reg["ledger"][0]
        self.assertEqual((b["n"], b["gross"]), ({"spend": 1, "build": 1}, {"spend": 30000, "build": 5000}))
        self.assertEqual(b["made"], {"makeVehicleBuyCmd": 1, "makeLineCreateCmd": 1})
        self.assertEqual((a["n"], a["gross"]), ({"spend": 1}, {"spend": 30000}))
        self.assertEqual({k: (m["kind"], m["by"]) for k, m in reg["made"].items()},
                         {"1:1": ("TRANSPORT_VEHICLE", 1), "1:2": ("LINE", 1), "0:1": ("TRANSPORT_VEHICLE", 0)})
        # shared mode changes nothing about who pays: one account
        self.assertEqual(self.both("ACCOUNT.balance"), (1000000 - 65000, 1000000 - 65000))
        self.assertEqual(self.both("#GAME.applied"), (3, 3))
        self.assertFalse(self.log_has("company layer failed"))
        self.assertFalse(self.log_has("company cost booking failed"))
        self.relay.checksum_request(2, self.relay.future_stamp(), False)
        self.tick(20)
        pa, pb = self.relay.reports[2][0][1], self.relay.reports[2][1][1]
        self.assertIn("companies", pa)
        self.assertEqual(pa["companies"], pb["companies"])
        # a cost the engine cannot tell (infinite money) still counts the action, without money
        self.A.L.execute("INFINITE = true")
        self.B.L.execute("INFINITE = true")
        self.assertTrue(self.buy(self.B))
        self.assertSameRegistry()
        self.assertEqual(self.A.registry()["ledger"][1]["n"]["spend"], 2)
        self.assertEqual(self.A.registry()["ledger"][1]["gross"], {"spend": 30000, "build": 5000})
        # in shared mode a payment is only a ledger entry
        balances = self.both("ACCOUNT.balance")
        self.assertTrue(self.op(self.B, op="pay", to=0, amount=12000))
        self.assertEqual(self.both("ACCOUNT.balance"), balances)
        self.assertEqual(self.both("#JOURNAL"), (0, 0))
        self.assertEqual(self.A.registry()["ledger"]["client"]["paid"], 12000)
        self.assertSameRegistry()
        # a registry that differs on one game is named
        self.B.L.execute("local st = STATE:get(); st.co.limits[3] = { amount = 5 }; STATE:set(st)")
        self.relay.checksum_request(3, self.relay.future_stamp(), False)
        self.tick(20)
        pa, pb = self.relay.reports[3][0][1], self.relay.reports[3][1][1]
        self.assertEqual({k for k in pa if pa[k] != pb[k]}, {"companies"})

    # ---- limits and permissions

    BUY_BODY = '{["fn"]="makeVehicleBuyCmd",["args"]={["n"]=3,[1]={["__ref"]="player"},[2]={["__ref"]="entity",["key"]="s:55"},[3]={}}}'

    def test_limits_stop_spending_at_the_originator_and_every_game_refuses_a_forged_action(self):
        self.start()
        self.tick(10)
        # only the host sets limits and the gameplay type; every game counts the refusal alike
        self.assertFalse(self.op(self.B, op="limit", player=1, amount=999999999))
        self.assertFalse(self.op(self.B, op="mode", mode="shared"))
        self.assertSameRegistry()
        reg = self.A.registry()
        self.assertEqual((reg["limits"], reg["companies"]), ({}, {}))
        self.assertEqual(reg["ledger"]["client"]["refused"], 2)
        self.assertIn("only the host", reg["ledger"]["client"]["last"])
        # issued in the same tick, so stamped alike: the host's operation (sender 0) is applied before the client's purchase
        self.A.send_gui('api.cmd.sendCommand(api.cmd.feverlink3Company({ op = "limit", player = 1, amount = 1 }))')
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(api.engine.util.getPlayer(), 55, { vehicles = {} }), function(d, s) SAME_BUY = s end)")
        self.tick(20)
        self.assertSameRegistry()
        reg = self.A.registry()
        self.assertEqual(reg["limits"], {1: {"amount": 1}})
        self.assertEqual(reg["ledger"]["client"]["gross"], {"spend": 30000}, "nothing was spent yet when the purchase was checked")
        self.assertEqual(self.A.applied(), self.B.applied())
        self.assertFalse(self.buy(self.B), "and now the limit holds")
        # a forged action beyond the limit is refused by every game alike
        before, refused = len(self.A.applied()), self.A.registry()["ledger"]["client"]["refused"]
        self.forge(self.BUY_BODY)
        self.assertEqual((len(self.A.applied()), len(self.B.applied())), (before, before), "neither game applied it")
        self.assertSameRegistry()
        led = self.A.registry()["ledger"]["client"]
        self.assertEqual(led["refused"], refused + 1, "each game refused it once, and the registry (identical on both) counts it once")
        self.assertIn("spending limit reached", led["last"])
        # a loan is a `spend` event: over the limit it is stopped where it is issued; repaying it is not
        self.B.send_gui('LOAN = nil; api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Obtain", { { type = "Small" }, { amount = 1000 } }), function(d, s) LOAN = s end)')
        self.tick(15)
        self.assertEqual(self.B.ev("LOAN"), False)
        self.assertEqual(self.both("#EVENTS"), (0, 0))
        self.B.send_gui('REPAY = nil; api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Loan", "Repay", { { type = "Small" }, { amount = 1000 } }), function(d, s) REPAY = s end)')
        self.tick(15)
        self.assertEqual(self.B.ev("REPAY"), True)
        # the company page's level event is replicated; a notification is nobody's business but this game's
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Companies", "applyLevel", { level = 3 }))')
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeScriptingSendEventCmd("", "Notifications", "add", { type = "x" }))')
        self.tick(15)

        def events(game):
            joined = game.ev('table.concat(EVENTS, ",")')
            return joined.split(",") if joined else []

        host, client = events(self.A), events(self.B)
        self.assertEqual([e.split("@")[0] for e in host], ["Loan/Repay", "Companies/applyLevel"])
        self.assertEqual(host, [e for e in client if not e.startswith("Notifications/")], "both games' scripts got them at the same times")
        self.assertTrue(any(e.startswith("Notifications/add@") for e in client), "the client's own game got its notification")
        self.assertSameRegistry()
        led = self.A.registry()["ledger"]["client"]
        self.assertEqual(led["made"], {"makeVehicleBuyCmd": 1, "event Loan/Repay": 1, "event Companies/applyLevel": 1})
        self.assertEqual((led["n"], led["gross"]), ({"spend": 2, "other": 1}, {"spend": 30000}), "no money was booked for an event")
        labels = sorted(m["label"] for m in row(self.B.status(), 1)["made"].values())
        self.assertEqual(labels, ["company levels applied", "loans repaid", "vehicles bought"])

    def limit_set_and_lifted(self):
        self.assertTrue(self.op(self.A, op="limit", player=1, amount=50000))
        self.assertEqual(self.A.registry()["limits"], {1: {"amount": 50000}})
        self.assertSameRegistry()
        self.assertTrue(self.buy(self.B))          # 0 of 50 000 used
        self.assertTrue(self.buy(self.B))          # 30 000 used: one more is allowed, it may overshoot
        self.assertEqual(len(self.A.applied()), 2)
        shipped = self.shipped(self.B)
        self.assertFalse(self.buy(self.B), "60 000 of 50 000: the window hears about it at once")
        self.assertEqual(self.shipped(self.B), shipped, "nothing was stamped or shipped")
        self.assertEqual(len(self.A.applied()), 2)
        self.assertTrue(any("spending limit reached (60,000 of 50,000)" in why for (_, _, why) in self.relay.refused), self.relay.refused)
        self.assertTrue(self.line(self.A), "the host has no limit")
        self.assertFalse(self.line(self.B), "building counts against the same limit")
        self.assertTrue(self.op(self.A, op="limit", player=1))      # the host lifts it
        self.assertTrue(self.buy(self.B))
        self.assertSameRegistry()
        self.assertEqual(self.A.registry()["ledger"]["client"]["gross"]["spend"], 90000)

    def limit_per_period(self):
        self.assertTrue(self.op(self.A, op="limit", player=1, amount=10000, per=1000000))
        self.assertTrue(self.buy(self.B))
        self.assertFalse(self.buy(self.B), "30 000 of 10 000 in this period")
        self.assertEqual(self.A.registry()["ledger"]["client"]["per"], {"idx": 0, "gross": 30000})
        self.assertEqual(row(self.A.status(), 1)["limit"], {"amount": 10000, "per": 1000000})
        for game in (self.A, self.B):          # a million game-time units later
            game.L.execute("GAME.time = GAME.time + 1000000")
        self.tick(10)
        self.assertTrue(self.buy(self.B), "a new period, a fresh count")
        self.assertEqual(self.A.registry()["ledger"]["client"]["per"]["idx"], 1)
        self.assertEqual(self.A.registry()["ledger"]["client"]["gross"]["spend"], 60000, "the history is kept")
        self.assertSameRegistry()

    def forbidden_category(self):
        self.assertTrue(self.op(self.A, op="perm", player=1, category="spend", allowed=False))
        self.assertSameRegistry()
        shipped = self.shipped(self.B)
        self.assertFalse(self.buy(self.B), "buying is forbidden: the window hears about it at once")
        self.assertEqual(self.shipped(self.B), shipped)
        self.assertTrue(any("not allowed to spend" in why for (_, _, why) in self.relay.refused), self.relay.refused)
        self.assertTrue(self.line(self.B), "building is not")
        self.forge(self.BUY_BODY)              # a client that sends it anyway is refused by every game
        self.assertEqual(self.both("#GAME.applied"), (1, 1), "only the line was built")
        self.assertSameRegistry()
        led = self.A.registry()["ledger"]["client"]
        self.assertEqual((led["refused"], led["last"]), (1, "not allowed to spend"))
        self.assertTrue(self.op(self.A, op="perm", player=1, category="spend", allowed=True))
        self.assertTrue(self.buy(self.B))
        self.assertSameRegistry()

    @tl.full
    def test_a_limit_set_lifted_or_per_period_and_a_forbidden_category_on_both_games(self):
        for i, case in enumerate((self.limit_set_and_lifted, self.limit_per_period, self.forbidden_category)):
            with self.subTest(case.__name__):
                if i:
                    self.fresh_games()
                self.start()
                self.tick(10)
                case()

    def test_mixed_traffic_leaves_both_games_with_the_same_registry_and_ledger(self):
        import random
        rnd = random.Random(20261010)
        self.start()
        self.tick(10)
        self.assertTrue(self.op(self.A, op="found", name="Ironworks", open=True))
        self.assertTrue(self.op(self.A, op="found", name="Bakery", open=True))
        self.assertTrue(self.op(self.A, op="limit", player=1, amount=30000))      # one purchase fills it, until the host resets or lifts it
        from_host = [
            lambda: 'api.cmd.feverlink3Company({ op = "limit", player = 1, amount = %d })' % rnd.choice([1, 1, 25000, 70000]),
            lambda: 'api.cmd.feverlink3Company({ op = "limit", player = 1 })',
            lambda: 'api.cmd.feverlink3Company({ op = "perm", player = 1, category = "%s", allowed = %s })' % (rnd.choice(["build", "spend", "spend", "bulldoze"]), rnd.choice(["true", "false", "false"])),
            lambda: 'api.cmd.feverlink3Company({ op = "join", company = "%s" })' % rnd.choice(["c1", "c2"]),
            lambda: 'api.cmd.feverlink3Company({ op = "reset" })',
            lambda: 'api.cmd.feverlink3Company({ op = "pay", to = 1, amount = %d, from = 0 })' % rnd.randint(1, 900),
            lambda: 'api.cmd.makeVehicleBuyCmd(api.engine.util.getPlayer(), 55, { vehicles = {} })',
        ]
        from_client = [
            lambda: 'api.cmd.makeVehicleBuyCmd(api.engine.util.getPlayer(), 55, { vehicles = {} })',
            lambda: 'api.cmd.makeLineCreateCmd("L", { x = 0, y = 0, z = 0 }, api.engine.util.getPlayer(), { stops = {} })',
            lambda: 'api.cmd.feverlink3Company({ op = "pay", to = 0, amount = %d })' % rnd.randint(1, 900),
            lambda: 'api.cmd.feverlink3Company({ op = "join", company = "%s" })' % rnd.choice(["c1", "c2"]),
            lambda: 'api.cmd.feverlink3Company({ op = "leave" })',
            lambda: 'api.cmd.feverlink3Company({ op = "limit", player = 1, amount = 5 })',
        ]
        for _ in range(20):
            for game, pool in ((self.A, from_host), (self.B, from_client)):
                if rnd.random() < 0.7:
                    game.send_gui("api.cmd.sendCommand(%s)" % rnd.choice(pool)())
            self.tick(rnd.choice([3, 6, 10]))
        self.tick(40)
        self.assertSameRegistry()
        self.assertEqual(self.A.applied(), self.B.applied())
        self.assertEqual(self.both("ACCOUNT.balance")[0], self.both("ACCOUNT.balance")[1])
        reg = self.A.registry()
        self.assertGreater(reg["rev"], 10, "something happened")
        self.assertFalse(self.log_has("company layer failed"))
        self.assertFalse(self.log_has("LATE"))

    # ---- a company for each player: attribution on every game

    def separate(self, start_balance=2000000):
        """The host and the client found a company each; the host picks a company for each player (its module reports
        acting_player); the host's game binds its own company to the savegame's player and adds a player entity for the
        client's by itself (X.companyChores)."""
        self.A.L.execute("START_BALANCE = %d" % start_balance)
        self.B.L.execute("START_BALANCE = %d" % start_balance)
        self.assertTrue(self.op(self.A, op="found", name="Host Co"))
        self.assertTrue(self.op(self.B, op="found", name="Ironworks", color=[0.9, 0.2, 0.2]))
        self.assertFalse(self.op(self.A, op="mode", mode="separate"), "not without the capability")
        self.assertTrue(self.op(self.A, op="mode", mode="separate", acting=True))
        self.tick(30)

    def test_the_interface_player_follows_the_company_and_the_host_reloads_once(self):
        """native iplayer.rs (module listing `interface_player`): each game asks its module to make the interface act as its own company;
        the host asks its controller for ONE resync once a company's player entity was made in the session (the build tools take the
        player at a world load); the module's savegame player stays the canonical player (checksum account, record `player`)."""
        ready = ('ready\tnative\t{["build"]="40425",["clock"]=false,["hooks"]={[1]="interface_player"},["poolpatch"]=false,'
                 '["sites"]=0,["version"]="test"}\n')
        for g in (self.A, self.B):
            with open(os.path.join(g.dir, "native_events.log"), "w", encoding="utf-8", newline="") as f:
                f.write(ready)
            g.L.execute(tl.FIND_UP)
        self.start()
        self.tick(10)
        self.A.L.execute("FIND_UP('X').CO_RELOAD_DELAY_S = 0")
        self.separate()
        self.tick(10)
        pa, pb = self.both("GAME.applied[1].entity")

        def orders(g):
            text = "".join(tl.read_text(p) or "" for p in sorted(glob.glob(os.path.join(g.dir, "native_ctl.*.log"))))
            return [l.split(TAB)[2] for l in text.splitlines() if l.startswith("interface_player" + TAB)]

        self.assertEqual(orders(self.B), ['{["entity"]=%d}' % pb], "the client's interface acts as its company")
        self.assertEqual(orders(self.A), ['{["entity"]=7}'], "the host's company is the savegame's player")
        self.assertIn("interface player: asking the native module to act as entity %d" % pb, self.B.log())
        resyncs = [l for l in (tl.read_text(os.path.join(self.A.dir, "ctl_cmd.log")) or "").splitlines() if l.startswith("resync" + TAB)]
        self.assertEqual(len(resyncs), 1, "one resync round for the company made in this session")
        self.assertIn("company: c2 got its player entity in this session", self.A.log())
        self.assertFalse(os.path.exists(os.path.join(self.B.dir, "ctl_cmd.log")), "only the host asks")
        # the client's module answers: the savegame's player is canonical while the interface's own getPlayer names the company
        with open(os.path.join(self.B.dir, "native_events.log"), "a", encoding="utf-8", newline="") as f:
            f.write('interface_player\tnative\t{["front"]=%d,["player"]=%d,["savegame"]=7,["why"]="",["world"]=1}\n' % (pb, pb))
        # the game's own getPlayer of the interface now names the company (the module wrote it into the state the interface reads)
        self.B.L.execute("local f = FIND_UP('engine').canonicalPlayer; for i = 1, 30 do if debug.getupvalue(f, i) == 'realGetPlayer' then "
                         "debug.setupvalue(f, i, function() return %d end) end end" % pb)
        self.tick(5)
        self.assertEqual(self.B.ev("FIND_UP('engine').canonicalPlayer()"), 7)
        self.assertIn("interface player: the native module acts as entity %d (savegame player 7" % pb, self.B.log())
        self.assertEqual(orders(self.B), ['{["entity"]=%d}' % pb], "said once")

    def test_separate_companies_act_as_the_senders_company_on_every_game(self):
        self.start()
        self.tick(10)
        self.assertEqual(self.B.ev("GUI_API.engine.util.getPlayer()"), 7)
        self.separate()
        # adding a player binds a company to the new entity, which has another id in each game
        pa, pb = self.both("GAME.applied[1].entity")
        self.assertNotEqual(pa, pb)
        self.assertSameRegistry()
        reg = self.A.registry()
        self.assertEqual(reg["mode"], "separate")
        self.assertEqual(reg["companies"]["c1"]["entity"], "s:7", "the host's company is the savegame's player")
        c = reg["companies"]["c2"]
        key = c["entity"]
        self.assertEqual((c["name"], c["owner"], key.split(":")[0]), ("Ironworks", "client", "0"))
        self.assertIn("company: adding a player entity for company c2 (Ironworks)", self.A.log())
        self.assertEqual([c["color"][1], c["color"][2], c["color"][3]], [0.9, 0.2, 0.2])
        self.assertEqual(self.A.ev("WORLD[%d].NAME" % pa), "Ironworks")
        self.assertEqual(self.both("STATE:get().bind['%s']" % key), (pa, pb), "the simulation state keeps each game's own binding")
        # the game's windows follow the registry; the simulation half is never switched
        self.assertEqual(self.B.ev("GUI_API.engine.util.getPlayer()"), pb, "the client's windows act as its company")
        self.assertEqual(self.A.ev("GUI_API.engine.util.getPlayer()"), 7, "the host's company is the savegame's player")
        self.assertEqual(self.B.ev("SIM_ENV.api.engine.util.getPlayer()"), 7)
        self.assertIn("local company: entity %d" % pb, self.B.log())
        # the client's window passes getPlayer(); every game buys for the client's company, with its own id
        self.assertTrue(self.buy(self.B))
        va, vb = self.both("GAME.applied[2].entity")
        self.assertEqual((self.A.ev("OWNER[%d]" % va), self.B.ev("OWNER[%d]" % vb)), (pa, pb))
        self.assertEqual((self.A.ev("GAME.applied[2].args[1]"), self.B.ev("GAME.applied[2].args[1]")), (pa, pb))
        # the money left that company's account, on both games; the savegame's player was not touched; the ledger measured it there
        self.assertEqual((self.A.ev("ACCOUNTS[%d].balance" % pa), self.B.ev("ACCOUNTS[%d].balance" % pb)), (2000000 - 30000, 2000000 - 30000))
        self.assertEqual(self.both("ACCOUNT.balance"), (1000000, 1000000))
        self.assertEqual(self.A.registry()["ledger"]["client"]["gross"], {"spend": 30000})
        # the host acts as its company, which is the savegame's player (key s:7)
        self.assertTrue(self.line(self.A))
        self.assertEqual(self.A.ev("OWNER[%d]" % self.A.ev("GAME.applied[3].entity")), 7)
        self.assertEqual(self.both("GAME.applied[3].args[3]"), (7, 7))
        self.assertEqual(self.both("ACCOUNT.balance"), (1000000 - 5000, 1000000 - 5000))
        self.assertEqual(self.A.applied(), self.B.applied())
        # Give to company: the client cannot take the host's line (refused at the stamp on every game); the host's window gives it
        # (the page shows it once the player selected it in the game: a click on one of its vehicles, then its line window, whose
        # closing ends the selection), the client's gives it back
        la, lb = self.both("GAME.applied[3].entity")
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeEntitySetPlayerCmd(%d, %d))" % (lb, pb))
        self.tick(15)
        self.assertEqual(self.both("#GAME.applied"), (3, 3))
        self.assertIn("can hand it over", self.A.registry()["ledger"]["client"]["last"])
        self.assertIsNone(self.A.status().get("selected"))
        gui = "GUI.guiHandleEvent(nil, STATE, GSTATE, 0, '%s', '%s', %s)"
        self.A.L.execute("WORLD[2900] = { TRANSPORT_VEHICLE = { line = %d } }; " % la + gui % ("mainView", "select", "2900"))
        self.tick(5)
        sel = self.A.status()["selected"]
        self.assertEqual((sel["id"], sel["mine"], sel["company"], sel["vehicles"], sel["stops"]), (la, True, "c1", 0, 0))
        self.assertIn("company: selected line %d (" % la, self.A.log())
        self.A.L.execute(gui % ("temp.view.entity_%d" % la, "idAdded", "{}") + "; " + gui % ("temp.view.entity_2900", "destroy", "{}"))
        self.tick(5)
        self.assertEqual(self.A.status()["selected"]["id"], la, "the vehicle's window closed, the line's is open")
        self.A.L.execute(gui % ("temp.view.entity_%d" % la, "destroy", "{}"))
        self.tick(5)
        self.assertIsNone(self.A.status().get("selected"))
        self.assertIn("company: selected line %d cleared (temp.view.entity_%d/destroy)" % (la, la), self.A.log())
        self.B.L.execute(gui % ("temp.view.entity_%d" % lb, "idAdded", "{}"))
        self.tick(5)
        sel = self.B.status()["selected"]
        self.assertEqual((sel["mine"], sel["companyName"]), (False, "Host Co"), "the client sees whose line it is")
        self.assertTrue(self.A.ui_company(op="give", line=la, company="c2"))
        self.tick(20)
        self.assertEqual((self.A.ev("OWNER[%d]" % la), self.B.ev("OWNER[%d]" % lb)), (pa, pb))
        self.assertIn("company: give line %d to c2 (Ironworks): 1 move(s)" % la, self.A.log())
        self.assertTrue(self.B.ui_company(op="give", line=lb, company="c1"))
        self.tick(20)
        self.assertEqual(self.both("OWNER[GAME.applied[3].entity]"), (7, 7))
        self.assertEqual(self.both("#GAME.applied"), (5, 5))
        self.assertTrue(self.B.ui_company(op="give", line=lb, company="c1"))
        self.tick(10)
        self.assertIn("not given away: your company does not own that line", self.B.log())
        self.assertEqual(self.both("#GAME.applied"), (5, 5))
        # the operations module's commands follow the sender's company: a rename in the finance window, a journal booking (the cost of a
        # marketing campaign) that lands on the sender's company and in the ledger
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeEntitySetNameCmd(api.engine.util.getPlayer(), "Ironworks Ltd"))')
        self.tick(15)
        self.assertEqual((self.A.ev("WORLD[%d].NAME" % pa), self.B.ev("WORLD[%d].NAME" % pb)), ("Ironworks Ltd", "Ironworks Ltd"))
        self.assertIsNone(self.A.ev("WORLD[7].NAME"), "the host's company was not renamed")
        self.B.send_gui("local e = api.type.JournalEntry.new(); e.amount = -4000; e.time = -1; e.category.type = api.type.JournalEntry.Type.OTHER; "
                        "api.cmd.sendCommand(api.cmd.makeJournalBookAssetCmd(api.engine.util.getPlayer(), e))")
        self.tick(15)
        self.assertEqual((self.A.ev("ACCOUNTS[%d].balance" % pa), self.B.ev("ACCOUNTS[%d].balance" % pb)), (2000000 - 34000, 2000000 - 34000))
        self.assertEqual(self.both("ACCOUNT.balance"), (1000000 - 5000, 1000000 - 5000))
        led = self.A.registry()["ledger"]["client"]
        self.assertEqual((led["gross"], led["made"]), ({"spend": 34000}, {"makeVehicleBuyCmd": 1, "makeEntitySetPlayerCmd": 1, "makeEntitySetNameCmd": 1, "makeJournalBookAssetCmd": 1}))
        self.assertSameRegistry()
        # a payment between companies is a journal entry on each account, not a replicated action
        applied = self.both("#GAME.applied")
        self.assertTrue(self.op(self.B, op="pay", to=0, amount=12000, memo="the station"))
        self.assertEqual((self.A.ev("ACCOUNTS[%d].balance" % pa), self.B.ev("ACCOUNTS[%d].balance" % pb)), (2000000 - 46000, 2000000 - 46000))
        self.assertEqual(self.both("ACCOUNT.balance"), (1000000 + 7000, 1000000 + 7000))
        self.assertEqual((self.A.ev("JOURNALS[%d][#JOURNALS[%d]]" % (pa, pa)), self.A.ev("JOURNALS[7][#JOURNALS[7]]")), (-12000, 12000))
        reg = self.A.registry()
        self.assertEqual((reg["ledger"]["client"]["paid"], reg["ledger"]["host"]["received"]), (12000, 12000))
        self.assertSameRegistry()
        self.assertEqual(self.both("#GAME.applied"), applied)
        self.assertTrue(self.op(self.A, op="pay", to=0, amount=500, **{"from": 1}))      # the host charges the client a fee
        self.assertEqual(self.B.ev("ACCOUNTS[%d].balance" % pb), 2000000 - 46500)
        # a player outside every company acts as the game's own player
        self.forge(self.BUY_BODY, sender=2, seq=4, steps=6)
        self.assertEqual(self.both("GAME.applied[#GAME.applied].args[1]"), (7, 7))
        # a client cannot add players: every game refuses, no entity appears
        applied = self.both("#GAME.applied")
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeGameAddPlayerCmd("Mine", api.type.Vec3f.new(1, 1, 1)))')
        self.tick(20)
        self.assertEqual(self.both("#GAME.applied"), applied)
        self.assertEqual(len(self.A.registry()["companies"]), 2)
        # the host's money authority still corrects the savegame's player, and leaves the client's company (the windows show it) alone
        company_b, journals_b = self.B.ev("ACCOUNTS[%d].balance" % pb), self.B.ev("#(JOURNALS[%d] or {})" % pb)
        own = self.B.ev("ACCOUNT.balance")
        self.B.L.execute("ACCOUNT.balance = ACCOUNT.balance + 5000")        # the savegame's player drifts on the client
        self.relay.pump()
        self.relay.checksum_request(1, self.relay.future_stamp(), False)
        self.tick(20)
        ra, rb = self.relay.reports[1][0][1], self.relay.reports[1][1][1]
        self.assertIn("balance", {k for k in ra if ra[k] != rb[k]})
        self.relay.send_authority(1)
        self.tick(6)
        self.assertEqual(self.B.ev("ACCOUNT.balance"), own + 5000, "one round alone is not corrected")
        self.relay.checksum_request(2, self.relay.future_stamp(), False)
        self.tick(20)
        self.relay.send_authority(2)
        self.tick(6)
        self.assertEqual(self.B.ev("ACCOUNT.balance"), own, "corrected once the same difference was seen twice")
        self.assertEqual(self.B.ev("JOURNALS[7][#JOURNALS[7]]"), -5000)
        self.assertEqual((self.B.ev("ACCOUNTS[%d].balance" % pb), self.B.ev("#(JOURNALS[%d] or {})" % pb)), (company_b, journals_b))
        # nobody leaves in this mode; joining the host's company plays as the savegame's player, joining back switches back
        self.assertFalse(self.op(self.B, op="leave"))
        self.assertTrue(self.op(self.B, op="join", company="c1"))
        self.assertEqual(self.B.ev("GUI_API.engine.util.getPlayer()"), 7)
        self.assertTrue(self.op(self.B, op="join", company="c2"))
        self.assertEqual(self.B.ev("GUI_API.engine.util.getPlayer()"), pb)
        # a company whose entity this game does not know (it lost the binding) refuses instead of guessing
        self.B.L.execute("local st = STATE:get(); st.bind['%s'] = nil; STATE:set(st)" % key)
        before = len(self.B.applied())
        self.buy(self.B)
        self.assertEqual(len(self.B.applied()), before, "the client's game did not buy with a wrong player")
        self.assertEqual(len(self.A.applied()), before + 1, "the host's game, which knows the entity, did: the games now differ (a checksum shows it)")
        self.assertIn("not known on this game", self.B.log())
        # back to shared: the game's own player
        self.assertTrue(self.op(self.A, op="mode", mode="shared"))
        self.assertEqual(self.B.ev("GUI_API.engine.util.getPlayer()"), 7)
        self.assertIn("the game's own player", self.B.log())

    # ---- during a pause

    def test_registry_and_ledger_changes_made_while_paused_end_up_in_the_same_state(self):
        self.start(speed=2)
        self.tick(10, lag_b=True)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(40, lag_b=True)
        self.assertEqual((self.A.time(), self.B.time()), (self.relay.pauseAt, self.relay.pauseAt))
        # both are applied during the pause, at the stamps the host gives them
        self.A.send_gui('api.cmd.sendCommand(api.cmd.feverlink3Company({ op = "limit", player = 1, amount = 50000 }), function(d, s) PAUSED_OP = s end)')
        self.tick(8)
        self.assertTrue(self.A.ev("PAUSED_OP"))
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function(d, s) PAUSED_BUY = s end)")
        self.tick(8)
        self.assertTrue(self.B.ev("PAUSED_BUY"))
        # applied at the host's stamps, a step or two past the pause: the games ran on to them and stand still again
        self.assertEqual((self.A.speed(), self.B.speed()), (0, 0), "still paused")
        self.assertEqual(self.A.time(), self.B.time())
        self.assertEqual(self.A.applied(), self.B.applied())
        self.assertSameRegistry()
        reg = self.A.registry()
        self.assertEqual(reg["limits"], {1: {"amount": 50000}})
        self.assertEqual((reg["ledger"][1]["n"], reg["ledger"][1]["gross"]), ({"spend": 1}, {"spend": 30000}))
        self.assertIn("1:1", reg["made"])
        # the company page, written by the interface half from its mirror, shows the same
        self.assertEqual(row(self.B.status(), 1)["total"], 30000)
        self.assertEqual(row(self.A.status(), 1)["limit"]["amount"], 50000)
        # forged actions of player 1 made during the pause (the host stamps them, every game applies them): the first is still under
        # the limit, the second is not; it is refused, and the refusal is in the state the savegame keeps
        body = self.relay.C.serialize(self.relay.C.parse(self.BUY_BODY))

        def forge(seq):
            p = self.relay.lua({"stamp": self.relay.pauseAt, "seq": seq, "body": body, "from": 1, "pause": True})
            self.relay.pause_held.append(("action", self.B, p))
            self.relay.flush_pause()

        forge(9)
        self.tick(8)
        self.assertEqual(self.A.registry()["ledger"][1]["refused"], 0, "30 000 of 50 000 is still under the limit")
        self.assertEqual(self.A.applied()[-1].split("@")[0], "makeVehicleBuyCmd")
        forge(10)
        self.tick(8)
        self.assertEqual(self.A.registry()["ledger"][1]["refused"], 1, "60 000 of 50 000: refused, and the refusal is in the state the savegame keeps")
        self.assertEqual(row(self.A.status(), 1)["refused"], 1)
        # mod-level operations during the pause are replayed into the state, each seeing the one before
        self.assertTrue(self.op(self.A, ticks=8, op="found", name="Ironworks", open=True))
        self.assertTrue(self.op(self.B, ticks=8, op="join", company="c1"))
        self.assertTrue(self.op(self.A, ticks=8, op="found", name="Bakery"))
        self.assertSameRegistry()
        reg = self.A.registry()
        self.assertEqual((sorted(reg["companies"]), {w: p.get("company") for w, p in reg["people"].items()}), (["c1", "c2"], {"client": "c1", "host": "c2"}))
        # the game resumes: the registry goes on from where the pause left it
        self.relay.pauseAt, self.relay.speed = None, 2
        self.relay.session()
        self.tick(20)
        self.assertTrue(self.op(self.A, op="found", name="Mill"))
        self.assertSameRegistry()
        self.assertEqual(sorted(self.A.registry()["companies"]), ["c1", "c2", "c3"])
        self.assertEqual(self.B.status()["companies"][1]["name"], "Ironworks")

    # ---- persistence

    @tl.full
    def test_a_reload_keeps_the_registry_and_the_page(self):
        self.start()
        self.tick(10)
        self.assertTrue(self.op(self.A, op="found", name="Ironworks", open=True))
        self.assertTrue(self.op(self.B, op="join", company="c1"))
        self.assertTrue(self.op(self.A, op="limit", player=1, amount=50000))
        self.assertTrue(self.buy(self.B))
        reg = self.B.registry()
        # a resync load, as the host asks for it: the line's sequence is kept so nothing is dispatched twice
        self.relay.send(self.B, "resync_load", 0, self.relay.ser({"id": 1, "name": "hostsave", "sha256": "00"}))
        self.B.frame()
        self.B.reload()
        self.relay.session()          # the host tells the reloaded game where the session stands, as it does after the hello
        self.tick(10)
        self.assertEqual(self.B.registry(), reg, "the registry is part of the script state the savegame carries")
        status = self.B.status()
        self.assertEqual(row(status, 1)["total"], 30000)
        self.assertEqual([m["name"] for m in status["companies"][1]["members"].values()], ["client", "host"], "members by their stable identity")
        # the reloaded mod goes on: the limit still applies. (The window's callback is not asked: the reloaded mod numbers
        # its actions from 1 again, so the key of the first one is a key the state already holds a result for.)
        applied = len(self.A.applied())
        self.buy(self.B)
        self.assertEqual((len(self.A.applied()), len(self.B.applied())), (applied + 1, applied + 1))
        self.buy(self.B)
        self.assertEqual((len(self.A.applied()), len(self.B.applied())), (applied + 1, applied + 1), "the limit (50 000, 60 000 spent) stops the next one")
        self.assertSameRegistry()

    # ---- the verify round after a resync reload (bug list 2026-10-17 #2; friend test 2026-10-16: five repairs that worked were
    # reported as failed with "still differ after the reload: companies", because the reloaded game hashed its still empty mirror)

    def paused_with_a_company(self):
        """Both games hold a registry with a company and stand still at a pause; returns the pause time."""
        self.start()
        self.tick(10)
        self.assertTrue(self.op(self.A, op="found", name="Ironworks", open=True))
        self.assertTrue(self.op(self.B, op="join", company="c1"))
        self.assertSameRegistry()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(40)
        self.assertEqual((self.A.time(), self.B.time()), (self.relay.pauseAt, self.relay.pauseAt))
        return self.relay.pauseAt

    def reload_b_and_verify(self, n, at):
        """B reloads as the resync asks; the host's session line and the paused verify round are waiting in its link when it starts."""
        self.relay.send(self.B, "resync_load", 0, self.relay.ser({"id": 1, "name": "hostsave", "sha256": "00"}))
        self.B.frame()
        self.B.reload()
        self.relay.session()
        self.relay.checksum_request(n, at, True)

    def frames(self, n=1):
        """Interface frames only: the simulation half of neither game runs."""
        for _ in range(n):
            for g in (self.A, self.B):
                g.frame()
            self.relay.pump()

    def verify_in_the_first_frame(self):
        self.reload_b_and_verify(7, self.paused_with_a_company())
        self.frames(1)                    # the round is read in the reloaded game's very first frame, before its mirror is filled
        self.tick(3)
        a, b = self.relay.reports[7][0], self.relay.reports[7][1]
        self.assertIn("companies", b[1], "the reloaded game hashed its registry, not the empty mirror")
        self.assertEqual(a, b, "the repair worked: the same world on both games")
        self.assertEqual(self.relay.not_ready[7], {0: [], 1: []})

    def verify_waits_for_the_loaded_state(self):
        self.reload_b_and_verify(8, self.paused_with_a_company())
        # the simulation half has not started: the interface half of B cannot read the game-script state yet
        self.B.L.execute("REAL_GET = STATE.get; STATE.get = function() return nil end")
        self.frames(6)
        self.assertIn(0, self.relay.reports[8])
        self.assertNotIn(1, self.relay.reports.get(8, {}), "B waits instead of reporting the empty registry")
        self.assertIn("the company registry is not read from the loaded state yet; the round waits for it", self.B.log())
        self.B.L.execute("STATE.get = REAL_GET")
        self.tick(3)
        self.assertEqual(self.relay.reports[8][0], self.relay.reports[8][1])
        self.assertEqual(self.relay.not_ready[8][1], [])
        self.assertIn("the company registry was read from the loaded state after", self.B.log())

    def verify_reports_an_unreadable_registry_not_ready(self):
        self.reload_b_and_verify(9, self.paused_with_a_company())
        self.B.L.execute("REAL_GET = STATE.get; STATE.get = function() return nil end")
        self.frames(2)
        self.assertNotIn(1, self.relay.reports.get(9, {}))
        self.B.L.execute("REAL_CLOCK = os.clock; os.clock = function() return REAL_CLOCK() + 60 end")   # the bound (10 s) is over
        self.frames(2)
        self.B.L.execute("os.clock = REAL_CLOCK; STATE.get = REAL_GET")
        b = self.relay.reports[9][1][1]
        self.assertNotIn("companies", b, "no hash of the empty mirror")
        self.assertEqual(self.relay.not_ready[9][1], ["companies"])
        self.assertEqual({k: v for k, v in b.items()}, {k: v for k, v in self.relay.reports[9][0][1].items() if k != "companies"},
                         "every other part is measured and compared as always")
        self.assertIn("companies are reported not ready", self.B.log())

    def verify_still_reports_a_real_difference(self):
        at = self.paused_with_a_company()
        # the world B loads holds another registry than the host's (a real difference the repair did not remove)
        self.B.L.execute("local st = STATE:get(); st.co.companies.c1.name = 'Elsewhere'; st.co.rev = st.co.rev + 1; STATE:set(st)")
        self.reload_b_and_verify(10, at)
        self.frames(1)
        self.tick(3)
        a, b = self.relay.reports[10][0][1], self.relay.reports[10][1][1]
        self.assertEqual({k for k in a if a[k] != b.get(k)}, {"companies"}, "the registry differs and is reported as a difference")
        self.assertEqual(self.relay.not_ready[10], {0: [], 1: []}, "nothing is hidden as not ready")

    def test_the_verify_round_after_a_resync_reload_hashes_the_registry_the_save_loaded(self):
        for i, case in enumerate((self.verify_in_the_first_frame, self.verify_waits_for_the_loaded_state,
                                  self.verify_reports_an_unreadable_registry_not_ready, self.verify_still_reports_a_real_difference)):
            with self.subTest(case.__name__):
                if i:
                    self.fresh_games()
                case()

    # ---- the window, faults

    @tl.full
    def test_the_company_page_and_the_window_records(self):
        self.start()
        self.tick(10)
        self.relay.send(self.B, "welcome", 0, self.relay.ser({"you": 1, "hostName": "host", "version": "0.1", "players": [{"id": 1, "name": "client", "ready": True}], "session": {}}))
        self.tick(4)
        st = self.B.status()
        self.assertEqual((st["mode"], st["self"], st["isHost"], st["hostEntityKey"]), ("shared", 1, False, "s:7"))
        self.assertEqual([p["name"] for p in st["players"].values()], ["host", "client"])
        self.assertTrue(self.A.status()["isHost"])
        self.assertTrue(self.line(self.B))
        self.A.L.execute("LINE_RESULT[%d] = 1700" % self.A.ev("GAME.applied[1].entity"))
        self.B.L.execute("LINE_RESULT[%d] = 1700" % self.B.ev("GAME.applied[1].entity"))
        for game in (self.A, self.B):          # the page is written again when the roster changes (and every few seconds)
            self.relay.send(game, "player_joined", 0, self.relay.ser({"player": {"id": 9, "name": "dave", "ready": True}}))
        self.tick(4)
        for game in (self.A, self.B):
            st = game.status()
            self.assertEqual(row(st, 1)["earned"], 1700)
            self.assertEqual(row(st, 1)["net"], 1700 - 5000)
        # a window record becomes a company action
        self.assertTrue(self.A.ui_company(op="found", name="Ironworks", open=True))
        self.tick(20)
        self.assertEqual(sorted(self.A.registry()["companies"]), ["c1"])
        self.assertSameRegistry()
        self.assertTrue(self.B.ui_company(op="join", company="c1"))
        self.tick(20)
        self.assertEqual(self.A.registry()["seats"], {0: "host", 1: "client"}, "each record says who sends it")
        self.assertEqual(self.A.registry()["people"]["client"]["company"], "c1")
        self.assertTrue(self.A.ui_company(op="format the disk"))       # a record the module does not know is ignored
        self.tick(10)
        # the gameplay type the host's window picks: this game's module has no acting_player capability, so a company for each
        # player is refused on every game (the window greys it out)
        self.assertTrue(self.A.ui_company(op="mode", mode="separate"))
        self.tick(20)
        self.assertEqual(self.A.registry()["mode"], "shared")
        self.assertIn("is not available in this game", self.A.registry()["ledger"]["host"]["last"])
        self.assertFalse(self.A.status()["acting"])
        self.assertEqual(self.both("#GAME.applied"), (1, 1))
        self.assertSameRegistry()
        # an api whose command table refuses assignment: nothing is wrapped, so an operation or a new player would apply on this game
        # alone; the record is dropped and the log says why
        self.fresh_games()
        self.B.L.execute("local real = GUI_API.cmd; GUI_API.cmd = setmetatable({}, { __index = real, __newindex = function() error('read-only') end })")
        self.start()
        self.tick(10)
        self.assertIn("sendCommand is read-only", self.B.log())
        self.assertTrue(self.B.ui_company(op="leave"))
        self.tick(10)
        self.assertIn("company action not sent", self.B.log())
        self.assertEqual(self.B.ev("#GAME.applied"), 0, "no player was added on this game alone")
        self.assertEqual(self.shipped(self.B), 0)

    def test_faults_and_forged_company_actions_never_stop_the_game_nor_lose_the_players_action(self):
        self.start()
        self.tick(10)
        for seq, body in enumerate(('{["fn"]="feverlink3Company",["args"]={["n"]=1,[1]="limit"}}',
                                    '{["fn"]="feverlink3Company",["args"]={["n"]=1,[1]={["op"]=5}}}',
                                    '{["fn"]="feverlink3Company",["args"]={["n"]=1,[1]={["op"]="limit",["player"]="x",["amount"]=1}}}',
                                    '{["fn"]="feverlink3Company",["args"]={["n"]=0}}'), 20):
            self.forge(body, sender=0, seq=seq, steps=6)
        reg = self.B.registry()
        self.assertEqual((reg["limits"], reg["companies"]), ({}, {}), "a forged action with a bad shape is refused everywhere")
        self.assertFalse(self.log_has("company layer failed"))
        # garbage in the registry of one game: the purchase still goes through on both
        self.assertTrue(self.op(self.A, op="found", name="Ironworks"))
        self.B.L.execute("local st = STATE:get(); st.co.limits = 5; STATE:set(st)")
        self.assertTrue(self.buy(self.B))
        self.assertEqual(self.A.applied(), self.B.applied(), "the purchase went through on both")
        self.assertIn("company layer failed", self.B.log())
        # garbage the page cannot show (with a newer revision, so the interface half copies it): logged once, the game goes on
        self.fresh_games()
        self.start()
        self.tick(10)
        self.assertTrue(self.op(self.A, op="found", name="Ironworks"))
        self.B.L.execute("local st = STATE:get(); st.co.made = 5; st.co.rev = st.co.rev + 1; STATE:set(st)")
        before = self.B.time()
        self.tick(20)
        self.assertGreater(self.B.time(), before + 10 * STEP, "the game keeps pacing and answering the host")
        self.assertEqual(self.B.log().count("company layer failed in page"), 1, "logged once, not every frame")
        self.assertTrue(self.buy(self.B), "and the player's actions still go through")


if __name__ == "__main__":
    unittest.main(verbosity=2)
