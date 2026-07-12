#!/usr/bin/env python3
"""Differential test harness: run each script under agsh and a reference shell
(bash by default) and compare stdout, exit code, stderr presence, and resulting
filesystem state. Diagnostic wording is not compared because shells use
different prefixes, but one side may not emit an unexpected diagnostic.

Usage:  python3 tests/differential/diff.py
Env:    AGSH=<path>   REF=bash|sh   VERBOSE=1
Exit:   0 if the only failures are the documented expected diffs, else 1.
"""
import subprocess, sys, os, tempfile, shutil

_REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
AGSH = os.environ.get("AGSH", os.path.join(_REPO, "target", "debug", "agsh"))
REF = os.environ.get("REF", "bash")

# Cases that legitimately differ from bash (documented). Keep this list tiny and
# justified; everything else must match.
EXPECTED_DIFFS = {
    # description -> reason (keep tiny and justified)
    "redir fd close":
        "`1>&-` then writing: bash reports a write error but still emits to the "
        "captured pipe; agsh silently drops the write. Pathological fd-close edge.",
}

def setup(work):
    os.makedirs(os.path.join(work, "dir1"), exist_ok=True)
    os.makedirs(os.path.join(work, "dir2"), exist_ok=True)
    for name, content in [("file.txt","filecontent\n"),("a.txt","a\n"),("b.txt","b\n"),("c.log","c\n")]:
        with open(os.path.join(work,name),"w") as f: f.write(content)
    open(os.path.join(work,".hidden"),"w").close()

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
            [shell,"-c",script], cwd=work, env=isolated_env(work),
            capture_output=True, text=True, timeout=15,
        )
        return p.stdout, p.stderr, p.returncode
    except subprocess.TimeoutExpired:
        return "<TIMEOUT>","",-99

def snapshot_tree(root):
    snapshot = {}
    for current, dirs, files in os.walk(root):
        dirs.sort()
        files.sort()
        rel_current = os.path.relpath(current, root)
        if rel_current != ".":
            snapshot[rel_current] = ("dir",)
        for name in files:
            path = os.path.join(current, name)
            rel = os.path.relpath(path, root)
            if os.path.islink(path):
                snapshot[rel] = ("symlink", os.readlink(path))
            else:
                with open(path, "rb") as handle:
                    snapshot[rel] = ("file", handle.read())
    return snapshot

TESTS = [
 ("simple var", 'X=hello; echo $X'),
 ("braced var", 'X=hello; echo ${X}world'),
 ("export to child", 'export Y=1; sh -c "echo $Y"'),
 ("exported reassignment", 'export Y=old; Y=new; sh -c \'printf "%s\\n" "$Y"\''),
 ("temp assignment", 'FOO=bar sh -c "printf %s \\"\\$FOO\\""'),
 ("temp assign no leak", 'FOO=bar sh -c "echo $FOO"; echo "after=$FOO"'),
 ("single quote suppress", "X=5; echo '$X'"),
 ("double quote expand", 'X=5; echo "val=$X"'),
 ("empty var", 'echo "[$NOPE]"'),
 ("default unset", 'echo ${UNSET:-fallback}'),
 ("default set empty", 'E=; echo ${E:-fb}'),
 ("default noColon empty", 'E=; echo "[${E-fb}]"'),
 ("default quoted word", 'unset U; set -- ${U:-"a b"}; printf "%s:<%s>\\n" "$#" "$1"'),
 ("default mixed quoted word", 'unset U; set -- p${U:-"a b"}s; printf "%s:<%s>\\n" "$#" "$1"'),
 ("default escaped word", r'unset U; set -- ${U:-a\ b}; printf "%s:<%s>\n" "$#" "$1"'),
 ("default quoted positional fields", 'set -- a "b c"; unset U; f(){ printf "<%s>\\n" "$@"; }; f ${U:-"$@"}; f "${U:-$@}"'),
 ("alt set", 'X=1; echo ${X:+yes}'),
 ("alt unset", 'echo "[${NOPE:+yes}]"'),
 ("length", 'X=hello; echo ${#X}'),
 ("assign default", 'echo ${Z:=assigned}; echo $Z'),
 ("prefix removal short", 'P=a/b/c; echo ${P#*/}'),
 ("prefix removal long", 'P=a/b/c; echo ${P##*/}'),
 ("prefix removal quoted pattern", 'P="a*b"; Q="a*"; printf "%s|%s|%s\\n" "${P#'"'"'a*'"'"'}" "${P#\"$Q\"}" "${P#$Q}"'),
 ("suffix removal short", 'P=a.b.c; echo ${P%.*}'),
 ("suffix removal long", 'P=a.b.c; echo ${P%%.*}'),
 ("nested default", 'A=x; echo ${B:-$A}'),
 ("special $?", 'true; echo $?'),
 ("special $# args", 'set -- a b c; echo $#'),
 ("special $@ count", 'set -- a b c; for x in "$@"; do echo $x; done'),
 ("special $0", 'echo ${0:+has0}'),
 ("special $$", 'echo $$ | grep -qE "^[0-9]+$" && echo ok'),
 ("arith add", 'echo $((2+3))'),
 ("arith precedence", 'echo $((2+3*4))'),
 ("arith paren", 'echo $(( (2+3)*4 ))'),
 ("arith var", 'N=10; echo $((N*2))'),
 ("arith mod", 'echo $((17%5))'),
 ("arith neg", 'echo $(( -5 + 3 ))'),
 ("arith compare", 'echo $((3 > 2))'),
 ("arith bitand", 'echo $((6 & 3))'),
 ("arith shift", 'echo $((1 << 4))'),
 ("arith ternary", 'echo $((1 ? 7 : 8))'),
 ("cmdsub dollar", 'echo "[$(echo hi)]"'),
 ("cmdsub backtick", 'echo "[`echo hi`]"'),
 ("cmdsub nested", 'echo $(echo $(echo deep))'),
 ("cmdsub in arg", 'X=$(printf "a b"); echo "$X"'),
 ("cmdsub strip newline", 'echo "[$(printf "x\\n\\n")]"'),
 ("cmdsub preserves carriage return", 'x=$(printf "a\\r\\n"); printf "%sZ\\n" "$x"'),
 ("cmdsub stderr", 'x=$(printf err >&2); printf "<%s>\\n" "$x"'),
 ("cmdsub stderr same-command redir", 'x=$(printf err >&2) 2>cmd.err; printf "file=<"; cat cmd.err; echo ">"'),
 ("cmdsub stderr surrounding redir", '{ x=$(printf err >&2); } 2>cmd.err; printf "file=<"; cat cmd.err; echo ">"'),
 ("cmdsub stderr function redir scope", 'f(){ printf body >&2; }; f "$(printf argument >&2)" 2>func.err; printf " file=<"; cat func.err; echo ">"'),
 ("procsub stderr same-command redir", 'cat <(printf out; printf err >&2) 2>proc.err; printf " file=<"; cat proc.err; echo ">"'),
 ("glob star", 'echo *.txt'),
 ("glob question", 'echo ?.txt'),
 ("glob class", 'echo [ab].txt'),
 ("glob class range", 'echo [a-c].*'),
 ("glob no match literal", 'echo nomatch_zzz*.q'),
 ("glob hidden not matched", 'echo *'),
 ("glob in dir", 'echo dir*/'),
 ("brace expand", 'echo {a,b,c}'),
 ("brace with prefix", 'echo pre{1,2}post'),
 ("brace nested", 'echo {a,b{1,2}}'),
 ("unquoted split", 'X="a b c"; printf "%s\\n" $X'),
 ("quoted no split", 'X="a b c"; printf "%s\\n" "$X"'),
 ("ifs custom", 'IFS=:; X="a:b:c"; printf "%s\\n" $X'),
 ("redir out", 'echo hi > out.txt; cat out.txt'),
 ("redir append", 'echo a > ap.txt; echo b >> ap.txt; cat ap.txt'),
 ("redir in", 'cat < file.txt'),
 ("redir stderr", 'sh -c "echo err >&2" 2> e.txt; cat e.txt'),
 ("redir 2>&1", 'sh -c "echo o; echo e >&2" > c.txt 2>&1; cat c.txt'),
 ("redir order o-then-dup", 'sh -c "echo o; echo e >&2" 2>&1 1>n.txt; echo done'),
 ("redir noclobber off", 'echo x>f1.txt; echo y>f1.txt; cat f1.txt'),
 ("redir heredoc", 'cat <<EOF\nline1\nline2\nEOF'),
 ("redir heredoc quoted delimiter", "X=value; cat <<E'O'F\n$X\nEOF"),
 ("redir heredoc escapes", 'X=value; cat <<EOF\n\\$X|\\\\|a\\\nb\nEOF'),
 ("redir heredoc strip tabs", 'cat <<-EOF\n\tone\n\tEOF'),
 ("redir heredoc in compound", 'if true; then cat <<EOF\ninside\nEOF\nfi; (cat <<'"'"'EOF'"'"'\n$HOME\nEOF\n)'),
 ("redir herestring", 'cat <<< "hello"'),
 ("redir fd close", 'echo hi 1>&-; echo after'),
 ("pipe simple", 'echo hello | tr a-z A-Z'),
 ("pipe three", 'printf "c\\nb\\na\\n" | sort | head -1'),
 ("pipe exit status", 'false | true; echo $?'),
 ("pipe count", 'printf "a\\nb\\nc\\n" | wc -l'),
 ("pipe builtin src", 'echo hello | cat'),
 ("pipe to read", 'echo "x y" | { read a b; echo "$b $a"; }'),
 ("true status", 'true; echo $?'),
 ("false status", 'false; echo $?'),
 ("and chain", 'true && echo yes'),
 ("or chain", 'false || echo no'),
 ("and short circuit", 'false && echo nope; echo after'),
 ("semicolon seq", 'echo a; echo b'),
 ("exit code propagate", 'sh -c "exit 7"; echo $?'),
 ("not found status", 'nonexistentcmd_xyz 2>/dev/null; echo $?'),
 ("subshell group", '(echo a; echo b)'),
 ("subshell var isolation", 'X=1; (X=2); echo $X'),
 ("brace group", '{ echo a; echo b; }'),
 ("negate status", '! false; echo $?'),
 ("type builtin", 'type cd >/dev/null; echo $?'),
 ("type external code", 'type ls >/dev/null; echo $?'),
 ("type not found", 'type nonexistentcmd_xyz >/dev/null 2>&1; echo $?'),
 ("command -v builtin", 'command -v cd'),
 ("command -v ext code", 'command -v ls >/dev/null; echo $?'),
 ("which echo present", 'which echo >/dev/null; echo $?'),
 ("echo -n", 'echo -n hi; echo X'),
 ("echo multi", 'echo a b c'),
 ("pwd code", 'pwd >/dev/null; echo $?'),
 ("cd relative", 'cd dir1 && pwd | grep -q dir1 && echo ok'),
 ("cd dash", 'cd dir1; cd dir2; cd - >/dev/null; pwd | grep -q dir1 && echo ok'),
 ("test string", '[ -n "x" ] && echo yes'),
 ("test file", '[ -f file.txt ] && echo yes'),
 ("test numeric", '[ 5 -gt 3 ] && echo yes'),
 ("test and", '[ -n "x" -a -n "y" ] && echo yes'),
 ("printf format", 'printf "%s-%s\\n" a b'),
 ("printf int", 'printf "%d\\n" 42'),
 ("printf multi cycle", 'printf "%s\\n" a b c'),
 ("printf percent", 'printf "100%%\\n"'),
 ("if true", 'if true; then echo yes; fi'),
 ("if else", 'if false; then echo a; else echo b; fi'),
 ("if elif", 'if false; then echo a; elif true; then echo b; fi'),
 ("for loop", 'for i in 1 2 3; do echo $i; done'),
 ("while loop", 'i=0; while [ $i -lt 3 ]; do echo $i; i=$((i+1)); done'),
 ("until loop", 'i=0; until [ $i -ge 2 ]; do echo $i; i=$((i+1)); done'),
 ("case match", 'case foo in foo) echo matched;; *) echo no;; esac'),
 ("case glob", 'case abc in a*) echo yes;; esac'),
 ("for glob", 'for f in *.txt; do echo $f; done'),
 ("function def", 'f() { echo "in func $1"; }; f arg'),
 ("function return", 'f() { return 3; }; f; echo $?'),
 ("break in loop", 'for i in 1 2 3; do [ $i = 2 ] && break; echo $i; done'),
 ("continue in loop", 'for i in 1 2 3; do [ $i = 2 ] && continue; echo $i; done'),
 ("tilde home", 'echo ~ | grep -q / && echo ok'),
 ("escaped dollar", 'echo "\\$HOME"'),
 ("escaped newline cont", 'echo a\\\nb'),
 ("backslash literal", 'echo a\\tb'),
 ("multiple assignment", 'A=1 B=2; echo $A$B'),
 ("var in dquote concat", 'X=foo; echo "${X}bar"'),
 ("nested quotes", 'echo "a'"'"'b"'),
 ("local var in func", 'f() { local x=5; echo $x; }; f; echo "[${x}]"'),
 ("regular builtin prefix restores export", 'export AGSH_DIFF_BIND=outer; AGSH_DIFF_BIND=temp true; sh -c \'echo "$AGSH_DIFF_BIND"\''),
 ("function prefix exported and temporary", 'AGSH_DIFF_FUNC=outer; f() { sh -c \'printf "%s" "$AGSH_DIFF_FUNC"\'; }; AGSH_DIFF_FUNC=inner f; echo ":$AGSH_DIFF_FUNC"'),
 ("local restores exported value", 'export AGSH_DIFF_LOCAL=outer; f() { local AGSH_DIFF_LOCAL=inner; sh -c \'echo "$AGSH_DIFF_LOCAL"\'; }; f; sh -c \'echo "$AGSH_DIFF_LOCAL"\''),
 ("declare function scope", 'f() { declare scoped=inside; echo "$scoped"; }; f; echo "${scoped-unset}"'),
 ("declare integer persists", 'declare -i n; n=2+3; echo $n'),
 ("export unset stays unset", 'unset AGSH_DIFF_UNSET; export AGSH_DIFF_UNSET; sh -c \'echo "${AGSH_DIFF_UNSET-unset}"\''),
 # Phase 1 (compat milestone) — cases supported by the reference bash (3.2+).
 ("substr offset len", 'v=hello; echo ${v:1:2}'),
 ("substr offset only", 'v=hello; echo ${v:2}'),
 ("substr neg offset", 'v=hello; echo "${v: -2}"'),
 ("indirect expansion", 'a=b; b=val; echo ${!a}'),
 ("brace range up", 'echo {1..5}'),
 ("brace range down", 'echo {5..1}'),
 ("brace range affix", 'echo pre{1..3}post'),
 ("brace char range", 'echo {a..e}'),
 ("brace neg range", 'echo {-2..2}'),
 ("brace range times list", 'echo {1..3}{a,b}'),
 ("brace single elem", 'echo {5..5}'),
 ("brace single literal", 'echo {a}'),
 ("let assign", 'let x=3+4; echo $x'),
 ("let spaces", 'let "y = 5 * 6"; echo $y'),
 ("let compound", 'x=10; let x+=5; echo $x'),
 ("let zero exit", 'let z=0; echo $?'),
 ("let nonzero exit", 'let z=1; echo $?'),
 ("seconds numeric", 'echo $SECONDS | grep -qE "^[0-9]+$" && echo ok'),
 ("dbracket glob", '[[ abc == a* ]] && echo y || echo n'),
 ("dbracket regex", '[[ abc123 =~ [0-9]+ ]] && echo y'),
 ("dbracket file", '[[ -f /etc/hosts ]] && echo y'),
 ("dbracket -z", '[[ -z "" ]] && echo y'),
 ("dbracket -n", '[[ -n x ]] && echo y'),
 ("dbracket int", '[[ 5 -gt 3 ]] && echo y'),
 ("dbracket and", '[[ a == a && b == b ]] && echo y'),
 ("dbracket or", '[[ a == x || b == b ]] && echo y'),
 ("dbracket negate", '[[ ! -f /no/such ]] && echo y'),
 ("dbracket parens", '[[ ( a == a ) && b == b ]] && echo y'),
 ("dbracket unsplit", 'x="a b"; [[ $x == "a b" ]] && echo y'),
 ("dbracket in if", 'if [[ -d /etc ]]; then echo y; fi'),
 ("getopts loop", 'set -- -a -b val -c x; while getopts "ab:c" o; do echo "$o:$OPTARG"; done; echo "i=$OPTIND"'),
 ("getopts cluster", 'OPTIND=1; set -- -abc; while getopts abc o; do echo $o; done'),
 ("getopts unknown", 'OPTIND=1; set -- -x; getopts ab o 2>/dev/null; echo "$o"'),
 ("trap exit", 'trap "echo bye" EXIT; echo hi'),
 ("trap exit order", 'trap "echo cleanup" EXIT; echo a; echo b'),
 ("trap on exit builtin", 'trap "echo done" EXIT; exit 3'),
 ("trap reset", 'trap "echo X" EXIT; trap - EXIT; echo hi'),
 ("trap zero alias", 'trap "echo bye" 0; echo hi'),
 ("declare -i", 'declare -i n=3+4; echo $n'),
 ("typeset -i", 'typeset -i m=10*2; echo $m'),
 ("declare plain", 'declare x=hi; echo $x'),
 ("declare -p", 'FOO=bar; declare -p FOO'),
 ("export -p grep", 'export FOO=bar; export -p | grep -q \'FOO="bar"\' && echo ok'),
 ("cstyle for", 'for ((i=0;i<3;i++)); do echo $i; done'),
 ("cstyle accumulate", 's=0; for ((i=1;i<=4;i++)); do s=$((s+i)); done; echo $s'),
 ("cstyle break", 'for ((i=0;i<9;i++)); do [ $i = 2 ] && break; echo $i; done'),
 ("arith cmd true", '((3 > 2)) && echo yes'),
 ("arith cmd false", '((0)) && echo y || echo n'),
 ("arith cmd assign", '((x=5)); echo $x'),
 ("array index", 'a=(x y z); echo ${a[1]}'),
 ("array bare elt0", 'a=(x y z); echo $a'),
 ("array length", 'a=(x y z); echo ${#a[@]}'),
 ("array all", 'a=(x y z); echo ${a[@]}'),
 ("array iterate", 'a=(x y z); for e in ${a[@]}; do echo $e; done'),
 ("array element set", 'a=(1 2 3); a[1]=X; echo ${a[@]}'),
 ("array append", 'a=(1 2); a+=(3 4); echo ${a[@]}'),
 ("array indices", 'a=(x y z); echo ${!a[@]}'),
 ("array arith index", 'i=2; a=(x y z); echo ${a[i]}'),
 ("array elt length", 'a=(ab cde); echo ${#a[1]}'),
 ("bash_rematch", '[[ abc123 =~ ([a-z]+)([0-9]+) ]]; echo ${BASH_REMATCH[1]}-${BASH_REMATCH[2]}'),
 ("single pipestatus", 'false; echo ${PIPESTATUS[0]}'),
 ("name listing", 'FOO_A=1; FOO_B=2; BAR=9; echo ${!FOO*}'),
 ("err trap fires", 'trap "echo E" ERR; false; echo done'),
 ("err no fire ok", 'trap "echo E" ERR; true; echo done'),
 ("err no fire operand", 'trap "echo E" ERR; false || true; echo done'),
 ("quoted array @ count", 'a=("x y" z); n=0; for e in "${a[@]}"; do n=$((n+1)); done; echo $n'),
 ("pipestatus pair", 'true | false; echo ${PIPESTATUS[0]}-${PIPESTATUS[1]}'),
 ("pipestatus all", 'false | true | true; echo ${PIPESTATUS[@]}'),
 ("procsub cat", 'cat <(echo hi)'),
 ("procsub diff", 'diff <(printf "a\\n") <(printf "a\\n"); echo $?'),
 ("procsub two", 'cat <(echo one) <(echo two)'),
 ("procsub read redirect", 'read l < <(printf "x\\n"); echo $l'),
 ("posix class digit", 'case 5 in [[:digit:]]) echo y;; esac'),
 ("posix class alpha", 'case a in [[:alpha:]]) echo y;; esac'),
 ("posix class nomatch", 'case x in [[:digit:]]) echo d;; *) echo n;; esac'),
 ("times runs", 'times >/dev/null; echo $?'),
 ("ppid numeric", 'echo $PPID | grep -qE "^[0-9]+$" && echo ok'),
 ("ifs default", 'printf %s "$IFS" | wc -c | tr -d " "'),
 ("dash flag e", 'set -e; case $- in *e*) echo y;; esac'),
]

def main():
    npass = nfail = 0
    fails = []
    for desc, script in TESTS:
        ref_work = tempfile.mkdtemp(prefix="agsh-diff-ref-")
        agsh_work = tempfile.mkdtemp(prefix="agsh-diff-agsh-")
        setup(ref_work)
        setup(agsh_work)
        ro, re_, rc = runsh(REF, script, ref_work)
        ao, ae, ac = runsh(AGSH, script, agsh_work)
        ref_tree = snapshot_tree(ref_work)
        agsh_tree = snapshot_tree(agsh_work)
        stderr_presence_matches = bool(re_.strip()) == bool(ae.strip())
        if ro == ao and rc == ac and stderr_presence_matches and ref_tree == agsh_tree:
            npass += 1
        else:
            nfail += 1
            fails.append((desc, script, (ro,rc,re_,ref_tree), (ao,ac,ae,agsh_tree)))
        shutil.rmtree(ref_work, ignore_errors=True)
        shutil.rmtree(agsh_work, ignore_errors=True)
    unexpected = []
    for desc, script, (ro,rc,re_,ref_tree), (ao,ac,ae,agsh_tree) in fails:
        tag = "XFAIL" if desc in EXPECTED_DIFFS else "FAIL"
        if tag == "FAIL":
            unexpected.append(desc)
        print(f"\n### {tag}: {desc}")
        print(f"  script: {script!r}")
        print(f"  REF : out={ro!r} code={rc} err={re_.strip()[:120]!r}")
        print(f"  AGSH: out={ao!r} code={ac} err={ae.strip()[:120]!r}")
        if ref_tree != agsh_tree:
            changed = sorted(set(ref_tree) | set(agsh_tree))
            changed = [path for path in changed if ref_tree.get(path) != agsh_tree.get(path)]
            print(f"  FS  : differing paths={changed[:12]!r}")
    print(f"\n================  REF={REF}  PASS={npass}  FAIL={nfail}"
          f"  (unexpected={len(unexpected)})  ================")
    if not os.path.exists(AGSH):
        print(f"ERROR: agsh binary not found at {AGSH} (run `cargo build`)")
        sys.exit(2)
    sys.exit(1 if unexpected else 0)

main()
