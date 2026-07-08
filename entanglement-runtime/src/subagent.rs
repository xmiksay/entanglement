//! Sub-agent spawn orchestration (#60, ADR-0021/0010).
//!
//! `spawn_agent` is not a filesystem tool in the [`ToolRegistry`] — it is an
//! engine-coordination primitive owned by the runtime. When the model calls it,
//! [`spawn_subagent`] creates a child session via [`InMsg::Spawn`], lets it run
//! to completion, and relays the child's final answer back to the parent as the
//! tool's output. The parent's turn loop sees an ordinary tool result (#58), so
//! core needs no notion of "child session".
//!
//! Because it only orchestrates sessions (it touches no host resource), the
//! executor runs it *before* permission resolution — it bypasses the permission
//! profile exactly like core's `update_plan` / `update_tasks` built-ins.

use entanglement_core::{Holly, InMsg, OutEvent, SessionId, ToolSpec};
use tokio::sync::broadcast::{error::RecvError, Receiver};

/// Tool name the model calls to spawn a sub-agent.
pub const SPAWN_TOOL: &str = "spawn_agent";

/// Sub-agent profile used when the model omits `agent` — read-only explore is
/// the safe default.
const DEFAULT_SUBAGENT: &str = "explore";

/// The `spawn_agent` tool schema advertised to the model. Appended to the
/// engine's `tool_specs` alongside the host quartet.
pub fn spawn_agent_spec() -> ToolSpec {
    ToolSpec::with_schema(
        SPAWN_TOOL,
        "Spawn a sub-agent session to handle a focused subtask. The sub-agent \
         runs to completion under the named agent profile and its final answer \
         becomes this tool's output.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Agent profile for the sub-agent (build | plan | explore | custom). Defaults to explore (read-only)."
                },
                "prompt": {
                    "type": "string",
                    "description": "The task or question for the sub-agent to work on."
                }
            },
            "required": ["agent", "prompt"]
        }),
    )
}

/// Orchestrate one `spawn_agent` call: start a child session, run `prompt`
/// under `agent`, and reply to `parent` with the child's final text.
///
/// `events` must be a receiver subscribed *before* the [`InMsg::Spawn`] is sent
/// (the caller subscribes synchronously), so the child's events — including its
/// terminal `Done` — cannot race ahead of the watcher.
pub async fn spawn_subagent(
    holly: Holly,
    mut events: Receiver<OutEvent>,
    parent: SessionId,
    request_id: String,
    input: String,
) {
    let (agent, prompt) = parse_input(&input);
    let child = SessionId::new_uuid();

    if holly
        .send(InMsg::Spawn {
            session: child.clone(),
            parent: parent.clone(),
            agent,
            prompt,
        })
        .await
        .is_err()
    {
        reply(
            &holly,
            parent,
            request_id,
            "sub-agent spawn failed: engine inbox closed".to_string(),
        )
        .await;
        return;
    }

    let output = collect_child_answer(&mut events, &child).await;
    reply(&holly, parent, request_id, output).await;
}

/// Watch the child's event stream, accumulating its assistant text until the
/// child's turn finishes (`Done`). Returns the final answer, or an explanatory
/// note when the child errored or produced nothing.
async fn collect_child_answer(events: &mut Receiver<OutEvent>, child: &SessionId) -> String {
    let mut text = String::new();
    let mut error: Option<String> = None;
    loop {
        match events.recv().await {
            Ok(ev) if ev.session() != child => {}
            Ok(OutEvent::TextDelta { text: delta, .. }) => text.push_str(&delta),
            Ok(OutEvent::Error { message, .. }) => error = Some(message),
            Ok(OutEvent::Done { .. }) => break,
            Ok(_) => {}
            // A lagging watcher could miss the child's `Done` and park the parent
            // forever; surface what we have instead of blocking indefinitely.
            Err(RecvError::Lagged(_)) => break,
            Err(RecvError::Closed) => break,
        }
    }
    let text = text.trim();
    match (text.is_empty(), error) {
        (false, _) => text.to_string(),
        (true, Some(e)) => format!("sub-agent ended with error: {e}"),
        (true, None) => "sub-agent produced no output".to_string(),
    }
}

/// Parse the `spawn_agent` tool input. Providers send a JSON object
/// `{"agent": …, "prompt": …}`; scripted/raw backends may send a bare string,
/// which is treated as the prompt under the default sub-agent profile.
fn parse_input(input: &str) -> (String, String) {
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(v) => {
            let agent = v
                .get("agent")
                .and_then(|a| a.as_str())
                .filter(|a| !a.is_empty())
                .unwrap_or(DEFAULT_SUBAGENT)
                .to_string();
            let prompt = v
                .get("prompt")
                .and_then(|p| p.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| input.to_string());
            (agent, prompt)
        }
        Err(_) => (DEFAULT_SUBAGENT.to_string(), input.to_string()),
    }
}

async fn reply(holly: &Holly, session: SessionId, request_id: String, output: String) {
    let _ = holly
        .send(InMsg::ToolResult {
            session,
            request_id,
            output,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_input_reads_json_object() {
        let (agent, prompt) = parse_input(r#"{"agent":"build","prompt":"do it"}"#);
        assert_eq!(agent, "build");
        assert_eq!(prompt, "do it");
    }

    #[test]
    fn parse_input_defaults_agent_to_explore() {
        let (agent, prompt) = parse_input(r#"{"prompt":"look around"}"#);
        assert_eq!(agent, DEFAULT_SUBAGENT);
        assert_eq!(prompt, "look around");
    }

    #[test]
    fn parse_input_falls_back_to_raw_string() {
        let (agent, prompt) = parse_input("just a prompt");
        assert_eq!(agent, DEFAULT_SUBAGENT);
        assert_eq!(prompt, "just a prompt");
    }
}
