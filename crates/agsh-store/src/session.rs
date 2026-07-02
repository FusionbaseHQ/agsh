//! Durable per-session journal: crash-safe state recovery for interactive
//! sessions.
//!
//! Every state *delta* an interactive session produces (cwd change, export,
//! alias/function definition, …) is appended to a per-session JSONL journal the
//! moment it is observed, using the same O_APPEND single-write discipline as the
//! history store — so the journal is always current and no save-on-exit step
//! exists to miss (crash-only design). A session that ends cleanly appends an
//! `exit` record; a journal without one whose shell process is gone marks a
//! session that died (crash, SIGHUP from a closed terminal, reboot) and can be
//! restored by *replaying the deltas* — never by re-running commands, so replay
//! has no side effects.
//!
//! The journal also acts as a flight recorder: `fg`/`fg_end` bracket the
//! foreground command line and `job`/`job_end` bracket background jobs, so a
//! restored session knows what was running when the previous one died.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// One journaled session event. State deltas carry the *net new value* (last
/// write wins on replay); lifecycle events carry timestamps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "e", rename_all = "snake_case")]
pub enum SessionEvent {
    /// Session opened: identity of the shell process that owns this journal.
    Start {
        id: String,
        pid: u32,
        cwd: String,
        host: String,
        at: u64,
        version: String,
    },
    /// Working directory changed.
    Cwd {
        path: String,
    },
    /// Exported variable set or changed.
    Env {
        k: String,
        v: String,
    },
    /// Shell-local (non-exported) variable set or changed.
    Var {
        k: String,
        v: String,
    },
    /// Variable removed (from both the shell and the exported environment).
    Unset {
        k: String,
    },
    Alias {
        k: String,
        v: String,
    },
    Unalias {
        k: String,
    },
    Abbr {
        k: String,
        v: String,
    },
    Unabbr {
        k: String,
    },
    /// Function defined (v is the body source).
    Func {
        k: String,
        v: String,
    },
    Unfunc {
        k: String,
    },
    /// Shell option toggled (`errexit`, `pipefail`, `shopt` names, …).
    Opt {
        k: String,
        on: bool,
    },
    /// Foreground command line started.
    Fg {
        cmd: String,
        at: u64,
    },
    /// Foreground command line finished.
    FgEnd {
        code: i32,
        at: u64,
    },
    /// Background job registered (pgid is the job's process-group id).
    Job {
        pgid: i32,
        cmd: String,
        at: u64,
    },
    /// Background job reaped.
    JobEnd {
        pgid: i32,
    },
    /// The shell received SIGHUP (terminal closed / connection dropped).
    Hup {
        at: u64,
    },
    /// A later session restored this journal's state (consumes it, so `resume`
    /// doesn't silently re-apply the same dead session twice). The file stays
    /// for inspection until pruned.
    Restored {
        by: String,
        at: u64,
    },
    /// Clean session end. A journal without one is a candidate for restore.
    Exit {
        code: i32,
        at: u64,
    },
}

/// Append-only writer for one session's journal file (`<dir>/<id>.jsonl`).
///
/// Stateless between appends: each append opens with O_APPEND and writes the
/// whole line in one `write_all`, so concurrent processes can't interleave a
/// half-line and a crash can lose at most the event being written.
#[derive(Debug, Clone)]
pub struct SessionJournal {
    path: PathBuf,
}

impl SessionJournal {
    pub fn create(dir: &Path, id: &str) -> Self {
        Self {
            path: dir.join(format!("{id}.jsonl")),
        }
    }

    /// A writer for an existing journal file (e.g. to mark it consumed).
    pub fn from_path(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one event. Best-effort: journaling must never break the shell,
    /// so I/O errors are swallowed. Journals can carry exported secrets, so the
    /// directory is created 0700 and the file 0600 (like the trace dir).
    pub fn append(&self, event: &SessionEvent) {
        if let Some(parent) = self.path.parent() {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            let _ = builder.create(parent);
        }
        if let Ok(mut line) = serde_json::to_string(event) {
            line.push('\n');
            let mut options = std::fs::OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            if let Ok(mut file) = options.open(&self.path) {
                let _ = file.write_all(line.as_bytes());
            }
        }
    }
}

/// Read a journal, skipping corrupt or non-UTF8 lines (a bad line must not
/// truncate the newer events after it — same rationale as the history loader).
pub fn read_journal(path: &Path) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    let Ok(file) = std::fs::File::open(path) else {
        return events;
    };
    let mut reader = BufReader::new(file);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<SessionEvent>(line) {
            events.push(event);
        }
    }
    events
}

/// A foreground command that was running when the journal's last event was
/// written (an `fg` without a matching `fg_end`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningCommand {
    pub cmd: String,
    pub started_at: u64,
}

/// A background job with no `job_end` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningJob {
    pub pgid: i32,
    pub cmd: String,
    pub started_at: u64,
}

/// The net state of a session, folded from its journal: per-key last-write-wins
/// deltas (`None` = removed), plus what was running at the end. Replaying these
/// onto a fresh shell reconstructs the dead session's state without re-running
/// anything.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RestorableSession {
    pub id: String,
    pub pid: u32,
    pub host: String,
    pub started_at: u64,
    pub cwd: Option<String>,
    /// Exported-variable deltas (`None` = unset).
    pub env: BTreeMap<String, Option<String>>,
    /// Shell-local variable deltas (`None` = unset).
    pub vars: BTreeMap<String, Option<String>>,
    pub aliases: BTreeMap<String, Option<String>>,
    pub abbrs: BTreeMap<String, Option<String>>,
    pub funcs: BTreeMap<String, Option<String>>,
    pub opts: BTreeMap<String, bool>,
    /// Foreground command running at the last journaled moment.
    pub foreground: Option<RunningCommand>,
    /// Background jobs never reaped by the session.
    pub jobs: Vec<RunningJob>,
    pub hangup: bool,
    /// A later session already restored this one.
    pub restored: bool,
    pub clean_exit: bool,
}

impl RestorableSession {
    /// Whether the session recorded any state worth restoring.
    pub fn has_state(&self) -> bool {
        self.cwd.is_some()
            || !self.env.is_empty()
            || !self.vars.is_empty()
            || !self.aliases.is_empty()
            || !self.abbrs.is_empty()
            || !self.funcs.is_empty()
            || !self.opts.is_empty()
    }

    /// Rough count of state deltas, for the restore banner.
    pub fn delta_count(&self) -> usize {
        usize::from(self.cwd.is_some())
            + self.env.len()
            + self.vars.len()
            + self.aliases.len()
            + self.abbrs.len()
            + self.funcs.len()
            + self.opts.len()
    }
}

/// Fold a journal into the session's net restorable state. A repeated `start`
/// record resets the fold (a reused journal file describes its newest session).
pub fn fold_session(events: &[SessionEvent]) -> RestorableSession {
    let mut s = RestorableSession::default();
    for event in events {
        match event {
            SessionEvent::Start {
                id,
                pid,
                cwd,
                host,
                at,
                ..
            } => {
                s = RestorableSession {
                    id: id.clone(),
                    pid: *pid,
                    host: host.clone(),
                    started_at: *at,
                    ..Default::default()
                };
                let _ = cwd;
            }
            SessionEvent::Cwd { path } => s.cwd = Some(path.clone()),
            SessionEvent::Env { k, v } => {
                s.env.insert(k.clone(), Some(v.clone()));
            }
            SessionEvent::Var { k, v } => {
                s.vars.insert(k.clone(), Some(v.clone()));
            }
            SessionEvent::Unset { k } => {
                // An unset only needs replaying if the key wasn't introduced by
                // this same session (rc-file or inherited state); a set-then-unset
                // within the session folds away to nothing.
                let served_env = s.env.remove(k).is_some_and(|v| v.is_some());
                let served_var = s.vars.remove(k).is_some_and(|v| v.is_some());
                if !served_env && !served_var {
                    s.vars.insert(k.clone(), None);
                }
            }
            SessionEvent::Alias { k, v } => {
                s.aliases.insert(k.clone(), Some(v.clone()));
            }
            SessionEvent::Unalias { k } => {
                if s.aliases.remove(k).flatten().is_none() {
                    s.aliases.insert(k.clone(), None);
                }
            }
            SessionEvent::Abbr { k, v } => {
                s.abbrs.insert(k.clone(), Some(v.clone()));
            }
            SessionEvent::Unabbr { k } => {
                if s.abbrs.remove(k).flatten().is_none() {
                    s.abbrs.insert(k.clone(), None);
                }
            }
            SessionEvent::Func { k, v } => {
                s.funcs.insert(k.clone(), Some(v.clone()));
            }
            SessionEvent::Unfunc { k } => {
                if s.funcs.remove(k).flatten().is_none() {
                    s.funcs.insert(k.clone(), None);
                }
            }
            SessionEvent::Opt { k, on } => {
                s.opts.insert(k.clone(), *on);
            }
            SessionEvent::Fg { cmd, at } => {
                s.foreground = Some(RunningCommand {
                    cmd: cmd.clone(),
                    started_at: *at,
                });
            }
            SessionEvent::FgEnd { .. } => s.foreground = None,
            SessionEvent::Job { pgid, cmd, at } => {
                s.jobs.push(RunningJob {
                    pgid: *pgid,
                    cmd: cmd.clone(),
                    started_at: *at,
                });
            }
            SessionEvent::JobEnd { pgid } => {
                s.jobs.retain(|j| j.pgid != *pgid);
            }
            SessionEvent::Hup { .. } => s.hangup = true,
            SessionEvent::Restored { .. } => s.restored = true,
            SessionEvent::Exit { .. } => s.clean_exit = true,
        }
    }
    s
}

/// One discovered session journal, newest first in [`list_sessions`].
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub session: RestorableSession,
    pub modified: SystemTime,
}

/// Discover and fold every journal in `dir`, newest first (by file mtime, which
/// tracks the last journaled activity).
pub fn list_sessions(dir: &Path) -> Vec<SessionInfo> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let events = read_journal(&path);
        if events.is_empty() {
            continue;
        }
        out.push(SessionInfo {
            path,
            session: fold_session(&events),
            modified,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

/// Keep at most this many session journals; [`prune_sessions`] drops the oldest.
pub const SESSION_FILE_CAP: usize = 40;

/// Remove the oldest journals beyond `keep` files, so the sessions directory
/// stays bounded across restarts (same policy as the trace-dir pruner).
pub fn prune_sessions(dir: &Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(PathBuf, SystemTime)> = rd
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                return None;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((path, modified))
        })
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort_by_key(|f| f.1); // oldest first
    let drop = files.len() - keep;
    for (path, _) in files.into_iter().take(drop) {
        let _ = std::fs::remove_file(path);
    }
}

/// A short, sortable-enough, collision-resistant session id.
pub fn new_session_id(pid: u32, unix_secs: u64) -> String {
    format!("{unix_secs}-{pid}")
}

/// Sessions directory: `$AGSH_SESSION_DIR`, else `$XDG_STATE_HOME/agsh/sessions`,
/// else `$HOME/.local/state/agsh/sessions`. (XDG *state*, not data: journals are
/// machine-local runtime state, like logs.)
pub fn default_sessions_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("AGSH_SESSION_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Some(Path::new(&xdg).join("agsh/sessions"));
    }
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".local/state/agsh/sessions"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agsh_sess_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn start(id: &str, pid: u32) -> SessionEvent {
        SessionEvent::Start {
            id: id.into(),
            pid,
            cwd: "/home/u".into(),
            host: "box".into(),
            at: 100,
            version: "0.1.0".into(),
        }
    }

    #[test]
    fn journal_round_trips_and_folds() {
        let dir = temp_dir("rt");
        let journal = SessionJournal::create(&dir, "s1");
        journal.append(&start("s1", 42));
        journal.append(&SessionEvent::Cwd {
            path: "/work".into(),
        });
        journal.append(&SessionEvent::Env {
            k: "FOO".into(),
            v: "bar".into(),
        });
        journal.append(&SessionEvent::Var {
            k: "local1".into(),
            v: "x".into(),
        });
        journal.append(&SessionEvent::Alias {
            k: "gs".into(),
            v: "git status".into(),
        });
        journal.append(&SessionEvent::Func {
            k: "hi".into(),
            v: "echo hi".into(),
        });
        journal.append(&SessionEvent::Opt {
            k: "pipefail".into(),
            on: true,
        });

        let events = read_journal(journal.path());
        assert_eq!(events.len(), 7);
        let s = fold_session(&events);
        assert_eq!(s.id, "s1");
        assert_eq!(s.pid, 42);
        assert_eq!(s.cwd.as_deref(), Some("/work"));
        assert_eq!(s.env.get("FOO"), Some(&Some("bar".to_string())));
        assert_eq!(s.vars.get("local1"), Some(&Some("x".to_string())));
        assert_eq!(s.aliases.get("gs"), Some(&Some("git status".to_string())));
        assert_eq!(s.funcs.get("hi"), Some(&Some("echo hi".to_string())));
        assert_eq!(s.opts.get("pipefail"), Some(&true));
        assert!(!s.clean_exit);
        assert!(s.has_state());
        assert_eq!(s.delta_count(), 6);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_line_does_not_truncate_newer_events() {
        let dir = temp_dir("bad");
        let path = dir.join("s.jsonl");
        let mut bytes = serde_json::to_vec(&start("s", 1)).unwrap();
        bytes.push(b'\n');
        bytes.extend_from_slice(&[0xff, 0xfe, b'\n']); // invalid UTF-8 line
        bytes.extend_from_slice(b"{\"e\":\"nonsense\"}\n"); // unknown event
        bytes.extend_from_slice(
            serde_json::to_string(&SessionEvent::Cwd { path: "/w".into() })
                .unwrap()
                .as_bytes(),
        );
        bytes.push(b'\n');
        std::fs::write(&path, bytes).unwrap();

        let events = read_journal(&path);
        assert_eq!(events.len(), 2, "kept events around the corrupt lines");
        assert_eq!(fold_session(&events).cwd.as_deref(), Some("/w"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_write_wins_and_unset_folds_away_session_local_sets() {
        let events = vec![
            start("s", 1),
            SessionEvent::Env {
                k: "A".into(),
                v: "1".into(),
            },
            SessionEvent::Env {
                k: "A".into(),
                v: "2".into(),
            },
            // Set within the session then unset: folds away entirely.
            SessionEvent::Var {
                k: "B".into(),
                v: "x".into(),
            },
            SessionEvent::Unset { k: "B".into() },
            // Unset of something the session never set: must replay as an unset.
            SessionEvent::Unset { k: "C".into() },
            SessionEvent::Alias {
                k: "gs".into(),
                v: "git status".into(),
            },
            SessionEvent::Unalias { k: "gs".into() },
            SessionEvent::Unalias { k: "ll".into() },
        ];
        let s = fold_session(&events);
        assert_eq!(s.env.get("A"), Some(&Some("2".to_string())));
        assert!(!s.vars.contains_key("B"), "set-then-unset folds away");
        assert_eq!(s.vars.get("C"), Some(&None), "external unset is kept");
        assert!(!s.aliases.contains_key("gs"));
        assert_eq!(s.aliases.get("ll"), Some(&None));
        let _ = std::fs::remove_dir_all(std::path::Path::new("/nonexistent"));
    }

    #[test]
    fn flight_recorder_tracks_running_foreground_and_jobs() {
        let mut events = vec![
            start("s", 1),
            SessionEvent::Fg {
                cmd: "make test".into(),
                at: 110,
            },
            SessionEvent::FgEnd { code: 0, at: 120 },
            SessionEvent::Job {
                pgid: 500,
                cmd: "sleep 100 &".into(),
                at: 121,
            },
            SessionEvent::Job {
                pgid: 501,
                cmd: "server &".into(),
                at: 122,
            },
            SessionEvent::JobEnd { pgid: 500 },
            SessionEvent::Fg {
                cmd: "claude".into(),
                at: 130,
            },
        ];
        let s = fold_session(&events);
        assert_eq!(
            s.foreground.as_ref().map(|f| f.cmd.as_str()),
            Some("claude")
        );
        assert_eq!(s.jobs.len(), 1);
        assert_eq!(s.jobs[0].pgid, 501);
        assert!(!s.clean_exit);

        events.push(SessionEvent::FgEnd { code: 0, at: 140 });
        events.push(SessionEvent::Exit { code: 0, at: 141 });
        let s = fold_session(&events);
        assert!(s.foreground.is_none());
        assert!(s.clean_exit);
    }

    #[test]
    fn repeated_start_resets_the_fold() {
        let events = vec![
            start("old", 1),
            SessionEvent::Env {
                k: "STALE".into(),
                v: "1".into(),
            },
            start("new", 2),
            SessionEvent::Cwd { path: "/n".into() },
        ];
        let s = fold_session(&events);
        assert_eq!(s.id, "new");
        assert!(s.env.is_empty());
        assert_eq!(s.cwd.as_deref(), Some("/n"));
    }

    #[test]
    fn list_sessions_orders_newest_first_and_prune_keeps_newest() {
        let dir = temp_dir("list");
        for i in 0..5u32 {
            let journal = SessionJournal::create(&dir, &format!("s{i}"));
            journal.append(&start(&format!("s{i}"), i));
            // Distinct mtimes (filesystem clocks can be coarse).
            let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 + i as u64);
            filetime_set(journal.path(), t).unwrap();
        }
        let sessions = list_sessions(&dir);
        assert_eq!(sessions.len(), 5);
        assert_eq!(sessions[0].session.id, "s4", "newest first");

        prune_sessions(&dir, 2);
        let sessions = list_sessions(&dir);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session.id, "s4");
        assert_eq!(sessions[1].session.id, "s3");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Set a file's mtime without external crates (via File::set_times).
    fn filetime_set(path: &Path, t: SystemTime) -> std::io::Result<()> {
        let file = std::fs::OpenOptions::new().write(true).open(path)?;
        file.set_times(std::fs::FileTimes::new().set_modified(t))
    }

    #[test]
    fn sessions_dir_honors_env_override() {
        // NOTE: env-var reads are process-global; this only asserts the override
        // path to avoid depending on the runner's HOME/XDG layout.
        std::env::set_var("AGSH_SESSION_DIR", "/tmp/agsh-test-sessions");
        assert_eq!(
            default_sessions_dir(),
            Some(PathBuf::from("/tmp/agsh-test-sessions"))
        );
        std::env::remove_var("AGSH_SESSION_DIR");
    }
}
