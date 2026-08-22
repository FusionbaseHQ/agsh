//! Wire protocol: one JSON request line per connection, one JSON response
//! line back. Two ops continue past their response line: `attach` switches
//! the connection to bidirectional raw bytes (client stdin ↔ job PTY), and
//! `tail` sends `len` raw bytes after its header.

use std::io::{BufRead, Read, Write};

use serde::{Deserialize, Serialize};

pub const MAX_PROTOCOL_LINE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_TAIL_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    /// Spawn a kept job on a broker-held PTY.
    Spawn(SpawnSpec),
    List,
    Status {
        id: String,
        /// Only attach clients receive this token. It lets the broker retain
        /// the authoritative terminal status across immediate record pruning.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attach_token: Option<u64>,
    },
    /// Send the last `bytes` of the job's output log (header + raw bytes).
    Tail {
        id: String,
        bytes: u64,
    },
    /// Signal the job's process group (name without SIG prefix, e.g. "TERM").
    Signal {
        id: String,
        signal: String,
    },
    Resize {
        id: String,
        rows: u16,
        cols: u16,
    },
    /// Attach this connection to the job's PTY. After the `attached` response
    /// the server replays up to `replay` bytes of scrollback, then streams
    /// live output; bytes from the client are written to the PTY.
    Attach {
        id: String,
        rows: u16,
        cols: u16,
        replay: u64,
    },
    /// Drop a finished job from the table and delete its log generations.
    Remove {
        id: String,
    },
    /// Stop the daemon. Kept jobs are hung up (their PTY controller closes).
    Shutdown,
}

/// Everything the daemon needs to start a kept job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnSpec {
    pub cmd: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    /// Unix environment entries whose key or value is not valid UTF-8, encoded
    /// as exact bytes for JSON transport to the broker supervisor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub opaque_env: Vec<(Vec<u8>, Vec<u8>)>,
    pub rows: u16,
    pub cols: u16,
    pub kind: JobKind,
    /// Display title (the command line as typed).
    pub title: String,
}

/// What a kept entry is: an ad-hoc job (`keep -- cmd`) or a whole interactive
/// agsh session (`agsh --keep`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Job,
    Session,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: String,
    pub kind: JobKind,
    pub title: String,
    pub cwd: String,
    /// The job's pid == pgid == session id (the supervisor execs the payload).
    pub pid: i32,
    pub started_at: u64,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub attached: bool,
    /// Path of the output log on disk.
    pub log: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "r", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Pong {
        version: String,
    },
    Job {
        info: JobInfo,
    },
    Jobs {
        jobs: Vec<JobInfo>,
    },
    /// `len` raw bytes follow this line.
    Tail {
        len: u64,
    },
    /// The connection now streams raw bytes both ways.
    Attached {
        info: JobInfo,
        /// Correlates the stream's terminal EOF with its authoritative status.
        #[serde(default)]
        token: u64,
    },
    Err {
        message: String,
    },
}

impl Response {
    pub fn err(message: impl Into<String>) -> Self {
        Response::Err {
            message: message.into(),
        }
    }
}

/// Write one JSON line (single `write_all`, so lines can't interleave).
pub fn write_line<T: Serialize>(writer: &mut impl Write, value: &T) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).map_err(std::io::Error::other)?;
    line.push('\n');
    if line.len() > MAX_PROTOCOL_LINE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "broker protocol line exceeds size limit",
        ));
    }
    writer.write_all(line.as_bytes())?;
    writer.flush()
}

/// Read and parse one JSON line. `Ok(None)` on clean EOF.
pub fn read_line<T: for<'de> Deserialize<'de>>(
    reader: &mut impl BufRead,
) -> std::io::Result<Option<T>> {
    let mut line = Vec::new();
    let read = reader
        .take((MAX_PROTOCOL_LINE_BYTES + 1) as u64)
        .read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > MAX_PROTOCOL_LINE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "broker protocol line exceeds size limit",
        ));
    }
    serde_json::from_slice(&line)
        .map(Some)
        .map_err(|e| std::io::Error::other(format!("bad protocol line: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_responses_round_trip() {
        let reqs = vec![
            Request::Ping,
            Request::Spawn(SpawnSpec {
                cmd: vec!["sleep".into(), "5".into()],
                cwd: "/w".into(),
                env: vec![("PATH".into(), "/bin".into())],
                opaque_env: vec![(b"OPAQUE".to_vec(), vec![b'a', 0xff, b'z'])],
                rows: 24,
                cols: 80,
                kind: JobKind::Job,
                title: "sleep 5".into(),
            }),
            Request::Attach {
                id: "k1".into(),
                rows: 40,
                cols: 120,
                replay: 32768,
            },
            Request::Status {
                id: "k1".into(),
                attach_token: Some(7),
            },
            Request::Signal {
                id: "k1".into(),
                signal: "TERM".into(),
            },
        ];
        for req in reqs {
            let mut buf = Vec::new();
            write_line(&mut buf, &req).unwrap();
            let back: Request = read_line(&mut buf.as_slice()).unwrap().unwrap();
            assert_eq!(
                serde_json::to_string(&req).unwrap(),
                serde_json::to_string(&back).unwrap()
            );
        }

        let resp = Response::Attached {
            info: JobInfo {
                id: "k1".into(),
                kind: JobKind::Session,
                title: "agsh".into(),
                cwd: "/w".into(),
                pid: 42,
                started_at: 1,
                running: true,
                exit_code: None,
                attached: true,
                log: "/l/k1.log".into(),
            },
            token: 7,
        };
        let mut buf = Vec::new();
        write_line(&mut buf, &resp).unwrap();
        let back: Response = read_line(&mut buf.as_slice()).unwrap().unwrap();
        match back {
            Response::Attached { info, token } => {
                assert_eq!(info.id, "k1");
                assert!(matches!(info.kind, JobKind::Session));
                assert_eq!(token, 7);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn eof_reads_as_none() {
        let empty: &[u8] = b"";
        let got: Option<Request> = read_line(&mut &*empty).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn oversized_protocol_lines_are_rejected_before_deserialization() {
        let mut input = vec![b' '; MAX_PROTOCOL_LINE_BYTES + 1];
        input.push(b'\n');

        let error = read_line::<Request>(&mut input.as_slice()).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("size limit"));
    }

    #[test]
    fn oversized_serialized_messages_are_not_written() {
        let response = Response::Err {
            message: "x".repeat(MAX_PROTOCOL_LINE_BYTES),
        };
        let mut output = Vec::new();

        let error = write_line(&mut output, &response).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(output.is_empty());
    }
}
