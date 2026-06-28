//! Markdown rendering — hand-rolled pulldown-cmark walker.
//!
//! ## Why a hand-rolled renderer?
//!
//! `tui-markdown` hardcodes a neon palette (yellow headings, cyan code,
//! magenta blockquotes) that violates the calm-terminal one-accent rule
//! (AD7 in project memory). The crate's `StyleSheet` trait only exists
//! from 0.3.6+ and requires ratatui 0.30 — out of scope. We read the
//! pulldown-cmark event stream directly and map each element to a `Span`
//! styled from `crate::theme`, so headings pick up the teal accent and
//! body text stays in `theme::FG`.
//!
//! ## Visual rhythm
//!
//! Each rendered `Vec<Span>` carries a leading "indent" span (one of
//! `INDENT_PARAGRAPH`, `INDENT_HEADING`, etc., all-space) so the caller
//! can drop the outer body indent without re-counting columns. The
//! walker is self-describing — the visual rhythm is encoded in the
//! output, not reconstructed by the caller.
//!
//! - Headings: `▌` accent bar at column 0, blank above and below.
//! - Paragraphs: 3-space indent, no surrounding blanks.
//! - Bulleted items: 5-space indent + `›`/`·`/`∘` rotating markers
//!   in teal, no surrounding blanks.
//! - Ordered items: 5-space indent + `n.` marker in muted.
//! - Code fences: 3-space indent + `━━` thick rule on the lang header
//!   so the fence reads as a single chunk, distinct from `---` rules.
//! - Blockquotes: 2-space indent + `╎` broken-bar gutter (U+254E) in
//!   muted italic — different from `│` so it can't be confused with the
//!   chat-message gutter.
//! - Horizontal rules: single `───` in muted, no surrounding blanks.
//!
//! ## What's covered (everything an LLM typically emits)
//!
//! - `# ## ###` etc. headings (accent bar + bold)
//! - body paragraphs
//! - `-` / `*` / `+` bullet lists (rotated `›`/`·`/`∘` markers)
//! - `1.` ordered lists
//! - `**bold**` and `*italic*`
//! - `` `inline code` `` with bg.overlay highlight
//! - fenced code blocks (bg.overlay bg, every line)
//! - `> ` blockquotes (italic + muted, broken-bar gutter)
//! - `---` horizontal rules
//! - `[text](url)` links (text shown; URL dropped)
//!
//! ## Streaming caveat
//!
//! The LLM streams token-by-token, so mid-paragraph markdown may be
//! malformed (unclosed `**`, half-typed `>`). pulldown-cmark tolerates
//! this gracefully — unclosed emphasis just renders as literal
//! asterisks. At 10 FPS redraw the user doesn't see the transition
//! when a half-formed list finally gets its `\n`.
//!
//! ## Module locality
//!
//! Before C2 (2026-06-28) this function lived inside `ui.rs` next to
//! the panel draw code. The mix made `ui.rs` 1673 lines and forced a
//! reader scrolling for "how does a bullet list render" to scroll
//! past 400 lines of draw code first. The deepening moves the walker
//! here, leaving `ui.rs` as pure panel-draw. The interface
//! (`render(input) -> Vec<Vec<Span<'static>>>`) is unchanged so the
//! single call site in `ui::render_chat_lines` keeps working without
//! further changes.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use crate::theme;

/// Render a markdown string into `Vec<Span<'static>>` values, one per
/// rendered `Line`. See module docs for the visual-rhythm contract.
pub fn render(input: &str) -> Vec<Vec<Span<'static>>> {
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
                inline_stack.push(Style::default().fg(theme::BLUE).add_modifier(Modifier::BOLD));
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
            .fg(theme::BLUE)
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H2 | HeadingLevel::H3 => Style::default()
            .fg(theme::BLUE)
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => Style::default()
            .fg(theme::BLUE)
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

#[cfg(test)]
mod tests {
    //! Smoke tests for the hand-rolled markdown walker.
    //!
    //! The walker returns `Vec<Vec<Span<'static>>>` — one outer entry per
    //! rendered `Line`, one inner entry per `Span`. Tests below assert on
    //! outer line counts and on the textual content of the spans so the
    //! regressions in the actual rendering are visible without depending
    //! on internal style state.

    use super::render;

    fn flatten(lines: &[Vec<ratatui::text::Span<'static>>]) -> Vec<String> {
        lines
            .iter()
            .map(|spans| spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn plain_text_renders_one_line() {
        let out = render("hello world");
        let flat = flatten(&out);
        assert_eq!(flat, vec!["hello world".to_string()]);
    }

    #[test]
    fn heading_has_accent_bar_and_following_blank() {
        // v6: heading line starts with the `▌` accent bar in teal, gets
        // a blank line above and below for visual air. Text is NOT
        // uppercased anymore — the ▌ bar does the structural anchoring
        // and uppercase competed with the agent-name row for attention.
        let out = render("# Title\n\nbody\n");
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
        let out = render("- alpha\n- beta\n- gamma\n");
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
        let out = render("- a\n- b\n- c\n- d\n");
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
        let out = render("1. first\n2. second\n3. third\n");
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
        let out = render("```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n");
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
        let out = render("```rust\nx\n```\n");
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
        let out = render("> quoted line\n");
        let flat = flatten(&out);
        assert!(
            flat.iter().any(|l| l.contains("╎")),
            "blockquote gutter missing: {:?}",
            flat
        );
    }

    #[test]
    fn bold_inline_gets_emphasis_style() {
        let out = render("hello **world** end");
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
        let out = render("above\n\n---\n\nbelow\n");
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
        let out = render("done.\n\n\n");
        let flat = flatten(&out);
        assert_eq!(flat.last().map(|s| s.as_str()), Some("done."));
    }

    #[test]
    fn consecutive_paragraphs_have_no_blank_between_them() {
        // v6 change: paragraphs no longer push blanks between them.
        // The 3-space indent plus the natural paragraph break is
        // enough — v5's per-paragraph blank made responses feel
        // metronomic ("every block gets a blank" fatigue).
        let out = render("first.\n\nsecond.\n\nthird.\n");
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
        let out = render("# H\n\np1\n\np2\n");
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
