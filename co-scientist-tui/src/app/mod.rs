//! Application state shared between the UI thread and the agent task.
//!
//! `AppState` is held behind `Arc<Mutex<_>>` so the agent task (which owns the
//! `Runner`) can push assistant responses and marker lists while the UI thread
//! reads them on every redraw. Keeping the lock short (a few field writes or
//! a clone of a small struct) is enough — neither task holds it across an
//! `.await`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use co_scientist::agents::AGENTS;
use tokio::sync::{watch, Mutex};

use co_scientist::bus::MemoryEvent;

pub mod reducers;

/// Which panel has keyboard focus. `Tab`/`Shift+Tab` cycles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Input,
    Chat,
    SidebarTasks,
    SidebarMemory,
    Agents,
}

/// One entry in the visible chat log.
#[derive(Debug, Clone)]
pub enum ChatMsg {
    User(String),
    Assistant { agent: String, text: String },
    ToolCall { agent: String, tool: String, args: serde_json::Value },
    System(String),
}

/// Idle vs running indicator shown in the status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    Idle,
    Running,
}

/// A live supervisor task the sidebar renders.
#[derive(Debug, Clone)]
pub struct TaskEntry {
    pub id: String,
    pub worker: Option<String>,
    /// When the latest status arrived. Used by the sidebar to render
    /// "X seconds ago" hint when we have vertical room — currently stored
    /// only for forward compatibility.
    #[allow(dead_code)]
    pub at: Instant,
    pub action: Option<String>,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Claimed,
    /// Reserved for a future "worker picked up task but hasn't reported
    /// progress" event; the current supervisor only emits Claimed/Done/Failed.
    #[allow(dead_code)]
    Running,
    Done,
    Failed,
}

/// Compact memory-write event shown in the right sidebar.
#[derive(Debug, Clone)]
pub enum MemoryEntry {
    Semantic { scope: String, summary: String },
    Behavior { agent: String, pattern: String },
}

/// Highest-volume telemetry is dropped at the seam so it never reaches the
/// chat log. Kept for the curated `Sidebar` feeds (tasks + memory).
/// `EventLogged::type_` values that are pure scaffolding.
const DROPPED_EVENT_TYPES: &[&str] = &[
    "turn_started",
    "turn_completed",
    "task_scheduled",
    "task_enqueued",
    "tournament_recorded",
];

/// Should this bus event be filtered out before reaching the chat log?
pub fn should_drop_bus_event(ev: &MemoryEvent) -> bool {
    if let MemoryEvent::EventLogged { type_, .. } = ev {
        return DROPPED_EVENT_TYPES.contains(&type_.as_str());
    }
    false
}

/// Does this event produce a sidebar entry (and what kind)?
pub fn sidebar_entry(ev: &MemoryEvent) -> Option<SidebarEvent> {
    match ev {
        MemoryEvent::TaskClaimed { task_id, worker_id, action } => {
            Some(SidebarEvent::Task(TaskEntry {
                id: task_id.clone(),
                worker: Some(worker_id.clone()),
                action: Some(action.clone()),
                status: TaskStatus::Claimed,
                at: Instant::now(),
            }))
        }
        MemoryEvent::TaskCompleted { task_id, worker_id } => {
            Some(SidebarEvent::Task(TaskEntry {
                id: task_id.clone(),
                worker: Some(worker_id.clone()),
                action: None,
                status: TaskStatus::Done,
                at: Instant::now(),
            }))
        }
        MemoryEvent::TaskFailed { task_id, worker_id, error } => {
            Some(SidebarEvent::Task(TaskEntry {
                id: task_id.clone(),
                worker: Some(worker_id.clone()),
                action: Some(format!("failed: {}", truncate(error, 40))),
                status: TaskStatus::Failed,
                at: Instant::now(),
            }))
        }
        MemoryEvent::SemanticSaved { scope, summary, .. } => {
            Some(SidebarEvent::Memory(MemoryEntry::Semantic {
                scope: scope.clone(),
                summary: truncate(summary, 80),
            }))
        }
        MemoryEvent::BehaviorSaved { agent, pattern, .. } => {
            Some(SidebarEvent::Memory(MemoryEntry::Behavior {
                agent: agent.clone(),
                pattern: truncate(pattern, 80),
            }))
        }
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub enum SidebarEvent {
    Task(TaskEntry),
    Memory(MemoryEntry),
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n).collect();
        format!("{cut}…")
    }
}

#[derive(Debug)]
pub struct AppState {
    pub run_id: String,
    pub agent_idx: usize,
    pub busy: Busy,
    pub log: Vec<ChatMsg>,
    pub status: String,
    pub input: String,
    pub follow_tail: bool,
    pub tick: u64,

    // v4 — multi-panel state
    pub focus: Focus,
    pub chat_scroll: u16,
    /// Maximum legal value of `chat_scroll`. Recomputed by `draw_chat`
    /// every frame based on the rendered line count and the chat panel
    /// height. Read by `handle_key_chat` so input clamping can happen
    /// without re-rendering. Without this, `saturating_add(1)` on a u16
    /// walks the value up to 65535 across many PageDowns and the user
    /// gets stuck below the visible viewport until a draw happens.
    pub chat_max_scroll: u16,
    /// Visible height of the chat panel in rows, last computed by
    /// `draw_chat`. Used by `handle_key_chat` to make PageUp/PageDown
    /// scroll by one screen minus one row (industry convention) instead
    /// of a fixed 10 lines.
    pub chat_visible_h: u16,
    pub show_help: bool,
    pub sidebar_selected: usize,

    /// Index into `log` of the assistant message currently being streamed
    /// via `TurnDelta`. `None` while idle. Set by `TurnStarted`, advanced
    /// on each delta, cleared by `TurnDone` / `TurnFailed`.
    pub streaming_assistant: Option<usize>,
    /// Carry-over of an unterminated `[[MEMORY_OP:` prefix or body that
    /// spans the boundary between two `TurnDelta` frames. The marker
    /// scrubber (`scrub_markers` in main.rs) holds the half-marker here
    /// until the closing `]]` arrives; cleared on `TurnStarted` /
    /// `TurnDone` / `TurnFailed` so the carry never leaks across turns.
    pub partial_marker_carry: String,
    /// Model the runner is actually using. Updated on `TurnStarted`. The
    /// UI displays this verbatim instead of reading `CO_SCIENTIST_MODEL`
    /// independently (which could disagree if the env var changes after
    /// process start or is unset).
    pub model: String,

    // v4 — supervisor telemetry
    pub supervisor_running: bool,
    pub supervisor_session: Option<String>,
    pub supervisor_started_at: Option<Instant>,
    pub supervisor_stop_tx: Option<watch::Sender<bool>>,
    /// Last N tasks (newest at back). Old entries dropped.
    pub tasks: VecDeque<TaskEntry>,
    /// Last N memory writes.
    pub memory: VecDeque<MemoryEntry>,
    /// Task completion count (denominator for the status bar gauge).
    pub tasks_done: usize,
    pub tasks_failed: usize,

    // splash
    pub show_splash: bool,
}

pub(crate) const LOG_CAP: usize = 1000;
pub(crate) const SIDEBAR_CAP: usize = 64;

impl AppState {
    pub fn new(run_id: String) -> Self {
        Self {
            run_id,
            agent_idx: 0,
            busy: Busy::Idle,
            log: Vec::new(),
            status: "ready".to_string(),
            input: String::new(),
            follow_tail: true,
            tick: 0,

            focus: Focus::Input,
            chat_scroll: 0,
            chat_max_scroll: 0,
            chat_visible_h: 0,
            show_help: false,
            sidebar_selected: 0,
            streaming_assistant: None,
            partial_marker_carry: String::new(),
            model: String::new(),

            supervisor_running: false,
            supervisor_session: None,
            supervisor_started_at: None,
            supervisor_stop_tx: None,
            tasks: VecDeque::with_capacity(SIDEBAR_CAP),
            memory: VecDeque::with_capacity(SIDEBAR_CAP),
            tasks_done: 0,
            tasks_failed: 0,

            show_splash: true,
        }
    }

    pub fn current_agent_name(&self) -> &'static str {
        AGENTS[self.agent_idx % AGENTS.len()].name
    }

    pub fn cycle_agent(&mut self, dir: i32) {
        let n = AGENTS.len() as i32;
        let cur = self.agent_idx as i32;
        let next = ((cur + dir).rem_euclid(n)) as usize;
        self.agent_idx = next;
    }

    pub fn cycle_focus(&mut self, dir: i32) {
        let order = [Focus::Input, Focus::Chat, Focus::Agents, Focus::SidebarTasks, Focus::SidebarMemory];
        let n = order.len() as i32;
        let cur = order.iter().position(|f| *f == self.focus).unwrap_or(0) as i32;
        let next = ((cur + dir).rem_euclid(n)) as usize;
        self.focus = order[next];
    }

    pub fn push_log(&mut self, msg: ChatMsg) {
        self.log.push(msg);
        if self.log.len() > LOG_CAP {
            let drop = self.log.len() - LOG_CAP;
            self.log.drain(0..drop);
        }
    }

    pub fn push_task(&mut self, t: TaskEntry) {
        // De-dup by id: if a `Done` or `Failed` for the same id arrives,
        // update the existing row in place so the list is a live view.
        if let Some(existing) = self.tasks.iter_mut().find(|e| e.id == t.id) {
            *existing = t;
        } else {
            if self.tasks.len() >= SIDEBAR_CAP {
                self.tasks.pop_front();
            }
            self.tasks.push_back(t);
        }
    }

    pub fn push_memory(&mut self, m: MemoryEntry) {
        if self.memory.len() >= SIDEBAR_CAP {
            self.memory.pop_front();
        }
        self.memory.push_back(m);
    }
}

pub type SharedState = Arc<Mutex<AppState>>;
