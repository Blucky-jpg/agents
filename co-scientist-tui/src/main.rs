//! Co-scientist TUI — ratatui front-end for the co-scientist memory layer.
//!
//! Run with:
//!
//! ```bash
//! cargo run -p co-scientist-tui
//! ```
//!
//! Honours the same env vars as the CLI (`CO_SCIENTIST_DB`,
//! `CO_SCIENTIST_MODEL`, `CO_SCIENTIST_SKILLS`, `RUST_LOG`). If the DB file
//! doesn't exist the TUI runs the same init dance as `co-scientist init`
//! (open + seed default agents) so the user doesn't have to.
//!
//! ## Commands
//!
//! - `Enter` with non-empty input → `runner.turn()` against the active agent.
//! - `/start <goal>` → spawn a Supervisor + Worker + Consolidation session
//!   that runs the full research pipeline. Progress streams into the chat log
//!   via the `MemoryEvent` bus.
//! - `/stop` → shut the supervisor down cleanly.
//! - `/help` → list commands.
//! - `Tab` / `BackTab` → cycle focus between panels.
//! - `Ctrl-N` → start a new single-agent run (clears the chat log).
//! - `Ctrl-L` → clear chat log.
//! - `Ctrl-C` / `Esc` → quit.
//! - `?` → toggle the help overlay.

mod app;
mod ipc;
mod markdown;
mod marker_scrubber;
mod splash;
mod supervisor_session;
mod theme;
mod ui;

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::{mpsc, Mutex};
use tracing_subscriber::EnvFilter;

use co_scientist::runner::RunnerConfig;
use co_scientist::Memory;

use crate::app::{AppState, Busy, ChatMsg, Focus, SharedState};
use crate::ipc::{AgentToUi, UiToAgent};

#[tokio::main]
async fn main() -> Result<()> {
    let log_path = std::env::var("CO_SCIENTIST_TUI_LOG")
        .unwrap_or_else(|_| "co_scientist_tui.log".to_string());
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .with_writer(file)
            .try_init();
    }

    let db_path = db_path();
    let _mem = ensure_db(&db_path).await.context("initializing co-scientist DB")?;

    let mut terminal = setup_terminal().context("setting up terminal")?;
    let res = run_app(&mut terminal, db_path).await;
    restore_terminal(&mut terminal).ok();
    res
}

fn db_path() -> PathBuf {
    PathBuf::from(
        std::env::var("CO_SCIENTIST_DB").unwrap_or_else(|_| "co_scientist.db".to_string()),
    )
}

async fn ensure_db(path: &Path) -> Result<co_scientist::Db> {
    let existed = path.exists();
    let d = co_scientist::db::open(path.to_str().unwrap()).await?;
    if !existed {
        let mem = Memory::new(d.clone());
        let runner = co_scientist::Runner::new(
            mem,
            co_scientist::memory::new_run_id(),
            RunnerConfig::default(),
        );
        runner.seed_default_agents().await?;
        tracing::info!(?path, "initialized fresh DB and seeded agents");
    }
    Ok(d)
}

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Blinking block cursor makes the input focus unambiguous.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        SetCursorStyle::BlinkingBlock,
    )?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(t: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        t.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        // Restore the user's preferred cursor shape — default user shape
        // (steady bar or whatever their terminal was configured with).
        SetCursorStyle::DefaultUserShape,
    )?;
    t.show_cursor()?;
    Ok(())
}

async fn run_app(terminal: &mut Term, db_path: PathBuf) -> Result<()> {
    let run_id = co_scientist::memory::new_run_id();
    let state: SharedState = Arc::new(Mutex::new(AppState::new(run_id.clone())));

    let (tx_to_agent, rx_to_agent) = mpsc::unbounded_channel::<UiToAgent>();
    let (tx_to_ui, mut rx_to_ui) = mpsc::unbounded_channel::<AgentToUi>();

    let agent_state = state.clone();
    let agent_tx_to_ui = tx_to_ui.clone();
    tokio::spawn(async move {
        run_agent_task(db_path, run_id, agent_state, rx_to_agent, agent_tx_to_ui).await;
    });

    // The `model` displayed in the UI comes from `state.model`, which is
    // populated by the first `AgentToUi::TurnStarted` from the runner.
    // This avoids a hardcoded fallback string and keeps the UI truthful
    // about which model the runner is actually using (resolved from
    // `CO_SCIENTIST_MODEL` inside `RunnerConfig::default`).

    let mut tick = tokio::time::interval(Duration::from_millis(100));

    // Latest chat-panel metrics, owned by the event loop. `ui::draw`
    // updates this every frame; the input handler reads from it on
    // the next key event. Replaces the old `AppState::chat_max_scroll`
    // / `chat_visible_h` fields that the draw path used to write
    // back onto state (C6, 2026-06-28).
    let mut last_metrics: ui::ChatMetrics = ui::ChatMetrics {
        total: 0,
        visible_h: 0,
        max_scroll: 0,
    };

    loop {
        while let Ok(msg) = rx_to_ui.try_recv() {
            handle_agent_msg(&state, msg).await;
        }

        {
            let mut s = state.lock().await;
            // `terminal.draw`'s closure must return `()`, so we route
            // the metrics out through a `Cell`. Same thread, no
            // contention — the cell is just a return-value channel.
            let metrics_cell = std::cell::Cell::new(None);
            terminal.draw(|f| {
                metrics_cell.set(ui::draw(f, &mut s));
            })?;
            if let Some(m) = metrics_cell.into_inner() {
                last_metrics = m;
            }
            drop(s);
        }

        {
            let mut s = state.lock().await;
            s.tick = s.tick.wrapping_add(1);
            // While following the tail, keep `chat_scroll` pinned to the
            // current bottom so the *next* time the user hits `j`/`k`/
            // PageDown (which sets `follow_tail = false`), the anchor
            // helper in `handle_key_chat` has a meaningful starting
            // point. Previously this block set `chat_scroll = 0`, which
            // (a) was redundant because the draw path picks `max_scroll`
            // when `follow_tail` is on, and (b) meant leaving follow-tail
            // mode started from row 0 — so the first `j` press scrolled
            // one row down from the top instead of from where the user
            // was visually anchored.
            if s.follow_tail {
                s.chat_scroll = last_metrics.max_scroll as u16;
            }
        }

        let event_ready = event::poll(Duration::from_millis(50))?;
        if event_ready
            && let Event::Key(key) = event::read()?
            && handle_key(&state, key, &tx_to_agent, &last_metrics).await?
        {
            let _ = tx_to_agent.send(UiToAgent::Shutdown);
            break;
        }
        tick.tick().await;
    }

    Ok(())
}

async fn handle_agent_msg(state: &SharedState, msg: AgentToUi) {
    let mut guard = state.lock().await;
    crate::app::reducers::reduce(msg, &mut guard);
}


async fn handle_key(
    state: &SharedState,
    key: KeyEvent,
    tx: &mpsc::UnboundedSender<UiToAgent>,
    metrics: &ui::ChatMetrics,
) -> Result<bool> {
    let mut s = state.lock().await;

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Ok(true);
    }

    // Help overlay: any key (except Ctrl-C) dismisses it.
    if s.show_help {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') => s.show_help = false,
            _ => {}
        }
        return Ok(false);
    }

    // Splash: any key dismisses. Ctrl-C above already returned.
    if s.show_splash {
        s.show_splash = false;
        return Ok(false);
    }

    // Global shortcuts that work regardless of focus.
    match key.code {
        KeyCode::Char('?') => {
            s.show_help = true;
            return Ok(false);
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let new_id = co_scientist::memory::new_run_id();
            s.run_id = new_id;
            s.log.clear();
            s.status = "new run".to_string();
            s.follow_tail = true;
            tx.send(UiToAgent::Shutdown)?;
            return Ok(false);
        }
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            s.log.clear();
            s.status = "log cleared".to_string();
            s.follow_tail = true;
            return Ok(false);
        }
        _ => {}
    }

    // Per-focus handling.
    match s.focus {
        Focus::Input => {
            if handle_key_input(&mut s, key, tx)? {
                return Ok(true);
            }
        }
        Focus::Chat => handle_key_chat(&mut s, key, metrics),
        Focus::Agents => handle_key_agents(&mut s, key),
        Focus::SidebarTasks | Focus::SidebarMemory => handle_key_sidebar(&mut s, key),
    }

    Ok(false)
}

/// Returns `Ok(true)` if the user wants to quit.
fn handle_key_input(
    s: &mut AppState,
    key: KeyEvent,
    tx: &mpsc::UnboundedSender<UiToAgent>,
) -> Result<bool> {
    match key.code {
        KeyCode::Tab => s.cycle_focus(1),
        KeyCode::BackTab => s.cycle_focus(-1),
        KeyCode::Enter => {
            let raw = std::mem::take(&mut s.input);
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(false);
            }
            if let Some(cmd) = trimmed.strip_prefix('/') {
                let (name, arg) = cmd
                    .split_once(char::is_whitespace)
                    .map(|(n, a)| (n, a.trim()))
                    .unwrap_or((cmd, ""));
                match name {
                    "start" => {
                        if arg.is_empty() {
                            s.push_log(ChatMsg::System("usage: /start <goal>".to_string()));
                        } else if s.supervisor_running {
                            s.push_log(ChatMsg::System(
                                "session already running; /stop first".to_string(),
                            ));
                        } else {
                            s.push_log(ChatMsg::User(format!("/start {arg}")));
                            tx.send(UiToAgent::StartSupervisor {
                                goal: arg.to_string(),
                            })?;
                        }
                    }
                    "stop" => {
                        if !s.supervisor_running {
                            s.push_log(ChatMsg::System("no session running".to_string()));
                        } else if let Some(tx_stop) = s.supervisor_stop_tx.as_ref() {
                            let _ = tx_stop.send(true);
                            s.push_log(ChatMsg::System(
                                "stop signal sent; draining…".to_string(),
                            ));
                        }
                    }
                    "help" => {
                        s.show_help = true;
                    }
                    "test" => {
                        // Inject a fixed sequence of `ChatMsg` entries covering
                        // every visual element so the user can eyeball every
                        // style without spinning up an LLM. No IPC traffic —
                        // purely local. Markers in the assistant text are
                        // already scrubbed, so the `[[MEMORY_OP:…]]` lines
                        // below render as plain prose (which is what the
                        // user would see before `TurnDone` arrives).
                        s.push_log(ChatMsg::User("/test fixture".into()));
                        s.push_log(ChatMsg::Assistant {
                            agent: "supervisor".into(),
                            text: "# Test fixture\n\nThis entry covers the full markdown surface: `inline code`, **bold**, *italic*, ~~strike~~.\n\n## Bullets\n\n- alpha\n- beta\n- gamma\n\n## Numbered\n\n1. one\n2. two\n\n## Code fence\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\n## Quote\n\n> blockquote line\n\nEnd of fixture.".into(),
                        });
                        s.push_log(ChatMsg::ToolCall {
                            agent: "supervisor".into(),
                            tool: "save_semantic".into(),
                            args: serde_json::json!({
                                "scope": "hyp_3",
                                "summary": "porous carbon cathode with hierarchical pores",
                            }),
                        });
                        s.push_log(ChatMsg::Assistant {
                            agent: "supervisor".into(),
                            text: "follow-up after the tool call".into(),
                        });
                        s.push_log(ChatMsg::System("test fixture loaded".into()));
                        s.follow_tail = true;
                    }
                    other => {
                        s.push_log(ChatMsg::System(format!(
                            "unknown command: /{other} (try /help)"
                        )));
                    }
                }
            } else if s.supervisor_running {
                s.push_log(ChatMsg::System(
                    "supervisor running; /stop to end before chatting".to_string(),
                ));
                s.input = raw;
            } else if s.busy == Busy::Idle {
                let agent_name = s.current_agent_name().to_string();
                s.push_log(ChatMsg::User(raw.clone()));
                tx.send(UiToAgent::Turn {
                    agent_name,
                    user_text: raw,
                })?;
                s.busy = Busy::Running;
                s.status = "queued".to_string();
                s.follow_tail = true;
            } else {
                s.input = raw;
            }
        }
        KeyCode::Backspace => {
            s.input.pop();
        }
        KeyCode::Esc => {
            if s.input.is_empty() {
                // Empty input + Esc = quit. Same convention as the original
                // TUI — users build muscle memory for this.
                return Ok(true);
            } else {
                s.input.clear();
            }
        }
        KeyCode::Char(c) => {
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT)
            {
                s.input.push(c);
            }
        }
        _ => {}
    }
    Ok(false)
}

fn handle_key_chat(s: &mut AppState, key: KeyEvent, metrics: &ui::ChatMetrics) {
    // Page-scroll unit: one screen minus one row. Using a fixed 10 was a
    // UX bug — on a 30-row chat panel it scrolled less than half a screen.
    // `metrics.visible_h` is updated by `draw_chat` every frame; when
    // it's 0 the chat hasn't rendered yet, fall back to 1 so `j` still
    // does something. Before C6 these came from `s.chat_visible_h` /
    // `s.chat_max_scroll` (written by the draw path back onto state);
    // the event loop now owns them and passes them in directly.
    let page = metrics.visible_h.saturating_sub(1).max(1) as i32;
    let max_scroll = metrics.max_scroll as u16;
    // Helper: when leaving follow-tail mode, anchor `chat_scroll` to the
    // current bottom so the next `j`/`PageDown` scrolls *from* the bottom
    // by one (not from 0 + 1 = clamped-to-bottom). Without this anchor,
    // the first `j` after `G` or `f` silently no-ops because
    // `chat_scroll = 0` clamps against `max_scroll` to the same value the
    // user just left.
    let leave_tail = |s: &mut AppState| {
        if s.follow_tail {
            s.chat_scroll = max_scroll;
            s.follow_tail = false;
        }
    };
    match key.code {
        KeyCode::Tab => s.cycle_focus(1),
        KeyCode::BackTab => s.cycle_focus(-1),
        KeyCode::Char('j') | KeyCode::Down => {
            leave_tail(s);
            s.chat_scroll = s.chat_scroll.saturating_add(1).min(max_scroll);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            leave_tail(s);
            s.chat_scroll = s.chat_scroll.saturating_sub(1);
        }
        KeyCode::PageDown => {
            leave_tail(s);
            let next = (s.chat_scroll as i32 + page).max(0) as u32;
            s.chat_scroll = (next as u16).min(max_scroll);
        }
        KeyCode::PageUp => {
            leave_tail(s);
            let next = (s.chat_scroll as i32 - page).max(0) as u32;
            s.chat_scroll = next as u16;
        }
        // Vim convention: `g` = top, `G` = bottom. The previous code had
        // these swapped — `g` set `chat_scroll = u16::MAX` which the draw
        // path's `.min(max_scroll)` clamped to the bottom, so both `g`
        // and `G` ended up at the bottom.
        KeyCode::Char('G') => {
            s.follow_tail = true;
            // Pin immediately so a subsequent `j` (which calls
            // `leave_tail`) anchors to the right place without an extra
            // draw frame.
            s.chat_scroll = max_scroll;
        }
        KeyCode::Char('g') => {
            s.follow_tail = false;
            s.chat_scroll = 0;
        }
        KeyCode::Char('f') => {
            s.follow_tail = !s.follow_tail;
            if s.follow_tail {
                s.chat_scroll = max_scroll;
            }
        }
        KeyCode::End => {
            s.follow_tail = true;
            s.chat_scroll = max_scroll;
        }
        KeyCode::Home => {
            s.follow_tail = false;
            s.chat_scroll = 0;
        }
        _ => {}
    }
}

fn handle_key_agents(s: &mut AppState, key: KeyEvent) {
    match key.code {
        KeyCode::Tab => s.cycle_focus(1),
        KeyCode::BackTab => s.cycle_focus(-1),
        KeyCode::Char('j') | KeyCode::Down => s.cycle_agent(1),
        KeyCode::Char('k') | KeyCode::Up => s.cycle_agent(-1),
        _ => {}
    }
}

fn handle_key_sidebar(s: &mut AppState, key: KeyEvent) {
    match key.code {
        KeyCode::Tab => s.cycle_focus(1),
        KeyCode::BackTab => s.cycle_focus(-1),
        KeyCode::Up | KeyCode::Char('k') => {
            if s.sidebar_selected > 0 {
                s.sidebar_selected -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            s.sidebar_selected = s.sidebar_selected.saturating_add(1);
        }
        _ => {}
    }
}

async fn run_agent_task(
    db_path: PathBuf,
    initial_run_id: String,
    state: SharedState,
    mut rx: mpsc::UnboundedReceiver<UiToAgent>,
    tx: mpsc::UnboundedSender<AgentToUi>,
) {
    let mut run_id = initial_run_id;
    let mut runner: Option<co_scientist::Runner> = None;

    while let Some(msg) = rx.recv().await {
        match msg {
            UiToAgent::Shutdown => {
                runner = None;
                let new_id = {
                    let s = state.lock().await;
                    s.run_id.clone()
                };
                run_id = new_id;
                if let Err(e) = rebuild_runner(&db_path, &run_id, &mut runner).await {
                    let mut s = state.lock().await;
                    s.push_log(ChatMsg::System(format!("rebuild failed: {e}")));
                    s.busy = Busy::Idle;
                }
            }
            UiToAgent::StartSupervisor { goal } => {
                let sup_state = state.clone();
                let sup_tx = tx.clone();
                let sup_db = db_path.clone();
                supervisor_session::start(sup_db, goal, sup_state, sup_tx);
            }
            UiToAgent::Turn {
                agent_name,
                user_text,
            } => {
                if runner.is_none()
                    && let Err(e) = rebuild_runner(&db_path, &run_id, &mut runner).await
                {
                    let _ = tx.send(AgentToUi::TurnFailed {
                        agent_name,
                        error: format!("runner init failed: {e}"),
                    });
                    continue;
                }
                let runner = runner.as_mut().expect("just initialized");
                let agent = match co_scientist::agents::AGENTS
                    .iter()
                    .find(|a| a.name == agent_name)
                    .cloned()
                {
                    Some(a) => a,
                    None => {
                        let _ = tx.send(AgentToUi::TurnFailed {
                            agent_name,
                            error: "unknown agent".to_string(),
                        });
                        continue;
                    }
                };

                let _ = tx.send(AgentToUi::TurnStarted {
                    model: runner.model().to_string(),
                });

                // Streaming path: each text delta is forwarded to the UI as
                // it arrives from the LLM subprocess. The runner pushes raw
                // strings into a dedicated channel. The agent task itself
                // drains the channel and forwards deltas through the UI
                // channel — no separate task is needed and we get a clean
                // happens-before for `TurnDone` (all deltas are flushed
                // before we send it).
                let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<String>();
                let forward_tx = tx.clone();
                let forward_agent = agent.name.to_string();
                let forward_deltas = async {
                    while let Some(delta) = delta_rx.recv().await {
                        // If the UI has dropped its receiver (Ctrl-C, etc.),
                        // stop forwarding — there's no point piling up work.
                        if forward_tx
                            .send(AgentToUi::TurnDelta {
                                agent_name: forward_agent.clone(),
                                delta,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                };

                // Run the streaming turn concurrently with the forwarder.
                // The forwarder exits when `delta_tx` is dropped (i.e. when
                // `turn_stream` returns), so this `join` guarantees all
                // deltas have been forwarded before we move on.
                let turn_fut = runner.turn_stream(&agent, &user_text, Some(delta_tx));
                let (turn_result, _) = futures_join(turn_fut, forward_deltas).await;

                match turn_result {
                    Ok(outcome) => {
                        let markers = outcome.markers.as_ref().clone();
                        let _ = tx.send(AgentToUi::TurnDone {
                            cleaned_text: outcome.cleaned_text,
                            markers,
                            agent_name: agent.name.to_string(),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(AgentToUi::TurnFailed {
                            agent_name: agent.name.to_string(),
                            error: format!("{e:#}"),
                        });
                    }
                }
            }
        }
    }
}

/// `tokio::join!` re-exported under a single function name so we don't pull
/// the macro into scope at every call site.
async fn futures_join<A, B>(a: A, b: B) -> (A::Output, B::Output)
where
    A: std::future::Future,
    B: std::future::Future,
{
    tokio::join!(a, b)
}


async fn rebuild_runner(
    db_path: &Path,
    run_id: &str,
    slot: &mut Option<co_scientist::Runner>,
) -> Result<()> {
    let conn = co_scientist::db::Db::connect_fresh(db_path.to_str().unwrap()).await?;
    let d = co_scientist::Db::new(conn);
    let mem = Memory::new(d);
    *slot = Some(co_scientist::Runner::new(
        mem,
        run_id.to_string(),
        RunnerConfig::default(),
    ));
    Ok(())
}

#[cfg(test)]
mod scroll_tests {
    //! Scroll-key behavior regression tests. The bugs being prevented:
    //!
    //! 1. `g` / `G` were both ending up at the bottom (the `g` arm set
    //!    `chat_scroll = u16::MAX` which the draw path's `.min(max_scroll)`
    //!    clamped to `max_scroll`).
    //! 2. The first `j`/`k`/`PageDown` after entering follow-tail mode
    //!    silently no-oped because `chat_scroll` was 0 (set by the main
    //!    loop's "while tail" reset) and `0 + 1 = clamped-to-bottom`.
    //! 3. `PageUp`/`PageDown` scrolled by a fixed 10 lines regardless of
    //!    panel height.
    //! 4. `saturating_add` on u16 walked the scroll position past
    //!    `max_scroll` to 65535 across many PageDowns; the user got
    //!    stuck below the viewport until a draw happened.
    //!
    //! These tests build an `AppState` directly, simulate the metrics
    //! that `draw_chat` would have published, dispatch a key, and assert
    //! on the resulting `chat_scroll` + `follow_tail` state. They don't
    //! touch the renderer.

    use super::handle_key_chat;
    use crate::app::{AppState, ChatMsg};
    use crate::ui::ChatMetrics;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// Build a (state, metrics) pair for testing. Before C6 the
    /// metrics lived on `AppState` (the draw path wrote them back
    /// onto state); now they're a parameter to `handle_key_chat`.
    fn state_with_metrics(max_scroll: u16, visible_h: u16) -> (AppState, ChatMetrics) {
        let mut s = AppState::new("test-run".into());
        // Simulate a chat with enough content to be scrollable.
        for i in 0..50 {
            s.log.push(ChatMsg::User(format!("message {i}")));
        }
        let metrics = ChatMetrics {
            // `total` doesn't affect these tests; any positive value
            // works. 50 lines is consistent with the log we just
            // pushed.
            total: 50,
            visible_h: visible_h as usize,
            max_scroll: max_scroll as usize,
        };
        (s, metrics)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn g_goes_to_top() {
        let (mut s, m) = state_with_metrics(100, 20);
        s.chat_scroll = 80;
        s.follow_tail = false;
        handle_key_chat(&mut s, key(KeyCode::Char('g')), &m);
        assert!(!s.follow_tail, "g should leave tail mode");
        assert_eq!(s.chat_scroll, 0, "g should pin scroll to top");
    }

    #[test]
    fn capital_g_goes_to_bottom_and_enters_tail_mode() {
        let (mut s, m) = state_with_metrics(100, 20);
        s.chat_scroll = 0;
        s.follow_tail = false;
        handle_key_chat(&mut s, key(KeyCode::Char('G')), &m);
        assert!(s.follow_tail, "G should enter tail mode");
        assert_eq!(s.chat_scroll, 100, "G should pin scroll to max_scroll");
    }

    #[test]
    fn j_after_tail_mode_anchors_before_incrementing() {
        // Bug 2 regression: previously `chat_scroll = 0` (set every frame
        // by the main loop's tail reset), so the first `j` press
        // computed `0 + 1 = 1` — which was clamped to `max_scroll` (100)
        // in the draw path, leaving the user at the bottom even though
        // they explicitly said "scroll down". Now the `leave_tail`
        // helper anchors `chat_scroll = max_scroll` first, so the user
        // is genuinely at the bottom and `j` either moves down by 1
        // (when new content arrived) or stays at the bottom (when it
        // didn't). Either way, the position is meaningful.
        let (mut s, m) = state_with_metrics(100, 20);
        s.follow_tail = true;
        s.chat_scroll = 0; // the bogus value the main loop used to write
        handle_key_chat(&mut s, key(KeyCode::Char('j')), &m);
        assert!(!s.follow_tail, "j should leave tail mode");
        // 100 (anchor) + 1 = 101, clamped to 100. The user stays at the
        // bottom because there's nothing below them — that's correct.
        assert_eq!(s.chat_scroll, 100, "j should anchor to max_scroll then clamp");
    }

    #[test]
    fn j_from_middle_of_log_moves_down_one() {
        // Sanity check: j from row 40 in a log with max 100 should land
        // at row 41. Without the `leave_tail` anchor, this would still
        // work — the anchor only matters when leaving tail mode from 0.
        let (mut s, m) = state_with_metrics(100, 20);
        s.follow_tail = false;
        s.chat_scroll = 40;
        handle_key_chat(&mut s, key(KeyCode::Char('j')), &m);
        assert_eq!(s.chat_scroll, 41);
    }

    #[test]
    fn page_down_unit_equals_visible_h_minus_one() {
        // Bug 3 regression: PageDown scrolled by 10 regardless of panel.
        let (mut s, m) = state_with_metrics(200, 30);
        s.chat_scroll = 0;
        s.follow_tail = false;
        handle_key_chat(&mut s, key(KeyCode::PageDown), &m);
        // page = visible_h - 1 = 29, clamped to max_scroll = 200.
        assert_eq!(s.chat_scroll, 29);
    }

    #[test]
    fn page_down_clamps_at_max_scroll() {
        // Bug 4 regression: PageDown walked the value to u16::MAX across
        // many presses. Now it clamps at max_scroll.
        let (mut s, m) = state_with_metrics(50, 20);
        s.chat_scroll = 45;
        s.follow_tail = false;
        for _ in 0..100 {
            handle_key_chat(&mut s, key(KeyCode::PageDown), &m);
        }
        assert_eq!(s.chat_scroll, 50, "PageDown should clamp at max_scroll, not saturate");
    }

    #[test]
    fn j_clamps_at_max_scroll() {
        let (mut s, m) = state_with_metrics(50, 20);
        s.chat_scroll = 50;
        s.follow_tail = false;
        handle_key_chat(&mut s, key(KeyCode::Char('j')), &m);
        assert_eq!(s.chat_scroll, 50, "j should not exceed max_scroll");
    }

    #[test]
    fn k_clamps_at_zero() {
        let (mut s, m) = state_with_metrics(50, 20);
        s.chat_scroll = 0;
        s.follow_tail = false;
        handle_key_chat(&mut s, key(KeyCode::Char('k')), &m);
        assert_eq!(s.chat_scroll, 0, "k should not underflow");
    }

    #[test]
    fn f_toggles_follow_tail_and_anchors_when_entering() {
        let (mut s, m) = state_with_metrics(100, 20);
        s.chat_scroll = 5;
        s.follow_tail = false;
        handle_key_chat(&mut s, key(KeyCode::Char('f')), &m);
        assert!(s.follow_tail, "f should enter tail mode");
        assert_eq!(s.chat_scroll, 100, "entering tail should pin to max_scroll");

        handle_key_chat(&mut s, key(KeyCode::Char('f')), &m);
        assert!(!s.follow_tail, "second f should leave tail mode");
        assert_eq!(
            s.chat_scroll, 100,
            "leaving tail should preserve the anchor so next j works"
        );
    }

    #[test]
    fn home_end_keys_match_vim_conventions() {
        let (mut s, m) = state_with_metrics(100, 20);
        s.chat_scroll = 50;
        s.follow_tail = false;
        handle_key_chat(&mut s, key(KeyCode::Home), &m);
        assert_eq!(s.chat_scroll, 0);
        assert!(!s.follow_tail);

        handle_key_chat(&mut s, key(KeyCode::End), &m);
        assert!(s.follow_tail);
        assert_eq!(s.chat_scroll, 100);
    }

    #[test]
    fn page_down_with_zero_visible_h_falls_back_to_one() {
        // Defensive: if metrics haven't been published yet (chat panel
        // hasn't rendered), page should still do something instead of
        // dividing by zero or no-op'ing.
        let (mut s, m) = state_with_metrics(100, 0);
        s.chat_scroll = 0;
        s.follow_tail = false;
        handle_key_chat(&mut s, key(KeyCode::PageDown), &m);
        assert_eq!(s.chat_scroll, 1, "fallback page unit should be 1, not 0");
    }
}
