use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use agsh_core::{CommandId, ShellError, Value};
use agsh_index::{GitContext, PathCache};
use agsh_output::{CompactorConfig, OutputMode};
use agsh_store::history::{self, HistoryEntry};
use agsh_store::{HistoryStore, TraceRecord, TraceStore};
use agsh_style::{Role, Theme};

/// Max graph-execution nesting before we error instead of overflowing the stack.
/// Generous for legitimate recursion/nesting yet well below the point where the
/// heavier per-frame executor stack would abort the process. See
/// [`ShellState::enter_exec`].
const MAX_EXEC_DEPTH: usize = 256;

/// Cache of PATH executable names, keyed by the `$PATH` value it was built from.
type CommandNameCache = Arc<Mutex<Option<(String, std::collections::HashSet<String>)>>>;

/// An activated project `.env`: its directory and the prior value of each key it
/// set (so the values can be restored when leaving the directory).
type ActiveEnv = Option<(PathBuf, Vec<(String, Option<String>)>)>;

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

    fn resolve_spec(&self, spec: &str) -> Option<usize> {
        // %+ / %% = current, %- = previous, %n = job n, %name = prefix match.
        let body = spec.strip_prefix('%').unwrap_or(spec);
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

/// Saved prior binding for a variable shadowed by `local` in a function scope.
#[derive(Debug, Clone)]
struct LocalSaved {
    name: String,
    prior_var: Option<String>,
    prior_value: Option<Value>,
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
    reader: Arc<Mutex<io::PipeReader>>,
}

impl StreamingStdin {
    pub(crate) fn new(reader: io::PipeReader) -> Self {
        Self {
            reader: Arc::new(Mutex::new(reader)),
        }
    }

    fn read_line(&self) -> io::Result<Option<String>> {
        let mut reader = self
            .reader
            .lock()
            .map_err(|_| io::Error::other("streaming stdin lock poisoned"))?;
        let mut bytes = Vec::new();
        let mut byte = [0u8; 1];

        loop {
            let read = reader.read(&mut byte)?;
            if read == 0 {
                return if bytes.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(String::from_utf8_lossy(&bytes).to_string()))
                };
            }

            bytes.push(byte[0]);
            if byte[0] == b'\n' {
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
pub struct ShellState {
    cwd: PathBuf,
    vars: BTreeMap<String, String>,
    values: BTreeMap<String, Value>,
    env: BTreeMap<String, String>,
    aliases: BTreeMap<String, String>,
    abbreviations: BTreeMap<String, String>,
    functions: BTreeMap<String, ShellFunction>,
    history: Arc<Mutex<HistoryStore>>,
    path_cache: PathCache,
    path_cache_value: Option<String>,
    last_status: i32,
    last_command_substitution_status: i32,
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
    local_scopes: Vec<Vec<LocalSaved>>,
    loop_depth: usize,
    loop_control: Option<LoopControl>,
    buffered_stdin: Option<BufferedStdin>,
    streaming_stdin: Option<StreamingStdin>,
    streaming_stdout: Option<StreamingStdout>,
    jobs: Arc<Mutex<JobTable>>,
    interrupt: Arc<AtomicBool>,
    traces: Arc<Mutex<TraceStore>>,
    /// First-use hashes for advisories, so a repeated advisory (loop / agent retry)
    /// is shown once instead of flooding the context. Advisory channel only.
    advisories: Arc<Mutex<std::collections::HashSet<u64>>>,
    config: Arc<CompactorConfig>,
    /// Session default output mode (set by config/env/flag at startup and the
    /// runtime `mode` builtin); `None` means use the executor's mode.
    default_output_mode: Option<OutputMode>,
    git_cache: Arc<Mutex<Option<GitCacheEntry>>>,
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
    readonly_vars: std::collections::HashSet<String>,
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

impl ShellState {
    pub fn from_current_process() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let env = std::env::vars().collect::<BTreeMap<_, _>>();
        let mut vars = env.clone();
        // POSIX: IFS is initialized to <space><tab><newline> (2.5.3).
        vars.entry("IFS".to_string())
            .or_insert_with(|| " \t\n".to_string());
        let values = vars
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect();
        Self {
            cwd,
            vars,
            values,
            env,
            aliases: BTreeMap::new(),
            abbreviations: BTreeMap::new(),
            functions: BTreeMap::new(),
            history: Arc::new(Mutex::new(HistoryStore::in_memory())),
            path_cache: PathCache::default(),
            path_cache_value: None,
            last_status: 0,
            last_command_substitution_status: 0,
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
            advisories: Arc::new(Mutex::new(std::collections::HashSet::new())),
            config: Arc::new(CompactorConfig::load()),
            default_output_mode: None,
            git_cache: Arc::new(Mutex::new(None)),
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
            readonly_vars: std::collections::HashSet::new(),
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
        if append {
            let entry = self.arrays.entry(name.to_string()).or_default();
            entry.append(&mut elements);
        } else {
            self.arrays.insert(name.to_string(), elements);
        }
        self.vars.remove(name);
    }

    /// Assign a single array element (growing the array as needed).
    pub fn set_array_element(&mut self, name: &str, index: usize, value: String, append: bool) {
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
    }

    /// The elements of an indexed array, if `name` is one.
    pub fn array(&self, name: &str) -> Option<&[String]> {
        self.arrays.get(name).map(Vec::as_slice)
    }

    /// Declare `name` as an associative array (`declare -A`).
    pub fn declare_assoc(&mut self, name: &str) {
        self.assoc_arrays.entry(name.to_string()).or_default();
        self.vars.remove(name);
    }
    /// Whether `name` is an associative array.
    pub fn is_assoc(&self, name: &str) -> bool {
        self.assoc_arrays.contains_key(name)
    }
    /// Set one associative-array element.
    pub fn set_assoc_element(&mut self, name: &str, key: String, value: String, append: bool) {
        let map = self.assoc_arrays.entry(name.to_string()).or_default();
        if append {
            map.entry(key).or_default().push_str(&value);
        } else {
            map.insert(key, value);
        }
        self.vars.remove(name);
    }
    /// Replace an associative array's contents from key/value pairs.
    pub fn set_assoc(&mut self, name: &str, pairs: Vec<(String, String)>, append: bool) {
        let map = self.assoc_arrays.entry(name.to_string()).or_default();
        if !append {
            map.clear();
        }
        for (k, v) in pairs {
            map.insert(k, v);
        }
        self.vars.remove(name);
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

    /// Apply (or refresh) the current directory's trusted `.env`, restoring any
    /// previously-activated project env first. Untrusted `.env` files are
    /// ignored, so this is a no-op unless the user ran `trust` — keeping default
    /// behavior identical to no env activation.
    pub fn activate_project_env(&mut self) {
        if let Some((_dir, saved)) = self.active_env.take() {
            for (key, prior) in saved {
                match prior {
                    Some(value) => self.export_var(key, value),
                    None => self.unset(&key),
                }
            }
        }
        let cwd = self.cwd().to_path_buf();
        let Some(envfile) = agsh_index::find_dotenv(&cwd) else {
            return;
        };
        let Some(hash) = agsh_index::content_hash(&envfile) else {
            return;
        };
        let dir = cwd.display().to_string();
        if !agsh_index::TrustStore::load().is_trusted(&dir, hash) {
            return;
        }
        let mut saved = Vec::new();
        for (key, value) in agsh_index::parse_dotenv(&envfile) {
            saved.push((key.clone(), self.lookup(&key).map(str::to_string)));
            self.export_var(key, value);
        }
        if !saved.is_empty() {
            self.active_env = Some((cwd, saved));
        }
    }

    /// Trust the current directory's `.env` and activate it. Returns
    /// `Some(var_count)` if a `.env` was found, else `None`.
    pub fn trust_current_env(&mut self) -> Option<usize> {
        let cwd = self.cwd().to_path_buf();
        let envfile = agsh_index::find_dotenv(&cwd)?;
        let hash = agsh_index::content_hash(&envfile)?;
        let dir = cwd.display().to_string();
        let mut store = agsh_index::TrustStore::load();
        store.trust(&dir, hash);
        let count = agsh_index::parse_dotenv(&envfile).len();
        self.activate_project_env();
        Some(count)
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

    /// Git context for the current directory, cached briefly so repeated prompt
    /// renders don't re-probe. Never blocks (the dirty probe is time-bounded).
    pub fn git_context(&self) -> Option<GitContext> {
        const TTL: std::time::Duration = std::time::Duration::from_millis(1500);
        let cwd = self.cwd().to_path_buf();
        if let Ok(cache) = self.git_cache.lock() {
            if let Some(entry) = cache.as_ref() {
                if entry.cwd == cwd && entry.computed_at.elapsed() < TTL {
                    return entry.context.clone();
                }
            }
        }
        let context = agsh_index::git_context(&cwd);
        if let Ok(mut cache) = self.git_cache.lock() {
            *cache = Some(GitCacheEntry {
                cwd,
                computed_at: Instant::now(),
                context: context.clone(),
            });
        }
        context
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
    /// prior binding for restoration on function exit. The variable starts
    /// unset within the scope. Returns false when not inside a function.
    pub(crate) fn declare_local(&mut self, name: &str) -> bool {
        if self.local_scopes.is_empty() {
            return false;
        }
        let already = self
            .local_scopes
            .last()
            .is_some_and(|scope| scope.iter().any(|saved| saved.name == name));
        if !already {
            let prior_var = self.vars.get(name).cloned();
            let prior_value = self.values.get(name).cloned();
            if let Some(scope) = self.local_scopes.last_mut() {
                scope.push(LocalSaved {
                    name: name.to_string(),
                    prior_var,
                    prior_value,
                });
            }
            if name == "PATH" {
                self.clear_path_cache();
            }
            self.vars.remove(name);
            self.values.remove(name);
        }
        true
    }

    fn restore_local(&mut self, saved: LocalSaved) {
        if saved.name == "PATH" {
            self.clear_path_cache();
        }
        match saved.prior_var {
            Some(value) => {
                self.vars.insert(saved.name.clone(), value);
            }
            None => {
                self.vars.remove(&saved.name);
            }
        }
        match saved.prior_value {
            Some(value) => {
                self.values.insert(saved.name, value);
            }
            None => {
                self.values.remove(&saved.name);
            }
        }
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
            return Some(Ok(self.read_buffered_stdin_line().flatten()));
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
        if let Ok(mut store) = self.traces.lock() {
            store.record(TraceRecord::new(
                cmd_id,
                command,
                exit_code,
                stdout.to_vec(),
                stderr.to_vec(),
            ));
        }
        // In the interception/observe path the in-memory store dies with this
        // one-shot process, so persist to `$AGSH_TRACE_DIR` too — that makes the
        // `raw:` file-path references resolvable across processes (an agent can
        // `grep`/`cat` them from plain bash).
        persist_trace_to_disk(cmd_id, stdout, stderr);
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

    /// Resolve a `trace://<id>/stdout|stderr` (or bare id) reference to bytes.
    pub fn resolve_trace(&self, reference: &str) -> Option<Vec<u8>> {
        let store = self.traces.lock().ok()?;
        store.resolve(reference).map(<[u8]>::to_vec)
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
    /// completion notices, removing the reported jobs from the table.
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
                    job.state = JobState::Done(status.code().unwrap_or(0));
                }
            }
        }
        table.jobs.retain(|job| {
            if let JobState::Done(code) = job.state {
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
                false
            } else {
                true
            }
        });
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
                    job.state = JobState::Done(status.code().unwrap_or(0));
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

    /// Resolve a job spec (`%n`, `%+`, `%-`, `%name`, or a bare id) to a pgid.
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
    /// Returns the exit status of the waited job, or the last job for "all".
    pub fn wait_for_jobs(&self, spec: Option<&str>) -> Option<i32> {
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
        }
        if targets.is_empty() {
            return Some(0);
        }
        let mut last = 0;
        for (_, mut child, _) in targets {
            if let Ok(status) = child.wait() {
                last = status.code().unwrap_or(0);
            }
        }
        Some(last)
    }

    pub(crate) fn read_buffered_stdin_line(&mut self) -> Option<Option<String>> {
        let buffered = self.buffered_stdin.as_mut()?;
        if buffered.offset >= buffered.data.len() {
            return Some(None);
        }

        let start = buffered.offset;
        let end = buffered.data[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(buffered.data.len(), |index| start + index + 1);
        buffered.offset = end;
        Some(Some(
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
        let key = key.into();
        // Readonly variables cannot be reassigned (POSIX `readonly`/`declare -r`).
        if self.readonly_vars.contains(&key) {
            return;
        }
        if key == "PATH" {
            self.clear_path_cache();
        }
        let value = value.into();
        self.values
            .insert(key.clone(), Value::String(value.clone()));
        self.vars.insert(key, value);
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

    pub fn set_value(&mut self, key: impl Into<String>, value: Value) {
        let key = key.into();
        if key == "PATH" {
            self.clear_path_cache();
        }
        self.vars.insert(key.clone(), value.as_string_lossy());
        self.values.insert(key, value);
    }

    pub fn export_var(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        let value = value.into();
        if key == "PATH" {
            self.clear_path_cache();
        }
        self.values
            .insert(key.clone(), Value::String(value.clone()));
        self.vars.insert(key.clone(), value.clone());
        self.env.insert(key, value);
    }

    pub fn unset(&mut self, key: &str) {
        // Readonly variables cannot be unset.
        if self.readonly_vars.contains(key) {
            return;
        }
        if key == "PATH" {
            self.clear_path_cache();
        }
        self.vars.remove(key);
        self.values.remove(key);
        self.env.remove(key);
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
        let line = line.into();
        if line.trim().is_empty() {
            return;
        }
        let mut entry = HistoryEntry::new(line, self.cwd().display().to_string(), unix_now());
        entry.hostname = history::hostname();
        if let Ok(mut store) = self.history.lock() {
            store.push(entry);
        }
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

    /// Directories ranked by frecency, for directory jumping.
    pub fn frecent_dirs(&self) -> Vec<(String, i64)> {
        self.history
            .lock()
            .map(|s| s.frecent_dirs(unix_now()))
            .unwrap_or_default()
    }

    pub fn cached_path_lookup(&mut self, path_value: &str, name: &str) -> Option<PathBuf> {
        self.ensure_path_cache_for(path_value);
        self.path_cache.get(name).cloned()
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

fn is_positional_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|ch| ch.is_ascii_digit())
}

/// Keep at most this many trace files in `$AGSH_TRACE_DIR` (2 per command).
/// Default cap on files in `$AGSH_TRACE_DIR` (2 per command ⇒ ~256 commands).
/// Override with `$AGSH_TRACE_DIR_CAP`. Keeps the newest, drops the oldest.
const TRACE_DIR_FILE_CAP: usize = 512;

fn trace_dir_cap() -> usize {
    std::env::var("AGSH_TRACE_DIR_CAP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(TRACE_DIR_FILE_CAP)
        .max(2)
}

/// Persist a command's raw stdout/stderr to `$AGSH_TRACE_DIR` as
/// `<pid>_<cmd_id>.out` / `.err`, so `raw:` file-path references survive the
/// process that produced them. No-op unless `$AGSH_TRACE_DIR` is set. The dir is
/// bounded (oldest files reaped) on every write, so it never grows without bound.
fn persist_trace_to_disk(cmd_id: &CommandId, stdout: &[u8], stderr: &[u8]) {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let Some(dir) = std::env::var_os("AGSH_TRACE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    // Raw traces are stored verbatim and are never redacted, so they can contain
    // secrets (from `env`, `cat .env`, `curl -v`, …). Keep them private to this
    // user: 0700 dir, 0600 files — not the umask-default 0755/0644.
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    let write_private = |path: PathBuf, bytes: &[u8]| {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
        {
            let _ = f.write_all(bytes);
        }
    };
    let pid = std::process::id();
    write_private(dir.join(format!("{pid}_{cmd_id}.out")), stdout);
    write_private(dir.join(format!("{pid}_{cmd_id}.err")), stderr);
    prune_trace_dir(&dir, trace_dir_cap());
}

/// Bound the trace dir to `cap` files: when it exceeds the cap, drop the oldest.
fn prune_trace_dir(dir: &Path, cap: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let mtime = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((mtime, path))
        })
        .collect();
    if files.len() <= cap {
        return;
    }
    files.sort_by_key(|a| a.0); // oldest first
    let drop = files.len() - cap;
    for (_, path) in files.into_iter().take(drop) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod trace_dir_tests {
    use super::prune_trace_dir;

    #[test]
    fn prune_keeps_newest_and_bounds_the_dir() {
        let dir = std::env::temp_dir().join(format!("agsh_prune_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Write 50 files with increasing mtimes (name order == age order here).
        for i in 0..50u32 {
            let path = dir.join(format!("{i:04}.out"));
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
        // The 10 kept must be the newest (0040..0049).
        assert!(
            remaining.iter().all(|n| n.as_str() >= "0040"),
            "kept the newest: {remaining:?}"
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
