//! Markdown renderer: turns a tasteful subset of Markdown into a styled string
//! for HUMAN TERMINAL display.
//!
//! This is deliberately a small, dependency-free (std + theme only) renderer
//! that handles the common 80% of Markdown: ATX headings, bold/italic/inline
//! code, fenced code blocks, bullet/ordered lists, blockquotes, horizontal
//! rules, links/images (as OSC 8 hyperlinks) and word-wrapped paragraphs. It is
//! intentionally forgiving — unbalanced markers and unknown syntax fall through
//! as literal text and never panic. Inputs are size-capped so pathological
//! documents stay bounded.

use agsh_style::{Role, Style, Theme};

/// Maximum input we will look at (bytes); larger inputs are truncated on a char
/// boundary so huge documents stay bounded.
const MAX_INPUT_BYTES: usize = 1 << 20; // 1 MiB
/// Maximum number of source lines processed.
const MAX_LINES: usize = 50_000;
/// Maximum number of rendered output lines kept.
const MAX_OUTPUT_LINES: usize = 60_000;
/// Fallback width when the caller passes 0, and the largest width we will honor.
const DEFAULT_WIDTH: usize = 80;
const MAX_WIDTH: usize = 1000;

/// Render a Markdown subset to a styled terminal string wrapped to `width`.
pub fn render(input: &str, theme: &Theme, width: usize) -> String {
    let width = if width == 0 {
        DEFAULT_WIDTH
    } else {
        width.min(MAX_WIDTH)
    };

    // Bound the input on a char boundary.
    let input = if input.len() > MAX_INPUT_BYTES {
        let mut end = MAX_INPUT_BYTES;
        while end > 0 && !input.is_char_boundary(end) {
            end -= 1;
        }
        &input[..end]
    } else {
        input
    };

    let lines: Vec<&str> = input.lines().take(MAX_LINES).collect();
    let mut out: Vec<String> = Vec::new();
    let mut para: Vec<String> = Vec::new();
    let mut pending_blank = false;

    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let lead = raw.trim_start();

        // Fenced code block: consume until the closing fence (or EOF). Markdown
        // inside is *not* parsed.
        if is_fence(lead) {
            flush_para(&mut para, &mut out, &mut pending_blank, theme, width);
            i += 1;
            let mut code: Vec<String> = Vec::new();
            while i < lines.len() && !is_fence(lines[i].trim_start()) {
                code.push(render_code_line(lines[i], theme));
                i += 1;
            }
            if i < lines.len() {
                i += 1; // skip the closing fence
            }
            push_block(&mut out, &mut pending_blank, code);
            continue;
        }

        // Blank line: ends the current paragraph; runs of blanks collapse to one.
        if raw.trim().is_empty() {
            flush_para(&mut para, &mut out, &mut pending_blank, theme, width);
            pending_blank = true;
            i += 1;
            continue;
        }

        // Horizontal rule (checked before lists so "- - -" is a rule).
        if is_hr(raw) {
            flush_para(&mut para, &mut out, &mut pending_blank, theme, width);
            let rule = theme.paint(Role::Border, &"─".repeat(width));
            push_block(&mut out, &mut pending_blank, vec![rule]);
            i += 1;
            continue;
        }

        // ATX heading.
        if let Some((level, content)) = parse_heading(lead) {
            flush_para(&mut para, &mut out, &mut pending_blank, theme, width);
            let block = render_heading(level, &content, theme, width);
            push_block(&mut out, &mut pending_blank, block);
            i += 1;
            continue;
        }

        // Blockquote.
        if let Some(rest) = lead.strip_prefix('>') {
            flush_para(&mut para, &mut out, &mut pending_blank, theme, width);
            let words = plain_words(rest.trim_start(), theme.style(Role::Muted));
            let block = wrap_words(words, theme, width, &Prefix::blockquote(theme));
            push_block(&mut out, &mut pending_blank, block);
            i += 1;
            continue;
        }

        // List item (bullet or ordered).
        if let Some((prefix, content)) = parse_list_item(raw, theme, width) {
            flush_para(&mut para, &mut out, &mut pending_blank, theme, width);
            let words = parse_inline(&content, theme);
            let block = wrap_words(words, theme, width, &prefix);
            push_block(&mut out, &mut pending_blank, block);
            i += 1;
            continue;
        }

        // Otherwise: paragraph text (consecutive lines join, then wrap).
        para.push(raw.trim().to_string());
        i += 1;
    }
    flush_para(&mut para, &mut out, &mut pending_blank, theme, width);

    out.truncate(MAX_OUTPUT_LINES);
    let mut s = out.join("\n");
    if !s.is_empty() {
        s.push('\n');
    }
    s
}

/// Append a finished block, inserting a single separating blank line if blanks
/// were pending. Empty blocks are ignored.
fn push_block(out: &mut Vec<String>, pending_blank: &mut bool, block: Vec<String>) {
    if block.is_empty() {
        return;
    }
    if *pending_blank && !out.is_empty() {
        out.push(String::new());
    }
    *pending_blank = false;
    out.extend(block);
}

/// Wrap and emit the accumulated paragraph, if any.
fn flush_para(
    para: &mut Vec<String>,
    out: &mut Vec<String>,
    pending_blank: &mut bool,
    theme: &Theme,
    width: usize,
) {
    if para.is_empty() {
        return;
    }
    let text = para.join(" ");
    para.clear();
    let words = parse_inline(&text, theme);
    let block = wrap_words(words, theme, width, &Prefix::none());
    push_block(out, pending_blank, block);
}

/// A line whose leading content opens/closes a fenced code block.
fn is_fence(line: &str) -> bool {
    line.starts_with("```") || line.starts_with("~~~")
}

/// A horizontal rule: >= 3 of the same `-`, `*` or `_`, ignoring whitespace.
fn is_hr(raw: &str) -> bool {
    let trimmed: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    if trimmed.chars().count() < 3 {
        return false;
    }
    let Some(first) = trimmed.chars().next() else {
        return false;
    };
    matches!(first, '-' | '*' | '_') && trimmed.chars().all(|c| c == first)
}

/// Parse an ATX heading from a leading-trimmed line, returning `(level, text)`.
fn parse_heading(lead: &str) -> Option<(usize, String)> {
    let hashes = lead.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &lead[hashes..]; // '#' is ASCII, so `hashes` is a byte offset.
    if rest.is_empty() {
        return Some((hashes, String::new()));
    }
    // A space must follow the hashes (so "#tag" stays literal text).
    rest.strip_prefix(' ')
        .map(|after| (hashes, after.trim().to_string()))
}

/// Render a heading. Level 1 is uppercased with a full-width underline rule;
/// level 2 gets an underline rule the width of its text; deeper levels are just
/// bold.
fn render_heading(level: usize, content: &str, theme: &Theme, width: usize) -> Vec<String> {
    let mut v = Vec::new();
    if level == 1 {
        let text = content.to_uppercase();
        if !text.is_empty() {
            v.push(theme.paint(Role::Heading, &text));
        }
        v.push(theme.paint(Role::Border, &"─".repeat(width)));
    } else {
        if !content.is_empty() {
            v.push(theme.paint(Role::Heading, content));
        }
        if level == 2 {
            let n = content.chars().count().clamp(1, width);
            v.push(theme.paint(Role::Border, &"─".repeat(n)));
        }
    }
    v
}

/// Render a single fenced-code-block line: a `│` bar (Border) then the raw line
/// styled as code. Content is never re-parsed as Markdown.
fn render_code_line(raw: &str, theme: &Theme) -> String {
    let bar = theme.paint(Role::Border, "│");
    let code = theme.paint(Role::Code, raw);
    format!("{bar} {code}")
}

/// Parse a list item from a raw line, returning its line prefix and content.
fn parse_list_item(raw: &str, theme: &Theme, width: usize) -> Option<(Prefix, String)> {
    let pad = raw.chars().take_while(|c| *c == ' ').count();
    let rest = &raw[pad..]; // leading spaces are ASCII.
    let indent = pad.min(width / 2);

    // Unordered: '-', '*' or '+' followed by a space (or nothing).
    if let Some(after) = rest.strip_prefix(|c: char| matches!(c, '-' | '*' | '+')) {
        if after.is_empty() {
            return Some((Prefix::bullet(theme, indent), String::new()));
        }
        if let Some(content) = after.strip_prefix(' ') {
            return Some((
                Prefix::bullet(theme, indent),
                content.trim_start().to_string(),
            ));
        }
    }

    // Ordered: digits then '.' or ')' then a space (or nothing).
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let after_digits = &rest[digits.len()..];
        if let Some(sep) = after_digits.chars().next() {
            if sep == '.' || sep == ')' {
                let after = &after_digits[1..]; // sep is ASCII.
                if after.is_empty() || after.starts_with(' ') {
                    let marker = format!("{digits}{sep}");
                    let content = after.trim_start().to_string();
                    return Some((Prefix::ordered(theme, indent, &marker), content));
                }
            }
        }
    }
    None
}

/// A wrapped block's line prefixes: one for the first line, one for
/// continuation lines, with their visible widths.
struct Prefix {
    first: String,
    first_w: usize,
    cont: String,
    cont_w: usize,
}

impl Prefix {
    fn none() -> Self {
        Self {
            first: String::new(),
            first_w: 0,
            cont: String::new(),
            cont_w: 0,
        }
    }

    fn blockquote(theme: &Theme) -> Self {
        let bar = theme.paint(Role::Border, "│");
        let first = format!("{bar} ");
        Self {
            first: first.clone(),
            first_w: 2,
            cont: first,
            cont_w: 2,
        }
    }

    fn bullet(theme: &Theme, indent: usize) -> Self {
        let pad = " ".repeat(indent);
        let bullet = theme.paint(Role::Accent, "•");
        let w = indent + 2;
        Self {
            first: format!("{pad}{bullet} "),
            first_w: w,
            cont: " ".repeat(w),
            cont_w: w,
        }
    }

    fn ordered(theme: &Theme, indent: usize, marker: &str) -> Self {
        let pad = " ".repeat(indent);
        let painted = theme.paint(Role::Accent, marker);
        let w = indent + marker.chars().count() + 1;
        Self {
            first: format!("{pad}{painted} "),
            first_w: w,
            cont: " ".repeat(w),
            cont_w: w,
        }
    }
}

/// A wrappable unit of inline content.
enum Word {
    /// Plain/styled text with no spaces; may be hard-split when too long.
    Text { visible: String, style: Style },
    /// A pre-rendered, unbreakable atom (e.g. a link) with its visible width.
    Atom { rendered: String, width: usize },
}

impl Word {
    fn visible_width(&self) -> usize {
        match self {
            Word::Text { visible, .. } => visible.chars().count(),
            Word::Atom { width, .. } => *width,
        }
    }

    fn render(&self, theme: &Theme) -> String {
        match self {
            Word::Text { visible, style } => style.paint(visible, theme.level),
            Word::Atom { rendered, .. } => rendered.clone(),
        }
    }
}

/// Split `text` into whitespace-delimited words all sharing `style`.
fn plain_words(text: &str, style: Style) -> Vec<Word> {
    text.split_whitespace()
        .map(|w| Word::Text {
            visible: w.to_string(),
            style,
        })
        .collect()
}

/// Build an OSC 8 hyperlink wrapping already-styled `text`.
fn osc8(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x07{text}\x1b]8;;\x07")
}

/// Append a styled run (which may contain spaces) as individual words so the
/// wrapper can break between them.
fn push_run(words: &mut Vec<Word>, text: &str, style: Style) {
    for w in text.split_whitespace() {
        words.push(Word::Text {
            visible: w.to_string(),
            style,
        });
    }
}

/// Append a link/image as a single unbreakable atom. With color enabled it is an
/// OSC 8 hyperlink; otherwise it degrades to `text (url)` so piped output stays
/// clean.
fn push_link(words: &mut Vec<Word>, theme: &Theme, display: &str, url: &str) {
    let (rendered, width) = if theme.enabled() {
        let painted = theme.paint(Role::Link, display);
        (osc8(url, &painted), display.chars().count())
    } else {
        let r = format!("{display} ({url})");
        let w = r.chars().count();
        (r, w)
    };
    words.push(Word::Atom { rendered, width });
}

/// Parse inline Markdown into wrappable words. Recognizes `**bold**`/`__bold__`,
/// `*italic*`/`_italic_`, `` `code` ``, `[text](url)` and `![alt](url)`. Unknown
/// or unbalanced markers fall through as literal text.
fn parse_inline(text: &str, theme: &Theme) -> Vec<Word> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let base = Style::new();
    let bold = theme.style(Role::Heading);
    let italic = theme.style(Role::Emphasis);
    let code = theme.style(Role::Code);

    let mut words: Vec<Word> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    while i < n {
        let c = chars[i];

        // Image: ![alt](url)
        if c == '!' && chars.get(i + 1) == Some(&'[') {
            if let Some((alt, url, next)) = parse_link_at(&chars, i + 1) {
                push_run(&mut words, &buf, base);
                buf.clear();
                push_link(&mut words, theme, &format!("🖼 {alt}"), &url);
                i = next;
                continue;
            }
        }

        // Link: [text](url)
        if c == '[' {
            if let Some((txt, url, next)) = parse_link_at(&chars, i) {
                push_run(&mut words, &buf, base);
                buf.clear();
                push_link(&mut words, theme, &txt, &url);
                i = next;
                continue;
            }
        }

        // Inline code: `code`
        if c == '`' {
            if let Some((inner, next)) = parse_code_at(&chars, i) {
                push_run(&mut words, &buf, base);
                buf.clear();
                push_run(&mut words, &inner, code);
                i = next;
                continue;
            }
        }

        // Emphasis: bold (** / __) takes precedence over italic (* / _).
        if c == '*' || c == '_' {
            if chars.get(i + 1) == Some(&c) {
                if let Some((inner, next)) = parse_delim(&chars, i, c, 2) {
                    push_run(&mut words, &buf, base);
                    buf.clear();
                    push_run(&mut words, &inner, bold);
                    i = next;
                    continue;
                }
            }
            if let Some((inner, next)) = parse_delim(&chars, i, c, 1) {
                push_run(&mut words, &buf, base);
                buf.clear();
                push_run(&mut words, &inner, italic);
                i = next;
                continue;
            }
        }

        buf.push(c);
        i += 1;
    }
    push_run(&mut words, &buf, base);
    words
}

/// Parse `[text](url)` starting at `start` (which must be `[`). Returns
/// `(text, url, index_after_close_paren)`.
fn parse_link_at(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    if chars.get(start) != Some(&'[') {
        return None;
    }
    let mut j = start + 1;
    let mut text = String::new();
    while j < chars.len() && chars[j] != ']' {
        text.push(chars[j]);
        j += 1;
    }
    if j >= chars.len() {
        return None; // no closing ']'
    }
    let mut k = j + 1;
    if chars.get(k) != Some(&'(') {
        return None;
    }
    k += 1;
    let mut url = String::new();
    while k < chars.len() && chars[k] != ')' {
        url.push(chars[k]);
        k += 1;
    }
    if k >= chars.len() {
        return None; // no closing ')'
    }
    Some((text, url, k + 1))
}

/// Parse a single-backtick code span starting at `start` (which must be a
/// backtick). Returns `(content, index_after_closing_backtick)`.
fn parse_code_at(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut j = start + 1;
    let mut inner = String::new();
    while j < chars.len() && chars[j] != '`' {
        inner.push(chars[j]);
        j += 1;
    }
    if j >= chars.len() {
        return None; // unbalanced
    }
    Some((inner, j + 1))
}

/// Parse a delimited emphasis span: an opening run of `len` copies of `c` at
/// `start`, up to the next run of at least `len` copies. Returns
/// `(inner, index_after_closing_run)`. Empty spans are rejected.
fn parse_delim(chars: &[char], start: usize, c: char, len: usize) -> Option<(String, usize)> {
    let begin = start + len;
    let mut j = begin;
    while j < chars.len() {
        if chars[j] == c {
            let mut run = 0;
            while j + run < chars.len() && chars[j + run] == c {
                run += 1;
            }
            if run >= len {
                if j == begin {
                    return None; // empty span
                }
                let inner: String = chars[begin..j].iter().collect();
                return Some((inner, j + len));
            }
            j += run;
        } else {
            j += 1;
        }
    }
    None
}

/// Word-wrap `words` to `width` using `prefix`, returning the rendered lines.
/// Words longer than the content width are hard-split.
fn wrap_words(words: Vec<Word>, theme: &Theme, width: usize, prefix: &Prefix) -> Vec<String> {
    // The widest possible content area (smallest available with either prefix).
    let max_w = width
        .saturating_sub(prefix.first_w.max(prefix.cont_w))
        .max(1);

    // Pre-expand over-long breakable words so the wrap loop never overflows.
    let mut expanded: Vec<Word> = Vec::new();
    for w in words {
        match w {
            Word::Text { visible, style } if visible.chars().count() > max_w => {
                let cs: Vec<char> = visible.chars().collect();
                for chunk in cs.chunks(max_w) {
                    expanded.push(Word::Text {
                        visible: chunk.iter().collect(),
                        style,
                    });
                }
            }
            other => expanded.push(other),
        }
    }

    let mut lines: Vec<String> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut cur_w = 0usize;
    let mut first = true;

    for w in &expanded {
        let ww = w.visible_width();
        let avail = width
            .saturating_sub(if first { prefix.first_w } else { prefix.cont_w })
            .max(1);
        if cur.is_empty() {
            cur.push(w.render(theme));
            cur_w = ww;
        } else if cur_w + 1 + ww <= avail {
            cur.push(w.render(theme));
            cur_w += 1 + ww;
        } else {
            let p = if first { &prefix.first } else { &prefix.cont };
            let content = cur.join(" ");
            lines.push(format!("{p}{content}"));
            first = false;
            cur = vec![w.render(theme)];
            cur_w = ww;
        }
    }

    if !cur.is_empty() {
        let p = if first { &prefix.first } else { &prefix.cont };
        let content = cur.join(" ");
        lines.push(format!("{p}{content}"));
    } else if expanded.is_empty() {
        // Empty content (e.g. a bare blockquote/bullet): emit the prefix alone.
        lines.push(prefix.first.clone());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use agsh_style::{ColorLevel, Icons, Palette};

    fn color_theme() -> Theme {
        Theme {
            palette: Palette::dark(),
            level: ColorLevel::TrueColor,
            icons: Icons::disabled(),
        }
    }

    fn nonempty_lines(s: &str) -> Vec<&str> {
        s.lines().filter(|l| !l.is_empty()).collect()
    }

    #[test]
    fn heading_level_one_is_uppercased_with_rule() {
        let out = render("# Title\n\nbody", &Theme::plain(), 40);
        assert!(out.contains("TITLE"), "h1 should be uppercased: {out:?}");
        assert!(out.contains('─'), "h1 should have an underline rule");
        assert!(out.contains("body"));
    }

    #[test]
    fn heading_level_two_renders_text() {
        let out = render("## Sub\ntext", &Theme::plain(), 40);
        assert!(out.contains("Sub"));
        assert!(out.contains("text"));
        assert!(out.contains('─'));
    }

    #[test]
    fn inline_markers_are_stripped() {
        let out = render(
            "This is **bold** and *italic* and `code` here.",
            &Theme::plain(),
            80,
        );
        assert!(out.contains("bold"));
        assert!(out.contains("italic"));
        assert!(out.contains("code"));
        assert!(!out.contains("**"), "bold markers should be gone: {out:?}");
        assert!(!out.contains('`'), "code markers should be gone");
    }

    #[test]
    fn fenced_code_is_preserved_verbatim() {
        let input = "```rust\n# not a heading\nlet x = **1**;\n```";
        let out = render(input, &Theme::plain(), 40);
        assert!(out.contains('│'), "code lines should have a bar");
        assert!(out.contains("# not a heading"), "no md parsing in code");
        assert!(out.contains("let x = **1**;"), "markers kept in code");
    }

    #[test]
    fn bullet_list_uses_bullets() {
        let out = render("- one\n* two\n+ three", &Theme::plain(), 40);
        assert_eq!(nonempty_lines(&out).len(), 3);
        assert!(out.contains('•'));
        assert!(out.contains("one") && out.contains("two") && out.contains("three"));
    }

    #[test]
    fn ordered_list_keeps_numbers() {
        let out = render("1. first\n2. second", &Theme::plain(), 40);
        assert!(out.contains("1."));
        assert!(out.contains("2."));
        assert!(out.contains("first") && out.contains("second"));
    }

    #[test]
    fn blockquote_has_bar_and_text() {
        let out = render("> a quoted line", &Theme::plain(), 40);
        assert!(out.contains('│'));
        assert!(out.contains("quoted"));
    }

    #[test]
    fn horizontal_rule_renders() {
        for hr in ["---", "***", "___", "- - -"] {
            let out = render(hr, &Theme::plain(), 12);
            assert!(out.contains('─'), "{hr:?} should be a rule: {out:?}");
        }
    }

    #[test]
    fn link_becomes_osc8_hyperlink() {
        let out = render(
            "see [Anthropic](https://example.com) now",
            &color_theme(),
            80,
        );
        assert!(out.contains("\x1b]8;;https://example.com\x07"));
        assert!(out.contains("Anthropic"));
        assert!(out.contains("\x1b]8;;\x07"));
    }

    #[test]
    fn image_renders_with_frame_glyph() {
        let out = render("![diagram](https://img.example/x.png)", &color_theme(), 80);
        assert!(out.contains('🖼'));
        assert!(out.contains("diagram"));
        assert!(out.contains("\x1b]8;;https://img.example/x.png\x07"));
    }

    #[test]
    fn link_degrades_without_color() {
        let out = render("[home](https://h)", &Theme::plain(), 80);
        assert!(out.contains("home"));
        assert!(out.contains("https://h"));
        assert!(!out.contains('\x1b'), "no escapes when color is off");
    }

    #[test]
    fn paragraph_wraps_to_width() {
        let text = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor";
        let out = render(text, &Theme::plain(), 20);
        for line in out.lines() {
            assert!(line.chars().count() <= 20, "line too long: {line:?}");
        }
        assert!(out.lines().count() > 1, "should wrap to multiple lines");
    }

    #[test]
    fn very_long_word_is_hard_split() {
        let word = "x".repeat(50);
        let out = render(&word, &Theme::plain(), 10);
        for line in out.lines() {
            assert!(line.chars().count() <= 10, "line too long: {line:?}");
        }
        assert!(out.contains('x'));
    }

    #[test]
    fn blank_runs_collapse() {
        let out = render("alpha\n\n\n\n\nbeta", &Theme::plain(), 40);
        assert!(
            !out.contains("\n\n\n"),
            "blank runs should collapse: {out:?}"
        );
        assert!(out.contains("alpha") && out.contains("beta"));
    }

    #[test]
    fn empty_input_is_empty() {
        assert_eq!(render("", &Theme::plain(), 80), "");
    }

    #[test]
    fn malformed_input_does_not_panic() {
        let weird = "**unbalanced [x]( `code _italic ![img](\n###### deep\n> q\n- \n1.\n```\nfoo";
        let _ = render(weird, &Theme::plain(), 5);
        let _ = render(weird, &color_theme(), 1);
        // Pathological markers, zero-ish width: just must not panic.
        let _ = render("[](", &Theme::plain(), 0);
    }
}
