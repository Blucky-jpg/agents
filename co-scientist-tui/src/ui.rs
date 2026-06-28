//! Ratatui draw — calm-terminal v5.
//!
//! ## Visual identity
//!
//! Cool monochrome (near-black bg with a hint of blue) + one soft teal
//! accent. The accent appears in exactly four places:
//!
//! 1. The border of the focused panel.
//! 2. The brand glyph (`_`) in the splash wordmark.
//! 3. The idle status indicator.
//! 4. The keystroke names in the footer hints.
//!
//! Anywhere else, the accent is wrong. The status colors (success/warning/
//! error) are reserved for actual semantic states.
//!
//! ## Layout
//!
//! ```text
//!   co_scientist · model · sonnet · idle                         ~ 0.1.0
//!   ─────────────────────────────────────────────────────────────────
//!   ╭ agents ──────╮ ╭ chat ────────────────╮ ╭ tasks ────────────╮
//!   │ ▶ supervisor │ │ │ you › draft a plan  │ │ ✓ t1 worker w1    │
//!   │   generation │ │ │ gen  ‹ here's my    │ │ ▶ t2 worker w2    │
//!   │   reflection │ │ │       answer…       │ │ ────────────────  │
//!   │              │ │ │                     │ │ + plan: outline   │
//!   │              │ │ │                     │ │ * generation: p…  │
//!   ╰──────────────╯ ╰─────────────────────╯ ╰───────────────────╯
//!   ╭ input ─────────────────────────────────────────────────────╮
//!   │ │ > draft a research plan_                                  │
//!   ╰─────────────────────────────────────────────────────────────╯
//!     focus: input · Enter send · / command · Tab focus · ? help · Ctrl-C quit
//! ```
//!
//! ### Breakpoints
//!
//! - ≥100 cols: 3 columns (agents | chat | sidebar).
//! - 60-99 cols: 2 columns (chat | sidebar).
//! - <60 cols OR <20 rows: "terminal too small" gate.
//!
//! ### Focus
//!
//! `Tab`/`Shift+Tab` cycles: `Input → Chat → Agents → Sidebar`.
//! Focused panel: teal border + subtle bg.surface fill.
//! Unfocused: muted border + bg default fill.
//!
//! ### Chrome
//!
//! - All borders use **rounded** corners (`╭╮╰╯`).
//! - No `│` wall on every chat line — the gutter is a single accent
//!   vertical line down the left side, with each message indented by
//!   one space next to it.
//! - Status bar is full-width, height 1, **bottom-bordered** rather than
//!   boxed.
//! - Sidebar sub-panels (tasks + memory) are separated by a single
//!   horizontal `─` rule, not nested borders.



use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::Frame;

use crate::app::{AppState, Busy, ChatMsg, Focus, MemoryEntry, TaskEntry, TaskStatus};
use crate::splash;
use crate::theme;

const SPINNER: &[char] = &['·', '·', '·', '✦', '·', '·', '✧', '·'];
const SPINNER_FRAMES: usize = SPINNER.len();

/// Braille spinner — used in the footer right-side brand tag to indicate
/// "agent working" / "supervisor running". 10 frames at 100ms tick = 10 FPS,
/// which is the sweet spot from the tui-design playbook (smooth without
/// flicker). Different glyph from the decorative `SPINNER` above because
/// this one is doing real work signalling.
const BUSY_SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const BUSY_SPINNER_FRAMES: usize = BUSY_SPINNER.len();

const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 20;

/// Width of the rounded-corner border style. Stored so multiple sites agree.
const ROUNDED: Borders = Borders::ALL;
// `ratatui::widgets::Borders` doesn't carry a corner-style flag — corners
// are always rounded in v0.29's default border set when both left+right
// and top+bottom are set. We use Borders::ALL and trust the default.

pub fn draw(f: &mut Frame, state: &mut AppState) {
    let area = f.area();

    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        draw_too_small(f, area, state);
        return;
    }

    if state.show_help {
        draw_main(f, state);
        draw_help_overlay(f, area);
        return;
    }

    if state.show_splash {
        splash::draw(f, area, state);
        return;
    }

    draw_main(f, state);
}

fn draw_too_small(f: &mut Frame, area: Rect, state: &mut AppState) {
    let block = Block::default()
        .borders(ROUNDED)
        .border_style(dim_border())
        .title(Span::styled(" co_scientist ", accent_bold()));
    let mut text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "terminal too small: {}×{} (need {}×{})",
                area.width, area.height, MIN_WIDTH, MIN_HEIGHT
            ),
            Style::default().fg(theme::WARNING),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "resize the window and the UI will recover.",
            italic_muted(),
        )),
        Line::from(""),
    ];
    if !state.model.is_empty() {
        text.push(Line::from(Span::styled(
            format!("model · {}", state.model),
            italic_muted(),
        )));
    }
    f.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: false }), area);
}

// -- main layout -----------------------------------------------------------

fn draw_main(f: &mut Frame, state: &mut AppState) {
    let area = f.area();

    // Vertical: status (1) | body (min) | input (3) | footer (1).
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    draw_status(f, v[0], state);
    draw_body(f, v[1], state);
    draw_input(f, v[2], state);
    draw_footer(f, v[3], state);
}

fn draw_body(f: &mut Frame, area: Rect, state: &mut AppState) {
    if area.width >= 100 {
        let h = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(18),
                Constraint::Min(20),
                Constraint::Length(34),
            ])
            .split(area);
        draw_agents(f, h[0], state);
        draw_chat(f, h[1], state);
        draw_sidebar(f, h[2], state);
    } else {
        let h = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(20), Constraint::Length(34)])
            .split(area);
        draw_chat(f, h[0], state);
        draw_sidebar(f, h[1], state);
    }
}

// -- status bar ------------------------------------------------------------

fn draw_status(f: &mut Frame, area: Rect, state: &mut AppState) {
    let spinner = SPINNER[(state.tick as usize) % SPINNER_FRAMES];
    let busy_color = match state.busy {
        Busy::Idle => theme::TEAL,
        Busy::Running => theme::WARNING,
    };
    let busy_text = match state.busy {
        Busy::Idle => "idle".to_string(),
        Busy::Running => format!("{spinner} running"),
    };

    // Brand on the far left, model next, run id in muted italic, busy on right.
    let mut left_spans = vec![
        Span::styled("co", Style::default().fg(theme::FG).add_modifier(Modifier::BOLD)),
        Span::styled("_", theme::accent()),
        Span::styled("scientist", Style::default().fg(theme::FG).add_modifier(Modifier::BOLD)),
        Span::styled("  ·  ", theme::dim()),
    ];
    if !state.model.is_empty() {
        left_spans.push(Span::styled(state.model.clone(), italic_muted()));
        left_spans.push(Span::styled("  ·  ", theme::dim()));
    }
    left_spans.push(Span::styled(short_id(&state.run_id), italic_muted()));
    left_spans.push(Span::styled("  ·  ", theme::dim()));
    left_spans.push(Span::styled(
        state.current_agent_name(),
        Style::default().fg(theme::FG),
    ));
    left_spans.push(Span::styled("  ·  ", theme::dim()));
    left_spans.push(Span::styled(busy_text, Style::default().fg(busy_color)));
    let left = Line::from(left_spans);

    // Bottom border separates status from body. Thin line across the row below.
    // ratatui doesn't draw a border on a Paragraph directly; we render the line
    // and let the body widgets paint over it. Instead we use a 1-row paragraph
    // with no border but the body widgets have their own top rules via their
    // border styles. To get a single hairline under the status: use a Block
    // with Borders::BOTTOM only.
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(dim_border());
    f.render_widget(Paragraph::new(left).block(block), area);

    if state.supervisor_running {
        // Second status line is rendered as part of the chat header so we
        // don't fight the 1-row constraint above. The chat panel can show
        // "session=… elapsed=… done/failed" in its title bar instead.
        // See draw_chat's title composition.
    }
}

// -- agents panel ----------------------------------------------------------

fn draw_agents(f: &mut Frame, area: Rect, state: &mut AppState) {
    use co_scientist::agents::AGENTS;
    let focused = state.focus == Focus::Agents;

    let block = panel_block("agents", focused, Some(state.agent_idx + 1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::with_capacity(AGENTS.len() + 2);
    for (i, agent) in AGENTS.iter().enumerate() {
        let is_current = i == state.agent_idx;
        // Marker: a left rail of ` ` for non-current, accent `›` for current.
        // We avoid color for actor identity (v5 rule: no rainbow actors).
        let marker = if is_current { "›" } else { " " };
        let marker_style = if is_current && focused {
            theme::accent()
        } else if is_current {
            Style::default().fg(theme::FG).add_modifier(Modifier::BOLD)
        } else {
            theme::dim()
        };
        let name_style = if is_current {
            Style::default().fg(theme::EMPHASIS).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::FG)
        };
        let role_style = italic_muted();
        let role = role_one_liner(agent.name);
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} "), marker_style),
            Span::styled(agent.name.to_string(), name_style),
        ]));
        // Only show the role hint if we have vertical room.
        if (inner.height as usize) > AGENTS.len() + 1 {
            lines.push(Line::from(Span::styled(format!("   {role}"), role_style)));
        }
        lines.push(Line::from(""));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn role_one_liner(name: &str) -> &'static str {
    match name {
        "supervisor" => "plan + dispatch",
        "generation" => "novel hypotheses",
        "reflection" => "review + verify",
        "ranking" => "pairwise ELO",
        "evolution" => "combine + simplify",
        "metareview" => "synthesis + verdict",
        _ => "",
    }
}

// -- chat panel ------------------------------------------------------------

/// Derived viewport metrics for the chat panel. Computed once per frame
/// from the rendered lines + inner area, then read by both the draw path
/// (for scroll position + scrollbar math) and by input handlers in
/// `main.rs` (for PageUp/PageDown unit and scroll clamping). Returning
/// a small struct instead of recomputing everywhere keeps scroll math
/// consistent — a u16 `chat_scroll` that gets clamped against the wrong
/// `max_scroll` is the #1 cause of "stuck below the viewport" bugs.
#[derive(Debug, Clone, Copy)]
pub struct ChatMetrics {
    /// Total wrapped-line count of the rendered chat content.
    pub total: usize,
    /// Visible row count of the chat inner area.
    pub visible_h: usize,
    /// Maximum legal `chat_scroll` value (`total - visible_h`, saturating).
    pub max_scroll: usize,
}

/// Compute the chat viewport metrics. `lines` is the already-rendered
/// `Vec<Line>` from `render_chat_lines`; `inner` is the chat panel's
/// inner `Rect` after border is subtracted.
pub fn compute_chat_metrics(lines: &[Line], inner: Rect) -> ChatMetrics {
    let visible_w = inner.width.max(1) as usize;
    let total: usize = lines
        .iter()
        .map(|l| {
            let w = l.width();
            if w == 0 { 1 } else { w.div_ceil(visible_w) }
        })
        .sum();
    let visible_h = inner.height as usize;
    let max_scroll = total.saturating_sub(visible_h);
    ChatMetrics {
        total,
        visible_h,
        max_scroll,
    }
}

/// Pick the effective scroll position for this frame. When `follow_tail`
/// is on, anchor to the bottom regardless of `chat_scroll`. Otherwise
/// clamp the stored `chat_scroll` against the freshly computed
/// `max_scroll` so out-of-range values (e.g. after a resize) snap back
/// to a legal position.
pub fn pick_chat_scroll(state: &AppState, metrics: ChatMetrics) -> u16 {
    if state.follow_tail {
        metrics.max_scroll as u16
    } else {
        state.chat_scroll.min(metrics.max_scroll as u16)
    }
}

fn draw_chat(f: &mut Frame, area: Rect, state: &mut AppState) {
    let focused = state.focus == Focus::Chat;

    // Title varies: when supervisor is running, prepend a status line.
    let title = if state.supervisor_running {
        let elapsed = state
            .supervisor_started_at
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        let done = state.tasks_done;
        let failed = state.tasks_failed;
        let bar = mini_bar(done + failed, 20);
        format!(" chat · sup {bar} {done}✓ {failed}✗ {elapsed}s ")
    } else {
        " chat ".to_string()
    };
    let block = panel_block_raw(&title, focused);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines = render_chat_lines(state);
    let metrics = compute_chat_metrics(&lines, inner);
    // Publish viewport metrics back onto AppState so input handlers
    // (PageUp/PageDown unit, scroll clamping) see the fresh values
    // before the next key event is dispatched. Without this, the very
    // first `j` after a window resize would scroll by the OLD visible
    // height — a subtle off-by-screen bug.
    state.chat_max_scroll = metrics.max_scroll as u16;
    state.chat_visible_h = metrics.visible_h as u16;
    let scroll = pick_chat_scroll(state, metrics);

    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(p, inner);

    // Scrollbar on the right.
    let thumb_style = if focused {
        Style::default().fg(theme::TEAL)
    } else {
        theme::dim()
    };
    let mut sb_state = ScrollbarState::new(metrics.total.max(1))
        .position(scroll as usize)
        .viewport_content_length(metrics.visible_h);
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .thumb_style(thumb_style)
            .track_style(Style::default().fg(theme::BG_SURFACE)),
        inner,
        &mut sb_state,
    );
}

// -- markdown rendering -----------------------------------------------------

/// Render a markdown string into `Line<'static>` values using a small
/// hand-rolled pulldown-cmark walker.
///
/// Why hand-rolled and not `tui-markdown`: the crate hardcodes a neon
/// palette (yellow headings, cyan code, magenta blockquotes) that
/// violates the calm-terminal one-accent rule. The crate's `StyleSheet`
/// trait only exists from 0.3.6+ and requires ratatui 0.30 — out of
/// scope. Our renderer reads the event stream directly and maps each
/// element to a `Span` styled from `crate::theme`, so headings pick up
/// the teal accent and body text stays in `theme::FG`.
///
/// Each returned `Vec<Span>` carries a leading "indent" span (one of
/// `INDENT_PARAGRAPH`, `INDENT_HEADING`, etc., all-space) so the caller
/// can drop the outer body indent without re-counting columns. This
/// keeps the walker self-describing — the visual rhythm is encoded in
/// the output, not reconstructed by the caller.
///
/// Visual rhythm (v6 redesign — the v5 every-block-gets-a-blank
/// approach made long responses feel uniform and rhythm-fatiguing):
/// - Headings: `▌` accent bar at column 0, blank above and below.
/// - Paragraphs: 3-space indent, no surrounding blanks.
/// - Bulleted items: 5-space indent + `›`/`·`/`∘` rotating markers
///   in teal, no surrounding blanks.
/// - Ordered items: 5-space indent + `n.` marker in muted.
/// - Code fences: 3-space indent + `━━` thick rule on the lang header
///   so the fence reads as a single chunk, distinct from `---` rules.
/// - Blockquotes: 2-space indent + `╎` broken-bar gutter (U+254E) in
///   muted italic — different from `│` so it can't be confused with the
///   chat-message gutter.
/// - Horizontal rules: single `───` in muted, no surrounding blanks.
///
/// What's covered (everything an LLM typically emits):
/// - `# ## ###` etc. headings (accent bar + bold)
/// - body paragraphs
/// - `-` / `*` / `+` bullet lists (rotated `›`/`·`/`∘` markers)
/// - `1.` ordered lists
/// - `**bold**` and `*italic*`
/// - `` `inline code` `` with bg.overlay highlight
/// - fenced code blocks (bg.overlay bg, every line)
/// - `> ` blockquotes (italic + muted, broken-bar gutter)
/// - `---` horizontal rules
/// - `[text](url)` links (text shown; URL dropped)
///
/// Streaming caveat: the LLM streams token-by-token, so mid-paragraph
/// markdown may be malformed (unclosed `**`, half-typed `>`). pulldown-
/// cmark tolerates this gracefully — unclosed emphasis just renders as
/// literal asterisks. At 10 FPS redraw the user doesn't see the
/// transition when a half-formed list finally gets its `\n`.
fn render_markdown(input: &str) -> Vec<Vec<Span<'static>>> {
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);

    let parser = Parser::new_ext(input, options);

    // Block-level state. We collect spans into the current "block" and
    // flush to `lines` when the block ends. One block = one or more
    // rendered `Line`s.
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();

    // Inline style stack. Each frame in `inline_stack` is a style that
    // should apply to subsequent text events. Push on opening tags
    // (Strong, Emphasis, Code, Link), pop on closing.
    let mut inline_stack: Vec<Style> = Vec::new();

    // List-item marker we're currently building. None = we're not inside
    // a list. For ordered lists we also track the current item index.
    let mut list_kind: Option<ListKind> = None;
    let mut list_counter: u64 = 1;
    let mut unordered_index: usize = 0;
    enum ListKind {
        Unordered,
        Ordered,
    }
    // Whether the current block is a code block (so we render every line
    // with the code style, no inline parsing).
    let mut in_code_block = false;
    // Blockquote gutter character prefix. Set when inside a `>` block.
    let mut blockquote_depth: u32 = 0;

    // The leading indent span we'll prepend to the next non-empty line.
    // Defaults to paragraph indent. Heading / list / code / blockquote
    // opens update this; closes restore the prior value.
    let mut current_indent = INDENT_PARAGRAPH;
    let mut indent_stack: Vec<&'static str> = Vec::new();

    // Flush helper: append `current` to `lines` if non-empty, then reset.
    macro_rules! flush {
        () => {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
        };
    }

    for event in parser {
        match event {
            // -- block-level open --
            Event::Start(Tag::Heading { level, .. }) => {
                flush!();
                // Headings are the ONE block that reserves vertical air —
                // a blank above gives the eye a breath, the ▌ accent bar
                // gives structural anchoring. Other block transitions
                // stay tight so long responses don't drown in whitespace.
                if !lines.last().map(|l| l.is_empty()).unwrap_or(true) {
                    lines.push(Vec::new());
                }
                indent_stack.push(current_indent);
                current_indent = INDENT_HEADING;
                current.push(Span::styled("▌ ", heading_style(level)));
            }
            Event::End(TagEnd::Heading(_)) => {
                flush!();
                if let Some(prev) = indent_stack.pop() {
                    current_indent = prev;
                }
                lines.push(Vec::new());
            }
            Event::Start(Tag::Paragraph) => {
                flush!();
                indent_stack.push(current_indent);
                current_indent = INDENT_PARAGRAPH;
                // No blank before paragraphs — v5 had one here, but the
                // cumulative effect was every block getting a blank,
                // which made responses feel metronomic. The 3-space
                // indent plus the natural paragraph break is enough.
            }
            Event::End(TagEnd::Paragraph) => {
                flush!();
                if let Some(prev) = indent_stack.pop() {
                    current_indent = prev;
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                flush!();
                blockquote_depth += 1;
                indent_stack.push(current_indent);
                current_indent = INDENT_BLOCKQUOTE;
                // Pre-seed the current line with the broken-bar gutter
                // so the first line of the quote reads as quoted
                // immediately. Subsequent lines pick up the indent span
                // only (the gutter is a per-block visual marker, not a
                // per-line prefix).
                current.push(Span::styled("╎ ", blockquote_style()));
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush!();
                blockquote_depth = blockquote_depth.saturating_sub(1);
                if let Some(prev) = indent_stack.pop() {
                    current_indent = prev;
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                flush!();
                indent_stack.push(current_indent);
                current_indent = INDENT_CODE;
                in_code_block = true;
                // Fenced code blocks get a thick `━━` rule on the lang
                // header so they read as one chunk and don't get
                // confused with `---` horizontal rules. v6.1: rule is
                // SECTION (steel) instead of MUTED so structural chrome
                // is visually distinct from helper-text metadata.
                if let CodeBlockKind::Fenced(lang) = kind {
                    if !lang.is_empty() {
                        current.push(Span::styled(
                            format!("━━ {} ", lang),
                            theme::section(),
                        ));
                        lines.push(std::mem::take(&mut current));
                    } else {
                        current.push(Span::styled("━━", theme::section()));
                    }
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                flush!();
                in_code_block = false;
                if let Some(prev) = indent_stack.pop() {
                    current_indent = prev;
                }
                lines.push(Vec::new());
            }
            Event::Start(Tag::List(start)) => {
                flush!();
                list_kind = Some(if start.is_some() {
                    ListKind::Ordered
                } else {
                    ListKind::Unordered
                });
                list_counter = start.unwrap_or(1);
                unordered_index = 0;
                indent_stack.push(current_indent);
                current_indent = INDENT_LIST;
            }
            Event::End(TagEnd::List(_)) => {
                flush!();
                list_kind = None;
                if let Some(prev) = indent_stack.pop() {
                    current_indent = prev;
                }
            }
            Event::Start(Tag::Item) => {
                flush!();
                match list_kind {
                    Some(ListKind::Unordered) => {
                        // Rotate marker per item so a long bulleted list
                        // reads as a progression rather than `› › › ›`.
                        // Three glyphs cycle; resets when a new list
                        // starts. Stays in teal so the accent is visible
                        // but not noisy.
                        let marker = match unordered_index % 3 {
                            0 => "›",
                            1 => "·",
                            _ => "∘",
                        };
                        current.push(Span::styled(
                            format!("{marker} "),
                            Style::default().fg(theme::TEAL),
                        ));
                        unordered_index += 1;
                    }
                    Some(ListKind::Ordered) => {
                        current.push(Span::styled(
                            format!("{}. ", list_counter),
                            Style::default().fg(theme::MUTED),
                        ));
                        list_counter += 1;
                    }
                    None => {}
                }
            }
            Event::End(TagEnd::Item) => {
                flush!();
            }
            Event::Start(Tag::Strong) => {
                inline_stack.push(Style::default().fg(theme::EMPHASIS).add_modifier(Modifier::BOLD));
            }
            Event::End(TagEnd::Strong) => {
                inline_stack.pop();
            }
            Event::Start(Tag::Emphasis) => {
                inline_stack.push(
                    Style::default()
                        .fg(theme::FG)
                        .add_modifier(Modifier::ITALIC),
                );
            }
            Event::End(TagEnd::Emphasis) => {
                inline_stack.pop();
            }
            Event::Start(Tag::Strikethrough) => {
                inline_stack.push(
                    Style::default()
                        .fg(theme::MUTED)
                        .add_modifier(Modifier::CROSSED_OUT),
                );
            }
            Event::End(TagEnd::Strikethrough) => {
                inline_stack.pop();
            }
            Event::Start(Tag::Link { .. }) => {
                // v6.1: link uses SECTION (steel) instead of TEAL — teal
                // was visually indistinguishable from the heading accent
                // bar at a glance, and links share the underline attribute
                // with bold-emphasis in some terminals. Steel + underline
                // is unique to links.
                inline_stack.push(
                    Style::default()
                        .fg(theme::SECTION)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Event::End(TagEnd::Link) => {
                inline_stack.pop();
            }
            Event::Code(text) => {
                // Inline code span. Warm peach (ACCENT2) on BG_OVERLAY so
                // the run reads as "different medium" — the hue shift is
                // what your eye catches first, the bg tint is a secondary
                // cue. Same color as fenced code body so inline + block
                // feel like the same language.
                let style = Style::default()
                    .fg(theme::ACCENT2)
                    .bg(theme::BG_OVERLAY);
                push_with_newlines(
                    &mut current,
                    &mut lines,
                    text.as_ref(),
                    style,
                    |c, l| {
                        if !c.is_empty() {
                            l.push(std::mem::take(c));
                        }
                    },
                );
            }
            Event::Text(text) => {
                let style = if in_code_block {
                    code_block_style()
                } else {
                    compose_inline_style(&inline_stack).unwrap_or_default()
                };
                // v6: dropped the uppercase-on-heading transform. It
                // competed with the agent-name row above for attention
                // and made long responses read as ALL CAPS WALL. The
                // ▌ accent bar now does the structural anchoring work.
                push_with_newlines(
                    &mut current,
                    &mut lines,
                    text.as_ref(),
                    style,
                    |c, l| {
                        if !c.is_empty() {
                            l.push(std::mem::take(c));
                        }
                    },
                );
            }
            Event::SoftBreak | Event::HardBreak => {
                // Newline within a paragraph: emit a space (SoftBreak) or
                // flush the current line (HardBreak).
                if matches!(event, Event::HardBreak) {
                    flush!();
                } else {
                    current.push(Span::raw(" "));
                }
            }
            Event::Rule => {
                flush!();
                // v6.1: horizontal rule uses SECTION (steel). The `───`
                // stays thin — the color shift is what makes it read as
                // structural chrome rather than body punctuation.
                lines.push(vec![Span::styled("───", theme::section())]);
            }
            Event::Html(_) | Event::InlineHtml(_) => {
                // Ignore inline HTML — terminal can't render and LLM responses
                // occasionally emit stray `<br>` / `<sub>` markup.
            }
            Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {
                // Out of scope — footnotes, task list markers, math
                // environments are uncommon in LLM chat output. Drop silently.
            }
            Event::Start(_) | Event::End(_) => {
                // Catch-all for tag variants we don't model explicitly
                // (Table, HtmlBlock, FootnoteDefinition, DefinitionList,
                // Strikethrough wrappers, etc.). The Tag matches one of
                // the `Start` arms above; any other Tag is silently
                // treated as transparent so future pulldown-cmark
                // versions adding new block types don't break us.
            }
        }
    }

    flush!();
    // Drop trailing blank line so the chat log doesn't gain a phantom
    // empty row at the end of every assistant entry.
    while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines
}

/// Indent widths used by the markdown walker. Pre-computed so the walker
/// doesn't allocate per line.
const INDENT_PARAGRAPH: &str = "   ";
const INDENT_HEADING: &str = "  ";
const INDENT_LIST: &str = "     ";
const INDENT_CODE: &str = "   ";
const INDENT_BLOCKQUOTE: &str = "  ";

/// Push a styled string onto `current`, splitting on internal `\n`
/// characters and flushing the prior content to `lines` between them.
/// This is how a single text event that contains a hard newline (e.g.
/// inside a fenced code block) becomes multiple rendered lines without
/// requiring the caller to manually flush.
///
/// The caller passes a closure that flushes `current` into `lines` —
/// passing it in keeps `push_with_newlines` decoupled from the walker
/// state instead of smuggling references through globals.
fn push_with_newlines<F>(
    current: &mut Vec<Span<'static>>,
    lines: &mut Vec<Vec<Span<'static>>>,
    text: &str,
    style: Style,
    mut flush: F,
) where
    F: FnMut(&mut Vec<Span<'static>>, &mut Vec<Vec<Span<'static>>>),
{
    let mut iter = text.split('\n');
    let first = iter.next().unwrap_or("");
    if !first.is_empty() {
        current.push(Span::styled(first.to_string(), style));
    }
    for rest in iter {
        // Hard newline: flush the prior line, then continue building
        // the next one. Empty `rest` (consecutive newlines) becomes a
        // blank line — `flush` skips empty buffers, so the very next
        // non-empty fragment just continues on a new buffer.
        flush(current, lines);
        if !rest.is_empty() {
            current.push(Span::styled(rest.to_string(), style));
        }
    }
}

/// Combine the inline style stack into a single style. The top of the
/// stack wins for color/weight (later events override earlier ones).
///
/// v6.1: when the stack is empty we return `FG + ITALIC` — body prose
/// reads as italic calm typography. Bold (Strong) and emphasis (Emphasis)
/// override via the stack frames; both render upright, which makes them
/// the only upright thing in body text and gives them automatic
/// prominence. This is the editorial trick Medium / are.na use: italic
/// body, upright emphasis. The walker doesn't emit Emphasis for the
/// assistant `*foo*` case, but the convention is set so future prose
/// layers behave consistently.
fn compose_inline_style(stack: &[Style]) -> Option<Style> {
    if stack.is_empty() {
        return Some(Style::default().fg(theme::FG).add_modifier(Modifier::ITALIC));
    }
    let mut combined = Style::default();
    let mut has_any = false;
    for s in stack {
        if let Some(c) = s.fg {
            combined = combined.fg(c);
            has_any = true;
        }
        combined = combined.add_modifier(s.add_modifier);
    }
    if has_any || !combined.add_modifier.is_empty() {
        Some(combined)
    } else {
        None
    }
}

fn heading_style(level: pulldown_cmark::HeadingLevel) -> Style {
    // The `▌ ` accent bar is pushed inline at heading open (see the
    // walker). This style applies to the heading text spans that follow.
    // H1 is teal-accented — the only place outside focus indicators
    // where teal is allowed for non-interactive content, because H1s
    // are rare in LLM chat output and act as section dividers.
    use pulldown_cmark::HeadingLevel;
    match level {
        HeadingLevel::H1 => Style::default()
            .fg(theme::TEAL)
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H2 | HeadingLevel::H3 => Style::default()
            .fg(theme::EMPHASIS)
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => Style::default()
            .fg(theme::FG)
            .add_modifier(Modifier::BOLD),
    }
}

fn blockquote_style() -> Style {
    Style::default()
        .fg(theme::MUTED)
        .add_modifier(Modifier::ITALIC)
}

fn code_block_style() -> Style {
    // v6.1: warm peach (ACCENT2) for code foreground. The hue shift
    // signals "different medium" without using the teal accent, which is
    // reserved for focus/interactive. Inline `Event::Code` uses the
    // same color but with the BG_OVERLAY tint to mark the run as code.
    Style::default()
        .fg(theme::ACCENT2)
        .bg(theme::BG_OVERLAY)
}


fn render_chat_lines(state: &AppState) -> Vec<Line<'static>> {
    // Three states for an assistant entry, each with its own visual marker:
    //
    // - **streaming** (`state.streaming_assistant == Some(idx)`): a braille
    //   spinner in accent teal, followed by `…` instead of `‹`. The user
    //   reads this as "tokens still arriving".
    // - **done**: a `✓` knob in success green. The user reads this as
    //   "response is final — read at your own pace".
    // - **failed**: a `✗` knob in error red. Distinguished from `✓` by
    //   color so the colourblind-safe red/green pair still differentiates
    //   (we never show ✓ and ✗ adjacent in the same log without other text
    //   in between).
    //
    // The `‹` directional marker is replaced by the knob so the state is
    // unambiguous from a glance — important when scrolling back through a
    // long conversation.
    let mut out: Vec<Line> = Vec::with_capacity(state.log.len() * 3);
    for (idx, msg) in state.log.iter().enumerate() {
        match msg {
            ChatMsg::User(t) => {
                // User label on its own row (`› you`), body text on the next
                // row indented 3 spaces — same shape as the assistant entry.
                // This keeps short user messages from eating the right edge
                // and matches MiMo's two-line layout.
                out.push(Line::from(""));
                out.push(Line::from(vec![
                    Span::styled("›", theme::accent()),
                    Span::styled(" ", Style::default()),
                    Span::styled(
                        "you",
                        Style::default().fg(theme::EMPHASIS).add_modifier(Modifier::BOLD),
                    ),
                ]));
                out.push(Line::from(vec![
                    Span::styled("   ", Style::default()),
                    Span::styled(t.clone(), Style::default().fg(theme::EMPHASIS)),
                ]));
            }
            ChatMsg::ToolCall { agent, tool, args } => {
                // Tool calls render in ACCENT2 (warm peach) so they
                // share a hue with code — both are "different medium
                // than prose" and the warm hue clusters them visually.
                // v6.1: marker changed from `▸` to `╴` (U+2574, hairline
                // right-tab) so it can't be confused with `›`/`·`/`∘`
                // bullet rotation in the markdown walker. The hairline
                // + warm color = instant "this is a system action".
                out.push(Line::from(""));
                out.push(Line::from(vec![
                    Span::styled("╴", Style::default().fg(theme::ACCENT2)),
                    Span::styled(" ", Style::default()),
                    Span::styled(
                        tool.clone(),
                        Style::default()
                            .fg(theme::ACCENT2)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" · {agent}"),
                        Style::default().fg(theme::MUTED),
                    ),
                ]));
                let args_str = serde_json::to_string(args).unwrap_or_default();
                out.push(Line::from(vec![
                    Span::styled("   ", Style::default()),
                    Span::styled(args_str, Style::default().fg(theme::ACCENT2)),
                ]));
            }
            ChatMsg::Assistant { agent, text } => {
                // Prefix span composition: knob (state indicator) FIRST,
                // agent name SECOND, gutter LAST. Same pattern as the user
                // prompt above (`› you …`) — marker → role → content — so
                // the eye can scan the left margin for "is this message done
                // or still loading" without re-parsing the line.
                let (knob, knob_color) = if state.streaming_assistant == Some(idx) {
                    // Animated braille spinner, frame derived from the global
                    // tick so it stays in sync with the rest of the UI.
                    let frame = BUSY_SPINNER[state.tick as usize % BUSY_SPINNER_FRAMES];
                    (frame.to_string(), Style::default().fg(theme::TEAL))
                } else {
                    // Finalised entry — `✓` knob in success-green.
                    ("✓".to_string(), Style::default().fg(theme::SUCCESS))
                };
                let prefix_spans: Vec<Span<'static>> = vec![
                    Span::styled(knob, knob_color),
                    Span::styled(" ", Style::default()),
                    Span::styled(
                        agent.clone(),
                        Style::default().fg(theme::EMPHASIS).add_modifier(Modifier::BOLD),
                    ),
                ];

                // Render the assistant's text as Markdown. LLM responses are
                // always markdown-shaped: headings, bullet lists, inline code,
                // bold, blockquotes. We use `pulldown-cmark` directly (instead
                // of `tui-markdown`) so we can build a renderer that respects
                // the calm-terminal palette — see `render_markdown` below.
                //
                // Streaming note: we re-parse on every frame. The full text
                // is re-parsed even though only a tail delta arrived. This is
                // fine for chat-sized responses (markdown parsing of a 5KB
                // string is sub-millisecond). At 10 FPS redraw, the parse
                // cost is in the noise compared to the LLM response.
                let markdown: String = if text.is_empty() {
                    // Empty text — render a muted "…" so the user knows the
                    // streaming entry is alive but has no tokens yet.
                    "…".to_string()
                } else {
                    text.clone()
                };
                let md_lines = render_markdown(&markdown);

                // The walker returns lines that already carry their own
                // indent spans (see INDENT_* constants). The first line of
                // the assistant response replaces its leading indent with
                // the prefix (knob + agent name) so the marker sits at
                // column 0 of the chat panel. Subsequent lines keep their
                // walker-supplied indent unchanged — paragraphs get 3
                // spaces, list items 5, headings 2, code blocks 3,
                // blockquotes 2. This is what produces the varied left
                // margin that breaks the v5 "every line at column 3" feel.
                out.push(Line::from(""));
                let mut owned_lines = md_lines;
                if let Some(first) = owned_lines.first_mut() {
                    // Strip the leading indent span(s) the walker inserted
                    // for this line — the prefix takes its place. Indent
                    // spans are leading all-space Spans; we drop them.
                    while let Some(span) = first.first() {
                        if span.content.chars().all(|c| c == ' ') {
                            first.remove(0);
                        } else {
                            break;
                        }
                    }
                    let mut combined = prefix_spans.clone();
                    combined.extend(std::mem::take(first));
                    out.push(Line::from(combined));
                    owned_lines.remove(0);
                } else {
                    // Empty markdown — render the prefix alone as a stub,
                    // followed by a muted ellipsis on the next line so the
                    // user knows the streaming entry is alive.
                    out.push(Line::from(prefix_spans));
                    out.push(Line::from(Span::styled(
                        " …",
                        Style::default().fg(theme::MUTED),
                    )));
                }
                for line in owned_lines {
                    // Walker already produced the indent — pass through.
                    out.push(Line::from(line));
                }
            }
            ChatMsg::System(t) => {
                out.push(Line::from(""));
                out.push(Line::from(vec![
                    Span::styled(" · ", theme::dim()),
                    Span::styled(t.clone(), italic_muted()),
                ]));
            }
        }
    }
    out
}

// -- input -----------------------------------------------------------------

fn draw_input(f: &mut Frame, area: Rect, state: &mut AppState) {
    let focused = state.focus == Focus::Input;
    let block = panel_block("input", focused, None);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let (prompt, body, body_style): (Span, String, Style) = if state.supervisor_running {
        (
            Span::styled("   ", theme::dim()),
            format!(
                "supervisor running · {} done / {} failed — /stop to end",
                state.tasks_done, state.tasks_failed
            ),
            italic_muted(),
        )
    } else if state.busy == Busy::Running {
        (
            Span::styled(" › ", theme::accent()),
            "agent thinking…".to_string(),
            italic_muted(),
        )
    } else {
        (
            Span::styled(" › ", theme::accent()),
            state.input.clone(),
            Style::default().fg(theme::FG),
        )
    };
    let line = Line::from(vec![prompt, Span::styled(body, body_style)]);
    f.render_widget(Paragraph::new(line).wrap(Wrap { trim: false }), inner);

    // Blinking block cursor at end of input — only when the user can actually
    // type (Input focus + nothing else is busy). Crossterm puts the terminal
    // in raw mode on setup, which leaves the OS cursor visible; combined with
    // the `SetCursorStyle::BlinkingBlock` we set on setup, this renders as a
    // blinking block exactly where the next character will land.
    if focused
        && !state.supervisor_running
        && state.busy != Busy::Running
    {
        let prompt_width: u16 = 3; // " › "
        let cursor_x = inner.x
            + prompt_width
            + state.input.chars().count() as u16;
        f.set_cursor_position((cursor_x, inner.y));
    }
}

// -- footer hints ----------------------------------------------------------

fn draw_footer(f: &mut Frame, area: Rect, state: &mut AppState) {
    // Build left hints + right status, then pad so the right group sits
    // flush against the right edge of the footer.
    let left = build_footer_left(state);
    let right = build_footer_right(state);

    let left_w: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let right_w: usize = right.iter().map(|s| s.content.chars().count()).sum();
    let total_w = area.width as usize;
    let pad = total_w.saturating_sub(left_w + right_w);

    let mut spans = left;
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), Style::default()));
    }
    spans.extend(right);

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Left side: context-sensitive keybinding hints.
/// Kept narrow — should never exceed ~half the footer on a typical terminal.
fn build_footer_left(state: &AppState) -> Vec<Span<'static>> {
    let mut spans: Vec<Span> = Vec::new();
    let push = |s: &str, accent_key: bool, out: &mut Vec<Span>| {
        if accent_key {
            out.push(Span::styled(s.to_string(), theme::accent()));
        } else {
            out.push(Span::styled(s.to_string(), italic_muted()));
        }
    };

    push("focus ", false, &mut spans);
    push(focus_label(state.focus), true, &mut spans);

    match state.focus {
        Focus::Input => {
            push("  ·  ", false, &mut spans);
            push("Enter", true, &mut spans);
            push(" send  ", false, &mut spans);
            push("/", true, &mut spans);
            push(" command  ", false, &mut spans);
            push("Tab", true, &mut spans);
            push(" focus", false, &mut spans);
        }
        Focus::Chat => {
            push("  ·  ", false, &mut spans);
            push("j/k", true, &mut spans);
            push(" scroll  ", false, &mut spans);
            push("G/g", true, &mut spans);
            push(" end/beg  ", false, &mut spans);
            push("f", true, &mut spans);
            push(" follow", false, &mut spans);
        }
        Focus::Agents => {
            push("  ·  ", false, &mut spans);
            push("Tab", true, &mut spans);
            push(" cycle agent  ", false, &mut spans);
            push("Enter", true, &mut spans);
            push(" chat", false, &mut spans);
        }
        Focus::SidebarTasks | Focus::SidebarMemory => {
            push("  ·  ", false, &mut spans);
            push("Tab", true, &mut spans);
            push(" other sidebar  ", false, &mut spans);
            push("↑↓", true, &mut spans);
            push(" select", false, &mut spans);
        }
    }

    push("    ", false, &mut spans);
    push("?", true, &mut spans);
    push(" help  ", false, &mut spans);
    push("Ctrl-N", true, &mut spans);
    push(" new  ", false, &mut spans);
    push("Ctrl-L", true, &mut spans);
    push(" clear  ", false, &mut spans);
    push("Ctrl-C", true, &mut spans);
    push(" quit", false, &mut spans);

    spans
}

/// Right side: brand tag (`co_scientist`) prefixed by a busy indicator.
///
/// Three states:
/// - idle            → muted `·`
/// - single agent    → teal braille spinner (busy but not the full pipeline)
/// - supervisor run  → success-green braille spinner (the big loop)
///
/// The spinner frame index uses `state.tick` so it's already in sync with
/// the 100ms redraw tick.
fn build_footer_right(state: &AppState) -> Vec<Span<'static>> {
    let mut spans: Vec<Span> = Vec::new();

    let (glyph, color): (String, Style) = if state.supervisor_running {
        let frame = BUSY_SPINNER[state.tick as usize % BUSY_SPINNER_FRAMES];
        (frame.to_string(), Style::default().fg(theme::SUCCESS))
    } else if state.busy == Busy::Running {
        let frame = BUSY_SPINNER[state.tick as usize % BUSY_SPINNER_FRAMES];
        (frame.to_string(), theme::accent())
    } else {
        // Idle: a static muted dot. Stays quiet.
        ("·".to_string(), Style::default().fg(theme::MUTED))
    };
    spans.push(Span::styled(glyph, color));
    spans.push(Span::styled("  ", Style::default()));

    // Brand tag — matches the splash wordmark: `_` in accent, rest in fg.
    spans.push(Span::styled("co", Style::default().fg(theme::FG).add_modifier(Modifier::BOLD)));
    spans.push(Span::styled("_", theme::accent()));
    spans.push(Span::styled(
        "scientist",
        Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
    ));

    spans
}

fn focus_label(f: Focus) -> &'static str {
    match f {
        Focus::Input => "input",
        Focus::Chat => "chat",
        Focus::Agents => "agents",
        Focus::SidebarTasks => "tasks",
        Focus::SidebarMemory => "memory",
    }
}

// -- sidebar (tasks + memory) ---------------------------------------------

fn draw_sidebar(f: &mut Frame, area: Rect, state: &mut AppState) {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    draw_tasks(f, v[0], state);
    draw_memory(f, v[1], state);
}

fn draw_tasks(f: &mut Frame, area: Rect, state: &mut AppState) {
    let focused = state.focus == Focus::SidebarTasks;
    let block = panel_block("tasks", focused, None);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    if state.tasks.is_empty() {
        lines.push(Line::from(Span::styled(
            if state.supervisor_running { " waiting…" } else { " (idle)" },
            italic_muted(),
        )));
    } else {
        for t in state.tasks.iter().rev().take(inner.height as usize) {
            lines.push(render_task_line(t));
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_task_line(t: &TaskEntry) -> Line<'static> {
    let (icon, color) = match t.status {
        TaskStatus::Claimed => ("›", theme::FG),
        TaskStatus::Running => ("›", theme::TEAL),
        TaskStatus::Done => ("✓", theme::SUCCESS),
        TaskStatus::Failed => ("✗", theme::ERROR),
    };
    let action = t.action.as_deref().unwrap_or("");
    let worker = t.worker.as_deref().map(short_id).unwrap_or("?");
    let line = format!("{icon} {worker} {action}");
    Line::from(Span::styled(line, Style::default().fg(color)))
}

fn draw_memory(f: &mut Frame, area: Rect, state: &mut AppState) {
    let focused = state.focus == Focus::SidebarMemory;
    let block = panel_block("memory", focused, None);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    if state.memory.is_empty() {
        lines.push(Line::from(Span::styled(" (none yet)", italic_muted())));
    } else {
        for m in state.memory.iter().rev().take(inner.height as usize) {
            lines.push(render_memory_line(m));
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_memory_line(m: &MemoryEntry) -> Line<'static> {
    match m {
        MemoryEntry::Semantic { scope, summary } => Line::from(vec![
            Span::styled("+ ", Style::default().fg(theme::SUCCESS)),
            Span::styled(
                format!("[{scope}] "),
                Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
            ),
            Span::styled(summary.clone(), Style::default().fg(theme::FG)),
        ]),
        MemoryEntry::Behavior { agent, pattern } => Line::from(vec![
            Span::styled("· ", Style::default().fg(theme::TEAL)),
            Span::styled(
                format!("{agent}: "),
                Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
            ),
            Span::styled(pattern.clone(), italic_muted()),
        ]),
    }
}

// -- help overlay ----------------------------------------------------------

fn draw_help_overlay(f: &mut Frame, area: Rect) {
    let width = (area.width as usize).saturating_sub(8).min(72);
    let height = 22usize.min(area.height.saturating_sub(4) as usize);
    let x = area.x + (area.width.saturating_sub(width as u16)) / 2;
    let y = area.y + (area.height.saturating_sub(height as u16)) / 2;
    let popup = Rect::new(x, y, width as u16, height as u16);

    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(ROUNDED)
        .border_style(Style::default().fg(theme::TEAL))
        .title(Span::styled(" help ", accent_bold()))
        .title_bottom(Span::styled(" press ? or Esc to close ", italic_muted()));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let groups: [(&str, &[(&str, &str)]); 4] = [
        (
            "always",
            &[
                ("Tab / Shift+Tab", "cycle focus"),
                ("↑/↓ or j/k", "scroll chat / sidebar"),
                ("Enter", "send"),
                ("Esc", "cancel input / close overlay"),
                ("?", "this overlay"),
                ("Ctrl-C", "quit"),
            ],
        ),
        (
            "chat",
            &[
                ("PgUp / PgDn", "page"),
                ("g / G", "top / bottom"),
                ("f", "follow tail (toggle)"),
            ],
        ),
        (
            "actions",
            &[
                ("Tab / Shift+Tab", "cycle active agent"),
                ("Ctrl-N", "new run (resets chat)"),
                ("Ctrl-L", "clear chat log"),
            ],
        ),
        (
            "slash commands",
            &[
                ("/start <goal>", "spawn supervisor pipeline"),
                ("/stop", "stop supervisor"),
                ("/help", "show this overlay"),
                ("/test", "load chat-log fixture showing every style"),
            ],
        ),
    ];

    let mut lines: Vec<Line> = Vec::new();
    for (title, rows) in groups.iter() {
        lines.push(Line::from(Span::styled(
            title.to_string(),
            Style::default().fg(theme::TEAL).add_modifier(Modifier::BOLD),
        )));
        for (key, desc) in rows.iter() {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<18}"), Style::default().fg(theme::EMPHASIS)),
                Span::styled("  ", theme::dim()),
                Span::styled(*desc, Style::default().fg(theme::FG)),
            ]));
        }
        lines.push(Line::from(""));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

// -- shared chrome helpers -------------------------------------------------

fn panel_block(title: &str, focused: bool, badge: Option<usize>) -> Block<'static> {
    let title_text = match badge {
        Some(n) if n > 0 => format!(" {title} · {n} "),
        _ => format!(" {title} "),
    };
    panel_block_raw(&title_text, focused)
}

fn panel_block_raw(title: &str, focused: bool) -> Block<'static> {
    let border_style = if focused {
        Style::default().fg(theme::TEAL)
    } else {
        dim_border()
    };
    let title_style = if focused {
        accent_bold()
    } else {
        Style::default().fg(theme::FG).add_modifier(Modifier::BOLD)
    };
    Block::default()
        .borders(ROUNDED)
        .border_style(border_style)
        .title(Span::styled(title.to_string(), title_style))
}

fn mini_bar(value: usize, max: usize) -> String {
    let filled = if max == 0 { 0 } else { value.min(max) };
    let pct = (filled * 10 / max.max(1)) as u16;
    let mut s = String::with_capacity(12);
    s.push('[');
    for i in 0..10 {
        s.push(if i < pct { '█' } else { '░' });
    }
    s.push(']');
    s
}

fn dim_border() -> Style {
    Style::default().fg(theme::MUTED)
}
fn accent_bold() -> Style {
    Style::default().fg(theme::TEAL).add_modifier(Modifier::BOLD)
}
fn italic_muted() -> Style {
    Style::default().fg(theme::MUTED).add_modifier(Modifier::ITALIC)
}

fn short_id(s: &str) -> &str {
    if s.len() >= 8 { &s[..8] } else { s }
}

#[cfg(test)]
mod render_markdown_tests {
    //! Smoke tests for the hand-rolled markdown walker.
    //!
    //! The walker returns `Vec<Vec<Span<'static>>>` — one outer entry per
    //! rendered `Line`, one inner entry per `Span`. Tests below assert on
    //! outer line counts and on the textual content of the spans so the
    //! regressions in the actual rendering are visible without depending
    //! on internal style state.

    use super::render_markdown;

    fn flatten(lines: &[Vec<ratatui::text::Span<'static>>]) -> Vec<String> {
        lines
            .iter()
            .map(|spans| spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn plain_text_renders_one_line() {
        let out = render_markdown("hello world");
        let flat = flatten(&out);
        assert_eq!(flat, vec!["hello world".to_string()]);
    }

    #[test]
    fn heading_has_accent_bar_and_following_blank() {
        // v6: heading line starts with the `▌` accent bar in teal, gets
        // a blank line above and below for visual air. Text is NOT
        // uppercased anymore — the ▌ bar does the structural anchoring
        // and uppercase competed with the agent-name row for attention.
        let out = render_markdown("# Title\n\nbody\n");
        let flat = flatten(&out);
        // First line should start with the accent bar.
        assert!(
            flat[0].contains("▌"),
            "expected ▌ accent bar on heading line: {:?}",
            flat
        );
        assert!(
            flat[0].contains("Title"),
            "heading text missing: {:?}",
            flat
        );
        assert!(
            !flat[0].contains("TITLE"),
            "heading should not be uppercased in v6: {:?}",
            flat
        );
        // Blank above (none — it's first) and below the heading.
        assert!(
            flat[1].is_empty(),
            "expected blank line after heading: {:?}",
            flat
        );
        assert!(flat[2].contains("body"), "body missing: {:?}", flat);
    }

    #[test]
    fn unordered_list_each_item_is_its_own_line() {
        // Regression test for the bug where internal newlines inside a
        // Text event were dropped, collapsing every list item into one
        // line. After the fix each `- item` should land on its own line.
        let out = render_markdown("- alpha\n- beta\n- gamma\n");
        let flat = flatten(&out);
        assert!(
            flat.iter().any(|l| l.contains("alpha")),
            "alpha missing: {:?}",
            flat
        );
        assert!(
            flat.iter().any(|l| l.contains("beta")),
            "beta missing: {:?}",
            flat
        );
        assert!(
            flat.iter().any(|l| l.contains("gamma")),
            "gamma missing: {:?}",
            flat
        );
        // The three items must be on distinct lines — not jammed onto one.
        let mut seen = 0;
        for line in &flat {
            if line.contains("alpha") || line.contains("beta") || line.contains("gamma") {
                seen += 1;
            }
        }
        assert!(seen >= 3, "items collapsed to fewer lines: {:?}", flat);
    }

    #[test]
    fn unordered_list_markers_rotate() {
        // v6: bulleted list markers rotate `›` → `·` → `∘` so a long
        // list reads as a progression rather than `› › › › ›`.
        let out = render_markdown("- a\n- b\n- c\n- d\n");
        let flat = flatten(&out);
        // First three items should use distinct markers.
        let a_line = flat.iter().find(|l| l.contains("a")).unwrap();
        let b_line = flat.iter().find(|l| l.contains("b")).unwrap();
        let c_line = flat.iter().find(|l| l.contains("c")).unwrap();
        assert!(a_line.contains("›"), "first marker should be ›: {:?}", a_line);
        assert!(b_line.contains("·"), "second marker should be ·: {:?}", b_line);
        assert!(c_line.contains("∘"), "third marker should be ∘: {:?}", c_line);
    }

    #[test]
    fn ordered_list_numbers_increment() {
        let out = render_markdown("1. first\n2. second\n3. third\n");
        let flat = flatten(&out);
        assert!(flat.iter().any(|l| l.contains("1") && l.contains("first")));
        assert!(flat.iter().any(|l| l.contains("2") && l.contains("second")));
        assert!(flat.iter().any(|l| l.contains("3") && l.contains("third")));
    }

    #[test]
    fn fenced_code_block_internal_newlines_become_lines() {
        // Regression test: previously the newlines inside the code fence
        // were dropped, so the whole block rendered as one line with
        // the word "fn" jammed against "main".
        let out = render_markdown("```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n");
        let flat = flatten(&out);
        assert!(flat.iter().any(|l| l.contains("fn main()")));
        assert!(flat.iter().any(|l| l.contains("println!")));
        assert!(flat.iter().any(|l| l.contains('}')));
    }

    #[test]
    fn fenced_code_block_uses_thick_rule_header() {
        // v6: code fences use `━━` (thick) on the lang header so they
        // don't get confused with `---` horizontal rules (which use
        // `───`, a single line).
        let out = render_markdown("```rust\nx\n```\n");
        let flat = flatten(&out);
        assert!(
            flat.iter().any(|l| l.contains("━━ rust")),
            "thick code-fence rule missing: {:?}",
            flat
        );
    }

    #[test]
    fn blockquote_uses_broken_bar_gutter() {
        // v6: blockquotes use `╎` (U+254E, broken vertical bar) instead
        // of `│` so they can't be confused with the chat-message gutter.
        let out = render_markdown("> quoted line\n");
        let flat = flatten(&out);
        assert!(
            flat.iter().any(|l| l.contains("╎")),
            "blockquote gutter missing: {:?}",
            flat
        );
    }

    #[test]
    fn bold_inline_gets_emphasis_style() {
        let out = render_markdown("hello **world** end");
        // Find the bold span — it should have BOLD modifier set.
        let bold_span = out
            .iter()
            .flat_map(|line| line.iter())
            .find(|s| s.content.contains("world"))
            .expect("world span missing");
        assert!(
            bold_span.style.add_modifier.contains(ratatui::style::Modifier::BOLD),
            "expected BOLD modifier on 'world' span: {:?}",
            bold_span
        );
    }

    #[test]
    fn horizontal_rule_renders_dashes() {
        let out = render_markdown("above\n\n---\n\nbelow\n");
        let flat = flatten(&out);
        assert!(
            flat.iter().any(|l| l.contains("───")),
            "hr marker missing: {:?}",
            flat
        );
    }

    #[test]
    fn trailing_blank_lines_are_trimmed() {
        // Streaming responses often end with `\n\n` — the renderer
        // should not emit a phantom empty Line at the end of the log.
        let out = render_markdown("done.\n\n\n");
        let flat = flatten(&out);
        assert_eq!(flat.last().map(|s| s.as_str()), Some("done."));
    }

    #[test]
    fn consecutive_paragraphs_have_no_blank_between_them() {
        // v6 change: paragraphs no longer push blanks between them.
        // The 3-space indent plus the natural paragraph break is
        // enough — v5's per-paragraph blank made responses feel
        // metronomic ("every block gets a blank" fatigue).
        let out = render_markdown("first.\n\nsecond.\n\nthird.\n");
        let flat = flatten(&out);
        let first_idx = flat.iter().position(|l| l.contains("first")).unwrap();
        let second_idx = flat.iter().position(|l| l.contains("second")).unwrap();
        let third_idx = flat.iter().position(|l| l.contains("third")).unwrap();
        // Consecutive paragraphs are now adjacent (no blank between).
        assert_eq!(
            second_idx - first_idx,
            1,
            "expected no blank between consecutive paragraphs: {:?}",
            flat
        );
        assert_eq!(
            third_idx - second_idx,
            1,
            "expected no blank between consecutive paragraphs: {:?}",
            flat
        );
    }

    #[test]
    fn heading_reserves_air_but_paragraph_does_not() {
        // The one block type that still gets blank-line air is the
        // heading — that's the only structural anchor in long LLM
        // responses. Paragraphs sit tight together.
        let out = render_markdown("# H\n\np1\n\np2\n");
        let flat = flatten(&out);
        let h_idx = flat.iter().position(|l| l.contains("H")).unwrap();
        let p1_idx = flat.iter().position(|l| l.contains("p1")).unwrap();
        let p2_idx = flat.iter().position(|l| l.contains("p2")).unwrap();
        // Heading → paragraph: blank above (none, first) and below.
        assert_eq!(p1_idx - h_idx, 2, "heading should be followed by a blank then the paragraph: {:?}", flat);
        // Paragraph → paragraph: no blank.
        assert_eq!(p2_idx - p1_idx, 1, "paragraphs should be adjacent: {:?}", flat);
    }
}
