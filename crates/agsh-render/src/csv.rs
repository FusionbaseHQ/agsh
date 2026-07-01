//! CSV/TSV table renderer for human terminal display.
//!
//! Parses delimiter-separated text with the `csv` crate and renders it as an
//! aligned, box-drawn table: a top border, a bold header row (the first record),
//! a header separator, left-aligned data rows, and a bottom border. Column widths
//! are computed from the data, capped per column, and shrunk to fit the terminal
//! `width` by truncating long cells with `…`. Output is bounded: at most
//! [`MAX_ROWS`] records and [`MAX_COLS`] columns are rendered, with a trailing
//! "… N more rows" note when input is longer.
//!
//! This is a *display-only* transform (for a real TTY); it never panics and
//! degrades to plain text at the terminal's `None` color level.

use agsh_style::{Role, Theme};
use csv::ReaderBuilder;

/// Maximum number of records (header + data) collected; the rest are counted.
const MAX_ROWS: usize = 200;
/// Maximum number of columns rendered; extra columns are ignored.
const MAX_COLS: usize = 64;
/// Maximum natural width (in characters) of any single column.
const MAX_COL_WIDTH: usize = 40;

/// Render `input` (delimiter-separated) as an aligned box-drawn table.
///
/// `width` is the target terminal width used to shrink columns; `delimiter` is
/// the field separator (e.g. `b','` for CSV, `b'\t'` for TSV). Returns
/// `input.to_string()` unchanged when no records parse.
pub fn render(input: &str, theme: &Theme, width: usize, delimiter: u8) -> String {
    let mut rdr = ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .from_reader(input.as_bytes());

    let mut records: Vec<Vec<String>> = Vec::new();
    let mut extra: usize = 0;
    // `flatten` skips malformed records rather than aborting the whole render.
    for rec in rdr.records().flatten() {
        if records.len() < MAX_ROWS {
            records.push(rec.iter().map(sanitize).collect());
        } else {
            extra += 1;
        }
    }

    if records.is_empty() {
        return input.to_string();
    }

    // Column count: widest record, bounded, at least one.
    let ncols = records
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .clamp(1, MAX_COLS);

    // Natural column widths (chars), each at least 1 and capped.
    let mut widths: Vec<usize> = vec![1; ncols];
    for r in &records {
        for (c, w) in widths.iter_mut().enumerate() {
            if let Some(cell) = r.get(c) {
                let len = cell.chars().count().min(MAX_COL_WIDTH);
                if len > *w {
                    *w = len;
                }
            }
        }
    }

    // Shrink the widest columns until the table fits `width`. Each column costs
    // its content width plus two padding spaces and one separator; plus the
    // leading border: total = sum(widths) + 3*ncols + 1.
    if width > 0 {
        let overhead = 3 * ncols + 1;
        let target = width.saturating_sub(overhead);
        let mut total: usize = widths.iter().sum();
        while total > target {
            let (idx, maxw) = widths
                .iter()
                .enumerate()
                .max_by_key(|&(_, &w)| w)
                .map(|(i, &w)| (i, w))
                .unwrap_or((0, 0));
            if maxw <= 1 {
                break;
            }
            widths[idx] -= 1;
            total -= 1;
        }
    }

    let bar = theme.paint(Role::Border, "│");
    let header = &records[0];
    let data = &records[1..];

    let mut out = String::new();
    out.push_str(&theme.paint(Role::Border, &border_line(&widths, '┌', '┬', '┐')));
    out.push('\n');
    out.push_str(&render_row(header, &widths, true, theme, &bar));
    out.push('\n');
    if !data.is_empty() {
        out.push_str(&theme.paint(Role::Border, &border_line(&widths, '├', '┼', '┤')));
        out.push('\n');
        for row in data {
            out.push_str(&render_row(row, &widths, false, theme, &bar));
            out.push('\n');
        }
    }
    out.push_str(&theme.paint(Role::Border, &border_line(&widths, '└', '┴', '┘')));
    if extra > 0 {
        out.push('\n');
        out.push_str(&theme.paint(Role::Muted, &format!("… {extra} more rows")));
    }
    out
}

/// Replace control characters (newlines, tabs, etc.) with spaces so a cell stays
/// on one line and keeps the table aligned.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Truncate `s` to at most `w` display characters, using `…` as the final
/// character when content is dropped.
fn truncate(s: &str, w: usize) -> String {
    let count = s.chars().count();
    if count <= w {
        return s.to_string();
    }
    match w {
        0 => String::new(),
        1 => "…".to_string(),
        _ => {
            let mut out: String = s.chars().take(w - 1).collect();
            out.push('…');
            out
        }
    }
}

/// Build a horizontal border line, e.g. `┌────┬────┐`, sized to `widths`.
fn border_line(widths: &[usize], left: char, mid: char, right: char) -> String {
    let mut s = String::new();
    s.push(left);
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            s.push(mid);
        }
        s.push_str(&"─".repeat(w + 2));
    }
    s.push(right);
    s
}

/// Render one table row: each cell padded to its column width and left-aligned,
/// separated by the (already painted) `bar`. Header cells are painted in
/// [`Role::Heading`] (bold).
fn render_row(
    row: &[String],
    widths: &[usize],
    is_header: bool,
    theme: &Theme,
    bar: &str,
) -> String {
    let mut line = String::new();
    line.push_str(bar);
    for (c, w) in widths.iter().enumerate() {
        let raw = row.get(c).map(String::as_str).unwrap_or("");
        let cell = truncate(raw, *w);
        let pad = w.saturating_sub(cell.chars().count());
        let content = format!(" {cell}{} ", " ".repeat(pad));
        if is_header {
            line.push_str(&theme.paint(Role::Heading, &content));
        } else {
            line.push_str(&content);
        }
        line.push_str(bar);
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use agsh_style::Theme;

    fn plain() -> Theme {
        Theme::plain()
    }

    #[test]
    fn renders_basic_table() {
        let t = plain();
        let out = render("name,age\nalice,30\nbob,25\n", &t, 80, b',');
        assert!(out.contains("name"));
        assert!(out.contains("age"));
        assert!(out.contains("alice"));
        assert!(out.contains("bob"));
        assert!(out.contains('┌'));
        assert!(out.contains('│'));
        assert!(out.contains('└'));
        // top, header, separator, two data rows, bottom = 6 lines.
        assert_eq!(out.lines().count(), 6);
    }

    #[test]
    fn empty_input_returns_input() {
        let t = plain();
        assert_eq!(render("", &t, 80, b','), "");
    }

    #[test]
    fn ragged_rows_do_not_panic() {
        let t = plain();
        let out = render("a,b,c\n1\n2,3\n", &t, 80, b',');
        assert!(out.contains('a'));
        assert!(out.contains('3'));
        // Header has 3 columns; missing cells become empty without panic.
        assert_eq!(out.lines().count(), 6);
    }

    #[test]
    fn long_cells_truncated() {
        let t = plain();
        let long = "x".repeat(60);
        let input = format!("col\n{long}\n");
        let out = render(&input, &t, 200, b',');
        // The 60-char cell exceeds the per-column cap, so it is truncated.
        assert!(out.contains('…'));
        assert!(out.contains('x'));
    }

    #[test]
    fn tsv_delimiter_splits_on_tabs() {
        let t = plain();
        let out = render("a\tb\n1\t2\n", &t, 80, b'\t');
        assert!(out.contains('a'));
        assert!(out.contains('b'));
        assert!(out.contains('2'));
        assert!(out.contains('│'));
        assert_eq!(out.lines().count(), 5);
    }

    #[test]
    fn caps_rows_and_notes_extra() {
        let t = plain();
        let mut input = String::from("h\n");
        for i in 0..250 {
            input.push_str(&format!("r{i}\n"));
        }
        let out = render(&input, &t, 80, b',');
        assert!(out.contains("more rows"));
    }

    #[test]
    fn narrow_width_shrinks_columns_without_panic() {
        let t = plain();
        let long = "y".repeat(40);
        let input = format!("a,b\n{long},{long}\n");
        let out = render(&input, &t, 30, b',');
        // Wide columns are shrunk to fit and truncated with '…'.
        assert!(out.contains('…'));
        for line in out.lines() {
            assert!(!line.is_empty());
        }
    }

    #[test]
    fn header_only_table_renders() {
        let t = plain();
        let out = render("only,header\n", &t, 80, b',');
        assert!(out.contains("only"));
        assert!(out.contains("header"));
        // top, header, bottom (no separator/data rows).
        assert_eq!(out.lines().count(), 3);
    }
}
