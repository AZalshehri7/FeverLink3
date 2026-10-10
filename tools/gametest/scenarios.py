"""Scenarios of the system test. Each one acts in one game and checks that the two worlds are identical afterwards.

    python scenarios.py economy | names | roads | all

The games must be running and connected (full_setup / demo_setup), started with FEVERLINK3_DEV=1, and the mod window open in
both games (the probe that issues window commands runs from the window's timer). The scenarios that need mouse clicks (the
game's own tools) are driven by clicking.ps1-style helpers from the repo's ui.ps1; the ones that issue commands go through the
UI state's own api, the same one the game's windows use, so the UI hook sees them as it sees a click.
"""
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import systest as S  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
UI_PS1 = os.path.join(HERE, "ui.ps1")
RESULTS = []


def pids():
    with open(os.path.join(os.environ["TEMP"], "fl3-pids.txt")) as f:
        left, right = f.read().split()
    return int(left), int(right)


def ps(script):
    """Runs PowerShell with the UI helpers loaded."""
    cmd = ". '%s'; %s" % (UI_PS1, script)
    r = subprocess.run(["powershell", "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", cmd], capture_output=True, text=True, timeout=120)
    if r.returncode != 0:
        raise RuntimeError(r.stderr[:400])
    return r.stdout


def click(game, x, y, wait=1.0):
    left, right = pids()
    pid = left if game == "host" else right
    ps("Click-Win %d %d %d; Start-Sleep -Milliseconds %d" % (pid, x, y, int(wait * 1000)))


def key(game, vk):
    left, right = pids()
    pid = left if game == "host" else right
    ps("Focus-Game %d; Send-Key %d $true; Start-Sleep -Milliseconds 100; Send-Key %d $false; Start-Sleep -Milliseconds 400" % (pid, vk, vk))


def shot(name, game=None):
    x = 0 if game == "host" else 1280 if game == "guest" else 0
    w = 1280 if game else 2560
    ps("Save-Region (Join-Path $env:TEMP 'fl3-gametest\\%s.png') %d 0 %d 1396 1100" % (name, x, w))


def ui(game, lua):
    """Runs Lua in the UI state of the given game, opening its mod window first if the probe does not answer (the probe runs
    from the window's timer, and a world reload closes the window)."""
    g = S.HOST if game == "host" else S.GUEST
    try:
        return g.probe("ui", lua, timeout=5)
    except TimeoutError:
        click(game, 747, 1338, 3.0)
        return g.probe("ui", lua, timeout=20)


# The freeze and the resume use the game's own pause and play buttons in the bottom bar (the same path a player takes).
BAR_PAUSE, BAR_PLAY, BAR_2X, BAR_3X, BAR_Y = 1087, 1104, 1119, 1135, 1336


def native_freeze(timeout=40):
    click("host", BAR_PAUSE, BAR_Y, 1.0)
    end = time.time() + timeout
    while time.time() < end:
        time.sleep(1.5)
        a, b = S.HOST.fingerprint(), S.GUEST.fingerprint()
        if a.get("time") == b.get("time"):
            time.sleep(1.0)
            a2 = S.HOST.fingerprint()
            if a2.get("time") == a.get("time"):
                return a2, S.GUEST.fingerprint()
    raise TimeoutError("the two games did not stop at one game time")


def native_resume(speed=1):
    click("host", {1: BAR_PLAY, 2: BAR_2X, 3: BAR_3X}[speed], BAR_Y, 1.0)


S.freeze, S.resume = native_freeze, native_resume


DEV_BUTTON = (400, 340)   # the DEV run button: the first row of the mod window (calibrated on a screenshot)


def dev(game, lua):
    """Runs Lua in a game's UI state from inside a click on the window's DEV button (the one place the engine lets a
    command be sent from). Opens the window first when the button does not answer."""
    g = S.HOST if game == "host" else S.GUEST
    try:
        return g.click_run(lua, lambda: click(game, DEV_BUTTON[0], DEV_BUTTON[1], 0.5), timeout=6)
    except TimeoutError:
        click(game, 747, 1338, 3.0)
        return g.click_run(lua, lambda: click(game, DEV_BUTTON[0], DEV_BUTTON[1], 0.5), timeout=20)


def record(label, ok):
    RESULTS.append((label, ok))


def changed_sections(before, after):
    """Which parts of the world differ between two fingerprints of the same game (time does not count)."""
    out = []
    for k in sorted(set(before) | set(after)):
        if k == "time":
            continue
        va, vb = before.get(k), after.get(k)
        if isinstance(va, dict) and isinstance(vb, dict) and "hash" in va:
            if va.get("hash") != vb.get("hash"):
                out.append("%s %s->%s" % (k, va.get("n"), vb.get("n")))
        elif va != vb:
            out.append("%s %s->%s" % (k, va, vb))
    return out


def run(label, fn, expect=None, effect=True):
    """Runs a scenario step. PASS needs (1) both worlds identical at one game time, (2) no resync and no differing checksum
    round meanwhile, and (3) when `effect` is true, that the action changed the world (otherwise it did nothing and the
    comparison proves nothing). `expect` is a section name that must be among the changes."""
    S.mark()
    try:
        before = S.HOST.fingerprint()
        fn()
        ok, problems = S.check(label)
        after = S.HOST.fingerprint()
        changes = changed_sections(before, after)
        if effect and not changes:
            print("FAIL  %s: the action changed nothing (the click or command did not take effect)" % label)
            ok = False
        elif expect and not any(c.startswith(expect) for c in changes):
            print("FAIL  %s: expected a change in %s, saw %s" % (label, expect, changes))
            ok = False
        else:
            print("      changed: %s" % (", ".join(changes) or "nothing (expected)"))
        record(label, ok)
    except Exception as e:  # noqa: BLE001
        print("FAIL  %s: %s" % (label, e))
        record(label, False)


def summary():
    print("\n==== summary ====")
    for label, ok in RESULTS:
        print(("PASS  " if ok else "FAIL  ") + label)
    print("%d of %d passed" % (sum(1 for _, ok in RESULTS if ok), len(RESULTS)))


# ---------------------------------------------------------------------------------------------------- scenarios

def economy():
    # the Account window: Loans tab, offers at the top, obtained below. The host takes the third offer, the guest the first.
    left, right = pids()

    def obtain(game, offer_x):
        click(game, 62, 1305, 2.5)       # Account
        click(game, 372, 419, 2.0)       # Loans
        click(game, offer_x, 624, 2.5)   # Obtain
        click(game, 1232, 21, 1.0)       # close
    run("loan obtained on the host (real window)", lambda: obtain("host", 336))
    run("loan obtained on the guest (real window)", lambda: obtain("guest", 533))


def entities():
    return S.HOST.probe("sim", """
local CT = api.type.ComponentType
local function all(c) local t = {} api.engine.forEachEntityWithComponent(function(e) t[#t+1] = e end, CT[c]) table.sort(t) return t end
local t, g = all("TOWN"), all("STATION_GROUP")
return { town = t[1], town2 = t[2], town3 = t[3], group = g[1], group2 = g[2], player = api.engine.util.getPlayer() }
""")


def names():
    ids = entities()

    def rename(game, entity, name):
        dev(game, 'api.cmd.sendCommand(api.cmd.makeEntitySetNameCmd(%d, "%s")); return "sent"' % (entity, name))
    run("town renamed on the host", lambda: rename("host", ids["town"], "Testville"), expect="towns")
    run("town renamed on the guest", lambda: rename("guest", ids["town2"], "Guestburg"), expect="towns")
    run("company renamed on the host", lambda: rename("host", ids["player"], "Anna Rail Co"), expect="companyName")
    run("station group renamed on the guest", lambda: rename("guest", ids["group"], "Central Yard"), expect="stationGroups")


def lines():
    ids = entities()
    make = """
local g1, g2 = %d, %d
local function stop(group)
  local st = api.type.Line.Stop.new()
  st.stationGroup = group
  st.station = 0
  st.terminal = 0
  return st
end
local line = api.type.Line.new()
line.stops = { stop(g1), stop(g2) }
local cmd = api.cmd.makeLineCreateCmd("%s", api.type.Vec3f.new(0.8, 0.2, 0.2), api.engine.util.getPlayer(), line)
api.cmd.sendCommand(cmd, function(res, ok) end)
return "sent"
"""
    run("line created on the host", lambda: dev("host", make % (ids["group"], ids["group2"], "Line One")), expect="lines")
    run("line created on the guest", lambda: dev("guest", make % (ids["group2"], ids["group"], "Line Two")), expect="lines")


def tool_road(game, a, b):
    """The game's road tool: open it, click two points on the map, leave the tool (Escape twice)."""
    click(game, 632, 1311, 2.0)               # the roads tool in the toolbar
    click(game, a[0], a[1], 1.0)
    click(game, b[0], b[1], 2.0)
    key(game, 0x1B)
    key(game, 0x1B)


def roads():
    run("road built on the host with the game's road tool", lambda: tool_road("host", (341, 1018), (574, 1053)), expect="streetEdges")
    run("road built on the guest with the game's road tool", lambda: tool_road("guest", (434, 1040), (597, 1110)), expect="streetEdges")


ALL = {"economy": economy, "names": names, "lines": lines, "roads": roads}

if __name__ == "__main__":
    which = sys.argv[1:] or ["all"]
    for w in which:
        for name, fn in (ALL.items() if w == "all" else [(w, ALL[w])]):
            print("\n#### %s" % name)
            fn()
    summary()
