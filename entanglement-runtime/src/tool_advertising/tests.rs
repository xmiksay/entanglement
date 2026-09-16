use super::*;

fn config_with(mode: Option<ToolAdvertising>) -> Config {
    Config {
        tool_advertising: mode,
        ..crate::config::bare_config()
    }
}

/// A minimal catalog with one `tool_advertising: full` model — enough
/// for the precedence chain without touching the embedded defaults. The
/// catalog preference is deliberately the *opposite* of the new default
/// (`tool_search`) so a test asserting "catalog wins over default"
/// actually exercises an override.
fn full_catalog() -> Catalog {
    serde_yaml::from_str(
        "providers:\n  - name: p\n    default_model: fully_advertised\n    models:\n      \
         - id: fully_advertised\n        tool_advertising: full\n      - id: plain\n",
    )
    .expect("test catalog parses")
}

/// The env+config tier as the resolver sees it, with the env value passed in
/// rather than set on the process — see `precedence.rs`'s module doc.
fn configured(env: Option<&str>, mode: Option<ToolAdvertising>) -> Option<ToolAdvertising> {
    precedence::configured_advertising_from(env, &config_with(mode))
}

#[test]
fn defaults_to_tool_search_with_nothing_set() {
    assert_eq!(
        resolve_advertising(configured(None, None), None, "any", "thing"),
        ToolAdvertising::ToolSearch
    );
}

#[test]
fn catalog_preference_wins_over_default() {
    let catalog = full_catalog();
    let none = configured(None, None);
    assert_eq!(
        resolve_advertising(none, Some(&catalog), "p", "fully_advertised"),
        ToolAdvertising::Full
    );
    // A different model under the same config falls to the new default
    // (tool_search), and an unknown provider/model is not an error.
    assert_eq!(
        resolve_advertising(none, Some(&catalog), "p", "plain"),
        ToolAdvertising::ToolSearch
    );
    assert_eq!(
        resolve_advertising(none, Some(&catalog), "nope", "fully_advertised"),
        ToolAdvertising::ToolSearch
    );
}

#[test]
fn config_beats_catalog_and_env_beats_config() {
    let catalog = full_catalog();

    // config (tool_search) > catalog (full).
    assert_eq!(
        resolve_advertising(
            configured(None, Some(ToolAdvertising::ToolSearch)),
            Some(&catalog),
            "p",
            "fully_advertised"
        ),
        ToolAdvertising::ToolSearch
    );
    // config (full) > catalog (absent → tool_search).
    assert_eq!(
        resolve_advertising(
            configured(None, Some(ToolAdvertising::Full)),
            Some(&catalog),
            "p",
            "plain"
        ),
        ToolAdvertising::Full
    );
    // env beats both.
    assert_eq!(
        resolve_advertising(
            configured(Some("full"), Some(ToolAdvertising::ToolSearch)),
            Some(&catalog),
            "p",
            "fully_advertised"
        ),
        ToolAdvertising::Full,
        "env must beat both config and catalog"
    );
    // Case-insensitive.
    assert_eq!(
        resolve_advertising(
            configured(Some("TOOL_SEARCH"), Some(ToolAdvertising::Full)),
            None,
            "p",
            "x"
        ),
        ToolAdvertising::ToolSearch
    );
    // A bad value warns and falls through to config (tool_search here).
    assert_eq!(
        resolve_advertising(
            configured(Some("hybrid"), Some(ToolAdvertising::ToolSearch)),
            Some(&catalog),
            "p",
            "fully_advertised"
        ),
        ToolAdvertising::ToolSearch,
        "unparseable env falls back to the config tier, not the default"
    );
    // An empty env value is unset, not unparseable.
    assert_eq!(configured(Some(""), None), None);
}

#[test]
fn by_id_resolution_matches_the_paired_form() {
    let catalog = full_catalog();
    assert_eq!(
        resolve_advertising_by_id(None, Some(&catalog), "fully_advertised"),
        ToolAdvertising::Full
    );
    assert_eq!(
        resolve_advertising_by_id(None, Some(&catalog), "plain"),
        ToolAdvertising::ToolSearch
    );
}

#[test]
fn source_reporting_tracks_the_winning_tier() {
    let source = precedence::configured_advertising_source_from;
    assert_eq!(source(None, &config_with(None)), AdvertisingSource::Default);
    assert_eq!(
        source(None, &config_with(Some(ToolAdvertising::Full))),
        AdvertisingSource::Config
    );
    assert_eq!(
        source(
            Some("tool_search"),
            &config_with(Some(ToolAdvertising::Full))
        ),
        AdvertisingSource::Env,
        "env beats config in the provenance view too"
    );
    // An unparseable env value is reported as *not* the winner — the
    // resolution above fell through to config/default.
    assert_eq!(
        source(Some("banana"), &config_with(None)),
        AdvertisingSource::Default
    );
}

#[test]
fn map_pins_reads_and_forgets() {
    let mut modes = SessionToolAdvertising::new();
    let s = SessionId::new("s");
    assert_eq!(
        modes.get(&s),
        None,
        "unpinned reads as absent, not a resolved default"
    );
    assert_eq!(modes.get_encoding(&s), None);
    modes.pin(s.clone(), ToolAdvertising::Full, Encoding::AnthropicNative);
    assert_eq!(modes.get(&s), Some(ToolAdvertising::Full));
    assert_eq!(modes.get_encoding(&s), Some(Encoding::AnthropicNative));
    modes.forget(&s);
    assert_eq!(modes.get(&s), None);
    assert_eq!(modes.get_encoding(&s), None);
}

/// A provider on each wire this crate cares about (ADR-0196 §3): `anthro`
/// speaks the Anthropic wire, `p` falls to `Wire`'s default (`openai`) by
/// omitting the field entirely — exercising the same "unset means openai"
/// path every z.ai/OpenAI/Ollama catalog entry takes.
fn wired_catalog() -> Catalog {
    serde_yaml::from_str(
        "providers:\n  - name: p\n    default_model: plain\n    models:\n      - id: plain\n  \
         - name: anthro\n    wire: anthropic\n    default_model: claude_model\n    models:\n      \
         - id: claude_model\n  \
         - name: resp\n    wire: openai_responses\n    default_model: resp_model\n    models:\n      \
         - id: resp_model\n",
    )
    .expect("test catalog parses")
}

#[test]
fn encoding_resolves_from_the_provider_wire() {
    let catalog = wired_catalog();
    assert_eq!(
        resolve_encoding(Some(&catalog), "anthro"),
        Encoding::AnthropicNative
    );
    assert_eq!(resolve_encoding(Some(&catalog), "p"), Encoding::ClientSide);
    // An unknown provider, or no catalog at all, is not an error — it just
    // contributes no preference, falling to the safe default.
    assert_eq!(
        resolve_encoding(Some(&catalog), "nope"),
        Encoding::ClientSide
    );
    assert_eq!(resolve_encoding(None, "anthro"), Encoding::ClientSide);
    // P7: the Responses wire resolves to its own encoding, distinct from
    // both `client_side` and `anthropic_native`.
    assert_eq!(
        resolve_encoding(Some(&catalog), "resp"),
        Encoding::ResponsesNative
    );
}

#[test]
fn encoding_by_id_finds_the_owning_providers_wire() {
    let catalog = wired_catalog();
    assert_eq!(
        resolve_encoding_by_id(Some(&catalog), "claude_model"),
        Encoding::AnthropicNative
    );
    assert_eq!(
        resolve_encoding_by_id(Some(&catalog), "plain"),
        Encoding::ClientSide
    );
    assert_eq!(
        resolve_encoding_by_id(Some(&catalog), "unknown_id"),
        Encoding::ClientSide
    );
    assert_eq!(
        resolve_encoding_by_id(Some(&catalog), "resp_model"),
        Encoding::ResponsesNative
    );
}
