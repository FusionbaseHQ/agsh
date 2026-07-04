//! Context-aware completion engine for the dropdown menu.
//!
//! Given the current line and cursor, [`complete`] determines the completion
//! context (command, path, `cd`, git subcommand/branch, variable, redirection
//! target) and returns the replacement start plus the full candidate set for
//! that context. The editor filters/ranks this set live as the user types via
//! [`filter_rank`], so the dropdown narrows without re-querying the filesystem.

use std::collections::BTreeSet;

use agsh_exec::ShellState;
use agsh_style::Role;

/// What kind of thing a candidate is — drives the dim type tag in the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    Builtin,
    Command,
    Alias,
    Function,
    Dir,
    File,
    Branch,
    History,
    Variable,
}

impl CandidateKind {
    pub fn tag(self) -> &'static str {
        match self {
            CandidateKind::Builtin => "builtin",
            CandidateKind::Command => "cmd",
            CandidateKind::Alias => "alias",
            CandidateKind::Function => "fn",
            CandidateKind::Dir => "dir",
            CandidateKind::File => "file",
            CandidateKind::Branch => "branch",
            CandidateKind::History => "history",
            CandidateKind::Variable => "var",
        }
    }

    /// The theme role used to color this kind's value in the menu.
    pub fn role(self) -> Role {
        match self {
            CandidateKind::Builtin | CandidateKind::Command => Role::Command,
            CandidateKind::Alias | CandidateKind::Function => Role::Accent,
            CandidateKind::Dir => Role::Dir,
            CandidateKind::File => Role::File,
            CandidateKind::Branch => Role::Branch,
            CandidateKind::History => Role::History,
            CandidateKind::Variable => Role::Var,
        }
    }
}

/// Char indices in `hay` that match `needle` as a (case-insensitive) prefix or
/// subsequence, for highlighting in the menu. Empty if there's no match.
pub fn highlight_positions(needle: &str, hay: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let n: Vec<char> = needle.to_lowercase().chars().collect();
    let h: Vec<char> = hay.to_lowercase().chars().collect();
    let mut positions = Vec::new();
    let mut ni = 0;
    for (hi, &hc) in h.iter().enumerate() {
        if ni >= n.len() {
            break;
        }
        if hc == n[ni] {
            positions.push(hi);
            ni += 1;
        }
    }
    if ni == n.len() {
        positions
    } else {
        Vec::new()
    }
}

#[derive(Debug, Clone)]
pub struct Candidate {
    /// Text inserted to complete (dirs end with `/`).
    pub value: String,
    pub kind: CandidateKind,
    /// Append a space after accepting (false for directories, which descend).
    pub append_space: bool,
    /// Optional one-line description shown (muted) in the dropdown.
    pub description: Option<String>,
}

impl Candidate {
    fn new(value: impl Into<String>, kind: CandidateKind) -> Self {
        let append_space = kind != CandidateKind::Dir;
        Self {
            value: value.into(),
            kind,
            append_space,
            description: None,
        }
    }

    fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }
}

/// A completion: replace `start..cursor` (char indices) with a chosen candidate.
pub struct Completion {
    pub start: usize,
    pub candidates: Vec<Candidate>,
}

/// Compute completion candidates for `line` at char position `cursor`.
pub fn complete(line: &str, cursor: usize, state: &ShellState) -> Completion {
    let chars: Vec<char> = line.chars().collect();
    let cursor = cursor.min(chars.len());

    // The word under the cursor begins after the last delimiter.
    let word_start = chars[..cursor]
        .iter()
        .rposition(|c| c.is_whitespace() || matches!(c, '|' | '&' | ';' | '(' | '<' | '>'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let word: String = chars[word_start..cursor].iter().collect();

    // Tokens preceding the word (for context).
    let prefix: String = chars[..word_start].iter().collect();
    let prev_tokens: Vec<String> = prefix.split_whitespace().map(str::to_string).collect();
    let in_command_position = prev_tokens
        .last()
        .map(|t| matches!(t.as_str(), "|" | "&&" | "||" | ";" | "("))
        .unwrap_or(prev_tokens.is_empty());

    // $VAR completion.
    if let Some(name) = word.strip_prefix('$') {
        let var_start = word_start + 1; // after '$'
        return Completion {
            start: var_start,
            candidates: variable_candidates(state, name),
        };
    }

    // Redirection target -> files.
    let after_redirect = prefix.trim_end().ends_with(['>', '<']);

    if in_command_position && !word.contains('/') && !after_redirect {
        return Completion {
            start: word_start,
            candidates: command_candidates(state, &word),
        };
    }

    // Programmable completion: a `complete -W` word list registered for this
    // command supplies the argument candidates.
    if !after_redirect {
        if let Some(cmd) = prev_tokens.first() {
            if let Some(words) = state.completion_spec(cmd) {
                let candidates = words
                    .iter()
                    .map(|w| match w.split_once(':') {
                        // `word:description` lets specs carry descriptions.
                        Some((value, desc)) if !desc.is_empty() => {
                            Candidate::new(value, CandidateKind::Command).with_description(desc)
                        }
                        _ => Candidate::new(w.clone(), CandidateKind::Command),
                    })
                    .collect();
                return Completion {
                    start: word_start,
                    candidates,
                };
            }
        }
    }

    // git-aware completion.
    if prev_tokens.first().map(String::as_str) == Some("git") && !after_redirect {
        if let Some(c) = git_candidates(state, &prev_tokens) {
            return Completion {
                start: word_start,
                candidates: c,
            };
        }
    }

    // Path completion (default, cd, redirections). `cd` restricts to dirs.
    let dirs_only = prev_tokens.last().map(String::as_str) == Some("cd")
        || prev_tokens.last().map(String::as_str) == Some("pushd");
    let (dir_prefix, file_start) = split_path(&chars, word_start, cursor);
    Completion {
        start: file_start,
        candidates: path_candidates(state, &dir_prefix, dirs_only),
    }
}

/// Filter `candidates` by `word` and return indices ranked best-first. Prefix
/// matches rank above subsequence matches; original order breaks ties.
pub fn filter_rank(candidates: &[Candidate], word: &str) -> Vec<usize> {
    if word.is_empty() {
        return (0..candidates.len()).collect();
    }
    let needle = word.to_lowercase();
    let mut scored: Vec<(i64, usize)> = Vec::new();
    for (i, cand) in candidates.iter().enumerate() {
        let hay = cand.value.to_lowercase();
        if let Some(score) = match_score(&needle, &hay) {
            scored.push((score, i));
        }
    }
    // Stable: higher score first, original index for ties.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// Prefix matches score highest; then contiguous/word-boundary subsequence.
fn match_score(needle: &str, hay: &str) -> Option<i64> {
    if hay.starts_with(needle) {
        return Some(10_000 - hay.len() as i64);
    }
    // Subsequence with bonuses.
    let n: Vec<char> = needle.chars().collect();
    let h: Vec<char> = hay.chars().collect();
    let mut ni = 0;
    let mut score = 0i64;
    let mut prev: Option<usize> = None;
    for (hi, &hc) in h.iter().enumerate() {
        if ni >= n.len() {
            break;
        }
        if hc == n[ni] {
            score += 10;
            if let Some(p) = prev {
                if hi == p + 1 {
                    score += 8;
                }
            }
            if hi == 0 || matches!(h.get(hi.wrapping_sub(1)), Some('/' | '-' | '_' | '.')) {
                score += 6;
            }
            prev = Some(hi);
            ni += 1;
        }
    }
    (ni == n.len()).then_some(score - h.len() as i64 / 4)
}

fn variable_candidates(state: &ShellState, _name: &str) -> Vec<Candidate> {
    let mut names: Vec<String> = state.vars().keys().cloned().collect();
    names.sort();
    names
        .into_iter()
        .map(|n| Candidate::new(format!("${n}"), CandidateKind::Variable))
        .collect()
}

/// One-line descriptions for builtins, shown in the completion dropdown.
fn builtin_description(name: &str) -> Option<&'static str> {
    Some(match name {
        "cd" => "change the working directory",
        "pwd" => "print the working directory",
        "echo" => "write arguments to stdout",
        "printf" => "format and print arguments",
        "export" => "set environment variables",
        "unset" => "remove variables or functions",
        "alias" => "define or list aliases",
        "unalias" => "remove aliases",
        "source" | "." => "run a script in the current shell",
        "eval" => "evaluate arguments as a command",
        "exec" => "replace the shell with a command",
        "exit" => "exit the shell",
        "return" => "return from a function",
        "read" => "read a line into variables",
        "test" | "[" => "evaluate a conditional expression",
        "[[" => "evaluate an extended conditional",
        "declare" | "typeset" => "declare variables and attributes",
        "local" => "declare function-local variables",
        "readonly" => "mark variables read-only",
        "trap" => "run a command on a signal",
        "jobs" => "list background jobs",
        "fg" => "resume a job in the foreground",
        "bg" => "resume a job in the background",
        "kill" => "send a signal to a process",
        "wait" => "wait for background jobs",
        "shift" => "shift positional parameters",
        "getopts" => "parse positional options",
        "type" => "describe how a name is resolved",
        "command" => "run a command bypassing functions",
        "shopt" => "toggle shell options",
        "complete" => "define programmable completions",
        "agmath" => "evaluate floating-point arithmetic",
        "agview" => "render a file for the terminal (markdown, code, images, …)",
        "agpatch" => "apply a structured patch",
        "agz" | "agjump" => "jump to a frecent directory",
        "agtrace" => "inspect captured command output",
        "agtrust" => "manage agsh trust decisions",
        "agcontext" => "show project/session context for agents",
        "confine" => "restrict a command to an allowlist (agent guardrail)",
        "mode" => "show or set session default modes (e.g. mode:output compact)",
        "mode:output" => "set the session default output mode",
        "mode:intercept" => "route the agent's shells through agsh (compact:deep, off)",
        "agjob" => "run a command in the background with captured output",
        "sessions" => "list/resume Claude & Codex sessions for this folder",
        "history" => "search, inspect, and navigate command history",
        "umask" => "set the file-creation mask",
        "set" => "set shell options and positionals",
        _ => return None,
    })
}

fn command_candidates(state: &ShellState, _word: &str) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for name in agsh_exec::builtins::builtin_names() {
        if seen.insert(name.to_string()) {
            let cand = Candidate::new(*name, CandidateKind::Builtin);
            out.push(match builtin_description(name) {
                Some(desc) => cand.with_description(desc),
                None => cand,
            });
        }
    }
    for name in state.aliases().keys() {
        if seen.insert(name.clone()) {
            out.push(Candidate::new(name.clone(), CandidateKind::Alias));
        }
    }
    for name in state.functions().keys() {
        if seen.insert(name.clone()) {
            out.push(Candidate::new(name.clone(), CandidateKind::Function));
        }
    }
    let mut path_names: Vec<String> = state
        .lookup("PATH")
        .map(agsh_exec::suggest::path_executables)
        .unwrap_or_default();
    path_names.sort();
    for name in path_names {
        if seen.insert(name.clone()) {
            out.push(Candidate::new(name, CandidateKind::Command));
        }
    }
    // Recent history commands, most-recent first. Skip multiline commands
    // (e.g. heredocs): they can't be inline-completed into a single-line buffer
    // and would break the dropdown layout.
    for cmd in state.history_recent(300).into_iter().rev() {
        if cmd.contains('\n') || cmd.contains('\r') {
            continue;
        }
        if seen.insert(cmd.clone()) {
            out.push(Candidate::new(cmd, CandidateKind::History));
        }
    }
    out
}

fn git_candidates(state: &ShellState, prev_tokens: &[String]) -> Option<Vec<Candidate>> {
    // prev_tokens[0] == "git". Completing the subcommand (only "git" precedes)?
    let Some(sub_index) = git_subcommand_index(prev_tokens) else {
        const SUBS: &[&str] = &[
            "add",
            "branch",
            "checkout",
            "cherry-pick",
            "clone",
            "commit",
            "diff",
            "fetch",
            "init",
            "log",
            "merge",
            "pull",
            "push",
            "rebase",
            "remote",
            "reset",
            "restore",
            "show",
            "stash",
            "status",
            "switch",
            "tag",
        ];
        return Some(
            SUBS.iter()
                .map(|s| Candidate::new(*s, CandidateKind::Command))
                .collect(),
        );
    };
    let sub = prev_tokens[sub_index].as_str();
    if matches!(
        sub,
        "checkout" | "switch" | "merge" | "rebase" | "branch" | "log" | "diff" | "reset"
    ) {
        let branches = agsh_index::git_branches(state.cwd());
        if !branches.is_empty() {
            return Some(
                branches
                    .into_iter()
                    .map(|b| Candidate::new(b, CandidateKind::Branch))
                    .collect(),
            );
        }
    }
    None
}

/// Find the git subcommand token in `tokens` (`tokens[0] == "git"`), skipping global
/// options AND the arguments they consume. Returns `None` when the subcommand hasn't
/// been typed yet. Fixes the `git -C /path status` case where `-C`'s path argument
/// (not `-`-prefixed) was previously misread as the subcommand.
fn git_subcommand_index(tokens: &[String]) -> Option<usize> {
    // Global git options that take a SEPARATE argument (the next token).
    const TAKES_ARG: &[&str] = &[
        "-C",
        "-c",
        "--git-dir",
        "--work-tree",
        "--namespace",
        "--super-prefix",
        "--exec-path",
        "--config-env",
    ];
    let mut i = 1;
    while i < tokens.len() {
        let token = tokens[i].as_str();
        if let Some(rest) = token.strip_prefix('-') {
            if rest.is_empty() {
                // A bare "-" is not an option; treat as the (unusual) subcommand.
                return Some(i);
            }
            // `--opt=value` / `-cname=val` carry their argument inline: skip one.
            // A flag that takes a separate argument skips this token AND the next.
            if !token.contains('=') && TAKES_ARG.contains(&token) {
                i += 2;
            } else {
                i += 1;
            }
        } else {
            return Some(i);
        }
    }
    None
}

/// Split a path word into (directory-prefix-to-read, char index where the bare
/// filename being completed begins).
fn split_path(chars: &[char], word_start: usize, cursor: usize) -> (String, usize) {
    let word: String = chars[word_start..cursor].iter().collect();
    match word.rfind('/') {
        Some(idx) => {
            // dir prefix includes through the '/'
            let dir = word[..=idx].to_string();
            // file_start = word_start + char-count of dir prefix
            let file_start = word_start + dir.chars().count();
            (dir, file_start)
        }
        None => (String::new(), word_start),
    }
}

fn path_candidates(state: &ShellState, dir_prefix: &str, dirs_only: bool) -> Vec<Candidate> {
    // Resolve the directory to read, honoring ~ and relative paths.
    let read_path = if dir_prefix.is_empty() {
        state.cwd().to_path_buf()
    } else {
        let expanded = if let Some(rest) = dir_prefix.strip_prefix("~/") {
            if let Some(home) = state.lookup("HOME") {
                std::path::PathBuf::from(home).join(rest)
            } else {
                std::path::PathBuf::from(dir_prefix)
            }
        } else {
            let p = std::path::PathBuf::from(dir_prefix);
            if p.is_absolute() {
                p
            } else {
                state.cwd().join(p)
            }
        };
        expanded
    };

    let Ok(entries) = std::fs::read_dir(&read_path) else {
        return Vec::new();
    };
    let mut dirs: Vec<Candidate> = Vec::new();
    let mut files: Vec<Candidate> = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue; // skip dotfiles unless explicitly requested (future)
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            dirs.push(Candidate::new(format!("{name}/"), CandidateKind::Dir));
        } else if !dirs_only {
            files.push(Candidate::new(name, CandidateKind::File));
        }
    }
    dirs.sort_by_key(|c| c.value.clone());
    files.sort_by_key(|c| c.value.clone());
    dirs.extend(files);
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use agsh_exec::ShellState;

    fn toks(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn git_subcommand_index_skips_flag_arguments() {
        // Still completing the subcommand.
        assert_eq!(git_subcommand_index(&toks(&["git"])), None);
        assert_eq!(git_subcommand_index(&toks(&["git", "-C", "/path"])), None);
        assert_eq!(git_subcommand_index(&toks(&["git", "-p"])), None);
        // Subcommand present — the fix: `-C /path` no longer misreads /path.
        assert_eq!(git_subcommand_index(&toks(&["git", "status"])), Some(1));
        assert_eq!(
            git_subcommand_index(&toks(&["git", "-C", "/path", "status"])),
            Some(3)
        );
        assert_eq!(
            git_subcommand_index(&toks(&["git", "--git-dir=/foo", "log"])),
            Some(2)
        );
        assert_eq!(
            git_subcommand_index(&toks(&["git", "-p", "-C", "/x", "commit"])),
            Some(4)
        );
    }

    #[test]
    fn filter_ranks_prefix_first() {
        let cands = vec![
            Candidate::new("cargo", CandidateKind::Command),
            Candidate::new("scratch-cat", CandidateKind::File),
            Candidate::new("cat", CandidateKind::Command),
        ];
        let ranked = filter_rank(&cands, "cat");
        // "cat" (prefix) and "cargo"? no — needle "cat": "cat" prefix, "cargo" no
        // prefix but subseq c-a..t? cargo has c,a,r,g,o -> no 't'. So only cat +
        // scratch-cat (subseq). cat (prefix) ranks first.
        assert_eq!(cands[ranked[0]].value, "cat");
    }

    #[test]
    fn command_position_includes_builtins() {
        let state = ShellState::from_current_process();
        let c = complete("ec", 2, &state);
        assert_eq!(c.start, 0);
        assert!(c.candidates.iter().any(|c| c.value == "echo"));
    }

    #[test]
    fn excludes_multiline_history_candidates() {
        let state = ShellState::from_current_process();
        state.record_history("cat <<EOF\nline1\nline2\nEOF");
        state.record_history("echo single-line");
        let c = complete("ec", 2, &state);
        // No candidate value may contain a newline (would break the dropdown).
        assert!(c.candidates.iter().all(|cand| !cand.value.contains('\n')));
    }

    #[test]
    fn cd_completes_dirs_only() {
        let state = ShellState::from_current_process();
        let c = complete("cd ", 3, &state);
        assert!(c.candidates.iter().all(|c| c.kind == CandidateKind::Dir));
    }

    #[test]
    fn path_split_keeps_subdir() {
        let chars: Vec<char> = "cat src/ma".chars().collect();
        let (dir, start) = split_path(&chars, 4, 10);
        assert_eq!(dir, "src/");
        assert_eq!(start, 8); // after "src/"
    }
}
