//! One-release migration warnings for the knobs ADR-0213, ADR-0226 and
//! ADR-0227 retired.
//!
//! A retired env var or config key is **ignored**, never fatal — but silently
//! ignoring a setting a user still relies on is the worst failure, so each one
//! still present produces exactly one warning line naming its replacement.
//! Planned removal of this module: the release after the one that retired
//! the knobs (#729's 0.7 guide carries the same table).

use std::collections::BTreeSet;

use entanglement_core::{DebugFlags, DEBUG_ENV};
use serde_yaml::Value;

use super::RawLayer;

/// `(retired env var, what replaces it)`.
const RETIRED_ENV: &[(&str, &str)] = &[
    (
        "ENTANGLEMENT_ENABLE_BASH",
        "nothing — `bash` is always registered and graded by the permission mode (ADR-0195)",
    ),
    (
        "ENTANGLEMENT_SESSION_RETENTION_DAYS",
        "`session_retention_days:` in config.yml",
    ),
    (
        "ENTANGLEMENT_TOOL_ADVERTISING",
        "nothing — one advertised surface on every wire, no knob (ADR-0226)",
    ),
    (
        "ENTANGLEMENT_MAX_CONCURRENCY",
        "a per-provider `concurrency:` in providers.yml",
    ),
    (
        "ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE",
        "`ENTANGLEMENT_DEBUG=no-shared-state`",
    ),
    ("ENTANGLEMENT_ECHO_FULL", "`ENTANGLEMENT_DEBUG=echo`"),
    ("ENTANGLEMENT_LOG_BODIES", "`ENTANGLEMENT_DEBUG=bodies`"),
    (
        "ENTANGLEMENT_TUI_NOTIFY",
        "`tui: {notify: true}` in config.yml",
    ),
    (
        "ENTANGLEMENT_TUI_NO_MOUSE",
        "`tui: {mouse: false}` in config.yml",
    ),
    ("ENTANGLEMENT_GRANTS_FILE", STATE_DIR_HINT),
    ("ENTANGLEMENT_AGENT_MODELS_FILE", STATE_DIR_HINT),
    ("ENTANGLEMENT_AGENT_GENERATION_FILE", STATE_DIR_HINT),
    ("ENTANGLEMENT_AUX_MODELS_FILE", STATE_DIR_HINT),
    ("ENTANGLEMENT_EXTRA_ROOTS_FILE", STATE_DIR_HINT),
    ("ENTANGLEMENT_MCP_TOKENS_FILE", STATE_DIR_HINT),
    ("ENTANGLEMENT_LLM_TOKENS_FILE", STATE_DIR_HINT),
];

const STATE_DIR_HINT: &str = "`ENTANGLEMENT_STATE_DIR` (one directory for all managed files)";

/// Per-provider env suffixes no longer read, keyed off each catalog entry's
/// uppercased name; `{NAME}` in the replacement is filled in.
const RETIRED_PROVIDER_SUFFIXES: &[(&str, &str)] = &[
    ("_BASE", "`{NAME}_API_BASE`"),
    ("_RPM", "`rpm:` on the provider in providers.yml"),
    (
        "_CONCURRENCY",
        "`concurrency:` on the provider in providers.yml",
    ),
];

/// Top-level `config.yml` keys no longer read: `(key, retiring ADR, what
/// replaces it)`.
const RETIRED_CONFIG_KEYS: &[(&str, &str, &str)] = &[
    (
        "discovery",
        "ADR-0213",
        "nothing — one advertised surface on every wire (ADR-0226)",
    ),
    (
        "tool_advertising",
        "ADR-0226",
        "nothing — one advertised surface on every wire, no mode",
    ),
    (
        "endpoints",
        "ADR-0227",
        "a skill `lib.rhai` function calling `http()`, credentials in `http_auth:`",
    ),
];

/// What replaces a retired per-server `mcp.<name>.state:` key (ADR-0226).
const MCP_STATE_REPLACEMENT: &str =
    "every configured server is available; `disabled: true` is the one opt-out";

/// One warning per retired env var `is_set` reports, plus one for any
/// unknown `ENTANGLEMENT_DEBUG` token. `providers` are the catalog entry names
/// the per-provider suffixes are checked against. Pure, so every case is
/// testable without mutating process env (#747).
pub fn env_warnings(
    is_set: impl Fn(&str) -> bool,
    debug_value: Option<&str>,
    providers: &[&str],
) -> Vec<String> {
    let mut out: Vec<String> = RETIRED_ENV
        .iter()
        .filter(|(var, _)| is_set(var))
        .map(|(var, with)| retired_line(var, with))
        .collect();
    // A set keeps the order stable and drops a duplicate catalog name.
    let names: BTreeSet<String> = providers.iter().map(|p| p.to_uppercase()).collect();
    for name in &names {
        for (suffix, with) in RETIRED_PROVIDER_SUFFIXES {
            let var = format!("{name}{suffix}");
            if is_set(&var) {
                out.push(retired_line(&var, &with.replace("{NAME}", name)));
            }
        }
    }
    if let Some(raw) = debug_value {
        let (_, unknown) = DebugFlags::parse(raw);
        if !unknown.is_empty() {
            out.push(format!(
                "{DEBUG_ENV}: ignoring unknown token(s) {} — known: bodies, echo, no-shared-state",
                unknown.join(", ")
            ));
        }
    }
    out
}

/// [`env_warnings`] against the real process env.
pub fn process_env_warnings(providers: &[&str]) -> Vec<String> {
    let debug = std::env::var(DEBUG_ENV).ok();
    env_warnings(
        |var| std::env::var_os(var).is_some(),
        debug.as_deref(),
        providers,
    )
}

fn retired_line(var: &str, with: &str) -> String {
    format!("`{var}` is no longer read (ADR-0213) — use {with}")
}

/// Drop every retired key from each layer **before** the merge — the
/// top-level ones and an MCP server's `state:` — so `deny_unknown_fields`
/// doesn't turn a stale line into a startup failure, and return one warning
/// per occurrence naming the file that still carries it.
pub(super) fn strip_retired_keys(layers: &mut [RawLayer]) -> Vec<String> {
    let mut out = Vec::new();
    for layer in layers {
        let Value::Mapping(map) = &mut layer.doc else {
            continue;
        };
        for (key, adr, with) in RETIRED_CONFIG_KEYS {
            if map.remove(*key).is_some() {
                out.push(format!(
                    "{}: `{key}:` is no longer read ({adr}) and was ignored — {with}",
                    layer.source
                ));
            }
        }
        let Some(Value::Mapping(servers)) = map.get_mut("mcp") else {
            continue;
        };
        for (name, server) in servers.iter_mut() {
            let Some(server) = server.as_mapping_mut() else {
                continue;
            };
            if let Some(state) = server.remove("state") {
                // An old opt-out must stay an opt-out, never silently re-enable.
                if state.as_str() == Some("disabled") {
                    server.insert(Value::from("disabled"), Value::Bool(true));
                }
                let name = name.as_str().unwrap_or("?");
                out.push(format!(
                    "{}: `mcp.{name}.state:` is no longer read (ADR-0226) and was ignored — \
                     {MCP_STATE_REPLACEMENT}",
                    layer.source
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::ConfigLayer;
    use super::*;

    fn set_of(vars: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |v| vars.contains(&v)
    }

    #[test]
    fn nothing_set_means_no_warnings() {
        assert!(env_warnings(|_| false, None, &["zai"]).is_empty());
    }

    #[test]
    fn every_retired_var_warns_exactly_once_naming_its_replacement() {
        for (var, with) in RETIRED_ENV {
            let w = env_warnings(|v| v == *var, None, &["zai"]);
            assert_eq!(w.len(), 1, "{var}: {w:?}");
            assert!(w[0].contains(var) && w[0].contains(with), "{}", w[0]);
        }
    }

    #[test]
    fn the_folded_vars_name_their_debug_token() {
        let w = env_warnings(set_of(&["ENTANGLEMENT_LOG_BODIES"]), None, &[]);
        assert!(w[0].contains("ENTANGLEMENT_DEBUG=bodies"), "{}", w[0]);
        let w = env_warnings(set_of(&["ENTANGLEMENT_ECHO_FULL"]), None, &[]);
        assert!(w[0].contains("ENTANGLEMENT_DEBUG=echo"), "{}", w[0]);
        let w = env_warnings(
            set_of(&["ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE"]),
            None,
            &[],
        );
        assert!(
            w[0].contains("ENTANGLEMENT_DEBUG=no-shared-state"),
            "{}",
            w[0]
        );
    }

    #[test]
    fn per_provider_suffixes_are_checked_against_catalog_names_only() {
        let w = env_warnings(
            set_of(&[
                "OLLAMA_BASE",
                "ZAI_RPM",
                "ZAI_PAAS_CONCURRENCY",
                "OTHER_RPM",
            ]),
            None,
            &["zai", "zai_paas", "ollama", "zai"],
        );
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(w
            .iter()
            .any(|l| l.contains("`OLLAMA_BASE`") && l.contains("OLLAMA_API_BASE")));
        assert!(w
            .iter()
            .any(|l| l.contains("`ZAI_RPM`") && l.contains("rpm:")));
        assert!(w
            .iter()
            .any(|l| l.contains("`ZAI_PAAS_CONCURRENCY`") && l.contains("concurrency:")));
    }

    #[test]
    fn the_kept_api_base_spelling_never_warns() {
        let w = env_warnings(set_of(&["ZAI_API_BASE", "ZAI_MODEL"]), None, &["zai"]);
        assert!(w.is_empty(), "{w:?}");
    }

    #[test]
    fn unknown_debug_tokens_yield_one_warning() {
        assert!(env_warnings(|_| false, Some("bodies,echo"), &[]).is_empty());
        assert!(env_warnings(|_| false, Some(""), &[]).is_empty());
        let w = env_warnings(|_| false, Some("bodies,1,verbose"), &[]);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("1, verbose"), "{}", w[0]);
    }

    fn layer(source: &str, yaml: &str) -> RawLayer {
        RawLayer {
            layer: ConfigLayer::User,
            source: source.to_string(),
            doc: serde_yaml::from_str(yaml).unwrap(),
        }
    }

    #[test]
    fn tool_advertising_and_an_mcp_state_are_stripped_naming_adr_0226() {
        let mut layers = vec![layer(
            "user.yml",
            "tool_advertising: full\nmcp:\n  kb:\n    url: https://kb/mcp\n    state: allowed\n",
        )];
        let w = strip_retired_keys(&mut layers);
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].contains("`tool_advertising:`") && w[0].contains("ADR-0226"));
        assert!(w[1].contains("`mcp.kb.state:`") && w[1].contains("disabled: true"));
        let kb = &layers[0].doc["mcp"]["kb"];
        assert!(kb.get("state").is_none());
        assert!(kb.get("url").is_some(), "siblings survive");
    }

    #[test]
    fn a_retired_mcp_state_disabled_becomes_disabled_true() {
        let mut layers = vec![layer(
            "user.yml",
            "mcp:\n  off:\n    url: https://off/mcp\n    state: disabled\n  on:\n    url: https://on/mcp\n    state: enabled\n",
        )];
        let w = strip_retired_keys(&mut layers);
        assert_eq!(w.len(), 2, "{w:?}");
        let mcp = &layers[0].doc["mcp"];
        assert_eq!(mcp["off"]["disabled"], Value::Bool(true));
        assert!(mcp["on"].get("disabled").is_none());
        assert!(mcp["on"].get("state").is_none());
    }

    #[test]
    fn a_retired_config_key_is_stripped_with_one_warning_per_file() {
        let mut layers = vec![
            layer("user.yml", "discovery: {zai: invoke}\nagent: general\n"),
            layer("project.yml", "agent: build\n"),
        ];
        let w = strip_retired_keys(&mut layers);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].starts_with("user.yml:") && w[0].contains("`discovery:`"));
        assert!(layers[0].doc.get("discovery").is_none());
        assert!(layers[0].doc.get("agent").is_some(), "siblings survive");
    }
}
