//! The keep broker: a per-user daemon that owns PTYs so processes survive
//! their terminal.
//!
//! Every other shell ties a child's lifetime to the terminal: close the
//! window (or drop the SSH connection during standby) and SIGHUP kills the
//! shell and everything under it. The broker inverts that — a *kept* job runs
//! on a PTY whose controller end is held by `agshd` (a detached `agsh
//! --broker-daemon` process), so the job's lifetime is tied to the daemon,
//! not the terminal. Shells come and go; they *attach* to jobs over a unix
//! socket.
//!
//! Design (deliberately shpool-shaped, not tmux-shaped): one PTY per job, no
//! windows, no panes, no screen redrawing — just lifetime + scrollback. The
//! daemon journals each job's output to a bounded log, keeps an in-memory
//! scrollback tail for instant replay on attach, and tracks exits.
//!
//! Process shape: the daemon spawns each job as `agsh --supervise -- CMD…`
//! with the PTY user side on stdio; the supervisor calls `setsid()` +
//! `TIOCSCTTY` (making the PTY a real controlling terminal, so Ctrl-C and
//! job signals work) and then execs the payload — all in safe Rust, no
//! `pre_exec`. The payload is therefore its own session leader; killing the
//! daemon hangs jobs up, but a *client* dying just detaches.
//!
//! Security: the socket lives in an owned 0700 directory, is created 0600, and
//! every accepted connection must present the daemon user's peer credentials
//! (the ssh-agent model). Job environments are passed explicitly by the
//! spawning shell (confinement propagates via `AGSH_CONFINE` like any child).

pub mod attach;
pub mod client;
pub mod daemon;
pub mod paths;
pub mod protocol;

pub use attach::{attach_interactive, AttachOutcome};
pub use client::Client;
pub use protocol::{JobInfo, JobKind, Request, Response, SpawnSpec};

/// Become a session leader with the PTY on stdin as controlling terminal,
/// then exec the payload. Run as `agsh --supervise -- CMD…` by the daemon.
/// Never returns on success (the process image is replaced).
pub fn supervise_exec(argv: &[String]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let rustix_error = |operation: &str, error: rustix::io::Errno| {
        std::io::Error::other(format!("supervise: {operation}: {error}"))
    };
    // New session: detach from any inherited controlling terminal…
    if let Err(error) = rustix::process::setsid() {
        return rustix_error("setsid", error);
    }
    // …and adopt the broker's PTY (our stdin) as the controlling terminal, so
    // the line discipline delivers SIGINT/SIGQUIT/SIGHUP to the payload.
    if let Err(error) = rustix::process::ioctl_tiocsctty(std::io::stdin()) {
        return rustix_error("acquire controlling terminal", error);
    }
    if argv.is_empty() {
        return std::io::Error::other("supervise: empty command");
    }
    std::process::Command::new(&argv[0]).args(&argv[1..]).exec()
}
