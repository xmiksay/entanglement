//! `config.yml` `endpoints:` schema (#560 P8) — see the module doc at
//! [`super`] for the full picture.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Map, Value};

use entanglement_core::EndpointMethod;

/// One `endpoints:` entry. `deny_unknown_fields` matches every other config
/// section (a typo'd key is a loud "validating merged user config" error, not
/// a silent drop).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointConfig {
    /// HTTP method. Case-insensitive; validated at config-parse time by
    /// [`EndpointMethod`]'s own `Deserialize` impl — an unsupported verb is a
    /// startup error, never a first-call surprise. Defaults to `GET`.
    #[serde(default = "default_method")]
    pub method: EndpointMethod,
    /// URL template: `{{param}}` tokens are substituted from the tool call's
    /// arguments (percent-encoded) at call time.
    pub url: String,
    /// Surfaced as the tool's advertised description.
    #[serde(default)]
    pub description: String,
    /// Declared parameters — drives both the advertised JSON Schema (so P4's
    /// pre-dispatch validation and `describe()` work naturally) and which
    /// `{{param}}` tokens the URL/body templates may reference.
    #[serde(default)]
    pub params: HashMap<String, EndpointParam>,
    /// Static request headers. Values may reference `${VAR}` from the
    /// environment, exactly like an MCP server's static headers.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Optional static body template (`{{param}}` tokens substituted,
    /// unencoded — unlike the URL template, a body has no single universal
    /// escaping rule, so the author is expected to shape it, e.g. a JSON
    /// template with string-typed params). Absent ⇒ no request body.
    #[serde(default)]
    pub body: Option<String>,
}

fn default_method() -> EndpointMethod {
    EndpointMethod::Get
}

/// One declared parameter of an [`EndpointConfig`] (or a skill's inline
/// endpoint-kind tool, `skills::tools::SkillToolDef::Endpoint`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointParam {
    /// JSON Schema primitive type (`string`/`number`/`integer`/`boolean`).
    /// Not validated against a fixed list — an author can spell out anything
    /// JSON Schema accepts; a typo just surfaces as a schema mismatch on a
    /// call, same as it would if hand-authored.
    #[serde(default = "default_param_type", rename = "type")]
    pub param_type: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_required")]
    pub required: bool,
}

fn default_param_type() -> String {
    "string".to_string()
}

fn default_required() -> bool {
    true
}

/// Build the JSON Schema `describe()`/P4 validation and the native tools
/// array all see — the same shape for every `EndpointConfig`-backed tool,
/// name-sorted for a deterministic, byte-stable schema across process
/// restarts.
pub fn build_schema(cfg: &EndpointConfig) -> Value {
    let mut names: Vec<&String> = cfg.params.keys().collect();
    names.sort();
    let mut properties = Map::new();
    let mut required = Vec::new();
    for name in names {
        let p = &cfg.params[name];
        properties.insert(
            name.clone(),
            json!({ "type": p.param_type, "description": p.description }),
        );
        if p.required {
            required.push(Value::String(name.clone()));
        }
    }
    let mut schema = json!({ "type": "object", "properties": properties });
    if !required.is_empty() {
        schema["required"] = Value::Array(required);
    }
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> EndpointConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn method_defaults_to_get() {
        let cfg = parse("url: https://example.com/x");
        assert_eq!(cfg.method, EndpointMethod::Get);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err = serde_yaml::from_str::<EndpointConfig>("url: https://example.com/x\ntypo: 1")
            .unwrap_err();
        assert!(format!("{err}").contains("typo"), "{err}");
    }

    #[test]
    fn bad_method_is_a_loud_error() {
        let err =
            serde_yaml::from_str::<EndpointConfig>("url: https://example.com/x\nmethod: FETCH")
                .unwrap_err();
        assert!(format!("{err}").contains("FETCH"), "{err}");
    }

    #[test]
    fn schema_reflects_required_and_optional_params() {
        let cfg = parse(
            "url: https://example.com/{{city}}\n\
             params:\n\
             \x20 city: { type: string, description: city name }\n\
             \x20 units: { type: string, required: false }\n",
        );
        let schema = build_schema(&cfg);
        assert_eq!(schema["properties"]["city"]["type"], "string");
        assert_eq!(schema["required"], json!(["city"]));
    }

    #[test]
    fn empty_params_omits_required_key() {
        let cfg = parse("url: https://example.com/x");
        let schema = build_schema(&cfg);
        assert!(schema.get("required").is_none());
    }
}
