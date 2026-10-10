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


def record(label, ok):
    RESULTS.append((label, ok))


def run(label, fn):
    """Runs a scenario step and records PASS/FAIL (a crash is a FAIL with the reason printed)."""
    S.mark()
    try:
        fn()
        ok, problems = S.check(label)
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


def names():
    ids = S.HOST.probe("sim", """
local CT = api.type.ComponentType
local t, g = {}, {}
api.engine.forEachEntityWithComponent(function(e) t[#t+1] = e end, CT.TOWN)
api.engine.forEachEntityWithComponent(function(e) g[#g+1] = e end, CT.STATION_GROUP)
table.sort(t); table.sort(g)
return { town = t[1], town2 = t[2], group = g[1], player = api.engine.util.getPlayer() }
""")

    def rename(game, entity, name):
        ui(game, "api.cmd.sendCommand(api.cmd.makeEntitySetNameCmd(%d, %s)); return 'sent'" % (entity, '"' + name + '"'))
    run("town renamed on the host", lambda: rename("host", ids["town"], "Testville"))
    run("town renamed on the guest", lambda: rename("guest", ids["town2"], "Guestburg"))
    run("company renamed", lambda: rename("host", ids["player"], "Anna Rail Co"))
    run("station group renamed", lambda: rename("guest", ids["group"], "Central Yard"))


ALL = {"economy": economy, "names": names}

if __name__ == "__main__":
    which = sys.argv[1:] or ["all"]
    for w in which:
        for name, fn in (ALL.items() if w == "all" else [(w, ALL[w])]):
            print("\n#### %s" % name)
            fn()
    summary()
