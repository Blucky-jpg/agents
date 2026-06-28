//! Streaming-delta marker scrubber.
//!
//! Strips `[[MEMORY_OP:<op>:{json}]]` markers from streaming deltas
//! before they land in the chat log. `TurnDone` parses + strips
//! markers from the final text, but `TurnDelta` carries the raw LLM
//! text — without this scrub the raw marker renders as a plain
//! markdown paragraph in the live log until `TurnDone` arrives (the
//! user's screenshot showed `memory_add` leaking during streaming).
//!
//! ## The state machine
//!
//! - `Outside` → just text. Pass through.
//! - `MaybeOpen` → one `[` seen. Pass through; if the next char is
//!   `[`, move to `Inside`. If anything else, drop back to `Outside`
//!   (the `[` was a literal bracket in prose, not a marker opener).
//! - `Inside` → consume until `]]` is seen. Internal `{` / `}` braces
//!   are counted (so a JSON body containing nested braces is consumed
//!   as one unit). The closing `]]` returns us to `Outside`.
//!
//! ## The carry
//!
//! `carry_in` is the partial-marker tail from the previous delta that
//! must be processed first. We mutate it to the new unconsumed tail
//! when the input ends mid-marker — that way a
//! `[[MEMORY_OP:save:{"scope":"x"` in one delta followed by
//! `,"summary":"y"}]]` in the next delta is scrubbed as a unit.
//!
//! ## Why not use `co_scientist::skill::parse_markers`?
//!
//! That function expects the FULL text in one shot; it returns the
//! cleaned text + parsed markers as a single `ParsedResponse`.
//! Streaming the cleaned text back per delta is awkward (the same
//! content would be re-emitted as the parser sees more text) and
//! would break the existing dedup contract in `query_stream`.
//!
//! ## Module locality
//!
//! Before C3 (2026-06-28) this function lived in `main.rs` next to
//! the bootstrap code, despite being a pure function over `&str` with
//! no async, no I/O, and no Ratatui dependency. The 8 unit tests
//! already exercised it in isolation (no `AppState`, no
//! `SharedState`) — the seam was begging to be made explicit. The
//! move doesn't change behavior; it makes the locality contract
//! enforceable. The reducer in `app::reducers::on_turn_delta` is
//! the sole caller.

/// Strip `[[MEMORY_OP:<op>:{json}]]` markers from a single streaming
/// delta. `carry_in` is the partial-marker tail from the previous
/// delta that must be processed first; on return it holds the new
/// unconsumed tail (or is empty if the input ended cleanly).
///
/// Returns the marker-stripped text the chat log should render.
pub fn scrub(carry_in: &mut String, delta: &str) -> String {
    let mut buf = String::with_capacity(carry_in.len() + delta.len());
    std::mem::swap(&mut buf, carry_in);
    buf.push_str(delta);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum State {
        Outside,
        MaybeOpen,
        Inside,
    }
    // After MaybeOpen has consumed the first `[`, `i` points at the
    // second `[`. So the prefix slice that must match starts AT `i`,
    // not at `i-1`. Length 11 = `[MEMORY_OP:`.
    const PREFIX: &[u8] = b"[MEMORY_OP:";
    const PREFIX_LEN: usize = 11; // "[MEMORY_OP:" length

    let mut out = String::with_capacity(buf.len());
    let bytes = buf.as_bytes();
    let mut i = 0;
    let mut state = State::Outside;
    let mut brace_depth: u32 = 0;

    while i < bytes.len() {
        let b = bytes[i];
        match state {
            State::Outside => {
                if b == b'[' {
                    // Buffer the `[` and enter MaybeOpen.
                    state = State::MaybeOpen;
                    i += 1;
                } else {
                    out.push(b as char);
                    i += 1;
                }
            }
            State::MaybeOpen => {
                if b == b'[' {
                    // Confirmed double-bracket. Check the rest of the prefix.
                    if i + PREFIX_LEN <= bytes.len()
                        && &bytes[i..i + PREFIX_LEN] == PREFIX
                    {
                        // MEMORY_OP marker — consume the prefix and enter
                        // Inside. Both `[` chars were buffered; we drop them.
                        i += PREFIX_LEN;
                        state = State::Inside;
                        brace_depth = 0;
                    } else {
                        // Not a MEMORY_OP marker. The two buffered `[`
                        // chars were literal prose — emit them both, then
                        // advance past THIS `[`. The character AFTER this
                        // `[` will be reprocessed from Outside (it's the
                        // next byte in the stream).
                        out.push('[');
                        out.push('[');
                        state = State::Outside;
                        i += 1;
                    }
                } else {
                    // Lone `[` — was literal prose. Emit it, drop back to
                    // Outside, and re-process this byte from Outside (don't
                    // advance — this byte is a non-`[` literal that gets
                    // emitted on the next iteration).
                    out.push('[');
                    state = State::Outside;
                    // i unchanged.
                }
            }
            State::Inside => {
                if b == b'{' {
                    brace_depth += 1;
                    i += 1;
                } else if b == b'}' {
                    brace_depth = brace_depth.saturating_sub(1);
                    i += 1;
                } else if b == b']' && i + 1 < bytes.len() && bytes[i + 1] == b']' {
                    // Closing `]]` — marker is done. Consume both.
                    i += 2;
                    state = State::Outside;
                } else {
                    i += 1;
                }
            }
        }
    }

    // Whatever's unconsumed at EOF goes back into carry_in.
    let leftover_start = match state {
        State::Outside => bytes.len(),
        State::MaybeOpen => bytes.len() - 1, // the lone `[`
        State::Inside => {
            // We started consuming a marker at the prefix position.
            // Reconstruct the partial marker from the prefix start.
            // We need to find the last occurrence of "[[MEMORY_OP:" in `buf`.
            // Since we emitted nothing during Inside, the byte offset where
            // we entered Inside is `prefix_start` — but we already advanced
            // `i` past it. Reconstruct by searching backward.
            find_marker_start_in_outside(&out, &buf)
        }
    };
    if leftover_start < bytes.len() {
        // Drain the unconsumed tail into carry_in.
        carry_in.clear();
        carry_in.push_str(&buf[leftover_start..]);
    }

    out
}

/// Helper: when EOF arrives mid-Inside, locate where the partial marker
/// started in the original `buf` so we can carry it forward. The marker
/// always starts with `[[MEMORY_OP:`, so we find the last occurrence of
/// that prefix in `buf` from a position where it isn't already in `out`.
fn find_marker_start_in_outside(_out: &str, buf: &str) -> usize {
    // Walk back from end looking for the prefix that begins a marker
    // not already emitted. Since Inside never emits to `out`, any
    // occurrence of "[[MEMORY_OP:" we encountered is still in the
    // `out` only if we False-Started it (the bytes "[[" made it in).
    // Simplest correct approach: search for the LAST occurrence of
    // "[[MEMORY_OP:" in `buf`; the marker began there.
    match buf.rfind("[[MEMORY_OP:") {
        Some(pos) => pos,
        None => buf.len(),
    }
}

#[cfg(test)]
mod tests {
    //! Marker-scrubbing unit tests. The bug-prone case is a marker
    //! split across two deltas — verified explicitly by
    //! `marker_split_across_two_deltas`.

    use super::scrub;

    #[test]
    fn whole_marker_stripped_in_single_delta() {
        let mut carry = String::new();
        let out = scrub(&mut carry, "hi [[MEMORY_OP:noop:{}]] bye");
        assert_eq!(out, "hi  bye");
        assert!(carry.is_empty(), "carry should be empty: {:?}", carry);
    }

    #[test]
    fn prefix_only_no_false_positive() {
        // A `[[` that isn't a MEMORY_OP opener must pass through.
        let mut carry = String::new();
        let out = scrub(&mut carry, "text [[ not a marker ]] more");
        assert_eq!(out, "text [[ not a marker ]] more");
        assert!(carry.is_empty());
    }

    #[test]
    fn bare_open_bracket_no_false_positive() {
        // A single `[` followed by non-`[` must pass through.
        let mut carry = String::new();
        let out = scrub(&mut carry, "price [$10] off");
        assert_eq!(out, "price [$10] off");
        assert!(carry.is_empty());
    }

    #[test]
    fn marker_split_across_two_deltas() {
        // The bug-prone case: marker opens in delta 1, closes in delta 2.
        let mut carry = String::new();
        let out1 = scrub(&mut carry, "hello [[MEMORY_OP:save_semantic:{\"scope\":\"x\"");
        assert_eq!(out1, "hello ");
        assert!(!carry.is_empty(), "carry should hold half-marker");
        assert!(carry.contains("MEMORY_OP"), "carry retained marker prefix");
        let out2 = scrub(&mut carry, ",\"summary\":\"y\"}]] world");
        assert_eq!(out2, " world");
        assert!(carry.is_empty(), "carry cleared after marker closes");
        // Combined emissions must contain NO `MEMORY_OP` text.
        assert!(!out1.contains("MEMORY_OP"));
        assert!(!out2.contains("MEMORY_OP"));
    }

    #[test]
    fn nested_json_brace_not_confused() {
        // Nested `{}` inside the JSON body must not confuse the close
        // detector — the marker is consumed as a unit.
        let mut carry = String::new();
        let out = scrub(&mut carry, "before [[MEMORY_OP:t:{\"k\":{\"v\":1}}]] after");
        assert_eq!(out, "before  after");
        assert!(carry.is_empty());
    }

    #[test]
    fn multiple_markers_in_one_delta() {
        let mut carry = String::new();
        let out = scrub(
            &mut carry,
            "[[MEMORY_OP:a:{}]] middle [[MEMORY_OP:b:{\"x\":1}]] end",
        );
        assert_eq!(out, " middle  end");
        assert!(carry.is_empty());
    }

    #[test]
    fn unterminated_marker_carries_forward() {
        // LLM emits half a marker and never closes it (malformed
        // response). The half-marker must be carried forward, not
        // leaked into the rendered output.
        let mut carry = String::new();
        let out = scrub(&mut carry, "before [[MEMORY_OP:save:{\"scope\":");
        assert_eq!(out, "before ");
        assert!(!carry.is_empty());
        assert!(carry.contains("MEMORY_OP"));
    }

    #[test]
    fn markdown_text_with_brackets_passes_through() {
        // Markdown emphasis + links contain `[` chars. Verify they
        // don't trip the state machine.
        let mut carry = String::new();
        let out = scrub(&mut carry, "see [docs](https://example.com) and `[]`");
        assert_eq!(out, "see [docs](https://example.com) and `[]`");
        assert!(carry.is_empty());
    }
}
