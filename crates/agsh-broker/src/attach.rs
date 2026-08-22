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
    /// Another client took over the still-running job's attach slot.
    TakenOver,
    /// The broker authoritatively reported the job's exit status.
    Exited(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PumpOutcome {
    Detached,
    StreamEnded,
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
    let (stream, _info, token): (UnixStream, JobInfo, u64) =
        client.attach_stream(id, rows, cols, 64 * 1024)?;
    let raw = RawTty::new()?;
    let outcome = pump(client, id, &stream, (rows, cols));
    drop(raw);
    // Leave the next prompt on a fresh line regardless of job output state.
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\r\n");
    let _ = out.flush();
    match outcome? {
        PumpOutcome::Detached => Ok(AttachOutcome::Detached),
        PumpOutcome::StreamEnded => classify_stream_end(client, id, token),
    }
}

/// A clean attach-stream EOF is ambiguous: either the job exited or a newer
/// client took over. Resolve it with an authoritative broker status response;
/// broker loss and internally inconsistent job state stay errors.
fn classify_stream_end(client: &Client, id: &str, token: u64) -> std::io::Result<AttachOutcome> {
    let info = client.status_after_attach(id, token)?;
    if info.running {
        Ok(AttachOutcome::TakenOver)
    } else if let Some(code) = info.exit_code {
        Ok(AttachOutcome::Exited(code))
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("broker returned no exit code for finished job {id}"),
        ))
    }
}

fn pump(
    client: &Client,
    id: &str,
    stream: &UnixStream,
    mut last_size: (u16, u16),
) -> std::io::Result<PumpOutcome> {
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
        let socket_ready = fds[1]
            .revents()
            .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR);

        if socket_ready {
            match socket_reader.read(&mut chunk) {
                Ok(0) => return Ok(PumpOutcome::StreamEnded),
                Ok(n) => {
                    stdout.write_all(&chunk[..n])?;
                    stdout.flush()?;
                }
                Err(ref error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        if stdin_ready {
            let n = { stdin.lock().read(&mut chunk)? };
            if n == 0 {
                // Local EOF (terminal gone): treat as detach — job survives.
                return Ok(PumpOutcome::Detached);
            }
            if let Some(pos) = chunk[..n].iter().position(|&b| b == DETACH) {
                if pos > 0 {
                    socket_writer.write_all(&chunk[..pos])?;
                }
                let _ = stream.shutdown(std::net::Shutdown::Both);
                return Ok(PumpOutcome::Detached);
            }
            socket_writer.write_all(&chunk[..n])?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_end_without_authoritative_status_is_an_error() {
        let socket = std::env::temp_dir().join(format!(
            "agsh-broker-missing-status-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket);
        let client = Client::at(socket);

        assert!(classify_stream_end(&client, "k1", 7).is_err());
    }
}
