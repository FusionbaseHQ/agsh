//! Inline image display via terminal graphics protocols.
//!
//! `view image.png` renders the image *in the terminal* when it supports a
//! graphics protocol — the iTerm2 inline-image protocol (iTerm2, WezTerm; any
//! format) or the Kitty graphics protocol (Kitty, Ghostty; PNG transmitted
//! directly, since we don't decode other formats offline). Terminals without a
//! protocol get a concise, themed info card (format, dimensions, size) instead
//! of a hexdump. This is a human-display transform only and is reached solely on
//! a TTY (see `rich_observation`).

use base64::Engine;

use agsh_style::{Color, Role, Theme};

/// An image format we recognize (by magic bytes, then by extension).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
    Tiff,
    Ico,
    /// Recognized by extension but not by a magic we parse (heic/avif/…).
    Other,
}

impl Format {
    fn label(self) -> &'static str {
        match self {
            Format::Png => "PNG",
            Format::Jpeg => "JPEG",
            Format::Gif => "GIF",
            Format::Webp => "WebP",
            Format::Bmp => "BMP",
            Format::Tiff => "TIFF",
            Format::Ico => "ICO",
            Format::Other => "image",
        }
    }
}

/// Which inline-image protocol the current terminal speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    /// iTerm2 inline-image protocol (iTerm2, WezTerm) — accepts any encoded format.
    ITerm2,
    /// Kitty graphics protocol (Kitty, Ghostty) — we only transmit PNG directly.
    Kitty,
    /// No inline-image support detected.
    None,
}

/// Whether these bytes (with an optional name hint) are a displayable image.
pub fn is_image(bytes: &[u8], name: Option<&str>) -> bool {
    sniff(bytes, name).is_some()
}

/// Detect the image format from magic bytes first, then the name's extension.
fn sniff(bytes: &[u8], name: Option<&str>) -> Option<Format> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some(Format::Png);
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(Format::Jpeg);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(Format::Gif);
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(Format::Webp);
    }
    if bytes.starts_with(b"BM") {
        return Some(Format::Bmp);
    }
    if bytes.starts_with(&[0x49, 0x49, 0x2a, 0x00]) || bytes.starts_with(&[0x4d, 0x4d, 0x00, 0x2a])
    {
        return Some(Format::Tiff);
    }
    if bytes.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
        return Some(Format::Ico);
    }
    // Extension fallback for formats whose magic we don't sniff (heic/avif/…),
    // but only when the bytes are actually binary (avoid catching text named *.ts).
    if let Some(name) = name {
        let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        let by_ext = match ext.as_str() {
            "png" => Some(Format::Png),
            "jpg" | "jpeg" | "jfif" => Some(Format::Jpeg),
            "gif" => Some(Format::Gif),
            "webp" => Some(Format::Webp),
            "bmp" => Some(Format::Bmp),
            "tif" | "tiff" => Some(Format::Tiff),
            "ico" => Some(Format::Ico),
            "heic" | "heif" | "avif" => Some(Format::Other),
            _ => None,
        };
        if by_ext.is_some() && bytes.iter().take(512).any(|&b| b == 0) {
            return by_ext;
        }
    }
    None
}

/// Don't stream more than this much base64 into the terminal inline; above it,
/// show the info card (most screenshots/photos are well under 10 MiB).
const MAX_INLINE_BYTES: usize = 10 * 1024 * 1024;

/// Render an image for terminal display. Preference order:
/// 1. a crisp terminal graphics protocol (iTerm2 / Kitty) when detected;
/// 2. otherwise truecolor half-block art (`▀`) — works in any color terminal;
/// 3. a themed info card if the image can't be decoded / color is off / too big.
pub fn render(bytes: &[u8], name: Option<&str>, theme: &Theme, max_cols: usize) -> String {
    let format = sniff(bytes, name).unwrap_or(Format::Other);
    let too_large = bytes.len() > MAX_INLINE_BYTES;
    // tmux/screen would need DCS passthrough wrapping; skip the crisp protocols.
    let in_mux = std::env::var_os("TMUX").is_some()
        || std::env::var("TERM")
            .map(|t| t.starts_with("screen"))
            .unwrap_or(false);
    if !too_large && !in_mux {
        match detect_protocol() {
            Protocol::ITerm2 => return iterm2_sequence(bytes),
            Protocol::Kitty if format == Format::Png => return kitty_png_sequence(bytes),
            _ => {}
        }
    }
    // Universal fallback: decode and draw the image as colored half-blocks.
    if !too_large && theme.enabled() {
        if let Some(art) = halfblocks(bytes, theme, max_cols) {
            return art;
        }
    }
    info_card(bytes, name, format, theme, too_large)
}

/// Terminal size in character cells `(cols, rows)` from the controlling tty,
/// defaulting to 80×24 when it can't be queried.
fn term_cells() -> (u16, u16) {
    rustix::termios::tcgetwinsize(std::io::stdout())
        .map(|w| (w.ws_col, w.ws_row))
        .ok()
        .filter(|&(c, r)| c > 0 && r > 0)
        .unwrap_or((80, 24))
}

/// Decode the image and render it as truecolor half-blocks: each character cell
/// is `▀` (upper half block) with the foreground = the upper pixel and the
/// background = the lower pixel, so one cell shows two vertically-stacked pixels.
/// Sized to fit the terminal (preserving aspect ratio). Returns None if the image
/// can't be decoded or the color level can't encode (NO_COLOR).
fn halfblocks(bytes: &[u8], theme: &Theme, max_cols: usize) -> Option<String> {
    let (cols, rows) = term_cells();
    // Fit within the terminal: at most `max_cols` wide; leave 2 rows for the
    // prompt; each cell row is 2 image pixels tall.
    let box_w = (max_cols.min(cols as usize).max(1)) as u32;
    let box_h = (rows.saturating_sub(2).max(1) as u32) * 2;

    let decoded = image::load_from_memory(bytes).ok()?;
    let rgb = decoded
        .resize(box_w, box_h, image::imageops::FilterType::Triangle)
        .to_rgb8();
    let (w, h) = (rgb.width(), rgb.height());
    if w == 0 || h == 0 {
        return None;
    }

    let level = theme.level;
    let mut out = String::with_capacity((w * h) as usize * 12 / 2);
    let mut y = 0u32;
    while y < h {
        for x in 0..w {
            let top = rgb.get_pixel(x, y).0;
            let fg = Color::rgb(top[0], top[1], top[2]).sgr(level, true)?;
            if y + 1 < h {
                let bot = rgb.get_pixel(x, y + 1).0;
                let bg = Color::rgb(bot[0], bot[1], bot[2]).sgr(level, false)?;
                out.push_str(&format!("\x1b[{fg};{bg}m\u{2580}"));
            } else {
                // Odd height: last band has no lower pixel — leave it transparent.
                out.push_str(&format!("\x1b[{fg}m\u{2580}"));
            }
        }
        out.push_str("\x1b[0m\n");
        y += 2;
    }
    Some(out)
}

/// Detect the terminal's inline-image protocol from the environment.
fn detect_protocol() -> Protocol {
    if std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var("TERM")
            .map(|t| t.contains("kitty"))
            .unwrap_or(false)
    {
        return Protocol::Kitty;
    }
    let prog = std::env::var("TERM_PROGRAM").unwrap_or_default();
    match prog.as_str() {
        "iTerm.app" | "WezTerm" => Protocol::ITerm2,
        "ghostty" => Protocol::Kitty,
        _ => {
            if std::env::var_os("WEZTERM_PANE").is_some() {
                Protocol::ITerm2
            } else {
                Protocol::None
            }
        }
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// iTerm2 inline-image escape: `ESC ] 1337 ; File=inline=1;…: <base64> BEL`.
/// iTerm2/WezTerm scale the image to fit the width while preserving aspect ratio.
fn iterm2_sequence(bytes: &[u8]) -> String {
    format!(
        "\x1b]1337;File=inline=1;size={};preserveAspectRatio=1:{}\x07\n",
        bytes.len(),
        b64(bytes)
    )
}

/// Kitty graphics escape for a PNG (`f=100`), transmitted-and-displayed (`a=T`)
/// in ≤4096-byte base64 chunks (`m=1` until the last).
fn kitty_png_sequence(bytes: &[u8]) -> String {
    let payload = b64(bytes);
    let chunks: Vec<&[u8]> = payload.as_bytes().chunks(4096).collect();
    let mut out = String::with_capacity(payload.len() + chunks.len() * 16 + 8);
    for (i, chunk) in chunks.iter().enumerate() {
        let more = if i + 1 < chunks.len() { 1 } else { 0 };
        let text = std::str::from_utf8(chunk).unwrap_or("");
        if i == 0 {
            out.push_str(&format!("\x1b_Ga=T,f=100,m={more};{text}\x1b\\"));
        } else {
            out.push_str(&format!("\x1b_Gm={more};{text}\x1b\\"));
        }
    }
    out.push('\n');
    out
}

/// A concise, themed fallback shown when the image can't be drawn inline (no
/// supporting terminal, inside tmux, or larger than `MAX_INLINE_BYTES`).
fn info_card(
    bytes: &[u8],
    name: Option<&str>,
    format: Format,
    theme: &Theme,
    too_large: bool,
) -> String {
    let title = name.unwrap_or("image");
    let mut line = format!(
        "{} {}",
        theme.paint(Role::Accent, "▢"),
        theme.paint(Role::Heading, title)
    );
    let mut meta = vec![format.label().to_string()];
    if let Some((w, h)) = dimensions(bytes, format) {
        meta.push(format!("{w}×{h}"));
    }
    meta.push(human_size(bytes.len()));
    line.push_str(&format!(
        "  {}\n",
        theme.paint(Role::Muted, &meta.join(" · "))
    ));
    let note = if too_large {
        "(too large to display inline)\n"
    } else {
        "(this terminal has no inline-image support — open in iTerm2, Kitty, WezTerm, or Ghostty)\n"
    };
    line.push_str(&theme.paint(Role::Muted, note));
    line
}

fn human_size(n: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Best-effort pixel dimensions parsed from the header (no decode). Returns None
/// for formats we don't parse (WebP/HEIC/…).
fn dimensions(bytes: &[u8], format: Format) -> Option<(u32, u32)> {
    match format {
        Format::Png => {
            // IHDR: 8-byte sig + 4 len + "IHDR" → width@16, height@20 (BE u32).
            let w = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
            let h = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
            Some((w, h))
        }
        Format::Gif => {
            let w = u16::from_le_bytes(bytes.get(6..8)?.try_into().ok()?);
            let h = u16::from_le_bytes(bytes.get(8..10)?.try_into().ok()?);
            Some((w as u32, h as u32))
        }
        Format::Bmp => {
            let w = i32::from_le_bytes(bytes.get(18..22)?.try_into().ok()?);
            let h = i32::from_le_bytes(bytes.get(22..26)?.try_into().ok()?);
            Some((w.unsigned_abs(), h.unsigned_abs()))
        }
        Format::Jpeg => jpeg_dimensions(bytes),
        _ => None,
    }
}

/// Walk JPEG segments to the first Start-Of-Frame marker and read its size.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2; // skip SOI (FFD8)
    while i + 9 < bytes.len() {
        if bytes[i] != 0xff {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        // SOF0..SOF15 carry the frame size, except DHT(C4)/JPG(C8)/DAC(CC).
        if (0xc0..=0xcf).contains(&marker) && marker != 0xc4 && marker != 0xc8 && marker != 0xcc {
            let h = u16::from_be_bytes(bytes.get(i + 5..i + 7)?.try_into().ok()?);
            let w = u16::from_be_bytes(bytes.get(i + 7..i + 9)?.try_into().ok()?);
            return Some((w as u32, h as u32));
        }
        // Standalone markers (RSTn, SOI, EOI, TEM) have no length.
        if marker == 0xd8 || marker == 0xd9 || (0xd0..=0xd7).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes(bytes.get(i + 2..i + 4)?.try_into().ok()?) as usize;
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        v.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]); // rest of IHDR
        v
    }

    #[test]
    fn sniffs_formats_by_magic() {
        assert_eq!(sniff(&png(1, 1), None), Some(Format::Png));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0], None), Some(Format::Jpeg));
        assert_eq!(sniff(b"GIF89a....", None), Some(Format::Gif));
        assert_eq!(sniff(b"BM........", None), Some(Format::Bmp));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0; 4]);
        webp.extend_from_slice(b"WEBP");
        assert_eq!(sniff(&webp, None), Some(Format::Webp));
        assert_eq!(sniff(b"plain text, not an image", Some("notes.txt")), None);
    }

    #[test]
    fn ext_fallback_requires_binary() {
        // A text file misnamed .png is not treated as an image.
        assert!(!is_image(b"hello world\n", Some("a.png")));
        // Binary bytes named .heic (magic we don't parse) are.
        assert!(is_image(&[0u8, 1, 2, 3, 0, 0], Some("photo.heic")));
    }

    #[test]
    fn parses_dimensions() {
        assert_eq!(dimensions(&png(640, 480), Format::Png), Some((640, 480)));
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&320u16.to_le_bytes());
        gif.extend_from_slice(&200u16.to_le_bytes());
        assert_eq!(dimensions(&gif, Format::Gif), Some((320, 200)));
    }

    #[test]
    fn human_size_scales() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn iterm2_sequence_wraps_base64() {
        let seq = iterm2_sequence(&png(1, 1));
        assert!(seq.starts_with("\x1b]1337;File=inline=1;"));
        assert!(seq.contains("preserveAspectRatio=1"));
        assert!(seq.ends_with("\x07\n"));
    }

    #[test]
    fn oversize_image_shows_card_not_inline() {
        // Above the inline cap, even a PNG falls back to the info card.
        let mut big = png(1, 1);
        big.resize(MAX_INLINE_BYTES + 1, 0);
        let out = render(&big, Some("huge.png"), &Theme::plain(), 80);
        assert!(out.contains("too large"), "{out}");
        assert!(
            !out.contains("\x1b]1337"),
            "should not emit inline protocol"
        );
    }

    #[test]
    fn halfblocks_decode_and_render() {
        use agsh_style::{ColorLevel, Icons, Palette};
        // A real red 4×4 PNG, encoded via the image crate.
        let mut img = image::RgbImage::new(4, 4);
        for p in img.pixels_mut() {
            *p = image::Rgb([255, 0, 0]);
        }
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        let png_bytes = buf.into_inner();

        let theme = Theme {
            palette: Palette::dark(),
            level: ColorLevel::TrueColor,
            icons: Icons::disabled(),
        };
        let art = halfblocks(&png_bytes, &theme, 20).expect("decodes + renders");
        assert!(art.contains('\u{2580}'), "expected half-block glyph");
        assert!(
            art.contains("\x1b[38;2;255;0;0"),
            "expected red truecolor fg: {art:?}"
        );
        // Non-image bytes can't be decoded -> None (caller shows the info card).
        assert!(halfblocks(b"not an image", &theme, 20).is_none());
    }

    #[test]
    fn kitty_chunks_and_terminates() {
        let big = vec![0u8; 7000]; // > one 4096 base64 chunk
        let seq = kitty_png_sequence(&big);
        assert!(seq.starts_with("\x1b_Ga=T,f=100,m=1;"));
        assert!(seq.contains("\x1b_Gm=0;")); // final chunk
        assert!(seq.ends_with("\x1b\\\n"));
    }
}
