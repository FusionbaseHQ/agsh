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

/// Private environment-key prefix used to carry exact `DYLD_*` bindings across
/// a hardened macOS helper or supervisor boundary. The destination removes
/// these bindings before replacing itself with the requested target.
pub const MACOS_EXEC_ENV_TRANSPORT_PREFIX: &str = "AGSH_INTERNAL_EXEC_DYLD_V1_";

/// Move the target's `DYLD_*` bindings into an inert private namespace before
/// launching a hardened macOS intermediary. macOS can strip the real bindings
/// while starting that intermediary; agsh's raw-exec path restores them only in
/// the target envp. Caller-provided private bindings are removed first.
#[cfg(target_os = "macos")]
pub fn transport_macos_dyld_environment(command: &mut std::process::Command) {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let entries = command
        .get_envs()
        .map(|(name, value)| (name.to_os_string(), value.map(OsStr::to_os_string)))
        .collect::<Vec<_>>();

    for (name, _) in &entries {
        if name
            .as_os_str()
            .as_bytes()
            .starts_with(MACOS_EXEC_ENV_TRANSPORT_PREFIX.as_bytes())
        {
            command.env_remove(name);
        }
    }

    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (name, value) in entries {
        let name_bytes = name.as_os_str().as_bytes();
        if !name_bytes.starts_with(b"DYLD_") {
            continue;
        }
        command.env_remove(&name);
        let Some(value) = value else {
            continue;
        };
        let mut transport = MACOS_EXEC_ENV_TRANSPORT_PREFIX.as_bytes().to_vec();
        transport.reserve(name_bytes.len() * 2);
        for byte in name_bytes {
            transport.push(HEX[(byte >> 4) as usize]);
            transport.push(HEX[(byte & 0x0f) as usize]);
        }
        command.env(OsString::from_vec(transport), value);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn transport_macos_dyld_environment(_command: &mut std::process::Command) {}

/// Become a session leader with the PTY on stdin as controlling terminal.
/// The agsh binary performs the subsequent raw exec so libc cannot add an
/// implicit ENOEXEC shell fallback.
pub fn prepare_supervised_terminal() -> std::io::Result<()> {
    let rustix_error = |operation: &str, error: rustix::io::Errno| {
        std::io::Error::other(format!("supervise: {operation}: {error}"))
    };
    // New session: detach from any inherited controlling terminal…
    if let Err(error) = rustix::process::setsid() {
        return Err(rustix_error("setsid", error));
    }
    // …and adopt the broker's PTY (our stdin) as the controlling terminal, so
    // the line discipline delivers SIGINT/SIGQUIT/SIGHUP to the payload.
    if let Err(error) = rustix::process::ioctl_tiocsctty(std::io::stdin()) {
        return Err(rustix_error("acquire controlling terminal", error));
    }
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{transport_macos_dyld_environment, MACOS_EXEC_ENV_TRANSPORT_PREFIX};
    use std::ffi::OsString;

    #[test]
    fn dyld_transport_replaces_real_and_caller_controlled_bindings() {
        let mut command = std::process::Command::new("/usr/bin/true");
        command
            .env_clear()
            .env("ORDINARY", "kept")
            .env("DYLD_INSERT_LIBRARIES", "/tmp/intercept.dylib")
            .env("DYLD_AGSH_TEST", "custom")
            .env(
                format!("{MACOS_EXEC_ENV_TRANSPORT_PREFIX}deadbeef"),
                "caller-controlled",
            );

        transport_macos_dyld_environment(&mut command);

        let poisoned = OsString::from(format!("{MACOS_EXEC_ENV_TRANSPORT_PREFIX}deadbeef"));
        let active = command
            .get_envs()
            .filter_map(|(name, value)| {
                value.map(|value| (name.to_os_string(), value.to_os_string()))
            })
            .collect::<Vec<(OsString, OsString)>>();
        assert!(active
            .iter()
            .any(|(name, value)| { name == "ORDINARY" && value == "kept" }));
        assert!(!active.iter().any(|(name, _)| {
            name == "DYLD_INSERT_LIBRARIES" || name == "DYLD_AGSH_TEST" || name == &poisoned
        }));
        let mut transported = active
            .iter()
            .filter(|(name, _)| {
                name.to_string_lossy()
                    .starts_with(MACOS_EXEC_ENV_TRANSPORT_PREFIX)
            })
            .map(|(_, value)| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        transported.sort();
        assert_eq!(transported, ["/tmp/intercept.dylib", "custom"]);
    }
}
