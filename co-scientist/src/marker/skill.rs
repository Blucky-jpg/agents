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
pub const SKILL: &str = include_str!("../../SKILL.md");

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
        // No `]]` terminator. Accept the marker anyway — the JSON
        // body is well-formed (we brace-counted it), and LLMs
        // frequently forget to close the marker when their response
        // runs long. Observed in the 2026-07-01 "Topologie of Neural
        // nets" session: 2 of 3 generation turns emitted complete
        // JSON bodies but no `]]`, and the parser silently dropped
        // them — `dispatched: 0` with no `memory_op_failed` event,
        // so the failure-stats counter showed 0 and the session
        // stalled with only 1 of 3 hypotheses recorded.
        //
        // Log at INFO level so this case is visible in production
        // logs. The strict-mode rejection is preserved for genuinely
        // malformed payloads (incomplete JSON, junk text where the
        // payload should be) — those are caught earlier by the
        // `json_end = json_end?` short-circuit above.
        tracing::info!(
            op = op_name,
            json_len = json_end - json_start,
            "marker accepted without trailing ']]'; LLM probably forgot to close the marker"
        );
    }
    let marker_end = json_end + term_len;
    Some((op_name, &text[json_start..json_end], marker_start, marker_end))
}

/// One malformed `[[MEMORY_OP:...]]` (or `[[memory_op:...]]`) prefix
/// found in an LLM response. Used by `warn_about_malformed_prefixes`
/// to surface bad markers, and exposed as data so the detection rule
/// can be unit-tested without a `tracing` subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedMarker {
    /// Byte position of the `[[MEMORY_OP:` / `[[memory_op:` prefix.
    pub position: usize,
    pub kind: MalformedKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedKind {
    /// Marker had something after the op_name but it wasn't a JSON object.
    EmptyOrNonObjectPayload,
    /// Marker had no terminator at all (no `:` after the prefix, or
    /// no bytes after the op_name).
    Unclosed,
}

/// Pure: walk `text` and identify every `[[MEMORY_OP:` / `[[memory_op:`
/// prefix that doesn't have a JSON object payload. Returns the position
/// and kind of each malformed marker.
///
/// Must agree with `find_next_marker` on what counts as a valid marker.
/// Both use the rule: the FIRST colon after the prefix separates the
/// op_name from the JSON payload. Walk past that colon (and any
/// whitespace) and the next non-whitespace byte should be `{`. The
/// previous version walked past colons IMMEDIATELY after the prefix,
/// which fired false positives on every valid marker whose op_name
/// was non-empty (e.g. `[[MEMORY_OP:save_semantic:{...}]]` was fine
/// because `s` isn't `:`, but `[[MEMORY_OP:memory_op:{...}]]` warned
/// because `m` isn't `:`).
pub(crate) fn find_malformed_prefixes(text: &str) -> Vec<MalformedMarker> {
    const PREFIXES: &[&str] = &["[[MEMORY_OP:", "[[memory_op:"];
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut search_from = 0usize;
    while let Some((prefix, rel_offset)) = PREFIXES
        .iter()
        .filter_map(|p| text.get(search_from..).and_then(|s| s.find(p).map(|o| (*p, o))))
        .min_by_key(|(_, o)| *o)
    {
        let abs = rel_offset + search_from;
        let after = abs + prefix.len();
        // Find the FIRST colon after the prefix. The marker format is
        // `[[MEMORY_OP:<op_name>:<json>]]` — that colon is the boundary
        // between op_name and JSON. The op_name itself is opaque to us
        // (the parser accepts any ASCII identifier-shaped token).
        let Some(colon_off) = text.get(after..).and_then(|s| s.find(':')) else {
            out.push(MalformedMarker { position: abs, kind: MalformedKind::Unclosed });
            search_from = abs + prefix.len();
            continue;
        };
        let mut probe = after + colon_off + 1;
        // Skip whitespace before the JSON. Some models emit `: {…}`.
        while probe < bytes.len() && (bytes[probe] == b' ' || bytes[probe] == b'\t') {
            probe += 1;
        }
        match bytes.get(probe) {
            Some(b'{') => {
                // Valid: the JSON payload starts here. Could still be
                // malformed downstream (unclosed brace), but
                // `find_next_marker` reports that via its own path.
            }
            Some(_) => {
                out.push(MalformedMarker {
                    position: abs,
                    kind: MalformedKind::EmptyOrNonObjectPayload,
                });
            }
            None => {
                out.push(MalformedMarker {
                    position: abs,
                    kind: MalformedKind::Unclosed,
                });
            }
        }
        search_from = abs + prefix.len();
    }
    out
}

/// Emit warnings for each malformed marker prefix found in `text`.
/// Used to surface marker errors so the LLM can self-correct on the
/// next turn.
fn warn_about_malformed_prefixes(text: &str) {
    for m in find_malformed_prefixes(text) {
        match m.kind {
            MalformedKind::EmptyOrNonObjectPayload => tracing::warn!(
                position = m.position,
                "skill marker has empty or non-object payload; skipping"
            ),
            MalformedKind::Unclosed => tracing::warn!(
                position = m.position,
                "skill marker is unclosed (no payload or terminator); skipping"
            ),
        }
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

    /// Regression for the 2026-07-01 silent-drop bug: the LLM emits a
    /// well-formed JSON body but forgets the closing `]]`. The parser
    /// must still extract the marker — otherwise `dispatched: 0` with
    /// no `memory_op_failed` event, and the failure is invisible.
    /// Observed: 2 of 3 generation turns in the "Topologie of Neural
    /// nets" session lost their `record_hypothesis` markers this way,
    /// so the session ended up with 1 of 3 hypotheses and stalled.
    #[test]
    fn accepts_marker_without_trailing_brackets() {
        let text = r#"preamble [[MEMORY_OP:record_hypothesis:{"summary":"x","details":{"k":1}}"#;
        let r = parse_markers(text);
        assert_eq!(r.markers.len(), 1, "marker must be extracted despite missing ]]");
        assert_eq!(r.markers[0].op, "record_hypothesis");
        assert_eq!(r.markers[0].payload["summary"], "x");
        assert!(r.cleaned_text.contains("preamble"));
    }

    /// Same as above but with the marker at end of input — common case
    /// when the LLM emits the marker as the very last thing and
    /// forgets the closing `]]`.
    #[test]
    fn accepts_marker_at_eof_without_trailing_brackets() {
        let text = r#"[[MEMORY_OP:save_semantic:{"scope":"x","summary":"y"}"#;
        let r = parse_markers(text);
        assert_eq!(r.markers.len(), 1);
        assert_eq!(r.markers[0].op, "save_semantic");
        assert_eq!(r.markers[0].payload["summary"], "y");
    }

    /// Incomplete JSON (no closing `}`) is still rejected — only the
    /// missing terminator is relaxed, not the brace count.
    #[test]
    fn incomplete_json_still_rejected() {
        let text = "[[MEMORY_OP:save_semantic:{\"scope\":\"x\"";
        let r = parse_markers(text);
        assert!(r.markers.is_empty(), "incomplete JSON must still fail");
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

    // ---- find_malformed_prefixes ------------------------------------------

    /// The exact marker the ranking LLM emitted during the
    /// 2026-06-30 "Topologie of Neural nets" session: op_name is
    /// `memory_op` (the prefix hallucinated into the op slot), payload
    /// is a JSON object. The previous walker fired
    /// "empty or non-object payload" because it saw `m` (not `{`) right
    /// after the prefix and didn't know to walk past the op_name.
    /// After the fix this marker must be reported as valid (no entries).
    #[test]
    fn find_malformed_accepts_valid_marker_with_nontrivial_op_name() {
        let text = r#"...reasoning.

[[MEMORY_OP:memory_op:{"match_id":"H2_vs_H8","winner":"H2"}]]"#;
        let malformed = find_malformed_prefixes(text);
        assert!(
            malformed.is_empty(),
            "valid marker with op_name=memory_op must NOT be flagged; got {:?}",
            malformed,
        );
    }

    /// Canonical case from production: `[[MEMORY_OP:save_semantic:{...}]]`
    /// was always valid under the old walker (lucky: `s` isn't `:`),
    /// and must stay valid under the new walker.
    #[test]
    fn find_malformed_accepts_canonical_save_semantic() {
        let text = r#"[[MEMORY_OP:save_semantic:{"scope":"x","summary":"y"}]]"#;
        assert!(find_malformed_prefixes(text).is_empty());
    }

    /// Empty payload (no `{` after the op_name colon): real malformed.
    #[test]
    fn find_malformed_flags_empty_payload() {
        let text = "before [[MEMORY_OP:save_semantic:]] after";
        let malformed = find_malformed_prefixes(text);
        assert_eq!(malformed.len(), 1);
        assert_eq!(malformed[0].kind, MalformedKind::EmptyOrNonObjectPayload);
    }

    /// No terminator at all (prefix then nothing): unclosed.
    #[test]
    fn find_malformed_flags_unclosed_prefix() {
        let text = "tail [[MEMORY_OP:save_semantic:";
        let malformed = find_malformed_prefixes(text);
        assert_eq!(malformed.len(), 1);
        assert_eq!(malformed[0].kind, MalformedKind::Unclosed);
    }

    /// Whitespace after the colon (`{` after `: {`) must be tolerated
    /// the same way `find_next_marker` tolerates it.
    #[test]
    fn find_malformed_accepts_whitespace_before_json() {
        let text = r#"[[MEMORY_OP:save_semantic: {"scope":"x"}]]"#;
        assert!(find_malformed_prefixes(text).is_empty());
    }

    /// Lowercase prefix is treated identically to uppercase.
    #[test]
    fn find_malformed_accepts_lowercase_prefix() {
        let text = r#"[[memory_op:save_semantic:{"k":"v"}]]"#;
        assert!(find_malformed_prefixes(text).is_empty());
    }

    /// Non-object payload (e.g. an array) is flagged.
    #[test]
    fn find_malformed_flags_non_object_payload() {
        let text = r#"[[MEMORY_OP:noop:[1,2,3]]]"#;
        let malformed = find_malformed_prefixes(text);
        assert_eq!(malformed.len(), 1);
        assert_eq!(malformed[0].kind, MalformedKind::EmptyOrNonObjectPayload);
    }
}

