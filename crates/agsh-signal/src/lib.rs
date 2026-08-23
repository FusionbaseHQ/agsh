//! Minimal process-global signal setup for agsh's raw-exec intermediaries.
//!
//! Rust initializes Unix programs with `SIGPIPE` ignored. That is useful for
//! ordinary Rust applications, but a transparent shell launch intermediary
//! must restore the Unix default before it replaces itself with a native
//! pipeline producer. This crate isolates that one process-boundary operation
//! from the otherwise unsafe-free shell.

#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]

use std::io;

/// Restore the Unix default `SIGPIPE` disposition before a raw target exec.
///
/// Call this only in a dedicated exec-helper path, before application signal
/// handlers or threads are installed. Changing a process-wide disposition at
/// arbitrary runtime points would create surprising signal behavior even
/// though the operation itself does not violate Rust memory safety.
#[allow(unsafe_code)]
pub fn reset_sigpipe_default_for_exec() -> io::Result<()> {
    // SAFETY: the only supported callers are single-threaded exec-helper entry
    // paths. They have installed no application signal handler, and SIG_DFL is
    // a valid disposition for SIGPIPE on every supported Unix platform.
    let previous = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    if previous == libc::SIG_ERR {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
