//! JSON pretty-printer + syntax colorizer for human terminal display.
//!
//! [`render`] parses `input` into a [`serde_json::Value`] and walks it
//! recursively, emitting 2-space-indented, multi-line JSON with semantic
//! coloring (keys, strings, numbers, literals, and structural punctuation each
//! get their own [`Role`]). If the input is not valid JSON it is returned
//! verbatim, so this is safe to attempt on arbitrary text.
//!
//! The walk is bounded two ways: `serde_json`'s own parse recursion limit caps
//! nesting depth (so we can't blow the stack), and [`MAX_OUTPUT`] caps the size
//! of the rendered buffer so a huge document degrades to a bounded, truncated
//! view instead of an unbounded allocation.

use agsh_style::{Role, Theme};
use serde_json::{Map, Value};

/// Upper bound on the rendered (colored) output in bytes. Once exceeded the
/// walk stops early and a truncation marker is appended.
const MAX_OUTPUT: usize = 2 * 1024 * 1024;

/// Pretty-print and syntax-color `input` as JSON for terminal display.
///
/// On parse failure the original text is returned unchanged. `width` is not
/// used (JSON is laid out by nesting depth, not wrapped to the terminal).
pub fn render(input: &str, theme: &Theme, width: usize) -> String {
    let _ = width;
    let value: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => return input.to_string(),
    };

    let mut printer = Printer {
        theme: *theme,
        out: String::new(),
        truncated: false,
    };
    printer.write_value(&value, 0);

    if printer.truncated {
        printer.out.push('\n');
        printer
            .out
            .push_str(&theme.paint(Role::Comment, "... (output truncated)"));
    }
    printer.out
}

/// Accumulates the colored output while tracking the size cap.
struct Printer {
    // `Theme` is `Copy`, so holding it by value keeps the struct lifetime-free
    // and avoids any borrow tangles between `self.theme` and `self.out`.
    theme: Theme,
    out: String,
    truncated: bool,
}

impl Printer {
    /// Whether we've hit the output cap and should stop emitting.
    fn over_cap(&self) -> bool {
        self.out.len() >= MAX_OUTPUT
    }

    /// Push `depth` levels of 2-space indentation.
    fn indent(&mut self, depth: usize) {
        for _ in 0..depth {
            self.out.push_str("  ");
        }
    }

    /// Push structural punctuation (`{`, `}`, `[`, `]`, `:`, `,`).
    fn punct(&mut self, s: &str) {
        let painted = self.theme.paint(Role::Border, s);
        self.out.push_str(&painted);
    }

    fn write_value(&mut self, value: &Value, depth: usize) {
        if self.over_cap() {
            self.truncated = true;
            return;
        }
        match value {
            Value::Null => {
                let painted = self.theme.paint(Role::Operator, "null");
                self.out.push_str(&painted);
            }
            Value::Bool(b) => {
                let lit = if *b { "true" } else { "false" };
                let painted = self.theme.paint(Role::Operator, lit);
                self.out.push_str(&painted);
            }
            Value::Number(n) => {
                let painted = self.theme.paint(Role::Accent, &n.to_string());
                self.out.push_str(&painted);
            }
            Value::String(s) => {
                let painted = self.theme.paint(Role::Str, &escape_json_string(s));
                self.out.push_str(&painted);
            }
            Value::Array(items) => self.write_array(items, depth),
            Value::Object(map) => self.write_object(map, depth),
        }
    }

    fn write_array(&mut self, items: &[Value], depth: usize) {
        if items.is_empty() {
            self.punct("[]");
            return;
        }
        self.punct("[");
        self.out.push('\n');
        let last = items.len() - 1;
        for (i, item) in items.iter().enumerate() {
            if self.over_cap() {
                self.truncated = true;
                break;
            }
            self.indent(depth + 1);
            self.write_value(item, depth + 1);
            if i != last {
                self.punct(",");
            }
            self.out.push('\n');
        }
        self.indent(depth);
        self.punct("]");
    }

    fn write_object(&mut self, map: &Map<String, Value>, depth: usize) {
        if map.is_empty() {
            self.punct("{}");
            return;
        }
        self.punct("{");
        self.out.push('\n');
        let last = map.len().saturating_sub(1);
        for (i, (key, val)) in map.iter().enumerate() {
            if self.over_cap() {
                self.truncated = true;
                break;
            }
            self.indent(depth + 1);
            let painted_key = self.theme.paint(Role::Var, &escape_json_string(key));
            self.out.push_str(&painted_key);
            self.punct(":");
            self.out.push(' ');
            self.write_value(val, depth + 1);
            if i != last {
                self.punct(",");
            }
            self.out.push('\n');
        }
        self.indent(depth);
        self.punct("}");
    }
}

/// Render `s` as a quoted, properly escaped JSON string literal (including the
/// surrounding double quotes). Handles quotes, backslashes, the named control
/// escapes, and `\u00XX` for the remaining control characters.
fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            c if u32::from(c) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Theme {
        // No-color theme: `paint` returns text unchanged, so we can assert on
        // the visible characters without matching escape sequences.
        Theme::plain()
    }

    #[test]
    fn invalid_json_returned_unchanged() {
        let t = plain();
        assert_eq!(render("not json at all", &t, 80), "not json at all");
        assert_eq!(render("", &t, 80), "");
        assert_eq!(render("{ broken", &t, 80), "{ broken");
    }

    #[test]
    fn object_is_multiline_and_indented() {
        let t = plain();
        let out = render(
            r#"{"name":"agsh","count":3,"ok":true,"nothing":null}"#,
            &t,
            80,
        );
        assert!(out.contains("\"name\""), "key present: {out}");
        assert!(out.contains("\"agsh\""), "string value present");
        assert!(out.contains('3'), "number present");
        assert!(out.contains("true"), "bool present");
        assert!(out.contains("null"), "null present");
        assert!(out.contains('\n'), "multi-line");
        assert!(out.contains("  "), "two-space indentation");
        // Keys are sorted deterministically (BTreeMap-backed Map).
        assert!(out.starts_with('{'));
        assert!(out.trim_end().ends_with('}'));
    }

    #[test]
    fn nested_structures_increase_indent_and_lines() {
        let t = plain();
        let out = render(r#"{"a":[1,2,{"b":3}]}"#, &t, 80);
        assert!(out.lines().count() >= 6, "expanded over many lines:\n{out}");
        assert!(out.contains("    "), "depth-2 four-space indent");
        assert!(out.contains("      "), "depth-3 six-space indent");
        assert!(out.contains("\"b\""), "nested key present");
    }

    #[test]
    fn empty_containers_stay_compact() {
        let t = plain();
        assert_eq!(render("{}", &t, 80), "{}");
        assert_eq!(render("[]", &t, 80), "[]");
        assert!(render(r#"{"x":[]}"#, &t, 80).contains("[]"));
    }

    #[test]
    fn top_level_scalars() {
        let t = plain();
        assert_eq!(render("42", &t, 80), "42");
        assert_eq!(render("true", &t, 80), "true");
        assert_eq!(render("null", &t, 80), "null");
        assert_eq!(render("-3.5", &t, 80), "-3.5");
        assert_eq!(render("\"hi\"", &t, 80), "\"hi\"");
    }

    #[test]
    fn strings_are_escaped() {
        let t = plain();
        // JSON parses these escapes into real chars; we must re-escape them.
        let out = render(r#"{"k":"line1\nline2 \"q\" \\back\ttab"}"#, &t, 80);
        assert!(out.contains("\\n"), "newline re-escaped: {out}");
        assert!(out.contains("\\\""), "quote re-escaped");
        assert!(out.contains("\\\\"), "backslash re-escaped");
        assert!(out.contains("\\t"), "tab re-escaped");
        // No literal newline should leak inside the string value's line.
        assert!(!out.contains("line1\nline2"));
    }

    #[test]
    fn control_chars_use_unicode_escape() {
        // A raw control char (0x01) has no named escape; expect a unicode form.
        assert_eq!(escape_json_string("\u{1}"), "\"\\u0001\"");
        assert_eq!(escape_json_string("ok"), "\"ok\"");
    }

    #[test]
    fn huge_input_is_bounded_and_marked() {
        let t = plain();
        // Build an array large enough that the rendered form exceeds the cap.
        let mut input = String::from("[");
        for i in 0..400_000 {
            if i != 0 {
                input.push(',');
            }
            input.push_str("1234567");
        }
        input.push(']');

        let out = render(&input, &t, 80);
        assert!(
            out.len() < MAX_OUTPUT + 8192,
            "output stays bounded near the cap, got {}",
            out.len()
        );
        assert!(out.contains("truncated"), "truncation marker appended");
    }

    #[test]
    fn unicode_passes_through() {
        let t = plain();
        let out = render(r#"{"greet":"héllo 世界"}"#, &t, 80);
        assert!(out.contains("héllo 世界"));
    }
}
