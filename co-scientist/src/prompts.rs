//! Prompt registry. Loads the 14 Jinja2 templates shipped under
//! `co-scientist/prompts/` (copied verbatim from the community
//! `Co-Scientist/config/prompts/`), and renders them with a structured
//! [`PromptContext`].
//!
//! # Agent -> prompt mapping
//!
//! The 6 community agents each own one or more "modes" (different tasks
//! the same agent performs). The mapping is:
//!
//! | Agent       | Modes (prompt file basenames)                                  |
//! |-------------|----------------------------------------------------------------|
//! | `supervisor`| `parse_goal`                                                   |
//! | `generation`| `generation_literature`, `generation_debate`                  |
//! | `reflection`| `reflection_review`, `reflection_observation`, `reflection_verification` |
//! | `ranking`   | `ranking_pairwise`, `ranking_debate`                          |
//! | `evolution` | `evolution_combine`, `evolution_simplify`, `evolution_feasibility`, `evolution_out_of_box` |
//! | `metareview`| `metareview_system`, `metareview_final`                       |
//!
//! All prompts are loaded via `include_str!` at compile time, so a
//! typo in a prompt file becomes a build error.
//!
//! # Variables
//!
//! Each prompt declares its own variables; missing variables fail
//! rendering with a clear error. The [`PromptContext`] is just a
//! `serde_json::Value` (an object) passed to minijinja. Callers build
//! the right shape per prompt.

use anyhow::{Context as _, Result};
use minijinja::Environment;
use serde_json::Value;

/// One prompt name per agent+mode. Public so the runner can look up the
/// right template for a given task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentMode {
    ParseGoal,
    GenerationLiterature,
    GenerationDebate,
    ReflectionReview,
    ReflectionObservation,
    ReflectionVerification,
    RankingPairwise,
    RankingDebate,
    EvolutionCombine,
    EvolutionSimplify,
    EvolutionFeasibility,
    EvolutionOutOfBox,
    MetaReviewSystem,
    MetaReviewFinal,
}

impl AgentMode {
    /// The file basename (without `.md`) in `co-scientist/prompts/`.
    pub fn filename(self) -> &'static str {
        match self {
            Self::ParseGoal => "parse_goal",
            Self::GenerationLiterature => "generation_literature",
            Self::GenerationDebate => "generation_debate",
            Self::ReflectionReview => "reflection_review",
            Self::ReflectionObservation => "reflection_observation",
            Self::ReflectionVerification => "reflection_verification",
            Self::RankingPairwise => "ranking_pairwise",
            Self::RankingDebate => "ranking_debate",
            Self::EvolutionCombine => "evolution_combine",
            Self::EvolutionSimplify => "evolution_simplify",
            Self::EvolutionFeasibility => "evolution_feasibility",
            Self::EvolutionOutOfBox => "evolution_out_of_box",
            Self::MetaReviewSystem => "metareview_system",
            Self::MetaReviewFinal => "metareview_final",
        }
    }

    /// The community agent name that owns this mode.
    pub fn agent(self) -> &'static str {
        match self {
            Self::ParseGoal => "supervisor",
            Self::GenerationLiterature | Self::GenerationDebate => "generation",
            Self::ReflectionReview
            | Self::ReflectionObservation
            | Self::ReflectionVerification => "reflection",
            Self::RankingPairwise | Self::RankingDebate => "ranking",
            Self::EvolutionCombine
            | Self::EvolutionSimplify
            | Self::EvolutionFeasibility
            | Self::EvolutionOutOfBox => "evolution",
            Self::MetaReviewSystem | Self::MetaReviewFinal => "metareview",
        }
    }

    /// All modes an agent owns.
    pub fn modes_for(agent: &str) -> &'static [AgentMode] {
        match agent {
            "supervisor" => &[Self::ParseGoal],
            "generation" => &[Self::GenerationLiterature, Self::GenerationDebate],
            "reflection" => &[
                Self::ReflectionReview,
                Self::ReflectionObservation,
                Self::ReflectionVerification,
            ],
            "ranking" => &[Self::RankingPairwise, Self::RankingDebate],
            "evolution" => &[
                Self::EvolutionCombine,
                Self::EvolutionSimplify,
                Self::EvolutionFeasibility,
                Self::EvolutionOutOfBox,
            ],
            "metareview" => &[Self::MetaReviewSystem, Self::MetaReviewFinal],
            _ => &[],
        }
    }
}

/// Bundles a minijinja environment with the 14 prompt templates.
pub struct Prompts {
    env: Environment<'static>,
}

impl std::fmt::Debug for Prompts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prompts").finish_non_exhaustive()
    }
}

impl Prompts {
    /// Build a `Prompts` registry with all 14 templates loaded. Once
    /// built, share one instance across `Runner`s via
    /// `Arc<Prompts>` — `Environment` is expensive to rebuild.
    pub fn new() -> Result<Self> {
        let mut env = Environment::new();
        env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
        for mode in PROMPT_MODES {
            let name = mode.filename();
            let body = match name {
                "parse_goal" => include_str!("../prompts/parse_goal.md"),
                "generation_literature" => include_str!("../prompts/generation_literature.md"),
                "generation_debate" => include_str!("../prompts/generation_debate.md"),
                "reflection_review" => include_str!("../prompts/reflection_review.md"),
                "reflection_observation" => include_str!("../prompts/reflection_observation.md"),
                "reflection_verification" => include_str!("../prompts/reflection_verification.md"),
                "ranking_pairwise" => include_str!("../prompts/ranking_pairwise.md"),
                "ranking_debate" => include_str!("../prompts/ranking_debate.md"),
                "evolution_combine" => include_str!("../prompts/evolution_combine.md"),
                "evolution_simplify" => include_str!("../prompts/evolution_simplify.md"),
                "evolution_feasibility" => include_str!("../prompts/evolution_feasibility.md"),
                "evolution_out_of_box" => include_str!("../prompts/evolution_out_of_box.md"),
                "metareview_system" => include_str!("../prompts/metareview_system.md"),
                "metareview_final" => include_str!("../prompts/metareview_final.md"),
                other => anyhow::bail!("unknown prompt file: {other}.md"),
            };
            env.add_template_owned(name.to_string(), body.to_string())
                .with_context(|| format!("invalid template syntax in {name}.md"))?;
        }
        Ok(Self { env })
    }

    /// Render a single prompt with the given context. Returns the
    /// rendered string or a clear error if a required variable is
    /// missing.
    pub fn render(&self, mode: AgentMode, ctx: &PromptContext) -> Result<String> {
        let name = mode.filename();
        let tmpl = self
            .env
            .get_template(name)
            .with_context(|| format!("prompt template not loaded: {name}"))?;
        tmpl.render(ctx.as_value())
            .with_context(|| format!("rendering {name}"))
    }

    /// Render a prompt with a raw `serde_json::Value` (an object).
    /// Convenience for callers that already have JSON.
    pub fn render_value(&self, mode: AgentMode, ctx: &Value) -> Result<String> {
        let name = mode.filename();
        let tmpl = self
            .env
            .get_template(name)
            .with_context(|| format!("prompt template not loaded: {name}"))?;
        tmpl.render(ctx)
            .with_context(|| format!("rendering {name}"))
    }
}

/// No `Default` impl on purpose: `Prompts::new` is fallible (templates
/// may have a syntax error or a missing file). Callers that need a
/// `Default`-like API should use `Prompts::new().expect(...)` or
/// share one `Arc<Prompts>` built at startup.

/// All 14 modes in a stable order. Used for tests + introspection.
pub const PROMPT_MODES: &[AgentMode] = &[
    AgentMode::ParseGoal,
    AgentMode::GenerationLiterature,
    AgentMode::GenerationDebate,
    AgentMode::ReflectionReview,
    AgentMode::ReflectionObservation,
    AgentMode::ReflectionVerification,
    AgentMode::RankingPairwise,
    AgentMode::RankingDebate,
    AgentMode::EvolutionCombine,
    AgentMode::EvolutionSimplify,
    AgentMode::EvolutionFeasibility,
    AgentMode::EvolutionOutOfBox,
    AgentMode::MetaReviewSystem,
    AgentMode::MetaReviewFinal,
];

/// A thin wrapper around `serde_json::Value` (an object) for prompt
/// rendering. The community prompts use plain string interpolation
/// (`{{ goal }}`) and Jinja filters (`{{ preferences | default('') }}`),
/// so an object is the natural shape.
#[derive(Debug, Clone)]
pub struct PromptContext {
    inner: Value,
}

impl Default for PromptContext {
    fn default() -> Self {
        Self {
            inner: Value::Object(Default::default()),
        }
    }
}

impl PromptContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from a JSON object (any `Map<String, Value>` works).
    pub fn from_object(obj: serde_json::Map<String, Value>) -> Self {
        Self {
            inner: Value::Object(obj),
        }
    }

    /// Insert a string field.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        let obj = self
            .inner
            .as_object_mut()
            .expect("PromptContext is always an object");
        obj.insert(key.into(), Value::String(value.into()));
        self
    }

    /// Insert a JSON value.
    pub fn set_value(&mut self, key: impl Into<String>, value: Value) -> &mut Self {
        let obj = self
            .inner
            .as_object_mut()
            .expect("PromptContext is always an object");
        obj.insert(key.into(), value);
        self
    }

    /// Borrow the inner JSON value.
    pub fn as_value(&self) -> &Value {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_14_prompts_load() {
        // Loading is a separate concern from rendering. A prompt may
        // be syntactically valid but still fail to render without the
        // right variables (strict mode is the default in Prompts::new).
        let p = Prompts::new().expect("prompts load");
        assert_eq!(PROMPT_MODES.len(), 14);
        for mode in PROMPT_MODES {
            // Touch the template; this parses + caches it. Rendering is
            // covered by the per-template tests below.
            p.env.get_template(mode.filename()).unwrap();
        }
    }

    #[test]
    fn parse_goal_renders_goal_variable() {
        let p = Prompts::new().unwrap();
        let mut ctx = PromptContext::new();
        ctx.set("goal", "Identify new KRAS G12C inhibitors");
        let out = p.render(AgentMode::ParseGoal, &ctx).unwrap();
        assert!(out.contains("Identify new KRAS G12C inhibitors"));
        assert!(out.contains("record_research_plan"));
    }

    #[test]
    fn generation_literature_renders_with_minimum_vars() {
        let p = Prompts::new().unwrap();
        let mut ctx = PromptContext::new();
        ctx.set("goal", "x");
        ctx.set("preferences", "");
        ctx.set("articles_with_reasoning", "(none)");
        // The `{% if source_hypothesis %}` and `{% if instructions %}`
        // blocks need a defined (even empty) value to render under
        // strict mode.
        ctx.set("source_hypothesis", "");
        ctx.set("instructions", "");
        let out = p.render(AgentMode::GenerationLiterature, &ctx).unwrap();
        assert!(out.contains("record_hypothesis"));
    }

    #[test]
    fn conditional_blocks_render() {
        let p = Prompts::new().unwrap();
        let mut ctx = PromptContext::new();
        ctx.set("goal", "x");
        ctx.set("preferences", "");
        // No source_hypothesis, no instructions — the `{% if %}` blocks
        // should be omitted.
        ctx.set("source_hypothesis", "");
        ctx.set("instructions", "");
        ctx.set("articles_with_reasoning", "(none)");
        let out = p.render(AgentMode::GenerationLiterature, &ctx).unwrap();
        assert!(!out.contains("Existing hypothesis (if applicable)"));
    }

    #[test]
    fn missing_required_var_fails_loudly() {
        let p = Prompts::new().unwrap();
        let ctx = PromptContext::new();
        let err = p.render(AgentMode::ParseGoal, &ctx).unwrap_err();
        // Strict mode: missing `goal` should fail.
        assert!(err.to_string().contains("goal") || err.to_string().contains("undefined"));
    }

    #[test]
    fn generation_debate_renders_with_required_vars() {
        let p = Prompts::new().unwrap();
        let mut ctx = PromptContext::new();
        ctx.set("goal", "x");
        ctx.set("preferences", "");
        ctx.set("instructions", "");
        ctx.set("reviews_overview", "(none)");
        ctx.set("transcript", "(none)");
        let out = p.render(AgentMode::GenerationDebate, &ctx).unwrap();
        assert!(out.contains("HYPOTHESIS"));
    }

    #[test]
    fn metareview_final_renders_with_required_vars() {
        let p = Prompts::new().unwrap();
        let mut ctx = PromptContext::new();
        ctx.set("goal", "x");
        ctx.set("preferences", "");
        ctx.set("system_feedback", "(none)");
        ctx.set("top_hypotheses_block", "(none)");
        let out = p.render(AgentMode::MetaReviewFinal, &ctx).unwrap();
        assert!(out.contains("Executive summary"));
    }

    #[test]
    fn modes_for_agent_returns_all_owned_modes() {
        let m = AgentMode::modes_for("evolution");
        assert_eq!(m.len(), 4);
        assert!(m.contains(&AgentMode::EvolutionCombine));
    }

    #[test]
    fn unknown_agent_returns_empty_modes() {
        assert!(AgentMode::modes_for("nope").is_empty());
    }
}
