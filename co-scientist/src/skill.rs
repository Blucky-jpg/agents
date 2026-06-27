//! The LLM-facing skill.
//!
//! The skill is a markdown document loaded into the system prompt. It tells
//! the model:
//!   1. What it is (which co-scientist role)
//!   2. When to call which memory operation
//!   3. The exact marker format it must emit so we can parse and dispatch
//!
//! The marker format is the integration contract between the model and our
//! `Memory` API. It's how the model "calls a tool" without us having to
//! modify the ante runtime to register real tools.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The full skill document, embedded at compile time from `SKILL.md`.
pub const SKILL: &str = include_str!("../SKILL.md");

/// One parsed memory operation emitted by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    /// Raw tool name (e.g. "save_semantic", "peek_context").
    /// Dispatched through the ToolRegistry — no enum gate.
    pub op: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerOp {
    SaveSemantic,
    SaveBehavior,
    GetContext,
}

impl MarkerOp {
    pub fn as_str(self) -> &'static str {
        match self {
            MarkerOp::SaveSemantic => "save_semantic",
            MarkerOp::SaveBehavior => "save_behavior",
            MarkerOp::GetContext => "get_context",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "save_semantic" => Some(Self::SaveSemantic),
            "save_behavior" => Some(Self::SaveBehavior),
            "get_context" => Some(Self::GetContext),
            _ => None,
        }
    }
}

pub struct ParsedResponse {
    pub cleaned_text: String,
    pub markers: Vec<Marker>,
}

/// Find the next `[[MEMORY_OP:` after `start` and return
/// `(op_name, json, marker_start, marker_end)`. The JSON is located by
/// brace-counting so nested objects work; braces inside strings are
/// ignored.
///
/// LLM-tolerance rules (each one motivated by a real observed failure):
/// - Case-insensitive prefix match. Some models emit `[[memory_op:`
///   lowercase. We accept either case.
/// - Up to 10 trailing `]` characters are consumed (canonical `]]`,
///   sometimes `]]]`, occasionally just `]`). The cap protects against
///   runaway loops on pathological input.
/// - An unclosed marker (brace-counting reaches EOT before depth=0) is
///   reported via `None` but with the rest of `text` after the prefix
///   swallowed as the malformed-JSON path — `parse_markers` logs a
///   warning so the LLM sees the error in next-turn feedback.
fn find_next_marker(text: &str, start: usize) -> Option<(&str, &str, usize, usize)> {
    // Case-insensitive prefix lookup. We try the canonical form first;
    // if it doesn't match, fall back to the lowercase form. This avoids
    // the cost of lowercasing the whole text.
    let canonical_prefix = "[[MEMORY_OP:";
    let lowercase_prefix = "[[memory_op:";
    let (prefix, marker_start) = match text.get(start..)?.find(canonical_prefix) {
        Some(off) => (canonical_prefix, off + start),
        None => match text.get(start..)?.to_ascii_lowercase().find(lowercase_prefix) {
            // The lowercase search returned a position in the lowercased
            // string, which is byte-identical to the original (ASCII only).
            Some(off) => (lowercase_prefix, off + start),
            None => return None,
        },
    };
    let after = marker_start + prefix.len();
    let colon = text.get(after..)?.find(':')? + after;
    let op_name = text.get(after..colon)?.trim();
    let json_start = colon + 1;
    // Skip whitespace before the JSON. Some models add a space after
    // the colon: `[[MEMORY_OP:op: {...}]]`.
    let bytes = text.as_bytes();
    let mut probe = json_start;
    while probe < bytes.len() && (bytes[probe] == b' ' || bytes[probe] == b'\t') {
        probe += 1;
    }
    let first = bytes.get(probe).copied()?;
    if first != b'{' {
        // Reject empty or non-object payloads at the prefix level. The
        // outer `parse_markers` loop will treat this as a no-match and
        // search for the NEXT marker (skipping the bad one). We log
        // inside `parse_markers` instead so the warning includes
        // context. Returning None here is the signal.
        return None;
    }
    let json_start = probe;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut json_end = None;
    let mut i = json_start;
    while i < bytes.len() {
        let b = bytes[i];
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        if in_string {
            match b {
                b'\\' => escape = true,
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    json_end = Some(i + 1);
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    let json_end = json_end?;
    // Consume up to 10 trailing `]` characters. We bound this so a
    // pathological input like `]]]]]]]]]]...` can't loop us.
    let after_json = &text[json_end..];
    let mut term_len = 0usize;
    for (i, b) in after_json.bytes().enumerate() {
        if b == b']' && i < 10 {
            term_len = i + 1;
        } else {
            break;
        }
    }
    if term_len == 0 {
        // No terminator at all — treat the marker as malformed. `parse_markers`
        // will surface this as a warning so the LLM sees the issue.
        return None;
    }
    let marker_end = json_end + term_len;
    Some((op_name, &text[json_start..json_end], marker_start, marker_end))
}

/// Detect malformed marker prefixes that `find_next_marker` rejects
/// (empty payload, no terminator). Emits one warning per occurrence
/// so the LLM can self-correct on the next turn.
fn warn_about_malformed_prefixes(text: &str) {
    let prefixes = ["[[MEMORY_OP:", "[[memory_op:"];
    let mut search_from = 0usize;
    while let Some(rel) = prefixes
        .iter()
        .filter_map(|p| text.get(search_from..)?.find(p).map(|o| (p, o)))
        .min_by_key(|(_, o)| *o)
    {
        let (prefix, offset) = rel;
        let abs = offset + search_from;
        let after = abs + prefix.len();
        // Walk past the colon and any whitespace.
        let mut probe = after;
        let bytes = text.as_bytes();
        while probe < bytes.len() && (bytes[probe] == b':' || bytes[probe] == b' ' || bytes[probe] == b'\t') {
            probe += 1;
        }
        // Is there a `{` at probe? If not, this prefix is malformed.
        match bytes.get(probe) {
            Some(b'{') => {
                // Could be malformed in other ways (unclosed) but the
                // brace-counting loop in find_next_marker will report it.
            }
            Some(_) => {
                tracing::warn!(
                    position = abs,
                    "skill marker has empty or non-object payload; skipping"
                );
            }
            None => {
                tracing::warn!(
                    position = abs,
                    "skill marker is unclosed (no payload or terminator); skipping"
                );
            }
        }
        search_from = abs + prefix.len();
    }
}

/// Pull every `[[MEMORY_OP:<op>:<json>]]` marker out of `text`. Returns the
/// remaining text (markers stripped) and the parsed operations in order.
pub fn parse_markers(text: &str) -> ParsedResponse {
    warn_about_malformed_prefixes(text);
    let mut markers = Vec::new();
    let mut cleaned = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let prefix = "[[MEMORY_OP:";

    loop {
        match find_next_marker(text, cursor) {
            Some((op_name, json, marker_start, marker_end)) => {
                cleaned.push_str(&text[cursor..marker_start]);
                cursor = marker_end;

                match serde_json::from_str::<Value>(json) {
                    Ok(payload) => markers.push(Marker {
                        op: op_name.to_string(),
                        payload,
                    }),
                    Err(e) => {
                        tracing::warn!(
                            op = op_name,
                            error = %e,
                            "skill marker has invalid JSON; skipping"
                        );
                    }
                }
            }
            None => {
                // No marker at `cursor`. If we still have a partial
                // `[[MEMORY_OP:` ahead, advance past it (and copy the
                // preceding prose to `cleaned`) so the next iteration
                // can search for the marker AFTER the bad one. Without
                // this, a single malformed marker would swallow
                // everything after it into cleaned_text.
                if let Some(rel) = text.get(cursor..).and_then(|s| s.find(prefix)) {
                    let abs = rel + cursor;
                    cleaned.push_str(&text[cursor..abs]);
                    cursor = abs + prefix.len();
                } else {
                    break;
                }
            }
        }
    }
    cleaned.push_str(&text[cursor..]);
    ParsedResponse {
        cleaned_text: cleaned.trim().to_string(),
        markers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cleanly_emitted_marker() {
        let r = parse_markers(
            r#"I'll save that.

[[MEMORY_OP:save_semantic:{"scope":"experiment","summary":"x > y","details":{"k":1}}]]

Done."#,
        );
        assert_eq!(r.markers.len(), 1);
        assert_eq!(r.markers[0].op, "save_semantic");
        assert_eq!(r.markers[0].payload["scope"], "experiment");
        assert_eq!(r.markers[0].payload["details"]["k"], 1);
        assert!(r.cleaned_text.contains("I'll save that"));
        assert!(r.cleaned_text.contains("Done."));
        assert!(!r.cleaned_text.contains("MEMORY_OP"));
    }

    #[test]
    fn skips_invalid_json() {
        let r = parse_markers("text [[MEMORY_OP:save_semantic:{not json}]] more");
        assert!(r.markers.is_empty());
        assert!(r.cleaned_text.contains("text"));
        assert!(r.cleaned_text.contains("more"));
    }

    #[test]
    fn handles_multiple_markers() {
        let r = parse_markers(
            r#"a [[MEMORY_OP:save_semantic:{"scope":"x","summary":"y"}]] b [[MEMORY_OP:save_behavior:{"pattern":"p","notes":"n"}]] c"#,
        );
        assert_eq!(r.markers.len(), 2);
        assert_eq!(r.markers[0].op, "save_semantic");
        assert_eq!(r.markers[1].op, "save_behavior");
        assert!(r.cleaned_text.starts_with("a"));
        assert!(r.cleaned_text.ends_with("c"));
    }

    #[test]
    fn handles_nested_braces_in_json() {
        let r = parse_markers(
            r#"x [[MEMORY_OP:save_semantic:{"a":{"b":{"c":1}},"summary":"deep"}]] y"#,
        );
        assert_eq!(r.markers.len(), 1);
        assert_eq!(r.markers[0].payload["a"]["b"]["c"], 1);
        assert_eq!(r.markers[0].payload["summary"], "deep");
    }

    /// Real-world stress test: a `details` field containing escaped JSON
    /// (a string that itself contains `{`, `}`, escaped quotes, and arrays).
    /// Reproduces the failure observed when the generation agent emitted
    /// `record_hypothesis` with `details` as a JSON string.
    #[test]
    fn parses_realistic_record_hypothesis_marker() {
        let marker = r#"[[MEMORY_OP:record_hypothesis:{"summary":"H0 persistence predicts robustness","details":"{\"statement\":\"x\",\"mechanism\":[\"a\",\"b\"],\"entities\":{\"k\":\"v\"}}"}]]"#;
        let r = parse_markers(marker);
        assert_eq!(
            r.markers.len(),
            1,
            "expected 1 marker, got {}; cleaned={:?}",
            r.markers.len(),
            r.cleaned_text
        );
        assert_eq!(r.markers[0].op, "record_hypothesis");
        assert_eq!(r.markers[0].payload["summary"], "H0 persistence predicts robustness");
        // details is a STRING containing escaped JSON
        let details_str = r.markers[0].payload["details"].as_str().unwrap();
        assert!(details_str.contains("\"statement\":\"x\""));
        assert!(details_str.contains("\"entities\":{\"k\":\"v\"}"));
    }

    /// LLMs frequently truncate the canonical `]]` to a single `]` when
    /// emitting markers at end of turn. Accept that.
    #[test]
    fn accepts_single_close_bracket_terminator() {
        let truncated = r#"[[MEMORY_OP:save_semantic:{"scope":"experiment","summary":"x"}]"#;
        let r = parse_markers(truncated);
        assert_eq!(r.markers.len(), 1, "single ] should terminate");
        assert_eq!(r.markers[0].op, "save_semantic");
    }

    #[test]
    fn accepts_single_close_bracket_followed_by_newline() {
        let truncated = "[[MEMORY_OP:save_semantic:{\"scope\":\"experiment\",\"summary\":\"x\"}]\n";
        let r = parse_markers(truncated);
        assert_eq!(r.markers.len(), 1, "single ] + newline should terminate");
        assert_eq!(r.markers[0].op, "save_semantic");
    }

    // ---- LLM-tolerance edge cases ----
    //
    // Each test reproduces a real failure mode observed when the LLM
    // emits markers with extra, missing, or wrong-case brackets.

    /// LLMs occasionally emit 3 or more trailing `]`. Consume them all
    /// so they don't leak into cleaned_text as visible garbage.
    #[test]
    fn consumes_extra_trailing_brackets() {
        let text = "[[MEMORY_OP:save_semantic:{\"scope\":\"x\",\"summary\":\"y\"}]]]] trailing";
        let r = parse_markers(text);
        assert_eq!(r.markers.len(), 1, "marker parsed despite extra ]]");
        assert_eq!(r.markers[0].op, "save_semantic");
        assert!(
            !r.cleaned_text.contains(']'),
            "all trailing ] consumed; cleaned={:?}",
            r.cleaned_text
        );
        assert!(r.cleaned_text.contains("trailing"));
    }

    /// Five extra `]` should still parse — bounded by 10 in the parser.
    #[test]
    fn consumes_many_extra_trailing_brackets() {
        let text = "[[MEMORY_OP:save_semantic:{\"scope\":\"x\",\"summary\":\"y\"}]]]]]]";
        let r = parse_markers(text);
        assert_eq!(r.markers.len(), 1);
        assert!(!r.cleaned_text.contains(']'));
    }

    /// LLMs sometimes lowercase the prefix. Accept it — case is
    /// cosmetic; the op name is the semantic identifier.
    #[test]
    fn accepts_lowercase_prefix() {
        let text = "[[memory_op:save_semantic:{\"scope\":\"x\",\"summary\":\"y\"}]]";
        let r = parse_markers(text);
        assert_eq!(r.markers.len(), 1, "lowercase prefix accepted");
        assert_eq!(r.markers[0].op, "save_semantic");
    }

    /// LLMs occasionally add whitespace after the colon: `: {` instead
    /// of `:{`. Tolerate it.
    #[test]
    fn accepts_whitespace_after_colon() {
        let text = "[[MEMORY_OP:save_semantic: {\"scope\":\"x\",\"summary\":\"y\"}]]";
        let r = parse_markers(text);
        assert_eq!(r.markers.len(), 1, "space after colon accepted");
        assert_eq!(r.markers[0].payload["scope"], "x");
    }

    /// Empty payload (`[[MEMORY_OP:op:]]`) is a malformed marker. The
    /// parser must NOT extract it, must NOT crash, and must NOT swallow
    /// the following text. The malformed prefix is consumed (logged as
    /// a warning) so subsequent prose/markers parse normally.
    #[test]
    fn empty_payload_marks_no_marker_extracted() {
        let text = "before [[MEMORY_OP:save_semantic:]] after";
        let r = parse_markers(text);
        assert!(r.markers.is_empty(), "empty payload = no marker");
        // Surrounding text survives — the malformed prefix doesn't
        // swallow the rest of the response.
        assert!(r.cleaned_text.contains("before"));
        assert!(r.cleaned_text.contains("after"));
    }

    /// Unclosed marker (no `]]`, no `}`, no terminator at all). The
    /// parser skips it without crashing. The half-marker is consumed
    /// (logged as warning) and surrounding text survives.
    #[test]
    fn unclosed_marker_does_not_crash() {
        let text = "preamble [[MEMORY_OP:save_semantic:{\"scope\":\"x\"";
        let r = parse_markers(text);
        assert!(r.markers.is_empty());
        assert!(r.cleaned_text.contains("preamble"));
    }

    /// A mix: one valid marker, one malformed (empty payload), one
    /// valid. Parser must extract the two valid markers and ignore
    /// the malformed one.
    #[test]
    fn mixed_valid_and_malformed_markers() {
        let text = r#"
            [[MEMORY_OP:save_semantic:{"scope":"x","summary":"first"}]]
            oops this one is broken [[MEMORY_OP:save_behavior:]]
            and another good one [[MEMORY_OP:save_semantic:{"scope":"y","summary":"second"}]]
            done.
        "#;
        let r = parse_markers(text);
        assert_eq!(r.markers.len(), 2, "valid markers extracted");
        assert_eq!(r.markers[0].op, "save_semantic");
        assert_eq!(r.markers[0].payload["summary"], "first");
        assert_eq!(r.markers[1].op, "save_semantic");
        assert_eq!(r.markers[1].payload["summary"], "second");
        // The malformed marker stays in cleaned_text as a hint.
        assert!(r.cleaned_text.contains("save_behavior"));
    }

    /// Uppercase prefix should still work (canonical).
    #[test]
    fn uppercase_prefix_still_works() {
        let text = "[[MEMORY_OP:save_semantic:{\"scope\":\"x\",\"summary\":\"y\"}]]";
        let r = parse_markers(text);
        assert_eq!(r.markers.len(), 1);
    }

    /// Stray `[` in prose that doesn't form a full prefix should be
    /// ignored — the parser only matches when `MEMORY_OP:` follows.
    #[test]
    fn stray_open_bracket_is_ignored() {
        let text = "Here is [a bracket] and [[a double bracket] but no marker.";
        let r = parse_markers(text);
        assert!(r.markers.is_empty());
        assert!(r.cleaned_text.contains("[a bracket]"));
    }

    /// A pathological run of 20+ `]` doesn't hang or panic. The parser
    /// caps trailing-`]` consumption at 10 after each marker; the rest
    /// stays in cleaned_text but doesn't hang the parser.
    #[test]
    fn pathological_many_brackets_does_not_hang() {
        let text = "[[MEMORY_OP:save_semantic:{\"scope\":\"x\",\"summary\":\"y\"}]" .to_string() + &"]".repeat(20);
        let r = parse_markers(&text);
        assert_eq!(r.markers.len(), 1);
        // Some `]` chars remain in cleaned_text (parser caps at 10) but
        // the run didn't hang.
    }
}

