"""PTY harness for agsh interactive tests.

Spawns agsh under a pseudo-terminal in an isolated environment, feeds keystrokes,
and reconstructs the on-screen grid via termemu so tests can assert on what a
user would see. This is agsh's equivalent of fish's pexpect suite.
"""
import fcntl
import os
import pty
import select
import shutil
import signal
import struct
import tempfile
import termios
import time

from termemu import Term

_HERE = os.path.dirname(os.path.abspath(__file__))
_REPO = os.path.dirname(os.path.dirname(_HERE))
AGSH = os.environ.get("AGSH", os.path.join(_REPO, "target", "debug", "agsh"))


class Session:
    def __init__(self, rows=20, cols=80, history=None, args=None, session_dir=None,
                 broker_dir=None, extra_env=None):
        self.rows, self.cols = rows, cols
        self.term = Term(rows, cols)
        env = dict(os.environ)
        home = "/tmp/agsh-pty-home"
        os.makedirs(home, exist_ok=True)
        # Session journals go to a per-Session temp dir so scenarios never see
        # each other's (or the developer's) crashed-session restore banners.
        self._owns_session_dir = session_dir is None
        self.session_dir = session_dir or tempfile.mkdtemp(prefix="agsh-pty-sess-")
        # Keep-broker state is isolated too: a scenario that uses `keep` must
        # never autostart a broker in the developer's real state dir. (The
        # scenario is responsible for `keep stop` before closing.)
        self._owns_broker_dir = broker_dir is None
        self.broker_dir = broker_dir or tempfile.mkdtemp(prefix="agsh-pty-broker-")
        env.update(
            HOME=home,
            XDG_CONFIG_HOME=os.path.join(home, ".config"),
            XDG_DATA_HOME=os.path.join(home, ".local", "share"),
            AGSH_HISTORY_FILE=history or "/tmp/agsh-pty-history.jsonl",
            AGSH_SESSION_DIR=self.session_dir,
            AGSH_BROKER_DIR=self.broker_dir,
            TERM="xterm",
            LANG="C",
        )
        env.pop("AGSH_OUTPUT_MODE", None)
        env.pop("NO_COLOR", None)
        if extra_env:
            env.update(extra_env)
        pid, fd = pty.fork()
        # Default to --norc so scenarios are deterministic regardless of any rc
        # file in the isolated HOME; the rc scenario opts in with args=["--rcfile", …].
        argv = [AGSH] + (args if args is not None else ["--norc"])
        if pid == 0:
            os.environ.clear()
            os.environ.update(env)
            os.execv(AGSH, argv)
        self.pid, self.fd = pid, fd
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.drain(0.4)

    def drain(self, dt=0.35):
        # Read WHILE waiting (not sleep-then-read): on macOS a pty's queued
        # output is discarded when its last peripheral fd closes (revoke), so a
        # reader that naps first can lose a dying child's final bytes.
        buf = b""
        deadline = time.time() + dt
        while True:
            r, _, _ = select.select([self.fd], [], [], 0.05)
            if r:
                try:
                    chunk = os.read(self.fd, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                buf += chunk
                continue
            if time.time() >= deadline:
                break
        self.term.feed(buf.decode(errors="replace"))
        return buf

    def send(self, data, dt=0.35):
        os.write(self.fd, data.encode() if isinstance(data, str) else data)
        return self.drain(dt)

    def screen(self):
        return self.term.screen()

    def _wait_exit(self, timeout):
        """Poll-wait for the shell to exit, draining the pty so it never
        blocks on a full output buffer. True once reaped."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                pid, _ = os.waitpid(self.pid, os.WNOHANG)
            except OSError:
                return True  # already reaped
            if pid:
                return True
            self.drain(0.05)
        return False

    def close(self):
        # Polite exit first — but never trust it blindly: the ^C can race the
        # cooked-mode window between a command returning and the editor
        # re-entering raw mode, where it raises SIGINT and the shell discards
        # the type-ahead (including our "exit"). Retype once at the settled
        # prompt, then escalate to SIGKILL. A cleanup path must not be able to
        # hang the whole suite.
        try:
            os.write(self.fd, b"\x03")  # interrupt any pending line
            os.write(self.fd, b"exit\r")
        except OSError:
            pass
        if not self._wait_exit(1.5):
            try:
                os.write(self.fd, b"exit\r")
            except OSError:
                pass
            if not self._wait_exit(1.5):
                try:
                    os.kill(self.pid, signal.SIGKILL)
                except OSError:
                    pass
                try:
                    os.waitpid(self.pid, 0)
                except OSError:
                    pass
        try:
            os.close(self.fd)
        except OSError:
            pass
        if self._owns_session_dir:
            shutil.rmtree(self.session_dir, ignore_errors=True)
        if self._owns_broker_dir:
            shutil.rmtree(self.broker_dir, ignore_errors=True)


# Common control keys.
ENTER = "\r"
TAB = "\t"
ESC = "\x1b"
CTRL_C = "\x03"
CTRL_D = "\x04"
CTRL_R = "\x12"
CTRL_U = "\x15"
CTRL_A = "\x01"
CTRL_E = "\x05"
RIGHT = "\x1b[C"
