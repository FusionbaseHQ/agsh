//! A text style (foreground/background color + attributes) that renders to SGR.

use crate::color::{Color, ColorLevel};

#[derive(Debug, Clone, Copy, Default)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

impl Style {
    pub const fn new() -> Self {
        Self {
            fg: None,
            bg: None,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            reverse: false,
        }
    }

    pub const fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }
    pub const fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }
    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
    pub const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }
    pub const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }
    pub const fn underline(mut self) -> Self {
        self.underline = true;
        self
    }
    pub const fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }

    fn is_plain(&self) -> bool {
        self.fg.is_none()
            && self.bg.is_none()
            && !self.bold
            && !self.dim
            && !self.italic
            && !self.underline
            && !self.reverse
    }

    /// The SGR prefix (e.g. `\x1b[1;38;2;…m`) for this style at `level`, or empty
    /// when there's nothing to set.
    pub fn prefix(&self, level: ColorLevel) -> String {
        let mut params: Vec<String> = Vec::new();
        if self.bold {
            params.push("1".into());
        }
        if self.dim {
            params.push("2".into());
        }
        if self.italic {
            params.push("3".into());
        }
        if self.underline {
            params.push("4".into());
        }
        if self.reverse {
            params.push("7".into());
        }
        if level != ColorLevel::None {
            if let Some(fg) = self.fg.and_then(|c| c.sgr(level, true)) {
                params.push(fg);
            }
            if let Some(bg) = self.bg.and_then(|c| c.sgr(level, false)) {
                params.push(bg);
            }
        }
        if params.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", params.join(";"))
        }
    }

    /// Wrap `text` in this style's SGR codes. At [`ColorLevel::None`] (NO_COLOR /
    /// non-TTY) the text is returned completely unstyled — no color *and* no
    /// attributes — so piped output stays byte-clean.
    pub fn paint(&self, text: &str, level: ColorLevel) -> String {
        if self.is_plain() || level == ColorLevel::None {
            return text.to_string();
        }
        let prefix = self.prefix(level);
        if prefix.is_empty() {
            text.to_string()
        } else {
            format!("{prefix}{text}\x1b[0m")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_passthrough() {
        assert_eq!(Style::new().paint("x", ColorLevel::TrueColor), "x");
    }

    #[test]
    fn none_level_is_fully_plain() {
        // At None, neither color nor attributes render.
        let s = Style::new().bold().fg(Color::rgb(1, 2, 3));
        assert_eq!(s.paint("x", ColorLevel::None), "x");
    }

    #[test]
    fn attributes_apply_at_ansi() {
        let s = Style::new().bold();
        assert_eq!(s.paint("x", ColorLevel::Ansi16), "\x1b[1mx\x1b[0m");
    }

    #[test]
    fn color_suppressed_at_none_but_kept_at_truecolor() {
        let s = Style::new().fg(Color::rgb(1, 2, 3));
        assert_eq!(s.paint("x", ColorLevel::None), "x");
        assert_eq!(
            s.paint("x", ColorLevel::TrueColor),
            "\x1b[38;2;1;2;3mx\x1b[0m"
        );
    }
}
