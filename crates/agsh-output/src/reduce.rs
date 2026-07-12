//! Generic output reduction — ported and extended from rtk's filter pipeline.
//!
//! rtk (the "Rust Token Killer") reduces arbitrary command output with a
//! declarative line pipeline (strip-ANSI → drop noise → truncate → head/tail →
//! max). agsh already produces rich *structured* summaries for known command
//! families; this module brings the same generic line reduction to the long
//! tail of commands that have no bespoke compactor, so `compact` mode shrinks
//! everything — not just the seven families.
//!
//! Differences/improvements over rtk:
//! - agsh never modifies the command (no injected flags); it only reduces the
//!   captured output, so reduction is purely post-hoc and deterministic.
//! - Consecutive-duplicate lines collapse to `line  (×N)` (rtk has no generic
//!   dedup).
//! - Carriage-return progress (`\r`-overwritten lines) collapses to the final
//!   rendered state.
//! - Overflow is recoverable via agsh's `trace://` addressing (the caller adds
//!   the reference), a cleaner replacement for rtk's temp-file tee.
//! - A safe fallback never returns empty when the input was non-empty.

use std::collections::VecDeque;

const MAX_RETAINED_LINES: usize = 1_024;
const MAX_RETAINED_LINE_CHARS: usize = 4 * 1_024;

/// Options controlling generic line reduction.
#[derive(Debug, Clone)]
pub struct ReduceOptions {
    /// Strip ANSI escape sequences (CSI/OSC).
    pub strip_ansi: bool,
    /// Collapse runs of blank lines to a single blank line.
    pub collapse_blanks: bool,
    /// Collapse consecutive identical lines to `line  (×N)`.
    pub dedup_consecutive: bool,
    /// Keep only the final segment of `\r`-overwritten progress lines.
    pub collapse_cr: bool,
    /// Drop well-known progress/noise lines (Compiling/Downloading/…).
    pub drop_noise: bool,
    /// Clip each line to at most N characters (with an ellipsis).
    pub truncate_line: Option<usize>,
    /// When over `max_lines`, keep this many leading lines.
    pub head: usize,
    /// When over `max_lines`, keep this many trailing lines.
    pub tail: usize,
    /// Absolute cap on retained lines (head + tail window).
    pub max_lines: usize,
}

impl Default for ReduceOptions {
    fn default() -> Self {
        Self {
            strip_ansi: true,
            collapse_blanks: true,
            dedup_consecutive: true,
            collapse_cr: true,
            drop_noise: true,
            truncate_line: Some(400),
            head: 60,
            tail: 40,
            max_lines: 100,
        }
    }
}

/// The result of reducing some text.
#[derive(Debug, Clone)]
pub struct Reduced {
    /// Retained, reduced lines.
    pub lines: Vec<String>,
    /// How many original lines were removed (dropped, collapsed, or windowed).
    pub dropped: usize,
}

/// Reduce `text` line-by-line per `opts`. Never returns empty for non-empty
/// input (safe filter→raw fallback).
pub fn reduce(text: &str, opts: &ReduceOptions) -> Reduced {
    let mut original_count = 0usize;
    let (capacity, head, tail) = retained_limits(opts);
    let mut window = ReducedWindow::new(capacity, head, tail);
    let mut pending_blank = false;
    let mut seen_content = false;
    let mut run_line: Option<String> = None;
    let mut run_count = 0usize;

    for raw in text.lines() {
        original_count = original_count.saturating_add(1);
        let raw = if opts.collapse_cr {
            raw.rsplit('\r').next().unwrap_or(raw)
        } else {
            raw
        };
        let mut line = clip(raw, MAX_RETAINED_LINE_CHARS);
        if opts.strip_ansi {
            line = strip_ansi(&line);
        }
        if opts.drop_noise && is_noise_line(&line) {
            continue;
        }
        let truncate = opts
            .truncate_line
            .unwrap_or(MAX_RETAINED_LINE_CHARS)
            .min(MAX_RETAINED_LINE_CHARS);
        line = clip(&line, truncate);

        if opts.collapse_blanks && line.trim().is_empty() {
            pending_blank |= seen_content;
            continue;
        }
        if pending_blank {
            push_reduced_line(
                String::new(),
                opts.dedup_consecutive,
                &mut window,
                &mut run_line,
                &mut run_count,
            );
            pending_blank = false;
        }
        seen_content |= !line.trim().is_empty();
        push_reduced_line(
            line,
            opts.dedup_consecutive,
            &mut window,
            &mut run_line,
            &mut run_count,
        );
    }
    flush_run(&mut window, &mut run_line, &mut run_count);
    let mut lines = window.finish();

    // Safe fallback: never produce empty output from non-empty input.
    if lines.is_empty() && original_count > 0 {
        lines = text
            .lines()
            .take(capacity)
            .map(|line| clip(line, MAX_RETAINED_LINE_CHARS))
            .collect();
    }

    let kept_real = lines.iter().filter(|l| !l.starts_with("… (")).count();
    let dropped = original_count.saturating_sub(kept_real);
    Reduced { lines, dropped }
}

fn push_reduced_line(
    line: String,
    dedup: bool,
    window: &mut ReducedWindow,
    run_line: &mut Option<String>,
    run_count: &mut usize,
) {
    if !dedup {
        window.push(line);
        return;
    }
    if run_line.as_deref() == Some(line.as_str()) {
        *run_count = run_count.saturating_add(1);
    } else {
        flush_run(window, run_line, run_count);
        *run_line = Some(line);
        *run_count = 1;
    }
}

fn flush_run(window: &mut ReducedWindow, run_line: &mut Option<String>, run_count: &mut usize) {
    if let Some(line) = run_line.take() {
        let line = if *run_count > 1 {
            clip(
                &format!("{line}  (×{})", *run_count),
                MAX_RETAINED_LINE_CHARS,
            )
        } else {
            line
        };
        window.push(line);
    }
    *run_count = 0;
}

fn retained_limits(opts: &ReduceOptions) -> (usize, usize, usize) {
    let capacity = if opts.max_lines == 0 || opts.max_lines == usize::MAX {
        MAX_RETAINED_LINES
    } else {
        opts.max_lines.min(MAX_RETAINED_LINES)
    }
    .max(1);
    let mut head = opts.head.min(capacity);
    let tail = opts.tail.min(capacity.saturating_sub(head));
    if head == 0 && tail == 0 {
        head = capacity;
    }
    (capacity, head, tail)
}

struct ReducedWindow {
    capacity: usize,
    head: usize,
    tail_capacity: usize,
    total: usize,
    prefix: Vec<String>,
    tail: VecDeque<String>,
}

impl ReducedWindow {
    fn new(capacity: usize, head: usize, tail_capacity: usize) -> Self {
        Self {
            capacity,
            head,
            tail_capacity,
            total: 0,
            prefix: Vec::with_capacity(capacity),
            tail: VecDeque::with_capacity(tail_capacity),
        }
    }

    fn push(&mut self, line: String) {
        self.total = self.total.saturating_add(1);
        if self.prefix.len() < self.capacity {
            if self.tail_capacity == 0 {
                self.prefix.push(line);
                return;
            }
            self.prefix.push(line.clone());
        }
        if self.tail_capacity > 0 {
            if self.tail.len() == self.tail_capacity {
                self.tail.pop_front();
            }
            self.tail.push_back(line);
        }
    }

    fn finish(self) -> Vec<String> {
        if self.total <= self.capacity {
            return self.prefix;
        }
        let head = self.head.min(self.prefix.len());
        let tail = self.tail_capacity.min(self.tail.len());
        let omitted = self.total.saturating_sub(head).saturating_sub(tail);
        let mut lines = Vec::with_capacity(head + tail + usize::from(omitted > 0));
        lines.extend(self.prefix.into_iter().take(head));
        if omitted > 0 {
            lines.push(format!("… ({omitted} lines omitted) …"));
        }
        let skip = self.tail.len().saturating_sub(tail);
        lines.extend(self.tail.into_iter().skip(skip));
        lines
    }
}

/// Strip ANSI escape sequences (CSI `ESC [ … letter` and OSC `ESC ] … BEL/ST`).
pub fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    // CSI: parameters/intermediates until a final byte 0x40..=0x7e.
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    i += 1; // consume the final byte
                    continue;
                }
                b']' => {
                    // OSC: until BEL or ST (ESC \).
                    i += 2;
                    while i < bytes.len() && bytes[i] != 0x07 {
                        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
                _ => {
                    i += 2; // other two-byte escape
                    continue;
                }
            }
        }
        // Copy this UTF-8 character whole.
        let ch_len = utf8_len(bytes[i]);
        let end = (i + ch_len).min(bytes.len());
        out.push_str(&s[i..end]);
        i = end;
    }
    out
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// Clip a line to `limit` characters, appending `…` if it was cut.
fn clip(line: &str, limit: usize) -> String {
    if line.chars().count() <= limit {
        return line.to_string();
    }
    let mut out: String = line.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Whether a line is well-known progress/build noise that carries no signal.
/// Conservative on purpose — a generic reducer must not drop real output.
fn is_noise_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false; // blank handling is separate (collapse, not drop)
    }
    // Progress prefixes common to build tools and package managers.
    const NOISE_PREFIXES: &[&str] = &[
        "Compiling ",
        "Downloading ",
        "Downloaded ",
        "Installing ",
        "Fetching ",
        "Updating ",
        "Building ",
        "Compressing objects",
        "Receiving objects",
        "Resolving deltas",
        "Enumerating objects",
        "Counting objects",
        "Writing objects",
        "Unpacking objects",
        "remote: Compressing",
        "remote: Counting",
        "remote: Enumerating",
        "remote: Total",
        "make[",
    ];
    if NOISE_PREFIXES.iter().any(|p| t.starts_with(p)) {
        return true;
    }
    if t.contains("Entering directory") || t.contains("Leaving directory") {
        return true;
    }
    // A pure progress/percentage line, e.g. tqdm `45%|███…` or `[ 50%] Building`.
    if t.contains("%|") {
        return true;
    }
    if let Some(stripped) = t.strip_prefix('[') {
        // `[ 50%]` cmake/ninja-style progress.
        if let Some(close) = stripped.find(']') {
            let inner = stripped[..close].trim().trim_end_matches('%').trim();
            if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\x1b]0;title\x07ok"), "ok");
    }

    #[test]
    fn collapses_cr_progress() {
        let opts = ReduceOptions::default();
        let r = reduce("a\rb\rfinal", &opts);
        assert_eq!(r.lines, vec!["final"]);
    }

    #[test]
    fn dedups_consecutive() {
        let opts = ReduceOptions::default();
        let r = reduce("x\nx\nx\ny", &opts);
        assert_eq!(r.lines, vec!["x  (×3)".to_string(), "y".to_string()]);
    }

    #[test]
    fn drops_noise_keeps_signal() {
        let opts = ReduceOptions::default();
        let r = reduce(
            "Compiling foo v0.1\nDownloading bar\nreal output\nerror: boom",
            &opts,
        );
        assert!(r.lines.contains(&"real output".to_string()));
        assert!(r.lines.iter().any(|l| l.contains("error: boom")));
        assert!(!r.lines.iter().any(|l| l.starts_with("Compiling")));
        assert!(r.dropped >= 2);
    }

    #[test]
    fn windows_when_over_cap() {
        let opts = ReduceOptions {
            max_lines: 10,
            head: 3,
            tail: 2,
            dedup_consecutive: false,
            ..Default::default()
        };
        let input: String = (0..100).map(|i| format!("line{i}\n")).collect();
        let r = reduce(&input, &opts);
        assert_eq!(r.lines.len(), 6); // 3 head + omit marker + 2 tail
        assert!(r.lines[3].contains("omitted"));
        assert_eq!(r.lines[0], "line0");
        assert_eq!(r.lines[5], "line99");
    }

    #[test]
    fn never_empty_for_nonempty_input() {
        let opts = ReduceOptions::default();
        // All lines are noise; fallback must keep something.
        let r = reduce("Compiling a\nCompiling b", &opts);
        assert!(!r.lines.is_empty());
    }

    #[test]
    fn collapses_blank_runs() {
        let opts = ReduceOptions::default();
        let r = reduce("a\n\n\n\nb", &opts);
        assert_eq!(
            r.lines,
            vec!["a".to_string(), String::new(), "b".to_string()]
        );
    }

    #[test]
    fn unlimited_options_still_have_a_hard_retention_bound() {
        let opts = ReduceOptions {
            max_lines: usize::MAX,
            head: usize::MAX,
            tail: usize::MAX,
            dedup_consecutive: false,
            ..Default::default()
        };
        let input = (0..MAX_RETAINED_LINES + 1_000)
            .map(|i| format!("line-{i}\n"))
            .collect::<String>();
        let reduced = reduce(&input, &opts);
        assert!(reduced.lines.len() <= MAX_RETAINED_LINES + 1);
        assert!(reduced.lines.iter().any(|line| line.contains("omitted")));
    }

    #[test]
    fn absent_line_truncation_still_has_a_hard_width_bound() {
        let opts = ReduceOptions {
            truncate_line: None,
            ..Default::default()
        };
        let reduced = reduce(&"x".repeat(MAX_RETAINED_LINE_CHARS * 4), &opts);
        assert_eq!(reduced.lines.len(), 1);
        assert!(reduced.lines[0].chars().count() <= MAX_RETAINED_LINE_CHARS);
    }
}
