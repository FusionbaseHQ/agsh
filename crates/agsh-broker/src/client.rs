//! Broker client: one connection per request, plus a dedicated streaming
//! connection for attach. Auto-starts the daemon on first use.

use std::io::{BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::paths;
use crate::protocol::{
    read_line, write_line, JobInfo, Request, Response, SpawnSpec, MAX_TAIL_BYTES,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
}

impl Client {
    pub fn at(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// The default-socket client (no daemon liveness implied).
    pub fn from_env() -> std::io::Result<Self> {
        paths::socket_path()
            .map(Self::at)
            .ok_or_else(|| std::io::Error::other("no broker socket path (HOME unset?)"))
    }

    /// Connect to a running daemon, starting one (via `exe --broker-launch`)
    /// if none answers. `exe` is the agsh binary.
    pub fn connect_or_start(exe: &Path) -> std::io::Result<Self> {
        let client = Self::from_env()?;
        if client.ping().is_ok() {
            return Ok(client);
        }
        // The launcher spawns the daemon detached and exits immediately, so
        // waiting on it can't hang and the daemon reparents to init.
        let status = std::process::Command::new(exe)
            .arg("--broker-launch")
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other("broker launch failed"));
        }
        for _ in 0..40 {
            if client.ping().is_ok() {
                return Ok(client);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(std::io::Error::other(
            "broker did not come up (see agshd.log in the broker dir)",
        ))
    }

    fn connect(&self) -> std::io::Result<UnixStream> {
        UnixStream::connect(&self.socket)
    }

    fn roundtrip(&self, request: &Request) -> std::io::Result<Response> {
        let stream = self.connect()?;
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        let mut writer = stream.try_clone()?;
        write_line(&mut writer, request)?;
        let mut reader = BufReader::new(stream);
        read_line(&mut reader)?.ok_or_else(|| std::io::Error::other("broker closed connection"))
    }

    fn expect_ok(&self, request: &Request) -> std::io::Result<()> {
        match self.roundtrip(request)? {
            Response::Ok => Ok(()),
            Response::Err { message } => Err(std::io::Error::other(message)),
            other => Err(std::io::Error::other(format!(
                "unexpected broker reply: {other:?}"
            ))),
        }
    }

    pub fn ping(&self) -> std::io::Result<String> {
        match self.roundtrip(&Request::Ping)? {
            Response::Pong { version } => Ok(version),
            other => Err(std::io::Error::other(format!(
                "unexpected broker reply: {other:?}"
            ))),
        }
    }

    pub fn spawn_job(&self, spec: SpawnSpec) -> std::io::Result<JobInfo> {
        let request = Request::Spawn(spec);
        match self.roundtrip(&request)? {
            Response::Job { info } => Ok(info),
            Response::Err { message } => Err(std::io::Error::other(message)),
            other => Err(std::io::Error::other(format!(
                "unexpected broker reply: {other:?}"
            ))),
        }
    }

    pub fn list(&self) -> std::io::Result<Vec<JobInfo>> {
        match self.roundtrip(&Request::List)? {
            Response::Jobs { jobs } => Ok(jobs),
            Response::Err { message } => Err(std::io::Error::other(message)),
            other => Err(std::io::Error::other(format!(
                "unexpected broker reply: {other:?}"
            ))),
        }
    }

    pub fn status(&self, id: &str) -> std::io::Result<JobInfo> {
        match self.roundtrip(&Request::Status { id: id.into() })? {
            Response::Job { info } => Ok(info),
            Response::Err { message } => Err(std::io::Error::other(message)),
            other => Err(std::io::Error::other(format!(
                "unexpected broker reply: {other:?}"
            ))),
        }
    }

    /// Last `bytes` of a job's output log.
    pub fn tail(&self, id: &str, bytes: u64) -> std::io::Result<Vec<u8>> {
        let stream = self.connect()?;
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        let mut writer = stream.try_clone()?;
        write_line(
            &mut writer,
            &Request::Tail {
                id: id.into(),
                bytes,
            },
        )?;
        let mut reader = BufReader::new(stream);
        let header: Response = read_line(&mut reader)?
            .ok_or_else(|| std::io::Error::other("broker closed connection"))?;
        match header {
            Response::Tail { len } => {
                if len > MAX_TAIL_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "broker tail response exceeds size limit",
                    ));
                }
                let mut buf = vec![0u8; len as usize];
                reader.read_exact(&mut buf)?;
                Ok(buf)
            }
            Response::Err { message } => Err(std::io::Error::other(message)),
            other => Err(std::io::Error::other(format!(
                "unexpected broker reply: {other:?}"
            ))),
        }
    }

    pub fn signal(&self, id: &str, signal: &str) -> std::io::Result<()> {
        self.expect_ok(&Request::Signal {
            id: id.into(),
            signal: signal.into(),
        })
    }

    pub fn resize(&self, id: &str, rows: u16, cols: u16) -> std::io::Result<()> {
        self.expect_ok(&Request::Resize {
            id: id.into(),
            rows,
            cols,
        })
    }

    pub fn remove(&self, id: &str) -> std::io::Result<()> {
        self.expect_ok(&Request::Remove { id: id.into() })
    }

    pub fn shutdown(&self) -> std::io::Result<()> {
        self.expect_ok(&Request::Shutdown)
    }

    /// Open a streaming attach connection. On success the returned stream is
    /// past its handshake: bytes written go to the job's PTY, bytes read are
    /// job output (scrollback replay first).
    pub fn attach_stream(
        &self,
        id: &str,
        rows: u16,
        cols: u16,
        replay: u64,
    ) -> std::io::Result<(UnixStream, JobInfo)> {
        let stream = self.connect()?;
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        let mut writer = stream.try_clone()?;
        write_line(
            &mut writer,
            &Request::Attach {
                id: id.into(),
                rows,
                cols,
                replay,
            },
        )?;
        // Read the single handshake line without buffering past it (a
        // BufReader would swallow the first output bytes).
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        let mut raw = stream.try_clone()?;
        loop {
            let n = raw.read(&mut byte)?;
            if n == 0 {
                return Err(std::io::Error::other("broker closed connection"));
            }
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
            if line.len() > 64 * 1024 {
                return Err(std::io::Error::other("oversized attach handshake"));
            }
        }
        let header: Response = serde_json::from_slice(&line).map_err(std::io::Error::other)?;
        match header {
            Response::Attached { info } => {
                stream.set_read_timeout(None)?;
                stream.set_write_timeout(None)?;
                Ok((stream, info))
            }
            Response::Err { message } => Err(std::io::Error::other(message)),
            other => Err(std::io::Error::other(format!(
                "unexpected broker reply: {other:?}"
            ))),
        }
    }
}
