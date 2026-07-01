//! Glyphs for the UI. Nerd Font icons are opt-in (they render as boxes without
//! a Nerd Font), enabled via `AGSH_ICONS=1`; when disabled, icon accessors
//! return an empty string so layouts simply omit them. Plain-Unicode status
//! marks (✓ ✗ ⚠ …) are always available.

#[derive(Debug, Clone, Copy)]
pub struct Icons {
    pub enabled: bool,
}

impl Icons {
    pub fn detect() -> Self {
        let enabled = std::env::var_os("AGSH_ICONS")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false);
        Self { enabled }
    }

    pub fn disabled() -> Self {
        Self { enabled: false }
    }

    fn nf(&self, glyph: &'static str) -> &'static str {
        if self.enabled {
            glyph
        } else {
            ""
        }
    }

    /// Icon for a file, chosen by extension (Nerd Font; empty when disabled).
    pub fn file(&self, name: &str) -> &'static str {
        if !self.enabled {
            return "";
        }
        let ext = name.rsplit('.').next().unwrap_or("");
        match ext {
            "rs" => "\u{e7a8}", // rust
            "toml" | "ini" | "cfg" | "conf" => "\u{e615}",
            "md" | "markdown" => "\u{e73e}",
            "json" => "\u{e60b}",
            "yaml" | "yml" => "\u{e615}",
            "js" | "mjs" | "cjs" => "\u{e74e}",
            "ts" | "tsx" => "\u{e628}",
            "py" => "\u{e73c}",
            "go" => "\u{e627}",
            "sh" | "bash" | "zsh" | "fish" => "\u{e795}",
            "lock" => "\u{f023}",
            "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" => "\u{f1c5}",
            "zip" | "gz" | "tar" | "tgz" | "xz" | "zst" => "\u{f1c6}",
            "txt" | "log" => "\u{f15c}",
            _ => "\u{f15b}", // generic file
        }
    }

    pub fn dir(&self) -> &'static str {
        self.nf("\u{e5ff}") // folder
    }
    pub fn git_branch(&self) -> &'static str {
        self.nf("\u{e725}")
    }
    pub fn rust(&self) -> &'static str {
        self.nf("\u{e7a8}")
    }
    pub fn python(&self) -> &'static str {
        self.nf("\u{e73c}")
    }
    pub fn node(&self) -> &'static str {
        self.nf("\u{e718}")
    }
    pub fn docker(&self) -> &'static str {
        self.nf("\u{f308}")
    }
    pub fn kube(&self) -> &'static str {
        self.nf("\u{f10fe}")
    }
    pub fn aws(&self) -> &'static str {
        self.nf("\u{f0ef0}")
    }
    pub fn history(&self) -> &'static str {
        self.nf("\u{f1da}")
    }

    // Always-available plain-Unicode status marks.
    pub fn ok(&self) -> &'static str {
        "\u{2713}" // ✓
    }
    pub fn error(&self) -> &'static str {
        "\u{2717}" // ✗
    }
    pub fn warn(&self) -> &'static str {
        "\u{26a0}" // ⚠
    }
    pub fn ahead(&self) -> &'static str {
        "\u{21e1}" // ⇡
    }
    pub fn behind(&self) -> &'static str {
        "\u{21e3}" // ⇣
    }
    pub fn prompt(&self) -> &'static str {
        "\u{276f}" // ❯
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_icons_are_empty() {
        let i = Icons::disabled();
        assert_eq!(i.dir(), "");
        assert_eq!(i.file("main.rs"), "");
        // Status marks are always present.
        assert_eq!(i.ok(), "✓");
        assert_eq!(i.error(), "✗");
    }

    #[test]
    fn enabled_icons_present() {
        let i = Icons { enabled: true };
        assert!(!i.dir().is_empty());
        assert!(!i.file("x.rs").is_empty());
    }
}
