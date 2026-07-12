//! Rich, persistent command history with frecency ranking.
//!
//! Each entry records the command plus context (cwd, exit status, start time,
//! duration, hostname, project). The store ranks by *frecency* (frequency +
//! recency) for autosuggestions and fuzzy search, and persists as JSONL so a
//! session can be reconstructed. Ranking helpers take `now` explicitly so they
//! are deterministic and testable.

use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One executed command and its outcome/context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub command: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Unix seconds when the command started.
    #[serde(default)]
    pub started_at: u64,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub project: Option<String>,
    /// User account that ran the command, when known.
    #[serde(default)]
    pub user: Option<String>,
    /// agsh interactive session id (`$AGSH_SESSION`), when journaling is active.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Git worktree root for the command's cwd, when known.
    #[serde(default)]
    pub git_root: Option<String>,
    /// Git branch/detached label at command start, when known.
    #[serde(default)]
    pub git_branch: Option<String>,
    /// agsh output mode active for the command, when known.
    #[serde(default)]
    pub output_mode: Option<String>,
    /// Raw trace id for captured output, when one is attached.
    #[serde(default)]
    pub trace_id: Option<String>,
    /// First executable/builtin word, normalized for filtering and stats.
    #[serde(default)]
    pub command_family: Option<String>,
}

impl HistoryEntry {
    pub fn new(command: impl Into<String>, cwd: impl Into<String>, started_at: u64) -> Self {
        Self {
            command: command.into(),
            cwd: cwd.into(),
            exit_code: None,
            started_at,
            duration_ms: 0,
            hostname: String::new(),
            project: None,
            user: None,
            session_id: None,
            git_root: None,
            git_branch: None,
            output_mode: None,
            trace_id: None,
            command_family: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryScope {
    Global,
    Host(String),
    Session(String),
    Cwd(String),
    Project(String),
    GitRoot(String),
    Failures,
    LongRunning { min_ms: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Fuzzy,
    Prefix,
    FullText,
    Exact,
    Family,
}

#[derive(Debug, Clone)]
pub struct HistoryQuery {
    pub text: String,
    pub mode: SearchMode,
    pub scope: HistoryScope,
    pub limit: usize,
    pub exit_code: Option<i32>,
    pub failed: bool,
    pub after: Option<u64>,
    pub before: Option<u64>,
    pub cwd: Option<String>,
    pub project: Option<String>,
    pub min_duration_ms: Option<u64>,
    pub max_duration_ms: Option<u64>,
    pub dedupe: bool,
}

impl Default for HistoryQuery {
    fn default() -> Self {
        Self {
            text: String::new(),
            mode: SearchMode::Fuzzy,
            scope: HistoryScope::Global,
            limit: 50,
            exit_code: None,
            failed: false,
            after: None,
            before: None,
            cwd: None,
            project: None,
            min_duration_ms: None,
            max_duration_ms: None,
            dedupe: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HistoryMatch {
    pub entry: HistoryEntry,
    /// 1-based position in the history file.
    pub index: usize,
    pub score: i64,
    /// Number of matching entries with the same command when de-duping.
    pub count: usize,
}

#[derive(Debug, Clone, Default)]
pub struct HistoryStats {
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub most_used: Vec<(String, usize)>,
    pub by_family: Vec<(String, usize)>,
    pub slowest: Vec<HistoryEntry>,
}

/// In-memory history with optional JSONL persistence.
#[derive(Debug)]
pub struct HistoryStore {
    entries: Vec<HistoryEntry>,
    path: Option<PathBuf>,
    max: usize,
    retained_bytes: usize,
}

impl Default for HistoryStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

const DEFAULT_MAX: usize = 50_000;
const MAX_HISTORY_LINE_BYTES: usize = 1024 * 1024;
const MAX_HISTORY_COMMAND_BYTES: usize = 256 * 1024;
const MAX_HISTORY_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HISTORY_MEMORY_BYTES: usize = 32 * 1024 * 1024;

fn history_entry_bytes(entry: &HistoryEntry) -> usize {
    entry
        .command
        .len()
        .saturating_add(entry.cwd.len())
        .saturating_add(entry.hostname.len())
        .saturating_add(entry.user.as_ref().map_or(0, String::len))
        .saturating_add(entry.project.as_ref().map_or(0, String::len))
        .saturating_add(entry.session_id.as_ref().map_or(0, String::len))
        .saturating_add(entry.git_root.as_ref().map_or(0, String::len))
        .saturating_add(entry.git_branch.as_ref().map_or(0, String::len))
        .saturating_add(entry.output_mode.as_ref().map_or(0, String::len))
        .saturating_add(entry.trace_id.as_ref().map_or(0, String::len))
        .saturating_add(entry.command_family.as_ref().map_or(0, String::len))
        .saturating_add(std::mem::size_of::<HistoryEntry>())
}

fn trim_entries_to_bytes(
    entries: &mut Vec<HistoryEntry>,
    retained_bytes: &mut usize,
    limit: usize,
) {
    if *retained_bytes <= limit {
        return;
    }
    let target = limit.saturating_mul(9) / 10;
    let mut drop_count = 0usize;
    let mut dropped_bytes = 0usize;
    while retained_bytes.saturating_sub(dropped_bytes) > target && drop_count < entries.len() {
        dropped_bytes = dropped_bytes.saturating_add(history_entry_bytes(&entries[drop_count]));
        drop_count += 1;
    }
    entries.drain(0..drop_count);
    *retained_bytes = retained_bytes.saturating_sub(dropped_bytes);
}

enum BoundedLine {
    Eof,
    Line,
    Oversized,
}

fn read_bounded_line(
    reader: &mut impl BufRead,
    output: &mut Vec<u8>,
    limit: usize,
) -> io::Result<BoundedLine> {
    output.clear();
    let mut oversized = false;

    loop {
        let (consumed, ended) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Ok(if oversized {
                    BoundedLine::Oversized
                } else if output.is_empty() {
                    BoundedLine::Eof
                } else {
                    BoundedLine::Line
                });
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
            if !oversized {
                let remaining = limit.saturating_add(1).saturating_sub(output.len());
                output.extend_from_slice(&available[..consumed.min(remaining)]);
                if output.len() > limit {
                    output.clear();
                    oversized = true;
                }
            }
            (consumed, newline.is_some())
        };
        reader.consume(consumed);
        if ended {
            return Ok(if oversized {
                BoundedLine::Oversized
            } else {
                BoundedLine::Line
            });
        }
    }
}

fn ensure_parent_dir(path: &Path) -> io::Result<()> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent)
}

#[cfg(unix)]
fn make_file_private(file: &std::fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = file.metadata()?.permissions();
    if permissions.mode() & 0o7777 != 0o600 {
        permissions.set_mode(0o600);
        file.set_permissions(permissions)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_file_private(_file: &std::fs::File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn open_history_for_read(path: &Path) -> io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    let file = std::fs::File::from(descriptor);
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "history path is not a regular file",
        ));
    }
    make_file_private(&file)?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_history_for_read(path: &Path) -> io::Result<std::fs::File> {
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "history path is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_history_for_append(path: &Path) -> io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::open(
        path,
        OFlags::WRONLY
            | OFlags::CREATE
            | OFlags::APPEND
            | OFlags::CLOEXEC
            | OFlags::NOFOLLOW
            | OFlags::NONBLOCK,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    let file = std::fs::File::from(descriptor);
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "history path is not a regular file",
        ));
    }
    make_file_private(&file)?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_history_for_append(path: &Path) -> io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "history path is not a regular file",
        ));
    }
    Ok(file)
}

fn history_lock_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "history".into());
    path.with_file_name(format!(".{name}.lock"))
}

struct HistoryLock {
    file: std::fs::File,
}

impl Drop for HistoryLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = rustix::fs::flock(&self.file, rustix::fs::FlockOperation::Unlock);
    }
}

#[cfg(unix)]
fn lock_history(path: &Path) -> io::Result<HistoryLock> {
    use rustix::fs::{FlockOperation, Mode, OFlags};

    ensure_parent_dir(path)?;
    let descriptor = rustix::fs::open(
        history_lock_path(path),
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    let file = std::fs::File::from(descriptor);
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "history lock path is not a regular file",
        ));
    }
    make_file_private(&file)?;
    loop {
        match rustix::fs::flock(&file, FlockOperation::LockExclusive) {
            Ok(()) => break,
            Err(error) => {
                let error = io::Error::from_raw_os_error(error.raw_os_error());
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
    Ok(HistoryLock { file })
}

#[cfg(unix)]
fn try_lock_history(path: &Path) -> io::Result<HistoryLock> {
    use rustix::fs::{FlockOperation, Mode, OFlags};

    ensure_parent_dir(path)?;
    let descriptor = rustix::fs::open(
        history_lock_path(path),
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    let file = std::fs::File::from(descriptor);
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "history lock path is not a regular file",
        ));
    }
    make_file_private(&file)?;
    rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive)
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    Ok(HistoryLock { file })
}

#[cfg(not(unix))]
fn lock_history(path: &Path) -> io::Result<HistoryLock> {
    ensure_parent_dir(path)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(history_lock_path(path))?;
    Ok(HistoryLock { file })
}

#[cfg(not(unix))]
fn try_lock_history(path: &Path) -> io::Result<HistoryLock> {
    lock_history(path)
}

fn create_rewrite_temp(path: &Path) -> io::Result<(PathBuf, std::fs::File)> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("history");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);

    for _ in 0..128 {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(
            ".{name}.tmp.{}.{stamp:x}.{sequence:x}",
            std::process::id()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temp) {
            Ok(file) => {
                if let Err(error) = make_file_private(&file) {
                    drop(file);
                    let _ = std::fs::remove_file(&temp);
                    return Err(error);
                }
                return Ok((temp, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a history rewrite file",
    ))
}

impl HistoryStore {
    pub fn in_memory() -> Self {
        Self {
            entries: Vec::new(),
            path: None,
            max: DEFAULT_MAX,
            retained_bytes: 0,
        }
    }

    /// Open a history store backed by `path`, loading existing entries (capped
    /// to the most recent `max`). A missing or unreadable file starts empty.
    ///
    /// The file is streamed line-by-line (not slurped whole) and the in-memory
    /// set is kept bounded during load, so a large log doesn't spike memory. If
    /// the on-disk log has grown well past the retained window it is compacted
    /// in place, so it cannot grow without bound across sessions.
    pub fn with_file(path: PathBuf, max: usize) -> Self {
        Self::with_file_load_limit(path, max, MAX_HISTORY_FILE_BYTES)
    }

    fn with_file_load_limit(path: PathBuf, max: usize, load_limit: u64) -> Self {
        let max = max.max(1);
        // Loading is allowed to proceed from its open-file snapshot when another
        // shell is writing. Only optional compaction requires the nonblocking
        // guard, so startup never waits behind another process.
        let rewrite_guard = try_lock_history(&path).ok();
        let mut entries: Vec<HistoryEntry> = Vec::new();
        let mut retained_bytes = 0usize;
        let mut total = 0usize;
        let mut needs_rewrite = false;
        if let Ok(mut file) = open_history_for_read(&path) {
            // Read line-by-line as bytes and lossy-decode: a corrupt/non-UTF8 line
            // just fails to parse and is skipped, instead of truncating the whole
            // (newest) history the way `.lines().map_while(Result::ok)` did at the
            // first bad byte. (Also avoids the `lines_filter_map_ok` lint, whose
            // suggested `map_while` is exactly that truncating behavior.)
            // Oversized files are read from a bounded tail so startup work cannot
            // be amplified by a sparse/corrupt file or a writer that ignores the
            // lock. The first partial tail line is discarded before JSON parsing.
            let file_len = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            let start = file_len.saturating_sub(load_limit);
            let discard_partial_line = if start > 0 {
                let mut previous = [0u8; 1];
                file.seek(SeekFrom::Start(start - 1))
                    .and_then(|_| file.read_exact(&mut previous))
                    .is_err()
                    || previous[0] != b'\n'
            } else {
                false
            };
            if file.seek(SeekFrom::Start(start)).is_err() {
                return Self {
                    entries,
                    path: Some(path),
                    max,
                    retained_bytes,
                };
            }
            let mut reader = BufReader::new(file).take(load_limit);
            let mut buf: Vec<u8> = Vec::new();
            if start > 0 {
                needs_rewrite = true;
            }
            if discard_partial_line {
                match read_bounded_line(&mut reader, &mut buf, MAX_HISTORY_LINE_BYTES) {
                    Ok(BoundedLine::Eof) | Err(_) => {}
                    Ok(BoundedLine::Line | BoundedLine::Oversized) => {}
                }
            }
            loop {
                match read_bounded_line(&mut reader, &mut buf, MAX_HISTORY_LINE_BYTES) {
                    Ok(BoundedLine::Eof) | Err(_) => break,
                    Ok(BoundedLine::Oversized) => {
                        needs_rewrite = true;
                        continue;
                    }
                    Ok(BoundedLine::Line) => {}
                }
                let Ok(line) = std::str::from_utf8(&buf) else {
                    needs_rewrite = true;
                    continue;
                };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(entry) = serde_json::from_str::<HistoryEntry>(line) {
                    total += 1;
                    retained_bytes = retained_bytes.saturating_add(history_entry_bytes(&entry));
                    entries.push(entry);
                    // Keep load memory bounded: never hold much more than `max`.
                    if entries.len() > max.saturating_mul(2) {
                        let drop = entries.len() - max;
                        let dropped = entries[..drop]
                            .iter()
                            .map(history_entry_bytes)
                            .fold(0usize, usize::saturating_add);
                        entries.drain(0..drop);
                        retained_bytes = retained_bytes.saturating_sub(dropped);
                    }
                    trim_entries_to_bytes(
                        &mut entries,
                        &mut retained_bytes,
                        MAX_HISTORY_MEMORY_BYTES,
                    );
                } else {
                    needs_rewrite = true;
                }
            }
        }
        if entries.len() > max {
            let drop = entries.len() - max;
            let dropped = entries[..drop]
                .iter()
                .map(history_entry_bytes)
                .fold(0usize, usize::saturating_add);
            entries.drain(0..drop);
            retained_bytes = retained_bytes.saturating_sub(dropped);
        }
        let store = Self {
            entries,
            path: Some(path),
            max,
            retained_bytes,
        };
        // Compact a log that has outgrown the retained window down to `max`.
        if rewrite_guard.is_some() && (needs_rewrite || total > max.saturating_mul(2)) {
            store.rewrite();
        }
        store
    }

    /// Atomically rewrite the backing file with the current (bounded) entries,
    /// via a temp file + rename, so a crash or a concurrent reader never sees a
    /// half-written log.
    fn rewrite(&self) {
        let Some(path) = &self.path else { return };
        let _ = (|| -> io::Result<()> {
            ensure_parent_dir(path)?;
            let (temp, mut file) = create_rewrite_temp(path)?;
            for entry in &self.entries {
                let Ok(mut line) = serde_json::to_string(entry) else {
                    continue;
                };
                line.push('\n');
                if line.len() > MAX_HISTORY_LINE_BYTES {
                    continue;
                }
                if let Err(error) = file.write_all(line.as_bytes()) {
                    let _ = std::fs::remove_file(&temp);
                    return Err(error);
                }
            }
            if let Err(error) = file.sync_all() {
                let _ = std::fs::remove_file(&temp);
                return Err(error);
            }
            drop(file);
            if let Err(error) = std::fs::rename(&temp, path) {
                let _ = std::fs::remove_file(&temp);
                return Err(error);
            }
            #[cfg(unix)]
            if let Some(parent) = path.parent() {
                if let Ok(directory) = std::fs::File::open(parent) {
                    let _ = directory.sync_all();
                }
            }
            Ok(())
        })();
    }

    /// Append a new (not-yet-finalized) entry; returns its index.
    pub fn push(&mut self, mut entry: HistoryEntry) -> usize {
        if entry.command.len() > MAX_HISTORY_COMMAND_BYTES {
            entry.command = format!(
                "# agsh: command omitted from history (exceeded {MAX_HISTORY_COMMAND_BYTES} bytes)"
            );
        }
        self.entries.push(entry);
        self.retained_bytes = self.retained_bytes.saturating_add(history_entry_bytes(
            self.entries.last().expect("just pushed"),
        ));
        if self.entries.len() > self.max {
            let drop = self.entries.len() - self.max;
            let dropped = self.entries[..drop]
                .iter()
                .map(history_entry_bytes)
                .fold(0usize, usize::saturating_add);
            self.entries.drain(0..drop);
            self.retained_bytes = self.retained_bytes.saturating_sub(dropped);
        }
        trim_entries_to_bytes(
            &mut self.entries,
            &mut self.retained_bytes,
            MAX_HISTORY_MEMORY_BYTES,
        );
        self.entries.len() - 1
    }

    /// Finalize the most recent entry with its exit code and duration, then
    /// persist it (append to the JSONL file, if any).
    pub fn finalize_last(&mut self, exit_code: i32, duration_ms: u64) {
        if let Some(entry) = self.entries.last_mut() {
            entry.exit_code = Some(exit_code);
            entry.duration_ms = duration_ms;
            let snapshot = entry.clone();
            self.persist(&snapshot);
        }
    }

    fn persist(&self, entry: &HistoryEntry) {
        self.persist_with_file_limit(entry, MAX_HISTORY_FILE_BYTES);
    }

    fn persist_with_file_limit(&self, entry: &HistoryEntry, file_limit: u64) {
        let Some(path) = &self.path else { return };
        let _ = ensure_parent_dir(path);
        let Ok(_guard) = lock_history(path) else {
            return;
        };
        if let Ok(mut line) = serde_json::to_string(entry) {
            line.push('\n');
            if line.len() > MAX_HISTORY_LINE_BYTES {
                return;
            }
            if let Ok(mut file) = open_history_for_append(path) {
                let current_len = file
                    .metadata()
                    .map(|metadata| metadata.len())
                    .unwrap_or(u64::MAX);
                if current_len.saturating_add(line.len() as u64) > file_limit {
                    return;
                }
                // One write_all of the whole line (not writeln!'s two writes): with
                // O_APPEND this lands atomically, so concurrent sessions can't
                // interleave a half-line.
                let _ = file.write_all(line.as_bytes());
            }
        }
    }

    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clear the in-memory list (does not erase the persisted file).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.retained_bytes = 0;
    }

    /// The most recent command (excluding an exact `prefix` match) that begins
    /// with `prefix`, for inline autosuggestion (fish-style most-recent match).
    pub fn suggest(&self, prefix: &str) -> Option<&str> {
        if prefix.is_empty() {
            return None;
        }
        self.entries
            .iter()
            .rev()
            .map(|e| e.command.as_str())
            .find(|cmd| cmd.len() > prefix.len() && cmd.starts_with(prefix))
    }

    /// Fuzzy-search history, ranked by match quality and frecency. Returns the
    /// most relevant entries first, de-duplicated by command text.
    pub fn fuzzy_search(&self, query: &str, now: u64, limit: usize) -> Vec<&HistoryEntry> {
        let mut best: Vec<(&HistoryEntry, i64)> = Vec::new();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        // Walk newest-first so the first occurrence of each command is the most
        // recent one.
        for entry in self.entries.iter().rev() {
            if !seen.insert(entry.command.as_str()) {
                continue;
            }
            let Some(match_score) = fuzzy_score(query, &entry.command) else {
                continue;
            };
            let score = match_score + frecency_weight(now, entry.started_at);
            best.push((entry, score));
        }
        best.sort_by_key(|b| std::cmp::Reverse(b.1));
        best.truncate(limit);
        best.into_iter().map(|(e, _)| e).collect()
    }

    /// Query the native history store with Atuin-style scope and search modes.
    /// Results are newest/relevance ranked and optionally de-duplicated by
    /// command text, keeping the newest matching occurrence as the display row.
    pub fn query(&self, query: &HistoryQuery, now: u64) -> Vec<HistoryMatch> {
        let limit = query.limit.max(1);
        let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        let mut matches: Vec<HistoryMatch> = Vec::new();

        for (idx, entry) in self.entries.iter().enumerate().rev() {
            if !entry_matches_filters(entry, query) {
                continue;
            }
            let Some(search_score) = search_score(entry, &query.text, query.mode) else {
                continue;
            };
            if query.dedupe {
                let count = seen.entry(entry.command.as_str()).or_insert(0);
                *count += 1;
                if *count > 1 {
                    continue;
                }
            }
            let status_score = match entry.exit_code {
                Some(0) => 20,
                Some(_) => -10,
                None => 0,
            };
            let duration_score = if entry.duration_ms > 0 {
                -(entry.duration_ms.min(60_000) as i64 / 1_000)
            } else {
                0
            };
            matches.push(HistoryMatch {
                entry: entry.clone(),
                index: idx + 1,
                score: search_score
                    + frecency_weight(now, entry.started_at)
                    + status_score
                    + duration_score,
                count: 1,
            });
        }

        if query.dedupe {
            for row in &mut matches {
                row.count = seen.get(row.entry.command.as_str()).copied().unwrap_or(1);
            }
        }
        matches.sort_by_key(|row| std::cmp::Reverse(row.score));
        matches.truncate(limit);
        matches
    }

    /// Aggregate command-history statistics for `history stats`.
    pub fn stats(&self) -> HistoryStats {
        let mut stats = HistoryStats {
            total: self.entries.len(),
            ..HistoryStats::default()
        };
        let mut command_counts: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        let mut family_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();

        for entry in &self.entries {
            match entry.exit_code {
                Some(0) => stats.succeeded += 1,
                Some(_) => stats.failed += 1,
                None => {}
            }
            *command_counts.entry(entry.command.as_str()).or_insert(0) += 1;
            let family = entry
                .command_family
                .clone()
                .or_else(|| command_family(&entry.command));
            if let Some(family) = family {
                *family_counts.entry(family).or_insert(0) += 1;
            }
        }

        stats.most_used = command_counts
            .into_iter()
            .map(|(cmd, count)| (cmd.to_string(), count))
            .collect();
        stats
            .most_used
            .sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        stats.most_used.truncate(10);

        stats.by_family = family_counts.into_iter().collect();
        stats
            .by_family
            .sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        stats.by_family.truncate(10);

        stats.slowest = self.entries.clone();
        stats
            .slowest
            .sort_by_key(|entry| std::cmp::Reverse(entry.duration_ms));
        stats.slowest.truncate(10);
        stats
    }

    /// Directories ranked by frecency, for directory jumping (`z`).
    pub fn frecent_dirs(&self, now: u64) -> Vec<(String, i64)> {
        let mut scores: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
        for entry in &self.entries {
            if entry.cwd.is_empty() {
                continue;
            }
            *scores.entry(entry.cwd.as_str()).or_insert(0) +=
                frecency_weight(now, entry.started_at);
        }
        let mut ranked: Vec<(String, i64)> = scores
            .into_iter()
            .map(|(d, s)| (d.to_string(), s))
            .collect();
        ranked.sort_by_key(|b| std::cmp::Reverse(b.1));
        ranked
    }
}

fn entry_matches_filters(entry: &HistoryEntry, query: &HistoryQuery) -> bool {
    match &query.scope {
        HistoryScope::Global => {}
        HistoryScope::Host(host) => {
            if &entry.hostname != host {
                return false;
            }
        }
        HistoryScope::Session(session) => {
            if entry.session_id.as_deref() != Some(session.as_str()) {
                return false;
            }
        }
        HistoryScope::Cwd(cwd) => {
            if &entry.cwd != cwd {
                return false;
            }
        }
        HistoryScope::Project(project) => {
            if entry.project.as_deref() != Some(project.as_str())
                && entry.git_root.as_deref() != Some(project.as_str())
            {
                return false;
            }
        }
        HistoryScope::GitRoot(root) => {
            if entry.git_root.as_deref() != Some(root.as_str()) {
                return false;
            }
        }
        HistoryScope::Failures => {
            if entry.exit_code.is_none_or(|code| code == 0) {
                return false;
            }
        }
        HistoryScope::LongRunning { min_ms } => {
            if entry.duration_ms < *min_ms {
                return false;
            }
        }
    }
    if let Some(code) = query.exit_code {
        if entry.exit_code != Some(code) {
            return false;
        }
    }
    if query.failed && entry.exit_code.is_none_or(|code| code == 0) {
        return false;
    }
    if let Some(after) = query.after {
        if entry.started_at < after {
            return false;
        }
    }
    if let Some(before) = query.before {
        if entry.started_at > before {
            return false;
        }
    }
    if let Some(cwd) = &query.cwd {
        if &entry.cwd != cwd {
            return false;
        }
    }
    if let Some(project) = &query.project {
        if entry.project.as_deref() != Some(project.as_str())
            && entry.git_root.as_deref() != Some(project.as_str())
        {
            return false;
        }
    }
    if let Some(min) = query.min_duration_ms {
        if entry.duration_ms < min {
            return false;
        }
    }
    if let Some(max) = query.max_duration_ms {
        if entry.duration_ms > max {
            return false;
        }
    }
    true
}

fn search_score(entry: &HistoryEntry, text: &str, mode: SearchMode) -> Option<i64> {
    let text = text.trim();
    if text.is_empty() {
        return Some(100);
    }
    let cmd = entry.command.as_str();
    let needle = text.to_lowercase();
    let hay = cmd.to_lowercase();
    match mode {
        SearchMode::Fuzzy => fuzzy_score(text, cmd),
        SearchMode::Prefix => hay
            .starts_with(&needle)
            .then_some(10_000 - hay.len() as i64),
        SearchMode::FullText => hay.find(&needle).map(|pos| 8_000 - pos as i64),
        SearchMode::Exact => (cmd == text).then_some(20_000),
        SearchMode::Family => {
            if let Some(family) = entry.command_family.as_deref() {
                return (family == needle || family.starts_with(&needle)).then_some(12_000);
            }
            let family = command_family(cmd)?;
            (family == needle || family.starts_with(&needle)).then_some(12_000)
        }
    }
}

/// Best-effort command-family extraction for history grouping. This is not a
/// shell parser; it intentionally stays cheap and deterministic for hot paths.
pub fn command_family(command: &str) -> Option<String> {
    let wrappers = [
        "raw",
        "clean",
        "compact",
        "semantic",
        "lossless-ref",
        "lossless_ref",
        "silent",
        "rich",
        "command",
        "builtin",
        "external",
        "pty",
        "agpty",
    ];
    for word in command.split_whitespace() {
        if word.contains('=') && !word.starts_with('=') && !word.contains('/') {
            let Some((name, _)) = word.split_once('=') else {
                continue;
            };
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
        }
        let unquoted = word.trim_matches(['\'', '"']);
        if wrappers.contains(&unquoted) {
            continue;
        }
        let family = unquoted.rsplit('/').next().unwrap_or(unquoted);
        if !family.is_empty() {
            return Some(family.to_ascii_lowercase());
        }
    }
    None
}

/// Recency weight in frecency buckets (more recent = higher).
pub fn frecency_weight(now: u64, then: u64) -> i64 {
    let age = now.saturating_sub(then);
    match age {
        0..=3_600 => 100,         // last hour
        3_601..=86_400 => 50,     // last day
        86_401..=604_800 => 20,   // last week
        604_801..=2_592_000 => 8, // last month
        _ => 2,
    }
}

/// Subsequence fuzzy match: returns a score (higher = better) if every char of
/// `query` appears in order in `text` (case-insensitive). Contiguous and
/// word-start matches score higher; `None` if no match. An empty query matches
/// everything with a neutral score.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let mut qi = 0usize;
    let mut score = 0i64;
    let mut prev_match: Option<usize> = None;
    for (ti, &tc) in t.iter().enumerate() {
        if qi >= q.len() {
            break;
        }
        if tc == q[qi] {
            score += 10;
            if let Some(p) = prev_match {
                if ti == p + 1 {
                    score += 15; // contiguous run bonus
                }
            }
            if ti == 0 || matches!(t.get(ti.wrapping_sub(1)), Some(' ' | '/' | '-' | '_' | '.')) {
                score += 10; // word-start bonus
            }
            prev_match = Some(ti);
            qi += 1;
        }
    }
    if qi == q.len() {
        // Prefer shorter haystacks (tighter matches).
        Some(score - (t.len() as i64 / 4))
    } else {
        None
    }
}

/// Best-effort hostname for history entries.
pub fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("HOST").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

/// Best-effort account name for history entries.
pub fn username() -> Option<String> {
    std::env::var("USER")
        .ok()
        .or_else(|| std::env::var("LOGNAME").ok())
        .filter(|u| !u.trim().is_empty())
}

/// Default history file: `$AGSH_HISTORY_FILE`, else
/// `$XDG_DATA_HOME/agsh/history.jsonl`, else `$HOME/.local/share/agsh/...`.
pub fn default_history_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("AGSH_HISTORY_FILE") {
        return Some(PathBuf::from(p));
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return Some(Path::new(&xdg).join("agsh/history.jsonl"));
    }
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".local/share/agsh/history.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(cmd: &str, cwd: &str, started_at: u64) -> HistoryEntry {
        HistoryEntry::new(cmd, cwd, started_at)
    }

    #[test]
    fn oversized_log_is_streamed_capped_and_compacted() {
        let dir = std::env::temp_dir().join(format!("agsh_histc_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.jsonl");
        // A log far larger than the retained window (> 2*max triggers compaction).
        let mut text = String::new();
        for i in 0..100u64 {
            text.push_str(&serde_json::to_string(&entry(&format!("cmd{i}"), "/x", i)).unwrap());
            text.push('\n');
        }
        std::fs::write(&path, &text).unwrap();

        let store = HistoryStore::with_file(path.clone(), 10);
        assert_eq!(store.len(), 10, "in-memory capped to max");
        assert_eq!(
            store.entries().last().unwrap().command,
            "cmd99",
            "keeps newest"
        );
        // The on-disk log was compacted, so it cannot grow without bound.
        let on_disk = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(on_disk <= 10, "log not compacted: {on_disk} lines");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_history_line_is_skipped_without_losing_newer_entries() {
        let dir = std::env::temp_dir().join(format!(
            "agsh_histlinecap_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&vec![b'x'; MAX_HISTORY_LINE_BYTES + 1])
            .unwrap();
        file.write_all(b"\n").unwrap();
        serde_json::to_writer(&mut file, &entry("newer", "/x", 2)).unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);

        let store = HistoryStore::with_file(path.clone(), 10);

        assert_eq!(store.len(), 1);
        assert_eq!(store.entries()[0].command, "newer");
        assert!(std::fs::metadata(&path).unwrap().len() < MAX_HISTORY_LINE_BYTES as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_history_file_load_is_bounded_to_its_newest_tail() {
        use std::io::{Seek, SeekFrom};

        let dir = std::env::temp_dir().join(format!(
            "agsh_histfilecap_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        serde_json::to_writer(&mut file, &entry("too-old", "/x", 1)).unwrap();
        file.write_all(b"\n").unwrap();
        file.seek(SeekFrom::Start(8 * 1024)).unwrap();
        file.write_all(b"discard this partial record\n").unwrap();
        serde_json::to_writer(&mut file, &entry("newest", "/x", 2)).unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);

        let store = HistoryStore::with_file_load_limit(path.clone(), 10, 1024);

        assert_eq!(
            store
                .entries()
                .iter()
                .map(|entry| entry.command.as_str())
                .collect::<Vec<_>>(),
            ["newest"]
        );
        assert!(std::fs::metadata(&path).unwrap().len() <= 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn history_append_waits_for_lock_contention_instead_of_dropping_the_record() {
        let dir = std::env::temp_dir().join(format!(
            "agsh_histlockwait_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.jsonl");
        let held = lock_history(&path).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let thread_path = path.clone();
        let writer = std::thread::spawn(move || {
            let store = HistoryStore {
                entries: Vec::new(),
                path: Some(thread_path),
                max: 10,
                retained_bytes: 0,
            };
            started_tx.send(()).unwrap();
            store.persist_with_file_limit(&entry("retained", "/x", 1), 1024);
            finished_tx.send(()).unwrap();
        });

        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            finished_rx.recv_timeout(std::time::Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        );
        drop(held);
        finished_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        writer.join().unwrap();

        let persisted = std::fs::read_to_string(&path).unwrap();
        assert!(persisted.contains("retained"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn history_load_does_not_wait_for_the_optional_compaction_lock() {
        let dir = std::env::temp_dir().join(format!(
            "agsh_histloadlock_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        serde_json::to_writer(&mut file, &entry("visible", "/x", 1)).unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);
        let held = lock_history(&path).unwrap();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let thread_path = path.clone();
        let loader = std::thread::spawn(move || {
            let store = HistoryStore::with_file_load_limit(thread_path, 10, 1024);
            finished_tx.send(store.len()).unwrap();
        });

        let loaded = finished_rx.recv_timeout(std::time::Duration::from_secs(2));
        drop(held);
        loader.join().unwrap();

        assert_eq!(loaded.unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_history_stops_persisting_at_file_ceiling() {
        let dir = std::env::temp_dir().join(format!("agsh_histactive_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.jsonl");
        let mut store = HistoryStore::with_file(path.clone(), 100);
        for index in 0..100 {
            let item = entry(&format!("command-{index}-with-some-payload"), "/x", index);
            store.push(item.clone());
            store.persist_with_file_limit(&item, 1024);
        }

        let bytes = std::fs::metadata(&path).unwrap().len();
        assert!(bytes <= 1024, "history exceeded its file ceiling: {bytes}");
        assert!(
            store.len() > 10,
            "in-memory history was unexpectedly capped by disk"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn persisted_history_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("agsh_histperm_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.jsonl");

        let mut store = HistoryStore::with_file(path.clone(), 10);
        store.push(entry("secret command", "/x", 1));
        store.finalize_last(0, 1);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "history file mode was {mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn opening_history_tightens_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("agsh_histtight_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.jsonl");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let _store = HistoryStore::with_file(path.clone(), 10);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "history file mode was {mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn history_never_follows_a_configured_symlink() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!("agsh_histpathlink_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        let path = dir.join("history.jsonl");
        std::fs::write(&victim, b"do not append\n").unwrap();
        symlink(&victim, &path).unwrap();

        let mut store = HistoryStore::with_file(path, 10);
        store.push(entry("secret command", "/x", 1));
        store.finalize_last(0, 1);

        assert_eq!(std::fs::read(&victim).unwrap(), b"do not append\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn non_regular_history_input_is_rejected_without_reading() {
        let store = HistoryStore::with_file(PathBuf::from("/dev/zero"), 10);
        assert!(store.is_empty());
    }

    #[test]
    fn giant_command_is_replaced_with_a_non_executable_history_marker() {
        let mut store = HistoryStore::in_memory();
        store.push(entry(&"x".repeat(MAX_HISTORY_COMMAND_BYTES + 1), "/x", 1));

        let command = &store.entries()[0].command;
        assert!(command.starts_with("# agsh:"));
        assert!(command.len() < 256);
    }

    #[test]
    fn retained_history_has_an_aggregate_memory_ceiling() {
        let mut entries = (0..20)
            .map(|index| entry(&format!("{index}:{}", "x".repeat(200)), "/x", index))
            .collect::<Vec<_>>();
        let mut retained = entries
            .iter()
            .map(history_entry_bytes)
            .fold(0usize, usize::saturating_add);

        trim_entries_to_bytes(&mut entries, &mut retained, 1024);

        assert!(retained <= 1024);
        assert!(!entries.is_empty());
        assert!(entries.last().unwrap().command.starts_with("19:"));
    }

    #[cfg(unix)]
    #[test]
    fn compaction_does_not_follow_predictable_temp_symlink() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!("agsh_histsymlink_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.jsonl");
        let victim = dir.join("victim");
        std::fs::write(&victim, "do not overwrite").unwrap();

        let mut text = String::new();
        for i in 0..100u64 {
            text.push_str(&serde_json::to_string(&entry(&format!("cmd{i}"), "/x", i)).unwrap());
            text.push('\n');
        }
        std::fs::write(&path, text).unwrap();

        let old_temp = path.with_extension(format!("tmp{}", std::process::id()));
        symlink(&victim, &old_temp).unwrap();

        let store = HistoryStore::with_file(path.clone(), 10);

        assert_eq!(store.len(), 10);
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "do not overwrite"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_skips_corrupt_line_and_keeps_newer_entries() {
        // SHIP_READINESS_PLAN P1-23: a single non-UTF8/unparseable line must not
        // truncate the rest of the (newest) history.
        let dir = std::env::temp_dir().join(format!("agsh_histbad_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.jsonl");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(
            serde_json::to_string(&entry("first", "/x", 1))
                .unwrap()
                .as_bytes(),
        );
        bytes.push(b'\n');
        bytes.extend_from_slice(&[0xff, 0xfe, 0x00]); // invalid UTF-8 line
        bytes.push(b'\n');
        bytes.extend_from_slice(
            serde_json::to_string(&entry("last", "/x", 2))
                .unwrap()
                .as_bytes(),
        );
        bytes.push(b'\n');
        std::fs::write(&path, &bytes).unwrap();

        let store = HistoryStore::with_file(path, 100);
        let cmds: Vec<&str> = store.entries().iter().map(|e| e.command.as_str()).collect();
        assert!(cmds.contains(&"first"), "first entry missing: {cmds:?}");
        assert!(
            cmds.contains(&"last"),
            "entry after the corrupt line was dropped: {cmds:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn suggest_returns_most_recent_prefix_match() {
        let mut h = HistoryStore::in_memory();
        h.push(entry("git status", "/p", 1));
        h.push(entry("git checkout main", "/p", 2));
        h.push(entry("git commit -m x", "/p", 3));
        assert_eq!(h.suggest("git c"), Some("git commit -m x"));
        assert_eq!(h.suggest("git ch"), Some("git checkout main"));
        assert_eq!(h.suggest("zzz"), None);
    }

    #[test]
    fn fuzzy_search_ranks_and_dedupes() {
        let mut h = HistoryStore::in_memory();
        h.push(entry("docker build -t api .", "/p", 1));
        h.push(entry("docker buildx build .", "/p", 2));
        h.push(entry("ls -la", "/p", 3));
        h.push(entry("docker build -t api .", "/p", 4)); // duplicate, newer
        let results = h.fuzzy_search("dockbuil", 100, 10);
        assert!(!results.is_empty());
        assert!(results[0].command.contains("docker build"));
        // The duplicate command appears once.
        let count = results
            .iter()
            .filter(|e| e.command == "docker build -t api .")
            .count();
        assert_eq!(count, 1);
        // Non-matching command excluded.
        assert!(!results.iter().any(|e| e.command == "ls -la"));
    }

    #[test]
    fn query_filters_scopes_and_counts_duplicates() {
        let mut h = HistoryStore::in_memory();
        let mut first = entry("cargo test parser", "/repo", 100);
        first.exit_code = Some(0);
        first.session_id = Some("s1".into());
        first.git_root = Some("/repo".into());
        first.command_family = command_family(&first.command);
        h.push(first);

        let mut second = entry("cargo test parser", "/repo", 200);
        second.exit_code = Some(101);
        second.session_id = Some("s1".into());
        second.git_root = Some("/repo".into());
        second.command_family = command_family(&second.command);
        h.push(second);

        let mut third = entry("npm test", "/repo/web", 300);
        third.exit_code = Some(0);
        third.session_id = Some("s2".into());
        third.git_root = Some("/repo".into());
        third.command_family = command_family(&third.command);
        h.push(third);

        let q = HistoryQuery {
            text: "cargo".into(),
            scope: HistoryScope::Session("s1".into()),
            mode: SearchMode::Prefix,
            limit: 10,
            ..HistoryQuery::default()
        };
        let rows = h.query(&q, 300);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entry.command, "cargo test parser");
        assert_eq!(rows[0].count, 2);

        let failures = HistoryQuery {
            scope: HistoryScope::Failures,
            limit: 10,
            dedupe: false,
            ..HistoryQuery::default()
        };
        let rows = h.query(&failures, 300);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entry.exit_code, Some(101));
    }

    #[test]
    fn family_search_and_stats_group_commands() {
        let mut h = HistoryStore::in_memory();
        for (cmd, at) in [
            ("raw cargo check", 1),
            ("FOO=1 cargo test", 2),
            ("git status", 3),
        ] {
            let mut e = entry(cmd, "/repo", at);
            e.command_family = command_family(cmd);
            h.push(e);
        }

        let q = HistoryQuery {
            text: "cargo".into(),
            mode: SearchMode::Family,
            limit: 10,
            dedupe: false,
            ..HistoryQuery::default()
        };
        let rows = h.query(&q, 100);
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .all(|row| { row.entry.command_family.as_deref() == Some("cargo") }));

        let stats = h.stats();
        assert!(stats
            .by_family
            .iter()
            .any(|(name, count)| name == "cargo" && *count == 2));
        assert_eq!(
            command_family("semantic command /usr/bin/git status").as_deref(),
            Some("git")
        );
    }

    #[test]
    fn frecency_prefers_recent() {
        assert!(frecency_weight(1000, 1000) > frecency_weight(10_000_000, 1000));
    }

    #[test]
    fn finalize_sets_exit_and_duration() {
        let mut h = HistoryStore::in_memory();
        h.push(entry("make", "/p", 1));
        h.finalize_last(2, 1500);
        let e = h.entries().last().unwrap();
        assert_eq!(e.exit_code, Some(2));
        assert_eq!(e.duration_ms, 1500);
    }

    #[test]
    fn frecent_dirs_aggregates() {
        let mut h = HistoryStore::in_memory();
        h.push(entry("a", "/home/x/api", 100));
        h.push(entry("b", "/home/x/api", 100));
        h.push(entry("c", "/home/x/web", 100));
        let dirs = h.frecent_dirs(100);
        assert_eq!(dirs[0].0, "/home/x/api");
    }
}
