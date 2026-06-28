//! Calm-terminal theme — cool monochrome with a single soft teal accent.
//!
//! v5 redesign. Replaces the orange-on-dark MiMo-faithful palette with
//! something closer to charm/atuin/gum: near-black cool bg, soft white fg,
//! one accent used sparingly for focus and emphasis.
//!
//! Rules:
//! - The accent (`teal`) is used for: focused panel border, brand wordmark,
//!   idle status, keybinding hints in the footer. Nowhere else.
//! - Actor colors are GONE. Agents are distinguished by glyph + position.
//! - Status colors (`success`/`warning`/`error`) stay because they carry
//!   semantic meaning, but they're muted — not the saturated
//!   green/yellow/red of v4.
//!
//! v6.1 additions: `accent2` (warm peach) and `section` (cool steel).
//! These are the only allowed "second color" usage — `accent2` marks
//! inline code + fenced code body (warm reads as "different medium");
//! `section` marks structural chrome (rules, fence headers, links). They
//! never appear together on the same span, so the palette stays at
//! "one accent + chrome" rather than escalating to three-way rainbow.

use ratatui::style::{Color, Modifier, Style};

#[allow(dead_code)]
pub const BG: Color = Color::Rgb(0x0e, 0x11, 0x16);
#[allow(dead_code)]
pub const BG_SURFACE: Color = Color::Rgb(0x14, 0x18, 0x1f);
#[allow(dead_code)]
pub const BG_OVERLAY: Color = Color::Rgb(0x1b, 0x20, 0x29);
pub const FG: Color = Color::Rgb(0xdc, 0xe0, 0xe8);
pub const MUTED: Color = Color::Rgb(0x5c, 0x63, 0x70);
pub const EMPHASIS: Color = Color::Rgb(0xff, 0xff, 0xff);
pub const TEAL: Color = Color::Rgb(0x5f, 0xb3, 0xb3);
#[allow(dead_code)]
pub const TEAL_DIM: Color = Color::Rgb(0x3a, 0x6e, 0x6e);
pub const SUCCESS: Color = Color::Rgb(0x7a, 0xc8, 0xa0);
pub const WARNING: Color = Color::Rgb(0xd9, 0xb3, 0x6a);
pub const ERROR: Color = Color::Rgb(0xc8, 0x6a, 0x6a);
/// Warm peach — used on code blocks (inline + fenced). Warm hue reads
/// as "different medium" without competing with the teal accent.
pub const ACCENT2: Color = Color::Rgb(0xd9, 0xa3, 0x73);
/// Cool steel — used on structural chrome (horizontal rules, code-fence
/// headers, link underlines). Separates "scaffolding" from "metadata"
/// since both were previously `MUTED` and indistinguishable.
pub const SECTION: Color = Color::Rgb(0x7a, 0x8f, 0x9f);
/// Cool blue — used for markdown content accents: H1 headings, H2/H3
/// headings, and bold inline text. Distinct from TEAL (chrome accent)
/// and from the body grey family. Calibrated to read as a heading
/// without competing with the teal focus border.
pub const BLUE: Color = Color::Rgb(0x7a, 0xb8, 0xff);

pub fn bold(c: Color) -> Style {
    Style::default().fg(c).add_modifier(Modifier::BOLD)
}
pub fn fg(c: Color) -> Style {
    Style::default().fg(c)
}
pub fn italic(c: Color) -> Style {
    Style::default().fg(c).add_modifier(Modifier::ITALIC)
}
pub fn dim() -> Style {
    Style::default().fg(MUTED)
}
pub fn accent() -> Style {
    Style::default().fg(TEAL)
}
pub fn section() -> Style {
    Style::default().fg(SECTION)
}
