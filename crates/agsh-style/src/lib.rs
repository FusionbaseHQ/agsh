//! Shared visual styling for agsh: capability-aware colors, a semantic theme,
//! and an opt-in icon set. Components style by *role* so the whole UI is
//! consistent and degrades to the terminal's color level (truecolor → 256 → 16 →
//! none), honoring `NO_COLOR`.

pub mod color;
pub mod icons;
pub mod shell;
pub mod style;
pub mod theme;

pub use color::{Color, ColorLevel};
pub use icons::Icons;
pub use shell::{highlight_shell, highlight_shell_without_resolution};
pub use style::Style;
pub use theme::{Palette, Role, Theme};
