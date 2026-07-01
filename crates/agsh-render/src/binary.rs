//! Binary + diff rendering for human terminal display.
//!
//! Two transforms, both for a real TTY only (never for bytes destined for pipes,
//! files, or agents):
//!
//! * [`hexdump`] — a classic `xxd`-style hex/ascii dump of up to `max_bytes`
//!   bytes, 16 per row, with the offset and ascii gutter dimmed and the hex
//!   column left plain. Short or odd final rows pad the hex column so the ascii
//!   gutter stays aligned, and an oversized input gets a trailing "… N more
//!   bytes" notice instead of being dumped in full.
//! * [`render_diff`] — line-by-line colorization of a unified diff. File/header
//!   lines become headings, hunk markers become info, added/removed lines take
//!   the add/remove roles, and every other line (plus overall line order) is
//!   preserved verbatim.
//!
//! At the terminal's `None` color level every paint is a no-op, so both
//! functions degrade to clean, deterministic plain text.

use std::fmt::Write as _;

use agsh_style::{Role, Theme};

/// How many bytes are shown per hexdump row.
const ROW: usize = 16;

/// Render `bytes` (capped at `max_bytes`) as an `xxd`-style hex/ascii dump.
///
/// Each row is `"{offset:08x}  {hex}  |{ascii}|"`: the offset and `|ascii|`
/// gutter are painted [`Role::Muted`], the hex column is left plain. Printable
/// bytes (`0x20..=0x7e`) appear literally in the gutter, everything else as `.`.
/// When the input is longer than `max_bytes`, a final muted line reports how
/// many bytes were elided. Never panics on short or odd final rows.
pub fn hexdump(bytes: &[u8], theme: &Theme, max_bytes: usize) -> String {
    let shown = &bytes[..bytes.len().min(max_bytes)];
    let mut lines: Vec<String> = Vec::with_capacity(shown.len() / ROW + 2);

    for (idx, chunk) in shown.chunks(ROW).enumerate() {
        let offset = idx * ROW;
        let mut line = String::with_capacity(96);

        // Offset column (dimmed).
        line.push_str(&theme.paint(Role::Muted, &format!("{offset:08x}")));
        line.push_str("  ");

        // Hex column (plain), padded to a fixed width so the gutter aligns.
        let mut hex = String::with_capacity(48);
        for i in 0..ROW {
            if i == ROW / 2 {
                // Extra gap splitting the two groups of eight.
                hex.push(' ');
            }
            match chunk.get(i) {
                Some(b) => {
                    let _ = write!(hex, "{b:02x}");
                }
                None => hex.push_str("  "),
            }
            if i != ROW - 1 {
                hex.push(' ');
            }
        }
        line.push_str(&hex);
        line.push_str("  ");

        // Ascii gutter (dimmed), including the bracketing pipes.
        let mut ascii = String::with_capacity(chunk.len() + 2);
        ascii.push('|');
        for &b in chunk {
            ascii.push(if (0x20..=0x7e).contains(&b) {
                b as char
            } else {
                '.'
            });
        }
        ascii.push('|');
        line.push_str(&theme.paint(Role::Muted, &ascii));

        lines.push(line);
    }

    if bytes.len() > max_bytes {
        let more = bytes.len() - max_bytes;
        lines.push(theme.paint(Role::Muted, &format!("… {more} more bytes")));
    }

    lines.join("\n")
}

/// Colorize a unified diff `input`, preserving every line and its order.
///
/// File/header lines (`+++ `, `--- `, `diff --git`) become [`Role::Heading`]
/// (bold), hunk headers (`@@`) become [`Role::Info`], added lines (`+`, but not
/// the `+++` header) become [`Role::Added`], removed lines (`-`, but not the
/// `---` header) become [`Role::Removed`]; all other lines are emitted
/// unchanged.
pub fn render_diff(input: &str, theme: &Theme) -> String {
    // Split on '\n' (not `.lines()`) so trailing newlines and blank lines are
    // preserved exactly when rejoined.
    input
        .split('\n')
        .map(|line| match classify(line) {
            Some(role) => theme.paint(role, line),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The role a diff line should be painted in, or `None` for verbatim.
fn classify(line: &str) -> Option<Role> {
    if line.starts_with("diff --git") || line.starts_with("+++ ") || line.starts_with("--- ") {
        Some(Role::Heading)
    } else if line.starts_with("@@") {
        Some(Role::Info)
    } else if line.starts_with('+') && !line.starts_with("+++") {
        Some(Role::Added)
    } else if line.starts_with('-') && !line.starts_with("---") {
        Some(Role::Removed)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Theme {
        Theme::plain()
    }

    #[test]
    fn hexdump_basic_row() {
        let out = hexdump(b"hello world!", &plain(), 4096);
        // Single row, no truncation.
        assert_eq!(out.lines().count(), 1);
        assert!(out.starts_with("00000000  "), "offset prefix: {out:?}");
        assert!(
            out.contains("68 65 6c 6c 6f 20 77 6f"),
            "hex group: {out:?}"
        );
        assert!(out.contains("72 6c 64 21"), "hex tail: {out:?}");
        assert!(out.contains("|hello world!|"), "ascii gutter: {out:?}");
    }

    #[test]
    fn hexdump_nonprintable_becomes_dot() {
        let out = hexdump(&[0x00, 0x09, 0x41, 0xff], &plain(), 4096);
        // 0x41 == 'A' is printable; the rest are dots.
        assert!(out.contains("|..A.|"), "gutter: {out:?}");
        assert!(out.contains("00 09 41 ff"), "hex: {out:?}");
    }

    #[test]
    fn hexdump_short_final_row_aligns_gutter() {
        // Three bytes: the hex column must be padded so the gutter still aligns
        // at the same column a full row would use.
        let short = hexdump(&[1, 2, 3], &plain(), 4096);
        let full = hexdump(&[0u8; 16], &plain(), 4096);
        let col = |s: &str| s.find('|').expect("gutter present");
        assert_eq!(col(&short), col(&full), "gutter column mismatch");
    }

    #[test]
    fn hexdump_truncates_with_notice() {
        let data = vec![0xABu8; 20];
        let out = hexdump(&data, &plain(), 16);
        let rows: Vec<&str> = out.lines().collect();
        assert_eq!(rows.len(), 2, "one row + notice: {out:?}");
        assert!(rows[1].contains("4 more bytes"), "notice: {out:?}");
    }

    #[test]
    fn hexdump_empty_is_empty_no_panic() {
        assert_eq!(hexdump(&[], &plain(), 4096), "");
        // max_bytes == 0 with content: nothing dumped, only the notice.
        let out = hexdump(b"abc", &plain(), 0);
        assert_eq!(out, "… 3 more bytes");
    }

    #[test]
    fn hexdump_multiple_rows_offsets() {
        let data = vec![0u8; 33];
        let out = hexdump(&data, &plain(), 4096);
        let rows: Vec<&str> = out.lines().collect();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].starts_with("00000000  "));
        assert!(rows[1].starts_with("00000010  "));
        assert!(rows[2].starts_with("00000020  "));
    }

    #[test]
    fn diff_plain_theme_is_identity() {
        // With no color, every line is emitted verbatim, so order and content
        // (including blanks and a trailing newline) round-trip exactly.
        let input = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n ctx\n-old\n+new\n\n";
        assert_eq!(render_diff(input, &plain()), input);
    }

    #[test]
    fn diff_classifies_lines() {
        assert_eq!(classify("diff --git a/x b/x"), Some(Role::Heading));
        assert_eq!(classify("--- a/x"), Some(Role::Heading));
        assert_eq!(classify("+++ b/x"), Some(Role::Heading));
        assert_eq!(classify("@@ -1 +1 @@"), Some(Role::Info));
        assert_eq!(classify("+added"), Some(Role::Added));
        assert_eq!(classify("-removed"), Some(Role::Removed));
        assert_eq!(classify(" context"), None);
        assert_eq!(classify(""), None);
        // '+++'/'---' without a trailing space are neither headers nor add/remove.
        assert_eq!(classify("+++"), None);
        assert_eq!(classify("---"), None);
    }

    #[test]
    fn diff_preserves_line_count() {
        let input = "line1\nline2\nline3";
        let out = render_diff(input, &plain());
        assert_eq!(out.split('\n').count(), 3);
        assert_eq!(out, input);
    }
}
