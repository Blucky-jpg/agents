//! Reducers for `AgentToUi` messages.
//!
//! Each variant of `AgentToUi` knows how to mutate `AppState` when it
//! arrives from the agent task. Before C4 (2026-06-28) this logic
//! lived in a single 160-line `match` inside `main::handle_agent_msg`,
//! making it untestable in isolation and forcing every reducer fix to
//! land in the bootstrap file.
//!
//! The `AgentToUi::reduce` method on the enum dispatches to one named
//! method per variant. Each reducer is a small fn on `&mut AppState`,
//! with the same private helpers (`render_bus_event`) the original
//! inline code used.
//!
//! ## Why one method per variant (not one big match)
//!
//! The deletion test on the original 160-line match: would deleting
//! it concentrate complexity, or just move it? The original code was
//! eight intertwined match arms, each mutating 3–8 fields of
//! `AppState` with embedded business logic (TurnDone parses markers,
//! derives ToolCall entries, manages `partial_marker_carry`). The
//! "interface" of each reducer was implicit — no `TurnDone::apply` to
//! test in isolation. Splitting each arm into a named method
//! concentrates each reducer's logic + tests in one place; the dispatch
//! becomes a 1-liner.
//!
//! ## Cross-`.await` callbacks / state
//!
//! Reducers take `&mut AppState` (not `&mut SharedState`) because the
//! caller already holds the lock. None of the reducers `.await` —
//! `handle_agent_msg` locks once at the top, dispatches synchronously,
//! and releases. This is the same pattern the original code used; the
//! split doesn't change concurrency semantics.
//!
//! ## What lives here vs. `app.rs` vs. `main.rs`
//!
//! - `app.rs` — types (`AppState`, `ChatMsg`, `Focus`, `Busy`, etc.) +
//!   `sidebar_entry`/`should_drop_bus_event` (pure state-shape filters
//!   on `MemoryEvent`).
//! - `app/reducers.rs` (this module) — state-mutating transitions for
//!   `AgentToUi` variants + `render_bus_event` helper.
//! - `main.rs` — bootstrap, event loop, key dispatch, IPC plumbing.
//!   `handle_agent_msg` is now a 1-liner that calls
//!   `msg.reduce(&mut state.lock().await)`.

use std::time::Instant;

use co_scientist::bus::MemoryEvent;

use crate::app::{AppState, Busy, ChatMsg, LOG_CAP};
use crate::ipc::AgentToUi;

/// Dispatch a single `AgentToUi` message against the held `AppState`.
/// The caller is responsible for the `SharedState` lock.
pub fn reduce(msg: AgentToUi, state: &mut AppState) {
    match msg {
        AgentToUi::TurnStarted { model } => on_turn_started(state, model),
        AgentToUi::TurnDelta { agent_name, delta } => on_turn_delta(state, agent_name, delta),
        AgentToUi::TurnDone {
            cleaned_text,
            markers,
            agent_name,
        } => on_turn_done(state, cleaned_text, markers, agent_name),
        AgentToUi::TurnFailed { error, agent_name } => on_turn_failed(state, error, agent_name),
        AgentToUi::SupervisorStarted {
            session_id,
            stop_tx,
        } => on_supervisor_started(state, session_id, stop_tx),
        AgentToUi::SupervisorEvent(ev) => on_supervisor_event(state, ev),
        AgentToUi::SupervisorFinished { reason, session_id } => {
            on_supervisor_finished(state, reason, session_id)
        }
        AgentToUi::SupervisorFailed { error } => on_supervisor_failed(state, error),
    }
}

/// A turn has begun. Reset transient streaming state, pre-create the
/// assistant entry deltas will accumulate into, and update the model
/// label. Trims the log to `LOG_CAP` after the push.
fn on_turn_started(state: &mut AppState, model: String) {
    state.busy = Busy::Running;
    state.status = "calling claude CLI…".to_string();
    state.model = model;
    // Reset the partial-marker carry at the start of every turn so
    // any unconsumed marker tail from a prior (failed/aborted) turn
    // never leaks into the new turn's text.
    state.partial_marker_carry.clear();
    // Pre-create the assistant entry that deltas will accumulate into.
    // Empty text now; `TurnDelta` appends; `TurnDone` finalizes.
    let agent_name = state.current_agent_name().to_string();
    state.log.push(ChatMsg::Assistant {
        agent: agent_name,
        text: String::new(),
    });
    let cap_drop = state.log.len().saturating_sub(LOG_CAP);
    if cap_drop > 0 {
        state.log.drain(0..cap_drop);
    }
    state.streaming_assistant = Some(state.log.len() - 1);
}

/// A delta arrived. Scrub raw `[[MEMORY_OP:…:{json}]]` markers BEFORE
/// mutating the log so the live chat log doesn't render unparsed
/// markers until `TurnDone` arrives. `scrub_markers` is a pure
/// function in `main.rs` (C3 candidate for its own module).
fn on_turn_delta(state: &mut AppState, agent_name: String, delta: String) {
    let scrubbed = crate::scrub_markers(&mut state.partial_marker_carry, &delta);
    if let Some(idx) = state.streaming_assistant
        && let Some(ChatMsg::Assistant { agent, text }) = state.log.get_mut(idx)
    {
        // The agent name is set once on TurnStarted; subsequent
        // deltas carry it for redundancy but we trust the entry.
        if agent.is_empty() {
            *agent = agent_name;
        }
        if !scrubbed.is_empty() {
            text.push_str(&scrubbed);
        }
        state.follow_tail = true;
    }
}

/// The turn finished cleanly. Replace the streamed raw text with the
/// cleaned/marker-augmented final form. If a streaming entry exists,
/// mutate in place; otherwise (shouldn't happen, but defensively)
/// append a fresh one. Emit one `ChatMsg::ToolCall` per parsed marker
/// so the visual layout distinguishes tool calls from assistant text.
fn on_turn_done(
    state: &mut AppState,
    cleaned_text: String,
    markers: Vec<co_scientist::skill::Marker>,
    agent_name: String,
) {
    let mut final_text = cleaned_text;
    if !markers.is_empty() {
        let ops: Vec<String> = markers.iter().map(|m| m.op.clone()).collect();
        final_text.push_str(&format!("\n  ⚙ {}", ops.join(", ")));
        for m in &markers {
            state.log.push(ChatMsg::ToolCall {
                agent: agent_name.clone(),
                tool: m.op.clone(),
                args: m.payload.clone(),
            });
        }
    }
    if let Some(idx) = state.streaming_assistant.take() {
        if let Some(ChatMsg::Assistant { agent, text }) = state.log.get_mut(idx) {
            *agent = agent_name;
            *text = final_text;
        }
    } else {
        state.push_log(ChatMsg::Assistant {
            agent: agent_name,
            text: final_text,
        });
    }
    state.partial_marker_carry.clear();
    state.busy = Busy::Idle;
    state.status = "ready".to_string();
    state.follow_tail = true;
}

/// The turn failed. Remove the empty streaming entry if present so the
/// log doesn't show a blank assistant row, then push a system message.
fn on_turn_failed(state: &mut AppState, error: String, agent_name: String) {
    if let Some(idx) = state.streaming_assistant.take()
        && let Some(ChatMsg::Assistant { text, .. }) = state.log.get(idx)
        && text.is_empty()
    {
        state.log.remove(idx);
    }
    state.push_log(ChatMsg::System(format!("[{agent_name}] turn failed: {error}")));
    state.partial_marker_carry.clear();
    state.busy = Busy::Idle;
    state.status = "error".to_string();
    state.follow_tail = true;
}

/// A supervisor session has been spawned. Reset the sidebar counters,
/// store the stop signal, and announce the session in the chat log.
fn on_supervisor_started(
    state: &mut AppState,
    session_id: String,
    stop_tx: tokio::sync::watch::Sender<bool>,
) {
    state.supervisor_running = true;
    state.supervisor_session = Some(session_id.clone());
    state.supervisor_started_at = Some(Instant::now());
    state.supervisor_stop_tx = Some(stop_tx);
    state.tasks_done = 0;
    state.tasks_failed = 0;
    state.tasks.clear();
    state.status = format!("supervisor session {session_id}");
    state.input.clear();
    state.follow_tail = true;
    state.push_log(ChatMsg::System(format!(
        "supervisor session started: {session_id}"
    )));
}

/// A live `MemoryEvent` from the supervisor bus. Route to the sidebars
/// (tasks + memory) and to the chat log if it's not high-volume
/// scaffolding.
fn on_supervisor_event(state: &mut AppState, ev: MemoryEvent) {
    if let Some(se) = crate::app::sidebar_entry(&ev) {
        match se {
            crate::app::SidebarEvent::Task(t) => {
                match t.status {
                    crate::app::TaskStatus::Done => state.tasks_done += 1,
                    crate::app::TaskStatus::Failed => state.tasks_failed += 1,
                    _ => {}
                }
                state.push_task(t);
            }
            crate::app::SidebarEvent::Memory(m) => state.push_memory(m),
        }
    }
    if crate::app::should_drop_bus_event(&ev) {
        return;
    }
    state.push_log(ChatMsg::System(render_bus_event(&ev)));
    state.follow_tail = true;
}

/// Supervisor returned normally. Clear session state and announce.
fn on_supervisor_finished(state: &mut AppState, reason: String, session_id: String) {
    state.supervisor_running = false;
    state.supervisor_started_at = None;
    state.supervisor_stop_tx = None;
    state.status = format!("session {session_id} done: {reason}");
    state.push_log(ChatMsg::System(format!(
        "supervisor finished: {reason}"
    )));
}

/// Supervisor failed to start. Clear session state and announce the
/// error.
fn on_supervisor_failed(state: &mut AppState, error: String) {
    state.supervisor_running = false;
    state.supervisor_started_at = None;
    state.supervisor_stop_tx = None;
    state.status = "supervisor failed".to_string();
    state.push_log(ChatMsg::System(format!("supervisor failed: {error}")));
}

/// Render a `MemoryEvent` as a one-line string for the chat log.
/// High-volume events are filtered upstream by `should_drop_bus_event`.
fn render_bus_event(ev: &MemoryEvent) -> String {
    match ev {
        MemoryEvent::EventLogged {
            agent, type_, payload, ..
        } => match payload {
            Some(p) => format!("· {agent} {type_} {}", compact_json(p)),
            None => format!("· {agent} {type_}"),
        },
        MemoryEvent::MarkerFailed { agent, op, error } => {
            format!("! marker {op} from {agent}: {error}")
        }
        _ => String::new(),
    }
}

fn compact_json(v: &serde_json::Value) -> String {
    let s = v.to_string();
    truncate(&s, 80)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    //! Smoke tests for the reducers. The point of C4 was to make
    //! each variant testable in isolation — these tests exercise the
    //! single most safety-critical transition (the streaming
    //! TurnDelta→TurnDone handoff that the user's screenshot showed
    //! leaking `[[MEMORY_OP:…]]` markers during streaming). The other
    //! variants are simple field assignments and are covered by manual
    //! smoke testing in the TUI itself.
    //!
    //! We construct an `AppState` directly and call `reduce` against
    //! it. No `SharedState`, no `Runner`, no TUI render — proves the
    //! interface is module-scope testable.
    use super::reduce;
    use crate::app::{AppState, Busy, ChatMsg};
    use crate::ipc::AgentToUi;

    #[test]
    fn turn_started_pre_creates_streaming_entry() {
        let mut s = AppState::new("test-run".into());
        reduce(AgentToUi::TurnStarted { model: "sonnet".into() }, &mut s);

        assert_eq!(s.busy, Busy::Running);
        assert_eq!(s.model, "sonnet");
        assert!(s.partial_marker_carry.is_empty());
        // One Assistant entry exists with empty text; the streaming
        // index points at it.
        assert_eq!(s.log.len(), 1);
        assert!(matches!(&s.log[0], ChatMsg::Assistant { text, .. } if text.is_empty()));
        assert_eq!(s.streaming_assistant, Some(0));
    }

    #[test]
    fn turn_delta_scrubs_markers_before_appending() {
        // Regression test: the user's screenshot showed raw
        // `[[MEMORY_OP:memory_add:{...}]]` markers leaking into the
        // chat log during streaming. The TurnDelta reducer calls
        // `scrub_markers` BEFORE mutating the entry, so the rendered
        // log never sees the raw marker.
        let mut s = AppState::new("test-run".into());
        reduce(AgentToUi::TurnStarted { model: "sonnet".into() }, &mut s);
        reduce(
            AgentToUi::TurnDelta {
                agent_name: "supervisor".into(),
                delta: "before [[MEMORY_OP:noop:{}]] after".into(),
            },
            &mut s,
        );
        if let ChatMsg::Assistant { text, .. } = &s.log[0] {
            assert!(!text.contains("MEMORY_OP"), "raw marker leaked: {text:?}");
            assert!(text.contains("before"));
            assert!(text.contains("after"));
        } else {
            panic!("expected Assistant entry at idx 0");
        }
    }

    #[test]
    fn turn_done_replaces_streamed_text_and_pushes_tool_call_per_marker() {
        // The TurnDone reducer mutates the streaming entry in place
        // (replacing raw text with cleaned text) AND emits one
        // `ChatMsg::ToolCall` per parsed marker. Both behaviours
        // verified here.
        let mut s = AppState::new("test-run".into());
        reduce(AgentToUi::TurnStarted { model: "sonnet".into() }, &mut s);
        reduce(
            AgentToUi::TurnDelta {
                agent_name: "supervisor".into(),
                delta: "streamed draft".into(),
            },
            &mut s,
        );
        reduce(
            AgentToUi::TurnDone {
                cleaned_text: "final cleaned".into(),
                markers: vec![
                    co_scientist::skill::Marker {
                        op: "save_semantic".into(),
                        payload: serde_json::json!({"scope": "x"}),
                    },
                    co_scientist::skill::Marker {
                        op: "save_behavior".into(),
                        payload: serde_json::json!({"agent": "a"}),
                    },
                ],
                agent_name: "supervisor".into(),
            },
            &mut s,
        );

        // Three entries: the finalized Assistant + 2 ToolCalls.
        assert_eq!(s.log.len(), 3);
        if let ChatMsg::Assistant { text, .. } = &s.log[0] {
            // TurnDone appends an ops-summary footer to the cleaned
            // text so the chat log can show "⚙ save_semantic,
            // save_behavior" alongside the response body.
            assert!(text.starts_with("final cleaned"));
            assert!(text.contains("⚙ save_semantic, save_behavior"));
        } else {
            panic!("expected Assistant at idx 0");
        }
        assert!(matches!(&s.log[1], ChatMsg::ToolCall { tool, .. } if tool == "save_semantic"));
        assert!(matches!(&s.log[2], ChatMsg::ToolCall { tool, .. } if tool == "save_behavior"));
        // Streaming entry cleared, busy reset.
        assert_eq!(s.streaming_assistant, None);
        assert_eq!(s.busy, Busy::Idle);
    }

    #[test]
    fn turn_failed_drops_empty_streaming_entry() {
        // TurnFailed removes the empty streaming entry so the log
        // doesn't show a blank assistant row.
        let mut s = AppState::new("test-run".into());
        reduce(AgentToUi::TurnStarted { model: "sonnet".into() }, &mut s);
        assert_eq!(s.log.len(), 1);
        reduce(
            AgentToUi::TurnFailed {
                error: "claude CLI exited 1".into(),
                agent_name: "supervisor".into(),
            },
            &mut s,
        );
        // Empty streaming entry removed; only the System message remains.
        assert_eq!(s.log.len(), 1);
        assert!(matches!(&s.log[0], ChatMsg::System(t) if t.contains("turn failed")));
        assert_eq!(s.busy, Busy::Idle);
    }
}
