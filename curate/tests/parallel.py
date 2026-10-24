"""The suite across processes: `python3 -m tests.parallel [-j N] [name ...]`.

Nearly all of a test's time is waiting on the processes it starts -- git, and
curate, and on a terminal a pause before each key -- so the suite runs about
as many times faster as there are workers, up to the cores to run them on.

Safe because every test makes its own throwaway repository and nothing else:
no shared files, no class- or module-level fixtures, nothing written to the
environment of the process the test runs in. A test that needs any of those
is a test that cannot run here, and has to say so before it is written.

One test per task rather than one file, so a slow file does not leave the
other workers idle at the end. Each worker runs its tests on its main thread,
which the Ctrl-C tests need. Failures are reported as `unittest` reports them,
`FAIL: name (id)`, so either runner's output reads the same.
"""

import argparse
import io
import os
import sys
import time
import unittest
from concurrent.futures import ProcessPoolExecutor, as_completed
from typing import Iterator


def ids(suite: unittest.TestSuite | unittest.TestCase) -> Iterator[str]:
    if isinstance(suite, unittest.TestCase):
        yield suite.id()
        return
    for test in suite:
        yield from ids(test)


def run_one(test_id: str) -> tuple[str, str, str]:
    """(id, outcome, report) for one test, run in this worker."""
    test = unittest.defaultTestLoader.loadTestsFromName(test_id)
    stream = io.StringIO()
    result = unittest.TextTestResult(unittest.runner._WritelnDecorator(stream),
                                     descriptions=False, verbosity=0)
    test.run(result)
    for outcome, found in (("ERROR", result.errors), ("FAIL", result.failures),
                           ("FAIL", result.unexpectedSuccesses)):
        if found:
            entry = found[0]
            text = entry[1] if isinstance(entry, tuple) else ""
            return test_id, outcome, text
    if result.skipped:
        return test_id, "skip", ""
    return test_id, "ok", ""


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="python3 -m tests.parallel")
    parser.add_argument("-j", "--jobs", type=int, default=os.cpu_count() or 1)
    parser.add_argument("names", nargs="*",
                        help="tests, classes or modules; all of them by default")
    args = parser.parse_args(argv)

    loader = unittest.defaultTestLoader
    suite = (loader.loadTestsFromNames(args.names) if args.names else
             loader.discover("tests", top_level_dir="."))
    todo = list(ids(suite))
    began = time.perf_counter()
    marks = {"ok": ".", "skip": "s", "FAIL": "F", "ERROR": "E"}
    bad: list[tuple[str, str, str]] = []
    skipped = 0
    with ProcessPoolExecutor(max_workers=args.jobs) as pool:
        for done in as_completed([pool.submit(run_one, t) for t in todo]):
            test_id, outcome, text = done.result()
            sys.stderr.write(marks[outcome])
            sys.stderr.flush()
            skipped += outcome == "skip"
            if outcome in ("FAIL", "ERROR"):
                bad.append((test_id, outcome, text))
    took = time.perf_counter() - began

    sys.stderr.write("\n")
    for test_id, outcome, text in sorted(bad):
        name = test_id.rsplit(".", 1)[-1]
        sys.stderr.write(f"{'=' * 70}\n{outcome}: {name} ({test_id})\n"
                         f"{'-' * 70}\n{text}\n")
    sys.stderr.write(f"{'-' * 70}\nRan {len(todo)} tests in {took:.3f}s "
                     f"on {args.jobs} workers\n\n")
    failures = sum(o == "FAIL" for _, o, _ in bad)
    errors = len(bad) - failures
    if bad:
        parts = [f"failures={failures}"] * bool(failures) + [f"errors={errors}"] * bool(errors)
        sys.stderr.write(f"FAILED ({', '.join(parts)})\n")
        return 1
    sys.stderr.write("OK" + (f" (skipped={skipped})" if skipped else "") + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
