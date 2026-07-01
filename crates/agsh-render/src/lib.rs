//! Type-aware rich rendering of command/file output for human display.
//!
//! [`render`] detects the content type (by an optional name hint plus content
//! sniffing) and dispatches to a renderer (markdown, JSON, CSV/TSV, diff, or a
//! binary hexdump). This is a *human display* transform only — callers must
//! never apply it to bytes destined for pipes, redirects, files, or agents.

use agsh_style::Theme;

pub mod binary;
pub mod code;
pub mod csv;
pub mod image;
pub mod json;
pub mod markdown;

/// A detected content type for rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    Markdown,
    Json,
    Csv,
    Tsv,
    Diff,
    Image,
    /// Source code (the name's extension maps to a known language).
    Code,
    Binary,
    PlainText,
}

/// How much leading content to sample when sniffing.
const SNIFF_BYTES: usize = 8192;

/// Detect the content type from an optional `name` hint (e.g. a file name) and
/// the bytes themselves. The name hint wins for ambiguous text types like
/// markdown; binary/JSON/CSV/diff are recognized from content.
pub fn detect(bytes: &[u8], name: Option<&str>) -> ContentType {
    // Image: recognized by magic bytes (or a binary file with an image extension)
    // — checked before the generic binary path since images are binary too.
    if image::is_image(bytes, name) {
        return ContentType::Image;
    }

    // Binary: a NUL byte in the sampled prefix.
    if bytes.iter().take(SNIFF_BYTES).any(|&b| b == 0) {
        return ContentType::Binary;
    }

    // Extension hint.
    if let Some(name) = name {
        let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "md" | "markdown" | "mkd" => return ContentType::Markdown,
            "json" => return ContentType::Json,
            "csv" => return ContentType::Csv,
            "tsv" => return ContentType::Tsv,
            "diff" | "patch" => return ContentType::Diff,
            _ => {}
        }
        // A known source-code extension → syntax highlighting.
        if code::detect_language(name).is_some() {
            return ContentType::Code;
        }
    }

    let sample = String::from_utf8_lossy(&bytes[..bytes.len().min(SNIFF_BYTES)]);
    let trimmed = sample.trim_start();

    // JSON: starts with { or [ (and the document parses).
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(sample.trim()).is_ok()
    {
        return ContentType::Json;
    }
    if looks_like_diff(&sample) {
        return ContentType::Diff;
    }
    if looks_like_csv(&sample) {
        return ContentType::Csv;
    }
    ContentType::PlainText
}

fn looks_like_diff(sample: &str) -> bool {
    sample
        .lines()
        .take(40)
        .any(|l| l.starts_with("diff --git") || l.starts_with("@@ "))
        || (sample.contains("\n--- ") && sample.contains("\n+++ "))
}

fn looks_like_csv(sample: &str) -> bool {
    let lines: Vec<&str> = sample.lines().take(10).filter(|l| !l.is_empty()).collect();
    if lines.len() < 2 {
        return false;
    }
    let commas = lines[0].matches(',').count();
    commas >= 1 && lines.iter().all(|l| l.matches(',').count() == commas)
}

/// Render `bytes` for human display, choosing a renderer by detected type. The
/// `name` hint helps disambiguate (e.g. a `.md` file). `width` is the terminal
/// width for wrapping/tables.
pub fn render(bytes: &[u8], name: Option<&str>, theme: &Theme, width: usize) -> String {
    match detect(bytes, name) {
        ContentType::Markdown => markdown::render(&String::from_utf8_lossy(bytes), theme, width),
        ContentType::Json => json::render(&String::from_utf8_lossy(bytes), theme, width),
        ContentType::Csv => csv::render(&String::from_utf8_lossy(bytes), theme, width, b','),
        ContentType::Tsv => csv::render(&String::from_utf8_lossy(bytes), theme, width, b'\t'),
        ContentType::Diff => binary::render_diff(&String::from_utf8_lossy(bytes), theme),
        ContentType::Image => image::render(bytes, name, theme, width),
        ContentType::Code => {
            let source = String::from_utf8_lossy(bytes);
            match name.and_then(code::detect_language) {
                Some(lang) => code::highlight(&source, lang, theme),
                None => source.into_owned(),
            }
        }
        ContentType::Binary => binary::hexdump(bytes, theme, 4096),
        ContentType::PlainText => String::from_utf8_lossy(bytes).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_extension_and_content() {
        assert_eq!(
            detect(b"# Title\n", Some("README.md")),
            ContentType::Markdown
        );
        assert_eq!(detect(b"{\"a\":1}", None), ContentType::Json);
        assert_eq!(detect(b"a,b\n1,2\n", None), ContentType::Csv);
        assert_eq!(detect(b"\x7fELF\0\0", None), ContentType::Binary);
        assert_eq!(
            detect(b"diff --git a b\n@@ -1 +1 @@\n", None),
            ContentType::Diff
        );
        assert_eq!(detect(b"just words here", None), ContentType::PlainText);
    }
}
