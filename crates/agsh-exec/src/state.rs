use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use agsh_core::{CommandId, ShellError, Value};
use agsh_index::{GitContext, PathCache};
use agsh_output::{CompactorConfig, OutputMode, RawStorageOptions, RawStreamRef, RawTraceStatus};
use agsh_store::history::{
    self, command_family, HistoryEntry, HistoryMatch, HistoryQuery, HistoryStats,
};
use agsh_store::{parse_trace_ref, HistoryStore, TraceRecord, TraceStore, TraceStream};
use agsh_style::{Role, Theme};
use serde::{Deserialize, Serialize};

/// Max graph-execution nesting before we error instead of overflowing the stack.
/// Generous for legitimate recursion/nesting yet well below the point where the
/// heavier per-frame executor stack would abort the process. See
/// [`ShellState::enter_exec`].
const MAX_EXEC_DEPTH: usize = 128;
pub const BACKGROUND_SNAPSHOT_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const BACKGROUND_SNAPSHOT_READY: u8 = 0xa7;
const MAX_READ_LINE_BYTES: usize = 1024 * 1024;

/// Cache of PATH executable names, keyed by the `$PATH` value it was built from.
type CommandNameCache = Arc<Mutex<Option<(String, std::collections::HashSet<String>)>>>;

/// An activated project `.env`: its directory and one complete snapshot for
/// each unique binding it touched, so leaving restores values and attributes.
type ActiveEnv = Option<(PathBuf, Vec<VariableSnapshot>)>;

/// Decode a bounded state handoff used by an asynchronous child shell. The
/// reader is normally one half of a private Unix socket pair; no pathname or
/// persistent secret-bearing file is involved.
pub fn restore_background_snapshot_reader(
    state: &mut ShellState,
    reader: impl Read,
) -> io::Result<()> {
    let mut bytes = Vec::new();
    let mut limited = reader.take(BACKGROUND_SNAPSHOT_MAX_BYTES as u64 + 1);
    limited.read_to_end(&mut bytes)?;
    if bytes.len() > BACKGROUND_SNAPSHOT_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background state exceeds its size limit",
        ));
    }
    state.restore_background_snapshot(&bytes)
}

/// Consume a background snapshot from stdin and acknowledge successful decode
/// on that same full-duplex descriptor. Once the parent receives the byte and
/// closes its socket half, commands launched by this shell observe stdin EOF,
/// matching a normal asynchronous shell's `/dev/null` input behavior.
pub fn restore_background_snapshot_stdin(state: &mut ShellState) -> io::Result<()> {
    use std::os::fd::AsFd;

    let stdin = io::stdin();
    let mut locked = stdin.lock();
    restore_background_snapshot_reader(state, &mut locked)?;
    drop(locked);
    let acknowledgement = rustix::io::dup(stdin.as_fd())
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    let null = std::fs::File::open("/dev/null")?;
    rustix::stdio::dup2_stdin(&null)
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    let written = rustix::io::write(&acknowledgement, &[BACKGROUND_SNAPSHOT_READY])
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    if written != 1 {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "could not acknowledge background state",
        ));
    }
    Ok(())
}

/// An exact captured stream fragment held on private disk. Temporary fragments
/// are deleted unless they are consumed into a command's durable trace files.
#[derive(Debug)]
pub(crate) struct ExactTraceFile {
    path: Option<PathBuf>,
    status: RawTraceStatus,
    total_bytes: u64,
    stored_bytes: u64,
}

impl ExactTraceFile {
    pub(crate) fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("stored trace fragment has no file path")
    }

    pub(crate) const fn is_complete(&self) -> bool {
        matches!(self.status, RawTraceStatus::Complete)
    }
}

impl Drop for ExactTraceFile {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Exact output can be assembled from bounded in-memory fragments and private
/// spool files without ever loading a large captured stream back into memory.
#[derive(Debug)]
pub(crate) enum ExactTraceSegment {
    Memory(Vec<u8>),
    File(ExactTraceFile),
}

impl ExactTraceSegment {
    /// Whether this segment contains every byte that passed through its source.
    /// Semantic consumers such as process substitution must reject incomplete
    /// segments instead of copying a persisted prefix as though it were exact.
    pub(crate) fn is_complete(&self) -> bool {
        match self {
            Self::Memory(_) => true,
            Self::File(file) => file.is_complete(),
        }
    }
}

pub(crate) struct CapturedTraceStreams<'a> {
    pub(crate) stdout_preview: &'a [u8],
    pub(crate) stderr_preview: &'a [u8],
    /// Whether the preview itself contains every byte when no exact segments are
    /// supplied. Bounded previews after spool failure must set this false.
    pub(crate) stdout_preview_complete: bool,
    pub(crate) stderr_preview_complete: bool,
    pub(crate) stdout_exact: Option<Vec<ExactTraceSegment>>,
    pub(crate) stderr_exact: Option<Vec<ExactTraceSegment>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTrace {
    pub bytes: Vec<u8>,
    pub status: RawTraceStatus,
}

#[derive(Debug)]
pub struct TraceReader {
    inner: TraceReaderInner,
    pub status: RawTraceStatus,
}

const MAX_ALLOCATED_TRACE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
enum TraceReaderInner {
    File(std::fs::File),
    Memory(io::Cursor<Vec<u8>>),
}

impl Read for TraceReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match &mut self.inner {
            TraceReaderInner::File(file) => file.read(buffer),
            TraceReaderInner::Memory(cursor) => cursor.read(buffer),
        }
    }
}

#[derive(Debug)]
pub(crate) struct TraceSpoolWriter {
    path: Option<PathBuf>,
    file: Option<std::fs::File>,
    keep: bool,
    budget: Arc<TraceCaptureBudget>,
    incomplete: TraceSpoolIncompleteMarker,
    total_bytes: u64,
    stored_bytes: u64,
}

/// Shared one-way signal for capture readers that intentionally stop before
/// kernel EOF. The writer may already have moved to another thread, so the
/// cutoff cannot be represented reliably by a mutable writer method alone.
#[derive(Debug, Clone)]
pub(crate) struct TraceSpoolIncompleteMarker(Arc<AtomicBool>);

impl TraceSpoolIncompleteMarker {
    pub(crate) fn mark_incomplete(&self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Debug)]
struct TraceCaptureBudget {
    enabled: bool,
    remaining: AtomicU64,
}

impl TraceCaptureBudget {
    fn new(options: RawStorageOptions) -> Self {
        Self {
            enabled: options.enabled,
            remaining: AtomicU64::new(options.max_bytes),
        }
    }

    fn reserve(&self, requested: usize) -> usize {
        if !self.enabled || requested == 0 {
            return 0;
        }
        let requested = u64::try_from(requested).unwrap_or(u64::MAX);
        let mut remaining = self.remaining.load(Ordering::Relaxed);
        loop {
            let reserved = requested.min(remaining);
            match self.remaining.compare_exchange_weak(
                remaining,
                remaining - reserved,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return usize::try_from(reserved).unwrap_or(usize::MAX),
                Err(actual) => remaining = actual,
            }
        }
    }
}

impl TraceSpoolWriter {
    pub(crate) fn incomplete_marker(&self) -> TraceSpoolIncompleteMarker {
        self.incomplete.clone()
    }

    pub(crate) fn finish(mut self) -> io::Result<ExactTraceFile> {
        if let Some(file) = self.file.take() {
            file.sync_all()?;
            drop(file);
        }
        self.keep = true;
        let status = if !self.budget.enabled {
            RawTraceStatus::Disabled
        } else if !self.incomplete.0.load(Ordering::Acquire)
            && self.total_bytes == self.stored_bytes
        {
            RawTraceStatus::Complete
        } else {
            RawTraceStatus::Truncated
        };
        Ok(ExactTraceFile {
            path: self.path.clone(),
            status,
            total_bytes: self.total_bytes,
            stored_bytes: self.stored_bytes,
        })
    }
}

impl Write for TraceSpoolWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.total_bytes = self
            .total_bytes
            .saturating_add(u64::try_from(buf.len()).unwrap_or(u64::MAX));
        let reserved = self.budget.reserve(buf.len());
        if reserved > 0 {
            let result = self
                .file
                .as_mut()
                .expect("enabled trace spool has no file")
                .write_all(&buf[..reserved]);
            result?;
            self.stored_bytes = self
                .stored_bytes
                .saturating_add(u64::try_from(reserved).unwrap_or(u64::MAX));
        }
        // Bytes beyond the persisted quota are deliberately consumed so the
        // capture reader continues draining the child to EOF.
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

impl Drop for TraceSpoolWriter {
    fn drop(&mut self) {
        if !self.keep {
            if let Some(path) = &self.path {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Current Unix time in whole seconds (0 if the clock is before the epoch).
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellFunction {
    pub body: String,
}

impl ShellFunction {
    pub fn new(body: impl Into<String>) -> Self {
        Self { body: body.into() }
    }
}

const BACKGROUND_SNAPSHOT_VERSION: u8 = 2;
const MAX_BACKGROUND_SNAPSHOT_COLLECTION_ENTRIES: usize = 16_384;
const MAX_BACKGROUND_SNAPSHOT_TOTAL_ENTRIES: usize = 65_536;
const MAX_BACKGROUND_SNAPSHOT_NAME_BYTES: usize = 4 * 1024;
const MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES: usize = 4 * 1024 * 1024;
const MAX_BACKGROUND_SNAPSHOT_VALUE_DEPTH: usize = 64;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackgroundSnapshot {
    version: u8,
    arg0: String,
    vars: BTreeMap<String, String>,
    exported_vars: Vec<String>,
    aliases: BTreeMap<String, String>,
    abbreviations: BTreeMap<String, String>,
    functions: BTreeMap<String, String>,
    arrays: BTreeMap<String, Vec<String>>,
    assoc_arrays: BTreeMap<String, BTreeMap<String, String>>,
    readonly_vars: Vec<String>,
    integer_vars: Vec<String>,
    allexport: bool,
    errexit: bool,
    nounset: bool,
    noclobber: bool,
    noglob: bool,
    pipefail: bool,
    xtrace: bool,
    shopt: BTreeMap<String, bool>,
    last_status: i32,
    default_output_mode: Option<String>,
    active_env: Option<BackgroundActiveEnv>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackgroundActiveEnv {
    directory: Vec<u8>,
    saved: Vec<BackgroundVariableSnapshot>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackgroundVariableSnapshot {
    name: String,
    prior_var: Option<String>,
    prior_value: Option<BackgroundValue>,
    prior_env: Option<String>,
    prior_opaque_env: Option<Vec<u8>>,
    prior_array: Option<Vec<String>>,
    prior_assoc: Option<BTreeMap<String, String>>,
    was_exported: bool,
    was_readonly: bool,
    was_integer: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
enum BackgroundValue {
    Null,
    Bool(bool),
    Int(i64),
    FloatBits(u64),
    String(String),
    Bytes(Vec<u8>),
    Path(Vec<u8>),
    List(Vec<BackgroundValue>),
    Record(BTreeMap<String, BackgroundValue>),
}

impl BackgroundValue {
    fn from_value(
        value: &Value,
        limits: &mut BackgroundSnapshotLimits,
        depth: usize,
    ) -> io::Result<Self> {
        if depth > MAX_BACKGROUND_SNAPSHOT_VALUE_DEPTH {
            return Err(invalid_background_snapshot(
                "background state value exceeds its nesting limit",
            ));
        }
        limits.add_entries(1)?;
        Ok(match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(*value),
            Value::Int(value) => Self::Int(*value),
            Value::Float(value) => Self::FloatBits(value.to_bits()),
            Value::String(value) => {
                limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
                Self::String(value.clone())
            }
            Value::Bytes(value) => {
                limits.add_bytes(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
                Self::Bytes(value.clone())
            }
            Value::Path(value) => {
                limits.add_path(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
                Self::Path(path_bytes(value))
            }
            Value::List(values) => {
                limits.check_collection(values.len())?;
                Self::List(
                    values
                        .iter()
                        .map(|value| Self::from_value(value, limits, depth + 1))
                        .collect::<io::Result<_>>()?,
                )
            }
            Value::Record(values) => {
                limits.check_collection(values.len())?;
                Self::Record(
                    values
                        .iter()
                        .map(|(name, value)| {
                            limits.add_string(name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
                            Ok((name.clone(), Self::from_value(value, limits, depth + 1)?))
                        })
                        .collect::<io::Result<_>>()?,
                )
            }
        })
    }

    fn into_value(self) -> io::Result<Value> {
        Ok(match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Int(value) => Value::Int(value),
            Self::FloatBits(value) => Value::Float(f64::from_bits(value)),
            Self::String(value) => Value::String(value),
            Self::Bytes(value) => Value::Bytes(value),
            Self::Path(value) => Value::Path(path_from_bytes(value)?),
            Self::List(values) => Value::List(
                values
                    .into_iter()
                    .map(Self::into_value)
                    .collect::<io::Result<_>>()?,
            ),
            Self::Record(values) => Value::Record(
                values
                    .into_iter()
                    .map(|(name, value)| Ok((name, value.into_value()?)))
                    .collect::<io::Result<_>>()?,
            ),
        })
    }

    fn validate(&self, limits: &mut BackgroundSnapshotLimits, depth: usize) -> io::Result<()> {
        if depth > MAX_BACKGROUND_SNAPSHOT_VALUE_DEPTH {
            return Err(invalid_background_snapshot(
                "background state value exceeds its nesting limit",
            ));
        }
        limits.add_entries(1)?;
        match self {
            Self::Null | Self::Bool(_) | Self::Int(_) | Self::FloatBits(_) => Ok(()),
            Self::String(value) => limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES),
            Self::Bytes(value) | Self::Path(value) => {
                limits.add_bytes(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)
            }
            Self::List(values) => {
                limits.check_collection(values.len())?;
                for value in values {
                    value.validate(limits, depth + 1)?;
                }
                Ok(())
            }
            Self::Record(values) => {
                limits.check_collection(values.len())?;
                for (name, value) in values {
                    limits.add_string(name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
                    value.validate(limits, depth + 1)?;
                }
                Ok(())
            }
        }
    }
}

impl BackgroundVariableSnapshot {
    fn from_snapshot(
        snapshot: &VariableSnapshot,
        limits: &mut BackgroundSnapshotLimits,
    ) -> io::Result<Self> {
        limits.add_string(&snapshot.name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
        if let Some(value) = &snapshot.prior_var {
            limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
        }
        let prior_value = snapshot
            .prior_value
            .as_ref()
            .map(|value| BackgroundValue::from_value(value, limits, 0))
            .transpose()?;
        if let Some(value) = &snapshot.prior_env {
            limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
        }
        if let Some(value) = &snapshot.prior_opaque_env {
            limits.add_os_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
        }
        if let Some(values) = &snapshot.prior_array {
            limits.add_collection(values.len())?;
            for value in values {
                limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
            }
        }
        if let Some(values) = &snapshot.prior_assoc {
            limits.add_collection(values.len())?;
            for (key, value) in values {
                limits.add_string(key, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
                limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
            }
        }
        if snapshot.prior_array.is_some() && snapshot.prior_assoc.is_some() {
            return Err(invalid_background_snapshot(
                "background state contains conflicting saved variable bindings",
            ));
        }
        if snapshot.prior_env.is_some() && snapshot.prior_opaque_env.is_some() {
            return Err(invalid_background_snapshot(
                "background state contains conflicting saved environment bindings",
            ));
        }

        Ok(Self {
            name: snapshot.name.clone(),
            prior_var: snapshot.prior_var.clone(),
            prior_value,
            prior_env: snapshot.prior_env.clone(),
            prior_opaque_env: snapshot.prior_opaque_env.as_ref().map(os_string_bytes),
            prior_array: snapshot.prior_array.clone(),
            prior_assoc: snapshot.prior_assoc.clone(),
            was_exported: snapshot.was_exported,
            was_readonly: snapshot.was_readonly,
            was_integer: snapshot.was_integer,
        })
    }

    fn into_snapshot(self) -> io::Result<VariableSnapshot> {
        Ok(VariableSnapshot {
            name: self.name,
            prior_var: self.prior_var,
            prior_value: self
                .prior_value
                .map(BackgroundValue::into_value)
                .transpose()?,
            prior_env: self.prior_env,
            prior_opaque_env: self
                .prior_opaque_env
                .map(os_string_from_bytes)
                .transpose()?,
            prior_array: self.prior_array,
            prior_assoc: self.prior_assoc,
            was_exported: self.was_exported,
            was_readonly: self.was_readonly,
            was_integer: self.was_integer,
        })
    }
}

impl BackgroundActiveEnv {
    fn from_active(
        directory: &Path,
        saved: &[VariableSnapshot],
        limits: &mut BackgroundSnapshotLimits,
    ) -> io::Result<Self> {
        limits.add_collection(saved.len())?;
        limits.add_path(directory, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
        let mut names = HashSet::with_capacity(saved.len());
        if saved
            .iter()
            .any(|snapshot| !names.insert(snapshot.name.as_str()))
        {
            return Err(invalid_background_snapshot(
                "background state contains duplicate project environment bindings",
            ));
        }
        Ok(Self {
            directory: path_bytes(directory),
            saved: saved
                .iter()
                .map(|snapshot| BackgroundVariableSnapshot::from_snapshot(snapshot, limits))
                .collect::<io::Result<_>>()?,
        })
    }

    fn into_active(self) -> io::Result<(PathBuf, Vec<VariableSnapshot>)> {
        Ok((
            path_from_bytes(self.directory)?,
            self.saved
                .into_iter()
                .map(BackgroundVariableSnapshot::into_snapshot)
                .collect::<io::Result<_>>()?,
        ))
    }
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> io::Result<PathBuf> {
    String::from_utf8(bytes)
        .map(PathBuf::from)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(unix)]
fn os_string_bytes(value: &OsString) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_string_bytes(value: &OsString) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

#[cfg(unix)]
fn os_string_from_bytes(bytes: Vec<u8>) -> io::Result<OsString> {
    use std::os::unix::ffi::OsStringExt;
    Ok(OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn os_string_from_bytes(bytes: Vec<u8>) -> io::Result<OsString> {
    String::from_utf8(bytes)
        .map(OsString::from)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn invalid_background_snapshot(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn check_background_snapshot_collection(len: usize) -> io::Result<()> {
    if len > MAX_BACKGROUND_SNAPSHOT_COLLECTION_ENTRIES {
        return Err(invalid_background_snapshot(
            "background state collection exceeds its entry limit",
        ));
    }
    Ok(())
}

fn check_background_snapshot_bytes(value: &[u8], limit: usize) -> io::Result<()> {
    if value.len() > limit {
        return Err(invalid_background_snapshot(
            "background state string exceeds its size limit",
        ));
    }
    Ok(())
}

#[derive(Default)]
struct BackgroundSnapshotLimits {
    entries: usize,
    bytes: usize,
}

impl BackgroundSnapshotLimits {
    fn check_collection(&self, len: usize) -> io::Result<()> {
        check_background_snapshot_collection(len)
    }

    fn add_collection(&mut self, len: usize) -> io::Result<()> {
        self.check_collection(len)?;
        self.add_entries(len)
    }

    fn add_entries(&mut self, count: usize) -> io::Result<()> {
        self.entries = self.entries.saturating_add(count);
        if self.entries > MAX_BACKGROUND_SNAPSHOT_TOTAL_ENTRIES {
            return Err(invalid_background_snapshot(
                "background state exceeds its aggregate entry limit",
            ));
        }
        Ok(())
    }

    fn add_string(&mut self, value: &str, limit: usize) -> io::Result<()> {
        self.add_bytes(value.as_bytes(), limit)
    }

    fn add_bytes(&mut self, value: &[u8], limit: usize) -> io::Result<()> {
        check_background_snapshot_bytes(value, limit)?;
        self.bytes = self.bytes.saturating_add(value.len());
        if self.bytes > BACKGROUND_SNAPSHOT_MAX_BYTES {
            return Err(invalid_background_snapshot(
                "background state strings exceed their aggregate size limit",
            ));
        }
        Ok(())
    }

    fn add_path(&mut self, value: &Path, limit: usize) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            self.add_bytes(value.as_os_str().as_bytes(), limit)
        }
        #[cfg(not(unix))]
        {
            self.add_bytes(value.to_string_lossy().as_bytes(), limit)
        }
    }

    fn add_os_string(&mut self, value: &OsString, limit: usize) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            self.add_bytes(value.as_os_str().as_bytes(), limit)
        }
        #[cfg(not(unix))]
        {
            self.add_bytes(value.to_string_lossy().as_bytes(), limit)
        }
    }
}

impl BackgroundSnapshot {
    fn validate(&self) -> io::Result<()> {
        let mut limits = BackgroundSnapshotLimits::default();
        for len in [
            self.vars.len(),
            self.exported_vars.len(),
            self.aliases.len(),
            self.abbreviations.len(),
            self.functions.len(),
            self.arrays.len(),
            self.assoc_arrays.len(),
            self.readonly_vars.len(),
            self.integer_vars.len(),
            self.shopt.len(),
        ] {
            limits.add_collection(len)?;
        }

        limits.add_string(&self.arg0, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
        for (name, value) in &self.vars {
            limits.add_string(name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
            limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
        }
        for name in self
            .exported_vars
            .iter()
            .chain(&self.readonly_vars)
            .chain(&self.integer_vars)
        {
            limits.add_string(name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
        }
        for (name, value) in self
            .aliases
            .iter()
            .chain(&self.abbreviations)
            .chain(&self.functions)
        {
            limits.add_string(name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
            limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
        }
        for (name, values) in &self.arrays {
            limits.add_collection(values.len())?;
            limits.add_string(name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
            for value in values {
                limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
            }
        }
        for (name, values) in &self.assoc_arrays {
            limits.add_collection(values.len())?;
            limits.add_string(name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
            for (key, value) in values {
                limits.add_string(key, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
                limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
            }
        }
        for name in self.shopt.keys() {
            limits.add_string(name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
        }
        if let Some(mode) = &self.default_output_mode {
            limits.add_string(mode, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
            mode.parse::<OutputMode>().map_err(|_| {
                invalid_background_snapshot("background state contains an invalid output mode")
            })?;
        }

        if let Some(active) = &self.active_env {
            limits.add_collection(active.saved.len())?;
            limits.add_bytes(&active.directory, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
            let mut names = HashSet::with_capacity(active.saved.len());
            for saved in &active.saved {
                if !names.insert(saved.name.as_str()) {
                    return Err(invalid_background_snapshot(
                        "background state contains duplicate project environment bindings",
                    ));
                }
                limits.add_string(&saved.name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
                if let Some(value) = &saved.prior_var {
                    limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
                }
                if let Some(value) = &saved.prior_value {
                    value.validate(&mut limits, 0)?;
                }
                if let Some(value) = &saved.prior_env {
                    limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
                }
                if let Some(value) = &saved.prior_opaque_env {
                    limits.add_bytes(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
                }
                if let Some(values) = &saved.prior_array {
                    limits.add_collection(values.len())?;
                    for value in values {
                        limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
                    }
                }
                if let Some(values) = &saved.prior_assoc {
                    limits.add_collection(values.len())?;
                    for (key, value) in values {
                        limits.add_string(key, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
                        limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
                    }
                }
                if saved.prior_array.is_some() && saved.prior_assoc.is_some() {
                    return Err(invalid_background_snapshot(
                        "background state contains conflicting saved variable bindings",
                    ));
                }
                if saved.prior_env.is_some() && saved.prior_opaque_env.is_some() {
                    return Err(invalid_background_snapshot(
                        "background state contains conflicting saved environment bindings",
                    ));
                }
            }
        }

        if self
            .vars
            .keys()
            .any(|name| self.arrays.contains_key(name) || self.assoc_arrays.contains_key(name))
            || self
                .arrays
                .keys()
                .any(|name| self.assoc_arrays.contains_key(name))
        {
            return Err(invalid_background_snapshot(
                "background state contains conflicting variable bindings",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopControlKind {
    Break,
    Continue,
}

/// State of a background job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Stopped,
    Done(i32),
}

/// A background job: a process group with the child handle(s) used to wait on
/// it. Backgrounded commands run as a child `agsh -c` process which is the
/// group leader (so its pgid equals its pid).
#[derive(Debug)]
pub struct Job {
    pub id: usize,
    pub pgid: i32,
    pub command: String,
    pub state: JobState,
    pub child: Child,
    pub reported_done: bool,
}

#[derive(Debug, Default)]
pub struct JobTable {
    jobs: Vec<Job>,
    next_id: usize,
    current: Option<usize>,
    previous: Option<usize>,
}

const MAX_RETAINED_JOB_STATUSES: usize = 1024;

impl JobTable {
    fn register(&mut self, child: Child, command: String) -> usize {
        self.next_id += 1;
        let id = self.next_id;
        let pgid = child.id() as i32;
        self.jobs.push(Job {
            id,
            pgid,
            command,
            state: JobState::Running,
            child,
            reported_done: false,
        });
        self.previous = self.current;
        self.current = Some(id);
        id
    }

    fn job_mut(&mut self, id: usize) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|job| job.id == id)
    }

    fn refresh_markers(&mut self) {
        if !self
            .current
            .is_some_and(|id| self.jobs.iter().any(|job| job.id == id))
        {
            self.current = self.jobs.last().map(|job| job.id);
        }
        if !self
            .previous
            .is_some_and(|id| Some(id) != self.current && self.jobs.iter().any(|job| job.id == id))
        {
            self.previous = self
                .jobs
                .iter()
                .rev()
                .map(|job| job.id)
                .find(|id| Some(*id) != self.current);
        }
    }

    fn resolve_spec(&self, spec: &str) -> Option<usize> {
        // %+ / %% = current, %- = previous, %n = job n, %name = prefix match.
        let Some(body) = spec.strip_prefix('%') else {
            let pid = spec.parse::<i32>().ok()?;
            return self
                .jobs
                .iter()
                .find(|job| job.pgid == pid)
                .map(|job| job.id);
        };
        match body {
            "" | "+" | "%" => self.current,
            "-" => self.previous,
            _ => {
                if let Ok(n) = body.parse::<usize>() {
                    self.jobs.iter().find(|j| j.id == n).map(|j| j.id)
                } else {
                    self.jobs
                        .iter()
                        .find(|j| j.command.starts_with(body))
                        .map(|j| j.id)
                }
            }
        }
    }
}

/// Complete saved binding for a temporarily shadowed variable. Export and type
/// attributes are separate from a value in real shells, so restoring only the
/// scalar text can leak a temporary value or destroy an array binding.
#[derive(Debug, Clone)]
pub(crate) struct VariableSnapshot {
    name: String,
    prior_var: Option<String>,
    prior_value: Option<Value>,
    prior_env: Option<String>,
    prior_opaque_env: Option<OsString>,
    prior_array: Option<Vec<String>>,
    prior_assoc: Option<BTreeMap<String, String>>,
    was_exported: bool,
    was_readonly: bool,
    was_integer: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LoopControl {
    kind: LoopControlKind,
    levels: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct BufferedStdin {
    data: Vec<u8>,
    offset: usize,
}

impl BufferedStdin {
    pub(crate) fn new(data: Vec<u8>) -> Self {
        Self { data, offset: 0 }
    }
}

#[derive(Clone)]
pub(crate) struct StreamingStdin {
    reader: Arc<Mutex<Box<dyn BufRead + Send>>>,
}

impl StreamingStdin {
    pub(crate) fn new(reader: impl Read + Send + 'static) -> Self {
        Self {
            reader: Arc::new(Mutex::new(Box::new(io::BufReader::new(reader)))),
        }
    }

    fn read_line(&self) -> io::Result<Option<String>> {
        let mut reader = self
            .reader
            .lock()
            .map_err(|_| io::Error::other("streaming stdin lock poisoned"))?;
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
            if bytes.len().saturating_add(take) > MAX_READ_LINE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("read input line exceeds {MAX_READ_LINE_BYTES} bytes"),
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
}

impl fmt::Debug for StreamingStdin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamingStdin").finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub(crate) struct StreamingStdout {
    writer: Arc<Mutex<io::PipeWriter>>,
}

impl StreamingStdout {
    pub(crate) fn new(writer: io::PipeWriter) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("streaming stdout lock poisoned"))?;
        writer.write_all(bytes)?;
        writer.flush()
    }

    /// A dup of the underlying pipe writer, for handing a child process's stdout
    /// straight to the downstream pipe.
    fn try_clone_writer(&self) -> Option<io::PipeWriter> {
        self.writer.lock().ok()?.try_clone().ok()
    }
}

impl fmt::Debug for StreamingStdout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamingStdout").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InterceptInstall {
    pub(crate) directory: PathBuf,
    pub(crate) prior_path: Option<String>,
    pub(crate) prior_shell: Option<String>,
    pub(crate) introduced_env: Vec<(String, String)>,
    pub(crate) deep_env: Vec<VariableSnapshot>,
}

#[derive(Debug, Clone)]
pub struct ShellState {
    cwd: PathBuf,
    arg0: String,
    vars: BTreeMap<String, String>,
    values: BTreeMap<String, Value>,
    env: BTreeMap<String, String>,
    /// Names carrying the export attribute. This is deliberately separate from
    /// `env`: `export NAME` marks an unset variable but must not invent `NAME=`
    /// in child environments until the variable receives a value.
    exported_vars: HashSet<String>,
    /// Inherited environment entries that are not valid UTF-8. Expansion cannot
    /// represent them, but external children still receive their exact bytes.
    opaque_env: BTreeMap<OsString, OsString>,
    aliases: BTreeMap<String, String>,
    abbreviations: BTreeMap<String, String>,
    functions: BTreeMap<String, ShellFunction>,
    history: Arc<Mutex<HistoryStore>>,
    path_cache: PathCache,
    path_cache_value: Option<String>,
    last_status: i32,
    last_command_substitution_status: i32,
    /// Stderr produced while expanding `$(...)` or `<(...)`. Expansion happens
    /// before a simple command installs its own redirections, so this is held
    /// aside until that command has finished and then attached to its caller's
    /// stderr. A scoped drain in the executor keeps nested function/compound
    /// execution from consuming a caller's pending bytes.
    pending_substitution_stderr: Vec<u8>,
    allexport: bool,
    errexit: bool,
    nounset: bool,
    noclobber: bool,
    noglob: bool,
    pipefail: bool,
    xtrace: bool,
    /// `shopt` options (globstar/extglob/nullglob/dotglob/nocaseglob), default off.
    shopt: BTreeMap<String, bool>,
    /// Programmable completion word lists (`complete -W`), keyed by command.
    completion_specs: BTreeMap<String, Vec<String>>,
    should_exit: bool,
    return_request: Option<i32>,
    /// Set when a streaming pipeline stage writes to a downstream pipe that has
    /// closed (`… | head`); the producing loop/list then stops (SIGPIPE-like).
    stream_pipe_closed: bool,
    source_depth: usize,
    /// Nesting depth of graph execution (functions, `$( )`, `<( )`, subshells,
    /// brace groups, `eval`, `source`). Bounds runaway recursion before it can
    /// overflow the process stack. Copied on `clone()` so it keeps accumulating
    /// across the state-clone that command substitution performs.
    exec_depth: usize,
    local_scopes: Vec<Vec<VariableSnapshot>>,
    loop_depth: usize,
    loop_control: Option<LoopControl>,
    buffered_stdin: Option<BufferedStdin>,
    streaming_stdin: Option<StreamingStdin>,
    streaming_stdout: Option<StreamingStdout>,
    jobs: Arc<Mutex<JobTable>>,
    interrupt: Arc<AtomicBool>,
    traces: Arc<Mutex<TraceStore>>,
    disk_traces: Arc<Mutex<VecDeque<(String, RawStreamRef)>>>,
    /// Stable base for a relative `$AGSH_TRACE_DIR`. Trace paths must not move
    /// when the shell changes directory between preflight, persistence, and a
    /// later `trace://` read.
    trace_dir_base: PathBuf,
    /// Shared by every stdout/stderr spool that contributes to the current
    /// top-level command. Replaced when a new top-level graph begins.
    trace_capture_budget: Arc<TraceCaptureBudget>,
    /// First-use hashes for advisories, so a repeated advisory (loop / agent retry)
    /// is shown once instead of flooding the context. Advisory channel only.
    advisories: Arc<Mutex<std::collections::HashSet<u64>>>,
    config: Arc<CompactorConfig>,
    /// Session default output mode (set by config/env/flag at startup and the
    /// runtime `mode` builtin); `None` means use the executor's mode.
    default_output_mode: Option<OutputMode>,
    /// True only while a builtin is rendering directly to a human terminal.
    /// Pipes, redirects, scripts, and capture modes keep this false so rich
    /// presentation never contaminates raw bytes.
    rich_stdout: bool,
    /// Internal semantic consumers (`$(...)`, `<(...)`) require exact bytes even
    /// when they use the executor's capture plumbing.
    exact_capture: bool,
    /// Exact state needed to turn runtime shell interception off without guessing
    /// the user's original PATH or preferred shell.
    intercept_install: Option<InterceptInstall>,
    git_cache: Arc<Mutex<GitCacheState>>,
    command_names: CommandNameCache,
    /// Directory and saved prior values for a currently-activated project `.env`.
    active_env: ActiveEnv,
    /// Resolved visual theme (colors + icons), built once per process.
    theme: Theme,
    /// Shell start time, for `$SECONDS`.
    start_time: Instant,
    /// LCG state for `$RANDOM` (advances on each read).
    rng: std::cell::Cell<u64>,
    /// `getopts` position within the current clustered option argument.
    getopts_char: usize,
    /// Trap handlers by normalized condition name (`EXIT`, `ERR`, `INT`, …).
    traps: HashMap<String, String>,
    /// Pending-signal flags (normalized signal name -> flag set by the handler),
    /// polled at command boundaries to fire signal traps.
    signal_flags: HashMap<String, Arc<AtomicBool>>,
    /// Indexed array variables (name -> dense element list).
    arrays: BTreeMap<String, Vec<String>>,
    /// Associative array variables (name -> key/value map). A name in this map
    /// is treated as associative for subscript assignment/expansion.
    assoc_arrays: BTreeMap<String, BTreeMap<String, String>>,
    /// Temp files backing process substitutions, removed at command boundaries.
    proc_sub_temps: Vec<PathBuf>,
    /// Variables marked read-only (`readonly` / `declare -r`).
    readonly_vars: HashSet<String>,
    /// Variables carrying bash's integer attribute (`declare -i`).
    integer_vars: HashSet<String>,
    /// Active command allowlist (`confine`). `None` = unrestricted. Narrow-only.
    confine: Option<Arc<agsh_policy::AllowPolicy>>,
    /// PID of the most recent background command, for `$!`.
    last_bg_pid: std::cell::Cell<Option<u32>>,
    /// Current source line, for `$LINENO`.
    current_line: std::cell::Cell<u32>,
}

/// The signal number for a normalized signal name, or None for non-signal
/// conditions (`EXIT`, `ERR`) or unknown names.
pub fn signal_number(name: &str) -> Option<i32> {
    use signal_hook::consts::*;
    Some(match name {
        "HUP" => SIGHUP,
        "INT" => SIGINT,
        "QUIT" => SIGQUIT,
        "ABRT" => SIGABRT,
        "ALRM" => SIGALRM,
        "TERM" => SIGTERM,
        "USR1" => SIGUSR1,
        "USR2" => SIGUSR2,
        "PIPE" => SIGPIPE,
        "TSTP" => SIGTSTP,
        "CONT" => SIGCONT,
        "WINCH" => SIGWINCH,
        _ => return None,
    })
}

/// Normalize a trap condition name: uppercase, strip a leading `SIG`, and map
/// `0` to `EXIT` (bash accepts `trap ... 0`).
pub fn normalize_trap_signal(name: &str) -> String {
    let upper = name.to_ascii_uppercase();
    let upper = upper.strip_prefix("SIG").unwrap_or(&upper);
    if upper == "0" {
        "EXIT".to_string()
    } else {
        upper.to_string()
    }
}

#[derive(Debug, Clone)]
struct GitCacheEntry {
    cwd: PathBuf,
    computed_at: Instant,
    context: Option<GitContext>,
}

#[derive(Debug, Default)]
struct GitCacheState {
    entry: Option<GitCacheEntry>,
    refreshing: Option<(PathBuf, u64)>,
    generation: u64,
}

const GIT_CACHE_TTL: std::time::Duration = std::time::Duration::from_millis(1500);

impl ShellState {
    pub fn from_current_process() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trace_dir_base = absolute_trace_dir_base(&cwd);
        let mut env = BTreeMap::new();
        let mut opaque_env = BTreeMap::new();
        for (key, value) in std::env::vars_os() {
            match key.into_string() {
                Ok(key) => match value.into_string() {
                    Ok(value) => {
                        env.insert(key, value);
                    }
                    Err(value) => {
                        opaque_env.insert(OsString::from(key), value);
                    }
                },
                Err(key) => {
                    opaque_env.insert(key, value);
                }
            }
        }
        let exported_vars = env
            .keys()
            .cloned()
            .chain(
                opaque_env
                    .keys()
                    .filter_map(|name| name.to_str().map(str::to_string)),
            )
            .collect();
        let mut vars = env.clone();
        // POSIX: IFS is initialized to <space><tab><newline> (2.5.3).
        vars.entry("IFS".to_string())
            .or_insert_with(|| " \t\n".to_string());
        let values = vars
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect();
        let config = Arc::new(CompactorConfig::load());
        let trace_capture_budget = Arc::new(TraceCaptureBudget::new(config.raw_storage_options()));
        Self {
            trace_dir_base,
            cwd,
            arg0: "agsh".to_string(),
            vars,
            values,
            env,
            exported_vars,
            opaque_env,
            aliases: BTreeMap::new(),
            abbreviations: BTreeMap::new(),
            functions: BTreeMap::new(),
            history: Arc::new(Mutex::new(HistoryStore::in_memory())),
            path_cache: PathCache::default(),
            path_cache_value: None,
            last_status: 0,
            last_command_substitution_status: 0,
            pending_substitution_stderr: Vec::new(),
            allexport: false,
            errexit: false,
            nounset: false,
            noclobber: false,
            noglob: false,
            pipefail: false,
            xtrace: false,
            shopt: BTreeMap::new(),
            completion_specs: BTreeMap::new(),
            should_exit: false,
            return_request: None,
            stream_pipe_closed: false,
            source_depth: 0,
            exec_depth: 0,
            local_scopes: Vec::new(),
            loop_depth: 0,
            loop_control: None,
            buffered_stdin: None,
            streaming_stdin: None,
            streaming_stdout: None,
            jobs: Arc::new(Mutex::new(JobTable::default())),
            interrupt: Arc::new(AtomicBool::new(false)),
            traces: Arc::new(Mutex::new(TraceStore::default())),
            disk_traces: Arc::new(Mutex::new(VecDeque::new())),
            trace_capture_budget,
            advisories: Arc::new(Mutex::new(std::collections::HashSet::new())),
            config,
            default_output_mode: None,
            rich_stdout: false,
            exact_capture: false,
            intercept_install: None,
            git_cache: Arc::new(Mutex::new(GitCacheState::default())),
            command_names: Arc::new(Mutex::new(None)),
            active_env: None,
            theme: Theme::detect(true),
            start_time: Instant::now(),
            getopts_char: 0,
            traps: HashMap::new(),
            signal_flags: HashMap::new(),
            arrays: BTreeMap::new(),
            assoc_arrays: BTreeMap::new(),
            proc_sub_temps: Vec::new(),
            readonly_vars: HashSet::new(),
            integer_vars: HashSet::new(),
            confine: None,
            last_bg_pid: std::cell::Cell::new(None),
            current_line: std::cell::Cell::new(1),
            rng: std::cell::Cell::new(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0x9E3779B97F4A7C15)
                    ^ (std::process::id() as u64).wrapping_mul(0x2545F4914F6CDD1D),
            ),
        }
    }

    /// Seconds elapsed since the shell started, for `$SECONDS`.
    pub fn uptime_secs(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }

    /// `getopts` char position within the current clustered argument.
    pub fn getopts_char(&self) -> usize {
        self.getopts_char
    }
    pub fn set_getopts_char(&mut self, pos: usize) {
        self.getopts_char = pos;
    }

    /// Set (or append to) an indexed array variable. Clears any scalar of the
    /// same name so `$name` reads element 0.
    pub fn set_array(&mut self, name: &str, mut elements: Vec<String>, append: bool) {
        if self.readonly_vars.contains(name) {
            return;
        }
        if append {
            let entry = self.arrays.entry(name.to_string()).or_default();
            entry.append(&mut elements);
        } else {
            self.arrays.insert(name.to_string(), elements);
        }
        self.vars.remove(name);
        self.values.remove(name);
        self.assoc_arrays.remove(name);
        self.env.remove(name);
        self.opaque_env.remove(OsStr::new(name));
    }

    /// Assign a single array element (growing the array as needed).
    pub fn set_array_element(&mut self, name: &str, index: usize, value: String, append: bool) {
        if self.readonly_vars.contains(name) {
            return;
        }
        let entry = self.arrays.entry(name.to_string()).or_default();
        if index >= entry.len() {
            entry.resize(index + 1, String::new());
        }
        if append {
            entry[index].push_str(&value);
        } else {
            entry[index] = value;
        }
        self.vars.remove(name);
        self.values.remove(name);
        self.assoc_arrays.remove(name);
        self.env.remove(name);
        self.opaque_env.remove(OsStr::new(name));
    }

    /// The elements of an indexed array, if `name` is one.
    pub fn array(&self, name: &str) -> Option<&[String]> {
        self.arrays.get(name).map(Vec::as_slice)
    }

    pub fn is_array(&self, name: &str) -> bool {
        self.arrays.contains_key(name)
    }

    pub fn remove_array_element(&mut self, name: &str, index: usize) -> bool {
        if self.is_readonly(name) {
            return false;
        }
        if let Some(array) = self.arrays.get_mut(name) {
            if index < array.len() {
                array.remove(index);
            }
        }
        true
    }

    pub fn declare_array(&mut self, name: &str) {
        if self.readonly_vars.contains(name) {
            return;
        }
        self.arrays.entry(name.to_string()).or_default();
        self.assoc_arrays.remove(name);
        self.vars.remove(name);
        self.values.remove(name);
        self.env.remove(name);
        self.opaque_env.remove(OsStr::new(name));
    }

    /// Declare `name` as an associative array (`declare -A`).
    pub fn declare_assoc(&mut self, name: &str) {
        if self.readonly_vars.contains(name) {
            return;
        }
        self.assoc_arrays.entry(name.to_string()).or_default();
        self.arrays.remove(name);
        self.vars.remove(name);
        self.values.remove(name);
        self.env.remove(name);
        self.opaque_env.remove(OsStr::new(name));
    }
    /// Whether `name` is an associative array.
    pub fn is_assoc(&self, name: &str) -> bool {
        self.assoc_arrays.contains_key(name)
    }
    /// Set one associative-array element.
    pub fn set_assoc_element(&mut self, name: &str, key: String, value: String, append: bool) {
        if self.readonly_vars.contains(name) {
            return;
        }
        let map = self.assoc_arrays.entry(name.to_string()).or_default();
        if append {
            map.entry(key).or_default().push_str(&value);
        } else {
            map.insert(key, value);
        }
        self.vars.remove(name);
        self.values.remove(name);
        self.arrays.remove(name);
        self.env.remove(name);
        self.opaque_env.remove(OsStr::new(name));
    }
    /// Replace an associative array's contents from key/value pairs.
    pub fn set_assoc(&mut self, name: &str, pairs: Vec<(String, String)>, append: bool) {
        if self.readonly_vars.contains(name) {
            return;
        }
        let map = self.assoc_arrays.entry(name.to_string()).or_default();
        if !append {
            map.clear();
        }
        for (k, v) in pairs {
            map.insert(k, v);
        }
        self.vars.remove(name);
        self.values.remove(name);
        self.arrays.remove(name);
        self.env.remove(name);
        self.opaque_env.remove(OsStr::new(name));
    }
    /// Look up an associative-array element.
    pub fn assoc_get(&self, name: &str, key: &str) -> Option<&str> {
        self.assoc_arrays.get(name)?.get(key).map(String::as_str)
    }
    /// Keys of an associative array (sorted; bash order is unspecified).
    pub fn assoc_keys(&self, name: &str) -> Option<Vec<String>> {
        self.assoc_arrays
            .get(name)
            .map(|m| m.keys().cloned().collect())
    }
    /// Values of an associative array (in key-sorted order).
    pub fn assoc_values(&self, name: &str) -> Option<Vec<String>> {
        self.assoc_arrays
            .get(name)
            .map(|m| m.values().cloned().collect())
    }

    pub fn assoc_entries(&self, name: &str) -> Option<Vec<(&str, &str)>> {
        self.assoc_arrays.get(name).map(|values| {
            values
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect()
        })
    }

    pub fn remove_assoc_element(&mut self, name: &str, key: &str) -> bool {
        if self.is_readonly(name) {
            return false;
        }
        if let Some(values) = self.assoc_arrays.get_mut(name) {
            values.remove(key);
        }
        true
    }

    /// Register a temp file backing a process substitution.
    pub fn register_proc_sub_temp(&mut self, path: PathBuf) {
        self.proc_sub_temps.push(path);
    }

    /// Record the PID of the most recent background command (`$!`).
    pub fn set_last_bg_pid(&self, pid: u32) {
        self.last_bg_pid.set(Some(pid));
    }
    pub fn last_bg_pid(&self) -> Option<u32> {
        self.last_bg_pid.get()
    }

    /// Current source line for `$LINENO`.
    pub fn set_current_line(&self, line: u32) {
        self.current_line.set(line);
    }
    pub fn current_line(&self) -> u32 {
        self.current_line.get()
    }

    /// The `$-` option-flags string (POSIX 2.5.2): one char per enabled option.
    pub fn option_flags(&self) -> String {
        let mut s = String::new();
        if self.allexport() {
            s.push('a');
        }
        if self.errexit() {
            s.push('e');
        }
        if self.noglob() {
            s.push('f');
        }
        if self.nounset() {
            s.push('u');
        }
        if self.xtrace() {
            s.push('x');
        }
        if self.noclobber() {
            s.push('C');
        }
        s
    }

    /// Take the pending process-substitution temp files for cleanup.
    pub fn take_proc_sub_temps(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.proc_sub_temps)
    }

    /// Install (or with `None`, clear) a trap handler for a condition. When the
    /// condition names a catchable signal, a handler flag is registered so the
    /// trap fires at the next command boundary.
    pub fn set_trap(&mut self, signal: &str, action: Option<String>) {
        let key = normalize_trap_signal(signal);
        match action {
            Some(cmd) => {
                if let Some(num) = signal_number(&key) {
                    self.signal_flags.entry(key.clone()).or_insert_with(|| {
                        let flag = Arc::new(AtomicBool::new(false));
                        let _ = signal_hook::flag::register(num, Arc::clone(&flag));
                        flag
                    });
                }
                self.traps.insert(key, cmd);
            }
            None => {
                self.traps.remove(&key);
            }
        }
    }

    /// Names of signals whose handler has fired since the last poll (cleared).
    pub fn take_pending_signal_traps(&self) -> Vec<String> {
        let mut fired = Vec::new();
        for (name, flag) in &self.signal_flags {
            if flag.swap(false, std::sync::atomic::Ordering::SeqCst)
                && self.traps.contains_key(name)
            {
                fired.push(name.clone());
            }
        }
        fired.sort();
        fired
    }

    /// The trap action for a condition, if any.
    pub fn trap_action(&self, signal: &str) -> Option<String> {
        self.traps.get(&normalize_trap_signal(signal)).cloned()
    }

    /// All installed traps (condition, action), sorted, for `trap -p`.
    pub fn trap_entries(&self) -> Vec<(String, String)> {
        let mut entries: Vec<(String, String)> = self
            .traps
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        entries.sort();
        entries
    }

    /// Next `$RANDOM` value (0..=32767), advancing the LCG. Interior-mutable so
    /// it works through a shared `&ShellState` in the expansion path.
    pub fn next_random(&self) -> u16 {
        // Numerical Recipes LCG; take the high bits for better distribution.
        let next = self
            .rng
            .get()
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.rng.set(next);
        ((next >> 33) as u16) & 0x7FFF
    }

    /// The resolved visual theme (colors honor terminal capability + NO_COLOR;
    /// icons honor AGSH_ICONS). Built once and shared via clones.
    pub fn theme(&self) -> Theme {
        self.theme
    }

    pub fn rich_stdout_enabled(&self) -> bool {
        self.rich_stdout
    }

    pub(crate) fn replace_rich_stdout(&mut self, enabled: bool) -> bool {
        std::mem::replace(&mut self.rich_stdout, enabled)
    }

    pub(crate) fn exact_capture_enabled(&self) -> bool {
        self.exact_capture
    }

    pub(crate) fn replace_exact_capture(&mut self, enabled: bool) -> bool {
        std::mem::replace(&mut self.exact_capture, enabled)
    }

    pub(crate) fn record_intercept_install(&mut self, install: InterceptInstall) {
        self.intercept_install = Some(install);
    }

    pub(crate) fn intercept_install_directory(&self) -> Option<&Path> {
        self.intercept_install
            .as_ref()
            .map(|install| install.directory.as_path())
    }

    pub(crate) fn take_intercept_install(&mut self) -> Option<InterceptInstall> {
        self.intercept_install.take()
    }

    pub(crate) fn is_top_level_execution(&self) -> bool {
        self.exec_depth == 1
    }

    /// Apply (or refresh) the current directory's trusted `.env`, restoring any
    /// previously-activated project env first. Untrusted `.env` files are
    /// ignored, so this is a no-op unless the user ran `trust` — keeping default
    /// behavior identical to no env activation.
    pub fn activate_project_env(&mut self) {
        self.deactivate_project_env();
        let cwd = self.cwd().to_path_buf();
        let Some(envfile) = agsh_index::find_dotenv(&cwd) else {
            return;
        };
        let Some(snapshot) = agsh_index::read_dotenv(&envfile) else {
            return;
        };
        let Ok(store) = agsh_index::TrustStore::load() else {
            return;
        };
        if !store.is_trusted(&cwd, snapshot.digest) {
            return;
        }
        self.apply_project_env_snapshot(cwd, snapshot.pairs);
    }

    fn deactivate_project_env(&mut self) {
        if let Some((_dir, saved)) = self.active_env.take() {
            for binding in saved {
                self.restore_variable(binding);
            }
        }
    }

    fn apply_project_env_snapshot(&mut self, cwd: PathBuf, pairs: Vec<(String, String)>) -> usize {
        let mut saved = Vec::new();
        let mut seen = HashSet::new();
        for (key, value) in pairs {
            if seen.insert(key.clone()) {
                saved.push(self.snapshot_variable(&key));
            }
            self.export_var(key, value);
        }
        let count = saved.len();
        if !saved.is_empty() {
            self.active_env = Some((cwd, saved));
        }
        count
    }

    /// Trust the current directory's `.env` and activate it. Returns
    /// `Ok(Some(var_count))` if a `.env` was found, `Ok(None)` if absent, or an
    /// I/O error when the file/store cannot be validated and persisted.
    pub fn trust_current_env(&mut self) -> io::Result<Option<usize>> {
        let cwd = self.cwd().to_path_buf();
        let Some(envfile) = agsh_index::find_dotenv(&cwd) else {
            return Ok(None);
        };
        let snapshot = agsh_index::read_dotenv_checked(&envfile).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot read {}: {error}", envfile.display()),
            )
        })?;
        let mut store = agsh_index::TrustStore::load().map_err(|error| {
            io::Error::new(error.kind(), format!("cannot read trust store: {error}"))
        })?;
        store.trust(&cwd, snapshot.digest).map_err(|error| {
            io::Error::new(error.kind(), format!("cannot persist trust store: {error}"))
        })?;
        self.deactivate_project_env();
        let count = self.apply_project_env_snapshot(cwd, snapshot.pairs);
        Ok(Some(count))
    }

    /// True if `name` resolves to a builtin, alias, function, or PATH executable.
    /// The PATH executable set is cached and rebuilt only when `$PATH` changes,
    /// so this is cheap enough to call per keystroke (e.g. for highlighting).
    pub fn is_command_name(&self, name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        if crate::builtins::is_builtin(name)
            || name == "agview" // top-level rich-view sugar (handled in main)
            || self.aliases.contains_key(name)
            || self.functions.contains_key(name)
        {
            return true;
        }
        if name.contains('/') {
            return Path::new(name).exists();
        }
        let path = self.lookup("PATH").unwrap_or("").to_string();
        if let Ok(mut cache) = self.command_names.lock() {
            let stale = cache.as_ref().map(|(p, _)| p != &path).unwrap_or(true);
            if stale {
                let set: std::collections::HashSet<String> =
                    crate::suggest::path_executables(&path)
                        .into_iter()
                        .collect();
                *cache = Some((path, set));
            }
            return cache
                .as_ref()
                .map(|(_, set)| set.contains(name))
                .unwrap_or(false);
        }
        false
    }

    /// The most recent `n` commands with exit code + duration (newest last), for
    /// the `context` builtin.
    pub fn recent_commands(&self, n: usize) -> Vec<(String, Option<i32>, u64)> {
        self.history
            .lock()
            .map(|s| {
                let entries = s.entries();
                let start = entries.len().saturating_sub(n);
                entries[start..]
                    .iter()
                    .map(|e| (e.command.clone(), e.exit_code, e.duration_ms))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The most recent `n` command lines (newest last), for history navigation.
    pub fn history_recent(&self, n: usize) -> Vec<String> {
        self.history
            .lock()
            .map(|s| {
                let entries = s.entries();
                let start = entries.len().saturating_sub(n);
                entries[start..].iter().map(|e| e.command.clone()).collect()
            })
            .unwrap_or_default()
    }

    /// Git context for synchronous consumers such as explicit history/context
    /// commands. The subprocess probe is bounded, but this call can wait for it;
    /// prompt rendering must use [`Self::prompt_git_context`] instead.
    pub fn git_context(&self) -> Option<GitContext> {
        let cwd = self.cwd().to_path_buf();
        if let Ok(cache) = self.git_cache.lock() {
            if let Some(entry) = cache.entry.as_ref() {
                if entry.cwd == cwd && entry.computed_at.elapsed() < GIT_CACHE_TTL {
                    return entry.context.clone();
                }
            }
        }
        let generation = self.git_cache.lock().ok().map(|mut cache| {
            cache.generation = cache.generation.wrapping_add(1);
            cache.refreshing = None;
            cache.generation
        });
        let context = agsh_index::git_context(&cwd);
        if let Ok(mut cache) = self.git_cache.lock() {
            if generation.is_none_or(|generation| cache.generation == generation) {
                cache.entry = Some(GitCacheEntry {
                    cwd,
                    computed_at: Instant::now(),
                    context: context.clone(),
                });
            }
        }
        context
    }

    /// Return cached Git context immediately and refresh stale data on a worker.
    /// On a repository's first prompt this intentionally returns `None`; the next
    /// render uses the populated cache rather than delaying terminal input.
    pub fn prompt_git_context(&self) -> Option<GitContext> {
        self.prompt_git_context_with(|cwd| agsh_index::git_context(&cwd))
    }

    fn prompt_git_context_with<F>(&self, compute: F) -> Option<GitContext>
    where
        F: FnOnce(PathBuf) -> Option<GitContext> + Send + 'static,
    {
        let cwd = self.cwd().to_path_buf();
        let (stale, generation) = {
            let Ok(mut cache) = self.git_cache.lock() else {
                return None;
            };
            if let Some(entry) = cache.entry.as_ref() {
                if entry.cwd == cwd && entry.computed_at.elapsed() < GIT_CACHE_TTL {
                    return entry.context.clone();
                }
            }
            let stale = cache
                .entry
                .as_ref()
                .filter(|entry| entry.cwd == cwd)
                .and_then(|entry| entry.context.clone());
            if cache
                .refreshing
                .as_ref()
                .is_some_and(|(refresh_cwd, _)| refresh_cwd == &cwd)
            {
                return stale;
            }
            cache.generation = cache.generation.wrapping_add(1);
            let generation = cache.generation;
            cache.refreshing = Some((cwd.clone(), generation));
            (stale, generation)
        };

        let cache = Arc::clone(&self.git_cache);
        let cleanup_cache = Arc::clone(&cache);
        let refresh_cwd = cwd.clone();
        let spawn = std::thread::Builder::new()
            .name("agsh-git-refresh".to_string())
            .spawn(move || {
                let context = compute(refresh_cwd.clone());
                if let Ok(mut cache) = cache.lock() {
                    let is_current = cache.generation == generation
                        && cache
                            .refreshing
                            .as_ref()
                            .is_some_and(|(path, id)| path == &refresh_cwd && *id == generation);
                    if is_current {
                        cache.entry = Some(GitCacheEntry {
                            cwd: refresh_cwd,
                            computed_at: Instant::now(),
                            context,
                        });
                        cache.refreshing = None;
                    }
                }
            });
        if spawn.is_err() {
            if let Ok(mut cache) = cleanup_cache.lock() {
                if cache.generation == generation {
                    cache.refreshing = None;
                }
            }
        }
        stale
    }

    /// Duration of the most recently finalized command, for the prompt.
    pub fn last_duration_ms(&self) -> Option<u64> {
        self.history
            .lock()
            .ok()
            .and_then(|s| s.entries().last().map(|e| e.duration_ms))
    }

    /// The loaded token-economy configuration.
    pub fn output_config(&self) -> &CompactorConfig {
        &self.config
    }

    #[cfg(test)]
    pub(crate) fn replace_output_config_for_test(&mut self, config: CompactorConfig) {
        self.config = Arc::new(config);
        self.trace_capture_budget =
            Arc::new(TraceCaptureBudget::new(self.config.raw_storage_options()));
    }

    /// The session default output mode (`mode` builtin / startup config), if set.
    pub fn default_output_mode(&self) -> Option<OutputMode> {
        self.default_output_mode
    }

    /// Set (or clear with `None`) the session default output mode.
    pub fn set_default_output_mode(&mut self, mode: Option<OutputMode>) {
        self.default_output_mode = mode;
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn set_cwd(&mut self, cwd: PathBuf) {
        self.cwd = cwd;
    }

    pub fn should_exit(&self) -> bool {
        self.should_exit
    }

    /// True once a streaming pipeline stage hit a closed downstream pipe, so the
    /// producing loop/list should stop iterating (SIGPIPE-like early exit).
    pub(crate) fn stream_pipe_closed(&self) -> bool {
        self.stream_pipe_closed
    }

    pub(crate) fn set_stream_pipe_closed(&mut self) {
        self.stream_pipe_closed = true;
    }

    pub fn request_exit(&mut self) {
        self.should_exit = true;
    }

    pub(crate) fn request_return(&mut self, code: i32) {
        self.return_request = Some(code);
    }

    pub(crate) fn return_requested(&self) -> bool {
        self.return_request.is_some()
    }

    pub(crate) fn take_return(&mut self) -> Option<i32> {
        self.return_request.take()
    }

    pub(crate) fn enter_function_scope(&mut self) {
        self.local_scopes.push(Vec::new());
    }

    pub(crate) fn leave_function_scope(&mut self) {
        if let Some(scope) = self.local_scopes.pop() {
            for saved in scope.into_iter().rev() {
                self.restore_local(saved);
            }
        }
    }

    pub(crate) fn in_function(&self) -> bool {
        !self.local_scopes.is_empty()
    }

    pub(crate) fn enter_source(&mut self) {
        self.source_depth += 1;
    }

    pub(crate) fn leave_source(&mut self) {
        self.source_depth = self.source_depth.saturating_sub(1);
    }

    pub(crate) fn in_source(&self) -> bool {
        self.source_depth > 0
    }

    /// Enter one level of graph execution, erroring if nested too deeply (a
    /// runaway `f() { f; }`, `$( $( … ) )`, or `( ( … ) )`). Returns the error
    /// instead of letting the recursion overflow the stack (SIGABRT). Pair every
    /// `Ok(())` with a [`leave_exec`](Self::leave_exec).
    pub(crate) fn enter_exec(&mut self) -> Result<(), ShellError> {
        if self.exec_depth == 0 {
            self.trace_capture_budget =
                Arc::new(TraceCaptureBudget::new(self.config.raw_storage_options()));
        }
        self.exec_depth += 1;
        if self.exec_depth > MAX_EXEC_DEPTH {
            self.exec_depth -= 1;
            return Err(ShellError::execution(
                "execution nested too deeply (possible infinite recursion)",
            ));
        }
        Ok(())
    }

    pub(crate) fn leave_exec(&mut self) {
        self.exec_depth = self.exec_depth.saturating_sub(1);
    }

    /// Declare `name` as local to the innermost function scope, recording its
    /// complete prior binding for restoration on function exit. The variable
    /// starts unset; an inherited export attribute remains attached, matching
    /// bash, but no child environment entry exists until a value is assigned.
    /// Returns false outside a function or when the outer binding is readonly.
    pub(crate) fn declare_local(&mut self, name: &str) -> bool {
        if self.local_scopes.is_empty() || self.is_readonly(name) {
            return false;
        }
        let already = self
            .local_scopes
            .last()
            .is_some_and(|scope| scope.iter().any(|saved| saved.name == name));
        if !already {
            let saved = self.snapshot_variable(name);
            let inherit_export = saved.was_exported;
            if let Some(scope) = self.local_scopes.last_mut() {
                scope.push(saved);
            }
            self.clear_variable_binding(name);
            if inherit_export {
                self.exported_vars.insert(name.to_string());
            }
        }
        true
    }

    fn restore_local(&mut self, saved: VariableSnapshot) {
        self.restore_variable(saved);
    }

    pub fn loop_depth(&self) -> usize {
        self.loop_depth
    }

    pub fn enter_loop(&mut self) {
        self.loop_depth += 1;
    }

    pub fn leave_loop(&mut self) {
        self.loop_depth = self.loop_depth.saturating_sub(1);
    }

    pub(crate) fn request_loop_control(&mut self, kind: LoopControlKind, levels: usize) {
        self.loop_control = Some(LoopControl {
            kind,
            levels: levels.max(1).min(self.loop_depth.max(1)),
        });
    }

    pub(crate) fn loop_control_requested(&self) -> bool {
        self.loop_control.is_some()
    }

    pub(crate) fn handle_loop_control_for_current_loop(&mut self) -> Option<LoopControlKind> {
        let control = self.loop_control?;
        if control.levels <= 1 {
            self.loop_control = None;
        } else {
            self.loop_control = Some(LoopControl {
                levels: control.levels - 1,
                ..control
            });
        }
        Some(control.kind)
    }

    pub(crate) fn replace_buffered_stdin(
        &mut self,
        buffered_stdin: Option<BufferedStdin>,
    ) -> Option<BufferedStdin> {
        std::mem::replace(&mut self.buffered_stdin, buffered_stdin)
    }

    pub(crate) fn replace_streaming_stdin(
        &mut self,
        streaming_stdin: Option<StreamingStdin>,
    ) -> Option<StreamingStdin> {
        std::mem::replace(&mut self.streaming_stdin, streaming_stdin)
    }

    pub(crate) fn replace_streaming_stdout(
        &mut self,
        streaming_stdout: Option<StreamingStdout>,
    ) -> Option<StreamingStdout> {
        std::mem::replace(&mut self.streaming_stdout, streaming_stdout)
    }

    pub(crate) fn read_shell_stdin_line(&mut self) -> Option<io::Result<Option<String>>> {
        if self.buffered_stdin.is_some() {
            return Some(self.read_buffered_stdin_line());
        }

        self.streaming_stdin.as_ref().map(StreamingStdin::read_line)
    }

    pub(crate) fn write_shell_stdout(&self, bytes: &[u8]) -> Option<io::Result<()>> {
        self.streaming_stdout
            .as_ref()
            .map(|stdout| stdout.write_all(bytes))
    }

    pub(crate) fn streaming_stdout_is_none(&self) -> bool {
        self.streaming_stdout.is_none()
    }

    /// A clone of the downstream pipe writer when this scope streams stdout to a
    /// pipeline consumer, so a leaf external can write straight to the pipe
    /// (backpressure + SIGPIPE on early close) instead of being captured. `None`
    /// outside a streaming pipeline stage.
    pub(crate) fn streaming_stdout_writer(&self) -> Option<io::PipeWriter> {
        self.streaming_stdout.as_ref()?.try_clone_writer()
    }

    /// Register a backgrounded child process and return its job id.
    pub fn register_job(&self, child: Child, command: impl Into<String>) -> (usize, i32) {
        let mut table = self.jobs.lock().expect("job table poisoned");
        let pgid = child.id() as i32;
        let id = table.register(child, command.into());
        (id, pgid)
    }

    /// Capture a command's raw output so its `trace://<id>/...` references can
    /// be read back later (e.g. with the `trace` builtin).
    pub fn record_trace(
        &self,
        cmd_id: &CommandId,
        command: &str,
        exit_code: i32,
        stdout: &[u8],
        stderr: &[u8],
    ) {
        let _ = self.record_trace_captured(
            cmd_id,
            command,
            exit_code,
            CapturedTraceStreams {
                stdout_preview: stdout,
                stderr_preview: stderr,
                stdout_preview_complete: true,
                stderr_preview_complete: true,
                stdout_exact: None,
                stderr_exact: None,
            },
        );
    }

    /// Persist exact captured streams and return references that remain valid
    /// after a one-shot shell exits. `stdout`/`stderr` are bounded observation
    /// previews; exact segments, when supplied, are copied to disk incrementally.
    pub(crate) fn record_trace_captured(
        &self,
        cmd_id: &CommandId,
        command: &str,
        exit_code: i32,
        streams: CapturedTraceStreams<'_>,
    ) -> io::Result<RawStreamRef> {
        let storage = self.config.raw_storage_options();
        let dir = if storage.enabled {
            self.trace_dir_for_storage()?
        } else {
            self.trace_dir()
        };
        let persisted = persist_trace_segments_to_dir(&dir, cmd_id, streams, storage);
        if persisted.is_ok() {
            // Exact bytes live on disk. Do not retain another bounded preview for
            // every command: 200 head/tail previews would still consume hundreds
            // of MiB. On persistence failure there is no trace reference at all;
            // an observation preview may contain elision markers and must never be
            // inserted into the exact in-memory trace store.
            if let Ok(mut store) = self.traces.lock() {
                store.record(TraceRecord::new(
                    cmd_id,
                    command,
                    exit_code,
                    Vec::new(),
                    Vec::new(),
                ));
            }
        }
        let raw = persisted?;
        if let Ok(mut traces) = self.disk_traces.lock() {
            while traces.len() >= 200 {
                traces.pop_front();
            }
            traces.push_back((cmd_id.to_string(), raw.clone()));
        }
        Ok(raw)
    }

    /// Create a private file for teeing one exact child stream while retaining
    /// only a bounded observation preview in memory.
    pub(crate) fn create_trace_spool(&self, extension: &str) -> io::Result<TraceSpoolWriter> {
        use std::os::unix::fs::OpenOptionsExt;

        if !self.trace_capture_budget.enabled {
            return Ok(TraceSpoolWriter {
                path: None,
                file: None,
                keep: false,
                budget: Arc::clone(&self.trace_capture_budget),
                incomplete: TraceSpoolIncompleteMarker(Arc::new(AtomicBool::new(false))),
                total_bytes: 0,
                stored_bytes: 0,
            });
        }
        let dir = self.trace_dir_for_storage()?;
        prepare_private_trace_dir(&dir)?;
        prune_stale_trace_spools(&dir);
        for _ in 0..128 {
            let path = dir.join(format!(
                ".capture-{}-{}-{}.{}",
                std::process::id(),
                self.next_random(),
                CommandId::new(),
                extension
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(TraceSpoolWriter {
                        path: Some(path),
                        file: Some(file),
                        keep: false,
                        budget: Arc::clone(&self.trace_capture_budget),
                        incomplete: TraceSpoolIncompleteMarker(Arc::new(AtomicBool::new(false))),
                        total_bytes: 0,
                        stored_bytes: 0,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a unique trace spool file",
        ))
    }

    /// Validate and, when necessary, create the raw-trace directory before a
    /// command whose contract requires a durable reference is allowed to run.
    /// Later write failures remain best effort so they cannot replace a child's
    /// exit status after execution has begun.
    pub(crate) fn prepare_required_trace_storage(&self) -> io::Result<()> {
        if !self.config.raw_storage_options().enabled {
            return Ok(());
        }
        prepare_private_trace_dir(&self.trace_dir_for_storage()?)
    }

    fn trace_dir(&self) -> PathBuf {
        let configured = self
            .lookup("AGSH_TRACE_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        anchor_trace_dir(&self.trace_dir_base, configured, default_trace_dir())
    }

    fn trace_dir_for_storage(&self) -> io::Result<PathBuf> {
        let dir = self.trace_dir();
        if dir.to_str().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "trace directory is not valid UTF-8 and cannot be represented losslessly",
            ));
        }
        Ok(dir)
    }

    /// Record a first-use advisory key: returns `true` the FIRST time this key is
    /// seen in the session and `false` thereafter, so a repeated advisory (in a shell
    /// loop or an agent retry) is emitted once instead of flooding the context.
    /// Advisory channel only — never gate a real error or an exit code on this.
    pub fn advise_once(&self, key: &str) -> bool {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        let hash = hasher.finish();
        match self.advisories.lock() {
            Ok(mut set) => set.insert(hash),
            Err(_) => true,
        }
    }

    /// Resolve a trace only when every byte is available. Callers that can
    /// explicitly handle a stored prefix use [`resolve_trace_with_status`].
    pub fn resolve_trace(&self, reference: &str) -> Option<Vec<u8>> {
        let resolved = self.resolve_trace_with_status(reference)?;
        (resolved.status == RawTraceStatus::Complete).then_some(resolved.bytes)
    }

    /// Resolve a trace with explicit completeness metadata. Allocating reads are
    /// capped at 16 MiB; a larger or storage-truncated result is marked as a
    /// prefix, never an exact/lossless stream. Disabled or unavailable storage is
    /// represented by an empty byte vector plus its explicit status.
    pub fn resolve_trace_with_status(&self, reference: &str) -> Option<ResolvedTrace> {
        let reader = self.open_trace_reader(reference)?;
        let mut status = reader.status;
        let mut bytes = Vec::with_capacity(64 * 1024);
        reader
            .take((MAX_ALLOCATED_TRACE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > MAX_ALLOCATED_TRACE_BYTES {
            bytes.truncate(MAX_ALLOCATED_TRACE_BYTES);
            status = RawTraceStatus::Truncated;
        }
        Some(ResolvedTrace { bytes, status })
    }

    /// Open a validated trace as a stream, carrying the same explicit
    /// completeness status as the allocating resolver. This supports bounded
    /// line/range/grep consumers without first loading a large trace into memory.
    pub fn open_trace_reader(&self, reference: &str) -> Option<TraceReader> {
        let (id, stream) = parse_trace_ref(reference);
        if let Some(raw) = self.disk_traces.lock().ok().and_then(|traces| {
            traces
                .iter()
                .rev()
                .find(|(trace_id, _)| trace_id == id)
                .map(|(_, raw)| raw.clone())
        }) {
            let (path, status) = match stream {
                TraceStream::Stdout => (raw.stdout, raw.stdout_status),
                TraceStream::Stderr => (raw.stderr, raw.stderr_status),
            };
            if matches!(
                status,
                RawTraceStatus::Disabled | RawTraceStatus::Unavailable
            ) {
                return Some(TraceReader {
                    inner: TraceReaderInner::Memory(io::Cursor::new(Vec::new())),
                    status,
                });
            }
            return open_private_trace_file(Path::new(&path))
                .ok()
                .map(|file| TraceReader {
                    inner: TraceReaderInner::File(file),
                    status,
                });
        }
        let store = self.traces.lock().ok()?;
        store.resolve(reference).map(|bytes| {
            let stored = bytes.len().min(MAX_ALLOCATED_TRACE_BYTES);
            TraceReader {
                inner: TraceReaderInner::Memory(io::Cursor::new(bytes[..stored].to_vec())),
                status: if stored == bytes.len() {
                    RawTraceStatus::Complete
                } else {
                    RawTraceStatus::Truncated
                },
            }
        })
    }

    /// Completeness metadata for a persisted raw path emitted by this session.
    /// Arbitrary regular files are not trace-index entries and return `None`.
    pub fn trace_status_for_path(&self, path: &Path) -> Option<RawTraceStatus> {
        self.disk_traces.lock().ok().and_then(|traces| {
            traces.iter().rev().find_map(|(_, raw)| {
                if !raw.stdout.is_empty() && Path::new(&raw.stdout) == path {
                    Some(raw.stdout_status)
                } else if !raw.stderr.is_empty() && Path::new(&raw.stderr) == path {
                    Some(raw.stderr_status)
                } else {
                    None
                }
            })
        })
    }

    /// Summaries of recent traces as `(id, exit_code, command)` for listing.
    pub fn trace_summaries(&self) -> Vec<(String, i32, String)> {
        self.traces
            .lock()
            .map(|store| {
                store
                    .records()
                    .map(|r| (r.cmd_id.clone(), r.exit_code, r.command.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Shared flag set by a SIGINT handler so the shell can interrupt loops and
    /// long-running command lists without terminating.
    pub fn interrupt_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.interrupt)
    }

    pub fn interrupted(&self) -> bool {
        self.interrupt.load(Ordering::Relaxed)
    }

    pub fn clear_interrupt(&self) {
        self.interrupt.store(false, Ordering::Relaxed);
    }

    /// Live background jobs as `(pgid, command)`, for the session journal's
    /// flight recorder (running or stopped; done-but-unreaped jobs excluded).
    pub fn running_jobs_snapshot(&self) -> Vec<(i32, String)> {
        self.jobs
            .lock()
            .map(|table| {
                table
                    .jobs
                    .iter()
                    .filter(|job| matches!(job.state, JobState::Running | JobState::Stopped))
                    .map(|job| (job.pgid, job.command.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn has_running_jobs(&self) -> bool {
        self.jobs
            .lock()
            .map(|table| {
                table
                    .jobs
                    .iter()
                    .any(|job| matches!(job.state, JobState::Running | JobState::Stopped))
            })
            .unwrap_or(false)
    }

    /// Poll background jobs; mark finished ones Done and return human-readable
    /// completion notices. Completed status remains waitable by PID/job spec;
    /// old reported statuses are bounded to avoid an ever-growing table.
    pub fn reap_finished_jobs(&self) -> Vec<String> {
        let mut notices = Vec::new();
        // Notices go to stderr; color only when that is a TTY.
        let theme = if io::stderr().is_terminal() {
            self.theme()
        } else {
            Theme::plain()
        };
        let Ok(mut table) = self.jobs.lock() else {
            return notices;
        };
        for job in table.jobs.iter_mut() {
            if job.state == JobState::Running {
                if let Ok(Some(status)) = job.child.try_wait() {
                    job.state = JobState::Done(job_exit_status(status));
                }
            }
        }
        for job in &mut table.jobs {
            if let JobState::Done(code) = job.state {
                if job.reported_done {
                    continue;
                }
                let (label, role) = if code == 0 {
                    ("Done".to_string(), Role::Ok)
                } else {
                    (format!("Exit {code}"), Role::Error)
                };
                notices.push(format!(
                    "[{}]  {}\t{}",
                    job.id,
                    theme.paint(role, &label),
                    job.command
                ));
                job.reported_done = true;
            }
        }
        let mut remove = table.jobs.len().saturating_sub(MAX_RETAINED_JOB_STATUSES);
        let current = table.current;
        table.jobs.retain(|job| {
            if remove > 0 && job.reported_done && Some(job.id) != current {
                remove -= 1;
                false
            } else {
                true
            }
        });
        table.refresh_markers();
        notices
    }

    /// Render the current job table for the `jobs` builtin.
    pub fn job_listing(&self) -> Vec<String> {
        let Ok(mut table) = self.jobs.lock() else {
            return Vec::new();
        };
        // Refresh statuses first.
        for job in table.jobs.iter_mut() {
            if job.state == JobState::Running {
                if let Ok(Some(status)) = job.child.try_wait() {
                    job.state = JobState::Done(job_exit_status(status));
                }
            }
        }
        let current = table.current;
        let previous = table.previous;
        table
            .jobs
            .iter()
            .map(|job| {
                let marker = if Some(job.id) == current {
                    '+'
                } else if Some(job.id) == previous {
                    '-'
                } else {
                    ' '
                };
                let status = match job.state {
                    JobState::Running => "Running".to_string(),
                    JobState::Stopped => "Stopped".to_string(),
                    JobState::Done(0) => "Done".to_string(),
                    JobState::Done(code) => format!("Exit {code}"),
                };
                format!("[{}]{}  {}\t{}", job.id, marker, status, job.command)
            })
            .collect()
    }

    /// Resolve a job spec (`%n`, `%+`, `%-`, `%name`) or a bare PID to a pgid.
    pub fn job_pgid(&self, spec: &str) -> Option<i32> {
        let table = self.jobs.lock().ok()?;
        let id = table.resolve_spec(spec)?;
        table.jobs.iter().find(|j| j.id == id).map(|j| j.pgid)
    }

    pub fn set_job_running(&self, spec: &str) -> bool {
        let Ok(mut table) = self.jobs.lock() else {
            return false;
        };
        let Some(id) = table.resolve_spec(spec) else {
            return false;
        };
        if let Some(job) = table.job_mut(id) {
            job.state = JobState::Running;
            true
        } else {
            false
        }
    }

    /// Block until the given job (or all jobs if `spec` is None) finishes.
    /// An explicit PID/job returns that job's status; no operand returns zero
    /// after all known jobs complete, matching POSIX `wait`.
    pub fn wait_for_jobs(&self, spec: Option<&str>) -> Option<i32> {
        let wait_all = spec.is_none();
        // Take the targeted child(ren) out of the table, then wait without
        // holding the lock.
        let mut targets: Vec<(usize, Child, String)> = Vec::new();
        {
            let mut table = self.jobs.lock().ok()?;
            match spec {
                Some(spec) => {
                    let id = table.resolve_spec(spec)?;
                    if let Some(pos) = table.jobs.iter().position(|j| j.id == id) {
                        let job = table.jobs.remove(pos);
                        targets.push((job.id, job.child, job.command));
                    } else {
                        return None;
                    }
                }
                None => {
                    let drained = std::mem::take(&mut table.jobs);
                    for job in drained {
                        targets.push((job.id, job.child, job.command));
                    }
                }
            }
            table.refresh_markers();
        }
        if targets.is_empty() {
            return Some(0);
        }
        let mut last = 0;
        for (_, mut child, _) in targets {
            if let Ok(status) = child.wait() {
                last = job_exit_status(status);
            }
        }
        Some(if wait_all { 0 } else { last })
    }

    pub(crate) fn read_buffered_stdin_line(&mut self) -> io::Result<Option<String>> {
        let Some(buffered) = self.buffered_stdin.as_mut() else {
            return Ok(None);
        };
        if buffered.offset >= buffered.data.len() {
            return Ok(None);
        }

        let start = buffered.offset;
        let end = buffered.data[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffered.data.len(), |index| start + index + 1);
        buffered.offset = end;
        if end.saturating_sub(start) > MAX_READ_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("read input line exceeds {MAX_READ_LINE_BYTES} bytes"),
            ));
        }
        Ok(Some(
            String::from_utf8_lossy(&buffered.data[start..end]).to_string(),
        ))
    }

    pub fn last_status(&self) -> i32 {
        self.last_status
    }

    pub fn set_last_status(&mut self, status: i32) {
        self.last_status = status;
    }

    /// Exit status of the most recent command substitution. Used so an
    /// assignment whose value is a command substitution (`x=$(cmd)`) reports
    /// the command's status as `$?`.
    pub(crate) fn last_command_substitution_status(&self) -> i32 {
        self.last_command_substitution_status
    }

    pub(crate) fn set_command_substitution_status(&mut self, status: i32) {
        self.last_command_substitution_status = status;
    }

    pub(crate) fn append_pending_substitution_stderr(&mut self, bytes: Vec<u8>) {
        self.pending_substitution_stderr.extend_from_slice(&bytes);
    }

    pub(crate) fn take_pending_substitution_stderr(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_substitution_stderr)
    }

    pub fn allexport(&self) -> bool {
        self.allexport
    }

    pub fn set_allexport(&mut self, enabled: bool) {
        self.allexport = enabled;
    }

    pub fn errexit(&self) -> bool {
        self.errexit
    }

    pub fn set_errexit(&mut self, enabled: bool) {
        self.errexit = enabled;
    }

    pub fn nounset(&self) -> bool {
        self.nounset
    }

    pub fn set_nounset(&mut self, enabled: bool) {
        self.nounset = enabled;
    }

    pub fn noclobber(&self) -> bool {
        self.noclobber
    }

    pub fn set_noclobber(&mut self, enabled: bool) {
        self.noclobber = enabled;
    }

    pub fn noglob(&self) -> bool {
        self.noglob
    }

    pub fn set_noglob(&mut self, enabled: bool) {
        self.noglob = enabled;
    }

    pub fn pipefail(&self) -> bool {
        self.pipefail
    }

    pub fn set_pipefail(&mut self, enabled: bool) {
        self.pipefail = enabled;
    }

    /// A `shopt` option's state (default off).
    pub fn shopt(&self, name: &str) -> bool {
        self.shopt.get(name).copied().unwrap_or(false)
    }
    /// Set a `shopt` option on/off.
    pub fn set_shopt(&mut self, name: &str, on: bool) {
        self.shopt.insert(name.to_string(), on);
    }

    /// Register a `complete -W` word list for a command (replaces any prior).
    pub fn register_completion_spec(&mut self, command: &str, words: Vec<String>) {
        self.completion_specs.insert(command.to_string(), words);
    }
    /// Remove a command's completion spec.
    pub fn remove_completion_spec(&mut self, command: &str) {
        self.completion_specs.remove(command);
    }
    /// The registered completion word list for a command, if any.
    pub fn completion_spec(&self, command: &str) -> Option<&[String]> {
        self.completion_specs.get(command).map(Vec::as_slice)
    }
    /// All registered completion specs (for `complete -p`).
    pub fn completion_specs(&self) -> &BTreeMap<String, Vec<String>> {
        &self.completion_specs
    }

    pub fn xtrace(&self) -> bool {
        self.xtrace
    }

    pub fn set_xtrace(&mut self, enabled: bool) {
        self.xtrace = enabled;
    }

    pub fn set_var(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let _ = self.try_set_var(key, value);
    }

    /// Assign a scalar, returning false rather than silently changing a
    /// readonly binding. Existing array names receive element zero, as in bash.
    pub fn try_set_var(&mut self, key: impl Into<String>, value: impl Into<String>) -> bool {
        let key = key.into();
        if self.readonly_vars.contains(&key) {
            return false;
        }
        if key == "PATH" {
            self.clear_path_cache();
        }
        let value = value.into();
        if self.arrays.contains_key(&key) {
            self.set_array_element(&key, 0, value, false);
            return true;
        }
        if self.assoc_arrays.contains_key(&key) {
            self.set_assoc_element(&key, "0".to_string(), value, false);
            return true;
        }
        self.values
            .insert(key.clone(), Value::String(value.clone()));
        self.vars.insert(key.clone(), value.clone());
        self.arrays.remove(&key);
        self.assoc_arrays.remove(&key);
        self.opaque_env.remove(OsStr::new(&key));
        if self.exported_vars.contains(&key) {
            self.env.insert(key, value);
        }
        true
    }

    /// The active command allowlist, if this session is confined.
    pub fn confine_policy(&self) -> Option<&agsh_policy::AllowPolicy> {
        self.confine.as_deref()
    }
    /// Whether this session restricts which external commands may run.
    pub fn is_confined(&self) -> bool {
        self.confine.is_some()
    }
    /// Apply (or narrow) the command allowlist. Narrow-only: if already confined,
    /// the result is the intersection, so a session can never widen its own jail.
    pub fn set_confine(&mut self, names: &[String]) {
        let policy = match &self.confine {
            Some(existing) => existing.intersect(names),
            None => agsh_policy::AllowPolicy::from_names(names),
        };
        self.confine = Some(Arc::new(policy));
    }

    /// Mark a variable read-only (assignment/unset are then refused).
    pub fn mark_readonly(&mut self, name: &str) {
        self.readonly_vars.insert(name.to_string());
    }
    pub fn is_readonly(&self, name: &str) -> bool {
        self.readonly_vars.contains(name)
    }

    pub fn mark_integer(&mut self, name: &str) {
        self.integer_vars.insert(name.to_string());
    }

    pub fn is_integer(&self, name: &str) -> bool {
        self.integer_vars.contains(name)
    }

    pub(crate) fn readonly_variable_names(&self) -> Vec<&str> {
        let mut names = self
            .readonly_vars
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        names.sort_unstable();
        names
    }

    pub(crate) fn variable_names(&self) -> Vec<&str> {
        let mut names = self
            .vars
            .keys()
            .chain(self.arrays.keys())
            .chain(self.assoc_arrays.keys())
            .chain(self.exported_vars.iter())
            .chain(self.integer_vars.iter())
            .map(String::as_str)
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        names
    }

    pub fn variable_exists(&self, name: &str) -> bool {
        self.vars.contains_key(name)
            || self.arrays.contains_key(name)
            || self.assoc_arrays.contains_key(name)
            || self.exported_vars.contains(name)
            || self.integer_vars.contains(name)
    }

    pub fn set_value(&mut self, key: impl Into<String>, value: Value) {
        let key = key.into();
        if self.readonly_vars.contains(&key) {
            return;
        }
        if key == "PATH" {
            self.clear_path_cache();
        }
        let serialized = value.as_string_lossy();
        self.vars.insert(key.clone(), serialized.clone());
        self.values.insert(key.clone(), value);
        self.arrays.remove(&key);
        self.assoc_arrays.remove(&key);
        self.opaque_env.remove(OsStr::new(&key));
        if self.exported_vars.contains(&key) {
            self.env.insert(key, serialized);
        }
    }

    pub fn export_var(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let _ = self.try_export_var(key, value);
    }

    pub fn try_export_var(&mut self, key: impl Into<String>, value: impl Into<String>) -> bool {
        let key = key.into();
        if self.readonly_vars.contains(&key) {
            return false;
        }
        let value = value.into();
        if key == "PATH" {
            self.clear_path_cache();
        }
        if self.arrays.contains_key(&key) {
            self.set_array_element(&key, 0, value, false);
            self.exported_vars.insert(key);
            return true;
        }
        if self.assoc_arrays.contains_key(&key) {
            self.set_assoc_element(&key, "0".to_string(), value, false);
            self.exported_vars.insert(key);
            return true;
        }
        self.values
            .insert(key.clone(), Value::String(value.clone()));
        self.vars.insert(key.clone(), value.clone());
        self.arrays.remove(&key);
        self.assoc_arrays.remove(&key);
        self.exported_vars.insert(key.clone());
        self.opaque_env.remove(OsStr::new(&key));
        self.env.insert(key, value);
        true
    }

    /// Remove a variable and all of its attributes. Returns false for readonly
    /// bindings so builtins can provide a deterministic diagnostic/status.
    pub fn unset(&mut self, key: &str) -> bool {
        if self.readonly_vars.contains(key) {
            return false;
        }
        self.clear_variable_binding(key);
        true
    }

    /// Mark an existing or unset variable for export without manufacturing an
    /// empty value. Arrays carry the attribute but are not serialized to env.
    pub fn mark_exported(&mut self, key: &str) {
        self.exported_vars.insert(key.to_string());
        if let Some(value) = self.vars.get(key).cloned() {
            self.opaque_env.remove(OsStr::new(key));
            self.env.insert(key.to_string(), value);
        } else if !self.opaque_env.contains_key(OsStr::new(key)) {
            self.env.remove(key);
        }
    }

    pub fn unexport(&mut self, key: &str) -> bool {
        self.exported_vars.remove(key);
        self.env.remove(key);
        self.opaque_env.remove(OsStr::new(key));
        true
    }

    pub fn is_exported(&self, key: &str) -> bool {
        self.exported_vars.contains(key)
    }

    pub(crate) fn exported_variable_names(&self) -> Vec<&str> {
        let mut names = self
            .exported_vars
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        names.sort_unstable();
        names
    }

    pub(crate) fn snapshot_variable(&self, name: &str) -> VariableSnapshot {
        VariableSnapshot {
            name: name.to_string(),
            prior_var: self.vars.get(name).cloned(),
            prior_value: self.values.get(name).cloned(),
            prior_env: self.env.get(name).cloned(),
            prior_opaque_env: self.opaque_env.get(OsStr::new(name)).cloned(),
            prior_array: self.arrays.get(name).cloned(),
            prior_assoc: self.assoc_arrays.get(name).cloned(),
            was_exported: self.exported_vars.contains(name),
            was_readonly: self.readonly_vars.contains(name),
            was_integer: self.integer_vars.contains(name),
        }
    }

    pub(crate) fn restore_variable(&mut self, saved: VariableSnapshot) {
        let name = saved.name.clone();
        self.clear_variable_binding(&name);
        if let Some(value) = saved.prior_var {
            self.vars.insert(name.clone(), value);
        }
        if let Some(value) = saved.prior_value {
            self.values.insert(name.clone(), value);
        }
        if let Some(value) = saved.prior_env {
            self.env.insert(name.clone(), value);
        }
        if let Some(value) = saved.prior_opaque_env {
            self.opaque_env.insert(OsString::from(&name), value);
        }
        if let Some(value) = saved.prior_array {
            self.arrays.insert(name.clone(), value);
        }
        if let Some(value) = saved.prior_assoc {
            self.assoc_arrays.insert(name.clone(), value);
        }
        if saved.was_exported {
            self.exported_vars.insert(name.clone());
        }
        if saved.was_readonly {
            self.readonly_vars.insert(name.clone());
        }
        if saved.was_integer {
            self.integer_vars.insert(name);
        }
    }

    fn clear_variable_binding(&mut self, key: &str) {
        if key == "PATH" {
            self.clear_path_cache();
        }
        self.vars.remove(key);
        self.values.remove(key);
        self.env.remove(key);
        self.opaque_env.remove(OsStr::new(key));
        self.exported_vars.remove(key);
        self.arrays.remove(key);
        self.assoc_arrays.remove(key);
        self.readonly_vars.remove(key);
        self.integer_vars.remove(key);
    }

    pub fn lookup(&self, key: &str) -> Option<&str> {
        self.vars
            .get(key)
            .or_else(|| self.env.get(key))
            .map(String::as_str)
    }

    pub fn lookup_value(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    pub fn vars(&self) -> &BTreeMap<String, String> {
        &self.vars
    }

    pub fn exported_env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    fn validate_background_snapshot_top_level(
        &self,
        limits: &mut BackgroundSnapshotLimits,
    ) -> io::Result<()> {
        for len in [
            self.vars.len(),
            self.exported_vars.len(),
            self.aliases.len(),
            self.abbreviations.len(),
            self.functions.len(),
            self.arrays.len(),
            self.assoc_arrays.len(),
            self.readonly_vars.len(),
            self.integer_vars.len(),
            self.shopt.len(),
        ] {
            limits.add_collection(len)?;
        }

        limits.add_string(&self.arg0, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
        for (name, value) in &self.vars {
            limits.add_string(name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
            limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
        }
        for name in self
            .exported_vars
            .iter()
            .chain(&self.readonly_vars)
            .chain(&self.integer_vars)
        {
            limits.add_string(name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
        }
        for (name, value) in self.aliases.iter().chain(&self.abbreviations) {
            limits.add_string(name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
            limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
        }
        for (name, function) in &self.functions {
            limits.add_string(name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
            limits.add_string(&function.body, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
        }
        for (name, values) in &self.arrays {
            limits.add_collection(values.len())?;
            limits.add_string(name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
            for value in values {
                limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
            }
        }
        for (name, values) in &self.assoc_arrays {
            limits.add_collection(values.len())?;
            limits.add_string(name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
            for (key, value) in values {
                limits.add_string(key, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
                limits.add_string(value, MAX_BACKGROUND_SNAPSHOT_VALUE_BYTES)?;
            }
        }
        for name in self.shopt.keys() {
            limits.add_string(name, MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
        }
        if let Some(mode) = self.default_output_mode {
            limits.add_string(mode.as_str(), MAX_BACKGROUND_SNAPSHOT_NAME_BYTES)?;
        }

        if self
            .vars
            .keys()
            .any(|name| self.arrays.contains_key(name) || self.assoc_arrays.contains_key(name))
            || self
                .arrays
                .keys()
                .any(|name| self.assoc_arrays.contains_key(name))
        {
            return Err(invalid_background_snapshot(
                "background state contains conflicting variable bindings",
            ));
        }
        Ok(())
    }

    /// Serialize the state a POSIX asynchronous subshell inherits but may mutate
    /// independently. Exported bytes travel through the real child environment;
    /// this snapshot carries shell-only bindings and options that an `agsh -c`
    /// child cannot otherwise reconstruct.
    pub fn encode_background_snapshot(&self) -> io::Result<Vec<u8>> {
        let mut limits = BackgroundSnapshotLimits::default();
        self.validate_background_snapshot_top_level(&mut limits)?;
        let active_env = self
            .active_env
            .as_ref()
            .map(|(directory, saved)| {
                BackgroundActiveEnv::from_active(directory, saved, &mut limits)
            })
            .transpose()?;

        let mut exported_vars = self.exported_vars.iter().cloned().collect::<Vec<_>>();
        exported_vars.sort();
        let mut readonly_vars = self.readonly_vars.iter().cloned().collect::<Vec<_>>();
        readonly_vars.sort();
        let mut integer_vars = self.integer_vars.iter().cloned().collect::<Vec<_>>();
        integer_vars.sort();
        let snapshot = BackgroundSnapshot {
            version: BACKGROUND_SNAPSHOT_VERSION,
            arg0: self.arg0.clone(),
            vars: self.vars.clone(),
            exported_vars,
            aliases: self.aliases.clone(),
            abbreviations: self.abbreviations.clone(),
            functions: self
                .functions
                .iter()
                .map(|(name, function)| (name.clone(), function.body.clone()))
                .collect(),
            arrays: self.arrays.clone(),
            assoc_arrays: self.assoc_arrays.clone(),
            readonly_vars,
            integer_vars,
            allexport: self.allexport,
            errexit: self.errexit,
            nounset: self.nounset,
            noclobber: self.noclobber,
            noglob: self.noglob,
            pipefail: self.pipefail,
            xtrace: self.xtrace,
            shopt: self.shopt.clone(),
            last_status: self.last_status,
            default_output_mode: self
                .default_output_mode
                .map(|mode| mode.as_str().to_string()),
            active_env,
        };
        snapshot.validate()?;
        let bytes = serde_json::to_vec(&snapshot)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if bytes.len() > BACKGROUND_SNAPSHOT_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "background state exceeds its size limit",
            ));
        }
        Ok(bytes)
    }

    /// Restore a snapshot produced by [`ShellState::encode_background_snapshot`].
    /// Process-global and security state (cwd, environment bytes, confinement,
    /// signal handlers, jobs, traces) deliberately remains owned by this child.
    pub fn restore_background_snapshot(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > BACKGROUND_SNAPSHOT_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "background state exceeds its size limit",
            ));
        }
        let snapshot: BackgroundSnapshot = serde_json::from_slice(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if snapshot.version != BACKGROUND_SNAPSHOT_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported background snapshot version",
            ));
        }
        snapshot.validate()?;
        let default_output_mode = match snapshot.default_output_mode.as_deref() {
            Some(mode) => Some(mode.parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "background state contains an invalid output mode",
                )
            })?),
            None => None,
        };
        let active_env = snapshot
            .active_env
            .map(BackgroundActiveEnv::into_active)
            .transpose()?;

        self.arg0 = snapshot.arg0;
        self.vars = snapshot.vars;
        self.values = self
            .vars
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect();
        if self.vars.contains_key("@") {
            self.values.insert(
                "@".to_string(),
                Value::List(self.positionals().into_iter().map(Value::String).collect()),
            );
        }
        self.exported_vars = snapshot.exported_vars.into_iter().collect();
        self.aliases = snapshot.aliases;
        self.abbreviations = snapshot.abbreviations;
        self.functions = snapshot
            .functions
            .into_iter()
            .map(|(name, body)| (name, ShellFunction::new(body)))
            .collect();
        self.arrays = snapshot.arrays;
        self.assoc_arrays = snapshot.assoc_arrays;
        self.readonly_vars = snapshot.readonly_vars.into_iter().collect();
        self.integer_vars = snapshot.integer_vars.into_iter().collect();
        self.allexport = snapshot.allexport;
        self.errexit = snapshot.errexit;
        self.nounset = snapshot.nounset;
        self.noclobber = snapshot.noclobber;
        self.noglob = snapshot.noglob;
        self.pipefail = snapshot.pipefail;
        self.xtrace = snapshot.xtrace;
        self.shopt = snapshot.shopt;
        self.last_status = snapshot.last_status;
        self.default_output_mode = default_output_mode;
        self.active_env = active_env;
        self.clear_path_cache();
        Ok(())
    }

    /// Replace a child's inherited environment with the shell's exported state,
    /// retaining exact non-UTF-8 entries inherited at shell startup.
    pub(crate) fn configure_child_env(&self, command: &mut Command) {
        command.env_clear();
        command.envs(&self.opaque_env);
        command.envs(&self.env);
    }

    /// Exported names representable by the shell, including names whose values
    /// are opaque bytes. Used by deterministic secret/injection classification.
    pub(crate) fn exported_env_names(&self) -> Vec<&str> {
        self.env
            .keys()
            .map(String::as_str)
            .chain(self.opaque_env.keys().filter_map(|name| name.to_str()))
            .collect()
    }

    #[cfg(unix)]
    pub(crate) fn opaque_exported_env_bytes(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        use std::os::unix::ffi::OsStrExt;

        self.opaque_env
            .iter()
            .filter(|(name, _)| name.as_os_str() != OsStr::new("AGSH_SESSION"))
            .map(|(name, value)| {
                (
                    name.as_os_str().as_bytes().to_vec(),
                    value.as_os_str().as_bytes().to_vec(),
                )
            })
            .collect()
    }

    /// Temporarily remove one name from the child-process environment without
    /// changing its shell-local value or readonly status.
    pub fn take_exported_env(&mut self, key: &str) -> Option<OsString> {
        self.env
            .remove(key)
            .map(OsString::from)
            .or_else(|| self.opaque_env.remove(OsStr::new(key)))
    }

    /// Restore an entry previously returned by [`Self::take_exported_env`].
    pub fn restore_exported_env(&mut self, key: String, value: OsString) {
        match value.into_string() {
            Ok(value) => {
                self.env.insert(key, value);
            }
            Err(value) => {
                self.opaque_env.insert(OsString::from(key), value);
            }
        }
    }

    pub fn arg0(&self) -> &str {
        &self.arg0
    }

    pub fn set_arg0(&mut self, value: impl Into<String>) {
        self.arg0 = value.into();
    }

    pub fn set_positionals(&mut self, args: &[String]) {
        self.clear_positionals();
        for (index, value) in args.iter().enumerate() {
            self.vars.insert((index + 1).to_string(), value.clone());
            self.values
                .insert((index + 1).to_string(), Value::String(value.clone()));
        }
        self.vars.insert("@".to_string(), args.join(" "));
        self.values.insert(
            "@".to_string(),
            Value::List(args.iter().cloned().map(Value::String).collect()),
        );
    }

    /// Drop the first `count` positional parameters. Returns false if there
    /// are fewer than `count` positionals (matching `shift`'s error behavior).
    pub fn shift_positionals(&mut self, count: usize) -> bool {
        let current = self.positionals();
        if count > current.len() {
            return false;
        }
        self.set_positionals(&current[count..]);
        true
    }

    pub fn clear_positionals(&mut self) {
        self.vars
            .retain(|name, _| name != "@" && !is_positional_name(name));
        self.values
            .retain(|name, _| name != "@" && !is_positional_name(name));
    }

    pub fn positional_count(&self) -> usize {
        self.positionals().len()
    }

    pub fn positionals(&self) -> Vec<String> {
        let mut values = Vec::new();
        for index in 1.. {
            let Some(value) = self.vars.get(&index.to_string()) else {
                break;
            };
            values.push(value.clone());
        }
        values
    }

    pub fn set_alias(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.aliases.insert(name.into(), value.into());
    }

    pub fn remove_alias(&mut self, name: &str) -> bool {
        self.aliases.remove(name).is_some()
    }

    pub fn alias(&self, name: &str) -> Option<&str> {
        self.aliases.get(name).map(String::as_str)
    }

    pub fn aliases(&self) -> &BTreeMap<String, String> {
        &self.aliases
    }

    pub fn set_abbreviation(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.abbreviations.insert(name.into(), value.into());
    }

    pub fn remove_abbreviation(&mut self, name: &str) -> bool {
        self.abbreviations.remove(name).is_some()
    }

    pub fn abbreviation(&self, name: &str) -> Option<&str> {
        self.abbreviations.get(name).map(String::as_str)
    }

    pub fn abbreviations(&self) -> &BTreeMap<String, String> {
        &self.abbreviations
    }

    pub fn set_function(&mut self, name: impl Into<String>, function: ShellFunction) {
        self.functions.insert(name.into(), function);
    }

    pub fn remove_function(&mut self, name: &str) -> bool {
        self.functions.remove(name).is_some()
    }

    pub fn function(&self, name: &str) -> Option<&ShellFunction> {
        self.functions.get(name)
    }

    pub fn functions(&self) -> &BTreeMap<String, ShellFunction> {
        &self.functions
    }

    /// Switch to a file-backed history store, loading existing entries. Called
    /// once by the interactive binary; tests and subshells stay in-memory.
    pub fn load_persistent_history(&self) {
        if let Some(path) = history::default_history_path() {
            if let Ok(mut store) = self.history.lock() {
                *store = HistoryStore::with_file(path, 50_000);
            }
        }
    }

    /// Record a command as it starts (exit code and duration are filled in later
    /// by [`finalize_history`]).
    pub fn record_history(&self, line: impl Into<String>) {
        self.record_history_with_mode(line, None);
    }

    pub fn record_history_with_mode(
        &self,
        line: impl Into<String>,
        output_mode: Option<OutputMode>,
    ) {
        let line = line.into();
        if line.trim().is_empty() {
            return;
        }
        let mut entry = HistoryEntry::new(line, self.cwd().display().to_string(), unix_now());
        entry.hostname = history::hostname();
        entry.user = history::username();
        entry.session_id = self.lookup("AGSH_SESSION").map(str::to_string);
        entry.output_mode = output_mode.map(|mode| mode.as_str().to_string());
        entry.command_family = command_family(&entry.command);
        if let Some(git) = self.git_context() {
            entry.git_root = Some(git.root.display().to_string());
            entry.git_branch = git.branch;
        }
        if let Ok(mut store) = self.history.lock() {
            store.push(entry);
        }
    }

    #[cfg(test)]
    pub(crate) fn push_history_entry_for_test(&self, entry: HistoryEntry) {
        if let Ok(mut store) = self.history.lock() {
            store.push(entry);
        }
    }

    #[cfg(test)]
    pub(crate) fn set_theme_for_test(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Attach the exit code and wall-clock duration to the most recent command.
    pub fn finalize_history(&self, exit_code: i32, duration_ms: u64) {
        if let Ok(mut store) = self.history.lock() {
            store.finalize_last(exit_code, duration_ms);
        }
    }

    pub fn clear_history(&self) {
        if let Ok(mut store) = self.history.lock() {
            store.clear();
        }
    }

    pub fn history_len(&self) -> usize {
        self.history.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// The recorded command lines, oldest first.
    pub fn history_commands(&self) -> Vec<String> {
        self.history
            .lock()
            .map(|s| s.entries().iter().map(|e| e.command.clone()).collect())
            .unwrap_or_default()
    }

    pub fn history_entries(&self) -> Vec<HistoryEntry> {
        self.history
            .lock()
            .map(|s| s.entries().to_vec())
            .unwrap_or_default()
    }

    /// The most recent command beginning with `prefix`, for autosuggestion.
    pub fn history_suggest(&self, prefix: &str) -> Option<String> {
        self.history
            .lock()
            .ok()
            .and_then(|s| s.suggest(prefix).map(str::to_string))
    }

    /// Fuzzy-search history, ranked by relevance and frecency (newest-first ties).
    pub fn history_search(&self, query: &str, limit: usize) -> Vec<HistoryEntry> {
        self.history
            .lock()
            .map(|s| {
                s.fuzzy_search(query, unix_now(), limit)
                    .into_iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn history_query(&self, query: &HistoryQuery) -> Vec<HistoryMatch> {
        self.history
            .lock()
            .map(|s| s.query(query, unix_now()))
            .unwrap_or_default()
    }

    pub fn history_stats(&self) -> HistoryStats {
        self.history.lock().map(|s| s.stats()).unwrap_or_default()
    }

    /// Directories ranked by frecency, for directory jumping.
    pub fn frecent_dirs(&self) -> Vec<(String, i64)> {
        self.history
            .lock()
            .map(|s| s.frecent_dirs(unix_now()))
            .unwrap_or_default()
    }

    pub fn cached_path_lookup(&mut self, path_value: &str, name: &str) -> Option<PathBuf> {
        self.ensure_path_cache_for(path_value);
        let cached = self.path_cache.get(name).cloned()?;
        if cached_executable_exists(&cached) {
            Some(cached)
        } else {
            self.path_cache.remove(name);
            None
        }
    }

    pub fn cache_path_lookup(
        &mut self,
        path_value: impl Into<String>,
        name: impl Into<String>,
        path: PathBuf,
    ) {
        let path_value = path_value.into();
        self.ensure_path_cache_for(&path_value);
        self.path_cache.insert(name, path);
    }

    pub fn path_cache_len_for_tests(&self) -> usize {
        self.path_cache.len()
    }

    fn ensure_path_cache_for(&mut self, path_value: &str) {
        if self.path_cache_value.as_deref() != Some(path_value) {
            self.clear_path_cache();
            self.path_cache_value = Some(path_value.to_string());
        }
    }

    fn clear_path_cache(&mut self) {
        self.path_cache.clear();
        self.path_cache_value = None;
    }
}

fn job_exit_status(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;

    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

fn cached_executable_exists(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn is_positional_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|ch| ch.is_ascii_digit())
}

/// Keep at most this many trace files in `$AGSH_TRACE_DIR` (2 per command).
/// Default cap on files in `$AGSH_TRACE_DIR` (2 per command ⇒ ~256 commands).
/// Override with `$AGSH_TRACE_DIR_CAP`. Keeps the newest, drops the oldest.
const TRACE_DIR_FILE_CAP: usize = 512;
const TRACE_DIR_FILE_CAP_MAX: usize = 4096;
const TRACE_DIR_TOTAL_BYTES_CAP: u64 = 2 * 1024 * 1024 * 1024;

fn trace_dir_cap() -> usize {
    std::env::var("AGSH_TRACE_DIR_CAP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(TRACE_DIR_FILE_CAP)
        .clamp(2, TRACE_DIR_FILE_CAP_MAX)
}

fn default_trace_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "agsh-traces-{}",
        rustix::process::geteuid().as_raw()
    ))
}

fn absolute_trace_dir_base(cwd: &Path) -> PathBuf {
    absolute_trace_dir_base_with_default(cwd, &default_trace_dir())
}

fn absolute_trace_dir_base_with_default(cwd: &Path, default: &Path) -> PathBuf {
    if cwd.is_absolute() {
        return cwd.to_path_buf();
    }
    if default.is_absolute() {
        default.to_path_buf()
    } else {
        PathBuf::from("/tmp")
    }
}

fn anchor_trace_dir(base: &Path, configured: Option<PathBuf>, default: PathBuf) -> PathBuf {
    let path = configured.unwrap_or(default);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

#[cfg(test)]
fn persist_trace_to_dir(
    dir: &Path,
    cmd_id: &CommandId,
    stdout: &[u8],
    stderr: &[u8],
) -> io::Result<()> {
    persist_trace_segments_to_dir(
        dir,
        cmd_id,
        CapturedTraceStreams {
            stdout_preview: stdout,
            stderr_preview: stderr,
            stdout_preview_complete: true,
            stderr_preview_complete: true,
            stdout_exact: None,
            stderr_exact: None,
        },
        RawStorageOptions {
            enabled: true,
            max_bytes: agsh_output::DEFAULT_MAX_RAW_BYTES,
        },
    )
    .map(|_| ())
}

fn persist_trace_segments_to_dir(
    dir: &Path,
    cmd_id: &CommandId,
    streams: CapturedTraceStreams<'_>,
    storage: RawStorageOptions,
) -> io::Result<RawStreamRef> {
    let CapturedTraceStreams {
        stdout_preview,
        stderr_preview,
        stdout_preview_complete,
        stderr_preview_complete,
        stdout_exact,
        stderr_exact,
    } = streams;
    if !storage.enabled {
        return Ok(RawStreamRef::disabled());
    }
    prepare_private_trace_dir(dir)?;
    let pid = std::process::id();
    let stdout_path = dir.join(format!("{pid}_{cmd_id}.out"));
    let stderr_path = dir.join(format!("{pid}_{cmd_id}.err"));
    let mut remaining = storage.max_bytes;
    let stdout_status = write_private_trace_segments(
        &stdout_path,
        stdout_preview,
        stdout_preview_complete,
        stdout_exact,
        &mut remaining,
    )?;
    let stderr_status = match write_private_trace_segments(
        &stderr_path,
        stderr_preview,
        stderr_preview_complete,
        stderr_exact,
        &mut remaining,
    ) {
        Ok(status) => status,
        Err(error) => {
            let _ = std::fs::remove_file(&stdout_path);
            return Err(error);
        }
    };
    prune_trace_dir_protecting(
        dir,
        trace_dir_cap(),
        &[stdout_path.as_path(), stderr_path.as_path()],
    );

    // Pruning is best-effort and other shell processes can prune the same shared
    // directory concurrently. Never publish a complete-looking reference until
    // both files have survived pruning and still pass the private-file checks.
    let validation = open_private_trace_file(&stdout_path)
        .and_then(|stdout| open_private_trace_file(&stderr_path).map(|stderr| (stdout, stderr)));
    if let Err(error) = validation {
        let _ = std::fs::remove_file(&stdout_path);
        let _ = std::fs::remove_file(&stderr_path);
        return Err(error);
    }

    Ok(RawStreamRef::persisted(
        stdout_path.display().to_string(),
        stderr_path.display().to_string(),
        stdout_status,
        stderr_status,
        storage.max_bytes,
    ))
}

/// Raw traces can contain credentials and arbitrary command output. Require a
/// private directory owned by this process's user and reject untrusted symlinks
/// in its existing path before creating or opening files beneath it.
fn prepare_private_trace_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    reject_symlink_components(dir)?;
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "trace path is not a directory",
            ));
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700).create(dir)?;
        }
        Err(e) => return Err(e),
    }
    reject_symlink_components(dir)?;

    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "trace directory must not be a symlink",
        ));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "trace directory is owned by another user",
        ));
    }

    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o7777 != 0o700
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "trace directory is not private",
        ));
    }
    Ok(())
}

fn reject_symlink_components(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            // macOS exposes system paths such as /var and /tmp through
            // root-owned compatibility symlinks. They are not replaceable by an
            // unprivileged user; reject every other symlink component.
            Ok(metadata) if metadata.file_type().is_symlink() && metadata.uid() != 0 => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("trace path contains symlink: {}", current.display()),
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn write_private_trace_segments(
    path: &Path,
    preview: &[u8],
    preview_complete: bool,
    exact: Option<Vec<ExactTraceSegment>>,
    remaining: &mut u64,
) -> io::Result<RawTraceStatus> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    let result = (|| {
        let mut total_bytes = 0u64;
        let mut stored_bytes = 0u64;
        let mut source_complete = true;
        match exact {
            Some(segments) => {
                for segment in segments {
                    source_complete &= segment.is_complete();
                    match segment {
                        ExactTraceSegment::Memory(bytes) => {
                            let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                            total_bytes = total_bytes.saturating_add(length);
                            let write = length.min(*remaining);
                            let write = usize::try_from(write).unwrap_or(bytes.len());
                            file.write_all(&bytes[..write])?;
                            stored_bytes = stored_bytes.saturating_add(write as u64);
                            *remaining = remaining.saturating_sub(write as u64);
                        }
                        ExactTraceSegment::File(exact) => {
                            total_bytes = total_bytes.saturating_add(exact.total_bytes);
                            let available = exact.stored_bytes.min(*remaining);
                            if available > 0 {
                                let mut source = std::fs::File::open(exact.path())?.take(available);
                                let copied = io::copy(&mut source, &mut file)?;
                                if copied != available {
                                    return Err(io::Error::new(
                                        io::ErrorKind::UnexpectedEof,
                                        "trace spool ended before its recorded length",
                                    ));
                                }
                                stored_bytes = stored_bytes.saturating_add(copied);
                                *remaining = remaining.saturating_sub(copied);
                            }
                        }
                    }
                }
            }
            None => {
                source_complete = preview_complete;
                let length = u64::try_from(preview.len()).unwrap_or(u64::MAX);
                total_bytes = length;
                let write = length.min(*remaining);
                let write = usize::try_from(write).unwrap_or(preview.len());
                file.write_all(&preview[..write])?;
                stored_bytes = write as u64;
                *remaining = remaining.saturating_sub(stored_bytes);
            }
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.sync_all()?;
        Ok(if source_complete && stored_bytes == total_bytes {
            RawTraceStatus::Complete
        } else {
            RawTraceStatus::Truncated
        })
    })();
    if result.is_err() {
        drop(file);
        let _ = std::fs::remove_file(path);
    }
    result
}

fn open_private_trace_file(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    let file = std::fs::File::from(descriptor);
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "trace file is not a private regular file",
        ));
    }
    Ok(file)
}

/// Bound the trace directory by file count and total stored bytes, dropping the
/// oldest retained streams first while leaving unrelated entries untouched.
#[cfg(test)]
fn prune_trace_dir(dir: &Path, cap: usize) {
    prune_trace_dir_with_byte_cap(dir, cap, TRACE_DIR_TOTAL_BYTES_CAP);
}

#[cfg(test)]
fn prune_trace_dir_with_byte_cap(dir: &Path, cap: usize, byte_cap: u64) {
    prune_trace_dir_with_byte_cap_protecting(dir, cap, byte_cap, &[]);
}

fn prune_trace_dir_protecting(dir: &Path, cap: usize, protected: &[&Path]) {
    prune_trace_dir_with_byte_cap_protecting(dir, cap, TRACE_DIR_TOTAL_BYTES_CAP, protected);
}

fn prune_trace_dir_with_byte_cap_protecting(
    dir: &Path,
    cap: usize,
    byte_cap: u64,
    protected: &[&Path],
) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let Ok(dir_metadata) = std::fs::symlink_metadata(dir) else {
        return;
    };
    if !dir_metadata.is_dir()
        || dir_metadata.file_type().is_symlink()
        || dir_metadata.uid() != rustix::process::geteuid().as_raw()
        || dir_metadata.permissions().mode() & 0o077 != 0
    {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf, u64)> = entries
        .flatten()
        .filter_map(|e| {
            if !is_agsh_trace_filename(&e.file_name()) || !e.file_type().ok()?.is_file() {
                return None;
            }
            let path = e.path();
            let metadata = e.metadata().ok()?;
            if metadata.uid() != rustix::process::geteuid().as_raw() {
                return None;
            }
            let mtime = metadata.modified().ok()?;
            Some((mtime, path, metadata.len()))
        })
        .collect();
    let mut total_bytes = files
        .iter()
        .map(|(_, _, bytes)| *bytes)
        .fold(0u64, u64::saturating_add);
    if files.len() <= cap && total_bytes <= byte_cap {
        return;
    }
    files.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
    let mut remaining = files.len();
    for (_, path, bytes) in files {
        if remaining <= cap && total_bytes <= byte_cap {
            break;
        }
        if protected.iter().any(|protected| path == *protected) {
            continue;
        }
        if std::fs::remove_file(path).is_ok() {
            remaining = remaining.saturating_sub(1);
            total_bytes = total_bytes.saturating_sub(bytes);
        }
    }
}

fn prune_stale_trace_spools(dir: &Path) {
    use std::os::unix::fs::MetadataExt;

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(pid) = name
            .strip_prefix(".capture-")
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() || trace_process_alive(pid) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if file_type.is_file() && metadata.uid() == rustix::process::geteuid().as_raw() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn trace_process_alive(pid: u32) -> bool {
    let Some(pid) = rustix::process::Pid::from_raw(pid as i32) else {
        return false;
    };
    match rustix::process::test_kill_process(pid) {
        Ok(()) => true,
        Err(error) => error == rustix::io::Errno::PERM,
    }
}

fn is_agsh_trace_filename(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    if !matches!(extension, "out" | "err") {
        return false;
    }
    let Some((pid, id)) = stem.split_once("_cmd_") else {
        return false;
    };
    !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && id.len() == 16
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod variable_tests {
    use super::ShellState;
    use agsh_core::Value;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn reassigning_an_exported_variable_updates_the_child_environment() {
        let mut state = ShellState::from_current_process();
        state.export_var("AGSH_EXPORTED_REASSIGN_TEST", "old");

        state.set_var("AGSH_EXPORTED_REASSIGN_TEST", "new");

        assert_eq!(state.lookup("AGSH_EXPORTED_REASSIGN_TEST"), Some("new"));
        assert_eq!(
            state
                .exported_env()
                .get("AGSH_EXPORTED_REASSIGN_TEST")
                .map(String::as_str),
            Some("new")
        );

        state.set_value(
            "AGSH_EXPORTED_REASSIGN_TEST",
            Value::String("newer".to_string()),
        );
        assert_eq!(
            state
                .exported_env()
                .get("AGSH_EXPORTED_REASSIGN_TEST")
                .map(String::as_str),
            Some("newer")
        );
    }

    #[test]
    fn temporary_export_removal_preserves_readonly_shell_value() {
        let mut state = ShellState::from_current_process();
        state.export_var("AGSH_PRELAUNCH_ENV_TEST", "secret");
        state.mark_readonly("AGSH_PRELAUNCH_ENV_TEST");

        let saved = state
            .take_exported_env("AGSH_PRELAUNCH_ENV_TEST")
            .expect("exported value");
        assert_eq!(state.lookup("AGSH_PRELAUNCH_ENV_TEST"), Some("secret"));
        assert!(!state.exported_env().contains_key("AGSH_PRELAUNCH_ENV_TEST"));

        state.restore_exported_env("AGSH_PRELAUNCH_ENV_TEST".to_string(), saved);
        assert_eq!(
            state
                .exported_env()
                .get("AGSH_PRELAUNCH_ENV_TEST")
                .map(String::as_str),
            Some("secret")
        );
        assert!(state.is_readonly("AGSH_PRELAUNCH_ENV_TEST"));
    }

    #[test]
    fn project_env_restores_each_complete_binding_once() {
        let mut state = ShellState::from_current_process();
        for name in [
            "AGSH_PROJECT_LOCAL",
            "AGSH_PROJECT_INTEGER",
            "AGSH_PROJECT_READONLY",
            "AGSH_PROJECT_ARRAY",
            "AGSH_PROJECT_ASSOC",
            "AGSH_PROJECT_EXPORTED_UNSET",
        ] {
            let _ = state.unset(name);
        }

        state.set_var("AGSH_PROJECT_LOCAL", "outer");
        state.export_var("AGSH_PROJECT_INTEGER", "17");
        state.mark_integer("AGSH_PROJECT_INTEGER");
        state.set_var("AGSH_PROJECT_READONLY", "fixed");
        state.mark_readonly("AGSH_PROJECT_READONLY");
        state.set_array(
            "AGSH_PROJECT_ARRAY",
            vec!["zero".to_string(), "one".to_string()],
            false,
        );
        state.mark_exported("AGSH_PROJECT_ARRAY");
        state.set_assoc(
            "AGSH_PROJECT_ASSOC",
            vec![("key".to_string(), "value".to_string())],
            false,
        );
        state.mark_exported("AGSH_PROJECT_EXPORTED_UNSET");

        state.apply_project_env_snapshot(
            PathBuf::from("/trusted/project"),
            vec![
                ("AGSH_PROJECT_LOCAL".to_string(), "first".to_string()),
                ("AGSH_PROJECT_LOCAL".to_string(), "second".to_string()),
                ("AGSH_PROJECT_INTEGER".to_string(), "99".to_string()),
                ("AGSH_PROJECT_READONLY".to_string(), "changed".to_string()),
                ("AGSH_PROJECT_ARRAY".to_string(), "changed".to_string()),
                ("AGSH_PROJECT_ASSOC".to_string(), "changed".to_string()),
                (
                    "AGSH_PROJECT_EXPORTED_UNSET".to_string(),
                    "temporary".to_string(),
                ),
            ],
        );
        assert_eq!(state.lookup("AGSH_PROJECT_LOCAL"), Some("second"));
        assert!(state.is_exported("AGSH_PROJECT_LOCAL"));

        state.deactivate_project_env();

        assert_eq!(state.lookup("AGSH_PROJECT_LOCAL"), Some("outer"));
        assert!(!state.is_exported("AGSH_PROJECT_LOCAL"));
        assert_eq!(state.lookup("AGSH_PROJECT_INTEGER"), Some("17"));
        assert!(state.is_exported("AGSH_PROJECT_INTEGER"));
        assert!(state.is_integer("AGSH_PROJECT_INTEGER"));
        assert_eq!(state.lookup("AGSH_PROJECT_READONLY"), Some("fixed"));
        assert!(state.is_readonly("AGSH_PROJECT_READONLY"));
        assert_eq!(
            state.array("AGSH_PROJECT_ARRAY"),
            Some(&["zero".to_string(), "one".to_string()][..])
        );
        assert!(state.is_exported("AGSH_PROJECT_ARRAY"));
        assert_eq!(
            state.assoc_arrays.get("AGSH_PROJECT_ASSOC"),
            Some(&BTreeMap::from([("key".to_string(), "value".to_string())]))
        );
        assert!(!state.is_exported("AGSH_PROJECT_ASSOC"));
        assert_eq!(state.lookup("AGSH_PROJECT_EXPORTED_UNSET"), None);
        assert!(state.is_exported("AGSH_PROJECT_EXPORTED_UNSET"));
        assert!(!state
            .exported_env()
            .contains_key("AGSH_PROJECT_EXPORTED_UNSET"));
    }

    #[cfg(unix)]
    #[test]
    fn project_env_restores_opaque_exported_environment_value() {
        use std::ffi::{OsStr, OsString};
        use std::os::unix::ffi::OsStringExt;

        const NAME: &str = "AGSH_PROJECT_OPAQUE";
        let mut state = ShellState::from_current_process();
        state.clear_variable_binding(NAME);
        let opaque = OsString::from_vec(vec![b'a', 0xff, b'z']);
        state
            .opaque_env
            .insert(OsString::from(NAME), opaque.clone());
        state.exported_vars.insert(NAME.to_string());

        state.apply_project_env_snapshot(
            PathBuf::from("/trusted/project"),
            vec![(NAME.to_string(), "temporary".to_string())],
        );
        state.deactivate_project_env();

        assert_eq!(state.lookup(NAME), None);
        assert!(state.is_exported(NAME));
        assert_eq!(state.opaque_env.get(OsStr::new(NAME)), Some(&opaque));
    }
}

#[cfg(test)]
mod background_snapshot_tests {
    use super::{
        BackgroundSnapshot, BackgroundValue, ShellState, BACKGROUND_SNAPSHOT_MAX_BYTES,
        MAX_BACKGROUND_SNAPSHOT_COLLECTION_ENTRIES,
    };
    use agsh_core::Value;
    use std::collections::BTreeMap;
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    #[test]
    fn malformed_or_oversized_snapshot_does_not_mutate_state() {
        let mut state = ShellState::from_current_process();
        state.set_var("AGSH_SNAPSHOT_SENTINEL", "before");

        assert!(state.restore_background_snapshot(b"not json").is_err());
        assert_eq!(state.lookup("AGSH_SNAPSHOT_SENTINEL"), Some("before"));

        let oversized = vec![b'x'; BACKGROUND_SNAPSHOT_MAX_BYTES + 1];
        assert!(
            super::restore_background_snapshot_reader(&mut state, Cursor::new(oversized)).is_err()
        );
        assert_eq!(state.lookup("AGSH_SNAPSHOT_SENTINEL"), Some("before"));
    }

    #[test]
    fn restoring_shell_state_cannot_widen_existing_confinement() {
        let mut source = ShellState::from_current_process();
        source.set_var("AGSH_SNAPSHOT_VALUE", "child");
        let bytes = source.encode_background_snapshot().unwrap();

        let mut target = ShellState::from_current_process();
        target.set_confine(&["printf".to_string()]);
        target.restore_background_snapshot(&bytes).unwrap();

        let policy = target.confine_policy().expect("confinement remains active");
        assert!(policy.allows("printf"));
        assert!(!policy.allows("sh"));
        assert_eq!(target.lookup("AGSH_SNAPSHOT_VALUE"), Some("child"));
    }

    #[test]
    fn semantic_snapshot_limits_fail_before_mutating_or_launching() {
        let mut oversized_source = ShellState::from_current_process();
        oversized_source.set_array(
            "AGSH_TOO_MANY",
            vec![String::new(); MAX_BACKGROUND_SNAPSHOT_COLLECTION_ENTRIES + 1],
            false,
        );
        assert!(oversized_source.encode_background_snapshot().is_err());

        let valid = ShellState::from_current_process()
            .encode_background_snapshot()
            .unwrap();
        let mut forged: BackgroundSnapshot = serde_json::from_slice(&valid).unwrap();
        forged.arrays.insert(
            "AGSH_TOO_MANY".to_string(),
            vec![String::new(); MAX_BACKGROUND_SNAPSHOT_COLLECTION_ENTRIES + 1],
        );
        let forged = serde_json::to_vec(&forged).unwrap();
        assert!(forged.len() < BACKGROUND_SNAPSHOT_MAX_BYTES);

        let mut target = ShellState::from_current_process();
        target.set_var("AGSH_SNAPSHOT_SENTINEL", "before");
        assert!(target.restore_background_snapshot(&forged).is_err());
        assert_eq!(target.lookup("AGSH_SNAPSHOT_SENTINEL"), Some("before"));
    }

    #[test]
    fn nested_saved_values_are_bounded_before_encode_and_restore() {
        fn nested_value(depth: usize) -> Value {
            (0..depth).fold(Value::Null, |value, _| Value::List(vec![value]))
        }

        let mut oversized_source = ShellState::from_current_process();
        oversized_source.set_value("AGSH_NESTED_BASELINE", nested_value(65));
        oversized_source.apply_project_env_snapshot(
            PathBuf::from("/trusted/project"),
            vec![("AGSH_NESTED_BASELINE".to_string(), "project".to_string())],
        );
        assert!(oversized_source.encode_background_snapshot().is_err());

        let mut valid_source = ShellState::from_current_process();
        valid_source.set_value("AGSH_NESTED_BASELINE", Value::Null);
        valid_source.apply_project_env_snapshot(
            PathBuf::from("/trusted/project"),
            vec![("AGSH_NESTED_BASELINE".to_string(), "project".to_string())],
        );
        let valid = valid_source.encode_background_snapshot().unwrap();
        let mut forged: BackgroundSnapshot = serde_json::from_slice(&valid).unwrap();
        forged.active_env.as_mut().unwrap().saved[0].prior_value =
            Some((0..65).fold(BackgroundValue::Null, |value, _| {
                BackgroundValue::List(vec![value])
            }));
        let forged = serde_json::to_vec(&forged).unwrap();

        let mut target = ShellState::from_current_process();
        target.set_var("AGSH_SNAPSHOT_SENTINEL", "before");
        assert!(target.restore_background_snapshot(&forged).is_err());
        assert_eq!(target.lookup("AGSH_SNAPSHOT_SENTINEL"), Some("before"));
    }

    #[cfg(unix)]
    #[test]
    fn active_project_env_baseline_round_trips_without_losing_variable_semantics() {
        use std::ffi::{OsStr, OsString};
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        const TYPED: &str = "AGSH_BG_PROJECT_TYPED";
        const ARRAY: &str = "AGSH_BG_PROJECT_ARRAY";
        const ASSOC: &str = "AGSH_BG_PROJECT_ASSOC";
        const INTEGER: &str = "AGSH_BG_PROJECT_INTEGER";
        const READONLY: &str = "AGSH_BG_PROJECT_READONLY";
        const EXPORTED_UNSET: &str = "AGSH_BG_PROJECT_EXPORTED_UNSET";
        const OPAQUE: &str = "AGSH_BG_PROJECT_OPAQUE";

        let raw_path = PathBuf::from(OsString::from_vec(vec![b'/', b't', 0xff]));
        let typed = Value::Record(BTreeMap::from([
            ("bool".to_string(), Value::Bool(true)),
            ("bytes".to_string(), Value::Bytes(vec![0, 0xff])),
            ("float".to_string(), Value::Float(1.25)),
            ("int".to_string(), Value::Int(42)),
            (
                "list".to_string(),
                Value::List(vec![Value::Null, Value::String("text".to_string())]),
            ),
            ("path".to_string(), Value::Path(raw_path)),
        ]));
        let opaque = OsString::from_vec(vec![b'o', 0xff, b'z']);

        let mut source = ShellState::from_current_process();
        for name in [
            TYPED,
            ARRAY,
            ASSOC,
            INTEGER,
            READONLY,
            EXPORTED_UNSET,
            OPAQUE,
        ] {
            source.clear_variable_binding(name);
        }
        source.set_value(TYPED, typed.clone());
        source.set_array(ARRAY, vec!["zero".to_string(), "one".to_string()], false);
        source.mark_exported(ARRAY);
        source.set_assoc(ASSOC, vec![("key".to_string(), "value".to_string())], false);
        source.export_var(INTEGER, "17");
        source.mark_integer(INTEGER);
        source.set_var(READONLY, "fixed");
        source.mark_readonly(READONLY);
        source.mark_exported(EXPORTED_UNSET);
        source
            .opaque_env
            .insert(OsString::from(OPAQUE), opaque.clone());
        source.exported_vars.insert(OPAQUE.to_string());

        source.apply_project_env_snapshot(
            PathBuf::from(OsString::from_vec(vec![b'/', b'p', 0xff])),
            [
                TYPED,
                ARRAY,
                ASSOC,
                INTEGER,
                READONLY,
                EXPORTED_UNSET,
                OPAQUE,
            ]
            .into_iter()
            .map(|name| (name.to_string(), "project".to_string()))
            .collect(),
        );
        let encoded = source.encode_background_snapshot().unwrap();

        let mut restored = ShellState::from_current_process();
        restored.restore_background_snapshot(&encoded).unwrap();
        let active_dir = &restored.active_env.as_ref().unwrap().0;
        assert_eq!(active_dir.as_os_str().as_bytes(), &[b'/', b'p', 0xff]);
        restored.deactivate_project_env();

        assert_eq!(restored.lookup_value(TYPED), Some(&typed));
        assert_eq!(
            restored.array(ARRAY),
            Some(&["zero".to_string(), "one".to_string()][..])
        );
        assert!(restored.is_exported(ARRAY));
        assert_eq!(
            restored.assoc_arrays.get(ASSOC),
            Some(&BTreeMap::from([("key".to_string(), "value".to_string())]))
        );
        assert_eq!(restored.lookup(INTEGER), Some("17"));
        assert!(restored.is_exported(INTEGER));
        assert!(restored.is_integer(INTEGER));
        assert_eq!(restored.lookup(READONLY), Some("fixed"));
        assert!(restored.is_readonly(READONLY));
        assert_eq!(restored.lookup(EXPORTED_UNSET), None);
        assert!(restored.is_exported(EXPORTED_UNSET));
        assert!(!restored.exported_env().contains_key(EXPORTED_UNSET));
        assert_eq!(restored.opaque_env.get(OsStr::new(OPAQUE)), Some(&opaque));
        assert!(restored.is_exported(OPAQUE));
    }

    #[test]
    fn reported_completed_job_status_remains_waitable_by_pid() {
        let state = ShellState::from_current_process();
        let child = Command::new("sh")
            .args(["-c", "exit 9"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        state.register_job(child, "sh -c 'exit 9'");

        let mut reported = false;
        for _ in 0..100 {
            if !state.reap_finished_jobs().is_empty() {
                reported = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(reported, "background child did not finish in time");
        assert_eq!(state.wait_for_jobs(Some(&pid.to_string())), Some(9));
    }
}

#[cfg(test)]
mod git_cache_tests {
    use super::{GitContext, ShellState};
    use std::time::{Duration, Instant};

    #[test]
    fn prompt_git_context_returns_before_a_slow_refresh() {
        let mut state = ShellState::from_current_process();
        let cwd = std::env::temp_dir().join(format!(
            "agsh-prompt-git-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        state.set_cwd(cwd.clone());

        let started = Instant::now();
        let first = state.prompt_git_context_with(move |root| {
            std::thread::sleep(Duration::from_millis(250));
            Some(GitContext {
                root,
                branch: Some("async-branch".to_string()),
                dirty: Some(false),
                ahead: 0,
                behind: 0,
            })
        });
        assert_eq!(first, None);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "prompt waited for the Git refresh"
        );

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let cached = state
                .git_cache
                .lock()
                .ok()
                .and_then(|cache| cache.entry.as_ref().and_then(|entry| entry.context.clone()));
            if cached.as_ref().and_then(|git| git.branch.as_deref()) == Some("async-branch") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Git refresh never populated cache"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let cached = state.prompt_git_context_with(|_| panic!("fresh cache was recomputed"));
        assert_eq!(
            cached.and_then(|git| git.branch),
            Some("async-branch".to_string())
        );
    }
}

#[cfg(test)]
mod trace_reader_tests {
    use super::{CapturedTraceStreams, ShellState, MAX_ALLOCATED_TRACE_BYTES};
    use agsh_core::CommandId;
    use agsh_output::{CompactorConfig, RawStreamRef, RawTraceStatus};
    use agsh_store::trace::TraceRecord;
    use std::io::Read;

    #[test]
    fn allocating_trace_resolution_returns_a_status_marked_bounded_prefix() {
        let state = ShellState::from_current_process();
        let command_id = CommandId::new();
        state.traces.lock().unwrap().record(TraceRecord::new(
            &command_id,
            "large in-memory trace",
            0,
            vec![b'x'; MAX_ALLOCATED_TRACE_BYTES + 1],
            Vec::new(),
        ));

        let resolved = state
            .resolve_trace_with_status(&command_id.to_string())
            .unwrap();

        assert_eq!(resolved.bytes.len(), MAX_ALLOCATED_TRACE_BYTES);
        assert_eq!(resolved.status, RawTraceStatus::Truncated);
        assert!(state.resolve_trace(&command_id.to_string()).is_none());
    }

    #[test]
    fn persistence_failure_never_indexes_an_observation_preview_as_exact() {
        let mut state = ShellState::from_current_process();
        state.set_var("AGSH_TRACE_DIR", "/dev/null");
        let command_id = CommandId::new();
        let preview = b"head\n[agsh: bytes elided]\ntail\n";

        state.record_trace(&command_id, "unpersistable", 17, preview, b"diagnostic");

        assert!(state.trace_summaries().is_empty());
        assert!(state
            .open_trace_reader(&format!("trace://{command_id}/stdout"))
            .is_none());
        assert!(state.resolve_trace(&command_id.to_string()).is_none());
    }

    #[test]
    fn unavailable_trace_entry_has_status_but_no_readable_bytes() {
        let state = ShellState::from_current_process();
        let command_id = CommandId::new();
        state
            .disk_traces
            .lock()
            .unwrap()
            .push_back((command_id.to_string(), RawStreamRef::unavailable()));
        let reference = format!("trace://{command_id}/stdout");

        let mut reader = state.open_trace_reader(&reference).unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert!(bytes.is_empty());
        assert_eq!(reader.status, RawTraceStatus::Unavailable);

        let resolved = state.resolve_trace_with_status(&reference).unwrap();
        assert!(resolved.bytes.is_empty());
        assert_eq!(resolved.status, RawTraceStatus::Unavailable);
        assert!(state.resolve_trace(&reference).is_none());
    }

    #[test]
    fn incomplete_preview_without_exact_segments_never_becomes_complete() {
        let dir = std::env::temp_dir().join(format!(
            "agsh-incomplete-preview-{}-{}",
            std::process::id(),
            CommandId::new()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut state = ShellState::from_current_process();
        state.replace_output_config_for_test(CompactorConfig::default());
        state.set_var("AGSH_TRACE_DIR", dir.display().to_string());
        let command_id = CommandId::new();
        let preview = b"head\n[agsh: bytes elided]\ntail\n";

        let raw = state
            .record_trace_captured(
                &command_id,
                "bounded preview",
                0,
                CapturedTraceStreams {
                    stdout_preview: preview,
                    stderr_preview: b"exact diagnostic",
                    stdout_preview_complete: false,
                    stderr_preview_complete: true,
                    stdout_exact: None,
                    stderr_exact: None,
                },
            )
            .unwrap();

        assert_eq!(raw.stdout_status, RawTraceStatus::Truncated);
        assert_eq!(raw.stderr_status, RawTraceStatus::Complete);
        assert!(state
            .resolve_trace(&format!("trace://{command_id}/stdout"))
            .is_none());
        let resolved = state
            .resolve_trace_with_status(&format!("trace://{command_id}/stdout"))
            .unwrap();
        assert_eq!(resolved.bytes, preview);
        assert_eq!(resolved.status, RawTraceStatus::Truncated);

        let exact_id = CommandId::new();
        state.record_trace(&exact_id, "exact preview API", 0, b"exact bytes", b"");
        assert_eq!(
            state.resolve_trace(&format!("trace://{exact_id}/stdout")),
            Some(b"exact bytes".to_vec())
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod trace_dir_tests {
    use super::{
        absolute_trace_dir_base, absolute_trace_dir_base_with_default, anchor_trace_dir,
        persist_trace_segments_to_dir, persist_trace_to_dir, prepare_private_trace_dir,
        prune_trace_dir, prune_trace_dir_with_byte_cap, prune_trace_dir_with_byte_cap_protecting,
        CapturedTraceStreams, ShellState, TRACE_DIR_TOTAL_BYTES_CAP,
    };
    use agsh_core::CommandId;
    use agsh_output::{CompactorConfig, RawStorageOptions, RawTraceStatus};
    use std::io::Write;

    fn trace_dir(label: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agsh-trace-{label}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn trace_name(id: u64, extension: &str) -> String {
        format!("123_cmd_{id:016x}.{extension}")
    }

    #[test]
    fn unavailable_startup_cwd_uses_an_absolute_trace_anchor() {
        let base = absolute_trace_dir_base(std::path::Path::new("."));
        assert!(base.is_absolute(), "trace base was relative: {base:?}");
    }

    #[test]
    fn relative_default_trace_dir_uses_the_startup_anchor() {
        let base = std::path::Path::new("/absolute/startup");
        assert_eq!(
            anchor_trace_dir(base, None, std::path::PathBuf::from("relative-default")),
            base.join("relative-default")
        );
    }

    #[test]
    fn relative_default_is_anchored_once_when_startup_cwd_is_unavailable() {
        let default = std::path::Path::new("relative-temp/agsh-traces");
        let base = absolute_trace_dir_base_with_default(std::path::Path::new("."), default);

        assert_eq!(base, std::path::Path::new("/tmp"));
        assert_eq!(
            anchor_trace_dir(&base, None, default.to_path_buf()),
            std::path::Path::new("/tmp/relative-temp/agsh-traces")
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_relative_trace_path_is_rejected_before_storage() {
        use std::os::unix::ffi::OsStringExt;

        let mut bytes = format!("/tmp/agsh-non-utf8-trace-{}-", std::process::id()).into_bytes();
        bytes.push(0xff);
        let base = std::path::PathBuf::from(std::ffi::OsString::from_vec(bytes));
        let resolved = base.join("traces");
        let _ = std::fs::remove_dir_all(&base);
        let mut state = ShellState::from_current_process();
        state.replace_output_config_for_test(CompactorConfig::default());
        state.trace_dir_base = base.clone();
        state.set_var("AGSH_TRACE_DIR", "traces");

        let preflight = state.prepare_required_trace_storage().unwrap_err();
        assert_eq!(preflight.kind(), std::io::ErrorKind::InvalidInput);
        assert!(preflight
            .to_string()
            .contains("cannot be represented losslessly"));
        assert!(state.create_trace_spool("out").is_err());
        assert!(state
            .record_trace_captured(
                &CommandId::new(),
                "non-UTF-8 trace path",
                0,
                CapturedTraceStreams {
                    stdout_preview: b"exact bytes",
                    stderr_preview: b"",
                    stdout_preview_complete: true,
                    stderr_preview_complete: true,
                    stdout_exact: None,
                    stderr_exact: None,
                },
            )
            .is_err());
        assert!(state.disk_traces.lock().unwrap().is_empty());
        assert!(!resolved.exists());
    }

    #[test]
    fn relative_trace_dir_stays_anchored_across_shell_cwd_changes() {
        let root = trace_dir("relative-root");
        std::fs::create_dir(&root).unwrap();
        let expected = root.join("traces");
        let mut state = ShellState::from_current_process();
        state.replace_output_config_for_test(CompactorConfig::default());
        state.trace_dir_base = root.clone();
        state.set_cwd(root.clone());
        state.set_var("AGSH_TRACE_DIR", "traces");

        state.prepare_required_trace_storage().unwrap();
        assert_eq!(state.trace_dir(), expected);
        let first = CommandId::new();
        state.record_trace(&first, "before cd", 0, b"first stdout", b"");

        state.set_cwd(std::path::PathBuf::from("/"));
        state.prepare_required_trace_storage().unwrap();
        assert_eq!(state.trace_dir(), expected);
        let second = CommandId::new();
        state.record_trace(&second, "after cd", 0, b"second stdout", b"");

        let traces = state.disk_traces.lock().unwrap();
        assert_eq!(traces.len(), 2);
        assert!(traces.iter().all(|(_, raw)| {
            let stdout = std::path::Path::new(&raw.stdout);
            stdout.is_absolute() && stdout.parent() == Some(expected.as_path())
        }));
        drop(traces);
        assert_eq!(
            state.resolve_trace(&format!("trace://{first}/stdout")),
            Some(b"first stdout".to_vec())
        );
        assert_eq!(
            state.resolve_trace(&format!("trace://{second}/stdout")),
            Some(b"second stdout".to_vec())
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn externally_marked_spool_finishes_as_truncated_with_equal_byte_counts() {
        let root = trace_dir("external-cutoff");
        std::fs::create_dir(&root).unwrap();
        let mut state = ShellState::from_current_process();
        state.replace_output_config_for_test(CompactorConfig::default());
        state.trace_dir_base = root.clone();
        state.set_var("AGSH_TRACE_DIR", "traces");

        let mut spool = state.create_trace_spool("out").unwrap();
        let incomplete = spool.incomplete_marker();
        spool.write_all(b"observed bytes").unwrap();
        incomplete.mark_incomplete();
        let exact = spool.finish().unwrap();

        assert_eq!(exact.status, RawTraceStatus::Truncated);
        assert_eq!(exact.total_bytes, exact.stored_bytes);
        drop(exact);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn prune_keeps_newest_and_bounds_the_dir() {
        let dir = trace_dir("prune");
        prepare_private_trace_dir(&dir).unwrap();
        // Write 50 files with increasing mtimes (name order == age order here).
        for i in 0..50u32 {
            let path = dir.join(trace_name(i as u64, "out"));
            std::fs::write(&path, b"x").unwrap();
            // Bump mtime deterministically so "oldest" is well-defined.
            let t =
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 + i as u64);
            let _ = filetime_set(&path, t);
        }
        prune_trace_dir(&dir, 10);
        let remaining: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(remaining.len(), 10, "dir must be bounded to the cap");
        // The 10 kept must be the newest (ids 40..49).
        assert!(
            remaining
                .iter()
                .all(|name| { (40..50).any(|id| name == &trace_name(id, "out")) }),
            "kept the newest: {remaining:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_also_bounds_total_trace_bytes() {
        let dir = trace_dir("prune-bytes");
        prepare_private_trace_dir(&dir).unwrap();
        for id in 0..3u64 {
            let path = dir.join(trace_name(id, "out"));
            std::fs::write(&path, vec![id as u8; 600]).unwrap();
            let timestamp =
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 + id);
            filetime_set(&path, timestamp).unwrap();
        }

        prune_trace_dir_with_byte_cap(&dir, 10, 1000);

        let remaining = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .collect::<Vec<_>>();
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining[0].file_name().to_string_lossy(),
            trace_name(2, "out")
        );
        assert!(remaining[0].metadata().unwrap().len() <= 1000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistence_never_returns_paths_pruned_by_a_future_dated_oversized_trace() {
        let dir = trace_dir("prune-protected-persistence");
        prepare_private_trace_dir(&dir).unwrap();
        let hostile = dir.join(trace_name(0, "out"));
        let hostile_file = std::fs::File::create(&hostile).unwrap();
        hostile_file
            .set_len(TRACE_DIR_TOTAL_BYTES_CAP.saturating_add(1))
            .unwrap();
        drop(hostile_file);
        filetime_set(
            &hostile,
            std::time::SystemTime::now() + std::time::Duration::from_secs(24 * 60 * 60),
        )
        .unwrap();

        let raw = persist_trace_segments_to_dir(
            &dir,
            &CommandId::new(),
            CapturedTraceStreams {
                stdout_preview: b"exact stdout",
                stderr_preview: b"exact stderr",
                stdout_preview_complete: true,
                stderr_preview_complete: true,
                stdout_exact: None,
                stderr_exact: None,
            },
            RawStorageOptions {
                enabled: true,
                max_bytes: 1024,
            },
        )
        .unwrap();

        assert_eq!(raw.stdout_status, RawTraceStatus::Complete);
        assert_eq!(raw.stderr_status, RawTraceStatus::Complete);
        assert_eq!(std::fs::read(&raw.stdout).unwrap(), b"exact stdout");
        assert_eq!(std::fs::read(&raw.stderr).unwrap(), b"exact stderr");
        assert!(!hostile.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn protected_trace_pair_counts_toward_an_exact_file_cap() {
        let dir = trace_dir("prune-protected-count");
        prepare_private_trace_dir(&dir).unwrap();
        let protected_stdout = dir.join(trace_name(0, "out"));
        let protected_stderr = dir.join(trace_name(0, "err"));
        for path in [&protected_stdout, &protected_stderr] {
            std::fs::write(path, b"protected").unwrap();
            filetime_set(path, std::time::SystemTime::UNIX_EPOCH).unwrap();
        }
        for id in 1..=4 {
            let path = dir.join(trace_name(id, "out"));
            std::fs::write(&path, b"discard").unwrap();
            filetime_set(
                &path,
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 + id),
            )
            .unwrap();
        }

        prune_trace_dir_with_byte_cap_protecting(
            &dir,
            2,
            u64::MAX,
            &[protected_stdout.as_path(), protected_stderr.as_path()],
        );

        assert!(protected_stdout.exists());
        assert!(protected_stderr.exists());
        assert_eq!(
            std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                .count(),
            2
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persisted_trace_directory_and_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = trace_dir("private");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let cmd_id = CommandId::new();

        persist_trace_to_dir(&dir, &cmd_id, b"stdout secret", b"stderr secret").unwrap();

        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o7777,
            0o700
        );
        for extension in ["out", "err"] {
            let path = dir.join(format!("{}_{}.{}", std::process::id(), cmd_id, extension));
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o7777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persisted_byte_limit_is_shared_across_stdout_and_stderr() {
        let dir = trace_dir("shared-byte-limit");
        let cmd_id = CommandId::new();
        let stdout = [b'o'; 800];
        let stderr = [b'e'; 800];
        let raw = persist_trace_segments_to_dir(
            &dir,
            &cmd_id,
            CapturedTraceStreams {
                stdout_preview: &stdout,
                stderr_preview: &stderr,
                stdout_preview_complete: true,
                stderr_preview_complete: true,
                stdout_exact: None,
                stderr_exact: None,
            },
            RawStorageOptions {
                enabled: true,
                max_bytes: 1024,
            },
        )
        .unwrap();

        let stdout_size = std::fs::metadata(&raw.stdout).unwrap().len();
        let stderr_size = std::fs::metadata(&raw.stderr).unwrap().len();
        assert_eq!(stdout_size + stderr_size, 1024);
        assert_eq!(raw.stdout_status, RawTraceStatus::Complete);
        assert_eq!(raw.stderr_status, RawTraceStatus::Truncated);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_raw_storage_writes_no_directory_or_trace_files() {
        let dir = trace_dir("storage-disabled");
        let raw = persist_trace_segments_to_dir(
            &dir,
            &CommandId::new(),
            CapturedTraceStreams {
                stdout_preview: b"secret stdout",
                stderr_preview: b"secret stderr",
                stdout_preview_complete: true,
                stderr_preview_complete: true,
                stdout_exact: None,
                stderr_exact: None,
            },
            RawStorageOptions {
                enabled: false,
                max_bytes: 0,
            },
        )
        .unwrap();

        assert_eq!(raw.stdout_status, RawTraceStatus::Disabled);
        assert_eq!(raw.stderr_status, RawTraceStatus::Disabled);
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn trace_file_symlink_is_not_followed_or_truncated() {
        use std::os::unix::fs::symlink;

        let dir = trace_dir("file-symlink");
        prepare_private_trace_dir(&dir).unwrap();
        let victim = dir.parent().unwrap().join(format!(
            "agsh-trace-victim-{}-{}",
            std::process::id(),
            CommandId::new()
        ));
        std::fs::write(&victim, b"do not overwrite").unwrap();
        let cmd_id = CommandId::new();
        let hostile = dir.join(format!("{}_{}.out", std::process::id(), cmd_id));
        symlink(&victim, &hostile).unwrap();

        assert!(persist_trace_to_dir(&dir, &cmd_id, b"hostile", b"stderr").is_err());
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "do not overwrite"
        );
        assert!(std::fs::symlink_metadata(&hostile)
            .unwrap()
            .file_type()
            .is_symlink());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(victim);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_trace_directory_is_rejected() {
        use std::os::unix::fs::symlink;

        let real = trace_dir("real-dir");
        let link = trace_dir("dir-symlink");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, &link).unwrap();

        assert!(persist_trace_to_dir(&link, &CommandId::new(), b"stdout", b"stderr").is_err());
        assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
        let _ = std::fs::remove_file(link);
        let _ = std::fs::remove_dir_all(real);
    }

    #[cfg(unix)]
    #[test]
    fn prune_preserves_unrelated_files_directories_and_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = trace_dir("prune-unrelated");
        prepare_private_trace_dir(&dir).unwrap();
        for id in 0..5 {
            let path = dir.join(trace_name(id, "out"));
            std::fs::write(&path, b"trace").unwrap();
            let timestamp =
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 + id);
            filetime_set(&path, timestamp).unwrap();
        }
        let unrelated = dir.join("user-notes.txt");
        std::fs::write(&unrelated, b"keep").unwrap();
        let unrelated_dir = dir.join("nested");
        std::fs::create_dir(&unrelated_dir).unwrap();
        let victim = dir.parent().unwrap().join(format!(
            "agsh-prune-victim-{}-{}",
            std::process::id(),
            CommandId::new()
        ));
        std::fs::write(&victim, b"victim").unwrap();
        let matching_symlink = dir.join(trace_name(99, "err"));
        symlink(&victim, &matching_symlink).unwrap();

        prune_trace_dir(&dir, 2);

        let trace_count = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| super::is_agsh_trace_filename(&entry.file_name()))
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .count();
        assert_eq!(trace_count, 2);
        assert_eq!(std::fs::read_to_string(unrelated).unwrap(), "keep");
        assert!(unrelated_dir.is_dir());
        assert!(std::fs::symlink_metadata(matching_symlink)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "victim");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(victim);
    }

    #[test]
    fn prune_ignores_files_that_only_resemble_trace_names() {
        let dir = trace_dir("prune-names");
        prepare_private_trace_dir(&dir).unwrap();
        let unrelated = [
            "123_cmd_1.out",
            "123_cmd_000000000000000g.out",
            "pid_cmd_0000000000000001.out",
            "123_cmd_0000000000000001.log",
            "123_cmd_0000000000000001.out.backup",
        ];
        for name in unrelated {
            std::fs::write(dir.join(name), b"keep").unwrap();
        }
        for id in 0..4 {
            std::fs::write(dir.join(trace_name(id, "err")), b"trace").unwrap();
        }

        prune_trace_dir(&dir, 2);

        assert!(unrelated.iter().all(|name| dir.join(name).exists()));
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            unrelated.len() + 2
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Set mtime explicitly so "oldest" is deterministic regardless of write speed.
    fn filetime_set(path: &std::path::Path, t: std::time::SystemTime) -> std::io::Result<()> {
        let f = std::fs::OpenOptions::new().write(true).open(path)?;
        let times = std::fs::FileTimes::new().set_accessed(t).set_modified(t);
        f.set_times(times)
    }
}
