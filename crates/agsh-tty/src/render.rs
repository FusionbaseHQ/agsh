//! Build the terminal byte sequence that repaints the edit line.
//!
//! Uses a linenoise-style multi-row refresh: move to the top of the previously
//! rendered region, clear it, repaint, then place the cursor. Display width is
//! approximated as one column per `char` (wide-character support is future
//! work); the positioning math is pure and unit-tested.

/// Row of the last printed cell, and the (row, col) of the cursor, for content
/// of `total_visible` cells with the cursor at `cursor_visible` cells, wrapped
/// at `cols` columns.
pub fn position(total_visible: usize, cursor_visible: usize, cols: usize) -> (usize, usize, usize) {
    let cols = cols.max(1);
    let end_row = total_visible.saturating_sub(1) / cols;
    let cursor_row = cursor_visible / cols;
    let cursor_col = cursor_visible % cols;
    (end_row, cursor_row, cursor_col)
}

/// Produce the escape sequence to repaint the line and return the cursor's new
/// row (relative to the first prompt row) for the next refresh.
///
/// - `content`: the fully colored string to print (prompt + buffer + ghost).
/// - `total_visible`: visible cell count of `content`.
/// - `cursor_visible`: visible cells before the cursor (prompt + buffer prefix).
/// - `cols`: terminal width.
/// - `prev_cursor_row`: the cursor row left by the previous refresh.
pub fn refresh_seq(
    content: &str,
    total_visible: usize,
    cursor_visible: usize,
    cols: usize,
    prev_cursor_row: usize,
) -> (String, usize) {
    let (end_row, cursor_row, cursor_col) = position(total_visible, cursor_visible, cols);
    let mut out = String::new();

    // 1. Move to the top of the previously rendered region.
    if prev_cursor_row > 0 {
        out.push_str(&format!("\x1b[{prev_cursor_row}A"));
    }
    // 2. Column 0, clear to end of screen.
    out.push_str("\r\x1b[0J");
    // 3. Repaint.
    out.push_str(content);
    // 4. Move from the end of the content back to the cursor row/col.
    let up = end_row.saturating_sub(cursor_row);
    if up > 0 {
        out.push_str(&format!("\x1b[{up}A"));
    }
    out.push('\r');
    if cursor_col > 0 {
        out.push_str(&format!("\x1b[{cursor_col}C"));
    }
    (out, cursor_row)
}

/// Scroll-safe repaint of the input line plus an optional dropdown below it.
///
/// Clears relative to the cursor's row *within the previously rendered block*
/// (move to the block's bottom, erase each line upward), then repaints and
/// repositions from the new block's bottom. Because every move is relative to
/// the cursor — which scrolls together with the content — this stays correct
/// even when printing the menu scrolls the terminal (the bug that made stacked
/// prompts appear near the screen bottom).
///
/// Returns the emitted bytes, the number of rows the new block occupies, and the
/// cursor's row within it (both fed back on the next call).
#[allow(clippy::too_many_arguments)]
pub fn render_block(
    content: &str,
    total_visible: usize,
    cursor_visible: usize,
    menu_lines: &[String],
    cols: usize,
    old_rows: usize,
    old_cursor_rpos: usize,
) -> (String, usize, usize) {
    let cols = cols.max(1);
    let input_rows = total_visible.saturating_sub(1) / cols + 1;
    let cursor_row = cursor_visible / cols;
    let cursor_col = cursor_visible % cols;
    let new_rows = input_rows + menu_lines.len();

    let mut out = String::new();
    // 1. From the old cursor position, drop to the bottom of the old block, then
    //    erase each row from bottom to top, ending at the block's top, column 0.
    if old_rows > 0 {
        let down = old_rows.saturating_sub(1).saturating_sub(old_cursor_rpos);
        if down > 0 {
            out.push_str(&format!("\x1b[{down}B"));
        }
        out.push_str("\r\x1b[2K");
        for _ in 0..old_rows.saturating_sub(1) {
            out.push_str("\x1b[1A\r\x1b[2K");
        }
    } else {
        out.push_str("\r\x1b[2K");
    }
    // 2. Repaint the input line and the menu rows below it.
    out.push_str(content);
    for line in menu_lines {
        out.push_str("\r\n");
        out.push_str(line);
    }
    // 3. From the bottom of the new block, move up to the input cursor row.
    let bottom = new_rows.saturating_sub(1);
    let up = bottom.saturating_sub(cursor_row);
    if up > 0 {
        out.push_str(&format!("\x1b[{up}A"));
    }
    out.push('\r');
    if cursor_col > 0 {
        out.push_str(&format!("\x1b[{cursor_col}C"));
    }
    (out, new_rows, cursor_row)
}

/// Like [`refresh_seq`] but renders `menu_lines` below the input line (a
/// completion dropdown), then returns the cursor to the input position. The next
/// refresh clears the menu because it moves up to the input cursor row and emits
/// erase-to-end-of-screen.
pub fn refresh_with_menu(
    input_content: &str,
    input_total_visible: usize,
    cursor_visible: usize,
    menu_lines: &[String],
    cols: usize,
    prev_cursor_row: usize,
) -> (String, usize) {
    if menu_lines.is_empty() {
        return refresh_seq(
            input_content,
            input_total_visible,
            cursor_visible,
            cols,
            prev_cursor_row,
        );
    }
    let cols = cols.max(1);
    let input_last_row = input_total_visible.saturating_sub(1) / cols;
    let cursor_row = cursor_visible / cols;
    let cursor_col = cursor_visible % cols;

    let mut out = String::new();
    if prev_cursor_row > 0 {
        out.push_str(&format!("\x1b[{prev_cursor_row}A"));
    }
    out.push_str("\r\x1b[0J");
    out.push_str(input_content);
    for line in menu_lines {
        out.push_str("\r\n\x1b[K");
        out.push_str(line);
    }
    // Cursor is now at the last menu row; return to the input cursor.
    let rows_below = input_last_row + menu_lines.len();
    let up = rows_below.saturating_sub(cursor_row);
    if up > 0 {
        out.push_str(&format!("\x1b[{up}A"));
    }
    out.push('\r');
    if cursor_col > 0 {
        out.push_str(&format!("\x1b[{cursor_col}C"));
    }
    (out, cursor_row)
}

/// Visible width of a string, ignoring ANSI escape sequences (CSI/SGR), counting
/// one column per remaining `char`.
pub fn visible_width(s: &str) -> usize {
    let mut width = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip an escape sequence: ESC [ ... final-byte.
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() || n == '~' {
                        break;
                    }
                }
            }
            continue;
        }
        width += 1;
    }
    width
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_single_row() {
        // prompt(2) + "echo"(4) = 6 cells, cursor at end, 80 cols.
        let (end, row, col) = position(6, 6, 80);
        assert_eq!((end, row, col), (0, 0, 6));
    }

    #[test]
    fn position_wraps() {
        // 25 cells, cols=10 => rows 0,1,2; last cell on row 2.
        let (end, row, col) = position(25, 13, 10);
        assert_eq!(end, 2);
        assert_eq!(row, 1); // cursor at cell 13 -> row 1
        assert_eq!(col, 3);
    }

    #[test]
    fn refresh_moves_up_from_prev_row() {
        let (seq, new_row) = refresh_seq("PROMPT", 6, 6, 80, 2);
        assert!(seq.starts_with("\x1b[2A")); // move up 2 from previous cursor row
        assert!(seq.contains("\x1b[0J"));
        assert!(seq.contains("PROMPT"));
        assert_eq!(new_row, 0);
    }

    #[test]
    fn visible_width_ignores_ansi() {
        assert_eq!(visible_width("\x1b[31mred\x1b[0m"), 3);
        assert_eq!(visible_width("abc"), 3);
    }

    #[test]
    fn render_block_clears_old_block_relative_to_cursor() {
        // Previous block: 3 rows, cursor on row 0 (top). New block: input only.
        let (seq, rows, rpos) = render_block("P> hi", 5, 5, &[], 80, 3, 0);
        // Must drop to the old block's bottom (down 2) then erase upward.
        assert!(seq.contains("\x1b[2B"), "seq: {seq:?}");
        assert!(seq.contains("\x1b[2K"));
        assert!(seq.contains("\x1b[1A"));
        assert!(seq.contains("P> hi"));
        assert_eq!(rows, 1);
        assert_eq!(rpos, 0);
    }

    #[test]
    fn render_block_counts_menu_rows() {
        let menu = vec!["a".to_string(), "b".to_string()];
        let (_seq, rows, rpos) = render_block("P> ", 3, 3, &menu, 80, 0, 0);
        assert_eq!(rows, 3); // 1 input row + 2 menu rows
        assert_eq!(rpos, 0); // cursor stays on the input row
    }
}
