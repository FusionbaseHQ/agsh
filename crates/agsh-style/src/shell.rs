//! Tolerant shell syntax highlighting, styled through the shared theme.
//!
//! Unlike the real lexer/parser (which error on incomplete input), this scanner
//! never fails: it colors a line by walking characters and classifying spans.
//! Command words are validated via a caller-supplied predicate so interactive
//! contexts can show unknown commands in the error color without this crate
//! knowing how to resolve PATH.

use crate::{Role, Theme};

/// Produce a styled rendering of `line`. `valid` decides whether a
/// command-position word is a known command.
pub fn highlight_shell(line: &str, theme: &Theme, valid: &dyn Fn(&str) -> bool) -> String {
    highlight_shell_inner(line, theme, valid, true, true)
}

/// Produce shell syntax coloring without command or filesystem resolution.
///
/// This is meant for already-recorded command text, such as history rows, where
/// correctness comes from syntax spans (strings, variables, operators, flags)
/// and not from re-checking today's PATH or probing paths on disk.
pub fn highlight_shell_without_resolution(line: &str, theme: &Theme) -> String {
    highlight_shell_inner(line, theme, &|_| true, false, false)
}

fn highlight_shell_inner(
    line: &str,
    theme: &Theme,
    valid: &dyn Fn(&str) -> bool,
    validate_commands: bool,
    underline_existing_paths: bool,
) -> String {
    if !theme.enabled() {
        return line.to_string();
    }

    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len() + 16);
    let mut i = 0;
    // A word is in "command position" at the start and after a separator.
    let mut command_position = true;
    let mut assignment_word = false;
    let mut can_start_comment = true;

    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => {
                out.push(c);
                i += 1;
                assignment_word = false;
                can_start_comment = true;
            }
            '#' if can_start_comment && !assignment_word => {
                let rest: String = chars[i..].iter().collect();
                out.push_str(&theme.paint(Role::Comment, &rest));
                break;
            }
            '#' => {
                out.push('#');
                i += 1;
                can_start_comment = false;
            }
            '\'' => {
                let (span, next) = read_quoted(&chars, i, '\'');
                out.push_str(&theme.paint(Role::Str, &span));
                i = next;
                if !assignment_word {
                    command_position = false;
                }
                can_start_comment = false;
            }
            '"' => {
                let (span, next) = read_quoted(&chars, i, '"');
                out.push_str(&theme.paint(Role::Str, &span));
                i = next;
                if !assignment_word {
                    command_position = false;
                }
                can_start_comment = false;
            }
            '$' => {
                let (span, next) = read_var(&chars, i);
                out.push_str(&theme.paint(Role::Var, &span));
                i = next;
                if !assignment_word {
                    command_position = false;
                }
                can_start_comment = false;
            }
            '|' | '&' | ';' | '<' | '>' | '(' | ')' => {
                let (span, next) = read_operator(&chars, i);
                out.push_str(&theme.paint(Role::Operator, &span));
                i = next;
                command_position = true;
                assignment_word = false;
                can_start_comment = true;
            }
            _ => {
                let (word, next) = read_word(&chars, i);
                if assignment_word {
                    out.push_str(&word);
                    i = next;
                    can_start_comment = false;
                    continue;
                }
                if command_position && is_assignment_word(&word) {
                    out.push_str(&theme.paint(Role::Var, &word));
                    i = next;
                    assignment_word = true;
                    can_start_comment = false;
                    continue;
                }
                if command_position && is_mode_keyword(&word) {
                    // agsh mode vocabulary: paint orange. The output-mode wrappers
                    // (`compact git ...`) wrap a following command, so keep command
                    // position. `agview`/`mode`/`mode:*` take FILE/value args, not a
                    // command, so their following word should not be red as invalid.
                    out.push_str(&theme.paint(Role::ModeKeyword, &word));
                    i = next;
                    command_position = is_output_mode_wrapper(&word);
                    can_start_comment = false;
                    continue;
                }
                if command_position {
                    let role = if !validate_commands || valid(&word) {
                        Role::Command
                    } else {
                        Role::CommandInvalid
                    };
                    out.push_str(&theme.paint(role, &word));
                } else if word.contains('/') || word.starts_with('~') {
                    // Underline paths that actually exist on disk.
                    let style = theme.style(Role::Path);
                    let style = if underline_existing_paths && std::path::Path::new(&word).exists()
                    {
                        style.underline()
                    } else {
                        style
                    };
                    out.push_str(&style.paint(&word, theme.level));
                } else if word.starts_with('-') {
                    out.push_str(&theme.paint(Role::Flag, &word));
                } else {
                    out.push_str(&word);
                }
                i = next;
                command_position = false;
                can_start_comment = false;
            }
        }
    }
    out
}

fn is_assignment_word(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// The output-mode wrappers that precede a command (`compact git ...`,
/// `raw npm ...`). These keep command position so the wrapped command
/// highlights correctly.
fn is_output_mode_wrapper(word: &str) -> bool {
    matches!(
        word,
        "raw"
            | "clean"
            | "compact"
            | "semantic"
            | "lossless-ref"
            | "lossless_ref"
            | "silent"
            | "rich"
    )
}

/// The agsh output-mode vocabulary: the per-command wrappers (mirrors
/// `OutputMode`), the `agview` rich-render sugar, and the `mode`/`mode:<aspect>`
/// builtin family that sets session defaults.
fn is_mode_keyword(word: &str) -> bool {
    if word == "mode" || word.starts_with("mode:") {
        return true;
    }
    matches!(
        word,
        "raw"
            | "clean"
            | "compact"
            | "semantic"
            | "lossless-ref"
            | "lossless_ref"
            | "silent"
            | "rich"
            | "agview"
    )
}

fn read_quoted(chars: &[char], start: usize, quote: char) -> (String, usize) {
    let mut i = start + 1;
    while i < chars.len() {
        if chars[i] == quote {
            i += 1;
            break;
        }
        if quote == '"' && chars[i] == '\\' && i + 1 < chars.len() {
            i += 2;
            continue;
        }
        i += 1;
    }
    (chars[start..i].iter().collect(), i)
}

fn read_var(chars: &[char], start: usize) -> (String, usize) {
    let mut i = start + 1;
    if chars.get(i) == Some(&'{') {
        while i < chars.len() && chars[i] != '}' {
            i += 1;
        }
        if i < chars.len() {
            i += 1;
        }
    } else {
        while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
    }
    (chars[start..i].iter().collect(), i)
}

fn read_operator(chars: &[char], start: usize) -> (String, usize) {
    let c = chars[start];
    let mut i = start + 1;
    if matches!(c, '&' | '|' | '>' | '<') && chars.get(i) == Some(&c) {
        i += 1;
    }
    (chars[start..i].iter().collect(), i)
}

fn read_word(chars: &[char], start: usize) -> (String, usize) {
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace()
            || matches!(
                c,
                '|' | '&' | ';' | '<' | '>' | '(' | ')' | '\'' | '"' | '#' | '$'
            )
        {
            break;
        }
        i += 1;
    }
    (chars[start..i].iter().collect(), i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ColorLevel, Icons, Palette, Theme};

    fn theme() -> Theme {
        Theme {
            palette: Palette::dark(),
            level: ColorLevel::TrueColor,
            icons: Icons::disabled(),
        }
    }

    fn known(cmd: &str) -> bool {
        matches!(cmd, "echo" | "git" | "ls")
    }

    #[test]
    fn valid_and_invalid_commands_differ() {
        let t = theme();
        let valid = highlight_shell("echo hi", &t, &known);
        let invalid = highlight_shell("nope hi", &t, &known);
        assert!(valid.contains(&t.paint(Role::Command, "echo")));
        assert!(invalid.contains(&t.paint(Role::CommandInvalid, "nope")));
    }

    #[test]
    fn colors_strings_vars_and_operators() {
        let t = theme();
        let out = highlight_shell("echo 'a' $HOME | ls", &t, &known);
        assert!(out.contains(&t.paint(Role::Str, "'a'")));
        assert!(out.contains(&t.paint(Role::Var, "$HOME")));
        assert!(out.contains(&t.paint(Role::Operator, "|")));
    }

    #[test]
    fn plain_theme_is_uncolored_but_intact() {
        let t = Theme::plain();
        let called = std::cell::Cell::new(false);

        assert_eq!(
            highlight_shell("echo hi", &t, &|_| {
                called.set(true);
                true
            }),
            "echo hi"
        );
        assert!(!called.get());
    }

    #[test]
    fn unresolved_highlight_does_not_validate_commands() {
        let t = theme();
        let called = std::cell::Cell::new(false);

        let out = highlight_shell_without_resolution("definitelynotacmd --flag \"$HOME\"", &t);
        let resolved = highlight_shell("definitelynotacmd", &t, &|_| {
            called.set(true);
            false
        });

        assert!(out.contains(&t.paint(Role::Command, "definitelynotacmd")));
        assert!(out.contains(&t.paint(Role::Flag, "--flag")));
        assert!(out.contains(&t.paint(Role::Str, "\"$HOME\"")));
        assert!(resolved.contains(&t.paint(Role::CommandInvalid, "definitelynotacmd")));
        assert!(called.get());
    }

    #[test]
    fn mode_keywords_are_colored_and_keep_command_position_for_wrappers() {
        let t = theme();
        let out = highlight_shell("compact git status", &t, &known);
        assert!(out.contains(&t.paint(Role::ModeKeyword, "compact")));
        assert!(out.contains(&t.paint(Role::Command, "git")));
        assert!(!out.contains(&t.paint(Role::ModeKeyword, "echo")));
    }

    #[test]
    fn mode_tools_do_not_mark_file_arguments_invalid() {
        let t = theme();
        let out = highlight_shell("agview rust-toolchain.toml", &t, &known);
        assert!(out.contains(&t.paint(Role::ModeKeyword, "agview")), "{out}");
        assert!(
            !out.contains(&t.paint(Role::CommandInvalid, "rust-toolchain.toml")),
            "{out}"
        );
    }

    #[test]
    fn assignments_before_commands_keep_command_position() {
        let t = theme();
        let out = highlight_shell("FOO=$HOME BAR=baz echo \"$FOO\"", &t, &known);

        assert!(out.contains(&t.paint(Role::Var, "FOO=")), "{out}");
        assert!(out.contains(&t.paint(Role::Var, "$HOME")), "{out}");
        assert!(out.contains(&t.paint(Role::Var, "BAR=baz")), "{out}");
        assert!(out.contains(&t.paint(Role::Command, "echo")), "{out}");
    }

    #[test]
    fn hash_only_starts_comment_at_word_boundary() {
        let t = theme();
        let out = highlight_shell("echo foo#bar # real", &t, &known);

        assert!(out.contains("foo#bar"), "{out}");
        assert!(out.contains(&t.paint(Role::Comment, "# real")), "{out}");
    }

    #[test]
    fn no_panic_on_partial_input() {
        let t = theme();
        let _ = highlight_shell("echo \"unterminated", &t, &known);
        let _ = highlight_shell("ls ${", &t, &known);
        let _ = highlight_shell("git ", &t, &known);
    }
}
