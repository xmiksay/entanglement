//! Opt-in live probes of Anthropic prompt caching with deferred tools
//! (`make test-live`).
//!
//! WHY: the `anthropic_native` encoding (ADR-0196 §3, ADR-0202 §1) keeps every
//! non-kernel tool `defer_loading: true` for the whole session, resting on the
//! tool-search docs' statement that "Deferred tools are not included in the
//! system-prompt prefix". These pin that fact against the real API, and
//! measure the two questions ADR-0204 and the TUI `/set` dialog leave open:
//! does adding a deferred tool mid-session (an MCP server enabled live) keep
//! the cache, and does a `tool_reference` in history?
//!
//! Every test is `#[ignore]` (network + a real key) and skips with a note
//! when `ANTHROPIC_API_KEY` is unset. Env: `ANTHROPIC_PROBE_MODEL` (default
//! `claude-haiku-4-5`).

use serde_json::{json, Value};

const URL: &str = "https://api.anthropic.com/v1/messages";

struct Probe {
    http: reqwest::Client,
    key: String,
    model: String,
}

fn probe() -> Option<Probe> {
    let Some(key) = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
    else {
        eprintln!("skipping live probe: ANTHROPIC_API_KEY is unset");
        return None;
    };
    let model = std::env::var("ANTHROPIC_PROBE_MODEL")
        .ok()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "claude-haiku-4-5".to_string());
    Some(Probe {
        http: reqwest::Client::new(),
        key,
        model,
    })
}

/// `usage` of one non-streaming request.
#[derive(Debug, Clone, Copy)]
struct Usage {
    input: u64,
    created: u64,
    read: u64,
}

impl Probe {
    async fn send(&self, tools: &Value, messages: &Value) -> Usage {
        let body = json!({
            "model": self.model,
            "max_tokens": 16,
            "system": [{
                "type": "text",
                "text": system_prompt(),
                "cache_control": {"type": "ephemeral"},
            }],
            "tools": tools,
            "messages": messages,
        });
        let resp = self
            .http
            .post(URL)
            .header("x-api-key", &self.key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .expect("request sent");
        let status = resp.status();
        let json: Value = resp.json().await.expect("json body");
        assert!(status.is_success(), "HTTP {status}: {json}");
        let n = |k: &str| json["usage"][k].as_u64().unwrap_or(0);
        Usage {
            input: n("input_tokens"),
            created: n("cache_creation_input_tokens"),
            read: n("cache_read_input_tokens"),
        }
    }

    /// Send the base request until it reads from the cache; the cacheable
    /// prefix is whatever that warm read covers.
    async fn warm_prefix(&self) -> u64 {
        let first = self.send(&tools(10), &base_messages()).await;
        eprintln!("base #1: {first:?}");
        let second = self.send(&tools(10), &base_messages()).await;
        eprintln!("base #2: {second:?}");
        first.created.max(first.read).max(second.read)
    }
}

/// ~6k tokens of distinct, deterministic text: above every model's minimum
/// cacheable length.
fn system_prompt() -> String {
    (0..600)
        .map(|i| format!("Rule {i}: keep the probe deterministic and answer in one word. "))
        .collect()
}

fn tool(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "input_schema": {
            "type": "object",
            "properties": {"query": {"type": "string"}},
        },
    })
}

/// `describe`, the kernel `read` (carrying the tools breakpoint — a
/// deferred tool may not), then `deferred` `defer_loading` tools.
fn tools(deferred: usize) -> Value {
    let mut all = vec![
        tool("describe", "Load the schema of a discovered tool by name."),
        {
            let mut read = tool("read", "Read a file.");
            read["cache_control"] = json!({"type": "ephemeral"});
            read
        },
    ];
    for i in 0..deferred {
        let mut t = tool(
            &format!("tool_{i:02}"),
            &format!("Deferred probe tool {i}."),
        );
        t["defer_loading"] = json!(true);
        all.push(t);
    }
    Value::Array(all)
}

fn base_messages() -> Value {
    json!([{"role": "user", "content": "Reply with the word ok."}])
}

fn share(read: u64, prefix: u64) -> f64 {
    read as f64 / prefix.max(1) as f64
}

#[tokio::test]
#[ignore = "live Anthropic API; run via make test-live"]
async fn identical_repeat_and_an_appended_turn_read_the_cached_prefix() {
    let Some(p) = probe() else { return };
    let prefix = p.warm_prefix().await;
    let repeat = p.send(&tools(10), &base_messages()).await;
    eprintln!("repeat: {repeat:?} share {:.2}", share(repeat.read, prefix));
    assert!(
        share(repeat.read, prefix) >= 0.9,
        "{repeat:?} vs prefix {prefix}"
    );

    let appended = json!([
        {"role": "user", "content": "Reply with the word ok."},
        {"role": "assistant", "content": "ok"},
        {"role": "user", "content": "Once more."},
    ]);
    let turn = p.send(&tools(10), &appended).await;
    eprintln!(
        "appended turn: {turn:?} share {:.2}",
        share(turn.read, prefix)
    );
    assert!(
        share(turn.read, prefix) >= 0.9,
        "{turn:?} vs prefix {prefix}"
    );
}

#[tokio::test]
#[ignore = "live Anthropic API; run via make test-live"]
async fn adding_one_deferred_tool_reports_its_cache_share() {
    let Some(p) = probe() else { return };
    let prefix = p.warm_prefix().await;
    let grown = p.send(&tools(11), &base_messages()).await;
    eprintln!(
        "one new deferred tool: {grown:?} read share {:.2} of prefix {prefix}",
        share(grown.read, prefix)
    );
}

#[tokio::test]
#[ignore = "live Anthropic API; run via make test-live"]
async fn a_tool_reference_in_history_reports_its_cache_share() {
    let Some(p) = probe() else { return };
    let prefix = p.warm_prefix().await;
    let history = json!([
        {"role": "user", "content": "Load tool_03."},
        {"role": "assistant", "content": [{
            "type": "tool_use",
            "id": "toolu_probe_1",
            "name": "describe",
            "input": {"query": "tool_03"},
        }]},
        {"role": "user", "content": [{
            "type": "tool_result",
            "tool_use_id": "toolu_probe_1",
            "content": [{"type": "tool_reference", "tool_name": "tool_03"}],
        }]},
    ]);
    let referenced = p.send(&tools(10), &history).await;
    eprintln!(
        "tool_reference in history: {referenced:?} read share {:.2} of prefix {prefix}",
        share(referenced.read, prefix)
    );
    assert!(referenced.input > 0);
}
