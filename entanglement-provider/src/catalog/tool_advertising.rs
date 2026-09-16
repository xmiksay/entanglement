//! The per-model tool-advertising knob (ADR-0196, Phase P1): whether a
//! session running this model gets the full advertised tool surface
//! (`full`) or the lean kernel + discovery pair (`tool_search`).
//!
//! This is a per-model *catalog preference*, the same shape as
//! [`ThinkingStyle`][super::ThinkingStyle]/[`ThinkingFormat`][super::ThinkingFormat]:
//! a fact about the model that a user overrides through `config.yml`/env
//! without a code change (#118's "catalog data, not hardcode" property). The
//! *resolution* (env > config > catalog > `tool_search`) lives in the
//! runtime (`entanglement_runtime::tool_advertising`), not here — the
//! provider crate only carries the data.
//!
//! Phase P1 note: nothing consumes the mode yet beyond logging and
//! `skutter inspect config`; the lean-kernel/discovery-pair advertisement
//! change lands in Phase P3. Every embedded catalog entry leaves it unset,
//! so an unset install now resolves `tool_search` by default (P1's one
//! behavior change) — no dispatch-level effect yet since nothing branches on
//! it.

use serde::Deserialize;

/// How a session's tools are advertised to the model (ADR-0196).
///
/// `full` keeps today's behavior: every registered tool's schema reaches the
/// model inline. `tool_search` (the default) swaps it for a lean kernel plus
/// the `explore`/`describe` discovery pair — the rest of the surface stays
/// registered but reachable only after discovery (Phase P3). Selected per
/// model in the catalog, overridable per install via `tool_advertising` in
/// `config.yml` / the `ENTANGLEMENT_TOOL_ADVERTISING` env var, and resolved
/// **once per session** from its initial model — never mid-session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolAdvertising {
    /// Full advertised surface; every registered tool's schema reaches the
    /// model inline. Today's behavior.
    Full,
    /// Lean kernel + `explore`/`describe` discovery meta-tools; everything
    /// else stays registered but is reachable only through discovery
    /// (Phase P3). The default.
    #[default]
    ToolSearch,
}

impl ToolAdvertising {
    /// The lowercase wire/YAML spelling, shared by the catalog, the config
    /// key, and the env var so one string maps one way everywhere.
    pub fn label(self) -> &'static str {
        match self {
            ToolAdvertising::Full => "full",
            ToolAdvertising::ToolSearch => "tool_search",
        }
    }

    /// Parse the mode from its YAML/env spelling, case-insensitively. The one
    /// decoder both the env override and any future string-bearing surface
    /// share, so `"TOOL_SEARCH"` and `"tool_search"` can't drift apart.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "full" => Some(ToolAdvertising::Full),
            "tool_search" => Some(ToolAdvertising::ToolSearch),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_tool_search_and_absent_yaml_is_none() {
        // The catalog flag is Option-shaped like thinking_style: absent YAML
        // means "no catalog preference", not "tool_search" — resolution must
        // fall through to config/env before the default.
        let entry: super::super::ModelEntry =
            serde_yaml::from_str("id: m\n").expect("bare entry parses");
        assert_eq!(entry.tool_advertising, None);
        assert_eq!(ToolAdvertising::default(), ToolAdvertising::ToolSearch);
    }

    #[test]
    fn parses_tool_search_and_full() {
        let entry: super::super::ModelEntry =
            serde_yaml::from_str("id: m\ntool_advertising: tool_search\n")
                .expect("tool_search parses");
        assert_eq!(entry.tool_advertising, Some(ToolAdvertising::ToolSearch));
        let full: super::super::ModelEntry =
            serde_yaml::from_str("id: m\ntool_advertising: full\n").expect("full parses");
        assert_eq!(full.tool_advertising, Some(ToolAdvertising::Full));
    }

    #[test]
    fn unknown_value_is_rejected_loudly() {
        let err =
            serde_yaml::from_str::<super::super::ModelEntry>("id: m\ntool_advertising: xml\n")
                .expect_err("a typo'd mode must be a validation error, not silent");
        assert!(err.to_string().contains("tool_advertising"), "got: {err}");
    }

    #[test]
    fn parse_is_case_insensitive_and_round_trips_the_label() {
        assert_eq!(ToolAdvertising::parse("full"), Some(ToolAdvertising::Full));
        assert_eq!(
            ToolAdvertising::parse("TOOL_SEARCH"),
            Some(ToolAdvertising::ToolSearch)
        );
        assert_eq!(
            ToolAdvertising::parse(" Tool_Search "),
            Some(ToolAdvertising::ToolSearch)
        );
        assert_eq!(ToolAdvertising::parse("hybrid"), None);
        for mode in [ToolAdvertising::Full, ToolAdvertising::ToolSearch] {
            assert_eq!(ToolAdvertising::parse(mode.label()), Some(mode));
        }
    }

    #[test]
    fn embedded_defaults_carry_no_tool_advertising_preference() {
        // P1 ships with every model unset: no entry opts into a specific
        // mode. Guards against an accidental catalog-level override riding
        // along in defaults.yml.
        for p in &super::super::Catalog::builtin().providers {
            for m in &p.models {
                assert_eq!(
                    m.tool_advertising, None,
                    "{}.{} must not set tool_advertising in the embedded defaults",
                    p.name, m.id
                );
            }
        }
    }
}
