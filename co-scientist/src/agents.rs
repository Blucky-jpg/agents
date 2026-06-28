//! The 7 co-scientist agents, named after the community / paper:
//!
//!   - `supervisor`  — parses the goal into a research plan and
//!                      dispatches tasks (orchestrator)
//!   - `generation`  — proposes new hypotheses (literature + debate)
//!   - `reflection`  — reviews, observes, and verifies hypotheses
//!   - `ranking`     — runs the tournament (pairwise + debate)
//!   - `evolution`   — combines, simplifies, makes feasible, or
//!                      out-of-box re-imagines top hypotheses
//!   - `metareview`  — synthesizes system-wide feedback + final overview
//!   - `experiment`  — closes the empirical loop: designs, executes,
//!                      and evaluates Python experiments that test
//!                      hypotheses. The result feeds back into the
//!                      tournament via `reflection_on_result`.
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
    /// Short behavior preamble injected after the role block. Sets tone and
    /// posture only — never the task-specific rules (those live in the
    /// prompt .md files for each mode). Empty string means no override.
    pub personality: &'static str,
    /// Skill names resolved from the skills directory and appended to the
    /// system prompt after the personality block. Each name maps to a
    /// `SKILL.md` body that teaches the LLM how to perform a specific
    /// capability (e.g. `prose-style`, `blind-spot-detective`). Empty
    /// slice means no extra skills.
    pub skills: &'static [&'static str],
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
const EXPERIMENT_MODES: &[AgentMode] = &[
    AgentMode::ExperimentDesign,
    AgentMode::ExperimentExecute,
    AgentMode::ExperimentEvaluate,
];

pub const AGENTS: &[Agent] = &[
    Agent {
        name: "supervisor",
        role: "Parses the goal into a research plan and dispatches generation / reflection / ranking / evolution / metareview tasks through the durable queue.",
        modes: SUPERVISOR_MODES,
        personality: "Smart router, not a leader. Dispatch tasks; do not plan or evaluate downstream output. Pass through the agent's own 'good enough' signals — only block on a task that genuinely fails to complete.",
        skills: &[],
    },
    Agent {
        name: "generation",
        role: "Proposes novel hypotheses via literature review and simulated multi-expert debate.",
        modes: GENERATION_MODES,
        personality: "Cross-domain hunter. Recast one concept from an unrelated field per hypothesis — the analogy IS the mechanism, not decoration. Name the entities, the transfer rule, and the predicted outcome. Iterate one idea across multiple drafts; don't proliferate. Generate early, ship rough — let reflection and ranking filter.",
        skills: &[],
    },
    Agent {
        name: "reflection",
        role: "Reviews hypotheses for novelty, correctness, and testability; runs observation-based and deep-verification passes.",
        modes: REFLECTION_MODES,
        personality: "Analyst, not critic. Decompose the hypothesis into its claims, judge each on its own evidence. State findings plainly — strength, weakness, what's missing. Move to the next claim; don't wrap up with an overall judgment the parts don't jointly support.",
        skills: &[],
    },
    Agent {
        name: "ranking",
        role: "Runs the Elo tournament: pairwise comparisons and multi-turn debates between competing hypotheses.",
        modes: RANKING_MODES,
        personality: "Adaptive evaluator. Each pair rewards different traits — read the two hypotheses before applying any rule. Pick a winner; state the single load-bearing reason and stop.",
        skills: &[],
    },
    Agent {
        name: "evolution",
        role: "Improves top-ranked hypotheses via combine / simplify / feasibility / out-of-box re-imagination strategies.",
        modes: EVOLUTION_MODES,
        personality: "",
        skills: &[],
    },
    Agent {
        name: "metareview",
        role: "Synthesizes system-wide feedback for future generations and produces the final research overview.",
        modes: METAREVIEW_MODES,
        personality: "",
        skills: &[],
    },
    Agent {
        name: "experiment",
        role: "Closes the empirical loop: writes Python code that tests a hypothesis, runs it in a sandbox, and interprets the metric. Results feed back into the tournament via reflection_on_result.",
        modes: EXPERIMENT_MODES,
        personality: "",
        skills: &[],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_agent_has_a_skills_field() {
        // Adding a new Agent must also pick a skills list — empty is
        // fine, but the field must be present so the prompt builder
        // never has to special-case a missing field.
        for a in AGENTS {
            // `skills` is `&'static [&'static str]`; accessing the
            // field is enough to prove the type contract holds.
            let _ = a.skills.len();
        }
    }

    #[test]
    fn agents_with_empty_personality_have_empty_skills_by_default() {
        // evolution / metareview / experiment ship personality-free.
        // Their skills slice must also be empty so the system prompt
        // does not grow a stray "## Agent skills" header when none
        // are configured.
        for name in ["evolution", "metareview", "experiment"] {
            let a = AGENTS.iter().find(|a| a.name == name).unwrap();
            assert!(a.personality.is_empty(), "{name} personality unexpectedly non-empty");
            assert!(a.skills.is_empty(), "{name} skills unexpectedly non-empty");
        }
    }
}
