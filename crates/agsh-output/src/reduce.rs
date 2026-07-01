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
    let original_count = text.lines().count();

    // Stage 1: per-line transforms (CR-collapse, ANSI strip, noise drop, clip).
    let mut lines: Vec<String> = Vec::new();
    for raw in text.lines() {
        let mut line = raw.to_string();
        if opts.collapse_cr {
            if let Some(idx) = line.rfind('\r') {
                line = line[idx + 1..].to_string();
            }
        }
        if opts.strip_ansi {
            line = strip_ansi(&line);
        }
        if opts.drop_noise && is_noise_line(&line) {
            continue;
        }
        if let Some(limit) = opts.truncate_line {
            line = clip(&line, limit);
        }
        lines.push(line);
    }

    // Stage 2: collapse blank runs.
    if opts.collapse_blanks {
        let mut collapsed: Vec<String> = Vec::with_capacity(lines.len());
        let mut prev_blank = false;
        for line in lines {
            let blank = line.trim().is_empty();
            if blank && prev_blank {
                continue;
            }
            prev_blank = blank;
            collapsed.push(line);
        }
        // Trim leading/trailing blank lines.
        while collapsed.first().is_some_and(|l| l.trim().is_empty()) {
            collapsed.remove(0);
        }
        while collapsed.last().is_some_and(|l| l.trim().is_empty()) {
            collapsed.pop();
        }
        lines = collapsed;
    }

    // Stage 3: collapse consecutive duplicate lines to `line  (×N)`.
    if opts.dedup_consecutive {
        let mut deduped: Vec<String> = Vec::with_capacity(lines.len());
        let mut run_count: usize = 0;
        let mut run_line: Option<String> = None;
        let flush = |out: &mut Vec<String>, line: &Option<String>, count: usize| {
            if let Some(l) = line {
                if count > 1 {
                    out.push(format!("{l}  (×{count})"));
                } else {
                    out.push(l.clone());
                }
            }
        };
        for line in lines {
            if run_line.as_deref() == Some(line.as_str()) {
                run_count += 1;
            } else {
                flush(&mut deduped, &run_line, run_count);
                run_line = Some(line);
                run_count = 1;
            }
        }
        flush(&mut deduped, &run_line, run_count);
        lines = deduped;
    }

    // Stage 4: head/tail window when over the cap.
    if lines.len() > opts.max_lines && opts.max_lines > 0 {
        let head = opts.head.min(lines.len());
        let tail = opts.tail.min(lines.len().saturating_sub(head));
        if head + tail < lines.len() {
            let omitted = lines.len() - head - tail;
            let mut windowed: Vec<String> = Vec::with_capacity(head + tail + 1);
            windowed.extend(lines[..head].iter().cloned());
            windowed.push(format!("… ({omitted} lines omitted) …"));
            windowed.extend(lines[lines.len() - tail..].iter().cloned());
            lines = windowed;
        } else {
            lines.truncate(opts.max_lines);
        }
    }

    // Safe fallback: never produce empty output from non-empty input.
    if lines.is_empty() && original_count > 0 {
        lines = text
            .lines()
            .take(opts.max_lines.max(1))
            .map(str::to_string)
            .collect();
    }

    let kept_real = lines.iter().filter(|l| !l.starts_with("… (")).count();
    let dropped = original_count.saturating_sub(kept_real);
    Reduced { lines, dropped }
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
}
