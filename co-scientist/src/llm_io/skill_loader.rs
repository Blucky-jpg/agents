//! Skill loader. Walks a directory of `SKILL.md` files and turns each
//! into a real [`Tool`] that runs a subprocess with a sanitized env
//! and JSON-in/JSON-out protocol.
//!
//! # SKILL.md format
//!
//! ```markdown
//! ---
//! name: my-skill
//! description: One-line summary
//! timeout_seconds: 30
//! inputs:
//!   query: { type: string }
//! ---
//!
//! # Body
//!
//! Free-form markdown. Included in the tool's description.
//! ```
//!
//! The `name` field is the tool name. The `description` is shown to the
//! LLM. `timeout_seconds` defaults to 120. `inputs` is a JSONSchema
//! object used as the tool's `input_schema`.
//!
//! # Entrypoint
//!
//! Each skill must have one of: an `entrypoint:` field, a `scripts/run.py`,
//! `scripts/main.py`, `scripts/cli.py`, or `scripts/run.sh` in that
//! order. The entrypoint must be a file inside the skill's directory
//! — anything outside is rejected with a path-traversal error.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::tool::{Tool, ToolCtx, ToolOutput};

#[derive(Debug, Clone, Deserialize)]
pub struct SkillFrontmatter {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub inputs: Option<Value>,
}

fn default_timeout() -> u64 {
    120
}

#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub meta: SkillFrontmatter,
    pub dir: PathBuf,
    pub body: String,
    pub entrypoint: PathBuf,
    pub name: String,
    pub description: String,
}

pub fn discover(dir: &Path) -> Result<Vec<LoadedSkill>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let skill_dir = entry.path();
        let md = skill_dir.join("SKILL.md");
        if !md.is_file() {
            continue;
        }
        match parse_skill_md(&skill_dir) {
            Ok(Some(skill)) => out.push(skill),
            Ok(None) => {}
            Err(e) => tracing::warn!(
                skill_dir = %skill_dir.display(),
                error = %e,
                "skipping skill with invalid SKILL.md"
            ),
        }
    }
    Ok(out)
}

fn parse_skill_md(skill_dir: &Path) -> Result<Option<LoadedSkill>> {
    let md_path = skill_dir.join("SKILL.md");
    let text = std::fs::read_to_string(&md_path)
        .with_context(|| format!("reading {}", md_path.display()))?;
    let (front, body) = split_frontmatter(&text);
    let meta: SkillFrontmatter = match front {
        Some(s) if !s.trim().is_empty() => serde_yaml::from_str(&s)
            .with_context(|| format!("parsing frontmatter in {}", md_path.display()))?,
        _ => SkillFrontmatter {
            name: None,
            description: None,
            entrypoint: None,
            timeout_seconds: default_timeout(),
            inputs: None,
        },
    };
    let name = meta
        .name
        .clone()
        .or_else(|| {
            skill_dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        })
        .ok_or_else(|| anyhow!("skill has no name"))?;
    let description = meta
        .description
        .clone()
        .unwrap_or_else(|| body.lines().next().unwrap_or("").to_string());
    let entrypoint = resolve_entrypoint(skill_dir, meta.entrypoint.as_deref())?;
    Ok(Some(LoadedSkill {
        meta,
        dir: skill_dir.to_path_buf(),
        body,
        entrypoint,
        name,
        description,
    }))
}

/// Split "front\n---\nrest" into ("front", "rest"). Returns ("", whole)
/// if there's no `---` fence.
fn split_frontmatter(text: &str) -> (Option<String>, String) {
    let trimmed = text.trim_start();
    if !trimmed.starts_with("---") {
        return (None, text.to_string());
    }
    // Find the closing `---` on its own line.
    let rest = &trimmed[3..];
    let rest = rest.trim_start_matches('\n');
    if let Some(end) = rest.find("\n---") {
        let front = &rest[..end];
        let body_start = end + 4; // skip past \n---
        let body = rest[body_start..].trim_start_matches('\n').to_string();
        (Some(front.to_string()), body)
    } else {
        (None, text.to_string())
    }
}

fn resolve_entrypoint(skill_dir: &Path, explicit: Option<&str>) -> Result<PathBuf> {
    let skill_dir_resolved = skill_dir
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", skill_dir.display()))?;
    let candidates: Vec<PathBuf> = if let Some(e) = explicit {
        vec![skill_dir.join(e)]
    } else {
        [
            "scripts/run.py",
            "scripts/main.py",
            "scripts/cli.py",
            "scripts/run.sh",
        ]
        .iter()
        .map(|p| skill_dir.join(p))
        .collect()
    };
    for c in candidates {
        if c.is_file() {
            // Path-traversal guard: entrypoint must be inside skill_dir.
            let resolved = c
                .canonicalize()
                .with_context(|| format!("canonicalizing {}", c.display()))?;
            if !resolved.starts_with(&skill_dir_resolved) {
                return Err(anyhow!(
                    "entrypoint escapes skill dir: {}",
                    c.display()
                ));
            }
            // Return the canonical (absolute) path so the subprocess
            // runner can find the script regardless of cwd.
            return Ok(resolved);
        }
    }
    Err(anyhow!(
        "no entrypoint found in {} (looked for scripts/run.py, main.py, cli.py, run.sh)",
        skill_dir.display()
    ))
}

/// Convert a loaded skill into a real `Tool` impl. The tool runs the
/// entrypoint as a subprocess with a sanitized env.
pub fn into_tool(skill: LoadedSkill) -> Arc<dyn Tool> {
    Arc::new(SkillProcessTool { skill })
}

struct SkillProcessTool {
    skill: LoadedSkill,
}

#[async_trait]
impl Tool for SkillProcessTool {
    fn name(&self) -> &str {
        &self.skill.name
    }
    fn description(&self) -> String {
        if self.skill.body.is_empty() {
            self.skill.description.clone()
        } else {
            // First paragraph of the body if it's longer than the meta
            // description; otherwise just the meta description.
            let first = self
                .skill
                .body
                .lines()
                .take_while(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if first.len() > self.skill.description.len() + 20 {
                first
            } else {
                self.skill.description.clone()
            }
        }
    }
    fn input_schema(&self) -> Value {
        self.skill
            .meta
            .inputs
            .clone()
            .unwrap_or_else(|| {
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "args": { "description": "Free-form args passed as JSON on stdin." }
                    }
                })
            })
    }
    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let timeout = Duration::from_secs(self.skill.meta.timeout_seconds.max(1));
        let mut cmd = build_command(&self.skill.entrypoint);
        cmd.current_dir(&self.skill.dir)
            .env_clear()
            .envs(sanitized_env())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning {}", self.skill.entrypoint.display()))?;

        if let Some(mut stdin) = child.stdin.take() {
            let payload = serde_json::to_vec(&args)?;
            stdin.write_all(&payload).await?;
            stdin.shutdown().await.ok();
        }

        let result = tokio::time::timeout(timeout, child.wait_with_output()).await;
        match result {
            Ok(Ok(out)) => {
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    return Err(anyhow!(
                        "skill {} exited with status {}: {}",
                        self.skill.name,
                        out.status,
                        stderr.trim()
                    ));
                }
                let stdout = String::from_utf8_lossy(&out.stdout);
                match serde_json::from_str::<Value>(stdout.trim()) {
                    Ok(v) => Ok(v),
                    Err(_) => Ok(Value::String(stdout.to_string())),
                }
            }
            Ok(Err(e)) => Err(anyhow!("skill {} failed: {e}", self.skill.name)),
            Err(_) => Err(anyhow!(
                "skill {} timed out after {}s",
                self.skill.name,
                timeout.as_secs()
            )),
        }
    }
}

fn build_command(entrypoint: &Path) -> Command {
    let ext = entrypoint
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "py" => {
            let mut c = Command::new("python3");
            c.arg(entrypoint);
            c
        }
        "sh" => {
            let mut c = Command::new("bash");
            c.arg(entrypoint);
            c
        }
        _ => Command::new(entrypoint),
    }
}

/// Build a sanitized env: only the keys we explicitly want to forward
/// from the parent process. The full env would leak secrets.
fn sanitized_env() -> Vec<(&'static str, String)> {
    let mut env: Vec<(&'static str, String)> = Vec::new();
    for key in [
        "PATH",
        "HOME",
        "LANG",
        "LANGUAGE",
        "LC_ALL",
        "LC_CTYPE",
        "TZ",
        "TMPDIR",
    ] {
        if let Ok(v) = std::env::var(key) {
            env.push((key, v));
        }
    }
    // Forward API keys only for the providers the user has configured.
    for key in [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "OPENROUTER_API_KEY",
        "GEMINI_API_KEY",
        "GROQ_API_KEY",
        "TOGETHER_API_KEY",
        "MISTRAL_API_KEY",
        "VOYAGE_API_KEY",
        "TAVILY_API_KEY",
        "BRAVE_API_KEY",
    ] {
        if let Ok(v) = std::env::var(key) {
            env.push((key, v));
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn split_with_frontmatter() {
        let text = "---\nname: foo\n---\nbody line 1\nbody line 2\n";
        let (front, body) = split_frontmatter(text);
        assert!(front.unwrap().contains("name: foo"));
        assert!(body.starts_with("body line 1"));
    }

    #[test]
    fn split_without_frontmatter() {
        let text = "no fence here\njust body\n";
        let (front, body) = split_frontmatter(text);
        assert!(front.is_none());
        assert!(body.contains("no fence here"));
    }

    #[test]
    fn entrypoint_path_traversal_is_rejected() {
        let dir = tempdir().unwrap();
        let parent = dir.path();
        let skill = parent.join("skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::create_dir_all(skill.join("scripts")).unwrap();
        // Malicious file lives in the parent of the skill dir; the
        // SKILL.md tries to point at it via `../escape.sh`.
        std::fs::write(parent.join("escape.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nentrypoint: ../escape.sh\n---\n",
        )
        .unwrap();
        let err = parse_skill_md(&skill).unwrap_err();
        assert!(err.to_string().contains("escapes"), "got: {err}");
    }

    #[test]
    fn discover_finds_skills() {
        let dir = tempdir().unwrap();
        let skill = dir.path().join("echo-skill");
        std::fs::create_dir_all(skill.join("scripts")).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: echo-skill\ndescription: echoes stdin\ntimeout_seconds: 5\n---\nEchoes.",
        )
        .unwrap();
        std::fs::write(
            skill.join("scripts/run.sh"),
            "#!/bin/sh\ncat\n",
        )
        .unwrap();
        let skills = discover(dir.path()).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "echo-skill");
        assert_eq!(skills[0].meta.timeout_seconds, 5);
    }

    #[test]
    fn discover_returns_empty_for_missing_dir() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let skills = discover(&missing).unwrap();
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn skill_tool_round_trip() {
        let dir = tempdir().unwrap();
        let skill = dir.path().join("passthrough");
        std::fs::create_dir_all(skill.join("scripts")).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: passthrough\ndescription: pass through\n---\n",
        )
        .unwrap();
        // Echo the input as JSON.
        std::fs::write(
            skill.join("scripts/run.sh"),
            "#!/bin/sh\ncat\n",
        )
        .unwrap();
        std::fs::set_permissions(skill.join("scripts/run.sh"), {
            use std::os::unix::fs::PermissionsExt;
            std::fs::Permissions::from_mode(0o755)
        })
        .unwrap();
        let skill = parse_skill_md(&skill).unwrap().unwrap();
        let tool = into_tool(skill);
        let ctx = ToolCtx {
            memory: crate::Memory::new(
                crate::db::open_memory().await.unwrap(),
            ),
            run_id: "r".into(),
            agent_name: "a".into(),
        };
        let out = tool
            .call(serde_json::json!({"hello": "world"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out["hello"], "world");
    }
}
