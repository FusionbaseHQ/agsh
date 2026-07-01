use std::io::{self, IsTerminal, Write};

use agsh_exec::ShellState;

use crate::editor::read_line_raw;

const CONTINUATION_PROMPT: &str = "> ";

/// Read a complete command. On an interactive terminal this uses the raw-mode
/// editor (syntax highlighting, autosuggestions, history search, completion);
/// otherwise (piped stdin, `-c`, non-TTY) it falls back to the cooked reader so
/// behavior stays byte-identical to a plain line read.
pub fn read_line(prompt: &str, state: &ShellState) -> io::Result<Option<String>> {
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        read_line_raw(prompt, state)
    } else {
        read_line_cooked(prompt)
    }
}

/// Cooked-mode multiline reader (non-TTY / piped / `-c`).
fn read_line_cooked(prompt: &str) -> io::Result<Option<String>> {
    print!("{prompt}");
    io::stdout().flush()?;

    let mut buffer = String::new();
    loop {
        let mut line = String::new();
        let bytes = io::stdin().read_line(&mut line)?;
        if bytes == 0 {
            // EOF: return whatever was accumulated, or None on a clean EOF.
            let command = buffer.trim_end_matches(['\r', '\n']);
            return if command.is_empty() {
                Ok(None)
            } else {
                Ok(Some(command.to_string()))
            };
        }

        buffer.push_str(&strip_bracketed_paste(&line));

        let command = buffer.trim_end_matches(['\r', '\n']);
        if command.is_empty() || !agsh_core::is_incomplete(command) {
            return Ok(Some(command.to_string()));
        }

        // The command continues on the next line.
        print!("{CONTINUATION_PROMPT}");
        io::stdout().flush()?;
    }
}

/// Remove bracketed-paste markers (`ESC[200~` / `ESC[201~`) if a terminal sends
/// them, so pasted text is treated literally. (A full raw-mode line editor with
/// proper bracketed-paste buffering is future terminal-UX work.)
fn strip_bracketed_paste(line: &str) -> String {
    line.replace("\u{1b}[200~", "").replace("\u{1b}[201~", "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_bracketed_paste_markers() {
        assert_eq!(
            strip_bracketed_paste("\u{1b}[200~echo hi\u{1b}[201~"),
            "echo hi"
        );
        assert_eq!(strip_bracketed_paste("plain"), "plain");
    }
}
