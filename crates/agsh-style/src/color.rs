//! Terminal color with capability-aware downsampling.
//!
//! Colors are authored once as 24-bit RGB and rendered to whatever the terminal
//! supports: truecolor, 256-color, 16-color, or no color. The level is detected
//! from the environment (`NO_COLOR`, `CLICOLOR_FORCE`, `COLORTERM`, `TERM`).

/// How much color the terminal can render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorLevel {
    None,
    Ansi16,
    Ansi256,
    TrueColor,
}

impl ColorLevel {
    /// Detect the terminal's color capability from the environment. Returns
    /// [`ColorLevel::None`] when `NO_COLOR` is set or the terminal is `dumb`.
    pub fn detect() -> ColorLevel {
        if std::env::var_os("NO_COLOR").is_some() {
            return ColorLevel::None;
        }
        let force = std::env::var_os("CLICOLOR_FORCE")
            .map(|v| v != "0")
            .unwrap_or(false);
        let term = std::env::var("TERM").unwrap_or_default();
        if term == "dumb" && !force {
            return ColorLevel::None;
        }
        if let Ok(ct) = std::env::var("COLORTERM") {
            if ct.contains("truecolor") || ct.contains("24bit") {
                return ColorLevel::TrueColor;
            }
        }
        if term.contains("256color") {
            return ColorLevel::Ansi256;
        }
        if term.is_empty() && !force {
            // No terminal type and not forced: be conservative.
            return ColorLevel::Ansi16;
        }
        ColorLevel::Ansi16
    }
}

/// A 24-bit RGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parse `#rrggbb` (or `rrggbb`). Returns None on malformed input.
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.trim().strip_prefix('#').unwrap_or(s.trim());
        if s.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(Self { r, g, b })
    }

    /// The SGR parameter list for this color at `level` (e.g. `38;2;r;g;b`), or
    /// `None` when the level is `None`. `foreground` picks 38/3x vs 48/4x.
    pub fn sgr(&self, level: ColorLevel, foreground: bool) -> Option<String> {
        match level {
            ColorLevel::None => None,
            ColorLevel::TrueColor => {
                let lead = if foreground { 38 } else { 48 };
                Some(format!("{lead};2;{};{};{}", self.r, self.g, self.b))
            }
            ColorLevel::Ansi256 => {
                let lead = if foreground { 38 } else { 48 };
                Some(format!("{lead};5;{}", self.to_256()))
            }
            ColorLevel::Ansi16 => Some(self.to_16(foreground).to_string()),
        }
    }

    /// Nearest xterm-256 palette index.
    fn to_256(self) -> u8 {
        // Grayscale ramp for near-gray colors.
        let (r, g, b) = (self.r as i32, self.g as i32, self.b as i32);
        if (r - g).abs() < 8 && (g - b).abs() < 8 && (r - b).abs() < 8 {
            let gray = (r + g + b) / 3;
            if gray < 8 {
                return 16;
            }
            if gray > 248 {
                return 231;
            }
            return (232 + (gray - 8) * 24 / 240) as u8;
        }
        let q = |v: i32| -> i32 {
            // Map 0..255 to the 6-level cube (0,95,135,175,215,255).
            if v < 48 {
                0
            } else if v < 115 {
                1
            } else {
                (v - 35) / 40
            }
        };
        (16 + 36 * q(r) + 6 * q(g) + q(b)) as u8
    }

    /// Nearest of the 16 ANSI colors; returns the SGR code (3x/9x or 4x/10x).
    fn to_16(self, foreground: bool) -> u16 {
        const PALETTE: [(u8, u8, u8); 16] = [
            (0, 0, 0),
            (205, 0, 0),
            (0, 205, 0),
            (205, 205, 0),
            (0, 0, 238),
            (205, 0, 205),
            (0, 205, 205),
            (229, 229, 229),
            (127, 127, 127),
            (255, 0, 0),
            (0, 255, 0),
            (255, 255, 0),
            (92, 92, 255),
            (255, 0, 255),
            (0, 255, 255),
            (255, 255, 255),
        ];
        let mut best = 0usize;
        let mut best_d = i32::MAX;
        for (i, (r, g, b)) in PALETTE.iter().enumerate() {
            let d = (self.r as i32 - *r as i32).pow(2)
                + (self.g as i32 - *g as i32).pow(2)
                + (self.b as i32 - *b as i32).pow(2);
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        let (base_lo, base_hi) = if foreground { (30, 90) } else { (40, 100) };
        if best < 8 {
            (base_lo + best) as u16
        } else {
            (base_hi + (best - 8)) as u16
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing() {
        assert_eq!(
            Color::from_hex("#7aa2f7"),
            Some(Color::rgb(0x7a, 0xa2, 0xf7))
        );
        assert_eq!(
            Color::from_hex("9ece6a"),
            Some(Color::rgb(0x9e, 0xce, 0x6a))
        );
        assert_eq!(Color::from_hex("xyz"), None);
    }

    #[test]
    fn truecolor_sgr() {
        let c = Color::rgb(10, 20, 30);
        assert_eq!(c.sgr(ColorLevel::TrueColor, true).unwrap(), "38;2;10;20;30");
        assert_eq!(
            c.sgr(ColorLevel::TrueColor, false).unwrap(),
            "48;2;10;20;30"
        );
        assert_eq!(c.sgr(ColorLevel::None, true), None);
    }

    #[test]
    fn downsamples_pure_colors() {
        // Pure red maps to ANSI 16 bright/normal red foreground.
        let red = Color::rgb(255, 0, 0);
        let code = red.sgr(ColorLevel::Ansi16, true).unwrap();
        assert!(code == "31" || code == "91", "got {code}");
        // 256 path produces a 38;5;N sequence.
        assert!(red
            .sgr(ColorLevel::Ansi256, true)
            .unwrap()
            .starts_with("38;5;"));
    }
}
