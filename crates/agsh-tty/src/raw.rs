//! RAII raw-mode terminal guard built on `rustix` termios (unsafe-free).
//!
//! Entering raw mode disables canonical input/echo so the editor sees every
//! keystroke; the original terminal settings are restored on drop (including
//! on panic, via a guard the editor installs). Bracketed paste is enabled while
//! raw so pasted text arrives as a single block.
//!
//! Termination signals (SIGTERM/SIGHUP) don't unwind, so the `Drop` restore
//! would not run — [`arm_terminal_restore_on_signals`] installs a small handler
//! thread that restores the saved terminal state before the process dies, so a
//! `kill` at the prompt doesn't leave the tty non-canonical (needing `reset`).

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use rustix::termios::{tcgetattr, tcgetwinsize, tcsetattr, OptionalActions, Termios};

/// The terminal's pre-raw settings while a `RawGuard` is live, so the signal
/// handler thread can restore them. `Some` only while we are actually in raw
/// mode (at the prompt); cleared when the guard drops.
static SAVED_TERMIOS: Mutex<Option<Termios>> = Mutex::new(None);
/// Ensures the handler thread is spawned at most once.
static SIGNAL_RESTORE_ARMED: AtomicBool = AtomicBool::new(false);

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

        // Publish the pre-raw settings so a SIGTERM/SIGHUP handler can restore
        // them if we're killed while at the prompt.
        if let Ok(mut saved) = SAVED_TERMIOS.lock() {
            *saved = Some(original.clone());
        }

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
        // Leaving raw mode: a later signal must not re-apply now-stale settings.
        if let Ok(mut saved) = SAVED_TERMIOS.lock() {
            *saved = None;
        }
    }
}

/// Install a handler thread that, on SIGTERM/SIGHUP, restores the terminal to
/// its pre-raw settings before the process dies — so a `kill` while sitting at
/// the raw-mode prompt doesn't leave the tty non-canonical. Idempotent; call
/// once at interactive startup. Panics are already covered (they unwind through
/// the `RawGuard`'s `Drop`); this closes the termination-signal gap.
///
/// The restore runs on a normal thread (via `signal_hook`'s iterator), not in a
/// signal-handler context, so acquiring the mutex and calling `tcsetattr` is
/// sound. After restoring we re-raise the signal's default disposition so the
/// process still terminates as it normally would.
pub fn arm_terminal_restore_on_signals() {
    if SIGNAL_RESTORE_ARMED.swap(true, Ordering::SeqCst) {
        return;
    }
    use signal_hook::consts::{SIGHUP, SIGTERM};
    use signal_hook::iterator::Signals;
    let Ok(mut signals) = Signals::new([SIGTERM, SIGHUP]) else {
        return;
    };
    std::thread::spawn(move || {
        for signal in signals.forever() {
            if let Ok(saved) = SAVED_TERMIOS.lock() {
                if let Some(termios) = saved.as_ref() {
                    let mut out = io::stdout();
                    let _ = out.write_all(b"\x1b[?2004l");
                    let _ = out.flush();
                    let _ = tcsetattr(io::stdin(), OptionalActions::Now, termios);
                }
            }
            // Terminate as if the signal had been unhandled.
            let _ = signal_hook::low_level::emulate_default_handler(signal);
        }
    });
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
