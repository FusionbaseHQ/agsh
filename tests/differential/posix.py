#!/usr/bin/env python3
"""POSIX conformance differential: run POSIX-only scripts under agsh and a POSIX
shell (default `sh`) and compare stdout + exit code. Unlike `diff.py` (which
compares against bash and includes bash extensions), every case here is valid
POSIX `sh`, so it can be checked against a strict POSIX shell.

Usage:  python3 tests/differential/posix.py
Env:    AGSH=<path>   REF=sh|dash   VERBOSE=1
Note:   On macOS, /bin/sh is bash in POSIX mode (lenient); use `REF=dash` for a
        strict check where available.
"""
import os
import subprocess
import sys
import tempfile
import shutil

_REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
AGSH = os.environ.get("AGSH", os.path.join(_REPO, "target", "debug", "agsh"))
REF = os.environ.get("REF", "sh")

# POSIX Shell Command Language cases (each valid POSIX sh). Grouped by section.
TESTS = [
    # 2.5 parameters / special parameters
    ("special $#", "set -- a b c; echo $#"),
    ('special "$@"', 'set -- a b c; for x in "$@"; do echo $x; done'),
    ("special $? success", "true; echo $?"),
    ("special $? failure", "false; echo $?"),
    ("special $- has flags", "set -e; case $- in *e*) echo y;; *) echo n;; esac"),
    ("$$ numeric", 'echo $$ | grep -qE "^[0-9]+$" && echo ok'),
    ("$PPID numeric", 'echo $PPID | grep -qE "^[0-9]+$" && echo ok'),
    ("$! background pid", "sleep 0.1 & p=$!; echo $p | grep -qE '^[0-9]+$' && echo ok; wait"),
    # ($LINENO is validated by the script-file golden test; macOS /bin/sh = bash
    #  3.2 reports 0 for `-c` LINENO, so it is not checked against `sh` here.)
    ("default IFS splits", 'x="a b c"; set -- $x; echo $#'),
    ("IFS custom", 'IFS=:; x="a:b:c"; set -- $x; echo $2'),
    # 2.6 expansions (POSIX parameter expansion only)
    ("param default", "echo ${u:-fallback}"),
    ("param quoted default word", 'unset u; set -- ${u:-"a b"}; printf "%s:<%s>\\n" "$#" "$1"'),
    ("param escaped default word", r'unset u; set -- ${u:-a\ b}; printf "%s:<%s>\n" "$#" "$1"'),
    ("param quoted positional fields", 'set -- a "b c"; unset u; f(){ printf "<%s>\\n" "$@"; }; f ${u:-"$@"}; f "${u:-$@}"'),
    ("param assign", "echo ${v:=set}; echo $v"),
    ("param alt", "x=1; echo ${x:+yes}"),
    ("param length", "x=hello; echo ${#x}"),
    ("remove prefix", "p=a/b/c; echo ${p##*/}"),
    ("remove quoted prefix", 'p="a*b"; q="a*"; printf "%s|%s|%s\\n" "${p#'"'"'a*'"'"'}" "${p#\"$q\"}" "${p#$q}"'),
    ("remove suffix", "p=a.b.c; echo ${p%.*}"),
    ("arith expansion", "echo $((2 + 3 * 4))"),
    ("command sub", 'echo "[$(echo hi)]"'),
    ("tilde root", "echo ~root | grep -q / && echo ok"),
    # 2.7 redirection
    ("redir out/in", "echo hi > o.txt; cat < o.txt"),
    ("redir append", "echo a > a.txt; echo b >> a.txt; cat a.txt"),
    ("heredoc", "cat <<EOF\nline1\nline2\nEOF"),
    ("heredoc quoted delimiter", "x=value; cat <<E'O'F\n$x\nEOF"),
    ("heredoc escapes", "x=value; cat <<EOF\n\\$x|\\\\|a\\\nb\nEOF"),
    ("heredoc strip tabs", "cat <<-EOF\n\tone\n\tEOF"),
    ("heredoc in compound", "if true; then cat <<EOF\ninside\nEOF\nfi; (cat <<'EOF'\n$HOME\nEOF\n)"),
    ("read-write fd0", "printf 'rw\\n' > rw.txt; cat <>rw.txt"),
    # 2.9 compound commands
    ("if/elif/else", "if false; then echo a; elif true; then echo b; else echo c; fi"),
    ("for in", "for i in 1 2 3; do echo $i; done"),
    ("while", "i=0; while [ $i -lt 3 ]; do echo $i; i=$((i+1)); done"),
    ("until", "i=0; until [ $i -ge 2 ]; do echo $i; i=$((i+1)); done"),
    ("case", "case foo in foo) echo m;; *) echo n;; esac"),
    ("subshell isolation", "x=1; (x=2); echo $x"),
    ("function", "f() { echo \"in $1\"; }; f arg"),
    ("pipeline", "echo hello | tr a-z A-Z"),
    ("and-or", "true && echo a; false || echo b"),
    # 2.11 traps
    ("trap EXIT", 'trap "echo bye" EXIT; echo hi'),
    ("trap signal fires", 'trap "echo caught" USR1; kill -USR1 $$; echo after'),
    # 2.13 pattern matching
    ("class digit", "case 5 in [[:digit:]]) echo y;; esac"),
    ("class alpha", "case a in [[:alpha:]]) echo y;; esac"),
    ("class negate", "case b in [!0-9]) echo y;; esac"),
    ("glob class", "cd \"$(mktemp -d)\"; : > f1; : > fx; echo f[[:digit:]]"),
    # 2.14 special built-ins
    ("times runs", "times >/dev/null; echo $?"),
    ("getopts", "set -- -a -b val x; while getopts ab: o; do echo $o:$OPTARG; done"),
    ("export to child", "export Y=1; sh -c 'echo $Y'"),
    (
        "special builtin prefix persists",
        "unset AGSH_POSIX_PREFIX; AGSH_POSIX_PREFIX=x :; echo $AGSH_POSIX_PREFIX",
    ),
    (
        "readonly reassignment fails",
        "readonly AGSH_POSIX_RO=one; "
        "(readonly AGSH_POSIX_RO=two) 2>/dev/null; "
        "s=$?; [ $s -ne 0 ] && s=nonzero; echo $s:$AGSH_POSIX_RO",
    ),
    (
        "readonly unset fails",
        "readonly AGSH_POSIX_UNSET=one; "
        "(unset AGSH_POSIX_UNSET) 2>/dev/null; "
        "s=$?; [ $s -ne 0 ] && s=nonzero; echo $s:$AGSH_POSIX_UNSET",
    ),
    ("set positionals", "set -- one two; echo $1 $2"),
    ("shift", "set -- a b c; shift; echo $1"),
]


def isolated_env(work):
    env = dict(os.environ)
    for key in list(env):
        if key.startswith("AGSH_") or key in {
            "HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME",
            "BASH_ENV", "ENV",
        }:
            env.pop(key, None)
    home = os.path.join(work, ".test-home")
    os.makedirs(home, exist_ok=True)
    env.update({
        "HOME": home,
        "XDG_CONFIG_HOME": os.path.join(home, ".config"),
        "XDG_DATA_HOME": os.path.join(home, ".local", "share"),
        "XDG_STATE_HOME": os.path.join(home, ".local", "state"),
        "AGSH_HISTORY_FILE": os.path.join(home, "history.jsonl"),
        "AGSH_SESSION_DIR": os.path.join(home, "sessions"),
        "AGSH_BROKER_DIR": os.path.join(home, "broker"),
    })
    return env


def runsh(shell, script, work):
    try:
        p = subprocess.run(
            [shell, "-c", script], cwd=work, env=isolated_env(work),
            capture_output=True, text=True, timeout=15
        )
        return p.stdout, p.returncode
    except subprocess.TimeoutExpired:
        return "<TIMEOUT>", -99


def main():
    if not os.path.exists(AGSH):
        print(f"ERROR: agsh not found at {AGSH} (run `cargo build`)")
        sys.exit(2)
    npass = nfail = 0
    for desc, script in TESTS:
        ref_work = tempfile.mkdtemp(prefix="agsh-posix-ref-")
        agsh_work = tempfile.mkdtemp(prefix="agsh-posix-agsh-")
        ro, rc = runsh(REF, script, ref_work)
        ao, ac = runsh(AGSH, script, agsh_work)
        shutil.rmtree(ref_work, ignore_errors=True)
        shutil.rmtree(agsh_work, ignore_errors=True)
        if ro == ao and rc == ac:
            npass += 1
            if os.environ.get("VERBOSE"):
                print(f"PASS {desc}")
        else:
            nfail += 1
            print(f"\n### FAIL: {desc}\n  script: {script!r}")
            print(f"  {REF}: out={ro!r} rc={rc}")
            print(f"  agsh: out={ao!r} rc={ac}")
    print(f"\n================  POSIX vs {REF}  PASS={npass}  FAIL={nfail}  ================")
    sys.exit(1 if nfail else 0)


main()
