//! Splash — minimal cold-start screen.
//!
//! v5: dropped the wordmark, starfield, and tip callout. What's left is
//! a centred brand line, a model identifier, a single rule, and a
//! single-line input prompt. The user dismissed the splash by pressing
//! any key. Total height: 7 rows. That's deliberate — splash screens
//! that look like launchers are dated; a TUI's splash should look like
//! a quiet welcome, not a billboard.
//!
//! Layout (≥60×18):
//!
//! ```text
//!                  co_scientist                            (centered, teal)
//!                  model · sonnet                         (muted, small)
//!                  ─────────────────────────────────────── (rule)
//!                  │ > press any key to start…             (input)
//! ```
//!
//! Bottom-left: `~ 0.1.0`. No fake branding, no keybind wall.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::AppState;
use crate::theme;

pub fn draw(f: &mut Frame, area: Rect, state: &AppState) {
    if area.width < 40 || area.height < 10 {
        // Truly tiny terminal: render only the input prompt.
        let line = Line::from(vec![
            Span::styled("│ ", theme::accent()),
            Span::styled("> ", theme::accent()),
            Span::styled("press any key to start", theme::fg(theme::FG)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    // Vertical layout: top filler, 5 content rows, bottom filler.
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1), // brand
            Constraint::Length(1), // model
            Constraint::Length(1), // rule
            Constraint::Length(1), // input
            Constraint::Min(1),
        ])
        .split(area);

    draw_brand(f, v[1], area.width);
    // Only show the model line if we actually have one. Before the first
    // turn, `state.model` is empty — the brand line alone is the splash.
    if !state.model.is_empty() {
        draw_model(f, v[2], area.width, &state.model);
    }
    draw_rule(f, v[3], area.width);
    draw_input(f, v[4], area.width);

    draw_version_corner(f, area);
}

fn centre(width: u16, content_w: usize) -> u16 {
    width.saturating_sub(content_w as u16) / 2
}

fn draw_brand(f: &mut Frame, area: Rect, width: u16) {
    // The brand is "co_scientist" — the underscore gets the accent, the
    // rest is fg. No emoji, no all-caps, no block letters.
    let brand = "co_scientist";
    let pad = centre(width, brand.len()) as usize;
    let mut spans: Vec<Span> = Vec::new();
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    // Walk the string and split fg vs accent at the underscore.
    let mut current = String::new();
    let mut in_accent = false;
    for c in brand.chars() {
        let next_accent = c == '_';
        if next_accent != in_accent && !current.is_empty() {
            let style = if in_accent { theme::accent() } else { theme::bold(theme::FG) };
            spans.push(Span::styled(std::mem::take(&mut current), style));
        }
        current.push(c);
        in_accent = next_accent;
    }
    if !current.is_empty() {
        let style = if in_accent { theme::accent() } else { theme::bold(theme::FG) };
        spans.push(Span::styled(current, style));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_model(f: &mut Frame, area: Rect, width: u16, model: &str) {
    let line = format!("model · {model}");
    let pad = centre(width, line.len()) as usize;
    let mut spans = Vec::new();
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans.push(Span::styled(line, theme::italic(theme::MUTED)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_rule(f: &mut Frame, area: Rect, width: u16) {
    let pad = centre(width, 32) as usize;
    let mut spans = Vec::new();
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    // 32 chars of `─` in muted.
    spans.push(Span::styled("─".repeat(32), theme::dim()));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_input(f: &mut Frame, area: Rect, width: u16) {
    let prompt = "│ > press any key to start";
    let pad = centre(width, prompt.len()) as usize;
    let mut spans = Vec::new();
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans.push(Span::styled("│ ", theme::accent()));
    spans.push(Span::styled("> ", theme::accent()));
    spans.push(Span::styled("press any key to start", theme::fg(theme::FG)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_version_corner(f: &mut Frame, area: Rect) {
    let version = env!("CARGO_PKG_VERSION");
    let w = format!("~ {version}");
    if area.width < w.len() as u16 {
        return;
    }
    let cell = Rect::new(area.x, area.y + area.height - 1, w.len() as u16, 1);
    f.render_widget(
        Paragraph::new(Span::styled(w, theme::italic(theme::MUTED))),
        cell,
    );
}
