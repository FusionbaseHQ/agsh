#!/usr/bin/env python3
"""Interactive (PTY) regression tests for agsh's line editor, completion,
history, and signals. Reconstructs the visible screen and asserts on it.

Usage:  python3 tests/interactive/run.py
Env:    AGSH=<path>
Exit:   0 only if every check runs and passes, else 1. Supported release hosts
        must provide PTYs and Unix sockets; missing prerequisites are failures.
"""
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_helper import Session, ENTER, TAB, ESC, CTRL_C, CTRL_U, CTRL_A, CTRL_E, CTRL_R, RIGHT  # noqa: E402

PASS = 0
FAIL = 0
SKIP = 0
_BROKER_RUNTIME = None
_SUITE_TMP = tempfile.mkdtemp(prefix="agsh-pty-suite-")


def suite_path(name):
    return os.path.join(_SUITE_TMP, name)


def check(name, cond, detail=""):
    global PASS, FAIL
    if cond:
        PASS += 1
    else:
        FAIL += 1
        print(f"### FAIL {name}\n  {detail}")


def skip(name, reason):
    global SKIP
    SKIP += 1
    print(f"### SKIP {name}: {reason}")


def broker_runtime_available():
    global _BROKER_RUNTIME
    if _BROKER_RUNTIME is not None:
        return _BROKER_RUNTIME
    try:
        with tempfile.TemporaryDirectory(prefix="agsh-pty-broker-probe-") as d:
            path = os.path.join(d, "agshd.sock")
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                sock.bind(path)
            finally:
                sock.close()
    except OSError as e:
        _BROKER_RUNTIME = (False, f"AF_UNIX sockets unavailable: {e}")
    else:
        _BROKER_RUNTIME = (True, "")
    return _BROKER_RUNTIME


def has(screen, *subs):
    return all(s in screen for s in subs)


def wait_screen(session, needle, timeout=5.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if needle in session.screen():
            return True
        session.drain(0.1)
    return needle in session.screen()


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
    hist = suite_path("ml-history.jsonl")
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
    hist = suite_path("rsearch.jsonl")
    _seed_history(hist, ["echo apple", "echo banana", "echo cherry"])
    s = Session(history=hist)
    try:
        s.send(CTRL_R, 0.3)
        scr = s.screen()
        check("history picker opens", "history [" in scr and "fuzzy" in scr, scr)
        s.send("banana", 0.3)
        s.send(ENTER, 0.4)
        check("reverse-search runs the matched command", "banana" in s.screen(), s.screen())
    finally:
        s.close()


def scenario_history_picker_tab_edits_and_mode_cycles():
    hist = suite_path("history-picker.jsonl")
    _seed_history(hist, ["echo picker-run", "echo picker-edit", "git status"])
    s = Session(history=hist)
    try:
        s.send(CTRL_R, 0.3)
        s.send("\x13", 0.2)  # Ctrl-S: fuzzy -> prefix
        scr = s.screen()
        check("history picker cycles search mode", "prefix" in scr, scr)
        s.send("echo picker-e", 0.3)
        s.send(TAB, 0.3)
        scr = s.screen()
        check("tab inserts selected history command for editing", "echo picker-edit" in scr, scr)
        s.send(ENTER, 0.5)
        check("edited history command runs", "picker-edit" in s.screen(), s.screen())
    finally:
        s.close()


def scenario_history_tui_command_opens_picker():
    hist = suite_path("history-tui.jsonl")
    _seed_history(hist, ["echo tui-command", "echo other"])
    s = Session(history=hist)
    try:
        s.send("history tui" + ENTER, 0.4)
        scr = s.screen()
        check("history tui opens picker", "history [" in scr and "tui-command" in scr, scr)
        s.send("tui", 0.3)
        s.send(ENTER, 0.5)
        check("history tui selection runs", "tui-command" in s.screen(), s.screen())
    finally:
        s.close()


def scenario_history_picker_scrolls_all_matches():
    hist = suite_path("history-scroll.jsonl")
    _seed_history(hist, [f"echo scroll-{i:02d}" for i in range(20)])
    s = Session(history=hist)
    try:
        s.send(CTRL_R, 0.4)
        scr = s.screen()
        check("history picker shows total count", "/20" in scr, scr)
        s.send("\x1b[B" * 15, 0.8)
        scr = s.screen()
        check("history picker scrolls beyond visible rows", "16/20" in scr, scr)
        s.send(ENTER, 0.6)
        check("scrolled history selection runs", "scroll-04" in s.screen(), s.screen())
    finally:
        s.close()


def scenario_autosuggestion_accept():
    hist = suite_path("suggest.jsonl")
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


def scenario_huge_history_ghost_is_clipped():
    # Regression: a pathological history entry (e.g. `agmath '((((…'` with
    # thousands of chars from arithmetic fuzzing) must show as a clipped
    # one-line hint, not flood the screen with parens. Accepting with → must
    # still insert the FULL command, not the clipped display text.
    hist = suite_path("monster.jsonl")
    monster = "agmath '" + "(" * 2000 + "1" + ")" * 2000 + "'"
    _seed_history(hist, ["echo before", monster])
    s = Session(history=hist)
    try:
        s.send("agm", 0.5)
        scr = s.screen()
        check("ghost is clipped, no paren flood", scr.count("(") < 100, scr)
        check("clip is marked with an ellipsis", "…" in scr, scr)
        prompts = scr.count("❯")
        check("prompt still on one line", prompts <= 1, f"{prompts} prompts:\n{scr}")
        # Accept the full suggestion and run it: the real agmath evaluates the
        # whole 2000-deep expression (the depth guard errors politely if not —
        # either way the FULL text was inserted, since the clipped text with a
        # literal `…` would be a parse error mentioning "…").
        s.send(RIGHT, 0.3)
        s.send(ENTER, 0.8)
        scr = s.screen()
        check("accepted full text, not the clipped display", "…'" not in scr, scr)
    finally:
        s.close()
        os.unlink(hist)


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


def scenario_frecent_jump():
    with tempfile.TemporaryDirectory(prefix="agsh-frecent-", dir="/tmp") as base:
        api = os.path.join(base, "backend-api")
        web = os.path.join(base, "frontend-web")
        os.makedirs(api)
        os.makedirs(web)
        s = Session(cols=160)
        try:
            s.send(f"cd {api}" + ENTER, 0.3)
            s.send(f"cd {web}" + ENTER, 0.3)
            s.send("agz backend" + ENTER, 0.4)
            s.send("printf 'FRECENT=%s\\n' \"$PWD\"" + ENTER, 0.4)
            scr = s.screen()
            check("agz jumps to a frecent interactive cwd", f"FRECENT={api}" in scr, scr)
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

    d = suite_path("img")
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

    d = suite_path("code")
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


def scenario_clear_passes_through_in_compact_mode():
    s = Session()
    try:
        s.send("mode:output compact" + ENTER, 0.3)
        raw = s.send("clear" + ENTER, 0.4).decode("latin-1")
        esc = ("\x1b[2J" in raw) or ("\x1b[3J" in raw) or ("\x1b[H" in raw)
        check("clear emits the real screen-clear escape in compact mode", esc, raw[:120])
        check("clear is not swallowed into a compact summary", "clear [ok]" not in raw, raw[:120])
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
    rc = suite_path("rc.agshrc")
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


def scenario_terminal_restore_on_signal():
    # SHIP_READINESS_PLAN P0-10: a SIGTERM/SIGHUP at the raw-mode prompt must
    # restore the terminal (canonical + echo) before the shell dies, so the tty
    # isn't left broken (needing `reset`). Driven directly (not via Session)
    # because we inspect the pty's termios and send signals.
    import pty
    import signal
    import termios
    import time
    from pty_helper import AGSH

    import tempfile

    icanon, echo = termios.ICANON, termios.ECHO
    for sig, name in [(signal.SIGTERM, "SIGTERM"), (signal.SIGHUP, "SIGHUP")]:
        master, slave = pty.openpty()
        base = termios.tcgetattr(slave)[3]
        # Isolated session dir: these shells die by signal and must not leave
        # unclean journals in the developer's real state directory.
        sess_dir = tempfile.mkdtemp(prefix="agsh-pty-sig-")
        p = subprocess.Popen(
            [AGSH, "--norc"],
            stdin=slave, stdout=slave, stderr=slave,
            start_new_session=True,
            env={**os.environ, "TERM": "xterm", "AGSH_SESSION_DIR": sess_dir},
        )
        # Wait until agsh enters raw mode (ICANON+ECHO cleared on the tty).
        entered = False
        deadline = time.time() + 5
        while time.time() < deadline:
            lf = termios.tcgetattr(slave)[3]
            if not (lf & icanon) and not (lf & echo):
                entered = True
                break
            time.sleep(0.05)
        check(f"{name}: reached raw-mode prompt", entered,
              f"base={base:#x} lflag={termios.tcgetattr(slave)[3]:#x}")
        p.send_signal(sig)
        try:
            p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            p.kill()
            check(f"{name}: shell exited on signal", False, "timed out")
            os.close(master); os.close(slave)
            shutil.rmtree(sess_dir, ignore_errors=True)
            continue
        lf = termios.tcgetattr(slave)[3]
        os.close(master)
        os.close(slave)
        check(f"{name}: terminal restored (canonical+echo)",
              bool(lf & icanon) and bool(lf & echo), f"lflag={lf:#x}")
        shutil.rmtree(sess_dir, ignore_errors=True)


def scenario_session_resume_after_kill():
    # Session resilience: a session killed hard (simulated crash) leaves a
    # journal; the next session in the same journal dir shows the restore
    # banner, and `resume` replays the dead session's state.
    import signal
    import tempfile

    sess_dir = tempfile.mkdtemp(prefix="agsh-pty-resume-")
    banner_on = {"AGSH_RESUME_BANNER": "1"}
    try:
        s = Session(session_dir=sess_dir, extra_env=banner_on)
        try:
            s.send("export RESUME_PROBE=alive-42" + ENTER, 0.5)
            s.send("alias rgs='git status'" + ENTER, 0.5)
            os.kill(s.pid, signal.SIGKILL)  # crash: no clean `exit` record
            os.waitpid(s.pid, 0)
        finally:
            try:
                os.close(s.fd)
            except OSError:
                pass

        s2 = Session(session_dir=sess_dir, extra_env=banner_on)
        try:
            check(
                "restore banner offers resume",
                wait_screen(s2, "2 changes") and "resume" in s2.screen(),
                s2.screen(),
            )
            s2.send("resume" + ENTER, 0.2)
            check("resume reports restore", wait_screen(s2, "restored session"), s2.screen())
            s2.send("echo probe=$RESUME_PROBE" + ENTER, 0.2)
            check("export restored", wait_screen(s2, "probe=alive-42"), s2.screen())
            # A third session sees nothing: the journal was consumed and the
            # second session is still alive (its own journal is not offered).
            s3 = Session(session_dir=sess_dir, extra_env=banner_on)
            try:
                check("consumed journal not re-offered", "resume" not in s3.screen(), s3.screen())
            finally:
                s3.close()
        finally:
            s2.close()
    finally:
        shutil.rmtree(sess_dir, ignore_errors=True)


def scenario_keep_attach_detach():
    ok, reason = broker_runtime_available()
    if not ok:
        skip("scenario_keep_attach_detach", reason)
        return
    # Phase-2 interactive path: `keep -- cmd` attaches to a broker-held PTY;
    # Ctrl-] detaches leaving the job running; reattach replays scrollback;
    # the job survives the whole shell being replaced.
    CTRL_RBRACKET = "\x1d"
    s = Session()
    try:
        s.send("keep -- sh -c 'echo kept-hello; exec cat'" + ENTER, 1.2)
        scr = s.screen()
        check("keep spawn+attach shows job output", "kept-hello" in scr, scr)
        check("keep attach hint shown", "Ctrl-]" in scr, scr)

        # Typed input reaches the kept job (cat echoes it through the PTY).
        s.send("marco" + ENTER, 0.6)
        check("input reaches kept job", "marco" in s.screen(), s.screen())

        # Detach: back at the prompt, job still running.
        s.send(CTRL_RBRACKET, 0.8)
        scr = s.screen()
        check("detach returns to prompt", "detached" in scr, scr)
        s.send("keep list" + ENTER, 0.8)
        scr = s.screen()
        check("job listed running after detach", "running" in scr, scr)

        # Reattach in a brand-new shell process (the old one exits): the
        # scrollback replay brings back what happened before.
        broker_dir = s.broker_dir
        s._owns_broker_dir = False  # keep the broker alive across sessions
        s.close()
        s2 = Session(broker_dir=broker_dir)
        try:
            s2.send("keep attach k1" + ENTER, 1.0)
            scr = s2.screen()
            check("reattach replays scrollback", "kept-hello" in scr, scr)
            s2.send(CTRL_RBRACKET, 0.8)
            s2.send("keep kill k1 KILL" + ENTER, 0.6)
            s2.send("keep stop" + ENTER, 0.6)
            check("broker stops cleanly", "broker stopped" in s2.screen(), s2.screen())
        finally:
            s2._owns_broker_dir = True  # last user cleans up
            s2.close()
    finally:
        shutil.rmtree(s.broker_dir, ignore_errors=True)


def scenario_keep_full_session_survives_client_death():
    ok, reason = broker_runtime_available()
    if not ok:
        skip("scenario_keep_full_session_survives_client_death", reason)
        return
    # Phase 3: `agsh --keep` runs the whole session under the broker. Killing
    # the attach client with SIGKILL (what terminal death looks like) leaves
    # the inner session alive with all its state; `agsh --attach` from a new
    # terminal resumes it exactly where it was.
    import signal
    import tempfile

    import time

    def wait_for(session, needle, timeout=6.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if needle in session.screen():
                return True
            session.drain(0.2)
        return needle in session.screen()

    broker_dir = tempfile.mkdtemp(prefix="agsh-pty-fullsess-")
    session_dir = None
    try:
        s = Session(args=["--norc", "--keep"], broker_dir=broker_dir)
        session_dir = s.session_dir
        try:
            s.drain(0.8)  # broker autostart + inner session spawn + attach
            s.send("export SURVIVE_PROBE=through-death" + ENTER, 0.6)
            s.send("cd /tmp" + ENTER, 0.5)
            check("kept session is live", "SURVIVE_PROBE" in s.screen(), s.screen())
            # Terminal death: SIGKILL the CLIENT. No detach key, no cleanup.
            os.kill(s.pid, signal.SIGKILL)
            os.waitpid(s.pid, 0)
        finally:
            try:
                os.close(s.fd)
            except OSError:
                pass

        s2 = Session(args=["--norc", "--attach"], broker_dir=broker_dir)
        try:
            s2.drain(0.8)
            s2.send("echo probe=$SURVIVE_PROBE in $PWD" + ENTER, 0.8)
            ok = wait_for(s2, "probe=through-death")
            scr = s2.screen()
            check(
                "session state survived client death",
                ok and "/tmp" in scr,
                scr,
            )
            # `exit` inside the kept session ends it; the client reports that.
            s2.send("exit" + ENTER, 1.0)
            check("session end reported", wait_for(s2, "ended"), s2.screen())
        finally:
            s2._owns_broker_dir = False
            s2.close()

        # Stop the broker from a plain shell sharing the broker dir.
        s3 = Session(broker_dir=broker_dir)
        try:
            s3.send("keep stop" + ENTER, 0.6)
        finally:
            s3._owns_broker_dir = True
            s3.close()
    finally:
        shutil.rmtree(broker_dir, ignore_errors=True)
        if session_dir:
            shutil.rmtree(session_dir, ignore_errors=True)


def scenario_keep_attach_takeover():
    ok, reason = broker_runtime_available()
    if not ok:
        skip("scenario_keep_attach_takeover", reason)
        return
    # Two terminals, one job: the second attach takes over (last wins); the
    # first client lands back at its prompt with an honest message — the job
    # was NOT killed, just re-owned.
    import tempfile

    broker_dir = tempfile.mkdtemp(prefix="agsh-pty-steal-")
    try:
        s1 = Session(broker_dir=broker_dir)
        s2 = Session(broker_dir=broker_dir)
        try:
            s1.send("keep -- sh -c 'echo steal-hello; exec cat'" + ENTER, 1.2)
            check("first client attached", "steal-hello" in s1.screen(), s1.screen())

            s2.send("keep attach k1" + ENTER, 1.2)
            check("second client gets replay", "steal-hello" in s2.screen(), s2.screen())

            s1.drain(0.6)
            scr = s1.screen()
            check("loser told the truth (taken over, still running)",
                  "taken over" in scr and "keeps running" in scr, scr)
            check("loser not told the job exited", "exited" not in scr, scr)

            s2.send("\x1d", 0.6)  # detach the winner
            s2.send("keep kill k1 KILL" + ENTER, 0.5)
            s2.send("keep stop" + ENTER, 0.5)
        finally:
            s1._owns_broker_dir = False
            s2._owns_broker_dir = False
            s1.close()
            s2.close()
    finally:
        shutil.rmtree(broker_dir, ignore_errors=True)


SCENARIOS = [
    scenario_terminal_restore_on_signal,
    scenario_session_resume_after_kill,
    scenario_keep_attach_detach,
    scenario_keep_full_session_survives_client_death,
    scenario_keep_attach_takeover,
    scenario_clear_passes_through_in_compact_mode,
    scenario_mode_intercept_toggle,
    scenario_rc_autoload,
    scenario_mode_builtin_session_default,
    scenario_view_image_inline_and_fallback,
    scenario_view_code_highlighting,
    scenario_ls_color_env_seeded,
    scenario_hooks_precmd_preexec_chpwd,
    scenario_programmable_completion,
    scenario_sequential_commands,
    scenario_frecent_jump,
    scenario_huge_history_ghost_is_clipped,
    scenario_basic_echo,
    scenario_line_editing,
    scenario_completion_dropdown,
    scenario_multiline_history_no_staircase,
    scenario_ctrl_c_aborts_line,
    scenario_heredoc_continuation,
    scenario_reverse_search,
    scenario_history_picker_tab_edits_and_mode_cycles,
    scenario_history_tui_command_opens_picker,
    scenario_history_picker_scrolls_all_matches,
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
        if os.environ.get("AGSH_PTY_VERBOSE"):
            print(f"--- {sc.__name__}", flush=True)
        try:
            sc()
        except Exception as e:  # noqa: BLE001
            global FAIL
            FAIL += 1
            print(f"### ERROR in {sc.__name__}: {e}")
    print(
        f"\n================  interactive  PASS={PASS}  FAIL={FAIL}  SKIP={SKIP}  ================"
    )
    shutil.rmtree(_SUITE_TMP, ignore_errors=True)
    sys.exit(1 if FAIL or SKIP else 0)


main()
