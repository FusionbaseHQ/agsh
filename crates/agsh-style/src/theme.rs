//! Semantic roles mapped to styles via a palette, with capability detection and
//! optional `theme.toml` overrides. Components paint by *role* (e.g.
//! `Role::Command`), never by raw color, so the whole UI restyles from one place
//! and degrades to the terminal's color level.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::color::{Color, ColorLevel};
use crate::icons::Icons;
use crate::style::Style;

/// A named role a piece of text can play in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Accent,
    Ok,
    Warn,
    Error,
    Muted,
    Info,
    // Syntax highlighting.
    Command,
    CommandInvalid,
    Str,
    Var,
    Operator,
    Path,
    Flag,
    Comment,
    /// Source-code highlighting (the `view`/rich code renderer).
    Keyword,
    Number,
    Function,
    /// agsh output-mode keywords (`compact`, `semantic`, `raw`, `view`, …).
    ModeKeyword,
    // Completion menu.
    Selected,
    Match,
    Tag,
    Dir,
    File,
    Branch,
    History,
    // Rich rendering (markdown, diffs, tables).
    Heading,
    Emphasis,
    Code,
    Link,
    Added,
    Removed,
    Border,
}

/// The set of base colors a theme is built from.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub fg: Color,
    pub accent: Color,
    pub ok: Color,
    pub warn: Color,
    pub error: Color,
    pub muted: Color,
    pub info: Color,
    pub string: Color,
    pub path: Color,
    pub secondary: Color,
    /// Distinct accent for agsh mode keywords (orange).
    pub keyword: Color,
    pub selection_bg: Color,
}

impl Palette {
    /// A balanced dark palette (Tokyo-Night-ish).
    pub const fn dark() -> Self {
        Self {
            fg: Color::rgb(0xc0, 0xca, 0xf5),
            accent: Color::rgb(0x7a, 0xa2, 0xf7),
            ok: Color::rgb(0x9e, 0xce, 0x6a),
            warn: Color::rgb(0xe0, 0xaf, 0x68),
            error: Color::rgb(0xf7, 0x76, 0x8e),
            muted: Color::rgb(0x56, 0x5f, 0x89),
            info: Color::rgb(0x7d, 0xcf, 0xff),
            string: Color::rgb(0x9e, 0xce, 0x6a),
            path: Color::rgb(0x7d, 0xcf, 0xff),
            secondary: Color::rgb(0xbb, 0x9a, 0xf7),
            keyword: Color::rgb(0xff, 0x9e, 0x64),
            selection_bg: Color::rgb(0x2d, 0x3f, 0x76),
        }
    }

    /// A light palette for light terminals.
    pub const fn light() -> Self {
        Self {
            fg: Color::rgb(0x34, 0x3b, 0x58),
            accent: Color::rgb(0x2e, 0x7d, 0xe9),
            ok: Color::rgb(0x58, 0x7a, 0x0b),
            warn: Color::rgb(0x8c, 0x62, 0x00),
            error: Color::rgb(0xc6, 0x4d, 0x5b),
            muted: Color::rgb(0x9a, 0xa0, 0xb3),
            info: Color::rgb(0x07, 0x87, 0x9a),
            string: Color::rgb(0x58, 0x7a, 0x0b),
            path: Color::rgb(0x07, 0x87, 0x9a),
            secondary: Color::rgb(0x7a, 0x4d, 0xc6),
            keyword: Color::rgb(0xb5, 0x5a, 0x00),
            selection_bg: Color::rgb(0xcf, 0xdb, 0xf5),
        }
    }
}

/// A resolved theme: a palette + the terminal color level + icon set.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub palette: Palette,
    pub level: ColorLevel,
    pub icons: Icons,
}

impl Theme {
    /// Build a theme: detect color level (suppressed when `enabled` is false,
    /// e.g. non-TTY) and icons, apply any `theme.toml`, default to the dark
    /// palette.
    pub fn detect(enabled: bool) -> Theme {
        let level = if enabled {
            ColorLevel::detect()
        } else {
            ColorLevel::None
        };
        let mut palette = Palette::dark();
        if let Some(cfg) = ThemeConfig::load() {
            if cfg.variant.as_deref() == Some("light") {
                palette = Palette::light();
            }
            cfg.apply(&mut palette);
        }
        Theme {
            palette,
            level,
            icons: Icons::detect(),
        }
    }

    /// A no-color theme (icons still per environment), for plain output paths.
    pub fn plain() -> Theme {
        Theme {
            palette: Palette::dark(),
            level: ColorLevel::None,
            icons: Icons::disabled(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.level != ColorLevel::None
    }

    /// The style for a role.
    pub fn style(&self, role: Role) -> Style {
        let p = &self.palette;
        match role {
            Role::Accent => Style::new().fg(p.accent),
            Role::Ok => Style::new().fg(p.ok),
            Role::Warn => Style::new().fg(p.warn),
            Role::Error => Style::new().fg(p.error),
            Role::Muted => Style::new().fg(p.muted),
            Role::Info => Style::new().fg(p.info),
            Role::Command => Style::new().fg(p.ok).bold(),
            Role::CommandInvalid => Style::new().fg(p.error).bold(),
            Role::Str => Style::new().fg(p.string),
            Role::Var => Style::new().fg(p.info),
            Role::Operator => Style::new().fg(p.secondary),
            Role::Path => Style::new().fg(p.path),
            Role::Flag => Style::new().fg(p.muted),
            Role::Comment => Style::new().fg(p.muted).italic(),
            Role::Keyword => Style::new().fg(p.secondary).bold(),
            Role::Number => Style::new().fg(p.keyword),
            Role::Function => Style::new().fg(p.accent),
            Role::ModeKeyword => Style::new().fg(p.keyword).bold(),
            Role::Selected => Style::new().fg(p.fg).bg(p.selection_bg).bold(),
            Role::Match => Style::new().fg(p.accent).bold(),
            Role::Tag => Style::new().fg(p.muted),
            Role::Dir => Style::new().fg(p.accent).bold(),
            Role::File => Style::new().fg(p.fg),
            Role::Branch => Style::new().fg(p.secondary),
            Role::History => Style::new().fg(p.muted),
            Role::Heading => Style::new().fg(p.accent).bold(),
            Role::Emphasis => Style::new().fg(p.fg).italic(),
            Role::Code => Style::new().fg(p.string),
            Role::Link => Style::new().fg(p.info).underline(),
            Role::Added => Style::new().fg(p.ok),
            Role::Removed => Style::new().fg(p.error),
            Role::Border => Style::new().fg(p.muted),
        }
    }

    /// Paint `text` in a role's style at the theme's color level.
    pub fn paint(&self, role: Role, text: &str) -> String {
        self.style(role).paint(text, self.level)
    }

    /// A GNU `LS_COLORS` string derived from this theme (truecolor), so the real
    /// `ls --color` colors directories, symlinks, and executables to match agsh.
    pub fn ls_colors(&self) -> String {
        let p = &self.palette;
        let fg = |c: Color| c.sgr(ColorLevel::TrueColor, true).unwrap_or_default();
        [
            format!("di=01;{}", fg(p.accent)), // directory (bold)
            format!("ln=01;{}", fg(p.path)),   // symlink
            format!("ex=01;{}", fg(p.ok)),     // executable
            format!("fi={}", fg(p.fg)),        // regular file
            format!("so={}", fg(p.secondary)), // socket
            format!("pi={}", fg(p.warn)),      // fifo / pipe
            format!("bd={}", fg(p.warn)),      // block device
            format!("cd={}", fg(p.warn)),      // char device
            format!("su=01;{}", fg(p.error)),  // setuid
            format!("sg=01;{}", fg(p.error)),  // setgid
            format!("tw=01;{}", fg(p.accent)), // sticky, other-writable dir
            format!("ow=01;{}", fg(p.accent)), // other-writable dir
        ]
        .join(":")
    }

    /// A BSD/macOS `LSCOLORS` string (11 foreground/background letter pairs). BSD
    /// `ls` has only the 8 base hues (a capital = bold) and no truecolor, so the
    /// theme's intent is mapped *semantically* rather than by nearest-RGB (which
    /// turns pastel palettes muddy): bold-blue directories, bold-cyan symlinks,
    /// bold-magenta sockets, brown pipes, bold-green executables — matching the
    /// `Dir`/`Path`/`Command` role intents. The rest keep the conventional
    /// defaults.
    pub fn bsd_lscolors(&self) -> String {
        // dir | link | socket | pipe | exec | (defaults: block char setuid setgid sticky ow)
        "ExGxFxdxCxegedabagacad".to_string()
    }
}

/// `theme.toml` overrides (all optional).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ThemeConfig {
    variant: Option<String>,
    palette: PaletteConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PaletteConfig {
    fg: Option<String>,
    accent: Option<String>,
    ok: Option<String>,
    warn: Option<String>,
    error: Option<String>,
    muted: Option<String>,
    info: Option<String>,
    string: Option<String>,
    path: Option<String>,
    secondary: Option<String>,
    keyword: Option<String>,
    selection_bg: Option<String>,
}

impl ThemeConfig {
    fn load() -> Option<ThemeConfig> {
        let text = std::fs::read_to_string(theme_path()?).ok()?;
        toml::from_str(&text).ok()
    }

    fn apply(&self, palette: &mut Palette) {
        let p = &self.palette;
        let set = |slot: &mut Color, hex: &Option<String>| {
            if let Some(c) = hex.as_deref().and_then(Color::from_hex) {
                *slot = c;
            }
        };
        set(&mut palette.fg, &p.fg);
        set(&mut palette.accent, &p.accent);
        set(&mut palette.ok, &p.ok);
        set(&mut palette.warn, &p.warn);
        set(&mut palette.error, &p.error);
        set(&mut palette.muted, &p.muted);
        set(&mut palette.info, &p.info);
        set(&mut palette.string, &p.string);
        set(&mut palette.path, &p.path);
        set(&mut palette.secondary, &p.secondary);
        set(&mut palette.keyword, &p.keyword);
        set(&mut palette.selection_bg, &p.selection_bg);
    }
}

fn theme_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("AGSH_THEME_FILE") {
        return Some(PathBuf::from(p));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(Path::new(&xdg).join("agsh/theme.toml"));
    }
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".config/agsh/theme.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_theme_paints_nothing() {
        let t = Theme::plain();
        assert_eq!(t.paint(Role::Command, "ls"), "ls");
        assert!(!t.enabled());
    }

    #[test]
    fn ls_colors_match_palette() {
        let t = Theme {
            palette: Palette::dark(),
            level: ColorLevel::TrueColor,
            icons: Icons::disabled(),
        };
        let ls = t.ls_colors();
        // Directory uses the bold accent (#7aa2f7 = 122,162,247) as truecolor.
        assert!(ls.contains("di=01;38;2;122;162;247"), "{ls}");
        assert!(ls.contains("ex=01;")); // executables bold
        assert!(ls.contains("ln=")); // symlinks colored
                                     // BSD form: 11 letter pairs, bold-blue dir, bold-green exec.
        let bsd = t.bsd_lscolors();
        assert_eq!(bsd.len(), 22);
        assert!(bsd.starts_with("Ex")); // dir = bold blue
        assert_eq!(&bsd[8..10], "Cx"); // exec = bold green
    }

    #[test]
    fn truecolor_theme_paints_role() {
        let t = Theme {
            palette: Palette::dark(),
            level: ColorLevel::TrueColor,
            icons: Icons::disabled(),
        };
        let painted = t.paint(Role::Error, "boom");
        assert!(painted.starts_with("\x1b[38;2;"));
        assert!(painted.ends_with("\x1b[0m"));
        assert!(painted.contains("boom"));
    }

    #[test]
    fn config_overrides_palette() {
        let toml = r##"
variant = "dark"
[palette]
accent = "#010203"
"##;
        let cfg: ThemeConfig = toml::from_str(toml).unwrap();
        let mut p = Palette::dark();
        cfg.apply(&mut p);
        assert_eq!(p.accent, Color::rgb(1, 2, 3));
    }
}
