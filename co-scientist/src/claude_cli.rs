//! Minimal in-tree client for the `claude` CLI subprocess.
//!
//! Implements only the JSON-line stdio protocol surface that
//! `runner` actually uses: spawn the CLI, send one user prompt,
//! collect the assistant text, return on `result`. No MCP,
//! no tool_use parsing, no control-request buffering beyond
//! the initialize handshake.
//!
//! Replaces `ante-agent-sdk` to drop its transitive dep graph
//! from co-scientist.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex};

#[derive(Debug, Clone, Default)]
pub struct ClaudeOptions {
    pub cli_path: Option<PathBuf>,
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub permission_mode: Option<PermissionMode>,
    pub allowed_tools: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    BypassPermissions,
}

impl PermissionMode {
    fn as_arg(self) -> &'static str {
        match self {
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

const INIT_REQUEST_ID: &str = "req_1";
const INIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Tag stamped onto outbound `user` frames. The CLI requires the field but
/// does not route on it (session is fixed at launch time).
const USER_FRAME_SESSION_TAG: &str = "default";

pub struct ClaudeCli {
    inner: Mutex<Inner>,
    rx: Mutex<mpsc::UnboundedReceiver<Value>>,
    _reader: tokio::task::JoinHandle<()>,
}

struct Inner {
    child: Child,
}

pub struct TurnResponse {
    pub assistant_text: String,
}

impl ClaudeCli {
    pub async fn connect(options: ClaudeOptions) -> Result<Self> {
        let cli = match &options.cli_path {
            Some(p) => p.clone(),
            None => which::which("claude").context("claude CLI not found in PATH")?,
        };
        let args = build_args(&options);
        let mut cmd = Command::new(cli);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = cmd.spawn().context("spawning claude CLI")?;

        // Spawn the background stdout reader FIRST so it owns child.stdout
        // for the entire lifetime. Init and query both drain from the same
        // channel; we just look for different frames.
        let stdout = child
            .stdout
            .take()
            .context("claude child stdout unavailable")?;
        let (tx, rx) = mpsc::unbounded_channel::<Value>();
        let reader = tokio::spawn(async move {
            use tokio::io::BufReader;
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Value>(&line) {
                            Ok(v) => {
                                if tx.send(v).is_err() {
                                    return;
                                }
                            }
                            Err(_) => {
                                // skip malformed lines
                            }
                        }
                    }
                    Ok(None) | Err(_) => return,
                }
            }
        });

        // Init handshake: write control_request, then drain the channel
        // until we see req_1 success (or timeout / EOF).
        {
            let stdin = child
                .stdin
                .as_mut()
                .context("claude child stdin unavailable")?;
            let init = json!({
                "type": "control_request",
                "request_id": INIT_REQUEST_ID,
                "request": { "subtype": "initialize", "hooks": null },
            });
            stdin
                .write_all(init.to_string().as_bytes())
                .await
                .context("writing init request to claude stdin")?;
            stdin.write_all(b"\n").await.context("newline after init")?;
            stdin.flush().await.context("flushing init")?;
        }

        let mut rx_init = rx;
        let deadline = tokio::time::Instant::now() + INIT_TIMEOUT;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    anyhow::bail!("claude init handshake timed out after {:?}", INIT_TIMEOUT);
                }
                msg = rx_init.recv() => {
                    match msg {
                        Some(v) => {
                            if is_init_success(&v) {
                                break;
                            }
                            // ignore other frames (system, etc.) during init
                        }
                        None => {
                            anyhow::bail!("claude stdout closed before init response");
                        }
                    }
                }
            }
        }

        Ok(Self {
            inner: Mutex::new(Inner { child }),
            rx: Mutex::new(rx_init),
            _reader: reader,
        })
    }

    pub async fn query(&self, prompt: String) -> Result<TurnResponse> {
        // Write the user frame under the inner lock so concurrent queries
        // serialize (the subprocess expects ordered stdin frames).
        {
            let mut inner = self.inner.lock().await;
            let stdin = inner
                .child
                .stdin
                .as_mut()
                .context("claude child stdin closed")?;
            let frame = json!({
                "type": "user",
                "message": { "role": "user", "content": prompt },
                "parent_tool_use_id": null,
                "session_id": USER_FRAME_SESSION_TAG,
            });
            stdin
                .write_all(frame.to_string().as_bytes())
                .await
                .context("writing user frame to claude stdin")?;
            stdin.write_all(b"\n").await.context("newline after user")?;
            stdin.flush().await.context("flushing user frame")?;
        }

        let mut assistant_text = String::new();
        let mut rx = self.rx.lock().await;
        loop {
            match rx.recv().await {
                Some(v) => match v.get("type").and_then(|t| t.as_str()) {
                    Some("assistant") => {
                        if let Some(text) = extract_assistant_text(&v) {
                            if !assistant_text.is_empty() {
                                assistant_text.push('\n');
                            }
                            assistant_text.push_str(&text);
                        }
                    }
                    Some("result") => {
                        return Ok(TurnResponse { assistant_text });
                    }
                    // system, control_request, control_response, stream_event:
                    // we don't act on them. The runner doesn't configure hooks,
                    // so the CLI shouldn't emit control_request mid-turn, but
                    // we silently drop it if it does.
                    _ => {}
                },
                None => {
                    anyhow::bail!("claude stdout closed before result frame");
                }
            }
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        let mut inner = self.inner.lock().await;
        let _ = inner.child.start_kill();
        let _ = inner.child.wait().await;
        Ok(())
    }
}

fn build_args(options: &ClaudeOptions) -> Vec<String> {
    let mut args = vec![
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
    ];
    if let Some(p) = &options.system_prompt {
        args.push("--system-prompt".to_string());
        args.push(p.clone());
    }
    if !options.allowed_tools.is_empty() {
        args.push("--allowedTools".to_string());
        args.push(options.allowed_tools.join(","));
    }
    if let Some(t) = options.max_turns {
        args.push("--max-turns".to_string());
        args.push(t.to_string());
    }
    if let Some(m) = &options.model {
        args.push("--model".to_string());
        args.push(m.clone());
    }
    if let Some(pm) = options.permission_mode {
        args.push("--permission-mode".to_string());
        args.push(pm.as_arg().to_string());
    }
    args
}

fn is_init_success(v: &Value) -> bool {
    v.get("type").and_then(|t| t.as_str()) == Some("control_response")
        && v.pointer("/response/request_id").and_then(|r| r.as_str()) == Some(INIT_REQUEST_ID)
        && v.pointer("/response/subtype").and_then(|s| s.as_str()) == Some("success")
}

fn extract_assistant_text(v: &Value) -> Option<String> {
    let content = v.get("message").and_then(|m| m.get("content"))?;
    let blocks = content.as_array()?;
    let mut parts: Vec<&str> = Vec::new();
    for block in blocks {
        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                parts.push(t);
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn fake_script_path(label: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("unix epoch")
            .as_nanos();
        let pid = std::process::id();
        std::env::temp_dir().join(format!("co-scientist-claude-cli-{label}-{pid}-{now}.sh"))
    }

    #[tokio::test]
    async fn query_returns_assistant_text_then_terminates_on_result() {
        let path = fake_script_path("basic");
        let script = r#"#!/bin/sh
IFS= read -r _ || exit 0
printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"req_1","response":{"session_id":"s1","ready":true}}}'
IFS= read -r _ || exit 0
printf '%s\n' '{"type":"assistant","message":{"model":"claude-sonnet-4-5","content":[{"type":"text","text":"Hello "},{"type":"text","text":"world"}]}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"session_id":"s1","total_cost_usd":0.001,"result":"done"}'
"#;
        fs::write(&path, script).expect("write fake claude script");
        let mut perms = fs::metadata(&path).expect("stat").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).expect("chmod");

        let client = ClaudeCli::connect(ClaudeOptions {
            cli_path: Some(path.clone()),
            model: Some("claude-sonnet-4-5".to_string()),
            allowed_tools: vec!["Bash".to_string()],
            permission_mode: Some(PermissionMode::BypassPermissions),
            ..ClaudeOptions::default()
        })
        .await
        .expect("connect succeeds");

        let resp = client.query("say hi".to_string()).await.expect("query");
        assert_eq!(resp.assistant_text, "Hello \nworld");

        client.shutdown().await.expect("shutdown");
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn non_assistant_frames_before_result_are_ignored() {
        let path = fake_script_path("mixed");
        let script = r#"#!/bin/sh
IFS= read -r _ || exit 0
printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"req_1","response":{}}}'
IFS= read -r _ || exit 0
printf '%s\n' '{"type":"system","subtype":"some_status"}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"only this"}]}}'
printf '%s\n' '{"type":"control_request","request_id":"req_99","request":{"subtype":"hook_callback"}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"session_id":"s1","total_cost_usd":0.0,"result":"done"}'
"#;
        fs::write(&path, script).expect("write");
        let mut perms = fs::metadata(&path).expect("stat").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).expect("chmod");

        let client = ClaudeCli::connect(ClaudeOptions {
            cli_path: Some(path.clone()),
            ..ClaudeOptions::default()
        })
        .await
        .expect("connect");

        let resp = client.query("go".to_string()).await.expect("query");
        assert_eq!(resp.assistant_text, "only this");

        client.shutdown().await.expect("shutdown");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn build_args_matches_sdk_shape_for_runner_use() {
        let opts = ClaudeOptions {
            system_prompt: Some("you are a helper".to_string()),
            model: Some("sonnet".to_string()),
            max_turns: Some(3),
            permission_mode: Some(PermissionMode::BypassPermissions),
            allowed_tools: vec!["Bash".to_string(), "Read".to_string()],
            ..ClaudeOptions::default()
        };
        let args = build_args(&opts);
        assert_eq!(
            args,
            vec![
                "--output-format",
                "stream-json",
                "--input-format",
                "stream-json",
                "--verbose",
                "--system-prompt",
                "you are a helper",
                "--allowedTools",
                "Bash,Read",
                "--max-turns",
                "3",
                "--model",
                "sonnet",
                "--permission-mode",
                "bypassPermissions",
            ]
        );
    }

    #[test]
    fn init_success_detected() {
        let v = json!({
            "type":"control_response",
            "response":{"subtype":"success","request_id":"req_1","response":{"ok":true}}
        });
        assert!(is_init_success(&v));
        let v2 = json!({"type":"control_response","response":{"subtype":"success","request_id":"req_2"}});
        assert!(!is_init_success(&v2));
        let v3 = json!({"type":"control_response","response":{"subtype":"error","request_id":"req_1"}});
        assert!(!is_init_success(&v3));
    }
}