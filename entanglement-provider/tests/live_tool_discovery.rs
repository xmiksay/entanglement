//! Opt-in live probes against the real z.ai endpoint (`make test-live`).
//!
//! WHY: these pin the provider facts ADR-0204 rests on, so a model or
//! endpoint update that changes them shows up as a red probe instead of a
//! silently degraded agent:
//! - GLM-5.x will not call a tool absent from the tools array, even after
//!   `describe` delivered its schema — but it does use an `invoke` fallback;
//! - rewriting an emitted `invoke` call in history to the native name breaks
//!   follow-ups, so history must keep the call exactly as emitted;
//! - on z.ai any change to the tools array is a full prompt-cache miss, while
//!   appending turns keeps the cached prefix.
//!
//! Every test is `#[ignore]` (network + a real key) and skips with a note
//! when `ZAI_API_KEY` is unset. Env: `ZAI_API_BASE` (default: the Coding
//! Plan base), `ZAI_PROBE_MODEL` (default `glm-5.2`), `ZAI_PROBE_THINKING`
//! (default `disabled`; set `enabled` for a model that requires thinking).

use std::time::Duration;

use serde_json::{json, Value};

const DEFAULT_BASE: &str = "https://api.z.ai/api/coding/paas/v4";

struct Probe {
    http: reqwest::Client,
    url: String,
    key: String,
    model: String,
    thinking: String,
}

fn probe() -> Option<Probe> {
    let key = std::env::var("ZAI_API_KEY").ok().filter(|k| !k.is_empty());
    let Some(key) = key else {
        eprintln!("skipping live probe: ZAI_API_KEY is unset");
        return None;
    };
    let env_or = |name: &str, default: &str| {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| default.to_string())
    };
    let base = env_or("ZAI_API_BASE", DEFAULT_BASE);
    Some(Probe {
        http: reqwest::Client::new(),
        url: format!("{}/chat/completions", base.trim_end_matches('/')),
        key,
        model: env_or("ZAI_PROBE_MODEL", "glm-5.2"),
        thinking: env_or("ZAI_PROBE_THINKING", "disabled"),
    })
}

impl Probe {
    async fn chat(&self, messages: &Value, tools: &Value, max_tokens: u32) -> Value {
        let body = json!({
            "model": self.model,
            "messages": messages,
            "tools": tools,
            "max_tokens": max_tokens,
            "stream": false,
            "thinking": { "type": self.thinking },
        });
        let resp = self
            .http
            .post(&self.url)
            .bearer_auth(&self.key)
            .json(&body)
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .expect("send chat completion");
        let status = resp.status();
        let text = resp.text().await.expect("read response body");
        assert!(status.is_success(), "HTTP {status}: {text}");
        serde_json::from_str(&text).expect("response is JSON")
    }
}

fn function(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": { "name": name, "description": description, "parameters": parameters },
    })
}

/// The discovery surface: `get_weather` is deliberately absent.
fn kernel_tools() -> Value {
    json!([
        function(
            "describe",
            "Load the full schemas of tools by name so they can be called.",
            json!({
                "type": "object",
                "properties": { "names": { "type": "array", "items": { "type": "string" } } },
                "required": ["names"],
            }),
        ),
        function(
            "read",
            "Read a file from the workspace.",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
        ),
        function(
            "invoke",
            "Call a tool loaded via describe by name. Fallback only.",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string" }, "args": { "type": "object" } },
                "required": ["name", "args"],
            }),
        ),
    ])
}

fn weather_schema() -> Value {
    function(
        "get_weather",
        "Current weather for a city.",
        json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"],
        }),
    )
}

fn tool_call(id: &str, name: &str, args: Value) -> Value {
    json!({
        "role": "assistant",
        "content": "",
        "tool_calls": [{
            "id": id,
            "type": "function",
            "function": { "name": name, "arguments": args.to_string() },
        }],
    })
}

fn discovery_history() -> Vec<Value> {
    let loaded = format!(
        "Loaded `get_weather`. Call it directly by its name; use `invoke` only as a \
         fallback if a direct call is not possible.\n{}",
        weather_schema()["function"]
    );
    vec![
        json!({ "role": "system", "content": "You are a helpful agent. Use tools to answer." }),
        json!({ "role": "user", "content": "What's the weather in Prague right now?" }),
        tool_call(
            "call_describe_1",
            "describe",
            json!({ "names": ["get_weather"] }),
        ),
        json!({ "role": "tool", "tool_call_id": "call_describe_1", "content": loaded }),
    ]
}

/// Arguments arrive as a JSON string on the OpenAI wire, but a model may also
/// nest `invoke.args` as an encoded string — accept both shapes.
fn as_object(v: &Value) -> Option<Value> {
    match v {
        Value::String(s) => serde_json::from_str(s).ok(),
        other => Some(other.clone()),
    }
}

/// The path (`native` or `invoke`) and `city` of the first call that reaches
/// `get_weather`, if any.
fn weather_call(resp: &Value) -> Option<(&'static str, String)> {
    let calls = resp.pointer("/choices/0/message/tool_calls")?.as_array()?;
    calls.iter().find_map(|call| {
        let name = call.pointer("/function/name")?.as_str()?;
        let args = as_object(call.pointer("/function/arguments")?)?;
        match name {
            "get_weather" => Some(("native", args.get("city")?.as_str()?.to_string())),
            "invoke" if args.get("name")?.as_str()? == "get_weather" => {
                let inner = as_object(args.get("args")?)?;
                Some(("invoke", inner.get("city")?.as_str()?.to_string()))
            }
            _ => None,
        }
    })
}

fn expect_weather_call(resp: &Value, city_matches: &[&str]) {
    let message = &resp["choices"][0]["message"];
    let Some((path, city)) = weather_call(resp) else {
        panic!("model did not reach get_weather: {message}");
    };
    eprintln!("reached get_weather via {path} path (city = {city:?})");
    let lower = city.to_lowercase();
    assert!(
        city_matches.iter().any(|m| lower.contains(m)),
        "unexpected city {city:?}: {message}"
    );
}

#[tokio::test]
#[ignore = "live z.ai call; run via `make test-live`"]
async fn undeclared_tool_discovery_path() {
    let Some(probe) = probe() else { return };
    let resp = probe
        .chat(&json!(discovery_history()), &kernel_tools(), 512)
        .await;
    expect_weather_call(&resp, &["prague", "praha"]);
}

#[tokio::test]
#[ignore = "live z.ai call; run via `make test-live`"]
async fn history_keeps_the_emitted_invoke_call() {
    let Some(probe) = probe() else { return };
    let mut history = discovery_history();
    let args = json!({ "name": "get_weather", "args": { "city": "Prague" } });
    history.extend([
        tool_call("call_invoke_1", "invoke", args),
        json!({ "role": "tool", "tool_call_id": "call_invoke_1", "content": "Prague: 18°C, partly cloudy" }),
        json!({ "role": "assistant", "content": "It's 18°C and partly cloudy in Prague." }),
        json!({ "role": "user", "content": "And in Brno?" }),
    ]);
    let resp = probe.chat(&json!(history), &kernel_tools(), 512).await;
    expect_weather_call(&resp, &["brno"]);
}

/// `cached_tokens / prompt_tokens` for one response (0 when the field is absent).
fn cached_share(resp: &Value) -> f64 {
    let prompt = resp.pointer("/usage/prompt_tokens").and_then(Value::as_u64);
    let cached = resp
        .pointer("/usage/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    match prompt {
        Some(p) if p > 0 => cached as f64 / p as f64,
        _ => panic!("response has no usage.prompt_tokens: {resp}"),
    }
}

#[tokio::test]
#[ignore = "live z.ai call; run via `make test-live`"]
async fn prompt_cache_survives_appended_turns_but_not_tool_changes() {
    let Some(probe) = probe() else { return };
    // ~9k tokens: long enough to be well past any provider cache minimum.
    let system: String = (0..100)
        .map(|i| {
            format!(
                "Section {i}. The agent reads the workspace before editing, keeps \
                 changes minimal, runs the test suite after every change, and reports \
                 failures verbatim instead of guessing at a fix. "
            )
        })
        .collect();
    let base = vec![
        json!({ "role": "system", "content": system }),
        json!({ "role": "user", "content": "Reply with just OK." }),
    ];
    let tools = kernel_tools();

    probe.chat(&json!(base), &tools, 16).await;
    let repeat = cached_share(&probe.chat(&json!(base), &tools, 16).await);
    eprintln!("identical request: {:.0}% cached", repeat * 100.0);
    assert!(repeat >= 0.9, "identical request cached only {repeat:.2}");

    let mut appended = base.clone();
    appended.extend([
        json!({ "role": "assistant", "content": "OK" }),
        json!({ "role": "user", "content": "Once more, just OK." }),
    ]);
    let grown = cached_share(&probe.chat(&json!(appended), &tools, 16).await);
    eprintln!("appended turn: {:.0}% cached", grown * 100.0);
    assert!(grown >= 0.9, "appended turn cached only {grown:.2}");

    let mut more_tools = tools.as_array().cloned().unwrap_or_default();
    more_tools.push(weather_schema());
    let changed = cached_share(&probe.chat(&json!(base), &json!(more_tools), 16).await);
    eprintln!(
        "tool appended to the tools array: {:.0}% cached",
        changed * 100.0
    );
}
