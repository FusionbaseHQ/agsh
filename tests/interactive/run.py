#!/usr/bin/env python3
"""Interactive (PTY) regression tests for agsh's line editor, completion,
history, and signals. Reconstructs the visible screen and asserts on it.

Usage:  python3 tests/interactive/run.py
Env:    AGSH=<path>
Exit:   0 if all checks pass, else 1. Requires a working PTY (skips with code 0
        if pty.fork is unavailable).
"""
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_helper import Session, ENTER, TAB, ESC, CTRL_C, CTRL_U, CTRL_A, CTRL_E, RIGHT  # noqa: E402

PASS = 0
FAIL = 0


def check(name, cond, detail=""):
    global PASS, FAIL
    if cond:
        PASS += 1
    else:
        FAIL += 1
        print(f"### FAIL {name}\n  {detail}")


def has(screen, *subs):
    return all(s in screen for s in subs)


def scenario_basic_echo():
    s = Session()
    try:
        s.send("echo hello-interactive" + ENTER, 0.5)
        scr = s.screen()
        check("echo output appears", "hello-interactive" in scr, scr)
        check("prompt symbol present", "❯" in scr or ">" in scr, scr)
    finally:
        s.close()


def scenario_line_editing():
    s = Session()
    try:
        # Type, jump to start, fix a typo.
        s.send("echo XYZ", 0.2)
        s.send(CTRL_A, 0.1)  # to start
        s.send(CTRL_E, 0.1)  # to end
        s.send(ENTER, 0.4)
        check("typed line runs", "XYZ" in s.screen(), s.screen())
        # Ctrl-U clears the line: the new command should run cleanly.
        s.send("garbage-text", 0.2)
        s.send(CTRL_U, 0.2)
        s.send("echo cleaned" + ENTER, 0.4)
        scr = s.screen()
        check("ctrl-u cleared line", "cleaned" in scr, scr)
    finally:
        s.close()


def scenario_completion_dropdown():
    s = Session()
    try:
        s.send("ec", 0.2)
        s.send(TAB, 0.5)
        scr = s.screen()
        # Completion should offer `echo` without staircasing the prompt.
        check("dropdown offers echo", "echo" in scr, scr)
        prompts = scr.count("❯")
        check("no prompt staircase (<=2 prompts)", prompts <= 2, f"{prompts} prompts:\n{scr}")
        s.send(ESC, 0.2)
        s.send(CTRL_U + "echo afterward" + ENTER, 0.4)
        check("esc dismissed cleanly", "afterward" in s.screen(), s.screen())
    finally:
        s.close()


def scenario_multiline_history_no_staircase():
    # Regression for the dropdown/ghost staircase: a multiline heredoc in history
    # must not corrupt the single-line editor when its prefix is retyped.
    hist = "/tmp/agsh-pty-ml-history.jsonl"
    with open(hist, "w") as f:
        f.write(json.dumps({
            "command": "cat <<EOF\nline1\nline2\nEOF", "cwd": "/x",
            "exit_code": 0, "started_at": 1, "duration_ms": 0,
            "hostname": "h", "project": None,
        }) + "\n")
    s = Session(rows=16, history=hist)
    try:
        s.send("cat <<EOF", 0.3)
        scr = s.screen()
        prompts = scr.count("❯")
        check("no ghost staircase on heredoc prefix", prompts <= 1, f"{prompts} prompts:\n{scr}")
        s.send(CTRL_U, 0.2)
        s.send("ca", 0.2)
        s.send(TAB, 0.4)
        scr = s.screen()
        # The multiline heredoc must not appear as a candidate.
        check("multiline history not a candidate", "line1" not in scr and "line2" not in scr, scr)
    finally:
        s.close()


def scenario_ctrl_c_aborts_line():
    s = Session()
    try:
        s.send("echo should-not-run", 0.2)
        s.send(CTRL_C, 0.2)
        s.send("echo ran-after-ctrlc" + ENTER, 0.4)
        scr = s.screen()
        check("ctrl-c discarded the line", "ran-after-ctrlc" in scr, scr)
        # The aborted command must not have executed (no stray output line that is
        # just `should-not-run` on its own).
        ran = any(line.strip() == "should-not-run" for line in scr.splitlines())
        check("aborted command did not execute", not ran, scr)
    finally:
        s.close()


def scenario_heredoc_continuation():
    s = Session()
    try:
        s.send("cat <<END" + ENTER, 0.3)
        scr = s.screen()
        check("continuation prompt shown", ">" in scr, scr)
        s.send("body-line" + ENTER, 0.3)
        s.send("END" + ENTER, 0.5)
        check("heredoc body printed", "body-line" in s.screen(), s.screen())
    finally:
        s.close()


def _seed_history(path, commands):
    with open(path, "w") as f:
        for i, cmd in enumerate(commands):
            f.write(json.dumps({
                "command": cmd, "cwd": "/x", "exit_code": 0,
                "started_at": i + 1, "duration_ms": 0, "hostname": "h", "project": None,
            }) + "\n")


def scenario_reverse_search():
    hist = "/tmp/agsh-pty-rsearch.jsonl"
    _seed_history(hist, ["echo apple", "echo banana", "echo cherry"])
    from pty_helper import CTRL_R
    s = Session(history=hist)
    try:
        s.send(CTRL_R, 0.3)
        scr = s.screen()
        check("reverse-search prompt opens", "search" in scr.lower() or "`" in scr or ":" in scr, scr)
        s.send("banana", 0.3)
        s.send(ENTER, 0.4)
        check("reverse-search runs the matched command", "banana" in s.screen(), s.screen())
    finally:
        s.close()


def scenario_autosuggestion_accept():
    hist = "/tmp/agsh-pty-suggest.jsonl"
    _seed_history(hist, ["echo suggested-tail"])
    s = Session(history=hist)
    try:
        s.send("echo sugg", 0.3)
        scr = s.screen()
        check("autosuggestion ghost shows the rest", "suggested-tail" in scr, scr)
        s.send(RIGHT, 0.2)   # accept suggestion
        s.send(ENTER, 0.4)
        check("accepted suggestion runs", "suggested-tail" in s.screen(), s.screen())
    finally:
        s.close()


def scenario_completion_accept_and_run():
    s = Session()
    try:
        s.send("ec", 0.2)
        s.send(TAB, 0.4)   # open dropdown
        s.send(ENTER, 0.3)  # accept selected (echo)
        s.send(" completed-ok" + ENTER, 0.4)
        check("completed command runs", "completed-ok" in s.screen(), s.screen())
    finally:
        s.close()


def scenario_sequential_commands():
    s = Session()
    try:
        s.send("echo one-cmd" + ENTER, 0.4)
        check("first command output", "one-cmd" in s.screen(), s.screen())
        s.send("echo two-cmd" + ENTER, 0.4)
        check("second command output", "two-cmd" in s.screen(), s.screen())
        s.send("false" + ENTER, 0.3)
        s.send("echo three-cmd" + ENTER, 0.4)
        check("command after failure still runs", "three-cmd" in s.screen(), s.screen())
    finally:
        s.close()


def _make_png(path, w=2, h=2):
    import struct
    import zlib

    def chunk(typ, data):
        c = typ + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)

    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)
    raw = b"".join(b"\x00" + b"\xff\x00\x00" * w for _ in range(h))
    with open(path, "wb") as f:
        f.write(sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


def scenario_view_image_inline_and_fallback():
    import os as _os

    d = "/tmp/agsh-pty-img"
    _os.makedirs(d, exist_ok=True)
    _make_png(_os.path.join(d, "pic.png"), 8, 8)
    # 1) Image-capable terminal: `view` emits the crisp iTerm2 inline-image escape.
    _os.environ["TERM_PROGRAM"] = "WezTerm"
    s = Session()
    try:
        s.send(f"cd {d}" + ENTER, 0.3)
        raw = s.send("agview pic.png" + ENTER, 0.6).decode("latin-1")
        check("agview emits inline-image protocol", "\x1b]1337;File=inline=1" in raw, raw[:80])
    finally:
        s.close()
        _os.environ.pop("TERM_PROGRAM", None)
    # 2) Plain (no-protocol) but truecolor terminal: decoded half-block color art,
    #    so the user sees the image in place — no "open another terminal" card.
    _os.environ["COLORTERM"] = "truecolor"
    s = Session()
    try:
        s.send(f"cd {d}" + ENTER, 0.3)
        raw = s.send("agview pic.png" + ENTER, 0.6).decode("utf-8", "replace")
        check("agview renders half-block art", "▀" in raw and "\x1b[38;2;" in raw, raw[:160])
        check("no fallback-to-other-terminal note", "no inline-image support" not in raw, raw[:160])
    finally:
        s.close()


def scenario_mode_builtin_session_default():
    s = Session()
    try:
        s.send("mode" + ENTER, 0.3)
        check("mode shows the output aspect", "output: raw" in s.screen(), s.screen())
        # Namespaced form: mode:output <value>.
        s.send("mode:output compact" + ENTER, 0.3)
        s.send("mode" + ENTER, 0.3)
        check("mode:output compact persists", "output: compact" in s.screen(), s.screen())
        s.send("mode:bogus x" + ENTER, 0.3)
        check("unknown aspect rejected", "unknown aspect" in s.screen(), s.screen())
        # Shorthand still works.
        s.send("mode raw" + ENTER, 0.3)
        s.send("mode:output" + ENTER, 0.3)
        check("mode raw shorthand sets output", "raw" in s.screen(), s.screen())
        s.send("mode:output off" + ENTER, 0.3)
        s.send("echo back-to-default" + ENTER, 0.4)
        check("mode off + command still runs", "back-to-default" in s.screen(), s.screen())
    finally:
        s.close()


def scenario_view_code_highlighting():
    import os as _os

    d = "/tmp/agsh-pty-code"
    _os.makedirs(d, exist_ok=True)
    with open(_os.path.join(d, "hello.py"), "w") as f:
        f.write('def greet(name):  # hi\n    return "hello " + name\n')
    _os.environ["COLORTERM"] = "truecolor"
    s = Session()
    try:
        s.send(f"cd {d}" + ENTER, 0.3)
        raw = s.send("agview hello.py" + ENTER, 0.6).decode("utf-8", "replace")
        check("agview highlights code (truecolor SGR)", "\x1b[" in raw and "38;2;" in raw, raw[:160])
        check("source content preserved", "greet" in raw and "hello " in raw, raw[:160])
    finally:
        s.close()


def scenario_ls_color_env_seeded():
    s = Session()
    try:
        # On a TTY, agsh seeds CLICOLOR + LSCOLORS so the real `ls` colorizes.
        s.send('echo "C=$CLICOLOR L=$LSCOLORS"' + ENTER, 0.4)
        scr = s.screen()
        check("CLICOLOR seeded on tty", "C=1" in scr, scr)
        check("LSCOLORS seeded on tty", "L=ExGxFxdxCx" in scr, scr)
        # `ls` of a fresh dir with a subdirectory emits ANSI color (raw bytes).
        s.send('cd "$(mktemp -d)" && mkdir adir && : > afile' + ENTER, 0.4)
        raw = s.send("ls" + ENTER, 0.5)
        check("ls emits ANSI color on tty", "\x1b[" in raw.decode("latin-1"), repr(raw[:120]))
    finally:
        s.close()


def scenario_hooks_precmd_preexec_chpwd():
    s = Session()
    try:
        s.send("preexec() { echo PREEXEC:$1; }" + ENTER, 0.3)
        s.send("chpwd() { echo CHPWD-FIRED; }" + ENTER, 0.3)
        s.send("echo hello" + ENTER, 0.4)
        scr = s.screen()
        check("preexec hook fires with command", "PREEXEC:echo hello" in scr, scr)
        s.send("cd /tmp" + ENTER, 0.4)
        check("chpwd hook fires on cd", "CHPWD-FIRED" in s.screen(), s.screen())
    finally:
        s.close()


def scenario_programmable_completion():
    s = Session()
    try:
        s.send("complete -W 'deploy build test' mytool" + ENTER, 0.3)
        s.send("mytool de" + TAB, 0.5)
        check("complete -W word offered", "deploy" in s.screen(), s.screen())
        s.send(ESC, 0.2)
        s.send(CTRL_U, 0.1)
        s.send("c" + TAB, 0.5)
        check(
            "builtin description in dropdown",
            "change the working" in s.screen(),
            s.screen(),
        )
    finally:
        s.close()


def scenario_mode_intercept_toggle():
    # Runtime toggle of shell interception via the mode: namespace.
    s = Session()
    try:
        s.send("mode:intercept" + ENTER, 0.3)
        check("intercept off by default", "off" in s.screen(), s.screen())
        s.send("mode:intercept compact" + ENTER, 0.3)
        check("toggle on confirms", "interception on" in s.screen(), s.screen())
        s.send("mode:intercept off" + ENTER, 0.3)
        check("toggle off confirms", "interception off" in s.screen(), s.screen())
    finally:
        s.close()


def scenario_rc_autoload():
    # An interactive session sources its rc file: aliases + exports must persist.
    rc = "/tmp/agsh-pty-rc.agshrc"
    with open(rc, "w") as f:
        f.write("alias hi='echo hello-from-rc'\nexport RCVAR=rcworks\n")
    s = Session(args=["--rcfile", rc])
    try:
        s.send("hi" + ENTER, 0.4)
        check("rc alias is loaded", "hello-from-rc" in s.screen(), s.screen())
        s.send("echo $RCVAR" + ENTER, 0.4)
        check("rc export is loaded", "rcworks" in s.screen(), s.screen())
    finally:
        s.close()
        os.remove(rc)


SCENARIOS = [
    scenario_mode_intercept_toggle,
    scenario_rc_autoload,
    scenario_mode_builtin_session_default,
    scenario_view_image_inline_and_fallback,
    scenario_view_code_highlighting,
    scenario_ls_color_env_seeded,
    scenario_hooks_precmd_preexec_chpwd,
    scenario_programmable_completion,
    scenario_sequential_commands,
    scenario_basic_echo,
    scenario_line_editing,
    scenario_completion_dropdown,
    scenario_multiline_history_no_staircase,
    scenario_ctrl_c_aborts_line,
    scenario_heredoc_continuation,
    scenario_reverse_search,
    scenario_autosuggestion_accept,
    scenario_completion_accept_and_run,
]


def main():
    if not os.path.exists(os.environ.get("AGSH", os.path.join(
        os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
        "target", "debug", "agsh"))):
        print("ERROR: agsh binary not found (run `cargo build`)")
        sys.exit(2)
    for sc in SCENARIOS:
        try:
            sc()
        except Exception as e:  # noqa: BLE001
            global FAIL
            FAIL += 1
            print(f"### ERROR in {sc.__name__}: {e}")
    print(f"\n================  interactive  PASS={PASS}  FAIL={FAIL}  ================")
    sys.exit(1 if FAIL else 0)


main()
