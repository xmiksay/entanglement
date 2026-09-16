//! The runtime half of ADR-0204's `invoke` envelope. Core unwraps a
//! well-formed `invoke` call before emitting `ToolExec`, so every gate and
//! dispatch path here sees the inner tool. What is left for the runtime: a
//! `ToolExec` still named `invoke` (a malformed envelope, a reserved inner
//! name, or `invoke` not advertised at all), and the example shape an
//! arg-validate decline should show.

use entanglement_core::{Discovery, SessionId};

use super::AdvertisingState;
use crate::tool_names::{INVOKE_TOOL, RESPONSES_TOOL_SEARCH_TOOL, TOOL_SEARCH_KERNEL};
use crate::tools::ToolRegistry;

/// The decline for a tool name that isn't registered: the `invoke` schema
/// explanation when this session advertises the envelope (the model tried
/// to use it and got the shape wrong), otherwise the ordinary unknown-tool
/// hint — a stray `invoke` elsewhere is just an unknown name.
pub fn unknown_tool_reply(
    state: &AdvertisingState,
    session: &SessionId,
    tools: &ToolRegistry,
    tool: &str,
) -> String {
    if tool == INVOKE_TOOL && state.discovery(session).advertises_invoke() {
        malformed_envelope_message()
    } else {
        tools.unknown_tool_message(tool)
    }
}

fn malformed_envelope_message() -> String {
    let schema =
        serde_json::to_string_pretty(&crate::discover::invoke_spec().schema).unwrap_or_default();
    format!(
        "malformed invoke call — expected {{\"name\": \"<tool>\", \"args\": {{...}}}}: \
         `name` is the loaded tool's name (a string), `args` its arguments as a JSON \
         object; `{INVOKE_TOOL}` and `{RESPONSES_TOOL_SEARCH_TOOL}` cannot be invoked.\n\n\
         invoke schema:\n{schema}"
    )
}

/// Whether an arg-validate decline for `tool` should show its example call
/// wrapped in the envelope: only under `invoke`, and only for a tool the
/// session reaches through it — kernel tools stay directly advertised.
pub fn example_via_invoke(state: &AdvertisingState, session: &SessionId, tool: &str) -> bool {
    state.discovery(session) == Discovery::Invoke && !TOOL_SEARCH_KERNEL.contains(&tool)
}

#[cfg(test)]
mod tests {
    use entanglement_core::ToolAdvertising;

    use super::*;
    use crate::tool_advertising::Encoding;

    fn state(mode: ToolAdvertising, encoding: Encoding, d: Discovery) -> AdvertisingState {
        let state = AdvertisingState::new();
        let s = SessionId::new("s");
        let mut modes = state.modes.lock().unwrap();
        modes.pin(s.clone(), mode, encoding);
        modes.set_discovery(&s, d);
        drop(modes);
        state
    }

    fn reply(state: &AdvertisingState, tool: &str) -> String {
        unknown_tool_reply(state, &SessionId::new("s"), &ToolRegistry::new(), tool)
    }

    #[test]
    fn a_stray_invoke_under_an_advertising_strategy_explains_the_envelope() {
        for d in [Discovery::NativeFirst, Discovery::Invoke] {
            let out = reply(
                &state(ToolAdvertising::ToolSearch, Encoding::ClientSide, d),
                "invoke",
            );
            assert!(out.starts_with("malformed invoke call"), "{d:?}: {out}");
            assert!(out.contains(r#""args": {...}"#), "{out}");
            assert!(out.contains("responses_tool_search"), "{out}");
            assert!(out.contains(r#""required""#), "carries the schema: {out}");
        }
    }

    #[test]
    fn a_stray_invoke_elsewhere_is_an_ordinary_unknown_tool() {
        for st in [
            state(
                ToolAdvertising::ToolSearch,
                Encoding::ClientSide,
                Discovery::Append,
            ),
            state(
                ToolAdvertising::Full,
                Encoding::ClientSide,
                Discovery::Invoke,
            ),
            state(
                ToolAdvertising::ToolSearch,
                Encoding::AnthropicNative,
                Discovery::Invoke,
            ),
            AdvertisingState::new(),
        ] {
            assert!(reply(&st, "invoke").starts_with("unknown tool: `invoke`"));
        }
        let st = state(
            ToolAdvertising::ToolSearch,
            Encoding::ClientSide,
            Discovery::Invoke,
        );
        assert!(reply(&st, "nope").starts_with("unknown tool: `nope`"));
    }

    #[test]
    fn only_non_kernel_tools_under_invoke_get_a_wrapped_example() {
        let s = SessionId::new("s");
        let inv = state(
            ToolAdvertising::ToolSearch,
            Encoding::ClientSide,
            Discovery::Invoke,
        );
        assert!(example_via_invoke(&inv, &s, "grep"));
        assert!(!example_via_invoke(&inv, &s, "read"));
        let nf = state(
            ToolAdvertising::ToolSearch,
            Encoding::ClientSide,
            Discovery::NativeFirst,
        );
        assert!(!example_via_invoke(&nf, &s, "grep"));
    }
}
