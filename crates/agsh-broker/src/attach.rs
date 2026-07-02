//! Client-side interactive attach: put the local terminal in raw mode and
//! pump bytes between it and the job's PTY over the attach socket.
//!
//! Single-threaded by design: one `poll` loop over {stdin, socket}, so no
//! thread is ever left blocked on stdin to swallow a keystroke after detach.
//! Ctrl-] (0x1D, the telnet escape) detaches; job exit closes the socket.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use crate::client::Client;
use crate::protocol::JobInfo;

/// How an interactive attach ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachOutcome {
    /// The user pressed the detach key; the job keeps running.
    Detached,
    /// The job exited (or the broker went away) while attached.
    Ended,
}

/// The detach byte: Ctrl-].
const DETACH: u8 = 0x1d;

/// RAII raw-mode guard for the client terminal (rustix termios; restores on
/// drop, including on panic).
struct RawTty {
    saved: rustix::termios::Termios,
}

impl RawTty {
    fn new() -> std::io::Result<Self> {
        let stdin = std::io::stdin();
        let saved = rustix::termios::tcgetattr(&stdin)
            .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
        let mut raw = saved.clone();
        raw.make_raw();
        rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &raw)
            .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
        Ok(Self { saved })
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        let _ = rustix::termios::tcsetattr(
            std::io::stdin(),
            rustix::termios::OptionalActions::Now,
            &self.saved,
        );
    }
}

/// Current terminal size as (rows, cols), defaulting to 24×80.
pub fn term_size() -> (u16, u16) {
    rustix::termios::tcgetwinsize(std::io::stdin())
        .map(|w| (w.ws_row.max(1), w.ws_col.max(1)))
        .unwrap_or((24, 80))
}

/// Attach the current terminal to job `id` until detach or job exit. The
/// terminal is in raw mode for the duration; window resizes are forwarded
/// (polled — no signal handler is installed in the host shell).
pub fn attach_interactive(client: &Client, id: &str) -> std::io::Result<AttachOutcome> {
    let (rows, cols) = term_size();
    let (stream, _info): (UnixStream, JobInfo) = client.attach_stream(id, rows, cols, 64 * 1024)?;
    let raw = RawTty::new()?;
    let outcome = pump(client, id, &stream, (rows, cols));
    drop(raw);
    // Leave the next prompt on a fresh line regardless of job output state.
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\r\n");
    let _ = out.flush();
    outcome
}

fn pump(
    client: &Client,
    id: &str,
    stream: &UnixStream,
    mut last_size: (u16, u16),
) -> std::io::Result<AttachOutcome> {
    use rustix::event::{poll, PollFd, PollFlags};

    let stdin = std::io::stdin();
    let mut socket_reader = stream.try_clone()?;
    let mut socket_writer = stream.try_clone()?;
    let mut stdout = std::io::stdout();
    let mut chunk = [0u8; 8192];

    loop {
        let mut fds = [
            PollFd::new(&stdin, PollFlags::IN),
            PollFd::new(stream, PollFlags::IN),
        ];
        // 200ms tick doubles as the resize poll (no SIGWINCH handler needed).
        let ready = poll(
            &mut fds,
            Some(&rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 200_000_000,
            }),
        )
        .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;

        let size = term_size();
        if size != last_size {
            last_size = size;
            let _ = client.resize(id, size.0, size.1);
        }
        if ready == 0 {
            continue;
        }
        let stdin_ready = fds[0].revents().intersects(PollFlags::IN | PollFlags::HUP);
        let socket_ready = fds[1].revents().intersects(PollFlags::IN | PollFlags::HUP);

        if socket_ready {
            match socket_reader.read(&mut chunk) {
                Ok(0) | Err(_) => return Ok(AttachOutcome::Ended),
                Ok(n) => {
                    stdout.write_all(&chunk[..n])?;
                    stdout.flush()?;
                }
            }
        }
        if stdin_ready {
            let n = { stdin.lock().read(&mut chunk)? };
            if n == 0 {
                // Local EOF (terminal gone): treat as detach — job survives.
                return Ok(AttachOutcome::Detached);
            }
            if let Some(pos) = chunk[..n].iter().position(|&b| b == DETACH) {
                if pos > 0 {
                    socket_writer.write_all(&chunk[..pos])?;
                }
                let _ = stream.shutdown(std::net::Shutdown::Both);
                return Ok(AttachOutcome::Detached);
            }
            socket_writer.write_all(&chunk[..n])?;
        }
    }
}
