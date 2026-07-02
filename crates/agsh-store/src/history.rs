//! Rich, persistent command history with frecency ranking.
//!
//! Each entry records the command plus context (cwd, exit status, start time,
//! duration, hostname, project). The store ranks by *frecency* (frequency +
//! recency) for autosuggestions and fuzzy search, and persists as JSONL so a
//! session can be reconstructed. Ranking helpers take `now` explicitly so they
//! are deterministic and testable.

use std::io::{BufRead, BufReader, Write};
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
        }
    }
}

/// In-memory history with optional JSONL persistence.
#[derive(Debug, Default)]
pub struct HistoryStore {
    entries: Vec<HistoryEntry>,
    path: Option<PathBuf>,
    max: usize,
}

const DEFAULT_MAX: usize = 50_000;

impl HistoryStore {
    pub fn in_memory() -> Self {
        Self {
            entries: Vec::new(),
            path: None,
            max: DEFAULT_MAX,
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
        let max = max.max(1);
        let mut entries: Vec<HistoryEntry> = Vec::new();
        let mut total = 0usize;
        if let Ok(file) = std::fs::File::open(&path) {
            for line in BufReader::new(file).lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(entry) = serde_json::from_str::<HistoryEntry>(&line) {
                    total += 1;
                    entries.push(entry);
                    // Keep load memory bounded: never hold much more than `max`.
                    if entries.len() > max.saturating_mul(2) {
                        let drop = entries.len() - max;
                        entries.drain(0..drop);
                    }
                }
            }
        }
        if entries.len() > max {
            let drop = entries.len() - max;
            entries.drain(0..drop);
        }
        let store = Self {
            entries,
            path: Some(path),
            max,
        };
        // Compact a log that has outgrown the retained window down to `max`.
        if total > max.saturating_mul(2) {
            store.rewrite();
        }
        store
    }

    /// Atomically rewrite the backing file with the current (bounded) entries,
    /// via a temp file + rename, so a crash or a concurrent reader never sees a
    /// half-written log.
    fn rewrite(&self) {
        let Some(path) = &self.path else { return };
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        let mut buf = String::new();
        for entry in &self.entries {
            if let Ok(line) = serde_json::to_string(entry) {
                buf.push_str(&line);
                buf.push('\n');
            }
        }
        if let Ok(mut file) = std::fs::File::create(&tmp) {
            if file.write_all(buf.as_bytes()).is_ok() && file.flush().is_ok() {
                let _ = std::fs::rename(&tmp, path);
            } else {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    /// Append a new (not-yet-finalized) entry; returns its index.
    pub fn push(&mut self, entry: HistoryEntry) -> usize {
        self.entries.push(entry);
        if self.entries.len() > self.max {
            let drop = self.entries.len() - self.max;
            self.entries.drain(0..drop);
        }
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
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut line) = serde_json::to_string(entry) {
            line.push('\n');
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
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
