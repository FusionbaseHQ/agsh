#!/usr/bin/env python3
"""Golden (littlecheck-style) test runner for agsh.

Each `tests/checks/*.agsh` file is an agsh script with embedded directives in
`#` comments:

    # RUN: <command>      how to run it; %agsh = the binary, %s = this file.
    #                     defaults to `%agsh %s` if omitted.
    # REQUIRES: <cond>    skip the file unless `sh -c <cond>` succeeds.
    # CHECK: <pattern>    expected stdout line (matched in order, 1:1).
    # CHECKERR: <pattern> expected stderr line (matched as an ordered subsequence).
    # CHECKEXIT: <code>   expected process exit code (defaults to 0).

Patterns are literal except for `{{regex}}` placeholders, which match as a
regular expression (e.g. `{{\\d+}}`, `{{.*}}`).

Usage:  python3 tests/checks/run.py [file.agsh ...]
Env:    AGSH=<path>   VERBOSE=1
Exit:   0 if every file passes, else 1.
"""
import glob
import os
import re
import shutil
import subprocess
import sys
import tempfile

_HERE = os.path.dirname(os.path.abspath(__file__))
_REPO = os.path.dirname(os.path.dirname(_HERE))
AGSH = os.environ.get("AGSH", os.path.join(_REPO, "target", "debug", "agsh"))
VERBOSE = os.environ.get("VERBOSE")

DIRECTIVE = re.compile(r"^\s*#\s*(RUN|REQUIRES|CHECK|CHECKERR|CHECKEXIT):\s?(.*)$")


def pattern_to_regex(pat):
    """Turn a CHECK pattern into a full-line anchored regex. Literal text is
    escaped; `{{...}}` segments are treated as raw regex."""
    out = []
    i = 0
    for m in re.finditer(r"\{\{(.*?)\}\}", pat):
        out.append(re.escape(pat[i : m.start()]))
        out.append(m.group(1))
        i = m.end()
    out.append(re.escape(pat[i:]))
    return re.compile("^" + "".join(out) + "$")


def parse(path):
    run = None
    requires = []
    checks = []
    checkerrs = []
    checkexit = 0
    with open(path) as f:
        for line in f:
            m = DIRECTIVE.match(line.rstrip("\n"))
            if not m:
                continue
            kind, rest = m.group(1), m.group(2)
            if kind == "RUN":
                run = rest
            elif kind == "REQUIRES":
                requires.append(rest)
            elif kind == "CHECK":
                checks.append(rest)
            elif kind == "CHECKERR":
                checkerrs.append(rest)
            elif kind == "CHECKEXIT":
                checkexit = int(rest)
    return run, requires, checks, checkerrs, checkexit


def out_lines(text):
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    return lines


def run_file(path):
    run, requires, checks, checkerrs, checkexit = parse(path)
    for cond in requires:
        if subprocess.run(["sh", "-c", cond], capture_output=True).returncode != 0:
            return ("skip", f"REQUIRES not met: {cond}")

    cmd = (run or "%agsh %s").replace("%agsh", AGSH).replace("%s", path)
    # Run in an isolated temp HOME so user config/history can't leak in.
    env = dict(os.environ)
    home = tempfile.mkdtemp(prefix="agsh-golden-")
    for key in list(env):
        if key.startswith("AGSH_") or key in {
            "HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME",
            "BASH_ENV", "ENV",
        }:
            env.pop(key, None)
    env.update(
        HOME=home,
        XDG_CONFIG_HOME=os.path.join(home, ".config"),
        XDG_DATA_HOME=os.path.join(home, ".local", "share"),
        XDG_STATE_HOME=os.path.join(home, ".local", "state"),
        AGSH_HISTORY_FILE=os.path.join(home, "history.jsonl"),
        AGSH_SESSION_DIR=os.path.join(home, "sessions"),
        AGSH_BROKER_DIR=os.path.join(home, "broker"),
        LANG="C",
    )
    for noisy in ("TERM", "COLORTERM", "AGSH_OUTPUT_MODE", "AGSH_ICONS", "NO_COLOR"):
        env.pop(noisy, None)
    try:
        p = subprocess.run(
            ["sh", "-c", cmd], capture_output=True, text=True, env=env, timeout=20
        )
    except subprocess.TimeoutExpired:
        shutil.rmtree(home, ignore_errors=True)
        return ("fail", "timeout")
    shutil.rmtree(home, ignore_errors=True)

    if p.returncode != checkexit:
        return (
            "fail",
            f"exit code {p.returncode} != {checkexit} expected\n"
            f"  stdout: {out_lines(p.stdout)}\n  stderr: {out_lines(p.stderr)}",
        )

    got = out_lines(p.stdout)
    if [c for c in checks] != [] or got != []:
        if len(checks) != len(got):
            return (
                "fail",
                f"stdout line count {len(got)} != {len(checks)} expected\n"
                f"  expected: {checks}\n  got:      {got}",
            )
        for pat, line in zip(checks, got):
            if not pattern_to_regex(pat).match(line):
                return ("fail", f"stdout mismatch:\n  expected: {pat!r}\n  got:      {line!r}")

    # stderr is empty by default. Files that intentionally diagnose may specify
    # CHECKERR lines, matched as an ordered subsequence.
    if checkerrs:
        err = out_lines(p.stderr)
        idx = 0
        for pat in checkerrs:
            rx = pattern_to_regex(pat)
            while idx < len(err) and not rx.match(err[idx]):
                idx += 1
            if idx >= len(err):
                return ("fail", f"stderr missing match for {pat!r}\n  stderr: {err}")
            idx += 1
    elif p.stderr:
        return ("fail", f"unexpected stderr: {out_lines(p.stderr)}")
    return ("pass", "")


def main():
    files = sys.argv[1:] or sorted(glob.glob(os.path.join(_HERE, "*.agsh")))
    if not os.path.exists(AGSH):
        print(f"ERROR: agsh binary not found at {AGSH} (run `cargo build`)")
        sys.exit(2)
    npass = nfail = nskip = 0
    for path in files:
        status, msg = run_file(path)
        name = os.path.basename(path)
        if status == "pass":
            npass += 1
            if VERBOSE:
                print(f"PASS {name}")
        elif status == "skip":
            nskip += 1
            print(f"SKIP {name}: {msg}")
        else:
            nfail += 1
            print(f"\n### FAIL {name}\n{msg}")
    print(f"\n================  golden  PASS={npass}  FAIL={nfail}  SKIP={nskip}  ================")
    sys.exit(1 if nfail else 0)


main()
