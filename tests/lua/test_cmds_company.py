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
    """A Lua value as plain Python: tables become dicts keyed by int/str, everything else is unchanged."""
    if lupa.lua_type(v) == "table":
        out = {}
        for k, x in v.items():
            if isinstance(k, float) and k == int(k):
                k = int(k)
            out[k] = to_py(x)
        return out
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
        self.C = self.L.execute(tl.COMMON_SRC)
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

    def test_found_assign_join_leave_and_who_may(self):
        fx = self.must(0, op="found", name="  Ironworks \t", color=[0.9, 0.2, 0.2])
        self.assertEqual(fx["company"], "c1")
        c = self.reg()["companies"]["c1"]
        self.assertEqual((c["name"], c["owner"], c["open"], c.get("entity")), ("Ironworks", 0, False, None))
        self.assertEqual([c["color"][1], c["color"][2], c["color"][3]], [0.9, 0.2, 0.2])
        self.refused(1, "only the host", op="found", name="Mine")
        self.refused(0, "needs a name", op="found", name="   ")
        self.assertEqual(self.must(0, op="found", name="Bakery", open=True)["company"], "c2", "keys are never reused")
        self.refused(1, "closed", op="join", company="c1")
        self.must(1, op="join", company="c2")
        self.assertEqual(self.reg()["members"], {1: "c2"})
        self.refused(1, "no such company", op="join", company="c9")
        self.refused(2, "only the host", op="assign", player=2, company="c1")
        self.must(0, op="assign", player=2, company="c1")
        self.assertEqual(self.reg()["members"], {1: "c2", 2: "c1"})
        self.refused(0, "bad player", op="assign", player=-1, company="c1")
        self.refused(0, "no such company", op="assign", player=3, company="c7")
        self.must(1, op="leave")
        self.must(0, op="assign", player=2)     # no company: takes the player out
        self.assertEqual(self.reg()["members"], {})
        # the host may always join; the owner of a closed company too
        self.must(0, op="join", company="c1")
        self.assertEqual(self.reg()["members"], {0: "c1"})

    def test_rename_open_remove_and_bind(self):
        self.must(0, op="found", name="One")
        self.must(0, op="assign", player=1, company="c1")
        self.refused(1, "host or the owner", op="rename", company="c1", name="Mine")      # a member is not the owner
        self.must(0, op="rename", company="c1", name="Uno")
        self.assertEqual(self.reg()["companies"]["c1"]["name"], "Uno")
        self.refused(0, "needs a name", op="rename", company="c1", name="")
        self.must(0, op="open", company="c1", open=True)
        self.assertTrue(self.reg()["companies"]["c1"]["open"])
        self.must(0, op="bind", company="c1", entity="s:7")
        self.refused(0, "another company", op="found", name="Two", entity="s:7")
        self.must(0, op="found", name="Two", entity="0:4")
        self.refused(0, "another company", op="bind", company="c2", entity="s:7")
        self.refused(0, "bad entity key", op="bind", company="c2", entity="seven")
        self.refused(1, "only the host", op="remove", company="c2")
        self.must(0, op="assign", player=2, company="c2")
        self.must(0, op="remove", company="c2")
        self.assertNotIn("c2", self.reg()["companies"])
        self.assertEqual(self.reg()["members"], {1: "c1"}, "members of a removed company are taken out")
        self.assertEqual(self.must(0, op="found", name="Three")["company"], "c3")

    def test_rev_counts_changes_and_pristine_is_a_tracked_state(self):
        self.assertTrue(self.Co.pristine(self.co))
        self.assertIsNone(self.Co.digest(self.co))
        self.must(0, op="found", name="One")
        self.assertFalse(self.Co.pristine(self.co))
        rev = self.reg()["rev"]
        self.must(0, op="assign", player=1, company="c1")
        self.assertEqual(self.reg()["rev"], rev + 1)
        self.must(0, op="assign", player=1, company="c1")      # no change: the window does not redraw
        self.assertEqual(self.reg()["rev"], rev + 1)
        self.refused(0, "unknown company action", op="dance")
        self.assertEqual(self.reg()["rev"], rev + 1)
        ok, why = self.Co.apply(self.co, self.ctx(0), 5)
        self.assertFalse(ok)

    def test_companies_are_limited_and_names_cleaned(self):
        for i in range(16):
            self.must(0, op="found", name="C%d" % i)
        self.refused(0, "too many", op="found", name="One more")
        self.must(0, op="remove", company="c3")
        self.assertEqual(self.must(0, op="found", name="x" * 200)["company"], "c17")
        self.assertEqual(len(self.reg()["companies"]["c17"]["name"]), 40)

    def test_mode_switch_needs_the_hosts_confirmation_and_an_entity_per_company(self):
        self.must(0, op="found", name="One", entity="s:7")
        self.must(0, op="found", name="Two")
        self.refused(1, "only the host", op="mode", mode="separate", experimental=True)
        self.refused(0, "experimental", op="mode", mode="separate")
        self.refused(0, "no player entity", op="mode", mode="separate", experimental=True)
        self.must(0, op="bind", company="c2", entity="0:3")
        self.must(0, op="mode", mode="separate", experimental=True)
        self.assertEqual(self.reg()["mode"], "separate")
        self.refused(0, "needs its player entity", op="found", name="Three")
        self.refused(0, "unknown mode", op="mode", mode="chaos")
        self.must(0, op="mode", mode="shared")
        self.assertEqual(self.reg()["mode"], "shared")

    def test_add_player_registers_the_company_under_the_action_key(self):
        r = self.Co.addPlayer(self.co, self.ctx(0, key="0:5"), "Ironworks", self.lua({"x": 0.1, "y": 0.2, "z": 0.3}))
        self.assertTrue(r[0])
        c = self.reg()["companies"]["c1"]
        self.assertEqual((c["name"], c["entity"], c["owner"]), ("Ironworks", "0:5", 0))
        self.assertEqual([c["color"][1], c["color"][2], c["color"][3]], [0.1, 0.2, 0.3])
        before = self.C.serialize(self.co)
        for sender, key, name, text in [(1, "1:5", "Mine", "only the host"), (0, None, "Mine", "no action key"), (0, "0:6", " ", "needs a name")]:
            ok, why = self.Co.addPlayer(self.co, self.ctx(sender, key=key), name, None)
            self.assertFalse(ok)
            self.assertIn(text, why)
        self.assertEqual(self.C.serialize(self.co), before)

    def resolver(self, table):
        return self.L.eval("function(t) return function(k) return t[k] end end")(self.lua(table))

    def test_actor_rewrites_the_player_per_sender(self):
        self.must(0, op="found", name="One", entity="s:7")
        self.must(0, op="found", name="Two", entity="0:2")
        self.must(0, op="assign", player=0, company="c1")
        self.must(0, op="assign", player=1, company="c2")
        resolve = self.resolver({"s:7": 7, "0:2": 52001, "s:55": 55})
        # shared mode: every sender keeps this game's own player entity
        for sender in (0, 1, 2):
            c = self.L.table_from({"player": 7, "from": sender, "co": self.co, "resolve": resolve})
            self.Co.actor(c)
            self.assertEqual(c.player, 7)
            self.assertIsNone(c.refuse)
        self.must(0, op="mode", mode="separate", experimental=True)
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

    def test_limit_gate_periods_and_categories(self):
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
        self.assertTrue(Co.gate(co, 1, "bulldoze", 0), "bulldozing is never stopped")
        self.assertTrue(Co.gate(co, 1, "other", 0))
        self.assertTrue(Co.gate(co, 0, "spend", 0), "a player without a limit is free")
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
        self.assertTrue(Co.gate(co, 1, "build", 0))
        self.assertNotIn(1, self.reg()["limits"])
        # a limit per period: the count restarts when the period changes, by the stamp and not by the clock of a game
        self.must(0, op="limit", player=1, amount=50000, per=1000, at=2500)
        Co.record(co, 1, 2600, self.lua({"fn": "x", "category": "build", "cost": 50000}))
        self.assertFalse(Co.gate(co, 1, "build", 2900)[0])
        self.assertTrue(Co.gate(co, 1, "build", 3000), "the next period starts at 3000")
        Co.record(co, 1, 3100, self.lua({"fn": "x", "category": "build", "cost": 10000}))
        self.assertEqual(self.reg()["ledger"][1]["per"], {"idx": 3, "gross": 10000})

    def test_a_cost_the_engine_could_not_tell_is_counted_without_money(self):
        self.Co.record(self.co, 1, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend"}))
        self.Co.record(self.co, 1, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": float("nan")}))
        led = self.reg()["ledger"][1]
        self.assertEqual((led["n"]["spend"], led["made"]["makeVehicleBuyCmd"], led["gross"], led["refund"]), (2, 2, {}, 0))

    def test_refusals_are_counted_and_remembered(self):
        self.Co.refuse(self.co, 3, "spending limit reached")
        self.Co.refuse(self.co, 3, "only the host can do that")
        led = self.reg()["ledger"][3]
        self.assertEqual((led["refused"], led["last"]), (2, "only the host can do that"))

    def test_pay_is_a_ledger_entry_and_in_separate_mode_two_journal_entries(self):
        self.must(0, op="found", name="One", entity="s:7")
        self.must(0, op="found", name="Two", entity="0:2")
        self.must(0, op="assign", player=0, company="c1")
        self.must(0, op="assign", player=1, company="c2")
        self.must(0, op="assign", player=2, company="c2")
        fx = self.must(1, op="pay", to=0, amount=10000, memo="the station")
        self.assertNotIn("postings", fx, "shared mode: nothing moves in the game")
        led = self.reg()["ledger"]
        self.assertEqual((led[1]["paid"], led[0]["received"]), (10000, 10000))
        self.must(0, op="mode", mode="separate", experimental=True)
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
        self.assertEqual((led[1]["paid"], led[0]["received"]), (10000 + 2500 + 100 + 700, 10000 + 2500 + 700))

    def test_run_effects_books_each_posting_on_the_companys_entity(self):
        self.must(0, op="found", name="One", entity="s:7")
        self.must(0, op="found", name="Two", entity="0:2")
        booked = self.L.eval("{}")
        engine = self.L.eval("function(booked) return { bookJournalFor = function(e, n, memo) booked[#booked + 1] = e .. ':' .. n .. ':' .. tostring(memo) end } end")(booked)
        ctx = self.L.table_from({"co": self.co, "resolve": self.resolver({"s:7": 7, "0:2": 52001})})
        fx = self.lua({"postings": [{"company": "c2", "amount": -2500, "memo": "fee"}, {"company": "c1", "amount": 2500, "memo": "fee"}]})
        done, failed = self.Co.runEffects(ctx, fx, engine)
        self.assertEqual(int(done), 2)
        self.assertEqual([booked[i] for i in (1, 2)], ["52001:-2500:fee", "7:2500:fee"])
        # an entity this game does not know, or an engine that refuses: counted as failed, the rest still goes in
        ctx2 = self.L.table_from({"co": self.co, "resolve": self.resolver({"s:7": 7})})
        done, failed = self.Co.runEffects(ctx2, fx, engine)
        self.assertEqual(int(done), 1)
        self.assertEqual(len(failed), 1)
        self.assertIn("2,500", failed[1] if lupa.lua_type(failed) == "table" else failed[0])
        nobook = self.L.table_from({})
        done, failed = self.Co.runEffects(ctx, fx, nobook)
        self.assertEqual((int(done), len(failed)), (0, 2))

    def test_digest_has_counts_and_configuration_but_no_measured_money(self):
        self.must(0, op="found", name="One")
        self.Co.record(self.co, 1, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 30000}))
        a = self.C.hashValue(self.Co.digest(self.co))
        self.Co.record(self.co, 2, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 100}))
        b = self.C.hashValue(self.Co.digest(self.co))
        self.assertNotEqual(a, b, "another player's purchase changes the digest")
        # same actions, another measured cost: same digest
        other = self.Co.new()
        self.Co.apply(other, self.ctx(0), self.lua({"op": "found", "name": "One"}))
        self.Co.record(other, 1, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 29990}))
        self.Co.record(other, 2, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 5}))
        self.assertEqual(self.C.hashValue(self.Co.digest(other)), b)
        self.Co.record(other, 2, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 5}))
        self.assertNotEqual(self.C.hashValue(self.Co.digest(other)), b, "one more action does")

    def test_registry_state_is_plain_data_the_link_and_the_savegame_can_carry(self):
        self.must(0, op="found", name="One", entity="s:7", color=[0.5, 0.5, 0.5])
        self.must(0, op="assign", player=1, company="c1")
        self.must(0, op="limit", player=1, amount=1000, per=5000)
        self.must(1, op="pay", to=0, amount=5, memo="x")
        self.Co.record(self.co, 1, 100, self.lua({"fn": "makeLineCreateCmd", "category": "build", "cost": 12, "created": {"key": "1:2", "kind": "LINE"}}))
        text = self.C.serialize(self.co)          # raises on functions, userdata, cycles
        back = self.C.parse(text)
        self.assertEqual(self.C.serialize(back), text)
        self.assertEqual(to_py(back), self.reg())

    def test_made_is_bounded_and_forgets_the_oldest(self):
        for i in range(1, 421):
            self.Co.record(self.co, 1, i, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "created": {"key": "1:%d" % i, "kind": "TRANSPORT_VEHICLE"}}))
        made = self.reg()["made"]
        self.assertLessEqual(len(made), 400)
        self.assertIn("1:420", made)
        self.assertNotIn("1:1", made)
        self.assertEqual(self.reg()["nmade"], len(made))

    def test_earned_sums_the_results_of_each_players_lines(self):
        for key, by in (("1:2", 1), ("1:3", 1), ("0:4", 0)):
            self.Co.record(self.co, by, 0, self.lua({"fn": "makeLineCreateCmd", "category": "build", "created": {"key": key, "kind": "LINE"}}))
        self.Co.record(self.co, 0, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "created": {"key": "0:5", "kind": "TRANSPORT_VEHICLE"}}))
        results = {"1:2": 1200.4, "1:3": -300, "0:4": None}
        got = to_py(self.Co.earned(self.co, self.L.eval("function(t) return function(k) return t[k] end end")(self.lua({k: v for k, v in results.items() if v is not None}))))
        self.assertEqual(got, {1: 900}, "vehicles are not lines, unreadable lines count for nothing")

    def test_status_table_for_the_window(self):
        self.must(0, op="found", name="Ironworks", open=True)
        self.must(0, op="assign", player=1, company="c1")
        self.must(0, op="limit", player=1, amount=50000)
        self.Co.record(self.co, 1, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 45000}))
        self.Co.record(self.co, 1, 0, self.lua({"fn": "makeLineCreateCmd", "category": "build", "cost": 3000}))
        self.Co.record(self.co, 0, 0, self.lua({"fn": "makeWorldBuildProposalCmd", "category": "build", "cost": 20000}))
        self.Co.record(self.co, 0, 0, self.lua({"fn": "makeWorldBuildProposalCmd", "category": "bulldoze", "cost": -1500}))
        view = self.lua({"self": 1, "names": {0: "Boss", 1: "Alice", 2: "Carol"}, "now": 0, "earned": {1: 800}, "hostEntityKey": "s:7",
                         "yearLength": 96000, "perms": {"build": True, "bulldoze": False, "spend": True, "other": True}})
        st = to_py(self.Co.status(self.co, view))
        self.assertEqual((st["yearLength"], st["perms"]["bulldoze"], st["perms"]["build"]), (96000, False, True))
        self.assertEqual((st["mode"], st["self"], st["isHost"], st["hostEntityKey"], st["openCompanies"]), ("shared", 1, False, "s:7", {1: "c1"}))
        self.assertEqual([p["id"] for p in st["players"].values()], [0, 1, 2])
        boss, alice, carol = st["players"][1], st["players"][2], st["players"][3]
        self.assertEqual((alice["name"], alice["you"], alice["company"], alice["bought"], alice["built"], alice["total"]), ("Alice", True, "c1", 1, 1, 48000))
        self.assertEqual((alice["limit"]["amount"], alice["used"], alice["remaining"], alice["blocked"]), (50000, 48000, 2000, False))
        self.assertEqual((alice["earned"], alice["net"]), (800, 800 - 48000))
        self.assertEqual([m["label"] for m in alice["made"].values()], ["lines created", "vehicles bought"])
        self.assertEqual((boss["refunded"], boss["bulldozed"], boss["total"]), (1500, 1, 20000))
        self.assertEqual((carol["name"], carol["total"], carol.get("company")), ("Carol", 0, None))
        c1 = st["companies"][1]
        self.assertEqual((c1["key"], c1["name"], c1["members"], c1["spent"], c1["bound"], c1["you"]), ("c1", "Ironworks", {1: 1}, 48000, False, True))
        self.assertTrue(any("used 96% of the spending limit" in w for w in st["warnings"].values()), st["warnings"])
        lines = list(st["lines"].values())
        self.assertEqual(lines[1], "Alice (you): 1 built, 1 bought; spent 48,000; lines 800; limit 50,000, used 48,000")
        self.assertEqual(lines[2], "Carol: nothing yet")
        # at the limit
        self.Co.record(self.co, 1, 0, self.lua({"fn": "makeVehicleBuyCmd", "category": "spend", "cost": 5000}))
        st = to_py(self.Co.status(self.co, view))
        self.assertTrue(st["players"][2]["blocked"])
        self.assertTrue(any("reached the spending limit" in w for w in st["warnings"].values()))
        # without a registry or a view the table is still whole
        st = to_py(self.Co.status(None, None))
        self.assertEqual((st["mode"], len(st["players"]), len(st["companies"])), ("shared", 0, 0))

    def test_money_formatting(self):
        self.assertEqual([self.Co.money(n) for n in (0, 5, 999, 1000, 1234567, -45000.4, 3e9)], ["0", "5", "999", "1,000", "1,234,567", "-45,000", "3,000,000,000"])

    # ---- command specs

    def test_command_codecs(self):
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

    def test_non_host_cannot_add_players_or_move_owners(self):
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

    def test_window_records_become_commands(self):
        api = self.L.eval("""{ cmd = { makeGameAddPlayerCmd = function(n, c) return { add = n, r = c.x, g = c.y, b = c.z } end,
                                       feverlink3Company = function(p) return { company = p.op } end },
                               type = { Vec3f = { new = function(x, y, z) return { x = x, y = y, z = z } end } } }""")
        add = self.Co.windowCommand(self.lua({"op": "add", "name": " Bakery ", "color": [0.1, 0.2, 0.3]}), api)
        self.assertEqual((add.add, add.r, add.g, add.b), ("Bakery", 0.1, 0.2, 0.3))
        self.assertEqual(self.Co.windowCommand(self.lua({"op": "add", "name": ""}), api).add, "Company")
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
              engine = { util = { getPlayer = REAL, finance = {
                getPlayersBalance = function(e) if e == 99 then return nil end return e * 1000 end,
                calculateBalance = function(list, from, to, flag) return list[1] + from + to end } } },
              util = { getDefaultYearDuration = function() return 96000 end },
              type = { JournalEntry = { new = function() return { category = {} } end, Type = { OTHER = 5 } } },
              cmd = { makeJournalBookAssetCmd = function(e, entry) return { to = e, amount = entry.amount, time = entry.time, kind = entry.category.type } end,
                      sendCommand = function(c) NATIVE_SENT = c end },
            }
        """)
        engine = L.eval("""{ player = function() return api.engine.util.getPlayer() end,
                             account = function() return api.engine.util.getPlayer() * 10, 3 end,
                             bookJournal = function(n) BOOKED_ON = api.engine.util.getPlayer() end,
                             broken = function() error('boom') end }""")
        sent = L.eval("{}")
        deps = L.eval("function(sent) return { sender = function() return function(c) sent[#sent + 1] = c end end } end")(sent)
        self.Co.extendEngine(engine, deps)
        self.assertEqual(engine.balanceOf(7), 7000)
        self.assertIsNone(engine.balanceOf(99), "nil is infinite money")
        self.assertEqual(engine.lineResult(5, 10, 20), 35)
        self.assertEqual(engine.yearLength(), 96000)
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
        g = self.L.globals()
        g.COMMON_SRC, g.LOCKSTEP_SRC, g.BRIDGE_SRC, g.HUD_SRC = tl.COMMON_SRC, tl.LOCKSTEP_SRC, tl.BRIDGE_SRC, tl.HUD_SRC
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
        self.A = CoGame("host", 0, self.tmp, 50000)
        self.B = CoGame("client", 1, self.tmp, 51000)
        self.relay = tl.Relay([self.A, self.B])

    def tearDown(self):
        if os.environ.get("FEVERLINK3_TEST_VERBOSE"):
            print("\n--- host mod.log ---\n" + self.A.log() + "\n--- client mod.log ---\n" + self.B.log())
        shutil.rmtree(self.tmp, ignore_errors=True)

    # ---- scenario helpers

    def op(self, game, ticks=30, **fields):
        """A registry operation issued from `game`'s interface, as the company page does; returns its callback result."""
        game.send_gui("LAST_OP = nil; api.cmd.sendCommand(api.cmd.feverlink3Company(%s), function(d, s) LAST_OP = s end)" % lit(fields))
        self.tick(ticks)
        return game.ev("LAST_OP")

    def buy(self, game, ticks=40):
        game.send_gui("LAST_BUY = nil; api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(api.engine.util.getPlayer(), 55, { vehicles = {} }), function(d, s) LAST_BUY = s end)")
        self.tick(ticks)
        return game.ev("LAST_BUY")

    def line(self, game, ticks=40):
        game.send_gui('LAST_LINE = nil; api.cmd.sendCommand(api.cmd.makeLineCreateCmd("L", { x = 0, y = 0, z = 0 }, api.engine.util.getPlayer(), { stops = {} }), function(d, s) LAST_LINE = s end)')
        self.tick(ticks)
        return game.ev("LAST_LINE")

    def add_company(self, name="Ironworks", ticks=30):
        self.A.send_gui('api.cmd.sendCommand(api.cmd.makeGameAddPlayerCmd(%s, api.type.Vec3f.new(0.9, 0.2, 0.2)))' % tl.lua_str(name))
        self.tick(ticks)

    def both(self, expr):
        return self.A.ev(expr), self.B.ev(expr)

    def assertSameRegistry(self):
        self.assertEqual(self.A.registry(), self.B.registry(), "the two games' registries differ")

    # ---- shared company: the ledger

    def test_ledger_records_who_bought_and_built_what_identically_on_both_games(self):
        self.start()
        self.tick(10)
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

    def test_checksum_has_a_companies_part_once_something_was_recorded_and_it_agrees(self):
        self.start()
        self.tick(10)
        self.relay.pump()
        self.relay.checksum_request(1, self.relay.future_stamp(), False)
        self.tick(40)
        self.assertEqual(set(self.relay.reports[1][0][1]), {"time", "balance", "loan", "constructions", "lines"}, "pristine: no part")
        self.buy(self.B)
        self.relay.checksum_request(2, self.relay.future_stamp(), False)
        self.tick(40)
        pa, pb = self.relay.reports[2][0][1], self.relay.reports[2][1][1]
        self.assertIn("companies", pa)
        self.assertEqual(pa["companies"], pb["companies"])
        # a registry that differs on one game is named
        self.B.L.execute("local st = STATE:get(); st.co.limits[3] = { amount = 5 }; STATE:set(st)")
        self.relay.checksum_request(3, self.relay.future_stamp(), False)
        self.tick(40)
        pa, pb = self.relay.reports[3][0][1], self.relay.reports[3][1][1]
        self.assertEqual({k for k in pa if pa[k] != pb[k]}, {"companies"})

    def test_a_cost_the_engine_cannot_tell_still_counts_the_action(self):
        self.start()
        self.tick(10)
        self.A.L.execute("INFINITE = true")
        self.B.L.execute("INFINITE = true")
        self.assertTrue(self.buy(self.B))
        self.assertSameRegistry()
        led = self.A.registry()["ledger"][1]
        self.assertEqual((led["n"], led["gross"]), ({"spend": 1}, {}))

    # ---- limits

    def test_the_host_sets_a_limit_and_further_spending_is_stopped(self):
        self.start()
        self.tick(10)
        self.assertTrue(self.op(self.A, op="limit", player=1, amount=50000))
        self.assertEqual(self.A.registry()["limits"], {1: {"amount": 50000}})
        self.assertSameRegistry()
        self.assertTrue(self.buy(self.B))          # 0 of 50 000 used
        self.assertTrue(self.buy(self.B))          # 30 000 used: one more is allowed, it may overshoot
        self.assertEqual(len(self.A.applied()), 2)
        shipped = len([l for l in self.B.lines(self.B.from_game) if l.split(TAB)[1] == "action"])
        self.assertFalse(self.buy(self.B), "60 000 of 50 000: the window hears about it at once")
        self.assertEqual(len([l for l in self.B.lines(self.B.from_game) if l.split(TAB)[1] == "action"]), shipped, "nothing was stamped or shipped")
        self.assertEqual(len(self.A.applied()), 2)
        self.assertTrue(any("spending limit reached (60,000 of 50,000)" in why for (_, _, why) in self.relay.refused), self.relay.refused)
        self.assertTrue(self.line(self.A), "the host has no limit")
        # building is limited too; bulldozing and other things are not (the category comes from the command)
        self.assertFalse(self.line(self.B), "building counts against the same limit")
        # the host lifts it
        self.assertTrue(self.op(self.A, op="limit", player=1))
        self.assertTrue(self.buy(self.B))
        self.assertSameRegistry()
        self.assertEqual(self.A.registry()["ledger"][1]["gross"]["spend"], 90000)

    def test_a_forged_action_beyond_the_limit_is_refused_by_every_game_alike(self):
        self.start()
        self.tick(10)
        self.assertTrue(self.op(self.A, op="limit", player=1, amount=1))
        self.assertTrue(self.buy(self.B))
        before = len(self.A.applied())
        body = self.relay.C.serialize(self.relay.C.parse(
            '{["fn"]="makeVehicleBuyCmd",["args"]={["n"]=3,[1]={["__ref"]="player"},[2]={["__ref"]="entity",["key"]="s:55"},[3]={}}}'))
        stamp = self.A.time() + 5 * STEP
        payload = self.relay.ser({"stamp": stamp, "seq": 9, "body": body, "from": 1})
        self.relay.send(self.A, "action", 1, payload)
        self.relay.send(self.B, "action", 1, payload)   # the originator's own copy is not relayed back; both get it here for the test
        self.tick(30)
        self.assertEqual(len(self.A.applied()), before, "the host did not apply it")
        self.assertEqual(len(self.B.applied()), before, "nor did the client")
        self.assertSameRegistry()
        led = self.A.registry()["ledger"][1]
        self.assertEqual(led["refused"], 1, "each game refused it once, and the registry (identical on both) counts it once")
        self.assertIn("spending limit reached", led["last"])

    def test_only_the_host_can_set_limits_and_found_companies(self):
        self.start()
        self.tick(10)
        self.assertFalse(self.op(self.B, op="limit", player=1, amount=999999999))
        self.assertFalse(self.op(self.B, op="found", name="Mine"))
        self.assertSameRegistry()
        reg = self.A.registry()
        self.assertEqual((reg["limits"], reg["companies"]), ({}, {}))
        self.assertEqual(reg["ledger"][1]["refused"], 2)
        self.assertIn("only the host", reg["ledger"][1]["last"])
        self.assertTrue(self.op(self.A, op="found", name="Ironworks", open=True))
        self.assertTrue(self.op(self.B, op="join", company="c1"))
        self.assertEqual(self.B.registry()["members"], {1: "c1"})
        self.assertSameRegistry()

    def test_a_limit_per_period_starts_again_with_the_next_period(self):
        self.start()
        self.tick(10)
        self.assertTrue(self.op(self.A, op="limit", player=1, amount=10000, per=1000000))
        self.assertTrue(self.buy(self.B))
        self.assertFalse(self.buy(self.B), "30 000 of 10 000 in this period")
        self.assertEqual(self.A.registry()["ledger"][1]["per"], {"idx": 0, "gross": 30000})
        self.assertEqual(row(self.A.status(), 1)["limit"], {"amount": 10000, "per": 1000000})
        for game in (self.A, self.B):          # a million game-time units later
            game.L.execute("GAME.time = GAME.time + 1000000")
        self.tick(10)
        self.assertTrue(self.buy(self.B), "a new period, a fresh count")
        self.assertEqual(self.A.registry()["ledger"][1]["per"]["idx"], 1)
        self.assertEqual(self.A.registry()["ledger"][1]["gross"]["spend"], 60000, "the history is kept")
        self.assertSameRegistry()

    # ---- separate companies (experimental): attribution on every game

    def separate(self, start_balance=2000000):
        """The host's company is the savegame's player; a second company is a player entity added by command; the client
        belongs to it. Then the host switches the mode."""
        self.A.L.execute("START_BALANCE = %d" % start_balance)
        self.B.L.execute("START_BALANCE = %d" % start_balance)
        self.assertTrue(self.op(self.A, op="found", name="Host Co", entity="s:7"))
        self.add_company("Ironworks")
        self.assertTrue(self.op(self.A, op="assign", player=0, company="c1"))
        self.assertTrue(self.op(self.A, op="assign", player=1, company="c2"))
        self.assertTrue(self.op(self.A, op="mode", mode="separate", experimental=True))

    def test_adding_a_player_binds_a_company_to_the_new_entity_on_every_game(self):
        self.start()
        self.tick(10)
        self.add_company("Ironworks")
        ea, eb = self.both("GAME.applied[1].entity")
        self.assertNotEqual(ea, eb, "the entity has another id in each game")
        reg = self.A.registry()
        c = reg["companies"]["c1"]
        self.assertEqual((c["name"], c["entity"], c["owner"]), ("Ironworks", "0:1", 0))
        self.assertEqual([c["color"][1], c["color"][2], c["color"][3]], [0.9, 0.2, 0.2])
        self.assertSameRegistry()
        self.assertEqual(self.A.ev("WORLD[%d].NAME" % ea), "Ironworks")
        # the key is bound to this game's own entity (the simulation state keeps the binding)
        self.assertEqual((self.A.ev("STATE:get().bind['0:1']"), self.B.ev("STATE:get().bind['0:1']")), (ea, eb))
        # a client cannot add players: every game refuses, no entity appears
        self.B.send_gui('api.cmd.sendCommand(api.cmd.makeGameAddPlayerCmd("Mine", api.type.Vec3f.new(1, 1, 1)))')
        self.tick(30)
        self.assertEqual(self.both("#GAME.applied"), (1, 1))
        self.assertEqual(len(self.A.registry()["companies"]), 1)

    def test_player_arguments_become_the_senders_company_entity_on_each_game(self):
        self.start()
        self.tick(10)
        self.separate()
        pa, pb = self.both("GAME.applied[1].entity")
        self.assertNotEqual(pa, pb)
        self.assertSameRegistry()
        self.assertEqual(self.A.registry()["mode"], "separate")
        # the client's window passes its own player (7); every game buys for the client's company, with its own id
        self.assertTrue(self.buy(self.B))
        va, vb = self.both("GAME.applied[2].entity")
        self.assertEqual((self.A.ev("OWNER[%d]" % va), self.B.ev("OWNER[%d]" % vb)), (pa, pb))
        self.assertEqual(self.A.ev("GAME.applied[2].args[1]"), pa)
        self.assertEqual(self.B.ev("GAME.applied[2].args[1]"), pb)
        # the money left that company's account, on both games; the savegame's player was not touched
        self.assertEqual(self.A.ev("ACCOUNTS[%d].balance" % pa), 2000000 - 30000)
        self.assertEqual(self.B.ev("ACCOUNTS[%d].balance" % pb), 2000000 - 30000)
        self.assertEqual(self.both("ACCOUNT.balance"), (1000000, 1000000))
        # and the ledger measured it there
        self.assertEqual(self.A.registry()["ledger"][1]["gross"], {"spend": 30000})
        self.assertSameRegistry()
        # the host acts as its company, which is the savegame's player (key s:7)
        self.assertTrue(self.line(self.A))
        la = self.A.ev("GAME.applied[3].entity")
        self.assertEqual(self.A.ev("OWNER[%d]" % la), 7)
        self.assertEqual(self.both("ACCOUNT.balance"), (1000000 - 5000, 1000000 - 5000))
        self.assertEqual(self.A.ev("GAME.applied[3].args[3]"), 7)
        self.assertEqual(self.B.ev("GAME.applied[3].args[3]"), 7)
        self.assertEqual(self.A.applied(), self.B.applied())
        self.assertSameRegistry()

    def test_a_player_outside_every_company_acts_as_the_games_own_player(self):
        self.start()
        self.tick(10)
        self.separate()
        body = self.relay.C.serialize(self.relay.C.parse(
            '{["fn"]="makeVehicleBuyCmd",["args"]={["n"]=3,[1]={["__ref"]="player"},[2]={["__ref"]="entity",["key"]="s:55"},[3]={}}}'))
        stamp = self.A.time() + 6 * STEP
        payload = self.relay.ser({"stamp": stamp, "seq": 4, "body": body, "from": 2})
        self.relay.send(self.A, "action", 2, payload)
        self.relay.send(self.B, "action", 2, payload)
        self.tick(30)
        self.assertEqual(self.A.ev("GAME.applied[#GAME.applied].args[1]"), 7)
        self.assertEqual(self.B.ev("GAME.applied[#GAME.applied].args[1]"), 7)

    def test_a_company_whose_entity_is_not_known_yet_refuses_instead_of_guessing(self):
        self.start()
        self.tick(10)
        self.separate()
        self.B.L.execute("local st = STATE:get(); st.bind['0:2'] = nil; STATE:set(st)")      # this game lost the binding
        before = len(self.B.applied())
        self.buy(self.B)
        self.assertEqual(len(self.B.applied()), before, "the client's game did not buy with a wrong player")
        self.assertEqual(len(self.A.applied()), before + 1, "the host's game, which knows the entity, did: the games now differ (a checksum shows it)")
        self.assertIn("not known on this game", self.B.log())

    def test_a_payment_between_companies_is_a_journal_entry_on_each_account(self):
        self.start()
        self.tick(10)
        self.separate()
        pa, pb = self.both("GAME.applied[1].entity")
        self.assertTrue(self.op(self.B, op="pay", to=0, amount=12000, memo="the station"))
        self.assertEqual(self.A.ev("ACCOUNTS[%d].balance" % pa), 2000000 - 12000)
        self.assertEqual(self.B.ev("ACCOUNTS[%d].balance" % pb), 2000000 - 12000)
        self.assertEqual(self.both("ACCOUNT.balance"), (1000000 + 12000, 1000000 + 12000))
        self.assertEqual(self.A.ev("JOURNALS[%d][1]" % pa), -12000)
        self.assertEqual(self.A.ev("JOURNALS[7][1]"), 12000)
        reg = self.A.registry()
        self.assertEqual((reg["ledger"][1]["paid"], reg["ledger"][0]["received"]), (12000, 12000))
        self.assertSameRegistry()
        self.assertEqual(self.both("#GAME.applied"), (1, 1), "a journal entry is not a replicated action")
        # the host charges the client a fee
        self.assertTrue(self.op(self.A, op="pay", to=0, amount=500, **{"from": 1}))
        self.assertEqual(self.B.ev("ACCOUNTS[%d].balance" % pb), 2000000 - 12500)

    def test_in_shared_mode_a_payment_is_only_a_ledger_entry(self):
        self.start()
        self.tick(10)
        self.assertTrue(self.op(self.B, op="pay", to=0, amount=12000))
        self.assertEqual(self.both("ACCOUNT.balance"), (1000000, 1000000))
        self.assertEqual(self.both("#JOURNAL"), (0, 0))
        self.assertEqual(self.A.registry()["ledger"][1]["paid"], 12000)
        self.assertSameRegistry()

    def test_the_games_windows_follow_the_registry_in_separate_mode_and_are_restored(self):
        self.start()
        self.tick(10)
        self.assertEqual(self.B.ev("GUI_API.engine.util.getPlayer()"), 7)
        self.separate()
        pa, pb = self.both("GAME.applied[1].entity")
        self.assertEqual(self.B.ev("GUI_API.engine.util.getPlayer()"), pb, "the client's windows act as its company")
        self.assertEqual(self.A.ev("GUI_API.engine.util.getPlayer()"), 7, "the host's company is the savegame's player")
        self.assertEqual(self.B.ev("SIM_ENV.api.engine.util.getPlayer()"), 7, "the simulation half is never switched")
        self.assertIn("local company: entity %d" % pb, self.B.log())
        # the window's own purchase passes getPlayer() and still lands on the right company
        self.assertTrue(self.buy(self.B))
        self.assertEqual(self.B.ev("ACCOUNTS[%d].balance" % pb), 2000000 - 30000)
        self.assertEqual(self.B.ev("GAME.applied[2].args[1]"), pb)
        # leaving the company, or switching back to shared, restores the game's own player
        self.assertTrue(self.op(self.B, op="leave"))
        self.assertEqual(self.B.ev("GUI_API.engine.util.getPlayer()"), 7)
        self.assertTrue(self.op(self.A, op="assign", player=1, company="c2"))
        self.assertEqual(self.B.ev("GUI_API.engine.util.getPlayer()"), pb)
        self.assertTrue(self.op(self.A, op="mode", mode="shared"))
        self.assertEqual(self.B.ev("GUI_API.engine.util.getPlayer()"), 7)
        self.assertIn("the game's own player", self.B.log())

    def test_the_hosts_money_authority_still_corrects_the_savegames_player_in_separate_mode(self):
        self.start()
        self.tick(10)
        self.separate()
        pa, pb = self.both("GAME.applied[1].entity")
        self.assertEqual(self.B.ev("GUI_API.engine.util.getPlayer()"), pb)
        self.B.L.execute("ACCOUNT.balance = ACCOUNT.balance + 5000")        # the savegame's player drifts on the client
        self.relay.pump()
        self.relay.checksum_request(1, self.relay.future_stamp(), False)
        self.tick(40)
        pa_, pb_ = self.relay.reports[1][0][1], self.relay.reports[1][1][1]
        self.assertIn("balance", {k for k in pa_ if pa_[k] != pb_[k]})
        self.relay.send_authority(1)
        self.tick(6)
        self.assertEqual(self.B.ev("ACCOUNT.balance"), 1000000, "corrected")
        self.assertEqual(self.B.ev("JOURNALS[7][1]"), -5000)
        self.assertEqual(self.B.ev("ACCOUNTS[%d].balance" % pb), 2000000, "the client's company, which the windows show, was left alone")
        self.assertIsNone(self.B.ev("JOURNALS[%d]" % pb))

    # ---- during a pause

    def test_registry_and_ledger_changes_made_while_paused_end_up_in_the_same_state(self):
        self.start(speed=2)
        self.tick(10, lag_b=True)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(40, lag_b=True)
        self.assertEqual((self.A.time(), self.B.time()), (self.relay.pauseAt, self.relay.pauseAt))
        # both are applied by the interface halves, at once
        self.A.send_gui('api.cmd.sendCommand(api.cmd.feverlink3Company({ op = "limit", player = 1, amount = 50000 }), function(d, s) PAUSED_OP = s end)')
        self.tick(8)
        self.assertTrue(self.A.ev("PAUSED_OP"))
        self.B.send_gui("api.cmd.sendCommand(api.cmd.makeVehicleBuyCmd(7, 55, { vehicles = {} }), function(d, s) PAUSED_BUY = s end)")
        self.tick(8)
        self.assertTrue(self.B.ev("PAUSED_BUY"))
        self.assertEqual((self.A.time(), self.B.time()), (self.relay.pauseAt, self.relay.pauseAt), "still paused")
        self.assertEqual(self.A.applied(), self.B.applied())
        self.assertSameRegistry()
        reg = self.A.registry()
        self.assertEqual(reg["limits"], {1: {"amount": 50000}})
        self.assertEqual((reg["ledger"][1]["n"], reg["ledger"][1]["gross"]), ({"spend": 1}, {"spend": 30000}))
        self.assertIn("1:1", reg["made"])
        # the company page, written by the interface half from its mirror, shows the same
        self.assertEqual(row(self.B.status(), 1)["total"], 30000)
        self.assertEqual(row(self.A.status(), 1)["limit"]["amount"], 50000)
        # forged actions of player 1 delivered to the host's game during the pause: the first is still under the limit, the
        # second is not; it is refused on the interface half and replayed as a refusal into the state the savegame keeps
        body = self.relay.C.serialize(self.relay.C.parse(
            '{["fn"]="makeVehicleBuyCmd",["args"]={["n"]=3,[1]={["__ref"]="player"},[2]={["__ref"]="entity",["key"]="s:55"},[3]={}}}'))
        self.relay.send(self.A, "action", 1, self.relay.ser({"stamp": self.relay.pauseAt, "seq": 9, "body": body, "from": 1}))
        self.tick(8)
        self.assertEqual(self.A.registry()["ledger"][1]["refused"], 0, "30 000 of 50 000 is still under the limit")
        self.assertEqual(self.A.applied()[-1].split("@")[0], "makeVehicleBuyCmd")
        self.relay.send(self.A, "action", 1, self.relay.ser({"stamp": self.relay.pauseAt, "seq": 10, "body": body, "from": 1}))
        self.tick(8)
        self.assertEqual(self.A.registry()["ledger"][1]["refused"], 1, "60 000 of 50 000: refused, and the refusal is in the state the savegame keeps")
        self.assertEqual(row(self.A.status(), 1)["refused"], 1)

    def test_a_mod_level_action_during_a_pause_is_replayed_into_the_state(self):
        self.start(speed=2)
        self.tick(10, lag_b=True)
        self.relay.pump()
        self.relay.pauseAt = self.relay.future_stamp()
        self.relay.session()
        self.tick(40, lag_b=True)
        self.assertTrue(self.op(self.A, ticks=8, op="found", name="Ironworks", open=True))
        self.assertTrue(self.op(self.B, ticks=8, op="join", company="c1"))
        self.assertTrue(self.op(self.A, ticks=8, op="found", name="Bakery"))      # two operations in a row, each sees the one before
        self.assertSameRegistry()
        reg = self.A.registry()
        self.assertEqual((sorted(reg["companies"]), reg["members"]), (["c1", "c2"], {1: "c1"}))
        # the game resumes: the registry goes on from where the pause left it
        self.relay.pauseAt, self.relay.speed = None, 2
        self.relay.session()
        self.tick(20)
        self.assertTrue(self.op(self.A, op="found", name="Mill"))
        self.assertSameRegistry()
        self.assertEqual(sorted(self.A.registry()["companies"]), ["c1", "c2", "c3"])
        self.assertEqual(self.B.status()["companies"][1]["name"], "Ironworks")

    # ---- persistence, the window, faults

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
        self.assertEqual(status["companies"][1]["members"], {1: 1})
        # the reloaded mod goes on: the limit still applies. (The window's callback is not asked: the reloaded mod numbers
        # its actions from 1 again, so the key of the first one is a key the state already holds a result for.)
        applied = len(self.A.applied())
        self.buy(self.B)
        self.assertEqual((len(self.A.applied()), len(self.B.applied())), (applied + 1, applied + 1))
        self.buy(self.B)
        self.assertEqual((len(self.A.applied()), len(self.B.applied())), (applied + 1, applied + 1), "the limit (50 000, 60 000 spent) stops the next one")
        self.assertSameRegistry()

    def test_the_company_page_is_written_for_the_window(self):
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
        self.tick(300)           # the earnings of lines are read again every few seconds
        for game in (self.A, self.B):
            st = game.status()
            self.assertEqual(row(st, 1)["earned"], 1700)
            self.assertEqual(row(st, 1)["net"], 1700 - 5000)

    def test_a_window_record_becomes_a_company_action(self):
        self.start()
        self.tick(10)
        self.assertTrue(self.A.ui_company(op="found", name="Ironworks", open=True))
        self.tick(30)
        self.assertEqual(sorted(self.A.registry()["companies"]), ["c1"])
        self.assertSameRegistry()
        self.assertTrue(self.B.ui_company(op="join", company="c1"))
        self.tick(30)
        self.assertEqual(self.A.registry()["members"], {1: "c1"})
        self.assertSameRegistry()
        # a record the module does not know is ignored, one that is no table too
        self.assertTrue(self.A.ui_company(op="format the disk"))
        self.tick(10)
        # "add" is the engine command that adds a player
        self.assertTrue(self.A.ui_company(op="add", name="Mill", color=[0.1, 0.2, 0.3]))
        self.tick(30)
        self.assertEqual(self.A.registry()["companies"]["c2"]["name"], "Mill")
        self.assertEqual(self.both("#GAME.applied"), (1, 1))
        self.assertSameRegistry()

    def test_a_fault_in_the_company_layer_never_loses_the_players_action(self):
        self.start()
        self.tick(10)
        self.assertTrue(self.op(self.A, op="found", name="Ironworks"))
        self.B.L.execute("local st = STATE:get(); st.co.limits = 5; STATE:set(st)")    # garbage in the registry of one game
        self.assertTrue(self.buy(self.B))
        self.assertEqual(self.A.applied(), self.B.applied(), "the purchase went through on both")
        self.assertIn("company layer failed", self.B.log())

    def test_a_forged_company_action_with_a_bad_shape_is_refused_everywhere(self):
        self.start()
        self.tick(10)
        for body in ('{["fn"]="feverlink3Company",["args"]={["n"]=1,[1]="limit"}}',
                     '{["fn"]="feverlink3Company",["args"]={["n"]=1,[1]={["op"]=5}}}',
                     '{["fn"]="feverlink3Company",["args"]={["n"]=1,[1]={["op"]="limit",["player"]="x",["amount"]=1}}}',
                     '{["fn"]="feverlink3Company",["args"]={["n"]=0}}'):
            payload = self.relay.ser({"stamp": self.A.time() + 6 * STEP, "seq": 20, "body": self.relay.C.serialize(self.relay.C.parse(body)), "from": 0})
            self.relay.send(self.B, "action", 0, payload)
            self.tick(20)
        reg = self.B.registry()
        self.assertEqual((reg["limits"], reg["companies"]), ({}, {}))
        self.assertFalse(self.log_has("company layer failed"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
