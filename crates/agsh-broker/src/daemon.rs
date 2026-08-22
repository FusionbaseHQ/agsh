//! The broker daemon (`agshd`): owns job PTYs, journals their output, and
//! serves the unix-socket protocol. Runs as `agsh --broker-daemon` in its own
//! session (started detached via `agsh --broker-launch`).

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::paths;
use crate::protocol::{
    read_line, write_line, JobInfo, JobKind, Request, Response, SpawnSpec, MAX_TAIL_BYTES,
};

/// In-memory scrollback kept per job, replayed on attach.
const SCROLLBACK_CAP: usize = 64 * 1024;
/// Rotate a job log once it exceeds this (old generation kept ⇒ ≤2× on disk).
const LOG_ROTATE_BYTES: u64 = 8 * 1024 * 1024;
/// Rotate the daemon's sparse operational log at runtime; retain one generation.
const DAEMON_LOG_ROTATE_BYTES: u64 = 1024 * 1024;
/// Retain a small, bounded recovery window for logs orphaned by a prior daemon
/// generation. They are not addressable through the fresh in-memory job table.
const ORPHAN_LOG_JOB_CAP: usize = 20;
const ORPHAN_LOG_BYTES_CAP: u64 = 128 * 1024 * 1024;
/// Keep at most this many finished jobs listed (oldest pruned on completion).
const FINISHED_CAP: usize = 20;
/// Each kept PTY owns a process, descriptors, memory, and log state. Same-UID
/// clients share this hard global ceiling rather than growing them unboundedly.
const MAX_RUNNING_JOBS: usize = 64;
const MAX_CONNECTIONS: usize = 64;
/// Token-scoped terminal statuses bridge attach EOF to its follow-up status
/// request without allowing clients that never follow up to grow memory.
const PENDING_ATTACH_EXIT_CAP: usize = MAX_CONNECTIONS;
const CONTROL_IO_TIMEOUT: Duration = Duration::from_secs(5);
const ATTACH_WRITE_TIMEOUT: Duration = Duration::from_millis(250);
const ATTACH_INPUT_WRITE_TIMEOUT: Duration = Duration::from_millis(250);
static DAEMON_LOG_REFRESH: Mutex<()> = Mutex::new(());

struct ActiveConnection {
    count: Arc<AtomicUsize>,
}

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
    }
}

fn reserve_connection(count: &Arc<AtomicUsize>, limit: usize) -> Option<ActiveConnection> {
    count
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current < limit).then_some(current + 1)
        })
        .ok()?;
    Some(ActiveConnection {
        count: Arc::clone(count),
    })
}

fn spawn_connection_worker_with<F>(
    worker: F,
    spawn: impl FnOnce(F) -> std::io::Result<()>,
) -> std::io::Result<()> {
    spawn(worker)
}

fn spawn_connection_worker(worker: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    spawn_connection_worker_with(worker, |worker| {
        std::thread::Builder::new()
            .name("agsh-control".into())
            .spawn(worker)
            .map(drop)
    })
}

#[derive(Debug)]
struct ActiveRunningJob {
    count: Arc<AtomicUsize>,
}

impl Drop for ActiveRunningJob {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
    }
}

fn reserve_running_job(
    count: &Arc<AtomicUsize>,
    limit: usize,
) -> std::io::Result<ActiveRunningJob> {
    count
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current < limit).then_some(current + 1)
        })
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!("broker running-job limit reached ({limit})"),
            )
        })?;
    Ok(ActiveRunningJob {
        count: Arc::clone(count),
    })
}

fn open_private_append(path: &Path) -> std::io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::fs::MetadataExt;

    let descriptor = rustix::fs::open(
        path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::APPEND | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    let file = File::from(descriptor);
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "broker log path is not a regular file",
        ));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "broker log is owned by another user",
        ));
    }
    rustix::fs::fchmod(&file, Mode::RUSR | Mode::WUSR)
        .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    Ok(file)
}

fn remove_file_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn rotated_log_path(path: &Path) -> PathBuf {
    path.with_extension("log.old")
}

fn remove_job_logs(log_path: &Path) -> std::io::Result<()> {
    let current = remove_file_if_present(log_path);
    let old = remove_file_if_present(&rotated_log_path(log_path));
    current.and(old)
}

fn rotate_job_log(log_path: &Path) -> std::io::Result<()> {
    let old = rotated_log_path(log_path);
    remove_file_if_present(&old)?;
    std::fs::rename(log_path, old)
}

/// Rotate when the path reaches its threshold. `true` means callers must open
/// and install a fresh current file (the path was absent or was just rotated).
fn rotate_daemon_log(log_path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let metadata = match std::fs::symlink_metadata(log_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "broker daemon log path is not a regular file",
        ));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "broker daemon log is owned by another user",
        ));
    }
    if metadata.len() < DAEMON_LOG_ROTATE_BYTES {
        return Ok(false);
    }

    let old = rotated_log_path(log_path);
    remove_file_if_present(&old)?;
    std::fs::rename(log_path, old)?;
    Ok(true)
}

fn acquire_generation_lock(parent: &Path) -> std::io::Result<File> {
    let path = parent.join("agshd.lock");
    let file = open_private_append(&path)?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
        |error| {
            let io_error = std::io::Error::from_raw_os_error(error.raw_os_error());
            if io_error.kind() == std::io::ErrorKind::WouldBlock {
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "another broker generation is still running",
                )
            } else {
                io_error
            }
        },
    )?;
    Ok(file)
}

fn job_log_sequence(name: &str) -> Option<u64> {
    let rest = name.strip_prefix('k')?;
    let digits = rest
        .strip_suffix(".log.old")
        .or_else(|| rest.strip_suffix(".log"))?;
    let sequence = digits.parse::<u64>().ok()?;
    (sequence > 0 && sequence.to_string() == digits).then_some(sequence)
}

/// Bound logs left by an earlier, now-dead broker generation. The caller must
/// hold the generation lock before invoking this function. Exact broker names
/// are considered; unrelated files are untouched, and symlinks are unlinked
/// rather than followed.
fn prune_orphan_logs(logs_dir: &Path, job_cap: usize, bytes_cap: u64) -> std::io::Result<()> {
    let mut groups: BTreeMap<u64, Vec<(PathBuf, u64)>> = BTreeMap::new();
    let mut total_bytes = 0u64;
    for entry in std::fs::read_dir(logs_dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(sequence) = job_log_sequence(&name) else {
            continue;
        };
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            remove_file_if_present(&path)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("broker job log is not a regular file: {}", path.display()),
            ));
        }
        // Reopen without following a raced final symlink, validate ownership,
        // and restore the broker's 0600 invariant before retaining the file.
        let file = open_private_append(&path)?;
        let bytes = file.metadata()?.len();
        total_bytes = total_bytes.saturating_add(bytes);
        groups.entry(sequence).or_default().push((path, bytes));
    }

    while groups.len() > job_cap || total_bytes > bytes_cap {
        let Some((_sequence, files)) = groups.pop_first() else {
            break;
        };
        for (path, bytes) in files {
            remove_file_if_present(&path)?;
            total_bytes = total_bytes.saturating_sub(bytes);
        }
    }
    Ok(())
}

fn refreshed_daemon_log_file(log: &Path, force_reopen: bool) -> std::io::Result<Option<File>> {
    let reopen = rotate_daemon_log(log)?;
    if force_reopen || reopen {
        open_private_append(log).map(Some)
    } else {
        Ok(None)
    }
}

fn redirect_daemon_log_locked(force_reopen: bool) -> std::io::Result<()> {
    let Some(log) = paths::daemon_log_path() else {
        return Ok(());
    };
    let Some(out) = refreshed_daemon_log_file(&log, force_reopen)? else {
        return Ok(());
    };
    rustix::stdio::dup2_stdout(&out)
        .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    rustix::stdio::dup2_stderr(&out)
        .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))
}

fn redirect_daemon_log(force_reopen: bool) -> std::io::Result<()> {
    let _guard = DAEMON_LOG_REFRESH
        .lock()
        .map_err(|_| std::io::Error::other("daemon log lock poisoned"))?;
    redirect_daemon_log_locked(force_reopen)
}

fn log_daemon_error(message: String) {
    let Ok(_guard) = DAEMON_LOG_REFRESH.lock() else {
        return;
    };
    // Check immediately before the one bounded diagnostic. If this write
    // crosses the threshold, the next diagnostic or accepted connection
    // rotates it, so overshoot is limited to this message.
    let _ = redirect_daemon_log_locked(false);
    eprintln!("{message}");
}

fn configure_requested_cwd(command: &mut Command, cwd: &Path) -> std::io::Result<()> {
    use rustix::fs::{Mode, OFlags};

    if cwd.as_os_str().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "broker spawn cwd is empty",
        ));
    }
    let descriptor = rustix::fs::open(
        cwd,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    drop(descriptor);
    command.current_dir(cwd);
    Ok(())
}

fn ensure_peer_uid(peer_uid: u32) -> std::io::Result<()> {
    let expected_uid = rustix::process::geteuid().as_raw();
    if peer_uid == expected_uid {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("broker peer uid {peer_uid} does not match daemon uid {expected_uid}"),
        ))
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn socket_peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd;

    nix::sys::socket::getsockopt(
        stream.as_raw_fd(),
        nix::sys::socket::sockopt::PeerCredentials,
    )
    .map(|credentials| credentials.uid())
    .map_err(std::io::Error::from)
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly"
))]
fn socket_peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd;

    nix::sys::socket::getsockopt(stream.as_raw_fd(), nix::sys::socket::sockopt::LocalPeerCred)
        .map(|credentials| credentials.uid())
        .map_err(std::io::Error::from)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly"
)))]
fn socket_peer_uid(_stream: &UnixStream) -> std::io::Result<u32> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "broker peer credentials are unsupported on this platform",
    ))
}

fn verify_peer_uid(stream: &UnixStream) -> std::io::Result<()> {
    ensure_peer_uid(socket_peer_uid(stream)?)
}

fn prepare_socket_path(socket: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let metadata = match std::fs::symlink_metadata(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "broker socket path exists and is not a socket",
        ));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "broker socket is owned by another user",
        ));
    }
    match UnixStream::connect(socket) {
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "broker already running",
        )),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            std::fs::remove_file(socket)
        }
        Err(error) => Err(error),
    }
}

fn validate_bound_socket(socket: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

    let metadata = std::fs::symlink_metadata(socket)?;
    if !metadata.file_type().is_socket() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bound broker socket path is not a socket",
        ));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "bound broker socket is owned by another user",
        ));
    }
    if metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "bound broker socket is not mode 0600",
        ));
    }
    Ok(())
}

fn bind_private_listener(socket: &Path) -> std::io::Result<UnixListener> {
    // Unix sockets start as 0777 minus umask. The daemon has no worker threads
    // yet, so temporarily removing owner-execute is process-race-free here.
    let previous_umask = rustix::process::umask(rustix::fs::Mode::from_bits_truncate(0o177));
    let result = UnixListener::bind(socket);
    rustix::process::umask(previous_umask);
    let listener = result?;
    validate_bound_socket(socket)?;
    Ok(listener)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Clone, Copy, PartialEq)]
enum RunState {
    Running,
    /// The direct child was reaped, but the PTY pump is draining final bytes.
    /// PID-targeting operations and new attaches are already forbidden.
    Finishing(i32),
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

/// Run `action` only while the broker still owns an unreaped direct child.
/// The state guard remains held across the action so the pump cannot reap and
/// publish a non-running transition while a PID/controller operation is in
/// flight.
fn with_running_state<T>(
    state: &Mutex<RunState>,
    id: &str,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let state = state
        .lock()
        .map_err(|_| format!("job {id} state lock poisoned"))?;
    if !matches!(*state, RunState::Running) {
        return Err(format!("job {id} has exited"));
    }
    action()
}

/// Serialize attach installation against final exit publication. Both paths
/// take the locks in state → output order, so an attach either installs first
/// and is removed by exit or observes a non-running state and is refused.
fn with_running_output<T>(
    state: &Mutex<RunState>,
    output: &Mutex<Output>,
    action: impl FnOnce(&mut Output) -> T,
) -> Option<T> {
    let state = state.lock().unwrap_or_else(|error| error.into_inner());
    let mut output = output.lock().unwrap_or_else(|error| error.into_inner());
    matches!(*state, RunState::Running).then(|| action(&mut output))
}

fn publish_exit_and_take_attachment(
    state: &Mutex<RunState>,
    output: &Mutex<Output>,
    code: i32,
) -> Option<(u64, UnixStream)> {
    let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
    let mut output = output.lock().unwrap_or_else(|error| error.into_inner());
    *state = RunState::Exited(code);
    output.attach.take()
}

impl Output {
    fn push_and_forward(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if self.ring.len() == SCROLLBACK_CAP {
                self.ring.pop_front();
            }
            self.ring.push_back(byte);
        }
        let attach_failed = if let Some((_, stream)) = &mut self.attach {
            stream
                .write_all(chunk)
                .and_then(|()| stream.flush())
                .is_err()
        } else {
            false
        };
        if attach_failed {
            if let Some((_, stream)) = self.attach.take() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
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
    child: Mutex<ReapOnDropChild>,
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
        self.info_from(state, attached)
    }

    fn info_from(&self, state: RunState, attached: bool) -> JobInfo {
        JobInfo {
            id: self.id.clone(),
            kind: self.kind,
            title: self.title.clone(),
            cwd: self.cwd.clone(),
            pid: self.pid,
            started_at: self.started_at,
            running: !matches!(state, RunState::Exited(_)),
            exit_code: match state {
                RunState::Running | RunState::Finishing(_) => None,
                RunState::Exited(code) => Some(code),
            },
            attached,
            log: self.log_path.display().to_string(),
        }
    }

    fn set_winsize(&self, rows: u16, cols: u16) -> Result<(), String> {
        with_running_state(&self.state, &self.id, || {
            let size = rustix::termios::Winsize {
                ws_row: rows.max(1),
                ws_col: cols.max(1),
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let controller = self
                .controller
                .lock()
                .map_err(|_| format!("job {} controller lock poisoned", self.id))?;
            rustix::termios::tcsetwinsize(&*controller, size)
                .map_err(|error| format!("resize job {}: {error}", self.id))?;
            self.signal_unchecked("WINCH")
        })
    }

    fn signal(&self, name: &str) -> Result<(), String> {
        with_running_state(&self.state, &self.id, || self.signal_unchecked(name))
    }

    fn signal_unchecked(&self, name: &str) -> Result<(), String> {
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

fn retry_interrupted<T>(mut operation: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    loop {
        match operation() {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

struct ReapOnDropChild(Child);

impl ReapOnDropChild {
    fn new(child: Child) -> Self {
        Self(child)
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.0.try_wait()
    }

    fn id(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for ReapOnDropChild {
    fn drop(&mut self) {
        if matches!(retry_interrupted(|| self.0.try_wait()), Ok(Some(_))) {
            return;
        }

        // `Ok(None)` is the normal setup-failure path. For any other wait
        // error, the child state is unknown, so still make a best-effort kill
        // and reap rather than silently abandoning a possibly live process.
        if let Some(pid) = rustix::process::Pid::from_raw(self.0.id() as i32) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = retry_interrupted(|| self.0.kill());
        let _ = retry_interrupted(|| self.0.wait());
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

fn prune_oldest_finished<T>(
    records: &mut Vec<T>,
    cap: usize,
    mut is_finished: impl FnMut(&T) -> bool,
    mut cleanup: impl FnMut(&T) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let excess = records
        .iter()
        .filter(|record| is_finished(record))
        .count()
        .saturating_sub(cap);
    let mut first_error = None;
    for _ in 0..excess {
        let Some(pos) = records.iter().position(&mut is_finished) else {
            break;
        };
        if let Err(error) = cleanup(&records[pos]) {
            first_error.get_or_insert(error);
        }
        // The in-memory bound is authoritative even when best-effort unlinking
        // fails. Startup's orphan sweep gets another chance after this daemon.
        records.remove(pos);
    }
    first_error.map_or(Ok(()), Err)
}

type AttachedExitKey = (String, u64);

fn remember_attached_exit(
    pending: &Mutex<VecDeque<(AttachedExitKey, JobInfo)>>,
    id: &str,
    token: u64,
    info: JobInfo,
) {
    let mut pending = pending.lock().unwrap_or_else(|error| error.into_inner());
    let key = (id.to_string(), token);
    if let Some(pos) = pending.iter().position(|(existing, _)| existing == &key) {
        pending.remove(pos);
    }
    while pending.len() >= PENDING_ATTACH_EXIT_CAP {
        pending.pop_front();
    }
    pending.push_back((key, info));
}

fn take_attached_exit(
    pending: &Mutex<VecDeque<(AttachedExitKey, JobInfo)>>,
    id: &str,
    token: u64,
) -> Option<JobInfo> {
    let mut pending = pending.lock().unwrap_or_else(|error| error.into_inner());
    let key = (id.to_string(), token);
    let pos = pending.iter().position(|(existing, _)| existing == &key)?;
    pending.remove(pos).map(|(_, info)| info)
}

fn discard_attached_exit(
    pending: &Mutex<VecDeque<(AttachedExitKey, JobInfo)>>,
    id: &str,
    token: u64,
) {
    drop(take_attached_exit(pending, id, token));
}

struct Broker {
    jobs: Mutex<Vec<Arc<KeptJob>>>,
    pending_attach_exits: Mutex<VecDeque<(AttachedExitKey, JobInfo)>>,
    running_jobs: Arc<AtomicUsize>,
    next_id: AtomicU64,
    logs_dir: PathBuf,
    exe: PathBuf,
}

fn existing_job_sequence(logs_dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(logs_dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let digits = name.strip_prefix('k')?.strip_suffix(".log")?;
            let sequence = digits.parse::<u64>().ok()?;
            (sequence > 0 && sequence.to_string() == digits).then_some(sequence)
        })
        .max()
        .unwrap_or(0)
}

fn allocate_job_id(sequence: &AtomicU64) -> std::io::Result<String> {
    let previous = sequence
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map_err(|_| std::io::Error::other("broker job id space exhausted"))?;
    Ok(format!("k{}", previous + 1))
}

impl Broker {
    fn find(&self, id: &str) -> Option<Arc<KeptJob>> {
        self.jobs.lock().ok()?.iter().find(|j| j.id == id).cloned()
    }

    fn status(&self, id: &str, attach_token: Option<u64>) -> Option<JobInfo> {
        if let Some(token) = attach_token {
            if let Some(info) = take_attached_exit(&self.pending_attach_exits, id, token) {
                return Some(info);
            }
        }
        self.find(id).map(|job| job.info())
    }

    /// Drop the oldest finished jobs beyond the cap. Jobs are stored in spawn
    /// order, so the first finished match is the oldest.
    fn prune_finished(&self) -> std::io::Result<()> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| std::io::Error::other("broker job table lock poisoned"))?;
        let is_finished = |j: &Arc<KeptJob>| {
            matches!(
                *j.state.lock().unwrap_or_else(|e| e.into_inner()),
                RunState::Exited(_)
            )
        };
        prune_oldest_finished(&mut jobs, FINISHED_CAP, is_finished, |job| {
            remove_job_logs(&job.log_path)
        })
    }

    fn remove_finished(&self, id: &str) -> Result<(), String> {
        let mut jobs = self.jobs.lock().map_err(|_| "lock poisoned".to_string())?;
        let Some(pos) = jobs.iter().position(|job| job.id == id) else {
            return Err(format!("no finished job {id} (running jobs stay listed)"));
        };
        if !matches!(
            *jobs[pos]
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            RunState::Exited(_)
        ) {
            return Err(format!("no finished job {id} (running jobs stay listed)"));
        }
        remove_job_logs(&jobs[pos].log_path)
            .map_err(|error| format!("remove logs for job {id}: {error}"))?;
        jobs.remove(pos);
        Ok(())
    }

    fn spawn(self: &Arc<Self>, spec: SpawnSpec) -> std::io::Result<Arc<KeptJob>> {
        use rustix::fs::{Mode, OFlags};
        use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};

        let SpawnSpec {
            cmd,
            cwd,
            env,
            opaque_env,
            rows,
            cols,
            kind,
            title,
        } = spec;
        if cmd.is_empty() {
            return Err(std::io::Error::other("empty command"));
        }
        let mut command = Command::new(&self.exe);
        command.arg("--supervise").arg("--").args(&cmd);
        configure_requested_cwd(&mut command, Path::new(&cwd))?;
        self.prune_finished()?;
        let running_slot = reserve_running_job(&self.running_jobs, MAX_RUNNING_JOBS)?;

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

        let id = allocate_job_id(&self.next_id)?;
        paths::ensure_dir(&self.logs_dir)?;
        let log_path = self.logs_dir.join(format!("{id}.log"));

        command.env_clear();
        command.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        {
            use std::os::unix::ffi::OsStringExt;
            for (key, value) in opaque_env {
                command.env(
                    std::ffi::OsString::from_vec(key),
                    std::ffi::OsString::from_vec(value),
                );
            }
        }
        if !env.iter().any(|(k, _)| k == "TERM") {
            command.env("TERM", "xterm-256color");
        }
        command.env("AGSH_KEEP_ID", &id);
        command.stdin(Stdio::from(peripheral.try_clone()?));
        command.stdout(Stdio::from(peripheral.try_clone()?));
        command.stderr(Stdio::from(peripheral));
        // Non-blocking controller: the reader thread polls + drains without
        // ever wedging on a quiet PTY (macOS may block instead of EOFing).
        rustix::io::ioctl_fionbio(&controller, true)
            .map_err(|e| std::io::Error::other(format!("nonblock: {e}")))?;
        let controller = File::from(controller);
        let reader_half = controller.try_clone()?;

        // From this point onward every fallible setup path owns a guard that
        // kills and reaps the spawned child if job publication cannot finish.
        let child = ReapOnDropChild::new(command.spawn()?);
        let pid = child.id() as i32;

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
        let pumped = job.clone();
        let broker = Arc::clone(self);
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| std::io::Error::other("broker job table lock poisoned"))?;
        std::thread::Builder::new()
            .name(format!("agsh-pump-{}", job.id))
            .spawn(move || {
                pump_job(&broker, pumped, reader_half, running_slot);
                if let Err(error) = broker.prune_finished() {
                    log_daemon_error(format!("agshd: cannot prune finished job logs: {error}"));
                }
            })?;
        jobs.push(job.clone());
        Ok(job)
    }
}

/// Per-job pump: PTY controller → log file + scrollback + attached client;
/// detects exit, reaps the child, and notifies/detaches the client.
fn pump_job(
    broker: &Broker,
    job: Arc<KeptJob>,
    mut controller: File,
    _running_slot: ActiveRunningJob,
) {
    let mut log = open_private_append(&job.log_path).ok();
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
                    let write_error = if let Some(file) = &mut log {
                        match file.write_all(&chunk[..n]) {
                            Ok(()) => {
                                logged += n as u64;
                                None
                            }
                            Err(error) => Some(error),
                        }
                    } else {
                        None
                    };
                    if let Some(error) = write_error {
                        log = None;
                        logged = 0;
                        log_daemon_error(format!(
                            "agshd: disabling log for job {} after write error: {error}",
                            job.id
                        ));
                    } else if logged > LOG_ROTATE_BYTES {
                        if let Some(mut current) = log.take() {
                            let _ = current.flush();
                            drop(current);
                        }
                        match rotate_job_log(&job.log_path)
                            .and_then(|()| open_private_append(&job.log_path))
                        {
                            Ok(next) => log = Some(next),
                            Err(error) => {
                                log_daemon_error(format!(
                                    "agshd: disabling log for job {} after rotation error: {error}",
                                    job.id
                                ));
                            }
                        }
                        logged = 0;
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
            let mut state = job.state.lock().unwrap_or_else(|error| error.into_inner());
            match *state {
                RunState::Running => {
                    let mut child = job.child.lock().unwrap_or_else(|error| error.into_inner());
                    if let Ok(Some(status)) = child.try_wait() {
                        let code = exit_status_code(status);
                        // Publish non-running while the state guard still
                        // excludes signal/resize/input operations. No PID/PGID
                        // operation can begin after this reap.
                        *state = RunState::Finishing(code);
                        exit_code = Some(code);
                    }
                }
                RunState::Finishing(code) | RunState::Exited(code) => exit_code = Some(code),
            }
        }
        if let Some(code) = exit_code {
            if !saw_data {
                // Child gone and the PTY is drained: finalize.
                if let Some(file) = &mut log {
                    let _ = writeln!(file, "\n[keep {} exited: code {code}]", job.id);
                    let _ = file.flush();
                }
                // Exit publication and attachment removal are one state→output
                // critical section. If an attached record is pruned before the
                // client's status round trip, retain its token-scoped terminal
                // status until consumed (or the bounded cache evicts it).
                if let Some((token, mut stream)) =
                    publish_exit_and_take_attachment(&job.state, &job.output, code)
                {
                    remember_attached_exit(
                        &broker.pending_attach_exits,
                        &job.id,
                        token,
                        job.info(),
                    );
                    let _ = stream.write_all(
                        format!("\r\n[keep: job exited (code {code}) — detaching]\r\n").as_bytes(),
                    );
                    let _ = stream.shutdown(std::net::Shutdown::Both);
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

    // The daemon is a dedicated process. Keeping this umask for its lifetime
    // makes the socket private at bind time instead of exposing a chmod race.
    rustix::process::umask(rustix::fs::Mode::from_bits_truncate(0o077));

    let parent = socket
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    paths::ensure_socket_parent(parent)?;
    let logs_dir = paths::logs_dir()
        .ok_or_else(|| std::io::Error::other("no broker logs path (HOME unset?)"))?;
    paths::ensure_dir(&logs_dir)?;
    let state_dir = logs_dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    paths::ensure_dir(state_dir)?;
    // The socket normally excludes a second daemon, but its pathname can be
    // manually unlinked while the process lives. Hold a separate advisory lock
    // so startup cleanup can prove no prior generation still owns job logs.
    let _generation_lock = acquire_generation_lock(state_dir)?;
    prepare_socket_path(socket)?;
    let listener = bind_private_listener(socket)?;
    redirect_daemon_log(true)?;

    let next_id = existing_job_sequence(&logs_dir);
    prune_orphan_logs(&logs_dir, ORPHAN_LOG_JOB_CAP, ORPHAN_LOG_BYTES_CAP)?;
    let broker = Arc::new(Broker {
        jobs: Mutex::new(Vec::new()),
        pending_attach_exits: Mutex::new(VecDeque::new()),
        running_jobs: Arc::new(AtomicUsize::new(0)),
        next_id: AtomicU64::new(next_id),
        logs_dir,
        exe: std::env::current_exe()?,
    });

    eprintln!("agshd: listening on {}", socket.display());
    let active_connections = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        // The accept loop is the daemon's serialized operational checkpoint.
        // Reopen only at the threshold; a bounded request may overshoot it by
        // one bounded diagnostic before the next accepted connection.
        redirect_daemon_log(false)?;
        if verify_peer_uid(&stream).is_err() {
            continue;
        }
        let Some(connection) = reserve_connection(&active_connections, MAX_CONNECTIONS) else {
            continue;
        };
        let broker = broker.clone();
        if let Err(error) = spawn_connection_worker(move || {
            let _connection = connection;
            let _ = handle_conn(&broker, stream);
        }) {
            log_daemon_error(format!("agshd: cannot start connection worker: {error}"));
        }
    }
    Ok(())
}

fn handle_conn(broker: &Arc<Broker>, stream: UnixStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(CONTROL_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(CONTROL_IO_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream.try_clone()?;
    let Some(request): Option<Request> = read_line(&mut reader)? else {
        return Ok(());
    };
    stream.set_read_timeout(None)?;

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
        Request::Status { id, attach_token } => match broker.status(&id, attach_token) {
            Some(info) => write_line(&mut writer, &Response::Job { info }),
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
            Some(job) => match job.set_winsize(rows, cols) {
                Ok(()) => write_line(&mut writer, &Response::Ok),
                Err(message) => write_line(&mut writer, &Response::err(message)),
            },
            None => write_line(&mut writer, &Response::err(format!("no job {id}"))),
        },
        Request::Attach {
            id,
            rows,
            cols,
            replay,
        } => attach_conn(broker, stream, id, rows, cols, replay),
        Request::Remove { id } => match broker.remove_finished(&id) {
            Ok(()) => write_line(&mut writer, &Response::Ok),
            Err(message) => write_line(&mut writer, &Response::err(message)),
        },
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
    let take = bytes.min(len).min(MAX_TAIL_BYTES);
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
    writer.set_write_timeout(Some(ATTACH_WRITE_TIMEOUT))?;
    let Some(job) = broker.find(&id) else {
        return write_line(&mut writer, &Response::err(format!("no job {id}")));
    };
    if let Err(message) = job.set_winsize(rows, cols) {
        return write_line(&mut writer, &Response::err(message));
    }
    let installed_writer = writer.try_clone()?;

    // Handshake + takeover + replay + install are ONE atomic section under the
    // output lock, so "the client received its handshake" implies "the client
    // holds the attach slot". With the handshake outside the lock, two attaches
    // could invert on a slow machine: the newer one finds the slot still empty,
    // installs itself, and then the OLDER one's delayed install hangs up the
    // newer client and squats on the slot (caught by CI as a takeover test
    // timing failure on a loaded runner).
    let Some(generation) =
        with_running_output(&job.state, &job.output, |output| -> std::io::Result<u64> {
            let generation = output
                .attach_gen
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("attach generation space exhausted"))?;
            // Last attach wins (like `tmux attach -d`): hang up any previous client.
            if let Some((_, old)) = output.attach.take() {
                let _ = old.shutdown(std::net::Shutdown::Both);
            }
            let info = job.info_from(RunState::Running, true);
            write_line(
                &mut writer,
                &Response::Attached {
                    info,
                    token: generation,
                },
            )?;
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
            output.attach_gen = generation;
            output.attach = Some((generation, installed_writer));
            Ok(generation)
        })
    else {
        return write_line(&mut writer, &Response::err(format!("job {id} has exited")));
    };
    let generation = generation?;

    // This thread becomes the input pump: client bytes → PTY.
    let mut reader = stream;
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Err(error) = write_to_controller(&job, &chunk[..n]) {
                    // Once the direct child is reaped, keep this input half
                    // alive until the output pump publishes terminal EOF. That
                    // keeps the token-scoped status bounded by a live attach
                    // connection and avoids losing exit status during pruning.
                    if error.kind() != std::io::ErrorKind::BrokenPipe {
                        log_daemon_error(format!(
                            "agshd: dropping attachment for job {} after input error: {error}",
                            job.id
                        ));
                        break;
                    }
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
    let exited = matches!(
        *job.state.lock().unwrap_or_else(|error| error.into_inner()),
        RunState::Exited(_)
    );
    if !exited {
        discard_attached_exit(&broker.pending_attach_exits, &id, generation);
    }
    Ok(())
}

/// Write client input to the (non-blocking) PTY controller, retrying briefly
/// when the kernel input buffer is full.
fn write_to_controller(job: &KeptJob, bytes: &[u8]) -> std::io::Result<()> {
    with_running_state(&job.state, &job.id, || {
        let mut controller = job
            .controller
            .lock()
            .map_err(|_| format!("job {} controller lock poisoned", job.id))?;
        write_all_with_deadline(&mut *controller, bytes, ATTACH_INPUT_WRITE_TIMEOUT)
            .map_err(|error| format!("write input to job {}: {error}", job.id))
    })
    .map_err(|message| {
        let kind = if message.ends_with("has exited") {
            std::io::ErrorKind::BrokenPipe
        } else {
            std::io::ErrorKind::Other
        };
        std::io::Error::new(kind, message)
    })
}

fn write_all_with_deadline(
    writer: &mut impl Write,
    mut bytes: &[u8],
    timeout: Duration,
) -> std::io::Result<()> {
    let deadline = Instant::now() + timeout;
    while !bytes.is_empty() {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("PTY input remained blocked for {} ms", timeout.as_millis()),
            ));
        }
        match writer.write(bytes) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "PTY closed while writing input",
                ));
            }
            Ok(n) => bytes = &bytes[n..],
            Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(Duration::from_millis(2).min(remaining));
            }
            Err(ref error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
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
    let out = open_private_append(&log)?;
    Command::new(exe)
        .arg("--broker-daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out.try_clone()?))
        .stderr(Stdio::from(out))
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    use std::ffi::OsString;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::atomic::AtomicBool;

    fn test_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("agsh-broker-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn private_log_open_tightens_an_existing_file() {
        let dir = test_dir("log-mode");
        let path = dir.join("job.log");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();

        let mut file = open_private_append(&path).unwrap();
        file.write_all(b"-new").unwrap();
        drop(file);

        assert_eq!(std::fs::read(&path).unwrap(), b"old-new");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn private_log_open_rejects_a_final_symlink() {
        let dir = test_dir("log-symlink");
        let victim = dir.join("victim");
        let log = dir.join("job.log");
        std::fs::write(&victim, b"unchanged").unwrap();
        symlink(&victim, &log).unwrap();

        assert!(open_private_append(&log).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"unchanged");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn connection_reservations_are_bounded_and_released() {
        let count = Arc::new(AtomicUsize::new(0));
        let first = reserve_connection(&count, 2).unwrap();
        let second = reserve_connection(&count, 2).unwrap();
        assert!(reserve_connection(&count, 2).is_none());
        assert_eq!(count.load(Ordering::Acquire), 2);

        drop(first);
        let replacement = reserve_connection(&count, 2).unwrap();
        assert_eq!(count.load(Ordering::Acquire), 2);

        drop(second);
        drop(replacement);
        assert_eq!(count.load(Ordering::Acquire), 0);
    }

    #[test]
    fn failed_connection_worker_spawn_drops_captured_resources() {
        struct DropFlag(Arc<AtomicBool>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let captured = DropFlag(Arc::clone(&dropped));
        let error = spawn_connection_worker_with(
            move || drop(captured),
            |_worker| Err(std::io::Error::other("injected thread exhaustion")),
        )
        .unwrap_err();

        assert!(error.to_string().contains("thread exhaustion"));
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn running_job_reservations_are_bounded_and_released() {
        let count = Arc::new(AtomicUsize::new(0));
        let first = reserve_running_job(&count, 2).unwrap();
        let second = reserve_running_job(&count, 2).unwrap();

        let error = reserve_running_job(&count, 2).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("running-job limit"), "{error}");

        drop(first);
        let replacement = reserve_running_job(&count, 2).unwrap();
        assert_eq!(count.load(Ordering::Acquire), 2);

        drop(second);
        drop(replacement);
        assert_eq!(count.load(Ordering::Acquire), 0);
    }

    #[test]
    fn exit_publication_and_attach_install_are_atomic() {
        for _ in 0..128 {
            let state = Arc::new(Mutex::new(RunState::Running));
            let output = Arc::new(Mutex::new(Output {
                ring: VecDeque::new(),
                attach: None,
                attach_gen: 0,
            }));
            let barrier = Arc::new(std::sync::Barrier::new(3));
            let (attachment, _peer) = UnixStream::pair().unwrap();

            let attach_state = Arc::clone(&state);
            let attach_output = Arc::clone(&output);
            let attach_barrier = Arc::clone(&barrier);
            let attach = std::thread::spawn(move || {
                attach_barrier.wait();
                with_running_output(&attach_state, &attach_output, |output| {
                    output.attach_gen += 1;
                    output.attach = Some((output.attach_gen, attachment));
                })
                .is_some()
            });

            let exit_state = Arc::clone(&state);
            let exit_output = Arc::clone(&output);
            let exit_barrier = Arc::clone(&barrier);
            let exit = std::thread::spawn(move || {
                exit_barrier.wait();
                drop(publish_exit_and_take_attachment(
                    &exit_state,
                    &exit_output,
                    23,
                ));
            });

            barrier.wait();
            let _installed_before_exit = attach.join().unwrap();
            exit.join().unwrap();
            assert!(matches!(*state.lock().unwrap(), RunState::Exited(23)));
            assert!(
                output.lock().unwrap().attach.is_none(),
                "an attachment was installed after exit publication"
            );
        }
    }

    #[test]
    fn non_running_jobs_reject_pid_and_controller_actions() {
        for state in [RunState::Finishing(7), RunState::Exited(7)] {
            let state = Mutex::new(state);
            let touched = AtomicBool::new(false);

            let error = with_running_state(&state, "k1", || {
                touched.store(true, Ordering::Release);
                Ok(())
            })
            .unwrap_err();

            assert_eq!(error, "job k1 has exited");
            assert!(!touched.load(Ordering::Acquire));
        }
    }

    #[test]
    fn controller_write_would_block_has_a_deadline() {
        struct BlockedWriter;

        impl Write for BlockedWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::WouldBlock.into())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let started = Instant::now();
        let error =
            write_all_with_deadline(&mut BlockedWriter, b"input", Duration::from_millis(20))
                .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn spawned_child_guard_kills_and_reaps_on_drop() {
        let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();

        drop(ReapOnDropChild::new(child));

        assert_eq!(
            rustix::process::kill_process(pid, rustix::process::Signal::CONT).unwrap_err(),
            rustix::io::Errno::SRCH
        );
    }

    #[test]
    fn child_cleanup_retries_interrupted_wait_operations() {
        let mut attempts = 0;
        let result = retry_interrupted(|| {
            attempts += 1;
            if attempts <= 2 {
                Err(std::io::ErrorKind::Interrupted.into())
            } else {
                Ok(attempts)
            }
        });

        assert_eq!(result.unwrap(), 3);
        assert_eq!(attempts, 3);
    }

    #[test]
    fn prune_removes_excess_records_even_when_log_cleanup_fails() {
        let mut records = vec![(0_u8, true), (1, true), (2, true), (3, true)];

        let error = prune_oldest_finished(
            &mut records,
            2,
            |(_, finished)| *finished,
            |(id, _)| {
                if *id == 0 {
                    Err(std::io::Error::other("injected unlink failure"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("injected unlink failure"));
        assert_eq!(records, vec![(2, true), (3, true)]);
    }

    #[test]
    fn attached_exit_status_survives_record_pruning_until_consumed() {
        let pending = Mutex::new(VecDeque::new());
        let info = JobInfo {
            id: "k1".into(),
            kind: JobKind::Job,
            title: "exit 37".into(),
            cwd: "/".into(),
            pid: 42,
            started_at: 1,
            running: false,
            exit_code: Some(37),
            attached: false,
            log: "/tmp/k1.log".into(),
        };
        remember_attached_exit(&pending, "k1", 9, info.clone());

        let mut records = vec!["k1"];
        prune_oldest_finished(&mut records, 0, |_| true, |_| Ok(())).unwrap();

        let recovered = take_attached_exit(&pending, "k1", 9).expect("terminal status");
        assert_eq!(recovered.exit_code, Some(37));
        assert!(take_attached_exit(&pending, "k1", 9).is_none());
    }

    #[test]
    fn unclaimed_attached_exit_statuses_have_a_hard_cap() {
        let pending = Mutex::new(VecDeque::new());
        for index in 0..=PENDING_ATTACH_EXIT_CAP {
            let id = format!("k{index}");
            remember_attached_exit(
                &pending,
                &id,
                1,
                JobInfo {
                    id: id.clone(),
                    kind: JobKind::Job,
                    title: "exit 0".into(),
                    cwd: "/".into(),
                    pid: 42,
                    started_at: 1,
                    running: false,
                    exit_code: Some(0),
                    attached: false,
                    log: "/tmp/job.log".into(),
                },
            );
        }

        assert_eq!(pending.lock().unwrap().len(), PENDING_ATTACH_EXIT_CAP);
        assert!(take_attached_exit(&pending, "k0", 1).is_none());
        assert!(take_attached_exit(&pending, &format!("k{PENDING_ATTACH_EXIT_CAP}"), 1).is_some());
    }

    #[test]
    fn job_log_cleanup_removes_current_and_rotated_generations_only() {
        let dir = test_dir("job-log-cleanup");
        let log = dir.join("k1.log");
        let old = dir.join("k1.log.old");
        let neighbor = dir.join("k10.log");
        std::fs::write(&log, b"current").unwrap();
        std::fs::write(&old, b"old").unwrap();
        std::fs::write(&neighbor, b"neighbor").unwrap();

        remove_job_logs(&log).unwrap();

        assert!(!log.exists());
        assert!(!old.exists());
        assert_eq!(std::fs::read(&neighbor).unwrap(), b"neighbor");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn job_log_rotation_fails_if_the_old_generation_is_not_a_file() {
        let dir = test_dir("job-log-rotation-invalid-old");
        let log = dir.join("k1.log");
        let old = dir.join("k1.log.old");
        std::fs::write(&log, b"bounded-current").unwrap();
        std::fs::create_dir(&old).unwrap();

        assert!(rotate_job_log(&log).is_err());
        assert_eq!(std::fs::read(&log).unwrap(), b"bounded-current");
        assert!(old.is_dir());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn daemon_log_rotation_keeps_one_bounded_old_generation() {
        let dir = test_dir("daemon-log-rotation");
        let log = dir.join("agshd.log");
        let old = dir.join("agshd.log.old");
        std::fs::write(&log, vec![b'x'; DAEMON_LOG_ROTATE_BYTES as usize]).unwrap();
        std::fs::write(&old, b"stale").unwrap();

        rotate_daemon_log(&log).unwrap();

        assert!(!log.exists());
        assert_eq!(
            std::fs::metadata(&old).unwrap().len(),
            DAEMON_LOG_ROTATE_BYTES
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn runtime_daemon_log_refresh_reopens_after_the_threshold() {
        let dir = test_dir("daemon-log-runtime-refresh");
        let log = dir.join("agshd.log");
        let old = dir.join("agshd.log.old");
        let mut first = refreshed_daemon_log_file(&log, true)
            .unwrap()
            .expect("initial log file");
        first
            .write_all(&vec![b'x'; DAEMON_LOG_ROTATE_BYTES as usize])
            .unwrap();
        first.flush().unwrap();

        let mut second = refreshed_daemon_log_file(&log, false)
            .unwrap()
            .expect("threshold must reopen the log");
        second.write_all(b"new-generation").unwrap();
        second.flush().unwrap();

        assert_eq!(
            std::fs::metadata(&old).unwrap().len(),
            DAEMON_LOG_ROTATE_BYTES
        );
        assert_eq!(std::fs::read(&log).unwrap(), b"new-generation");
        assert!(refreshed_daemon_log_file(&log, false).unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn generation_lock_excludes_a_second_daemon() {
        let dir = test_dir("generation-lock");
        let first = acquire_generation_lock(&dir).unwrap();

        let error = acquire_generation_lock(&dir).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);

        drop(first);
        acquire_generation_lock(&dir).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn orphan_log_sweep_bounds_retained_jobs_and_never_follows_symlinks() {
        let dir = test_dir("orphan-log-sweep");
        for (name, contents) in [
            ("k1.log", b"1111".as_slice()),
            ("k1.log.old", b"aaaa".as_slice()),
            ("k2.log", b"2222".as_slice()),
            ("k3.log", b"3333".as_slice()),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, contents).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let victim = dir.join("victim");
        std::fs::write(&victim, b"unchanged").unwrap();
        symlink(&victim, dir.join("k4.log")).unwrap();

        prune_orphan_logs(&dir, 2, 8).unwrap();

        assert!(!dir.join("k1.log").exists());
        assert!(!dir.join("k1.log.old").exists());
        assert!(dir.join("k2.log").exists());
        assert!(dir.join("k3.log").exists());
        assert!(!dir.join("k4.log").exists());
        assert_eq!(std::fs::read(&victim).unwrap(), b"unchanged");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn existing_logs_advance_job_ids_without_accepting_similar_names() {
        let dir = test_dir("job-sequence");
        for name in [
            "k1.log",
            "k19.log",
            "k999.log.old",
            "k20.log.tmp",
            "other.log",
            "knot-a-number.log",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }

        let sequence = AtomicU64::new(existing_job_sequence(&dir));

        assert_eq!(allocate_job_id(&sequence).unwrap(), "k20");
        assert_eq!(allocate_job_id(&sequence).unwrap(), "k21");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exhausted_job_id_space_returns_an_error_instead_of_wrapping() {
        let sequence = AtomicU64::new(u64::MAX);
        assert!(allocate_job_id(&sequence).is_err());
        assert_eq!(sequence.load(Ordering::Acquire), u64::MAX);
    }

    #[test]
    fn requested_cwd_rejects_empty_missing_and_non_directory_paths() {
        let dir = test_dir("cwd-invalid");
        let file = dir.join("file");
        let missing = dir.join("missing");
        std::fs::write(&file, b"not a directory").unwrap();

        for path in [Path::new(""), &missing, &file] {
            let mut command = Command::new("/usr/bin/true");
            let error = configure_requested_cwd(&mut command, path).unwrap_err();
            assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::InvalidInput
                        | std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::NotADirectory
                ),
                "{path:?}: {error}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deleted_requested_cwd_fails_at_spawn_instead_of_falling_back() {
        let dir = test_dir("cwd-deleted");
        let cwd = dir.join("requested");
        std::fs::create_dir(&cwd).unwrap();
        let mut command = Command::new("/usr/bin/true");
        configure_requested_cwd(&mut command, &cwd).unwrap();
        std::fs::remove_dir(&cwd).unwrap();

        let error = command.spawn().unwrap_err();

        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn cwd_validation_accepts_non_utf8_paths_without_lossy_conversion() {
        let dir = test_dir("cwd-opaque");
        let opaque = OsString::from_vec(vec![b'd', b'i', b'r', b'-', 0xff]);
        let cwd = dir.join(opaque);
        std::fs::create_dir(&cwd).unwrap();
        let mut command = Command::new("/usr/bin/true");

        configure_requested_cwd(&mut command, &cwd).unwrap();
        let status = command.status().unwrap();

        assert!(status.success());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn peer_uid_must_match_the_daemon_owner() {
        let uid = rustix::process::geteuid().as_raw();
        assert!(ensure_peer_uid(uid).is_ok());
        assert_eq!(
            ensure_peer_uid(uid.wrapping_add(1)).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn connected_unix_peer_credentials_are_available_and_match() {
        let (left, _right) = UnixStream::pair().unwrap();
        verify_peer_uid(&left).unwrap();
    }

    #[test]
    fn socket_path_cleanup_rejects_non_socket_entries() {
        let dir = test_dir("socket-entry");
        let path = dir.join("agshd.sock");
        std::fs::write(&path, b"do not remove").unwrap();

        assert_eq!(
            prepare_socket_path(&path).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"do not remove");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn listener_socket_is_private_at_bind_time() {
        const CHILD_MARKER: &str = "AGSH_TEST_PRIVATE_SOCKET_BIND_CHILD";
        if std::env::var_os(CHILD_MARKER).is_none() {
            let status = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("daemon::tests::listener_socket_is_private_at_bind_time")
                .arg("--nocapture")
                .env(CHILD_MARKER, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let dir = test_dir("socket-mode");
        let path = dir.join("agshd.sock");

        let listener = bind_private_listener(&path).unwrap();

        assert_eq!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(listener);
        let _ = std::fs::remove_dir_all(dir);
    }
}
