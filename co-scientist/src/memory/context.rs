//! Context assembly: combine events, semantic memories, behavior notes into a
//! [`Context`] ready to drop into a prompt; render to markdown.
//!
//! The 3-layer retrieval pattern (peek → timeline → observation) lives here.

use anyhow::{Context as _, Result};

use super::{
    helpers::{approx_tokens, cite, render_context},
    types::{Context, ContextLimits, Event, Observation, ObservationKind},
    Memory, MemoryError,
};

impl Memory {
    /// Fetch context for the next turn. Uses FTS5 (BM25-ranked) for
    /// the semantic + behavior retrieval. One-shot convenience: callers
    /// who want token-efficient retrieval should use
    /// [`Memory::peek_context`] + [`Memory::get_observation`] instead.
    pub async fn get_context(
        &self,
        run_id: &str,
        agent_name: &str,
        query: &str,
        limits: ContextLimits,
    ) -> Result<Context, MemoryError> {
        let agent_id = self.ensure_agent(agent_name).await?;
        let mut ctx = Context::default();

        // Recent events for this session.
        if limits.events > 0 {
            ctx.recent_events = self.recent_events_for_run(run_id, limits.events).await?;
        }

        // Top-K semantic memories via FTS5. Falls back to recency order
        // when the query is empty.
        if limits.semantic > 0 {
            ctx.semantic = self.search_semantic(query, limits.semantic, false).await?;
            // Bias toward run-scoped results when the query is
            // non-empty: swap if a non-run-scoped result is ranked
            // above a run-scoped one. (FTS5 ranks by relevance only.)
            if !query.trim().is_empty() {
                ctx.semantic.sort_by_key(|m| (m.run_id != run_id, -m.id));
            }
        }

        // Behavior notes for this agent. These are agent-scoped
        // (the agent's own self-critique) and are always relevant,
        // regardless of the current turn's query. Skip the FTS
        // filter — return the most recent N.
        if limits.behavior > 0 {
            ctx.behavior = self.search_behavior(agent_id, "", limits.behavior).await?;
        }

        ctx.rendered = render_context(&ctx, limits.max_tokens, limits.full_count);
        ctx.tokens_approx = approx_tokens(&ctx.rendered);
        Ok(ctx)
    }

    /// Layer 1 of the 3-layer retrieval pattern. Returns a compact
    /// `id + one-liner` per match, no full detail. The LLM should
    /// scan these, decide which IDs are relevant, then call
    /// [`Memory::get_observation`] for the full detail.
    pub async fn peek_context(
        &self,
        agent_name: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<super::types::PeekedMemory>, MemoryError> {
        let _ = self.ensure_agent(agent_name).await?;
        let mut out = Vec::new();
        out.extend(
            self.peek_semantic(query, limit)
                .await?
                .into_iter()
                .map(|p| super::types::PeekedMemory {
                    tokens_approx: approx_tokens(&p.summary),
                    ..p
                }),
        );
        out.extend(
            self.peek_behavior(agent_name, query, limit)
                .await?
                .into_iter()
                .map(|p| super::types::PeekedMemory {
                    tokens_approx: approx_tokens(&format!("{}: {}", p.label, p.summary)),
                    ..p
                }),
        );
        // Truncate to the limit (combined semantic + behavior).
        out.truncate(limit);
        Ok(out)
    }

    /// Layer 3 of the 3-layer retrieval pattern. Fetch a single
    /// semantic memory or behavior memory by id. `kind` chooses
    /// which table. Returns `None` if the id doesn't exist (or has
    /// been archived, in the semantic case).
    pub async fn get_observation(
        &self,
        kind: ObservationKind,
        id: i64,
    ) -> Result<Option<Observation>, MemoryError> {
        match kind {
            ObservationKind::Semantic => self.get_semantic_observation(id).await,
            ObservationKind::Behavior => self.get_behavior_observation(id).await,
        }
    }

    /// Layer 2 of the 3-layer retrieval pattern. Returns the events
    /// that happened in the same run as the observation, in a window
    /// around the observation's `created_at`. The window is
    /// `±around` events (not time).
    pub async fn get_timeline(
        &self,
        observation_id: i64,
        kind: ObservationKind,
        around: usize,
    ) -> Result<Vec<Event>, MemoryError> {
        match kind {
            ObservationKind::Semantic => {
                let Some((run_id, created_at)) = self.semantic_timeline_key(observation_id).await?
                else {
                    return Ok(Vec::new());
                };
                let sql = "SELECT id, run_id, agent_id, step_index, type, payload_json, created_at
                           FROM events
                           WHERE run_id = ?1
                           ORDER BY ABS(strftime('%s', created_at) - strftime('%s', ?2)) ASC
                           LIMIT ?3";
                let mut rows = self
                    .db
                    .conn()
                    .query(sql, (run_id, created_at, (around * 2 + 1) as i64))
                    .await?;
                collect_events(&mut rows).await
            }
            ObservationKind::Behavior => {
                // Behavior memories aren't tied to a run; use the most
                // recent events for any agent.
                if self.behavior_timeline_key(observation_id).await?.is_none() {
                    return Ok(Vec::new());
                }
                let sql = "SELECT id, run_id, agent_id, step_index, type, payload_json, created_at
                           FROM events
                           ORDER BY id DESC
                           LIMIT ?1";
                let mut rows = self
                    .db
                    .conn()
                    .query(sql, [(around * 2 + 1) as i64])
                    .await?;
                collect_events(&mut rows).await
            }
        }
    }

    /// Build a "## From prior sessions" block: the most relevant
    /// recent semantic memories + behavior notes for an agent, ranked
    /// by FTS5 relevance (and recency as a tiebreaker). This is the
    /// cross-session auto-injection: the Runner prepends it to the
    /// system prompt on `connect` so the model carries knowledge
    /// from previous runs into a new session.
    ///
    /// `max_chars` caps the rendered string length. Pass 0 for
    /// unlimited. The truncation cuts at the nearest newline before
    /// `max_chars` to avoid leaving half-lines.
    pub async fn prior_session_summary(
        &self,
        agent_name: &str,
        query: &str,
        semantic_limit: usize,
        behavior_limit: usize,
        max_chars: usize,
    ) -> Result<String, MemoryError> {
        let _ = self.ensure_agent(agent_name).await?;
        let mut out = String::from("## From prior sessions\n");
        let q = if query.trim().is_empty() {
            "research progress"
        } else {
            query
        };
        let semantic = self.search_semantic(q, semantic_limit, false).await?;
        if !semantic.is_empty() {
            out.push_str("### Prior semantic memories (most relevant first)\n");
            for m in &semantic {
                out.push_str(&format!("- [{}] {} {}\n", m.scope, m.summary, cite(m.id)));
            }
            out.push('\n');
        }
        let agent_id = self
            .agent_id(agent_name)
            .await?
            .ok_or_else(|| MemoryError::AgentNotFound(agent_name.to_string()))?;
        let behavior = self.search_behavior(agent_id, "", behavior_limit).await?;
        if !behavior.is_empty() {
            out.push_str("### Prior self-critique\n");
            for b in &behavior {
                out.push_str(&format!("- {}: {} {}\n", b.pattern, b.notes, cite(b.id)));
            }
            out.push('\n');
        }
        if semantic.is_empty() && behavior.is_empty() {
            out.push_str("(no prior sessions)\n");
        }
        // Truncate at nearest newline before max_chars to avoid
        // leaving a dangling half-line. Header is preserved.
        if max_chars > 0 && out.len() > max_chars {
            if let Some(nl) = out[..max_chars].rfind('\n') {
                out.truncate(nl + 1);
                out.push_str("[...truncated]\n");
            }
        }
        Ok(out)
    }
}

/// Drain a `Rows` cursor into `Vec<Event>`, parsing the JSON payload column.
async fn collect_events(rows: &mut crate::db::Rows) -> Result<Vec<Event>, MemoryError> {
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        let payload_str: Option<String> = row.get(5)?;
        let payload = payload_str
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .context("parsing event payload")?;
        out.push(Event {
            id: row.get(0)?,
            run_id: row.get(1)?,
            agent_id: row.get(2)?,
            step_index: row.get(3)?,
            r#type: row.get(4)?,
            payload,
            created_at: row.get(6)?,
        });
    }
    Ok(out)
}