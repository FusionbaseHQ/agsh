//! Minimal process-global signal setup for agsh process boundaries.
//!
//! Rust initializes Unix programs with `SIGPIPE` ignored. That is useful for
//! ordinary Rust applications, but a transparent shell launch intermediary
//! must restore the Unix default before it replaces itself with a native
//! pipeline producer. A shell can also inherit `SIGCHLD` ignored, which lets the
//! kernel reap children before agsh can observe their status. This crate
//! isolates those process-boundary operations from the otherwise unsafe-free
//! shell.

#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]

use std::io;

#[allow(unsafe_code)]
fn reset_default(signal: libc::c_int) -> io::Result<()> {
    // SAFETY: callers use this only at documented single-threaded process entry
    // boundaries. SIG_DFL is a valid disposition for both supported signals on
    // every supported Unix platform.
    let previous = unsafe { libc::signal(signal, libc::SIG_DFL) };
    if previous == libc::SIG_ERR {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Restore the Unix default `SIGPIPE` disposition before a raw target exec.
///
/// Call this only in a dedicated exec-helper path, before application signal
/// handlers or threads are installed. Changing a process-wide disposition at
/// arbitrary runtime points would create surprising signal behavior even
/// though the operation itself does not violate Rust memory safety.
pub fn reset_sigpipe_default_for_exec() -> io::Result<()> {
    reset_default(libc::SIGPIPE)
}

/// Restore the Unix default `SIGCHLD` disposition before agsh manages children.
///
/// Call this once in the normal shell entry path, before application signal
/// handlers, threads, or child processes exist. An inherited ignored
/// disposition (or `SA_NOCLDWAIT`) may otherwise auto-reap a child and make its
/// exit status unavailable to the shell.
pub fn reset_sigchld_default_for_shell() -> io::Result<()> {
    reset_default(libc::SIGCHLD)
}
