//! A context-aware, non-blocking prompt, styled via the shared theme.
//!
//! Segments: working directory (home-shortened), git branch + dirty marker,
//! Python venv, AWS profile, last-command duration (when slow), and last exit
//! status (when non-zero). Git info comes from `ShellState::git_context`, which
//! reads the branch from `.git/HEAD` and time-bounds the dirty probe, so the
//! prompt never blocks on a large repository. Colors honor terminal capability
//! and `NO_COLOR`; when stdout is not a TTY the prompt is plain text.

use std::io::IsTerminal;

use agsh_exec::ShellState;
use agsh_style::{Role, Theme};

/// Only surface command duration once it is slow enough to matter.
const SLOW_COMMAND_MS: u64 = 2_000;

pub fn render_prompt(state: &ShellState) -> String {
    let theme = if std::io::stdout().is_terminal() {
        state.theme()
    } else {
        Theme::plain()
    };
    let mut out = String::new();

    // Working directory, home-shortened.
    out.push_str(&theme.paint(Role::Accent, &short_cwd(state)));

    // Git branch + dirty marker + ahead/behind.
    if let Some(git) = state.git_context() {
        if let Some(branch) = git.branch {
            let dirty = git.dirty.unwrap_or(false);
            let role = if dirty { Role::Warn } else { Role::Ok };
            let icon = theme.icons.git_branch();
            let sep = if icon.is_empty() { "" } else { " " };
            let marker = if dirty { "*" } else { "" };
            out.push(' ');
            out.push_str(&theme.paint(role, &format!("{icon}{sep}{branch}{marker}")));
            if git.ahead > 0 {
                out.push_str(&theme.paint(
                    Role::Info,
                    &format!(" {}{}", theme.icons.ahead(), git.ahead),
                ));
            }
            if git.behind > 0 {
                out.push_str(&theme.paint(
                    Role::Info,
                    &format!(" {}{}", theme.icons.behind(), git.behind),
                ));
            }
        }
    }

    // Python virtualenv.
    if let Some(venv) = state.lookup("VIRTUAL_ENV").and_then(base_name) {
        out.push(' ');
        out.push_str(&theme.paint(Role::Operator, &format!("(venv:{venv})")));
    }

    // AWS profile.
    if let Some(profile) = state.lookup("AWS_PROFILE").filter(|p| !p.is_empty()) {
        out.push(' ');
        out.push_str(&theme.paint(Role::Info, &format!("aws:{profile}")));
    }

    // Duration of the previous command, if slow.
    if let Some(ms) = state.last_duration_ms() {
        if ms >= SLOW_COMMAND_MS {
            out.push(' ');
            out.push_str(&theme.paint(Role::Muted, &format_duration(ms)));
        }
    }

    // Non-zero exit status of the previous command.
    let status = state.last_status();
    if status != 0 {
        out.push(' ');
        out.push_str(&theme.paint(Role::Error, &format!("{}{status}", theme.icons.error())));
    }

    out.push(' ');
    let symbol = theme.icons.prompt();
    let role = if status == 0 { Role::Ok } else { Role::Error };
    out.push_str(&theme.paint(role, symbol));
    out.push(' ');
    out
}

fn short_cwd(state: &ShellState) -> String {
    let cwd = state.cwd().display().to_string();
    if let Some(home) = state.lookup("HOME").filter(|h| !h.is_empty()) {
        if cwd == home {
            return "~".to_string();
        }
        if let Some(rest) = cwd.strip_prefix(&format!("{home}/")) {
            return format!("~/{rest}");
        }
    }
    cwd
}

fn base_name(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.rsplit('/').next().unwrap_or(trimmed).to_string())
}

fn format_duration(ms: u64) -> String {
    if ms >= 60_000 {
        format!("{}m{}s", ms / 60_000, (ms % 60_000) / 1_000)
    } else if ms >= 1_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{ms}ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(350), "350ms");
        assert_eq!(format_duration(1_500), "1.5s");
        assert_eq!(format_duration(90_000), "1m30s");
    }

    #[test]
    fn base_name_of_path() {
        assert_eq!(base_name("/home/u/envs/proj"), Some("proj".to_string()));
        assert_eq!(base_name("/home/u/envs/proj/"), Some("proj".to_string()));
        assert_eq!(base_name(""), None);
    }
}
