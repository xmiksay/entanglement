//! The client-side discovery strategy knob (ADR-0204): how a
//! `tool_search`-mode session on a `client_side` wire reaches a tool it found
//! through `explore`/`describe`. Catalog data like
//! [`ToolAdvertising`][super::ToolAdvertising] — set per provider, overridable
//! per model — while the *resolution* and per-session pin live in the runtime
//! (`entanglement_runtime::tool_advertising`). The native Anthropic/Responses
//! encodings have their own deferral primitive and ignore it.

use serde::Deserialize;

/// How a discovered tool becomes callable on a `client_side` wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Discovery {
    /// ADR-0196: `describe()` appends the tool's spec to the advertised
    /// array. One cache miss per discovery on wires that key the cache on
    /// `tools`; the default when nothing decides.
    #[default]
    Append,
    /// The array never changes; the model is told to call the tool by its
    /// real name and to fall back to the `invoke` envelope only if it can't
    /// (z.ai: GLM-5.x refuses undeclared names, the flash models don't).
    NativeFirst,
    /// The array never changes; discovered tools are called through
    /// `invoke` — for wires that reject an undeclared function call (Gemini)
    /// or don't document accepting one (OpenAI Chat Completions).
    Invoke,
}

impl Discovery {
    /// Whether sessions under this strategy advertise the `invoke` envelope.
    pub fn advertises_invoke(self) -> bool {
        !matches!(self, Discovery::Append)
    }

    /// The snake_case wire/YAML spelling, shared by the catalog, the
    /// `config.yml` `discovery:` map and every label a head renders — one
    /// string maps one way everywhere, exactly like
    /// [`ToolAdvertising::label`][super::ToolAdvertising::label].
    pub fn label(self) -> &'static str {
        match self {
            Discovery::Append => "append",
            Discovery::NativeFirst => "native_first",
            Discovery::Invoke => "invoke",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_three_snake_case_values() {
        for (raw, want) in [
            ("append", Discovery::Append),
            ("native_first", Discovery::NativeFirst),
            ("invoke", Discovery::Invoke),
        ] {
            let got: Discovery = serde_yaml::from_str(raw).expect("valid value parses");
            assert_eq!(got, want);
        }
        assert!(serde_yaml::from_str::<Discovery>("nativefirst").is_err());
        assert_eq!(Discovery::default(), Discovery::Append);
    }

    #[test]
    fn every_label_round_trips_through_the_parser() {
        for d in [Discovery::Append, Discovery::NativeFirst, Discovery::Invoke] {
            let back: Discovery =
                serde_yaml::from_str(d.label()).expect("a label is a parseable value");
            assert_eq!(back, d);
        }
    }

    #[test]
    fn only_append_leaves_invoke_unadvertised() {
        assert!(!Discovery::Append.advertises_invoke());
        assert!(Discovery::NativeFirst.advertises_invoke());
        assert!(Discovery::Invoke.advertises_invoke());
    }
}
