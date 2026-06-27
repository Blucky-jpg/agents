//! The 6 co-scientist agents, named after the community / paper:
//!
//!   - `supervisor`  — parses the goal into a research plan and
//!                      dispatches tasks (orchestrator)
//!   - `generation`  — proposes new hypotheses (literature + debate)
//!   - `reflection`  — reviews, observes, and verifies hypotheses
//!   - `ranking`     — runs the tournament (pairwise + debate)
//!   - `evolution`   — combines, simplifies, makes feasible, or
//!                      out-of-box re-imagines top hypotheses
//!   - `metareview`  — synthesizes system-wide feedback + final overview
//!
//! Each agent has a short role description and the modes (community
//! prompt basenames) it owns. The actual prompts live in
//! `co-scientist/prompts/*.md` and are loaded by [`crate::prompts`].

use crate::prompts::AgentMode;

#[derive(Debug, Clone)]
pub struct Agent {
    pub name: &'static str,
    pub role: &'static str,
    pub modes: &'static [AgentMode],
}

// Inline `&[AgentMode]` constants because `AgentMode::modes_for` is
// not const-callable.
const SUPERVISOR_MODES: &[AgentMode] = &[AgentMode::ParseGoal];
const GENERATION_MODES: &[AgentMode] = &[AgentMode::GenerationLiterature, AgentMode::GenerationDebate];
const REFLECTION_MODES: &[AgentMode] = &[
    AgentMode::ReflectionReview,
    AgentMode::ReflectionObservation,
    AgentMode::ReflectionVerification,
];
const RANKING_MODES: &[AgentMode] = &[AgentMode::RankingPairwise, AgentMode::RankingDebate];
const EVOLUTION_MODES: &[AgentMode] = &[
    AgentMode::EvolutionCombine,
    AgentMode::EvolutionSimplify,
    AgentMode::EvolutionFeasibility,
    AgentMode::EvolutionOutOfBox,
];
const METAREVIEW_MODES: &[AgentMode] = &[AgentMode::MetaReviewSystem, AgentMode::MetaReviewFinal];

pub const AGENTS: &[Agent] = &[
    Agent {
        name: "supervisor",
        role: "Parses the goal into a research plan and dispatches generation / reflection / ranking / evolution / metareview tasks through the durable queue.",
        modes: SUPERVISOR_MODES,
    },
    Agent {
        name: "generation",
        role: "Proposes novel hypotheses via literature review and simulated multi-expert debate.",
        modes: GENERATION_MODES,
    },
    Agent {
        name: "reflection",
        role: "Reviews hypotheses for novelty, correctness, and testability; runs observation-based and deep-verification passes.",
        modes: REFLECTION_MODES,
    },
    Agent {
        name: "ranking",
        role: "Runs the Elo tournament: pairwise comparisons and multi-turn debates between competing hypotheses.",
        modes: RANKING_MODES,
    },
    Agent {
        name: "evolution",
        role: "Improves top-ranked hypotheses via combine / simplify / feasibility / out-of-box re-imagination strategies.",
        modes: EVOLUTION_MODES,
    },
    Agent {
        name: "metareview",
        role: "Synthesizes system-wide feedback for future generations and produces the final research overview.",
        modes: METAREVIEW_MODES,
    },
];
