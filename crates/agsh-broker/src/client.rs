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

fn invalid_job_state(info: &JobInfo, detail: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "broker returned invalid state for job {}: {detail}",
            info.id
        ),
    )
}

fn validate_job_info(info: JobInfo) -> std::io::Result<JobInfo> {
    if info.running && info.exit_code.is_some() {
        return Err(invalid_job_state(&info, "running job has an exit code"));
    }
    if !info.running && info.exit_code.is_none() {
        return Err(invalid_job_state(
            &info,
            "finished job is missing exit code",
        ));
    }
    if info.attached && !info.running {
        return Err(invalid_job_state(&info, "finished job is marked attached"));
    }
    Ok(info)
}

fn validate_job_list(jobs: Vec<JobInfo>) -> std::io::Result<Vec<JobInfo>> {
    jobs.into_iter().map(validate_job_info).collect()
}

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
            Response::Job { info } => validate_job_info(info),
            Response::Err { message } => Err(std::io::Error::other(message)),
            other => Err(std::io::Error::other(format!(
                "unexpected broker reply: {other:?}"
            ))),
        }
    }

    pub fn list(&self) -> std::io::Result<Vec<JobInfo>> {
        match self.roundtrip(&Request::List)? {
            Response::Jobs { jobs } => validate_job_list(jobs),
            Response::Err { message } => Err(std::io::Error::other(message)),
            other => Err(std::io::Error::other(format!(
                "unexpected broker reply: {other:?}"
            ))),
        }
    }

    pub fn status(&self, id: &str) -> std::io::Result<JobInfo> {
        self.status_with_attach_token(id, None)
    }

    /// Resolve a terminal attach EOF even if normal finished-record pruning
    /// removed the job before this status round trip.
    pub fn status_after_attach(&self, id: &str, token: u64) -> std::io::Result<JobInfo> {
        self.status_with_attach_token(id, Some(token))
    }

    fn status_with_attach_token(
        &self,
        id: &str,
        attach_token: Option<u64>,
    ) -> std::io::Result<JobInfo> {
        match self.roundtrip(&Request::Status {
            id: id.into(),
            attach_token,
        })? {
            Response::Job { info } => validate_job_info(info),
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
    ) -> std::io::Result<(UnixStream, JobInfo, u64)> {
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
            Response::Attached { info, token } => {
                let info = validate_job_info(info)?;
                if !info.running {
                    return Err(invalid_job_state(&info, "attached job is not running"));
                }
                stream.set_read_timeout(None)?;
                stream.set_write_timeout(None)?;
                Ok((stream, info, token))
            }
            Response::Err { message } => Err(std::io::Error::other(message)),
            other => Err(std::io::Error::other(format!(
                "unexpected broker reply: {other:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::JobKind;

    fn invalid_info(running: bool, exit_code: Option<i32>) -> JobInfo {
        JobInfo {
            id: "k1".into(),
            kind: JobKind::Job,
            title: "invalid state".into(),
            cwd: "/".into(),
            pid: 42,
            started_at: 1,
            running,
            exit_code,
            attached: false,
            log: "/tmp/k1.log".into(),
        }
    }

    #[test]
    fn status_rejects_a_finished_job_without_an_exit_code() {
        let error = validate_job_info(invalid_info(false, None)).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("missing exit code"), "{error}");
    }

    #[test]
    fn list_rejects_a_running_job_with_an_exit_code() {
        let error = validate_job_list(vec![invalid_info(true, Some(0))]).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("running job has an exit code"),
            "{error}"
        );
    }
}
