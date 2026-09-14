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

#[test]
fn defaults_to_tool_search_with_nothing_set() {
    let config = config_with(None);
    assert_eq!(
        resolve_advertising(&config, None, "any", "thing"),
        ToolAdvertising::ToolSearch
    );
}

#[test]
fn catalog_preference_wins_over_default() {
    let config = config_with(None);
    let catalog = full_catalog();
    assert_eq!(
        resolve_advertising(&config, Some(&catalog), "p", "fully_advertised"),
        ToolAdvertising::Full
    );
    // A different model under the same config falls to the new default
    // (tool_search), and an unknown provider/model is not an error.
    assert_eq!(
        resolve_advertising(&config, Some(&catalog), "p", "plain"),
        ToolAdvertising::ToolSearch
    );
    assert_eq!(
        resolve_advertising(&config, Some(&catalog), "nope", "fully_advertised"),
        ToolAdvertising::ToolSearch
    );
}

#[test]
fn config_beats_catalog_and_env_beats_config() {
    let catalog = full_catalog();

    // config (tool_search) > catalog (full).
    let config = config_with(Some(ToolAdvertising::ToolSearch));
    assert_eq!(
        resolve_advertising(&config, Some(&catalog), "p", "fully_advertised"),
        ToolAdvertising::ToolSearch
    );
    // config (full) > catalog (absent → tool_search).
    let config = config_with(Some(ToolAdvertising::Full));
    assert_eq!(
        resolve_advertising(&config, Some(&catalog), "p", "plain"),
        ToolAdvertising::Full
    );

    // env beats both. `ENTANGLEMENT_TOOL_ADVERTISING` is process-global,
    // so serialize against every other env-touching test in this crate.
    let _g = crate::config::ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let config = config_with(Some(ToolAdvertising::ToolSearch));
    std::env::set_var(TOOL_ADVERTISING_ENV, "full");
    assert_eq!(
        resolve_advertising(&config, Some(&catalog), "p", "fully_advertised"),
        ToolAdvertising::Full,
        "env must beat both config and catalog"
    );
    // Case-insensitive.
    std::env::set_var(TOOL_ADVERTISING_ENV, "TOOL_SEARCH");
    assert_eq!(
        resolve_advertising(&config_with(Some(ToolAdvertising::Full)), None, "p", "x"),
        ToolAdvertising::ToolSearch
    );
    // A bad value warns and falls through to config (tool_search here).
    std::env::set_var(TOOL_ADVERTISING_ENV, "hybrid");
    assert_eq!(
        resolve_advertising(&config, Some(&catalog), "p", "fully_advertised"),
        ToolAdvertising::ToolSearch,
        "unparseable env falls back to the config tier, not the default"
    );
    std::env::remove_var(TOOL_ADVERTISING_ENV);
}

#[test]
fn by_id_resolution_matches_the_paired_form() {
    let config = config_with(None);
    let catalog = full_catalog();
    assert_eq!(
        resolve_advertising_by_id(&config, Some(&catalog), "fully_advertised"),
        ToolAdvertising::Full
    );
    assert_eq!(
        resolve_advertising_by_id(&config, Some(&catalog), "plain"),
        ToolAdvertising::ToolSearch
    );
}

#[test]
fn source_reporting_tracks_the_winning_tier() {
    let _g = crate::config::ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    assert_eq!(
        configured_advertising_source(&config_with(None)),
        AdvertisingSource::Default
    );
    assert_eq!(
        configured_advertising_source(&config_with(Some(ToolAdvertising::Full))),
        AdvertisingSource::Config
    );
    std::env::set_var(TOOL_ADVERTISING_ENV, "tool_search");
    assert_eq!(
        configured_advertising_source(&config_with(Some(ToolAdvertising::Full))),
        AdvertisingSource::Env,
        "env beats config in the provenance view too"
    );
    // An unparseable env value is reported as *not* the winner — the
    // resolution above fell through to config/default.
    std::env::set_var(TOOL_ADVERTISING_ENV, "banana");
    assert_eq!(
        configured_advertising_source(&config_with(None)),
        AdvertisingSource::Default
    );
    std::env::remove_var(TOOL_ADVERTISING_ENV);
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
