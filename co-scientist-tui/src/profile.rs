//! Frame-level performance instrumentation.
//!
//! Measures the wall-clock cost of each phase of the event loop and
//! surfaces rolling statistics:
//!
//! - **`co_scientist_tui.log`** always gets a per-session summary on
//!   shutdown (p50/p95/p99/max of each phase + total frame time).
//! - **Status-bar badge** (toggleable with `Ctrl-P`) shows the latest
//!   frame's draw+total in ms. Off by default; this is a debug tool.
//!
//! ## Why this exists
//!
//! User-reported "input lag spikes while tapping through windows or
//! writing in chat, no LLM stream running." Two candidate causes:
//!
//! 1. **Draw is slow** — every 100ms the loop re-parses the entire
//!    chat log via `markdown::render`. AD11's "sub-millisecond in
//!    the noise" claim is wrong for non-trivial logs (the user just
//!    contradicted it).
//! 2. **Input poll blocks** — crossterm `event::poll(50ms)` can take
//!    longer under terminal load (Windows Terminal, slow PTYs).
//!
//! Without instrumentation, fixing the wrong cause wastes a session.
//! This module tells you which one in 30 seconds of use.
//!
//! ## Design
//!
//! - No new dependencies. `std::time::Instant` is enough.
//! - No raw samples stored — only aggregates. The log file is
//!   append-only and we don't want to bloat it on a long session.
//! - Hot-path cost is one `Instant::now()` per phase plus a
//!   subtract-and-store. Sub-microsecond per frame.
//! - `FrameProfile` is `Copy` so passing it to `ui::draw` doesn't
//!   require an Arc/Mutex (the event loop owns it; draw reads it
//!   through the parameter, same shape as `last_metrics` from C6).
//!
//! ## What is NOT here
//!
//! - No per-entry markdown timing (that's a deeper drill-down for
//!   the C5 cache decision; add later if the gauge confirms draw).
//! - No flamegraph integration. `cargo flamegraph` is the next
//!   step if the aggregate stats aren't enough.
//! - No automatic alert / throttling. The user reads the gauge.

/// Per-frame timing snapshot. Cheap to construct (one Instant::now()
/// per phase) and `Copy` so it can be passed by value through
/// `ui::draw` without allocation.
#[derive(Debug, Clone, Copy)]
pub struct FrameSample {
    /// Time spent draining pending `AgentToUi` messages.
    pub drain_us: u64,
    /// Time spent in `terminal.draw` (the entire draw path:
    /// status, body, agents, chat, sidebar, input, footer).
    pub draw_us: u64,
    /// Time spent in the post-draw tick + `chat_scroll` reset.
    pub tick_us: u64,
    /// Time spent in `event::poll(50ms)` waiting for the next input.
    /// Spikes here mean the terminal is slow to deliver events.
    pub poll_us: u64,
    /// Time spent in `handle_key` (only non-zero on frames where a
    /// key was dispatched).
    pub key_us: u64,
    /// Total wall-clock time from the start of the frame to just
    /// before `tick.tick().await`. The await on `tick` is the
    /// natural frame cadence (100ms) so it's excluded from "frame
    /// work."
    pub total_us: u64,
    /// Number of `AgentToUi` messages drained this frame. The
    /// event loop's reducer storms (TurnDelta flooding) will show
    /// up here. Useful for diagnosing "input lag during streaming".
    pub agent_msg_count: u32,
}

impl FrameSample {
    fn zero() -> Self {
        Self {
            drain_us: 0,
            draw_us: 0,
            tick_us: 0,
            poll_us: 0,
            key_us: 0,
            total_us: 0,
            agent_msg_count: 0,
        }
    }
}

/// Rolling aggregate over the last `WINDOW` frames. On shutdown we
/// log a summary; mid-session we expose the latest sample for the
/// status-bar badge.
const WINDOW: usize = 256;

#[derive(Debug)]
pub struct FrameProfile {
    samples: [FrameSample; WINDOW],
    idx: usize,
    count: usize,
}

impl Default for FrameProfile {
    fn default() -> Self {
        Self {
            samples: [FrameSample::zero(); WINDOW],
            idx: 0,
            count: 0,
        }
    }
}

impl FrameProfile {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one frame. Overwrites the oldest sample when the
    /// window is full (ring buffer).
    pub fn record(&mut self, s: FrameSample) {
        self.samples[self.idx] = s;
        self.idx = (self.idx + 1) % WINDOW;
        if self.count < WINDOW {
            self.count += 1;
        }
    }

    /// Latest sample (whatever was last recorded). Used by the
    /// status-bar badge so the user sees the most recent frame's
    /// draw time. Returns `None` if no frames have been recorded
    /// yet (shouldn't happen in practice).
    pub fn latest(&self) -> Option<FrameSample> {
        if self.count == 0 {
            None
        } else {
            // idx points at the next slot to write; the most recent
            // write was at (idx - 1) mod WINDOW.
            let last = (self.idx + WINDOW - 1) % WINDOW;
            Some(self.samples[last])
        }
    }

    /// Number of frames recorded so far. Used by the log summary to
    /// print "over N frames."
    pub fn frame_count(&self) -> usize {
        self.count
    }

    /// Max `AgentToUi` messages drained in any single frame. A high
    /// value here is the smoking gun for "input lag during streaming"
    /// — it means the agent task flooded the IPC channel and the
    /// reducer ran many times back-to-back. Useful even when drain
    /// wall-clock is low (the lock-and-mutate is cheap).
    pub fn max_agent_msgs(&self) -> u32 {
        self.samples[..self.count]
            .iter()
            .map(|s| s.agent_msg_count)
            .max()
            .unwrap_or(0)
    }

    /// Compute percentile stats per phase. Returns `None` for any
    /// phase with fewer than `count` samples. Uses nearest-rank
    /// percentile — good enough for a debug gauge, no need for
    /// linear interpolation.
    pub fn summary(&self) -> FrameSummary {
        let phases = [
            ("drain", self.collect(|s| s.drain_us)),
            ("draw", self.collect(|s| s.draw_us)),
            ("tick", self.collect(|s| s.tick_us)),
            ("poll", self.collect(|s| s.poll_us)),
            ("key", self.collect(|s| s.key_us)),
            ("total", self.collect(|s| s.total_us)),
        ];
        FrameSummary { phases }
    }

    fn collect(&self, f: impl Fn(&FrameSample) -> u64) -> Option<PhaseStats> {
        if self.count == 0 {
            return None;
        }
        let mut values: Vec<u64> = self.samples[..self.count].iter().map(f).collect();
        values.sort_unstable();
        let p50 = percentile(&values, 50);
        let p95 = percentile(&values, 95);
        let p99 = percentile(&values, 99);
        let max = *values.last().unwrap_or(&0);
        Some(PhaseStats { p50, p95, p99, max })
    }
}

/// Summary across all phases. Designed for a one-shot log dump on
/// shutdown — the field names are short so the formatted line fits
/// on a single log line.
#[derive(Debug)]
pub struct FrameSummary {
    pub phases: [(&'static str, Option<PhaseStats>); 6],
}

impl FrameSummary {
    /// Format as a single log line: `frame_profile: frames=N
    /// draw(p50=Xms p95=Yms p99=Zms max=Wms) total(...) ...`
    pub fn format(&self, frames: usize) -> String {
        let mut out = format!("frame_profile: frames={frames}");
        for (name, stats) in &self.phases {
            if let Some(s) = stats {
                out.push_str(&format!(
                    " {name}(p50={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms)",
                    us_to_ms(s.p50),
                    us_to_ms(s.p95),
                    us_to_ms(s.p99),
                    us_to_ms(s.max),
                ));
            }
        }
        out
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PhaseStats {
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
}

fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    // Nearest-rank percentile. Index = ceil((pct/100) * n) - 1,
    // clamped to the last index.
    let n = sorted.len();
    let rank = (pct * n).div_ceil(100).saturating_sub(1);
    sorted[rank.min(n - 1)]
}

fn us_to_ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

/// Format the latest sample as a tiny "draw Xms / total Yms" badge
/// for the status bar. The draw call site (`ui::draw_status`)
/// colors the badge WARNING when draw exceeds 16ms; the text
/// itself stays plain so the badge stays readable regardless of
/// color.
pub fn badge_text(latest: FrameSample) -> String {
    format!(
        "draw {:.1}ms · frame {:.1}ms",
        us_to_ms(latest.draw_us),
        us_to_ms(latest.total_us),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_nearest_rank() {
        // 10 samples [10, 20, ..., 100]. p50 = rank 4 → value 50.
        let mut v: Vec<u64> = (1..=10).map(|i| i * 10).collect();
        v.sort_unstable();
        assert_eq!(percentile(&v, 50), 50);
        // p95 = rank 9 → value 95... wait, ceil(95/100 * 10) - 1 = ceil(9.5) - 1 = 10 - 1 = 9.
        assert_eq!(percentile(&v, 95), 100);
        assert_eq!(percentile(&v, 99), 100);
    }

    #[test]
    fn percentile_handles_empty() {
        let v: Vec<u64> = vec![];
        assert_eq!(percentile(&v, 50), 0);
    }

    #[test]
    fn percentile_handles_single() {
        assert_eq!(percentile(&[42], 99), 42);
    }

    #[test]
    fn record_overwrites_ring_buffer() {
        let mut p = FrameProfile::new();
        for i in 0..WINDOW + 5 {
            p.record(FrameSample {
                total_us: i as u64,
                ..FrameSample::zero()
            });
        }
        assert_eq!(p.frame_count(), WINDOW);
        // The latest 5 writes (i = 256..260) are the most recent.
        // After 256+5 writes the ring has overwritten its head.
        // Verify the count is the window size and `latest` returns
        // the highest index written.
        let latest = p.latest().unwrap();
        assert_eq!(latest.total_us, WINDOW as u64 + 4);
    }

    #[test]
    fn summary_uses_window() {
        let mut p = FrameProfile::new();
        for i in 1..=100u64 {
            p.record(FrameSample {
                draw_us: i * 1000,
                ..FrameSample::zero()
            });
        }
        let s = p.summary();
        let draw = s.phases.iter().find(|(n, _)| *n == "draw").unwrap().1.unwrap();
        // 100 samples [1ms, 2ms, ..., 100ms]. p50 → rank 50 - 1 = 49 → 50ms.
        assert_eq!(draw.p50, 50000);
        // p95 → rank 95 - 1 = 94 → 95ms (index 94 of 0-indexed).
        assert_eq!(draw.p95, 95000);
        assert_eq!(draw.max, 100000);
    }

    #[test]
    fn badge_text_renders_two_metrics() {
        let s = FrameSample {
            drain_us: 100,
            draw_us: 12_345,
            tick_us: 200,
            poll_us: 5_000,
            key_us: 50,
            total_us: 18_000,
            agent_msg_count: 0,
        };
        assert_eq!(badge_text(s), "draw 12.3ms · frame 18.0ms");
    }
}