//! Session journaling + restore: the shell's own crash-safe session state.
//!
//! An interactive session appends its state *deltas* (cwd, exports, vars,
//! aliases, abbreviations, functions, options) to a per-session JSONL journal
//! (see `agsh_store::session`) as they happen — diffed at each command
//! boundary, so a crash, closed terminal, or reboot never loses more than the
//! command in flight. `fg`/`fg_end` records bracket the foreground command
//! line as a flight recorder.
//!
//! Restore replays the folded deltas onto the live shell — it never re-runs
//! commands, so replay has no side effects. A session becomes *restorable*
//! when its journal has no clean `exit`, its shell pid is gone, and it either
//! carries state or had something running.
//!
//! Interactive-only by design: `agsh -c`, scripts, and piped sessions never
//! journal, so non-interactive behavior (and the differential suites) is
//! byte-identical.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use agsh_store::session::{
    default_sessions_dir, list_sessions, new_session_id, prune_sessions, RestorableSession,
    SessionEvent, SessionInfo, SessionJournal, SESSION_FILE_CAP,
};
use agsh_style::Role;

use crate::state::ShellState;
use crate::CommandOutcome;

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Variable names never journaled or replayed: session identity, prompt-time
/// bookkeeping the shell derives itself, and positional parameters.
fn skip_key(key: &str) -> bool {
    if matches!(
        key,
        "AGSH_SESSION" | "PWD" | "OLDPWD" | "SHLVL" | "_" | "@" | "#" | "*" | "?" | "!"
    ) {
        return true;
    }
    // Positional parameters ($1, $2, …) live in the vars map under digit names.
    !key.is_empty() && key.bytes().all(|b| b.is_ascii_digit())
}

/// Current option states (set -o flags + shopt), as one flat name → bool map.
fn option_states(state: &ShellState) -> BTreeMap<String, bool> {
    let mut opts = BTreeMap::new();
    opts.insert("allexport".to_string(), state.allexport());
    opts.insert("errexit".to_string(), state.errexit());
    opts.insert("nounset".to_string(), state.nounset());
    opts.insert("noclobber".to_string(), state.noclobber());
    opts.insert("noglob".to_string(), state.noglob());
    opts.insert("pipefail".to_string(), state.pipefail());
    opts.insert("xtrace".to_string(), state.xtrace());
    for name in crate::builtins::SHOPT_NAMES {
        opts.insert((*name).to_string(), state.shopt(name));
    }
    opts
}

fn apply_option(state: &mut ShellState, name: &str, on: bool) {
    match name {
        "allexport" => state.set_allexport(on),
        "errexit" => state.set_errexit(on),
        "nounset" => state.set_nounset(on),
        "noclobber" => state.set_noclobber(on),
        "noglob" => state.set_noglob(on),
        "pipefail" => state.set_pipefail(on),
        "xtrace" => state.set_xtrace(on),
        shopt if crate::builtins::SHOPT_NAMES.contains(&shopt) => state.set_shopt(shopt, on),
        _ => {}
    }
}

/// A cheap copy of the journaled slice of shell state, diffed at each command
/// boundary. Small maps at interactive cadence — the clone is negligible next
/// to running the command itself.
#[derive(Debug, Clone)]
struct StateSnapshot {
    cwd: String,
    env: BTreeMap<String, String>,
    vars: BTreeMap<String, String>,
    aliases: BTreeMap<String, String>,
    abbrs: BTreeMap<String, String>,
    funcs: BTreeMap<String, String>,
    opts: BTreeMap<String, bool>,
    /// Live background jobs (pgid → command), for `job`/`job_end` records.
    jobs: BTreeMap<i32, String>,
}

impl StateSnapshot {
    fn of(state: &ShellState) -> Self {
        Self {
            cwd: state.cwd().display().to_string(),
            env: state.exported_env().clone(),
            vars: state.vars().clone(),
            aliases: state.aliases().clone(),
            abbrs: state.abbreviations().clone(),
            funcs: state
                .functions()
                .iter()
                .map(|(name, f)| (name.clone(), f.body.clone()))
                .collect(),
            opts: option_states(state),
            jobs: state.running_jobs_snapshot().into_iter().collect(),
        }
    }
}

/// Journals one interactive session: diffs state at command boundaries and
/// appends the deltas, plus flight-recorder `fg`/`fg_end` records.
#[derive(Debug)]
pub struct SessionRecorder {
    journal: SessionJournal,
    id: String,
    snap: StateSnapshot,
}

impl SessionRecorder {
    /// Start journaling into the default sessions directory. Exports
    /// `$AGSH_SESSION` (the session id) into the live state. `None` when no
    /// sessions directory can be resolved — the shell just runs unjournaled.
    pub fn begin(state: &mut ShellState) -> Option<Self> {
        let dir = default_sessions_dir()?;
        Some(Self::begin_in(&dir, state))
    }

    /// Start journaling into `dir` (explicit for tests; `begin` resolves env).
    pub fn begin_in(dir: &Path, state: &mut ShellState) -> Self {
        prune_sessions(dir, SESSION_FILE_CAP);
        let at = unix_now();
        let id = new_session_id(std::process::id(), at);
        state.export_var("AGSH_SESSION", id.clone());
        let journal = SessionJournal::create(dir, &id);
        journal.append(&SessionEvent::Start {
            id: id.clone(),
            pid: std::process::id(),
            cwd: state.cwd().display().to_string(),
            host: agsh_store::history::hostname(),
            at,
            version: env!("CARGO_PKG_VERSION").to_string(),
        });
        Self {
            journal,
            id,
            snap: StateSnapshot::of(state),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// A handle for appending from another thread (e.g. a SIGHUP handler).
    pub fn journal_handle(&self) -> SessionJournal {
        self.journal.clone()
    }

    /// A callback for the terminal-restore signal thread: journals a `hup`
    /// record when the terminal hangs up (SIGHUP), so the restore banner can
    /// say *why* the session died. Owns its own journal handle (`Send`).
    pub fn hangup_hook(&self) -> Box<dyn Fn(i32) + Send> {
        let journal = self.journal.clone();
        Box::new(move |signal| {
            if signal == signal_hook::consts::SIGHUP {
                journal.append(&SessionEvent::Hup { at: unix_now() });
            }
        })
    }

    /// Flight recorder: the foreground command line is starting.
    pub fn command_started(&self, line: &str) {
        self.journal.append(&SessionEvent::Fg {
            cmd: line.to_string(),
            at: unix_now(),
        });
    }

    /// Command finished: close the flight-recorder bracket, then journal any
    /// state deltas the command produced.
    pub fn command_finished(&mut self, state: &ShellState, code: i32) {
        self.journal.append(&SessionEvent::FgEnd {
            code,
            at: unix_now(),
        });
        let next = StateSnapshot::of(state);
        for event in diff_snapshots(&self.snap, &next) {
            self.journal.append(&event);
        }
        self.snap = next;
    }

    /// Clean session end.
    pub fn finish(&self, code: i32) {
        self.journal.append(&SessionEvent::Exit {
            code,
            at: unix_now(),
        });
    }
}

/// The delta events that turn `old` into `new`. No I/O — unit-testable without
/// a filesystem.
fn diff_snapshots(old: &StateSnapshot, new: &StateSnapshot) -> Vec<SessionEvent> {
    let mut events = Vec::new();

    if old.cwd != new.cwd {
        events.push(SessionEvent::Cwd {
            path: new.cwd.clone(),
        });
    }

    for (k, on) in &new.opts {
        if old.opts.get(k) != Some(on) {
            events.push(SessionEvent::Opt {
                k: k.clone(),
                on: *on,
            });
        }
    }

    // Exported-variable sets first; a plain `Var` is only emitted for keys that
    // are NOT exported (an `Env` replays as export_var, which covers both maps).
    let mut env_set: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (k, v) in &new.env {
        if skip_key(k) || old.env.get(k) == Some(v) {
            continue;
        }
        env_set.insert(k.as_str());
        events.push(SessionEvent::Env {
            k: k.clone(),
            v: v.clone(),
        });
    }
    for (k, v) in &new.vars {
        if skip_key(k) || env_set.contains(k.as_str()) || new.env.contains_key(k) {
            continue;
        }
        if old.vars.get(k) != Some(v) {
            events.push(SessionEvent::Var {
                k: k.clone(),
                v: v.clone(),
            });
        }
    }
    // Removals: `unset` drops a key from both maps, so dedupe across them.
    let mut removed: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for k in old.env.keys() {
        if !skip_key(k) && !new.env.contains_key(k) && !new.vars.contains_key(k) {
            removed.insert(k.as_str());
        }
    }
    for k in old.vars.keys() {
        if !skip_key(k) && !new.vars.contains_key(k) && !new.env.contains_key(k) {
            removed.insert(k.as_str());
        }
    }
    for k in removed {
        events.push(SessionEvent::Unset { k: k.to_string() });
    }

    diff_string_map(&old.aliases, &new.aliases, &mut events, |k, v| {
        SessionEvent::Alias { k, v }
    });
    for k in old.aliases.keys() {
        if !new.aliases.contains_key(k) {
            events.push(SessionEvent::Unalias { k: k.clone() });
        }
    }
    diff_string_map(&old.abbrs, &new.abbrs, &mut events, |k, v| {
        SessionEvent::Abbr { k, v }
    });
    for k in old.abbrs.keys() {
        if !new.abbrs.contains_key(k) {
            events.push(SessionEvent::Unabbr { k: k.clone() });
        }
    }
    diff_string_map(&old.funcs, &new.funcs, &mut events, |k, v| {
        SessionEvent::Func { k, v }
    });
    for k in old.funcs.keys() {
        if !new.funcs.contains_key(k) {
            events.push(SessionEvent::Unfunc { k: k.clone() });
        }
    }

    // Background jobs: registered → `job`, reaped/gone → `job_end`.
    for (pgid, cmd) in &new.jobs {
        if !old.jobs.contains_key(pgid) {
            events.push(SessionEvent::Job {
                pgid: *pgid,
                cmd: cmd.clone(),
                at: unix_now(),
            });
        }
    }
    for pgid in old.jobs.keys() {
        if !new.jobs.contains_key(pgid) {
            events.push(SessionEvent::JobEnd { pgid: *pgid });
        }
    }

    events
}

fn diff_string_map(
    old: &BTreeMap<String, String>,
    new: &BTreeMap<String, String>,
    events: &mut Vec<SessionEvent>,
    make: impl Fn(String, String) -> SessionEvent,
) {
    for (k, v) in new {
        if old.get(k) != Some(v) {
            events.push(make(k.clone(), v.clone()));
        }
    }
}

/// Sessions that a new shell may restore: died without a clean `exit`, not yet
/// restored, shell process gone, and something worth restoring. Newest first.
pub fn restorable_sessions() -> Vec<SessionInfo> {
    match default_sessions_dir() {
        Some(dir) => restorable_sessions_in(&dir),
        None => Vec::new(),
    }
}

/// Explicit-directory variant (env-free, for tests).
pub fn restorable_sessions_in(dir: &Path) -> Vec<SessionInfo> {
    list_sessions(dir)
        .into_iter()
        .filter(|info| {
            let s = &info.session;
            !s.clean_exit
                && !s.restored
                && s.pid != std::process::id()
                && !pid_alive(s.pid)
                && (s.has_state() || s.foreground.is_some() || !s.jobs.is_empty())
        })
        .collect()
}

/// Whether a process with this pid exists (signal-0 probe; EPERM still means
/// it exists). A dead shell's journal is restorable; a live one's is not.
fn pid_alive(pid: u32) -> bool {
    let Some(pid) = rustix::process::Pid::from_raw(pid as i32) else {
        return false;
    };
    match rustix::process::test_kill_process(pid) {
        Ok(()) => true,
        Err(errno) => errno == rustix::io::Errno::PERM,
    }
}

/// Whether a job recorded at `registered_at` is still the same live process:
/// the pid must exist AND its observed start time must match the recorded
/// registration time. The second check guards against pid reuse — after a
/// reboot or a long gap, a recycled pid must not masquerade as the old job.
fn job_still_alive(pgid: i32, registered_at: u64) -> bool {
    if pgid <= 0 || !pid_alive(pgid as u32) {
        return false;
    }
    match process_start_unix(pgid as u32) {
        // `ps` etime has ~1s resolution and the job was registered a moment
        // after the process spawned; a recycled pid would differ by far more.
        Some(started) => started.abs_diff(registered_at) <= 120,
        None => false,
    }
}

/// A process's start time (unix seconds), derived from `ps -o etime=`
/// (elapsed time; POSIX, same format on macOS and Linux — locale-proof,
/// unlike `lstart`).
fn process_start_unix(pid: u32) -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let etime = String::from_utf8_lossy(&out.stdout);
    let elapsed = parse_etime(etime.trim())?;
    Some(unix_now().saturating_sub(elapsed))
}

/// Parse `ps` elapsed time — `[[dd-]hh:]mm:ss` — into seconds.
fn parse_etime(s: &str) -> Option<u64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, rest)) => (d.parse::<u64>().ok()?, rest),
        None => (0, s),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let (h, m, sec): (u64, u64, u64) = match parts.as_slice() {
        [h, m, s] => (h.parse().ok()?, m.parse().ok()?, s.parse().ok()?),
        [m, s] => (0, m.parse().ok()?, s.parse().ok()?),
        _ => return None,
    };
    Some(days * 86_400 + h * 3_600 + m * 60 + sec)
}

/// Replay a dead session's folded deltas onto the live shell. Returns human
/// summary lines describing what was restored (empty when nothing applied).
pub fn apply_session(session: &RestorableSession, state: &mut ShellState) -> Vec<String> {
    let mut notes = Vec::new();

    if let Some(cwd) = &session.cwd {
        let path = Path::new(cwd);
        if path.is_dir() && std::env::set_current_dir(path).is_ok() {
            let previous = state.cwd().display().to_string();
            state.set_cwd(path.to_path_buf());
            state.export_var("OLDPWD", previous);
            state.export_var("PWD", cwd.clone());
            // Same as `cd`: entering the directory activates its trusted .env.
            state.activate_project_env();
            notes.push(format!("cwd        {cwd}"));
        } else {
            notes.push(format!("cwd        {cwd} (gone — kept current directory)"));
        }
    }

    for (name, on) in &session.opts {
        apply_option(state, name, *on);
    }
    if !session.opts.is_empty() {
        notes.push(format!(
            "options    {}",
            session
                .opts
                .iter()
                .map(|(k, on)| format!("{}{k}", if *on { "+" } else { "-" }))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }

    let mut restored_env = Vec::new();
    for (k, v) in &session.env {
        if skip_key(k) {
            continue;
        }
        match v {
            Some(v) => {
                // Confinement is narrow-only and must survive restore: a session
                // that died confined comes back confined, not widened.
                if k == "AGSH_CONFINE" {
                    let names: Vec<String> = v
                        .split([',', ' ', '\t'])
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                    if !names.is_empty() {
                        state.set_confine(&names);
                    }
                }
                state.export_var(k, v);
                restored_env.push(k.as_str());
            }
            None => state.unset(k),
        }
    }
    if !restored_env.is_empty() {
        notes.push(format!("env        {}", name_list(&restored_env)));
    }

    let mut restored_vars = Vec::new();
    for (k, v) in &session.vars {
        if skip_key(k) {
            continue;
        }
        match v {
            Some(v) => {
                state.set_var(k, v);
                restored_vars.push(k.as_str());
            }
            None => state.unset(k),
        }
    }
    if !restored_vars.is_empty() {
        notes.push(format!("vars       {}", name_list(&restored_vars)));
    }

    apply_named(
        state,
        &session.aliases,
        &mut notes,
        "aliases",
        |state, k, v| state.set_alias(k, v),
        |state, k| {
            state.remove_alias(k);
        },
    );
    apply_named(
        state,
        &session.abbrs,
        &mut notes,
        "abbrs",
        |state, k, v| state.set_abbreviation(k, v),
        |state, k| {
            state.remove_abbreviation(k);
        },
    );
    apply_named(
        state,
        &session.funcs,
        &mut notes,
        "functions",
        |state, k, v| state.set_function(k, crate::state::ShellFunction::new(v)),
        |state, k| {
            state.remove_function(k);
        },
    );

    notes
}

fn apply_named(
    state: &mut ShellState,
    map: &BTreeMap<String, Option<String>>,
    notes: &mut Vec<String>,
    label: &str,
    set: fn(&mut ShellState, &str, &str),
    remove: fn(&mut ShellState, &str),
) {
    let mut restored = Vec::new();
    for (k, v) in map {
        match v {
            Some(v) => {
                set(state, k, v);
                restored.push(k.as_str());
            }
            None => remove(state, k),
        }
    }
    if !restored.is_empty() {
        notes.push(format!("{label:<10} {}", name_list(&restored)));
    }
}

/// "a, b, c" capped at 8 names ("… (+N more)").
fn name_list(names: &[&str]) -> String {
    const SHOW: usize = 8;
    if names.len() <= SHOW {
        names.join(", ")
    } else {
        format!(
            "{} (+{} more)",
            names[..SHOW].join(", "),
            names.len() - SHOW
        )
    }
}

/// "12s" / "4m" / "3h" / "2d" — compact duration for banners and listings.
fn human_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Compact age of a past unix timestamp.
fn ago(from: u64) -> String {
    human_secs(unix_now().saturating_sub(from))
}

/// One-line note after the system wakes from standby: how long it slept and
/// what survived. Sleep freezes processes rather than killing them, but it
/// drops TCP connections (ssh!) and makes wall-clock state stale — so say it
/// happened instead of letting the shell silently pretend time didn't pass.
pub fn wake_note(state: &ShellState, slept_secs: u64) -> String {
    let jobs = state.running_jobs_snapshot();
    let mut line = format!("agsh: system was asleep ~{}", human_secs(slept_secs));
    if !jobs.is_empty() {
        line.push_str(&format!(
            " — {} background job{} still running",
            jobs.len(),
            if jobs.len() == 1 { "" } else { "s" }
        ));
    }
    state.theme().paint(Role::Muted, &line)
}

fn mtime_unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Tilde-abbreviate a path for display.
fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => match path.strip_prefix(&home) {
            Some(rest) => format!("~{rest}"),
            None => path.to_string(),
        },
        _ => path.to_string(),
    }
}

/// One-line interactive-startup banner when a dead session is restorable, or
/// `None`. Muted styling; never blocks (one directory scan of small files).
pub fn restore_banner(state: &ShellState) -> Option<String> {
    let sessions = restorable_sessions();
    let info = sessions.first()?;
    let s = &info.session;
    let mut what = Vec::new();
    let cwd = s.cwd.clone().unwrap_or_default();
    if !cwd.is_empty() {
        what.push(format!("cwd {}", tilde(&cwd)));
    }
    let deltas = s.delta_count() - usize::from(s.cwd.is_some());
    if deltas > 0 {
        what.push(format!(
            "{deltas} change{}",
            if deltas == 1 { "" } else { "s" }
        ));
    }
    if let Some(fg) = &s.foreground {
        what.push(format!("`{}` was running", truncate(&fg.cmd, 40)));
    }
    if !s.jobs.is_empty() {
        let alive = s
            .jobs
            .iter()
            .filter(|j| job_still_alive(j.pgid, j.started_at))
            .count();
        what.push(match (alive, s.jobs.len()) {
            (0, n) => format!("{n} background job{} lost", if n == 1 { "" } else { "s" }),
            (a, n) if a == n => {
                format!(
                    "{a} background job{} still running",
                    if a == 1 { "" } else { "s" }
                )
            }
            (a, n) => format!("{a}/{n} background jobs still running"),
        });
    }
    let why = if s.hangup {
        "hung up"
    } else {
        "ended unexpectedly"
    };
    let more = if sessions.len() > 1 {
        format!("; {} older: `resume list`", sessions.len() - 1)
    } else {
        String::new()
    };
    let line = format!(
        "agsh: a session {why} {} ago ({}) — `resume` restores it{more}",
        ago(mtime_unix(info.modified)),
        what.join(", "),
    );
    Some(state.theme().paint(Role::Muted, &line))
}

fn truncate(s: &str, max: usize) -> String {
    let oneline = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if oneline.chars().count() <= max {
        oneline
    } else {
        let head: String = oneline.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// `resume` — restore the state of a session that died without a clean exit.
///
///   resume         restore the most recent dead session
///   resume list    show restorable sessions
///   resume N       restore the Nth listed session
///
/// Restoring replays journaled state deltas (cwd, exports, vars, aliases,
/// abbreviations, functions, options); it never re-runs commands. The restored
/// journal is marked consumed so it isn't offered twice.
pub fn builtin_resume(args: &[String], state: &mut ShellState) -> CommandOutcome {
    let first = args.first().map(String::as_str);
    let sessions = restorable_sessions();

    if matches!(first, Some("list" | "--list" | "-l")) {
        if sessions.is_empty() {
            return CommandOutcome::captured(
                1,
                Vec::new(),
                b"resume: no restorable sessions (dead sessions with journaled state)\n".to_vec(),
            );
        }
        let mut out = String::new();
        for (i, info) in sessions.iter().enumerate() {
            let s = &info.session;
            let running = match (&s.foreground, s.jobs.len()) {
                (Some(fg), _) => format!("  · was running `{}`", truncate(&fg.cmd, 40)),
                (None, n) if n > 0 => format!("  · {n} background job(s)"),
                _ => String::new(),
            };
            out.push_str(&format!(
                "{:>3}  {:>4} ago  {}  {} change(s){}\n",
                i + 1,
                ago(mtime_unix(info.modified)),
                tilde(&s.cwd.clone().unwrap_or_else(|| "?".into())),
                s.delta_count(),
                running,
            ));
        }
        return CommandOutcome::captured(0, out.into_bytes(), Vec::new());
    }

    let target = match first {
        None => sessions.first(),
        Some(arg) => match arg.parse::<usize>() {
            Ok(n) => n.checked_sub(1).and_then(|i| sessions.get(i)),
            Err(_) => {
                return CommandOutcome::captured(
                    2,
                    Vec::new(),
                    format!("resume: unknown argument `{arg}` (usage: resume [list | N])\n")
                        .into_bytes(),
                );
            }
        },
    };
    let Some(info) = target else {
        let message = match first {
            None => "resume: nothing to restore — no session died with unsaved state\n".to_string(),
            Some(n) => format!("resume: no session #{n} (have {})\n", sessions.len()),
        };
        return CommandOutcome::captured(1, Vec::new(), message.into_bytes());
    };

    let notes = apply_session(&info.session, state);

    // Consume the journal so the same dead session isn't offered again; the
    // next command boundary re-journals the applied state into OUR journal, so
    // a later crash of this session still restores everything.
    SessionJournal::from_path(info.path.clone()).append(&SessionEvent::Restored {
        by: state.lookup("AGSH_SESSION").unwrap_or_default().to_string(),
        at: unix_now(),
    });

    let mut out = format!(
        "resume: restored session from {} ago\n",
        ago(mtime_unix(info.modified))
    );
    for note in &notes {
        out.push_str("  ");
        out.push_str(note);
        out.push('\n');
    }
    if let Some(fg) = &info.session.foreground {
        out.push_str(&format!(
            "  was running `{}` — it did not survive; {}\n",
            truncate(&fg.cmd, 60),
            resume_hint(&fg.cmd),
        ));
    }
    for job in &info.session.jobs {
        if job_still_alive(job.pgid, job.started_at) {
            out.push_str(&format!(
                "  background job `{}` SURVIVED (pgid {}) — signal it with `kill -- -{}`\n",
                truncate(&job.cmd, 60),
                job.pgid,
                job.pgid,
            ));
        } else {
            out.push_str(&format!(
                "  background job `{}` was running when the session died\n",
                truncate(&job.cmd, 60),
            ));
        }
    }
    CommandOutcome::captured(0, out.into_bytes(), Vec::new())
}

/// A program-aware hint for resurrecting the command that was running when the
/// session died. Claude/Codex sessions are genuinely resumable (`sessions`
/// finds them by id); everything else gets a rerun suggestion.
fn resume_hint(cmd: &str) -> String {
    let first = cmd.split_whitespace().next().unwrap_or("");
    let program = first.rsplit('/').next().unwrap_or(first);
    match program {
        "claude" | "codex" => format!(
            "`sessions` lists its transcript — resume it with `sessions N` \
             (true resume; {program} keeps its context)"
        ),
        _ => format!("rerun it with `{}`", truncate(cmd, 48)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agsh_store::session::{fold_session, read_journal};
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agsh_journal_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn boundary_diff_journals_only_real_deltas() {
        let dir = temp_dir("diff");
        let mut state = ShellState::from_current_process();
        let mut recorder = SessionRecorder::begin_in(&dir, &mut state);
        assert_eq!(state.lookup("AGSH_SESSION"), Some(recorder.id()));

        // No state change → only the fg bracket is journaled.
        recorder.command_started("ls");
        recorder.command_finished(&state, 0);

        state.export_var("DEPLOY_ENV", "staging");
        state.set_var("count", "3");
        state.set_alias("gs", "git status");
        state.set_function("hi", crate::state::ShellFunction::new("echo hi"));
        state.set_pipefail(true);
        state.set_shopt("globstar", true);
        state.set_positionals(&["a".into(), "b".into()]); // must NOT be journaled
        recorder.command_started("setup");
        recorder.command_finished(&state, 0);

        let events = read_journal(recorder.journal_handle().path());
        let folded = fold_session(&events);
        assert_eq!(
            folded.env.get("DEPLOY_ENV"),
            Some(&Some("staging".to_string()))
        );
        assert_eq!(folded.vars.get("count"), Some(&Some("3".to_string())));
        assert!(!folded.vars.contains_key("1"), "positionals excluded");
        assert!(!folded.vars.contains_key("@"), "positionals excluded");
        assert_eq!(
            folded.aliases.get("gs"),
            Some(&Some("git status".to_string()))
        );
        assert_eq!(folded.funcs.get("hi"), Some(&Some("echo hi".to_string())));
        assert_eq!(folded.opts.get("pipefail"), Some(&true));
        assert_eq!(folded.opts.get("globstar"), Some(&true));
        assert!(
            !folded.env.contains_key("AGSH_SESSION"),
            "session identity is never journaled"
        );

        // Unset flows through as a removal.
        state.unset("DEPLOY_ENV");
        recorder.command_finished(&state, 0);
        let folded = fold_session(&read_journal(recorder.journal_handle().path()));
        assert!(
            !folded.env.get("DEPLOY_ENV").is_some_and(Option::is_some),
            "unset export must not survive the fold: {:?}",
            folded.env.get("DEPLOY_ENV")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_round_trips_state_onto_a_fresh_shell() {
        let dir = temp_dir("rt");
        let mut state = ShellState::from_current_process();
        let mut recorder = SessionRecorder::begin_in(&dir, &mut state);
        state.export_var("API_URL", "http://localhost:1234");
        state.set_var("attempts", "7");
        state.set_alias("d", "docker compose");
        state.set_abbreviation("gco", "git checkout");
        state.set_function("greet", crate::state::ShellFunction::new("echo hello"));
        state.set_errexit(true);
        recorder.command_finished(&state, 0);
        // Session dies here (no Exit event); fold and replay onto a new shell.
        let folded = fold_session(&read_journal(recorder.journal_handle().path()));
        assert!(!folded.clean_exit);

        let mut fresh = ShellState::from_current_process();
        let notes = apply_session(&folded, &mut fresh);
        assert_eq!(fresh.lookup("API_URL"), Some("http://localhost:1234"));
        assert_eq!(fresh.lookup("attempts"), Some("7"));
        assert_eq!(fresh.alias("d"), Some("docker compose"));
        assert_eq!(fresh.abbreviation("gco"), Some("git checkout"));
        assert_eq!(
            fresh.function("greet").map(|f| f.body.as_str()),
            Some("echo hello")
        );
        assert!(fresh.errexit());
        assert!(!notes.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_exit_and_restored_sessions_are_not_restorable() {
        let dir = temp_dir("filter");
        // A dead pid: fork-less test — use a pid that can't exist (> pid_max).
        let dead_pid = 99_999_999;
        let make = |id: &str, events: &[SessionEvent]| {
            let journal = SessionJournal::create(&dir, id);
            journal.append(&SessionEvent::Start {
                id: id.into(),
                pid: dead_pid,
                cwd: "/w".into(),
                host: "h".into(),
                at: 1,
                version: "0".into(),
            });
            for e in events {
                journal.append(e);
            }
        };
        make(
            "crashed",
            &[SessionEvent::Env {
                k: "A".into(),
                v: "1".into(),
            }],
        );
        make(
            "clean",
            &[
                SessionEvent::Env {
                    k: "B".into(),
                    v: "1".into(),
                },
                SessionEvent::Exit { code: 0, at: 2 },
            ],
        );
        make(
            "consumed",
            &[
                SessionEvent::Env {
                    k: "C".into(),
                    v: "1".into(),
                },
                SessionEvent::Restored {
                    by: "x".into(),
                    at: 3,
                },
            ],
        );
        make("empty", &[]); // nothing to restore

        let restorable = restorable_sessions_in(&dir);
        assert_eq!(restorable.len(), 1, "{restorable:?}");
        assert_eq!(restorable[0].session.id, "crashed");

        // A journal whose pid is alive (ours) is a live session, not restorable.
        let journal = SessionJournal::create(&dir, "live");
        journal.append(&SessionEvent::Start {
            id: "live".into(),
            pid: std::process::id(),
            cwd: "/w".into(),
            host: "h".into(),
            at: 1,
            version: "0".into(),
        });
        journal.append(&SessionEvent::Env {
            k: "D".into(),
            v: "1".into(),
        });
        let restorable = restorable_sessions_in(&dir);
        assert_eq!(restorable.len(), 1);
        assert_eq!(restorable[0].session.id, "crashed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_hint_knows_resumable_agents() {
        assert!(resume_hint("claude --model opus").contains("sessions"));
        assert!(resume_hint("codex").contains("sessions"));
        assert!(resume_hint("npm run dev").contains("rerun"));
    }

    #[test]
    fn skip_keys_cover_identity_and_positionals() {
        for k in [
            "AGSH_SESSION",
            "PWD",
            "OLDPWD",
            "SHLVL",
            "_",
            "@",
            "1",
            "42",
        ] {
            assert!(skip_key(k), "{k} must be skipped");
        }
        for k in ["FOO", "PATH", "count", "A1"] {
            assert!(!skip_key(k), "{k} must be journaled");
        }
    }

    #[test]
    fn background_jobs_are_journaled_and_closed() {
        let dir = temp_dir("jobs");
        let mut state = ShellState::from_current_process();
        let mut recorder = SessionRecorder::begin_in(&dir, &mut state);

        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        state.register_job(child, "sleep 30 &");
        recorder.command_finished(&state, 0);

        let folded = fold_session(&read_journal(recorder.journal_handle().path()));
        assert_eq!(folded.jobs.len(), 1, "job journaled: {folded:?}");
        assert_eq!(folded.jobs[0].pgid, pid);
        assert_eq!(folded.jobs[0].cmd, "sleep 30 &");

        // Kill + reap: the next boundary closes the job record.
        let _ = rustix::process::kill_process(
            rustix::process::Pid::from_raw(pid).unwrap(),
            rustix::process::Signal::KILL,
        );
        // try_wait needs the child to be reapable; give the kernel a moment.
        for _ in 0..50 {
            if !state.reap_finished_jobs().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        recorder.command_finished(&state, 0);
        let folded = fold_session(&read_journal(recorder.journal_handle().path()));
        assert!(folded.jobs.is_empty(), "job_end journaled: {folded:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn job_liveness_guards_against_pid_reuse() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        let now = unix_now();
        assert!(
            job_still_alive(pid, now),
            "live process registered now must count as alive"
        );
        assert!(
            !job_still_alive(pid, now - 10_000),
            "same pid with a far-off registration time is a recycled pid, not our job"
        );
        let _ = child.kill();
        let _ = child.wait();
        assert!(!job_still_alive(pid, now), "dead process is dead");
        assert!(!job_still_alive(99_999_999, now), "nonexistent pid");
        assert!(!job_still_alive(0, now), "pgid 0 is never a job");
    }

    #[test]
    fn etime_parses_all_ps_forms() {
        assert_eq!(parse_etime("00:05"), Some(5));
        assert_eq!(parse_etime("12:34"), Some(12 * 60 + 34));
        assert_eq!(parse_etime("01:02:03"), Some(3600 + 2 * 60 + 3));
        assert_eq!(
            parse_etime("2-01:02:03"),
            Some(2 * 86_400 + 3600 + 2 * 60 + 3)
        );
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("garbage"), None);
    }
}
