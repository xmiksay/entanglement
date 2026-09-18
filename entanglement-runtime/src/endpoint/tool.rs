//! [`EndpointTool`] — the `Tool` impl every `endpoints:` entry (and every
//! skill-declared endpoint-kind tool, `skills::tools::SkillToolDef::Endpoint`)
//! becomes. Execution rides `entanglement_core::endpoint_call` (provider-
//! owned, ADR-0053) — this module never touches `reqwest` directly.

use std::borrow::Cow;
use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Map, Value};

use entanglement_core::HttpClient;

use crate::capability::Capability;
use crate::tools::{Tool, ToolRegistry};

use super::config::{build_schema, EndpointConfig};

/// One registered endpoint tool: `endpoint__<name>` for a global `config.yml`
/// entry, `skill__<skill>__<name>` for a skill-declared one — the namespace
/// is the caller's choice ([`EndpointTool::new`] takes the final name
/// pre-built), everything else is identical.
pub struct EndpointTool {
    name: String,
    description: String,
    schema: Value,
    method: entanglement_core::EndpointMethod,
    url_template: String,
    headers: HashMap<String, String>,
    body_template: Option<String>,
    http: HttpClient,
}

impl EndpointTool {
    pub fn new(name: String, cfg: &EndpointConfig, http: HttpClient) -> Self {
        Self {
            name,
            description: cfg.description.clone(),
            schema: build_schema(cfg),
            method: cfg.method,
            url_template: cfg.url.clone(),
            headers: cfg.headers.clone(),
            body_template: cfg.body.clone(),
            http,
        }
    }
}

#[async_trait]
impl Tool for EndpointTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(self.name.clone())
    }

    // Every config-declared `endpoint__<name>` tool is a network call to an
    // outside base URL, regardless of what the endpoint itself does with the
    // request (tool_names.rs's `CAPABILITIES` comment makes the same call for
    // the old `call` capability-key fan-out).
    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::Exec]
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    async fn run(&self, input: &str) -> Result<String> {
        let args = parse_args(input);
        let url = substitute(&self.url_template, &args, true);
        let body = self
            .body_template
            .as_ref()
            .map(|t| substitute(t, &args, false));
        let resp = entanglement_core::call_endpoint(
            &self.http,
            self.method,
            &url,
            &self.headers,
            body.as_deref(),
        )
        .await?;
        Ok(format!("HTTP {}\n\n{}", resp.status, resp.body))
    }
}

/// Parse the tool call's JSON input into an object map — an empty/absent
/// body (a param-less endpoint) and malformed JSON both degrade to "no
/// arguments" rather than erroring: a missing *required* param is already
/// caught by P4's pre-dispatch schema validation before `run` is ever
/// reached, so this path only ever sees a call that already passed.
fn parse_args(input: &str) -> Map<String, Value> {
    if input.trim().is_empty() {
        return Map::new();
    }
    serde_json::from_str::<Value>(input)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Render `arg` as the raw string a template substitution inserts. A JSON
/// string inserts verbatim; every other JSON type (number/bool/array/object)
/// renders as its JSON text — reasonable for a URL/body token, and simple.
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Substitute every `{{name}}` token in `template` from `args`, percent-
/// encoding the value when `encode` (the URL template) — the body template
/// substitutes raw, since a body has no single universal escaping rule.
/// Double braces (Mustache-style), not single: a body template is commonly
/// itself JSON, whose own literal `{`/`}` structural braces would otherwise
/// collide with a single-brace token delimiter. A token whose name isn't in
/// `args` substitutes to an empty string; an unterminated `{{` is emitted
/// verbatim rather than erroring — a config-authored template is trusted
/// input, not something to be strict about at the risk of an opaque failure
/// on an otherwise-working call.
fn substitute(template: &str, args: &Map<String, Value>, encode: bool) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = after[..end].trim();
        let value = args.get(name).map(value_to_string).unwrap_or_default();
        if encode {
            percent_encode(&value, &mut out);
        } else {
            out.push_str(&value);
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// RFC 3986 unreserved-set percent-encoding — no external crate needed for
/// the handful of characters a URL path/query token requires escaping.
fn percent_encode(s: &str, out: &mut String) {
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
}

/// Every declared endpoint's `call`-capability membership (#560 P8): each
/// `endpoint__<name>` tool is unconditionally a network call, so — unlike an
/// MCP tool, which needs a per-tool `capabilities:` config hint (#426),
/// since a server can expose read/write/call tools alike — every endpoint
/// tool joins a bare `call: allow/ask/deny` with no annotation needed.
/// `entanglement_core::PermissionProfile::resolve` matches a rule key
/// against a tool name literally (or the single `*` wildcard) — not an
/// arbitrary glob — so this can't be a static `endpoint__*` table entry
/// (`tool_names::CAPABILITIES`'s doc); it has to be a concrete per-name
/// list, merged into the *same* data-driven index MCP capabilities use
/// (`crate::mcp::McpCapabilityIndex`'s `"call"` bucket) by the two config-
/// loading call sites (`config::parse`, `main.rs`'s agent-profile loading) —
/// see either call site for the merge.
pub fn call_capability_names(endpoints: &HashMap<String, EndpointConfig>) -> Vec<String> {
    let mut names: Vec<String> = endpoints
        .keys()
        .map(|name| format!("endpoint__{name}"))
        .collect();
    names.sort();
    names
}

/// Register every `config.yml` `endpoints:` entry as `endpoint__<name>`.
/// Registration is startup-only (P8's plan explicitly allows this: wiring
/// live-reload for a third definitions source — beside skills/agents — would
/// need its own debounced watcher wired through `main.rs`'s already-dense
/// startup sequence for a feature with no evidence anyone needs to add an
/// endpoint mid-session; skipped as disproportionate, unlike skills/agents
/// which already had `watch.rs` plumbing to reuse).
pub fn register_endpoints(
    tools: &mut ToolRegistry,
    endpoints: &HashMap<String, EndpointConfig>,
    http: &HttpClient,
) {
    let mut names: Vec<&String> = endpoints.keys().collect();
    names.sort();
    for name in names {
        let cfg = &endpoints[name];
        tools.register(EndpointTool::new(
            format!("endpoint__{name}"),
            cfg,
            http.clone(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_is_exec() {
        let http = HttpClient::new().unwrap();
        let cfg: EndpointConfig = serde_yaml::from_str("url: https://example.com/x").unwrap();
        let tool = EndpointTool::new("weather".to_string(), &cfg, http);
        assert_eq!(tool.capabilities(), &[Capability::Exec]);
    }

    #[test]
    fn substitute_replaces_declared_tokens_and_encodes_for_url() {
        let mut args = Map::new();
        args.insert("city".to_string(), Value::String("New York".to_string()));
        let url = substitute("https://api.example.com/{{city}}/weather", &args, true);
        assert_eq!(url, "https://api.example.com/New%20York/weather");
    }

    #[test]
    fn substitute_leaves_body_unencoded_and_survives_json_structural_braces() {
        let mut args = Map::new();
        args.insert("q".to_string(), Value::String("a b".to_string()));
        let body = substitute(r#"{"query": "{{q}}"}"#, &args, false);
        assert_eq!(body, r#"{"query": "a b"}"#);
    }

    #[test]
    fn substitute_missing_token_becomes_empty() {
        let args = Map::new();
        let out = substitute("https://x/{{missing}}", &args, true);
        assert_eq!(out, "https://x/");
    }

    #[test]
    fn substitute_unterminated_double_brace_is_left_verbatim() {
        let args = Map::new();
        let out = substitute("https://x/{{oops", &args, true);
        assert_eq!(out, "https://x/{{oops");
    }

    #[test]
    fn parse_args_degrades_gracefully_on_malformed_input() {
        assert!(parse_args("").is_empty());
        assert!(parse_args("not json").is_empty());
        assert!(parse_args("[]").is_empty());
    }

    #[test]
    fn register_endpoints_names_tools_with_the_endpoint_prefix() {
        let http = HttpClient::new().unwrap();
        let cfg: EndpointConfig = serde_yaml::from_str("url: https://example.com/x").unwrap();
        let mut endpoints = HashMap::new();
        endpoints.insert("weather".to_string(), cfg);
        let mut tools = ToolRegistry::new();
        register_endpoints(&mut tools, &endpoints, &http);
        assert!(tools.contains("endpoint__weather"));
    }

    #[test]
    fn call_capability_names_prefixes_and_sorts_every_declared_endpoint() {
        let cfg: EndpointConfig = serde_yaml::from_str("url: https://example.com/x").unwrap();
        let mut endpoints = HashMap::new();
        endpoints.insert("weather".to_string(), cfg.clone());
        endpoints.insert("alpha".to_string(), cfg);
        assert_eq!(
            call_capability_names(&endpoints),
            vec![
                "endpoint__alpha".to_string(),
                "endpoint__weather".to_string()
            ]
        );
    }
}
