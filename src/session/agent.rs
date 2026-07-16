use std::process::Stdio;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::config::{ClaudexConfig, ProfileConfig};

const ULTRACODE_SYSTEM_PROMPT: &str = "Ultracode orchestration is enabled. For substantial tasks, first identify independent workstreams and proactively delegate bounded work to the claudex-researcher, claudex-worker, or claudex-reviewer subagents. Run independent investigations in parallel. Give every subagent a concrete scope and expected result, keep file ownership disjoint, and never let two workers edit the same file concurrently. Keep tiny or inherently serial tasks in the main thread. Reconcile every result and perform final verification yourself.";

#[derive(Debug)]
pub enum AgentEvent {
    Message(Value),
    Stderr(String),
    ProtocolError(String),
    Exited(std::process::ExitStatus),
}

enum ProcessControl {
    Kill,
}

/// A long-lived Claude Code streaming process. Claudex owns presentation and
/// sends newline-delimited Agent SDK protocol messages over stdio, while the
/// underlying process retains Claude Code's tools, hooks, skills, and Agent
/// subagent harness.
pub struct AgentProcess {
    input: mpsc::UnboundedSender<String>,
    control: mpsc::UnboundedSender<ProcessControl>,
}

impl AgentProcess {
    pub async fn spawn(
        config: &ClaudexConfig,
        profile: &ProfileConfig,
        model: &str,
        effort: &str,
        fast_session_id: &str,
        resume_session: Option<&str>,
    ) -> Result<(Self, mpsc::Receiver<AgentEvent>)> {
        let command = crate::process::launch::configured_claude_command(
            config,
            profile,
            model,
            fast_session_id,
        )?;
        let mut command = Command::from(command);
        command
            .env("CLAUDE_CODE_ENTRYPOINT", "claudex")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--input-format")
            .arg("stream-json")
            .arg("--include-partial-messages")
            .arg("--permission-prompt-tool")
            .arg("stdio")
            .arg("--model")
            .arg(model)
            .arg("--effort")
            .arg(effort)
            .arg("--no-chrome")
            .arg("--add-dir")
            .arg(crate::integration::claude_integration_root()?);
        if effort == "xhigh" {
            command
                .arg("--append-system-prompt")
                .arg(ULTRACODE_SYSTEM_PROMPT)
                .arg("--agents")
                .arg(serde_json::to_string(&ultracode_agents())?);
        }
        command.args(crate::privacy::enforce_private_settings(&[])?);
        if let Some(session_id) = resume_session {
            command.arg(format!("--resume={session_id}"));
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .context("failed to start the Claudex agent harness")?;
        let stdin = child
            .stdin
            .take()
            .context("agent harness stdin was unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("agent harness stdout was unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("agent harness stderr was unavailable")?;

        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<String>();
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<ProcessControl>();
        // Backpressure keeps a very fast model stream from growing memory
        // without limit when the terminal cannot render at the same rate.
        let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(1024);

        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = input_rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });

        let stdout_events = event_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) if line.trim().is_empty() => {}
                    Ok(Some(line)) => match serde_json::from_str::<Value>(&line) {
                        Ok(message) => {
                            if stdout_events
                                .send(AgentEvent::Message(message))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(error) => {
                            if stdout_events
                                .send(AgentEvent::ProtocolError(format!(
                                    "invalid agent event ({error}): {line}"
                                )))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    },
                    Ok(None) => break,
                    Err(error) => {
                        let _ = stdout_events
                            .send(AgentEvent::ProtocolError(format!(
                                "failed to read agent output: {error}"
                            )))
                            .await;
                        break;
                    }
                }
            }
        });

        let stderr_events = event_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty()
                    && stderr_events.send(AgentEvent::Stderr(line)).await.is_err()
                {
                    break;
                }
            }
        });

        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                command = control_rx.recv() => {
                    if matches!(command, Some(ProcessControl::Kill)) {
                        let _ = child.kill().await;
                    }
                    child.wait().await
                }
            };
            if let Ok(status) = status {
                let _ = event_tx.send(AgentEvent::Exited(status)).await;
            }
        });

        let process = Self {
            input: input_tx,
            control: control_tx,
        };
        process.send_json(json!({
            "type": "control_request",
            "request_id": request_id(),
            "request": {
                "subtype": "initialize",
                "title": format!("Joey's Claudex v{}", env!("CARGO_PKG_VERSION")),
                "agentProgressSummaries": true,
                "forwardSubagentText": true,
                "promptSuggestions": false
            }
        }))?;
        Ok((process, event_rx))
    }

    pub fn send_user_message(&self, text: &str) -> Result<()> {
        self.send_json(json!({
            "type": "user",
            "session_id": "",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": text}]
            },
            "parent_tool_use_id": null
        }))
    }

    pub fn set_model(&self, model: &str) -> Result<()> {
        self.send_control(json!({"subtype": "set_model", "model": model}))
    }

    pub fn set_permission_mode(&self, mode: &str) -> Result<()> {
        self.send_control(json!({"subtype": "set_permission_mode", "mode": mode}))
    }

    pub fn interrupt(&self) -> Result<()> {
        self.send_control(json!({"subtype": "interrupt"}))
    }

    pub fn terminate(&self) {
        let _ = self.control.send(ProcessControl::Kill);
    }

    pub fn respond_to_permission(
        &self,
        request_id: &str,
        tool_use_id: &str,
        behavior: &str,
        input: &Value,
        message: Option<&str>,
    ) -> Result<()> {
        let response = if behavior == "allow" {
            json!({
                "behavior": "allow",
                "updatedInput": input,
                "toolUseID": tool_use_id,
                "decisionClassification": "user_temporary"
            })
        } else {
            json!({
                "behavior": "deny",
                "message": message.unwrap_or("User denied this action"),
                "toolUseID": tool_use_id,
                "decisionClassification": "user_reject"
            })
        };
        self.send_json(json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": response
            }
        }))
    }

    fn send_control(&self, request: Value) -> Result<()> {
        self.send_json(json!({
            "type": "control_request",
            "request_id": request_id(),
            "request": request
        }))
    }

    fn send_json(&self, value: Value) -> Result<()> {
        self.input
            .send(serde_json::to_string(&value)?)
            .map_err(|_| anyhow::anyhow!("the agent harness is no longer running"))
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        let _ = self.control.send(ProcessControl::Kill);
    }
}

fn request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn ultracode_agents() -> Value {
    json!({
        "claudex-researcher": {
            "description": "Codebase and documentation investigator. Use proactively for independent research that would otherwise fill the main context.",
            "prompt": "Investigate only the assigned question. Read broadly, do not edit files, and return concise evidence with exact file paths and relevant risks.",
            "tools": ["Read", "Glob", "Grep", "WebSearch", "WebFetch"],
            "model": "inherit"
        },
        "claudex-worker": {
            "description": "Bounded implementation worker. Use proactively for an independent workstream with explicitly disjoint file ownership.",
            "prompt": "Implement only the assigned scope. Respect repository instructions, avoid files owned by other workers, run focused checks, and report every file changed.",
            "model": "inherit"
        },
        "claudex-reviewer": {
            "description": "Independent correctness and regression reviewer. Use proactively after material changes or for a parallel risk audit.",
            "prompt": "Review the assigned change without editing files. Look for correctness, security, stability, and missing tests. Return findings ordered by severity with exact file references.",
            "tools": ["Read", "Glob", "Grep", "Bash"],
            "model": "inherit"
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_uses_streaming_sdk_shape() {
        let value = json!({
            "type": "user",
            "session_id": "",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": "hello"}]
            },
            "parent_tool_use_id": null
        });
        assert_eq!(value["message"]["content"][0]["text"], "hello");
        assert_eq!(value["session_id"], "");
    }

    #[test]
    fn ultracode_agents_inherit_the_selected_provider_model() {
        let agents = ultracode_agents();
        let agents = agents.as_object().unwrap();
        assert_eq!(agents.len(), 3);
        assert!(agents.values().all(|agent| agent["model"] == "inherit"));
        assert!(agents.values().all(|agent| agent["description"]
            .as_str()
            .is_some_and(|description| description.contains("Use proactively"))));
        assert!(ULTRACODE_SYSTEM_PROMPT.contains("delegate bounded work"));
        assert!(ULTRACODE_SYSTEM_PROMPT.contains("Run independent investigations in parallel"));
    }
}
