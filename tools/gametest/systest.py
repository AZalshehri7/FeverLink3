"""System test driver for two running games (host = live1, guest = live2), started with FEVERLINK3_DEV=1 and connected.

It runs Lua in each game through the developer probe (dev_<half>.lua in the live folder), freezes both games at the same
game time (a pause is a session-wide stop at one stamp), takes an id-independent fingerprint of each world and diffs them.
Used by hand while two real games run:

    python systest.py fp                 fingerprint both, print the differences
    python systest.py freeze | resume    pause / resume the session through the game's own speed command
    python systest.py probe gui|sim|ui host|guest FILE.lua

Needs lupa (pip install lupa) to read the games' answers (Lua table literals).
"""
import os
import sys
import time

import lupa

BASE = os.path.join(os.environ.get("LOCALAPPDATA", ""), "FeverLink3")
HERE = os.path.dirname(os.path.abspath(__file__))
FINGERPRINT = os.path.join(HERE, "fingerprint.lua")
_L = lupa.LuaRuntime(unpack_returned_tuples=True)


def to_py(v):
    if lupa.lua_type(v) == "table":
        keys = list(v.keys())
        if keys and all(isinstance(k, int) for k in keys):
            return [to_py(v[k]) for k in sorted(keys)]
        return {str(k): to_py(v[k]) for k in keys}
    return v


class Game:
    def __init__(self, role, folder):
        self.role = role
        self.dir = os.path.join(BASE, folder)

    def _out(self, half):
        return os.path.join(self.dir, "dev_%s_out.txt" % half)

    def probe(self, half, lua, timeout=40):
        """Runs `lua` in the given Lua state of this game ('gui', 'sim' or 'ui') and returns what it returned."""
        out = self._out(half)
        before = os.path.getsize(out) if os.path.exists(out) else 0
        with open(os.path.join(self.dir, "dev_%s.lua" % half), "w", encoding="utf-8") as f:
            f.write(lua)
        end = time.time() + timeout
        while time.time() < end:
            if os.path.exists(out) and os.path.getsize(out) > before:
                with open(out, encoding="utf-8", errors="replace") as f:
                    f.seek(before)
                    text = f.read()
                for marker in ("RESULT: ", "ERROR: ", "COMPILE ERROR: "):
                    at = text.find(marker)
                    if at >= 0:
                        body = text[at + len(marker):].strip()
                        if marker == "RESULT: ":
                            return to_py(_L.eval(body))
                        raise RuntimeError("%s probe on %s: %s" % (half, self.role, body[:600]))
            time.sleep(0.4)
        raise TimeoutError("no answer from the %s probe of the %s (is its window open for 'ui'? is the game stepping for 'sim'?)" % (half, self.role))

    def fingerprint(self, half="gui"):
        with open(FINGERPRINT, encoding="utf-8") as f:
            return self.probe(half, f.read())


    def click_run(self, lua, click_fn, timeout=25):
        """Runs `lua` in the game's UI state from inside a click on the window's DEV button (commands may only be sent from
        there). `click_fn()` clicks the button. Returns what the script returned."""
        out = os.path.join(self.dir, "dev_click_out.txt")
        before = os.path.getsize(out) if os.path.exists(out) else 0
        with open(os.path.join(self.dir, "dev_click.lua"), "w", encoding="utf-8") as f:
            f.write(lua)
        click_fn()
        end = time.time() + timeout
        while time.time() < end:
            if os.path.exists(out) and os.path.getsize(out) > before:
                with open(out, encoding="utf-8", errors="replace") as f:
                    f.seek(before)
                    text = f.read()
                for marker in ("RESULT: ", "ERROR: ", "COMPILE ERROR: "):
                    at = text.find(marker)
                    if at >= 0:
                        body = text[at + len(marker):].strip()
                        if marker == "RESULT: ":
                            return to_py(_L.eval(body))
                        raise RuntimeError("click script on %s: %s" % (self.role, body[:600]))
            time.sleep(0.4)
        raise TimeoutError("the DEV run button of the %s did not answer (is its window open?)" % self.role)


HOST = Game("host", "live1")
GUEST = Game("guest", "live2")


def diff(a, b, ignore=("time",)):
    """What differs between two fingerprints, section by section (the hash says whether, the first items say what)."""
    problems = []
    for key in sorted(set(a) | set(b)):
        if key in ignore:
            continue
        va, vb = a.get(key), b.get(key)
        if va != vb:
            if isinstance(va, dict) and isinstance(vb, dict) and "hash" in va:
                problems.append("%s: host %s items (%s) vs guest %s items (%s); host starts %s, guest starts %s"
                                % (key, va.get("n"), va.get("hash"), vb.get("n"), vb.get("hash"), va.get("first"), vb.get("first")))
            else:
                problems.append("%s: host %r vs guest %r" % (key, va, vb))
    return problems


def ask(lua):
    """Runs `lua` in the UI state of whichever game has its mod window open (the probe runs from the window's timer)."""
    last = None
    for game in (GUEST, HOST):
        try:
            return game.probe("ui", lua, timeout=12)
        except TimeoutError as e:
            last = e
    raise last


def freeze(timeout=40):
    """Pauses the session (the guest asks, as its pause button does) and waits until both worlds stand at one game time."""
    ask("api.cmd.sendCommand(api.cmd.makeGameSetSpeedCmd(0)); return 'pause asked'")
    end = time.time() + timeout
    while time.time() < end:
        time.sleep(1.5)
        a, b = HOST.fingerprint(), GUEST.fingerprint()
        if a.get("time") == b.get("time"):
            time.sleep(1.0)
            a2 = HOST.fingerprint()
            if a2.get("time") == a.get("time"):
                return a2, GUEST.fingerprint()
    raise TimeoutError("the two games did not stop at one game time")


def resume(speed=1):
    ask("api.cmd.sendCommand(api.cmd.makeGameSetSpeedCmd(%d)); return 'resume asked'" % speed)


def session_lines():
    """The host's session log (resync rounds, checksum differences): a list of lines."""
    path = os.path.join(HOST.dir, "session.log")
    if not os.path.exists(path):
        return []
    with open(path, encoding="utf-8", errors="replace") as f:
        return f.read().splitlines()


def resyncs(lines=None):
    return sum(1 for l in (session_lines() if lines is None else lines) if " started: " in l and l.startswith("resync "))


def differing(lines=None):
    """How many checksum rounds differed (a resync repairs a mismatch, so a passing comparison after one proves nothing)."""
    return sum(1 for l in (session_lines() if lines is None else lines) if l.startswith("checksum ") and " differs in " in l)


_marks = {"lines": 0}


def mark():
    _marks["lines"] = len(session_lines())


def since_mark():
    return session_lines()[_marks["lines"]:]


def check(label, settle=3.0):
    """Waits a moment for replays to land, freezes, compares, resumes. Returns (ok, problems).

    Fails also when a resync round started or a checksum round differed since the last mark(): the games' own checks agreed
    with the comparison or they did not, and a resync would have hidden a difference."""
    time.sleep(settle)
    a, b = freeze()
    problems = diff(a, b)
    new = since_mark()
    if resyncs(new):
        problems.append("a resync round ran during this scenario (it repairs, and so hides, a difference)")
    if differing(new):
        problems.append("%d checksum round(s) differed during this scenario: %s" % (differing(new), [l for l in new if " differs in " in l][:3]))
    resume()
    mark()
    print(("PASS  " if not problems else "FAIL  ") + label)
    for p in problems:
        print("      " + p)
    return not problems, problems


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else "fp"
    if cmd == "fp":
        a, b = HOST.fingerprint(), GUEST.fingerprint()
        problems = diff(a, b)
        print("same" if not problems else "\n".join(problems))
    elif cmd == "freeze":
        freeze()
        print("frozen")
    elif cmd == "resume":
        resume()
        print("resumed")
    elif cmd == "probe":
        half, who, path = sys.argv[2], sys.argv[3], sys.argv[4]
        game = HOST if who == "host" else GUEST
        print(game.probe(half, open(path, encoding="utf-8").read()))
    else:
        sys.exit(__doc__)
