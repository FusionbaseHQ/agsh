//! The interactive raw-mode line editor: ties together key decoding, the line
//! buffer, syntax highlighting, history navigation, reverse search, inline
//! autosuggestions, and basic Tab completion.
//!
//! Multiline input is handled a physical line at a time: when Enter is pressed
//! on a syntactically incomplete command (per `agsh_core::is_incomplete`), the
//! line is committed and editing continues on a continuation prompt — matching
//! the cooked reader's behavior while adding rich editing on the current line.

use std::io::{self, Read, Write};

use agsh_core::is_incomplete;
use agsh_exec::ShellState;
use agsh_style::Role;

use crate::buffer::LineBuffer;
use crate::complete::{complete, filter_rank, highlight_positions, Candidate, CandidateKind};
use crate::highlight::highlight;
use crate::key::{decode, Decoded, Key, PASTE_END, PASTE_START};
use crate::raw::{term_size, RawGuard};
use crate::render::{render_block, visible_width};

/// Maximum visible rows in the completion dropdown.
const MENU_ROWS: usize = 8;

/// An open completion dropdown: candidates for the context plus the selected row.
struct CompletionMenu {
    /// Char index where the completed word begins (replacement start).
    start: usize,
    candidates: Vec<Candidate>,
    selected: usize,
}

/// Read one complete command using the raw-mode editor. Restores the terminal
/// on return (including on error/panic via the `RawGuard`'s `Drop`).
pub fn read_line_raw(prompt: &str, state: &ShellState) -> io::Result<Option<String>> {
    let _raw = RawGuard::new()?;
    let mut editor = Editor::new(prompt.to_string(), state);
    let result = editor.run();
    // Leave the cursor on a fresh line regardless of how we exited.
    let mut out = io::stdout();
    let _ = out.flush();
    result
}

struct Editor<'a> {
    state: &'a ShellState,
    prompt: String,
    buffer: LineBuffer,
    accumulated: String,
    /// Total rows the last render occupied (input + menu) and the cursor's row
    /// within that block — used for scroll-safe in-place repaints.
    rendered_rows: usize,
    cursor_rpos: usize,
    cols: usize,
    rows: usize,
    current_ghost: Option<String>,
    history: Option<Vec<String>>,
    hist_pos: Option<usize>,
    stash: String,
    reader: KeyReader,
    completion: Option<CompletionMenu>,
}

impl<'a> Editor<'a> {
    fn new(prompt: String, state: &'a ShellState) -> Self {
        let (rows, cols) = term_size();
        Self {
            state,
            prompt,
            buffer: LineBuffer::new(),
            accumulated: String::new(),
            rendered_rows: 0,
            cursor_rpos: 0,
            cols: cols as usize,
            rows: rows as usize,
            current_ghost: None,
            history: None,
            hist_pos: None,
            stash: String::new(),
            reader: KeyReader::new(),
            completion: None,
        }
    }

    fn run(&mut self) -> io::Result<Option<String>> {
        self.render()?;
        loop {
            let key = self.reader.next_key()?;
            // While the completion dropdown is open, it gets first crack at keys.
            if self.completion.is_some() && self.handle_completion_key(&key)? {
                continue;
            }
            match key {
                Key::Enter => {
                    if let Some(line) = self.on_enter()? {
                        return Ok(Some(line));
                    }
                }
                Key::Interrupt => {
                    self.move_to_end()?;
                    write_all(b"^C\r\n")?;
                    return Ok(Some(String::new()));
                }
                Key::Eof => {
                    if self.buffer.is_empty() && self.accumulated.is_empty() {
                        self.move_to_end()?;
                        write_all(b"\r\n")?;
                        return Ok(None);
                    }
                    self.buffer.delete();
                    self.render()?;
                }
                Key::Char(c) => {
                    self.buffer.insert_char(c);
                    self.render()?;
                }
                Key::Paste(text) => {
                    // Insert pasted text literally (newlines become spaces are
                    // avoided: keep them; multiline accumulation handles them).
                    self.buffer.insert_str(&text.replace('\r', ""));
                    self.render()?;
                }
                Key::Backspace => {
                    self.buffer.backspace();
                    self.render()?;
                }
                Key::Delete => {
                    self.buffer.delete();
                    self.render()?;
                }
                Key::Left => {
                    self.buffer.left();
                    self.render()?;
                }
                Key::Right => {
                    // At end of line, accept the autosuggestion; else move right.
                    if self.buffer.at_end() {
                        if let Some(ghost) = self.current_ghost.take() {
                            self.buffer.insert_str(&ghost);
                        }
                    } else {
                        self.buffer.right();
                    }
                    self.render()?;
                }
                Key::Up => {
                    self.history_prev();
                    self.render()?;
                }
                Key::Down => {
                    self.history_next();
                    self.render()?;
                }
                Key::Home | Key::LineStart => {
                    self.buffer.home();
                    self.render()?;
                }
                Key::End | Key::LineEnd => {
                    self.buffer.end();
                    self.render()?;
                }
                Key::WordLeft => {
                    self.buffer.word_left();
                    self.render()?;
                }
                Key::WordRight => {
                    self.buffer.word_right();
                    self.render()?;
                }
                Key::KillToEnd => {
                    self.buffer.kill_to_end();
                    self.render()?;
                }
                Key::KillToStart => {
                    self.buffer.kill_to_start();
                    self.render()?;
                }
                Key::KillWord => {
                    self.buffer.kill_word();
                    self.render()?;
                }
                Key::DeleteWord => {
                    self.buffer.delete_word();
                    self.render()?;
                }
                Key::Clear => {
                    write_all(b"\x1b[2J\x1b[H")?;
                    self.rendered_rows = 0;
                    self.cursor_rpos = 0;
                    self.render()?;
                }
                Key::ReverseSearch => {
                    self.reverse_search()?;
                }
                Key::Tab => {
                    self.open_completion();
                    self.render()?;
                }
                Key::BackTab | Key::Escape | Key::Unknown => {}
            }
        }
    }

    fn on_enter(&mut self) -> io::Result<Option<String>> {
        let line = self.buffer.text();
        self.move_to_end()?;
        write_all(b"\r\n")?;
        self.accumulated.push_str(&line);
        if is_incomplete(&self.accumulated) {
            self.accumulated.push('\n');
            self.buffer.clear();
            self.prompt = "> ".to_string();
            self.hist_pos = None;
            self.render()?;
            Ok(None)
        } else {
            Ok(Some(std::mem::take(&mut self.accumulated)))
        }
    }

    /// Build and emit the refresh sequence for the current state.
    fn render(&mut self) -> io::Result<()> {
        let buffer_text = self.buffer.text();
        // Inline autosuggestion: only at end of a non-empty line, and not while
        // the completion dropdown is showing. The ghost is the remainder of a
        // matching history command, truncated to a single line — multiline
        // history entries (e.g. heredocs) must not inject newlines into the
        // single-line render.
        let prompt_w = visible_width(&self.prompt);
        let buf_w = buffer_text.chars().count();
        let ghost_full =
            if self.completion.is_none() && self.buffer.at_end() && !buffer_text.is_empty() {
                self.state
                    .history_suggest(&buffer_text)
                    .and_then(|full| single_line_ghost(&full, buffer_text.len()))
            } else {
                None
            };
        // Display-clip the ghost to the space left on the current visual line:
        // a pathological history entry (thousands of chars) must hint, not
        // flood the screen. Acceptance (→) still inserts the FULL suggestion —
        // but only while a hint is actually visible.
        let (ghost_display, ghost_accept) = match ghost_full {
            Some(full) => match clip_ghost(&full, prompt_w + buf_w, self.cols) {
                Some(display) => (Some(display), Some(full)),
                None => (None, None),
            },
            None => (None, None),
        };

        let theme = self.state.theme();
        let highlighted = highlight(&buffer_text, &theme, &|cmd| self.state.is_command_name(cmd));
        let mut content = String::with_capacity(self.prompt.len() + highlighted.len() + 16);
        content.push_str(&self.prompt);
        content.push_str(&highlighted);
        if let Some(g) = &ghost_display {
            content.push_str(&theme.paint(Role::Muted, g));
        }

        let ghost_w = ghost_display
            .as_ref()
            .map(|g| g.chars().count())
            .unwrap_or(0);
        let total_visible = prompt_w + buf_w + ghost_w;
        let cursor_visible = prompt_w + self.buffer.cursor();

        let menu = if self.completion.is_some() {
            self.menu_lines()
        } else {
            Vec::new()
        };
        let (seq, rows, rpos) = render_block(
            &content,
            total_visible,
            cursor_visible,
            &menu,
            self.cols,
            self.rendered_rows,
            self.cursor_rpos,
        );
        self.rendered_rows = rows;
        self.cursor_rpos = rpos;
        self.current_ghost = ghost_accept;
        write_all(seq.as_bytes())
    }

    /// Move the terminal cursor below the whole rendered block (input + menu) so
    /// following output starts on a clean line. Resets the render state.
    fn move_to_end(&mut self) -> io::Result<()> {
        let down = self
            .rendered_rows
            .saturating_sub(1)
            .saturating_sub(self.cursor_rpos);
        let mut seq = String::new();
        if down > 0 {
            seq.push_str(&format!("\x1b[{down}B"));
        }
        seq.push('\r');
        self.rendered_rows = 0;
        self.cursor_rpos = 0;
        write_all(seq.as_bytes())
    }

    fn ensure_history(&mut self) {
        if self.history.is_none() {
            self.history = Some(self.state.history_recent(5000));
        }
    }

    fn history_prev(&mut self) {
        self.ensure_history();
        let hist = self.history.as_ref().expect("history loaded");
        if hist.is_empty() {
            return;
        }
        let next = match self.hist_pos {
            None => {
                self.stash = self.buffer.text();
                hist.len() - 1
            }
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.hist_pos = Some(next);
        let text = hist[next].clone();
        self.buffer.set(&text);
    }

    fn history_next(&mut self) {
        let Some(pos) = self.hist_pos else { return };
        let hist = self.history.as_ref().expect("history loaded");
        if pos + 1 >= hist.len() {
            // Past the newest entry: restore the in-progress line.
            self.hist_pos = None;
            let stash = self.stash.clone();
            self.buffer.set(&stash);
        } else {
            self.hist_pos = Some(pos + 1);
            let text = hist[pos + 1].clone();
            self.buffer.set(&text);
        }
    }

    /// Incremental reverse history search (Ctrl-R). Typing refines the query;
    /// Ctrl-R cycles to the next match; Enter accepts; Esc/Ctrl-C cancels.
    fn reverse_search(&mut self) -> io::Result<()> {
        let mut query = String::new();
        let mut match_index = 0usize;
        loop {
            let matches = if query.is_empty() {
                Vec::new()
            } else {
                self.state.history_search(&query, 50)
            };
            let current = matches.get(match_index).map(|e| e.command.clone());
            self.render_search(&query, current.as_deref())?;

            match self.reader.next_key()? {
                Key::Enter => {
                    if let Some(cmd) = current {
                        self.buffer.set(&cmd);
                    }
                    break;
                }
                Key::ReverseSearch => {
                    if match_index + 1 < matches.len() {
                        match_index += 1;
                    }
                }
                Key::Backspace => {
                    query.pop();
                    match_index = 0;
                }
                Key::Char(c) => {
                    query.push(c);
                    match_index = 0;
                }
                Key::Interrupt | Key::Escape => break,
                _ => break,
            }
        }
        self.render()
    }

    fn render_search(&mut self, query: &str, current: Option<&str>) -> io::Result<()> {
        let shown = current.unwrap_or("");
        let line = format!("(reverse-i-search)`{query}': {shown}");
        let total = line.chars().count();
        let cursor = "(reverse-i-search)`".chars().count() + query.chars().count();
        let (seq, rows, rpos) = render_block(
            &line,
            total,
            cursor,
            &[],
            self.cols,
            self.rendered_rows,
            self.cursor_rpos,
        );
        self.rendered_rows = rows;
        self.cursor_rpos = rpos;
        write_all(seq.as_bytes())
    }

    /// Open the completion dropdown for the word under the cursor. With one
    /// candidate it completes inline; with several it inserts the longest common
    /// prefix and shows the menu.
    fn open_completion(&mut self) {
        let line = self.buffer.text();
        let comp = complete(&line, self.buffer.cursor(), self.state);
        if comp.candidates.is_empty() {
            return;
        }
        let word = self.buffer.text_range(comp.start, self.buffer.cursor());
        let filtered = filter_rank(&comp.candidates, &word);
        match filtered.as_slice() {
            [] => {}
            [only] => {
                let cand = comp.candidates[*only].clone();
                self.apply_candidate(comp.start, &cand);
            }
            many => {
                let values: Vec<String> = many
                    .iter()
                    .map(|i| comp.candidates[*i].value.clone())
                    .collect();
                let lcp = longest_common_prefix(&values);
                if lcp.chars().count() > word.chars().count() {
                    self.buffer
                        .replace_range(comp.start, self.buffer.cursor(), &lcp);
                }
                self.completion = Some(CompletionMenu {
                    start: comp.start,
                    candidates: comp.candidates,
                    selected: 0,
                });
            }
        }
    }

    /// Handle a key while the dropdown is open. Returns true if consumed (menu
    /// stays in control); false if the menu closed and the key should be
    /// processed normally.
    fn handle_completion_key(&mut self, key: &Key) -> io::Result<bool> {
        let visible_len = self.completion_visible().len();
        match key {
            Key::Tab | Key::Down => {
                if let Some(menu) = &mut self.completion {
                    if visible_len > 0 {
                        menu.selected = (menu.selected + 1) % visible_len;
                    }
                }
                self.render()?;
                Ok(true)
            }
            Key::BackTab | Key::Up => {
                if let Some(menu) = &mut self.completion {
                    if visible_len > 0 {
                        menu.selected = (menu.selected + visible_len - 1) % visible_len;
                    }
                }
                self.render()?;
                Ok(true)
            }
            Key::Enter | Key::Right => {
                self.accept_completion();
                self.render()?;
                Ok(true)
            }
            Key::Escape | Key::Interrupt => {
                self.completion = None;
                self.render()?;
                Ok(true)
            }
            Key::Char(c) => {
                self.buffer.insert_char(*c);
                if c.is_whitespace() {
                    self.completion = None;
                } else {
                    self.refilter_completion();
                }
                self.render()?;
                Ok(true)
            }
            Key::Backspace => {
                let at_start = self
                    .completion
                    .as_ref()
                    .map(|m| self.buffer.cursor() <= m.start)
                    .unwrap_or(true);
                self.buffer.backspace();
                if at_start {
                    self.completion = None;
                } else {
                    self.refilter_completion();
                }
                self.render()?;
                Ok(true)
            }
            _ => {
                // Any other key closes the menu and is handled normally.
                self.completion = None;
                Ok(false)
            }
        }
    }

    /// Candidate indices currently visible (filtered by the typed word).
    fn completion_visible(&self) -> Vec<usize> {
        match &self.completion {
            Some(menu) => {
                let word = self.buffer.text_range(menu.start, self.buffer.cursor());
                filter_rank(&menu.candidates, &word)
            }
            None => Vec::new(),
        }
    }

    fn refilter_completion(&mut self) {
        let len = self.completion_visible().len();
        if let Some(menu) = &mut self.completion {
            if len == 0 {
                menu.selected = 0;
            } else if menu.selected >= len {
                menu.selected = len - 1;
            }
        }
    }

    fn accept_completion(&mut self) {
        let visible = self.completion_visible();
        let (start, cand) = {
            let Some(menu) = &self.completion else { return };
            let Some(&idx) = visible.get(menu.selected) else {
                self.completion = None;
                return;
            };
            (menu.start, menu.candidates[idx].clone())
        };
        self.apply_candidate(start, &cand);
        if cand.kind == CandidateKind::Dir {
            // Descend: reopen completion for the new path.
            let line = self.buffer.text();
            let next = complete(&line, self.buffer.cursor(), self.state);
            self.completion = if next.candidates.is_empty() {
                None
            } else {
                Some(CompletionMenu {
                    start: next.start,
                    candidates: next.candidates,
                    selected: 0,
                })
            };
        } else {
            self.completion = None;
        }
    }

    fn apply_candidate(&mut self, start: usize, cand: &Candidate) {
        self.buffer
            .replace_range(start, self.buffer.cursor(), &cand.value);
        if cand.append_space {
            self.buffer.insert_char(' ');
        }
    }

    /// Build the dropdown rows: an accent bar marks the selected row; each row
    /// shows a type icon (when enabled), the value with matched characters
    /// highlighted and colored by kind, and a right-hand type tag.
    fn menu_lines(&self) -> Vec<String> {
        let theme = self.state.theme();
        let Some(menu) = &self.completion else {
            return Vec::new();
        };
        let visible = self.completion_visible();
        if visible.is_empty() {
            return vec![theme.paint(Role::Muted, "  (no matches)")];
        }
        let word = self.buffer.text_range(menu.start, self.buffer.cursor());
        let rows = MENU_ROWS.min(self.rows.saturating_sub(2).max(1));
        let total = visible.len();
        let start = if menu.selected >= rows {
            menu.selected - rows + 1
        } else {
            0
        };
        let end = (start + rows).min(total);
        let max_val = visible[start..end]
            .iter()
            .map(|i| menu.candidates[*i].value.chars().count())
            .max()
            .unwrap_or(0)
            .min(self.cols.saturating_sub(16).max(1));

        let mut lines = Vec::new();
        for (row, &i) in visible[start..end].iter().enumerate() {
            let cand = &menu.candidates[i];
            let selected = start + row == menu.selected;

            // Selection marker.
            let bar = if selected {
                theme.paint(Role::Accent, "\u{258c}") // ▌
            } else {
                " ".to_string()
            };
            // Type icon (empty unless AGSH_ICONS).
            let icon = self.candidate_icon(cand);
            let icon_cell = if icon.is_empty() {
                String::new()
            } else {
                format!("{icon} ")
            };
            // Value with matched chars highlighted, colored by kind, padded.
            let value = paint_value(&theme, &cand.value, cand.kind.role(), &word, max_val);
            let tag = theme.paint(Role::Tag, cand.kind.tag());
            // Optional one-line description (truncated to fit the terminal).
            let desc = match &cand.description {
                Some(d) if !d.is_empty() => {
                    let budget = self.cols.saturating_sub(max_val + 18);
                    let shown: String = d.chars().take(budget).collect();
                    if shown.is_empty() {
                        String::new()
                    } else {
                        format!("  {}", theme.paint(Role::Muted, &shown))
                    }
                }
                _ => String::new(),
            };
            lines.push(format!("{bar} {icon_cell}{value}  {tag}{desc}"));
        }
        if total > rows {
            lines.push(theme.paint(
                Role::Muted,
                &format!("  \u{2026} {}/{}", menu.selected + 1, total),
            ));
        }
        lines
    }

    /// The Nerd-Font icon for a candidate (empty when icons are disabled).
    fn candidate_icon(&self, cand: &Candidate) -> &'static str {
        let icons = self.state.theme().icons;
        match cand.kind {
            CandidateKind::Dir => icons.dir(),
            CandidateKind::File => icons.file(&cand.value),
            CandidateKind::Branch => icons.git_branch(),
            CandidateKind::History => icons.history(),
            _ => "",
        }
    }
}

/// Paint `value` in `base` role, highlighting characters that match `word`, and
/// pad to `width` visible columns. Consecutive same-style chars are grouped to
/// keep escape output small.
fn paint_value(
    theme: &agsh_style::Theme,
    value: &str,
    base: Role,
    word: &str,
    width: usize,
) -> String {
    let matches: std::collections::HashSet<usize> =
        highlight_positions(word, value).into_iter().collect();
    // Defensive: never let a control char (newline/tab/…) reach the menu row —
    // it would break the dropdown layout. Mapping char-for-char preserves the
    // highlight indices computed above.
    let chars: Vec<char> = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(width)
        .collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let is_match = matches.contains(&i);
        let mut j = i + 1;
        while j < chars.len() && matches.contains(&j) == is_match {
            j += 1;
        }
        let segment: String = chars[i..j].iter().collect();
        let role = if is_match { Role::Match } else { base };
        out.push_str(&theme.paint(role, &segment));
        i = j;
    }
    // Pad to the column width based on visible characters.
    let pad = width.saturating_sub(chars.len());
    out.push_str(&" ".repeat(pad));
    out
}

fn longest_common_prefix(items: &[String]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let first = &items[0];
    let mut len = first.chars().count();
    for item in &items[1..] {
        len = len.min(common_prefix_len(first, item));
    }
    first.chars().take(len).collect()
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

/// Reads keys from stdin, handling partial escape sequences and bracketed paste.
struct KeyReader {
    buf: Vec<u8>,
}

impl KeyReader {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn next_key(&mut self) -> io::Result<Key> {
        loop {
            if let Some(key) = self.try_extract() {
                return Ok(key);
            }
            // A lone ESC byte is ambiguous (the Escape key vs. the start of an
            // escape sequence). Wait briefly; if nothing follows, it's Escape.
            if self.buf == [0x1b] && !wait_readable_ms(40) {
                self.buf.clear();
                return Ok(Key::Escape);
            }
            let mut tmp = [0u8; 1024];
            let n = io::stdin().read(&mut tmp)?;
            if n == 0 {
                return Ok(Key::Eof);
            }
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }

    fn try_extract(&mut self) -> Option<Key> {
        if self.buf.is_empty() {
            return None;
        }
        // Bracketed paste: gather everything up to the end marker.
        if self.buf.starts_with(PASTE_START) {
            if let Some(end) = find_sub(&self.buf, PASTE_END) {
                let content =
                    String::from_utf8_lossy(&self.buf[PASTE_START.len()..end]).into_owned();
                self.buf.drain(0..end + PASTE_END.len());
                return Some(Key::Paste(content));
            }
            return None; // wait for the end marker
        }
        // Don't misdecode a partial paste-start prefix.
        if is_strict_prefix(&self.buf, PASTE_START) {
            return None;
        }
        match decode(&self.buf) {
            Decoded::Incomplete => None,
            Decoded::Key(key, used) => {
                self.buf.drain(0..used);
                Some(key)
            }
        }
    }
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn is_strict_prefix(buf: &[u8], full: &[u8]) -> bool {
    buf.len() < full.len() && full.starts_with(buf)
}

fn write_all(bytes: &[u8]) -> io::Result<()> {
    let mut out = io::stdout();
    out.write_all(bytes)?;
    out.flush()
}

/// The inline-suggestion ghost: the remainder of a matching history command
/// after `prefix_len` bytes, truncated to a single line. Multiline history
/// entries (heredocs, multi-line commands) must not inject newlines into the
/// single-line editor render. Returns None when the remainder is empty.
fn single_line_ghost(full: &str, prefix_len: usize) -> Option<String> {
    let rest = full.get(prefix_len..)?;
    let line = rest.split(['\n', '\r']).next().unwrap_or("");
    (!line.is_empty()).then(|| line.to_string())
}

/// Clip a ghost for display so it never wraps: it may only use what remains of
/// the visual line the cursor is on (after `used` columns of prompt + typed
/// text, which may themselves wrap), with a trailing `…` when cut. `None` when
/// there's no room for even a hint. Display only — the caller keeps the full
/// text for acceptance.
fn clip_ghost(ghost: &str, used: usize, cols: usize) -> Option<String> {
    let cols = cols.max(2);
    // Columns left on the cursor's line, keeping the last cell free so the
    // ghost can't push the cursor onto the next row.
    let avail = (cols - used % cols).saturating_sub(1);
    let width = ghost.chars().count();
    if width <= avail {
        return Some(ghost.to_string());
    }
    if avail < 2 {
        return None;
    }
    let mut clipped: String = ghost.chars().take(avail - 1).collect();
    clipped.push('…');
    Some(clipped)
}

/// Whether stdin has data ready within `ms` milliseconds. Used to disambiguate a
/// lone Escape from an escape sequence. On any poll error, assume readable so we
/// fall through to a blocking read rather than dropping input.
fn wait_readable_ms(ms: i64) -> bool {
    use rustix::event::{poll, PollFd, PollFlags, Timespec};
    let stdin = io::stdin();
    let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
    let timeout = Timespec {
        tv_sec: 0,
        tv_nsec: ms * 1_000_000,
    };
    match poll(&mut fds, Some(&timeout)) {
        Ok(n) => n > 0,
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lcp_of_candidates() {
        let items = vec!["cargo".to_string(), "case".to_string(), "cat".to_string()];
        assert_eq!(longest_common_prefix(&items), "ca");
    }

    #[test]
    fn ghost_display_never_wraps() {
        // Fits: returned whole.
        assert_eq!(
            clip_ghost("ath '2+2'", 10, 80),
            Some("ath '2+2'".to_string())
        );
        // A monster suggestion (the `agmath '((((…` history case) is clipped
        // to the space left on the cursor's line, ellipsized — never a flood.
        let monster = "(".repeat(3000);
        let clipped = clip_ghost(&monster, 10, 80).expect("hint shown");
        assert!(clipped.chars().count() <= 69, "{}", clipped.chars().count());
        assert!(clipped.ends_with('…'));
        // Cursor mid-wrapped-line: only the CURRENT line's remainder is used.
        let clipped = clip_ghost(&monster, 165, 80).expect("hint shown"); // col 5 of row 3
        assert!(clipped.chars().count() <= 74);
        // Almost no room: no hint at all rather than a useless sliver.
        assert_eq!(clip_ghost(&monster, 78, 80), None);
    }

    #[test]
    fn ghost_is_single_line() {
        // A multiline history entry (heredoc) must not produce a newline-bearing
        // ghost — that staircased the render.
        let full = "cat <<EOF\nheredoc-body\nEOF";
        assert_eq!(single_line_ghost(full, "cat <<EOF".len()), None);
        // A normal single-line continuation works.
        assert_eq!(
            single_line_ghost("git checkout main", "git ch".len()),
            Some("eckout main".to_string())
        );
        // A match whose remainder's first line is non-empty is truncated there.
        assert_eq!(
            single_line_ghost("run\nthen more", "ru".len()),
            Some("n".to_string())
        );
    }

    #[test]
    fn find_subsequence() {
        assert_eq!(find_sub(b"abXYc", b"XY"), Some(2));
        assert_eq!(find_sub(b"abc", b"ZZ"), None);
    }

    #[test]
    fn strict_prefix_detects_partial_paste() {
        assert!(is_strict_prefix(b"\x1b[2", PASTE_START));
        assert!(!is_strict_prefix(PASTE_START, PASTE_START));
    }
}
