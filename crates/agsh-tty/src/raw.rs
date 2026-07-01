//! RAII raw-mode terminal guard built on `rustix` termios (unsafe-free).
//!
//! Entering raw mode disables canonical input/echo so the editor sees every
//! keystroke; the original terminal settings are restored on drop (including
//! on panic, via a guard the editor installs). Bracketed paste is enabled while
//! raw so pasted text arrives as a single block.

use std::io::{self, Write};

use rustix::termios::{tcgetattr, tcgetwinsize, tcsetattr, OptionalActions, Termios};

/// Restores the terminal's original mode when dropped.
pub struct RawGuard {
    original: Termios,
}

impl RawGuard {
    /// Enter raw mode, returning a guard that restores the prior mode on drop.
    pub fn new() -> io::Result<Self> {
        let stdin = io::stdin();
        let original = tcgetattr(&stdin).map_err(errno)?;
        let mut raw = original.clone();
        raw.make_raw();
        tcsetattr(&stdin, OptionalActions::Now, &raw).map_err(errno)?;

        // Enable bracketed paste so pasted newlines aren't treated as Enter.
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[?2004h");
        let _ = out.flush();

        Ok(Self { original })
    }

    /// Restore the terminal explicitly (also done on drop).
    pub fn restore(&self) {
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[?2004l");
        let _ = out.flush();
        let _ = tcsetattr(io::stdin(), OptionalActions::Now, &self.original);
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Terminal size as (rows, cols), defaulting to 24x80 if it can't be queried.
pub fn term_size() -> (u16, u16) {
    tcgetwinsize(io::stdin())
        .map(|w| (w.ws_row.max(1), w.ws_col.max(1)))
        .unwrap_or((24, 80))
}

fn errno(e: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}
