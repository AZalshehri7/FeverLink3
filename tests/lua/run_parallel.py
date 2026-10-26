"""The lupa suite on several processes: python -I tests/lua/run_parallel.py [-j N] [-v] [pattern ...]

The same tests as `python -I -m unittest discover -s tests/lua -p "test_*.py"`, spread over worker processes one test at a
time (a class with a setUpClass / setUpModule fixture goes to one worker whole; modules that import each other stay
importable because every worker has tests/lua on sys.path, as discover gives it). Why: the suite is about 200 independent two-game simulations,
85 s on one core (2026-10-26); the classes share nothing but files under their own temp folders.

Patterns (optional) select the tests whose id contains one of them ("test_lockstep", "StallTest"). Exit code 0 only when every
selected test passed, none errored, and the workers ran exactly the tests discover found (a module that fails to import, or
a worker that dies, is a failure, never a smaller suite). Each worker's own output (the mocked games' prints) goes to a temp
file and is shown for the tests that failed. FEVERLINK3_FULL_TESTS and the other switches pass to the workers unchanged.
"""
import argparse
import concurrent.futures as cf
import io
import os
import sys
import tempfile
import time
import traceback
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def _shared_fixture(t):
    """True when the test's class or module has a fixture its tests share (they must then run in one process)."""
    cls = type(t)
    if cls.setUpClass.__func__ is not unittest.TestCase.setUpClass.__func__ or cls.tearDownClass.__func__ is not unittest.TestCase.tearDownClass.__func__:
        return True
    mod = sys.modules.get(cls.__module__)
    return bool(mod and (hasattr(mod, "setUpModule") or hasattr(mod, "tearDownModule")))


def _units(suite):
    """({unit id: [test ids]} in discover's order: one test each, or a whole class when it shares a fixture, [the modules that
    failed to import: unittest's _FailedTest objects, run here so their error text is shown])."""
    out, broken = {}, []
    stack = [suite]
    while stack:
        s = stack.pop(0)
        for t in s:
            if isinstance(t, unittest.TestSuite):
                stack.append(t)
            elif type(t).__name__ == "_FailedTest":
                broken.append(t)
            else:
                uid = t.id().rsplit(".", 1)[0] if _shared_fixture(t) else t.id()
                out.setdefault(uid, []).append(t.id())
    return out, broken


def _discover():
    if HERE not in sys.path:
        sys.path.insert(0, HERE)
    return unittest.defaultTestLoader.discover(HERE, pattern="test_*.py", top_level_dir=HERE)


_LOG = None


def _init(logdir):
    global _LOG
    if HERE not in sys.path:
        sys.path.insert(0, HERE)
    # the Lua print() of the mocked games writes to the C stdout: send this worker's fd 1 and 2 to its own file
    _LOG = os.path.join(logdir, "worker-%d.txt" % os.getpid())
    f = open(_LOG, "ab", buffering=0)
    sys.stdout.flush()
    sys.stderr.flush()
    os.dup2(f.fileno(), 1)
    os.dup2(f.fileno(), 2)
    sys.stdout = io.TextIOWrapper(os.fdopen(os.dup(1), "wb", buffering=0), write_through=True, errors="replace")
    sys.stderr = sys.stdout


def _run(cid, ids):
    t0 = time.perf_counter()
    start = os.path.getsize(_LOG) if os.path.exists(_LOG) else 0
    try:
        suite = unittest.defaultTestLoader.loadTestsFromNames(ids)
        stream = io.StringIO()
        res = unittest.TextTestRunner(stream=stream, verbosity=0).run(suite)
        bad = [(t.id(), tb) for t, tb in res.failures + res.errors + [(t, "unexpected success") for t in res.unexpectedSuccesses]]
        out = {"ran": res.testsRun, "skipped": len(res.skipped), "bad": bad}
    except BaseException:
        out = {"ran": 0, "skipped": 0, "bad": [(cid, traceback.format_exc())]}
    sys.stdout.flush()
    out["secs"] = time.perf_counter() - t0
    out["log"] = (_LOG, start)
    return cid, out


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("-j", type=int, default=max(1, min(os.cpu_count() or 2, 12)), help="worker processes (default: cores, at most 12)")
    ap.add_argument("-v", action="store_true", help="one line per unit (a test, or a class run whole) with its time")
    ap.add_argument("patterns", nargs="*")
    a = ap.parse_args()
    t0 = time.perf_counter()
    classes, broken = _units(_discover())
    if a.patterns:
        classes = {c: ids for c, ids in classes.items() if any(p in c for p in a.patterns)}
        broken = [t for t in broken if any(p in t.id() for p in a.patterns)]
    # a module that does not import is a failure of the run, never a smaller suite
    bad = []
    if broken:
        res = unittest.TestResult()
        unittest.TestSuite(broken).run(res)
        bad += [(t.id(), tb) for t, tb in res.errors + res.failures]
    want = sum(len(v) for v in classes.values())
    if want == 0 and not bad:
        print("run_parallel: no test found (patterns %s)" % a.patterns)
        return 1
    # whole classes first (the largest units), then the single tests in discover's order
    order = sorted(classes, key=lambda c: -len(classes[c]))
    ran = skipped = 0
    rows = []
    with tempfile.TemporaryDirectory(prefix="fl3-lua-par-") as logdir:
        with cf.ProcessPoolExecutor(max_workers=a.j, initializer=_init, initargs=(logdir,)) as ex:
            futs = [ex.submit(_run, c, classes[c]) for c in order]
            for f in cf.as_completed(futs):
                try:
                    cid, r = f.result()
                except BaseException:
                    bad.append(("(a worker process)", traceback.format_exc()))
                    continue
                ran += r["ran"]
                skipped += r["skipped"]
                rows.append((r["secs"], cid, r["ran"]))
                if r["bad"]:
                    path, start = r["log"]
                    try:
                        with open(path, "rb") as fh:
                            fh.seek(start)
                            tail = fh.read().decode("utf-8", "replace")[-4000:]
                    except OSError:
                        tail = ""
                    bad += [(i, tb + ("\n--- worker output (this unit and whatever ran after it on that worker):\n" + tail if tail else "")) for i, tb in r["bad"]]
    wall = time.perf_counter() - t0
    if a.v:
        for s, cid, n in sorted(rows, reverse=True):
            print("%6.2f s  %3d  %s" % (s, n, cid))
    for i, tb in bad:
        print("=" * 70 + "\nFAIL: %s\n%s" % (i, tb))
    missing = want - ran
    print("-" * 70)
    print("Ran %d of %d tests in %.1f s on %d workers (%.1f s of work), skipped %d, failed %d%s"
          % (ran, want, wall, a.j, sum(r[0] for r in rows), skipped, len(bad), "" if not missing else ", %d NOT RUN" % missing))
    ok = not bad and missing == 0
    print("OK" if ok else "FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
