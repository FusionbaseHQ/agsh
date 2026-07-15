//! `agenv` — view, set, and restore exported environment variables.
//!
//! The restore family re-applies `export` (and `agenv`) assignments recorded
//! in command history. Recorded values are re-expanded (parameters, command
//! substitutions, quotes) with the shell's own expander at restore time —
//! exactly as if the export were typed again today. Listing and previewing
//! never expand anything, so a `$(command)` inside a recorded value cannot
//! run as a side effect of merely looking at history.

use std::collections::HashSet;

use agsh_core::{parse_line, CommandInvocation, QuoteKind, WordSegment};
use agsh_store::history::HistoryEntry;

use crate::builtins::{ago, coalesce_spaced_assignments, is_assignment_word, is_identifier};
use crate::executor::expand_word;
use crate::{CommandOutcome, ShellState};

pub(crate) const AGENV_HELP: &str = "\
agenv — view, set, and restore exported environment variables.

  agenv                     list exported variables (NAME=VALUE, sorted)
  agenv NAME …              print the value of NAME
  agenv NAME=VALUE …        set + export; spaces are fine: agenv NAME = VALUE
  agenv set NAME VALUE      explicit set form (also set NAME=VALUE)
  agenv get NAME …          explicit get form (for names shadowing a subcommand)
  agenv unset NAME …        remove NAME from the environment
  agenv history [FILTER]    assignments recorded in history, newest per name
  agenv restore NAME …      re-apply the recorded assignment for NAME
  agenv restore --all       re-apply every recorded assignment (newest per name)
  agenv restore             preview what `agenv restore --all` would re-apply

Restore scans history for successful `export`/`agenv` commands (spaced forms
like `export XYZ = 123` included) and re-expands each recorded value with
today's state: `export PATH=$PATH:/x` appends to the current PATH again, and
`$(command)` substitutions run again. Listing and previewing expand nothing.
";

pub fn builtin_agenv(args: &[String], state: &mut ShellState) -> CommandOutcome {
    match args.first().map(String::as_str) {
        None => list_env(None, state),
        Some("list" | "ls") => list_env(args.get(1).map(String::as_str), state),
        Some("--help" | "-h" | "help") => {
            CommandOutcome::captured(0, AGENV_HELP.as_bytes().to_vec(), Vec::new())
        }
        Some("get") => get_values(&args[1..], state),
        Some("set") => set_values(&args[1..], state),
        Some("unset" | "rm") => unset_names(&args[1..], state),
        Some("history" | "hist") => history_list(&args[1..], state),
        Some("restore") => restore(&args[1..], state),
        Some(first) if first.starts_with('-') => usage(&format!("{first}: unknown option")),
        Some(_) => {
            // Sugar: `agenv NAME …` prints, `agenv NAME=VALUE …` sets — the
            // spaced forms (`agenv NAME = VALUE`) coalesce like `export`.
            let words = coalesce_spaced_assignments(args);
            if words.iter().any(|word| word.contains('=')) {
                apply_assignments(&words, state)
            } else {
                get_values(args, state)
            }
        }
    }
}

fn usage(message: &str) -> CommandOutcome {
    CommandOutcome::captured(
        2,
        Vec::new(),
        format!("agenv: {message} (see `help agenv`)\n").into_bytes(),
    )
}

fn list_env(filter: Option<&str>, state: &ShellState) -> CommandOutcome {
    let mut out = String::new();
    for (name, value) in state.exported_env() {
        if filter.is_some_and(|f| !name.contains(f)) {
            continue;
        }
        out.push_str(name);
        out.push('=');
        out.push_str(value);
        out.push('\n');
    }
    CommandOutcome::captured(0, out.into_bytes(), Vec::new())
}

fn get_values(names: &[String], state: &ShellState) -> CommandOutcome {
    if names.is_empty() {
        return usage("get: expected a variable name");
    }
    let mut out = String::new();
    let mut stderr = String::new();
    let mut status = 0;
    for name in names {
        if let Some(value) = state.exported_env().get(name) {
            out.push_str(value);
            out.push('\n');
        } else {
            if state.lookup(name).is_some() {
                stderr.push_str(&format!(
                    "agenv: {name}: not exported (a shell variable exists; \
                     `export {name}` or `agenv {name}=VALUE`)\n"
                ));
            } else {
                stderr.push_str(&format!("agenv: {name}: not set\n"));
            }
            status = 1;
        }
    }
    CommandOutcome::captured(status, out.into_bytes(), stderr.into_bytes())
}

fn set_values(operands: &[String], state: &mut ShellState) -> CommandOutcome {
    if operands.is_empty() {
        return usage("set: expected NAME=VALUE or NAME VALUE");
    }
    let words = coalesce_spaced_assignments(operands);
    // `agenv set NAME VALUE` — the classic two-word form.
    if words.len() == 2 && is_identifier(&words[0]) {
        return apply_assignments(&[format!("{}={}", words[0], words[1])], state);
    }
    apply_assignments(&words, state)
}

fn apply_assignments(words: &[String], state: &mut ShellState) -> CommandOutcome {
    let mut stderr = String::new();
    let mut status = 0;
    for word in words {
        let Some((name, value)) = word.split_once('=') else {
            stderr.push_str(&format!(
                "agenv: {word}: not a valid assignment \
                 (expected NAME=VALUE; quote values containing spaces)\n"
            ));
            status = 1;
            continue;
        };
        if !is_identifier(name) {
            stderr.push_str(&format!("agenv: {word}: not a valid identifier\n"));
            status = 1;
        } else if !state.try_export_var(name, value) {
            stderr.push_str(&format!("agenv: {name}: readonly variable\n"));
            status = 1;
        }
    }
    CommandOutcome::captured(status, Vec::new(), stderr.into_bytes())
}

fn unset_names(names: &[String], state: &mut ShellState) -> CommandOutcome {
    if names.is_empty() {
        return usage("unset: expected a variable name");
    }
    let mut stderr = String::new();
    let mut status = 0;
    for name in names {
        if !is_identifier(name) {
            stderr.push_str(&format!("agenv: {name}: not a valid identifier\n"));
            status = 1;
        } else if !state.unset(name) {
            stderr.push_str(&format!("agenv: {name}: readonly variable\n"));
            status = 1;
        } else {
            state.unexport(name);
        }
    }
    CommandOutcome::captured(status, Vec::new(), stderr.into_bytes())
}

fn history_list(args: &[String], state: &ShellState) -> CommandOutcome {
    let filter = match args {
        [] => None,
        [only] => Some(only.as_str()),
        _ => return usage("history: at most one FILTER"),
    };
    let entries = state.history_entries_for_reading();
    let mut found = scan_history_assignments(&entries);
    if let Some(filter) = filter {
        found.retain(|assignment| assignment.name.contains(filter));
    }
    if found.is_empty() {
        return CommandOutcome::captured(
            0,
            b"no export assignments found in history\n".to_vec(),
            Vec::new(),
        );
    }
    let width = name_column_width(found.iter().map(|a| a.name.as_str()));
    let mut out = String::new();
    for assignment in &found {
        out.push_str(&format!(
            "{:<width$}  {:>4}  {}\n",
            assignment.name,
            ago(assignment.started_at),
            display_line(&assignment.source, 88),
        ));
    }
    CommandOutcome::captured(0, out.into_bytes(), Vec::new())
}

fn restore(args: &[String], state: &mut ShellState) -> CommandOutcome {
    let mut all = false;
    let mut names: Vec<&str> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--all" | "-a" => all = true,
            other if other.starts_with('-') => {
                return usage(&format!("restore: {other}: unknown option"));
            }
            other => names.push(other),
        }
    }
    if all && !names.is_empty() {
        return usage("restore: give NAME… or --all, not both");
    }

    let entries = state.history_entries_for_reading();
    let candidates = scan_history_assignments(&entries);

    if !all && names.is_empty() {
        // Preview: show the plan; expand nothing, change nothing.
        if candidates.is_empty() {
            return CommandOutcome::captured(
                0,
                b"nothing to restore: no export assignments found in history\n".to_vec(),
                Vec::new(),
            );
        }
        let width = name_column_width(candidates.iter().map(|a| a.name.as_str()));
        let mut out = String::from("would restore (run `agenv restore --all`, or pick names):\n");
        for assignment in &candidates {
            out.push_str(&format!(
                "  {:<width$}  {}\n",
                assignment.name,
                display_line(&assignment.source, 80),
            ));
        }
        return CommandOutcome::captured(0, out.into_bytes(), Vec::new());
    }

    let mut stderr = String::new();
    let mut status = 0;
    let selected: Vec<&HistoryAssignment> = if all {
        if candidates.is_empty() {
            return CommandOutcome::captured(
                1,
                Vec::new(),
                b"agenv: restore: no export assignments found in history\n".to_vec(),
            );
        }
        candidates.iter().collect()
    } else {
        let mut picked = Vec::new();
        for name in &names {
            match candidates.iter().find(|a| a.name == *name) {
                Some(assignment) => picked.push(assignment),
                None => {
                    stderr.push_str(&format!(
                        "agenv: restore: {name}: no export assignment found in history\n"
                    ));
                    status = 1;
                }
            }
        }
        picked
    };

    // Apply oldest-first so an assignment referencing another restored
    // variable (e.g. `export A=$B` recorded after `export B=…`) re-expands
    // against the replayed state, matching the original execution order.
    let mut out = String::new();
    for assignment in selected.into_iter().rev() {
        match expand_word(&assignment.value_segments, state) {
            Ok(value) => {
                if state.try_export_var(&assignment.name, &value) {
                    out.push_str(&format!(
                        "restored {}={}\n",
                        assignment.name,
                        display_line(&value, 80),
                    ));
                } else {
                    stderr.push_str(&format!("agenv: {}: readonly variable\n", assignment.name));
                    status = 1;
                }
            }
            Err(error) => {
                stderr.push_str(&format!(
                    "agenv: restore: {}: cannot re-expand recorded value: {error}\n",
                    assignment.name
                ));
                status = 1;
            }
        }
    }
    CommandOutcome::captured(status, out.into_bytes(), stderr.into_bytes())
}

/// One restorable assignment recovered from history: the newest recorded
/// assignment for its variable name.
struct HistoryAssignment {
    name: String,
    /// Unexpanded value, exactly as parsed from the recorded command line.
    value_segments: Vec<WordSegment>,
    /// The full recorded command line, for display.
    source: String,
    started_at: u64,
}

/// A word as (expanded-later) text plus its quote-preserving segments.
type Word = (String, Vec<WordSegment>);

/// Scan history (newest first) for `export`/`agenv` assignments, keeping the
/// newest assignment per variable name. Lines that failed (nonzero exit) are
/// skipped, as are compound commands, pipelines, and option forms such as
/// `export -n`: only plain top-level setter commands count, so the preview
/// shows exactly what restore will re-apply.
fn scan_history_assignments(entries: &[HistoryEntry]) -> Vec<HistoryAssignment> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut found = Vec::new();
    for entry in entries.iter().rev() {
        if entry.exit_code.is_some_and(|code| code != 0) {
            continue;
        }
        // Cheap prefilter before parsing: candidate lines must mention the
        // commands assignments are extracted from.
        if !entry.command.contains("export") && !entry.command.contains("agenv") {
            continue;
        }
        let Ok(graph) = parse_line(&entry.command) else {
            continue;
        };
        // Later list items / later words win within one line, so walk both in
        // reverse: the first time a name is seen is its newest assignment.
        for item in graph.list.items.iter().rev() {
            if item.background || item.pipeline.negated || item.pipeline.commands.len() != 1 {
                continue;
            }
            let command = &item.pipeline.commands[0];
            if !command.assignments.is_empty() {
                continue;
            }
            let Some((words, allow_pair)) = setter_operands(command) else {
                continue;
            };
            for (name, value_segments) in extract_assignments(&words, allow_pair).into_iter().rev()
            {
                if seen.insert(name.clone()) {
                    found.push(HistoryAssignment {
                        name,
                        value_segments,
                        source: entry.command.trim().to_string(),
                        started_at: entry.started_at,
                    });
                }
            }
        }
    }
    found
}

/// The operand words of `command` when it is a recognized env-setting
/// invocation (`export …`, `agenv NAME=…`, `agenv set …`); `None` otherwise.
/// The flag is true for `agenv set`, whose two-word `NAME VALUE` form pairs.
fn setter_operands(command: &CommandInvocation) -> Option<(Vec<Word>, bool)> {
    let (skip, allow_pair) = match command.argv.first().map(String::as_str)? {
        "export" => (1, false),
        "agenv" => match command.argv.get(1).map(String::as_str) {
            Some("set") => (2, true),
            // Non-setting subcommand lines never carry assignments to restore
            // (e.g. `agenv restore X=1` treats `X=1` as a name, not a set).
            Some(
                "get" | "unset" | "rm" | "history" | "hist" | "restore" | "list" | "ls" | "help"
                | "--help" | "-h",
            )
            | None => return None,
            // Anything else may open the `agenv NAME=…` sugar form.
            Some(_) => (1, false),
        },
        _ => return None,
    };
    let words = command
        .argv
        .iter()
        .zip(command.argv_segments.iter())
        .skip(skip)
        .map(|(text, segments)| (text.clone(), segments.clone()))
        .collect();
    Some((words, allow_pair))
}

/// Pull `NAME=VALUE` assignments (spaced forms included, mirroring
/// [`coalesce_spaced_assignments`]) out of operand words, preserving each
/// value's quote segments so it can be re-expanded faithfully later.
fn extract_assignments(words: &[Word], allow_pair: bool) -> Vec<(String, Vec<WordSegment>)> {
    // `agenv set NAME VALUE`: exactly two words, the first a bare name.
    if allow_pair && words.len() == 2 {
        if let Some(name) = bare_identifier(&words[0]) {
            if !words[1].0.starts_with('=') {
                return vec![(name.to_string(), words[1].1.clone())];
            }
        }
    }

    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let (text, segments) = &words[i];
        // Option words (`export -n FOO=…`) change the meaning of the whole
        // command: extract nothing rather than restore an un-export.
        if text.starts_with('-') && text != "--" {
            return Vec::new();
        }
        if text == "--" {
            i += 1;
            continue;
        }
        if let Some(name) = bare_identifier(&words[i]) {
            match words.get(i + 1) {
                // `NAME = VALUE` (also `NAME =` at end of line: empty value).
                Some(eq) if is_literal_eq(eq) => {
                    let value = words.get(i + 2).map(|(_, s)| s.clone()).unwrap_or_default();
                    out.push((name.to_string(), value));
                    i += if words.len() > i + 2 { 3 } else { 2 };
                    continue;
                }
                // `NAME =VALUE`.
                Some(next) => {
                    if let Some(value) = leading_eq_value(next) {
                        out.push((name.to_string(), value));
                        i += 2;
                        continue;
                    }
                    // Plain `export NAME` re-exports; nothing to restore.
                    i += 1;
                    continue;
                }
                None => {
                    i += 1;
                    continue;
                }
            }
        }
        if let Some((name, value, consumed_next)) =
            split_assignment_word(segments, words.get(i + 1))
        {
            out.push((name, value));
            i += if consumed_next { 2 } else { 1 };
            continue;
        }
        i += 1;
    }
    out
}

/// A word that is exactly one unquoted literal identifier.
fn bare_identifier(word: &Word) -> Option<&str> {
    let (text, segments) = word;
    (segments.len() == 1 && segments[0].quote == QuoteKind::None && is_identifier(text))
        .then_some(text.as_str())
}

/// A word that is exactly one unquoted literal `=`.
fn is_literal_eq(word: &Word) -> bool {
    let (text, segments) = word;
    text == "=" && segments.len() == 1 && segments[0].quote == QuoteKind::None
}

/// `=VALUE`: an unquoted leading `=` marks a spaced assignment's value word;
/// returns the value segments with that marker stripped.
fn leading_eq_value(word: &Word) -> Option<Vec<WordSegment>> {
    let (text, segments) = word;
    if text == "=" {
        return None;
    }
    let first = segments.first()?;
    if first.quote != QuoteKind::None || !first.text.starts_with('=') {
        return None;
    }
    let mut value = segments.clone();
    if first.text.len() == 1 {
        value.remove(0);
    } else {
        value[0].text = first.text[1..].to_string();
    }
    Some(value)
}

/// `NAME=VALUE` in a single word (plus the `NAME= VALUE` rescue, mirroring
/// [`coalesce_spaced_assignments`]): the name must be an unquoted literal.
fn split_assignment_word(
    segments: &[WordSegment],
    next: Option<&Word>,
) -> Option<(String, Vec<WordSegment>, bool)> {
    let first = segments.first()?;
    if first.quote != QuoteKind::None {
        return None;
    }
    let (name, rest) = first.text.split_once('=')?;
    if !is_identifier(name) {
        return None;
    }
    let mut value: Vec<WordSegment> = Vec::new();
    if !rest.is_empty() {
        value.push(WordSegment::new(rest, QuoteKind::None));
    }
    value.extend(segments[1..].iter().cloned());
    if value.is_empty() {
        if let Some((next_text, next_segments)) = next {
            if !is_identifier(next_text)
                && !is_assignment_word(next_text)
                && !next_text.starts_with('=')
                && next_text != "--"
            {
                return Some((name.to_string(), next_segments.clone(), true));
            }
        }
    }
    Some((name.to_string(), value, false))
}

fn name_column_width<'a>(names: impl Iterator<Item = &'a str>) -> usize {
    names.map(str::len).max().unwrap_or(0).min(28)
}

/// One display line: control characters flattened to spaces, truncated by
/// character count so long values or commands cannot wreck the layout.
fn display_line(text: &str, max_chars: usize) -> String {
    let sanitized: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if sanitized.chars().count() <= max_chars {
        sanitized
    } else {
        let mut cut: String = sanitized
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect();
        cut.push('…');
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state whose history reads never touch the developer's real history
    /// file, so assertions see exactly what each test records.
    fn hermetic_state() -> ShellState {
        let mut state = ShellState::from_current_process();
        state.disable_history_read_fallback_for_test();
        state
    }

    fn run(state: &mut ShellState, args: &[&str]) -> CommandOutcome {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        builtin_agenv(&args, state)
    }

    fn stdout(outcome: &CommandOutcome) -> String {
        String::from_utf8_lossy(&outcome.stdout).into_owned()
    }

    fn stderr(outcome: &CommandOutcome) -> String {
        String::from_utf8_lossy(&outcome.stderr).into_owned()
    }

    /// Record a finished command into the in-memory history.
    fn record(state: &mut ShellState, line: &str, exit_code: i32) {
        state.record_history(line);
        state.finalize_history(exit_code, 1);
    }

    #[test]
    fn set_get_list_unset_roundtrip() {
        let mut state = hermetic_state();

        assert_eq!(run(&mut state, &["AGENV_RT=one"]).exit_code, 0);
        assert_eq!(state.lookup("AGENV_RT"), Some("one"));
        assert!(state.is_exported("AGENV_RT"));

        // Spaced sugar re-sets it.
        assert_eq!(run(&mut state, &["AGENV_RT", "=", "two"]).exit_code, 0);
        assert_eq!(state.lookup("AGENV_RT"), Some("two"));

        let get = run(&mut state, &["AGENV_RT"]);
        assert_eq!(get.exit_code, 0);
        assert_eq!(stdout(&get), "two\n");

        assert_eq!(run(&mut state, &["set", "AGENV_RT", "three"]).exit_code, 0);
        let get = run(&mut state, &["get", "AGENV_RT"]);
        assert_eq!(stdout(&get), "three\n");

        let list = run(&mut state, &[]);
        assert!(
            stdout(&list).contains("AGENV_RT=three\n"),
            "{}",
            stdout(&list)
        );
        let filtered = run(&mut state, &["list", "AGENV_RT"]);
        assert!(stdout(&filtered)
            .lines()
            .all(|line| line.contains("AGENV_RT")));

        assert_eq!(run(&mut state, &["unset", "AGENV_RT"]).exit_code, 0);
        assert_eq!(state.lookup("AGENV_RT"), None);
        assert!(!state.is_exported("AGENV_RT"));
    }

    #[test]
    fn get_distinguishes_unset_from_unexported() {
        let mut state = hermetic_state();

        let missing = run(&mut state, &["get", "AGENV_MISSING"]);
        assert_eq!(missing.exit_code, 1);
        assert!(stderr(&missing).contains("not set"));

        assert!(state.try_set_var("AGENV_SHELLVAR", "x"));
        let unexported = run(&mut state, &["get", "AGENV_SHELLVAR"]);
        assert_eq!(unexported.exit_code, 1);
        assert!(stderr(&unexported).contains("not exported"));
    }

    #[test]
    fn history_lists_newest_assignment_per_name() {
        let mut state = hermetic_state();
        record(&mut state, "export AGENV_H1=alpha", 0);
        record(&mut state, "export AGENV_H1=beta AGENV_H2=gamma", 0);
        record(&mut state, "echo not-an-export", 0);

        let listing = run(&mut state, &["history"]);
        assert_eq!(listing.exit_code, 0);
        let text = stdout(&listing);
        assert_eq!(text.lines().count(), 2, "one row per name: {text}");
        assert!(text.contains("AGENV_H1=beta"), "{text}");
        assert!(text.contains("AGENV_H2"), "{text}");
        assert!(
            !text.contains("alpha"),
            "newest assignment must win: {text}"
        );

        let filtered = run(&mut state, &["history", "AGENV_H2"]);
        assert_eq!(
            stdout(&filtered).lines().count(),
            1,
            "{}",
            stdout(&filtered)
        );
    }

    #[test]
    fn restore_one_name_reapplies_only_that_name() {
        let mut state = hermetic_state();
        record(&mut state, "export AGENV_R1=beta AGENV_R2=gamma", 0);

        let outcome = run(&mut state, &["restore", "AGENV_R1"]);
        assert_eq!(outcome.exit_code, 0, "{}", stderr(&outcome));
        assert_eq!(state.lookup("AGENV_R1"), Some("beta"));
        assert!(state.is_exported("AGENV_R1"));
        assert_eq!(state.lookup("AGENV_R2"), None);
    }

    #[test]
    fn restore_all_reapplies_every_newest_assignment() {
        let mut state = hermetic_state();
        record(&mut state, "export AGENV_A1=old", 0);
        record(&mut state, "export AGENV_A1=new", 0);
        record(&mut state, "export AGENV_A2='two words'", 0);

        let outcome = run(&mut state, &["restore", "--all"]);
        assert_eq!(outcome.exit_code, 0, "{}", stderr(&outcome));
        assert_eq!(state.lookup("AGENV_A1"), Some("new"));
        assert_eq!(state.lookup("AGENV_A2"), Some("two words"));
        assert!(state.is_exported("AGENV_A1") && state.is_exported("AGENV_A2"));
    }

    #[test]
    fn restore_expands_recorded_value_against_current_state() {
        let mut state = hermetic_state();
        state.export_var("AGENV_BASE", "first");
        record(&mut state, "export AGENV_REF=$AGENV_BASE/sub", 0);
        state.export_var("AGENV_BASE", "second");

        let outcome = run(&mut state, &["restore", "AGENV_REF"]);
        assert_eq!(outcome.exit_code, 0, "{}", stderr(&outcome));
        assert_eq!(state.lookup("AGENV_REF"), Some("second/sub"));
    }

    #[test]
    fn restore_preserves_quoting_semantics() {
        let mut state = hermetic_state();
        record(&mut state, "export AGENV_DQ=\"a b\"", 0);
        record(&mut state, "export AGENV_SQ='$HOME stays literal'", 0);

        let outcome = run(&mut state, &["restore", "--all"]);
        assert_eq!(outcome.exit_code, 0, "{}", stderr(&outcome));
        assert_eq!(state.lookup("AGENV_DQ"), Some("a b"));
        assert_eq!(state.lookup("AGENV_SQ"), Some("$HOME stays literal"));
    }

    #[test]
    fn spaced_and_agenv_forms_in_history_are_restorable() {
        let mut state = hermetic_state();
        record(&mut state, "export AGENV_SP = 42", 0);
        record(&mut state, "agenv AGENV_SG=77", 0);
        record(&mut state, "agenv set AGENV_PAIR 88", 0);

        let outcome = run(&mut state, &["restore", "--all"]);
        assert_eq!(outcome.exit_code, 0, "{}", stderr(&outcome));
        assert_eq!(state.lookup("AGENV_SP"), Some("42"));
        assert_eq!(state.lookup("AGENV_SG"), Some("77"));
        assert_eq!(state.lookup("AGENV_PAIR"), Some("88"));
    }

    #[test]
    fn failed_option_and_subcommand_lines_are_skipped() {
        let mut state = hermetic_state();
        record(&mut state, "export AGENV_FAILED=oops", 1);
        record(&mut state, "export -n AGENV_OPT=1", 0);
        record(&mut state, "agenv restore AGENV_NOTSET=1", 0);
        record(&mut state, "export AGENV_PIPE=1 | cat", 0);

        let listing = run(&mut state, &["history"]);
        assert!(
            stdout(&listing).contains("no export assignments"),
            "{}",
            stdout(&listing)
        );

        let outcome = run(&mut state, &["restore", "--all"]);
        assert_eq!(outcome.exit_code, 1);
        assert!(stderr(&outcome).contains("no export assignments"));
        assert_eq!(state.lookup("AGENV_FAILED"), None);
        assert_eq!(state.lookup("AGENV_OPT"), None);
    }

    #[test]
    fn listing_and_preview_never_expand_recorded_values() {
        let mut state = hermetic_state();
        record(&mut state, "export AGENV_SUB=$(echo ran)", 0);

        let listing = run(&mut state, &["history"]);
        assert!(
            stdout(&listing).contains("$(echo ran)"),
            "{}",
            stdout(&listing)
        );
        assert_eq!(state.lookup("AGENV_SUB"), None);

        let preview = run(&mut state, &["restore"]);
        assert_eq!(preview.exit_code, 0);
        assert!(
            stdout(&preview).contains("AGENV_SUB"),
            "{}",
            stdout(&preview)
        );
        assert_eq!(state.lookup("AGENV_SUB"), None, "preview must not expand");

        let outcome = run(&mut state, &["restore", "AGENV_SUB"]);
        assert_eq!(outcome.exit_code, 0, "{}", stderr(&outcome));
        assert_eq!(state.lookup("AGENV_SUB"), Some("ran"));
    }

    #[test]
    fn restore_missing_name_reports_and_fails() {
        let mut state = hermetic_state();
        record(&mut state, "export AGENV_KNOWN=1", 0);

        let outcome = run(&mut state, &["restore", "AGENV_UNKNOWN"]);
        assert_eq!(outcome.exit_code, 1);
        assert!(stderr(&outcome).contains("AGENV_UNKNOWN"));
        assert!(stderr(&outcome).contains("no export assignment"));
    }

    #[test]
    fn restore_replays_oldest_first_for_cross_references() {
        let mut state = hermetic_state();
        record(&mut state, "export AGENV_X1=base", 0);
        record(&mut state, "export AGENV_X2=$AGENV_X1/leaf", 0);

        let outcome = run(&mut state, &["restore", "--all"]);
        assert_eq!(outcome.exit_code, 0, "{}", stderr(&outcome));
        assert_eq!(state.lookup("AGENV_X2"), Some("base/leaf"));
    }

    #[test]
    fn unknown_options_are_usage_errors() {
        let mut state = hermetic_state();
        assert_eq!(run(&mut state, &["--bogus"]).exit_code, 2);
        assert_eq!(run(&mut state, &["restore", "--bogus"]).exit_code, 2);
        assert_eq!(run(&mut state, &["restore", "--all", "NAME"]).exit_code, 2);
    }
}
