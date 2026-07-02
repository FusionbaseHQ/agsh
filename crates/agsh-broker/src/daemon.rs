//! The broker daemon (`agshd`): owns job PTYs, journals their output, and
//! serves the unix-socket protocol. Runs as `agsh --broker-daemon` in its own
//! session (started detached via `agsh --broker-launch`).

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::paths;
use crate::protocol::{read_line, write_line, JobInfo, JobKind, Request, Response, SpawnSpec};

/// In-memory scrollback kept per job, replayed on attach.
const SCROLLBACK_CAP: usize = 64 * 1024;
/// Rotate a job log once it exceeds this (old generation kept ⇒ ≤2× on disk).
const LOG_ROTATE_BYTES: u64 = 8 * 1024 * 1024;
/// Keep at most this many finished jobs listed (oldest pruned on spawn).
const FINISHED_CAP: usize = 20;

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Clone, Copy, PartialEq)]
enum RunState {
    Running,
    Exited(i32),
}

/// The single-lock output plane of a job: scrollback ring + the (at most one)
/// attached client. One mutex ⇒ replay-on-attach and live writes can't
/// reorder or duplicate.
struct Output {
    ring: VecDeque<u8>,
    /// Attached client write-half, tagged with a generation so a stale input
    /// pump can't clear a newer attachment.
    attach: Option<(u64, UnixStream)>,
    attach_gen: u64,
}

impl Output {
    fn push_and_forward(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if self.ring.len() == SCROLLBACK_CAP {
                self.ring.pop_front();
            }
            self.ring.push_back(byte);
        }
        if let Some((_, stream)) = &mut self.attach {
            if stream
                .write_all(chunk)
                .and_then(|()| stream.flush())
                .is_err()
            {
                self.attach = None;
            }
        }
    }
}

struct KeptJob {
    id: String,
    kind: JobKind,
    title: String,
    cwd: String,
    pid: i32,
    started_at: u64,
    log_path: PathBuf,
    /// Non-blocking controller clone for client input and winsize ioctls.
    controller: Mutex<File>,
    child: Mutex<Child>,
    state: Mutex<RunState>,
    output: Mutex<Output>,
}

impl KeptJob {
    fn info(&self) -> JobInfo {
        let state = *self.state.lock().unwrap_or_else(|e| e.into_inner());
        let attached = self
            .output
            .lock()
            .map(|o| o.attach.is_some())
            .unwrap_or(false);
        JobInfo {
            id: self.id.clone(),
            kind: self.kind,
            title: self.title.clone(),
            cwd: self.cwd.clone(),
            pid: self.pid,
            started_at: self.started_at,
            running: state == RunState::Running,
            exit_code: match state {
                RunState::Running => None,
                RunState::Exited(code) => Some(code),
            },
            attached,
            log: self.log_path.display().to_string(),
        }
    }

    fn set_winsize(&self, rows: u16, cols: u16) {
        let size = rustix::termios::Winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if let Ok(controller) = self.controller.lock() {
            let _ = rustix::termios::tcsetwinsize(&*controller, size);
        }
        let _ = self.signal("WINCH");
    }

    fn signal(&self, name: &str) -> Result<(), String> {
        let Some(signal) = signal_by_name(name) else {
            return Err(format!("unknown signal {name}"));
        };
        let Some(pid) = rustix::process::Pid::from_raw(self.pid) else {
            return Err(format!("bad pid {}", self.pid));
        };
        match rustix::process::kill_process_group(pid, signal) {
            Ok(()) => Ok(()),
            // Startup race: the supervisor hasn't called setsid() yet, so the
            // process exists but its GROUP doesn't. Signal the process itself.
            Err(rustix::io::Errno::SRCH) => rustix::process::kill_process(pid, signal)
                .map_err(|e| format!("kill -{name} {}: {e}", self.pid)),
            Err(e) => Err(format!("kill -{name} -{}: {e}", self.pid)),
        }
    }
}

fn signal_by_name(name: &str) -> Option<rustix::process::Signal> {
    use rustix::process::Signal;
    let upper = name.to_ascii_uppercase();
    let upper = upper.strip_prefix("SIG").unwrap_or(&upper);
    Some(match upper {
        "HUP" => Signal::HUP,
        "INT" => Signal::INT,
        "QUIT" => Signal::QUIT,
        "KILL" => Signal::KILL,
        "TERM" => Signal::TERM,
        "CONT" => Signal::CONT,
        "STOP" => Signal::STOP,
        "TSTP" => Signal::TSTP,
        "USR1" => Signal::USR1,
        "USR2" => Signal::USR2,
        "WINCH" => Signal::WINCH,
        _ => return None,
    })
}

struct Broker {
    jobs: Mutex<Vec<Arc<KeptJob>>>,
    next_id: AtomicU64,
    logs_dir: PathBuf,
    exe: PathBuf,
}

impl Broker {
    fn find(&self, id: &str) -> Option<Arc<KeptJob>> {
        self.jobs.lock().ok()?.iter().find(|j| j.id == id).cloned()
    }

    /// Drop the oldest finished jobs beyond the cap (called on spawn). Jobs
    /// are stored in spawn order, so the first finished match is the oldest.
    fn prune_finished(&self) {
        let Ok(mut jobs) = self.jobs.lock() else {
            return;
        };
        let is_finished = |j: &Arc<KeptJob>| {
            !matches!(
                *j.state.lock().unwrap_or_else(|e| e.into_inner()),
                RunState::Running
            )
        };
        let excess = jobs
            .iter()
            .filter(|j| is_finished(j))
            .count()
            .saturating_sub(FINISHED_CAP);
        for _ in 0..excess {
            if let Some(pos) = jobs.iter().position(is_finished) {
                jobs.remove(pos);
            }
        }
    }

    fn spawn(self: &Arc<Self>, spec: SpawnSpec) -> std::io::Result<Arc<KeptJob>> {
        use rustix::fs::{Mode, OFlags};
        use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};

        let SpawnSpec {
            cmd,
            cwd,
            env,
            rows,
            cols,
            kind,
            title,
        } = spec;
        if cmd.is_empty() {
            return Err(std::io::Error::other("empty command"));
        }
        let controller = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)
            .map_err(|e| std::io::Error::other(format!("openpt: {e}")))?;
        // CLOEXEC, or the controller leaks into every job we spawn — a job
        // holding its own PTY controller can never be hung up by our exit
        // (rustix, unlike std, does not set it automatically).
        rustix::io::fcntl_setfd(&controller, rustix::io::FdFlags::CLOEXEC)
            .map_err(|e| std::io::Error::other(format!("cloexec: {e}")))?;
        grantpt(&controller).map_err(|e| std::io::Error::other(format!("grantpt: {e}")))?;
        unlockpt(&controller).map_err(|e| std::io::Error::other(format!("unlockpt: {e}")))?;
        let size = rustix::termios::Winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let _ = rustix::termios::tcsetwinsize(&controller, size);
        let peripheral_name = ptsname(&controller, Vec::new())
            .map_err(|e| std::io::Error::other(format!("ptsname: {e}")))?;
        // CLOEXEC here too: Stdio::from dup2s it onto the job's 0/1/2 (dup2
        // clears the flag), so the job gets stdio without leaking this fd.
        let peripheral = rustix::fs::open(
            &peripheral_name,
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| std::io::Error::other(format!("open pts: {e}")))?;

        let id = format!("k{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        paths::ensure_dir(&self.logs_dir)?;
        let log_path = self.logs_dir.join(format!("{id}.log"));

        let mut command = Command::new(&self.exe);
        command.arg("--supervise").arg("--").args(&cmd);
        if !cwd.is_empty() && Path::new(&cwd).is_dir() {
            command.current_dir(&cwd);
        }
        command.env_clear();
        command.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        if !env.iter().any(|(k, _)| k == "TERM") {
            command.env("TERM", "xterm-256color");
        }
        command.env("AGSH_KEEP_ID", &id);
        command.stdin(Stdio::from(peripheral.try_clone()?));
        command.stdout(Stdio::from(peripheral.try_clone()?));
        command.stderr(Stdio::from(peripheral));
        let child = command.spawn()?;
        let pid = child.id() as i32;

        // Non-blocking controller: the reader thread polls + drains without
        // ever wedging on a quiet PTY (macOS may block instead of EOFing).
        rustix::io::ioctl_fionbio(&controller, true)
            .map_err(|e| std::io::Error::other(format!("nonblock: {e}")))?;
        let controller = File::from(controller);
        let reader_half = controller.try_clone()?;

        let job = Arc::new(KeptJob {
            id,
            kind,
            title,
            cwd,
            pid,
            started_at: unix_now(),
            log_path,
            controller: Mutex::new(controller),
            child: Mutex::new(child),
            state: Mutex::new(RunState::Running),
            output: Mutex::new(Output {
                ring: VecDeque::new(),
                attach: None,
                attach_gen: 0,
            }),
        });
        self.prune_finished();
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.push(job.clone());
        }

        let pumped = job.clone();
        std::thread::spawn(move || pump_job(pumped, reader_half));
        Ok(job)
    }
}

/// Per-job pump: PTY controller → log file + scrollback + attached client;
/// detects exit, reaps the child, and notifies/detaches the client.
fn pump_job(job: Arc<KeptJob>, mut controller: File) {
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&job.log_path)
        .ok();
    let mut logged: u64 = log
        .as_ref()
        .and_then(|f| f.metadata().ok())
        .map(|m| m.len())
        .unwrap_or(0);
    let mut chunk = [0u8; 8192];
    let mut exit_code: Option<i32> = None;

    loop {
        // Drain everything currently readable.
        let mut saw_data = false;
        loop {
            match controller.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    saw_data = true;
                    if let Some(file) = &mut log {
                        let _ = file.write_all(&chunk[..n]);
                        logged += n as u64;
                        if logged > LOG_ROTATE_BYTES {
                            let _ = file.flush();
                            let old = job.log_path.with_extension("log.old");
                            let _ = std::fs::rename(&job.log_path, &old);
                            log = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&job.log_path)
                                .ok();
                            logged = 0;
                        }
                    }
                    if let Ok(mut output) = job.output.lock() {
                        output.push_and_forward(&chunk[..n]);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break, // EIO: peripheral side fully closed
            }
        }

        if exit_code.is_none() {
            if let Ok(mut child) = job.child.lock() {
                if let Ok(Some(status)) = child.try_wait() {
                    exit_code = Some(exit_status_code(status));
                }
            }
        }
        if let Some(code) = exit_code {
            if !saw_data {
                // Child gone and the PTY is drained: finalize.
                if let Ok(mut state) = job.state.lock() {
                    *state = RunState::Exited(code);
                }
                if let Some(file) = &mut log {
                    let _ = writeln!(file, "\n[keep {} exited: code {code}]", job.id);
                }
                if let Ok(mut output) = job.output.lock() {
                    if let Some((_, mut stream)) = output.attach.take() {
                        let _ = stream.write_all(
                            format!("\r\n[keep: job exited (code {code}) — detaching]\r\n")
                                .as_bytes(),
                        );
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                    }
                }
                return;
            }
            // Data still flowing after exit: loop once more to drain.
            continue;
        }

        // Nothing readable and still running: wait for more.
        if !saw_data {
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}

fn exit_status_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(signal) = status.signal() {
        128 + signal
    } else {
        128
    }
}

/// Run the daemon on `socket` (foreground; the caller detaches us). Returns
/// only on a fatal setup error — `shutdown` exits the process.
pub fn run(socket: &Path) -> std::io::Result<()> {
    // Our own session: no controlling terminal, so a terminal dying can never
    // HUP the broker. (Fails harmlessly if we're already a group leader.)
    let _ = rustix::process::setsid();

    if socket.exists() {
        // A live daemon answers; a stale socket (crashed daemon) gets cleaned.
        if UnixStream::connect(socket).is_ok() {
            return Err(std::io::Error::other("broker already running"));
        }
        let _ = std::fs::remove_file(socket);
    }
    if let Some(parent) = socket.parent() {
        paths::ensure_dir(parent)?;
    }
    let listener = UnixListener::bind(socket)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
    }

    let logs_dir = paths::logs_dir().unwrap_or_else(|| PathBuf::from("."));
    let broker = Arc::new(Broker {
        jobs: Mutex::new(Vec::new()),
        next_id: AtomicU64::new(0),
        logs_dir,
        exe: std::env::current_exe()?,
    });

    eprintln!("agshd: listening on {}", socket.display());
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let broker = broker.clone();
        std::thread::spawn(move || {
            let _ = handle_conn(&broker, stream);
        });
    }
    Ok(())
}

fn handle_conn(broker: &Arc<Broker>, stream: UnixStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream.try_clone()?;
    let Some(request): Option<Request> = read_line(&mut reader)? else {
        return Ok(());
    };

    match request {
        Request::Ping => write_line(
            &mut writer,
            &Response::Pong {
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        ),
        Request::Spawn(spec) => match broker.spawn(spec) {
            Ok(job) => write_line(&mut writer, &Response::Job { info: job.info() }),
            Err(e) => write_line(&mut writer, &Response::err(format!("spawn: {e}"))),
        },
        Request::List => {
            let jobs = broker
                .jobs
                .lock()
                .map(|jobs| jobs.iter().map(|j| j.info()).collect())
                .unwrap_or_default();
            write_line(&mut writer, &Response::Jobs { jobs })
        }
        Request::Status { id } => match broker.find(&id) {
            Some(job) => write_line(&mut writer, &Response::Job { info: job.info() }),
            None => write_line(&mut writer, &Response::err(format!("no job {id}"))),
        },
        Request::Tail { id, bytes } => match broker.find(&id) {
            Some(job) => send_tail(&mut writer, &job.log_path, bytes),
            None => write_line(&mut writer, &Response::err(format!("no job {id}"))),
        },
        Request::Signal { id, signal } => match broker.find(&id) {
            Some(job) => match job.signal(&signal) {
                Ok(()) => write_line(&mut writer, &Response::Ok),
                Err(message) => write_line(&mut writer, &Response::err(message)),
            },
            None => write_line(&mut writer, &Response::err(format!("no job {id}"))),
        },
        Request::Resize { id, rows, cols } => match broker.find(&id) {
            Some(job) => {
                job.set_winsize(rows, cols);
                write_line(&mut writer, &Response::Ok)
            }
            None => write_line(&mut writer, &Response::err(format!("no job {id}"))),
        },
        Request::Attach {
            id,
            rows,
            cols,
            replay,
        } => attach_conn(broker, stream, id, rows, cols, replay),
        Request::Remove { id } => {
            let Ok(mut jobs) = broker.jobs.lock() else {
                return write_line(&mut writer, &Response::err("lock poisoned"));
            };
            let before = jobs.len();
            jobs.retain(|j| {
                j.id != id
                    || matches!(
                        *j.state.lock().unwrap_or_else(|e| e.into_inner()),
                        RunState::Running
                    )
            });
            if jobs.len() < before {
                write_line(&mut writer, &Response::Ok)
            } else {
                write_line(
                    &mut writer,
                    &Response::err(format!("no finished job {id} (running jobs stay listed)")),
                )
            }
        }
        Request::Shutdown => {
            write_line(&mut writer, &Response::Ok)?;
            // Kept jobs are their own sessions: closing our PTY controllers
            // hangs them up, which is the documented `broker stop` contract.
            std::process::exit(0);
        }
    }
}

/// Send the last `bytes` of the log file: a `tail` header line, then raw bytes.
fn send_tail(writer: &mut UnixStream, log_path: &Path, bytes: u64) -> std::io::Result<()> {
    let Ok(mut file) = File::open(log_path) else {
        write_line(writer, &Response::Tail { len: 0 })?;
        return Ok(());
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let take = bytes.min(len);
    file.seek(SeekFrom::Start(len - take))?;
    write_line(writer, &Response::Tail { len: take })?;
    let mut remaining = take;
    let mut chunk = [0u8; 8192];
    while remaining > 0 {
        let want = chunk.len().min(remaining as usize);
        let n = file.read(&mut chunk[..want])?;
        if n == 0 {
            break;
        }
        writer.write_all(&chunk[..n])?;
        remaining -= n as u64;
    }
    writer.flush()
}

/// Wire an attach connection to a job: winsize, `attached` response, replay,
/// install as the live client, and pump client bytes into the PTY until EOF.
fn attach_conn(
    broker: &Arc<Broker>,
    stream: UnixStream,
    id: String,
    rows: u16,
    cols: u16,
    replay: u64,
) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    let Some(job) = broker.find(&id) else {
        return write_line(&mut writer, &Response::err(format!("no job {id}")));
    };
    if !matches!(
        *job.state.lock().unwrap_or_else(|e| e.into_inner()),
        RunState::Running
    ) {
        return write_line(&mut writer, &Response::err(format!("job {id} has exited")));
    }
    job.set_winsize(rows, cols);
    write_line(&mut writer, &Response::Attached { info: job.info() })?;

    // Install under the output lock: replay and live output can't interleave.
    let generation = {
        let Ok(mut output) = job.output.lock() else {
            return Ok(());
        };
        // Last attach wins (like `tmux attach -d`): hang up any previous client.
        if let Some((_, old)) = output.attach.take() {
            let _ = old.shutdown(std::net::Shutdown::Both);
        }
        let ring = &output.ring;
        let take = (replay as usize).min(ring.len());
        let start = ring.len() - take;
        let (a, b) = ring.as_slices();
        let mut sent = 0usize;
        for slice in [a, b] {
            let begin = start.saturating_sub(sent);
            if begin < slice.len() {
                writer.write_all(&slice[begin..])?;
            }
            sent += slice.len();
        }
        writer.flush()?;
        output.attach_gen += 1;
        let generation = output.attach_gen;
        output.attach = Some((generation, writer));
        generation
    };

    // This thread becomes the input pump: client bytes → PTY.
    let mut reader = stream;
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if write_to_controller(&job, &chunk[..n]).is_err() {
                    break;
                }
            }
        }
    }
    // Detach (only if we're still the current client).
    if let Ok(mut output) = job.output.lock() {
        if matches!(&output.attach, Some((generation_now, _)) if *generation_now == generation) {
            output.attach = None;
        }
    }
    Ok(())
}

/// Write client input to the (non-blocking) PTY controller, retrying briefly
/// when the kernel input buffer is full.
fn write_to_controller(job: &KeptJob, mut bytes: &[u8]) -> std::io::Result<()> {
    let Ok(mut controller) = job.controller.lock() else {
        return Err(std::io::Error::other("controller lock poisoned"));
    };
    while !bytes.is_empty() {
        match controller.write(bytes) {
            Ok(0) => return Err(std::io::Error::other("pty closed")),
            Ok(n) => bytes = &bytes[n..],
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Spawn the daemon detached (stdin null, stdout/stderr → the daemon log).
/// Used by `agsh --broker-launch`, which exits immediately afterward so the
/// daemon reparents to init and nobody leaks a zombie.
pub fn launch_detached(exe: &Path) -> std::io::Result<()> {
    let log = paths::daemon_log_path()
        .ok_or_else(|| std::io::Error::other("no broker dir (HOME unset?)"))?;
    if let Some(parent) = log.parent() {
        paths::ensure_dir(parent)?;
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    Command::new(exe)
        .arg("--broker-daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out.try_clone()?))
        .stderr(Stdio::from(out))
        .spawn()?;
    Ok(())
}
