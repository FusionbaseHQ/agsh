//! Output normalization for agent observations. These transforms apply only to
//! the observation/display stream — never to the raw process streams (which are
//! preserved exactly and addressable via `trace://` references).

/// Options controlling normalization, mirroring the `[normalization]` config.
#[derive(Debug, Clone)]
pub struct NormalizeOptions {
    pub strip_ansi: bool,
    pub collapse_progress: bool,
    pub dedupe_repeated_lines: bool,
    pub shorten_home: bool,
    pub shorten_workspace: bool,
    pub home: Option<String>,
    pub workspace: Option<String>,
}

impl Default for NormalizeOptions {
    fn default() -> Self {
        Self {
            strip_ansi: true,
            collapse_progress: true,
            dedupe_repeated_lines: true,
            shorten_home: true,
            shorten_workspace: true,
            home: None,
            workspace: None,
        }
    }
}

/// Apply the normalization pipeline to a block of text.
pub fn normalize(input: &str, options: &NormalizeOptions) -> String {
    let mut text = input.to_string();
    if options.collapse_progress {
        text = collapse_carriage_returns(&text);
    }
    if options.strip_ansi {
        text = strip_ansi(&text);
        text = sanitize_control_chars(&text);
    }
    if options.shorten_workspace {
        if let Some(workspace) = options.workspace.as_deref() {
            text = shorten_prefix(&text, workspace, ".");
        }
    }
    if options.shorten_home {
        if let Some(home) = options.home.as_deref() {
            text = shorten_prefix(&text, home, "~");
        }
    }
    if options.dedupe_repeated_lines {
        text = dedupe_repeated_lines(&text);
    }
    text
}

/// Replace bare C0/C1 control characters (except `\n` and `\t`) with U+FFFD, so a
/// crafted filename or log line can't use raw backspaces, BEL, or other controls
/// to rewrite what a human sees (e.g. hide a `rm -rf`) or spam the terminal.
/// Observation-only — the raw stream is preserved exactly. Runs after
/// [`strip_ansi`], which has already removed well-formed escape sequences; this
/// catches the stray controls those don't cover.
fn sanitize_control_chars(input: &str) -> String {
    if !input
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return input.to_string();
    }
    input
        .chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

/// Remove ANSI escape sequences: CSI (`ESC[...`), OSC (`ESC]...BEL`/`ST`), and
/// two-character escapes.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                // CSI: parameters/intermediates then a final byte 0x40..=0x7e.
                let _ = chars.next();
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                // OSC: terminated by BEL or ESC\ (ST).
                let _ = chars.next();
                while let Some(next) = chars.next() {
                    if next == '\u{07}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        let _ = chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                // Two-character escape (e.g. ESC=, ESC>).
                let _ = chars.next();
            }
            None => {}
        }
    }
    out
}

/// Collapse carriage-return progress lines: within a physical line, keep only
/// the text after the last `\r` (what the terminal would actually show).
pub fn collapse_carriage_returns(input: &str) -> String {
    input
        .split('\n')
        .map(|line| {
            let trimmed = line.strip_suffix('\r').unwrap_or(line);
            match trimmed.rsplit_once('\r') {
                Some((_, last)) => last,
                None => trimmed,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collapse runs of identical consecutive lines into `line  (xN)`.
pub fn dedupe_repeated_lines(input: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut last: Option<String> = None;
    let mut count = 0usize;

    let flush = |out: &mut Vec<String>, line: &str, count: usize| {
        if count > 1 {
            out.push(format!("{line}  (x{count})"));
        } else {
            out.push(line.to_string());
        }
    };

    for line in input.split('\n') {
        match &last {
            Some(prev) if prev == line => count += 1,
            Some(prev) => {
                flush(&mut out, prev, count);
                last = Some(line.to_string());
                count = 1;
            }
            None => {
                last = Some(line.to_string());
                count = 1;
            }
        }
    }
    if let Some(prev) = last {
        flush(&mut out, &prev, count);
    }
    out.join("\n")
}

/// Replace occurrences of an absolute `prefix` path with a short marker.
fn shorten_prefix(input: &str, prefix: &str, marker: &str) -> String {
    if prefix.is_empty() {
        return input.to_string();
    }
    input.replace(prefix, marker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_and_osc() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{07}body"), "body");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn sanitizes_bare_control_chars() {
        // SHIP_READINESS_PLAN P1-15: backspace/BEL/etc. must not survive into an
        // observation and rewrite the display; newlines and tabs are preserved.
        assert_eq!(sanitize_control_chars("rm\u{08} -rf"), "rm\u{FFFD} -rf");
        assert_eq!(
            sanitize_control_chars("a\u{07}b\u{00}c"),
            "a\u{FFFD}b\u{FFFD}c"
        );
        assert_eq!(
            sanitize_control_chars("keep\tthis\nand this"),
            "keep\tthis\nand this"
        );
        // And it's wired into the full pipeline via normalize().
        let opts = NormalizeOptions {
            home: None,
            workspace: None,
            ..Default::default()
        };
        assert_eq!(normalize("x\u{08}y", &opts), "x\u{FFFD}y");
    }

    #[test]
    fn collapses_progress() {
        assert_eq!(collapse_carriage_returns("10%\r50%\r100%"), "100%");
        assert_eq!(collapse_carriage_returns("a\nb"), "a\nb");
    }

    #[test]
    fn dedupes_repeated() {
        assert_eq!(dedupe_repeated_lines("x\nx\nx\ny"), "x  (x3)\ny");
        assert_eq!(dedupe_repeated_lines("a\nb"), "a\nb");
    }

    #[test]
    fn shortens_home_and_workspace() {
        let options = NormalizeOptions {
            home: Some("/home/u".to_string()),
            workspace: Some("/home/u/proj".to_string()),
            ..NormalizeOptions::default()
        };
        // Workspace is applied before home, so the more specific prefix wins.
        let out = normalize("/home/u/proj/src and /home/u/file", &options);
        assert_eq!(out, "./src and ~/file");
    }
}
