//! Tolerant syntax highlighting for the line editor, styled via the theme.
//!
//! Unlike the real lexer/parser (which error on incomplete input), this scanner
//! never fails: it colors a partially-typed line by walking characters and
//! classifying spans. Command words are validated via a caller-supplied
//! predicate so unknown commands show in the error color (fish-style), without
//! the editor needing to resolve PATH itself.

use agsh_style::{Role, Theme};

/// Produce a styled rendering of `line`. `valid` decides whether a
/// command-position word is a known command.
pub fn highlight(line: &str, theme: &Theme, valid: &dyn Fn(&str) -> bool) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len() + 16);
    let mut i = 0;
    // A word is in "command position" at the start and after a separator.
    let mut command_position = true;

    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => {
                out.push(c);
                i += 1;
            }
            '#' => {
                let rest: String = chars[i..].iter().collect();
                out.push_str(&theme.paint(Role::Comment, &rest));
                break;
            }
            '\'' => {
                let (span, next) = read_quoted(&chars, i, '\'');
                out.push_str(&theme.paint(Role::Str, &span));
                i = next;
                command_position = false;
            }
            '"' => {
                let (span, next) = read_quoted(&chars, i, '"');
                out.push_str(&theme.paint(Role::Str, &span));
                i = next;
                command_position = false;
            }
            '$' => {
                let (span, next) = read_var(&chars, i);
                out.push_str(&theme.paint(Role::Var, &span));
                i = next;
                command_position = false;
            }
            '|' | '&' | ';' | '<' | '>' | '(' | ')' => {
                let (span, next) = read_operator(&chars, i);
                out.push_str(&theme.paint(Role::Operator, &span));
                i = next;
                command_position = true;
            }
            _ => {
                let (word, next) = read_word(&chars, i);
                if command_position && is_mode_keyword(&word) {
                    // agsh mode vocabulary: paint orange. The output-mode WRAPPERS
                    // (`compact git …`) wrap a following command, so keep command
                    // position. `agview`/`mode`/`mode:*` take FILE/value args, not a
                    // command — keeping command position would miscolor their file
                    // argument as an invalid command (red).
                    out.push_str(&theme.paint(Role::ModeKeyword, &word));
                    i = next;
                    command_position = is_output_mode_wrapper(&word);
                    continue;
                }
                if command_position {
                    let role = if valid(&word) {
                        Role::Command
                    } else {
                        Role::CommandInvalid
                    };
                    out.push_str(&theme.paint(role, &word));
                } else if word.contains('/') || word.starts_with('~') {
                    // Underline paths that actually exist on disk.
                    let style = theme.style(Role::Path);
                    let style = if std::path::Path::new(&word).exists() {
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
            }
        }
    }
    out
}

/// The output-mode WRAPPERS that precede a command (`compact git …`, `raw npm …`).
/// These keep command position so the wrapped command highlights; the rest of the
/// mode vocabulary (`agview`, `mode`, `mode:*`) takes file/value args instead.
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
/// builtin family that sets session defaults — all highlighted alike.
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
    use agsh_style::{ColorLevel, Icons, Palette, Theme};

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
        let valid = highlight("echo hi", &t, &known);
        let invalid = highlight("nope hi", &t, &known);
        assert!(valid.contains(&t.paint(Role::Command, "echo")));
        assert!(invalid.contains(&t.paint(Role::CommandInvalid, "nope")));
    }

    #[test]
    fn colors_strings_vars_operators() {
        let t = theme();
        let out = highlight("echo 'a' $HOME | ls", &t, &known);
        assert!(out.contains(&t.paint(Role::Str, "'a'")));
        assert!(out.contains(&t.paint(Role::Var, "$HOME")));
        assert!(out.contains(&t.paint(Role::Operator, "|")));
    }

    #[test]
    fn plain_theme_is_uncolored_but_intact() {
        let t = Theme::plain();
        assert_eq!(highlight("echo hi", &t, &known), "echo hi");
    }

    #[test]
    fn mode_keywords_are_orange_and_keep_command_position() {
        let t = theme();
        let out = highlight("compact git status", &t, &known);
        // `compact` paints with the (orange) ModeKeyword role...
        assert!(out.contains(&t.paint(Role::ModeKeyword, "compact")));
        // ...and the wrapped command still highlights as a command.
        assert!(out.contains(&t.paint(Role::Command, "git")));
        // Several mode words are recognized.
        for kw in ["semantic", "raw", "clean", "agview", "silent", "mode"] {
            let out = highlight(&format!("{kw} echo hi"), &t, &known);
            assert!(
                out.contains(&t.paint(Role::ModeKeyword, kw)),
                "{kw} not orange"
            );
        }
        // A non-mode word in command position is NOT painted as a keyword.
        let out = highlight("echo hi", &t, &known);
        assert!(!out.contains(&t.paint(Role::ModeKeyword, "echo")));
    }

    #[test]
    fn agview_file_argument_is_not_colored_as_invalid_command() {
        let t = theme();
        let out = highlight("agview rust-toolchain.toml", &t, &known);
        // agview is still orange, but its FILE argument must not be error-colored.
        assert!(out.contains(&t.paint(Role::ModeKeyword, "agview")), "{out}");
        assert!(
            !out.contains(&t.paint(Role::CommandInvalid, "rust-toolchain.toml")),
            "agview's existing file arg was colored as an invalid command:\n{out}"
        );
        // Sanity: a genuine unknown command in command position IS colored invalid,
        // so the assertion above is meaningful.
        let cmd = highlight("definitelynotacmd", &t, &known);
        assert!(cmd.contains(&t.paint(Role::CommandInvalid, "definitelynotacmd")));
        // An output-mode wrapper still keeps command position (regression guard).
        let wrapped = highlight("compact git status", &t, &known);
        assert!(wrapped.contains(&t.paint(Role::Command, "git")));
    }

    #[test]
    fn no_panic_on_partial() {
        let t = theme();
        let _ = highlight("echo \"unterminated", &t, &known);
        let _ = highlight("ls ${", &t, &known);
        let _ = highlight("git ", &t, &known);
    }
}
