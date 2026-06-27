//! Structured research tools: hypothesis recording, peer review, and
//! tournament Elo updates. These write to both `semantic_memories`
//! (for retrieval) and the dedicated structured tables (`hypotheses`,
//! `tournament_matches`).

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::elo::{self, Winner};
use crate::hypothesis::{HypothesisRepo, HypothesisState};
use crate::tournament::TournamentRepo;

use super::{Tool, ToolCtx, ToolOutput};

/// Tool: record a structured hypothesis. Saves to both `hypotheses`
/// table (for tournament tracking) and `semantic_memories` (for search).
/// Returns the hypothesis id.
pub struct RecordHypothesisTool;

#[async_trait]
impl Tool for RecordHypothesisTool {
    fn name(&self) -> &str {
        "record_hypothesis"
    }
    fn description(&self) -> String {
        "Record a research hypothesis. Saves to the hypothesis tournament \
         and long-term memory. Returns the hypothesis id for use in reviews \
         and tournament matches."
            .to_string()
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "One-sentence hypothesis statement."
                },
                "details": {
                    "description": "Full hypothesis text with reasoning, evidence, and citations."
                },
                "parent_ids": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "IDs of parent hypotheses (for evolved/combined hypotheses)."
                }
            },
            "required": ["summary"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("record_hypothesis: missing 'summary'"))?;
        let details = args.get("details").cloned();
        let parent_ids: Vec<i64> = args
            .get("parent_ids")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or_default();
        // Save to semantic_memories for search/retrieval.
        let semantic_id = ctx
            .memory
            .save_semantic(&ctx.run_id, Some(&ctx.agent_name), "hypothesis", summary, details)
            .await?;
        // Save to hypotheses table for tournament tracking.
        let repo = HypothesisRepo::new(ctx.memory.db_arc());
        let hyp_id = repo
            .insert(&ctx.run_id, Some(semantic_id), &parent_ids, 1200.0)
            .await?;
        Ok(serde_json::json!({ "id": hyp_id, "semantic_id": semantic_id }))
    }
}

/// Tool: record a review of a hypothesis. Saves to `semantic_memories`
/// and advances the hypothesis state to `reviewed`.
pub struct RecordReviewTool;

#[async_trait]
impl Tool for RecordReviewTool {
    fn name(&self) -> &str {
        "record_review"
    }
    fn description(&self) -> String {
        "Record a review of a hypothesis. Saves the review to long-term \
         memory and marks the hypothesis as reviewed."
            .to_string()
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "hypothesis_id": {
                    "type": "integer",
                    "description": "The hypothesis being reviewed."
                },
                "summary": {
                    "type": "string",
                    "description": "One-sentence review verdict."
                },
                "details": {
                    "description": "Full review with novelty, correctness, testability scores and evidence."
                }
            },
            "required": ["hypothesis_id", "summary"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let hypothesis_id = args
            .get("hypothesis_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("record_review: missing 'hypothesis_id'"))?;
        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("record_review: missing 'summary'"))?;
        let details = args.get("details").cloned();
        // Save review to semantic_memories.
        let review_id = ctx
            .memory
            .save_semantic(&ctx.run_id, Some(&ctx.agent_name), "review", summary, details)
            .await?;
        // Advance hypothesis state.
        let repo = HypothesisRepo::new(ctx.memory.db_arc());
        repo.update_state(hypothesis_id, HypothesisState::Reviewed, false)
            .await?;
        Ok(serde_json::json!({ "id": review_id, "hypothesis_id": hypothesis_id }))
    }
}

/// Tool: record a tournament match result. Updates Elo on both hypotheses.
pub struct RecordTournamentMatchTool;

#[async_trait]
impl Tool for RecordTournamentMatchTool {
    fn name(&self) -> &str {
        "record_tournament_match"
    }
    fn description(&self) -> String {
        "Record the result of a pairwise hypothesis comparison. Updates Elo \
         ratings for both hypotheses."
            .to_string()
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "hypothesis_a": {
                    "type": "integer",
                    "description": "First hypothesis id."
                },
                "hypothesis_b": {
                    "type": "integer",
                    "description": "Second hypothesis id."
                },
                "winner": {
                    "type": "integer",
                    "enum": [0, 1, 2],
                    "description": "Match result: 1 = A wins, 2 = B wins, 0 = draw."
                },
                "rationale": {
                    "type": "string",
                    "description": "Explanation for the decision."
                }
            },
            "required": ["hypothesis_a", "hypothesis_b", "winner"]
        })
    }
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let hyp_a_id = args
            .get("hypothesis_a")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("record_tournament_match: missing 'hypothesis_a'"))?;
        let hyp_b_id = args
            .get("hypothesis_b")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("record_tournament_match: missing 'hypothesis_b'"))?;
        let winner_val = args
            .get("winner")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("record_tournament_match: missing 'winner'"))?;
        let rationale = args.get("rationale").and_then(|v| v.as_str());
        let winner = match winner_val {
            0 => Winner::Draw,
            1 => Winner::A,
            2 => Winner::B,
            _ => return Err(anyhow::anyhow!("winner must be 0, 1, or 2")),
        };
        let hyp_repo = HypothesisRepo::new(ctx.memory.db_arc());
        let tour_repo = TournamentRepo::new(ctx.memory.db_arc());
        // Fetch both hypotheses.
        let hyp_a = hyp_repo
            .get(hyp_a_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("hypothesis {hyp_a_id} not found"))?;
        let hyp_b = hyp_repo
            .get(hyp_b_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("hypothesis {hyp_b_id} not found"))?;
        // Record the match.
        let match_id = tour_repo
            .insert(&ctx.run_id, hyp_a_id, hyp_b_id, winner_val, rationale)
            .await?;
        // Update Elo. K=32 for new entrants (<3 matches), K=16 for seasoned.
        let k_a = if hyp_a.matches_played < 3 { 32.0 } else { 16.0 };
        let k_b = if hyp_b.matches_played < 3 { 32.0 } else { 16.0 };
        let k = (k_a + k_b) / 2.0;
        let (new_elo_a, new_elo_b) = elo::update_elo(hyp_a.elo, hyp_b.elo, winner, k);
        hyp_repo
            .update_elo(hyp_a_id, new_elo_a, hyp_a.matches_played + 1)
            .await?;
        hyp_repo
            .update_elo(hyp_b_id, new_elo_b, hyp_b.matches_played + 1)
            .await?;
        // Advance both to in_tournament if they were just reviewed.
        hyp_repo
            .update_state(hyp_a_id, HypothesisState::InTournament, false)
            .await?;
        hyp_repo
            .update_state(hyp_b_id, HypothesisState::InTournament, false)
            .await?;
        Ok(serde_json::json!({
            "match_id": match_id,
            "new_elo_a": new_elo_a,
            "new_elo_b": new_elo_b,
        }))
    }
}