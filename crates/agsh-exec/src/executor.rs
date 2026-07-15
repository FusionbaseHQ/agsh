use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agsh_compat::{CommandResolution, Resolver};
use agsh_core::{
    lexer::lex, parse_line, Assignment, CommandGraph, CommandInvocation, CommandListItem,
    ListOperator, Pipeline, QuoteKind, RedirectionMode, RedirectionTarget, ShellError,
    ShellErrorKind, Value, WordSegment, INLINE_HEREDOC_PREFIX,
};
use agsh_output::{
    finalize_trace_status, render_observation_with_raw_ref, CompactionContext, ObservationStreams,
    OutputMode, OutputObservation, RawStreamRef,
};

use crate::builtins::{is_builtin, run_builtin};
use crate::state::{
    BufferedStdin, CapturedTraceStreams, ExactTraceFile, ExactTraceSegment, InterceptInstall,
    LoopControlKind, StreamingStdin, StreamingStdout, TraceSpoolIncompleteMarker, TraceSpoolWriter,
    BACKGROUND_SNAPSHOT_MAX_BYTES, BACKGROUND_SNAPSHOT_READY,
};
use crate::{ShellFunction, ShellState};

const MAX_SHELL_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const MAX_IN_MEMORY_CAPTURE_BYTES: usize = 64 * 1024 * 1024;
const MAX_AGGREGATE_CAPTURE_BYTES: usize = 64 * 1024 * 1024;
const MAX_READ_LINE_BYTES: usize = 1024 * 1024;
const MAX_PTY_CAPTURE_BYTES: usize = 64 * 1024 * 1024;
const BACKGROUND_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
const POST_CHILD_CAPTURE_DRAIN_BYTES: usize = 16 * 1024 * 1024;
const POST_CHILD_CAPTURE_DRAIN_TIME: Duration = Duration::from_millis(100);
const CAPTURE_DRAIN_ACK_TIMEOUT: Duration = Duration::from_secs(2);
pub const CAPTURE_DRAIN_READY: u8 = 0xa7;

static CAPTURE_DRAIN_HELPER: OnceLock<PathBuf> = OnceLock::new();

/// Register the trusted executable used to detach a reader for descriptors
/// retained by descendants after their direct parent exits.
pub fn set_capture_drain_helper(path: PathBuf) {
    let _ = CAPTURE_DRAIN_HELPER.set(path);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureDrainHandoff {
    Transferred,
    Unavailable,
    Ambiguous,
}

fn capture_drain_reaper() -> Option<&'static mpsc::Sender<Child>> {
    static REAPER: OnceLock<Option<mpsc::Sender<Child>>> = OnceLock::new();
    REAPER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel::<Child>();
            std::thread::Builder::new()
                .name("agsh-capture-drain-reaper".to_string())
                .spawn(move || {
                    let mut children = Vec::new();
                    loop {
                        match receiver.recv_timeout(Duration::from_millis(100)) {
                            Ok(child) => children.push(child),
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                        while let Ok(child) = receiver.try_recv() {
                            children.push(child);
                        }
                        children.retain_mut(|child| match child.try_wait() {
                            Ok(Some(_)) => false,
                            Ok(None) => true,
                            Err(_) => {
                                let _ = child.kill();
                                let _ = child.wait();
                                false
                            }
                        });
                    }
                })
                .ok()
                .map(|_| sender)
        })
        .as_ref()
}

fn terminate_capture_drain_worker(child: &mut Child) {
    if let Some(pgid) = rustix::process::Pid::from_raw(child.id() as i32) {
        let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn launch_capture_drain_worker(
    helper: &Path,
    reader: OwnedFd,
    timeout: Duration,
) -> CaptureDrainHandoff {
    let Ok((mut acknowledgement, acknowledge_writer)) = io::pipe() else {
        return CaptureDrainHandoff::Unavailable;
    };
    let Ok(flags) = rustix::fs::fcntl_getfl(&acknowledgement) else {
        return CaptureDrainHandoff::Unavailable;
    };
    if rustix::fs::fcntl_setfl(&acknowledgement, flags | rustix::fs::OFlags::NONBLOCK).is_err() {
        return CaptureDrainHandoff::Unavailable;
    }
    let mut command = Command::new(helper);
    command
        .arg("--capture-drain-run")
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::from(reader))
        .stdout(Stdio::from(acknowledge_writer))
        .stderr(Stdio::null());
    command.process_group(0);
    let Ok(mut worker) = command.spawn() else {
        return CaptureDrainHandoff::Unavailable;
    };

    let deadline = Instant::now() + timeout;
    let mut byte = [0_u8; 1];
    loop {
        match acknowledgement.read(&mut byte) {
            Ok(1) if byte[0] == CAPTURE_DRAIN_READY => {
                let Some(reaper) = capture_drain_reaper() else {
                    terminate_capture_drain_worker(&mut worker);
                    return CaptureDrainHandoff::Ambiguous;
                };
                if let Err(mpsc::SendError(mut worker)) = reaper.send(worker) {
                    terminate_capture_drain_worker(&mut worker);
                    return CaptureDrainHandoff::Ambiguous;
                }
                return CaptureDrainHandoff::Transferred;
            }
            Ok(0) | Ok(1) => {
                terminate_capture_drain_worker(&mut worker);
                return CaptureDrainHandoff::Ambiguous;
            }
            Ok(_) => unreachable!(),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => match worker.try_wait() {
                Ok(Some(_)) | Err(_) => {
                    terminate_capture_drain_worker(&mut worker);
                    return CaptureDrainHandoff::Ambiguous;
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(None) => {
                    terminate_capture_drain_worker(&mut worker);
                    return CaptureDrainHandoff::Ambiguous;
                }
            },
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                terminate_capture_drain_worker(&mut worker);
                return CaptureDrainHandoff::Ambiguous;
            }
        }
    }
}
const INTERRUPTED_CHILD_STATUS_GRACE: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub struct ExecutionOptions {
    pub output_mode: OutputMode,
    pub allow_process_replacement: bool,
}

impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            output_mode: OutputMode::Raw,
            allow_process_replacement: false,
        }
    }
}

#[derive(Debug)]
pub struct CommandOutcome {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub observation: Option<OutputObservation>,
    exact_stdout: Option<Vec<ExactTraceSegment>>,
    exact_stderr: Option<Vec<ExactTraceSegment>>,
    stdout_preview_complete: bool,
    stderr_preview_complete: bool,
    /// Emission order across the two logical streams. Byte ranges refer to the
    /// current `stdout`/`stderr` buffers and let an enclosing compound redirection
    /// merge them without collapsing all stdout ahead of all stderr.
    output_order: Option<Vec<OutputSpan>>,
}

const MAX_OUTPUT_SPANS: usize = 1 << 20;
const MAX_EXACT_TRACE_SEGMENTS: usize = 1 << 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone)]
enum CaptureDestination {
    Stdout,
    Stderr,
    File(Arc<File>),
    Pipe {
        kind: StreamingPipeKind,
        writer: Arc<io::PipeWriter>,
    },
    Discard,
}

#[derive(Debug, Clone)]
struct InheritedCaptureRouting {
    stdout: CaptureDestination,
    stderr: CaptureDestination,
}

impl InheritedCaptureRouting {
    const DEFAULT: Self = Self {
        stdout: CaptureDestination::Stdout,
        stderr: CaptureDestination::Stderr,
    };

    fn is_default(&self) -> bool {
        matches!(&self.stdout, CaptureDestination::Stdout)
            && matches!(&self.stderr, CaptureDestination::Stderr)
    }
}

std::thread_local! {
    static INHERITED_CAPTURE_ROUTING: std::cell::RefCell<InheritedCaptureRouting> =
        const { std::cell::RefCell::new(InheritedCaptureRouting::DEFAULT) };
    static STREAM_RAW_TO_PARENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CANCELLABLE_SHELL_STAGE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct InheritedCaptureRoutingGuard(InheritedCaptureRouting);

impl Drop for InheritedCaptureRoutingGuard {
    fn drop(&mut self) {
        INHERITED_CAPTURE_ROUTING.with(|routing| {
            *routing.borrow_mut() = self.0.clone();
        });
    }
}

fn inherited_capture_routing() -> InheritedCaptureRouting {
    INHERITED_CAPTURE_ROUTING.with(|routing| routing.borrow().clone())
}

fn with_inherited_capture_routing<T>(
    routing: InheritedCaptureRouting,
    run: impl FnOnce() -> T,
) -> T {
    let previous = INHERITED_CAPTURE_ROUTING
        .with(|current| std::mem::replace(&mut *current.borrow_mut(), routing));
    let _guard = InheritedCaptureRoutingGuard(previous);
    run()
}

struct StreamRawToParentGuard(bool);

impl Drop for StreamRawToParentGuard {
    fn drop(&mut self) {
        STREAM_RAW_TO_PARENT.with(|current| current.set(self.0));
    }
}

fn stream_raw_to_parent() -> bool {
    STREAM_RAW_TO_PARENT.with(std::cell::Cell::get)
}

fn with_stream_raw_to_parent<T>(enabled: bool, run: impl FnOnce() -> T) -> T {
    let previous = STREAM_RAW_TO_PARENT.with(|current| current.replace(enabled));
    let _guard = StreamRawToParentGuard(previous);
    run()
}

struct CancellableShellStageGuard(bool);

impl Drop for CancellableShellStageGuard {
    fn drop(&mut self) {
        CANCELLABLE_SHELL_STAGE.with(|current| current.set(self.0));
    }
}

fn cancellable_shell_stage() -> bool {
    CANCELLABLE_SHELL_STAGE.with(std::cell::Cell::get)
}

fn with_cancellable_shell_stage<T>(run: impl FnOnce() -> T) -> T {
    let previous = CANCELLABLE_SHELL_STAGE.with(|current| current.replace(true));
    let _guard = CancellableShellStageGuard(previous);
    run()
}

fn prepare_compound_redirections(
    redirections: &[ExpandedRedirection],
    state: &ShellState,
) -> Result<(InheritedCaptureRouting, Option<RedirectedShellStdin>), ShellError> {
    validate_expanded_redirection_descriptors(redirections)?;
    let mut routing = inherited_capture_routing();
    let mut stdin = None;
    for redirection in redirections {
        match (&redirection.mode, &redirection.target) {
            (RedirectionMode::Read, ExpandedRedirectionTarget::Path(path))
                if redirection.fd == 0 =>
            {
                stdin = Some(RedirectedShellStdin::File(open_read_redirection(path)?));
            }
            (
                RedirectionMode::HereDoc | RedirectionMode::HereString,
                ExpandedRedirectionTarget::Bytes(bytes),
            ) if redirection.fd == 0 => {
                stdin = Some(RedirectedShellStdin::Buffered(bytes.clone()));
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 0 => {
                stdin = Some(RedirectedShellStdin::Buffered(Vec::new()));
            }
            (
                RedirectionMode::Write | RedirectionMode::WriteClobber | RedirectionMode::Append,
                ExpandedRedirectionTarget::Path(path),
            ) if redirection.fd == 1 || redirection.fd == 2 => {
                let file = match redirection.mode {
                    RedirectionMode::Append => {
                        OpenOptions::new().create(true).append(true).open(path)?
                    }
                    RedirectionMode::WriteClobber => {
                        open_write_redirection(path, state.noclobber(), true)?
                    }
                    _ => open_write_redirection(path, state.noclobber(), false)?,
                };
                let destination = CaptureDestination::File(Arc::new(file));
                if redirection.fd == 1 {
                    routing.stdout = destination;
                } else {
                    routing.stderr = destination;
                }
            }
            (RedirectionMode::WriteBoth, ExpandedRedirectionTarget::Path(path)) => {
                let destination = CaptureDestination::File(Arc::new(open_write_redirection(
                    path,
                    state.noclobber(),
                    false,
                )?));
                routing.stdout = destination.clone();
                routing.stderr = destination;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 1 => {
                routing.stdout = CaptureDestination::Discard;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 2 => {
                routing.stderr = CaptureDestination::Discard;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(1)) if redirection.fd == 2 => {
                routing.stderr = routing.stdout.clone();
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(2)) if redirection.fd == 1 => {
                routing.stdout = routing.stderr.clone();
            }
            _ => {}
        }
    }
    Ok((routing, stdin))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutputSpan {
    stream: OutputStream,
    start: usize,
    len: usize,
}

impl CommandOutcome {
    pub fn captured(exit_code: i32, stdout: Vec<u8>, stderr: Vec<u8>) -> Self {
        let output_order = Some(initial_output_order(stdout.len(), stderr.len()));
        Self {
            exit_code,
            stdout,
            stderr,
            observation: None,
            exact_stdout: None,
            exact_stderr: None,
            stdout_preview_complete: true,
            stderr_preview_complete: true,
            output_order,
        }
    }

    fn captured_with_exact(
        exit_code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        exact_stdout: Option<ExactTraceFile>,
        exact_stderr: Option<ExactTraceFile>,
    ) -> Self {
        let output_order = Some(initial_output_order(stdout.len(), stderr.len()));
        Self {
            exit_code,
            stdout,
            stderr,
            observation: None,
            exact_stdout: exact_stdout.map(|file| vec![ExactTraceSegment::File(file)]),
            exact_stderr: exact_stderr.map(|file| vec![ExactTraceSegment::File(file)]),
            stdout_preview_complete: true,
            stderr_preview_complete: true,
            output_order,
        }
    }

    fn captured_from_streams(
        exit_code: i32,
        stdout: CapturedStream,
        stderr: CapturedStream,
    ) -> Self {
        let output_order = Some(initial_output_order(
            stdout.preview.len(),
            stderr.preview.len(),
        ));
        Self {
            exit_code,
            stdout: stdout.preview,
            stderr: stderr.preview,
            observation: None,
            exact_stdout: stdout.exact.map(|file| vec![ExactTraceSegment::File(file)]),
            exact_stderr: stderr.exact.map(|file| vec![ExactTraceSegment::File(file)]),
            stdout_preview_complete: stdout.preview_complete,
            stderr_preview_complete: stderr.preview_complete,
            output_order,
        }
    }

    fn merge_exact_stderr_into_stdout(&mut self) {
        if self.exact_stdout.is_some() || self.exact_stderr.is_some() {
            let stdout = take_exact_or_complete_preview(
                &mut self.exact_stdout,
                &self.stdout,
                self.stdout_preview_complete,
            );
            let stderr = take_exact_or_complete_preview(
                &mut self.exact_stderr,
                &self.stderr,
                self.stderr_preview_complete,
            );
            self.exact_stdout = stdout.zip(stderr).map(|(mut stdout, stderr)| {
                stdout.extend(stderr);
                stdout
            });
        }
        self.stdout_preview_complete &= self.stderr_preview_complete;
    }

    fn merge_exact_stdout_into_stderr(&mut self) {
        if self.exact_stdout.is_some() || self.exact_stderr.is_some() {
            let stderr = take_exact_or_complete_preview(
                &mut self.exact_stderr,
                &self.stderr,
                self.stderr_preview_complete,
            );
            let stdout = take_exact_or_complete_preview(
                &mut self.exact_stdout,
                &self.stdout,
                self.stdout_preview_complete,
            );
            self.exact_stderr = stderr.zip(stdout).map(|(mut stderr, stdout)| {
                stderr.extend(stdout);
                stderr
            });
        }
        self.stderr_preview_complete &= self.stdout_preview_complete;
    }

    fn take_exact_streams(
        &mut self,
    ) -> (
        Option<Vec<ExactTraceSegment>>,
        Option<Vec<ExactTraceSegment>>,
    ) {
        (self.exact_stdout.take(), self.exact_stderr.take())
    }

    fn append_streams(&mut self, other: &mut Self) -> Result<(), ShellError> {
        self.append_streams_with_limit(other, true, true, MAX_AGGREGATE_CAPTURE_BYTES)?;
        Ok(())
    }

    fn append_stderr(&mut self, other: &mut Self) -> Result<(), ShellError> {
        self.append_streams_with_limit(other, false, true, MAX_AGGREGATE_CAPTURE_BYTES)?;
        Ok(())
    }

    fn append_streams_with_limit(
        &mut self,
        other: &mut Self,
        include_stdout: bool,
        include_stderr: bool,
        limit: usize,
    ) -> Result<(), ShellError> {
        let projected =
            projected_aggregate_capture_bytes(self, other, include_stdout, include_stderr)?;
        if projected > limit {
            return Err(ShellError::execution(format!(
                "captured compound output exceeds the {limit}-byte aggregate memory limit"
            )));
        }

        self.append_output_order(other, include_stdout, include_stderr);
        if include_stdout {
            append_exact_segments(
                &mut self.exact_stdout,
                &self.stdout,
                self.stdout_preview_complete,
                other.exact_stdout.take(),
                &other.stdout,
                other.stdout_preview_complete,
            );
            self.stdout.append(&mut other.stdout);
            self.stdout_preview_complete &= other.stdout_preview_complete;
            if self.exact_stdout.is_some() {
                self.stdout_preview_complete &= bound_observation_preview(&mut self.stdout);
            }
        }
        if include_stderr {
            append_exact_segments(
                &mut self.exact_stderr,
                &self.stderr,
                self.stderr_preview_complete,
                other.exact_stderr.take(),
                &other.stderr,
                other.stderr_preview_complete,
            );
            self.stderr.append(&mut other.stderr);
            self.stderr_preview_complete &= other.stderr_preview_complete;
            if self.exact_stderr.is_some() {
                self.stderr_preview_complete &= bound_observation_preview(&mut self.stderr);
            }
        }
        Ok(())
    }

    fn append_output_order(&mut self, other: &Self, include_stdout: bool, include_stderr: bool) {
        let Some(mut destination) = self.validated_output_order() else {
            self.output_order = None;
            return;
        };
        let Some(source) = other.validated_output_order() else {
            self.output_order = None;
            return;
        };
        let stdout_offset = self.stdout.len();
        let stderr_offset = self.stderr.len();
        for mut span in source {
            match span.stream {
                OutputStream::Stdout if include_stdout => span.start += stdout_offset,
                OutputStream::Stderr if include_stderr => span.start += stderr_offset,
                _ => continue,
            }
            push_output_span(&mut destination, span);
            if destination.len() > MAX_OUTPUT_SPANS {
                self.output_order = None;
                return;
            }
        }
        self.output_order = Some(destination);
    }

    fn validated_output_order(&self) -> Option<Vec<OutputSpan>> {
        let spans = self.output_order.as_ref()?;
        let valid = spans.iter().all(|span| {
            let stream_len = match span.stream {
                OutputStream::Stdout => self.stdout.len(),
                OutputStream::Stderr => self.stderr.len(),
            };
            span.start
                .checked_add(span.len)
                .is_some_and(|end| end <= stream_len)
        });
        valid.then(|| spans.clone())
    }
}

fn projected_aggregate_capture_bytes(
    destination: &CommandOutcome,
    source: &CommandOutcome,
    include_stdout: bool,
    include_stderr: bool,
) -> Result<usize, ShellError> {
    let stdout = projected_stream_capture_bytes(
        &destination.stdout,
        destination.exact_stdout.as_deref(),
        destination.stdout_preview_complete,
        include_stdout.then_some(source.stdout.as_slice()),
        include_stdout
            .then_some(source.exact_stdout.as_deref())
            .flatten(),
        !include_stdout || source.stdout_preview_complete,
    )?;
    let stderr = projected_stream_capture_bytes(
        &destination.stderr,
        destination.exact_stderr.as_deref(),
        destination.stderr_preview_complete,
        include_stderr.then_some(source.stderr.as_slice()),
        include_stderr
            .then_some(source.exact_stderr.as_deref())
            .flatten(),
        !include_stderr || source.stderr_preview_complete,
    )?;
    let stream_bytes = stdout
        .checked_add(stderr)
        .ok_or_else(|| ShellError::execution("captured compound output length overflow"))?;
    let output_spans =
        projected_output_span_count(destination, source, include_stdout, include_stderr)?;
    if output_spans > MAX_OUTPUT_SPANS {
        return Err(ShellError::execution(format!(
            "captured compound output exceeds the {MAX_OUTPUT_SPANS}-span ordering limit"
        )));
    }
    let exact_segments =
        projected_exact_segment_count(destination, source, include_stdout, include_stderr)?;
    if exact_segments > MAX_EXACT_TRACE_SEGMENTS {
        return Err(ShellError::execution(format!(
            "captured compound output exceeds the {MAX_EXACT_TRACE_SEGMENTS}-segment trace limit"
        )));
    }

    stream_bytes
        .checked_add(
            output_spans
                .checked_mul(std::mem::size_of::<OutputSpan>())
                .ok_or_else(|| ShellError::execution("captured output metadata overflow"))?,
        )
        .and_then(|bytes| {
            bytes.checked_add(exact_segments * std::mem::size_of::<ExactTraceSegment>())
        })
        .ok_or_else(|| ShellError::execution("captured compound output length overflow"))
}

fn projected_output_span_count(
    destination: &CommandOutcome,
    source: &CommandOutcome,
    include_stdout: bool,
    include_stderr: bool,
) -> Result<usize, ShellError> {
    let Some(destination_spans) = destination.output_order.as_deref() else {
        return Ok(0);
    };
    let Some(source_spans) = source.output_order.as_deref() else {
        return Ok(0);
    };
    let included_source = source_spans
        .iter()
        .filter(|span| match span.stream {
            OutputStream::Stdout => include_stdout,
            OutputStream::Stderr => include_stderr,
        })
        .count();
    destination_spans
        .len()
        .checked_add(included_source)
        .ok_or_else(|| ShellError::execution("captured output metadata overflow"))
}

fn projected_exact_segment_count(
    destination: &CommandOutcome,
    source: &CommandOutcome,
    include_stdout: bool,
    include_stderr: bool,
) -> Result<usize, ShellError> {
    let stdout = projected_stream_exact_segment_count(
        destination.exact_stdout.as_deref(),
        &destination.stdout,
        destination.stdout_preview_complete,
        include_stdout
            .then_some(source.exact_stdout.as_deref())
            .flatten(),
        include_stdout.then_some(source.stdout.as_slice()),
        !include_stdout || source.stdout_preview_complete,
    );
    let stderr = projected_stream_exact_segment_count(
        destination.exact_stderr.as_deref(),
        &destination.stderr,
        destination.stderr_preview_complete,
        include_stderr
            .then_some(source.exact_stderr.as_deref())
            .flatten(),
        include_stderr.then_some(source.stderr.as_slice()),
        !include_stderr || source.stderr_preview_complete,
    );
    stdout
        .checked_add(stderr)
        .ok_or_else(|| ShellError::execution("captured output metadata overflow"))
}

fn projected_stream_exact_segment_count(
    destination_exact: Option<&[ExactTraceSegment]>,
    destination_preview: &[u8],
    destination_preview_complete: bool,
    source_exact: Option<&[ExactTraceSegment]>,
    source_preview: Option<&[u8]>,
    source_preview_complete: bool,
) -> usize {
    if destination_exact.is_none() && source_exact.is_none() {
        return 0;
    }
    if (destination_exact.is_none() && !destination_preview_complete)
        || (source_exact.is_none() && !source_preview_complete)
    {
        return 0;
    }
    destination_exact.map_or(
        usize::from(!destination_preview.is_empty()),
        <[ExactTraceSegment]>::len,
    ) + source_exact.map_or(
        usize::from(source_preview.is_some_and(|preview| !preview.is_empty())),
        <[ExactTraceSegment]>::len,
    )
}

fn projected_stream_capture_bytes(
    destination_preview: &[u8],
    destination_exact: Option<&[ExactTraceSegment]>,
    destination_preview_complete: bool,
    source_preview: Option<&[u8]>,
    source_exact: Option<&[ExactTraceSegment]>,
    source_preview_complete: bool,
) -> Result<usize, ShellError> {
    let source_preview = source_preview.unwrap_or_default();
    let exact_present = (destination_exact.is_some() || source_exact.is_some())
        && (destination_exact.is_some() || destination_preview_complete)
        && (source_exact.is_some() || source_preview_complete);
    let preview_bytes = destination_preview
        .len()
        .checked_add(source_preview.len())
        .ok_or_else(|| ShellError::execution("captured compound output length overflow"))?;
    let preview_bytes = if exact_present && preview_bytes > CAPTURE_HEAD + CAPTURE_TAIL {
        CAPTURE_HEAD + CAPTURE_TAIL + 512
    } else {
        preview_bytes
    };

    let exact_memory_bytes = if exact_present {
        let mut bytes = exact_segment_memory_bytes(destination_exact)?
            .checked_add(exact_segment_memory_bytes(source_exact)?)
            .ok_or_else(|| ShellError::execution("captured compound output length overflow"))?;
        if destination_exact.is_some() && source_exact.is_none() {
            bytes = bytes
                .checked_add(source_preview.len())
                .ok_or_else(|| ShellError::execution("captured compound output length overflow"))?;
        } else if destination_exact.is_none() && source_exact.is_some() {
            bytes = bytes
                .checked_add(destination_preview.len())
                .ok_or_else(|| ShellError::execution("captured compound output length overflow"))?;
        }
        bytes
    } else {
        0
    };

    preview_bytes
        .checked_add(exact_memory_bytes)
        .ok_or_else(|| ShellError::execution("captured compound output length overflow"))
}

fn exact_segment_memory_bytes(segments: Option<&[ExactTraceSegment]>) -> Result<usize, ShellError> {
    segments
        .unwrap_or_default()
        .iter()
        .try_fold(0usize, |total, segment| match segment {
            ExactTraceSegment::Memory(bytes) => total
                .checked_add(bytes.len())
                .ok_or_else(|| ShellError::execution("captured compound output length overflow")),
            ExactTraceSegment::File(_) => Ok(total),
        })
}

fn initial_output_order(stdout_len: usize, stderr_len: usize) -> Vec<OutputSpan> {
    let mut spans = Vec::with_capacity(usize::from(stdout_len > 0) + usize::from(stderr_len > 0));
    if stdout_len > 0 {
        spans.push(OutputSpan {
            stream: OutputStream::Stdout,
            start: 0,
            len: stdout_len,
        });
    }
    if stderr_len > 0 {
        spans.push(OutputSpan {
            stream: OutputStream::Stderr,
            start: 0,
            len: stderr_len,
        });
    }
    spans
}

fn push_output_span(spans: &mut Vec<OutputSpan>, span: OutputSpan) {
    if span.len == 0 {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if last.stream == span.stream && last.start.checked_add(last.len) == Some(span.start) {
            last.len = last.len.saturating_add(span.len);
            return;
        }
    }
    spans.push(span);
}

fn append_exact_segments(
    destination: &mut Option<Vec<ExactTraceSegment>>,
    destination_preview: &[u8],
    destination_preview_complete: bool,
    source: Option<Vec<ExactTraceSegment>>,
    source_preview: &[u8],
    source_preview_complete: bool,
) {
    if destination.is_none() && source.is_none() {
        return;
    }
    if (destination.is_none() && !destination_preview_complete)
        || (source.is_none() && !source_preview_complete)
    {
        *destination = None;
        return;
    }
    let segments = destination.get_or_insert_with(|| {
        if destination_preview.is_empty() {
            Vec::new()
        } else {
            vec![ExactTraceSegment::Memory(destination_preview.to_vec())]
        }
    });
    match source {
        Some(source) => {
            for segment in source {
                push_exact_segment(segments, segment);
            }
        }
        None if !source_preview.is_empty() => {
            push_exact_segment(segments, ExactTraceSegment::Memory(source_preview.to_vec()))
        }
        None => {}
    }
}

fn take_exact_or_complete_preview(
    exact: &mut Option<Vec<ExactTraceSegment>>,
    preview: &[u8],
    preview_complete: bool,
) -> Option<Vec<ExactTraceSegment>> {
    exact.take().or_else(|| {
        preview_complete.then(|| {
            if preview.is_empty() {
                Vec::new()
            } else {
                vec![ExactTraceSegment::Memory(preview.to_vec())]
            }
        })
    })
}

fn push_exact_segment(segments: &mut Vec<ExactTraceSegment>, segment: ExactTraceSegment) {
    match segment {
        ExactTraceSegment::Memory(bytes) if bytes.is_empty() => {}
        ExactTraceSegment::Memory(bytes) => {
            if let Some(ExactTraceSegment::Memory(previous)) = segments.last_mut() {
                previous.extend(bytes);
            } else {
                segments.push(ExactTraceSegment::Memory(bytes));
            }
        }
        ExactTraceSegment::File(file) => segments.push(ExactTraceSegment::File(file)),
    }
}

#[derive(Debug, Default)]
pub struct Executor {
    /// When true (top-level interactive/`-c` execution), each command's output
    /// is flushed to the real stdout/stderr in order in raw mode, so builtin
    /// and external output interleave correctly. Nested executors leave this
    /// off and accumulate into their outcome.
    flush_stdout: bool,
}

#[derive(Debug, Clone)]
struct ExpandedInvocation {
    assignments: Vec<Assignment>,
    argv: Vec<String>,
    redirections: Vec<ExpandedRedirection>,
}

#[derive(Debug, Clone)]
struct ExpandedRedirection {
    fd: u8,
    mode: RedirectionMode,
    target: ExpandedRedirectionTarget,
}

#[derive(Debug, Clone)]
struct ResolvedExternalInvocation {
    invocation: ExpandedInvocation,
    path: PathBuf,
}

enum ResolvedStreamingStage {
    External(ResolvedExternalInvocation),
    Shell(CommandInvocation),
}

enum RunningStreamingStage {
    External(Child),
    Shell(RunningShellStage),
}

struct RunningShellStage {
    thread: std::thread::JoinHandle<Result<CommandOutcome, ShellError>>,
    interrupt: Arc<AtomicBool>,
}

enum ExternalStageStdin {
    Inherit,
    Null,
    Pipe(io::PipeReader),
}

impl ExternalStageStdin {
    fn into_stdio(self) -> Stdio {
        match self {
            Self::Inherit => Stdio::inherit(),
            Self::Null => Stdio::null(),
            Self::Pipe(reader) => Stdio::from(reader),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamingPipeKind {
    Stdout,
    Stderr,
}

#[derive(Debug)]
enum StreamingOutputTarget {
    File(File),
    Inherit(InheritedOutput),
    Null,
    Pipe {
        kind: StreamingPipeKind,
        writer: io::PipeWriter,
    },
}

#[derive(Debug, Clone, Copy)]
enum InheritedOutput {
    Stdout,
    Stderr,
}

impl StreamingOutputTarget {
    fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::File(file) => Ok(Self::File(file.try_clone()?)),
            Self::Inherit(output) => Ok(Self::Inherit(*output)),
            Self::Null => Ok(Self::Null),
            Self::Pipe { kind, writer } => Ok(Self::Pipe {
                kind: *kind,
                writer: writer.try_clone()?,
            }),
        }
    }

    fn pipe_kind(&self) -> Option<StreamingPipeKind> {
        match self {
            Self::Null | Self::File(_) | Self::Inherit(_) => None,
            Self::Pipe { kind, .. } => Some(*kind),
        }
    }

    fn into_stdio(self) -> io::Result<Stdio> {
        match self {
            Self::Null => Ok(Stdio::null()),
            Self::File(file) => Ok(Stdio::from(file)),
            Self::Inherit(InheritedOutput::Stdout) => {
                Ok(Stdio::from(io::stdout().as_fd().try_clone_to_owned()?))
            }
            Self::Inherit(InheritedOutput::Stderr) => {
                Ok(Stdio::from(io::stderr().as_fd().try_clone_to_owned()?))
            }
            Self::Pipe { writer, .. } => Ok(Stdio::from(writer)),
        }
    }
}

#[derive(Debug)]
struct StreamingOutputReaders {
    stdout: Option<io::PipeReader>,
    stderr: Option<io::PipeReader>,
}

#[derive(Debug, Clone)]
struct PositionalSnapshot {
    args: Vec<String>,
    at_was_set: bool,
}

#[derive(Debug, Clone)]
struct IfBlock {
    clauses: Vec<IfClause>,
    else_body: Option<String>,
}

#[derive(Debug, Clone)]
struct IfClause {
    condition: String,
    body: String,
}

#[derive(Debug, Clone)]
struct WhileBlock {
    kind: ConditionLoopKind,
    condition: String,
    body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionLoopKind {
    While,
    Until,
}

impl ConditionLoopKind {
    fn keyword(self) -> &'static str {
        match self {
            Self::While => "while",
            Self::Until => "until",
        }
    }

    fn should_run_body(self, condition_status: i32) -> bool {
        match self {
            Self::While => condition_status == 0,
            Self::Until => condition_status != 0,
        }
    }
}

#[derive(Debug, Clone)]
struct ForBlock {
    variable: String,
    items: ForItems,
    body: String,
    /// For a C-style `for ((init; cond; step))`: the three arithmetic clauses.
    arithmetic: Option<(String, String, String)>,
}

#[derive(Debug, Clone)]
struct SelectBlock {
    variable: String,
    items: ForItems,
    body: String,
}

#[derive(Debug, Clone)]
enum ForItems {
    Words(Vec<Vec<WordSegment>>),
    Positionals,
}

#[derive(Debug, Clone)]
struct CaseBlock {
    value: Vec<WordSegment>,
    arms: Vec<CaseArm>,
}

#[derive(Debug, Clone)]
struct CaseArm {
    patterns: Vec<Vec<WordSegment>>,
    body: String,
    terminator: CaseTerminator,
}

#[derive(Debug, Clone)]
struct IfWord {
    text: String,
    source: String,
    quote: QuoteKind,
}

#[derive(Debug, Clone)]
enum ExpandedRedirectionTarget {
    Path(String),
    Fd(u8),
    Close,
    /// Literal bytes to feed to stdin (heredocs and herestrings).
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LookupMode {
    Normal,
    BypassAliases,
    DefaultPath,
    ExternalOnly,
    BuiltinOnly,
}

#[derive(Debug, Clone, Copy)]
struct InvocationContext {
    lookup_mode: LookupMode,
    alias_depth: usize,
    allow_process_replacement: bool,
}

const MAX_ALIAS_EXPANSIONS: usize = 16;
const DEFAULT_COMMAND_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

impl Executor {
    pub fn new() -> Self {
        Self {
            flush_stdout: false,
        }
    }

    /// Enable immediate stdout/stderr flushing for top-level execution.
    pub fn with_stdout_flush(mut self, enabled: bool) -> Self {
        self.flush_stdout = enabled;
        self
    }

    pub fn run_graph(
        &mut self,
        graph: &CommandGraph,
        state: &mut ShellState,
        options: &ExecutionOptions,
    ) -> Result<CommandOutcome, ShellError> {
        // Bound total execution nesting here — the single chokepoint every
        // nested path re-enters (functions, `$( )`, `<( )`, subshells, brace
        // groups, `eval`, `source`, loop/compound bodies). Guards against
        // runaway recursion (`f() { f; }`, deeply nested `$( … )`) that would
        // otherwise overflow the stack and abort the whole shell.
        state.enter_exec()?;
        let inherited_proc_sub_temps = state.take_proc_sub_temps();
        for path in &inherited_proc_sub_temps {
            state.register_proc_sub_temp(path.clone());
        }
        let raw_passthrough_options;
        let top_level = state.is_top_level_execution();
        let raw_passthrough = top_level
            && (rich_mode_requires_raw_passthrough(options.output_mode)
                || (options.output_mode.should_capture() && graph_contains_async_list(graph)));
        let options = if raw_passthrough {
            raw_passthrough_options = ExecutionOptions {
                output_mode: OutputMode::Raw,
                allow_process_replacement: options.allow_process_replacement,
            };
            &raw_passthrough_options
        } else {
            options
        };
        let result = if options.output_mode == OutputMode::LosslessRef
            && state.is_top_level_execution()
        {
            state
                .prepare_required_trace_storage()
                .map_err(|error| {
                    ShellError::execution(format!("lossless trace storage is unavailable: {error}"))
                })
                .and_then(|()| self.run_graph_inner(graph, state, options))
        } else {
            self.run_graph_inner(graph, state, options)
        };
        state.leave_exec();
        cleanup_new_proc_sub_temps(state, inherited_proc_sub_temps);
        let outcome = result?;
        state.set_last_status(outcome.exit_code);
        Ok(outcome)
    }

    fn run_graph_inner(
        &mut self,
        graph: &CommandGraph,
        state: &mut ShellState,
        options: &ExecutionOptions,
    ) -> Result<CommandOutcome, ShellError> {
        if graph.is_empty() {
            return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
        }

        if graph.list.items.len() > 1
            || graph.list.items.first().is_some_and(|item| item.background)
        {
            return run_command_list(graph, state, options, self.flush_stdout);
        }

        let pipeline = graph
            .list
            .items
            .first()
            .map(|item| &item.pipeline)
            .unwrap_or(&graph.pipeline);

        let mut outcome = run_pipeline_item(graph, pipeline, state, options, self.flush_stdout)?;
        // Stream this command's stdout to a downstream pipe if one is active (a
        // pipeline stage). Without this, a single-command loop body (`while true;
        // do echo x; done | head`) would accumulate its output and flush only at
        // stage end — never streaming — so an early-exiting consumer couldn't stop
        // an infinite producer. Multi-command lists already emit per command in
        // run_command_list. No-op when not streaming. (P0-8)
        emit_streaming_stdout(state, &mut outcome)?;
        if options.output_mode.should_capture()
            && state.is_top_level_execution()
            && outcome.observation.is_none()
        {
            record_trace_and_attach_observation(graph, state, options, &mut outcome)?;
        }
        Ok(outcome)
    }
}

fn rich_mode_requires_raw_passthrough(mode: OutputMode) -> bool {
    use std::io::IsTerminal;

    rich_mode_requires_raw_passthrough_with_terminal(mode, std::io::stdout().is_terminal())
}

fn rich_mode_requires_raw_passthrough_with_terminal(
    mode: OutputMode,
    stdout_is_terminal: bool,
) -> bool {
    mode == OutputMode::Rich && !stdout_is_terminal
}

fn graph_contains_async_list(graph: &CommandGraph) -> bool {
    graph.list.items.iter().any(|item| {
        item.background
            || item.pipeline.commands.iter().any(|invocation| {
                invocation.argv.iter().enumerate().any(|(index, _)| {
                    exact_unquoted_operator_word(invocation, index, "&")
                            // The lexer represents the case terminators `;&` and
                            // `;;&` as a segmentless `;` followed by `&`. They are
                            // fallthrough syntax, not asynchronous lists.
                            && !index.checked_sub(1).is_some_and(|previous| {
                                exact_unquoted_operator_word(invocation, previous, ";")
                            })
                })
            })
    })
}

fn exact_unquoted_operator_word(
    invocation: &CommandInvocation,
    index: usize,
    expected: &str,
) -> bool {
    invocation
        .argv
        .get(index)
        .is_some_and(|word| word == expected)
        && invocation.argv_quote.get(index) == Some(&QuoteKind::None)
        && invocation
            .argv_segments
            .get(index)
            .is_some_and(Vec::is_empty)
}

/// Remove process-substitution files created by this graph while leaving paths
/// owned by an enclosing graph registered for its eventual cleanup.
fn cleanup_new_proc_sub_temps(state: &mut ShellState, inherited: Vec<PathBuf>) {
    let pending = state.take_proc_sub_temps();
    for path in pending.into_iter().skip(inherited.len()) {
        let _ = std::fs::remove_file(path);
    }
    for path in inherited {
        state.register_proc_sub_temp(path);
    }
}

/// Fire any pending signal traps (POSIX 2.11) and merge their output into the
/// just-completed command's outcome. Run at command boundaries.
fn fire_signal_traps(
    state: &mut ShellState,
    options: &ExecutionOptions,
    outcome: &mut CommandOutcome,
) -> Result<(), ShellError> {
    let mut fired = false;
    for signal in state.take_pending_signal_traps() {
        if let Some(action) = state.trap_action(&signal) {
            fired = true;
            let mut trap_outcome = run_command_source(&action, state, options)?;
            outcome.append_streams(&mut trap_outcome)?;
        }
    }
    // A trapped signal does not take its default action: clear the interrupt so
    // execution continues after the handler (POSIX 2.11).
    if fired {
        state.clear_interrupt();
    }
    Ok(())
}

fn run_command_list(
    graph: &CommandGraph,
    state: &mut ShellState,
    options: &ExecutionOptions,
    flush_stdout: bool,
) -> Result<CommandOutcome, ShellError> {
    let mut final_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());
    let mut last_status = 0;

    for (index, item) in graph.list.items.iter().enumerate() {
        let should_run = match item.operator {
            ListOperator::Always => true,
            ListOperator::And => last_status == 0,
            ListOperator::Or => last_status != 0,
        };
        if !should_run {
            continue;
        }

        // Track the source line of this command for `$LINENO`.
        if let Some(start) = item
            .pipeline
            .commands
            .first()
            .and_then(|c| c.span)
            .map(|s| s.start)
        {
            let line = 1 + graph.source[..start.min(graph.source.len())]
                .bytes()
                .filter(|&b| b == b'\n')
                .count();
            state.set_current_line(line as u32);
        }

        if item.background {
            let mut outcome = run_background_item(graph, item, state)?;
            apply_builtin_redirections(&mut outcome, &[], state)?;
            last_status = outcome.exit_code;
            state.set_last_status(last_status);
            final_outcome.exit_code = outcome.exit_code;
            if flush_stdout && !options.output_mode.should_capture() {
                pipe_ok(io::stdout().write_all(&outcome.stdout))?;
                pipe_ok(io::stdout().flush())?;
                pipe_ok(io::stderr().write_all(&outcome.stderr))?;
                outcome.stdout.clear();
                outcome.stderr.clear();
            }
            final_outcome.append_streams(&mut outcome)?;
            continue;
        }

        let mut outcome = run_pipeline_item(graph, &item.pipeline, state, options, flush_stdout)?;
        last_status = outcome.exit_code;
        state.set_last_status(last_status);
        // $PIPESTATUS for a single command is a one-element array (multi-stage
        // pipelines record it themselves in run_pipeline).
        if item.pipeline.commands.len() == 1 {
            record_pipestatus(state, &[outcome.exit_code]);
        }
        fire_signal_traps(state, options, &mut outcome)?;
        emit_streaming_stdout(state, &mut outcome)?;
        final_outcome.exit_code = outcome.exit_code;
        // In raw (non-capturing) mode, flush each command's output immediately so
        // builtin output interleaves correctly with external commands instead of
        // being buffered until the end of the list.
        if flush_stdout && !options.output_mode.should_capture() && state.streaming_stdout_is_none()
        {
            pipe_ok(io::stdout().write_all(&outcome.stdout))?;
            pipe_ok(io::stdout().flush())?;
            pipe_ok(io::stderr().write_all(&outcome.stderr))?;
            outcome.stdout.clear();
            outcome.stderr.clear();
        }
        final_outcome.append_streams(&mut outcome)?;

        if state.should_exit()
            || state.loop_control_requested()
            || state.return_requested()
            || state.interrupted()
            || state.stream_pipe_closed()
        {
            break;
        }

        let next_operator = graph
            .list
            .items
            .get(index + 1)
            .map(|next| next.operator)
            .unwrap_or(ListOperator::Always);
        let failure_is_boolean_operand =
            matches!(next_operator, ListOperator::And | ListOperator::Or);

        // ERR trap (POSIX/bash): fires when a command fails, except as a boolean
        // operand of `&&`/`||`. Fired before any errexit exit.
        if outcome.exit_code != 0 && !failure_is_boolean_operand {
            if let Some(action) = state.trap_action("ERR") {
                state.set_trap("ERR", None); // prevent recursion in the handler
                if let Ok(mut handler) = run_command_source(&action, state, options) {
                    if flush_stdout
                        && !options.output_mode.should_capture()
                        && state.streaming_stdout_is_none()
                    {
                        pipe_ok(io::stdout().write_all(&handler.stdout))?;
                        pipe_ok(io::stdout().flush())?;
                        pipe_ok(io::stderr().write_all(&handler.stderr))?;
                    } else {
                        final_outcome.append_streams(&mut handler)?;
                    }
                }
                state.set_trap("ERR", Some(action));
                state.set_last_status(last_status);
            }
        }

        if state.errexit() && outcome.exit_code != 0 && !failure_is_boolean_operand {
            break;
        }
    }

    if options.output_mode.should_capture() && state.is_top_level_execution() {
        record_trace_and_attach_observation(graph, state, options, &mut final_outcome)?;
    }

    Ok(final_outcome)
}

fn record_trace_and_attach_observation(
    graph: &CommandGraph,
    state: &ShellState,
    options: &ExecutionOptions,
    outcome: &mut CommandOutcome,
) -> Result<(), ShellError> {
    let (exact_stdout, exact_stderr) = outcome.take_exact_streams();
    let raw = match state.record_trace_captured(
        &graph.id,
        &graph.source,
        outcome.exit_code,
        CapturedTraceStreams {
            stdout_preview: &outcome.stdout,
            stderr_preview: &outcome.stderr,
            stdout_exact: exact_stdout,
            stderr_exact: exact_stderr,
            stdout_preview_complete: outcome.stdout_preview_complete,
            stderr_preview_complete: outcome.stderr_preview_complete,
        },
    ) {
        Ok(raw) => raw,
        Err(_) => RawStreamRef::unavailable(),
    };
    let argv = graph_primary_argv(graph);
    outcome.observation = if options.output_mode == OutputMode::Rich {
        rich_observation(
            state,
            &graph.id,
            &argv,
            &outcome.stdout,
            &outcome.stderr,
            &raw,
        )
    } else {
        Some(render_observation_with_raw_ref(
            &compaction_context(state, &argv),
            options.output_mode,
            &graph.id,
            &argv,
            outcome.exit_code,
            ObservationStreams {
                stdout: &outcome.stdout,
                stderr: &outcome.stderr,
                raw: &raw,
            },
        ))
    };
    Ok(())
}

/// Reconstruct the source text of one command-list item from the original line
/// using its command spans, falling back to rebuilding from the parsed words.
fn item_source(graph: &CommandGraph, item: &CommandListItem) -> String {
    let commands = &item.pipeline.commands;
    let start = commands.first().and_then(|c| c.span).map(|s| s.start);
    let end = commands.last().and_then(|c| c.span).map(|s| s.end);
    if let (Some(start), Some(end)) = (start, end) {
        if start <= end && end <= graph.source.len() {
            let slice = graph.source[start..end].trim();
            if !slice.is_empty() {
                return slice.to_string();
            }
        }
    }
    commands
        .iter()
        .map(|command| {
            let words = if_words(command);
            words_to_source(&words)
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Run a command-list item asynchronously: launch it as a child `agsh -c`
/// process that leads its own process group, register it as a job, and return
/// immediately with a `[id] pid` notice (matching shell behavior). Running a
/// child shell uniformly backgrounds builtins, compounds, and pipelines and
/// keeps the parent's state isolated (like a subshell).
fn run_background_item(
    graph: &CommandGraph,
    item: &CommandListItem,
    state: &ShellState,
) -> Result<CommandOutcome, ShellError> {
    let source = item_source(graph, item);
    let snapshot = state.encode_background_snapshot()?;
    if snapshot.len() > BACKGROUND_SNAPSHOT_MAX_BYTES {
        return Err(ShellError::execution(format!(
            "background shell state exceeds {BACKGROUND_SNAPSHOT_MAX_BYTES} bytes"
        )));
    }
    let (mut handoff, child_handoff) = UnixStream::pair()?;
    handoff.set_read_timeout(Some(BACKGROUND_SNAPSHOT_TIMEOUT))?;
    handoff.set_write_timeout(Some(BACKGROUND_SNAPSHOT_TIMEOUT))?;
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .arg("--output")
        .arg("raw")
        .arg("--background-state-stdin")
        .arg("-c")
        .arg(&source);
    command.current_dir(state.cwd());
    state.configure_child_env(&mut command);
    command.process_group(0);
    // Startup consumes the state from this full-duplex socket, acknowledges a
    // successful decode, then replaces fd 0 with /dev/null before execution.
    command.stdin(Stdio::from(OwnedFd::from(child_handoff)));
    let mut routing = inherited_capture_routing();
    if let Some(writer) = state.streaming_stdout_writer() {
        let stage_stdout = CaptureDestination::Pipe {
            kind: StreamingPipeKind::Stdout,
            writer: Arc::new(writer),
        };
        if matches!(routing.stdout, CaptureDestination::Stdout) {
            routing.stdout = stage_stdout.clone();
        }
        if matches!(routing.stderr, CaptureDestination::Stdout) {
            routing.stderr = stage_stdout;
        }
    }
    command.stdout(stdio_for_capture_destination(&routing.stdout)?);
    command.stderr(stdio_for_capture_destination(&routing.stderr)?);

    let mut child = command.spawn()?;
    drop(command);
    let handoff_result = (|| -> io::Result<()> {
        handoff.write_all(&snapshot)?;
        handoff.shutdown(Shutdown::Write)?;
        let mut ready = [0u8; 1];
        handoff.read_exact(&mut ready)?;
        if ready[0] != BACKGROUND_SNAPSHOT_READY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "background child returned an invalid state acknowledgement",
            ));
        }
        Ok(())
    })();
    if let Err(error) = handoff_result {
        if let Some(pgid) = rustix::process::Pid::from_raw(child.id() as i32) {
            let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL);
        }
        let _ = child.kill();
        let _ = child.wait();
        return Err(ShellError::execution(format!(
            "background state handoff failed: {error}"
        )));
    }
    drop(handoff);
    let pid = child.id();
    state.set_last_bg_pid(pid);
    let (id, _pgid) = state.register_job(child, source);
    let notice = if io::stderr().is_terminal() {
        format!("[{id}] {pid}\n").into_bytes()
    } else {
        Vec::new()
    };
    Ok(CommandOutcome::captured(0, Vec::new(), notice))
}

fn stdio_for_capture_destination(destination: &CaptureDestination) -> io::Result<Stdio> {
    match destination {
        CaptureDestination::Stdout => Ok(Stdio::from(io::stdout().as_fd().try_clone_to_owned()?)),
        CaptureDestination::Stderr => Ok(Stdio::from(io::stderr().as_fd().try_clone_to_owned()?)),
        CaptureDestination::File(file) => Ok(Stdio::from(file.try_clone()?)),
        CaptureDestination::Pipe { writer, .. } => Ok(Stdio::from(writer.try_clone()?)),
        CaptureDestination::Discard => Ok(Stdio::null()),
    }
}

/// Treat a closed downstream pipe (SIGPIPE) as success when emitting output, so a
/// shell whose stdout is consumed by `head`/`less`/`grep -q` exits silently like
/// bash instead of printing a spurious "Broken pipe".
fn pipe_ok(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

fn emit_streaming_stdout(
    state: &mut ShellState,
    outcome: &mut CommandOutcome,
) -> Result<(), ShellError> {
    if outcome.stdout.is_empty() {
        return Ok(());
    }

    if let Some(result) = state.write_shell_stdout(&outcome.stdout) {
        match result {
            Ok(()) => {}
            // Downstream pipe closed (`… | head`): flag it so an enclosing
            // loop/list stops producing (SIGPIPE-like), rather than erroring.
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                state.set_stream_pipe_closed();
            }
            Err(error) => return Err(error.into()),
        }
        outcome.stdout.clear();
    }

    Ok(())
}

/// Route diagnostics produced while expanding an invocation through the fd
/// table that existed before that invocation's own redirections. Live commands
/// must see the prefix before they can write inherited stdout/stderr; capturing
/// commands retain the already-routed bytes for ordered aggregation.
fn prepare_pre_invocation_prefix(
    stderr: Vec<u8>,
    state: &ShellState,
    capture_outputs: bool,
) -> Result<CommandOutcome, ShellError> {
    let mut prefix = CommandOutcome::captured(0, Vec::new(), stderr);
    apply_builtin_redirections(&mut prefix, &[], state)?;

    if !capture_outputs && stream_raw_to_parent() {
        if !prefix.stdout.is_empty() {
            pipe_ok(io::stdout().write_all(&prefix.stdout))?;
            pipe_ok(io::stdout().flush())?;
            prefix.stdout.clear();
        }
        if !prefix.stderr.is_empty() {
            pipe_ok(io::stderr().write_all(&prefix.stderr))?;
            pipe_ok(io::stderr().flush())?;
            prefix.stderr.clear();
        }
        prefix.output_order = Some(Vec::new());
    }

    Ok(prefix)
}

fn take_pre_invocation_substitution_stderr(
    state: &mut ShellState,
    capture_outputs: bool,
) -> Result<CommandOutcome, ShellError> {
    let stderr = state.take_pending_substitution_stderr();
    prepare_pre_invocation_prefix(stderr, state, capture_outputs)
}

/// Run one pipeline item, converting runtime I/O failures (failed `cd`,
/// failed redirection open, etc.) into a non-zero outcome so the command list
/// continues instead of aborting — matching POSIX shell behavior.
fn run_pipeline_item(
    graph: &CommandGraph,
    pipeline: &Pipeline,
    state: &mut ShellState,
    options: &ExecutionOptions,
    stream_raw_to_parent: bool,
) -> Result<CommandOutcome, ShellError> {
    // A nested graph (for example a function body) must not consume stderr that
    // was produced while expanding its caller's arguments. Hold inherited bytes
    // outside this scope and restore them for the caller after this item ends.
    let inherited_substitution_stderr = state.take_pending_substitution_stderr();
    let result = with_stream_raw_to_parent(stream_raw_to_parent, || {
        run_pipeline_item_inner(graph, pipeline, state, options, stream_raw_to_parent)
    });
    let generated_substitution_stderr = state.take_pending_substitution_stderr();

    let outcome = match result {
        Ok(outcome) => Ok((outcome, false)),
        // A write to a downstream pipe that closed early (`… | head`, `… | grep -q`)
        // is a normal SIGPIPE, not an error: bash exits the producer silently. Emit
        // nothing, and flag the closed pipe so an enclosing loop/list stops
        // producing (P0-8) instead of iterating against a dead consumer.
        Err(error) if error.kind == ShellErrorKind::BrokenPipe => {
            state.set_stream_pipe_closed();
            Ok((CommandOutcome::captured(0, Vec::new(), Vec::new()), false))
        }
        Err(error) if error.kind == ShellErrorKind::Io => Ok((
            CommandOutcome::captured(
                1,
                Vec::new(),
                format!("agsh: {}\n", error.message).into_bytes(),
            ),
            true,
        )),
        // A missing command in a pipeline stage yields exit 127 and the list
        // continues, matching POSIX shells.
        Err(error) if error.kind == ShellErrorKind::NotFound => Ok((
            CommandOutcome::captured(
                127,
                Vec::new(),
                format!("agsh: {}\n", error.message).into_bytes(),
            ),
            true,
        )),
        // A command refused by a `confine` allowlist: exit 126, list continues.
        Err(error) if error.kind == ShellErrorKind::Policy => Ok((
            CommandOutcome::captured(
                126,
                Vec::new(),
                format!("agsh: {}\n", error.message).into_bytes(),
            ),
            true,
        )),
        Err(error) => Err(error),
    };

    match outcome {
        Ok((mut outcome, route_synthesized_outcome)) => {
            state.append_pending_substitution_stderr(inherited_substitution_stderr);
            if route_synthesized_outcome {
                apply_builtin_redirections(&mut outcome, &[], state)?;
            }
            if !generated_substitution_stderr.is_empty() {
                let exit_code = outcome.exit_code;
                let mut prefix =
                    CommandOutcome::captured(0, Vec::new(), generated_substitution_stderr);
                apply_builtin_redirections(&mut prefix, &[], state)?;
                prefix.append_streams(&mut outcome)?;
                prefix.exit_code = exit_code;
                outcome = prefix;
            }
            Ok(outcome)
        }
        Err(error) => {
            state.append_pending_substitution_stderr(inherited_substitution_stderr);
            state.append_pending_substitution_stderr(generated_substitution_stderr);
            Err(error)
        }
    }
}

fn run_pipeline_item_inner(
    graph: &CommandGraph,
    pipeline: &Pipeline,
    state: &mut ShellState,
    options: &ExecutionOptions,
    stream_raw_to_parent: bool,
) -> Result<CommandOutcome, ShellError> {
    if pipeline.commands.is_empty() {
        return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
    }

    if pipeline.commands.len() == 1 {
        if let Some(if_block) = parse_if_invocation(&pipeline.commands[0])? {
            let mut outcome = run_if_invocation(
                &if_block,
                &pipeline.commands[0],
                state,
                options.output_mode,
                options.output_mode.should_capture(),
                options.allow_process_replacement,
            )?;
            apply_pipeline_negation(&mut outcome, pipeline.negated);
            return Ok(outcome);
        }

        if let Some(while_block) = parse_while_invocation(&pipeline.commands[0])? {
            let mut outcome = run_while_invocation(
                &while_block,
                &pipeline.commands[0],
                state,
                options.output_mode,
                options.output_mode.should_capture(),
                options.allow_process_replacement,
            )?;
            apply_pipeline_negation(&mut outcome, pipeline.negated);
            return Ok(outcome);
        }

        if let Some(for_block) = parse_for_invocation(&pipeline.commands[0])? {
            let mut outcome = run_for_invocation(
                &for_block,
                &pipeline.commands[0],
                state,
                options.output_mode,
                options.output_mode.should_capture(),
                options.allow_process_replacement,
            )?;
            apply_pipeline_negation(&mut outcome, pipeline.negated);
            return Ok(outcome);
        }

        if let Some(select_block) = parse_select_invocation(&pipeline.commands[0])? {
            let mut outcome = run_select_invocation(
                &select_block,
                &pipeline.commands[0],
                state,
                options.output_mode,
                options.output_mode.should_capture(),
                options.allow_process_replacement,
            )?;
            apply_pipeline_negation(&mut outcome, pipeline.negated);
            return Ok(outcome);
        }

        if let Some(case_block) = parse_case_invocation(&pipeline.commands[0])? {
            let mut outcome = run_case_invocation(
                &case_block,
                &pipeline.commands[0],
                state,
                options.output_mode,
                options.output_mode.should_capture(),
                options.allow_process_replacement,
            )?;
            apply_pipeline_negation(&mut outcome, pipeline.negated);
            return Ok(outcome);
        }

        if let Some(inner) = parse_subshell_invocation(&pipeline.commands[0])? {
            let mut outcome = run_subshell_invocation(
                &inner,
                &pipeline.commands[0],
                state,
                options.output_mode,
                None,
                options.output_mode.should_capture(),
                options.allow_process_replacement,
            )?;
            apply_pipeline_negation(&mut outcome, pipeline.negated);
            return Ok(outcome);
        }

        if let Some(inner) = parse_brace_group_invocation(&pipeline.commands[0])? {
            let mut outcome = run_brace_group_invocation(
                &inner,
                &pipeline.commands[0],
                state,
                options.output_mode,
                None,
                options.output_mode.should_capture(),
                options.allow_process_replacement,
            )?;
            apply_pipeline_negation(&mut outcome, pipeline.negated);
            return Ok(outcome);
        }

        if !pipeline.negated {
            if let Some((name, function)) = parse_function_definition(&pipeline.commands[0])? {
                state.set_function(name, function);
                return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
            }
        }

        let invocation = expand_invocation(&pipeline.commands[0], state)?;
        let mut expansion_prefix =
            take_pre_invocation_substitution_stderr(state, options.output_mode.should_capture())?;
        if invocation.argv.is_empty() {
            let stderr = apply_shell_assignments(&invocation.assignments, state);
            // A redirection with no command still opens its input/output target
            // (`< missing` fails; `> file` creates/truncates it).
            let _redirected_stdin =
                redirected_stdin_from_expanded_redirections(&invocation.redirections)?;
            let mut outcome = CommandOutcome::captured(
                state.last_command_substitution_status(),
                Vec::new(),
                stderr,
            );
            apply_builtin_redirections(&mut outcome, &invocation.redirections, state)?;
            expansion_prefix.append_streams(&mut outcome)?;
            expansion_prefix.exit_code = outcome.exit_code;
            return Ok(expansion_prefix);
        }

        let rich_stdout = rich_stdout_allowed_for_invocation(&invocation, state, options);
        let mut outcome = with_rich_stdout(state, rich_stdout, |state| {
            run_invocation(
                &invocation,
                state,
                options.output_mode,
                None,
                options.output_mode.should_capture(),
                LookupMode::Normal,
                options.allow_process_replacement,
            )
        })?;
        apply_pipeline_negation(&mut outcome, pipeline.negated);
        let exit_code = outcome.exit_code;
        expansion_prefix.append_streams(&mut outcome)?;
        expansion_prefix.exit_code = exit_code;
        return Ok(expansion_prefix);
    }

    run_pipeline(graph, pipeline, state, options, stream_raw_to_parent)
}

fn apply_shell_assignments(assignments: &[Assignment], state: &mut ShellState) -> Vec<u8> {
    let mut stderr = Vec::new();
    for assignment in assignments {
        if let Some(message) = apply_assignment(state, assignment) {
            stderr.extend_from_slice(message.as_bytes());
        }
    }
    stderr
}

/// Apply one assignment, handling array literals (`a=(x y z)`), element
/// assignment (`a[i]=v`), append (`+=`), and plain scalars.
fn apply_assignment(state: &mut ShellState, assignment: &Assignment) -> Option<String> {
    let mut name = assignment.name.as_str();
    let append = name.ends_with('+');
    if append {
        name = &name[..name.len() - 1];
    }

    // Readonly enforcement (POSIX): refuse to reassign; report and fail.
    let base_name = name.split('[').next().unwrap_or(name);
    if state.is_readonly(base_name) {
        state.set_command_substitution_status(1);
        return Some(format!("agsh: {base_name}: readonly variable\n"));
    }

    // Subscript element: `a[index]=value`.
    if let Some(open) = name.find('[') {
        if let Some(close) = name.rfind(']') {
            let base = &name[..open];
            let sub = &name[open + 1..close];
            if state.is_assoc(base) {
                // Associative key: literal string (negative arith handled below
                // only for indexed arrays).
                state.set_assoc_element(base, sub.to_string(), assignment.value.clone(), append);
                return None;
            }
            let raw = eval_arithmetic(sub, state).unwrap_or(0);
            let len = state.array(base).map(<[String]>::len).unwrap_or(0) as i64;
            let index = if raw < 0 { (len + raw).max(0) } else { raw } as usize;
            state.set_array_element(base, index, assignment.value.clone(), append);
            return None;
        }
    }

    // Array literal: an unquoted `( ... )` value.
    let raw = assignment
        .value_segments
        .first()
        .filter(|s| s.quote == agsh_core::QuoteKind::None)
        .map(|s| s.text.as_str())
        .unwrap_or("");
    if raw.starts_with('(') && raw.ends_with(')') && assignment.value.starts_with('(') {
        let inner = &raw[1..raw.len() - 1];
        let elements = expand_array_elements(inner, state);
        if state.is_assoc(name) {
            state.set_assoc(name, assoc_pairs_from_elements(&elements), append);
        } else {
            state.set_array(name, elements, append);
        }
        return None;
    }

    // Scalar (with optional append).
    let value = if state.is_integer(name) {
        let expression = if append {
            format!(
                "{} + ({})",
                state.lookup(name).unwrap_or("0"),
                assignment.value
            )
        } else {
            assignment.value.clone()
        };
        match eval_arithmetic(&expression, state) {
            Ok(value) => value.to_string(),
            Err(error) => {
                state.set_command_substitution_status(1);
                return Some(format!("agsh: {name}: {error}\n"));
            }
        }
    } else if append {
        format!(
            "{}{}",
            state.lookup(name).unwrap_or_default(),
            assignment.value
        )
    } else {
        assignment.value.clone()
    };
    if state.allexport() {
        state.try_export_var(name, value);
    } else {
        state.try_set_var(name, value);
    }
    None
}

/// Expand the inner text of an array literal into elements, applying full
/// per-word expansion (parameter/command substitution, word-splitting, globbing).
/// Expand an array subscript expression (`a[i]`, `a[@]`, `#a[@]`, `!a[@]`, …) to
/// a string, or None when `expr` is not an array subscript of a known array.
/// `${a[@]}`/`${a[*]}` join elements with a space (quoted-element-with-spaces
/// preservation is a known limitation of the current return-string model).
fn expand_array_subscript(expr: &str, state: &mut ShellState) -> Option<String> {
    let (mode, rest) = match expr.chars().next() {
        Some('#') => ('#', &expr[1..]),
        Some('!') => ('!', &expr[1..]),
        _ => (' ', expr),
    };
    let open = rest.find('[')?;
    let close = rest.rfind(']')?;
    if close + 1 != rest.len() {
        return None;
    }
    let base = &rest[..open];
    let sub = &rest[open + 1..close];
    if !is_identifier(base) {
        return None;
    }

    // Associative arrays: keys are strings, not arithmetic.
    if state.is_assoc(base) {
        let result = match mode {
            '#' => {
                if sub == "@" || sub == "*" {
                    state.assoc_keys(base)?.len().to_string()
                } else {
                    state
                        .assoc_get(base, sub)
                        .map(|v| v.chars().count())
                        .unwrap_or(0)
                        .to_string()
                }
            }
            '!' => {
                if sub == "@" || sub == "*" {
                    state.assoc_keys(base)?.join(" ")
                } else {
                    return None;
                }
            }
            _ => {
                if sub == "@" || sub == "*" {
                    state.assoc_values(base)?.join(" ")
                } else {
                    state.assoc_get(base, sub).unwrap_or_default().to_string()
                }
            }
        };
        return Some(result);
    }

    let array = state.array(base)?.to_vec();
    let resolve = |raw: i64| -> Option<usize> {
        // Negative indices count from the end (bash 4.3): a[-1] is the last.
        let idx = if raw < 0 {
            array.len() as i64 + raw
        } else {
            raw
        };
        (idx >= 0).then_some(idx as usize)
    };
    let result = match mode {
        '#' => {
            if sub == "@" || sub == "*" {
                array.len().to_string()
            } else {
                let index = resolve(eval_arithmetic(sub, state).ok()?);
                index
                    .and_then(|i| array.get(i))
                    .map(|s| s.chars().count())
                    .unwrap_or(0)
                    .to_string()
            }
        }
        '!' => {
            if sub == "@" || sub == "*" {
                (0..array.len())
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                return None;
            }
        }
        _ => {
            if sub == "@" || sub == "*" {
                array.join(" ")
            } else {
                resolve(eval_arithmetic(sub, state).ok()?)
                    .and_then(|i| array.get(i))
                    .cloned()
                    .unwrap_or_default()
            }
        }
    };
    Some(result)
}

/// Build associative key/value pairs from expanded literal elements: `[k]=v`
/// form, or alternating `k v k2 v2`.
fn assoc_pairs_from_elements(elements: &[String]) -> Vec<(String, String)> {
    if elements.iter().any(|e| e.starts_with('[')) {
        elements
            .iter()
            .filter_map(|e| {
                e.strip_prefix('[')
                    .and_then(|r| r.split_once("]="))
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect()
    } else {
        let mut pairs = Vec::new();
        let mut it = elements.iter();
        while let Some(k) = it.next() {
            pairs.push((k.clone(), it.next().cloned().unwrap_or_default()));
        }
        pairs
    }
}

fn expand_array_elements(inner: &str, state: &mut ShellState) -> Vec<String> {
    let Ok(tokens) = agsh_core::lexer::lex(inner) else {
        return Vec::new();
    };
    let mut elements = Vec::new();
    for token in tokens {
        let segments = if token.segments.is_empty() {
            vec![WordSegment::new(token.text.clone(), token.quote)]
        } else {
            token.segments.clone()
        };
        if let Ok(fields) = expand_word_segments_to_argv_fields(&segments, state) {
            elements.extend(fields);
        }
    }
    elements
}

fn run_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    lookup_mode: LookupMode,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    run_invocation_inner(
        invocation,
        state,
        output_mode,
        stdin_data,
        capture_outputs,
        InvocationContext {
            lookup_mode,
            alias_depth: 0,
            allow_process_replacement,
        },
    )
}

fn run_invocation_inner(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    context: InvocationContext,
) -> Result<CommandOutcome, ShellError> {
    let Some(name) = invocation.argv.first() else {
        return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
    };

    match name.as_str() {
        "command" if context.lookup_mode == LookupMode::Normal => {
            let command_options = parse_command_options(invocation)?;
            if command_options.describe || command_options.unsupported_option.is_some() {
                let mut outcome = run_builtin(&to_command_invocation(invocation), state)?;
                apply_builtin_redirections(&mut outcome, &invocation.redirections, state)?;
                return Ok(outcome);
            }
            let wrapped = strip_command_wrapper(invocation, command_options.command_index)?;
            run_invocation_inner(
                &wrapped,
                state,
                output_mode,
                stdin_data,
                capture_outputs,
                InvocationContext {
                    lookup_mode: if command_options.default_path {
                        LookupMode::DefaultPath
                    } else {
                        LookupMode::BypassAliases
                    },
                    ..context
                },
            )
        }
        "external" if context.lookup_mode == LookupMode::Normal => {
            let wrapped = strip_wrapper(invocation, "external")?;
            run_invocation_inner(
                &wrapped,
                state,
                output_mode,
                stdin_data,
                capture_outputs,
                InvocationContext {
                    lookup_mode: LookupMode::ExternalOnly,
                    ..context
                },
            )
        }
        "builtin" if context.lookup_mode == LookupMode::Normal => {
            let wrapped = strip_wrapper(invocation, "builtin")?;
            run_invocation_inner(
                &wrapped,
                state,
                output_mode,
                stdin_data,
                capture_outputs,
                InvocationContext {
                    lookup_mode: LookupMode::BuiltinOnly,
                    ..context
                },
            )
        }
        "agpty" | "pty"
            if context.lookup_mode == LookupMode::Normal && invocation.argv.len() > 1 =>
        {
            let wrapped = strip_wrapper(invocation, name.as_str())?;
            run_under_pty(&wrapped, state)
        }
        _ if context.lookup_mode == LookupMode::Normal => {
            if let Some(function) = state.function(name).cloned() {
                let mut pre_invocation = if state.xtrace() {
                    prepare_pre_invocation_prefix(
                        render_xtrace(invocation),
                        state,
                        capture_outputs,
                    )?
                } else {
                    CommandOutcome::captured(0, Vec::new(), Vec::new())
                };
                let mut outcome = run_function_invocation(
                    &function,
                    invocation,
                    state,
                    output_mode,
                    stdin_data,
                    capture_outputs,
                    context.allow_process_replacement,
                )?;
                let exit_code = outcome.exit_code;
                pre_invocation.append_streams(&mut outcome)?;
                pre_invocation.exit_code = exit_code;
                return Ok(pre_invocation);
            }

            if let Some(expanded) = expand_alias_invocation(invocation, state)? {
                if context.alias_depth >= MAX_ALIAS_EXPANSIONS {
                    return Err(ShellError::execution(format!(
                        "{name}: alias expansion limit exceeded"
                    )));
                }
                return run_invocation_inner(
                    &expanded,
                    state,
                    output_mode,
                    stdin_data,
                    capture_outputs,
                    InvocationContext {
                        alias_depth: context.alias_depth + 1,
                        ..context
                    },
                );
            }
            if let Some(expanded) = expand_abbreviation_invocation(invocation, state)? {
                if context.alias_depth >= MAX_ALIAS_EXPANSIONS {
                    return Err(ShellError::execution(format!(
                        "{name}: abbreviation expansion limit exceeded"
                    )));
                }
                return run_invocation_inner(
                    &expanded,
                    state,
                    output_mode,
                    stdin_data,
                    capture_outputs,
                    InvocationContext {
                        alias_depth: context.alias_depth + 1,
                        ..context
                    },
                );
            }
            run_resolved_invocation(
                invocation,
                state,
                output_mode,
                stdin_data,
                capture_outputs,
                context.lookup_mode,
                context.allow_process_replacement,
            )
        }
        _ => run_resolved_invocation(
            invocation,
            state,
            output_mode,
            stdin_data,
            capture_outputs,
            context.lookup_mode,
            context.allow_process_replacement,
        ),
    }
}

fn parse_function_definition(
    invocation: &CommandInvocation,
) -> Result<Option<(String, ShellFunction)>, ShellError> {
    let argv = &invocation.argv;
    if argv.is_empty() {
        return Ok(None);
    }

    let (name, body_start) = if let Some(name) = argv[0].strip_suffix("()") {
        if argv.get(1).is_some_and(|word| word == "{") {
            (name.to_string(), 2)
        } else {
            return Ok(None);
        }
    } else if is_identifier(&argv[0])
        && argv.get(1).is_some_and(|word| word == "()")
        && argv.get(2).is_some_and(|word| word == "{")
    {
        // `name () { ... }` with a space before the parentheses.
        (argv[0].clone(), 3)
    } else if argv.first().is_some_and(|word| word == "function")
        && argv.get(2).is_some_and(|word| word == "{")
    {
        (argv[1].clone(), 3)
    } else {
        return Ok(None);
    };

    if !is_identifier(&name) {
        return Err(ShellError::parse(format!("invalid function name: {name}")));
    }
    if argv.last().is_none_or(|word| word != "}") || body_start >= argv.len() - 1 {
        return Err(ShellError::parse(format!(
            "{name}: malformed function definition"
        )));
    }
    if !invocation.assignments.is_empty() || !invocation.redirections.is_empty() {
        return Err(ShellError::unsupported(format!(
            "{name}: assignments or redirections on function definitions are not supported"
        )));
    }

    let body = invocation.argv_segments[body_start..argv.len() - 1]
        .iter()
        .zip(&argv[body_start..argv.len() - 1])
        .map(|(segments, fallback)| word_segments_to_source(segments, fallback))
        .collect::<Vec<_>>()
        .join(" ");

    Ok(Some((name, ShellFunction::new(body))))
}

fn parse_if_invocation(invocation: &CommandInvocation) -> Result<Option<IfBlock>, ShellError> {
    if !invocation
        .argv
        .first()
        .zip(invocation.argv_quote.first())
        .is_some_and(|(word, quote)| word == "if" && *quote == QuoteKind::None)
    {
        return Ok(None);
    }
    if !invocation.assignments.is_empty() {
        return Err(ShellError::parse(
            "if: assignments before if are not supported",
        ));
    }

    let words = if_words(invocation);
    let mut clauses = Vec::new();
    let mut else_body = None;
    let mut index = 1;

    loop {
        let then_index = find_top_level_reserved(&words, index, &["then"])
            .ok_or_else(|| ShellError::parse("if: missing then"))?
            .0;
        let condition = words_to_source(&words[index..then_index]);
        if condition.trim().is_empty() {
            return Err(ShellError::parse("if: empty condition"));
        }
        index = then_index + 1;

        let (body_end, boundary) = find_top_level_reserved(&words, index, &["elif", "else", "fi"])
            .ok_or_else(|| ShellError::parse("if: missing fi"))?;
        let body = words_to_source(&words[index..body_end]);
        clauses.push(IfClause { condition, body });
        index = body_end + 1;

        match boundary.as_str() {
            "elif" => continue,
            "else" => {
                let (else_end, else_boundary) = find_top_level_reserved(&words, index, &["fi"])
                    .ok_or_else(|| ShellError::parse("if: missing fi"))?;
                if else_boundary != "fi" {
                    return Err(ShellError::parse("if: expected fi"));
                }
                else_body = Some(words_to_source(&words[index..else_end]));
                index = else_end + 1;
                break;
            }
            "fi" => break,
            _ => return Err(ShellError::parse("if: invalid branch delimiter")),
        }
    }

    if index != words.len() {
        return Err(ShellError::parse("if: unexpected tokens after fi"));
    }

    Ok(Some(IfBlock { clauses, else_body }))
}

fn parse_while_invocation(
    invocation: &CommandInvocation,
) -> Result<Option<WhileBlock>, ShellError> {
    let Some((keyword, quote)) = invocation.argv.first().zip(invocation.argv_quote.first()) else {
        return Ok(None);
    };
    let kind = match (keyword.as_str(), *quote) {
        ("while", QuoteKind::None) => ConditionLoopKind::While,
        ("until", QuoteKind::None) => ConditionLoopKind::Until,
        _ => return Ok(None),
    };
    let keyword = kind.keyword();
    if !invocation.assignments.is_empty() {
        return Err(ShellError::parse(format!(
            "{keyword}: assignments before {keyword} are not supported"
        )));
    }

    let words = if_words(invocation);
    let (do_index, _) = find_top_level_reserved(&words, 1, &["do"])
        .ok_or_else(|| ShellError::parse(format!("{keyword}: missing do")))?;
    let condition = words_to_source(&words[1..do_index]);
    if condition.trim().is_empty() {
        return Err(ShellError::parse(format!("{keyword}: empty condition")));
    }

    let body_start = do_index + 1;
    let (done_index, _) = find_top_level_reserved(&words, body_start, &["done"])
        .ok_or_else(|| ShellError::parse(format!("{keyword}: missing done")))?;
    let body = words_to_source(&words[body_start..done_index]);
    if done_index + 1 != words.len() {
        return Err(ShellError::parse(format!(
            "{keyword}: unexpected tokens after done"
        )));
    }

    Ok(Some(WhileBlock {
        kind,
        condition,
        body,
    }))
}

fn parse_for_invocation(invocation: &CommandInvocation) -> Result<Option<ForBlock>, ShellError> {
    if !invocation
        .argv
        .first()
        .zip(invocation.argv_quote.first())
        .is_some_and(|(word, quote)| word == "for" && *quote == QuoteKind::None)
    {
        return Ok(None);
    }
    if !invocation.assignments.is_empty() {
        return Err(ShellError::parse(
            "for: assignments before for are not supported",
        ));
    }

    // C-style: `for (( init; cond; step )); do ... done`.
    if let Some(header) = invocation.argv.get(1) {
        if header.starts_with("((") && header.ends_with("))") {
            let inner = &header[2..header.len() - 2];
            let mut parts = inner.splitn(3, ';');
            let init = parts.next().unwrap_or("").trim().to_string();
            let cond = parts.next().unwrap_or("").trim().to_string();
            let step = parts.next().unwrap_or("").trim().to_string();
            let words = if_words(invocation);
            let (do_index, _) = find_top_level_reserved(&words, 2, &["do"])
                .ok_or_else(|| ShellError::parse("for: missing do"))?;
            let body_start = do_index + 1;
            let (done_index, _) = find_top_level_reserved(&words, body_start, &["done"])
                .ok_or_else(|| ShellError::parse("for: missing done"))?;
            let body = words_to_source(&words[body_start..done_index]);
            return Ok(Some(ForBlock {
                variable: String::new(),
                items: ForItems::Positionals,
                body,
                arithmetic: Some((init, cond, step)),
            }));
        }
    }

    let Some(variable) = invocation.argv.get(1) else {
        return Err(ShellError::parse("for: missing variable name"));
    };
    if !invocation
        .argv_quote
        .get(1)
        .is_some_and(|quote| *quote == QuoteKind::None)
        || !is_identifier(variable)
    {
        return Err(ShellError::parse(format!(
            "for: invalid variable name: {variable}"
        )));
    }

    let words = if_words(invocation);
    let (do_index, _) = find_top_level_reserved(&words, 2, &["do"])
        .ok_or_else(|| ShellError::parse("for: missing do"))?;
    let mut header_start = 2;
    let mut header_end = do_index;
    if header_end > header_start
        && words[header_end - 1].quote == QuoteKind::None
        && words[header_end - 1].text == ";"
    {
        header_end -= 1;
    }

    let items = if header_start < header_end
        && words[header_start].quote == QuoteKind::None
        && words[header_start].text == "in"
    {
        header_start += 1;
        ForItems::Words(
            (header_start..header_end)
                .map(|index| {
                    invocation
                        .argv_segments
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| {
                            vec![WordSegment::new(
                                invocation.argv[index].clone(),
                                invocation.argv_quote[index],
                            )]
                        })
                })
                .collect(),
        )
    } else if header_start == header_end {
        ForItems::Positionals
    } else {
        return Err(ShellError::parse("for: expected in or do"));
    };

    let body_start = do_index + 1;
    let (done_index, _) = find_top_level_reserved(&words, body_start, &["done"])
        .ok_or_else(|| ShellError::parse("for: missing done"))?;
    let body = words_to_source(&words[body_start..done_index]);
    if done_index + 1 != words.len() {
        return Err(ShellError::parse("for: unexpected tokens after done"));
    }

    Ok(Some(ForBlock {
        variable: variable.clone(),
        items,
        body,
        arithmetic: None,
    }))
}

fn parse_select_invocation(
    invocation: &CommandInvocation,
) -> Result<Option<SelectBlock>, ShellError> {
    if !invocation
        .argv
        .first()
        .zip(invocation.argv_quote.first())
        .is_some_and(|(word, quote)| word == "select" && *quote == QuoteKind::None)
    {
        return Ok(None);
    }
    if !invocation.assignments.is_empty() {
        return Err(ShellError::parse(
            "select: assignments before select are not supported",
        ));
    }

    let Some(variable) = invocation.argv.get(1) else {
        return Err(ShellError::parse("select: missing variable name"));
    };
    if !invocation
        .argv_quote
        .get(1)
        .is_some_and(|quote| *quote == QuoteKind::None)
        || !is_identifier(variable)
    {
        return Err(ShellError::parse(format!(
            "select: invalid variable name: {variable}"
        )));
    }

    let words = if_words(invocation);
    let (do_index, _) = find_top_level_reserved(&words, 2, &["do"])
        .ok_or_else(|| ShellError::parse("select: missing do"))?;
    let mut header_start = 2;
    let mut header_end = do_index;
    if header_end > header_start
        && words[header_end - 1].quote == QuoteKind::None
        && words[header_end - 1].text == ";"
    {
        header_end -= 1;
    }

    let items = if header_start < header_end
        && words[header_start].quote == QuoteKind::None
        && words[header_start].text == "in"
    {
        header_start += 1;
        ForItems::Words(
            (header_start..header_end)
                .map(|index| {
                    invocation
                        .argv_segments
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| {
                            vec![WordSegment::new(
                                invocation.argv[index].clone(),
                                invocation.argv_quote[index],
                            )]
                        })
                })
                .collect(),
        )
    } else if header_start == header_end {
        ForItems::Positionals
    } else {
        return Err(ShellError::parse("select: expected in or do"));
    };

    let body_start = do_index + 1;
    let (done_index, _) = find_top_level_reserved(&words, body_start, &["done"])
        .ok_or_else(|| ShellError::parse("select: missing done"))?;
    let body = words_to_source(&words[body_start..done_index]);
    if done_index + 1 != words.len() {
        return Err(ShellError::parse("select: unexpected tokens after done"));
    }

    Ok(Some(SelectBlock {
        variable: variable.clone(),
        items,
        body,
    }))
}

fn parse_case_invocation(invocation: &CommandInvocation) -> Result<Option<CaseBlock>, ShellError> {
    if !invocation
        .argv
        .first()
        .zip(invocation.argv_quote.first())
        .is_some_and(|(word, quote)| word == "case" && *quote == QuoteKind::None)
    {
        return Ok(None);
    }
    if !invocation.assignments.is_empty() {
        return Err(ShellError::parse(
            "case: assignments before case are not supported",
        ));
    }

    let Some(value) = invocation.argv.get(1) else {
        return Err(ShellError::parse("case: missing word"));
    };
    let value_segments = invocation
        .argv_segments
        .get(1)
        .cloned()
        .unwrap_or_else(|| vec![WordSegment::new(value.clone(), invocation.argv_quote[1])]);
    if !invocation
        .argv
        .get(2)
        .zip(invocation.argv_quote.get(2))
        .is_some_and(|(word, quote)| word == "in" && *quote == QuoteKind::None)
    {
        return Err(ShellError::parse("case: missing in"));
    }

    let words = if_words(invocation);
    let mut arms = Vec::new();
    let mut index = 3;
    loop {
        while words
            .get(index)
            .is_some_and(|word| word.quote == QuoteKind::None && word.text == ";")
        {
            index += 1;
        }
        let Some(word) = words.get(index) else {
            return Err(ShellError::parse("case: missing esac"));
        };
        if word.quote == QuoteKind::None && word.text == "esac" {
            index += 1;
            break;
        }

        let patterns = parse_case_patterns(invocation, &words, &mut index)?;
        let body_start = index;
        let (body_end, terminator) = find_case_arm_terminator(&words, body_start)
            .ok_or_else(|| ShellError::parse("case: missing esac"))?;
        let body = words_to_source(&words[body_start..body_end]);
        arms.push(CaseArm {
            patterns,
            body,
            terminator,
        });

        match terminator {
            CaseTerminator::ArmSeparator
            | CaseTerminator::FallThrough
            | CaseTerminator::PatternContinue => index = body_end + terminator.token_count(),
            CaseTerminator::Esac => {
                index = body_end + terminator.token_count();
                break;
            }
        }
    }

    if index != words.len() {
        return Err(ShellError::parse("case: unexpected tokens after esac"));
    }

    Ok(Some(CaseBlock {
        value: value_segments,
        arms,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaseTerminator {
    ArmSeparator,
    FallThrough,
    PatternContinue,
    Esac,
}

impl CaseTerminator {
    fn token_count(self) -> usize {
        match self {
            Self::ArmSeparator | Self::FallThrough => 2,
            Self::PatternContinue => 3,
            Self::Esac => 1,
        }
    }
}

fn parse_case_patterns(
    invocation: &CommandInvocation,
    words: &[IfWord],
    index: &mut usize,
) -> Result<Vec<Vec<WordSegment>>, ShellError> {
    let mut patterns = Vec::new();
    let mut current = Vec::new();

    while let Some(word) = words.get(*index) {
        if word.quote == QuoteKind::None && word.text == "|" {
            push_case_pattern(&mut patterns, &mut current)?;
            *index += 1;
            continue;
        }

        let terminates = case_pattern_token_terminates(invocation, *index);
        let mut segments = case_pattern_segments(invocation, *index);
        if terminates {
            strip_case_pattern_terminator(&mut segments);
        }
        current.extend(segments);
        *index += 1;

        if terminates {
            push_case_pattern(&mut patterns, &mut current)?;
            return Ok(patterns);
        }
    }

    Err(ShellError::parse("case: missing ) after pattern"))
}

fn push_case_pattern(
    patterns: &mut Vec<Vec<WordSegment>>,
    current: &mut Vec<WordSegment>,
) -> Result<(), ShellError> {
    strip_case_pattern_leading_paren(current);
    if current.is_empty() {
        return Err(ShellError::parse("case: empty pattern"));
    }
    patterns.push(std::mem::take(current));
    Ok(())
}

fn case_pattern_token_terminates(invocation: &CommandInvocation, index: usize) -> bool {
    invocation
        .argv_segments
        .get(index)
        .and_then(|segments| segments.last())
        .is_some_and(|segment| segment.quote == QuoteKind::None && segment.text.ends_with(')'))
}

fn case_pattern_segments(invocation: &CommandInvocation, index: usize) -> Vec<WordSegment> {
    invocation
        .argv_segments
        .get(index)
        .cloned()
        .unwrap_or_else(|| {
            vec![WordSegment::new(
                invocation.argv[index].clone(),
                invocation.argv_quote[index],
            )]
        })
}

fn strip_case_pattern_terminator(segments: &mut Vec<WordSegment>) {
    if let Some(last) = segments.last_mut() {
        if last.quote == QuoteKind::None && last.text.ends_with(')') {
            last.text.pop();
        }
    }
    while segments
        .last()
        .is_some_and(|segment| segment.text.is_empty())
    {
        segments.pop();
    }
}

fn strip_case_pattern_leading_paren(segments: &mut Vec<WordSegment>) {
    if let Some(first) = segments.first_mut() {
        if first.quote == QuoteKind::None && first.text.starts_with('(') {
            first.text.remove(0);
        }
    }
    while segments
        .first()
        .is_some_and(|segment| segment.text.is_empty())
    {
        segments.remove(0);
    }
}

fn find_case_arm_terminator(words: &[IfWord], start: usize) -> Option<(usize, CaseTerminator)> {
    let mut nested_case_depth = 0usize;
    for index in start..words.len() {
        let word = &words[index];
        let command_position = is_if_word_command_position(words, start, index);
        if command_position && word.quote == QuoteKind::None {
            match word.text.as_str() {
                "case" => {
                    nested_case_depth += 1;
                    continue;
                }
                "esac" => {
                    if nested_case_depth == 0 {
                        return Some((index, CaseTerminator::Esac));
                    }
                    nested_case_depth = nested_case_depth.saturating_sub(1);
                    continue;
                }
                _ => {}
            }
        }
        if nested_case_depth == 0 && word.quote == QuoteKind::None && word.text == ";" {
            if words
                .get(index + 1)
                .is_some_and(|next| next.quote == QuoteKind::None && next.text == ";")
                && words
                    .get(index + 2)
                    .is_some_and(|next| next.quote == QuoteKind::None && next.text == "&")
            {
                return Some((index, CaseTerminator::PatternContinue));
            }
            if words
                .get(index + 1)
                .is_some_and(|next| next.quote == QuoteKind::None && next.text == ";")
            {
                return Some((index, CaseTerminator::ArmSeparator));
            }
            if words
                .get(index + 1)
                .is_some_and(|next| next.quote == QuoteKind::None && next.text == "&")
            {
                return Some((index, CaseTerminator::FallThrough));
            }
        }
    }
    None
}

fn if_words(invocation: &CommandInvocation) -> Vec<IfWord> {
    invocation
        .argv
        .iter()
        .enumerate()
        .map(|(index, fallback)| {
            let quote = invocation
                .argv_quote
                .get(index)
                .copied()
                .unwrap_or(QuoteKind::None);
            let source = invocation
                .argv_segments
                .get(index)
                .map(|segments| word_segments_to_compound_source(segments, fallback))
                .unwrap_or_else(|| fallback.clone());
            IfWord {
                text: fallback.clone(),
                source,
                quote,
            }
        })
        .collect()
}

fn find_top_level_reserved(
    words: &[IfWord],
    start: usize,
    targets: &[&str],
) -> Option<(usize, String)> {
    let mut nested_if_depth = 0usize;
    let mut nested_done_depth = 0usize;

    for (index, word) in words.iter().enumerate().skip(start) {
        if word.quote != QuoteKind::None {
            continue;
        }
        let command_position = is_if_word_command_position(words, start, index);
        if word.text == "if" && command_position {
            nested_if_depth += 1;
            continue;
        }
        if word.text == "fi" && command_position {
            if nested_if_depth == 0 && nested_done_depth == 0 && targets.contains(&"fi") {
                return Some((index, word.text.clone()));
            }
            nested_if_depth = nested_if_depth.saturating_sub(1);
            continue;
        }
        if matches!(word.text.as_str(), "while" | "until" | "for" | "select") && command_position {
            nested_done_depth += 1;
            continue;
        }
        if word.text == "done" && command_position {
            if nested_if_depth == 0 && nested_done_depth == 0 && targets.contains(&"done") {
                return Some((index, word.text.clone()));
            }
            nested_done_depth = nested_done_depth.saturating_sub(1);
            continue;
        }
        if nested_if_depth == 0
            && nested_done_depth == 0
            && command_position
            && targets.contains(&word.text.as_str())
        {
            return Some((index, word.text.clone()));
        }
    }

    None
}

fn is_if_word_command_position(words: &[IfWord], start: usize, index: usize) -> bool {
    if index == start {
        return true;
    }

    let Some(previous) = words[start..index]
        .iter()
        .rev()
        .find(|word| word.quote == QuoteKind::None)
    else {
        return true;
    };

    matches!(
        previous.text.as_str(),
        ";" | "&&" | "||" | "|" | "&" | "then" | "else" | "elif" | "do"
    )
}

fn words_to_source(words: &[IfWord]) -> String {
    words
        .iter()
        .map(|word| word.source.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

fn run_control_with_live_routing(
    invocation: &CommandInvocation,
    state: &mut ShellState,
    run: impl FnOnce(&mut ShellState) -> Result<CommandOutcome, ShellError>,
) -> Result<CommandOutcome, ShellError> {
    let redirections = expand_redirections(&invocation.redirections, state)?;
    let (routing, redirected_stdin) = prepare_compound_redirections(&redirections, state)?;
    run_with_effective_shell_stdin(state, None, redirected_stdin, |state| {
        with_inherited_capture_routing(routing, || run(state))
    })
}

fn run_if_invocation(
    if_block: &IfBlock,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    run_control_with_live_routing(invocation, state, |state| {
        run_if_invocation_inner(
            if_block,
            state,
            output_mode,
            capture_outputs,
            allow_process_replacement,
        )
    })
}

fn run_if_invocation_inner(
    if_block: &IfBlock,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let mut final_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());
    let nested_options = ExecutionOptions {
        output_mode: if capture_outputs {
            OutputMode::Clean
        } else {
            output_mode
        },
        allow_process_replacement,
    };

    for clause in &if_block.clauses {
        let mut condition = run_command_source(&clause.condition, state, &nested_options)?;
        final_outcome.append_streams(&mut condition)?;
        if state.should_exit() || state.loop_control_requested() {
            final_outcome.exit_code = condition.exit_code;
            return Ok(final_outcome);
        }
        if condition.exit_code == 0 {
            let mut body = run_command_source(&clause.body, state, &nested_options)?;
            final_outcome.exit_code = body.exit_code;
            final_outcome.append_streams(&mut body)?;
            return Ok(final_outcome);
        }
    }

    if let Some(else_body) = &if_block.else_body {
        let mut body = run_command_source(else_body, state, &nested_options)?;
        final_outcome.exit_code = body.exit_code;
        final_outcome.append_streams(&mut body)?;
    }

    Ok(final_outcome)
}

fn run_while_invocation(
    while_block: &WhileBlock,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    state.enter_loop();
    let result = run_control_with_live_routing(invocation, state, |state| {
        run_while_invocation_inner(
            while_block,
            state,
            output_mode,
            capture_outputs,
            allow_process_replacement,
        )
    });
    state.leave_loop();
    result
}

fn run_while_invocation_inner(
    while_block: &WhileBlock,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let mut final_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());
    let nested_options = ExecutionOptions {
        output_mode: if capture_outputs {
            OutputMode::Clean
        } else {
            output_mode
        },
        allow_process_replacement,
    };

    loop {
        let mut condition = run_command_source(&while_block.condition, state, &nested_options)?;
        final_outcome.append_streams(&mut condition)?;
        if state.should_exit()
            || state.return_requested()
            || state.interrupted()
            || state.stream_pipe_closed()
        {
            final_outcome.exit_code = condition.exit_code;
            break;
        }
        if state.loop_control_requested() {
            match state.handle_loop_control_for_current_loop() {
                Some(LoopControlKind::Break) => break,
                Some(LoopControlKind::Continue) => continue,
                None => {}
            }
        }
        if !while_block.kind.should_run_body(condition.exit_code) {
            break;
        }

        let mut body = run_command_source(&while_block.body, state, &nested_options)?;
        final_outcome.exit_code = body.exit_code;
        final_outcome.append_streams(&mut body)?;
        if state.loop_control_requested() {
            match state.handle_loop_control_for_current_loop() {
                Some(LoopControlKind::Break) => break,
                Some(LoopControlKind::Continue) => continue,
                None => {}
            }
        }
        if state.should_exit()
            || state.return_requested()
            || state.interrupted()
            || state.stream_pipe_closed()
            || (state.errexit() && body.exit_code != 0)
        {
            break;
        }
    }

    Ok(final_outcome)
}

fn run_for_invocation(
    for_block: &ForBlock,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    state.enter_loop();
    let result = run_control_with_live_routing(invocation, state, |state| {
        run_for_invocation_inner(
            for_block,
            state,
            output_mode,
            capture_outputs,
            allow_process_replacement,
        )
    });
    state.leave_loop();
    result
}

fn run_for_invocation_inner(
    for_block: &ForBlock,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let mut final_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());
    let nested_options = ExecutionOptions {
        output_mode: if capture_outputs {
            OutputMode::Clean
        } else {
            output_mode
        },
        allow_process_replacement,
    };
    // C-style arithmetic for-loop.
    if let Some((init, cond, step)) = &for_block.arithmetic {
        if !init.is_empty() {
            eval_arithmetic(init, state)?;
        }
        loop {
            if !cond.is_empty() && eval_arithmetic(cond, state)? == 0 {
                break;
            }
            let mut body = run_command_source(&for_block.body, state, &nested_options)?;
            final_outcome.exit_code = body.exit_code;
            final_outcome.append_streams(&mut body)?;
            if state.loop_control_requested() {
                match state.handle_loop_control_for_current_loop() {
                    Some(LoopControlKind::Break) => break,
                    Some(LoopControlKind::Continue) => {}
                    None => {}
                }
            }
            if state.should_exit()
                || state.return_requested()
                || state.interrupted()
                || state.stream_pipe_closed()
                || (state.errexit() && body.exit_code != 0)
            {
                break;
            }
            if !step.is_empty() {
                eval_arithmetic(step, state)?;
            }
        }
        return Ok(final_outcome);
    }

    let items = expand_for_items(&for_block.items, state)?;

    for item in items {
        if !state.try_set_var(&for_block.variable, &item) {
            final_outcome.exit_code = 1;
            final_outcome.stderr.extend_from_slice(
                format!("for: {}: readonly variable\n", for_block.variable).as_bytes(),
            );
            break;
        }
        let mut body = run_command_source(&for_block.body, state, &nested_options)?;
        final_outcome.exit_code = body.exit_code;
        final_outcome.append_streams(&mut body)?;
        if state.loop_control_requested() {
            match state.handle_loop_control_for_current_loop() {
                Some(LoopControlKind::Break) => break,
                Some(LoopControlKind::Continue) => continue,
                None => {}
            }
        }
        if state.should_exit()
            || state.return_requested()
            || state.interrupted()
            || state.stream_pipe_closed()
            || (state.errexit() && body.exit_code != 0)
        {
            break;
        }
    }

    Ok(final_outcome)
}

fn run_select_invocation(
    select_block: &SelectBlock,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    state.enter_loop();
    let has_redirections = !invocation.redirections.is_empty();
    let result = run_control_with_live_routing(invocation, state, |state| {
        run_select_invocation_inner(
            select_block,
            state,
            output_mode,
            capture_outputs,
            allow_process_replacement,
            has_redirections,
        )
    });
    state.leave_loop();
    result
}

fn run_select_invocation_inner(
    select_block: &SelectBlock,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
    has_redirections: bool,
) -> Result<CommandOutcome, ShellError> {
    let mut final_outcome = CommandOutcome::captured(1, Vec::new(), Vec::new());
    let nested_options = ExecutionOptions {
        output_mode: if capture_outputs {
            OutputMode::Clean
        } else {
            output_mode
        },
        allow_process_replacement,
    };
    let items = expand_for_items(&select_block.items, state)?;
    let live_prompt = !capture_outputs
        && !has_redirections
        && inherited_capture_routing().is_default()
        && io::stdin().is_terminal();

    emit_select_stderr(
        &mut final_outcome,
        state,
        live_prompt,
        &format_select_menu(&items),
    )?;
    let prompt = state.lookup("PS3").unwrap_or("#? ").to_string();

    loop {
        emit_select_stderr(&mut final_outcome, state, live_prompt, prompt.as_bytes())?;
        let Some(mut line) = read_one_line(None, state)? else {
            break;
        };
        trim_line_ending(&mut line);
        if !state.try_set_var("REPLY", &line) {
            final_outcome.exit_code = 1;
            final_outcome
                .stderr
                .extend_from_slice(b"select: REPLY: readonly variable\n");
            break;
        }
        let selected = select_choice_index(&line, items.len())
            .and_then(|index| items.get(index))
            .cloned()
            .unwrap_or_default();
        if !state.try_set_var(&select_block.variable, &selected) {
            final_outcome.exit_code = 1;
            final_outcome.stderr.extend_from_slice(
                format!("select: {}: readonly variable\n", select_block.variable).as_bytes(),
            );
            break;
        }

        let mut body = run_command_source(&select_block.body, state, &nested_options)?;
        final_outcome.exit_code = body.exit_code;
        final_outcome.append_streams(&mut body)?;
        if state.loop_control_requested() {
            match state.handle_loop_control_for_current_loop() {
                Some(LoopControlKind::Break) => break,
                Some(LoopControlKind::Continue) => continue,
                None => {}
            }
        }
        if state.should_exit()
            || state.return_requested()
            || state.interrupted()
            || state.stream_pipe_closed()
            || (state.errexit() && body.exit_code != 0)
        {
            break;
        }
    }

    Ok(final_outcome)
}

fn format_select_menu(items: &[String]) -> Vec<u8> {
    let mut menu = Vec::new();
    for (index, item) in items.iter().enumerate() {
        menu.extend_from_slice(format!("{}) {item}\n", index + 1).as_bytes());
    }
    menu
}

fn emit_select_stderr(
    outcome: &mut CommandOutcome,
    state: &ShellState,
    live_prompt: bool,
    bytes: &[u8],
) -> Result<(), ShellError> {
    if live_prompt {
        let mut stderr = io::stderr();
        stderr.write_all(bytes)?;
        stderr.flush()?;
    } else {
        let mut emitted = CommandOutcome::captured(0, Vec::new(), bytes.to_vec());
        apply_builtin_redirections(&mut emitted, &[], state)?;
        outcome.append_streams(&mut emitted)?;
    }
    Ok(())
}

fn select_choice_index(input: &str, item_count: usize) -> Option<usize> {
    let choice = input.trim().parse::<usize>().ok()?;
    if (1..=item_count).contains(&choice) {
        Some(choice - 1)
    } else {
        None
    }
}

fn run_case_invocation(
    case_block: &CaseBlock,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    run_control_with_live_routing(invocation, state, |state| {
        run_case_invocation_inner(
            case_block,
            state,
            output_mode,
            capture_outputs,
            allow_process_replacement,
        )
    })
}

fn run_case_invocation_inner(
    case_block: &CaseBlock,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let value = expand_word(&case_block.value, state)?;
    let nested_options = ExecutionOptions {
        output_mode: if capture_outputs {
            OutputMode::Clean
        } else {
            output_mode
        },
        allow_process_replacement,
    };
    let mut final_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());

    let mut execute_next_arm = false;
    for arm in &case_block.arms {
        let should_execute = if execute_next_arm {
            true
        } else {
            let mut matched = false;
            for pattern in &arm.patterns {
                let pattern = expand_word(pattern, state)?;
                if glob_match_bytes(pattern.as_bytes(), value.as_bytes()) {
                    matched = true;
                    break;
                }
            }
            matched
        };
        if !should_execute {
            continue;
        }

        let mut body = run_command_source(&arm.body, state, &nested_options)?;
        final_outcome.exit_code = body.exit_code;
        final_outcome.append_streams(&mut body)?;
        match arm.terminator {
            CaseTerminator::ArmSeparator | CaseTerminator::Esac => break,
            CaseTerminator::FallThrough => execute_next_arm = true,
            CaseTerminator::PatternContinue => execute_next_arm = false,
        }
    }

    Ok(final_outcome)
}

fn expand_for_items(items: &ForItems, state: &mut ShellState) -> Result<Vec<String>, ShellError> {
    match items {
        ForItems::Words(words) => {
            let mut fields = Vec::new();
            for word in words {
                fields.extend(expand_word_segments_to_argv_fields(word, state)?);
            }
            Ok(fields)
        }
        ForItems::Positionals => Ok(state.positionals()),
    }
}

/// Detect a subshell `( list )` invocation and return its inner source.
fn parse_subshell_invocation(invocation: &CommandInvocation) -> Result<Option<String>, ShellError> {
    // `(( ... ))` is an arithmetic command, not a subshell.
    if invocation
        .argv
        .first()
        .is_some_and(|a| a.starts_with("((") && a.ends_with("))"))
    {
        return Ok(None);
    }
    let starts_paren = invocation
        .argv_segments
        .first()
        .and_then(|segments| segments.first())
        .map(|segment| segment.quote == QuoteKind::None && segment.text.starts_with('('))
        .unwrap_or_else(|| {
            invocation.argv_quote.first() == Some(&QuoteKind::None)
                && invocation
                    .argv
                    .first()
                    .is_some_and(|word| word.starts_with('('))
        });
    if !starts_paren {
        return Ok(None);
    }

    let words = if_words(invocation);
    let joined = words_to_source(&words);
    let trimmed = joined.trim();
    let inner = trimmed
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
        .ok_or_else(|| ShellError::parse("subshell: missing matching )"))?;
    Ok(Some(inner.trim().to_string()))
}

/// Detect a brace group `{ list; }` invocation and return its inner source.
fn parse_brace_group_invocation(
    invocation: &CommandInvocation,
) -> Result<Option<String>, ShellError> {
    let opens = invocation
        .argv
        .first()
        .zip(invocation.argv_quote.first())
        .is_some_and(|(word, quote)| word == "{" && *quote == QuoteKind::None);
    if !opens {
        return Ok(None);
    }
    let len = invocation.argv.len();
    let closes = len >= 2
        && invocation
            .argv
            .last()
            .zip(invocation.argv_quote.last())
            .is_some_and(|(word, quote)| word == "}" && *quote == QuoteKind::None);
    if !closes {
        return Err(ShellError::parse("brace group: missing closing }"));
    }

    let words = if_words(invocation);
    let inner = words_to_source(&words[1..words.len() - 1]);
    Ok(Some(inner.trim().to_string()))
}

/// Run a subshell: execute the inner list in an isolated copy of shell state so
/// variable/cwd/option changes do not propagate to the parent.
fn run_subshell_invocation(
    inner_source: &str,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let nested_options = ExecutionOptions {
        output_mode: if capture_outputs {
            OutputMode::Clean
        } else {
            output_mode
        },
        allow_process_replacement,
    };
    let redirections = expand_redirections(&invocation.redirections, state)?;
    let (routing, redirected_stdin) = prepare_compound_redirections(&redirections, state)?;
    let mut sub_state = state.clone();
    let result =
        run_with_effective_shell_stdin(&mut sub_state, stdin_data, redirected_stdin, |sub_state| {
            with_inherited_capture_routing(routing, || {
                run_command_source(inner_source, sub_state, &nested_options)
            })
        });
    // `cd` mutates the process working directory; restore it so the subshell's
    // directory changes stay isolated from the parent.
    let _ = std::env::set_current_dir(state.cwd());
    let mut outcome = result?;
    outcome.observation = None;
    Ok(outcome)
}

/// Run a brace group: execute the inner list in the current shell state.
fn run_brace_group_invocation(
    inner_source: &str,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let nested_options = ExecutionOptions {
        output_mode: if capture_outputs {
            OutputMode::Clean
        } else {
            output_mode
        },
        allow_process_replacement,
    };
    let redirections = expand_redirections(&invocation.redirections, state)?;
    let (routing, redirected_stdin) = prepare_compound_redirections(&redirections, state)?;
    let mut outcome =
        run_with_effective_shell_stdin(state, stdin_data, redirected_stdin, |state| {
            with_inherited_capture_routing(routing, || {
                run_command_source(inner_source, state, &nested_options)
            })
        })?;
    outcome.observation = None;
    Ok(outcome)
}

fn run_command_source(
    source: &str,
    state: &mut ShellState,
    options: &ExecutionOptions,
) -> Result<CommandOutcome, ShellError> {
    if source.trim().is_empty() {
        return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
    }
    let graph = parse_line(source)?;
    let mut executor = Executor::new().with_stdout_flush(stream_raw_to_parent());
    let mut outcome = executor.run_graph(&graph, state, options)?;
    outcome.observation = None;
    Ok(outcome)
}

fn run_function_invocation(
    function: &ShellFunction,
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let saved_positionals = save_positionals(state);
    let function_args = invocation.argv.iter().skip(1).cloned().collect::<Vec<_>>();
    state.set_positionals(&function_args);
    state.enter_function_scope();

    let mut prefix_stderr = Vec::new();
    let mut prefix_failed = false;
    for assignment in &invocation.assignments {
        let base = assignment_binding_name(&assignment.name);
        if !state.declare_local(base) {
            prefix_stderr
                .extend_from_slice(format!("agsh: {base}: readonly variable\n").as_bytes());
            prefix_failed = true;
            continue;
        }
        if let Some(message) = apply_assignment(state, assignment) {
            prefix_stderr.extend_from_slice(message.as_bytes());
            prefix_failed = true;
        } else {
            state.mark_exported(base);
        }
    }

    let (routing, redirected_stdin) =
        prepare_compound_redirections(&invocation.redirections, state)?;
    let result: Result<CommandOutcome, ShellError> = if prefix_failed {
        with_inherited_capture_routing(routing, || {
            let mut outcome = CommandOutcome::captured(1, Vec::new(), prefix_stderr);
            apply_builtin_redirections(&mut outcome, &[], state)?;
            Ok(outcome)
        })
    } else {
        run_with_effective_shell_stdin(state, stdin_data, redirected_stdin, |state| {
            with_inherited_capture_routing(routing, || {
                let graph = parse_line(&function.body)?;
                let mut executor = Executor::new().with_stdout_flush(stream_raw_to_parent());
                let mut outcome = executor.run_graph(
                    &graph,
                    state,
                    &ExecutionOptions {
                        output_mode: if capture_outputs {
                            OutputMode::Clean
                        } else {
                            output_mode
                        },
                        allow_process_replacement,
                    },
                )?;
                outcome.observation = None;
                Ok(outcome)
            })
        })
    };
    state.leave_function_scope();
    restore_positionals(state, &saved_positionals);
    let mut outcome = result?;
    // `return` inside the body stops the function; consume the request here so
    // it does not leak to the caller.
    if let Some(code) = state.take_return() {
        outcome.exit_code = code;
    }
    Ok(outcome)
}

fn save_positionals(state: &ShellState) -> PositionalSnapshot {
    PositionalSnapshot {
        args: state.positionals(),
        at_was_set: state.lookup("@").is_some(),
    }
}

fn restore_positionals(state: &mut ShellState, saved: &PositionalSnapshot) {
    if saved.at_was_set || !saved.args.is_empty() {
        state.set_positionals(&saved.args);
    } else {
        state.clear_positionals();
    }
}

fn word_segments_to_source(segments: &[WordSegment], fallback: &str) -> String {
    if segments.is_empty() {
        return fallback.to_string();
    }
    segments
        .iter()
        .map(|segment| match segment.quote {
            QuoteKind::None => shell_escape_unquoted(&segment.text),
            QuoteKind::Single => format!("'{}'", segment.text.replace('\'', "'\\''")),
            QuoteKind::Double => format!(
                "\"{}\"",
                segment.text.replace('\\', "\\\\").replace('"', "\\\"")
            ),
        })
        .collect::<String>()
}

fn word_segments_to_compound_source(segments: &[WordSegment], fallback: &str) -> String {
    if segments.is_empty() {
        return fallback.to_string();
    }
    segments
        .iter()
        .map(|segment| match segment.quote {
            QuoteKind::None => segment.text.clone(),
            QuoteKind::Single => format!("'{}'", segment.text.replace('\'', "'\\''")),
            QuoteKind::Double => format!(
                "\"{}\"",
                segment.text.replace('\\', "\\\\").replace('"', "\\\"")
            ),
        })
        .collect::<String>()
}

fn shell_escape_unquoted(text: &str) -> String {
    if text
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '\'' | '"' | '|' | '<' | '>' | '&'))
    {
        format!("'{}'", text.replace('\'', "'\\''"))
    } else {
        text.to_string()
    }
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// A command-not-found result: exit 127, error to stderr, execution continues
/// (matching POSIX shells) rather than aborting the whole command line.
fn command_not_found_outcome(name: &str, state: &ShellState) -> CommandOutcome {
    use std::io::IsTerminal;
    // Color the message only when stderr is a TTY (piped/`-c` stderr stays plain).
    let theme = if std::io::stderr().is_terminal() {
        state.theme()
    } else {
        agsh_style::Theme::plain()
    };

    let mut message = String::new();
    message.push_str(&theme.paint(
        agsh_style::Role::Error,
        &format!("{} {name}: command not found", theme.icons.error()),
    ));
    message.push('\n');

    // The "did you mean" + install advisory repeats verbatim if the same typo recurs
    // in a loop or an agent retry, flooding the context — so emit it only on the
    // first occurrence of this name per session. The error line + exit 127 always emit.
    if state.advise_once(&format!("cmd-not-found:{name}")) {
        let mut candidates: Vec<String> = crate::builtins::builtin_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        candidates.extend(state.aliases().keys().cloned());
        candidates.extend(state.functions().keys().cloned());
        if let Some(path) = state.lookup("PATH") {
            candidates.extend(crate::suggest::path_executables(path));
        }
        let suggestions = crate::suggest::did_you_mean(name, candidates.into_iter());
        if !suggestions.is_empty() {
            message.push_str(&theme.paint(agsh_style::Role::Muted, "Did you mean:"));
            message.push('\n');
            for suggestion in &suggestions {
                message.push_str(&format!(
                    "  {}\n",
                    theme.paint(agsh_style::Role::Command, suggestion)
                ));
            }
        }
        if let Some(hint) = crate::suggest::install_hint(name) {
            message.push_str(&theme.paint(agsh_style::Role::Muted, &format!("Install: {hint}")));
            message.push('\n');
        }
    }

    CommandOutcome::captured(127, Vec::new(), message.into_bytes())
}

/// `builtin <name>` where name is not a shell builtin: exit 1, like bash.
fn builtin_not_found_outcome(name: &str) -> CommandOutcome {
    CommandOutcome::captured(
        1,
        Vec::new(),
        format!("agsh: builtin: {name}: not a shell builtin\n").into_bytes(),
    )
}

fn run_resolved_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    lookup_mode: LookupMode,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let name = invocation.argv[0].as_str();
    let mut pre_invocation = if state.xtrace() {
        prepare_pre_invocation_prefix(render_xtrace(invocation), state, capture_outputs)?
    } else {
        CommandOutcome::captured(0, Vec::new(), Vec::new())
    };

    // Confinement gate (single-command path): refuse a non-allowlisted external
    // with a clear message and exit 126, before it resolves. Builtins are exempt
    // — they funnel any external targets back through the gated resolver. This is
    // hit by direct commands and by eval/`$(…)`/subshell re-entry alike.
    if let Some(policy) = state.confine_policy() {
        if !is_builtin(name) && !policy.allows(name) {
            let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
            let mut outcome = finish_synthetic_invocation_outcome(
                CommandOutcome::captured(
                    126,
                    Vec::new(),
                    format!(
                        "agsh: {base}: not permitted in this confined session (allowed: {})\n",
                        policy.display_list()
                    )
                    .into_bytes(),
                ),
                invocation,
                state,
            )?;
            let exit_code = outcome.exit_code;
            pre_invocation.append_streams(&mut outcome)?;
            pre_invocation.exit_code = exit_code;
            return Ok(pre_invocation);
        }
    }

    let mut outcome = match lookup_mode {
        LookupMode::BuiltinOnly => {
            if !is_builtin(name) {
                let mut outcome = finish_synthetic_invocation_outcome(
                    builtin_not_found_outcome(name),
                    invocation,
                    state,
                )?;
                let exit_code = outcome.exit_code;
                pre_invocation.append_streams(&mut outcome)?;
                pre_invocation.exit_code = exit_code;
                return Ok(pre_invocation);
            }
            run_shell_builtin_invocation(
                invocation,
                state,
                output_mode,
                stdin_data,
                capture_outputs,
                allow_process_replacement,
            )
        }
        LookupMode::ExternalOnly => {
            let Some(path) = resolve_external_path(invocation, state, name) else {
                let mut outcome = finish_synthetic_invocation_outcome(
                    command_not_found_outcome(name, state),
                    invocation,
                    state,
                )?;
                let exit_code = outcome.exit_code;
                pre_invocation.append_streams(&mut outcome)?;
                pre_invocation.exit_code = exit_code;
                return Ok(pre_invocation);
            };
            run_external(
                invocation,
                state,
                output_mode,
                stdin_data,
                capture_outputs,
                Some(path.as_path()),
            )
        }
        LookupMode::Normal | LookupMode::BypassAliases | LookupMode::DefaultPath
            if is_builtin(name) =>
        {
            run_shell_builtin_invocation(
                invocation,
                state,
                output_mode,
                stdin_data,
                capture_outputs,
                allow_process_replacement,
            )
        }
        LookupMode::Normal | LookupMode::BypassAliases | LookupMode::DefaultPath => {
            let path = if lookup_mode == LookupMode::DefaultPath {
                resolve_external_path_with(invocation, state, name, DEFAULT_COMMAND_PATH)
            } else {
                resolve_external_path(invocation, state, name)
            };
            let Some(path) = path else {
                let mut outcome = finish_synthetic_invocation_outcome(
                    command_not_found_outcome(name, state),
                    invocation,
                    state,
                )?;
                let exit_code = outcome.exit_code;
                pre_invocation.append_streams(&mut outcome)?;
                pre_invocation.exit_code = exit_code;
                return Ok(pre_invocation);
            };
            run_external(
                invocation,
                state,
                output_mode,
                stdin_data,
                capture_outputs,
                Some(path.as_path()),
            )
        }
    }?;

    let exit_code = outcome.exit_code;
    pre_invocation.append_streams(&mut outcome)?;
    pre_invocation.exit_code = exit_code;
    Ok(pre_invocation)
}

fn finish_synthetic_invocation_outcome(
    mut outcome: CommandOutcome,
    invocation: &ExpandedInvocation,
    state: &ShellState,
) -> Result<CommandOutcome, ShellError> {
    apply_builtin_redirections(&mut outcome, &invocation.redirections, state)?;
    Ok(outcome)
}

fn render_xtrace(invocation: &ExpandedInvocation) -> Vec<u8> {
    let mut parts = invocation
        .assignments
        .iter()
        .map(|assignment| {
            format!(
                "{}={}",
                assignment.name,
                shell_escape_unquoted(&assignment.value)
            )
        })
        .collect::<Vec<_>>();
    parts.extend(
        invocation
            .argv
            .iter()
            .map(|arg| shell_escape_unquoted(arg))
            .collect::<Vec<_>>(),
    );
    format!("+ {}\n", parts.join(" ")).into_bytes()
}

/// POSIX special builtins: a preceding variable assignment persists after the
/// builtin returns (unlike regular builtins, where it is transient).
fn is_special_builtin(name: &str) -> bool {
    matches!(
        name,
        ":" | "."
            | "source"
            | "eval"
            | "exec"
            | "exit"
            | "export"
            | "readonly"
            | "return"
            | "set"
            | "shift"
            | "unset"
            | "break"
            | "continue"
    )
}

fn assignment_binding_name(name: &str) -> &str {
    name.trim_end_matches('+')
        .split_once('[')
        .map_or(name.trim_end_matches('+'), |(base, _)| base)
}

fn run_shell_builtin_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let redirected_stdin = redirected_stdin_from_expanded_redirections(&invocation.redirections)?;
    run_with_effective_shell_stdin(state, stdin_data, redirected_stdin, |state| {
        run_shell_builtin_with_effective_stdin(
            invocation,
            state,
            output_mode,
            capture_outputs,
            allow_process_replacement,
        )
    })
}

fn run_shell_builtin_with_effective_stdin(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    // Prefix assignments form the command environment. Regular builtins get a
    // complete binding snapshot that is restored afterwards; POSIX special
    // builtins retain both the value and export attribute after they return.
    let transient = !is_special_builtin(&invocation.argv[0]);
    let mut saved_assignments = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if transient {
        for assignment in &invocation.assignments {
            let name = assignment_binding_name(&assignment.name);
            if seen.insert(name.to_string()) {
                saved_assignments.push(state.snapshot_variable(name));
            }
        }
    }

    let mut assignment_stderr = Vec::new();
    let mut assignment_failed = false;
    for assignment in &invocation.assignments {
        if let Some(message) = apply_assignment(state, assignment) {
            assignment_stderr.extend_from_slice(message.as_bytes());
            assignment_failed = true;
        } else {
            state.mark_exported(assignment_binding_name(&assignment.name));
        }
    }

    let result = if assignment_failed {
        Ok(CommandOutcome::captured(1, Vec::new(), assignment_stderr))
    } else {
        match invocation.argv[0].as_str() {
            "eval" => run_eval_invocation(
                invocation,
                state,
                output_mode,
                None,
                capture_outputs,
                allow_process_replacement,
            ),
            "source" | "." => run_source_invocation(
                invocation,
                state,
                output_mode,
                None,
                capture_outputs,
                allow_process_replacement,
            ),
            "exec" => run_exec_invocation(
                invocation,
                state,
                None,
                capture_outputs,
                allow_process_replacement,
            ),
            "read" => run_read_invocation(invocation, state, None),
            "agpatch" => run_patch_invocation(invocation, state, None),
            "agconfine" | "confine" => run_confine_invocation(
                invocation,
                state,
                output_mode,
                capture_outputs,
                allow_process_replacement,
            ),
            name if name.starts_with("((") && name.ends_with("))") => {
                // `(( expr ))` arithmetic command: exit 0 if the value is non-zero.
                let expr = name[2..name.len() - 2].to_string();
                match eval_arithmetic(&expr, state) {
                    Ok(value) => Ok(CommandOutcome::captured(
                        i32::from(value == 0),
                        Vec::new(),
                        Vec::new(),
                    )),
                    Err(error) => Ok(CommandOutcome::captured(
                        1,
                        Vec::new(),
                        format!("agsh: ((: {error}\n").into_bytes(),
                    )),
                }
            }
            _ => run_builtin(&to_command_invocation(invocation), state),
        }
    };

    for saved in saved_assignments.into_iter().rev() {
        state.restore_variable(saved);
    }

    let mut outcome = result?;
    apply_builtin_redirections(&mut outcome, &invocation.redirections, state)?;
    Ok(outcome)
}

/// POSIX-sh shim body; `__AGSH__` is replaced with the quoted agsh path.
const SHIM_TEMPLATE: &str = r#"#!/bin/sh
# agsh confine shim — route the agent's shell through confined agsh.
c=
while [ $# -gt 0 ]; do
  case "$1" in
    -c) c=$2; shift 2; break;;
    --) shift; break;;
    --rcfile|--init-file|-o|-O) shift 2;;
    --*) shift;;
    -*c*) c=$2; shift 2; break;;
    -*) shift;;
    *) break;;
  esac
done
if [ -n "$c" ]; then exec __AGSH__ -c "$c"; fi
case "${1:-}" in
  -*|"") exec __AGSH__;;
  *) exec __AGSH__ "$@";;
esac
"#;

/// Install shell shims so a confined agent's own shell invocations are routed
/// back through agsh and gated. Claude-style agents run commands via their own
/// `bash -c '…'` subprocess (not through agsh), which would otherwise bypass the
/// allowlist. This drops `bash`/`sh`/`zsh`/`dash` shims — each re-execs agsh, which
/// self-confines from the inherited `AGSH_CONFINE` — into a temp dir, prepends it
/// to `PATH`, and points `SHELL` at it.
///
/// Coverage caveat: this catches shells resolved via `PATH`/`SHELL`. A program
/// that calls `/bin/bash` by absolute path, or spawns an interpreter (python,
/// node) directly, still bypasses it — closing that needs an OS sandbox (G7).
pub fn install_confine_shims(state: &mut ShellState) -> std::io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    install_confine_shims_in(state, &std::env::temp_dir(), &exe)
}

fn install_confine_shims_in(
    state: &mut ShellState,
    temp_base: &Path,
    exe: &Path,
) -> io::Result<PathBuf> {
    // POSIX-sh shim: pull the command out of `-c CMD` (handling separate `-l -c`,
    // combined short bundles `-lc`/`-ic`, and arg-taking long options like
    // `--rcfile FILE`/`--norc`) and re-exec agsh, which self-confines from
    // AGSH_CONFINE. With no `-c` (interactive/persistent login shell) it drops to
    // an agsh REPL; stray flags are never forwarded to agsh (they would error).
    // Long `--*` options are matched before the `-*c*` short-bundle rule so that
    // e.g. `--norc`/`--rcfile` are NOT mistaken for a `-c`-bearing flag.
    let agsh = shell_quote(path_as_utf8(exe, "agsh executable")?);
    let shim = SHIM_TEMPLATE.replace("__AGSH__", &agsh);
    let definitions = ["bash", "sh", "zsh", "dash", "ksh"].map(|name| (name, shim.as_str()));
    let dir = build_shim_generation(temp_base, "confine", &definitions)?;
    let dir_text = path_as_utf8(&dir, "confine shim directory")?;
    let shell = dir.join("bash");
    let shell_text = path_as_utf8(&shell, "confine shell shim")?;

    // State changes happen only after every shim is complete and verified.
    let prev_path = state.lookup("PATH").unwrap_or_default().to_string();
    state.export_var("PATH", format!("{dir_text}:{prev_path}"));
    state.export_var("SHELL", shell_text.to_string());
    Ok(dir)
}

const SHIM_GENERATION_ATTEMPTS: usize = 128;
const SHIM_DIRECTORY_MODE: u32 = 0o700;
const SHIM_FILE_MODE: u32 = 0o500;
static NEXT_SHIM_GENERATION: AtomicU64 = AtomicU64::new(1);

fn path_as_utf8<'a>(path: &'a Path, description: &str) -> io::Result<&'a str> {
    path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{description} is not valid UTF-8"),
        )
    })
}

/// Validate the parent in which executable PATH shims are generated. A private
/// owner-controlled directory is safe; so is a conventional root-owned sticky
/// world-temp directory. Other shared writable parents permit path replacement.
fn validate_shim_temp_parent(base: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    path_as_utf8(base, "shim temporary directory")?;
    let canonical = std::fs::canonicalize(base)?;
    path_as_utf8(&canonical, "canonical shim temporary directory")?;
    let metadata = std::fs::symlink_metadata(&canonical)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "shim temporary path is not a real directory",
        ));
    }

    let mode = metadata.permissions().mode() & 0o7777;
    let uid = metadata.uid();
    let euid = rustix::process::geteuid().as_raw();
    let protected_shared_temp = uid == 0 && mode & 0o1000 != 0 && mode & 0o002 != 0;
    if uid != euid && !protected_shared_temp {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "shim temporary directory is not owned by this user",
        ));
    }
    if mode & 0o022 != 0 && !protected_shared_temp {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "shim temporary directory is writable by another user",
        ));
    }
    Ok(canonical)
}

fn verify_private_shim_directory(path: &Path, kind: &str) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let expected_prefix = format!("agsh-{kind}-");
    if !path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(&expected_prefix))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "shim directory name has an invalid prefix",
        ));
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o7777 != SHIM_DIRECTORY_MODE
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "shim generation directory is not private",
        ));
    }
    Ok(())
}

fn create_private_shim_directory(base: &Path, kind: &str) -> io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    let base = validate_shim_temp_parent(base)?;
    for _ in 0..SHIM_GENERATION_ATTEMPTS {
        let sequence = NEXT_SHIM_GENERATION.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let path = base.join(format!(
            "agsh-{kind}-{}-{nanos:032x}-{sequence:016x}",
            std::process::id()
        ));
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(SHIM_DIRECTORY_MODE);
        match builder.create(&path) {
            Ok(()) => {
                // A restrictive umask can remove owner bits. Tighten through an
                // already-open no-follow directory descriptor, never by pathname.
                let configured = (|| {
                    let descriptor = rustix::fs::open(
                        &path,
                        rustix::fs::OFlags::RDONLY
                            | rustix::fs::OFlags::DIRECTORY
                            | rustix::fs::OFlags::CLOEXEC
                            | rustix::fs::OFlags::NOFOLLOW,
                        rustix::fs::Mode::empty(),
                    )
                    .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
                    rustix::fs::fchmod(
                        &descriptor,
                        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
                    )
                    .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
                    drop(descriptor);
                    verify_private_shim_directory(&path, kind)
                })();
                if let Err(error) = configured {
                    let _ = std::fs::remove_dir_all(&path);
                    return Err(error);
                }
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a fresh private shim directory",
    ))
}

fn create_executable_shim(directory: &Path, name: &str, content: &str) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid shim filename",
        ));
    }
    let path = directory.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(content.as_bytes())?;
    file.flush()?;
    rustix::fs::fchmod(&file, rustix::fs::Mode::RUSR | rustix::fs::Mode::XUSR)
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    file.sync_all()?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o7777 != SHIM_FILE_MODE
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "generated shim is not a private executable regular file",
        ));
    }
    Ok(())
}

fn build_shim_generation(
    temp_base: &Path,
    kind: &str,
    definitions: &[(&str, &str)],
) -> io::Result<PathBuf> {
    let directory = create_private_shim_directory(temp_base, kind)?;
    let result = definitions
        .iter()
        .try_for_each(|(name, content)| create_executable_shim(&directory, name, content));
    if let Err(error) = result {
        // This path was created exclusively beneath a validated safe parent and
        // has not entered PATH, so rollback cannot remove caller-owned content.
        let _ = std::fs::remove_dir_all(&directory);
        return Err(error);
    }
    Ok(directory)
}

/// Interception shim body (the compacting-proxy / flavor B): route the agent's
/// shell through `agsh --observe`, which runs the REAL shell and captures+renders
/// its output. Passes straight through to the real shell once already inside an
/// observed subtree, so nested shells run normally and there is no re-entrancy.
const INTERCEPT_SHIM_TEMPLATE: &str = r#"#!/bin/sh
# agsh interception shim — observe the agent's shell through agsh.
if [ -n "$AGSH_INTERCEPT_ACTIVE" ]; then exec __REAL__ "$@"; fi
exec __AGSH__ --output __MODE__ --observe __REAL__ "$@"
"#;

/// Flavor-A (native-interpret) interception shim: agsh *interprets* the `-c`
/// command itself rather than running the real shell. `AGSH_INTERCEPT_ACTIVE=1` is
/// set for the agsh child so any nested shells run as the real shell (no recursion);
/// non-`-c` invocations fall through to the real shell unchanged.
const INTERCEPT_SHIM_TEMPLATE_NATIVE: &str = r#"#!/bin/sh
# agsh interception shim (native) — interpret the agent's command in agsh.
if [ -n "$AGSH_INTERCEPT_ACTIVE" ]; then exec __REAL__ "$@"; fi
c=
while [ $# -gt 0 ]; do
  case "$1" in
    -c) c=$2; shift 2; break;;
    --) shift; break;;
    --rcfile|--init-file|-o|-O) shift 2;;
    --*) shift;;
    -*c*) c=$2; shift 2; break;;
    -*) shift;;
    *) break;;
  esac
done
if [ -n "$c" ]; then AGSH_INTERCEPT_ACTIVE=1 exec __AGSH__ --output __MODE__ -c "$c"; fi
exec __REAL__ "$@"
"#;

/// Install shell-*interception* shims (opt-in via `AGSH_INTERCEPT`): route the
/// session's `bash`/`sh`/`zsh`/… — resolved by name or via `$SHELL` — through
/// `agsh --observe`, so an agent's own `bash -c …` output is captured and rendered
/// in `mode`. The real shells are located on the current `PATH`; only installed
/// shells are shimmed, and each shim execs the real shell by absolute path (so it
/// never bounces back through the shim). Unlike [`install_confine_shims`], this
/// runs the real shell (exact semantics) instead of re-interpreting in agsh.
///
/// Coverage caveat: this catches shells resolved via `PATH`/`$SHELL`; a program
/// calling `/bin/bash` by absolute path bypasses it (that needs the exec-
/// interposition layer).
pub fn install_intercept_shims(
    state: &mut ShellState,
    mode: agsh_output::OutputMode,
    native: bool,
) -> std::io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    install_intercept_shims_in(state, mode, native, &std::env::temp_dir(), &exe)
}

fn install_intercept_shims_in(
    state: &mut ShellState,
    mode: agsh_output::OutputMode,
    native: bool,
    temp_base: &Path,
    exe: &Path,
) -> io::Result<PathBuf> {
    let safe_temp_base = validate_shim_temp_parent(temp_base)?;
    let agsh = shell_quote(path_as_utf8(exe, "agsh executable")?);
    let template = if native {
        INTERCEPT_SHIM_TEMPLATE_NATIVE
    } else {
        INTERCEPT_SHIM_TEMPLATE
    };
    let prior_path = state.lookup("PATH").map(str::to_string);
    let prior_shell = state.lookup("SHELL").map(str::to_string);
    let deep_env = DEEP_INTERCEPT_ENV
        .iter()
        .map(|name| state.snapshot_variable(name))
        .collect();
    let path_str = prior_path.clone().unwrap_or_default();
    let mut definitions = Vec::new();
    let mut selected_shell = None;
    for name in ["bash", "sh", "zsh", "dash", "ksh"] {
        // Resolve against the ORIGINAL PATH (our shim dir isn't prepended yet), so
        // we never point a shim at itself.
        let Some(real) = resolve_on_path(name, &path_str) else {
            continue;
        };
        let real = path_as_utf8(&real, "resolved shell executable")?;
        let shim = template
            .replace("__REAL__", &shell_quote(real))
            .replace("__AGSH__", &agsh)
            .replace("__MODE__", mode.as_str());
        if selected_shell.is_none() || name == "bash" {
            selected_shell = Some(name);
        }
        definitions.push((name, shim));
    }
    let selected_shell = selected_shell.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no supported shell executable was found on PATH",
        )
    })?;
    let borrowed = definitions
        .iter()
        .map(|(name, content)| (*name, content.as_str()))
        .collect::<Vec<_>>();
    let dir = build_shim_generation(&safe_temp_base, "intercept", &borrowed)?;
    let dir_text = path_as_utf8(&dir, "interception shim directory")?;
    let shell_path = dir.join(selected_shell);
    let shell_text = path_as_utf8(&shell_path, "interception shell shim")?;

    // All fallible generation work is complete before any shell state is changed.
    // Raw traces keep using the normal UID-scoped, independently validated store.
    state.export_var("PATH", format!("{dir_text}:{path_str}"));
    state.export_var("SHELL", shell_text.to_string());
    let introduced_env = set_agent_fail_fast_env(state);
    state.record_intercept_install(InterceptInstall {
        directory: dir.clone(),
        prior_path,
        prior_shell,
        introduced_env,
        deep_env,
    });
    Ok(dir)
}

/// Make interactive tools FAIL FAST instead of blocking an agent forever on a
/// terminal password prompt (a hang `confine` can't see — it gates capabilities,
/// not a `/dev/tty` read). Only well-known non-interactive toggles, and only if the
/// user hasn't set them — no `unsafe`, no `setsid` (macOS ships no such binary),
/// portable. The dominant real case is git-over-HTTPS credential prompts.
fn set_agent_fail_fast_env(state: &mut ShellState) -> Vec<(String, String)> {
    const FAIL_FAST: &[(&str, &str)] = &[
        ("GIT_TERMINAL_PROMPT", "0"), // git: error instead of prompting for creds
        ("GCM_INTERACTIVE", "never"), // git-credential-manager: no interactive UI
        ("SSH_ASKPASS_REQUIRE", "never"), // ssh: don't pop an askpass helper
    ];
    let mut introduced = Vec::new();
    for (key, value) in FAIL_FAST {
        if state.lookup(key).is_none() {
            state.export_var(*key, *value);
            introduced.push(((*key).to_string(), (*value).to_string()));
        }
    }
    introduced
}

/// Find the first executable file named `name` in a colon-separated `PATH`.
fn resolve_on_path(name: &str, path: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let candidate = std::path::Path::new(dir).join(name);
        if let Ok(meta) = std::fs::metadata(&candidate) {
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                return Some(candidate);
            }
        }
    }
    None
}

/// Marker present in a PATH entry / `$SHELL` when interception is installed.
const INTERCEPT_DIR_MARKER: &str = "agsh-intercept-";

fn is_managed_intercept_directory(entry: &str) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let path = Path::new(entry);
    if !path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(INTERCEPT_DIR_MARKER))
        || verify_private_shim_directory(path, "intercept").is_err()
    {
        return false;
    }
    ["bash", "sh", "zsh", "dash", "ksh"].iter().any(|name| {
        let Ok(metadata) = std::fs::symlink_metadata(path.join(name)) else {
            return false;
        };
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == rustix::process::geteuid().as_raw()
            && metadata.permissions().mode() & 0o7777 == SHIM_FILE_MODE
    })
}

fn shell_uses_managed_intercept_directory(shell: &str) -> bool {
    Path::new(shell)
        .parent()
        .and_then(Path::to_str)
        .is_some_and(is_managed_intercept_directory)
}

/// Parse an interception spec `<mode>[:native][:deep]` into `(mode, native, deep)`.
/// Returns `None` when disabled (`off`/empty) or the mode name is invalid.
pub fn parse_intercept_spec(spec: &str) -> Option<(agsh_output::OutputMode, bool, bool)> {
    let spec = spec.trim().to_ascii_lowercase();
    let mut parts = spec.split(':');
    let mode_part = parts.next().unwrap_or("");
    let flags: Vec<&str> = parts.collect();
    let native = flags.iter().any(|f| matches!(*f, "native" | "interpret"));
    let deep = flags.contains(&"deep");
    let mode = match mode_part {
        "" | "0" | "off" | "false" | "no" => return None,
        "1" | "on" | "true" | "yes" => agsh_output::OutputMode::Compact,
        other => <agsh_output::OutputMode as std::str::FromStr>::from_str(other).ok()?,
    };
    Some((mode, native, deep))
}

/// Whether shell interception is currently installed in `state` (a shim dir is on
/// `PATH`).
pub fn intercept_active(state: &ShellState) -> bool {
    let path = state.lookup("PATH").unwrap_or_default();
    if let Some(expected) = state.intercept_install_directory() {
        return path
            .split(':')
            .map(Path::new)
            .any(|entry| entry == expected && is_managed_intercept_directory(path_as_str(entry)));
    }
    path.split(':').any(is_managed_intercept_directory)
}

fn path_as_str(path: &Path) -> &str {
    path.to_str().unwrap_or("")
}

const DEEP_INTERCEPT_ENV: [&str; 4] = [
    "DYLD_INSERT_LIBRARIES",
    "LD_PRELOAD",
    "AGSH_SELF",
    "AGSH_INTERCEPT_MODE",
];

/// Enable the exec-interposition (deep) layer for the session's children by
/// preloading the `agsh-intercept` library. Returns `false` if the library can't be
/// located (caller should fall back to the PATH shims).
pub fn install_deep_intercept(state: &mut ShellState, mode: agsh_output::OutputMode) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let dir = exe
        .parent()
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let (var, ext) = if cfg!(target_os = "macos") {
        ("DYLD_INSERT_LIBRARIES", "dylib")
    } else {
        ("LD_PRELOAD", "so")
    };
    let name = format!("libagsh_intercept.{ext}");
    // Next to the binary, or an installed `../lib` layout.
    let candidates = [dir.join(&name), dir.join("..").join("lib").join(&name)];
    let Some(lib) = candidates.iter().find(|p| p.exists()) else {
        return false;
    };
    let prev = state.lookup(var).unwrap_or_default().to_string();
    let value = if prev.is_empty() {
        lib.display().to_string()
    } else {
        format!("{}:{prev}", lib.display())
    };
    state.export_var(var, value);
    state.export_var("AGSH_SELF", exe.display().to_string());
    state.export_var("AGSH_INTERCEPT_MODE", mode.as_str());
    true
}

/// Remove any shell-interception install from `state` (the shim dir on `PATH`, the
/// preload env, `$SHELL`). Newly launched children are no longer intercepted;
/// already-running children keep their environment.
pub fn uninstall_intercept(state: &mut ShellState) {
    let path = state.lookup("PATH").unwrap_or_default().to_string();
    let install = state.take_intercept_install();
    let cleaned = install
        .as_ref()
        .and_then(|install| install.prior_path.clone())
        .unwrap_or_else(|| {
            path.split(':')
                .filter(|entry| !is_managed_intercept_directory(entry))
                .collect::<Vec<_>>()
                .join(":")
        });
    if install.is_none() {
        // Compatibility cleanup for state restored without an in-memory install
        // record. New installs restore the exact prior bindings below.
        for var in ["DYLD_INSERT_LIBRARIES", "LD_PRELOAD"] {
            if let Some(current) = state.lookup(var) {
                let kept = current
                    .split(':')
                    .filter(|e| !e.contains("libagsh_intercept"))
                    .collect::<Vec<_>>()
                    .join(":");
                if kept.is_empty() {
                    state.unset(var);
                } else {
                    state.export_var(var, kept);
                }
            }
        }
        state.unset("AGSH_SELF");
        state.unset("AGSH_INTERCEPT_MODE");
    }
    if let Some(install) = &install {
        restore_env(state, "PATH", install.prior_path.clone());
        restore_env(state, "SHELL", install.prior_shell.clone());
        for (name, installed_value) in &install.introduced_env {
            if state.lookup(name) == Some(installed_value.as_str()) {
                state.unset(name);
            }
        }
        for saved in install.deep_env.iter().cloned() {
            state.restore_variable(saved);
        }
    } else if state
        .lookup("SHELL")
        .is_some_and(shell_uses_managed_intercept_directory)
    {
        // Compatibility cleanup for state restored without an in-memory install
        // record. New installs always restore the exact prior value above.
        let replacement = ["bash", "sh", "zsh", "dash", "ksh"]
            .iter()
            .find_map(|name| resolve_on_path(name, &cleaned))
            .and_then(|path| path.to_str().map(str::to_string));
        if let Some(real) = replacement {
            state.export_var("SHELL", real);
        } else {
            state.unset("SHELL");
        }
        state.export_var("PATH", cleaned);
    } else {
        state.export_var("PATH", cleaned);
    }
    // Do not remove successful generations here: children launched before the
    // toggle inherited their paths and must remain able to resolve the shims.
}

/// `confine LIST [-- COMMAND…]` — restrict which external commands may run.
///
/// * Sticky (no COMMAND): confine the current session (and its children, via the
///   inherited `AGSH_CONFINE` env). Narrow-only and irreversible for the session.
/// * Scoped (`-- COMMAND`): run COMMAND as a trusted payload with the allowlist
///   applied to its *descendants* (`AGSH_CONFINE` exported for the child); the
///   payload itself is exempt unless the session is already confined. The env is
///   restored afterward so the interactive session is unchanged.
///
/// `LIST` is comma/space-separated (`ls,df`); a leading `--allow` is accepted for
/// symmetry with the `agsh --allow` launch flag.
fn run_confine_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let args = &invocation.argv[1..];
    // Everything up to the first `--` is the spec (presets, flags, exec-allowlist);
    // the rest is the payload command.
    let dashdash = args.iter().position(|a| a == "--");
    let (spec, command): (&[String], Vec<String>) = match dashdash {
        Some(idx) => (&args[..idx], args[idx + 1..].to_vec()),
        None => (args, Vec::new()),
    };
    if spec.is_empty() && command.is_empty() {
        return Ok(CommandOutcome::captured(
            2,
            Vec::new(),
            b"confine: usage: confine [PRESET] [--FLAG ...] [LIST] [-- COMMAND ...]\n  presets: read-only, workspace, offline\n".to_vec(),
        ));
    }
    let (requested, opts) = match crate::confine::parse_spec(spec) {
        Ok(parsed) => parsed,
        Err(message) => {
            return Ok(CommandOutcome::captured(
                2,
                Vec::new(),
                format!("{message}\n").into_bytes(),
            ))
        }
    };

    if command.is_empty() {
        // A preset is an OS-sandbox of a leaf payload; it needs a `-- COMMAND`.
        if opts.preset != crate::confine::Preset::ExecOnly || opts.net.is_some() {
            return Ok(CommandOutcome::captured(
                2,
                Vec::new(),
                b"confine: presets (read-only/workspace/offline) require '-- COMMAND'\n".to_vec(),
            ));
        }
        // Sticky: confine THIS agsh session (the agsh-routed gate) and propagate
        // to children (env + shims). Governs commands agsh itself runs; not an OS
        // sandbox (a running process can't be retroactively jailed).
        let effective = match state.confine_policy() {
            Some(p) => p.intersect(&requested),
            None => agsh_policy::AllowPolicy::from_names(&requested),
        };
        if let Err(error) = install_confine_shims(state) {
            return Ok(CommandOutcome::captured(
                1,
                Vec::new(),
                format!("confine: cannot install shell shims: {error}\n").into_bytes(),
            ));
        }
        state.set_confine(&requested);
        state.export_var("AGSH_CONFINE", effective.to_list());
        return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
    }

    // Scoped: OS-enforced confine of a leaf payload (kernel-level), or refuse.
    // payload0 is the exact program name; payload_shell re-quotes each argv token
    // so quoting (e.g. `python3 -c '…'`) survives the `/bin/sh -c` wrapper.
    let payload = command
        .iter()
        .map(|t| crate::confine::shell_quote(t))
        .collect::<Vec<_>>()
        .join(" ");
    match crate::confine::plan(state, &requested, &command, &payload, &opts) {
        crate::confine::ConfinePlan::Refuse { message, code } => Ok(CommandOutcome::captured(
            code,
            Vec::new(),
            message.into_bytes(),
        )),
        crate::confine::ConfinePlan::Sandboxed {
            command,
            cleanup,
            explain,
            env_remove,
        } => {
            // The kernel enforces the policy; no shims / AGSH_CONFINE needed.
            if let Some(summary) = explain {
                eprint!("{summary}");
            }
            let outcome = if opts.dry_run {
                eprintln!("confine: would run: {command}");
                Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()))
            } else {
                // Loader injection must be removed before sandbox-exec itself
                // starts. Keep shell-local/readonly values intact and restore
                // only the exported entries when the child returns.
                let removed: Vec<_> = env_remove
                    .iter()
                    .filter_map(|name| {
                        state
                            .take_exported_env(name)
                            .map(|value| (name.clone(), value))
                    })
                    .collect();
                let result = run_shell_source(
                    &command,
                    state,
                    output_mode,
                    capture_outputs,
                    allow_process_replacement,
                );
                for (name, value) in removed {
                    state.restore_exported_env(name, value);
                }
                result
            };
            for path in &cleanup {
                let _ = std::fs::remove_file(path);
                let _ = std::fs::remove_dir_all(path);
            }
            outcome
        }
        crate::confine::ConfinePlan::BestEffort => {
            // Weaker shim layer: route the payload's shells through confined agsh.
            let effective = match state.confine_policy() {
                Some(p) => p.intersect(&requested),
                None => agsh_policy::AllowPolicy::from_names(&requested),
            };
            let prev_confine = state.lookup("AGSH_CONFINE").map(str::to_string);
            let prev_path = state.lookup("PATH").map(str::to_string);
            let prev_shell = state.lookup("SHELL").map(str::to_string);
            if let Err(error) = install_confine_shims(state) {
                return Ok(CommandOutcome::captured(
                    1,
                    Vec::new(),
                    format!("confine: cannot install shell shims: {error}\n").into_bytes(),
                ));
            }
            state.export_var("AGSH_CONFINE", effective.to_list());
            let outcome = run_shell_source(
                &payload,
                state,
                output_mode,
                capture_outputs,
                allow_process_replacement,
            );
            restore_env(state, "AGSH_CONFINE", prev_confine);
            restore_env(state, "PATH", prev_path);
            restore_env(state, "SHELL", prev_shell);
            outcome
        }
    }
}

/// Restore (or unset) an environment variable to a saved prior value.
fn restore_env(state: &mut ShellState, name: &str, prev: Option<String>) {
    match prev {
        Some(value) => state.export_var(name, value),
        None => {
            state.unset(name);
        }
    }
}

fn run_eval_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    if invocation.argv.len() == 1 {
        return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
    }

    run_with_buffered_stdin(state, stdin_data, |state| {
        run_shell_source(
            &invocation.argv[1..].join(" "),
            state,
            output_mode,
            capture_outputs,
            allow_process_replacement,
        )
    })
}

fn run_source_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let Some(path) = invocation.argv.get(1) else {
        return Ok(CommandOutcome::captured(
            2,
            Vec::new(),
            format!("{}: missing file\n", invocation.argv[0]).into_bytes(),
        ));
    };

    let path = PathBuf::from(resolve_shell_path(path, state.cwd()));
    let source = match read_shell_source(&path) {
        Ok(source) => source,
        Err(error) => {
            return Ok(CommandOutcome::captured(
                1,
                Vec::new(),
                format!("{}: {}: {error}\n", invocation.argv[0], path.display()).into_bytes(),
            ));
        }
    };

    let source_args = invocation.argv[2..].to_vec();
    let saved_positionals = if source_args.is_empty() {
        None
    } else {
        let saved = save_positionals(state);
        state.set_positionals(&source_args);
        Some(saved)
    };

    state.enter_source();
    let result = run_with_buffered_stdin(state, stdin_data, |state| {
        run_shell_source(
            &source,
            state,
            output_mode,
            capture_outputs,
            allow_process_replacement,
        )
    });
    state.leave_source();
    if let Some(saved_positionals) = saved_positionals {
        restore_positionals(state, &saved_positionals);
    }
    let mut outcome = result?;
    // `return` inside a sourced script stops sourcing and sets the exit status.
    if let Some(code) = state.take_return() {
        outcome.exit_code = code;
    }
    Ok(outcome)
}

fn read_shell_source(path: &Path) -> io::Result<String> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take((MAX_SHELL_SOURCE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SHELL_SOURCE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("shell source exceeds {MAX_SHELL_SOURCE_BYTES} bytes"),
        ));
    }
    String::from_utf8(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("shell source is not valid UTF-8: {error}"),
        )
    })
}

fn run_exec_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let command_index = if invocation.argv.get(1).is_some_and(|arg| arg == "--") {
        2
    } else {
        1
    };
    if invocation.argv.get(command_index).is_none() {
        let stderr = apply_shell_assignments(&invocation.assignments, state);
        return Ok(CommandOutcome::captured(
            state.last_command_substitution_status(),
            Vec::new(),
            stderr,
        ));
    }

    if capture_outputs || stdin_data.is_some() || !allow_process_replacement {
        return Ok(CommandOutcome::captured(
            126,
            Vec::new(),
            b"exec: process replacement disabled in this execution context\n".to_vec(),
        ));
    }

    let exec_invocation = ExpandedInvocation {
        assignments: invocation.assignments.clone(),
        argv: invocation.argv[command_index..].to_vec(),
        redirections: invocation.redirections.clone(),
    };
    let name = exec_invocation.argv[0].as_str();
    if let Some(policy) = state.confine_policy() {
        if !policy.allows(name) {
            let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
            return Ok(CommandOutcome::captured(
                126,
                Vec::new(),
                format!(
                    "agsh: {base}: not permitted in this confined session (allowed: {})\n",
                    policy.display_list()
                )
                .into_bytes(),
            ));
        }
    }
    let Some(path) = resolve_external_path(&exec_invocation, state, name) else {
        return Ok(CommandOutcome::captured(
            127,
            Vec::new(),
            format!("exec: {name}: command not found\n").into_bytes(),
        ));
    };

    exec_external(&exec_invocation, state, path.as_path())
}

#[cfg(unix)]
fn exec_external(
    invocation: &ExpandedInvocation,
    state: &ShellState,
    command_path: &Path,
) -> Result<CommandOutcome, ShellError> {
    let mut command = Command::new(command_path);
    command.args(&invocation.argv[1..]);
    command.current_dir(state.cwd());
    state.configure_child_env(&mut command);
    for assignment in &invocation.assignments {
        command.env(&assignment.name, &assignment.value);
    }
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let mut stdin_is_piped = false;
    let mut merge_stderr_to_stdout = false;
    let mut merge_stdout_to_stderr = false;
    let mut ordered_sinks = None;
    let mut redirection_context = ExternalRedirectionContext {
        stdin_is_piped: &mut stdin_is_piped,
        merge_stderr_to_stdout: &mut merge_stderr_to_stdout,
        merge_stdout_to_stderr: &mut merge_stdout_to_stderr,
        capture_outputs: false,
        noclobber: state.noclobber(),
        ordered_sinks: &mut ordered_sinks,
    };
    apply_external_redirections(
        &mut command,
        &invocation.redirections,
        &mut redirection_context,
    )?;

    let error = command.exec();
    Err(ShellError::execution(format!(
        "exec: {}: {error}",
        invocation.argv[0]
    )))
}

#[cfg(not(unix))]
fn exec_external(
    _invocation: &ExpandedInvocation,
    _state: &ShellState,
    _command_path: &Path,
) -> Result<CommandOutcome, ShellError> {
    Err(ShellError::unsupported(
        "exec: process replacement is only supported on Unix",
    ))
}

/// `patch <file>`: collect the unified diff from stdin (heredoc redirection via
/// `stdin_data`, or an in-shell pipe / inherited stdin drained line-by-line) and
/// apply it.
fn run_patch_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    stdin_data: Option<&[u8]>,
) -> Result<CommandOutcome, ShellError> {
    let diff = if let Some(bytes) = stdin_data {
        Some(bytes.to_vec())
    } else {
        let mut buf = String::new();
        let mut any = false;
        while let Some(line) = read_one_line(None, state)? {
            any = true;
            buf.push_str(&line);
        }
        any.then_some(buf.into_bytes())
    };
    Ok(crate::agent::patch(
        &invocation.argv[1..],
        state,
        diff.as_deref(),
    ))
}

fn run_read_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    stdin_data: Option<&[u8]>,
) -> Result<CommandOutcome, ShellError> {
    let mut raw = false;
    let mut prompt = None;
    let mut names = Vec::new();
    let mut args = invocation.argv.iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        if arg == "-r" {
            raw = true;
        } else if arg == "-p" {
            let Some(prompt_arg) = args.next() else {
                return Ok(CommandOutcome::captured(
                    2,
                    Vec::new(),
                    b"read: -p requires a prompt\n".to_vec(),
                ));
            };
            prompt = Some(prompt_arg.clone());
        } else if let Some(prompt_arg) = arg.strip_prefix("-p") {
            prompt = Some(prompt_arg.to_string());
        } else if arg.starts_with('-') {
            return Ok(CommandOutcome::captured(
                2,
                Vec::new(),
                format!("read: unsupported option: {arg}\n").into_bytes(),
            ));
        } else if !is_identifier(arg) {
            return Ok(CommandOutcome::captured(
                2,
                Vec::new(),
                format!("read: invalid identifier: {arg}\n").into_bytes(),
            ));
        } else {
            names.push(arg.clone());
        }
    }

    if names.is_empty() {
        names.push("REPLY".to_string());
    }

    if let Some(prompt) = prompt {
        if stdin_data.is_none() && io::stdin().is_terminal() {
            let mut stderr = io::stderr();
            stderr.write_all(prompt.as_bytes())?;
            stderr.flush()?;
        }
    }

    let Some(mut line) = read_logical_line(stdin_data, state, raw)? else {
        return Ok(CommandOutcome::captured(1, Vec::new(), Vec::new()));
    };
    trim_line_ending(&mut line);
    if !raw {
        line = unescape_read_line(&line);
    }
    match assign_read_fields(state, &names, &line) {
        Ok(()) => Ok(CommandOutcome::captured(0, Vec::new(), Vec::new())),
        Err(name) => Ok(CommandOutcome::captured(
            1,
            Vec::new(),
            format!("read: {name}: readonly variable\n").into_bytes(),
        )),
    }
}

fn read_logical_line(
    stdin_data: Option<&[u8]>,
    state: &mut ShellState,
    raw: bool,
) -> Result<Option<String>, ShellError> {
    if raw {
        return read_one_line(stdin_data, state);
    }

    if let Some(input) = stdin_data {
        return read_logical_line_from_buffer(input);
    }

    let mut logical = String::new();
    loop {
        let Some(mut line) = read_one_line(None, state)? else {
            return if logical.is_empty() {
                Ok(None)
            } else {
                Ok(Some(logical))
            };
        };
        let continued = remove_read_continuation(&mut line);
        if logical.len().saturating_add(line.len()) > MAX_READ_LINE_BYTES {
            return Err(ShellError::execution(format!(
                "read input line exceeds {MAX_READ_LINE_BYTES} bytes"
            )));
        }
        logical.push_str(&line);
        if !continued {
            return Ok(Some(logical));
        }
    }
}

fn read_logical_line_from_buffer(input: &[u8]) -> Result<Option<String>, ShellError> {
    if input.is_empty() {
        return Ok(None);
    }

    let mut logical = String::new();
    let mut start = 0;
    while start < input.len() {
        let end = input[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(input.len(), |index| start + index + 1);
        let mut line = String::from_utf8_lossy(&input[start..end]).to_string();
        start = end;

        let continued = remove_read_continuation(&mut line);
        if logical.len().saturating_add(line.len()) > MAX_READ_LINE_BYTES {
            return Err(ShellError::execution(format!(
                "read input line exceeds {MAX_READ_LINE_BYTES} bytes"
            )));
        }
        logical.push_str(&line);
        if !continued {
            break;
        }
    }

    Ok(Some(logical))
}

fn read_one_line(
    stdin_data: Option<&[u8]>,
    state: &mut ShellState,
) -> Result<Option<String>, ShellError> {
    if let Some(input) = stdin_data {
        if input.is_empty() {
            return Ok(None);
        }
        let end = input
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(input.len(), |index| index + 1);
        if end > MAX_READ_LINE_BYTES {
            return Err(ShellError::execution(format!(
                "read input line exceeds {MAX_READ_LINE_BYTES} bytes"
            )));
        }
        return Ok(Some(String::from_utf8_lossy(&input[..end]).to_string()));
    }

    if let Some(line) = state.read_shell_stdin_line() {
        return Ok(line?);
    }

    let stdin = io::stdin();
    read_bounded_line(&mut stdin.lock(), MAX_READ_LINE_BYTES).map_err(ShellError::from)
}

fn read_bounded_line(reader: &mut impl BufRead, limit: usize) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Ok(Some(String::from_utf8_lossy(&bytes).to_string()))
            };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if bytes.len().saturating_add(take) > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("read input line exceeds {limit} bytes"),
            ));
        }
        let ended = available[take - 1] == b'\n';
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if ended {
            return Ok(Some(String::from_utf8_lossy(&bytes).to_string()));
        }
    }
}

fn remove_read_continuation(line: &mut String) -> bool {
    let Some(ending_start) = line_ending_start(line) else {
        return false;
    };

    let backslashes = line[..ending_start]
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'\\')
        .count();
    if backslashes % 2 == 0 {
        return false;
    }

    line.replace_range(ending_start - 1.., "");
    true
}

fn line_ending_start(line: &str) -> Option<usize> {
    let mut ending_start = line.len().checked_sub(1)?;
    if !line.ends_with('\n') {
        return None;
    }
    if line[..ending_start].ends_with('\r') {
        ending_start -= 1;
    }
    Some(ending_start)
}

fn trim_line_ending(line: &mut String) {
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
}

fn unescape_read_line(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn assign_read_fields(state: &mut ShellState, names: &[String], line: &str) -> Result<(), String> {
    if names.len() == 1 {
        return state
            .try_set_var(&names[0], line)
            .then_some(())
            .ok_or_else(|| names[0].clone());
    }

    let ifs = state.lookup("IFS").unwrap_or(" \t\n").to_string();
    let values = split_read_fields(line, names.len(), &ifs);
    for (name, value) in names.iter().zip(values) {
        if !state.try_set_var(name, value) {
            return Err(name.clone());
        }
    }
    Ok(())
}

fn split_read_fields(line: &str, count: usize, ifs: &str) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }
    if count == 1 {
        return vec![line.to_string()];
    }
    if ifs.is_empty() {
        let mut values = Vec::with_capacity(count);
        values.push(line.to_string());
        values.resize(count, String::new());
        return values;
    }

    let mut values = Vec::with_capacity(count);
    let mut rest = line;
    for _ in 0..count - 1 {
        let (field, next_rest) = read_next_field(rest, ifs);
        values.push(field);
        rest = next_rest;
    }
    values.push(trim_read_remainder(rest, ifs).to_string());
    values
}

fn read_next_field<'a>(input: &'a str, ifs: &str) -> (String, &'a str) {
    let input = trim_start_ifs_whitespace(input, ifs);
    if input.is_empty() {
        return (String::new(), "");
    }

    for (index, ch) in input.char_indices() {
        if !is_ifs_delimiter(ch, ifs) {
            continue;
        }

        if is_ifs_whitespace(ch, ifs) {
            let after_ws = skip_ifs_whitespace(input, index, ifs);
            if let Some((_, delimiter)) = first_char(&input[after_ws..]) {
                if is_ifs_non_whitespace_delimiter(delimiter, ifs) {
                    let rest_start =
                        skip_ifs_whitespace(input, after_ws + delimiter.len_utf8(), ifs);
                    return (input[..index].to_string(), &input[rest_start..]);
                }
            }
            return (input[..index].to_string(), &input[after_ws..]);
        }

        let field_end = trim_end_ifs_whitespace_index(&input[..index], ifs);
        let rest_start = skip_ifs_whitespace(input, index + ch.len_utf8(), ifs);
        return (input[..field_end].to_string(), &input[rest_start..]);
    }

    (trim_end_ifs_whitespace(input, ifs).to_string(), "")
}

fn trim_read_remainder<'a>(input: &'a str, ifs: &str) -> &'a str {
    let input = trim_start_ifs_whitespace(input, ifs);
    let input = trim_end_ifs_whitespace(input, ifs);
    if let Some(index) = trailing_single_non_whitespace_ifs(input, ifs) {
        trim_end_ifs_whitespace(&input[..index], ifs)
    } else {
        input
    }
}

fn skip_ifs_whitespace(input: &str, start: usize, ifs: &str) -> usize {
    let mut next = start;
    for (offset, ch) in input[start..].char_indices() {
        if !is_ifs_whitespace(ch, ifs) {
            return start + offset;
        }
        next = start + offset + ch.len_utf8();
    }
    next
}

fn trim_start_ifs_whitespace<'a>(input: &'a str, ifs: &str) -> &'a str {
    &input[skip_ifs_whitespace(input, 0, ifs)..]
}

fn trim_end_ifs_whitespace<'a>(input: &'a str, ifs: &str) -> &'a str {
    &input[..trim_end_ifs_whitespace_index(input, ifs)]
}

fn trim_end_ifs_whitespace_index(input: &str, ifs: &str) -> usize {
    let mut end = input.len();
    for (index, ch) in input.char_indices().rev() {
        if !is_ifs_whitespace(ch, ifs) {
            break;
        }
        end = index;
    }
    end
}

fn trailing_single_non_whitespace_ifs(input: &str, ifs: &str) -> Option<usize> {
    let (last_index, last) = input.char_indices().next_back()?;
    if !is_ifs_non_whitespace_delimiter(last, ifs) {
        return None;
    }
    let previous = input[..last_index].chars().next_back();
    if previous.is_some_and(|ch| is_ifs_non_whitespace_delimiter(ch, ifs)) {
        None
    } else {
        Some(last_index)
    }
}

fn first_char(input: &str) -> Option<(usize, char)> {
    input.char_indices().next()
}

fn is_ifs_non_whitespace_delimiter(ch: char, ifs: &str) -> bool {
    is_ifs_delimiter(ch, ifs) && !is_ifs_whitespace(ch, ifs)
}

fn run_shell_source(
    source: &str,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let nested_options = ExecutionOptions {
        output_mode: if capture_outputs {
            OutputMode::Clean
        } else {
            output_mode
        },
        allow_process_replacement,
    };
    let mut final_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());

    for line in source_logical_lines(source)? {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let graph = parse_line(&line)?;
        let mut executor = Executor::new().with_stdout_flush(stream_raw_to_parent());
        let mut outcome = executor.run_graph(&graph, state, &nested_options)?;
        final_outcome.exit_code = outcome.exit_code;
        final_outcome.append_streams(&mut outcome)?;

        if state.should_exit()
            || state.loop_control_requested()
            || state.return_requested()
            || state.interrupted()
            || state.stream_pipe_closed()
        {
            break;
        }

        if state.errexit() && outcome.exit_code != 0 {
            break;
        }
    }

    Ok(final_outcome)
}

fn source_logical_lines(source: &str) -> Result<Vec<String>, ShellError> {
    let physical_lines = source_physical_logical_lines(source);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut function_body_depth = 0usize;
    let mut if_block_depth = 0usize;
    let mut while_block_depth = 0usize;
    let mut for_block_depth = 0usize;
    let mut case_block_depth = 0usize;

    for line in physical_lines {
        let trimmed_end = line.trim_end_matches(['\r', '\n']);
        let trimmed = trimmed_end.trim();

        if function_body_depth > 0 {
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                current.push_str("; ");
                current.push_str(trimmed_end);
                function_body_depth =
                    function_body_depth.saturating_add_signed(brace_delta(trimmed_end)?);
            }
            if function_body_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
            continue;
        }

        if if_block_depth > 0 {
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                current.push_str("; ");
                current.push_str(trimmed_end);
                if_block_depth = if_block_depth.saturating_add_signed(if_delta(trimmed_end)?);
            }
            if if_block_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
            continue;
        }

        if while_block_depth > 0 {
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                current.push_str("; ");
                current.push_str(trimmed_end);
                while_block_depth =
                    while_block_depth.saturating_add_signed(while_delta(trimmed_end)?);
            }
            if while_block_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
            continue;
        }

        if for_block_depth > 0 {
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                current.push_str("; ");
                current.push_str(trimmed_end);
                for_block_depth = for_block_depth.saturating_add_signed(done_delta(trimmed_end)?);
            }
            if for_block_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
            continue;
        }

        if case_block_depth > 0 {
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                current.push_str("; ");
                current.push_str(trimmed_end);
                case_block_depth = case_block_depth.saturating_add_signed(case_delta(trimmed_end)?);
            }
            if case_block_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
            continue;
        }

        if let Some(depth) = function_definition_depth(trimmed_end)? {
            current.push_str(trimmed_end);
            function_body_depth = depth;
            if function_body_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
        } else if let Some(depth) = if_block_depth_for_line(trimmed_end)? {
            current.push_str(trimmed_end);
            if_block_depth = depth;
            if if_block_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
        } else if let Some(depth) = while_block_depth_for_line(trimmed_end)? {
            current.push_str(trimmed_end);
            while_block_depth = depth;
            if while_block_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
        } else if let Some(depth) = for_block_depth_for_line(trimmed_end)? {
            current.push_str(trimmed_end);
            for_block_depth = depth;
            if for_block_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
        } else if let Some(depth) = case_block_depth_for_line(trimmed_end)? {
            current.push_str(trimmed_end);
            case_block_depth = depth;
            if case_block_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
        } else {
            lines.push(line);
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }

    Ok(lines)
}

fn source_physical_logical_lines(source: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();

    for physical in source.split_inclusive('\n') {
        let mut line = physical.to_string();
        let continued = remove_read_continuation(&mut line);
        current.push_str(&line);
        if !continued {
            lines.push(std::mem::take(&mut current));
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }

    lines
}

fn function_definition_depth(line: &str) -> Result<Option<usize>, ShellError> {
    let tokens = lex(line)?;
    let Some(open_index) = function_definition_open_index(&tokens) else {
        return Ok(None);
    };

    let mut depth = 0usize;
    for token in &tokens[open_index..] {
        update_brace_depth(token, &mut depth);
    }
    Ok(Some(depth))
}

fn function_definition_open_index(tokens: &[agsh_core::lexer::Token]) -> Option<usize> {
    for index in 0..tokens.len() {
        if tokens[index].text != "{" || tokens[index].quote != QuoteKind::None {
            continue;
        }
        if index == 1 && tokens[0].text.ends_with("()") && tokens[0].quote == QuoteKind::None {
            return Some(index);
        }
        if index == 2
            && tokens[0].text == "function"
            && tokens[0].quote == QuoteKind::None
            && tokens[1].quote == QuoteKind::None
        {
            return Some(index);
        }
    }
    None
}

fn if_block_depth_for_line(line: &str) -> Result<Option<usize>, ShellError> {
    let tokens = lex(line)?;
    if !tokens
        .first()
        .is_some_and(|token| token.text == "if" && token.quote == QuoteKind::None)
    {
        return Ok(None);
    }

    let mut depth = 0usize;
    for index in 0..tokens.len() {
        update_if_depth(
            &tokens[index],
            &mut depth,
            is_source_reserved_command_position(&tokens, index),
        );
    }
    Ok(Some(depth))
}

fn while_block_depth_for_line(line: &str) -> Result<Option<usize>, ShellError> {
    let tokens = lex(line)?;
    if !tokens.first().is_some_and(|token| {
        matches!(token.text.as_str(), "while" | "until") && token.quote == QuoteKind::None
    }) {
        return Ok(None);
    }

    let mut depth = 0usize;
    for index in 0..tokens.len() {
        update_while_depth(
            &tokens[index],
            &mut depth,
            is_source_reserved_command_position(&tokens, index),
        );
    }
    Ok(Some(depth))
}

fn for_block_depth_for_line(line: &str) -> Result<Option<usize>, ShellError> {
    let tokens = lex(line)?;
    if !tokens.first().is_some_and(|token| {
        matches!(token.text.as_str(), "for" | "select") && token.quote == QuoteKind::None
    }) {
        return Ok(None);
    }

    let mut depth = 0usize;
    for index in 0..tokens.len() {
        update_done_block_depth(
            &tokens[index],
            &mut depth,
            is_source_reserved_command_position(&tokens, index),
        );
    }
    Ok(Some(depth))
}

fn case_block_depth_for_line(line: &str) -> Result<Option<usize>, ShellError> {
    let tokens = lex(line)?;
    if !tokens
        .first()
        .is_some_and(|token| token.text == "case" && token.quote == QuoteKind::None)
    {
        return Ok(None);
    }

    let mut depth = 0usize;
    for index in 0..tokens.len() {
        update_case_depth(
            &tokens[index],
            &mut depth,
            is_source_reserved_command_position(&tokens, index),
        );
    }
    Ok(Some(depth))
}

fn if_delta(line: &str) -> Result<isize, ShellError> {
    let mut delta = 0isize;
    let tokens = lex(line)?;
    for index in 0..tokens.len() {
        let token = &tokens[index];
        if token.quote != QuoteKind::None {
            continue;
        }
        if !is_source_reserved_command_position(&tokens, index) {
            continue;
        }
        match token.text.as_str() {
            "if" => delta += 1,
            "fi" => delta -= 1,
            _ => {}
        }
    }
    Ok(delta)
}

fn case_delta(line: &str) -> Result<isize, ShellError> {
    let mut delta = 0isize;
    let tokens = lex(line)?;
    for index in 0..tokens.len() {
        let token = &tokens[index];
        if token.quote != QuoteKind::None {
            continue;
        }
        if !is_source_reserved_command_position(&tokens, index) {
            continue;
        }
        match token.text.as_str() {
            "case" => delta += 1,
            "esac" => delta -= 1,
            _ => {}
        }
    }
    Ok(delta)
}

fn while_delta(line: &str) -> Result<isize, ShellError> {
    done_delta(line)
}

fn done_delta(line: &str) -> Result<isize, ShellError> {
    let mut delta = 0isize;
    let tokens = lex(line)?;
    for index in 0..tokens.len() {
        let token = &tokens[index];
        if token.quote != QuoteKind::None {
            continue;
        }
        if !is_source_reserved_command_position(&tokens, index) {
            continue;
        }
        match token.text.as_str() {
            "while" | "until" | "for" | "select" => delta += 1,
            "done" => delta -= 1,
            _ => {}
        }
    }
    Ok(delta)
}

fn brace_delta(line: &str) -> Result<isize, ShellError> {
    let mut delta = 0isize;
    for token in lex(line)? {
        if token.quote != QuoteKind::None {
            continue;
        }
        match token.text.as_str() {
            "{" => delta += 1,
            "}" => delta -= 1,
            _ => {}
        }
    }
    Ok(delta)
}

fn update_if_depth(token: &agsh_core::lexer::Token, depth: &mut usize, command_position: bool) {
    if token.quote != QuoteKind::None {
        return;
    }
    match token.text.as_str() {
        "if" if command_position => *depth += 1,
        "fi" if command_position => *depth = depth.saturating_sub(1),
        _ => {}
    }
}

fn update_while_depth(token: &agsh_core::lexer::Token, depth: &mut usize, command_position: bool) {
    update_done_block_depth(token, depth, command_position);
}

fn update_done_block_depth(
    token: &agsh_core::lexer::Token,
    depth: &mut usize,
    command_position: bool,
) {
    if token.quote != QuoteKind::None {
        return;
    }
    match token.text.as_str() {
        "while" | "until" | "for" | "select" if command_position => *depth += 1,
        "done" if command_position => *depth = depth.saturating_sub(1),
        _ => {}
    }
}

fn update_case_depth(token: &agsh_core::lexer::Token, depth: &mut usize, command_position: bool) {
    if token.quote != QuoteKind::None {
        return;
    }
    match token.text.as_str() {
        "case" if command_position => *depth += 1,
        "esac" if command_position => *depth = depth.saturating_sub(1),
        _ => {}
    }
}

fn is_source_reserved_command_position(tokens: &[agsh_core::lexer::Token], index: usize) -> bool {
    let Some(previous) = tokens[..index]
        .iter()
        .rev()
        .find(|token| token.quote == QuoteKind::None)
    else {
        return true;
    };

    matches!(
        previous.text.as_str(),
        ";" | "&&" | "||" | "|" | "then" | "else" | "elif" | "do"
    )
}

fn update_brace_depth(token: &agsh_core::lexer::Token, depth: &mut usize) {
    if token.quote != QuoteKind::None {
        return;
    }
    match token.text.as_str() {
        "{" => *depth += 1,
        "}" => *depth = depth.saturating_sub(1),
        _ => {}
    }
}

fn resolve_external_path(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    name: &str,
) -> Option<PathBuf> {
    let path_value = lookup_path_value(invocation, state);
    resolve_external_path_with(invocation, state, name, &path_value)
}

fn resolve_external_path_with(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    name: &str,
    path_value: &str,
) -> Option<PathBuf> {
    // Confinement backstop: a denied external command must not resolve to a path,
    // even via routes that skip preflight (e.g. `exec`). The friendly deny
    // message comes from preflight_resolved_invocation; this is the hard net.
    if let Some(policy) = state.confine_policy() {
        if !policy.allows(name) {
            return None;
        }
    }

    let uses_temporary_path = invocation
        .assignments
        .iter()
        .any(|assignment| assignment.name == "PATH");

    if !uses_temporary_path && !name.contains('/') {
        if let Some(path) = state.cached_path_lookup(path_value, name) {
            return Some(path);
        }
    }

    let resolver = Resolver::default();
    let path = match resolver.resolve_external_only(name, Some(path_value)) {
        Some(CommandResolution::External(path)) => path,
        _ => return None,
    };

    if !uses_temporary_path && !name.contains('/') {
        state.cache_path_lookup(path_value, name, path.clone());
    }
    Some(path)
}

fn lookup_path_value(invocation: &ExpandedInvocation, state: &ShellState) -> String {
    invocation
        .assignments
        .iter()
        .rev()
        .find(|assignment| assignment.name == "PATH")
        .map(|assignment| assignment.value.as_str())
        .or_else(|| state.lookup("PATH"))
        .unwrap_or_default()
        .to_string()
}

fn expand_alias_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
) -> Result<Option<ExpandedInvocation>, ShellError> {
    let Some(name) = invocation.argv.first() else {
        return Ok(None);
    };
    let Some(alias_value) = state.alias(name).map(str::to_string) else {
        return Ok(None);
    };

    let graph = parse_line(&alias_value)?;
    if graph.pipeline.commands.len() != 1 {
        return Err(ShellError::unsupported(format!(
            "{name}: aliases that expand to pipelines are not implemented"
        )));
    }

    let mut expanded = expand_invocation(&graph.pipeline.commands[0], state)?;
    if expanded.argv.is_empty() {
        return Ok(None);
    }

    expanded
        .assignments
        .splice(0..0, invocation.assignments.clone());
    expanded
        .argv
        .extend(invocation.argv.iter().skip(1).cloned());
    expanded
        .redirections
        .extend(invocation.redirections.clone());
    Ok(Some(expanded))
}

fn expand_abbreviation_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
) -> Result<Option<ExpandedInvocation>, ShellError> {
    let Some(name) = invocation.argv.first() else {
        return Ok(None);
    };
    let Some(abbreviation_value) = state.abbreviation(name).map(str::to_string) else {
        return Ok(None);
    };

    let graph = parse_line(&abbreviation_value)?;
    if graph.pipeline.commands.len() != 1 {
        return Err(ShellError::unsupported(format!(
            "{name}: abbreviations that expand to pipelines are not implemented"
        )));
    }

    let mut expanded = expand_invocation(&graph.pipeline.commands[0], state)?;
    if expanded.argv.is_empty() {
        return Ok(None);
    }

    expanded
        .assignments
        .splice(0..0, invocation.assignments.clone());
    expanded
        .argv
        .extend(invocation.argv.iter().skip(1).cloned());
    expanded
        .redirections
        .extend(invocation.redirections.clone());
    Ok(Some(expanded))
}

fn strip_wrapper(
    invocation: &ExpandedInvocation,
    wrapper: &str,
) -> Result<ExpandedInvocation, ShellError> {
    if invocation.argv.len() < 2 {
        return Err(ShellError::execution(format!("{wrapper}: missing command")));
    }
    let mut stripped = invocation.clone();
    stripped.argv.remove(0);
    if wrapper == "command" && stripped.argv.first().is_some_and(|arg| arg == "--") {
        stripped.argv.remove(0);
        if stripped.argv.is_empty() {
            return Err(ShellError::execution("command: missing command"));
        }
    }
    Ok(stripped)
}

#[derive(Debug, Clone, Copy)]
struct CommandOptions {
    default_path: bool,
    describe: bool,
    unsupported_option: Option<usize>,
    command_index: usize,
}

fn parse_command_options(invocation: &ExpandedInvocation) -> Result<CommandOptions, ShellError> {
    let mut options = CommandOptions {
        default_path: false,
        describe: false,
        unsupported_option: None,
        command_index: 1,
    };

    while let Some(arg) = invocation.argv.get(options.command_index) {
        match arg.as_str() {
            "--" => {
                options.command_index += 1;
                break;
            }
            "-p" => options.default_path = true,
            "-v" | "-V" => options.describe = true,
            other if other.starts_with('-') => {
                options.unsupported_option = Some(options.command_index);
                break;
            }
            _ => break,
        }
        options.command_index += 1;
    }

    if !options.describe
        && options.unsupported_option.is_none()
        && invocation.argv.get(options.command_index).is_none()
    {
        return Err(ShellError::execution("command: missing command"));
    }
    Ok(options)
}

fn strip_command_wrapper(
    invocation: &ExpandedInvocation,
    command_index: usize,
) -> Result<ExpandedInvocation, ShellError> {
    let mut stripped = invocation.clone();
    stripped.argv.drain(0..command_index);
    if stripped.argv.is_empty() {
        return Err(ShellError::execution("command: missing command"));
    }
    Ok(stripped)
}

fn preflight_buffered_pipeline_invocations(
    commands: &[ExpandedInvocation],
    state: &mut ShellState,
) -> Result<(), ShellError> {
    for command in commands {
        preflight_invocation(command, state, LookupMode::Normal, 0)?;
    }
    Ok(())
}

fn preflight_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    lookup_mode: LookupMode,
    alias_depth: usize,
) -> Result<(), ShellError> {
    let Some(name) = invocation.argv.first() else {
        return Ok(());
    };

    match name.as_str() {
        "command" if lookup_mode == LookupMode::Normal => {
            let command_options = parse_command_options(invocation)?;
            if command_options.describe || command_options.unsupported_option.is_some() {
                return Ok(());
            }
            let wrapped = strip_command_wrapper(invocation, command_options.command_index)?;
            preflight_invocation(
                &wrapped,
                state,
                if command_options.default_path {
                    LookupMode::DefaultPath
                } else {
                    LookupMode::BypassAliases
                },
                alias_depth,
            )
        }
        "external" if lookup_mode == LookupMode::Normal => {
            let wrapped = strip_wrapper(invocation, "external")?;
            preflight_invocation(&wrapped, state, LookupMode::ExternalOnly, alias_depth)
        }
        "builtin" if lookup_mode == LookupMode::Normal => {
            let wrapped = strip_wrapper(invocation, "builtin")?;
            preflight_invocation(&wrapped, state, LookupMode::BuiltinOnly, alias_depth)
        }
        _ if lookup_mode == LookupMode::Normal => {
            if state.function(name).is_some() {
                return Ok(());
            }

            if let Some(expanded) = expand_alias_invocation(invocation, state)? {
                if alias_depth >= MAX_ALIAS_EXPANSIONS {
                    return Err(ShellError::execution(format!(
                        "{name}: alias expansion limit exceeded"
                    )));
                }
                return preflight_invocation(&expanded, state, lookup_mode, alias_depth + 1);
            }
            if let Some(expanded) = expand_abbreviation_invocation(invocation, state)? {
                if alias_depth >= MAX_ALIAS_EXPANSIONS {
                    return Err(ShellError::execution(format!(
                        "{name}: abbreviation expansion limit exceeded"
                    )));
                }
                return preflight_invocation(&expanded, state, lookup_mode, alias_depth + 1);
            }
            preflight_resolved_invocation(invocation, state, lookup_mode)
        }
        _ => preflight_resolved_invocation(invocation, state, lookup_mode),
    }
}

fn preflight_resolved_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    lookup_mode: LookupMode,
) -> Result<(), ShellError> {
    let Some(name) = invocation.argv.first().map(String::as_str) else {
        return Ok(());
    };

    // Confinement gate: in a confined session, deny any external command not on
    // the allowlist. Builtins are exempt here — they are the shell's own surface
    // and funnel any external targets (exec/command/source) back through the
    // gated resolver below. eval/subshell/`$(…)` re-enter this preflight, so
    // they are covered too.
    if let Some(policy) = state.confine_policy() {
        if !is_builtin(name) && !policy.allows(name) {
            let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
            return Err(ShellError::denied(format!(
                "{base}: not permitted in this confined session (allowed: {})",
                policy.display_list()
            )));
        }
    }

    match lookup_mode {
        LookupMode::BuiltinOnly => {
            if is_builtin(name) {
                Ok(())
            } else {
                Err(ShellError::not_found(format!("{name}: builtin not found")))
            }
        }
        LookupMode::ExternalOnly => {
            if resolve_external_path(invocation, state, name).is_some() {
                Ok(())
            } else {
                Err(ShellError::not_found(format!(
                    "{name}: external command not found"
                )))
            }
        }
        LookupMode::Normal | LookupMode::BypassAliases | LookupMode::DefaultPath => {
            if is_builtin(name) {
                return Ok(());
            }

            let path = if lookup_mode == LookupMode::DefaultPath {
                resolve_external_path_with(invocation, state, name, DEFAULT_COMMAND_PATH)
            } else {
                resolve_external_path(invocation, state, name)
            };
            if path.is_some() {
                Ok(())
            } else {
                Err(ShellError::not_found(format!("{name}: command not found")))
            }
        }
    }
}

fn to_command_invocation(invocation: &ExpandedInvocation) -> CommandInvocation {
    CommandInvocation::new(
        invocation.assignments.clone(),
        invocation.argv.clone(),
        vec![QuoteKind::None; invocation.argv.len()],
        invocation
            .argv
            .iter()
            .map(|arg| vec![WordSegment::new(arg.clone(), QuoteKind::None)])
            .collect(),
        Vec::new(),
        None,
    )
}

/// Build a compaction context from the shell state and token config: shorten
/// the user's home and the working directory, redact the values of configured
/// secret env vars, apply the configured budget, and attach any `[[compactor]]`
/// that matches this command's argv.
fn compaction_context(state: &ShellState, argv: &[String]) -> CompactionContext {
    let config = state.output_config();
    let home = state.lookup("HOME").map(str::to_string);
    let workspace = Some(state.cwd().display().to_string());
    let mut secret_names = config
        .security
        .redact_env_names
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    for name in state.vars().keys().chain(state.exported_env().keys()) {
        if agsh_output::is_sensitive_env_name(name) {
            secret_names.insert(name.clone());
        }
    }
    let literal_secrets = secret_names
        .iter()
        .filter_map(|name| state.lookup(name).map(str::to_string))
        .filter(|value| value.len() >= 4)
        .collect();
    CompactionContext {
        normalize: config.normalize_options(home, workspace),
        redact: config.redact_options(literal_secrets),
        budget: config.budget_options(),
        compactor: config.matching_compactor(argv).cloned(),
    }
}

/// Evaluate a `[[ ... ]]` conditional. `args` are the already-expanded operands
/// after `[[` (no glob/word-split was applied), including a trailing `]]`.
/// Supports `&&`/`||`/`!`/`( )`, unary file/string tests, integer comparisons,
/// `==`/`!=` glob patterns, `=~` regex, and `<`/`>` string ordering.
pub(crate) fn eval_double_bracket(args: &[String], state: &mut ShellState) -> CommandOutcome {
    let mut toks: Vec<&str> = args.iter().map(String::as_str).collect();
    if toks.last() == Some(&"]]") {
        toks.pop();
    }
    let mut parser = DoubleBracket {
        toks: &toks,
        pos: 0,
        cwd: state.cwd().to_path_buf(),
        rematch: None,
    };
    let outcome = match parser.or_expr() {
        Ok(value) if parser.pos == toks.len() => {
            CommandOutcome::captured(i32::from(!value), Vec::new(), Vec::new())
        }
        Ok(_) => cond_error(format!("[[: unexpected token near `{}`", toks[parser.pos])),
        Err(msg) => cond_error(msg),
    };
    // Populate $BASH_REMATCH if a `=~` was evaluated.
    if let Some(groups) = parser.rematch {
        state.set_array("BASH_REMATCH", groups, false);
    }
    outcome
}

fn cond_error(msg: String) -> CommandOutcome {
    CommandOutcome::captured(2, Vec::new(), format!("{msg}\n").into_bytes())
}

struct DoubleBracket<'a> {
    toks: &'a [&'a str],
    pos: usize,
    cwd: std::path::PathBuf,
    /// Captures from the last `=~` match, written to `$BASH_REMATCH` afterward.
    rematch: Option<Vec<String>>,
}

impl<'a> DoubleBracket<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.toks.get(self.pos).copied()
    }
    fn advance(&mut self) -> Option<&'a str> {
        let t = self.toks.get(self.pos).copied();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    fn or_expr(&mut self) -> Result<bool, String> {
        let mut value = self.and_expr()?;
        while self.peek() == Some("||") {
            self.pos += 1;
            let rhs = self.and_expr()?;
            value = value || rhs;
        }
        Ok(value)
    }
    fn and_expr(&mut self) -> Result<bool, String> {
        let mut value = self.unary()?;
        while self.peek() == Some("&&") {
            self.pos += 1;
            let rhs = self.unary()?;
            value = value && rhs;
        }
        Ok(value)
    }
    fn unary(&mut self) -> Result<bool, String> {
        match self.peek() {
            Some("!") => {
                self.pos += 1;
                Ok(!self.unary()?)
            }
            Some("(") => {
                self.pos += 1;
                let value = self.or_expr()?;
                if self.advance() != Some(")") {
                    return Err("[[: expected `)`".to_string());
                }
                Ok(value)
            }
            _ => self.primary(),
        }
    }
    fn primary(&mut self) -> Result<bool, String> {
        let tok = self.peek().ok_or("[[: unexpected end of expression")?;
        if is_unary_cond_op(tok) {
            self.pos += 1;
            let operand = self
                .advance()
                .ok_or_else(|| format!("[[: {tok}: operand expected"))?;
            return Ok(test_bool(
                &[tok.to_string(), operand.to_string()],
                &self.cwd,
            ));
        }
        let a = self.advance().expect("peeked Some");
        if let Some(op) = self.peek() {
            if is_binary_cond_op(op) {
                self.pos += 1;
                let b = self
                    .advance()
                    .ok_or_else(|| format!("[[: {op}: operand expected"))?;
                return self.binary(a, op, b);
            }
        }
        // A lone word is true when non-empty (like `[ word ]`).
        Ok(!a.is_empty())
    }
    fn binary(&mut self, a: &str, op: &str, b: &str) -> Result<bool, String> {
        match op {
            "==" | "=" => Ok(glob_match_bytes(b.as_bytes(), a.as_bytes())),
            "!=" => Ok(!glob_match_bytes(b.as_bytes(), a.as_bytes())),
            "=~" => {
                let re = regex::Regex::new(b).map_err(|_| format!("[[: `{b}`: invalid regex"))?;
                match re.captures(a) {
                    // $BASH_REMATCH: [0]=whole match, [1..]=capture groups.
                    Some(caps) => {
                        self.rematch = Some(
                            caps.iter()
                                .map(|m| m.map(|x| x.as_str().to_string()).unwrap_or_default())
                                .collect(),
                        );
                        Ok(true)
                    }
                    None => {
                        self.rematch = Some(Vec::new());
                        Ok(false)
                    }
                }
            }
            "<" => Ok(a < b),
            ">" => Ok(a > b),
            _ => Ok(test_bool(
                &[a.to_string(), op.to_string(), b.to_string()],
                &self.cwd,
            )),
        }
    }
}

fn is_unary_cond_op(tok: &str) -> bool {
    matches!(
        tok,
        "-f" | "-d"
            | "-e"
            | "-r"
            | "-w"
            | "-x"
            | "-s"
            | "-L"
            | "-h"
            | "-b"
            | "-c"
            | "-p"
            | "-S"
            | "-z"
            | "-n"
            | "-t"
            | "-g"
            | "-u"
            | "-k"
            | "-O"
            | "-G"
            | "-N"
    )
}

fn is_binary_cond_op(tok: &str) -> bool {
    matches!(
        tok,
        "==" | "="
            | "!="
            | "=~"
            | "<"
            | ">"
            | "-eq"
            | "-ne"
            | "-lt"
            | "-le"
            | "-gt"
            | "-ge"
            | "-nt"
            | "-ot"
            | "-ef"
    )
}

fn test_bool(args: &[String], cwd: &std::path::Path) -> bool {
    crate::builtins::builtin_test(args, "[[", cwd).exit_code == 0
}

/// The argv of the first command in a graph, used to classify and match the
/// command for observation rendering. Falls back to the raw source line.
fn graph_primary_argv(graph: &CommandGraph) -> Vec<String> {
    graph
        .list
        .items
        .first()
        .and_then(|item| item.pipeline.commands.first())
        .map(|cmd| cmd.argv.clone())
        .filter(|argv| !argv.is_empty())
        .unwrap_or_else(|| vec![graph.source.clone()])
}

/// Build a `rich` observation: render the captured stdout by detected type for
/// human display (only on a TTY); the raw bytes remain available via the trace
/// ref and still flow to pipes/redirects (handled separately). stderr appended.
fn rich_observation(
    state: &ShellState,
    _cmd_id: &agsh_core::CommandId,
    argv: &[String],
    stdout: &[u8],
    stderr: &[u8],
    raw: &RawStreamRef,
) -> Option<OutputObservation> {
    use std::io::IsTerminal;
    // Rich rendering is a human-display transform only. When stdout is a pipe or
    // redirect, emit NO observation so the caller writes the raw bytes verbatim —
    // otherwise a lossy UTF-8 round-trip would corrupt images/binaries (e.g.
    // `view image.png > copy.png`).
    if !std::io::stdout().is_terminal() {
        return None;
    }
    let theme = state.theme();
    let mut display = agsh_render::render(
        stdout,
        rich_hint(argv).as_deref(),
        &theme,
        rich_term_width(),
    );
    if !stderr.is_empty() {
        display.push_str(&String::from_utf8_lossy(stderr));
    }
    Some(finish_rich_observation(display, raw))
}

fn finish_rich_observation(display: String, raw: &RawStreamRef) -> OutputObservation {
    finalize_trace_status(
        OutputMode::Rich,
        raw,
        OutputObservation {
            token_estimate: agsh_output::estimate_tokens(&display),
            display,
            raw: Some(raw.clone()),
        },
    )
}

/// A filename hint for type detection: the last path-like argument with an
/// extension (helps disambiguate markdown, whose content is ambiguous).
fn rich_hint(argv: &[String]) -> Option<String> {
    argv.iter()
        .skip(1)
        .rfind(|a| !a.starts_with('-') && a.contains('.'))
        .cloned()
}

/// Terminal width for rich rendering (columns), defaulting to 100.
fn rich_term_width() -> usize {
    rustix::termios::tcgetwinsize(std::io::stdout())
        .map(|w| w.ws_col as usize)
        .ok()
        .filter(|&c| c > 0)
        .unwrap_or(100)
}

/// PTY broker: run an external command connected to a pseudo-terminal so it
/// sees a TTY on its stdio (e.g. for tools that change behavior under `isatty`).
/// The controller side is drained into the captured output.
fn run_under_pty(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
) -> Result<CommandOutcome, ShellError> {
    use rustix::fs::{Mode, OFlags};
    use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};

    let name = invocation.argv[0].as_str();
    let Some(path) = resolve_external_path(invocation, state, name) else {
        return Ok(command_not_found_outcome(name, state));
    };

    fn pty_err(label: &str, e: rustix::io::Errno) -> ShellError {
        ShellError::execution(format!("pty: {label}: {e}"))
    }
    let controller =
        openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).map_err(|e| pty_err("openpt", e))?;
    // CLOEXEC, or the controller fd leaks into the child (rustix, unlike std,
    // does not set it) — a child holding its own PTY controller never EOFs it.
    rustix::io::fcntl_setfd(&controller, rustix::io::FdFlags::CLOEXEC)
        .map_err(|e| pty_err("cloexec", e))?;
    grantpt(&controller).map_err(|e| pty_err("grantpt", e))?;
    unlockpt(&controller).map_err(|e| pty_err("unlockpt", e))?;
    let peripheral_name = ptsname(&controller, Vec::new()).map_err(|e| pty_err("ptsname", e))?;
    let peripheral = rustix::fs::open(
        &peripheral_name,
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| pty_err("open pts", e))?;

    let mut command = Command::new(&path);
    command.args(&invocation.argv[1..]);
    command.current_dir(state.cwd());
    state.configure_child_env(&mut command);
    for assignment in &invocation.assignments {
        command.env(&assignment.name, &assignment.value);
    }
    command.stdin(Stdio::from(peripheral.try_clone()?));
    command.stdout(Stdio::from(peripheral.try_clone()?));
    command.stderr(Stdio::from(peripheral));

    let mut child = command.spawn()?;
    // Drain the controller. Linux EOFs (EIO) the master when the peripheral
    // closes, but macOS may block, so read non-blocking and poll the child.
    rustix::io::ioctl_fionbio(&controller, true).map_err(|e| pty_err("nonblock", e))?;
    let mut reader = std::fs::File::from(controller);
    let mut output = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut exited: Option<std::process::ExitStatus> = None;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if let Err(error) =
                    append_bounded_pty_output(&mut output, &chunk[..n], MAX_PTY_CAPTURE_BYTES)
                {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error.into());
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if exited.is_none() {
                    exited = child.try_wait()?;
                }
                if exited.is_some() {
                    // Child is gone; all its output is already buffered. Drain
                    // whatever remains without blocking, then stop.
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    while let Ok(n) = reader.read(&mut chunk) {
                        if n == 0 {
                            break;
                        }
                        if let Err(error) = append_bounded_pty_output(
                            &mut output,
                            &chunk[..n],
                            MAX_PTY_CAPTURE_BYTES,
                        ) {
                            let _ = child.kill();
                            let _ = child.wait();
                            return Err(error.into());
                        }
                    }
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
    let status = match exited {
        Some(status) => status,
        None => child.wait()?,
    };
    Ok(CommandOutcome::captured(
        exit_status_code(status),
        output,
        Vec::new(),
    ))
}

fn append_bounded_pty_output(output: &mut Vec<u8>, bytes: &[u8], limit: usize) -> io::Result<()> {
    if output.len().saturating_add(bytes.len()) > limit {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("PTY capture exceeds {limit} bytes"),
        ));
    }
    output.extend_from_slice(bytes);
    Ok(())
}

/// Map a child's exit status to a shell exit code: its code, or 128+signal when
/// terminated by a signal (so a SIGINT-killed command reports 130, like bash).
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

/// Per-stream capture caps: keep the first [`CAPTURE_HEAD`] and last
/// [`CAPTURE_TAIL`] bytes of a captured stream. Everything else is drained (so
/// the child's pipe never blocks) and replaced by a one-line marker. This bounds
/// memory in the capturing output modes (`compact`/`semantic`/…) so an agent
/// running `cat huge.bin` or a runaway producer can't OOM the shell. Raw mode
/// (which inherits fds) and redirections to files are unaffected — this only
/// touches the observation-capture plane.
const CAPTURE_HEAD: usize = 1 << 20; // 1 MiB
const CAPTURE_TAIL: usize = 1 << 20; // 1 MiB

/// Read `reader` to EOF, retaining at most the first `CAPTURE_HEAD` and last
/// `CAPTURE_TAIL` bytes. Under the cap the bytes are returned exactly; over it, a
/// `… [agsh: N bytes elided …] …` marker separates head from tail. Always drains
/// the reader fully so the child isn't blocked on a full pipe.
#[cfg(test)]
fn read_capped(reader: impl Read) -> io::Result<Vec<u8>> {
    read_capped_with_tee(reader, None).map(|capture| capture.preview)
}

#[derive(Debug)]
struct CappedPreview {
    preview: Vec<u8>,
    complete: bool,
}

fn read_capped_with_tee(
    mut reader: impl Read,
    mut exact: Option<&mut dyn Write>,
) -> io::Result<CappedPreview> {
    let mut head: Vec<u8> = Vec::new();
    let mut tail: Vec<u8> = Vec::new();
    let mut total: usize = 0;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        let bytes = &chunk[..n];
        if let Some(writer) = exact.as_mut() {
            if writer.write_all(bytes).is_err() {
                // Raw trace persistence is optional. Stop teeing after an I/O
                // failure but keep draining the child and building its bounded
                // observation preview.
                exact = None;
            }
        }
        total = total
            .checked_add(n)
            .ok_or_else(|| io::Error::other("captured stream length overflow"))?;
        if head.len() < CAPTURE_HEAD {
            let take = (CAPTURE_HEAD - head.len()).min(n);
            head.extend_from_slice(&bytes[..take]);
            tail.extend_from_slice(&bytes[take..]);
        } else {
            tail.extend_from_slice(bytes);
        }
        // Keep the tail bounded, trimming amortized-O(1) (only past 2×).
        if tail.len() > CAPTURE_TAIL * 2 {
            let cut = tail.len() - CAPTURE_TAIL;
            tail.drain(0..cut);
        }
    }
    if total <= CAPTURE_HEAD + CAPTURE_TAIL {
        // Nothing was dropped — head + tail is the exact stream.
        head.extend_from_slice(&tail);
        return Ok(CappedPreview {
            preview: head,
            complete: true,
        });
    }
    if tail.len() > CAPTURE_TAIL {
        let cut = tail.len() - CAPTURE_TAIL;
        tail.drain(0..cut);
    }
    let dropped = total - CAPTURE_HEAD - CAPTURE_TAIL;
    head.extend_from_slice(
        format!(
            "\n… [agsh: {dropped} bytes of output elided; {total} bytes total, \
             showing first {CAPTURE_HEAD} and last {CAPTURE_TAIL}] …\n"
        )
        .as_bytes(),
    );
    head.extend_from_slice(&tail);
    Ok(CappedPreview {
        preview: head,
        complete: false,
    })
}

#[derive(Debug)]
struct CapturedStream {
    preview: Vec<u8>,
    exact: Option<ExactTraceFile>,
    preview_complete: bool,
}

impl CapturedStream {
    fn complete(preview: Vec<u8>) -> Self {
        Self {
            preview,
            exact: None,
            preview_complete: true,
        }
    }
}

fn read_capped_to_spool(
    reader: impl Read,
    mut spool: TraceSpoolWriter,
) -> io::Result<CapturedStream> {
    let preview = read_capped_with_tee(reader, Some(&mut spool))?;
    let exact = spool.finish().ok();
    Ok(CapturedStream {
        preview: preview.preview,
        exact,
        preview_complete: preview.complete,
    })
}

fn read_capture_stream(
    mut reader: impl Read,
    spool: Option<TraceSpoolWriter>,
) -> io::Result<CapturedStream> {
    match spool {
        Some(spool) => {
            let capture = read_capped_to_spool(reader, spool)?;
            Ok(capture)
        }
        None => read_exact_capture(&mut reader, MAX_IN_MEMORY_CAPTURE_BYTES)
            .map(CapturedStream::complete),
    }
}

fn read_capture_stream_for_observation(
    reader: impl Read,
    spool: Option<TraceSpoolWriter>,
    bounded_observation: bool,
) -> io::Result<CapturedStream> {
    if bounded_observation {
        match spool {
            Some(spool) => read_capture_stream(reader, Some(spool)),
            None => read_capped_with_tee(reader, None).map(|preview| CapturedStream {
                preview: preview.preview,
                exact: None,
                preview_complete: preview.complete,
            }),
        }
    } else {
        read_capture_stream(reader, spool)
    }
}

trait CaptureReader: Read + std::os::fd::AsFd + Send {}

impl<T> CaptureReader for T where T: Read + std::os::fd::AsFd + Send {}

struct DirectChildCaptureReader {
    inner: Box<dyn CaptureReader>,
    direct_child_exited: Arc<AtomicBool>,
    incomplete: Option<TraceSpoolIncompleteMarker>,
    preview_incomplete: Arc<AtomicBool>,
    post_exit_started: Option<Instant>,
    post_exit_bytes: usize,
    handoff_result: Option<CaptureDrainHandoff>,
}

impl DirectChildCaptureReader {
    fn new(
        inner: Box<dyn CaptureReader>,
        direct_child_exited: Arc<AtomicBool>,
        incomplete: Option<TraceSpoolIncompleteMarker>,
        preview_incomplete: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let flags = rustix::fs::fcntl_getfl(&inner)
            .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
        rustix::fs::fcntl_setfl(&inner, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
        Ok(Self {
            inner,
            direct_child_exited,
            incomplete,
            preview_incomplete,
            post_exit_started: None,
            post_exit_bytes: 0,
            handoff_result: None,
        })
    }

    fn mark_incomplete(&self) {
        self.preview_incomplete.store(true, Ordering::Release);
        if let Some(marker) = &self.incomplete {
            marker.mark_incomplete();
        }
    }

    fn detach_remaining_writers(&mut self) -> CaptureDrainHandoff {
        if let Some(result) = self.handoff_result {
            return result;
        }
        let Some(helper) = CAPTURE_DRAIN_HELPER.get() else {
            self.handoff_result = Some(CaptureDrainHandoff::Unavailable);
            return CaptureDrainHandoff::Unavailable;
        };
        let Ok(reader) = self.inner.as_fd().try_clone_to_owned() else {
            self.handoff_result = Some(CaptureDrainHandoff::Unavailable);
            return CaptureDrainHandoff::Unavailable;
        };
        let result = launch_capture_drain_worker(helper, reader, CAPTURE_DRAIN_ACK_TIMEOUT);
        self.handoff_result = Some(result);
        if result == CaptureDrainHandoff::Ambiguous {
            self.mark_incomplete();
        }
        result
    }
}

impl Read for DirectChildCaptureReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let exited = self.direct_child_exited.load(Ordering::Acquire);
            if exited {
                let started = *self.post_exit_started.get_or_insert_with(Instant::now);
                if (self.post_exit_bytes >= POST_CHILD_CAPTURE_DRAIN_BYTES
                    || started.elapsed() >= POST_CHILD_CAPTURE_DRAIN_TIME)
                    && self.detach_remaining_writers() == CaptureDrainHandoff::Transferred
                {
                    self.mark_incomplete();
                    return Ok(0);
                }
            }

            match self.inner.read(buffer) {
                Ok(bytes) => {
                    if exited {
                        self.post_exit_bytes = self.post_exit_bytes.saturating_add(bytes);
                    }
                    return Ok(bytes);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if exited {
                        if self.detach_remaining_writers() == CaptureDrainHandoff::Transferred {
                            self.mark_incomplete();
                            return Ok(0);
                        }
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

fn spawn_exit_aware_capture_reader<R>(
    reader: R,
    spool: Option<TraceSpoolWriter>,
    bounded_capture: bool,
    direct_stages_exited: Arc<AtomicBool>,
) -> io::Result<CaptureJoinHandle>
where
    R: CaptureReader + 'static,
{
    let incomplete = spool.as_ref().map(TraceSpoolWriter::incomplete_marker);
    let preview_incomplete = Arc::new(AtomicBool::new(false));
    let reader = DirectChildCaptureReader::new(
        Box::new(reader),
        direct_stages_exited,
        incomplete,
        Arc::clone(&preview_incomplete),
    )?;
    std::thread::Builder::new().spawn(move || {
        let mut capture = read_capture_stream_for_observation(reader, spool, bounded_capture)?;
        if preview_incomplete.load(Ordering::Acquire) {
            capture.preview_complete = false;
        }
        Ok(capture)
    })
}

fn read_exact_capture(reader: &mut impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut exact = Vec::new();
    reader
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut exact)?;
    if exact.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("in-memory shell capture exceeds {limit} bytes"),
        ));
    }
    Ok(exact)
}

fn wait_child_interruptibly(child: &mut Child, state: &ShellState) -> io::Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            if state.interrupted() {
                signal_cancellable_child_group(child, rustix::process::Signal::KILL);
            }
            return Ok(status);
        }
        if state.interrupted() {
            // Stage children live outside agsh's foreground process group, so a
            // terminal SIGINT must be forwarded before waiting for their status.
            signal_cancellable_child_group(child, rustix::process::Signal::INT);
            let deadline = Instant::now() + INTERRUPTED_CHILD_STATUS_GRACE;
            while Instant::now() < deadline {
                if let Some(status) = child.try_wait()? {
                    // A non-interactive shell may exit on SIGINT while a
                    // background descendant in the same group ignores it.
                    signal_cancellable_child_group(child, rustix::process::Signal::KILL);
                    return Ok(status);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            // A shell pipeline stage runs on a worker thread. If a later stage
            // fails to spawn, cleanup signals this flag before joining the
            // worker. Those children have their own process group so descendants
            // cannot survive the direct child; ordinary foreground commands keep
            // their existing process-group behavior.
            signal_cancellable_child_group(child, rustix::process::Signal::KILL);
            let _ = child.kill();
            return child.wait();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn signal_cancellable_child_group(child: &Child, signal: rustix::process::Signal) {
    if cancellable_shell_stage() {
        if let Some(pgid) = rustix::process::Pid::from_raw(child.id() as i32) {
            let _ = rustix::process::kill_process_group(pgid, signal);
        }
    }
}

fn configure_cancellable_shell_stage_child(command: &mut Command) {
    if cancellable_shell_stage() {
        command.process_group(0);
    }
}

fn run_external(
    invocation: &ExpandedInvocation,
    state: &ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    command_path: Option<&Path>,
) -> Result<CommandOutcome, ShellError> {
    validate_expanded_redirection_descriptors(&invocation.redirections)?;
    let mut command = if let Some(command_path) = command_path {
        Command::new(command_path)
    } else {
        Command::new(&invocation.argv[0])
    };
    command.args(&invocation.argv[1..]);
    command.current_dir(state.cwd());
    state.configure_child_env(&mut command);
    for assignment in &invocation.assignments {
        command.env(&assignment.name, &assignment.value);
    }

    // Heredocs/herestrings carry literal stdin bytes; an explicit stdin
    // redirection overrides an inherited pipe.
    let heredoc_bytes: Option<Vec<u8>> = invocation.redirections.iter().find_map(|redirection| {
        match (&redirection.mode, &redirection.target) {
            (
                RedirectionMode::HereDoc | RedirectionMode::HereString,
                ExpandedRedirectionTarget::Bytes(bytes),
            ) => Some(bytes.clone()),
            _ => None,
        }
    });
    let stdin_data = heredoc_bytes.as_deref().or(stdin_data);

    let mut stdin_is_piped = stdin_data.is_some();
    if stdin_is_piped {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::inherit());
    }

    let inherited_routing = inherited_capture_routing();
    let ordered_routing = !inherited_routing.is_default()
        || invocation.redirections.iter().any(|redirection| {
            redirection.mode == RedirectionMode::DupFd
                && (redirection.fd == 1 || redirection.fd == 2)
        });
    let mut ordered_sinks = None;
    let mut ordered_readers = None;
    if ordered_routing {
        let (base_stdout, base_stderr) = if capture_outputs {
            let (stdout_reader, stdout_writer) = io::pipe()?;
            let (stderr_reader, stderr_writer) = io::pipe()?;
            ordered_readers = Some((stdout_reader, stderr_reader));
            (
                ExternalCaptureSink::Pipe(stdout_writer),
                ExternalCaptureSink::Pipe(stderr_writer),
            )
        } else {
            (
                ExternalCaptureSink::Inherit(InheritedOutput::Stdout),
                ExternalCaptureSink::Inherit(InheritedOutput::Stderr),
            )
        };
        let stdout_sink =
            external_sink_for_destination(&inherited_routing.stdout, &base_stdout, &base_stderr)?;
        let stderr_sink =
            external_sink_for_destination(&inherited_routing.stderr, &base_stdout, &base_stderr)?;
        command.stdout(stdout_sink.stdio()?);
        command.stderr(stderr_sink.stdio()?);
        ordered_sinks = Some((stdout_sink, stderr_sink));
    } else if capture_outputs {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    } else {
        command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    }

    let mut merge_stderr_to_stdout = false;
    let mut merge_stdout_to_stderr = false;
    let mut redirection_context = ExternalRedirectionContext {
        stdin_is_piped: &mut stdin_is_piped,
        merge_stderr_to_stdout: &mut merge_stderr_to_stdout,
        merge_stdout_to_stderr: &mut merge_stdout_to_stderr,
        capture_outputs,
        noclobber: state.noclobber(),
        ordered_sinks: &mut ordered_sinks,
    };
    apply_external_redirections(
        &mut command,
        &invocation.redirections,
        &mut redirection_context,
    )?;
    configure_cancellable_shell_stage_child(&mut command);
    // The command owns all writer duplicates now. Dropping the parent's routing
    // copies is required for the capture readers to observe EOF after child exit.
    drop(ordered_sinks.take());

    // Only the final, top-level agent observation is bounded. Nested capture is
    // shell-semantic data (functions, substitutions, compound commands) and must
    // remain exact; rich rendering likewise needs the complete input.
    let bounded_observation =
        capture_outputs && output_mode != OutputMode::Rich && !state.exact_capture_enabled();

    if capture_outputs {
        // P0-8: when this is a streaming pipeline stage (stdout is a downstream
        // pipe) and there are no redirections/merges to honor, hand the child's
        // stdout straight to that pipe. Output then flows incrementally with real
        // backpressure, and a consumer that exits early (`… | head`) closes the
        // pipe so the producer gets SIGPIPE — instead of being captured and run to
        // completion (or forever) first, which hung `{ yes; } | head`.
        if redirections_only_affect_stdin(&invocation.redirections)
            && !merge_stderr_to_stdout
            && !merge_stdout_to_stderr
            && inherited_routing.is_default()
        {
            if let Some(writer) = state.streaming_stdout_writer() {
                command.stdout(Stdio::from(writer));
                let stderr_spool = if bounded_observation {
                    state.create_trace_spool("err").ok()
                } else {
                    None
                };
                let stderr_incomplete = stderr_spool
                    .as_ref()
                    .map(TraceSpoolWriter::incomplete_marker);
                let mut child = command.spawn()?;
                let stdin_writer = match (stdin_is_piped, stdin_data, child.stdin.take()) {
                    (true, Some(input), Some(mut stdin)) => {
                        let buf = input.to_vec();
                        Some(std::thread::spawn(move || -> io::Result<()> {
                            match stdin.write_all(&buf) {
                                Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e),
                                _ => Ok(()),
                            }
                        }))
                    }
                    _ => None,
                };
                // stdout streams to the pipe. Make stderr exit-aware so a
                // background descendant retaining fd2 cannot keep the direct
                // pipeline stage alive after its child has exited.
                let direct_child_exited = Arc::new(AtomicBool::new(false));
                let stderr_preview_incomplete = Arc::new(AtomicBool::new(false));
                let stderr_reader = match child.stderr.take() {
                    Some(reader) => match DirectChildCaptureReader::new(
                        Box::new(reader),
                        Arc::clone(&direct_child_exited),
                        stderr_incomplete,
                        Arc::clone(&stderr_preview_incomplete),
                    ) {
                        Ok(reader) => Some(reader),
                        Err(error) => {
                            let _ = child.kill();
                            let _ = child.wait();
                            return Err(error.into());
                        }
                    },
                    None => None,
                };
                let stderr_handle = stderr_reader.map(|reader| {
                    std::thread::spawn(move || -> io::Result<CapturedStream> {
                        let mut capture = read_capture_stream_for_observation(
                            reader,
                            stderr_spool,
                            bounded_observation,
                        )?;
                        if stderr_preview_incomplete.load(Ordering::Acquire) {
                            capture.preview_complete = false;
                        }
                        Ok(capture)
                    })
                });
                let status_result =
                    wait_child_interruptibly(&mut child, state).map_err(ShellError::from);
                direct_child_exited.store(true, Ordering::Release);
                let stderr_result = match stderr_handle {
                    Some(handle) => handle
                        .join()
                        .map_err(|_| ShellError::execution("stderr reader thread panicked"))?
                        .map_err(ShellError::from),
                    None => Ok(CapturedStream::complete(Vec::new())),
                };
                let stdin_result = if let Some(handle) = stdin_writer {
                    handle
                        .join()
                        .map_err(|_| ShellError::execution("stdin writer thread panicked"))?
                        .map_err(ShellError::from)
                } else {
                    Ok(())
                };
                let stderr = stderr_result?;
                stdin_result?;
                let status = status_result?;
                return Ok(CommandOutcome::captured_from_streams(
                    exit_status_code(status),
                    CapturedStream::complete(Vec::new()),
                    stderr,
                ));
            }
        }

        let stdout_spool = if bounded_observation {
            state.create_trace_spool("out").ok()
        } else {
            None
        };
        let stderr_spool = if bounded_observation {
            state.create_trace_spool("err").ok()
        } else {
            None
        };
        let stdout_incomplete = stdout_spool
            .as_ref()
            .map(TraceSpoolWriter::incomplete_marker);
        let stderr_incomplete = stderr_spool
            .as_ref()
            .map(TraceSpoolWriter::incomplete_marker);
        let mut child = command.spawn()?;
        drop(command);
        // Feed stdin from a separate thread so `wait_with_output` can drain the
        // child's stdout/stderr concurrently. Writing all of stdin first would
        // deadlock once a child that echoes its input fills the stdout pipe
        // (e.g. `x=$(cat <<< "$(seq 200000)")`). BrokenPipe is benign — the child
        // may exit before consuming all input (e.g. `head -c1`).
        let stdin_writer = match (stdin_is_piped, stdin_data, child.stdin.take()) {
            (true, Some(input), Some(mut stdin)) => {
                let buf = input.to_vec();
                Some(std::thread::spawn(move || -> io::Result<()> {
                    match stdin.write_all(&buf) {
                        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e),
                        _ => Ok(()),
                    }
                    // `stdin` drops here, sending EOF.
                }))
            }
            _ => None,
        };
        // Drain stdout+stderr concurrently into *bounded* buffers (stderr on a
        // thread, stdout here) so a huge/streaming child can't OOM us the way
        // `wait_with_output`'s unbounded read could. Two streams still need two
        // readers to avoid a full-pipe deadlock.
        let (ordered_stdout, ordered_stderr) = match ordered_readers {
            Some((stdout, stderr)) => (Some(stdout), Some(stderr)),
            None => (None, None),
        };
        let stdout_pipe: Option<Box<dyn CaptureReader>> = match ordered_stdout {
            Some(reader) => Some(Box::new(reader)),
            None => child
                .stdout
                .take()
                .map(|reader| Box::new(reader) as Box<dyn CaptureReader>),
        };
        let stderr_pipe: Option<Box<dyn CaptureReader>> = match ordered_stderr {
            Some(reader) => Some(Box::new(reader)),
            None => child
                .stderr
                .take()
                .map(|reader| Box::new(reader) as Box<dyn CaptureReader>),
        };
        let direct_child_exited = Arc::new(AtomicBool::new(false));
        let stdout_preview_incomplete = Arc::new(AtomicBool::new(false));
        let stderr_preview_incomplete = Arc::new(AtomicBool::new(false));
        let stdout_reader = match stdout_pipe {
            Some(reader) => {
                match DirectChildCaptureReader::new(
                    reader,
                    Arc::clone(&direct_child_exited),
                    stdout_incomplete,
                    Arc::clone(&stdout_preview_incomplete),
                ) {
                    Ok(reader) => Some(reader),
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(error.into());
                    }
                }
            }
            None => None,
        };
        let stderr_reader = match stderr_pipe {
            Some(reader) => {
                match DirectChildCaptureReader::new(
                    reader,
                    Arc::clone(&direct_child_exited),
                    stderr_incomplete,
                    Arc::clone(&stderr_preview_incomplete),
                ) {
                    Ok(reader) => Some(reader),
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(error.into());
                    }
                }
            }
            None => None,
        };
        let stdout_handle = stdout_reader.map(|reader| {
            std::thread::spawn(move || -> io::Result<CapturedStream> {
                let mut capture =
                    read_capture_stream_for_observation(reader, stdout_spool, bounded_observation)?;
                if stdout_preview_incomplete.load(Ordering::Acquire) {
                    capture.preview_complete = false;
                }
                Ok(capture)
            })
        });
        let stderr_handle = stderr_reader.map(|reader| {
            std::thread::spawn(move || -> io::Result<CapturedStream> {
                let mut capture =
                    read_capture_stream_for_observation(reader, stderr_spool, bounded_observation)?;
                if stderr_preview_incomplete.load(Ordering::Acquire) {
                    capture.preview_complete = false;
                }
                Ok(capture)
            })
        });

        let status_result = wait_child_interruptibly(&mut child, state).map_err(ShellError::from);
        direct_child_exited.store(true, Ordering::Release);
        let stdout_result = match stdout_handle {
            Some(handle) => handle
                .join()
                .map_err(|_| ShellError::execution("stdout reader thread panicked"))?
                .map_err(ShellError::from),
            None => Ok(CapturedStream::complete(Vec::new())),
        };
        let stderr_result = match stderr_handle {
            Some(handle) => handle
                .join()
                .map_err(|_| ShellError::execution("stderr reader thread panicked"))?
                .map_err(ShellError::from),
            None => Ok(CapturedStream::complete(Vec::new())),
        };
        let stdin_result = if let Some(handle) = stdin_writer {
            handle
                .join()
                .map_err(|_| ShellError::execution("stdin writer thread panicked"))?
                .map_err(ShellError::from)
        } else {
            Ok(())
        };
        let stdout = stdout_result?;
        let stderr = stderr_result?;
        stdin_result?;
        let status = status_result?;
        let mut outcome =
            CommandOutcome::captured_from_streams(exit_status_code(status), stdout, stderr);
        if merge_stderr_to_stdout {
            outcome.merge_exact_stderr_into_stdout();
            let stderr = std::mem::take(&mut outcome.stderr);
            outcome.stdout.extend_from_slice(&stderr);
            outcome.stderr_preview_complete = true;
        }
        if merge_stdout_to_stderr {
            outcome.merge_exact_stdout_into_stderr();
            let stdout = std::mem::take(&mut outcome.stdout);
            outcome.stderr.extend_from_slice(&stdout);
            outcome.stdout_preview_complete = true;
        }
        Ok(outcome)
    } else {
        let mut child = command.spawn()?;
        if stdin_is_piped {
            if let (Some(input), Some(mut stdin)) = (stdin_data, child.stdin.take()) {
                // stdout/stderr are inherited here (no deadlock), but a child can
                // still exit early — treat BrokenPipe as benign.
                match stdin.write_all(input) {
                    Err(e) if e.kind() != io::ErrorKind::BrokenPipe => return Err(e.into()),
                    _ => {}
                }
            }
        }
        let status = wait_child_interruptibly(&mut child, state)?;
        Ok(CommandOutcome::captured(
            exit_status_code(status),
            Vec::new(),
            Vec::new(),
        ))
    }
}

fn run_pipeline(
    graph: &CommandGraph,
    pipeline: &Pipeline,
    state: &mut ShellState,
    options: &ExecutionOptions,
    stream_raw_to_parent: bool,
) -> Result<CommandOutcome, ShellError> {
    validate_pipeline_redirection_descriptors(pipeline)?;

    if let Some(outcome) =
        try_run_streaming_mixed_shell_stage_pipeline(graph, pipeline, state, options)?
    {
        return Ok(outcome);
    }

    if let Some(outcome) =
        try_run_streaming_external_shell_stage_pipeline(graph, pipeline, state, options)?
    {
        return Ok(outcome);
    }

    if let Some(outcome) =
        try_run_streaming_external_prefix_to_final_shell_command(graph, pipeline, state, options)?
    {
        return Ok(outcome);
    }

    if pipeline
        .commands
        .iter()
        .any(command_invocation_starts_compound)
    {
        return run_buffered_command_pipeline(graph, pipeline, state, options);
    }

    let commands = pipeline
        .commands
        .iter()
        .map(|invocation| expand_invocation(invocation, state))
        .collect::<Result<Vec<_>, _>>()?;

    if commands.iter().any(|command| command.argv.is_empty()) {
        return Err(ShellError::execution("empty command in pipeline"));
    }

    if let Some((resolved_prefix, final_read)) =
        resolve_streaming_external_prefix_to_final_read(&commands, state)
    {
        let mut outcome = run_streaming_external_prefix_to_final_read(
            &resolved_prefix,
            &final_read,
            state,
            options.output_mode,
            options.allow_process_replacement,
        )?;
        apply_pipeline_negation(&mut outcome, pipeline.negated);
        return Ok(outcome);
    }

    if let Some(resolved) = resolve_streaming_external_pipeline(&commands, state) {
        let inherit_raw_output = stream_raw_to_parent
            && !options.output_mode.should_capture()
            && state.streaming_stdout_is_none();
        let bounded_capture = options.output_mode.should_capture()
            && options.output_mode != OutputMode::Rich
            && !state.exact_capture_enabled();
        let mut outcome = run_streaming_external_pipeline(
            &resolved,
            state,
            inherit_raw_output,
            !inherit_raw_output,
            bounded_capture,
        )?;
        apply_pipeline_negation(&mut outcome, pipeline.negated);
        return Ok(outcome);
    }

    preflight_buffered_pipeline_invocations(&commands, state)?;

    let mut stdin_data: Option<Vec<u8>> = None;
    let mut stderr_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());
    let mut exit_codes = Vec::with_capacity(commands.len());
    let last_index = commands.len().saturating_sub(1);
    let mut outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());

    for (index, invocation) in commands.iter().enumerate() {
        outcome = if index == last_index {
            let rich_stdout = commands.len() == 1
                && rich_stdout_allowed_for_invocation(invocation, state, options);
            with_rich_stdout(state, rich_stdout, |state| {
                run_invocation(
                    invocation,
                    state,
                    options.output_mode,
                    stdin_data.as_deref(),
                    true,
                    LookupMode::Normal,
                    options.allow_process_replacement,
                )
            })?
        } else {
            let mut stage_state = state.clone();
            stage_state.replace_rich_stdout(false);
            stage_state.replace_exact_capture(true);
            run_invocation(
                invocation,
                &mut stage_state,
                options.output_mode,
                stdin_data.as_deref(),
                true,
                LookupMode::Normal,
                options.allow_process_replacement,
            )?
        };
        exit_codes.push(outcome.exit_code);
        stderr_outcome.append_stderr(&mut outcome)?;
        if index != last_index {
            stdin_data = Some(std::mem::take(&mut outcome.stdout));
        }
    }

    record_pipestatus(state, &exit_codes);
    outcome.exit_code = pipeline_exit_code(&exit_codes, state.pipefail());
    apply_pipeline_negation(&mut outcome, pipeline.negated);
    outcome.stderr = stderr_outcome.stderr;
    outcome.exact_stderr = stderr_outcome.exact_stderr;
    Ok(outcome)
}

fn with_rich_stdout<T>(
    state: &mut ShellState,
    rich_stdout: bool,
    f: impl FnOnce(&mut ShellState) -> Result<T, ShellError>,
) -> Result<T, ShellError> {
    let previous_rich_stdout = state.replace_rich_stdout(rich_stdout);
    let result = f(state);
    state.replace_rich_stdout(previous_rich_stdout);
    result
}

fn rich_stdout_allowed_for_invocation(
    invocation: &ExpandedInvocation,
    state: &ShellState,
    options: &ExecutionOptions,
) -> bool {
    rich_stdout_allowed_for_invocation_with_terminal(
        invocation,
        state,
        options,
        io::stdout().is_terminal(),
    )
}

fn rich_stdout_allowed_for_invocation_with_terminal(
    invocation: &ExpandedInvocation,
    state: &ShellState,
    options: &ExecutionOptions,
    stdout_is_terminal: bool,
) -> bool {
    !options.output_mode.should_capture()
        && state.streaming_stdout_is_none()
        && stdout_is_terminal
        && stdout_redirections_preserve_terminal(&invocation.redirections)
}

fn stdout_redirections_preserve_terminal(redirections: &[ExpandedRedirection]) -> bool {
    !redirections
        .iter()
        .any(|redirection| match redirection.mode {
            RedirectionMode::Write | RedirectionMode::WriteClobber | RedirectionMode::Append => {
                redirection.fd == 1
            }
            RedirectionMode::WriteBoth => true,
            RedirectionMode::DupFd => redirection.fd == 1,
            RedirectionMode::Read | RedirectionMode::HereDoc | RedirectionMode::HereString => false,
        })
}

fn redirections_only_affect_stdin(redirections: &[ExpandedRedirection]) -> bool {
    redirections.iter().all(|redirection| {
        redirection.fd == 0
            && matches!(
                (&redirection.mode, &redirection.target),
                (RedirectionMode::Read, ExpandedRedirectionTarget::Path(_))
                    | (
                        RedirectionMode::HereDoc | RedirectionMode::HereString,
                        ExpandedRedirectionTarget::Bytes(_)
                    )
                    | (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close)
            )
    })
}

fn validate_pipeline_redirection_descriptors(pipeline: &Pipeline) -> Result<(), ShellError> {
    for invocation in &pipeline.commands {
        for redirection in &invocation.redirections {
            let supported = match (&redirection.mode, &redirection.target) {
                (
                    RedirectionMode::Read | RedirectionMode::HereDoc | RedirectionMode::HereString,
                    RedirectionTarget::Word { .. },
                ) => redirection.fd == 0,
                (
                    RedirectionMode::Write
                    | RedirectionMode::WriteClobber
                    | RedirectionMode::Append,
                    RedirectionTarget::Word { .. },
                ) => redirection.fd == 1 || redirection.fd == 2,
                (RedirectionMode::WriteBoth, RedirectionTarget::Word { .. }) => redirection.fd == 1,
                (RedirectionMode::DupFd, RedirectionTarget::Close) => redirection.fd <= 2,
                (RedirectionMode::DupFd, RedirectionTarget::Fd(target)) => {
                    matches!((redirection.fd, *target), (1, 2) | (2, 1))
                }
                _ => false,
            };
            if !supported {
                return Err(unsupported_redirection_descriptor(redirection.fd));
            }
        }
    }
    Ok(())
}

fn validate_expanded_redirection_descriptors(
    redirections: &[ExpandedRedirection],
) -> Result<(), ShellError> {
    for redirection in redirections {
        let supported = match (&redirection.mode, &redirection.target) {
            (RedirectionMode::Read, ExpandedRedirectionTarget::Path(_))
            | (
                RedirectionMode::HereDoc | RedirectionMode::HereString,
                ExpandedRedirectionTarget::Bytes(_),
            ) => redirection.fd == 0,
            (
                RedirectionMode::Write | RedirectionMode::WriteClobber | RedirectionMode::Append,
                ExpandedRedirectionTarget::Path(_),
            ) => redirection.fd == 1 || redirection.fd == 2,
            (RedirectionMode::WriteBoth, ExpandedRedirectionTarget::Path(_)) => redirection.fd == 1,
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) => redirection.fd <= 2,
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(target)) => {
                matches!((redirection.fd, *target), (1, 2) | (2, 1))
            }
            _ => false,
        };
        if !supported {
            return Err(unsupported_redirection_descriptor(redirection.fd));
        }
    }
    Ok(())
}

fn unsupported_redirection_descriptor(fd: u8) -> ShellError {
    ShellError::unsupported(format!(
        "unsupported redirection for fd {fd}; only stdin, stdout, and stderr are supported"
    ))
}

fn run_buffered_command_pipeline(
    _graph: &CommandGraph,
    pipeline: &Pipeline,
    state: &mut ShellState,
    options: &ExecutionOptions,
) -> Result<CommandOutcome, ShellError> {
    let mut stdin_data: Option<Vec<u8>> = None;
    let mut stderr_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());
    let mut exit_codes = Vec::with_capacity(pipeline.commands.len());
    let last_index = pipeline.commands.len().saturating_sub(1);
    let mut outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());

    for (index, invocation) in pipeline.commands.iter().enumerate() {
        outcome = if index == last_index {
            run_pipeline_command_invocation(
                invocation,
                state,
                options.output_mode,
                stdin_data.as_deref(),
                true,
                !pipeline.negated,
                options.allow_process_replacement,
            )?
        } else {
            let mut stage_state = state.clone();
            stage_state.replace_exact_capture(true);
            run_pipeline_command_invocation(
                invocation,
                &mut stage_state,
                options.output_mode,
                stdin_data.as_deref(),
                true,
                !pipeline.negated,
                options.allow_process_replacement,
            )?
        };
        exit_codes.push(outcome.exit_code);
        stderr_outcome.append_stderr(&mut outcome)?;
        if index != last_index {
            stdin_data = Some(std::mem::take(&mut outcome.stdout));
        }
    }

    record_pipestatus(state, &exit_codes);
    outcome.exit_code = pipeline_exit_code(&exit_codes, state.pipefail());
    apply_pipeline_negation(&mut outcome, pipeline.negated);
    outcome.stderr = stderr_outcome.stderr;
    outcome.exact_stderr = stderr_outcome.exact_stderr;
    Ok(outcome)
}

fn command_invocation_starts_compound(invocation: &CommandInvocation) -> bool {
    let reserved = invocation
        .argv
        .first()
        .zip(invocation.argv_quote.first())
        .is_some_and(|(word, quote)| {
            *quote == QuoteKind::None
                && matches!(
                    word.as_str(),
                    "if" | "while" | "until" | "for" | "select" | "case" | "{"
                )
        });
    reserved
        || invocation
            .argv_segments
            .first()
            .and_then(|segments| segments.first())
            .is_some_and(|segment| {
                segment.quote == QuoteKind::None && segment.text.starts_with('(')
            })
}

fn run_pipeline_command_invocation(
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    allow_function_definition: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    if let Some(if_block) = parse_if_invocation(invocation)? {
        return run_compound_with_effective_stdin(state, invocation, stdin_data, |state| {
            run_if_invocation(
                &if_block,
                invocation,
                state,
                output_mode,
                capture_outputs,
                allow_process_replacement,
            )
        });
    }

    if let Some(while_block) = parse_while_invocation(invocation)? {
        return run_compound_with_effective_stdin(state, invocation, stdin_data, |state| {
            run_while_invocation(
                &while_block,
                invocation,
                state,
                output_mode,
                capture_outputs,
                allow_process_replacement,
            )
        });
    }

    if let Some(for_block) = parse_for_invocation(invocation)? {
        return run_compound_with_effective_stdin(state, invocation, stdin_data, |state| {
            run_for_invocation(
                &for_block,
                invocation,
                state,
                output_mode,
                capture_outputs,
                allow_process_replacement,
            )
        });
    }

    if let Some(select_block) = parse_select_invocation(invocation)? {
        return run_compound_with_effective_stdin(state, invocation, stdin_data, |state| {
            run_select_invocation(
                &select_block,
                invocation,
                state,
                output_mode,
                capture_outputs,
                allow_process_replacement,
            )
        });
    }

    if let Some(case_block) = parse_case_invocation(invocation)? {
        return run_compound_with_effective_stdin(state, invocation, stdin_data, |state| {
            run_case_invocation(
                &case_block,
                invocation,
                state,
                output_mode,
                capture_outputs,
                allow_process_replacement,
            )
        });
    }

    if let Some(inner) = parse_subshell_invocation(invocation)? {
        return run_subshell_invocation(
            &inner,
            invocation,
            state,
            output_mode,
            stdin_data,
            capture_outputs,
            allow_process_replacement,
        );
    }

    if let Some(inner) = parse_brace_group_invocation(invocation)? {
        return run_brace_group_invocation(
            &inner,
            invocation,
            state,
            output_mode,
            stdin_data,
            capture_outputs,
            allow_process_replacement,
        );
    }

    if allow_function_definition {
        if let Some((name, function)) = parse_function_definition(invocation)? {
            state.set_function(name, function);
            return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
        }
    }

    let invocation = expand_invocation(invocation, state)?;
    if invocation.argv.is_empty() {
        let stderr = apply_shell_assignments(&invocation.assignments, state);
        let _redirected_stdin =
            redirected_stdin_from_expanded_redirections(&invocation.redirections)?;
        let mut outcome =
            CommandOutcome::captured(state.last_command_substitution_status(), Vec::new(), stderr);
        apply_builtin_redirections(&mut outcome, &invocation.redirections, state)?;
        return Ok(outcome);
    }

    run_invocation(
        &invocation,
        state,
        output_mode,
        stdin_data,
        capture_outputs,
        LookupMode::Normal,
        allow_process_replacement,
    )
}

fn run_with_buffered_stdin<F>(
    state: &mut ShellState,
    stdin_data: Option<&[u8]>,
    run: F,
) -> Result<CommandOutcome, ShellError>
where
    F: FnOnce(&mut ShellState) -> Result<CommandOutcome, ShellError>,
{
    let Some(stdin_data) = stdin_data else {
        return run(state);
    };

    let previous = state.replace_buffered_stdin(Some(BufferedStdin::new(stdin_data.to_vec())));
    let result = run(state);
    state.replace_buffered_stdin(previous);
    result
}

fn run_compound_with_effective_stdin<F>(
    state: &mut ShellState,
    invocation: &CommandInvocation,
    stdin_data: Option<&[u8]>,
    run: F,
) -> Result<CommandOutcome, ShellError>
where
    F: FnOnce(&mut ShellState) -> Result<CommandOutcome, ShellError>,
{
    let redirected_stdin = redirected_stdin_from_command_redirections(invocation, state)?;
    run_with_effective_shell_stdin(state, stdin_data, redirected_stdin, run)
}

fn run_with_effective_shell_stdin<F>(
    state: &mut ShellState,
    stdin_data: Option<&[u8]>,
    redirected_stdin: Option<RedirectedShellStdin>,
    run: F,
) -> Result<CommandOutcome, ShellError>
where
    F: FnOnce(&mut ShellState) -> Result<CommandOutcome, ShellError>,
{
    match redirected_stdin {
        Some(RedirectedShellStdin::Buffered(bytes)) => {
            run_with_buffered_stdin(state, Some(&bytes), run)
        }
        Some(RedirectedShellStdin::File(file)) => run_with_streaming_stdin(state, file, run),
        None => run_with_buffered_stdin(state, stdin_data, run),
    }
}

enum RedirectedShellStdin {
    Buffered(Vec<u8>),
    File(File),
}

fn redirected_stdin_from_command_redirections(
    invocation: &CommandInvocation,
    state: &mut ShellState,
) -> Result<Option<RedirectedShellStdin>, ShellError> {
    let redirections = expand_redirections(&invocation.redirections, state)?;
    redirected_stdin_from_expanded_redirections(&redirections)
}

fn redirected_stdin_from_expanded_redirections(
    redirections: &[ExpandedRedirection],
) -> Result<Option<RedirectedShellStdin>, ShellError> {
    let mut stdin = None;
    for redirection in redirections {
        match (&redirection.mode, &redirection.target) {
            (RedirectionMode::Read, ExpandedRedirectionTarget::Path(path))
                if redirection.fd == 0 =>
            {
                stdin = Some(RedirectedShellStdin::File(open_read_redirection(path)?));
            }
            (
                RedirectionMode::HereDoc | RedirectionMode::HereString,
                ExpandedRedirectionTarget::Bytes(bytes),
            ) if redirection.fd == 0 => {
                stdin = Some(RedirectedShellStdin::Buffered(bytes.clone()));
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 0 => {
                stdin = Some(RedirectedShellStdin::Buffered(Vec::new()));
            }
            _ => {}
        }
    }
    Ok(stdin)
}

fn run_with_streaming_stdin<R, F>(
    state: &mut ShellState,
    reader: R,
    run: F,
) -> Result<CommandOutcome, ShellError>
where
    R: Read + Send + 'static,
    F: FnOnce(&mut ShellState) -> Result<CommandOutcome, ShellError>,
{
    let previous_buffered = state.replace_buffered_stdin(None);
    let previous = state.replace_streaming_stdin(Some(StreamingStdin::new(reader)));
    let result = run(state);
    state.replace_streaming_stdin(previous);
    state.replace_buffered_stdin(previous_buffered);
    result
}

fn run_with_streaming_stdout<F>(
    state: &mut ShellState,
    writer: io::PipeWriter,
    run: F,
) -> Result<CommandOutcome, ShellError>
where
    F: FnOnce(&mut ShellState) -> Result<CommandOutcome, ShellError>,
{
    let previous = state.replace_streaming_stdout(Some(StreamingStdout::new(writer)));
    let result = run(state);
    state.replace_streaming_stdout(previous);
    result
}

fn apply_pipeline_negation(outcome: &mut CommandOutcome, negated: bool) {
    if negated {
        outcome.exit_code = if outcome.exit_code == 0 { 1 } else { 0 };
    }
}

fn try_run_streaming_external_prefix_to_final_shell_command(
    _graph: &CommandGraph,
    pipeline: &Pipeline,
    state: &mut ShellState,
    options: &ExecutionOptions,
) -> Result<Option<CommandOutcome>, ShellError> {
    let Some((resolved_prefix, final_invocation)) =
        resolve_streaming_external_prefix_to_final_shell_command(pipeline, state)?
    else {
        return Ok(None);
    };

    let mut outcome = run_streaming_external_prefix_to_final_shell_command(
        &resolved_prefix,
        &final_invocation,
        state,
        options,
        !pipeline.negated,
    )?;
    apply_pipeline_negation(&mut outcome, pipeline.negated);
    Ok(Some(outcome))
}

fn resolve_streaming_external_prefix_to_final_shell_command(
    pipeline: &Pipeline,
    state: &mut ShellState,
) -> Result<Option<(Vec<ResolvedExternalInvocation>, CommandInvocation)>, ShellError> {
    let Some((final_invocation, prefix)) = pipeline.commands.split_last() else {
        return Ok(None);
    };
    if prefix.is_empty() || !command_invocation_accepts_streaming_stdin(final_invocation, state) {
        return Ok(None);
    }

    let mut resolved = Vec::with_capacity(prefix.len());
    for command in prefix {
        let invocation = expand_invocation(command, state)?;
        if invocation.argv.is_empty() {
            return Err(ShellError::execution("empty command in pipeline"));
        }
        if !supports_streaming_external_redirections(&invocation.redirections) {
            return Ok(None);
        }

        let name = invocation
            .argv
            .first()
            .expect("checked non-empty invocation");
        if state.function(name).is_some()
            || state.alias(name).is_some()
            || state.abbreviation(name).is_some()
            || is_builtin(name)
        {
            return Ok(None);
        }

        let Some(path) = resolve_external_path(&invocation, state, name) else {
            return Ok(None);
        };
        resolved.push(ResolvedExternalInvocation { invocation, path });
    }

    Ok(Some((resolved, final_invocation.clone())))
}

fn command_invocation_accepts_streaming_stdin(
    invocation: &CommandInvocation,
    state: &ShellState,
) -> bool {
    if command_invocation_starts_compound(invocation) {
        return true;
    }

    if invocation.redirections.iter().any(|redirection| {
        redirection.fd == 0
            && matches!(
                redirection.mode,
                RedirectionMode::HereDoc | RedirectionMode::HereString
            )
    }) {
        return true;
    }

    let Some((name, quote)) = invocation.argv.first().zip(invocation.argv_quote.first()) else {
        return false;
    };
    if *quote != QuoteKind::None {
        return false;
    }

    if state.function(name).is_some()
        || state.alias(name).is_some()
        || state.abbreviation(name).is_some()
    {
        return true;
    }

    if name == "builtin" {
        return invocation
            .argv
            .get(1)
            .zip(invocation.argv_quote.get(1))
            .is_some_and(|(wrapped, quote)| *quote == QuoteKind::None && is_builtin(wrapped));
    }

    is_builtin(name)
}

fn supports_streaming_shell_stage_redirections(redirections: &[agsh_core::Redirection]) -> bool {
    redirections.iter().all(
        |redirection| match (&redirection.mode, &redirection.target) {
            (RedirectionMode::Read, RedirectionTarget::Word { .. }) => redirection.fd == 0,
            (
                RedirectionMode::HereDoc | RedirectionMode::HereString,
                RedirectionTarget::Word { .. },
            ) => redirection.fd == 0,
            (
                RedirectionMode::Write | RedirectionMode::WriteClobber | RedirectionMode::Append,
                RedirectionTarget::Word { .. },
            )
            | (RedirectionMode::WriteBoth, RedirectionTarget::Word { .. }) => {
                redirection.fd == 1 || redirection.fd == 2
            }
            (RedirectionMode::DupFd, RedirectionTarget::Close) => redirection.fd <= 2,
            (RedirectionMode::DupFd, RedirectionTarget::Fd(1)) => redirection.fd == 2,
            (RedirectionMode::DupFd, RedirectionTarget::Fd(2)) => redirection.fd == 1,
            _ => false,
        },
    )
}

fn try_run_streaming_mixed_shell_stage_pipeline(
    _graph: &CommandGraph,
    pipeline: &Pipeline,
    state: &mut ShellState,
    options: &ExecutionOptions,
) -> Result<Option<CommandOutcome>, ShellError> {
    let Some(stages) = resolve_streaming_mixed_shell_stage_pipeline(pipeline, state)? else {
        return Ok(None);
    };

    let mut outcome = run_streaming_mixed_shell_stage_pipeline(&stages, state, options)?;
    apply_pipeline_negation(&mut outcome, pipeline.negated);
    Ok(Some(outcome))
}

fn resolve_streaming_mixed_shell_stage_pipeline(
    pipeline: &Pipeline,
    state: &mut ShellState,
) -> Result<Option<Vec<ResolvedStreamingStage>>, ShellError> {
    if pipeline.commands.len() < 2 {
        return Ok(None);
    }

    let shell_indices = pipeline
        .commands
        .iter()
        .enumerate()
        .filter_map(|(index, invocation)| {
            command_invocation_accepts_streaming_stdin(invocation, state).then_some(index)
        })
        .collect::<Vec<_>>();
    if shell_indices.is_empty()
        || shell_indices
            .last()
            .is_some_and(|index| *index + 1 == pipeline.commands.len())
    {
        return Ok(None);
    }
    let has_first_stage_shell = shell_indices.first() == Some(&0);
    let has_non_contiguous_shell_stages = shell_indices
        .windows(2)
        .any(|indices| indices[1] != indices[0] + 1);
    if !has_first_stage_shell && !has_non_contiguous_shell_stages {
        return Ok(None);
    }

    let mut stages = Vec::with_capacity(pipeline.commands.len());
    for command in &pipeline.commands {
        if command_invocation_accepts_streaming_stdin(command, state) {
            if !supports_streaming_shell_stage_redirections(&command.redirections) {
                return Ok(None);
            }
            stages.push(ResolvedStreamingStage::Shell(command.clone()));
            continue;
        }

        let invocation = expand_invocation(command, state)?;
        if invocation.argv.is_empty() {
            return Err(ShellError::execution("empty command in pipeline"));
        }
        if !supports_streaming_external_redirections(&invocation.redirections) {
            return Ok(None);
        }

        let name = invocation
            .argv
            .first()
            .expect("checked non-empty invocation");
        if state.function(name).is_some()
            || state.alias(name).is_some()
            || state.abbreviation(name).is_some()
            || is_builtin(name)
        {
            return Ok(None);
        }

        let Some(path) = resolve_external_path(&invocation, state, name) else {
            return Ok(None);
        };
        stages.push(ResolvedStreamingStage::External(
            ResolvedExternalInvocation { invocation, path },
        ));
    }

    Ok(Some(stages))
}

fn try_run_streaming_external_shell_stage_pipeline(
    _graph: &CommandGraph,
    pipeline: &Pipeline,
    state: &mut ShellState,
    options: &ExecutionOptions,
) -> Result<Option<CommandOutcome>, ShellError> {
    let Some(resolved) = resolve_streaming_external_shell_stage_pipeline(pipeline, state)? else {
        return Ok(None);
    };

    let mut outcome = run_streaming_external_shell_stage_pipeline(
        &resolved.prefix,
        &resolved.shell_stages,
        &resolved.suffix,
        state,
        options,
    )?;
    apply_pipeline_negation(&mut outcome, pipeline.negated);
    Ok(Some(outcome))
}

fn resolve_streaming_external_shell_stage_pipeline(
    pipeline: &Pipeline,
    state: &mut ShellState,
) -> Result<Option<ResolvedShellStagePipeline>, ShellError> {
    if pipeline.commands.len() < 3 {
        return Ok(None);
    }

    let shell_indices = pipeline
        .commands
        .iter()
        .enumerate()
        .filter_map(|(index, invocation)| {
            command_invocation_accepts_streaming_stdin(invocation, state).then_some(index)
        })
        .collect::<Vec<_>>();
    let (Some(first_shell_index), Some(last_shell_index)) = (
        shell_indices.first().copied(),
        shell_indices.last().copied(),
    ) else {
        return Ok(None);
    };
    if first_shell_index == 0 || last_shell_index + 1 == pipeline.commands.len() {
        return Ok(None);
    }
    if shell_indices
        .windows(2)
        .any(|indices| indices[1] != indices[0] + 1)
    {
        return Ok(None);
    }

    let shell_stages = &pipeline.commands[first_shell_index..=last_shell_index];
    if shell_stages
        .iter()
        .any(|stage| !supports_streaming_shell_stage_redirections(&stage.redirections))
    {
        return Ok(None);
    }

    let prefix = resolve_external_pipeline_slice(&pipeline.commands[..first_shell_index], state)?;
    let suffix =
        resolve_external_pipeline_slice(&pipeline.commands[last_shell_index + 1..], state)?;
    let (Some(prefix), Some(suffix)) = (prefix, suffix) else {
        return Ok(None);
    };

    Ok(Some(ResolvedShellStagePipeline {
        prefix,
        shell_stages: shell_stages.to_vec(),
        suffix,
    }))
}

fn resolve_external_pipeline_slice(
    commands: &[CommandInvocation],
    state: &mut ShellState,
) -> Result<Option<Vec<ResolvedExternalInvocation>>, ShellError> {
    let mut resolved = Vec::with_capacity(commands.len());
    for command in commands {
        let invocation = expand_invocation(command, state)?;
        if invocation.argv.is_empty() {
            return Err(ShellError::execution("empty command in pipeline"));
        }
        if !supports_streaming_external_redirections(&invocation.redirections) {
            return Ok(None);
        }

        let name = invocation
            .argv
            .first()
            .expect("checked non-empty invocation");
        if state.function(name).is_some()
            || state.alias(name).is_some()
            || state.abbreviation(name).is_some()
            || is_builtin(name)
        {
            return Ok(None);
        }

        let Some(path) = resolve_external_path(&invocation, state, name) else {
            return Ok(None);
        };
        resolved.push(ResolvedExternalInvocation { invocation, path });
    }
    Ok(Some(resolved))
}

fn resolve_streaming_external_prefix_to_final_read(
    commands: &[ExpandedInvocation],
    state: &mut ShellState,
) -> Option<(Vec<ResolvedExternalInvocation>, ExpandedInvocation)> {
    let (final_read, prefix) = commands.split_last()?;
    if prefix.is_empty()
        || final_read.argv.first().map(String::as_str) != Some("read")
        || !final_read.redirections.is_empty()
        || state.function("read").is_some()
        || state.alias("read").is_some()
        || state.abbreviation("read").is_some()
    {
        return None;
    }

    let mut resolved = Vec::with_capacity(prefix.len());
    for invocation in prefix {
        if !supports_streaming_external_redirections(&invocation.redirections) {
            return None;
        }

        let name = invocation.argv.first()?;
        if state.function(name).is_some()
            || state.alias(name).is_some()
            || state.abbreviation(name).is_some()
            || is_builtin(name)
        {
            return None;
        }

        let path = resolve_external_path(invocation, state, name)?;
        resolved.push(ResolvedExternalInvocation {
            invocation: invocation.clone(),
            path,
        });
    }

    Some((resolved, final_read.clone()))
}

fn resolve_streaming_external_pipeline(
    commands: &[ExpandedInvocation],
    state: &mut ShellState,
) -> Option<Vec<ResolvedExternalInvocation>> {
    let mut resolved = Vec::with_capacity(commands.len());

    for invocation in commands {
        if !supports_streaming_external_redirections(&invocation.redirections) {
            return None;
        }

        let name = invocation.argv.first()?;
        if state.function(name).is_some()
            || state.alias(name).is_some()
            || state.abbreviation(name).is_some()
            || is_builtin(name)
        {
            return None;
        }

        let path = resolve_external_path(invocation, state, name)?;
        resolved.push(ResolvedExternalInvocation {
            invocation: invocation.clone(),
            path,
        });
    }

    Some(resolved)
}

fn supports_streaming_external_redirections(redirections: &[ExpandedRedirection]) -> bool {
    redirections.iter().all(
        |redirection| match (&redirection.mode, &redirection.target) {
            (RedirectionMode::Read, ExpandedRedirectionTarget::Path(_)) => redirection.fd == 0,
            (
                RedirectionMode::Write | RedirectionMode::WriteClobber | RedirectionMode::Append,
                ExpandedRedirectionTarget::Path(_),
            ) => redirection.fd == 1 || redirection.fd == 2,
            (RedirectionMode::WriteBoth, ExpandedRedirectionTarget::Path(_)) => true,
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(target_fd)) => {
                (redirection.fd == 1 || redirection.fd == 2) && (*target_fd == 1 || *target_fd == 2)
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) => redirection.fd <= 2,
            _ => false,
        },
    )
}

fn spawn_resolved_external_stage(
    resolved: &ResolvedExternalInvocation,
    state: &ShellState,
    stdin: ExternalStageStdin,
    inherit_stdout: bool,
    inherit_stderr: bool,
) -> Result<(Child, StreamingOutputReaders), ShellError> {
    let mut command = Command::new(&resolved.path);
    command.args(&resolved.invocation.argv[1..]);
    command.current_dir(state.cwd());
    state.configure_child_env(&mut command);
    for assignment in &resolved.invocation.assignments {
        command.env(&assignment.name, &assignment.value);
    }
    command.stdin(stdin.into_stdio());

    let output_readers = apply_streaming_external_redirections(
        &mut command,
        &resolved.invocation.redirections,
        state.noclobber(),
        inherit_stdout,
        inherit_stderr,
    )?;
    let child = command.spawn()?;
    Ok((child, output_readers))
}

fn spawn_resolved_external_stage_with_targets(
    resolved: &ResolvedExternalInvocation,
    state: &ShellState,
    stdin: ExternalStageStdin,
    stdout_target: StreamingOutputTarget,
    stderr_target: StreamingOutputTarget,
) -> Result<Child, ShellError> {
    let mut command = Command::new(&resolved.path);
    command.args(&resolved.invocation.argv[1..]);
    command.current_dir(state.cwd());
    state.configure_child_env(&mut command);
    for assignment in &resolved.invocation.assignments {
        command.env(&assignment.name, &assignment.value);
    }
    command.stdin(stdin.into_stdio());

    let readers = apply_streaming_external_redirections_with_targets(
        &mut command,
        &resolved.invocation.redirections,
        state.noclobber(),
        false,
        false,
        Some(stdout_target),
        Some(stderr_target),
    )?;
    debug_assert!(readers.stdout.is_none());
    debug_assert!(readers.stderr.is_none());
    command.spawn().map_err(ShellError::from)
}

struct MaterializedExternalPipelineRouting {
    final_stdout: StreamingOutputTarget,
    stage_stderr: StreamingOutputTarget,
    stdout_capture: Option<CaptureJoinHandle>,
    stderr_capture: Option<CaptureJoinHandle>,
    exit_guard: DirectStageExitGuard,
}

struct DirectStageExitGuard {
    exited: Arc<AtomicBool>,
}

impl DirectStageExitGuard {
    fn signal(&self) {
        self.exited.store(true, Ordering::Release);
    }
}

impl Drop for DirectStageExitGuard {
    fn drop(&mut self) {
        self.signal();
    }
}

impl MaterializedExternalPipelineRouting {
    fn capture_routing(&self) -> io::Result<InheritedCaptureRouting> {
        Ok(InheritedCaptureRouting {
            stdout: capture_destination_from_streaming_target(&self.final_stdout)?,
            stderr: capture_destination_from_streaming_target(&self.stage_stderr)?,
        })
    }

    fn finish(self, exit_code: i32) -> Result<CommandOutcome, ShellError> {
        let Self {
            final_stdout,
            stage_stderr,
            stdout_capture,
            stderr_capture,
            exit_guard,
        } = self;
        exit_guard.signal();
        drop(final_stdout);
        drop(stage_stderr);
        let stdout = match stdout_capture {
            Some(handle) => join_capture_reader(handle)?,
            None => CapturedStream::complete(Vec::new()),
        };
        let stderr = match stderr_capture {
            Some(handle) => join_capture_reader(handle)?,
            None => CapturedStream::complete(Vec::new()),
        };
        Ok(CommandOutcome::captured_from_streams(
            exit_code, stdout, stderr,
        ))
    }
}

fn capture_destination_from_streaming_target(
    target: &StreamingOutputTarget,
) -> io::Result<CaptureDestination> {
    match target {
        StreamingOutputTarget::File(file) => {
            Ok(CaptureDestination::File(Arc::new(file.try_clone()?)))
        }
        StreamingOutputTarget::Inherit(InheritedOutput::Stdout) => Ok(CaptureDestination::Stdout),
        StreamingOutputTarget::Inherit(InheritedOutput::Stderr) => Ok(CaptureDestination::Stderr),
        StreamingOutputTarget::Null => Ok(CaptureDestination::Discard),
        StreamingOutputTarget::Pipe { kind, writer } => Ok(CaptureDestination::Pipe {
            kind: *kind,
            writer: Arc::new(writer.try_clone()?),
        }),
    }
}

fn materialize_external_pipeline_routing(
    state: &ShellState,
    capture_logical_streams: bool,
    bounded_capture: bool,
) -> Result<MaterializedExternalPipelineRouting, ShellError> {
    let routing = inherited_capture_routing();
    let direct_stages_exited = Arc::new(AtomicBool::new(false));
    let (base_stdout, stdout_capture) = materialize_pipeline_logical_stream(
        state,
        OutputStream::Stdout,
        capture_logical_streams,
        bounded_capture,
        Arc::clone(&direct_stages_exited),
    )?;
    let (base_stderr, stderr_capture) = materialize_pipeline_logical_stream(
        state,
        OutputStream::Stderr,
        capture_logical_streams,
        bounded_capture,
        Arc::clone(&direct_stages_exited),
    )?;
    let final_stdout =
        streaming_target_for_capture_destination(&routing.stdout, &base_stdout, &base_stderr)?;
    let stage_stderr =
        streaming_target_for_capture_destination(&routing.stderr, &base_stdout, &base_stderr)?;
    drop(base_stdout);
    drop(base_stderr);
    Ok(MaterializedExternalPipelineRouting {
        final_stdout,
        stage_stderr,
        stdout_capture,
        stderr_capture,
        exit_guard: DirectStageExitGuard {
            exited: direct_stages_exited,
        },
    })
}

fn materialize_pipeline_logical_stream(
    state: &ShellState,
    stream: OutputStream,
    capture: bool,
    bounded_capture: bool,
    direct_stages_exited: Arc<AtomicBool>,
) -> Result<(StreamingOutputTarget, Option<CaptureJoinHandle>), ShellError> {
    if !capture {
        let inherited = match stream {
            OutputStream::Stdout => InheritedOutput::Stdout,
            OutputStream::Stderr => InheritedOutput::Stderr,
        };
        return Ok((StreamingOutputTarget::Inherit(inherited), None));
    }

    let (reader, writer) = io::pipe()?;
    let kind = match stream {
        OutputStream::Stdout => StreamingPipeKind::Stdout,
        OutputStream::Stderr => StreamingPipeKind::Stderr,
    };
    let spool = if bounded_capture {
        state
            .create_trace_spool(match stream {
                OutputStream::Stdout => "out",
                OutputStream::Stderr => "err",
            })
            .ok()
    } else {
        None
    };
    let capture_handle =
        spawn_exit_aware_capture_reader(reader, spool, bounded_capture, direct_stages_exited)?;
    Ok((
        StreamingOutputTarget::Pipe { kind, writer },
        Some(capture_handle),
    ))
}

fn streaming_target_for_capture_destination(
    destination: &CaptureDestination,
    base_stdout: &StreamingOutputTarget,
    base_stderr: &StreamingOutputTarget,
) -> io::Result<StreamingOutputTarget> {
    match destination {
        CaptureDestination::Stdout => base_stdout.try_clone(),
        CaptureDestination::Stderr => base_stderr.try_clone(),
        CaptureDestination::File(file) => file.try_clone().map(StreamingOutputTarget::File),
        CaptureDestination::Pipe { kind, writer } => Ok(StreamingOutputTarget::Pipe {
            kind: *kind,
            writer: writer.try_clone()?,
        }),
        CaptureDestination::Discard => Ok(StreamingOutputTarget::Null),
    }
}

fn streaming_target_from_materialized_destination(
    destination: &CaptureDestination,
) -> io::Result<StreamingOutputTarget> {
    match destination {
        CaptureDestination::Stdout => Ok(StreamingOutputTarget::Inherit(InheritedOutput::Stdout)),
        CaptureDestination::Stderr => Ok(StreamingOutputTarget::Inherit(InheritedOutput::Stderr)),
        CaptureDestination::File(file) => file.try_clone().map(StreamingOutputTarget::File),
        CaptureDestination::Pipe { kind, writer } => Ok(StreamingOutputTarget::Pipe {
            kind: *kind,
            writer: writer.try_clone()?,
        }),
        CaptureDestination::Discard => Ok(StreamingOutputTarget::Null),
    }
}

fn run_live_streaming_external_pipeline(
    commands: &[ResolvedExternalInvocation],
    state: &mut ShellState,
    capture_logical_streams: bool,
    bounded_capture: bool,
) -> Result<CommandOutcome, ShellError> {
    let last_index = commands.len().saturating_sub(1);
    let materialized =
        materialize_external_pipeline_routing(state, capture_logical_streams, bounded_capture)?;
    let MaterializedExternalPipelineRouting {
        final_stdout,
        stage_stderr,
        stdout_capture,
        stderr_capture,
        exit_guard,
    } = materialized;
    let mut children = Vec::with_capacity(commands.len());
    let mut previous_stdout = None;

    for (index, resolved) in commands.iter().enumerate() {
        let stdin = previous_stdout
            .take()
            .map_or(ExternalStageStdin::Inherit, ExternalStageStdin::Pipe);
        let stdout_target = if index == last_index {
            match final_stdout.try_clone() {
                Ok(target) => target,
                Err(error) => {
                    terminate_children(&mut children);
                    return Err(error.into());
                }
            }
        } else {
            let (reader, writer) = match io::pipe() {
                Ok(pipe) => pipe,
                Err(error) => {
                    terminate_children(&mut children);
                    return Err(error.into());
                }
            };
            previous_stdout = Some(reader);
            StreamingOutputTarget::Pipe {
                kind: StreamingPipeKind::Stdout,
                writer,
            }
        };
        let stderr_target = match stage_stderr.try_clone() {
            Ok(target) => target,
            Err(error) => {
                terminate_children(&mut children);
                return Err(error.into());
            }
        };
        let child = match spawn_resolved_external_stage_with_targets(
            resolved,
            state,
            stdin,
            stdout_target,
            stderr_target,
        ) {
            Ok(child) => child,
            Err(error) => {
                terminate_children(&mut children);
                return Err(error);
            }
        };
        children.push(child);
    }

    drop(previous_stdout);
    drop(final_stdout);
    drop(stage_stderr);

    let mut exit_codes = Vec::with_capacity(children.len());
    for child in &mut children {
        exit_codes.push(exit_status_code(child.wait()?));
    }
    let exit_code = pipeline_exit_code(&exit_codes, state.pipefail());
    record_pipestatus(state, &exit_codes);
    exit_guard.signal();

    let stdout = match stdout_capture {
        Some(handle) => join_capture_reader(handle)?,
        None => CapturedStream::complete(Vec::new()),
    };
    let stderr = match stderr_capture {
        Some(handle) => join_capture_reader(handle)?,
        None => CapturedStream::complete(Vec::new()),
    };
    Ok(CommandOutcome::captured_from_streams(
        exit_code, stdout, stderr,
    ))
}

fn run_streaming_external_pipeline(
    commands: &[ResolvedExternalInvocation],
    state: &mut ShellState,
    inherit_raw_output: bool,
    capture_logical_streams: bool,
    bounded_capture: bool,
) -> Result<CommandOutcome, ShellError> {
    if !inherited_capture_routing().is_default() {
        return run_live_streaming_external_pipeline(
            commands,
            state,
            capture_logical_streams,
            bounded_capture,
        );
    }
    let last_index = commands.len().saturating_sub(1);
    let mut children: Vec<Child> = Vec::with_capacity(commands.len());
    let mut previous_stdout: Option<io::PipeReader> = None;
    let mut previous_pipe_closed = false;
    let mut stderr_handles = Vec::with_capacity(commands.len());
    let mut final_stdout_handle = None;
    let direct_stages_exited = Arc::new(AtomicBool::new(false));
    let exit_guard = DirectStageExitGuard {
        exited: Arc::clone(&direct_stages_exited),
    };

    for (index, resolved) in commands.iter().enumerate() {
        let stderr_spool = if bounded_capture {
            state.create_trace_spool("err").ok()
        } else {
            None
        };
        let stdout_spool = if bounded_capture && index == last_index {
            state.create_trace_spool("out").ok()
        } else {
            None
        };
        let stdin = if let Some(stdout) = previous_stdout.take() {
            ExternalStageStdin::Pipe(stdout)
        } else if previous_pipe_closed {
            previous_pipe_closed = false;
            ExternalStageStdin::Null
        } else {
            ExternalStageStdin::Inherit
        };

        let (child, output_readers) = match spawn_resolved_external_stage(
            resolved,
            state,
            stdin,
            inherit_raw_output && index == last_index,
            inherit_raw_output,
        ) {
            Ok(child) => child,
            Err(error) => {
                terminate_children(&mut children);
                return Err(error);
            }
        };

        children.push(child);

        if let Some(stderr) = output_readers.stderr {
            match spawn_exit_aware_capture_reader(
                stderr,
                stderr_spool,
                bounded_capture,
                Arc::clone(&direct_stages_exited),
            ) {
                Ok(handle) => stderr_handles.push(handle),
                Err(error) => {
                    terminate_children(&mut children);
                    return Err(error.into());
                }
            }
        }

        if let Some(stdout) = output_readers.stdout {
            if index == last_index {
                final_stdout_handle = match spawn_exit_aware_capture_reader(
                    stdout,
                    stdout_spool,
                    bounded_capture,
                    Arc::clone(&direct_stages_exited),
                ) {
                    Ok(handle) => Some(handle),
                    Err(error) => {
                        terminate_children(&mut children);
                        return Err(error.into());
                    }
                };
            } else {
                previous_stdout = Some(stdout);
                previous_pipe_closed = false;
            }
        } else if index != last_index {
            previous_pipe_closed = true;
        }
    }

    drop(previous_stdout);

    let mut last_exit_code = 0;
    let mut exit_codes = Vec::with_capacity(children.len());
    for (index, child) in children.iter_mut().enumerate() {
        let status = child.wait()?;
        let exit_code = exit_status_code(status);
        exit_codes.push(exit_code);
        if index == last_index {
            last_exit_code = exit_code;
        }
    }
    let exit_code = pipeline_exit_code(&exit_codes, state.pipefail());
    record_pipestatus(state, &exit_codes);
    exit_guard.signal();

    let stdout = match final_stdout_handle {
        Some(handle) => join_capture_reader(handle)?,
        None => CapturedStream::complete(Vec::new()),
    };
    let mut stderr_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());
    for handle in stderr_handles {
        let stderr = join_capture_reader(handle)?;
        let mut stage =
            CommandOutcome::captured_from_streams(0, CapturedStream::complete(Vec::new()), stderr);
        stderr_outcome.append_stderr(&mut stage)?;
    }

    let mut outcome = CommandOutcome::captured_with_exact(
        if state.pipefail() {
            exit_code
        } else {
            last_exit_code
        },
        stdout.preview,
        stderr_outcome.stderr,
        stdout.exact,
        None,
    );
    outcome.exact_stderr = stderr_outcome.exact_stderr;
    outcome.stdout_preview_complete = stdout.preview_complete;
    outcome.stderr_preview_complete = stderr_outcome.stderr_preview_complete;
    Ok(outcome)
}

fn run_live_streaming_mixed_shell_stage_pipeline(
    stages: &[ResolvedStreamingStage],
    state: &ShellState,
    options: &ExecutionOptions,
) -> Result<CommandOutcome, ShellError> {
    let bounded_capture = options.output_mode.should_capture()
        && options.output_mode != OutputMode::Rich
        && !state.exact_capture_enabled();
    let materialized = materialize_external_pipeline_routing(
        state,
        options.output_mode.should_capture(),
        bounded_capture,
    )?;
    let base_routing = materialized.capture_routing()?;
    let last_index = stages.len().saturating_sub(1);
    let mut running_stages = Vec::with_capacity(stages.len());
    let mut previous_stdout = None;

    for (index, stage) in stages.iter().enumerate() {
        let stage_stdin = previous_stdout.take();
        let mut stage_routing = base_routing.clone();
        if index != last_index {
            let (reader, writer) = match io::pipe() {
                Ok(pipe) => pipe,
                Err(error) => {
                    terminate_running_streaming_stages(&mut running_stages, state);
                    return Err(error.into());
                }
            };
            stage_routing.stdout = CaptureDestination::Pipe {
                kind: StreamingPipeKind::Stdout,
                writer: Arc::new(writer),
            };
            previous_stdout = Some(reader);
        }

        match stage {
            ResolvedStreamingStage::External(resolved) => {
                let stdin =
                    stage_stdin.map_or(ExternalStageStdin::Inherit, ExternalStageStdin::Pipe);
                let stdout_target =
                    match streaming_target_from_materialized_destination(&stage_routing.stdout) {
                        Ok(target) => target,
                        Err(error) => {
                            terminate_running_streaming_stages(&mut running_stages, state);
                            return Err(error.into());
                        }
                    };
                let stderr_target =
                    match streaming_target_from_materialized_destination(&stage_routing.stderr) {
                        Ok(target) => target,
                        Err(error) => {
                            terminate_running_streaming_stages(&mut running_stages, state);
                            return Err(error.into());
                        }
                    };
                let child = match spawn_resolved_external_stage_with_targets(
                    resolved,
                    state,
                    stdin,
                    stdout_target,
                    stderr_target,
                ) {
                    Ok(child) => child,
                    Err(error) => {
                        terminate_running_streaming_stages(&mut running_stages, state);
                        return Err(error);
                    }
                };
                running_stages.push(RunningStreamingStage::External(child));
            }
            ResolvedStreamingStage::Shell(shell_stage) => {
                running_stages.push(RunningStreamingStage::Shell(
                    spawn_shell_pipeline_stage_with_routing(
                        shell_stage.clone(),
                        state.clone(),
                        stage_stdin,
                        stage_routing,
                        options.output_mode,
                        options.allow_process_replacement,
                    ),
                ));
            }
        }
    }

    drop(previous_stdout);
    drop(base_routing);

    let mut exit_codes = Vec::with_capacity(running_stages.len());
    let mut first_shell_error = None;
    for stage in running_stages {
        match stage {
            RunningStreamingStage::External(mut child) => {
                exit_codes.push(exit_status_code(child.wait()?));
            }
            RunningStreamingStage::Shell(shell) => match shell
                .thread
                .join()
                .map_err(|_| ShellError::execution("pipeline shell stage thread panicked"))
            {
                Ok(Ok(outcome)) => {
                    debug_assert!(outcome.stdout.is_empty());
                    debug_assert!(outcome.stderr.is_empty());
                    exit_codes.push(outcome.exit_code);
                }
                Ok(Err(error)) | Err(error) => {
                    exit_codes.push(1);
                    if first_shell_error.is_none() {
                        first_shell_error = Some(error);
                    }
                }
            },
        }
    }
    if let Some(error) = first_shell_error {
        return Err(error);
    }

    materialized.finish(pipeline_exit_code(&exit_codes, state.pipefail()))
}

fn run_streaming_mixed_shell_stage_pipeline(
    stages: &[ResolvedStreamingStage],
    state: &ShellState,
    options: &ExecutionOptions,
) -> Result<CommandOutcome, ShellError> {
    if !inherited_capture_routing().is_default() {
        return run_live_streaming_mixed_shell_stage_pipeline(stages, state, options);
    }
    let last_index = stages.len().saturating_sub(1);
    let bounded_capture = options.output_mode.should_capture()
        && options.output_mode != OutputMode::Rich
        && !state.exact_capture_enabled();
    let mut final_stdout_spool = if bounded_capture {
        state.create_trace_spool("out").ok()
    } else {
        None
    };
    let mut running_stages = Vec::with_capacity(stages.len());
    let mut previous_stdout: Option<io::PipeReader> = None;
    let mut previous_pipe_closed = false;
    let mut stderr_handles = Vec::with_capacity(stages.len());
    let mut final_stdout_handle = None;
    let direct_stages_exited = Arc::new(AtomicBool::new(false));
    let exit_guard = DirectStageExitGuard {
        exited: Arc::clone(&direct_stages_exited),
    };

    for (index, stage) in stages.iter().enumerate() {
        match stage {
            ResolvedStreamingStage::External(resolved) => {
                let stderr_spool = if bounded_capture {
                    state.create_trace_spool("err").ok()
                } else {
                    None
                };
                let stdin = if let Some(stdout) = previous_stdout.take() {
                    ExternalStageStdin::Pipe(stdout)
                } else if previous_pipe_closed {
                    previous_pipe_closed = false;
                    ExternalStageStdin::Null
                } else {
                    ExternalStageStdin::Inherit
                };

                let (child, output_readers) =
                    match spawn_resolved_external_stage(resolved, state, stdin, false, false) {
                        Ok(child) => child,
                        Err(error) => {
                            terminate_running_streaming_stages(&mut running_stages, state);
                            return Err(error);
                        }
                    };
                running_stages.push(RunningStreamingStage::External(child));

                if let Some(stderr) = output_readers.stderr {
                    let handle = match spawn_exit_aware_capture_reader(
                        stderr,
                        stderr_spool,
                        bounded_capture,
                        Arc::clone(&direct_stages_exited),
                    ) {
                        Ok(handle) => handle,
                        Err(error) => {
                            terminate_running_streaming_stages(&mut running_stages, state);
                            return Err(error.into());
                        }
                    };
                    stderr_handles.push(handle);
                }

                if let Some(stdout) = output_readers.stdout {
                    if index == last_index {
                        let spool = final_stdout_spool.take();
                        final_stdout_handle = Some(
                            match spawn_exit_aware_capture_reader(
                                stdout,
                                spool,
                                bounded_capture,
                                Arc::clone(&direct_stages_exited),
                            ) {
                                Ok(handle) => handle,
                                Err(error) => {
                                    terminate_running_streaming_stages(&mut running_stages, state);
                                    return Err(error.into());
                                }
                            },
                        );
                    } else {
                        previous_stdout = Some(stdout);
                        previous_pipe_closed = false;
                    }
                } else if index != last_index {
                    previous_pipe_closed = true;
                }
            }
            ResolvedStreamingStage::Shell(shell_stage) => {
                let stage_stdin = previous_stdout.take();
                if previous_pipe_closed {
                    previous_pipe_closed = false;
                }
                let (stdout_reader, stdout_writer) = match io::pipe() {
                    Ok(pipe) => pipe,
                    Err(error) => {
                        terminate_running_streaming_stages(&mut running_stages, state);
                        return Err(error.into());
                    }
                };
                if index == last_index {
                    let spool = final_stdout_spool.take();
                    final_stdout_handle = Some(
                        match spawn_exit_aware_capture_reader(
                            stdout_reader,
                            spool,
                            bounded_capture,
                            Arc::clone(&direct_stages_exited),
                        ) {
                            Ok(handle) => handle,
                            Err(error) => {
                                terminate_running_streaming_stages(&mut running_stages, state);
                                return Err(error.into());
                            }
                        },
                    );
                } else {
                    previous_stdout = Some(stdout_reader);
                    previous_pipe_closed = false;
                }

                running_stages.push(RunningStreamingStage::Shell(spawn_shell_pipeline_stage(
                    shell_stage.clone(),
                    state.clone(),
                    stage_stdin,
                    stdout_writer,
                    options.output_mode,
                    options.allow_process_replacement,
                )));
            }
        }
    }

    drop(previous_stdout);

    let mut exit_codes = Vec::with_capacity(running_stages.len());
    let mut stderr_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());
    let mut first_shell_error = None;
    for stage in running_stages {
        match stage {
            RunningStreamingStage::External(mut child) => {
                let status = child.wait()?;
                exit_codes.push(exit_status_code(status));
            }
            RunningStreamingStage::Shell(shell) => match shell
                .thread
                .join()
                .map_err(|_| ShellError::execution("pipeline shell stage thread panicked"))
            {
                Ok(Ok(mut outcome)) => {
                    exit_codes.push(outcome.exit_code);
                    stderr_outcome.append_stderr(&mut outcome)?;
                }
                Ok(Err(error)) => {
                    exit_codes.push(1);
                    if first_shell_error.is_none() {
                        first_shell_error = Some(error);
                    }
                }
                Err(error) => {
                    exit_codes.push(1);
                    if first_shell_error.is_none() {
                        first_shell_error = Some(error);
                    }
                }
            },
        }
    }
    exit_guard.signal();

    if let Some(error) = first_shell_error {
        return Err(error);
    }

    let stdout = match final_stdout_handle {
        Some(handle) => join_capture_reader(handle)?,
        None => CapturedStream::complete(Vec::new()),
    };
    for handle in stderr_handles {
        let stderr = join_capture_reader(handle)?;
        let mut stage =
            CommandOutcome::captured_from_streams(0, CapturedStream::complete(Vec::new()), stderr);
        stderr_outcome.append_stderr(&mut stage)?;
    }

    let exit_code = pipeline_exit_code(&exit_codes, state.pipefail());
    let mut outcome = CommandOutcome::captured_with_exact(
        exit_code,
        stdout.preview,
        stderr_outcome.stderr,
        stdout.exact,
        None,
    );
    outcome.exact_stderr = stderr_outcome.exact_stderr;
    outcome.stdout_preview_complete = stdout.preview_complete;
    outcome.stderr_preview_complete = stderr_outcome.stderr_preview_complete;
    Ok(outcome)
}

fn terminate_running_streaming_stages(
    stages: &mut Vec<RunningStreamingStage>,
    _state: &ShellState,
) {
    let mut interrupted = Vec::new();
    for stage in stages.iter() {
        if let RunningStreamingStage::Shell(shell) = stage {
            let was_set = shell.interrupt.swap(true, Ordering::AcqRel);
            interrupted.push((Arc::clone(&shell.interrupt), was_set));
        }
    }

    for stage in stages.iter_mut() {
        if let RunningStreamingStage::External(child) = stage {
            let _ = child.kill();
        }
    }

    for stage in stages.drain(..) {
        match stage {
            RunningStreamingStage::External(mut child) => {
                let _ = child.wait();
            }
            RunningStreamingStage::Shell(shell) => {
                let _ = shell.thread.join();
            }
        }
    }

    for (interrupt, was_set) in interrupted {
        if !was_set {
            interrupt.store(false, Ordering::Release);
        }
    }
}

struct RunningExternalPrefix {
    children: Vec<Child>,
    stderr_handles: Vec<CaptureJoinHandle>,
    final_stdout: Option<io::PipeReader>,
}

struct RunningExternalSuffix {
    children: Vec<Child>,
    stderr_handles: Vec<CaptureJoinHandle>,
    final_stdout_handle: Option<CaptureJoinHandle>,
}

type CaptureJoinHandle = std::thread::JoinHandle<io::Result<CapturedStream>>;

struct ResolvedShellStagePipeline {
    prefix: Vec<ResolvedExternalInvocation>,
    shell_stages: Vec<CommandInvocation>,
    suffix: Vec<ResolvedExternalInvocation>,
}

fn run_streaming_external_shell_stage_pipeline(
    prefix: &[ResolvedExternalInvocation],
    shell_stages: &[CommandInvocation],
    suffix: &[ResolvedExternalInvocation],
    state: &ShellState,
    options: &ExecutionOptions,
) -> Result<CommandOutcome, ShellError> {
    if !inherited_capture_routing().is_default() {
        let stages = prefix
            .iter()
            .cloned()
            .map(ResolvedStreamingStage::External)
            .chain(
                shell_stages
                    .iter()
                    .cloned()
                    .map(ResolvedStreamingStage::Shell),
            )
            .chain(suffix.iter().cloned().map(ResolvedStreamingStage::External))
            .collect::<Vec<_>>();
        return run_live_streaming_mixed_shell_stage_pipeline(&stages, state, options);
    }

    let bounded_capture = options.output_mode.should_capture()
        && options.output_mode != OutputMode::Rich
        && !state.exact_capture_enabled();
    let direct_stages_exited = Arc::new(AtomicBool::new(false));
    let exit_guard = DirectStageExitGuard {
        exited: Arc::clone(&direct_stages_exited),
    };
    let mut prefix = spawn_external_prefix_for_shell_stage(
        prefix,
        state,
        bounded_capture,
        Arc::clone(&direct_stages_exited),
    )?;
    let mut shell_stdin = prefix.final_stdout.take();
    let mut stage_specs = Vec::with_capacity(shell_stages.len());
    let mut suffix_stdin = None;

    for (index, shell_stage) in shell_stages.iter().enumerate() {
        let stage_stdin = shell_stdin.take();
        let (shell_stdout_reader, shell_stdout_writer) = match io::pipe() {
            Ok(pipe) => pipe,
            Err(error) => {
                terminate_children(&mut prefix.children);
                return Err(error.into());
            }
        };
        if index + 1 == shell_stages.len() {
            suffix_stdin = Some(shell_stdout_reader);
        } else {
            shell_stdin = Some(shell_stdout_reader);
        }
        stage_specs.push((shell_stage.clone(), stage_stdin, shell_stdout_writer));
    }

    let Some(suffix_stdin) = suffix_stdin else {
        terminate_children(&mut prefix.children);
        return Err(ShellError::execution("missing shell pipeline output"));
    };
    let mut suffix = match spawn_external_suffix_from_shell_stage(
        suffix,
        state,
        suffix_stdin,
        bounded_capture,
        Arc::clone(&direct_stages_exited),
    ) {
        Ok(suffix) => suffix,
        Err(error) => {
            terminate_children(&mut prefix.children);
            return Err(error);
        }
    };
    let shell_threads = stage_specs
        .into_iter()
        .map(|(shell_stage, stage_stdin, shell_stdout_writer)| {
            spawn_shell_pipeline_stage(
                shell_stage,
                state.clone(),
                stage_stdin,
                shell_stdout_writer,
                options.output_mode,
                options.allow_process_replacement,
            )
        })
        .collect::<Vec<_>>();

    let mut shell_outcomes = Vec::with_capacity(shell_threads.len());
    for thread in shell_threads {
        shell_outcomes.push(
            thread
                .thread
                .join()
                .map_err(|_| ShellError::execution("pipeline shell stage thread panicked"))??,
        );
    }

    let mut exit_codes =
        Vec::with_capacity(prefix.children.len() + shell_outcomes.len() + suffix.children.len());
    for child in &mut prefix.children {
        let status = child.wait()?;
        exit_codes.push(exit_status_code(status));
    }
    for shell_outcome in &shell_outcomes {
        exit_codes.push(shell_outcome.exit_code);
    }
    for child in &mut suffix.children {
        let status = child.wait()?;
        exit_codes.push(exit_status_code(status));
    }
    exit_guard.signal();

    let stdout = match suffix.final_stdout_handle {
        Some(handle) => join_capture_reader(handle)?,
        None => CapturedStream::complete(Vec::new()),
    };
    let mut stderr_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());
    for mut shell_outcome in shell_outcomes {
        stderr_outcome.append_stderr(&mut shell_outcome)?;
    }
    for handle in prefix.stderr_handles {
        let stderr = join_capture_reader(handle)?;
        let mut stage =
            CommandOutcome::captured_from_streams(0, CapturedStream::complete(Vec::new()), stderr);
        stderr_outcome.append_stderr(&mut stage)?;
    }
    for handle in suffix.stderr_handles {
        let stderr = join_capture_reader(handle)?;
        let mut stage =
            CommandOutcome::captured_from_streams(0, CapturedStream::complete(Vec::new()), stderr);
        stderr_outcome.append_stderr(&mut stage)?;
    }

    let exit_code = pipeline_exit_code(&exit_codes, state.pipefail());
    let mut outcome = CommandOutcome::captured_with_exact(
        exit_code,
        stdout.preview,
        stderr_outcome.stderr,
        stdout.exact,
        None,
    );
    outcome.exact_stderr = stderr_outcome.exact_stderr;
    outcome.stdout_preview_complete = stdout.preview_complete;
    outcome.stderr_preview_complete = stderr_outcome.stderr_preview_complete;
    Ok(outcome)
}

fn spawn_shell_pipeline_stage(
    shell_stage: CommandInvocation,
    mut stage_state: ShellState,
    shell_stdin: Option<io::PipeReader>,
    shell_stdout_writer: io::PipeWriter,
    output_mode: OutputMode,
    allow_process_replacement: bool,
) -> RunningShellStage {
    let routing = inherited_capture_routing();
    let interrupt = stage_state.interrupt_flag();
    let thread = std::thread::spawn(move || {
        with_cancellable_shell_stage(|| {
            with_stream_raw_to_parent(!output_mode.should_capture(), || {
                with_inherited_capture_routing(routing, || {
                    let run_stage = |state: &mut ShellState| {
                        run_with_streaming_stdout(state, shell_stdout_writer, |state| {
                            let mut outcome = run_pipeline_command_invocation(
                                &shell_stage,
                                state,
                                output_mode,
                                None,
                                true,
                                true,
                                allow_process_replacement,
                            )?;
                            emit_streaming_stdout(state, &mut outcome)?;
                            Ok(outcome)
                        })
                    };

                    if let Some(stdin) = shell_stdin {
                        run_with_streaming_stdin(&mut stage_state, stdin, run_stage)
                    } else {
                        run_with_buffered_stdin(&mut stage_state, Some(&[]), run_stage)
                    }
                })
            })
        })
    });
    RunningShellStage { thread, interrupt }
}

fn spawn_shell_pipeline_stage_with_routing(
    shell_stage: CommandInvocation,
    mut stage_state: ShellState,
    shell_stdin: Option<io::PipeReader>,
    routing: InheritedCaptureRouting,
    output_mode: OutputMode,
    allow_process_replacement: bool,
) -> RunningShellStage {
    let interrupt = stage_state.interrupt_flag();
    let thread = std::thread::spawn(move || {
        with_cancellable_shell_stage(|| {
            with_stream_raw_to_parent(!output_mode.should_capture(), || {
                with_inherited_capture_routing(routing, || {
                    let run_stage = |state: &mut ShellState| {
                        let mut outcome = run_pipeline_command_invocation(
                            &shell_stage,
                            state,
                            output_mode,
                            None,
                            true,
                            true,
                            allow_process_replacement,
                        )?;
                        // Diagnostics synthesized above an invocation still need the
                        // stage fd table even when the invocation itself already wrote
                        // its streams live.
                        apply_builtin_redirections(&mut outcome, &[], state)?;
                        Ok(outcome)
                    };

                    if let Some(stdin) = shell_stdin {
                        run_with_streaming_stdin(&mut stage_state, stdin, run_stage)
                    } else {
                        run_with_buffered_stdin(&mut stage_state, Some(&[]), run_stage)
                    }
                })
            })
        })
    });
    RunningShellStage { thread, interrupt }
}

fn spawn_external_prefix_for_shell_stage(
    commands: &[ResolvedExternalInvocation],
    state: &ShellState,
    bounded_capture: bool,
    direct_stages_exited: Arc<AtomicBool>,
) -> Result<RunningExternalPrefix, ShellError> {
    let last_index = commands.len().saturating_sub(1);
    let mut children: Vec<Child> = Vec::with_capacity(commands.len());
    let mut previous_stdout: Option<io::PipeReader> = None;
    let mut previous_pipe_closed = false;
    let mut stderr_handles = Vec::with_capacity(commands.len());
    let mut final_stdout = None;

    for (index, resolved) in commands.iter().enumerate() {
        let stderr_spool = if bounded_capture {
            state.create_trace_spool("err").ok()
        } else {
            None
        };
        let stdin = if let Some(stdout) = previous_stdout.take() {
            ExternalStageStdin::Pipe(stdout)
        } else if previous_pipe_closed {
            previous_pipe_closed = false;
            ExternalStageStdin::Null
        } else {
            ExternalStageStdin::Inherit
        };

        let (child, output_readers) =
            match spawn_resolved_external_stage(resolved, state, stdin, false, false) {
                Ok(child) => child,
                Err(error) => {
                    terminate_children(&mut children);
                    return Err(error);
                }
            };
        children.push(child);

        if let Some(stderr) = output_readers.stderr {
            let handle = match spawn_exit_aware_capture_reader(
                stderr,
                stderr_spool,
                bounded_capture,
                Arc::clone(&direct_stages_exited),
            ) {
                Ok(handle) => handle,
                Err(error) => {
                    terminate_children(&mut children);
                    direct_stages_exited.store(true, Ordering::Release);
                    return Err(error.into());
                }
            };
            stderr_handles.push(handle);
        }

        if let Some(stdout) = output_readers.stdout {
            if index == last_index {
                final_stdout = Some(stdout);
            } else {
                previous_stdout = Some(stdout);
                previous_pipe_closed = false;
            }
        } else if index != last_index {
            previous_pipe_closed = true;
        }
    }

    drop(previous_stdout);
    Ok(RunningExternalPrefix {
        children,
        stderr_handles,
        final_stdout,
    })
}

fn spawn_external_suffix_from_shell_stage(
    commands: &[ResolvedExternalInvocation],
    state: &ShellState,
    initial_stdin: io::PipeReader,
    bounded_capture: bool,
    direct_stages_exited: Arc<AtomicBool>,
) -> Result<RunningExternalSuffix, ShellError> {
    let last_index = commands.len().saturating_sub(1);
    let mut children: Vec<Child> = Vec::with_capacity(commands.len());
    let mut previous_stdout: Option<io::PipeReader> = Some(initial_stdin);
    let mut previous_pipe_closed = false;
    let mut stderr_handles = Vec::with_capacity(commands.len());
    let mut final_stdout_handle = None;

    for (index, resolved) in commands.iter().enumerate() {
        let stderr_spool = if bounded_capture {
            state.create_trace_spool("err").ok()
        } else {
            None
        };
        let stdout_spool = if bounded_capture && index == last_index {
            state.create_trace_spool("out").ok()
        } else {
            None
        };
        let stdin = if let Some(stdout) = previous_stdout.take() {
            ExternalStageStdin::Pipe(stdout)
        } else if previous_pipe_closed {
            previous_pipe_closed = false;
            ExternalStageStdin::Null
        } else {
            ExternalStageStdin::Inherit
        };

        let (child, output_readers) =
            match spawn_resolved_external_stage(resolved, state, stdin, false, false) {
                Ok(child) => child,
                Err(error) => {
                    terminate_children(&mut children);
                    return Err(error);
                }
            };
        children.push(child);

        if let Some(stderr) = output_readers.stderr {
            let handle = match spawn_exit_aware_capture_reader(
                stderr,
                stderr_spool,
                bounded_capture,
                Arc::clone(&direct_stages_exited),
            ) {
                Ok(handle) => handle,
                Err(error) => {
                    terminate_children(&mut children);
                    direct_stages_exited.store(true, Ordering::Release);
                    return Err(error.into());
                }
            };
            stderr_handles.push(handle);
        }

        if let Some(stdout) = output_readers.stdout {
            if index == last_index {
                final_stdout_handle = Some(
                    match spawn_exit_aware_capture_reader(
                        stdout,
                        stdout_spool,
                        bounded_capture,
                        Arc::clone(&direct_stages_exited),
                    ) {
                        Ok(handle) => handle,
                        Err(error) => {
                            terminate_children(&mut children);
                            direct_stages_exited.store(true, Ordering::Release);
                            return Err(error.into());
                        }
                    },
                );
            } else {
                previous_stdout = Some(stdout);
                previous_pipe_closed = false;
            }
        } else if index != last_index {
            previous_pipe_closed = true;
        }
    }

    drop(previous_stdout);
    Ok(RunningExternalSuffix {
        children,
        stderr_handles,
        final_stdout_handle,
    })
}

fn spawn_live_external_prefix(
    commands: &[ResolvedExternalInvocation],
    state: &ShellState,
    base_routing: &InheritedCaptureRouting,
) -> Result<(Vec<Child>, Option<io::PipeReader>), ShellError> {
    let mut children = Vec::with_capacity(commands.len());
    let mut previous_stdout = None;

    for resolved in commands {
        let stdin = previous_stdout
            .take()
            .map_or(ExternalStageStdin::Inherit, ExternalStageStdin::Pipe);
        let (reader, writer) = match io::pipe() {
            Ok(pipe) => pipe,
            Err(error) => {
                terminate_children(&mut children);
                return Err(error.into());
            }
        };
        let stdout_target = StreamingOutputTarget::Pipe {
            kind: StreamingPipeKind::Stdout,
            writer,
        };
        let stderr_target =
            match streaming_target_from_materialized_destination(&base_routing.stderr) {
                Ok(target) => target,
                Err(error) => {
                    terminate_children(&mut children);
                    return Err(error.into());
                }
            };
        let child = match spawn_resolved_external_stage_with_targets(
            resolved,
            state,
            stdin,
            stdout_target,
            stderr_target,
        ) {
            Ok(child) => child,
            Err(error) => {
                terminate_children(&mut children);
                return Err(error);
            }
        };
        previous_stdout = Some(reader);
        children.push(child);
    }

    Ok((children, previous_stdout))
}

fn run_live_streaming_external_prefix_to_final_read(
    commands: &[ResolvedExternalInvocation],
    final_read: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let bounded_capture = output_mode.should_capture()
        && output_mode != OutputMode::Rich
        && !state.exact_capture_enabled();
    let materialized = materialize_external_pipeline_routing(
        state,
        output_mode.should_capture(),
        bounded_capture,
    )?;
    let base_routing = materialized.capture_routing()?;
    let (mut children, final_stdin) = spawn_live_external_prefix(commands, state, &base_routing)?;
    let raw = read_invocation_raw_mode(final_read);
    let stdin_bytes = match final_stdin {
        Some(reader) => match read_read_input_from_pipe(reader, raw) {
            Ok(input) => input,
            Err(error) => {
                terminate_children(&mut children);
                return Err(error);
            }
        },
        None => Vec::new(),
    };

    let mut exit_codes = Vec::with_capacity(children.len() + 1);
    for child in &mut children {
        exit_codes.push(exit_status_code(child.wait()?));
    }
    let read_outcome = with_inherited_capture_routing(base_routing.clone(), || {
        let mut outcome = run_invocation(
            final_read,
            state,
            output_mode,
            Some(&stdin_bytes),
            true,
            LookupMode::Normal,
            allow_process_replacement,
        )?;
        apply_builtin_redirections(&mut outcome, &[], state)?;
        Ok::<_, ShellError>(outcome)
    })?;
    debug_assert!(read_outcome.stdout.is_empty());
    debug_assert!(read_outcome.stderr.is_empty());
    exit_codes.push(read_outcome.exit_code);
    let exit_code = pipeline_exit_code(&exit_codes, state.pipefail());
    drop(base_routing);
    materialized.finish(exit_code)
}

fn run_streaming_external_prefix_to_final_read(
    commands: &[ResolvedExternalInvocation],
    final_read: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    if !inherited_capture_routing().is_default() {
        return run_live_streaming_external_prefix_to_final_read(
            commands,
            final_read,
            state,
            output_mode,
            allow_process_replacement,
        );
    }

    let last_index = commands.len().saturating_sub(1);
    let bounded_capture = output_mode.should_capture()
        && output_mode != OutputMode::Rich
        && !state.exact_capture_enabled();
    let mut children: Vec<Child> = Vec::with_capacity(commands.len());
    let mut previous_stdout: Option<io::PipeReader> = None;
    let mut previous_pipe_closed = false;
    let mut stderr_handles = Vec::with_capacity(commands.len());
    let mut final_stdin = None;
    let direct_stages_exited = Arc::new(AtomicBool::new(false));
    let exit_guard = DirectStageExitGuard {
        exited: Arc::clone(&direct_stages_exited),
    };

    for (index, resolved) in commands.iter().enumerate() {
        let stderr_spool = if bounded_capture {
            state.create_trace_spool("err").ok()
        } else {
            None
        };
        let stdin = if let Some(stdout) = previous_stdout.take() {
            ExternalStageStdin::Pipe(stdout)
        } else if previous_pipe_closed {
            previous_pipe_closed = false;
            ExternalStageStdin::Null
        } else {
            ExternalStageStdin::Inherit
        };

        let (child, output_readers) =
            match spawn_resolved_external_stage(resolved, state, stdin, false, false) {
                Ok(child) => child,
                Err(error) => {
                    terminate_children(&mut children);
                    return Err(error);
                }
            };
        children.push(child);

        if let Some(stderr) = output_readers.stderr {
            let handle = match spawn_exit_aware_capture_reader(
                stderr,
                stderr_spool,
                bounded_capture,
                Arc::clone(&direct_stages_exited),
            ) {
                Ok(handle) => handle,
                Err(error) => {
                    terminate_children(&mut children);
                    return Err(error.into());
                }
            };
            stderr_handles.push(handle);
        }

        if let Some(stdout) = output_readers.stdout {
            if index == last_index {
                final_stdin = Some(stdout);
            } else {
                previous_stdout = Some(stdout);
                previous_pipe_closed = false;
            }
        } else if index != last_index {
            previous_pipe_closed = true;
        }
    }

    drop(previous_stdout);

    let raw = read_invocation_raw_mode(final_read);
    let stdin_bytes = match final_stdin {
        Some(reader) => match read_read_input_from_pipe(reader, raw) {
            Ok(input) => input,
            Err(error) => {
                terminate_children(&mut children);
                return Err(error);
            }
        },
        None => Vec::new(),
    };

    let mut exit_codes = Vec::with_capacity(children.len() + 1);
    for child in &mut children {
        let status = child.wait()?;
        exit_codes.push(exit_status_code(status));
    }
    exit_guard.signal();

    let mut read_outcome = run_invocation(
        final_read,
        state,
        output_mode,
        Some(&stdin_bytes),
        true,
        LookupMode::Normal,
        allow_process_replacement,
    )?;
    exit_codes.push(read_outcome.exit_code);
    read_outcome.exit_code = pipeline_exit_code(&exit_codes, state.pipefail());

    for handle in stderr_handles {
        let stderr = join_capture_reader(handle)?;
        let mut stage =
            CommandOutcome::captured_from_streams(0, CapturedStream::complete(Vec::new()), stderr);
        read_outcome.append_stderr(&mut stage)?;
    }

    Ok(read_outcome)
}

fn run_live_streaming_external_prefix_to_final_shell_command(
    commands: &[ResolvedExternalInvocation],
    final_invocation: &CommandInvocation,
    state: &mut ShellState,
    options: &ExecutionOptions,
    allow_function_definition: bool,
) -> Result<CommandOutcome, ShellError> {
    let bounded_capture = options.output_mode.should_capture()
        && options.output_mode != OutputMode::Rich
        && !state.exact_capture_enabled();
    let materialized = materialize_external_pipeline_routing(
        state,
        options.output_mode.should_capture(),
        bounded_capture,
    )?;
    let base_routing = materialized.capture_routing()?;
    let (mut children, final_stdin) = spawn_live_external_prefix(commands, state, &base_routing)?;

    let final_result = with_inherited_capture_routing(base_routing.clone(), || {
        let run_final = |state: &mut ShellState| {
            let mut outcome = run_pipeline_command_invocation(
                final_invocation,
                state,
                options.output_mode,
                None,
                true,
                allow_function_definition,
                options.allow_process_replacement,
            )?;
            apply_builtin_redirections(&mut outcome, &[], state)?;
            Ok(outcome)
        };
        match final_stdin {
            Some(reader) => run_with_streaming_stdin(state, reader, run_final),
            None => run_with_buffered_stdin(state, Some(&[]), run_final),
        }
    });

    let mut exit_codes = Vec::with_capacity(children.len() + 1);
    for child in &mut children {
        exit_codes.push(exit_status_code(child.wait()?));
    }
    let final_outcome = final_result?;
    debug_assert!(final_outcome.stdout.is_empty());
    debug_assert!(final_outcome.stderr.is_empty());
    exit_codes.push(final_outcome.exit_code);
    let exit_code = pipeline_exit_code(&exit_codes, state.pipefail());
    drop(base_routing);
    materialized.finish(exit_code)
}

fn run_streaming_external_prefix_to_final_shell_command(
    commands: &[ResolvedExternalInvocation],
    final_invocation: &CommandInvocation,
    state: &mut ShellState,
    options: &ExecutionOptions,
    allow_function_definition: bool,
) -> Result<CommandOutcome, ShellError> {
    if !inherited_capture_routing().is_default() {
        return run_live_streaming_external_prefix_to_final_shell_command(
            commands,
            final_invocation,
            state,
            options,
            allow_function_definition,
        );
    }

    let last_index = commands.len().saturating_sub(1);
    let bounded_capture = options.output_mode.should_capture()
        && options.output_mode != OutputMode::Rich
        && !state.exact_capture_enabled();
    let mut children: Vec<Child> = Vec::with_capacity(commands.len());
    let mut previous_stdout: Option<io::PipeReader> = None;
    let mut previous_pipe_closed = false;
    let mut stderr_handles = Vec::with_capacity(commands.len());
    let mut final_stdin = None;
    let direct_stages_exited = Arc::new(AtomicBool::new(false));
    let exit_guard = DirectStageExitGuard {
        exited: Arc::clone(&direct_stages_exited),
    };

    for (index, resolved) in commands.iter().enumerate() {
        let stderr_spool = if bounded_capture {
            state.create_trace_spool("err").ok()
        } else {
            None
        };
        let stdin = if let Some(stdout) = previous_stdout.take() {
            ExternalStageStdin::Pipe(stdout)
        } else if previous_pipe_closed {
            previous_pipe_closed = false;
            ExternalStageStdin::Null
        } else {
            ExternalStageStdin::Inherit
        };

        let (child, output_readers) =
            match spawn_resolved_external_stage(resolved, state, stdin, false, false) {
                Ok(child) => child,
                Err(error) => {
                    terminate_children(&mut children);
                    return Err(error);
                }
            };
        children.push(child);

        if let Some(stderr) = output_readers.stderr {
            let handle = match spawn_exit_aware_capture_reader(
                stderr,
                stderr_spool,
                bounded_capture,
                Arc::clone(&direct_stages_exited),
            ) {
                Ok(handle) => handle,
                Err(error) => {
                    terminate_children(&mut children);
                    return Err(error.into());
                }
            };
            stderr_handles.push(handle);
        }

        if let Some(stdout) = output_readers.stdout {
            if index == last_index {
                final_stdin = Some(stdout);
            } else {
                previous_stdout = Some(stdout);
                previous_pipe_closed = false;
            }
        } else if index != last_index {
            previous_pipe_closed = true;
        }
    }

    drop(previous_stdout);

    let final_result = if let Some(reader) = final_stdin {
        run_with_streaming_stdin(state, reader, |state| {
            run_pipeline_command_invocation(
                final_invocation,
                state,
                options.output_mode,
                None,
                true,
                allow_function_definition,
                options.allow_process_replacement,
            )
        })
    } else {
        run_with_buffered_stdin(state, Some(&[]), |state| {
            run_pipeline_command_invocation(
                final_invocation,
                state,
                options.output_mode,
                None,
                true,
                allow_function_definition,
                options.allow_process_replacement,
            )
        })
    };

    let mut exit_codes = Vec::with_capacity(children.len() + 1);
    for child in &mut children {
        let status = child.wait()?;
        exit_codes.push(exit_status_code(status));
    }
    exit_guard.signal();

    let mut prefix_stderr = CommandOutcome::captured(0, Vec::new(), Vec::new());
    for handle in stderr_handles {
        let stderr = join_capture_reader(handle)?;
        let mut stage =
            CommandOutcome::captured_from_streams(0, CapturedStream::complete(Vec::new()), stderr);
        prefix_stderr.append_stderr(&mut stage)?;
    }

    let mut final_outcome = final_result?;
    exit_codes.push(final_outcome.exit_code);
    final_outcome.exit_code = pipeline_exit_code(&exit_codes, state.pipefail());
    final_outcome.append_stderr(&mut prefix_stderr)?;

    Ok(final_outcome)
}

fn read_invocation_raw_mode(invocation: &ExpandedInvocation) -> bool {
    invocation.argv.iter().skip(1).any(|arg| arg == "-r")
}

fn read_read_input_from_pipe<R>(mut reader: R, raw: bool) -> Result<Vec<u8>, ShellError>
where
    R: Read,
{
    let mut input = Vec::new();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        let read = reader.read(&mut byte)?;
        if read == 0 {
            break;
        }

        input.push(byte[0]);
        line.push(byte[0]);
        if byte[0] == b'\n' {
            if raw || !read_line_is_continued(&line) {
                break;
            }
            line.clear();
        }
    }

    Ok(input)
}

fn read_line_is_continued(line: &[u8]) -> bool {
    let mut text = String::from_utf8_lossy(line).to_string();
    remove_read_continuation(&mut text)
}

fn pipeline_exit_code(exit_codes: &[i32], pipefail: bool) -> i32 {
    if pipefail {
        exit_codes
            .iter()
            .rev()
            .copied()
            .find(|code| *code != 0)
            .unwrap_or(0)
    } else {
        exit_codes.last().copied().unwrap_or(0)
    }
}

fn apply_streaming_external_redirections(
    command: &mut Command,
    redirections: &[ExpandedRedirection],
    noclobber: bool,
    inherit_stdout: bool,
    inherit_stderr: bool,
) -> Result<StreamingOutputReaders, ShellError> {
    apply_streaming_external_redirections_with_targets(
        command,
        redirections,
        noclobber,
        inherit_stdout,
        inherit_stderr,
        None,
        None,
    )
}

fn apply_streaming_external_redirections_with_targets(
    command: &mut Command,
    redirections: &[ExpandedRedirection],
    noclobber: bool,
    inherit_stdout: bool,
    inherit_stderr: bool,
    stdout_target: Option<StreamingOutputTarget>,
    stderr_target: Option<StreamingOutputTarget>,
) -> Result<StreamingOutputReaders, ShellError> {
    let (stdout_reader, mut stdout_target) = if let Some(target) = stdout_target {
        (None, target)
    } else if inherit_stdout {
        (
            None,
            StreamingOutputTarget::Inherit(InheritedOutput::Stdout),
        )
    } else {
        let (reader, writer) = io::pipe()?;
        (
            Some(reader),
            StreamingOutputTarget::Pipe {
                kind: StreamingPipeKind::Stdout,
                writer,
            },
        )
    };
    let (stderr_reader, mut stderr_target) = if let Some(target) = stderr_target {
        (None, target)
    } else if inherit_stderr {
        (
            None,
            StreamingOutputTarget::Inherit(InheritedOutput::Stderr),
        )
    } else {
        let (reader, writer) = io::pipe()?;
        (
            Some(reader),
            StreamingOutputTarget::Pipe {
                kind: StreamingPipeKind::Stderr,
                writer,
            },
        )
    };

    for redirection in redirections {
        match (&redirection.mode, &redirection.target) {
            (RedirectionMode::Read, ExpandedRedirectionTarget::Path(path))
                if redirection.fd == 0 =>
            {
                command.stdin(Stdio::from(open_read_redirection(path)?));
            }
            (RedirectionMode::Write, ExpandedRedirectionTarget::Path(path)) => {
                let file = open_write_redirection(path, noclobber, false)?;
                if redirection.fd == 1 {
                    stdout_target = StreamingOutputTarget::File(file);
                } else if redirection.fd == 2 {
                    stderr_target = StreamingOutputTarget::File(file);
                }
            }
            (RedirectionMode::WriteClobber, ExpandedRedirectionTarget::Path(path)) => {
                let file = open_write_redirection(path, noclobber, true)?;
                if redirection.fd == 1 {
                    stdout_target = StreamingOutputTarget::File(file);
                } else if redirection.fd == 2 {
                    stderr_target = StreamingOutputTarget::File(file);
                }
            }
            (RedirectionMode::Append, ExpandedRedirectionTarget::Path(path)) => {
                let file = OpenOptions::new().create(true).append(true).open(path)?;
                if redirection.fd == 1 {
                    stdout_target = StreamingOutputTarget::File(file);
                } else if redirection.fd == 2 {
                    stderr_target = StreamingOutputTarget::File(file);
                }
            }
            (RedirectionMode::WriteBoth, ExpandedRedirectionTarget::Path(path)) => {
                let file = open_write_redirection(path, noclobber, false)?;
                stdout_target = StreamingOutputTarget::File(file.try_clone()?);
                stderr_target = StreamingOutputTarget::File(file);
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 0 => {
                command.stdin(Stdio::null());
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 1 => {
                stdout_target = StreamingOutputTarget::Null;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 2 => {
                stderr_target = StreamingOutputTarget::Null;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(1)) if redirection.fd == 2 => {
                stderr_target = stdout_target.try_clone()?;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(2)) if redirection.fd == 1 => {
                stdout_target = stderr_target.try_clone()?;
            }
            _ => {
                return Err(ShellError::unsupported(format!(
                    "unsupported streaming pipeline redirection for fd {}",
                    redirection.fd
                )));
            }
        }
    }

    let stdout_pipe_used = stdout_target.pipe_kind() == Some(StreamingPipeKind::Stdout)
        || stderr_target.pipe_kind() == Some(StreamingPipeKind::Stdout);
    let stderr_pipe_used = stdout_target.pipe_kind() == Some(StreamingPipeKind::Stderr)
        || stderr_target.pipe_kind() == Some(StreamingPipeKind::Stderr);

    command.stdout(stdout_target.into_stdio()?);
    command.stderr(stderr_target.into_stdio()?);

    Ok(StreamingOutputReaders {
        stdout: if stdout_pipe_used {
            stdout_reader
        } else {
            None
        },
        stderr: if stderr_pipe_used {
            stderr_reader
        } else {
            None
        },
    })
}

fn join_capture_reader(
    handle: std::thread::JoinHandle<io::Result<CapturedStream>>,
) -> Result<CapturedStream, ShellError> {
    handle
        .join()
        .map_err(|_| ShellError::execution("pipeline reader thread panicked"))?
        .map_err(ShellError::from)
}

fn terminate_children(children: &mut [Child]) {
    for child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn expand_invocation(
    invocation: &CommandInvocation,
    state: &mut ShellState,
) -> Result<ExpandedInvocation, ShellError> {
    // Reset so an assignment-only command can report the status of a command
    // substitution in its value (e.g. `x=$(cmd)`); stays 0 when none runs.
    state.set_command_substitution_status(0);
    // `[[ ... ]]` and `(( ... ))`: operands get parameter/tilde expansion but NO
    // globbing and NO word-splitting (each stays one word/expression).
    let no_split = invocation.argv.first().map(String::as_str) == Some("[[")
        || invocation
            .argv
            .first()
            .is_some_and(|a| a.starts_with("((") && a.ends_with("))"));
    let mut argv = Vec::new();
    for (index, arg) in invocation.argv.iter().enumerate() {
        // Input process substitution `<(cmd)` -> a temp-file path.
        if arg.starts_with("<(") && arg.ends_with(')') {
            argv.push(process_substitution_path(&arg[2..arg.len() - 1], state)?);
            continue;
        }
        let segments = invocation
            .argv_segments
            .get(index)
            .cloned()
            .unwrap_or_else(|| vec![WordSegment::new(arg.clone(), invocation.argv_quote[index])]);
        if no_split {
            argv.push(expand_word(&segments, state)?);
        } else {
            argv.extend(expand_word_segments_to_argv_fields(&segments, state)?);
        }
    }

    Ok(ExpandedInvocation {
        assignments: invocation
            .assignments
            .iter()
            .map(|assignment| {
                Ok(Assignment {
                    name: assignment.name.clone(),
                    value: expand_word(&assignment.value_segments, state)?,
                    value_segments: assignment.value_segments.clone(),
                })
            })
            .collect::<Result<Vec<_>, ShellError>>()?,
        argv,
        redirections: expand_redirections(&invocation.redirections, state)?,
    })
}

fn expand_word_segments_to_argv_fields(
    segments: &[WordSegment],
    state: &mut ShellState,
) -> Result<Vec<String>, ShellError> {
    let expanded_fields = expand_argument_fields(segments, state)?;
    let brace_expanded = if has_unquoted_brace(segments) {
        expanded_fields
            .into_iter()
            .flat_map(|field| {
                let glob_eligible = field.has_active_glob();
                expand_braces(&field.text).into_iter().map(move |text| {
                    let glob_mask = if glob_eligible {
                        text.chars().map(is_glob_metachar).collect()
                    } else {
                        vec![false; text.chars().count()]
                    };
                    ExpandedField { text, glob_mask }
                })
            })
            .collect::<Vec<_>>()
    } else {
        expanded_fields
    };

    let mut fields = Vec::new();
    for field in brace_expanded {
        if !state.noglob() && field.has_active_glob() {
            let opts = GlobOpts {
                globstar: state.shopt("globstar"),
                dotglob: state.shopt("dotglob"),
                nocaseglob: state.shopt("nocaseglob"),
            };
            let matches = expand_glob(&field.text, &field.glob_mask, state.cwd(), opts);
            if matches.is_empty() {
                // nullglob: a non-matching pattern expands to nothing (else the
                // literal pattern is kept, as in bash default).
                if !state.shopt("nullglob") {
                    fields.push(field.text);
                }
            } else {
                fields.extend(matches);
            }
        } else {
            fields.push(field.text);
        }
    }
    Ok(fields)
}

fn expand_redirections(
    redirections: &[agsh_core::Redirection],
    state: &mut ShellState,
) -> Result<Vec<ExpandedRedirection>, ShellError> {
    redirections
        .iter()
        .map(|redirection| {
            let target = match redirection.mode {
                RedirectionMode::HereDoc => match &redirection.target {
                    RedirectionTarget::Word { segments, .. } => {
                        // The parser marks an unquoted delimiter with a Double
                        // segment and a quoted delimiter with a Single segment.
                        // Expandable here-documents have distinct backslash
                        // rules from ordinary double-quoted shell words.
                        let body = if segments
                            .first()
                            .is_some_and(|segment| segment.quote == QuoteKind::Double)
                        {
                            expand_heredoc_body(&segments[0].text, state)?
                        } else {
                            expand_word(segments, state)?
                        };
                        ExpandedRedirectionTarget::Bytes(body.into_bytes())
                    }
                    _ => ExpandedRedirectionTarget::Bytes(Vec::new()),
                },
                RedirectionMode::HereString => match &redirection.target {
                    RedirectionTarget::Word { segments, .. } => {
                        if let Some((expand, body)) = inline_heredoc_body(segments) {
                            let body = if expand {
                                expand_heredoc_body(&body, state)?
                            } else {
                                body
                            };
                            return Ok(ExpandedRedirection {
                                fd: redirection.fd,
                                mode: RedirectionMode::HereDoc,
                                target: ExpandedRedirectionTarget::Bytes(body.into_bytes()),
                            });
                        }
                        let mut body = expand_word(segments, state)?;
                        body.push('\n');
                        ExpandedRedirectionTarget::Bytes(body.into_bytes())
                    }
                    _ => ExpandedRedirectionTarget::Bytes(b"\n".to_vec()),
                },
                _ => match &redirection.target {
                    // `< <(cmd)` / `> >(cmd)`: a process substitution as the
                    // redirection target resolves to its temp-file path.
                    RedirectionTarget::Word { text, .. }
                        if text.starts_with("<(") && text.ends_with(')') =>
                    {
                        let path = process_substitution_path(&text[2..text.len() - 1], state)?;
                        ExpandedRedirectionTarget::Path(resolve_shell_path(&path, state.cwd()))
                    }
                    RedirectionTarget::Word { segments, .. } => {
                        let path = expand_word(segments, state)?;
                        ExpandedRedirectionTarget::Path(resolve_shell_path(&path, state.cwd()))
                    }
                    RedirectionTarget::Fd(fd) => ExpandedRedirectionTarget::Fd(*fd),
                    RedirectionTarget::Close => ExpandedRedirectionTarget::Close,
                },
            };
            Ok(ExpandedRedirection {
                fd: redirection.fd,
                mode: redirection.mode,
                target,
            })
        })
        .collect()
}

fn inline_heredoc_body(segments: &[WordSegment]) -> Option<(bool, String)> {
    if segments.is_empty()
        || segments
            .iter()
            .any(|segment| segment.quote != QuoteKind::Single)
    {
        return None;
    }
    let text = segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect::<String>();
    let payload = text.strip_prefix(INLINE_HEREDOC_PREFIX)?;
    if let Some(body) = payload.strip_prefix("e:") {
        Some((true, body.to_string()))
    } else {
        payload
            .strip_prefix("l:")
            .map(|body| (false, body.to_string()))
    }
}

fn expand_heredoc_body(body: &str, state: &mut ShellState) -> Result<String, ShellError> {
    let chars = body.chars().collect::<Vec<_>>();
    let mut segments = Vec::new();
    let mut expandable = String::new();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] != '\\' {
            expandable.push(chars[i]);
            i += 1;
            continue;
        }

        match chars.get(i + 1).copied() {
            Some('\n') => {
                // Backslash-newline is removed before expansion.
                i += 2;
            }
            Some(next @ ('$' | '`' | '\\')) => {
                if !expandable.is_empty() {
                    segments.push(WordSegment::new(
                        std::mem::take(&mut expandable),
                        QuoteKind::Double,
                    ));
                }
                // A backslash quotes only these three metacharacters in an
                // expandable here-document. The backslash itself is removed.
                segments.push(WordSegment::new(next.to_string(), QuoteKind::Single));
                i += 2;
            }
            Some(next) => {
                // Before any other character the backslash is literal.
                expandable.push('\\');
                expandable.push(next);
                i += 2;
            }
            None => {
                expandable.push('\\');
                i += 1;
            }
        }
    }

    if !expandable.is_empty() || segments.is_empty() {
        segments.push(WordSegment::new(expandable, QuoteKind::Double));
    }
    expand_word(&segments, state)
}

fn resolve_shell_path(path: &str, cwd: &Path) -> String {
    let path = Path::new(path);
    if path.is_absolute() {
        path.display().to_string()
    } else {
        cwd.join(path).display().to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpansionFragment {
    text: String,
    split_eligible: bool,
    preserves_field: bool,
    glob_eligible: bool,
    field_boundary_before: bool,
    suppress_empty_field: bool,
}

impl ExpansionFragment {
    fn literal(text: impl Into<String>, preserves_field: bool, glob_eligible: bool) -> Self {
        Self {
            text: text.into(),
            split_eligible: false,
            preserves_field,
            glob_eligible,
            field_boundary_before: false,
            suppress_empty_field: false,
        }
    }

    fn expanded(text: impl Into<String>, split_eligible: bool) -> Self {
        Self {
            text: text.into(),
            split_eligible,
            preserves_field: false,
            glob_eligible: split_eligible,
            field_boundary_before: false,
            suppress_empty_field: false,
        }
    }

    fn positional(text: impl Into<String>, field_boundary_before: bool) -> Self {
        Self {
            text: text.into(),
            split_eligible: false,
            preserves_field: true,
            glob_eligible: false,
            field_boundary_before,
            suppress_empty_field: false,
        }
    }

    fn suppress_empty_field() -> Self {
        Self {
            text: String::new(),
            split_eligible: false,
            preserves_field: false,
            glob_eligible: false,
            field_boundary_before: false,
            suppress_empty_field: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpandedField {
    text: String,
    glob_mask: Vec<bool>,
}

impl ExpandedField {
    fn has_active_glob(&self) -> bool {
        self.text
            .chars()
            .zip(&self.glob_mask)
            .any(|(ch, active)| *active && is_glob_metachar(ch))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ExpandedFieldBuilder {
    text: String,
    glob_mask: Vec<bool>,
    material: bool,
}

impl ExpandedFieldBuilder {
    fn append_field(&mut self, field: ExpandedField) {
        self.material = true;
        self.text.push_str(&field.text);
        self.glob_mask.extend(field.glob_mask);
    }

    fn append_quoted_text(&mut self, text: String) {
        self.material = true;
        self.glob_mask.extend(text.chars().map(|_| false));
        self.text.push_str(&text);
    }

    fn into_field(self) -> Option<ExpandedField> {
        self.material.then_some(ExpandedField {
            text: self.text,
            glob_mask: self.glob_mask,
        })
    }
}

fn expand_substitutions(input: &str, state: &mut ShellState) -> Result<String, ShellError> {
    let fragments = expand_substitution_fragments(input, state, false, PositionalStarJoin::Space)?;
    Ok(fragments_to_string(&fragments))
}

fn expand_argument_fields(
    segments: &[WordSegment],
    state: &mut ShellState,
) -> Result<Vec<ExpandedField>, ShellError> {
    // `${a[@]}`/`${a[*]}` (quoted or not) -> one field per element (preserved),
    // so `for x in "${a[@]}"` keeps elements with spaces intact.
    if let Some(fields) = expand_array_at_fields(segments, state) {
        return Ok(fields);
    }

    if let Some(fields) = expand_native_list_fields(segments, state) {
        return Ok(fields);
    }

    if let Some(fields) = expand_quoted_positional_list_fields(segments, state)? {
        return Ok(fields);
    }

    let fragments = expand_word_fragments(segments, state, true)?;
    Ok(split_expanded_fields(&fragments, state))
}

/// Expand `[prefix]${name[@]}[suffix]` / `${name[*]}` (name an indexed array)
/// into preserved fields: one per element for `[@]` (prefix glued to the first,
/// suffix to the last), or a single IFS-joined field for `[*]`. Quoted and
/// unquoted are treated the same (per-element); unquoted IFS-splitting of
/// elements that themselves contain IFS chars is not applied (rare).
fn expand_array_at_fields(
    segments: &[WordSegment],
    state: &ShellState,
) -> Option<Vec<ExpandedField>> {
    if segments.len() != 1 {
        return None;
    }
    let text = &segments[0].text;
    let start = text.find("${")?;
    let after = &text[start + 2..];
    let close = after.find('}')?;
    let inner = &after[..close];
    let open = inner.find('[')?;
    if !inner.ends_with(']') {
        return None;
    }
    let name = &inner[..open];
    let sub = &inner[open + 1..inner.len() - 1];
    if (sub != "@" && sub != "*") || !is_identifier(name) {
        return None;
    }
    let prefix = &text[..start];
    let suffix = &text[start + 2 + close + 1..];
    if prefix.contains('$') || suffix.contains('$') {
        return None;
    }
    let array = if state.is_assoc(name) {
        state.assoc_values(name)?
    } else {
        state.array(name)?.to_vec()
    };
    let field = |t: String| ExpandedField {
        glob_mask: vec![false; t.chars().count()],
        text: t,
    };

    if sub == "*" {
        let sep = state
            .lookup("IFS")
            .and_then(|s| s.chars().next())
            .map(String::from)
            .unwrap_or_default();
        return Some(vec![field(format!("{prefix}{}{suffix}", array.join(&sep)))]);
    }
    // `[@]`
    if array.is_empty() {
        return Some(if prefix.is_empty() && suffix.is_empty() {
            Vec::new()
        } else {
            vec![field(format!("{prefix}{suffix}"))]
        });
    }
    let last = array.len() - 1;
    Some(
        array
            .iter()
            .enumerate()
            .map(|(i, elem)| {
                let p = if i == 0 { prefix } else { "" };
                let s = if i == last { suffix } else { "" };
                field(format!("{p}{elem}{s}"))
            })
            .collect(),
    )
}

fn expand_native_list_fields(
    segments: &[WordSegment],
    state: &ShellState,
) -> Option<Vec<ExpandedField>> {
    let word = native_list_word(segments)?;
    let Value::List(values) = state.lookup_value(word.name)? else {
        return None;
    };

    Some(
        values
            .iter()
            .map(|value| {
                let text = format!("{}{}{}", word.prefix, value.as_string_lossy(), word.suffix);
                ExpandedField {
                    glob_mask: vec![false; text.chars().count()],
                    text,
                }
            })
            .collect(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeListWord<'a> {
    prefix: &'a str,
    name: &'a str,
    suffix: &'a str,
}

fn native_list_word(segments: &[WordSegment]) -> Option<NativeListWord<'_>> {
    if segments.len() != 1 {
        return None;
    }
    let segment = segments.first()?;
    if segment.quote != QuoteKind::None {
        return None;
    }

    if let Some(braced_start) = segment.text.find("${") {
        let name_start = braced_start + 2;
        let rest = &segment.text[name_start..];
        let braced_end = rest.find('}').map(|offset| name_start + offset)?;
        let name = &segment.text[name_start..braced_end];
        let prefix = &segment.text[..braced_start];
        let suffix = &segment.text[braced_end + 1..];
        if !prefix.contains('$') && !suffix.contains('$') && is_identifier(name) {
            return Some(NativeListWord {
                prefix,
                name,
                suffix,
            });
        }
        return None;
    }

    let marker_start = segment.text.find('$')?;
    let prefix = &segment.text[..marker_start];
    if prefix.contains('$') {
        return None;
    }

    let name_start = marker_start + 1;
    let name_end = parameter_identifier_end(&segment.text, name_start)?;
    let name = &segment.text[name_start..name_end];
    let suffix = &segment.text[name_end..];
    if suffix.contains('$') {
        return None;
    }
    Some(NativeListWord {
        prefix,
        name,
        suffix,
    })
}

fn parameter_identifier_end(text: &str, start: usize) -> Option<usize> {
    let mut chars = text[start..].char_indices();
    let (_, first) = chars.next()?;
    if first != '_' && !first.is_ascii_alphabetic() {
        return None;
    }

    let mut end = start + first.len_utf8();
    for (offset, ch) in chars {
        if ch == '_' || ch.is_ascii_alphanumeric() {
            end = start + offset + ch.len_utf8();
        } else {
            break;
        }
    }
    Some(end)
}

fn expand_quoted_positional_list_fields(
    segments: &[WordSegment],
    state: &mut ShellState,
) -> Result<Option<Vec<ExpandedField>>, ShellError> {
    if !segments.iter().any(contains_double_quoted_at) {
        return Ok(None);
    }

    let mut fields = vec![ExpandedFieldBuilder::default()];
    for segment in segments {
        if contains_double_quoted_at(segment) {
            append_double_quoted_at_segment(&mut fields, &segment.text, state)?;
        } else {
            let fragments = expand_word_fragments(std::slice::from_ref(segment), state, true)?;
            append_expanded_fields(&mut fields, split_expanded_fields(&fragments, state));
        }
    }

    Ok(Some(
        fields
            .into_iter()
            .filter_map(ExpandedFieldBuilder::into_field)
            .collect(),
    ))
}

fn contains_double_quoted_at(segment: &WordSegment) -> bool {
    segment.quote == QuoteKind::Double && next_quoted_at_marker(&segment.text, 0).is_some()
}

fn append_double_quoted_at_segment(
    fields: &mut Vec<ExpandedFieldBuilder>,
    text: &str,
    state: &mut ShellState,
) -> Result<(), ShellError> {
    let mut start = 0;
    while let Some((marker_start, marker_end)) = next_quoted_at_marker(text, start) {
        append_double_quoted_chunk(fields, &text[start..marker_start], state)?;
        append_positionals(fields, &state.positionals());
        start = marker_end;
    }
    append_double_quoted_chunk(fields, &text[start..], state)
}

fn append_double_quoted_chunk(
    fields: &mut Vec<ExpandedFieldBuilder>,
    chunk: &str,
    state: &mut ShellState,
) -> Result<(), ShellError> {
    if chunk.is_empty() {
        return Ok(());
    }

    let fragments =
        expand_substitution_fragments(chunk, state, false, PositionalStarJoin::IfsFirst)?;
    let text = fragments_to_string(&fragments);
    ensure_field_builder(fields).append_quoted_text(text);
    Ok(())
}

fn append_positionals(fields: &mut Vec<ExpandedFieldBuilder>, positionals: &[String]) {
    let Some((first, rest)) = positionals.split_first() else {
        return;
    };

    ensure_field_builder(fields).append_quoted_text(first.clone());
    fields.extend(rest.iter().cloned().map(|text| {
        let mut field = ExpandedFieldBuilder::default();
        field.append_quoted_text(text);
        field
    }));
}

fn append_expanded_fields(fields: &mut Vec<ExpandedFieldBuilder>, expanded: Vec<ExpandedField>) {
    let mut expanded = expanded.into_iter();
    let Some(first) = expanded.next() else {
        return;
    };

    ensure_field_builder(fields).append_field(first);
    fields.extend(expanded.map(|field| {
        let mut builder = ExpandedFieldBuilder::default();
        builder.append_field(field);
        builder
    }));
}

fn ensure_field_builder(fields: &mut Vec<ExpandedFieldBuilder>) -> &mut ExpandedFieldBuilder {
    if fields.is_empty() {
        fields.push(ExpandedFieldBuilder::default());
    }
    fields.last_mut().expect("field builder must exist")
}

fn next_quoted_at_marker(text: &str, start: usize) -> Option<(usize, usize)> {
    let indexed = text.char_indices().collect::<Vec<_>>();
    let chars = indexed.iter().map(|(_, ch)| *ch).collect::<Vec<_>>();
    let mut index = indexed
        .iter()
        .position(|(byte, _)| *byte >= start)
        .unwrap_or(indexed.len());

    while index < chars.len() {
        let marker_len = if chars.get(index) == Some(&'$') && chars.get(index + 1) == Some(&'@') {
            Some(2)
        } else if chars.get(index) == Some(&'$')
            && chars.get(index + 1) == Some(&'{')
            && chars.get(index + 2) == Some(&'@')
            && chars.get(index + 3) == Some(&'}')
        {
            Some(4)
        } else {
            None
        };
        if let Some(marker_len) = marker_len {
            let start_byte = indexed[index].0;
            let end_char = index + marker_len;
            let end_byte = indexed.get(end_char).map_or(text.len(), |(byte, _)| *byte);
            return Some((start_byte, end_byte));
        }

        if let Some(next) = parameter_word_substitution_end(&chars, index) {
            index = next;
        } else {
            index += 1;
        }
    }
    None
}

pub(crate) fn expand_word(
    segments: &[WordSegment],
    state: &mut ShellState,
) -> Result<String, ShellError> {
    let fragments = expand_word_fragments(segments, state, false)?;
    Ok(fragments_to_string(&fragments))
}

fn expand_word_fragments(
    segments: &[WordSegment],
    state: &mut ShellState,
    split_expansions: bool,
) -> Result<Vec<ExpansionFragment>, ShellError> {
    let mut fragments = Vec::new();
    for segment in segments {
        match segment.quote {
            QuoteKind::Single => {
                push_fragment(
                    &mut fragments,
                    ExpansionFragment::literal(&segment.text, true, false),
                );
            }
            QuoteKind::Double => {
                let mut double_fragments = expand_substitution_fragments(
                    &segment.text,
                    state,
                    false,
                    PositionalStarJoin::IfsFirst,
                )?;
                if double_fragments.is_empty() {
                    push_fragment(&mut fragments, ExpansionFragment::literal("", true, false));
                    continue;
                }
                for fragment in &mut double_fragments {
                    fragment.split_eligible = false;
                    fragment.glob_eligible = false;
                    if !fragment.suppress_empty_field {
                        fragment.preserves_field = true;
                    }
                }
                for fragment in double_fragments {
                    push_fragment(&mut fragments, fragment);
                }
            }
            QuoteKind::None => {
                fragments.extend(expand_substitution_fragments(
                    &segment.text,
                    state,
                    split_expansions,
                    PositionalStarJoin::Space,
                )?);
            }
        }
    }

    if segments
        .first()
        .is_some_and(|segment| segment.quote == QuoteKind::None)
    {
        expand_tilde_in_fragments(&mut fragments, state);
    }

    Ok(fragments)
}

fn expand_substitution_fragments(
    input: &str,
    state: &mut ShellState,
    split_expansions: bool,
    positional_star_join: PositionalStarJoin,
) -> Result<Vec<ExpansionFragment>, ShellError> {
    let mut fragments = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '`' {
            let Some((command_text, next_i)) = read_backtick_command(&chars, i) else {
                return Err(ShellError::parse("unterminated backtick substitution"));
            };
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(
                    run_command_substitution(&command_text, state)?,
                    split_expansions,
                ),
            );
            i = next_i;
            continue;
        }

        if chars[i] != '$' {
            push_fragment(
                &mut fragments,
                ExpansionFragment::literal(chars[i].to_string(), true, true),
            );
            i += 1;
            continue;
        }

        i += 1;
        if i >= chars.len() {
            push_fragment(&mut fragments, ExpansionFragment::literal("$", true, true));
            break;
        }

        if chars[i] == '{' {
            let open = i;
            if let Some((end, next)) = find_parameter_expansion_end(&chars, open) {
                let expression = chars[open + 1..end].iter().collect::<String>();
                for fragment in expand_braced_parameter_fragments(
                    &expression,
                    state,
                    split_expansions,
                    positional_star_join,
                )? {
                    push_fragment(&mut fragments, fragment);
                }
                i = next;
            } else {
                push_fragment(&mut fragments, ExpansionFragment::literal("${", true, true));
                for ch in &chars[open + 1..] {
                    push_fragment(
                        &mut fragments,
                        ExpansionFragment::literal(ch.to_string(), true, true),
                    );
                }
                break;
            }
            continue;
        }

        if chars[i] == '@' {
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(joined_positionals(state), split_expansions),
            );
            i += 1;
            continue;
        }

        if chars[i] == '*' {
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(
                    joined_star_positionals(state, positional_star_join),
                    split_expansions,
                ),
            );
            i += 1;
            continue;
        }

        if chars[i] == '$' {
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(std::process::id().to_string(), split_expansions),
            );
            i += 1;
            continue;
        }

        // `$-`: current shell option flags (POSIX 2.5.2).
        if chars[i] == '-' {
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(state.option_flags(), split_expansions),
            );
            i += 1;
            continue;
        }

        // `$!`: PID of the most recent background command (POSIX 2.5.2).
        if chars[i] == '!' {
            let pid = state
                .last_bg_pid()
                .map(|p| p.to_string())
                .unwrap_or_default();
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(pid, split_expansions),
            );
            i += 1;
            continue;
        }

        if chars[i] == '?' {
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(state.last_status().to_string(), split_expansions),
            );
            i += 1;
            continue;
        }

        if chars[i] == '#' {
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(state.positional_count().to_string(), split_expansions),
            );
            i += 1;
            continue;
        }

        if chars[i] == '(' && chars.get(i + 1).is_some_and(|ch| *ch == '(') {
            let expr_start = i + 2;
            let Some((expr_end, next_i)) = find_arithmetic_end(&chars, expr_start) else {
                push_fragment(
                    &mut fragments,
                    ExpansionFragment::literal("$((", true, true),
                );
                i = expr_start;
                continue;
            };
            let expression = chars[expr_start..expr_end].iter().collect::<String>();
            // Expand $-substitutions ($var, $1, $(...), nested $((...))) inside the
            // arithmetic text before evaluating, matching shell semantics.
            let expanded_expression =
                expand_word(&[WordSegment::new(expression, QuoteKind::Double)], state)?;
            // Float-aware: expressions with a decimal point, exponent, or math
            // function/constant evaluate as floating point (bash has no float).
            let value = if crate::math::looks_floating(&expanded_expression) {
                let vars = StateFloatVars(state);
                crate::math::eval(&expanded_expression, &vars)
                    .map(crate::math::format_result)
                    .map_err(|e| ShellError::execution(format!("arithmetic: {e}")))?
            } else {
                eval_arithmetic(&expanded_expression, state)?.to_string()
            };
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(value, split_expansions),
            );
            i = next_i;
            continue;
        }

        if chars[i] == '(' {
            let command_start = i + 1;
            let Some((command_end, next_i)) = find_command_substitution_end(&chars, i) else {
                push_fragment(&mut fragments, ExpansionFragment::literal("$(", true, true));
                i = command_start;
                continue;
            };
            let command_text = chars[command_start..command_end].iter().collect::<String>();
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(
                    run_command_substitution(&command_text, state)?,
                    split_expansions,
                ),
            );
            i = next_i;
            continue;
        }

        let start = i;
        if chars[i].is_ascii_digit() {
            // An unbraced positional parameter is one digit: `$10` is `${1}0`.
            i += 1;
        } else {
            while i < chars.len() && (chars[i] == '_' || chars[i].is_ascii_alphanumeric()) {
                i += 1;
            }
        }
        if start == i {
            push_fragment(&mut fragments, ExpansionFragment::literal("$", true, true));
        } else {
            let name = chars[start..i].iter().collect::<String>();
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(
                    expand_required_parameter(&name, state)?,
                    split_expansions,
                ),
            );
        }
    }

    Ok(fragments)
}

fn push_fragment(fragments: &mut Vec<ExpansionFragment>, fragment: ExpansionFragment) {
    if fragment.text.is_empty() && !fragment.suppress_empty_field {
        if fragment.preserves_field {
            fragments.push(fragment);
        }
        return;
    }

    if let Some(previous) = fragments.last_mut() {
        if !fragment.field_boundary_before
            && !previous.suppress_empty_field
            && previous.split_eligible == fragment.split_eligible
            && previous.preserves_field == fragment.preserves_field
            && previous.glob_eligible == fragment.glob_eligible
        {
            previous.text.push_str(&fragment.text);
            return;
        }
    }
    fragments.push(fragment);
}

fn fragments_to_string(fragments: &[ExpansionFragment]) -> String {
    let mut out = String::new();
    for fragment in fragments {
        out.push_str(&fragment.text);
    }
    out
}

fn expand_tilde_in_fragments(fragments: &mut [ExpansionFragment], state: &ShellState) {
    let Some(first) = fragments.first_mut() else {
        return;
    };
    if first.split_eligible {
        return;
    }

    if first.text == "~" {
        if let Some(home) = state.lookup("HOME") {
            first.text = home.to_string();
        }
    } else if let Some(rest) = first.text.strip_prefix("~/") {
        if let Some(home) = state.lookup("HOME") {
            first.text = format!("{home}/{rest}");
        }
    } else if let Some(rest) = first.text.strip_prefix('~') {
        // `~user` / `~user/path` -> that user's home (POSIX 2.6.1).
        let (user, tail) = match rest.split_once('/') {
            Some((u, t)) => (u, Some(t)),
            None => (rest, None),
        };
        if let Some(home) = user_home(user) {
            first.text = match tail {
                Some(t) => format!("{home}/{t}"),
                None => home,
            };
        }
    }
}

/// Look up a user's home directory from `/etc/passwd` (best-effort; covers local
/// users — directory-service-only users on macOS are not resolved).
fn user_home(user: &str) -> Option<String> {
    let content = std::fs::read_to_string("/etc/passwd").ok()?;
    for line in content.lines() {
        let mut fields = line.split(':');
        if fields.next() == Some(user) {
            // name:passwd:uid:gid:gecos:home:shell -> home is the 5th remaining.
            return fields.nth(4).map(str::to_string);
        }
    }
    None
}

fn split_expanded_fields(
    fragments: &[ExpansionFragment],
    state: &ShellState,
) -> Vec<ExpandedField> {
    let ifs = state.lookup("IFS").unwrap_or(" \t\n");
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut current_glob_mask = Vec::new();
    let mut current_has_material = false;
    let mut previous_non_whitespace_delimiter = false;

    for fragment in fragments {
        if fragment.field_boundary_before && current_has_material {
            fields.push(ExpandedField {
                text: std::mem::take(&mut current),
                glob_mask: std::mem::take(&mut current_glob_mask),
            });
            current_has_material = false;
            previous_non_whitespace_delimiter = false;
        }
        if fragment.suppress_empty_field {
            continue;
        }
        if !fragment.split_eligible {
            current.push_str(&fragment.text);
            current_glob_mask.extend(fragment.text.chars().map(|_| fragment.glob_eligible));
            current_has_material |= fragment.preserves_field || !fragment.text.is_empty();
            if fragment.preserves_field || !fragment.text.is_empty() {
                previous_non_whitespace_delimiter = false;
            }
            continue;
        }

        for ch in fragment.text.chars() {
            if is_ifs_whitespace(ch, ifs) {
                if current_has_material {
                    fields.push(ExpandedField {
                        text: std::mem::take(&mut current),
                        glob_mask: std::mem::take(&mut current_glob_mask),
                    });
                    current_has_material = false;
                }
            } else if is_ifs_delimiter(ch, ifs) {
                if current_has_material || previous_non_whitespace_delimiter || fields.is_empty() {
                    fields.push(ExpandedField {
                        text: std::mem::take(&mut current),
                        glob_mask: std::mem::take(&mut current_glob_mask),
                    });
                } else {
                    current.clear();
                    current_glob_mask.clear();
                }
                current_has_material = false;
                previous_non_whitespace_delimiter = true;
            } else {
                current.push(ch);
                current_glob_mask.push(fragment.glob_eligible);
                current_has_material = true;
                previous_non_whitespace_delimiter = false;
            }
        }
    }

    if current_has_material {
        fields.push(ExpandedField {
            text: current,
            glob_mask: current_glob_mask,
        });
    }

    fields
}

fn is_ifs_whitespace(ch: char, ifs: &str) -> bool {
    matches!(ch, ' ' | '\t' | '\n') && ifs.contains(ch)
}

fn is_ifs_delimiter(ch: char, ifs: &str) -> bool {
    ifs.contains(ch)
}

fn is_glob_metachar(ch: char) -> bool {
    matches!(ch, '*' | '?' | '[')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PositionalStarJoin {
    Space,
    IfsFirst,
}

#[derive(Default)]
struct ZshFlags {
    split: Option<String>,
    join: Option<String>,
    unique: bool,
    sort: Option<bool>, // Some(false)=asc (o), Some(true)=desc (O)
    numeric: bool,
    case: Option<u8>, // b'U' | b'L' | b'C'
    quote: bool,
    unquote: bool,
    indirect: bool,
    at: bool,
}

/// Parse a `${(flags)...}` flag string. Returns None on a malformed/unknown flag
/// (the caller then falls back to normal expansion).
fn parse_zsh_flags(flag_str: &str) -> Option<ZshFlags> {
    let mut flags = ZshFlags::default();
    let chars: Vec<char> = flag_str.chars().collect();
    let mut i = 0;
    // A delimited argument: the char after the flag is the delimiter, read until
    // it recurs. e.g. `s.:.` -> ":".
    fn read_delim(chars: &[char], i: &mut usize) -> Option<String> {
        let delim = *chars.get(*i)?;
        *i += 1;
        let start = *i;
        while *i < chars.len() && chars[*i] != delim {
            *i += 1;
        }
        if *i >= chars.len() {
            return None;
        }
        let arg: String = chars[start..*i].iter().collect();
        *i += 1; // consume closing delimiter
        Some(arg)
    }
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        match c {
            'f' => flags.split = Some("\n".to_string()),
            's' => flags.split = Some(read_delim(&chars, &mut i)?),
            'j' => flags.join = Some(read_delim(&chars, &mut i)?),
            'u' => flags.unique = true,
            'o' => flags.sort = Some(false),
            'O' => flags.sort = Some(true),
            'n' => flags.numeric = true,
            'U' => flags.case = Some(b'U'),
            'L' => flags.case = Some(b'L'),
            'C' => flags.case = Some(b'C'),
            'q' => flags.quote = true,
            'Q' => flags.unquote = true,
            'P' => flags.indirect = true,
            '@' => flags.at = true,
            ' ' | ',' => {}
            _ => return None,
        }
    }
    Some(flags)
}

/// Expand `${(flags)name}` zsh-style. Returns Ok(None) if the flags or body are
/// not recognized (so the caller falls back to normal handling).
fn expand_zsh_param_flags(
    expression: &str,
    state: &mut ShellState,
) -> Result<Option<String>, ShellError> {
    let Some(close) = expression.find(')') else {
        return Ok(None);
    };
    let flag_str = &expression[1..close];
    let body = &expression[close + 1..];
    let Some(flags) = parse_zsh_flags(flag_str) else {
        return Ok(None);
    };

    // Compute the base items from the body (array elements, or a scalar).
    let base_name: String = if flags.indirect {
        lookup_parameter(body, state).unwrap_or("").to_string()
    } else {
        body.to_string()
    };
    let stripped = base_name
        .strip_suffix("[@]")
        .or_else(|| base_name.strip_suffix("[*]"));
    let mut items: Vec<String> = if let Some(stripped) = stripped {
        if state.is_assoc(stripped) {
            state.assoc_values(stripped).unwrap_or_default()
        } else {
            state
                .array(stripped)
                .map(<[String]>::to_vec)
                .unwrap_or_default()
        }
    } else if let Some(arr) = state.array(&base_name) {
        arr.to_vec()
    } else if state.is_assoc(&base_name) {
        state.assoc_values(&base_name).unwrap_or_default()
    } else {
        vec![lookup_parameter(&base_name, state)
            .unwrap_or("")
            .to_string()]
    };

    // Split (f / s:sep:).
    if let Some(sep) = &flags.split {
        items = items
            .iter()
            .flat_map(|item| {
                if sep.is_empty() {
                    vec![item.clone()]
                } else {
                    item.split(sep.as_str()).map(str::to_string).collect()
                }
            })
            .collect();
    }
    // Unique (preserving first-seen order).
    if flags.unique {
        let mut seen = std::collections::HashSet::new();
        items.retain(|item| seen.insert(item.clone()));
    }
    // Sort.
    if let Some(desc) = flags.sort {
        if flags.numeric {
            items.sort_by(|a, b| {
                let (x, y) = (
                    a.parse::<f64>().unwrap_or(0.0),
                    b.parse::<f64>().unwrap_or(0.0),
                );
                x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
            });
        } else {
            items.sort();
        }
        if desc {
            items.reverse();
        }
    }
    // Case.
    if let Some(kind) = flags.case {
        for item in &mut items {
            *item = match kind {
                b'U' => item.to_uppercase(),
                b'L' => item.to_lowercase(),
                _ => capitalize_words(item),
            };
        }
    }
    // Quote / unquote.
    if flags.quote {
        for item in &mut items {
            *item = shell_quote(item);
        }
    } else if flags.unquote {
        for item in &mut items {
            *item = item.trim_matches(|c| c == '\'' || c == '"').to_string();
        }
    }

    let sep = flags.join.clone().unwrap_or_else(|| " ".to_string());
    Ok(Some(items.join(&sep)))
}

/// Capitalize the first letter of each whitespace-separated word.
fn capitalize_words(s: &str) -> String {
    s.split(' ')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Single-quote a string for safe shell re-use (zsh `(q)` flag).
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:@%+=".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn expand_braced_parameter(
    expression: &str,
    state: &mut ShellState,
    positional_star_join: PositionalStarJoin,
) -> Result<String, ShellError> {
    // zsh-style parameter expansion flags: `${(flags)name}` (the `(` right after
    // `${` is invalid in bash, so this never collides). e.g. `${(U)v}`,
    // `${(s.:.)PATH}`, `${(j:,:)arr}`, `${(fu)var}`, `${(oa)arr}`.
    if expression.starts_with('(') {
        if let Some(result) = expand_zsh_param_flags(expression, state)? {
            return Ok(result);
        }
    }

    if expression == "#" {
        return Ok(state.positional_count().to_string());
    }

    if expression == "*" {
        return Ok(joined_star_positionals(state, positional_star_join));
    }

    if expression == "@" {
        return Ok(joined_positionals(state));
    }

    // Array subscripts: ${a[i]}, ${a[@]}, ${a[*]}, ${#a[@]}, ${!a[@]}.
    if expression.contains('[') {
        if let Some(value) = expand_array_subscript(expression, state) {
            return Ok(value);
        }
    }

    if let Some(name) = expression.strip_prefix('#') {
        let Some(value) = lookup_parameter(name, state) else {
            if state.nounset() {
                return Err(unset_parameter_error(name));
            }
            return Ok("0".to_string());
        };
        return Ok(value.chars().count().to_string());
    }

    if expression == "?" {
        return Ok(state.last_status().to_string());
    }

    if let Some(value) = dynamic_special_var(expression, state) {
        return Ok(value);
    }

    // Indirect expansion: `${!name}` -> value of the variable named by `$name`.
    if let Some(inner) = expression.strip_prefix('!') {
        // `${!prefix*}` / `${!prefix@}`: names of variables with this prefix.
        if let Some(prefix) = inner.strip_suffix('*').or_else(|| inner.strip_suffix('@')) {
            if prefix.is_empty() || is_identifier(prefix) {
                let names: Vec<String> = state
                    .vars()
                    .keys()
                    .filter(|k| k.starts_with(prefix))
                    .cloned()
                    .collect();
                return Ok(names.join(" "));
            }
        }
        if is_identifier(inner) {
            let target = lookup_parameter(inner, state)
                .unwrap_or_default()
                .to_string();
            if target.is_empty() {
                if state.nounset() {
                    return Err(unset_parameter_error(inner));
                }
                return Ok(String::new());
            }
            return Ok(lookup_parameter(&target, state)
                .unwrap_or_default()
                .to_string());
        }
    }

    let Some((name, operator, word)) = parse_parameter_expression(expression) else {
        return Ok(state.lookup(expression).unwrap_or_default().to_string());
    };

    let value = lookup_parameter(name, state).map(str::to_string);
    let is_set = value.is_some();
    let is_non_null = value.as_deref().is_some_and(|value| !value.is_empty());

    match operator {
        ParameterOperator::None => {
            if let Some(value) = value {
                Ok(value)
            } else if state.nounset() {
                Err(unset_parameter_error(name))
            } else {
                Ok(String::new())
            }
        }
        ParameterOperator::Default { require_non_null } => {
            if parameter_condition(is_set, is_non_null, require_non_null) {
                Ok(value.unwrap_or_default())
            } else {
                expand_substitutions(word, state)
            }
        }
        ParameterOperator::AssignDefault { require_non_null } => {
            if parameter_condition(is_set, is_non_null, require_non_null) {
                Ok(value.unwrap_or_default())
            } else {
                if !is_identifier(name) {
                    return Err(ShellError::execution(format!(
                        "{name}: cannot assign default to this parameter"
                    )));
                }
                let expanded = expand_substitutions(word, state)?;
                if !state.try_set_var(name, expanded.clone()) {
                    return Err(ShellError::execution(format!("{name}: readonly variable")));
                }
                Ok(expanded)
            }
        }
        ParameterOperator::Alternate { require_non_null } => {
            if parameter_condition(is_set, is_non_null, require_non_null) {
                expand_substitutions(word, state)
            } else {
                Ok(String::new())
            }
        }
        ParameterOperator::Error { require_non_null } => {
            if parameter_condition(is_set, is_non_null, require_non_null) {
                Ok(value.unwrap_or_default())
            } else {
                let message = if word.is_empty() {
                    if require_non_null {
                        "parameter null or not set"
                    } else {
                        "parameter not set"
                    }
                } else {
                    word
                };
                Err(ShellError::execution(format!("{name}: {message}")))
            }
        }
        ParameterOperator::RemovePrefix { longest } => {
            let pattern = expand_parameter_pattern(word, state)?;
            Ok(remove_pattern_prefix(
                &value.unwrap_or_default(),
                &pattern,
                longest,
            ))
        }
        ParameterOperator::RemoveSuffix { longest } => {
            let pattern = expand_parameter_pattern(word, state)?;
            Ok(remove_pattern_suffix(
                &value.unwrap_or_default(),
                &pattern,
                longest,
            ))
        }
        ParameterOperator::Substitute { mode } => {
            let (pattern, replacement) = split_substitution_word(word);
            let pattern = expand_substitutions(pattern, state)?;
            let replacement = expand_substitutions(replacement, state)?;
            Ok(substitute_pattern(
                &value.unwrap_or_default(),
                &pattern,
                &replacement,
                mode,
            ))
        }
        ParameterOperator::Substring => {
            let chars: Vec<char> = value.unwrap_or_default().chars().collect();
            let len = chars.len() as i64;
            let (offset_expr, length_expr) = match word.split_once(':') {
                Some((o, l)) => (o.trim(), Some(l.trim())),
                None => (word.trim(), None),
            };
            let offset = eval_arithmetic(offset_expr, state)?;
            let start = if offset < 0 {
                (len + offset).max(0)
            } else {
                offset.min(len)
            };
            let end = match length_expr {
                None => len,
                Some(l) => {
                    let length = eval_arithmetic(l, state)?;
                    if length < 0 {
                        (len + length).max(start)
                    } else {
                        (start + length).min(len)
                    }
                }
            };
            let (start, end) = (start as usize, end.max(start) as usize);
            Ok(chars[start..end].iter().collect())
        }
        ParameterOperator::Case { upper, all } => {
            let v = value.unwrap_or_default();
            let convert = |c: char| {
                if upper {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            };
            let result = if all {
                v.chars().map(convert).collect()
            } else {
                let mut chars = v.chars();
                match chars.next() {
                    Some(first) => {
                        let mut s = convert(first).to_string();
                        s.extend(chars);
                        s
                    }
                    None => String::new(),
                }
            };
            Ok(result)
        }
    }
}

fn expand_braced_parameter_fragments(
    expression: &str,
    state: &mut ShellState,
    split_expansions: bool,
    positional_star_join: PositionalStarJoin,
) -> Result<Vec<ExpansionFragment>, ShellError> {
    let Some((name, operator, word)) = parse_parameter_expression(expression) else {
        return Ok(vec![ExpansionFragment::expanded(
            expand_braced_parameter(expression, state, positional_star_join)?,
            split_expansions,
        )]);
    };

    if !matches!(
        operator,
        ParameterOperator::Default { .. }
            | ParameterOperator::AssignDefault { .. }
            | ParameterOperator::Alternate { .. }
            | ParameterOperator::Error { .. }
    ) {
        return Ok(vec![ExpansionFragment::expanded(
            expand_braced_parameter(expression, state, positional_star_join)?,
            split_expansions,
        )]);
    }

    let value = dynamic_special_var(name, state)
        .or_else(|| lookup_parameter(name, state).map(str::to_string));
    let is_set = value.is_some();
    let is_non_null = value.as_deref().is_some_and(|value| !value.is_empty());

    let current_value = || {
        vec![ExpansionFragment::expanded(
            value.clone().unwrap_or_default(),
            split_expansions,
        )]
    };

    match operator {
        ParameterOperator::Default { require_non_null } => {
            if parameter_condition(is_set, is_non_null, require_non_null) {
                Ok(current_value())
            } else {
                expand_parameter_operator_word(word, state, split_expansions, positional_star_join)
            }
        }
        ParameterOperator::AssignDefault { require_non_null } => {
            if parameter_condition(is_set, is_non_null, require_non_null) {
                return Ok(current_value());
            }
            if !is_identifier(name) {
                return Err(ShellError::execution(format!(
                    "{name}: cannot assign default to this parameter"
                )));
            }
            let fragments = expand_parameter_operator_word(
                word,
                state,
                split_expansions,
                positional_star_join,
            )?;
            if !state.try_set_var(name, fragments_to_string(&fragments)) {
                return Err(ShellError::execution(format!("{name}: readonly variable")));
            }
            Ok(fragments)
        }
        ParameterOperator::Alternate { require_non_null } => {
            if parameter_condition(is_set, is_non_null, require_non_null) {
                expand_parameter_operator_word(word, state, split_expansions, positional_star_join)
            } else {
                Ok(Vec::new())
            }
        }
        ParameterOperator::Error { require_non_null } => {
            if parameter_condition(is_set, is_non_null, require_non_null) {
                return Ok(current_value());
            }
            let message = if word.is_empty() {
                if require_non_null {
                    "parameter null or not set".to_string()
                } else {
                    "parameter not set".to_string()
                }
            } else {
                fragments_to_string(&expand_parameter_operator_word(
                    word,
                    state,
                    false,
                    positional_star_join,
                )?)
            };
            Err(ShellError::execution(format!("{name}: {message}")))
        }
        _ => unreachable!("non-word parameter operators returned above"),
    }
}

fn expand_parameter_operator_word(
    word: &str,
    state: &mut ShellState,
    split_expansions: bool,
    positional_star_join: PositionalStarJoin,
) -> Result<Vec<ExpansionFragment>, ShellError> {
    let segments = parse_parameter_operator_word(word)?;
    let mut fragments = Vec::new();
    for segment in &segments {
        let positional_list_is_quoted = segment.quote == QuoteKind::Double || !split_expansions;
        if positional_list_is_quoted && next_quoted_at_marker(&segment.text, 0).is_some() {
            append_quoted_at_fragments(
                &mut fragments,
                &segment.text,
                state,
                if segment.quote == QuoteKind::Double {
                    PositionalStarJoin::IfsFirst
                } else {
                    positional_star_join
                },
            )?;
            continue;
        }

        match segment.quote {
            QuoteKind::Single => push_fragment(
                &mut fragments,
                ExpansionFragment::literal(&segment.text, true, false),
            ),
            QuoteKind::Double => {
                let expanded = expand_substitution_fragments(
                    &segment.text,
                    state,
                    false,
                    PositionalStarJoin::IfsFirst,
                )?;
                append_protected_fragments(&mut fragments, expanded);
            }
            QuoteKind::None => {
                let expanded = expand_substitution_fragments(
                    &segment.text,
                    state,
                    split_expansions,
                    if split_expansions {
                        PositionalStarJoin::Space
                    } else {
                        positional_star_join
                    },
                )?;
                for fragment in expanded {
                    push_fragment(&mut fragments, fragment);
                }
            }
        }
    }

    if segments
        .first()
        .is_some_and(|segment| segment.quote == QuoteKind::None)
    {
        expand_tilde_in_fragments(&mut fragments, state);
    }

    for fragment in &mut fragments {
        if split_expansions && fragment.glob_eligible {
            // The selected operator word becomes the result of an unquoted
            // parameter expansion. Its unquoted literal text therefore takes
            // part in field splitting and pathname expansion too.
            fragment.split_eligible = true;
        } else if !split_expansions {
            fragment.split_eligible = false;
            fragment.glob_eligible = false;
            if !fragment.suppress_empty_field {
                fragment.preserves_field = true;
            }
        }
    }

    // An empty quoted operator word must still materialize one empty field.
    if fragments.is_empty()
        && segments
            .iter()
            .any(|segment| segment.quote != QuoteKind::None)
    {
        fragments.push(ExpansionFragment::literal("", true, false));
    }

    Ok(fragments)
}

fn append_quoted_at_fragments(
    fragments: &mut Vec<ExpansionFragment>,
    text: &str,
    state: &mut ShellState,
    positional_star_join: PositionalStarJoin,
) -> Result<(), ShellError> {
    let mut start = 0;
    while let Some((marker_start, marker_end)) = next_quoted_at_marker(text, start) {
        let prefix = expand_substitution_fragments(
            &text[start..marker_start],
            state,
            false,
            positional_star_join,
        )?;
        append_protected_fragments(fragments, prefix);

        let positionals = state.positionals();
        if positionals.is_empty() {
            push_fragment(fragments, ExpansionFragment::suppress_empty_field());
        } else {
            for (index, positional) in positionals.into_iter().enumerate() {
                push_fragment(
                    fragments,
                    ExpansionFragment::positional(positional, index > 0),
                );
            }
        }
        start = marker_end;
    }

    let suffix = expand_substitution_fragments(&text[start..], state, false, positional_star_join)?;
    append_protected_fragments(fragments, suffix);
    Ok(())
}

fn append_protected_fragments(
    fragments: &mut Vec<ExpansionFragment>,
    mut protected: Vec<ExpansionFragment>,
) {
    for fragment in &mut protected {
        fragment.split_eligible = false;
        fragment.glob_eligible = false;
        if !fragment.suppress_empty_field {
            fragment.preserves_field = true;
        }
    }
    for fragment in protected {
        push_fragment(fragments, fragment);
    }
}

fn expand_parameter_pattern(
    word: &str,
    state: &mut ShellState,
) -> Result<Vec<GlobToken>, ShellError> {
    let segments = parse_parameter_operator_word(word)?;
    let fragments = expand_word_fragments(&segments, state, true)?;
    Ok(fragments
        .into_iter()
        .flat_map(|fragment| {
            let active = fragment.glob_eligible;
            fragment
                .text
                .chars()
                .map(move |ch| GlobToken { ch, active })
                .collect::<Vec<_>>()
        })
        .collect())
}

fn parse_parameter_operator_word(word: &str) -> Result<Vec<WordSegment>, ShellError> {
    let chars = word.chars().collect::<Vec<_>>();
    let mut segments = Vec::new();
    let mut plain = String::new();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '\'' => {
                push_parameter_word_segment(&mut segments, &mut plain, QuoteKind::None, false);
                i += 1;
                let mut quoted = String::new();
                while i < chars.len() && chars[i] != '\'' {
                    quoted.push(chars[i]);
                    i += 1;
                }
                if i == chars.len() {
                    return Err(ShellError::parse(
                        "unterminated single quote in parameter word",
                    ));
                }
                push_parameter_word_segment(&mut segments, &mut quoted, QuoteKind::Single, true);
                i += 1;
            }
            '"' => {
                push_parameter_word_segment(&mut segments, &mut plain, QuoteKind::None, false);
                i += 1;
                let mut quoted = String::new();
                while i < chars.len() {
                    if chars[i] == '"' {
                        break;
                    }
                    if chars[i] == '\\' {
                        match chars.get(i + 1).copied() {
                            Some('\n') => {
                                i += 2;
                                continue;
                            }
                            Some(next @ ('$' | '`' | '"' | '\\')) => {
                                push_parameter_word_segment(
                                    &mut segments,
                                    &mut quoted,
                                    QuoteKind::Double,
                                    false,
                                );
                                let mut escaped = next.to_string();
                                push_parameter_word_segment(
                                    &mut segments,
                                    &mut escaped,
                                    QuoteKind::Single,
                                    true,
                                );
                                i += 2;
                                continue;
                            }
                            _ => {}
                        }
                    }
                    if let Some(next) = parameter_word_substitution_end(&chars, i) {
                        quoted.extend(chars[i..next].iter());
                        i = next;
                        continue;
                    }
                    quoted.push(chars[i]);
                    i += 1;
                }
                if i == chars.len() {
                    return Err(ShellError::parse(
                        "unterminated double quote in parameter word",
                    ));
                }
                push_parameter_word_segment(&mut segments, &mut quoted, QuoteKind::Double, true);
                i += 1;
            }
            '\\' => {
                push_parameter_word_segment(&mut segments, &mut plain, QuoteKind::None, false);
                match chars.get(i + 1).copied() {
                    Some('\n') => i += 2,
                    Some(next) => {
                        let mut escaped = next.to_string();
                        push_parameter_word_segment(
                            &mut segments,
                            &mut escaped,
                            QuoteKind::Single,
                            true,
                        );
                        i += 2;
                    }
                    None => {
                        plain.push('\\');
                        i += 1;
                    }
                }
            }
            _ => {
                if let Some(next) = parameter_word_substitution_end(&chars, i) {
                    plain.extend(chars[i..next].iter());
                    i = next;
                } else {
                    plain.push(chars[i]);
                    i += 1;
                }
            }
        }
    }

    push_parameter_word_segment(&mut segments, &mut plain, QuoteKind::None, false);
    Ok(segments)
}

fn push_parameter_word_segment(
    segments: &mut Vec<WordSegment>,
    text: &mut String,
    quote: QuoteKind,
    preserve_empty: bool,
) {
    if text.is_empty() && !preserve_empty {
        return;
    }
    let value = std::mem::take(text);
    if !value.is_empty() {
        if let Some(previous) = segments.last_mut() {
            if previous.quote == quote && !previous.text.is_empty() {
                previous.text.push_str(&value);
                return;
            }
        }
    }
    segments.push(WordSegment::new(value, quote));
}

fn parameter_word_substitution_end(chars: &[char], start: usize) -> Option<usize> {
    match (chars.get(start), chars.get(start + 1)) {
        (Some('$'), Some('{')) => {
            find_parameter_expansion_end(chars, start + 1).map(|(_, next)| next)
        }
        (Some('$'), Some('(')) => {
            find_command_substitution_end(chars, start + 1).map(|(_, next)| next)
        }
        (Some('`'), _) => read_backtick_command(chars, start).map(|(_, next)| next),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParameterOperator {
    None,
    Default {
        require_non_null: bool,
    },
    AssignDefault {
        require_non_null: bool,
    },
    Alternate {
        require_non_null: bool,
    },
    Error {
        require_non_null: bool,
    },
    RemovePrefix {
        longest: bool,
    },
    RemoveSuffix {
        longest: bool,
    },
    Substitute {
        mode: SubstitutionMode,
    },
    /// `${var:offset}` / `${var:offset:length}` substring (arithmetic operands).
    Substring,
    /// `${var^^}`/`${var^}`/`${var,,}`/`${var,}` case modification (bash 4).
    Case {
        upper: bool,
        all: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubstitutionMode {
    First,
    Global,
    Prefix,
    Suffix,
}

fn parse_parameter_expression(expression: &str) -> Option<(&str, ParameterOperator, &str)> {
    let name_len = parameter_name_len(expression)?;
    let name = &expression[..name_len];
    let rest = &expression[name_len..];
    match rest {
        "" => Some((name, ParameterOperator::None, "")),
        rest if rest.starts_with(":-") => Some((
            name,
            ParameterOperator::Default {
                require_non_null: true,
            },
            &rest[2..],
        )),
        rest if rest.starts_with(":=") => Some((
            name,
            ParameterOperator::AssignDefault {
                require_non_null: true,
            },
            &rest[2..],
        )),
        rest if rest.starts_with('-') => Some((
            name,
            ParameterOperator::Default {
                require_non_null: false,
            },
            &rest[1..],
        )),
        rest if rest.starts_with('=') => Some((
            name,
            ParameterOperator::AssignDefault {
                require_non_null: false,
            },
            &rest[1..],
        )),
        rest if rest.starts_with(":+") => Some((
            name,
            ParameterOperator::Alternate {
                require_non_null: true,
            },
            &rest[2..],
        )),
        rest if rest.starts_with('+') => Some((
            name,
            ParameterOperator::Alternate {
                require_non_null: false,
            },
            &rest[1..],
        )),
        rest if rest.starts_with(":?") => Some((
            name,
            ParameterOperator::Error {
                require_non_null: true,
            },
            &rest[2..],
        )),
        rest if rest.starts_with('?') => Some((
            name,
            ParameterOperator::Error {
                require_non_null: false,
            },
            &rest[1..],
        )),
        // Substring `${var:offset[:length]}`. Checked after the `:-/:=/:+/:?`
        // modifiers so they win; a bare `:` (incl. `: -1`) is a substring.
        rest if rest.starts_with(':') => Some((name, ParameterOperator::Substring, &rest[1..])),
        rest if rest.starts_with("##") => Some((
            name,
            ParameterOperator::RemovePrefix { longest: true },
            &rest[2..],
        )),
        rest if rest.starts_with('#') => Some((
            name,
            ParameterOperator::RemovePrefix { longest: false },
            &rest[1..],
        )),
        rest if rest.starts_with("%%") => Some((
            name,
            ParameterOperator::RemoveSuffix { longest: true },
            &rest[2..],
        )),
        rest if rest.starts_with('%') => Some((
            name,
            ParameterOperator::RemoveSuffix { longest: false },
            &rest[1..],
        )),
        rest if rest.starts_with("//") => Some((
            name,
            ParameterOperator::Substitute {
                mode: SubstitutionMode::Global,
            },
            &rest[2..],
        )),
        rest if rest.starts_with("/#") => Some((
            name,
            ParameterOperator::Substitute {
                mode: SubstitutionMode::Prefix,
            },
            &rest[2..],
        )),
        rest if rest.starts_with("/%") => Some((
            name,
            ParameterOperator::Substitute {
                mode: SubstitutionMode::Suffix,
            },
            &rest[2..],
        )),
        rest if rest.starts_with('/') => Some((
            name,
            ParameterOperator::Substitute {
                mode: SubstitutionMode::First,
            },
            &rest[1..],
        )),
        // Case modification (bash 4): ^^ ^ ,, , — optional trailing pattern.
        rest if rest.starts_with("^^") => Some((
            name,
            ParameterOperator::Case {
                upper: true,
                all: true,
            },
            &rest[2..],
        )),
        rest if rest.starts_with('^') => Some((
            name,
            ParameterOperator::Case {
                upper: true,
                all: false,
            },
            &rest[1..],
        )),
        rest if rest.starts_with(",,") => Some((
            name,
            ParameterOperator::Case {
                upper: false,
                all: true,
            },
            &rest[2..],
        )),
        rest if rest.starts_with(',') => Some((
            name,
            ParameterOperator::Case {
                upper: false,
                all: false,
            },
            &rest[1..],
        )),
        _ => None,
    }
}

fn remove_pattern_prefix(value: &str, pattern: &[GlobToken], longest: bool) -> String {
    let mut matched_index = None;
    for index in char_boundaries(value) {
        if parameter_pattern_matches(pattern, &value[..index]) {
            matched_index = Some(match (matched_index, longest) {
                (Some(previous), false) => previous,
                _ => index,
            });
            if !longest {
                break;
            }
        }
    }

    matched_index
        .map(|index| value[index..].to_string())
        .unwrap_or_else(|| value.to_string())
}

fn remove_pattern_suffix(value: &str, pattern: &[GlobToken], longest: bool) -> String {
    let mut matched_index = None;
    for index in char_boundaries(value) {
        if parameter_pattern_matches(pattern, &value[index..]) {
            matched_index = Some(match (matched_index, longest) {
                (Some(previous), true) => previous,
                _ => index,
            });
            if longest {
                break;
            }
        }
    }

    matched_index
        .map(|index| value[..index].to_string())
        .unwrap_or_else(|| value.to_string())
}

fn parameter_pattern_matches(pattern: &[GlobToken], value: &str) -> bool {
    if pattern.iter().all(|token| token.active) {
        let pattern = pattern.iter().map(|token| token.ch).collect::<String>();
        return glob_match_bytes(pattern.as_bytes(), value.as_bytes());
    }
    glob_match_tokens(pattern, &value.chars().collect::<Vec<_>>())
}

/// Record each pipeline stage's exit code into the `PIPESTATUS` array.
fn record_pipestatus(state: &mut ShellState, exit_codes: &[i32]) {
    state.set_array(
        "PIPESTATUS",
        exit_codes.iter().map(i32::to_string).collect(),
        false,
    );
}

fn bound_observation_preview(bytes: &mut Vec<u8>) -> bool {
    if bytes.len() <= CAPTURE_HEAD + CAPTURE_TAIL {
        return true;
    }
    let input = std::mem::take(bytes);
    *bytes = read_capped_with_tee(std::io::Cursor::new(input), None)
        .expect("reading an in-memory observation cannot fail")
        .preview;
    false
}

fn split_substitution_word(word: &str) -> (&str, &str) {
    word.split_once('/').unwrap_or((word, ""))
}

fn substitute_pattern(
    value: &str,
    pattern: &str,
    replacement: &str,
    mode: SubstitutionMode,
) -> String {
    if pattern.is_empty() {
        return value.to_string();
    }

    match mode {
        SubstitutionMode::First => replace_first_pattern_match(value, pattern, replacement)
            .unwrap_or_else(|| value.to_string()),
        SubstitutionMode::Global => replace_all_pattern_matches(value, pattern, replacement),
        SubstitutionMode::Prefix => replace_prefix_pattern_match(value, pattern, replacement)
            .unwrap_or_else(|| value.to_string()),
        SubstitutionMode::Suffix => replace_suffix_pattern_match(value, pattern, replacement)
            .unwrap_or_else(|| value.to_string()),
    }
}

fn replace_first_pattern_match(value: &str, pattern: &str, replacement: &str) -> Option<String> {
    let (start, end) = find_pattern_match(value, pattern, 0)?;
    Some(format!(
        "{}{}{}",
        &value[..start],
        replacement,
        &value[end..]
    ))
}

fn replace_all_pattern_matches(value: &str, pattern: &str, replacement: &str) -> String {
    let mut out = String::new();
    let mut search_start = 0;

    while search_start < value.len() {
        let Some((start, end)) = find_pattern_match(value, pattern, search_start) else {
            break;
        };
        out.push_str(&value[search_start..start]);
        out.push_str(replacement);
        if end == start {
            if end >= value.len() {
                search_start = end;
                break;
            }
            let next_index = value[end..]
                .char_indices()
                .nth(1)
                .map_or(value.len(), |(offset, _)| end + offset);
            out.push_str(&value[end..next_index]);
            search_start = next_index;
        } else {
            search_start = end;
        }
    }

    out.push_str(&value[search_start..]);
    out
}

fn replace_prefix_pattern_match(value: &str, pattern: &str, replacement: &str) -> Option<String> {
    let pattern_bytes = pattern.as_bytes();
    let value_bytes = value.as_bytes();
    let end = char_boundaries(value)
        .filter(|index| glob_match_bytes(pattern_bytes, &value_bytes[..*index]))
        .max()?;
    Some(format!("{replacement}{}", &value[end..]))
}

fn replace_suffix_pattern_match(value: &str, pattern: &str, replacement: &str) -> Option<String> {
    let pattern_bytes = pattern.as_bytes();
    let value_bytes = value.as_bytes();
    let start = char_boundaries(value)
        .filter(|index| glob_match_bytes(pattern_bytes, &value_bytes[*index..]))
        .min()?;
    Some(format!("{}{replacement}", &value[..start]))
}

fn find_pattern_match(value: &str, pattern: &str, search_start: usize) -> Option<(usize, usize)> {
    let pattern_bytes = pattern.as_bytes();
    let value_bytes = value.as_bytes();
    let boundaries = char_boundaries(value).collect::<Vec<_>>();

    for start in boundaries
        .iter()
        .copied()
        .filter(|index| *index >= search_start)
    {
        let mut best_end = None;
        for end in boundaries.iter().copied().filter(|index| *index >= start) {
            if glob_match_bytes(pattern_bytes, &value_bytes[start..end]) {
                best_end = Some(end);
            }
        }
        if let Some(end) = best_end {
            return Some((start, end));
        }
    }

    None
}

/// Parse a leading extended-glob group `?(..)`/`*(..)`/`+(..)`/`@(..)`/`!(..)`.
/// Returns (operator, alternatives, pattern after the closing `)`), or None.
/// (operator byte, alternatives, pattern remainder) for an extglob group.
type ExtglobGroup<'a> = (u8, Vec<&'a [u8]>, &'a [u8]);

fn parse_extglob_group(pattern: &[u8]) -> Option<ExtglobGroup<'_>> {
    let op = *pattern.first()?;
    if !matches!(op, b'?' | b'*' | b'+' | b'@' | b'!') || pattern.get(1) != Some(&b'(') {
        return None;
    }
    // Find the matching close paren (respecting nesting and `[...]`).
    let mut depth = 0usize;
    let mut i = 1;
    let mut in_class = false;
    while i < pattern.len() {
        match pattern[i] {
            b'[' if !in_class => in_class = true,
            b']' if in_class => in_class = false,
            b'(' if !in_class => depth += 1,
            b')' if !in_class => {
                depth -= 1;
                if depth == 0 {
                    let inner = &pattern[2..i];
                    let rest = &pattern[i + 1..];
                    return Some((op, split_extglob_alts(inner), rest));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split extglob inner on top-level `|` (respecting nested `(`/`[`).
fn split_extglob_alts(inner: &[u8]) -> Vec<&[u8]> {
    let mut alts = Vec::new();
    let mut depth = 0usize;
    let mut in_class = false;
    let mut start = 0;
    for (i, &b) in inner.iter().enumerate() {
        match b {
            b'[' if !in_class => in_class = true,
            b']' if in_class => in_class = false,
            b'(' if !in_class => depth += 1,
            b')' if !in_class && depth > 0 => depth -= 1,
            b'|' if !in_class && depth == 0 => {
                alts.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    alts.push(&inner[start..]);
    alts
}

/// Match an extended-glob group against `name`, then `rest` against the remainder.
fn match_extglob(op: u8, alts: &[&[u8]], rest: &[u8], name: &[u8], depth: usize) -> bool {
    // Whether some alternative fully matches `name[..len]`.
    let alt_matches = |len: usize| {
        alts.iter()
            .any(|alt| glob_match_bytes_with_depth(alt, &name[..len], depth + 1))
    };
    match op {
        b'@' => (0..=name.len())
            .any(|i| alt_matches(i) && glob_match_bytes_with_depth(rest, &name[i..], depth + 1)),
        b'?' => {
            glob_match_bytes_with_depth(rest, name, depth + 1)
                || (1..=name.len()).any(|i| {
                    alt_matches(i) && glob_match_bytes_with_depth(rest, &name[i..], depth + 1)
                })
        }
        b'*' => {
            glob_match_bytes_with_depth(rest, name, depth + 1)
                || (1..=name.len()).any(|i| {
                    alt_matches(i) && match_extglob(b'*', alts, rest, &name[i..], depth + 1)
                })
        }
        b'+' => (1..=name.len()).any(|i| {
            alt_matches(i)
                && (glob_match_bytes_with_depth(rest, &name[i..], depth + 1)
                    || match_extglob(b'*', alts, rest, &name[i..], depth + 1))
        }),
        b'!' => (0..=name.len())
            .any(|i| !alt_matches(i) && glob_match_bytes_with_depth(rest, &name[i..], depth + 1)),
        _ => false,
    }
}

fn glob_match_bytes(pattern: &[u8], name: &[u8]) -> bool {
    glob_match_bytes_with_depth(pattern, name, 0)
}

const MAX_EXTGLOB_MATCH_DEPTH: usize = 256;

fn glob_match_bytes_with_depth(pattern: &[u8], name: &[u8], depth: usize) -> bool {
    if depth > MAX_EXTGLOB_MATCH_DEPTH {
        return false;
    }
    if !pattern
        .windows(2)
        .any(|pair| matches!(pair[0], b'?' | b'*' | b'+' | b'@' | b'!') && pair[1] == b'(')
    {
        return glob_match_plain_bytes(pattern, name);
    }
    // Extended glob groups: ?(..) *(..) +(..) @(..) !(..) with `|` alternation.
    if let Some((op, alts, rest)) = parse_extglob_group(pattern) {
        return match_extglob(op, &alts, rest, name, depth);
    }
    match (pattern.split_first(), name.split_first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some((&b'*', rest)), _) => {
            glob_match_bytes_with_depth(rest, name, depth + 1)
                || name.split_first().is_some_and(|(_, name_rest)| {
                    glob_match_bytes_with_depth(pattern, name_rest, depth + 1)
                })
        }
        (Some((&b'?', rest)), Some((_, name_rest))) => {
            glob_match_bytes_with_depth(rest, name_rest, depth + 1)
        }
        (Some((&b'[', _)), Some((name_ch, name_rest))) => {
            if let Some((matched, rest)) = match_byte_char_class(pattern, *name_ch) {
                matched && glob_match_bytes_with_depth(rest, name_rest, depth + 1)
            } else {
                pattern.first() == Some(name_ch)
                    && glob_match_bytes_with_depth(&pattern[1..], name_rest, depth + 1)
            }
        }
        (Some((pattern_ch, pattern_rest)), Some((name_ch, name_rest))) if pattern_ch == name_ch => {
            glob_match_bytes_with_depth(pattern_rest, name_rest, depth + 1)
        }
        _ => false,
    }
}

fn glob_match_plain_bytes(pattern: &[u8], name: &[u8]) -> bool {
    let (tokens, name_chars) = match (std::str::from_utf8(pattern), std::str::from_utf8(name)) {
        (Ok(pattern), Ok(name)) => (
            pattern
                .chars()
                .map(|ch| GlobToken { ch, active: true })
                .collect::<Vec<_>>(),
            name.chars().collect::<Vec<_>>(),
        ),
        _ => (
            pattern
                .iter()
                .map(|byte| GlobToken {
                    ch: char::from(*byte),
                    active: true,
                })
                .collect::<Vec<_>>(),
            name.iter()
                .map(|byte| char::from(*byte))
                .collect::<Vec<_>>(),
        ),
    };
    glob_match_tokens(&tokens, &name_chars)
}

/// POSIX bracket character class membership (`[:alpha:]` etc.), C/POSIX locale.
fn char_class_matches(class: &[u8], ch: u8) -> bool {
    match class {
        b"alpha" => ch.is_ascii_alphabetic(),
        b"digit" => ch.is_ascii_digit(),
        b"alnum" => ch.is_ascii_alphanumeric(),
        b"upper" => ch.is_ascii_uppercase(),
        b"lower" => ch.is_ascii_lowercase(),
        b"space" => ch.is_ascii_whitespace() || ch == 0x0b,
        b"blank" => ch == b' ' || ch == b'\t',
        b"punct" => ch.is_ascii_punctuation(),
        b"print" => ch.is_ascii_graphic() || ch == b' ',
        b"graph" => ch.is_ascii_graphic(),
        b"cntrl" => ch.is_ascii_control(),
        b"xdigit" => ch.is_ascii_hexdigit(),
        _ => false,
    }
}

fn match_byte_char_class(pattern: &[u8], name_ch: u8) -> Option<(bool, &[u8])> {
    if pattern.first().copied() != Some(b'[') {
        return None;
    }
    let mut index = 1;
    let negate = matches!(pattern.get(index), Some(b'!') | Some(b'^'));
    if negate {
        index += 1;
    }

    let mut matched = false;
    let mut saw_member = false;
    while index < pattern.len() {
        if pattern[index] == b']' && saw_member {
            return Some((
                if negate { !matched } else { matched },
                &pattern[index + 1..],
            ));
        }

        // POSIX character class `[:name:]` (e.g. `[[:alpha:]]`).
        if pattern[index] == b'[' && pattern.get(index + 1) == Some(&b':') {
            if let Some(rel) = pattern[index + 2..].windows(2).position(|w| w == b":]") {
                let class = &pattern[index + 2..index + 2 + rel];
                if char_class_matches(class, name_ch) {
                    matched = true;
                }
                index += 2 + rel + 2; // past the closing ":]"
                saw_member = true;
                continue;
            }
        }

        let start = pattern[index];
        if pattern.get(index + 1) == Some(&b'-')
            && pattern.get(index + 2).is_some()
            && pattern[index + 2] != b']'
        {
            let end = pattern[index + 2];
            let (low, high) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            if (low..=high).contains(&name_ch) {
                matched = true;
            }
            index += 3;
        } else {
            if start == name_ch {
                matched = true;
            }
            index += 1;
        }
        saw_member = true;
    }

    None
}

fn char_boundaries(value: &str) -> impl Iterator<Item = usize> + '_ {
    value
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(value.len()))
}

fn parameter_name_len(expression: &str) -> Option<usize> {
    let mut chars = expression.char_indices();
    let (_, first) = chars.next()?;
    if first == '@' {
        return Some(first.len_utf8());
    }
    if first.is_ascii_digit() {
        let mut end = first.len_utf8();
        for (index, ch) in chars {
            if ch.is_ascii_digit() {
                end = index + ch.len_utf8();
            } else {
                break;
            }
        }
        return Some(end);
    }
    if first != '_' && !first.is_ascii_alphabetic() {
        return None;
    }

    let mut end = first.len_utf8();
    for (index, ch) in chars {
        if ch == '_' || ch.is_ascii_alphanumeric() {
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    Some(end)
}

fn parameter_condition(is_set: bool, is_non_null: bool, require_non_null: bool) -> bool {
    if require_non_null {
        is_non_null
    } else {
        is_set
    }
}

fn lookup_parameter<'a>(name: &str, state: &'a ShellState) -> Option<&'a str> {
    if name == "0" {
        // $0 is the shell/command name; it is not a normal positional parameter.
        return Some(state.arg0());
    }
    if name == "@" {
        state.lookup("@")
    } else {
        // `$name` on an array reads element 0 (bash semantics) when no scalar.
        state.lookup(name).or_else(|| {
            state
                .array(name)
                .and_then(|a| a.first())
                .map(String::as_str)
        })
    }
}

fn expand_required_parameter(name: &str, state: &ShellState) -> Result<String, ShellError> {
    if let Some(value) = dynamic_special_var(name, state) {
        Ok(value)
    } else if let Some(value) = lookup_parameter(name, state) {
        Ok(value.to_string())
    } else if state.nounset() {
        Err(unset_parameter_error(name))
    } else {
        Ok(String::new())
    }
}

/// Dynamic special variables computed on each read (`$RANDOM`, `$SECONDS`).
/// Checked before normal lookup so they behave like bash even if a same-named
/// variable was assigned.
fn dynamic_special_var(name: &str, state: &ShellState) -> Option<String> {
    match name {
        "RANDOM" => Some(state.next_random().to_string()),
        "SECONDS" => Some(state.uptime_secs().to_string()),
        "LINENO" => Some(state.current_line().to_string()),
        "PPID" => Some(
            rustix::process::getppid()
                .map(|p| p.as_raw_nonzero().get().to_string())
                .unwrap_or_else(|| "0".to_string()),
        ),
        _ => None,
    }
}

fn unset_parameter_error(name: &str) -> ShellError {
    ShellError::execution(format!("{name}: parameter not set"))
}

fn joined_positionals(state: &ShellState) -> String {
    state.positionals().join(" ")
}

fn joined_star_positionals(state: &ShellState, mode: PositionalStarJoin) -> String {
    match mode {
        PositionalStarJoin::Space => joined_positionals(state),
        PositionalStarJoin::IfsFirst => state.positionals().join(&quoted_star_separator(state)),
    }
}

fn quoted_star_separator(state: &ShellState) -> String {
    state
        .lookup("IFS")
        .map(|ifs| {
            ifs.chars()
                .next()
                .map(|ch| ch.to_string())
                .unwrap_or_default()
        })
        .unwrap_or_else(|| " ".to_string())
}

fn read_backtick_command(chars: &[char], start_i: usize) -> Option<(String, usize)> {
    let mut command = String::new();
    let mut i = start_i + 1;
    while i < chars.len() {
        match chars[i] {
            '`' => return Some((command, i + 1)),
            '\\' if chars.get(i + 1).is_some() => {
                i += 1;
                command.push(chars[i]);
            }
            ch => command.push(ch),
        }
        i += 1;
    }
    None
}

fn find_parameter_expansion_end(chars: &[char], open_index: usize) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut index = open_index;

    while index < chars.len() {
        let ch = chars[index];
        if let Some(active_quote) = quote {
            match (active_quote, ch) {
                ('\'', '\'') => quote = None,
                ('"', '"') => quote = None,
                ('"', '\\')
                    if chars
                        .get(index + 1)
                        .is_some_and(|next| matches!(next, '$' | '`' | '"' | '\\')) =>
                {
                    index += 2;
                    continue;
                }
                _ => {}
            }
            index += 1;
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '\\' => {
                index += 2;
                continue;
            }
            '`' => {
                let (_, next) = read_backtick_command(chars, index)?;
                index = next;
                continue;
            }
            '$' if chars.get(index + 1) == Some(&'(') => {
                let (_, next) = find_command_substitution_end(chars, index + 1)?;
                index = next;
                continue;
            }
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some((index, index + 1));
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn find_command_substitution_end(chars: &[char], open_index: usize) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    // Skip metacharacters inside quotes so `$(echo ')')` / `$(echo "a)b")` aren't
    // closed at a parenthesis that's actually inside a quoted string.
    let mut quote: Option<char> = None;
    let mut index = open_index;
    while index < chars.len() {
        let ch = chars[index];
        if let Some(active_quote) = quote {
            match (active_quote, ch) {
                ('\'', '\'') => quote = None,
                ('"', '"') => quote = None,
                ('"', '\\')
                    if chars
                        .get(index + 1)
                        .is_some_and(|next| matches!(next, '$' | '`' | '"' | '\\')) =>
                {
                    index += 2;
                    continue;
                }
                _ => {}
            }
            index += 1;
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '\\' => {
                index += 2;
                continue;
            }
            '`' => {
                let (_, next) = read_backtick_command(chars, index)?;
                index = next;
                continue;
            }
            '$' if chars.get(index + 1) == Some(&'{') => {
                let (_, next) = find_parameter_expansion_end(chars, index + 1)?;
                index = next;
                continue;
            }
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some((index, index + 1));
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn find_arithmetic_end(chars: &[char], expr_start: usize) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    let mut index = expr_start;
    while index < chars.len() {
        match chars[index] {
            '(' => depth += 1,
            ')' if chars.get(index + 1).is_some_and(|ch| *ch == ')') && depth == 0 => {
                return Some((index, index + 2));
            }
            ')' => depth = depth.checked_sub(1)?,
            _ => {}
        }
        index += 1;
    }
    None
}

fn private_stdout_routing(
    stdout_writer: io::PipeWriter,
    stderr_writer: io::PipeWriter,
) -> InheritedCaptureRouting {
    InheritedCaptureRouting {
        stdout: CaptureDestination::Pipe {
            kind: StreamingPipeKind::Stdout,
            writer: Arc::new(stdout_writer),
        },
        stderr: CaptureDestination::Pipe {
            kind: StreamingPipeKind::Stderr,
            writer: Arc::new(stderr_writer),
        },
    }
}

type PrivateCaptureResult<T> = (
    T,
    Vec<u8>,
    Option<ExactTraceFile>,
    Vec<u8>,
    Option<ExactTraceFile>,
);

fn run_with_private_stdout_capture<T>(
    stdout_spool: Option<TraceSpoolWriter>,
    stderr_spool: Option<TraceSpoolWriter>,
    run: impl FnOnce() -> Result<T, ShellError>,
) -> Result<PrivateCaptureResult<T>, ShellError> {
    let (stdout_reader, stdout_writer) = io::pipe()?;
    let (stderr_reader, stderr_writer) = io::pipe()?;
    let routing = private_stdout_routing(stdout_writer, stderr_writer);
    let stdout_capture = std::thread::Builder::new()
        .spawn(move || read_capture_stream(stdout_reader, stdout_spool))?;
    let stderr_capture = std::thread::Builder::new()
        .spawn(move || read_capture_stream(stderr_reader, stderr_spool))?;
    let result = with_inherited_capture_routing(routing, run);
    let captured_stdout = join_capture_reader(stdout_capture);
    let captured_stderr = join_capture_reader(stderr_capture);

    match result {
        Err(error) => {
            let _ = captured_stdout;
            let _ = captured_stderr;
            Err(error)
        }
        Ok(value) => {
            let captured_stdout = captured_stdout?;
            let captured_stderr = captured_stderr?;
            Ok((
                value,
                captured_stdout.preview,
                captured_stdout.exact,
                captured_stderr.preview,
                captured_stderr.exact,
            ))
        }
    }
}

fn process_substitution_path(inner: &str, state: &mut ShellState) -> Result<String, ShellError> {
    let graph = parse_line(inner)?;
    let mut sub_state = state.clone();
    // Do not copy diagnostics already queued by an earlier expansion into this
    // nested execution; only this process substitution's own stderr belongs to
    // its outcome.
    sub_state.take_pending_substitution_stderr();
    sub_state.replace_streaming_stdout(None);
    // Process substitution consumes a file path, so the executor can retain an
    // exact disk-backed stream instead of forcing arbitrarily large output into
    // `CommandOutcome::stdout` as command substitution must.
    // With trace storage enabled, use its bounded disk spool. If storage is
    // disabled, fall back to the executor's bounded exact in-memory capture so
    // semantic behavior does not depend on observation retention policy.
    let raw_storage_enabled = state.output_config().raw_storage_options().enabled;
    sub_state.replace_exact_capture(!raw_storage_enabled);
    let private_spool = raw_storage_enabled
        .then(|| state.create_trace_spool("procsub").ok())
        .flatten();
    let private_stderr_spool = raw_storage_enabled
        .then(|| state.create_trace_spool("procsub-err").ok())
        .flatten();
    let private_spooled = private_spool.is_some();
    let private_stderr_spooled = private_stderr_spool.is_some();
    let mut executor = Executor::new();
    let (mut outcome, private_stdout, private_exact, private_stderr, private_stderr_exact) =
        run_with_private_stdout_capture(private_spool, private_stderr_spool, || {
            executor.run_graph(
                &graph,
                &mut sub_state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
        })?;
    if private_spooled && private_exact.is_none() {
        return Err(ShellError::execution(
            "process substitution: exact stdout capture is unavailable",
        ));
    }
    if private_stderr_spooled && private_stderr_exact.is_none() {
        return Err(ShellError::execution(
            "process substitution: exact stderr capture is unavailable",
        ));
    }
    let mut private_stderr_outcome = CommandOutcome::captured_with_exact(
        outcome.exit_code,
        Vec::new(),
        private_stderr,
        None,
        private_stderr_exact,
    );
    let private_stderr =
        take_substitution_stderr(&mut private_stderr_outcome, MAX_IN_MEMORY_CAPTURE_BYTES)?;
    let stderr = take_substitution_stderr(&mut outcome, MAX_IN_MEMORY_CAPTURE_BYTES)?;
    queue_substitution_stderr(state, private_stderr)?;
    queue_substitution_stderr(state, stderr)?;

    let (path, mut file) = create_process_substitution_temp(state).map_err(|error| {
        ShellError::from(io::Error::new(
            error.kind(),
            format!("process substitution: {error}"),
        ))
    })?;
    let mut private_outcome = CommandOutcome::captured_with_exact(
        outcome.exit_code,
        private_stdout,
        Vec::new(),
        private_exact,
        None,
    );
    let write_result = write_process_substitution_stdout(&mut file, &mut private_outcome)
        .and_then(|()| write_process_substitution_stdout(&mut file, &mut outcome));
    if let Err(error) = write_result {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(ShellError::from(io::Error::new(
            error.kind(),
            format!("process substitution: {error}"),
        )));
    }
    drop(file);
    let display = path
        .to_str()
        .ok_or_else(|| {
            ShellError::execution("process substitution: temporary path is not valid UTF-8")
        })?
        .to_string();
    state.register_proc_sub_temp(path);
    Ok(display)
}

fn write_process_substitution_stdout(
    destination: &mut File,
    outcome: &mut CommandOutcome,
) -> io::Result<()> {
    let Some(segments) = outcome.exact_stdout.take() else {
        return destination.write_all(&outcome.stdout);
    };

    if segments.iter().any(|segment| !segment.is_complete()) {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "process-substitution stdout capture is incomplete",
        ));
    }

    for segment in segments {
        match segment {
            ExactTraceSegment::Memory(bytes) => destination.write_all(&bytes)?,
            ExactTraceSegment::File(exact) => {
                let mut source = File::open(exact.path())?;
                io::copy(&mut source, destination)?;
            }
        }
    }
    Ok(())
}

const PROCESS_SUBSTITUTION_TEMP_ATTEMPTS: usize = 128;

fn create_process_substitution_temp(state: &ShellState) -> io::Result<(PathBuf, File)> {
    create_process_substitution_temp_in(&std::env::temp_dir(), state)
}

fn create_process_substitution_temp_in(
    temp_dir: &Path,
    state: &ShellState,
) -> io::Result<(PathBuf, File)> {
    if temp_dir.to_str().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process-substitution temporary directory is not valid UTF-8",
        ));
    }

    for _ in 0..PROCESS_SUBSTITUTION_TEMP_ATTEMPTS {
        let path = temp_dir.join(format!(
            "agsh-procsub-{}-{}",
            std::process::id(),
            state.next_random()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);

        match options.open(&path) {
            Ok(file) => {
                #[cfg(unix)]
                if let Err(error) = file.set_permissions(std::fs::Permissions::from_mode(0o600)) {
                    drop(file);
                    let _ = std::fs::remove_file(path);
                    return Err(error);
                }
                return Ok((path, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique process-substitution temporary file",
    ))
}

fn run_command_substitution(
    command_text: &str,
    state: &mut ShellState,
) -> Result<String, ShellError> {
    let graph = parse_line(command_text)?;
    let mut substitution_state = state.clone();
    substitution_state.take_pending_substitution_stderr();
    substitution_state.replace_streaming_stdout(None);
    substitution_state.replace_exact_capture(true);
    let mut executor = Executor::new();
    let (outcome, mut private_stdout, private_exact, private_stderr, private_stderr_exact) =
        run_with_private_stdout_capture(None, None, || {
            executor.run_graph(
                &graph,
                &mut substitution_state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
        })?;
    debug_assert!(private_exact.is_none());
    debug_assert!(private_stderr_exact.is_none());
    // Record the status so `x=$(cmd)` can report it as `$?`.
    state.set_command_substitution_status(outcome.exit_code);
    let mut outcome = outcome;
    let stderr = take_substitution_stderr(&mut outcome, MAX_IN_MEMORY_CAPTURE_BYTES)?;
    queue_substitution_stderr(state, private_stderr)?;
    queue_substitution_stderr(state, stderr)?;
    let combined = private_stdout
        .len()
        .checked_add(outcome.stdout.len())
        .ok_or_else(|| substitution_stdout_limit_error(MAX_IN_MEMORY_CAPTURE_BYTES))?;
    if combined > MAX_IN_MEMORY_CAPTURE_BYTES {
        return Err(substitution_stdout_limit_error(MAX_IN_MEMORY_CAPTURE_BYTES));
    }
    private_stdout.extend_from_slice(&outcome.stdout);
    let mut text = String::from_utf8_lossy(&private_stdout).to_string();
    while text.ends_with('\n') {
        text.pop();
    }
    Ok(text)
}

fn substitution_stdout_limit_error(limit: usize) -> ShellError {
    ShellError::from(io::Error::new(
        io::ErrorKind::FileTooLarge,
        format!("in-memory shell capture exceeds {limit} bytes"),
    ))
}

fn take_substitution_stderr(
    outcome: &mut CommandOutcome,
    limit: usize,
) -> Result<Vec<u8>, ShellError> {
    let Some(segments) = outcome.exact_stderr.take() else {
        let stderr = std::mem::take(&mut outcome.stderr);
        if stderr.len() > limit {
            return Err(substitution_stderr_limit_error(limit));
        }
        return Ok(stderr);
    };

    if segments.iter().any(|segment| !segment.is_complete()) {
        return Err(ShellError::from(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "substitution stderr capture is incomplete",
        )));
    }

    let mut stderr = Vec::new();
    for segment in segments {
        match segment {
            ExactTraceSegment::Memory(bytes) => {
                let next_len = stderr
                    .len()
                    .checked_add(bytes.len())
                    .ok_or_else(|| substitution_stderr_limit_error(limit))?;
                if next_len > limit {
                    return Err(substitution_stderr_limit_error(limit));
                }
                stderr.extend_from_slice(&bytes);
            }
            ExactTraceSegment::File(exact) => {
                let file_len = usize::try_from(std::fs::metadata(exact.path())?.len())
                    .map_err(|_| substitution_stderr_limit_error(limit))?;
                let next_len = stderr
                    .len()
                    .checked_add(file_len)
                    .ok_or_else(|| substitution_stderr_limit_error(limit))?;
                if next_len > limit {
                    return Err(substitution_stderr_limit_error(limit));
                }
                let mut source = File::open(exact.path())?;
                source.read_to_end(&mut stderr)?;
            }
        }
    }
    Ok(stderr)
}

fn queue_substitution_stderr(state: &mut ShellState, stderr: Vec<u8>) -> Result<(), ShellError> {
    queue_substitution_stderr_with_limit(state, stderr, MAX_IN_MEMORY_CAPTURE_BYTES)
}

fn queue_substitution_stderr_with_limit(
    state: &mut ShellState,
    stderr: Vec<u8>,
    limit: usize,
) -> Result<(), ShellError> {
    let pending = state.take_pending_substitution_stderr();
    let Some(next_len) = pending.len().checked_add(stderr.len()) else {
        state.append_pending_substitution_stderr(pending);
        return Err(substitution_stderr_limit_error(limit));
    };
    if next_len > limit {
        state.append_pending_substitution_stderr(pending);
        return Err(substitution_stderr_limit_error(limit));
    }
    state.append_pending_substitution_stderr(pending);
    state.append_pending_substitution_stderr(stderr);
    Ok(())
}

fn substitution_stderr_limit_error(limit: usize) -> ShellError {
    ShellError::from(io::Error::new(
        io::ErrorKind::FileTooLarge,
        format!("substitution stderr exceeds {limit} bytes"),
    ))
}

/// Adapter so float expressions can read shell variables as numbers.
pub(crate) struct StateFloatVars<'a>(pub(crate) &'a ShellState);

impl crate::math::FloatVars for StateFloatVars<'_> {
    fn get(&self, name: &str) -> Option<f64> {
        self.0
            .lookup(name)
            .and_then(|v| v.trim().parse::<f64>().ok())
    }
}

pub(crate) fn eval_arithmetic(expression: &str, state: &mut ShellState) -> Result<i64, ShellError> {
    ArithmeticParser::new(expression, state).parse()
}

struct ArithmeticParser<'a> {
    chars: Vec<char>,
    index: usize,
    state: &'a mut ShellState,
    /// Active recursion depth; bounds pathological nesting (`((((…))))`, `~~~…`,
    /// `1**1**…`) before it can overflow the stack.
    depth: usize,
}

/// Max recursive-descent depth for `$(( … ))`; keeps the worst case well under
/// the stack while exceeding any real expression.
const MAX_ARITH_DEPTH: usize = 512;

impl<'a> ArithmeticParser<'a> {
    fn new(expression: &str, state: &'a mut ShellState) -> Self {
        Self {
            chars: expression.chars().collect(),
            index: 0,
            state,
            depth: 0,
        }
    }

    /// Run `body` one recursion level deeper, erroring if too deeply nested.
    fn guarded_arith(
        &mut self,
        body: impl FnOnce(&mut Self) -> Result<i64, ShellError>,
    ) -> Result<i64, ShellError> {
        self.depth += 1;
        if self.depth > MAX_ARITH_DEPTH {
            self.depth -= 1;
            return Err(ShellError::parse("arithmetic expression nested too deeply"));
        }
        let r = body(self);
        self.depth -= 1;
        r
    }

    fn parse(mut self) -> Result<i64, ShellError> {
        // An empty arithmetic expression evaluates to 0.
        self.skip_ws();
        if self.index == self.chars.len() {
            return Ok(0);
        }
        let value = self.parse_expr()?;
        self.skip_ws();
        if self.index != self.chars.len() {
            return Err(ShellError::parse("invalid arithmetic expression"));
        }
        Ok(value)
    }

    fn parse_expr(&mut self) -> Result<i64, ShellError> {
        self.parse_assignment()
    }

    /// Lowest precedence, right-associative: `lvalue [op]= rhs`.
    fn parse_assignment(&mut self) -> Result<i64, ShellError> {
        let save = self.index;
        self.skip_ws();
        if let Some(name) = self.try_read_identifier() {
            self.skip_ws();
            if let Some(op) = self.try_read_assignment_op() {
                // Guard the right-associative RHS recursion (`a=a=…=1`) so a long
                // assignment chain errors cleanly instead of overflowing the stack.
                let rhs = self.guarded_arith(|s| s.parse_assignment())?;
                let value = self.apply_assignment(&name, op, rhs)?;
                return Ok(value);
            }
        }
        self.index = save;
        self.parse_ternary()
    }

    fn try_read_identifier(&mut self) -> Option<String> {
        let start = self.index;
        match self.peek() {
            Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
            _ => return None,
        }
        self.index += 1;
        while self
            .peek()
            .is_some_and(|c| c == '_' || c.is_ascii_alphanumeric())
        {
            self.index += 1;
        }
        Some(self.chars[start..self.index].iter().collect())
    }

    /// Read an assignment operator (`=`, `+=`, `<<=`, ...) but never `==`.
    fn try_read_assignment_op(&mut self) -> Option<&'static str> {
        let two = (self.peek(), self.peek_at(1));
        let three = self.peek_at(2);
        let op: &'static str = match two {
            (Some('<'), Some('<')) if three == Some('=') => "<<=",
            (Some('>'), Some('>')) if three == Some('=') => ">>=",
            (Some('+'), Some('=')) => "+=",
            (Some('-'), Some('=')) => "-=",
            (Some('*'), Some('=')) => "*=",
            (Some('/'), Some('=')) => "/=",
            (Some('%'), Some('=')) => "%=",
            (Some('&'), Some('=')) => "&=",
            (Some('|'), Some('=')) => "|=",
            (Some('^'), Some('=')) => "^=",
            (Some('='), next) if next != Some('=') => "=",
            _ => return None,
        };
        self.index += op.len();
        Some(op)
    }

    fn apply_assignment(&mut self, name: &str, op: &str, rhs: i64) -> Result<i64, ShellError> {
        let current = self.read_var(name);
        let value = match op {
            "=" => rhs,
            "+=" => current.wrapping_add(rhs),
            "-=" => current.wrapping_sub(rhs),
            "*=" => current.wrapping_mul(rhs),
            "/=" if rhs == 0 => return Err(ShellError::execution("division by zero")),
            "/=" => current.wrapping_div(rhs),
            "%=" if rhs == 0 => return Err(ShellError::execution("division by zero")),
            "%=" => current.wrapping_rem(rhs),
            "&=" => current & rhs,
            "|=" => current | rhs,
            "^=" => current ^ rhs,
            "<<=" => current.wrapping_shl(rhs as u32),
            ">>=" => current.wrapping_shr(rhs as u32),
            _ => return Err(ShellError::parse("invalid assignment operator")),
        };
        self.write_var(name, value)?;
        Ok(value)
    }

    fn read_var(&self, name: &str) -> i64 {
        self.state
            .lookup(name)
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(0)
    }

    fn write_var(&mut self, name: &str, value: i64) -> Result<(), ShellError> {
        if self.state.try_set_var(name, value.to_string()) {
            Ok(())
        } else {
            Err(ShellError::execution(format!("{name}: readonly variable")))
        }
    }

    fn parse_ternary(&mut self) -> Result<i64, ShellError> {
        let condition = self.parse_logical_or()?;
        self.skip_ws();
        if self.peek() == Some('?') {
            self.index += 1;
            // Guard the ternary recursion (`1?1?…?1`) against stack overflow.
            let when_true = self.guarded_arith(|s| s.parse_ternary())?;
            self.skip_ws();
            if self.peek() != Some(':') {
                return Err(ShellError::parse("missing ':' in arithmetic ternary"));
            }
            self.index += 1;
            let when_false = self.guarded_arith(|s| s.parse_ternary())?;
            return Ok(if condition != 0 {
                when_true
            } else {
                when_false
            });
        }
        Ok(condition)
    }

    fn parse_logical_or(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_logical_and()?;
        loop {
            self.skip_ws();
            if self.eat2('|', '|') {
                let rhs = self.parse_logical_and()?;
                value = i64::from(value != 0 || rhs != 0);
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_logical_and(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_bit_or()?;
        loop {
            self.skip_ws();
            if self.eat2('&', '&') {
                let rhs = self.parse_bit_or()?;
                value = i64::from(value != 0 && rhs != 0);
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_bit_or(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_bit_xor()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('|') && self.peek_at(1) != Some('|') {
                self.index += 1;
                value |= self.parse_bit_xor()?;
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_bit_xor(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_bit_and()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('^') {
                self.index += 1;
                value ^= self.parse_bit_and()?;
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_bit_and(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_equality()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('&') && self.peek_at(1) != Some('&') {
                self.index += 1;
                value &= self.parse_equality()?;
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_equality(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_relational()?;
        loop {
            self.skip_ws();
            if self.eat2('=', '=') {
                value = i64::from(value == self.parse_relational()?);
            } else if self.eat2('!', '=') {
                value = i64::from(value != self.parse_relational()?);
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_relational(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_shift()?;
        loop {
            self.skip_ws();
            if self.eat2('<', '=') {
                value = i64::from(value <= self.parse_shift()?);
            } else if self.eat2('>', '=') {
                value = i64::from(value >= self.parse_shift()?);
            } else if self.peek() == Some('<') && self.peek_at(1) != Some('<') {
                self.index += 1;
                value = i64::from(value < self.parse_shift()?);
            } else if self.peek() == Some('>') && self.peek_at(1) != Some('>') {
                self.index += 1;
                value = i64::from(value > self.parse_shift()?);
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_shift(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_additive()?;
        loop {
            self.skip_ws();
            if self.eat2('<', '<') {
                let rhs = self.parse_additive()?;
                value = value.wrapping_shl(rhs as u32);
            } else if self.eat2('>', '>') {
                let rhs = self.parse_additive()?;
                value = value.wrapping_shr(rhs as u32);
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_additive(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_term()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('+') => {
                    self.index += 1;
                    value = value.wrapping_add(self.parse_term()?);
                }
                Some('-') => {
                    self.index += 1;
                    value = value.wrapping_sub(self.parse_term()?);
                }
                _ => return Ok(value),
            }
        }
    }

    fn parse_term(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_power()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('*') if self.peek_at(1) != Some('*') => {
                    self.index += 1;
                    value = value.wrapping_mul(self.parse_power()?);
                }
                Some('/') => {
                    self.index += 1;
                    let rhs = self.parse_power()?;
                    if rhs == 0 {
                        return Err(ShellError::execution("division by zero"));
                    }
                    value = value.wrapping_div(rhs);
                }
                Some('%') => {
                    self.index += 1;
                    let rhs = self.parse_power()?;
                    if rhs == 0 {
                        return Err(ShellError::execution("division by zero"));
                    }
                    value = value.wrapping_rem(rhs);
                }
                _ => return Ok(value),
            }
        }
    }

    fn parse_power(&mut self) -> Result<i64, ShellError> {
        self.guarded_arith(Self::parse_power_inner)
    }

    fn parse_power_inner(&mut self) -> Result<i64, ShellError> {
        let base = self.parse_factor()?;
        self.skip_ws();
        if self.eat2('*', '*') {
            let exponent = self.parse_power()?;
            if exponent < 0 {
                return Ok(0);
            }
            return Ok(base.wrapping_pow(exponent.min(u32::MAX as i64) as u32));
        }
        Ok(base)
    }

    fn parse_factor(&mut self) -> Result<i64, ShellError> {
        self.guarded_arith(Self::parse_factor_inner)
    }

    fn parse_factor_inner(&mut self) -> Result<i64, ShellError> {
        self.skip_ws();
        // Prefix increment/decrement: ++var / --var.
        if self.eat2('+', '+') {
            self.skip_ws();
            let name = self
                .try_read_identifier()
                .ok_or_else(|| ShellError::parse("arithmetic ++ requires a variable"))?;
            let value = self.read_var(&name).wrapping_add(1);
            self.write_var(&name, value)?;
            return Ok(value);
        }
        if self.eat2('-', '-') {
            self.skip_ws();
            let name = self
                .try_read_identifier()
                .ok_or_else(|| ShellError::parse("arithmetic -- requires a variable"))?;
            let value = self.read_var(&name).wrapping_sub(1);
            self.write_var(&name, value)?;
            return Ok(value);
        }
        match self.peek() {
            Some('+') => {
                self.index += 1;
                self.parse_factor()
            }
            Some('-') => {
                self.index += 1;
                Ok(self.parse_factor()?.wrapping_neg())
            }
            Some('!') => {
                self.index += 1;
                Ok(i64::from(self.parse_factor()? == 0))
            }
            Some('~') => {
                self.index += 1;
                Ok(!self.parse_factor()?)
            }
            Some('(') => {
                self.index += 1;
                let value = self.parse_expr()?;
                self.skip_ws();
                if self.peek() != Some(')') {
                    return Err(ShellError::parse("missing ')' in arithmetic expression"));
                }
                self.index += 1;
                Ok(value)
            }
            Some(ch) if ch.is_ascii_digit() => self.parse_number(),
            Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => self.parse_variable(),
            _ => Err(ShellError::parse("invalid arithmetic expression")),
        }
    }

    fn parse_number(&mut self) -> Result<i64, ShellError> {
        let start = self.index;
        // Hexadecimal: 0x.. / 0X..
        if self.peek() == Some('0') && matches!(self.peek_at(1), Some('x') | Some('X')) {
            self.index += 2;
            let digits_start = self.index;
            while self.peek().is_some_and(|ch| ch.is_ascii_hexdigit()) {
                self.index += 1;
            }
            let digits = self.chars[digits_start..self.index]
                .iter()
                .collect::<String>();
            return i64::from_str_radix(&digits, 16)
                .map_err(|_| ShellError::parse("invalid arithmetic number"));
        }

        while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            self.index += 1;
        }
        let text = self.chars[start..self.index].iter().collect::<String>();
        // Octal: a leading 0 with more digits (POSIX arithmetic).
        if text.len() > 1 && text.starts_with('0') {
            return i64::from_str_radix(&text, 8)
                .map_err(|_| ShellError::parse("invalid arithmetic number"));
        }
        text.parse::<i64>()
            .map_err(|_| ShellError::parse("invalid arithmetic number"))
    }

    fn parse_variable(&mut self) -> Result<i64, ShellError> {
        let start = self.index;
        self.index += 1;
        while self
            .peek()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        {
            self.index += 1;
        }
        let name = self.chars[start..self.index].iter().collect::<String>();
        let current = self.read_var(&name);
        // Postfix increment/decrement: var++ / var-- (returns the old value).
        if self.peek() == Some('+') && self.peek_at(1) == Some('+') {
            self.index += 2;
            self.write_var(&name, current.wrapping_add(1))?;
            return Ok(current);
        }
        if self.peek() == Some('-') && self.peek_at(1) == Some('-') {
            self.index += 2;
            self.write_var(&name, current.wrapping_sub(1))?;
            return Ok(current);
        }
        Ok(current)
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.index += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.index).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.index + offset).copied()
    }

    /// Consume a two-character operator if it is next, after skipping whitespace.
    fn eat2(&mut self, first: char, second: char) -> bool {
        if self.peek() == Some(first) && self.peek_at(1) == Some(second) {
            self.index += 2;
            true
        } else {
            false
        }
    }
}

enum ExternalCaptureSink {
    Pipe(io::PipeWriter),
    File(File),
    Inherit(InheritedOutput),
    Discard,
}

struct ExternalRedirectionContext<'a> {
    stdin_is_piped: &'a mut bool,
    merge_stderr_to_stdout: &'a mut bool,
    merge_stdout_to_stderr: &'a mut bool,
    capture_outputs: bool,
    noclobber: bool,
    ordered_sinks: &'a mut Option<(ExternalCaptureSink, ExternalCaptureSink)>,
}

impl ExternalCaptureSink {
    fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Pipe(writer) => writer.try_clone().map(Self::Pipe),
            Self::File(file) => file.try_clone().map(Self::File),
            Self::Inherit(output) => Ok(Self::Inherit(*output)),
            Self::Discard => Ok(Self::Discard),
        }
    }

    fn stdio(&self) -> io::Result<Stdio> {
        match self {
            Self::Pipe(writer) => writer.try_clone().map(Stdio::from),
            Self::File(file) => file.try_clone().map(Stdio::from),
            Self::Inherit(InheritedOutput::Stdout) => {
                io::stdout().as_fd().try_clone_to_owned().map(Stdio::from)
            }
            Self::Inherit(InheritedOutput::Stderr) => {
                io::stderr().as_fd().try_clone_to_owned().map(Stdio::from)
            }
            Self::Discard => Ok(Stdio::null()),
        }
    }
}

fn external_sink_for_destination(
    destination: &CaptureDestination,
    base_stdout: &ExternalCaptureSink,
    base_stderr: &ExternalCaptureSink,
) -> io::Result<ExternalCaptureSink> {
    match destination {
        CaptureDestination::Stdout => base_stdout.try_clone(),
        CaptureDestination::Stderr => base_stderr.try_clone(),
        CaptureDestination::File(file) => file.try_clone().map(ExternalCaptureSink::File),
        CaptureDestination::Pipe { writer, .. } => {
            writer.try_clone().map(ExternalCaptureSink::Pipe)
        }
        CaptureDestination::Discard => Ok(ExternalCaptureSink::Discard),
    }
}

fn apply_external_redirections(
    command: &mut Command,
    redirections: &[ExpandedRedirection],
    context: &mut ExternalRedirectionContext<'_>,
) -> Result<(), ShellError> {
    let mut stdout_file: Option<File> = None;
    let mut stderr_file: Option<File> = None;

    for redirection in redirections {
        if let Some((stdout_sink, stderr_sink)) = context.ordered_sinks.as_mut() {
            match (&redirection.mode, &redirection.target) {
                (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close)
                    if redirection.fd == 1 =>
                {
                    *stdout_sink = ExternalCaptureSink::Discard;
                    command.stdout(Stdio::null());
                    continue;
                }
                (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close)
                    if redirection.fd == 2 =>
                {
                    *stderr_sink = ExternalCaptureSink::Discard;
                    command.stderr(Stdio::null());
                    continue;
                }
                (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(1))
                    if redirection.fd == 2 =>
                {
                    let replacement = stdout_sink.try_clone()?;
                    command.stderr(replacement.stdio()?);
                    *stderr_sink = replacement;
                    continue;
                }
                (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(2))
                    if redirection.fd == 1 =>
                {
                    let replacement = stderr_sink.try_clone()?;
                    command.stdout(replacement.stdio()?);
                    *stdout_sink = replacement;
                    continue;
                }
                _ => {}
            }
        }
        match (&redirection.mode, &redirection.target) {
            (RedirectionMode::Read, ExpandedRedirectionTarget::Path(path)) => {
                command.stdin(Stdio::from(open_read_redirection(path)?));
                *context.stdin_is_piped = false;
            }
            // Heredoc/herestring bytes are written to the child's piped stdin by
            // the caller (via stdin_data), so leave the piped stdin in place.
            (
                RedirectionMode::HereDoc | RedirectionMode::HereString,
                ExpandedRedirectionTarget::Bytes(_),
            ) => {}
            (RedirectionMode::Write, ExpandedRedirectionTarget::Path(path)) => {
                let file = open_write_redirection(path, context.noclobber, false)?;
                if redirection.fd == 1 {
                    if let Some((sink, _)) = context.ordered_sinks.as_mut() {
                        *sink = ExternalCaptureSink::File(file.try_clone()?);
                    }
                    stdout_file = Some(file.try_clone()?);
                    command.stdout(Stdio::from(file));
                } else if redirection.fd == 2 {
                    if let Some((_, sink)) = context.ordered_sinks.as_mut() {
                        *sink = ExternalCaptureSink::File(file.try_clone()?);
                    }
                    stderr_file = Some(file.try_clone()?);
                    command.stderr(Stdio::from(file));
                }
            }
            (RedirectionMode::WriteClobber, ExpandedRedirectionTarget::Path(path)) => {
                let file = open_write_redirection(path, context.noclobber, true)?;
                if redirection.fd == 1 {
                    if let Some((sink, _)) = context.ordered_sinks.as_mut() {
                        *sink = ExternalCaptureSink::File(file.try_clone()?);
                    }
                    stdout_file = Some(file.try_clone()?);
                    command.stdout(Stdio::from(file));
                } else if redirection.fd == 2 {
                    if let Some((_, sink)) = context.ordered_sinks.as_mut() {
                        *sink = ExternalCaptureSink::File(file.try_clone()?);
                    }
                    stderr_file = Some(file.try_clone()?);
                    command.stderr(Stdio::from(file));
                }
            }
            (RedirectionMode::Append, ExpandedRedirectionTarget::Path(path)) => {
                let file = OpenOptions::new().create(true).append(true).open(path)?;
                if redirection.fd == 1 {
                    if let Some((sink, _)) = context.ordered_sinks.as_mut() {
                        *sink = ExternalCaptureSink::File(file.try_clone()?);
                    }
                    stdout_file = Some(file.try_clone()?);
                    command.stdout(Stdio::from(file));
                } else if redirection.fd == 2 {
                    if let Some((_, sink)) = context.ordered_sinks.as_mut() {
                        *sink = ExternalCaptureSink::File(file.try_clone()?);
                    }
                    stderr_file = Some(file.try_clone()?);
                    command.stderr(Stdio::from(file));
                }
            }
            (RedirectionMode::WriteBoth, ExpandedRedirectionTarget::Path(path)) => {
                let file = open_write_redirection(path, context.noclobber, false)?;
                if let Some((stdout_sink, stderr_sink)) = context.ordered_sinks.as_mut() {
                    *stdout_sink = ExternalCaptureSink::File(file.try_clone()?);
                    *stderr_sink = ExternalCaptureSink::File(file.try_clone()?);
                }
                stdout_file = Some(file.try_clone()?);
                stderr_file = Some(file.try_clone()?);
                command.stderr(Stdio::from(file.try_clone()?));
                command.stdout(Stdio::from(file));
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 0 => {
                command.stdin(Stdio::null());
                *context.stdin_is_piped = false;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 1 => {
                stdout_file = None;
                *context.merge_stdout_to_stderr = false;
                command.stdout(Stdio::null());
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 2 => {
                stderr_file = None;
                *context.merge_stderr_to_stdout = false;
                command.stderr(Stdio::null());
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(1)) if redirection.fd == 2 => {
                if let Some(file) = &stdout_file {
                    command.stderr(Stdio::from(file.try_clone()?));
                } else if context.capture_outputs {
                    *context.merge_stderr_to_stdout = true;
                    command.stderr(Stdio::piped());
                } else {
                    // `2>&1` with fd1 still at the process stdout: send stderr to
                    // a duplicate of the current stdout, so `2>&1 1>file` leaves
                    // stderr on the terminal's stdout (matches bash ordering).
                    command.stderr(Stdio::from(std::io::stdout().as_fd().try_clone_to_owned()?));
                }
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(2)) if redirection.fd == 1 => {
                if let Some(file) = &stderr_file {
                    command.stdout(Stdio::from(file.try_clone()?));
                } else if context.capture_outputs {
                    *context.merge_stdout_to_stderr = true;
                    command.stdout(Stdio::piped());
                } else {
                    command.stdout(Stdio::from(std::io::stderr().as_fd().try_clone_to_owned()?));
                }
            }
            _ => {
                return Err(ShellError::unsupported(format!(
                    "unsupported redirection for fd {}",
                    redirection.fd
                )));
            }
        }
    }

    Ok(())
}

fn open_write_redirection(path: &str, noclobber: bool, force: bool) -> Result<File, ShellError> {
    if noclobber && !force {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(ShellError::from)
    } else {
        File::create(path).map_err(ShellError::from)
    }
}

fn open_read_redirection(path: &str) -> io::Result<File> {
    File::open(path).map_err(|error| io::Error::new(error.kind(), format!("{path}: {error}")))
}

/// Where a builtin's fd points after applying redirections, resolved in order.
enum BuiltinSink {
    Stdout,
    Stderr,
    Discard,
    File(File),
    Pipe(io::PipeWriter),
}

impl BuiltinSink {
    fn try_clone(&self) -> Result<BuiltinSink, ShellError> {
        Ok(match self {
            BuiltinSink::Stdout => BuiltinSink::Stdout,
            BuiltinSink::Stderr => BuiltinSink::Stderr,
            BuiltinSink::Discard => BuiltinSink::Discard,
            BuiltinSink::File(file) => BuiltinSink::File(file.try_clone()?),
            BuiltinSink::Pipe(writer) => BuiltinSink::Pipe(writer.try_clone()?),
        })
    }

    fn captured_stream(&self) -> Option<OutputStream> {
        match self {
            Self::Stdout => Some(OutputStream::Stdout),
            Self::Stderr => Some(OutputStream::Stderr),
            Self::Discard | Self::File(_) | Self::Pipe(_) => None,
        }
    }
}

fn inherited_builtin_sinks() -> Result<(BuiltinSink, BuiltinSink), ShellError> {
    let routing = inherited_capture_routing();
    Ok((
        builtin_sink_for_destination(&routing.stdout)?,
        builtin_sink_for_destination(&routing.stderr)?,
    ))
}

fn builtin_sink_for_destination(
    destination: &CaptureDestination,
) -> Result<BuiltinSink, ShellError> {
    Ok(match destination {
        CaptureDestination::Stdout => BuiltinSink::Stdout,
        CaptureDestination::Stderr => BuiltinSink::Stderr,
        CaptureDestination::File(file) => BuiltinSink::File(file.try_clone()?),
        CaptureDestination::Pipe { writer, .. } => BuiltinSink::Pipe(writer.try_clone()?),
        CaptureDestination::Discard => BuiltinSink::Discard,
    })
}

fn apply_builtin_redirections(
    outcome: &mut CommandOutcome,
    redirections: &[ExpandedRedirection],
    state: &ShellState,
) -> Result<(), ShellError> {
    let routing = inherited_capture_routing();
    if redirections.is_empty() && routing.is_default() {
        return Ok(());
    }

    // Track the live destination of fd1 and fd2, mutating them in source order
    // so `>file 2>&1` and `2>&1 1>file` resolve with correct ordering semantics.
    let (mut dest1, mut dest2) = inherited_builtin_sinks()?;

    for redirection in redirections {
        match (&redirection.mode, &redirection.target) {
            // stdin redirections (file, heredoc, herestring) are handled via the
            // builtin's buffered stdin, not the output sinks.
            (RedirectionMode::Read | RedirectionMode::HereDoc | RedirectionMode::HereString, _) => {
            }
            (
                RedirectionMode::Write | RedirectionMode::WriteClobber | RedirectionMode::Append,
                ExpandedRedirectionTarget::Path(path),
            ) => {
                let file = match redirection.mode {
                    RedirectionMode::Append => {
                        OpenOptions::new().create(true).append(true).open(path)?
                    }
                    RedirectionMode::WriteClobber => {
                        open_write_redirection(path, state.noclobber(), true)?
                    }
                    _ => open_write_redirection(path, state.noclobber(), false)?,
                };
                if redirection.fd == 1 {
                    dest1 = BuiltinSink::File(file);
                } else if redirection.fd == 2 {
                    dest2 = BuiltinSink::File(file);
                }
            }
            (RedirectionMode::WriteBoth, ExpandedRedirectionTarget::Path(path)) => {
                let file = open_write_redirection(path, state.noclobber(), false)?;
                dest2 = BuiltinSink::File(file.try_clone()?);
                dest1 = BuiltinSink::File(file);
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 1 => {
                dest1 = BuiltinSink::Discard;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 2 => {
                dest2 = BuiltinSink::Discard;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 0 => {}
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(1)) if redirection.fd == 2 => {
                dest2 = dest1.try_clone()?;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(2)) if redirection.fd == 1 => {
                dest1 = dest2.try_clone()?;
            }
            _ => {
                return Err(ShellError::unsupported(format!(
                    "unsupported redirection for fd {}",
                    redirection.fd
                )));
            }
        }
    }

    let spans = outcome
        .validated_output_order()
        .unwrap_or_else(|| initial_output_order(outcome.stdout.len(), outcome.stderr.len()));
    route_exact_stream_destinations(outcome, dest1.captured_stream(), dest2.captured_stream());

    // Route in original emission order. Taking both buffers first keeps fd swaps
    // correct while a shared file/stream receives alternating writes in order.
    let stdout_bytes = std::mem::take(&mut outcome.stdout);
    let stderr_bytes = std::mem::take(&mut outcome.stderr);
    outcome.output_order = Some(Vec::new());
    for span in spans {
        let bytes = match span.stream {
            OutputStream::Stdout => &stdout_bytes[span.start..span.start + span.len],
            OutputStream::Stderr => &stderr_bytes[span.start..span.start + span.len],
        };
        let sink = match span.stream {
            OutputStream::Stdout => &mut dest1,
            OutputStream::Stderr => &mut dest2,
        };
        resolve_builtin_sink(sink, bytes, outcome)?;
    }
    Ok(())
}

fn resolve_builtin_sink(
    sink: &mut BuiltinSink,
    bytes: &[u8],
    outcome: &mut CommandOutcome,
) -> Result<(), ShellError> {
    match sink {
        BuiltinSink::Stdout => {
            let start = outcome.stdout.len();
            outcome.stdout.extend_from_slice(bytes);
            if let Some(order) = &mut outcome.output_order {
                push_output_span(
                    order,
                    OutputSpan {
                        stream: OutputStream::Stdout,
                        start,
                        len: bytes.len(),
                    },
                );
            }
        }
        BuiltinSink::Stderr => {
            let start = outcome.stderr.len();
            outcome.stderr.extend_from_slice(bytes);
            if let Some(order) = &mut outcome.output_order {
                push_output_span(
                    order,
                    OutputSpan {
                        stream: OutputStream::Stderr,
                        start,
                        len: bytes.len(),
                    },
                );
            }
        }
        BuiltinSink::Discard => {}
        BuiltinSink::File(file) => file.write_all(bytes)?,
        BuiltinSink::Pipe(writer) => writer.write_all(bytes)?,
    }
    Ok(())
}

fn route_exact_stream_destinations(
    outcome: &mut CommandOutcome,
    stdout_destination: Option<OutputStream>,
    stderr_destination: Option<OutputStream>,
) {
    match (stdout_destination, stderr_destination) {
        (Some(OutputStream::Stdout), Some(OutputStream::Stderr)) => {}
        (Some(OutputStream::Stderr), Some(OutputStream::Stdout)) => {
            std::mem::swap(&mut outcome.exact_stdout, &mut outcome.exact_stderr);
            std::mem::swap(
                &mut outcome.stdout_preview_complete,
                &mut outcome.stderr_preview_complete,
            );
        }
        (Some(OutputStream::Stdout), Some(OutputStream::Stdout)) => {
            outcome.merge_exact_stderr_into_stdout();
            outcome.exact_stderr = None;
            outcome.stderr_preview_complete = true;
        }
        (Some(OutputStream::Stderr), Some(OutputStream::Stderr)) => {
            outcome.merge_exact_stdout_into_stderr();
            outcome.exact_stdout = None;
            outcome.stdout_preview_complete = true;
        }
        (Some(OutputStream::Stdout), None) => {
            outcome.exact_stderr = None;
            outcome.stderr_preview_complete = true;
        }
        (Some(OutputStream::Stderr), None) => {
            outcome.exact_stderr = outcome.exact_stdout.take();
            outcome.stderr_preview_complete = outcome.stdout_preview_complete;
            outcome.stdout_preview_complete = true;
        }
        (None, Some(OutputStream::Stdout)) => {
            outcome.exact_stdout = outcome.exact_stderr.take();
            outcome.stdout_preview_complete = outcome.stderr_preview_complete;
            outcome.stderr_preview_complete = true;
        }
        (None, Some(OutputStream::Stderr)) => {
            outcome.exact_stdout = None;
            outcome.stdout_preview_complete = true;
        }
        (None, None) => {
            outcome.exact_stdout = None;
            outcome.exact_stderr = None;
            outcome.stdout_preview_complete = true;
            outcome.stderr_preview_complete = true;
        }
    }
}

fn has_unquoted_brace(segments: &[WordSegment]) -> bool {
    segments.iter().any(|segment| {
        segment.quote == QuoteKind::None
            && segment.text.contains('{')
            && segment.text.contains('}')
            // A comma (list `{a,b}`) or `..` (range `{1..5}`) makes it expandable.
            && (segment.text.contains(',') || segment.text.contains(".."))
    })
}

/// Caps that keep pathological brace input (`{1..1000000000}`, `{a,b}` repeated
/// dozens of times, `{{{{…}}}}`) from hanging or OOM-ing the shell. Both sit far
/// above any realistic use; input that would exceed them is left un-expanded
/// (literal) rather than crashing — never silently truncated to a wrong result.
const MAX_BRACE_ELEMENTS: usize = 1 << 20; // 1,048,576 generated words
const MAX_BRACE_DEPTH: usize = 64; // nested `{…}` levels

fn expand_braces(input: &str) -> Vec<String> {
    expand_braces_depth(input, 0)
}

fn expand_braces_depth(input: &str, depth: usize) -> Vec<String> {
    if depth > MAX_BRACE_DEPTH {
        return vec![input.to_string()];
    }
    let Some((open, close)) = find_brace_pair(input) else {
        return vec![input.to_string()];
    };
    let inner = &input[open + 1..close];
    let split = split_brace_alternatives(inner);
    let alternatives = if split.len() > 1 {
        split
    } else if let Some(range) = expand_brace_range(inner) {
        // `{1..5}`, `{5..1}`, `{0..10..2}`, `{a..e}` numeric/char sequences.
        range
    } else {
        return vec![input.to_string()];
    };

    let prefix = &input[..open];
    let suffix = &input[close + 1..];
    let suffix_expanded = expand_braces_depth(suffix, depth + 1);
    let mut out = Vec::new();
    for alternative in alternatives {
        // Each alternative may itself contain nested braces, e.g. {a,b{1,2}}.
        for expanded_alt in expand_braces_depth(&alternative, depth + 1) {
            for expanded_suffix in &suffix_expanded {
                if out.len() >= MAX_BRACE_ELEMENTS {
                    // Combinatorial blow-up: leave the original text literal.
                    return vec![input.to_string()];
                }
                out.push(format!("{prefix}{expanded_alt}{expanded_suffix}"));
            }
        }
    }
    out
}

fn find_brace_pair(input: &str) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    let mut open = None;
    for (index, ch) in input.char_indices() {
        match ch {
            '{' => {
                if depth == 0 {
                    open = Some(index);
                }
                depth += 1;
            }
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return open.map(|open| (open, index));
                }
            }
            _ => {}
        }
    }
    None
}

/// Expand a brace range `a..b` or `a..b..step` into a sequence, or `None` if
/// `inner` is not a valid numeric or single-char range. Numeric ranges honor
/// leading-zero padding (`{01..10}`); char ranges cover single ASCII letters.
fn expand_brace_range(inner: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = inner.split("..").collect();
    if parts.len() != 2 && parts.len() != 3 {
        return None;
    }
    let step_tok = parts.get(2).copied();

    // Numeric range.
    if let (Ok(start), Ok(end)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
        let step = match step_tok {
            None => 1,
            Some(s) => {
                let v = s.parse::<i64>().ok()?;
                // saturating_abs avoids a panic on i64::MIN.
                if v == 0 {
                    1
                } else {
                    v.saturating_abs()
                }
            }
        };
        // Bail out (leaving the brace literal) if the sequence would exceed the
        // element cap — computed without allocating, so a giant range like
        // `{1..1000000000}` can't OOM the shell.
        if start.abs_diff(end) / (step as u64) >= MAX_BRACE_ELEMENTS as u64 {
            return None;
        }
        let pad = brace_pad_width(parts[0], parts[1]);
        let mut out = Vec::new();
        let mut n = start;
        if start <= end {
            while n <= end {
                out.push(format_brace_number(n, pad));
                // checked_add: near i64::MAX this would otherwise overflow —
                // a panic in debug, an infinite loop via wraparound in release.
                match n.checked_add(step) {
                    Some(next) => n = next,
                    None => break,
                }
            }
        } else {
            while n >= end {
                out.push(format_brace_number(n, pad));
                match n.checked_sub(step) {
                    Some(next) => n = next,
                    None => break,
                }
            }
        }
        return Some(out);
    }

    // Single-character range (ASCII letters).
    let (sb, eb) = (parts[0].as_bytes(), parts[1].as_bytes());
    if sb.len() == 1 && eb.len() == 1 && sb[0].is_ascii_alphabetic() && eb[0].is_ascii_alphabetic()
    {
        let step: i32 = match step_tok {
            None => 1,
            Some(s) => {
                let v = s.parse::<i64>().ok()?;
                // Clamp the magnitude to a sane positive i32 (a char range spans
                // at most 26): avoids the `as i32` wrap that could make the step
                // negative and spin the loop forever.
                v.unsigned_abs().min(i32::MAX as u64).max(1) as i32
            }
        };
        let (s, e) = (sb[0] as i32, eb[0] as i32);
        let mut out = Vec::new();
        let mut c = s;
        if s <= e {
            while c <= e {
                out.push((c as u8 as char).to_string());
                match c.checked_add(step) {
                    Some(next) => c = next,
                    None => break,
                }
            }
        } else {
            while c >= e {
                out.push((c as u8 as char).to_string());
                match c.checked_sub(step) {
                    Some(next) => c = next,
                    None => break,
                }
            }
        }
        return Some(out);
    }
    None
}

/// Zero-pad width for a numeric brace range: nonzero only when an operand has an
/// explicit leading zero (e.g. `{01..10}` -> width 2).
fn brace_pad_width(a: &str, b: &str) -> usize {
    fn digits(s: &str) -> &str {
        s.trim_start_matches(['-', '+'])
    }
    fn leading_zero(s: &str) -> bool {
        let d = digits(s);
        d.len() > 1 && d.starts_with('0')
    }
    if leading_zero(a) || leading_zero(b) {
        digits(a).len().max(digits(b).len())
    } else {
        0
    }
}

fn format_brace_number(n: i64, pad: usize) -> String {
    if pad == 0 {
        n.to_string()
    } else if n < 0 {
        format!("-{:0>width$}", -n, width = pad)
    } else {
        format!("{:0>width$}", n, width = pad)
    }
}

fn split_brace_alternatives(input: &str) -> Vec<String> {
    let mut alternatives = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for ch in input.chars() {
        match ch {
            '{' => {
                depth += 1;
                current.push(ch);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => {
                alternatives.push(current);
                current = String::new();
            }
            _ => current.push(ch),
        }
    }
    alternatives.push(current);
    alternatives
}

#[derive(Debug, Clone)]
struct GlobPart {
    text: String,
    mask: Vec<bool>,
}

impl GlobPart {
    fn has_active_glob(&self) -> bool {
        self.text
            .chars()
            .zip(&self.mask)
            .any(|(ch, active)| *active && is_glob_metachar(ch))
    }

    fn starts_with_literal_dot(&self) -> bool {
        self.text.starts_with('.')
    }

    fn tokens(&self) -> Vec<GlobToken> {
        self.text
            .chars()
            .zip(&self.mask)
            .map(|(ch, active)| GlobToken {
                ch,
                active: *active,
            })
            .collect()
    }

    fn tokens_lowercased(&self) -> Vec<GlobToken> {
        self.text
            .chars()
            .zip(&self.mask)
            .map(|(ch, active)| GlobToken {
                ch: ch.to_ascii_lowercase(),
                active: *active,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy)]
struct GlobToken {
    ch: char,
    active: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct GlobOpts {
    globstar: bool,
    dotglob: bool,
    nocaseglob: bool,
}

fn expand_glob(pattern: &str, glob_mask: &[bool], cwd: &Path, opts: GlobOpts) -> Vec<String> {
    let mut matches = Vec::new();
    let is_absolute = pattern.starts_with('/');
    let directory_only = pattern.ends_with('/');
    let parts = split_glob_parts(pattern, glob_mask);
    let real_base = if is_absolute {
        PathBuf::from("/")
    } else {
        cwd.to_path_buf()
    };
    let display_base = if is_absolute {
        PathBuf::from("/")
    } else {
        PathBuf::new()
    };

    expand_glob_parts(&real_base, &display_base, &parts, &mut matches, opts);
    if directory_only {
        matches.retain(|matched| {
            let path = Path::new(matched);
            let real_path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            real_path.is_dir()
        });
        for matched in &mut matches {
            if !matched.ends_with('/') {
                matched.push('/');
            }
        }
    }
    matches.sort();
    matches
}

/// Recursively emit every file and directory under `real_base` (for a trailing
/// `**` globstar component). Hidden entries are skipped unless `dotglob`.
fn glob_recurse_all(
    real_base: &Path,
    display_base: &Path,
    matches: &mut Vec<String>,
    dotglob: bool,
) {
    let Ok(entries) = std::fs::read_dir(real_base) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && !dotglob {
            continue;
        }
        let display = display_base.join(&name);
        matches.push(display.display().to_string());
        if entry.path().is_dir() {
            glob_recurse_all(&entry.path(), &display, matches, dotglob);
        }
    }
}

fn split_glob_parts(pattern: &str, glob_mask: &[bool]) -> Vec<GlobPart> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut mask = Vec::new();

    for (ch, active) in pattern.chars().zip(glob_mask) {
        if ch == '/' {
            parts.push(GlobPart {
                text: std::mem::take(&mut text),
                mask: std::mem::take(&mut mask),
            });
        } else {
            text.push(ch);
            mask.push(*active);
        }
    }

    parts.push(GlobPart { text, mask });
    parts
}

fn expand_glob_parts(
    real_base: &Path,
    display_base: &Path,
    parts: &[GlobPart],
    matches: &mut Vec<String>,
    opts: GlobOpts,
) {
    let Some((part, rest)) = parts.split_first() else {
        let display = display_base.display().to_string();
        if !display.is_empty() {
            matches.push(display);
        }
        return;
    };

    if part.text.is_empty() {
        expand_glob_parts(real_base, display_base, rest, matches, opts);
        return;
    }

    // Globstar `**`: matches zero or more path segments (shopt globstar).
    if opts.globstar && part.text == "**" && part.has_active_glob() {
        if rest.is_empty() {
            // `**` at the end: every file and dir recursively, plus the base.
            let display = display_base.display().to_string();
            if !display.is_empty() {
                matches.push(display);
            }
            glob_recurse_all(real_base, display_base, matches, opts.dotglob);
        } else {
            // Zero segments: match the rest here.
            expand_glob_parts(real_base, display_base, rest, matches, opts);
            // One or more segments: descend into each subdirectory, keeping `**`.
            if let Ok(entries) = std::fs::read_dir(real_base) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if (name.starts_with('.') && !opts.dotglob) || !entry.path().is_dir() {
                        continue;
                    }
                    expand_glob_parts(
                        &entry.path(),
                        &display_base.join(&name),
                        parts,
                        matches,
                        opts,
                    );
                }
            }
        }
        return;
    }

    if part.has_active_glob() {
        let Ok(entries) = std::fs::read_dir(real_base) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if glob_component_matches(part, &name, opts) {
                expand_glob_parts(
                    &entry.path(),
                    &display_base.join(&name),
                    rest,
                    matches,
                    opts,
                );
            }
        }
    } else {
        // A literal path component in a glob pattern must actually exist for the
        // overall pattern to match; otherwise the whole pattern stays literal.
        let candidate = real_base.join(&part.text);
        if candidate.symlink_metadata().is_ok() {
            expand_glob_parts(
                &candidate,
                &display_base.join(&part.text),
                rest,
                matches,
                opts,
            );
        }
    }
}

/// Whether `pattern` contains an extended-glob group opener (`?(`/`*(`/etc.).
fn pattern_has_extglob(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    bytes
        .windows(2)
        .any(|w| matches!(w[0], b'?' | b'*' | b'+' | b'@' | b'!') && w[1] == b'(')
}

fn glob_component_matches(pattern: &GlobPart, name: &str, opts: GlobOpts) -> bool {
    if name.starts_with('.') && !pattern.starts_with_literal_dot() && !opts.dotglob {
        return false;
    }
    // Extended-glob components (?(..) *(..) +(..) @(..) !(..)) use the byte
    // matcher, which understands extglob groups.
    if pattern_has_extglob(&pattern.text) {
        return glob_match_bytes(pattern.text.as_bytes(), name.as_bytes());
    }
    if opts.nocaseglob {
        let tokens = pattern.tokens_lowercased();
        let name_chars = name.to_lowercase().chars().collect::<Vec<_>>();
        return glob_match_tokens(&tokens, &name_chars);
    }
    let tokens = pattern.tokens();
    let name_chars = name.chars().collect::<Vec<_>>();
    glob_match_tokens(&tokens, &name_chars)
}

fn glob_match_tokens(pattern: &[GlobToken], name: &[char]) -> bool {
    const MAX_MATCH_WORK: usize = 8_000_000;

    let mut current = vec![false; name.len() + 1];
    let mut next = vec![false; name.len() + 1];
    current[0] = true;
    let mut pattern_index = 0usize;
    let mut work = 0usize;

    while pattern_index < pattern.len() {
        next.fill(false);
        let token = &pattern[pattern_index];
        let mut consumed = 1usize;

        if token.active && token.ch == '*' {
            while pattern
                .get(pattern_index + consumed)
                .is_some_and(|next| next.active && next.ch == '*')
            {
                consumed += 1;
            }
            work = work.saturating_add(name.len() + 1);
            if work > MAX_MATCH_WORK {
                return false;
            }
            next[0] = current[0];
            for index in 1..=name.len() {
                next[index] = current[index] || next[index - 1];
            }
        } else if token.active && token.ch == '?' {
            work = work.saturating_add(name.len());
            if work > MAX_MATCH_WORK {
                return false;
            }
            next[1..].copy_from_slice(&current[..name.len()]);
        } else if token.active && token.ch == '[' {
            if let Some((_, rest)) = match_char_class(&pattern[pattern_index..], '\0') {
                consumed = pattern[pattern_index..].len() - rest.len();
                work = work.saturating_add(consumed.saturating_mul(name.len().max(1)));
                if work > MAX_MATCH_WORK {
                    return false;
                }
                for index in 1..=name.len() {
                    next[index] = current[index - 1]
                        && match_char_class(&pattern[pattern_index..], name[index - 1])
                            .is_some_and(|(matched, _)| matched);
                }
            } else {
                for index in 1..=name.len() {
                    next[index] = current[index - 1] && token.ch == name[index - 1];
                }
            }
        } else {
            work = work.saturating_add(name.len());
            if work > MAX_MATCH_WORK {
                return false;
            }
            for index in 1..=name.len() {
                next[index] = current[index - 1] && token.ch == name[index - 1];
            }
        }

        std::mem::swap(&mut current, &mut next);
        if !current.iter().any(|matched| *matched) {
            return false;
        }
        pattern_index += consumed;
    }

    current[name.len()]
}

fn match_char_class(pattern: &[GlobToken], name_ch: char) -> Option<(bool, &[GlobToken])> {
    if !pattern
        .first()
        .is_some_and(|token| token.ch == '[' && token.active)
    {
        return None;
    }
    let mut index = 1;
    let negate = matches!(
        pattern.get(index),
        Some(GlobToken {
            ch: '!' | '^',
            active: true
        })
    );
    if negate {
        index += 1;
    }

    let mut matched = false;
    let mut saw_member = false;
    while index < pattern.len() {
        if pattern[index].ch == ']' && pattern[index].active && saw_member {
            return Some((
                if negate { !matched } else { matched },
                &pattern[index + 1..],
            ));
        }

        // POSIX character class `[:name:]`.
        if pattern[index].ch == '[' && pattern.get(index + 1).is_some_and(|t| t.ch == ':') {
            let mut j = index + 2;
            let mut class = String::new();
            let mut close = None;
            while j + 1 < pattern.len() {
                if pattern[j].ch == ':' && pattern[j + 1].ch == ']' {
                    close = Some(j);
                    break;
                }
                class.push(pattern[j].ch);
                j += 1;
            }
            if let Some(close) = close {
                if name_ch.is_ascii() && char_class_matches(class.as_bytes(), name_ch as u8) {
                    matched = true;
                }
                index = close + 2; // past ":]"
                saw_member = true;
                continue;
            }
        }

        let start = pattern[index].ch;
        if pattern
            .get(index + 1)
            .is_some_and(|token| token.ch == '-' && token.active)
            && pattern.get(index + 2).is_some()
            && pattern[index + 2].ch != ']'
        {
            let end = pattern[index + 2].ch;
            let (low, high) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            if (low..=high).contains(&name_ch) {
                matched = true;
            }
            index += 3;
        } else {
            if start == name_ch {
                matched = true;
            }
            index += 1;
        }
        saw_member = true;
    }

    None
}

pub fn print_captured_if_needed(
    outcome: &CommandOutcome,
    _options: &ExecutionOptions,
) -> Result<(), ShellError> {
    if let Some(observation) = &outcome.observation {
        if !observation.display.is_empty() {
            print!("{}", observation.display);
            pipe_ok(std::io::stdout().flush())?;
        }
    } else {
        pipe_ok(std::io::stdout().write_all(&outcome.stdout))?;
        pipe_ok(std::io::stderr().write_all(&outcome.stderr))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agsh_core::parse_line;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn private_test_directory(label: &str) -> PathBuf {
        let path = unique_temp_dir(label);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn confine_shims_are_built_privately_before_path_mutation() {
        use std::os::unix::fs::MetadataExt;

        let base = private_test_directory("private-confine-shims");
        let mut state = ShellState::from_current_process();
        state.export_var("PATH", "/usr/bin:/bin");
        state.export_var("SHELL", "/bin/sh");

        let directory = install_confine_shims_in(&mut state, &base, Path::new("/bin/sh")).unwrap();

        let directory_metadata = std::fs::symlink_metadata(&directory).unwrap();
        assert!(directory_metadata.is_dir());
        assert!(!directory_metadata.file_type().is_symlink());
        assert_eq!(directory_metadata.permissions().mode() & 0o7777, 0o700);
        assert_eq!(
            directory_metadata.uid(),
            rustix::process::geteuid().as_raw()
        );
        for name in ["bash", "sh", "zsh", "dash", "ksh"] {
            let path = directory.join(name);
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            assert!(metadata.is_file(), "{path:?} is not a regular file");
            assert!(!metadata.file_type().is_symlink());
            assert_eq!(metadata.permissions().mode() & 0o7777, 0o500);
            assert_eq!(metadata.uid(), rustix::process::geteuid().as_raw());
            assert!(std::fs::read_to_string(path).unwrap().contains("/bin/sh"));
        }
        let directory_text = directory.to_str().unwrap();
        assert_eq!(
            state.lookup("PATH"),
            Some(format!("{directory_text}:/usr/bin:/bin").as_str())
        );
        assert_eq!(state.lookup("SHELL"), directory.join("bash").to_str());

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn unsafe_shim_parent_is_rejected_without_state_mutation() {
        let base = unique_temp_dir("unsafe-shim-parent");
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o777)).unwrap();
        let mut state = ShellState::from_current_process();
        state.export_var("PATH", "/original/path");
        state.export_var("SHELL", "/original/shell");

        let error = install_confine_shims_in(&mut state, &base, Path::new("/bin/sh")).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(state.lookup("PATH"), Some("/original/path"));
        assert_eq!(state.lookup("SHELL"), Some("/original/shell"));
        assert!(std::fs::read_dir(&base).unwrap().next().is_none());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn failed_shim_generation_rolls_back_its_private_directory() {
        let base = private_test_directory("shim-generation-rollback");
        let definitions = [("sh", "first"), ("sh", "duplicate")];

        let error = build_shim_generation(&base, "intercept", &definitions).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(std::fs::read_dir(&base).unwrap().next().is_none());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn executable_shim_creation_does_not_follow_a_planted_symlink() {
        use std::os::unix::fs::symlink;

        let base = private_test_directory("shim-file-symlink");
        let directory = create_private_shim_directory(&base, "intercept").unwrap();
        let target = base.join("target");
        std::fs::write(&target, b"unchanged").unwrap();
        symlink(&target, directory.join("sh")).unwrap();

        assert!(create_executable_shim(&directory, "sh", "hostile").is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"unchanged");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_shim_parent_is_rejected_before_generation() {
        use std::os::unix::ffi::OsStringExt;

        let base = private_test_directory("shim-non-utf8-parent");
        let invalid = base.join(std::ffi::OsString::from_vec(vec![b'x', 0xff]));

        let error = create_private_shim_directory(&invalid, "confine").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!invalid.exists());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn intercept_install_failure_leaves_mode_state_unchanged() {
        let base = private_test_directory("intercept-install-failure");
        let mut state = ShellState::from_current_process();
        state.export_var("PATH", "");
        state.export_var("SHELL", "/original/shell");
        state.export_var("AGSH_TRACE_DIR", "/original/traces");

        let error = install_intercept_shims_in(
            &mut state,
            OutputMode::Semantic,
            false,
            &base,
            Path::new("/bin/sh"),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(state.lookup("PATH"), Some(""));
        assert_eq!(state.lookup("SHELL"), Some("/original/shell"));
        assert_eq!(state.lookup("AGSH_TRACE_DIR"), Some("/original/traces"));
        assert!(!intercept_active(&state));
        assert!(std::fs::read_dir(&base).unwrap().next().is_none());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn intercept_uninstall_restores_exact_path_and_non_bash_shell() {
        let base = private_test_directory("intercept-exact-restore");
        let real_bin = base.join("real-bin");
        std::fs::create_dir(&real_bin).unwrap();
        std::fs::set_permissions(&real_bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        let real_shell = real_bin.join("sh");
        std::fs::write(&real_shell, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&real_shell, std::fs::Permissions::from_mode(0o500)).unwrap();
        let original_path = format!("{}:/custom/bin", real_bin.display());
        let mut state = ShellState::from_current_process();
        state.export_var("PATH", original_path.clone());
        state.export_var("SHELL", "/bin/zsh");
        state.export_var("GIT_TERMINAL_PROMPT", "custom");

        let generation = install_intercept_shims_in(
            &mut state,
            OutputMode::Semantic,
            false,
            &base,
            Path::new("/bin/sh"),
        )
        .unwrap();
        assert!(intercept_active(&state));
        assert_ne!(state.lookup("PATH"), Some(original_path.as_str()));

        uninstall_intercept(&mut state);

        assert_eq!(state.lookup("PATH"), Some(original_path.as_str()));
        assert_eq!(state.lookup("SHELL"), Some("/bin/zsh"));
        assert_eq!(state.lookup("GIT_TERMINAL_PROMPT"), Some("custom"));
        assert!(state.lookup("GCM_INTERACTIVE").is_none());
        assert!(state.lookup("SSH_ASKPASS_REQUIRE").is_none());
        assert!(!intercept_active(&state));
        assert!(generation.is_dir(), "active children still need this path");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn intercept_uninstall_restores_preexisting_deep_bindings_exactly() {
        let base = private_test_directory("intercept-deep-exact-restore");
        let real_bin = base.join("real-bin");
        std::fs::create_dir(&real_bin).unwrap();
        std::fs::set_permissions(&real_bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        let real_shell = real_bin.join("sh");
        std::fs::write(&real_shell, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&real_shell, std::fs::Permissions::from_mode(0o500)).unwrap();
        let mut state = ShellState::from_current_process();
        state.export_var("PATH", real_bin.to_str().unwrap());
        state.export_var("SHELL", "/bin/sh");
        state.export_var("LD_PRELOAD", "/user/original-preload.so");
        state.export_var("DYLD_INSERT_LIBRARIES", "/user/original-dyld.dylib");
        state.unset("AGSH_SELF");
        state.set_var("AGSH_SELF", "/user/shell-local-agsh");
        state.export_var("AGSH_INTERCEPT_MODE", "user-mode");

        install_intercept_shims_in(
            &mut state,
            OutputMode::Semantic,
            false,
            &base,
            Path::new("/bin/sh"),
        )
        .unwrap();
        // Model the mutations performed after shim installation when the deep
        // interposer is available on either supported platform.
        state.export_var(
            "LD_PRELOAD",
            "/generated/libagsh_intercept.so:/user/original-preload.so",
        );
        state.export_var(
            "DYLD_INSERT_LIBRARIES",
            "/generated/libagsh_intercept.dylib:/user/original-dyld.dylib",
        );
        state.export_var("AGSH_SELF", "/generated/agsh");
        state.export_var("AGSH_INTERCEPT_MODE", "semantic");

        uninstall_intercept(&mut state);

        assert_eq!(
            state.lookup("LD_PRELOAD"),
            Some("/user/original-preload.so")
        );
        assert_eq!(
            state.lookup("DYLD_INSERT_LIBRARIES"),
            Some("/user/original-dyld.dylib")
        );
        assert_eq!(state.lookup("AGSH_SELF"), Some("/user/shell-local-agsh"));
        assert_eq!(state.lookup("AGSH_INTERCEPT_MODE"), Some("user-mode"));
        assert!(state.is_exported("LD_PRELOAD"));
        assert!(state.is_exported("DYLD_INSERT_LIBRARIES"));
        assert!(!state.is_exported("AGSH_SELF"));
        assert!(state.is_exported("AGSH_INTERCEPT_MODE"));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn intercept_uninstall_preserves_unrelated_marker_substrings() {
        let mut state = ShellState::from_current_process();
        let unrelated = "/opt/user-agsh-intercept-tools/bin";
        state.export_var("PATH", format!("{unrelated}:/usr/bin"));
        state.export_var("SHELL", "/bin/sh");

        assert!(!intercept_active(&state));
        uninstall_intercept(&mut state);

        assert_eq!(
            state.lookup("PATH"),
            Some(format!("{unrelated}:/usr/bin").as_str())
        );
        assert_eq!(state.lookup("SHELL"), Some("/bin/sh"));
    }

    #[test]
    fn intercept_legacy_uninstall_uses_an_available_non_bash_shell() {
        let base = private_test_directory("intercept-legacy-non-bash");
        let real_bin = base.join("real-bin");
        std::fs::create_dir(&real_bin).unwrap();
        std::fs::set_permissions(&real_bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        let real_shell = real_bin.join("sh");
        std::fs::write(&real_shell, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&real_shell, std::fs::Permissions::from_mode(0o500)).unwrap();
        let original_path = real_bin.to_str().unwrap();
        let mut state = ShellState::from_current_process();
        state.export_var("PATH", original_path);
        state.export_var("SHELL", "/original/shell");

        install_intercept_shims_in(
            &mut state,
            OutputMode::Semantic,
            false,
            &base,
            Path::new("/bin/sh"),
        )
        .unwrap();
        let _ = state.take_intercept_install();

        uninstall_intercept(&mut state);

        assert_eq!(state.lookup("PATH"), Some(original_path));
        assert_eq!(state.lookup("SHELL"), real_shell.to_str());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn read_capped_returns_small_input_exactly() {
        let out = read_capped(std::io::Cursor::new(b"hello world".to_vec())).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn exact_in_memory_capture_has_a_hard_limit() {
        let mut within = std::io::Cursor::new(b"1234".to_vec());
        assert_eq!(read_exact_capture(&mut within, 4).unwrap(), b"1234");

        let mut oversized = std::io::Cursor::new(b"12345".to_vec());
        let error = read_exact_capture(&mut oversized, 4).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
        assert!(error.to_string().contains("exceeds 4 bytes"));
    }

    #[test]
    fn pty_capture_has_an_aggregate_limit() {
        let mut output = b"1234".to_vec();
        append_bounded_pty_output(&mut output, b"5", 5).unwrap();
        let error = append_bounded_pty_output(&mut output, b"6", 5).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(output, b"12345");
    }

    #[test]
    fn pending_substitution_stderr_has_an_aggregate_limit() {
        let mut state = ShellState::from_current_process();
        queue_substitution_stderr_with_limit(&mut state, b"1234".to_vec(), 4).unwrap();

        let error = queue_substitution_stderr_with_limit(&mut state, b"5".to_vec(), 4).unwrap_err();
        assert_eq!(error.kind, ShellErrorKind::Io);
        assert!(error
            .message
            .contains("substitution stderr exceeds 4 bytes"));
        assert_eq!(state.take_pending_substitution_stderr(), b"1234");
    }

    #[test]
    fn mixed_stage_cleanup_interrupts_and_joins_shell_threads() {
        let state = ShellState::from_current_process();
        let interrupt = Arc::new(AtomicBool::new(false));
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (started_send, started_receive) = std::sync::mpsc::channel();
        let thread_interrupt = Arc::clone(&interrupt);
        let thread_counter = Arc::clone(&counter);
        let thread = std::thread::spawn(move || {
            thread_counter.fetch_add(1, Ordering::Relaxed);
            started_send.send(()).unwrap();
            while !thread_interrupt.load(Ordering::Acquire) {
                thread_counter.fetch_add(1, Ordering::Relaxed);
                std::thread::yield_now();
            }
            Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()))
        });
        started_receive
            .recv_timeout(Duration::from_secs(1))
            .expect("shell stage started");
        let mut stages = vec![RunningStreamingStage::Shell(RunningShellStage {
            thread,
            interrupt: Arc::clone(&interrupt),
        })];

        terminate_running_streaming_stages(&mut stages, &state);

        assert!(stages.is_empty());
        assert!(!interrupt.load(Ordering::Acquire));
        let stopped_at = counter.load(Ordering::Relaxed);
        assert!(stopped_at > 0);
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(counter.load(Ordering::Relaxed), stopped_at);
    }

    #[cfg(unix)]
    #[test]
    fn mixed_stage_cleanup_kills_silent_external_child_before_join() {
        let temp = unique_temp_dir("mixed-stage-silent-child");
        let marker = temp.join("started");
        let sleeper = temp.join("sleeper");
        write_executable(
            &sleeper,
            &format!("printf started >'{}'\nexec sleep 30", marker.display()),
        );
        let graph = parse_line(&format!("{{ '{}'; }}", sleeper.display())).unwrap();
        let shell_stage = graph.list.items[0].pipeline.commands[0].clone();
        let state = ShellState::from_current_process();
        let (stdout_reader, stdout_writer) = io::pipe().unwrap();
        let shell = spawn_shell_pipeline_stage(
            shell_stage,
            state.clone(),
            None,
            stdout_writer,
            OutputMode::Raw,
            false,
        );
        let interrupt = Arc::clone(&shell.interrupt);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !marker.exists() {
            assert!(
                Instant::now() < deadline,
                "silent external child did not start"
            );
            std::thread::sleep(Duration::from_millis(2));
        }

        let mut stages = vec![RunningStreamingStage::Shell(shell)];
        let cleanup_started = Instant::now();
        terminate_running_streaming_stages(&mut stages, &state);

        assert!(cleanup_started.elapsed() < Duration::from_secs(2));
        assert!(stages.is_empty());
        assert!(!interrupt.load(Ordering::Acquire));
        drop(stdout_reader);
        let _ = std::fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[test]
    fn mixed_stage_cleanup_does_not_orphan_external_descendants() {
        let temp = unique_temp_dir("mixed-stage-descendant");
        let marker = temp.join("descendant-pid");
        let wrapper = temp.join("wrapper");
        write_executable(
            &wrapper,
            &format!(
                "sleep 30 &\nprintf '%s' \"$!\" >'{}'\nwait",
                marker.display()
            ),
        );
        let graph = parse_line(&format!("{{ '{}'; }}", wrapper.display())).unwrap();
        let shell_stage = graph.list.items[0].pipeline.commands[0].clone();
        let state = ShellState::from_current_process();
        let (stdout_reader, stdout_writer) = io::pipe().unwrap();
        let shell = spawn_shell_pipeline_stage(
            shell_stage,
            state.clone(),
            None,
            stdout_writer,
            OutputMode::Raw,
            false,
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let descendant = loop {
            if let Ok(pid) = std::fs::read_to_string(&marker) {
                if let Ok(pid) = pid.parse::<i32>() {
                    if let Some(pid) = rustix::process::Pid::from_raw(pid) {
                        break pid;
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "shell-stage descendant did not start"
            );
            std::thread::sleep(Duration::from_millis(2));
        };

        let mut stages = vec![RunningStreamingStage::Shell(shell)];
        terminate_running_streaming_stages(&mut stages, &state);

        let reap_deadline = Instant::now() + Duration::from_secs(2);
        let survived = loop {
            match rustix::process::test_kill_process(descendant) {
                Err(rustix::io::Errno::SRCH) => break false,
                Ok(()) | Err(rustix::io::Errno::PERM) if Instant::now() < reap_deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Ok(()) | Err(rustix::io::Errno::PERM) => break true,
                Err(error) => panic!("probe shell-stage descendant: {error}"),
            }
        };
        if survived {
            let _ = rustix::process::kill_process(descendant, rustix::process::Signal::KILL);
        }
        drop(stdout_reader);
        let _ = std::fs::remove_dir_all(temp);
        assert!(!survived, "cancelled shell stage orphaned descendant");
    }

    #[cfg(unix)]
    #[test]
    fn isolated_stage_child_reports_sigint_status_before_escalation() {
        let temp = unique_temp_dir("stage-child-sigint");
        let marker = temp.join("started");
        let state = ShellState::from_current_process();
        let status = with_cancellable_shell_stage(|| {
            let mut command = Command::new("/bin/sh");
            // Make the marker come from the foreground child. If the trapped
            // shell writes it itself, macOS Bash 3.2 can receive SIGINT before
            // entering its wait and defer the trap until a later sleep exits.
            command.arg("-c").arg(format!(
                "trap 'exit 130' INT; /bin/sh -c 'printf started >\"{}\"; exec sleep 30'",
                marker.display()
            ));
            configure_cancellable_shell_stage_child(&mut command);
            let mut child = command.spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !marker.exists() {
                assert!(Instant::now() < deadline, "isolated child did not start");
                std::thread::sleep(Duration::from_millis(2));
            }
            state.interrupt_flag().store(true, Ordering::Release);
            wait_child_interruptibly(&mut child, &state).unwrap()
        });

        state.clear_interrupt();
        assert_eq!(exit_status_code(status), 130);
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn read_capped_bounds_large_input_keeping_head_and_tail() {
        // SHIP_READINESS_PLAN P0-9: capture is bounded, keeping head + tail with a
        // marker rather than the whole (potentially huge) stream.
        let total = CAPTURE_HEAD + CAPTURE_TAIL + 3 * 1024 * 1024;
        let mut data = vec![b'x'; total];
        data[..5].copy_from_slice(b"START");
        let n = data.len();
        data[n - 3..].copy_from_slice(b"END");
        let out = read_capped(std::io::Cursor::new(data)).unwrap();
        assert!(out.len() < total, "not truncated: {} >= {total}", out.len());
        assert!(
            out.len() <= CAPTURE_HEAD + CAPTURE_TAIL + 512,
            "over cap: {}",
            out.len()
        );
        assert!(out.starts_with(b"START"), "head lost");
        assert!(out.ends_with(b"END"), "tail lost");
        assert!(
            String::from_utf8_lossy(&out).contains("bytes of output elided"),
            "missing truncation marker"
        );
    }

    #[test]
    fn bounded_capture_tees_exact_binary_bytes_to_private_disk() {
        let dir = std::env::temp_dir().join(format!(
            "agsh-capped-trace-{}-{}",
            std::process::id(),
            agsh_core::CommandId::new()
        ));
        let mut state = ShellState::from_current_process();
        state.export_var("AGSH_TRACE_DIR", dir.display().to_string());

        let total = CAPTURE_HEAD + CAPTURE_TAIL + 2 * 1024 * 1024;
        let mut data = vec![0u8; total];
        data[0..4].copy_from_slice(&[0xff, 0x80, 0x00, b'A']);
        data[total - 4..].copy_from_slice(&[b'Z', 0x00, 0xfe, 0x81]);
        let spool = state.create_trace_spool("out").unwrap();
        let captured = read_capped_to_spool(std::io::Cursor::new(&data), spool).unwrap();
        let exact = captured.exact.as_ref().expect("exact trace spool");

        assert!(captured.preview.len() < data.len());
        assert!(captured.preview.len() <= CAPTURE_HEAD + CAPTURE_TAIL + 512);
        assert_eq!(std::fs::read(exact.path()).unwrap(), data);
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(exact.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let exact_path = exact.path().to_path_buf();
        drop(captured);
        assert!(!exact_path.exists(), "temporary exact spool leaked");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn raw_trace_quota_drains_huge_producer_and_preserves_exit_status() {
        let dir = unique_temp_dir("trace-quota");
        let mut state = ShellState::from_current_process();
        state.export_var("AGSH_TRACE_DIR", dir.display().to_string());
        let mut config = agsh_output::CompactorConfig::default();
        config.storage.max_raw_per_command = "4kb".to_string();
        state.replace_output_config_for_test(config);
        let graph = parse_line(
            "sh -c 'i=0; while [ \"$i\" -lt 10000 ]; do printf 0123456789abcdef; i=$((i+1)); done; exit 37'",
        )
        .unwrap();

        let outcome = Executor::new()
            .run_graph(
                &graph,
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Semantic,
                    allow_process_replacement: false,
                },
            )
            .unwrap();

        assert_eq!(
            outcome.exit_code, 37,
            "capture must not change child status"
        );
        let observation = outcome.observation.as_ref().unwrap();
        assert!(observation.display.contains("\"complete\": false"));
        assert!(observation.display.contains("\"stdout\": \"truncated\""));
        let raw = observation.raw.as_ref().unwrap();
        let stored = [&raw.stdout, &raw.stderr]
            .iter()
            .map(|path| std::fs::metadata(path).unwrap().len())
            .sum::<u64>();
        assert!(
            stored <= 4096,
            "persisted {stored} bytes past the 4 KiB cap"
        );
        assert!(state
            .resolve_trace(&format!("trace://{}/stdout", graph.id))
            .is_none());
        let resolved = state
            .resolve_trace_with_status(&format!("trace://{}/stdout", graph.id))
            .unwrap();
        assert_eq!(resolved.status, agsh_output::RawTraceStatus::Truncated);
        assert_eq!(resolved.bytes.len(), 4096);
        assert!(std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .all(|entry| !entry.file_name().to_string_lossy().starts_with(".capture-")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_drain_helper_waits_for_real_eof() {
        use std::os::unix::net::UnixStream;

        let dir = unique_temp_dir("retained-spool-status");
        let mut state = ShellState::from_current_process();
        state.export_var("AGSH_TRACE_DIR", dir.display().to_string());
        let spool = state.create_trace_spool("out").unwrap();
        let incomplete = spool.incomplete_marker();
        let (reader, mut retained_writer) = UnixStream::pair().unwrap();
        retained_writer.write_all(b"captured").unwrap();
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(retained_writer);
        });
        let direct_child_exited = Arc::new(AtomicBool::new(true));
        let preview_incomplete = Arc::new(AtomicBool::new(false));
        let reader = DirectChildCaptureReader::new(
            Box::new(reader),
            direct_child_exited,
            Some(incomplete),
            preview_incomplete,
        )
        .unwrap();

        let captured = read_capped_to_spool(reader, spool).unwrap();
        closer.join().unwrap();

        assert_eq!(captured.preview, b"captured");
        assert!(captured.exact.unwrap().is_complete());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn ambiguous_drain_ack_is_killed_before_fallback_reader_resumes() {
        use std::os::fd::AsFd;

        let root = unique_temp_dir("capture-drain-ambiguous-ack");
        let cwd_file = root.join("worker.cwd");
        let timeout_helper = root.join("timeout-helper");
        write_executable(&timeout_helper, "/bin/sleep 30");
        let wrong_ack_helper = root.join("wrong-ack-helper");
        write_executable(
            &wrong_ack_helper,
            &format!("pwd > '{}'; printf X; /bin/sleep 30", cwd_file.display()),
        );

        for (helper, timeout) in [
            (&timeout_helper, Duration::from_millis(50)),
            (&wrong_ack_helper, CAPTURE_DRAIN_ACK_TIMEOUT),
        ] {
            let (mut fallback_reader, mut retained_writer) = io::pipe().unwrap();
            let worker_reader = fallback_reader.as_fd().try_clone_to_owned().unwrap();
            let result = launch_capture_drain_worker(helper, worker_reader, timeout);
            assert_eq!(result, CaptureDrainHandoff::Ambiguous);

            retained_writer.write_all(b"still-owned-locally").unwrap();
            drop(retained_writer);
            let mut captured = Vec::new();
            fallback_reader.read_to_end(&mut captured).unwrap();
            assert_eq!(captured, b"still-owned-locally");
        }
        assert_eq!(std::fs::read_to_string(&cwd_file).unwrap().trim(), "/");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn fd_routing_moves_preview_completeness_with_each_stream() {
        let mut swapped = CommandOutcome::captured(0, b"out".to_vec(), b"err".to_vec());
        swapped.stdout_preview_complete = false;
        route_exact_stream_destinations(
            &mut swapped,
            Some(OutputStream::Stderr),
            Some(OutputStream::Stdout),
        );
        assert!(swapped.stdout_preview_complete);
        assert!(!swapped.stderr_preview_complete);

        let mut merged = CommandOutcome::captured(0, b"out".to_vec(), b"err".to_vec());
        merged.stderr_preview_complete = false;
        route_exact_stream_destinations(
            &mut merged,
            Some(OutputStream::Stdout),
            Some(OutputStream::Stdout),
        );
        assert!(!merged.stdout_preview_complete);
        assert!(merged.stderr_preview_complete);

        let mut moved = CommandOutcome::captured(0, b"out".to_vec(), b"err".to_vec());
        moved.stdout_preview_complete = false;
        route_exact_stream_destinations(&mut moved, Some(OutputStream::Stderr), None);
        assert!(moved.stdout_preview_complete);
        assert!(!moved.stderr_preview_complete);
    }

    #[cfg(unix)]
    #[test]
    fn capture_without_helper_waits_for_descendant_inherited_pipes() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let root = unique_temp_dir("capture-descendant-fds");
        let pid_file = root.join("descendant.pid");
        let source = format!(
            "sh -c 'sleep 30 & echo $! > {}; printf ready; exit 23'",
            pid_file.display()
        );
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            let mut state = ShellState::from_current_process();
            let graph = parse_line(&source).unwrap();
            let result = Executor::new().run_graph(
                &graph,
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            );
            let _ = send.send(result);
        });

        let pid_deadline = Instant::now() + Duration::from_secs(2);
        let descendant_pid = loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = pid.trim().parse::<u32>() {
                    break pid;
                }
            }
            assert!(
                Instant::now() < pid_deadline,
                "descendant pid was not recorded"
            );
            std::thread::sleep(Duration::from_millis(10));
        };

        let completed = receive.recv_timeout(Duration::from_millis(500));
        let _ = std::process::Command::new("/bin/kill")
            .arg(descendant_pid.to_string())
            .status();
        assert!(
            completed.is_err(),
            "library capture unexpectedly detached without a helper"
        );
        let result = receive.recv_timeout(Duration::from_secs(2)).unwrap();
        let outcome = result.unwrap();
        assert_eq!(outcome.exit_code, 23);
        assert_eq!(outcome.stdout, b"ready");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pipeline_capture_without_helper_waits_for_descendant_inherited_pipes() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let root = unique_temp_dir("pipeline-descendant-fds");
        let pid_file = root.join("descendant.pid");
        let source = format!(
            "sh -c 'printf x' | sh -c 'sleep 30 & echo $! > {}; cat; printf ready; exit 23'",
            pid_file.display()
        );
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            let mut state = ShellState::from_current_process();
            let graph = parse_line(&source).unwrap();
            let result = Executor::new().run_graph(
                &graph,
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            );
            let _ = send.send(result);
        });

        let pid_deadline = Instant::now() + Duration::from_secs(2);
        let descendant_pid = loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = pid.trim().parse::<u32>() {
                    break pid;
                }
            }
            assert!(
                Instant::now() < pid_deadline,
                "pipeline descendant pid was not recorded"
            );
            std::thread::sleep(Duration::from_millis(10));
        };

        let completed = receive.recv_timeout(Duration::from_millis(500));
        let _ = std::process::Command::new("/bin/kill")
            .arg(descendant_pid.to_string())
            .status();
        assert!(
            completed.is_err(),
            "library capture unexpectedly detached without a helper"
        );
        let result = receive.recv_timeout(Duration::from_secs(2)).unwrap();
        let outcome = result.unwrap();
        assert_eq!(outcome.exit_code, 23);
        assert_eq!(outcome.stdout, b"xready");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pipeline_capture_without_helper_waits_for_descendant_stage_stderr() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let root = unique_temp_dir("pipeline-descendant-stderr");
        let pid_file = root.join("descendant.pid");
        let source = format!(
            "sh -c 'sleep 30 >&2 & echo $! > {}' | true",
            pid_file.display()
        );
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            let mut state = ShellState::from_current_process();
            let graph = parse_line(&source).unwrap();
            let result = Executor::new().run_graph(
                &graph,
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            );
            let _ = send.send(result);
        });

        let pid_deadline = Instant::now() + Duration::from_secs(2);
        let descendant_pid = loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = pid.trim().parse::<u32>() {
                    break pid;
                }
            }
            assert!(
                Instant::now() < pid_deadline,
                "pipeline descendant pid was not recorded"
            );
            std::thread::sleep(Duration::from_millis(10));
        };

        let completed = receive.recv_timeout(Duration::from_millis(500));
        let _ = std::process::Command::new("/bin/kill")
            .arg(descendant_pid.to_string())
            .status();
        assert!(
            completed.is_err(),
            "library capture unexpectedly detached without a helper"
        );
        let result = receive.recv_timeout(Duration::from_secs(2)).unwrap();
        let outcome = result.unwrap();
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
        assert!(outcome.stderr.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disabled_raw_storage_drains_without_creating_trace_files() {
        let root = unique_temp_dir("trace-disabled");
        let dir = root.join("traces");
        let mut state = ShellState::from_current_process();
        state.export_var("AGSH_TRACE_DIR", dir.display().to_string());
        let mut config = agsh_output::CompactorConfig::default();
        config.storage.store_raw = false;
        state.replace_output_config_for_test(config);
        let graph = parse_line(
            "sh -c 'i=0; while [ \"$i\" -lt 2000 ]; do printf secret; i=$((i+1)); done; exit 19'",
        )
        .unwrap();

        let outcome = Executor::new()
            .run_graph(
                &graph,
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Compact,
                    allow_process_replacement: false,
                },
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 19);
        let observation = outcome.observation.as_ref().unwrap();
        assert!(observation
            .display
            .contains("raw_trace: unavailable (raw storage disabled)"));
        let raw = observation.raw.as_ref().unwrap();
        assert_eq!(raw.stdout_status, agsh_output::RawTraceStatus::Disabled);
        assert_eq!(raw.stderr_status, agsh_output::RawTraceStatus::Disabled);
        assert!(!dir.exists(), "disabled raw storage created {dir:?}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unavailable_trace_storage_does_not_change_command_status() {
        for (source, expected_status, expected_stdout) in [
            ("true", 0, b"".as_slice()),
            ("printf retained; exit 23", 23, b"retained"),
            ("sh -c 'printf external; exit 17'", 17, b"external"),
        ] {
            let mut state = ShellState::from_current_process();
            state.export_var("AGSH_TRACE_DIR", "/dev/null");
            let graph = parse_line(source).unwrap();
            let outcome = Executor::new()
                .run_graph(
                    &graph,
                    &mut state,
                    &ExecutionOptions {
                        output_mode: OutputMode::Compact,
                        allow_process_replacement: false,
                    },
                )
                .unwrap();

            assert_eq!(outcome.exit_code, expected_status, "source={source:?}");
            assert_eq!(outcome.stdout, expected_stdout, "source={source:?}");
            let observation = outcome.observation.as_ref().unwrap();
            assert!(
                observation
                    .display
                    .contains("raw_trace: unavailable (stdout=unavailable, stderr=unavailable)"),
                "source={source:?}, display={:?}",
                observation.display
            );
            let raw = observation.raw.as_ref().unwrap();
            assert_eq!(raw.stdout_status, agsh_output::RawTraceStatus::Unavailable);
            assert_eq!(raw.stderr_status, agsh_output::RawTraceStatus::Unavailable);
        }
    }

    #[test]
    fn failed_capture_spool_is_not_reported_complete_after_later_persistence() {
        let root = unique_temp_dir("trace-preview-completeness");
        let trace_dir = root.join("valid-traces");
        let source = format!(
            "export AGSH_TRACE_DIR=/dev/null; head -c 3145728 /dev/zero; export AGSH_TRACE_DIR={}",
            trace_dir.display()
        );
        let mut state = ShellState::from_current_process();
        let graph = parse_line(&source).unwrap();

        let outcome = Executor::new()
            .run_graph(
                &graph,
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Semantic,
                    allow_process_replacement: false,
                },
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        let observation = outcome.observation.as_ref().unwrap();
        let raw = observation.raw.as_ref().unwrap();
        assert_eq!(raw.stdout_status, agsh_output::RawTraceStatus::Truncated);
        assert_eq!(raw.stderr_status, agsh_output::RawTraceStatus::Complete);
        assert!(
            observation.display.contains("\"complete\": false")
                && observation.display.contains("\"stdout\": \"truncated\""),
            "display={:?}",
            observation.display
        );
        let stored = std::fs::metadata(&raw.stdout).unwrap().len();
        assert!(stored < 3_145_728, "bounded preview was presented as exact");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compound_output_aggregation_fails_before_exceeding_memory_limit() {
        let mut aggregate = CommandOutcome::captured(0, b"1234".to_vec(), Vec::new());
        let mut next = CommandOutcome::captured(0, Vec::new(), b"56789".to_vec());

        let error = aggregate
            .append_streams_with_limit(&mut next, true, true, 8)
            .unwrap_err();

        assert!(error.to_string().contains("8-byte aggregate memory limit"));
        assert_eq!(aggregate.stdout, b"1234");
        assert!(aggregate.stderr.is_empty());
        assert!(next.stdout.is_empty());
        assert_eq!(next.stderr, b"56789");
    }

    #[cfg(unix)]
    #[test]
    fn process_substitution_temp_file_is_private() {
        let mut state = ShellState::from_current_process();
        let path = PathBuf::from(process_substitution_path("printf secret", &mut state).unwrap());

        let metadata = std::fs::metadata(&path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        let contents = std::fs::read(&path).unwrap();
        for registered in state.take_proc_sub_temps() {
            let _ = std::fs::remove_file(registered);
        }

        assert_eq!(mode, 0o600);
        assert_eq!(contents, b"secret");
    }

    #[cfg(unix)]
    #[test]
    fn process_substitution_rejects_non_utf8_temp_directory_before_creation() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let state = ShellState::from_current_process();
        let temp_dir = PathBuf::from(OsString::from_vec(
            b"/tmp/agsh-procsub-invalid-\xff".to_vec(),
        ));

        let error = create_process_substitution_temp_in(&temp_dir, &state).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("not valid UTF-8"));
        assert!(!temp_dir.exists());
    }

    #[test]
    fn process_substitution_copies_spooled_exact_stdout() {
        let trace_dir = unique_temp_dir("proc-sub-spool");
        let mut state = ShellState::from_current_process();
        state.export_var("AGSH_TRACE_DIR", trace_dir.display().to_string());

        let data = vec![0xa5; CAPTURE_HEAD + CAPTURE_TAIL + 1024 * 1024];
        let mut spool = state.create_trace_spool("out").unwrap();
        spool.write_all(&data).unwrap();
        let exact = spool.finish().unwrap();
        let exact_path = exact.path().to_path_buf();
        let mut outcome = CommandOutcome::captured_with_exact(
            0,
            b"bounded preview, not the process-substitution payload".to_vec(),
            Vec::new(),
            Some(exact),
            None,
        );
        let (path, mut destination) = create_process_substitution_temp(&state).unwrap();

        write_process_substitution_stdout(&mut destination, &mut outcome).unwrap();
        drop(destination);

        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert!(
            !exact_path.exists(),
            "consumed capture spool was not removed"
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir_all(trace_dir).unwrap();
    }

    #[test]
    fn process_substitution_rejects_quota_truncated_stdout() {
        let trace_dir = unique_temp_dir("proc-sub-quota");
        let mut state = ShellState::from_current_process();
        state.export_var("AGSH_TRACE_DIR", trace_dir.display().to_string());
        let mut config = agsh_output::CompactorConfig::default();
        config.storage.max_raw_per_command = "4kb".to_string();
        state.replace_output_config_for_test(config);

        let outcome = Executor::new()
            .run_graph(
                &parse_line("cat <(head -c 8192 /dev/zero)").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(outcome.exit_code, 1);
        let error = String::from_utf8_lossy(&outcome.stderr);
        assert!(
            error.contains("process-substitution stdout capture is incomplete"),
            "error={error}"
        );
        assert!(state.take_proc_sub_temps().is_empty());
        assert!(std::fs::read_dir(&trace_dir)
            .unwrap()
            .flatten()
            .all(|entry| !entry.file_name().to_string_lossy().starts_with(".capture-")));

        std::fs::remove_dir_all(trace_dir).unwrap();
    }

    #[test]
    fn process_substitution_reports_quota_truncated_stderr() {
        let trace_dir = unique_temp_dir("proc-sub-stderr-quota");
        let mut state = ShellState::from_current_process();
        state.export_var("AGSH_TRACE_DIR", trace_dir.display().to_string());
        let mut config = agsh_output::CompactorConfig::default();
        config.storage.max_raw_per_command = "4kb".to_string();
        state.replace_output_config_for_test(config);

        let outcome = Executor::new()
            .run_graph(
                &parse_line("cat <(head -c 8192 /dev/zero >&2)").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(outcome.exit_code, 1);
        let error = String::from_utf8_lossy(&outcome.stderr);
        assert!(
            error.contains("substitution stderr capture is incomplete"),
            "error={error}"
        );
        assert!(state.take_proc_sub_temps().is_empty());

        std::fs::remove_dir_all(trace_dir).unwrap();
    }

    #[test]
    fn process_substitution_uses_bounded_memory_when_trace_storage_is_disabled() {
        let mut state = ShellState::from_current_process();
        let mut config = agsh_output::CompactorConfig::default();
        config.storage.store_raw = false;
        state.replace_output_config_for_test(config);

        let path = PathBuf::from(process_substitution_path("printf safe", &mut state).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"safe");
        for registered in state.take_proc_sub_temps() {
            let _ = std::fs::remove_file(registered);
        }
    }

    #[test]
    fn shell_input_file_redirection_is_opened_as_a_stream() {
        let dir = unique_temp_dir("streamed-shell-input");
        let path = dir.join("large-sparse-input");
        let mut source = File::create(&path).unwrap();
        source.write_all(b"first line\n").unwrap();
        source.set_len(8 * 1024 * 1024 * 1024).unwrap();
        drop(source);

        let redirected = redirected_stdin_from_expanded_redirections(&[ExpandedRedirection {
            fd: 0,
            mode: RedirectionMode::Read,
            target: ExpandedRedirectionTarget::Path(path.display().to_string()),
        }])
        .unwrap();

        let Some(RedirectedShellStdin::File(mut source)) = redirected else {
            panic!("file input redirection was buffered instead of streamed");
        };
        let mut first_line = [0u8; 11];
        source.read_exact(&mut first_line).unwrap();
        assert_eq!(&first_line, b"first line\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn heredoc_on_nonstandard_fd_does_not_replace_stdin() {
        let redirected = redirected_stdin_from_expanded_redirections(&[ExpandedRedirection {
            fd: 3,
            mode: RedirectionMode::HereDoc,
            target: ExpandedRedirectionTarget::Bytes(b"not stdin\n".to_vec()),
        }])
        .unwrap();

        assert!(redirected.is_none());
    }

    #[test]
    fn null_command_reports_missing_input_redirection_and_list_continues() {
        let dir = unique_temp_dir("null-input-redirection");
        let mut state = ShellState::from_current_process();
        state.set_cwd(dir.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("< missing-input; echo status=$?").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"status=1\n");
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("missing-input"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn process_substitution_does_not_follow_preexisting_symlink() {
        use std::os::unix::fs::symlink;

        let temp = unique_temp_dir("proc-sub-symlink");
        let victim = temp.join("victim");
        std::fs::write(&victim, b"do not overwrite").unwrap();

        let mut state = ShellState::from_current_process();
        let predictor = state.clone();
        let planted = std::env::temp_dir().join(format!(
            "agsh-procsub-{}-{}",
            std::process::id(),
            predictor.next_random()
        ));
        assert!(std::fs::symlink_metadata(&planted).is_err());
        symlink(&victim, &planted).unwrap();

        let returned = PathBuf::from(process_substitution_path("printf safe", &mut state).unwrap());
        let victim_contents = std::fs::read(&victim).unwrap();
        let returned_contents = std::fs::read(&returned).unwrap();
        for registered in state.take_proc_sub_temps() {
            let _ = std::fs::remove_file(registered);
        }
        let _ = std::fs::remove_file(&planted);
        std::fs::remove_dir_all(temp).unwrap();

        assert_ne!(returned, planted);
        assert_eq!(victim_contents, b"do not overwrite");
        assert_eq!(returned_contents, b"safe");
    }

    #[test]
    fn process_substitution_temps_are_removed_after_graph_execution() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();
        let outcome = executor
            .run_graph(
                &parse_line("printf %s <(printf payload)").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let path = PathBuf::from(String::from_utf8(outcome.stdout).unwrap());
        let path_was_removed = !path.exists();
        let pending = state.take_proc_sub_temps();
        for registered in &pending {
            let _ = std::fs::remove_file(registered);
        }

        assert!(path_was_removed, "temporary path remained at {path:?}");
        assert!(pending.is_empty(), "temporary path remained registered");
    }

    #[test]
    fn nested_execution_keeps_enclosing_process_substitution_until_consumed() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();
        let outcome = executor
            .run_graph(
                &parse_line(r#"consume() { cat "$1"; }; consume <(printf payload)"#).unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    ..ExecutionOptions::default()
                },
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"payload");
        assert!(state.take_proc_sub_temps().is_empty());
    }

    #[test]
    fn brace_range_expansion() {
        assert_eq!(
            expand_brace_range("1..5").unwrap(),
            ["1", "2", "3", "4", "5"]
        );
        assert_eq!(
            expand_brace_range("5..1").unwrap(),
            ["5", "4", "3", "2", "1"]
        );
        assert_eq!(
            expand_brace_range("0..10..2").unwrap(),
            ["0", "2", "4", "6", "8", "10"]
        );
        assert_eq!(
            expand_brace_range("01..10").unwrap(),
            ["01", "02", "03", "04", "05", "06", "07", "08", "09", "10"]
        );
        assert_eq!(
            expand_brace_range("a..e").unwrap(),
            ["a", "b", "c", "d", "e"]
        );
        assert_eq!(expand_brace_range("a..e..2").unwrap(), ["a", "c", "e"]);
        assert_eq!(
            expand_brace_range("-2..2").unwrap(),
            ["-2", "-1", "0", "1", "2"]
        );
        assert_eq!(expand_brace_range("5..5").unwrap(), ["5"]);
        assert!(expand_brace_range("a").is_none());
        assert!(expand_brace_range("not..a..b..range").is_none());
    }

    #[test]
    fn assignment_without_command_sets_shell_var() {
        let graph = parse_line("FOO=bar").unwrap();
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();
        executor
            .run_graph(&graph, &mut state, &ExecutionOptions::default())
            .unwrap();
        assert_eq!(state.lookup("FOO"), Some("bar"));
    }

    #[test]
    fn readonly_assignment_diagnostic_honors_stderr_redirection() {
        let temp = unique_temp_dir("readonly-redirection");
        let error_file = temp.join("readonly.err");
        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();
        executor
            .run_graph(
                &parse_line("readonly LOCKED=value").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("LOCKED=changed 2>readonly.err").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 1);
        assert!(outcome.stderr.is_empty());
        assert_eq!(state.lookup("LOCKED"), Some("value"));
        assert!(std::fs::read_to_string(error_file)
            .unwrap()
            .contains("LOCKED: readonly variable"));
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn allexport_exports_subsequent_shell_assignments() {
        let mut state = ShellState::from_current_process();
        state.unset("AGSH_ALL_EXPORT");
        state.unset("AGSH_STILL_LOCAL");
        let mut executor = Executor::new();
        let capture_options = ExecutionOptions {
            output_mode: OutputMode::Clean,
            ..ExecutionOptions::default()
        };

        let local = executor
            .run_graph(
                &parse_line(
                    r#"AGSH_STILL_LOCAL=one; sh -c 'printf %s "${AGSH_STILL_LOCAL-unset}"'"#,
                )
                .unwrap(),
                &mut state,
                &capture_options,
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&local.stdout), "unset");
        assert_eq!(state.lookup("AGSH_STILL_LOCAL"), Some("one"));

        executor
            .run_graph(
                &parse_line("set -a").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.allexport());

        let exported = executor
            .run_graph(
                &parse_line(r#"AGSH_ALL_EXPORT=two; sh -c 'printf %s "$AGSH_ALL_EXPORT"'"#)
                    .unwrap(),
                &mut state,
                &capture_options,
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&exported.stdout), "two");

        executor
            .run_graph(
                &parse_line("set +a").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.allexport());

        let still_local = executor
            .run_graph(
                &parse_line(
                    r#"AGSH_STILL_LOCAL=three; sh -c 'printf %s "${AGSH_STILL_LOCAL-unset}"'"#,
                )
                .unwrap(),
                &mut state,
                &capture_options,
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&still_local.stdout), "unset");
        assert_eq!(state.lookup("AGSH_STILL_LOCAL"), Some("three"));
    }

    #[test]
    fn allexport_does_not_persist_temporary_command_assignments() {
        let mut state = ShellState::from_current_process();
        state.unset("AGSH_TEMP_EXPORT");
        let mut executor = Executor::new();
        let capture_options = ExecutionOptions {
            output_mode: OutputMode::Clean,
            ..ExecutionOptions::default()
        };

        executor
            .run_graph(
                &parse_line("set -a").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let temporary = executor
            .run_graph(
                &parse_line(
                    r#"AGSH_TEMP_EXPORT=tmp sh -c 'printf %s "$AGSH_TEMP_EXPORT"'; sh -c 'printf %s "${AGSH_TEMP_EXPORT-unset}"'"#,
                )
                .unwrap(),
                &mut state,
                &capture_options,
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&temporary.stdout), "tmpunset");
        assert_eq!(state.lookup("AGSH_TEMP_EXPORT"), None);
    }

    #[test]
    fn command_v_reports_builtin_and_external() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let builtin = executor
            .run_graph(
                &parse_line("command -v echo").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&builtin.stdout), "echo\n");

        let external = executor
            .run_graph(
                &parse_line("command -v sh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(String::from_utf8_lossy(&external.stdout).contains("/sh"));
    }

    #[test]
    fn command_v_verbose_reports_resolution_details() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("alias hi='echo hello'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let alias = executor
            .run_graph(
                &parse_line("command -V hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&alias.stdout),
            "hi is aliased to echo hello\n"
        );

        let builtin = executor
            .run_graph(
                &parse_line("command -V echo").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&builtin.stdout),
            "echo is an agsh builtin\n"
        );
    }

    #[test]
    fn command_double_dash_executes_wrapper_and_bad_options_fail() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let wrapped = executor
            .run_graph(
                &parse_line("command -- echo ok").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&wrapped.stdout), "ok\n");

        let err = executor
            .run_graph(
                &parse_line("command -Z echo").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap_err();
        assert!(err.message.contains("unsupported option"));
    }

    #[test]
    fn command_p_uses_default_path_for_execution_and_introspection() {
        let mut state = ShellState::from_current_process();
        state.export_var("PATH", "");
        let mut executor = Executor::new();

        let executed = executor
            .run_graph(
                &parse_line("command -p sh -c 'printf ok'").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(executed.stdout, b"ok");

        let described = executor
            .run_graph(
                &parse_line("command -p -v sh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(String::from_utf8_lossy(&described.stdout).contains("/sh\n"));

        let verbose = executor
            .run_graph(
                &parse_line("command -p -V sh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(String::from_utf8_lossy(&verbose.stdout).contains("sh is "));
    }

    #[test]
    fn wrappers_force_external_and_builtin_lookup() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let external = executor
            .run_graph(
                &parse_line("external echo ok").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&external.stdout), "ok\n");

        let builtin = executor
            .run_graph(
                &parse_line("builtin pwd").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(String::from_utf8_lossy(&builtin.stdout).contains('/'));
    }

    #[test]
    fn job_control_builtins_exist_without_jobs() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let jobs = executor
            .run_graph(
                &parse_line("jobs").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(jobs.exit_code, 0);
        assert!(jobs.stdout.is_empty());
        assert!(jobs.stderr.is_empty());

        let wait_empty = executor
            .run_graph(
                &parse_line("wait").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(wait_empty.exit_code, 0);

        let fg = executor
            .run_graph(
                &parse_line("fg").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(fg.exit_code, 1);
        assert!(String::from_utf8_lossy(&fg.stderr).contains("no such job"));

        let bg = executor
            .run_graph(
                &parse_line("bg %1").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(bg.exit_code, 1);
        assert!(String::from_utf8_lossy(&bg.stderr).contains("no such job"));

        let wait_job = executor
            .run_graph(
                &parse_line("wait 12345").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(wait_job.exit_code, 127);
        assert!(String::from_utf8_lossy(&wait_job.stderr).contains("no such job"));
    }

    #[test]
    fn job_control_builtins_report_resolution_consistently() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let command_v = executor
            .run_graph(
                &parse_line("command -v jobs fg bg wait").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&command_v.stdout),
            "jobs\nfg\nbg\nwait\n"
        );

        let type_out = executor
            .run_graph(
                &parse_line("type jobs fg bg wait").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&type_out.stdout),
            "jobs is an agsh builtin\nfg is an agsh builtin\nbg is an agsh builtin\nwait is an agsh builtin\n"
        );

        let builtin_jobs = executor
            .run_graph(
                &parse_line("builtin jobs").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(builtin_jobs.exit_code, 0);
    }

    #[test]
    fn loop_control_builtins_report_resolution_consistently() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let command_v = executor
            .run_graph(
                &parse_line("command -v break continue").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&command_v.stdout),
            "break\ncontinue\n"
        );

        let type_out = executor
            .run_graph(
                &parse_line("type break continue").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&type_out.stdout),
            "break is an agsh builtin\ncontinue is an agsh builtin\n"
        );
    }

    #[test]
    fn kill_builtin_delegates_to_platform_kill_and_reserves_job_specs() {
        let mut state = ShellState::from_current_process();
        state.export_var("PATH", "");
        let mut executor = Executor::new();

        let listed = executor
            .run_graph(
                &parse_line("kill -l").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(listed.exit_code, 0);
        let signal_list = format!(
            "{}{}",
            String::from_utf8_lossy(&listed.stdout),
            String::from_utf8_lossy(&listed.stderr)
        );
        assert!(signal_list.contains("TERM"));

        let job_spec = executor
            .run_graph(
                &parse_line("kill %1").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(job_spec.exit_code, 1);
        assert!(String::from_utf8_lossy(&job_spec.stderr).contains("no such job"));
    }

    #[test]
    fn kill_builtin_reports_resolution_consistently() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let command_v = executor
            .run_graph(
                &parse_line("command -v kill").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&command_v.stdout), "kill\n");

        let type_out = executor
            .run_graph(
                &parse_line("type kill").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&type_out.stdout),
            "kill is an agsh builtin\n"
        );

        let builtin_out = executor
            .run_graph(
                &parse_line("builtin kill -l").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(builtin_out.exit_code, 0);
    }

    #[test]
    fn exec_without_command_applies_assignments_without_replacing_process() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("FOO=bar exec").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("FOO"), Some("bar"));
    }

    #[test]
    fn exec_command_is_disabled_without_process_replacement_permission() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("exec sh -c 'printf should-not-run'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 126);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("disabled"));
        assert!(outcome.stdout.is_empty());
    }

    #[test]
    fn exec_builtin_reports_resolution_consistently() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let command_v = executor
            .run_graph(
                &parse_line("command -v exec").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&command_v.stdout), "exec\n");

        let type_out = executor
            .run_graph(
                &parse_line("type exec").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&type_out.stdout),
            "exec is an agsh builtin\n"
        );

        let builtin_out = executor
            .run_graph(
                &parse_line("builtin exec").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(builtin_out.exit_code, 0);
    }

    #[test]
    fn ulimit_and_umask_support_safe_queries() {
        let mut state = ShellState::from_current_process();
        state.export_var("PATH", "");
        let mut executor = Executor::new();

        let ulimit = executor
            .run_graph(
                &parse_line("ulimit").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(ulimit.exit_code, 0);
        assert!(!ulimit.stdout.is_empty());

        let ulimit_all = executor
            .run_graph(
                &parse_line("ulimit -a").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(ulimit_all.exit_code, 0);
        assert!(
            String::from_utf8_lossy(&ulimit_all.stdout).contains("open files")
                || String::from_utf8_lossy(&ulimit_all.stdout).contains("file")
        );

        let umask = executor
            .run_graph(
                &parse_line("umask").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(umask.exit_code, 0);
        let mask = String::from_utf8_lossy(&umask.stdout);
        assert!(mask.trim().chars().all(|ch| ('0'..='7').contains(&ch)));

        let umask_symbolic = executor
            .run_graph(
                &parse_line("umask -S").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(umask_symbolic.exit_code, 0);
        assert!(String::from_utf8_lossy(&umask_symbolic.stdout).contains("u="));
    }

    #[test]
    fn ulimit_and_umask_reject_mutating_forms_without_safe_backend() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let ulimit_set = executor
            .run_graph(
                &parse_line("ulimit 1024").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(ulimit_set.exit_code, 2);
        assert!(String::from_utf8_lossy(&ulimit_set.stderr).contains("unsupported"));

        let umask_set = executor
            .run_graph(
                &parse_line("umask 077").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(umask_set.exit_code, 2);
        assert!(String::from_utf8_lossy(&umask_set.stderr).contains("unsupported"));
    }

    #[test]
    fn ulimit_and_umask_report_resolution_consistently() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let command_v = executor
            .run_graph(
                &parse_line("command -v ulimit umask").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&command_v.stdout),
            "ulimit\numask\n"
        );

        let type_out = executor
            .run_graph(
                &parse_line("type ulimit umask").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&type_out.stdout),
            "ulimit is an agsh builtin\numask is an agsh builtin\n"
        );

        let builtin_umask = executor
            .run_graph(
                &parse_line("builtin umask").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(builtin_umask.exit_code, 0);
    }

    #[test]
    fn set_lists_shell_variables() {
        let mut state = ShellState::from_current_process();
        state.set_var("AGSH_SET_TEST", "value with space");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("set").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(
            String::from_utf8_lossy(&outcome.stdout).contains("AGSH_SET_TEST='value with space'\n")
        );
    }

    #[test]
    fn set_replaces_and_clears_positionals() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -- one two").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let outcome = executor
            .run_graph(
                &parse_line("echo \"$1/$2/$@\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "one/two/one two\n"
        );

        executor
            .run_graph(
                &parse_line("set --").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let cleared = executor
            .run_graph(
                &parse_line("echo \"${1:-empty}/${@:-empty}\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&cleared.stdout), "empty/empty\n");
    }

    #[test]
    fn set_rejects_unsupported_options() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("set -z").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 2);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("unsupported option"));
    }

    #[test]
    fn set_allexport_controls_option_and_reports_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let enabled = executor
            .run_graph(
                &parse_line("set -a").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(enabled.exit_code, 0);
        assert!(state.allexport());

        let listed = executor
            .run_graph(
                &parse_line("set -o").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&listed.stdout),
            "allexport\ton\nerrexit\toff\nnounset\toff\nnoclobber\toff\nnoglob\toff\npipefail\toff\nxtrace\toff\n"
        );

        executor
            .run_graph(
                &parse_line("set +a").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.allexport());

        executor
            .run_graph(
                &parse_line("set -o allexport").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.allexport());

        executor
            .run_graph(
                &parse_line("set +o allexport").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.allexport());
    }

    #[test]
    fn set_errexit_controls_option_and_reports_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let enabled = executor
            .run_graph(
                &parse_line("set -e").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(enabled.exit_code, 0);
        assert!(state.errexit());

        let listed = executor
            .run_graph(
                &parse_line("set -o").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&listed.stdout),
            "allexport\toff\nerrexit\ton\nnounset\toff\nnoclobber\toff\nnoglob\toff\npipefail\toff\nxtrace\toff\n"
        );

        executor
            .run_graph(
                &parse_line("set +e").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.errexit());

        executor
            .run_graph(
                &parse_line("set -o errexit").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.errexit());

        executor
            .run_graph(
                &parse_line("set +o errexit").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.errexit());
    }

    #[test]
    fn set_nounset_controls_unset_parameter_expansion() {
        let mut state = ShellState::from_current_process();
        state.unset("AGSH_NOUNSET_MISSING");
        let mut executor = Executor::new();

        let enabled = executor
            .run_graph(
                &parse_line("set -u").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(enabled.exit_code, 0);
        assert!(state.nounset());

        let err = executor
            .run_graph(
                &parse_line("echo $AGSH_NOUNSET_MISSING").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap_err();
        assert!(err
            .message
            .contains("AGSH_NOUNSET_MISSING: parameter not set"));

        let defaulted = executor
            .run_graph(
                &parse_line("echo ${AGSH_NOUNSET_MISSING:-fallback}").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&defaulted.stdout), "fallback\n");

        executor
            .run_graph(
                &parse_line("set +u").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.nounset());

        let disabled = executor
            .run_graph(
                &parse_line("echo $AGSH_NOUNSET_MISSING").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&disabled.stdout), "\n");
    }

    #[test]
    fn set_o_nounset_controls_option_and_reports_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let initial = executor
            .run_graph(
                &parse_line("set -o").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&initial.stdout),
            "allexport\toff\nerrexit\toff\nnounset\toff\nnoclobber\toff\nnoglob\toff\npipefail\toff\nxtrace\toff\n"
        );

        executor
            .run_graph(
                &parse_line("set -o nounset").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.nounset());

        let listed = executor
            .run_graph(
                &parse_line("set -o").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&listed.stdout),
            "allexport\toff\nerrexit\toff\nnounset\ton\nnoclobber\toff\nnoglob\toff\npipefail\toff\nxtrace\toff\n"
        );

        executor
            .run_graph(
                &parse_line("set +o nounset").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.nounset());
    }

    #[test]
    fn set_o_pipefail_controls_pipeline_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let enabled = executor
            .run_graph(
                &parse_line("set -o pipefail").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(enabled.exit_code, 0);
        assert!(state.pipefail());

        let listed = executor
            .run_graph(
                &parse_line("set -o").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&listed.stdout),
            "allexport\toff\nerrexit\toff\nnounset\toff\nnoclobber\toff\nnoglob\toff\npipefail\ton\nxtrace\toff\n"
        );

        executor
            .run_graph(
                &parse_line("set +o pipefail").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.pipefail());
    }

    #[test]
    fn set_noclobber_controls_option_and_reports_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let enabled = executor
            .run_graph(
                &parse_line("set -C").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(enabled.exit_code, 0);
        assert!(state.noclobber());

        let listed = executor
            .run_graph(
                &parse_line("set -o").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&listed.stdout),
            "allexport\toff\nerrexit\toff\nnounset\toff\nnoclobber\ton\nnoglob\toff\npipefail\toff\nxtrace\toff\n"
        );

        executor
            .run_graph(
                &parse_line("set +C").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.noclobber());

        executor
            .run_graph(
                &parse_line("set -o noclobber").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.noclobber());

        executor
            .run_graph(
                &parse_line("set +o noclobber").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.noclobber());
    }

    #[test]
    fn set_noglob_controls_option_and_reports_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let enabled = executor
            .run_graph(
                &parse_line("set -f").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(enabled.exit_code, 0);
        assert!(state.noglob());

        let listed = executor
            .run_graph(
                &parse_line("set -o").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&listed.stdout),
            "allexport\toff\nerrexit\toff\nnounset\toff\nnoclobber\toff\nnoglob\ton\npipefail\toff\nxtrace\toff\n"
        );

        executor
            .run_graph(
                &parse_line("set +f").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.noglob());

        executor
            .run_graph(
                &parse_line("set -o noglob").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.noglob());

        executor
            .run_graph(
                &parse_line("set +o noglob").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.noglob());
    }

    #[test]
    fn set_xtrace_traces_expanded_commands_to_stderr() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -x").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.xtrace());

        let traced = executor
            .run_graph(
                &parse_line(r#"echo "hello world""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&traced.stdout), "hello world\n");
        assert_eq!(
            String::from_utf8_lossy(&traced.stderr),
            "+ echo 'hello world'\n"
        );

        let disabled = executor
            .run_graph(
                &parse_line("set +x").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&disabled.stderr), "+ set +x\n");
        assert!(!state.xtrace());

        let quiet = executor
            .run_graph(
                &parse_line("echo quiet").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(quiet.stderr.is_empty());
    }

    #[test]
    fn set_o_xtrace_controls_tracing_option() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -o xtrace").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.xtrace());

        let listed = executor
            .run_graph(
                &parse_line("set -o").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&listed.stdout),
            "allexport\toff\nerrexit\toff\nnounset\toff\nnoclobber\toff\nnoglob\toff\npipefail\toff\nxtrace\ton\n"
        );
        assert_eq!(String::from_utf8_lossy(&listed.stderr), "+ set -o\n");

        executor
            .run_graph(
                &parse_line("set +o xtrace").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.xtrace());
    }

    #[test]
    fn nounset_errors_on_braced_length_of_unset_parameter() {
        let mut state = ShellState::from_current_process();
        state.unset("AGSH_NOUNSET_LENGTH_MISSING");
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -u").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let err = executor
            .run_graph(
                &parse_line("echo ${#AGSH_NOUNSET_LENGTH_MISSING}").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap_err();

        assert!(err
            .message
            .contains("AGSH_NOUNSET_LENGTH_MISSING: parameter not set"));
    }

    #[test]
    fn read_assigns_fields_from_pipeline_stdin() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'one two three\n' | read FIRST REST").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("FIRST"), Some("one"));
        assert_eq!(state.lookup("REST"), Some("two three"));
    }

    #[test]
    fn final_pipeline_read_stage_can_update_parent_state() {
        let mut state = ShellState::from_current_process();
        state.unset("PIPE_READ_VALUE");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'value\n' | read PIPE_READ_VALUE").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("PIPE_READ_VALUE"), Some("value"));
    }

    #[test]
    fn final_pipeline_while_read_stage_consumes_buffered_stdin() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'one\ntwo\n' | while read line; do echo \"<$line>\"; done")
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "<one>\n<two>\n");
    }

    #[test]
    fn final_pipeline_while_read_stage_uses_input_redirection() {
        let temp = unique_temp_dir("while-input-redir");
        std::fs::write(temp.join("in.txt"), "file\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf 'pipe\n' | while read line; do echo "$line"; done < in.txt"#)
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "file\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn non_final_compound_pipeline_stage_feeds_next_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "printf 'one\ntwo\n' | while read line; do echo \"$line\"; done | wc -l",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "2");
    }

    #[test]
    fn non_final_compound_pipeline_stage_streams_to_next_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"yes x | while read line; do echo "$line"; break; done | wc -l"#)
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "1");
    }

    #[test]
    fn non_final_for_pipeline_stage_streams_to_next_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"yes ignored | for item in one two three; do echo "$item"; done | wc -l"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "3");
    }

    #[test]
    fn buffered_pipeline_preflights_missing_command_before_starting_producer() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        // A missing command in a pipeline stage yields exit 127 (and an error on
        // stderr) without aborting the command list, matching POSIX shells.
        let outcome = executor
            .run_graph(
                &parse_line("yes x | AGSH_MISSING_PIPE_COMMAND | wc -l").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 127);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("AGSH_MISSING_PIPE_COMMAND"));
    }

    #[test]
    fn final_pipeline_select_stage_consumes_buffered_stdin() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "printf '2\n' | select item in one two; do echo \"$REPLY:$item\"; break; done",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "2:two\n");
        assert_eq!(state.lookup("REPLY"), Some("2"));
        assert_eq!(state.lookup("item"), Some("two"));
    }

    #[test]
    fn external_prefix_streams_into_final_compound_consumer() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"yes x | while read line; do echo "$line"; break; done"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "x\n");
    }

    #[test]
    fn external_prefix_streams_into_final_function_consumer() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"reader() { read line; echo "$line"; }; yes y | reader"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "y\n");
    }

    #[test]
    fn non_final_select_pipeline_stage_feeds_next_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "printf '1\n' | select item in one two; do echo \"$item\"; break; done | wc -l",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "1");
    }

    #[test]
    fn non_final_select_pipeline_stage_streams_to_next_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"yes 1 | select item in one two; do echo "$item"; break; done | wc -l"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "1");
    }

    #[test]
    fn adjacent_non_final_shell_stages_stream_to_next_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"yes x | while read line; do echo "$line"; break; done | while read line; do echo "$line"; break; done | wc -l"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "1");
    }

    #[test]
    fn first_stage_compound_producer_streams_to_external_consumer() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"for item in one two; do echo "$item"; done | wc -l"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "2");
    }

    #[test]
    fn first_stage_function_producer_streams_to_external_consumer() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"producer() { echo one; echo two; }; producer | wc -l"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "2");
    }

    #[test]
    fn first_stage_shell_producer_streams_through_later_shell_stage() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"for item in one; do echo "$item"; done | cat | while read line; do echo "$line"; done | wc -l"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "1");
    }

    #[test]
    fn non_contiguous_shell_stages_stream_through_external_middle_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"yes x | while read line; do echo "$line"; break; done | cat | while read line; do echo "$line"; break; done | wc -l"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "1");
    }

    #[test]
    fn non_contiguous_function_stages_stream_through_external_middle_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"reader() { read line; echo "$line"; }; yes y | reader | cat | reader | wc -l"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "1");
    }

    #[cfg(unix)]
    #[test]
    fn non_final_shell_stage_with_output_redirection_streams_to_next_command() {
        let temp = unique_temp_dir("stream-shell-redir");
        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"yes x | while read line; do echo "$line"; break; done > out.txt | wc -l"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "0");
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "x\n"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_final_shell_stage_with_input_redirection_streams_to_next_command() {
        let temp = unique_temp_dir("stream-shell-input-redir");
        std::fs::write(temp.join("in.txt"), "file\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"yes ignored | while read line; do echo "$line"; break; done < in.txt | cat"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "file\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_final_function_stage_with_input_redirection_streams_to_next_command() {
        let temp = unique_temp_dir("stream-function-input-redir");
        std::fs::write(temp.join("in.txt"), "file\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"reader() { read line; echo "$line"; }; yes ignored | reader < in.txt | cat"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "file\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_final_function_stage_with_output_redirection_streams_to_next_command() {
        let temp = unique_temp_dir("stream-function-redir");
        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"reader() { read line; echo "$line"; }; yes y | reader > out.txt | wc -l"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "0");
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "y\n"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn non_final_pipeline_stages_do_not_leak_shell_state() {
        let mut state = ShellState::from_current_process();
        state.unset("AGSH_PIPE_LEAK");
        let mut executor = Executor::new();
        let capture_options = ExecutionOptions {
            output_mode: OutputMode::Clean,
            ..ExecutionOptions::default()
        };

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"export AGSH_PIPE_LEAK=bad | sh -c 'printf %s "${AGSH_PIPE_LEAK-unset}"'"#,
                )
                .unwrap(),
                &mut state,
                &capture_options,
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "unset");
        assert_eq!(state.lookup("AGSH_PIPE_LEAK"), None);
    }

    #[test]
    fn read_uses_shell_ifs_for_field_splitting() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", ":");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'a:b:c\n' | read FIRST REST").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("FIRST"), Some("a"));
        assert_eq!(state.lookup("REST"), Some("b:c"));
    }

    #[test]
    fn read_uses_input_redirection() {
        let temp = unique_temp_dir("read-input-redir");
        std::fs::write(temp.join("in.txt"), "from-file\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("read LINE < in.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("LINE"), Some("from-file"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn function_uses_input_redirection() {
        let temp = unique_temp_dir("function-input-redir");
        std::fs::write(temp.join("in.txt"), "from-file\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"reader() { read line; echo "$line"; }; reader < in.txt"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "from-file\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn read_preserves_interior_empty_ifs_fields() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", ":");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'a::b:\n' | read FIRST SECOND THIRD FOURTH").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("FIRST"), Some("a"));
        assert_eq!(state.lookup("SECOND"), Some(""));
        assert_eq!(state.lookup("THIRD"), Some("b"));
        assert_eq!(state.lookup("FOURTH"), Some(""));
    }

    #[test]
    fn read_trims_ifs_whitespace_around_non_whitespace_delimiters() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " :");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf ' a : b : \n' | read FIRST REST").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("FIRST"), Some("a"));
        assert_eq!(state.lookup("REST"), Some("b"));
    }

    #[test]
    fn read_uses_reply_by_default_and_reports_eof() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let reply = executor
            .run_graph(
                &parse_line("printf 'hello\n' | read").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(reply.exit_code, 0);
        assert_eq!(state.lookup("REPLY"), Some("hello"));

        let eof = executor
            .run_graph(
                &parse_line("printf '' | read EMPTY").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(eof.exit_code, 1);
        assert_eq!(state.lookup("EMPTY"), None);
    }

    #[test]
    fn read_supports_raw_mode_and_rejects_bad_names() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let raw = executor
            .run_graph(
                &parse_line("printf 'a\\\\b\n' | read -r RAW").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(raw.exit_code, 0);
        assert_eq!(state.lookup("RAW"), Some("a\\b"));

        let invalid = executor
            .run_graph(
                &parse_line("printf 'x\n' | read 1BAD").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(invalid.exit_code, 2);
        assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid identifier"));
    }

    #[test]
    fn read_supports_prompt_option_for_non_terminal_stdin() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let separated = executor
            .run_graph(
                &parse_line("printf 'value\n' | read -p 'Name: ' NAME").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(separated.exit_code, 0);
        assert!(separated.stderr.is_empty());
        assert_eq!(state.lookup("NAME"), Some("value"));

        let inline = executor
            .run_graph(
                &parse_line("printf 'other\n' | read -pPrompt: OTHER").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(inline.exit_code, 0);
        assert!(inline.stderr.is_empty());
        assert_eq!(state.lookup("OTHER"), Some("other"));
    }

    #[test]
    fn read_prompt_option_requires_prompt_argument() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'value\n' | read -p").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 2);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("requires a prompt"));
    }

    #[test]
    fn read_joins_backslash_continuations_by_default() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'one\\\\\ntwo three\n' | read FIRST REST").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("FIRST"), Some("onetwo"));
        assert_eq!(state.lookup("REST"), Some("three"));
    }

    #[test]
    fn read_raw_mode_does_not_join_backslash_continuations() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'one\\\\\ntwo\n' | read -r RAW").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("RAW"), Some("one\\"));
    }

    #[test]
    fn history_lists_limits_and_clears_entries() {
        let mut state = ShellState::from_current_process();
        state.record_history("echo one");
        state.record_history("echo two");
        state.record_history("echo three");
        let mut executor = Executor::new();

        let all = executor
            .run_graph(
                &parse_line("history").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let all_text = String::from_utf8_lossy(&all.stdout);
        assert!(all_text.contains("    1  echo one\n"));
        assert!(all_text.contains("    3  echo three\n"));

        let tail = executor
            .run_graph(
                &parse_line("history 2").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let tail_text = String::from_utf8_lossy(&tail.stdout);
        assert!(!tail_text.contains("echo one"));
        assert!(tail_text.contains("    2  echo two\n"));
        assert!(tail_text.contains("    3  echo three\n"));

        let cleared = executor
            .run_graph(
                &parse_line("history -c").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(cleared.exit_code, 0);
        assert_eq!(state.history_len(), 0);
    }

    #[test]
    fn rich_stdout_gate_is_terminal_raw_and_unredirected_only() {
        let mut state = ShellState::from_current_process();
        let options = ExecutionOptions::default();
        let graph = parse_line("history").unwrap();
        let invocation = expand_invocation(&graph.pipeline.commands[0], &mut state).unwrap();

        assert!(rich_stdout_allowed_for_invocation_with_terminal(
            &invocation,
            &state,
            &options,
            true,
        ));
        assert!(!rich_stdout_allowed_for_invocation_with_terminal(
            &invocation,
            &state,
            &options,
            false,
        ));

        let capture_options = ExecutionOptions {
            output_mode: OutputMode::Clean,
            ..ExecutionOptions::default()
        };
        assert!(!rich_stdout_allowed_for_invocation_with_terminal(
            &invocation,
            &state,
            &capture_options,
            true,
        ));

        let redirected = parse_line("history > /tmp/agsh-history-color-test").unwrap();
        let redirected_invocation =
            expand_invocation(&redirected.pipeline.commands[0], &mut state).unwrap();
        assert!(!rich_stdout_allowed_for_invocation_with_terminal(
            &redirected_invocation,
            &state,
            &options,
            true,
        ));
    }

    #[test]
    fn history_rejects_invalid_arguments() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("history nope").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 2);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("invalid count"));
    }

    #[test]
    fn history_search_list_and_stats_are_richer() {
        let mut state = ShellState::from_current_process();
        state.record_history("cargo check");
        state.finalize_history(0, 1200);
        state.record_history("cargo test parser");
        state.finalize_history(101, 3400);
        state.record_history("git status");
        state.finalize_history(0, 20);
        let mut executor = Executor::new();

        let list = executor
            .run_graph(
                &parse_line("history list --limit 2").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let list_text = String::from_utf8_lossy(&list.stdout);
        assert!(list_text.contains("git status"), "list: {list_text}");
        assert!(list_text.contains("ok"), "list: {list_text}");

        let search = executor
            .run_graph(
                &parse_line("history search --mode family cargo").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let search_text = String::from_utf8_lossy(&search.stdout);
        assert!(search_text.contains("cargo check"), "search: {search_text}");
        assert!(
            search_text.contains("cargo test parser"),
            "search: {search_text}"
        );
        assert!(!search_text.contains("git status"), "search: {search_text}");

        let failed = executor
            .run_graph(
                &parse_line("history search --failed").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let failed_text = String::from_utf8_lossy(&failed.stdout);
        assert!(
            failed_text.contains("cargo test parser"),
            "failed: {failed_text}"
        );
        assert!(!failed_text.contains("git status"), "failed: {failed_text}");

        let today = executor
            .run_graph(
                &parse_line("history search --today cargo").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let today_text = String::from_utf8_lossy(&today.stdout);
        assert!(today_text.contains("cargo check"), "today: {today_text}");

        let since = executor
            .run_graph(
                &parse_line("history search --since yesterday git").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let since_text = String::from_utf8_lossy(&since.stdout);
        assert!(since_text.contains("git status"), "since: {since_text}");

        let json = executor
            .run_graph(
                &parse_line("history search --json cargo").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let json_text = String::from_utf8_lossy(&json.stdout);
        assert!(json_text.contains("\"command\""), "json: {json_text}");
        assert!(json_text.contains("\"family\""), "json: {json_text}");

        let stats = executor
            .run_graph(
                &parse_line("history stats").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let stats_text = String::from_utf8_lossy(&stats.stdout);
        assert!(stats_text.contains("commands: 3"), "stats: {stats_text}");
        assert!(stats_text.contains("cargo"), "stats: {stats_text}");
    }

    #[test]
    fn history_search_filters_by_explicit_date() {
        let mut state = ShellState::from_current_process();
        let cwd = state.cwd().display().to_string();
        let mut on_date =
            agsh_store::history::HistoryEntry::new("make july-four", cwd.clone(), 1_783_166_400);
        on_date.exit_code = Some(0);
        on_date.command_family = Some("make".to_string());
        let mut previous_day =
            agsh_store::history::HistoryEntry::new("make july-three", cwd, 1_783_080_000);
        previous_day.exit_code = Some(0);
        previous_day.command_family = Some("make".to_string());
        state.push_history_entry_for_test(previous_day);
        state.push_history_entry_for_test(on_date);
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("history search --date 2026-07-04 make").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let text = String::from_utf8_lossy(&outcome.stdout);
        assert!(text.contains("make july-four"), "date search: {text}");
        assert!(!text.contains("make july-three"), "date search: {text}");
    }

    #[test]
    fn history_records_session_output_mode_and_family_metadata() {
        let mut state = ShellState::from_current_process();
        state.export_var("AGSH_SESSION", "session-test");
        state.record_history_with_mode(
            "semantic cargo check",
            Some(agsh_output::OutputMode::Semantic),
        );
        state.finalize_history(0, 42);

        let entries = state.history_entries();
        let entry = entries.last().expect("history entry");
        assert_eq!(entry.session_id.as_deref(), Some("session-test"));
        assert_eq!(entry.output_mode.as_deref(), Some("semantic"));
        assert_eq!(entry.command_family.as_deref(), Some("cargo"));
    }

    #[test]
    fn test_builtin_evaluates_strings_numbers_and_brackets() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let non_empty = executor
            .run_graph(
                &parse_line("test -n value").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(non_empty.exit_code, 0);

        let empty = executor
            .run_graph(
                &parse_line("test -z ''").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(empty.exit_code, 0);

        let numeric = executor
            .run_graph(
                &parse_line("[ 4 -gt 2 ]").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(numeric.exit_code, 0);

        let mismatch = executor
            .run_graph(
                &parse_line("[ left = right ]").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(mismatch.exit_code, 1);
    }

    #[test]
    fn test_builtin_uses_shell_cwd_for_file_checks() {
        let temp = unique_temp_dir("test-builtin");
        std::fs::write(temp.join("file.txt"), "content").unwrap();
        std::fs::create_dir(temp.join("dir")).unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let file = executor
            .run_graph(
                &parse_line("test -f file.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(file.exit_code, 0);

        let dir = executor
            .run_graph(
                &parse_line("test -d dir").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(dir.exit_code, 0);

        let missing = executor
            .run_graph(
                &parse_line("test -e missing.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(missing.exit_code, 1);

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn eval_executes_joined_arguments_in_current_shell() {
        let mut state = ShellState::from_current_process();
        state.set_var("WORD", "expanded");
        let mut executor = Executor::new();

        let expanded = executor
            .run_graph(
                &parse_line("eval echo '$WORD'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(expanded.stdout, b"expanded\n");

        let assigned = executor
            .run_graph(
                &parse_line("eval FOO=from_eval").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(assigned.exit_code, 0);
        assert_eq!(state.lookup("FOO"), Some("from_eval"));
    }

    #[test]
    fn eval_pipeline_consumer_reads_buffered_stdin() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'one\ntwo\n' | eval 'read A; read B; echo \"$A/$B\"'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one/two\n");
    }

    #[test]
    fn source_executes_file_in_current_shell() {
        let temp = unique_temp_dir("source");
        std::fs::write(
            temp.join("script.agsh"),
            "FOO=from_source\nalias hi='echo sourced'\nhi\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"sourced\n");
        assert_eq!(state.lookup("FOO"), Some("from_source"));
        assert_eq!(state.alias("hi"), Some("echo sourced"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_rejects_oversized_files_without_allocating_them() {
        let temp = unique_temp_dir("source-oversized");
        let path = temp.join("script.agsh");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len((MAX_SHELL_SOURCE_BYTES + 1) as u64).unwrap();
        drop(file);

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();
        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 1);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("exceeds"));
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_pipeline_consumer_reads_buffered_stdin() {
        let temp = unique_temp_dir("source-pipeline-stdin");
        std::fs::write(temp.join("script.agsh"), "read A\nread B\necho \"$A/$B\"\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'one\ntwo\n' | source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one/two\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_joins_backslash_continued_lines() {
        let temp = unique_temp_dir("source-continuation");
        std::fs::write(
            temp.join("script.agsh"),
            "VALUE=one\\\ntwo\nprintf '%s %s\\n' \"$VALUE\" \\\nthree\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"onetwo three\n");
        assert_eq!(state.lookup("VALUE"), Some("onetwo"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_supports_multiline_function_definitions() {
        let temp = unique_temp_dir("source-multiline-function");
        std::fs::write(
            temp.join("script.agsh"),
            "hi() {\n  echo one\n  false && echo no || echo two\n}\nhi\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one\ntwo\n");
        assert!(state.function("hi").is_some());

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_supports_multiline_if_blocks() {
        let temp = unique_temp_dir("source-multiline-if");
        std::fs::write(
            temp.join("script.agsh"),
            "if false\nthen\n  echo no\nelif true\nthen\n  FOO=from_elif\nelse\n  FOO=from_else\nfi\necho \"$FOO\"\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "from_elif\n");
        assert_eq!(state.lookup("FOO"), Some("from_elif"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_supports_multiline_while_blocks() {
        let temp = unique_temp_dir("source-multiline-while");
        std::fs::write(
            temp.join("script.agsh"),
            "COUNT=0\nwhile [ \"$COUNT\" -lt 2 ]\ndo\n  echo \"$COUNT\"\n  COUNT=$(expr \"$COUNT\" + 1)\ndone\necho done:$COUNT\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "0\n1\ndone:2\n");
        assert_eq!(state.lookup("COUNT"), Some("2"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_supports_multiline_until_blocks() {
        let temp = unique_temp_dir("source-multiline-until");
        std::fs::write(
            temp.join("script.agsh"),
            "COUNT=0\nuntil [ \"$COUNT\" -ge 2 ]\ndo\n  echo \"$COUNT\"\n  COUNT=$(expr \"$COUNT\" + 1)\ndone\necho done:$COUNT\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "0\n1\ndone:2\n");
        assert_eq!(state.lookup("COUNT"), Some("2"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_supports_multiline_for_blocks() {
        let temp = unique_temp_dir("source-multiline-for");
        std::fs::write(
            temp.join("script.agsh"),
            "for item in one \"two words\"\ndo\n  printf '<%s>\\n' \"$item\"\ndone\necho last:$item\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<one>\n<two words>\nlast:two words\n"
        );
        assert_eq!(state.lookup("item"), Some("two words"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_supports_multiline_select_blocks() {
        let temp = unique_temp_dir("source-multiline-select");
        std::fs::write(
            temp.join("script.agsh"),
            "select item in one \"two words\"\ndo\n  printf '<%s:%s>\\n' \"$REPLY\" \"$item\"\n  break\ndone\necho last:$item\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf '2\n' | source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<2:two words>\nlast:two words\n"
        );
        assert_eq!(state.lookup("REPLY"), Some("2"));
        assert_eq!(state.lookup("item"), Some("two words"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_supports_multiline_case_blocks() {
        let temp = unique_temp_dir("source-multiline-case");
        std::fs::write(
            temp.join("script.agsh"),
            "KIND=rs\ncase \"$KIND\" in\n  rust|rs)\n    echo rust\n    ;;\n  *)\n    echo other\n    ;;\nesac\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "rust\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_supports_nested_multiline_case_blocks() {
        let temp = unique_temp_dir("source-nested-multiline-case");
        std::fs::write(
            temp.join("script.agsh"),
            "case outer in\n  outer)\n    case inner in\n      inner)\n        echo nested\n        ;;\n    esac\n    echo after\n    ;;\nesac\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "nested\nafter\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_errexit_stops_after_failed_command() {
        let temp = unique_temp_dir("source-errexit");
        std::fs::write(temp.join("script.agsh"), "set -e\nfalse\nFOO=after\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 1);
        assert!(state.errexit());
        assert_ne!(state.lookup("FOO"), Some("after"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_plus_errexit_continues_after_failed_command() {
        let temp = unique_temp_dir("source-plus-errexit");
        std::fs::write(
            temp.join("script.agsh"),
            "set -e\nset +e\nfalse\nFOO=after\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(!state.errexit());
        assert_eq!(state.lookup("FOO"), Some("after"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn dot_source_sets_positional_arguments_temporarily() {
        let temp = unique_temp_dir("dot-source");
        std::fs::write(temp.join("script.agsh"), "echo \"$1\"\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        state.set_var("1", "outer");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(". script.agsh inner").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"inner\n");
        assert_eq!(state.lookup("1"), Some("outer"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn bracket_test_reports_missing_closer_as_syntax_error() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("[ value").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 2);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("missing ]"));
    }

    #[cfg(unix)]
    #[test]
    fn path_cache_invalidates_when_path_changes() {
        let temp = unique_temp_dir("path-cache");
        let bin_one = temp.join("one");
        let bin_two = temp.join("two");
        std::fs::create_dir_all(&bin_one).unwrap();
        std::fs::create_dir_all(&bin_two).unwrap();
        write_executable(&bin_one.join("agsh-cache-cmd"), "printf %s one");
        write_executable(&bin_two.join("agsh-cache-cmd"), "printf %s two");

        let mut state = ShellState::from_current_process();
        state.export_var("PATH", bin_one.display().to_string());
        let mut executor = Executor::new();

        let first = executor
            .run_graph(
                &parse_line("agsh-cache-cmd").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(first.stdout, b"one");
        assert_eq!(state.path_cache_len_for_tests(), 1);

        state.export_var("PATH", bin_two.display().to_string());
        assert_eq!(state.path_cache_len_for_tests(), 0);
        let second = executor
            .run_graph(
                &parse_line("agsh-cache-cmd").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(second.stdout, b"two");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn path_cache_re_resolves_when_cached_executable_disappears() {
        let temp = unique_temp_dir("path-cache-stale");
        let bin_one = temp.join("one");
        let bin_two = temp.join("two");
        std::fs::create_dir_all(&bin_one).unwrap();
        std::fs::create_dir_all(&bin_two).unwrap();
        let first_path = bin_one.join("agsh-cache-stale-cmd");
        write_executable(&first_path, "printf %s one");
        write_executable(&bin_two.join("agsh-cache-stale-cmd"), "printf %s two");

        let mut state = ShellState::from_current_process();
        state.export_var(
            "PATH",
            format!("{}:{}", bin_one.display(), bin_two.display()),
        );
        let mut executor = Executor::new();
        let options = ExecutionOptions {
            output_mode: OutputMode::Clean,
            allow_process_replacement: false,
        };

        let first = executor
            .run_graph(
                &parse_line("agsh-cache-stale-cmd").unwrap(),
                &mut state,
                &options,
            )
            .unwrap();
        assert_eq!(first.stdout, b"one");
        std::fs::remove_file(first_path).unwrap();

        let second = executor
            .run_graph(
                &parse_line("agsh-cache-stale-cmd").unwrap(),
                &mut state,
                &options,
            )
            .unwrap();
        assert_eq!(second.stdout, b"two");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn temporary_path_assignment_does_not_poison_path_cache() {
        let temp = unique_temp_dir("path-cache-temp");
        let bin_one = temp.join("one");
        let bin_two = temp.join("two");
        std::fs::create_dir_all(&bin_one).unwrap();
        std::fs::create_dir_all(&bin_two).unwrap();
        write_executable(&bin_one.join("agsh-cache-temp-cmd"), "printf %s one");
        write_executable(&bin_two.join("agsh-cache-temp-cmd"), "printf %s two");

        let mut state = ShellState::from_current_process();
        state.export_var("PATH", bin_one.display().to_string());
        let mut executor = Executor::new();

        let cached = executor
            .run_graph(
                &parse_line("agsh-cache-temp-cmd").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(cached.stdout, b"one");
        assert_eq!(state.path_cache_len_for_tests(), 1);

        let temporary = executor
            .run_graph(
                &parse_line(&format!("PATH={} agsh-cache-temp-cmd", bin_two.display())).unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(temporary.stdout, b"two");
        assert_eq!(state.path_cache_len_for_tests(), 1);

        let still_cached = executor
            .run_graph(
                &parse_line("agsh-cache-temp-cmd").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(still_cached.stdout, b"one");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn aliases_expand_before_builtins_and_can_be_removed() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("alias false=true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let aliased = executor
            .run_graph(
                &parse_line("false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(aliased.exit_code, 0);

        let bypassed = executor
            .run_graph(
                &parse_line("command false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(bypassed.exit_code, 1);

        executor
            .run_graph(
                &parse_line("unalias false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let unaliased = executor
            .run_graph(
                &parse_line("false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(unaliased.exit_code, 1);
    }

    #[test]
    fn alias_values_can_include_arguments() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("alias hi='echo hello'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("hi world").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "hello world\n");
    }

    #[test]
    fn type_and_command_v_report_aliases() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("alias hi='echo hello'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let type_out = executor
            .run_graph(
                &parse_line("type hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&type_out.stdout),
            "hi is aliased to echo hello\n"
        );

        let command_v = executor
            .run_graph(
                &parse_line("command -v hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&command_v.stdout),
            "alias hi='echo hello'\n"
        );
    }

    #[test]
    fn abbreviations_expand_before_builtins_and_can_be_removed() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("abbr false=true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let abbreviated = executor
            .run_graph(
                &parse_line("false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(abbreviated.exit_code, 0);

        let bypassed = executor
            .run_graph(
                &parse_line("command false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(bypassed.exit_code, 1);

        executor
            .run_graph(
                &parse_line("unabbr false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let unabbreviated = executor
            .run_graph(
                &parse_line("false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(unabbreviated.exit_code, 1);
    }

    #[test]
    fn abbreviation_values_can_include_arguments() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("abbr hi='echo hello'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("hi world").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "hello world\n");
    }

    #[test]
    fn aliases_take_precedence_over_abbreviations() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("abbr hi='echo abbreviation'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        executor
            .run_graph(
                &parse_line("alias hi='echo alias'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "alias\n");
    }

    #[test]
    fn type_and_command_v_report_abbreviations() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("abbr hi='echo hello'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let type_out = executor
            .run_graph(
                &parse_line("type hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&type_out.stdout),
            "hi is abbreviated to echo hello\n"
        );

        let command_v = executor
            .run_graph(
                &parse_line("command -v hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&command_v.stdout),
            "abbr hi='echo hello'\n"
        );
    }

    #[test]
    fn functions_define_and_execute_with_arguments() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("greet() { echo \"hi $1 $@\" }").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("greet dev agent").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "hi dev dev agent\n"
        );
    }

    #[test]
    fn function_pipeline_consumer_reads_buffered_stdin() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("reader() { read A; read B; echo \"$A/$B\"; }").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'one\ntwo\n' | reader").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one/two\n");
    }

    #[test]
    fn non_final_function_pipeline_stage_feeds_next_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("reader() { while read line; do echo \"$line\"; done; }").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'one\ntwo\n' | reader | wc -l").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "2");
    }

    #[test]
    fn non_final_function_pipeline_stage_streams_to_next_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("reader() { read line; echo \"$line\"; }").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("yes y | reader | wc -l").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "1");
    }

    #[test]
    fn function_bodies_support_command_lists() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("hi() { echo one; false && echo no || echo two; }").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one\ntwo\n");
    }

    #[test]
    fn functions_scope_positionals_and_positional_count() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -- outer-one outer-two outer-three").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        executor
            .run_graph(
                &parse_line(
                    r#"show_args() { printf '<%s>|<%s>|<%s>|<%s>|<%s>\n' "$#" "$1" "$2" "$3" "$@" }"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let scoped = executor
            .run_graph(
                &parse_line("show_args inner").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&scoped.stdout),
            "<1>|<inner>|<>|<>|<inner>\n"
        );

        let restored = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>|<%s>|<%s>\n' "$#" "$1" "$2" "$3" "$*""#)
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&restored.stdout),
            "<3>|<outer-one>|<outer-two>|<outer-three>|<outer-one outer-two outer-three>\n"
        );
    }

    #[test]
    fn functions_take_precedence_over_aliases_and_builtins() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("alias false=true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        executor
            .run_graph(
                &parse_line("false() { echo function-false }").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let function = executor
            .run_graph(
                &parse_line("false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&function.stdout),
            "function-false\n"
        );

        let bypassed = executor
            .run_graph(
                &parse_line("command false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(bypassed.exit_code, 1);
    }

    #[test]
    fn type_and_command_v_report_functions() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("hi() { echo hello }").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let type_out = executor
            .run_graph(
                &parse_line("type hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&type_out.stdout),
            "hi is a function\n"
        );

        let command_v = executor
            .run_graph(
                &parse_line("command -v hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&command_v.stdout), "hi\n");
    }

    #[test]
    fn expands_variables_with_quote_rules() {
        let mut state = ShellState::from_current_process();
        state.set_var("FOO", "bar");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo \"$FOO x\" '$FOO'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "bar x $FOO\n");
    }

    #[test]
    fn unquoted_expansions_are_field_split() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " \t\n");
        state.set_var("WORDS", "one two\tthree\nfour");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>|<%s>\n' $WORDS"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<one>|<two>|<three>|<four>\n"
        );
    }

    #[test]
    fn native_list_values_expand_to_argv_without_ifs_splitting() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " \t\n");
        state.set_value(
            "ITEMS",
            Value::List(vec![
                Value::String("one word".to_string()),
                Value::String("two".to_string()),
                Value::String(String::new()),
            ]),
        );
        let mut executor = Executor::new();

        let dollar_name = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>\n' $ITEMS"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&dollar_name.stdout),
            "<one word>|<two>|<>\n"
        );

        let braced = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>\n' ${ITEMS}"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&braced.stdout),
            "<one word>|<two>|<>\n"
        );
    }

    #[test]
    fn native_list_values_expand_through_set_and_for_items() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " \t\n");
        state.set_value(
            "ITEMS",
            Value::List(vec![
                Value::String("one word".to_string()),
                Value::String("two".to_string()),
                Value::String(String::new()),
            ]),
        );
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -- $ITEMS").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(state.lookup("1"), Some("one word"));
        assert_eq!(state.lookup("2"), Some("two"));
        assert_eq!(state.lookup("3"), Some(""));
        assert_eq!(state.lookup("4"), None);

        let outcome = executor
            .run_graph(
                &parse_line(r#"for item in $ITEMS; do printf '<%s>\n' "$item"; done"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<one word>\n<two>\n<>\n"
        );
    }

    #[test]
    fn native_list_values_expand_inside_literal_prefix_suffix_words() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " \t\n");
        state.set_value(
            "ITEMS",
            Value::List(vec![
                Value::String("one word".to_string()),
                Value::String("two".to_string()),
            ]),
        );
        let mut executor = Executor::new();

        let braced = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' pre${ITEMS}post"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&braced.stdout),
            "<preone wordpost>|<pretwopost>\n"
        );

        let unbraced_with_prefix = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' pre$ITEMS"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&unbraced_with_prefix.stdout),
            "<preone word>|<pretwo>\n"
        );
    }

    #[test]
    fn non_whitespace_ifs_preserves_interior_empty_fields() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", ":");
        state.set_var("WORDS", "a::b:");
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -- $WORDS").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(state.lookup("1"), Some("a"));
        assert_eq!(state.lookup("2"), Some(""));
        assert_eq!(state.lookup("3"), Some("b"));
        assert_eq!(state.lookup("4"), None);
    }

    #[test]
    fn ifs_whitespace_around_non_whitespace_delimiters_is_trimmed() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " :");
        state.set_var("WORDS", " a : b : ");
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -- $WORDS").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(state.lookup("1"), Some("a"));
        assert_eq!(state.lookup("2"), Some("b"));
        assert_eq!(state.lookup("3"), None);
    }

    #[test]
    fn quoted_expansions_preserve_fields_and_empty_values() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " \t\n");
        state.set_var("WORDS", "one two");
        state.set_var("EMPTY", "");
        state.unset("MISSING");
        let mut executor = Executor::new();

        let quoted = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' "$WORDS" "$EMPTY""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&quoted.stdout), "<one two>|<>\n");

        let unquoted_empty = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>\n' a $EMPTY b "$MISSING""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&unquoted_empty.stdout),
            "<a>|<b>|<>\n"
        );
    }

    #[test]
    fn field_splitting_preserves_literal_word_parts() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " \t\n");
        state.set_var("WORDS", "one two");
        let mut executor = Executor::new();

        let mixed = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' pre${WORDS}post"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&mixed.stdout),
            "<preone>|<twopost>\n"
        );

        let escaped_space = executor
            .run_graph(
                &parse_line(r#"printf '<%s>\n' a\ b"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&escaped_space.stdout), "<a b>\n");
    }

    #[test]
    fn unquoted_command_substitution_is_field_split() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " \t\n");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' $(printf 'one two')"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "<one>|<two>\n");
    }

    #[test]
    fn expands_parameter_default_and_alternate_forms() {
        let mut state = ShellState::from_current_process();
        state.set_var("FOO", "bar");
        state.set_var("EMPTY", "");
        state.unset("MISSING");
        let mut executor = Executor::new();

        let defaults = executor
            .run_graph(
                &parse_line("echo \"${MISSING:-fallback}/${EMPTY:-fallback}/${EMPTY-fallback}/${FOO:-fallback}\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&defaults.stdout),
            "fallback/fallback//bar\n"
        );

        let alternates = executor
            .run_graph(
                &parse_line("echo \"${FOO:+yes}/${EMPTY:+yes}/${EMPTY+yes}/${MISSING+yes}\"")
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&alternates.stdout), "yes//yes/\n");
    }

    #[test]
    fn expands_parameter_length_and_nested_default_word() {
        let mut state = ShellState::from_current_process();
        state.set_var("FOO", "bar");
        state.unset("MISSING");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo \"${#FOO}/${MISSING:-$FOO}\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "3/bar\n");
    }

    #[test]
    fn expands_positional_count_parameter() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let empty = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' "$#" "${#}""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&empty.stdout), "<0>|<0>\n");

        executor
            .run_graph(
                &parse_line("set -- one two three").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let populated = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' "$#" "${#}""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&populated.stdout), "<3>|<3>\n");
    }

    #[test]
    fn expands_positional_star_parameter() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -- one two three").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' "$*" "${*}""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<one two three>|<one two three>\n"
        );
    }

    #[test]
    fn quoted_star_joins_positionals_with_first_ifs_character() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line(r#"set -- "one word" two three"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        executor
            .run_graph(
                &parse_line("IFS=:").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' "$*" "${*}""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<one word:two:three>|<one word:two:three>\n"
        );
    }

    #[test]
    fn quoted_star_uses_empty_separator_when_ifs_is_empty() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -- one two three").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        executor
            .run_graph(
                &parse_line("IFS=").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' "$*" "${*}""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<onetwothree>|<onetwothree>\n"
        );
    }

    #[test]
    fn quoted_star_uses_space_separator_when_ifs_is_unset() {
        let mut state = ShellState::from_current_process();
        state.unset("IFS");
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -- one two three").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' "$*" "${*}""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<one two three>|<one two three>\n"
        );
    }

    #[test]
    fn quoted_at_expands_to_separate_fields() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line(r#"set -- one "two words" three"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let dollar_at = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>\n' "$@""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&dollar_at.stdout),
            "<one>|<two words>|<three>\n"
        );

        let braced_at = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>\n' "${@}""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&braced_at.stdout),
            "<one>|<two words>|<three>\n"
        );
    }

    #[test]
    fn quoted_at_with_no_positionals_expands_to_zero_fields() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set --").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line(r#"echo pre "$@" post"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "pre post\n");
    }

    #[test]
    fn quoted_at_mixed_words_attach_prefix_and_suffix() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line(r#"set -- one two three"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let separated_segments = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>\n' pre"$@"post"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&separated_segments.stdout),
            "<preone>|<two>|<threepost>\n"
        );

        let embedded_segment = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>\n' "pre${@}post""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&embedded_segment.stdout),
            "<preone>|<two>|<threepost>\n"
        );
    }

    #[test]
    fn quoted_at_mixed_words_collapse_when_no_positionals() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set --").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>\n' pre"$@"post x"$@" "$@"y"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<prepost>|<x>|<y>\n"
        );
    }

    #[test]
    fn expands_shell_pid_parameter() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf '%s\n' "$$""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            format!("{}\n", std::process::id())
        );
    }

    #[test]
    fn expands_parameter_assignment_defaults_and_persists_values() {
        let mut state = ShellState::from_current_process();
        state.set_var("FOO", "bar");
        state.set_var("EMPTY", "");
        state.unset("MISSING");
        state.unset("MISSING2");
        let mut executor = Executor::new();

        let assigned = executor
            .run_graph(
                &parse_line(
                    "echo \"${MISSING:=fallback}/${EMPTY=kept}/${EMPTY:=filled}/${MISSING2=$FOO}\"",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&assigned.stdout),
            "fallback//filled/bar\n"
        );
        assert_eq!(state.lookup("MISSING"), Some("fallback"));
        assert_eq!(state.lookup("EMPTY"), Some("filled"));
        assert_eq!(state.lookup("MISSING2"), Some("bar"));

        let reused = executor
            .run_graph(
                &parse_line("echo \"$MISSING/$EMPTY/$MISSING2\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&reused.stdout),
            "fallback/filled/bar\n"
        );
    }

    #[test]
    fn parameter_error_form_fails_expansion() {
        let mut state = ShellState::from_current_process();
        state.unset("MISSING");
        let mut executor = Executor::new();

        let err = executor
            .run_graph(
                &parse_line("echo \"${MISSING:?need value}\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap_err();

        assert!(err.message.contains("MISSING: need value"));
    }

    #[test]
    fn parameter_assignment_rejects_special_targets() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let err = executor
            .run_graph(
                &parse_line("echo \"${1:=bad}\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap_err();

        assert!(err.message.contains("cannot assign default"));
    }

    #[test]
    fn expands_parameter_pattern_removal_forms() {
        let mut state = ShellState::from_current_process();
        state.set_var("PATHLIKE", "src/bin/tool.rs");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"echo "${PATHLIKE#*/}/${PATHLIKE##*/}/${PATHLIKE%/*}/${PATHLIKE%%/*}""#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "bin/tool.rs/tool.rs/src/bin/src\n"
        );
    }

    #[test]
    fn parameter_pattern_removal_expands_pattern_words() {
        let mut state = ShellState::from_current_process();
        state.set_var("PATHLIKE", "src/bin/tool.rs");
        state.set_var("PREFIX_PATTERN", "src/*/");
        state.unset("MISSING");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"echo "${PATHLIKE#$PREFIX_PATTERN}/${PATHLIKE#nomatch}/${MISSING#*}""#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "tool.rs/src/bin/tool.rs/\n"
        );
    }

    #[test]
    fn parameter_pattern_removal_honors_empty_star_matches() {
        let mut state = ShellState::from_current_process();
        state.set_var("PATHLIKE", "src/bin/tool.rs");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"echo "${PATHLIKE#*}/${PATHLIKE##*}/${PATHLIKE%*}/${PATHLIKE%%*}""#)
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "src/bin/tool.rs//src/bin/tool.rs/\n"
        );
    }

    #[test]
    fn expands_parameter_pattern_substitution_forms() {
        let mut state = ShellState::from_current_process();
        state.set_var("VALUE", "one-two-two");
        state.set_var("REPL", "2");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"echo "${VALUE/two/$REPL}/${VALUE//two/$REPL}/${VALUE/#one/1}/${VALUE/%two/2}""#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "one-2-two/one-2-2/1-two-two/one-two-2\n"
        );
    }

    #[test]
    fn parameter_pattern_substitution_uses_shell_patterns() {
        let mut state = ShellState::from_current_process();
        state.set_var("VALUE", "one-two-two");
        state.set_var("STAR", "abc");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"echo "${VALUE/t*/X}/${VALUE//t*/X}/${STAR//*/X}""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one-X/one-X/X\n");
    }

    #[test]
    fn expands_last_status_parameter_between_commands() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let failed = executor
            .run_graph(
                &parse_line("false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(failed.exit_code, 1);
        assert_eq!(state.last_status(), 1);

        let status = executor
            .run_graph(
                &parse_line("echo $? ${?}").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(status.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&status.stdout), "1 1\n");
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn command_lists_execute_in_order_and_update_last_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("FOO=one; echo $FOO; false; echo $?; FOO=two").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one\n1\n");
        assert_eq!(state.lookup("FOO"), Some("two"));
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn boolean_command_lists_short_circuit() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("false && echo no || echo yes; true || echo no; false && echo no")
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 1);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "yes\n");
        assert_eq!(state.last_status(), 1);
    }

    #[test]
    fn if_executes_matching_branch_and_updates_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("if false; then echo no; else echo yes; fi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "yes\n");
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn if_supports_elif_and_branch_state_changes() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "if false; then FOO=no; elif true; then FOO=elif; else FOO=else; fi; echo $FOO",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "elif\n");
        assert_eq!(state.lookup("FOO"), Some("elif"));
    }

    #[test]
    fn if_without_matching_branch_returns_zero() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("if false; then echo no; fi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
    }

    #[test]
    fn if_treats_if_and_fi_arguments_as_normal_words() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("if true; then echo if fi; fi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "if fi\n");
    }

    #[test]
    fn compound_parsers_accept_ampersand_before_reserved_closers() {
        let graph = parse_line("if true; then true & fi").unwrap();
        let invocation = &graph.list.items[0].pipeline.commands[0];
        assert!(parse_if_invocation(invocation).unwrap().is_some());

        let graph = parse_line("while false; do true & done").unwrap();
        let invocation = &graph.list.items[0].pipeline.commands[0];
        assert!(parse_while_invocation(invocation).unwrap().is_some());

        let graph = parse_line("for item in one; do true & done").unwrap();
        let invocation = &graph.list.items[0].pipeline.commands[0];
        assert!(parse_for_invocation(invocation).unwrap().is_some());

        let graph = parse_line("case x in x) true & esac").unwrap();
        let invocation = &graph.list.items[0].pipeline.commands[0];
        assert!(parse_case_invocation(invocation).unwrap().is_some());
    }

    #[test]
    fn compound_bodies_keep_unquoted_expansions_active() {
        let mut state = ShellState::from_current_process();
        state.set_var("WORD", "expanded");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("if true; then echo $WORD; fi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "expanded\n");
    }

    #[test]
    fn while_repeats_until_condition_fails_and_updates_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "COUNT=0; while [ \"$COUNT\" -lt 3 ]; do echo $COUNT; COUNT=$(expr \"$COUNT\" + 1); done",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "0\n1\n2\n");
        assert_eq!(state.lookup("COUNT"), Some("3"));
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn while_without_matching_condition_returns_zero() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("while false; do echo no; done").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
    }

    #[test]
    fn while_treats_while_and_done_arguments_as_normal_words() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "COUNT=0; while [ \"$COUNT\" -lt 1 ]; do echo while done; COUNT=1; done",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "while done\n");
    }

    #[test]
    fn until_repeats_until_condition_succeeds_and_updates_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "COUNT=0; until [ \"$COUNT\" -ge 3 ]; do echo $COUNT; COUNT=$(expr \"$COUNT\" + 1); done",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "0\n1\n2\n");
        assert_eq!(state.lookup("COUNT"), Some("3"));
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn break_stops_current_loop_body() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "for item in one two three; do echo $item; break; echo no; done; echo after",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one\nafter\n");
        assert_eq!(state.lookup("item"), Some("one"));
    }

    #[test]
    fn continue_skips_remaining_loop_body() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "for item in one two; do echo before:$item; continue; echo no; done; echo after",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "before:one\nbefore:two\nafter\n"
        );
        assert_eq!(state.lookup("item"), Some("two"));
    }

    #[test]
    fn loop_control_counts_cross_nested_loops() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "for outer in a b; do for inner in 1 2; do echo $outer:$inner; break 2; done; echo outer; done; echo after",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "a:1\nafter\n");
        assert_eq!(state.lookup("outer"), Some("a"));
        assert_eq!(state.lookup("inner"), Some("1"));
    }

    #[test]
    fn break_and_continue_outside_loop_report_errors() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        // `break` with no count outside a loop is a successful no-op (bash/sh).
        let break_outcome = executor
            .run_graph(
                &parse_line("break").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        // `continue 0` is rejected for the invalid count regardless of context.
        let continue_outcome = executor
            .run_graph(
                &parse_line("continue 0").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(break_outcome.exit_code, 0);
        assert!(break_outcome.stderr.is_empty());
        assert_eq!(continue_outcome.exit_code, 2);
        assert_eq!(
            String::from_utf8_lossy(&continue_outcome.stderr),
            "continue: loop count must be a positive number\n"
        );
    }

    #[test]
    fn for_iterates_explicit_words_and_updates_variable() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"for item in one "two words" three; do printf '<%s>\n' "$item"; done"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<one>\n<two words>\n<three>\n"
        );
        assert_eq!(state.lookup("item"), Some("three"));
    }

    #[test]
    fn for_defaults_to_positional_arguments() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line(r#"set -- one "two words""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line(r#"for item; do printf '<%s>\n' "$item"; done"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<one>\n<two words>\n"
        );
    }

    #[test]
    fn select_runs_body_for_valid_choice_and_sets_reply() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "printf '2\n' | select item in one \"two words\"; do printf '<%s:%s>\\n' \"$REPLY\" \"$item\"; break; done",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "<2:two words>\n");
        assert_eq!(state.lookup("REPLY"), Some("2"));
        assert_eq!(state.lookup("item"), Some("two words"));
    }

    #[test]
    fn select_sets_empty_variable_for_invalid_choice() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "printf '3\n' | select item in one two; do printf '<%s:%s>\\n' \"$REPLY\" \"$item\"; break; done",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "<3:>\n");
        assert_eq!(state.lookup("REPLY"), Some("3"));
        assert_eq!(state.lookup("item"), Some(""));
    }

    #[test]
    fn select_defaults_to_positional_arguments() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line(r#"set -- one "two words""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "printf '2\n' | select item; do printf '<%s>\\n' \"$item\"; break; done",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "<two words>\n");
    }

    #[test]
    fn for_expands_unquoted_item_words_and_globs() {
        let temp = unique_temp_dir("for-glob");
        std::fs::write(temp.join("one.txt"), "").unwrap();
        std::fs::write(temp.join("two.txt"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        state.set_var("WORDS", "alpha beta");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"for item in $WORDS *.txt; do printf '<%s>\n' "$item"; done"#)
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<alpha>\n<beta>\n<one.txt>\n<two.txt>\n"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn for_treats_for_and_done_arguments_as_normal_words() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("for item in one; do echo for done; done").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "for done\n");
    }

    #[test]
    fn case_matches_alternative_pattern_and_runs_body() {
        let mut state = ShellState::from_current_process();
        state.set_var("KIND", "rs");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"case "$KIND" in rust|rs) echo rust ;; py) echo python ;; esac"#)
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "rust\n");
    }

    #[test]
    fn case_uses_wildcard_fallback_and_no_match_is_success() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let fallback = executor
            .run_graph(
                &parse_line("case txt in *.rs) echo rust ;; *) echo other ;; esac").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(fallback.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&fallback.stdout), "other\n");

        let no_match = executor
            .run_graph(
                &parse_line("case txt in *.rs) echo rust ;; esac").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(no_match.exit_code, 0);
        assert!(no_match.stdout.is_empty());
    }

    #[test]
    fn case_expands_patterns_and_allows_leading_paren() {
        let mut state = ShellState::from_current_process();
        state.set_var("PATTERN", "*.rs");
        let mut executor = Executor::new();

        let expanded = executor
            .run_graph(
                &parse_line(r#"case main.rs in $PATTERN) echo expanded ;; esac"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(expanded.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&expanded.stdout), "expanded\n");

        let leading_paren = executor
            .run_graph(
                &parse_line("case one in (one) echo paren ;; esac").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(leading_paren.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&leading_paren.stdout), "paren\n");
    }

    #[test]
    fn case_treats_case_and_esac_arguments_as_normal_words() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("case x in x) echo case esac ;; esac").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "case esac\n");
    }

    #[test]
    fn case_arm_bodies_support_nested_case_blocks() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "case outer in outer) case inner in inner) echo nested ;; esac; echo after ;; esac",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "nested\nafter\n");
    }

    #[test]
    fn case_semicolon_ampersand_falls_through_to_next_body() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("case x in x) echo one ;& y) echo two ;; *) echo other ;; esac")
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one\ntwo\n");
    }

    #[test]
    fn case_semicolon_semicolon_ampersand_continues_pattern_testing() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("case x in x) echo one ;;& y) echo no ;; x) echo two ;; esac").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one\ntwo\n");
    }

    #[test]
    fn command_list_errexit_stops_after_unhandled_failure() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("set -e; false; FOO=after").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 1);
        assert!(state.errexit());
        assert_ne!(state.lookup("FOO"), Some("after"));
        assert_eq!(state.last_status(), 1);
    }

    #[test]
    fn command_list_errexit_allows_boolean_operands() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("set -e; false || echo recovered; FOO=after").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "recovered\n");
        assert_eq!(state.lookup("FOO"), Some("after"));
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn last_status_tracks_pipeline_exit_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let failed = executor
            .run_graph(
                &parse_line("true | false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(failed.exit_code, 1);
        assert_eq!(state.last_status(), 1);

        let status = executor
            .run_graph(
                &parse_line("printf '%s\n' $?").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&status.stdout), "1\n");

        let ok = executor
            .run_graph(
                &parse_line("false | true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(ok.exit_code, 0);
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn negated_pipelines_invert_status_without_changing_output() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let success = executor
            .run_graph(
                &parse_line("! false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(success.exit_code, 0);
        assert_eq!(state.last_status(), 0);

        let failure = executor
            .run_graph(
                &parse_line("! true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(failure.exit_code, 1);
        assert_eq!(state.last_status(), 1);

        let output = executor
            .run_graph(
                &parse_line("! printf %s visible").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(output.exit_code, 1);
        assert_eq!(output.stdout, b"visible");
    }

    #[test]
    fn negated_pipelines_participate_in_command_lists() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("! false && echo recovered; ! true || echo failed").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "recovered\nfailed\n"
        );
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn negated_pipeline_applies_after_pipefail_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let default_status = executor
            .run_graph(
                &parse_line("! false | true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(default_status.exit_code, 1);

        executor
            .run_graph(
                &parse_line("set -o pipefail").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let pipefailed = executor
            .run_graph(
                &parse_line("! false | true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(pipefailed.exit_code, 0);
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn pipefail_uses_rightmost_nonzero_pipeline_status_for_buffered_pipelines() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let default_status = executor
            .run_graph(
                &parse_line("false | true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(default_status.exit_code, 0);
        assert_eq!(state.last_status(), 0);

        executor
            .run_graph(
                &parse_line("set -o pipefail").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let pipefailed = executor
            .run_graph(
                &parse_line("false | true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(pipefailed.exit_code, 1);
        assert_eq!(state.last_status(), 1);
    }

    #[test]
    fn pipefail_uses_rightmost_nonzero_pipeline_status_for_streaming_external_pipelines() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let default_status = executor
            .run_graph(
                &parse_line("sh -c 'exit 7' | sh -c 'exit 0'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(default_status.exit_code, 0);
        assert_eq!(
            state.array("PIPESTATUS"),
            Some(["7".to_string(), "0".to_string()].as_slice())
        );

        executor
            .run_graph(
                &parse_line("set -o pipefail").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let pipefailed = executor
            .run_graph(
                &parse_line("sh -c 'exit 7' | sh -c 'exit 0'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(pipefailed.exit_code, 7);
        assert_eq!(state.last_status(), 7);
        assert_eq!(
            state.array("PIPESTATUS"),
            Some(["7".to_string(), "0".to_string()].as_slice())
        );
    }

    #[test]
    fn expands_command_substitution_in_unquoted_and_double_quoted_words() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let unquoted = executor
            .run_graph(
                &parse_line("echo $(printf %s hello)").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&unquoted.stdout), "hello\n");

        let double_quoted = executor
            .run_graph(
                &parse_line("echo \"a$(printf %s b)c\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&double_quoted.stdout), "abc\n");
    }

    #[test]
    fn expands_backtick_command_substitution_with_quote_rules() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let unquoted = executor
            .run_graph(
                &parse_line("echo `printf %s hi`").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&unquoted.stdout), "hi\n");

        let double_quoted = executor
            .run_graph(
                &parse_line("echo \"a`printf %s b`c\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&double_quoted.stdout), "abc\n");

        let single_quoted = executor
            .run_graph(
                &parse_line("echo '`printf %s hi`'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&single_quoted.stdout),
            "`printf %s hi`\n"
        );
    }

    #[test]
    fn single_quotes_suppress_command_substitution() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo '$(printf %s hello)'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "$(printf %s hello)\n"
        );
    }

    #[test]
    fn expands_arithmetic_substitution_with_variables() {
        let mut state = ShellState::from_current_process();
        state.set_var("COUNT", "4");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo $((COUNT * 2 + (3 - 1)))").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "10\n");
    }

    #[test]
    fn expands_simple_globs() {
        let temp = unique_temp_dir("glob");
        std::fs::write(temp.join("one.txt"), "").unwrap();
        std::fs::write(temp.join("two.log"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo *.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one.txt\n");

        let quoted = executor
            .run_graph(
                &parse_line("echo '*.txt'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&quoted.stdout), "*.txt\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn trailing_slash_globs_match_only_directories_and_keep_the_slash() {
        let temp = unique_temp_dir("directory-glob");
        std::fs::create_dir(temp.join("dir1")).unwrap();
        std::fs::create_dir(temp.join("dir2")).unwrap();
        std::fs::write(temp.join("dir-file"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo dir*/").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "dir1/ dir2/\n");
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn noglob_disables_pathname_expansion_without_disabling_field_splitting() {
        let temp = unique_temp_dir("noglob");
        std::fs::write(temp.join("one.rs"), "").unwrap();
        std::fs::write(temp.join("two.rs"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        state.set_var("PATTERNS", "*.rs literal");
        let mut executor = Executor::new();

        let expanded = executor
            .run_graph(
                &parse_line("echo *.rs").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&expanded.stdout), "one.rs two.rs\n");

        executor
            .run_graph(
                &parse_line("set -f").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.noglob());

        let literal = executor
            .run_graph(
                &parse_line("echo *.rs").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&literal.stdout), "*.rs\n");

        let split_without_glob = executor
            .run_graph(
                &parse_line("echo $PATTERNS").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&split_without_glob.stdout),
            "*.rs literal\n"
        );

        executor
            .run_graph(
                &parse_line("set +f").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.noglob());

        let reenabled = executor
            .run_graph(
                &parse_line("echo *.rs").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&reenabled.stdout),
            "one.rs two.rs\n"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn expands_globs_from_unquoted_expansions() {
        let temp = unique_temp_dir("expanded-glob");
        std::fs::write(temp.join("one.rs"), "").unwrap();
        std::fs::write(temp.join("two.rs"), "").unwrap();
        std::fs::write(temp.join("notes.txt"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        state.set_var("IFS", " \t\n");
        state.set_var("RS_PATTERN", "*.rs");
        state.set_var("PATTERNS", "*.rs *.txt");
        let mut executor = Executor::new();

        let expanded = executor
            .run_graph(
                &parse_line("echo $RS_PATTERN").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&expanded.stdout), "one.rs two.rs\n");

        let split_patterns = executor
            .run_graph(
                &parse_line("echo $PATTERNS").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&split_patterns.stdout),
            "one.rs two.rs notes.txt\n"
        );

        let quoted = executor
            .run_graph(
                &parse_line(r#"echo "$RS_PATTERN""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&quoted.stdout), "*.rs\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn expanded_glob_patterns_compose_with_literal_word_parts() {
        let temp = unique_temp_dir("expanded-glob-mixed");
        std::fs::write(temp.join("one.rs"), "").unwrap();
        std::fs::write(temp.join("two.rs"), "").unwrap();
        std::fs::write(temp.join("other.txt"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        state.set_var("STEM", "o*");
        state.set_var("NO_MATCH", "*.md");
        let mut executor = Executor::new();

        let mixed = executor
            .run_graph(
                &parse_line(r#"echo ${STEM}.rs"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&mixed.stdout), "one.rs\n");

        let no_match = executor
            .run_graph(
                &parse_line("echo $NO_MATCH").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&no_match.stdout), "*.md\n");

        let quoted_pattern_with_literal_suffix = executor
            .run_graph(
                &parse_line(r#"echo "$STEM".rs"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&quoted_pattern_with_literal_suffix.stdout),
            "o*.rs\n"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn quoted_glob_metachars_stay_literal_in_mixed_words() {
        let temp = unique_temp_dir("quoted-glob-mask");
        std::fs::write(temp.join("literal*x.rs"), "").unwrap();
        std::fs::write(temp.join("literalAx.rs"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        state.set_var("SUFFIX_PATTERN", "?.rs");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"echo "literal*"$SUFFIX_PATTERN"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "literal*x.rs\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn escaped_unquoted_glob_metachars_stay_literal_in_mixed_words() {
        let temp = unique_temp_dir("escaped-glob-mask");
        std::fs::write(temp.join("literal*x.rs"), "").unwrap();
        std::fs::write(temp.join("literalAx.rs"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        state.set_var("SUFFIX_PATTERN", "?.rs");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"echo literal\*$SUFFIX_PATTERN"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "literal*x.rs\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn escaped_unquoted_dollar_stays_literal() {
        let mut state = ShellState::from_current_process();
        state.set_var("WORD", "expanded");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"echo \$WORD $WORD"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "$WORD expanded\n");
    }

    #[test]
    fn unquoted_command_substitution_glob_patterns_expand() {
        let temp = unique_temp_dir("command-substitution-glob");
        std::fs::write(temp.join("one.rs"), "").unwrap();
        std::fs::write(temp.join("two.rs"), "").unwrap();
        std::fs::write(temp.join("notes.txt"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"echo $(printf '*.rs')"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one.rs two.rs\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn expands_simple_braces_before_globbing() {
        let temp = unique_temp_dir("brace");
        std::fs::write(temp.join("one.rs"), "").unwrap();
        std::fs::write(temp.join("one.txt"), "").unwrap();
        std::fs::write(temp.join("one.md"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo one.{rs,txt}").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one.rs one.txt\n");

        let quoted = executor
            .run_graph(
                &parse_line("echo 'one.{rs,txt}'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&quoted.stdout), "one.{rs,txt}\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn expands_character_class_globs() {
        let temp = unique_temp_dir("class-glob");
        std::fs::write(temp.join("file1.txt"), "").unwrap();
        std::fs::write(temp.join("file2.txt"), "").unwrap();
        std::fs::write(temp.join("filea.txt"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let range = executor
            .run_graph(
                &parse_line("echo file[0-9].txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&range.stdout),
            "file1.txt file2.txt\n"
        );

        let negated = executor
            .run_graph(
                &parse_line("echo file[!0-9].txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&negated.stdout), "filea.txt\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn redirects_builtin_stdout_to_file() {
        let temp = unique_temp_dir("redir");
        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo hello > out.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(outcome.stdout.is_empty());
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "hello\n"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn noclobber_blocks_normal_write_and_clobber_operator_overrides() {
        let temp = unique_temp_dir("noclobber");
        std::fs::write(temp.join("out.txt"), "old\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -C").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let blocked = executor
            .run_graph(
                &parse_line("echo new > out.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(blocked.exit_code, 1);
        assert!(String::from_utf8_lossy(&blocked.stderr).contains("exists"));
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "old\n"
        );

        let appended = executor
            .run_graph(
                &parse_line("echo append >> out.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(appended.exit_code, 0);
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "old\nappend\n"
        );

        let forced = executor
            .run_graph(
                &parse_line("echo forced >| out.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(forced.exit_code, 0);
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "forced\n"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn noclobber_applies_to_external_and_streaming_redirections() {
        let temp = unique_temp_dir("noclobber-external");
        std::fs::write(temp.join("external.txt"), "old").unwrap();
        std::fs::write(temp.join("stream.txt"), "old").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -o noclobber").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let external = executor
            .run_graph(
                &parse_line("sh -c 'printf new' > external.txt").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(external.exit_code, 1);
        assert!(String::from_utf8_lossy(&external.stderr).contains("exists"));
        assert_eq!(
            std::fs::read_to_string(temp.join("external.txt")).unwrap(),
            "old"
        );

        let streaming = executor
            .run_graph(
                &parse_line("sh -c 'printf new' | cat > stream.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(streaming.exit_code, 1);
        assert!(String::from_utf8_lossy(&streaming.stderr).contains("exists"));
        assert_eq!(
            std::fs::read_to_string(temp.join("stream.txt")).unwrap(),
            "old"
        );

        let forced_external = executor
            .run_graph(
                &parse_line("sh -c 'printf forced' >| external.txt").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(forced_external.exit_code, 0);
        assert_eq!(
            std::fs::read_to_string(temp.join("external.txt")).unwrap(),
            "forced"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn redirects_builtin_stdout_to_stderr() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo hello 1>&2").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert!(outcome.stdout.is_empty());
        assert_eq!(outcome.stderr, b"hello\n");
    }

    #[test]
    fn closes_builtin_output_descriptors() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let stdout_closed = executor
            .run_graph(
                &parse_line("echo hidden 1>&-").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(stdout_closed.stdout.is_empty());
        assert!(stdout_closed.stderr.is_empty());

        // `1>&2 2>&-` duplicates fd1 from fd2 *before* closing fd2, so output
        // still reaches the original stderr destination (matches bash/sh).
        let stderr_closed = executor
            .run_graph(
                &parse_line("echo hidden 1>&2 2>&-").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(stderr_closed.stdout.is_empty());
        assert_eq!(stderr_closed.stderr, b"hidden\n");
    }

    #[test]
    fn external_redirection_order_controls_duplication_target() {
        let temp = unique_temp_dir("redir-order");
        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let combined = executor
            .run_graph(
                &parse_line("sh -c 'printf out; printf err >&2' > both.txt 2>&1").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert!(combined.stdout.is_empty());
        assert!(combined.stderr.is_empty());
        assert_eq!(
            std::fs::read_to_string(temp.join("both.txt")).unwrap(),
            "outerr"
        );

        let split = executor
            .run_graph(
                &parse_line("sh -c 'printf out; printf err >&2' 2>&1 > out.txt").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(split.stdout, b"err");
        assert!(split.stderr.is_empty());
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "out"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn closes_external_stderr_descriptor() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("sh -c 'printf out; printf err >&2' 2>&-").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"out");
        assert!(outcome.stderr.is_empty());
    }

    #[test]
    fn pipeline_respects_stdout_to_stderr_duplication() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo hidden 1>&2 | cat").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
        assert_eq!(outcome.stderr, b"hidden\n");
    }

    #[test]
    fn pipeline_uses_last_exit_status_and_raw_bytes() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let ok = executor
            .run_graph(
                &parse_line("printf %s hello | cat").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(ok.exit_code, 0);
        assert_eq!(ok.stdout, b"hello");

        let failed = executor
            .run_graph(
                &parse_line("true | false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(failed.exit_code, 1);
    }

    #[cfg(unix)]
    #[test]
    fn external_pipeline_streams_to_early_exiting_consumers() {
        let temp = unique_temp_dir("stream-pipe");
        let bin = temp.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_executable(
            &bin.join("agsh-stream-producer"),
            "while :; do\n  if ! printf 'line\\n'; then\n    exit 0\n  fi\ndone",
        );
        write_executable(
            &bin.join("agsh-stream-consumer"),
            "IFS= read -r line\nprintf '%s\\n' \"$line\"",
        );

        let mut state = ShellState::from_current_process();
        state.export_var("PATH", bin.display().to_string());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("agsh-stream-producer | agsh-stream-consumer").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"line\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn top_level_raw_external_pipeline_does_not_buffer_final_streams() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new().with_stdout_flush(true);

        let outcome = executor
            .run_graph(
                &parse_line("sh -c 'printf child; printf error >&2' | cat").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
        assert!(outcome.stderr.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn external_pipeline_streams_into_final_read_builtin() {
        let temp = unique_temp_dir("stream-read");
        let bin = temp.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_executable(
            &bin.join("agsh-stream-producer"),
            "while :; do\n  if ! printf 'line\\n'; then\n    exit 0\n  fi\ndone",
        );

        let mut state = ShellState::from_current_process();
        state.export_var("PATH", bin.display().to_string());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("agsh-stream-producer | read LINE").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
        assert_eq!(state.lookup("LINE"), Some("line"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn external_pipeline_streams_with_final_stdout_redirection() {
        let temp = unique_temp_dir("stream-pipe-redir");
        let bin = temp.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_executable(
            &bin.join("agsh-stream-producer"),
            "while :; do\n  if ! printf 'line\\n'; then\n    exit 0\n  fi\ndone",
        );
        write_executable(
            &bin.join("agsh-stream-consumer"),
            "IFS= read -r line\nprintf '%s\\n' \"$line\"",
        );

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        state.export_var("PATH", bin.display().to_string());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("agsh-stream-producer | agsh-stream-consumer > out.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "line\n"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn external_pipeline_streams_with_input_and_stderr_redirections() {
        let temp = unique_temp_dir("stream-pipe-io-redir");
        std::fs::write(temp.join("in.txt"), "alpha\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "cat < in.txt | sh -c 'IFS= read -r line; printf \"%s\" \"$line\"; printf err >&2' 2> err.txt",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"alpha");
        assert!(outcome.stderr.is_empty());
        assert_eq!(
            std::fs::read_to_string(temp.join("err.txt")).unwrap(),
            "err"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn external_pipeline_streams_with_stderr_dup_to_stdout() {
        let temp = unique_temp_dir("stream-pipe-dup");
        let bin = temp.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_executable(
            &bin.join("agsh-stderr-producer"),
            "while :; do\n  if ! printf 'line\\n' >&2; then\n    exit 0\n  fi\ndone",
        );
        write_executable(
            &bin.join("agsh-stream-consumer"),
            "IFS= read -r line\nprintf '%s\\n' \"$line\"",
        );

        let mut state = ShellState::from_current_process();
        state.export_var("PATH", bin.display().to_string());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("agsh-stderr-producer 2>&1 | agsh-stream-consumer").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"line\n");
        assert!(outcome.stderr.is_empty());

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn external_pipeline_fd_dup_order_uses_current_targets() {
        let temp = unique_temp_dir("stream-pipe-dup-order");
        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("sh -c 'printf out; printf err >&2' 2>&1 > out.txt | cat").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"err");
        assert!(outcome.stderr.is_empty());
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "out"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn external_pipeline_stdout_dup_to_stderr_closes_pipe() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("sh -c 'printf out' 1>&2 | cat").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
        assert_eq!(outcome.stderr, b"out");
    }

    #[test]
    fn external_pipeline_stdout_close_closes_pipe() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("sh -c 'printf out' 1>&- | cat").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
        assert!(outcome.stderr.is_empty());
    }

    fn run_capture(line: &str) -> CommandOutcome {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();
        executor
            .run_graph(
                &parse_line(line).unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap()
    }

    #[test]
    fn printf_cycles_format_over_arguments() {
        let outcome = run_capture("printf '%s\\n' a b c");
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "a\nb\nc\n");
    }

    #[test]
    fn printf_supports_integer_and_padding_conversions() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("printf '%d\\n' 42").stdout),
            "42\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("printf '%03d\\n' 7").stdout),
            "007\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("printf '%x %X\\n' 255 255").stdout),
            "ff FF\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("printf '[%5s][%-5s]' hi hi").stdout),
            "[   hi][hi   ]"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("printf '100%%\\n'").stdout),
            "100%\n"
        );
    }

    #[test]
    fn printf_percent_b_decodes_escapes() {
        let outcome = run_capture("printf '%b' 'a\\tb'");
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "a\tb");
    }

    #[test]
    fn echo_handles_n_and_e_flags() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo -n hi").stdout),
            "hi"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo plain text").stdout),
            "plain text\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo -e 'a\\tb'").stdout),
            "a\tb\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo -ne 'x\\ny'").stdout),
            "x\ny"
        );
        // A non-flag leading argument stops option parsing.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo -- x").stdout),
            "-- x\n"
        );
    }

    #[test]
    fn type_reports_failure_exit_code() {
        let outcome = run_capture("type definitely_not_a_command_zzz");
        assert_eq!(outcome.exit_code, 1);
        assert!(outcome.stdout.is_empty());
    }

    #[test]
    fn test_builtin_supports_and_or_negation_and_grouping() {
        assert_eq!(run_capture("[ -n x -a -n y ]").exit_code, 0);
        assert_eq!(run_capture("[ -z x -a -n y ]").exit_code, 1);
        assert_eq!(run_capture("[ -z x -o -n y ]").exit_code, 0);
        assert_eq!(run_capture("[ ! -z x ]").exit_code, 0);
        assert_eq!(run_capture("[ ( -n x -o -z y ) -a -n z ]").exit_code, 0);
    }

    #[test]
    fn function_definition_syntax_variations() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("f(){ echo hi; }; f").stdout),
            "hi\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("f () { echo hi; }; f").stdout),
            "hi\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("greet(){ echo \"hi $1\"; }; greet bob").stdout),
            "hi bob\n"
        );
        // A subshell at command position is still not a function definition.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("(echo sub)").stdout),
            "sub\n"
        );
    }

    #[test]
    fn temporary_prefix_assignment_applies_to_read() {
        let outcome =
            run_capture("printf 'a:1\\nb:2\\n' | while IFS=: read -r k v; do echo \"$k=$v\"; done");
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "a=1\nb=2\n");
    }

    #[test]
    fn assignment_command_substitution_sets_status() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("x=$(exit 42); echo $?").stdout),
            "42\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("x=$(false) || echo handled").stdout),
            "handled\n"
        );
        // A plain assignment resets status to 0.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("false; x=5; echo $?").stdout),
            "0\n"
        );
    }

    #[test]
    fn substitutions_forward_stderr_in_expansion_order() {
        let outcome = run_capture(
            r#"value="$(printf first >&2)$(printf second >&2)"; printf '<%s>' "$value""#,
        );

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"<>");
        assert_eq!(outcome.stderr, b"firstsecond");

        let process = run_capture("cat <(printf out; printf err >&2)");
        assert_eq!(process.exit_code, 0);
        assert_eq!(process.stdout, b"out");
        assert_eq!(process.stderr, b"err");
    }

    #[test]
    fn substitution_stderr_uses_the_enclosing_pre_command_redirection() {
        let temp = unique_temp_dir("substitution-stderr-redirection");
        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let same_command = executor
            .run_graph(
                &parse_line("value=$(printf same >&2) 2>same.err").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(same_command.exit_code, 0);
        assert_eq!(same_command.stderr, b"same");
        assert_eq!(std::fs::read(temp.join("same.err")).unwrap(), b"");

        let surrounding_group = executor
            .run_graph(
                &parse_line("{ value=$(printf grouped >&2); } 2>group.err").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(surrounding_group.exit_code, 0);
        assert!(surrounding_group.stderr.is_empty());
        assert_eq!(std::fs::read(temp.join("group.err")).unwrap(), b"grouped");

        let function = executor
            .run_graph(
                &parse_line(
                    "f() { printf body >&2; }; f \"$(printf argument >&2)\" 2>function.err",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(function.exit_code, 0);
        assert_eq!(function.stderr, b"argument");
        assert_eq!(std::fs::read(temp.join("function.err")).unwrap(), b"body");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn redirections_inside_compound_and_function_bodies_stay_inside() {
        // Redirection inside a function body applies to that command only and
        // does not break the function definition.
        assert_eq!(
            String::from_utf8_lossy(
                &run_capture("f() { echo gone >/dev/null; echo kept; }; f").stdout
            ),
            "kept\n"
        );
        // Redirection inside a brace group applies to the inner command only.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("{ echo a >/dev/null; echo b; }").stdout),
            "b\n"
        );
        // Redirection inside a subshell applies to the inner command only.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("( echo gone >/dev/null; echo keep )").stdout),
            "keep\n"
        );
    }

    #[test]
    fn outer_fd_dup_preserves_nested_stdout_stderr_emission_order() {
        let body = "printf o1; printf e1 >&2; printf o2; printf e2 >&2";
        for source in [
            format!("{{ {body}; }} 2>&1"),
            format!("({body}) 2>&1"),
            format!("f() {{ {body}; }}; f 2>&1"),
        ] {
            let outcome = run_capture(&source);
            assert_eq!(outcome.exit_code, 0, "source={source:?}");
            assert_eq!(outcome.stdout, b"o1e1o2e2", "source={source:?}");
            assert!(outcome.stderr.is_empty(), "source={source:?}");
        }

        // The fd duplication must also be inherited before an external child is
        // spawned. Reconstructing two independently captured streams after the
        // child exits cannot recover this ordering.
        let external_body = "sh -c 'printf o1; printf e1 >&2; printf o2; printf e2 >&2'";
        for source in [
            format!("{{ {external_body}; }} 2>&1"),
            format!("({external_body}) 2>&1"),
            format!("f() {{ {external_body}; }}; f 2>&1"),
        ] {
            let outcome = run_capture(&source);
            assert_eq!(outcome.exit_code, 0, "source={source:?}");
            assert_eq!(outcome.stdout, b"o1e1o2e2", "source={source:?}");
            assert!(outcome.stderr.is_empty(), "source={source:?}");
        }
    }

    #[test]
    fn compound_file_dup_uses_one_live_file_description() {
        let temp = unique_temp_dir("compound-live-file");
        let body = "printf o1; sh -c 'printf e1 >&2'; printf o2; sh -c 'printf e2 >&2'";
        let cases = [
            format!("{{ {body}; }}"),
            format!("({body})"),
            format!("f() {{ {body}; }}; f"),
        ];

        for (mode_name, output_mode) in [("raw", OutputMode::Raw), ("clean", OutputMode::Clean)] {
            for (index, command) in cases.iter().enumerate() {
                let path = temp.join(format!("case-{mode_name}-{index}.out"));
                let source = format!("{command} >'{}' 2>&1", path.display());
                let mut state = ShellState::from_current_process();
                let outcome = Executor::new()
                    .run_graph(
                        &parse_line(&source).unwrap(),
                        &mut state,
                        &ExecutionOptions {
                            output_mode,
                            allow_process_replacement: false,
                        },
                    )
                    .unwrap();

                assert_eq!(outcome.exit_code, 0, "source={source:?}");
                assert!(outcome.stdout.is_empty(), "source={source:?}");
                assert!(outcome.stderr.is_empty(), "source={source:?}");
                assert_eq!(std::fs::read(&path).unwrap(), b"o1e1o2e2");
            }
        }

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn compound_live_routing_reaches_optimized_external_pipelines() {
        let temp = unique_temp_dir("compound-live-pipeline");
        let pipeline = "sh -c 'printf x; printf e >&2' | cat";
        let cases = [
            format!("{{ printf a; {pipeline}; printf b; }}"),
            format!("(printf a; {pipeline}; printf b)"),
            format!("f() {{ printf a; {pipeline}; printf b; }}; f"),
        ];

        for (mode_name, output_mode) in [("raw", OutputMode::Raw), ("clean", OutputMode::Clean)] {
            for (index, command) in cases.iter().enumerate() {
                let path = temp.join(format!("case-{mode_name}-{index}.out"));
                let source = format!("{command} >'{}' 2>&1", path.display());
                let mut state = ShellState::from_current_process();
                let outcome = Executor::new()
                    .run_graph(
                        &parse_line(&source).unwrap(),
                        &mut state,
                        &ExecutionOptions {
                            output_mode,
                            allow_process_replacement: false,
                        },
                    )
                    .unwrap();

                assert_eq!(outcome.exit_code, 0, "source={source:?}");
                assert!(outcome.stdout.is_empty(), "source={source:?}");
                assert!(outcome.stderr.is_empty(), "source={source:?}");
                let bytes = std::fs::read(&path).unwrap();
                assert!(
                    bytes == b"aexb" || bytes == b"axeb",
                    "source={source:?}, bytes={bytes:?}"
                );
            }
        }

        let merged = run_capture(&format!("{{ {pipeline}; }} 2>&1"));
        assert_eq!(merged.exit_code, 0);
        assert!(merged.stdout == b"ex" || merged.stdout == b"xe");
        assert!(merged.stderr.is_empty());

        for (index, mixed_pipeline) in [
            "printf x | sh -c 'cat; printf e >&2'",
            "sh -c 'printf \"x\\n\"; printf e >&2' | while read line; do printf \"$line\"; break; done",
            "sh -c 'printf \"x\\n\"; printf e >&2' | while read line; do printf \"$line\"; break; done | cat",
        ]
        .iter()
        .enumerate()
        {
            let path = temp.join(format!("mixed-{index}.out"));
            let source = format!("{{ {mixed_pipeline}; }} >'{}' 2>&1", path.display());
            let mut state = ShellState::from_current_process();
            let outcome = Executor::new()
                .run_graph(
                    &parse_line(&source).unwrap(),
                    &mut state,
                    &ExecutionOptions::default(),
                )
                .unwrap();
            assert_eq!(outcome.exit_code, 0, "source={source:?}");
            assert!(outcome.stdout.is_empty(), "source={source:?}");
            assert!(outcome.stderr.is_empty(), "source={source:?}");
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                bytes == b"ex" || bytes == b"xe",
                "source={source:?}, bytes={bytes:?}"
            );
        }

        let read_path = temp.join("final-read.out");
        let read_source = format!(
            "{{ sh -c 'printf \"x\\n\"; printf e >&2' | read line; printf \"$line\"; }} >'{}' 2>&1",
            read_path.display()
        );
        let mut state = ShellState::from_current_process();
        let outcome = Executor::new()
            .run_graph(
                &parse_line(&read_source).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(outcome.exit_code, 0, "source={read_source:?}");
        assert!(outcome.stdout.is_empty(), "source={read_source:?}");
        assert!(outcome.stderr.is_empty(), "source={read_source:?}");
        let bytes = std::fs::read(&read_path).unwrap();
        assert!(
            bytes == b"ex" || bytes == b"xe",
            "source={read_source:?}, bytes={bytes:?}"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn compound_live_file_honors_append_noclobber_and_inner_overrides() {
        let temp = unique_temp_dir("compound-live-file-options");
        let outer = temp.join("outer");
        let inner = temp.join("inner");
        let inner_err = temp.join("inner-err");
        std::fs::write(&outer, b"prefix-").unwrap();

        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();
        let append_source = format!(
            "{{ printf a; printf inner >'{}'; sh -c 'printf err >&2' 2>'{}'; printf b >&2; }} >>'{}' 2>&1",
            inner.display(),
            inner_err.display(),
            outer.display()
        );
        let appended = executor
            .run_graph(
                &parse_line(&append_source).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(appended.exit_code, 0);
        assert_eq!(std::fs::read(&outer).unwrap(), b"prefix-ab");
        assert_eq!(std::fs::read(&inner).unwrap(), b"inner");
        assert_eq!(std::fs::read(&inner_err).unwrap(), b"err");

        executor
            .run_graph(
                &parse_line("set -C").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let blocked_source = format!("{{ printf replaced; }} >'{}'", outer.display());
        let blocked = executor
            .run_graph(
                &parse_line(&blocked_source).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(blocked.exit_code, 1);
        assert!(String::from_utf8_lossy(&blocked.stderr).contains("exists"));
        assert_eq!(std::fs::read(&outer).unwrap(), b"prefix-ab");

        let missing = temp.join("missing-input");
        let created_first = temp.join("created-first");
        let created_second = temp.join("created-second");
        let write_then_read = format!(
            "{{ :; }} >'{}' <'{}'",
            created_first.display(),
            missing.display()
        );
        let read_then_write = format!(
            "{{ :; }} <'{}' >'{}'",
            missing.display(),
            created_second.display()
        );
        assert_eq!(
            executor
                .run_graph(
                    &parse_line(&write_then_read).unwrap(),
                    &mut state,
                    &ExecutionOptions::default(),
                )
                .unwrap()
                .exit_code,
            1
        );
        assert!(created_first.exists());
        assert_eq!(
            executor
                .run_graph(
                    &parse_line(&read_then_write).unwrap(),
                    &mut state,
                    &ExecutionOptions::default(),
                )
                .unwrap()
                .exit_code,
            1
        );
        assert!(!created_second.exists());

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn parameter_expansion_handles_spaces_and_nesting() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo ${UNSET:-a b}").stdout),
            "a b\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo \"${UNSET:-a b}\"").stdout),
            "a b\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("X=1; echo ${X:+a b c}").stdout),
            "a b c\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("V=hi; echo \"${UNSET:-${V}}\"").stdout),
            "hi\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("V=hi; echo ${UNSET:-${V}}").stdout),
            "hi\n"
        );
    }

    #[test]
    fn captures_raw_trace_and_resolves_reference() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();
        let graph = parse_line("printf 'hello\\nworld\\n'").unwrap();
        executor
            .run_graph(
                &graph,
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        let id = graph.id.to_string();
        let raw = state
            .resolve_trace(&format!("trace://{id}/stdout"))
            .expect("trace recorded");
        assert_eq!(String::from_utf8_lossy(&raw), "hello\nworld\n");
        // Raw mode does not capture, so it records no trace.
        assert!(state.trace_summaries().iter().any(|(tid, _, _)| tid == &id));
    }

    #[test]
    fn arithmetic_assignment_and_increment() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("x=5; echo $((x++)); echo $x").stdout),
            "5\n6\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("x=5; echo $((++x)); echo $x").stdout),
            "6\n6\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo $((a=5)); echo $a").stdout),
            "5\n5\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("x=2; echo $((x+=3)); echo $x").stdout),
            "5\n5\n"
        );
    }

    #[test]
    fn arithmetic_expands_dollar_references_and_wraps_overflow() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("x=10; echo $(($x+5))").stdout),
            "15\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("set -- 7; echo $(($1-1))").stdout),
            "6\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo $(( $((2+3)) * 2 ))").stdout),
            "10\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo $(( ))").stdout),
            "0\n"
        );
        // Overflow wraps (two's complement) instead of panicking.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo $(( 9223372036854775807 + 1 ))").stdout),
            "-9223372036854775808\n"
        );
    }

    #[test]
    fn quoted_metacharacters_and_comments_are_literal() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo \";\"").stdout),
            ";\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo \"|\"").stdout),
            "|\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo a \">\" b").stdout),
            "a > b\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo hi #comment").stdout),
            "hi\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("# all comment").stdout),
            ""
        );
        // Real operators still work.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo a; echo b").stdout),
            "a\nb\n"
        );
    }

    #[test]
    fn posix_helper_builtins_and_runtime_errors() {
        // `:` null builtin succeeds.
        assert_eq!(run_capture(":; echo $?").exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&run_capture("if :; then echo yes; fi").stdout),
            "yes\n"
        );
        // shift adjusts positionals and $#.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("set -- a b c; shift; echo \"$# $@\"").stdout),
            "2 b c\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("f() { shift; echo $1; }; f a b c").stdout),
            "b\n"
        );
        // A failed cd reports nonzero and continues the command list.
        assert_eq!(
            String::from_utf8_lossy(
                &run_capture("cd /no_such_dir_zzz 2>/dev/null; echo AFTER=$?").stdout
            ),
            "AFTER=1\n"
        );
        // cd updates PWD logically.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("cd /usr && echo \"$PWD\"").stdout),
            "/usr\n"
        );
        // break/continue outside a loop are successful no-ops.
        assert_eq!(run_capture("break; echo $?").exit_code, 0);
    }

    #[test]
    fn nested_braces_and_cmdsub_in_double_quotes() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo {a,b{1,2}}").stdout),
            "a b1 b2\n"
        );
        // Command substitution inside double quotes may contain its own quotes.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo \"r: $(echo \"inner\")\"").stdout),
            "r: inner\n"
        );
        // $0 is the shell name, not an empty positional.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo ${0:+set}").stdout),
            "set\n"
        );
    }

    #[test]
    fn heredocs_and_herestrings_feed_stdin() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("cat <<< hello").stdout),
            "hello\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("X=world; cat <<< \"hi $X\"").stdout),
            "hi world\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("cat <<EOF\nline1\nline2\nEOF").stdout),
            "line1\nline2\n"
        );
        // Unquoted delimiter expands the body.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("X=v; cat <<EOF\nval=$X\nEOF").stdout),
            "val=v\n"
        );
        // Quoted delimiter keeps the body literal.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("cat <<'EOF'\nliteral $X\nEOF").stdout),
            "literal $X\n"
        );
        // Here-documents have their own backslash rules: escaped expansion
        // metacharacters are literal and backslash-newline is removed.
        assert_eq!(
            String::from_utf8_lossy(
                &run_capture("X=value; cat <<EOF\n\\$X|\\\\|a\\\nb|\"q\"\nEOF").stdout
            ),
            "$X|\\|ab|\"q\"\n"
        );
        // Quote removal applies to mixed delimiters and `<<-` strips leading
        // tabs from both body and delimiter lines.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("cat <<E'O'F\n$HOME\nEOF").stdout),
            "$HOME\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("cat <<-EOF\n\tone\n\tEOF").stdout),
            "one\n"
        );
        // A command after the heredoc terminator still runs.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("cat <<EOF\nbody\nEOF\necho after").stdout),
            "body\nafter\n"
        );
    }

    #[test]
    fn heredocs_survive_compound_body_reparsing() {
        let source = "if true; then cat <<EOF\nif-body $X\nEOF\nfi; \
                      (cat <<'EOF'\nsubshell $X\nEOF\n); \
                      f() { cat <<-EOF\n\tfunction $X\n\tEOF\n}; \
                      X=value; f";
        let output = run_capture(source);
        assert_eq!(output.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "if-body \nsubshell $X\nfunction value\n"
        );
    }

    #[test]
    fn compound_heredoc_command_substitution_runs_once() {
        let temp = unique_temp_dir("compound-heredoc-once");
        let count = temp.join("count");
        let source = format!(
            "if true; then cat <<EOF\n$(printf x >> '{}'; printf body)\nEOF\nfi; n=$(cat '{}'); printf 'count=%s\\n' \"$n\"",
            count.display(),
            count.display()
        );
        let output = run_capture(&source);
        let _ = std::fs::remove_dir_all(temp);
        assert_eq!(output.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&output.stdout), "body\ncount=x\n");
    }

    #[test]
    fn parameter_operator_words_honor_quote_removal_and_field_protection() {
        let output = run_capture(
            "unset U; X='x y'; \
             set -- ${U:-\"a b\"}; printf 'default:%s:<%s>\\n' \"$#\" \"$1\"; \
             set -- ${U:-'a b $X'}; printf 'single:%s:<%s>\\n' \"$#\" \"$1\"; \
             set -- ${U:-a\\ b}; printf 'escaped:%s:<%s>\\n' \"$#\" \"$1\"; \
             set -- p${U:-\"a b\"}s; printf 'mixed:%s:<%s>\\n' \"$#\" \"$1\"; \
             set -- ${U:-\"$X\"}; printf 'nested:%s:<%s>\\n' \"$#\" \"$1\"",
        );
        assert_eq!(output.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "default:1:<a b>\n\
             single:1:<a b $X>\n\
             escaped:1:<a b>\n\
             mixed:1:<pa bs>\n\
             nested:1:<x y>\n"
        );
    }

    #[test]
    fn parameter_pattern_removal_honors_quoted_metacharacters() {
        let output = run_capture(
            "X='a*b'; P='a*'; \
             printf 'single=<%s> double=<%s> escaped=<%s> variable=<%s> active=<%s>\\n' \
             \"${X#'a*'}\" \"${X#\"a*\"}\" \"${X#a\\*}\" \"${X#\"$P\"}\" \"${X#$P}\"",
        );
        assert_eq!(output.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "single=<b> double=<b> escaped=<b> variable=<b> active=<*b>\n"
        );
    }

    #[test]
    fn parameter_assignment_and_alternate_words_honor_quotes() {
        let output = run_capture(
            "unset A; X='x y'; \
             set -- ${A:=\"$X\"}; printf 'assign:%s:<%s>:<%s>\\n' \"$#\" \"$1\" \"$A\"; \
             set -- ${A:+\"alternate value\"}; printf 'alt:%s:<%s>\\n' \"$#\" \"$1\"",
        );
        assert_eq!(output.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "assign:1:<x y>:<x y>\nalt:1:<alternate value>\n"
        );
    }

    #[test]
    fn parameter_operator_words_preserve_quoted_positional_fields() {
        let output = run_capture(
            "set -- a 'b c'; unset U; \
             f() { printf 'n=%s' \"$#\"; printf ':<%s>' \"$@\"; echo; }; \
             f ${U:-\"$@\"}; f \"${U:-$@}\"; \
             IFS=:; printf 'star=<%s>\\n' \"${U:-pre$*post}\"; \
             set --; f ${U:-\"$@\"}; f \"${U:-$@}\"; f pre${U:-\"$@\"}post",
        );
        assert_eq!(output.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "n=2:<a>:<b c>\n\
             n=2:<a>:<b c>\n\
             star=<prea:b cpost>\n\
             n=0:<>\n\
             n=0:<>\n\
             n=1:<prepost>\n"
        );
    }

    #[test]
    fn newlines_separate_commands() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo a\necho b").stdout),
            "a\nb\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("if true\nthen\necho yes\nfi").stdout),
            "yes\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("for i in 1 2 3\ndo\necho $i\ndone").stdout),
            "1\n2\n3\n"
        );
        // A newline after a list operator is a continuation, not a separator.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("true &&\necho ok").stdout),
            "ok\n"
        );
    }

    #[test]
    fn subshell_isolates_state_and_carries_status() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("(echo a; echo b)").stdout),
            "a\nb\n"
        );
        // Variable changes inside a subshell do not leak out.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("X=1; (X=2); echo $X").stdout),
            "1\n"
        );
        // The subshell's exit status propagates.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("(exit 5); echo $?").stdout),
            "5\n"
        );
        // A subshell can be a pipeline producer.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("(echo b; echo a) | sort").stdout),
            "a\nb\n"
        );
    }

    #[test]
    fn brace_group_runs_in_current_state() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("{ echo a; echo b; }").stdout),
            "a\nb\n"
        );
        // Brace groups do not create a subshell: assignments persist.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("{ X=5; }; echo $X").stdout),
            "5\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("{ echo b; echo a; } | sort").stdout),
            "a\nb\n"
        );
    }

    #[test]
    fn return_builtin_stops_function_with_status() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("f() { return 3; }; f; echo $?").stdout),
            "3\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("f() { echo a; return; echo b; }; f").stdout),
            "a\n"
        );
        assert_eq!(
            String::from_utf8_lossy(
                &run_capture("f() { for i in 1 2 3; do [ $i = 2 ] && return; echo $i; done; }; f")
                    .stdout
            ),
            "1\n"
        );
        // Nested functions consume their own return without leaking.
        assert_eq!(
            String::from_utf8_lossy(
                &run_capture(
                    "inner() { return 2; }; outer() { inner; echo a=$?; return 9; }; outer; echo b=$?"
                )
                .stdout
            ),
            "a=2\nb=9\n"
        );
    }

    #[test]
    fn local_variables_are_function_scoped() {
        assert_eq!(
            String::from_utf8_lossy(
                &run_capture("f() { local x=5; echo $x; }; f; echo \"[${x}]\"").stdout
            ),
            "5\n[]\n"
        );
        assert_eq!(
            String::from_utf8_lossy(
                &run_capture("x=g; f() { local x=l; echo $x; }; f; echo $x").stdout
            ),
            "l\ng\n"
        );
        // Non-local assignments still mutate the global binding.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("x=1; f() { x=2; }; f; echo $x").stdout),
            "2\n"
        );
    }

    #[test]
    fn special_builtin_prefix_assignment_persists_and_is_exported() {
        let outcome = run_capture(
            "unset AGSH_SPECIAL_PREFIX; AGSH_SPECIAL_PREFIX=value :; \
             printf '%s|' \"$AGSH_SPECIAL_PREFIX\"; \
             sh -c 'printf %s \"$AGSH_SPECIAL_PREFIX\"'",
        );
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"value|value");
    }

    #[test]
    fn regular_builtin_prefix_restores_export_and_array_bindings() {
        let outcome = run_capture(
            "export AGSH_TEMP_BINDING=outer; a=(global keep); \
             AGSH_TEMP_BINDING=temp a=changed true; \
             printf '%s|' \"$AGSH_TEMP_BINDING\"; \
             sh -c 'printf %s \"$AGSH_TEMP_BINDING\"'; \
             printf '|%s' \"${a[*]}\"",
        );
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"outer|outer|global keep");
    }

    #[test]
    fn function_prefix_assignment_is_temporary_and_exported_inside() {
        let outcome = run_capture(
            "AGSH_FUNCTION_PREFIX=outer; \
             f() { sh -c 'printf %s \"$AGSH_FUNCTION_PREFIX\"'; }; \
             AGSH_FUNCTION_PREFIX=inner f; \
             printf '|%s' \"$AGSH_FUNCTION_PREFIX\"",
        );
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"inner|outer");
    }

    #[test]
    fn local_restores_exported_scalar_and_array_bindings() {
        let outcome = run_capture(
            "export AGSH_LOCAL_EXPORT=outer; arr=(global keep); \
             f() { local AGSH_LOCAL_EXPORT=inner; local -a arr=(local values); \
                   sh -c 'printf %s \"$AGSH_LOCAL_EXPORT\"'; \
                   printf '|%s|' \"${arr[*]}\"; }; \
             f; sh -c 'printf %s \"$AGSH_LOCAL_EXPORT\"'; \
             printf '|%s' \"${arr[*]}\"",
        );
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"inner|local values|outer|global keep");
    }

    #[test]
    fn export_of_unset_name_does_not_invent_empty_environment_value() {
        let outcome = run_capture(
            "unset AGSH_EXPORTED_UNSET; export AGSH_EXPORTED_UNSET; \
             sh -c 'printf %s \"${AGSH_EXPORTED_UNSET-unset}\"'; \
             AGSH_EXPORTED_UNSET=now_set; \
             sh -c 'printf \"|%s\" \"$AGSH_EXPORTED_UNSET\"'",
        );
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"unset|now_set");
    }

    #[test]
    fn declare_is_function_local_by_default_and_integer_attribute_persists() {
        let outcome = run_capture(
            "f() { declare scoped=inside; declare -g global_decl=outside; \
                   declare -i number; number=2+3; printf '%s:%s' \"$scoped\" \"$number\"; }; \
             f; printf '|%s:%s' \"${scoped-unset}\" \"$global_decl\"",
        );
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"inside:5|unset:outside");
    }

    #[test]
    fn readonly_reassignment_and_unset_report_failure_without_mutation() {
        let outcome = run_capture(
            "readonly AGSH_READONLY_STATUS=one; \
             readonly AGSH_READONLY_STATUS=two 2>/dev/null; printf '%s:%s|' $? \"$AGSH_READONLY_STATUS\"; \
             unset AGSH_READONLY_STATUS 2>/dev/null; printf '%s:%s' $? \"$AGSH_READONLY_STATUS\"",
        );
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"1:one|1:one");
    }

    #[test]
    fn variable_builtins_reject_invalid_names_and_options() {
        let outcome = run_capture(
            "export 1BAD=x 2>/dev/null; printf '%s|' $?; \
             readonly 2BAD=x 2>/dev/null; printf '%s|' $?; \
             unset -z 2>/dev/null; printf '%s|' $?; \
             declare -Z bad 2>/dev/null; printf '%s' $?",
        );
        assert_eq!(outcome.stdout, b"1|1|2|2");
    }

    #[test]
    fn export_n_retains_value_but_removes_child_environment_entry() {
        let outcome = run_capture(
            "export AGSH_UNEXPORT=value; readonly AGSH_UNEXPORT; export -n AGSH_UNEXPORT; \
             printf '%s|' \"$AGSH_UNEXPORT\"; \
             sh -c 'printf %s \"${AGSH_UNEXPORT-unset}\"'",
        );
        assert_eq!(outcome.stdout, b"value|unset");
    }

    #[test]
    fn unset_removes_arrays_and_scalar_assignment_updates_array_element_zero() {
        let outcome = run_capture(
            "a=(one two); a=changed; printf '%s|' \"${a[*]}\"; \
             unset a; printf '%s' \"${a-unset}\"",
        );
        assert_eq!(outcome.stdout, b"changed two|unset");
    }

    #[test]
    fn readonly_is_enforced_in_arithmetic_and_read() {
        let outcome = run_capture(
            "readonly r=1; ((r=2)) 2>/dev/null; printf '%s:%s|' $? \"$r\"; \
             printf 'new\\n' | read r 2>/dev/null; printf '%s:%s' $? \"$r\"",
        );
        assert_eq!(outcome.stdout, b"1:1|1:1");
    }

    #[test]
    fn readonly_is_enforced_for_loop_variables() {
        let outcome = run_capture(
            "readonly item=old; for item in new; do :; done 2>/dev/null; \
             printf '%s:%s' $? \"$item\"",
        );
        assert_eq!(outcome.stdout, b"1:old");
    }

    #[test]
    fn command_not_found_returns_127_and_continues() {
        let outcome = run_capture("definitely_missing_zzz; echo after");
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "after\n");

        let status = run_capture("definitely_missing_zzz 2>/dev/null; echo $?");
        assert_eq!(String::from_utf8_lossy(&status.stdout), "127\n");
        assert!(
            status.stderr.is_empty(),
            "missing-command diagnostic bypassed stderr redirection: {:?}",
            String::from_utf8_lossy(&status.stderr)
        );

        // A missing command inside a function yields 127, not a hard abort.
        let in_fn = run_capture("f() { missing_inner_zzz; }; f; echo $?");
        assert_eq!(String::from_utf8_lossy(&in_fn.stdout), "127\n");
    }

    #[test]
    fn arithmetic_supports_full_operator_set() {
        let cases = [
            ("$((3 > 2))", "1"),
            ("$((3 < 2))", "0"),
            ("$((5 == 5))", "1"),
            ("$((5 != 4))", "1"),
            ("$((6 & 3))", "2"),
            ("$((6 | 1))", "7"),
            ("$((6 ^ 3))", "5"),
            ("$((1 << 4))", "16"),
            ("$((256 >> 2))", "64"),
            ("$((1 ? 7 : 8))", "7"),
            ("$((0 ? 7 : 8))", "8"),
            ("$((1 && 0))", "0"),
            ("$((0 || 3))", "1"),
            ("$((!0))", "1"),
            ("$((~5))", "-6"),
            ("$((2 ** 10))", "1024"),
            ("$((0xff))", "255"),
            ("$((010))", "8"),
            ("$(( (5 > 3) && (2 < 4) ))", "1"),
        ];
        for (expr, expected) in cases {
            let outcome = run_capture(&format!("echo {expr}"));
            assert_eq!(
                String::from_utf8_lossy(&outcome.stdout).trim(),
                expected,
                "expr {expr}"
            );
        }
    }

    #[test]
    fn builtin_redirection_orders_dup_after_file() {
        let temp = unique_temp_dir("builtin-redir");
        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();
        let outcome = executor
            .run_graph(
                &parse_line("type missing_cmd_zzz >out.txt 2>&1").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert!(outcome.stdout.is_empty());
        assert!(outcome.stderr.is_empty());
        let written = std::fs::read_to_string(temp.join("out.txt")).unwrap();
        assert!(
            written.contains("not found"),
            "expected error in file: {written}"
        );
    }

    #[test]
    fn async_graph_detection_uses_parsed_operators_not_source_text() {
        assert!(!graph_contains_async_list(
            &parse_line("printf '%s' '&'").unwrap()
        ));
        assert!(graph_contains_async_list(
            &parse_line("{ printf x & wait; }").unwrap()
        ));
        assert!(!graph_contains_async_list(
            &parse_line("cat <<EOF\n& '\nEOF").unwrap()
        ));
        assert!(graph_contains_async_list(
            &parse_line("{ printf x & wait; cat <<EOF\n'\nEOF\n}").unwrap()
        ));
        for source in [
            "case x in x) printf first ;& y) printf second ;; esac",
            "case x in x) printf first ;;& y) printf second ;; esac",
        ] {
            assert!(
                !graph_contains_async_list(&parse_line(source).unwrap()),
                "case fallthrough was mistaken for async: {source}"
            );
        }
        assert!(graph_contains_async_list(
            &parse_line("case x in x) printf async & wait ;; esac").unwrap()
        ));
    }

    #[test]
    fn rich_mode_uses_raw_passthrough_when_stdout_is_not_a_terminal() {
        assert!(rich_mode_requires_raw_passthrough_with_terminal(
            OutputMode::Rich,
            false
        ));
        assert!(!rich_mode_requires_raw_passthrough_with_terminal(
            OutputMode::Rich,
            true
        ));
        for mode in [
            OutputMode::Raw,
            OutputMode::Clean,
            OutputMode::Compact,
            OutputMode::Semantic,
            OutputMode::LosslessRef,
            OutputMode::Silent,
        ] {
            assert!(!rich_mode_requires_raw_passthrough_with_terminal(
                mode, false
            ));
        }
    }

    #[test]
    fn rich_display_surfaces_incomplete_capture_status() {
        let raw = RawStreamRef::persisted(
            "/tmp/out",
            "/tmp/err",
            agsh_output::RawTraceStatus::Truncated,
            agsh_output::RawTraceStatus::Complete,
            4096,
        );
        let observation = finish_rich_observation("rendered\n".to_string(), &raw);

        assert!(observation.display.starts_with("rendered\n"));
        assert!(observation.display.contains("raw_trace: incomplete"));
        assert!(observation
            .display
            .contains("stdout=truncated, stderr=complete"));
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("agsh-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}
