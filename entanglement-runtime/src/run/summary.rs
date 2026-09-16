//! One-line summaries and readable argument/output text for a tool call,
//! shared by the `run` text head and the TUI transcript (ADR-0204 §6): both
//! heads name a call by its real tool and describe it the same way, never as
//! raw JSON. Pure string work — no ratatui — so the text head stays UI-free.

use std::borrow::Cow;

use entanglement_runtime::permission;
use serde_json::Value;

/// Character budget for a prose hint (an `agent` prompt, an `ask_user`
/// question) inside a one-line summary.
const HINT_CHARS: usize = 40;
/// Character budget for the generic first-scalar hint.
const SCALAR_CHARS: usize = 60;

/// The call to display for a frame that may still carry an `invoke`
/// envelope. Core unwraps before emitting `ToolCall` (ADR-0204 §3), but a
/// streamed `ToolCallDelta` names `invoke` until its `ToolCall` lands, so a
/// well-formed `{name, args}` envelope renders as the inner call here too.
/// Anything core would not unwrap (§4 edge rules) is returned unchanged.
pub(crate) fn unwrap_invoke<'a>(tool: &'a str, input: &'a str) -> (Cow<'a, str>, Cow<'a, str>) {
    let inner = (tool == "invoke")
        .then(|| serde_json::from_str::<Value>(input).ok())
        .flatten()
        .and_then(|envelope| {
            let name = envelope.get("name")?.as_str()?;
            if name == "invoke" {
                return None;
            }
            let args = match envelope.get("args") {
                None | Some(Value::Null) => Value::Object(Default::default()),
                Some(Value::String(s)) => serde_json::from_str(s).ok()?,
                Some(other) => other.clone(),
            };
            args.is_object()
                .then(|| (name.to_string(), args.to_string()))
        });
    match inner {
        Some((name, args)) => (Cow::Owned(name), Cow::Owned(args)),
        None => (Cow::Borrowed(tool), Cow::Borrowed(input)),
    }
}

/// The human name of a tool: `mcp__<server>__<tool>` and
/// `skill__<skill>__<tool>` read `server › tool`, `endpoint__<name>` reads
/// `endpoint › name`; every other name is shown as is.
pub(crate) fn display_name(tool: &str) -> Cow<'_, str> {
    let namespaced = tool
        .strip_prefix("mcp__")
        .or_else(|| tool.strip_prefix("skill__"))
        .and_then(|rest| rest.split_once("__"));
    if let Some((owner, name)) = namespaced {
        return Cow::Owned(format!("{owner} › {name}"));
    }
    match tool.strip_prefix("endpoint__") {
        Some(name) => Cow::Owned(format!("endpoint › {name}")),
        None => Cow::Borrowed(tool),
    }
}

/// The primary argument of a call: the permission-graded value for the file
/// and exec tools (the same string an argument-scoped rule matches), a
/// readable hint for the orchestration/discovery tools, else a `pattern` or
/// the first scalar argument. `None` when nothing informative exists.
pub(crate) fn primary_arg(tool: &str, input: &str) -> Option<String> {
    if let Some(arg) = permission::permission_arg(tool, input) {
        return Some(arg);
    }
    let value: Value = serde_json::from_str(input).ok()?;
    orchestration_arg(tool, &value)
        .or_else(|| text(&value, "pattern").map(String::from))
        .or_else(|| first_scalar(&value))
}

fn orchestration_arg(tool: &str, value: &Value) -> Option<String> {
    match tool {
        "agent" => with_prompt_hint(text(value, "agent")?, value),
        "agent_send" => with_prompt_hint(text(value, "agent_id")?, value),
        "poll" => text(value, "handle").map(String::from),
        "ask_user" => ask_user_arg(value),
        "propose_plan" => Some("plan".to_string()),
        "update_tasks" => Some("snapshot".to_string()),
        "load_skill" => text(value, "skill_name").map(String::from),
        "mcp_enable" => text(value, "server").map(String::from),
        "rhai" => text(value, "script")?
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| truncate_chars(line, HINT_CHARS)),
        "explore" => Some(match (text(value, "filter"), text(value, "kind")) {
            (Some(filter), Some(kind)) => format!("\"{filter}\" in {kind}"),
            (Some(filter), None) => format!("\"{filter}\""),
            (None, Some(kind)) => format!("kind {kind}"),
            (None, None) => "whole catalog".to_string(),
        }),
        "describe" => {
            let names: Vec<&str> = value
                .get("names")?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            Some(names.join(", "))
        }
        _ => None,
    }
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn with_prompt_hint(head: &str, value: &Value) -> Option<String> {
    Some(match text(value, "prompt") {
        Some(prompt) => format!("{head}  {}", truncate_chars(prompt, HINT_CHARS)),
        None => head.to_string(),
    })
}

/// The first question of an `ask_user` call — the `questions` array shape or
/// the legacy single-question shape a pre-#488 log still carries.
fn ask_user_arg(value: &Value) -> Option<String> {
    let questions = value.get("questions").and_then(Value::as_array);
    let first = match questions {
        Some(list) => list.first()?,
        None => value,
    };
    let question = truncate_chars(text(first, "question")?, HINT_CHARS);
    Some(match questions.map_or(0, Vec::len) {
        n if n > 1 => format!("{question} (+{} more)", n - 1),
        _ => question,
    })
}

/// The first scalar value of a JSON object — the hint for a call whose input
/// shape is server-defined (MCP, endpoints, skill tools, unknown names).
fn first_scalar(value: &Value) -> Option<String> {
    value.as_object()?.values().find_map(|v| match v {
        Value::String(s) => Some(truncate_chars(s, SCALAR_CHARS)),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    })
}

/// Truncate `s` to `max` characters, marking a cut with `…`.
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// `name  arg` on one line (a multi-line arg keeps its first line, marked
/// `…`) — the text head's `→`/`?` summary.
pub(crate) fn call_line(tool: &str, input: &str) -> String {
    let (tool, input) = unwrap_invoke(tool, input);
    let name = display_name(&tool);
    let Some(arg) = primary_arg(&tool, &input) else {
        return name.into_owned();
    };
    let mut lines = arg.lines();
    let first = lines.next().unwrap_or_default();
    let more = if lines.next().is_some() { " …" } else { "" };
    format!("{name}  {first}{more}")
}

/// A call's arguments as `key: value` lines ([`readable_lines`]); non-JSON
/// input (a still-streaming fragment) is kept as its raw lines.
pub(crate) fn readable_args(input: &str) -> Vec<String> {
    match serde_json::from_str::<Value>(input) {
        Ok(Value::Object(map)) if map.is_empty() => vec!["(no arguments)".to_string()],
        Ok(value) => readable_lines(&value),
        Err(_) => input.lines().map(String::from).collect(),
    }
}

/// A tool's output: a JSON object/array becomes `key: value` lines, any
/// other text is kept line for line.
pub(crate) fn readable_output(output: &str) -> Vec<String> {
    let trimmed = output.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(value) = serde_json::from_str::<Value>(output) {
            return readable_lines(&value);
        }
    }
    output.lines().map(String::from).collect()
}

/// A JSON value as unquoted, indented lines: objects as `key: value` (a
/// nested value moves under its key, two columns deeper), arrays as `- item`,
/// multi-line strings kept as their own lines rather than `\n`-escaped.
pub(crate) fn readable_lines(value: &Value) -> Vec<String> {
    match value {
        Value::Object(map) if map.is_empty() => vec!["{}".to_string()],
        Value::Array(items) if items.is_empty() => vec!["[]".to_string()],
        Value::Object(map) => map
            .iter()
            .flat_map(|(key, v)| labeled(&format!("{key}:"), v))
            .collect(),
        Value::Array(items) => items.iter().flat_map(dash_item).collect(),
        Value::String(s) if s.is_empty() => vec![String::new()],
        Value::String(s) => s.lines().map(String::from).collect(),
        other => vec![other.to_string()],
    }
}

fn labeled(label: &str, value: &Value) -> Vec<String> {
    let body = readable_lines(value);
    let container = matches!(value, Value::Object(m) if !m.is_empty())
        || matches!(value, Value::Array(a) if !a.is_empty());
    match body.as_slice() {
        [single] if !container => vec![format!("{label} {single}").trim_end().to_string()],
        _ => std::iter::once(label.to_string())
            .chain(body.into_iter().map(|line| format!("  {line}")))
            .collect(),
    }
}

fn dash_item(value: &Value) -> Vec<String> {
    readable_lines(value)
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                format!("- {line}")
            } else {
                format!("  {line}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unwrap_invoke_yields_the_inner_call() {
        let (tool, input) = unwrap_invoke("invoke", r#"{"name":"edit","args":{"path":"a.rs"}}"#);
        assert_eq!(
            (tool.as_ref(), input.as_ref()),
            ("edit", r#"{"path":"a.rs"}"#)
        );
        let (tool, input) =
            unwrap_invoke("invoke", r#"{"name":"grep","args":"{\"pattern\":\"x\"}"}"#);
        assert_eq!(
            (tool.as_ref(), input.as_ref()),
            ("grep", r#"{"pattern":"x"}"#)
        );
        let (tool, input) = unwrap_invoke("invoke", r#"{"name":"explore"}"#);
        assert_eq!((tool.as_ref(), input.as_ref()), ("explore", "{}"));
    }

    #[test]
    fn unwrap_invoke_leaves_what_core_would_not_unwrap() {
        for input in [
            r#"{"name":"invoke","args":{}}"#,
            r#"{"args":{}}"#,
            r#"{"name":"edit","args":[1]}"#,
            r#"{"name":"ed"#,
        ] {
            assert_eq!(unwrap_invoke("invoke", input).0, "invoke", "{input}");
        }
        assert_eq!(unwrap_invoke("read", "{}").0, "read");
    }

    #[test]
    fn display_name_splits_namespaced_tools() {
        assert_eq!(display_name("mcp__chess__makemove"), "chess › makemove");
        assert_eq!(display_name("skill__arch__check"), "arch › check");
        assert_eq!(display_name("endpoint__weather"), "endpoint › weather");
        assert_eq!(display_name("edit"), "edit");
    }

    #[test]
    fn primary_arg_covers_discovery_script_and_mcp_management_tools() {
        let arg = |tool, input| primary_arg(tool, input);
        assert_eq!(
            arg("explore", r#"{"filter":"git"}"#).as_deref(),
            Some("\"git\"")
        );
        assert_eq!(arg("explore", "{}").as_deref(), Some("whole catalog"));
        assert_eq!(
            arg("describe", r#"{"names":["glob","grep"]}"#).as_deref(),
            Some("glob, grep")
        );
        assert_eq!(
            arg("rhai", r#"{"script":"\n let x = 1;\nx"}"#).as_deref(),
            Some("let x = 1;")
        );
        assert_eq!(
            arg("mcp_enable", r#"{"server":"github"}"#).as_deref(),
            Some("github")
        );
        assert_eq!(
            arg(
                "ask_user",
                r#"{"questions":[{"question":"A?"},{"question":"B?"}]}"#
            )
            .as_deref(),
            Some("A? (+1 more)")
        );
    }

    #[test]
    fn readable_lines_render_nested_structure_without_json_syntax() {
        let value = json!({
            "a": 1,
            "b": {"c": "x"},
            "d": ["p", {"q": 2}],
            "e": "l1\nl2",
            "f": []
        });
        assert_eq!(
            readable_lines(&value),
            vec!["a: 1", "b:", "  c: x", "d:", "  - p", "  - q: 2", "e:", "  l1", "  l2", "f: []"]
        );
    }

    #[test]
    fn readable_output_structures_json_and_keeps_text() {
        assert_eq!(readable_output(r#"{"ok":true}"#), vec!["ok: true"]);
        assert_eq!(
            readable_output("plain\n{not json"),
            vec!["plain", "{not json"]
        );
        assert_eq!(readable_args("{}"), vec!["(no arguments)"]);
    }

    #[test]
    fn call_line_is_one_readable_line() {
        assert_eq!(
            call_line("bash", r#"{"command":"echo a\necho b"}"#),
            "bash  echo a …"
        );
        assert_eq!(
            call_line(
                "invoke",
                r#"{"name":"mcp__gh__issue","args":{"title":"Bug"}}"#
            ),
            "gh › issue  Bug"
        );
        assert_eq!(call_line("propose_plan", "not json"), "propose_plan");
    }
}
