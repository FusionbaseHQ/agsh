//! Small shared helpers for compactors.

/// Join argv into a display string, single-quoting args with whitespace.
pub fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if arg.is_empty() || arg.chars().any(char::is_whitespace) {
                format!("'{arg}'")
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The basename of argv[0] (the program name), without a path or `.exe`.
pub fn command_basename(argv: &[String]) -> &str {
    argv.first()
        .map(String::as_str)
        .unwrap_or("")
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim_end_matches(".exe")
}

/// The first non-flag argument after the program (e.g. a git subcommand).
pub fn subcommand(argv: &[String]) -> Option<&str> {
    argv.iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .map(String::as_str)
}

/// Truncate a single line to `max` characters, appending an ellipsis marker.
pub fn clip(line: &str, max: usize) -> String {
    let line = line.trim_end();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let kept: String = line.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_and_subcommand() {
        let argv = vec![
            "/usr/bin/git".to_string(),
            "-C".to_string(),
            ".".to_string(),
            "status".to_string(),
        ];
        assert_eq!(command_basename(&argv), "git");
        // -C consumes ".", so the subcommand finder picks the first non-flag,
        // which is "." here; callers that need git semantics parse explicitly.
        assert_eq!(
            subcommand(&["git".to_string(), "status".to_string()]),
            Some("status")
        );
    }

    #[test]
    fn clips_long_lines() {
        assert_eq!(clip("hello", 10), "hello");
        assert_eq!(clip("hello world", 5), "hell…");
    }
}
