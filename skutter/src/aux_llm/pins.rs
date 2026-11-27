//! The per-purpose `(provider, model)` pins an [`AuxLlmRegistry`] consults
//! (Issue 5): plain in-memory data the embedder fills. `skutter` persists
//! them in its managed `aux-models.yml`; the library never reads a file.
//!
//! [`AuxLlmRegistry`]: super::AuxLlmRegistry

use std::collections::BTreeMap;

/// A recognized auxiliary LLM purpose (Issue 5). Each one has its own
/// `(provider, model)` pin and a runtime-side call site.
///
/// - [`Summarize`][Purpose::Summarize] — the compaction summary, reached by
///   core through [`AuxLlmRegistry::resolver`](super::AuxLlmRegistry::resolver).
/// - [`SessionTitle`][Purpose::SessionTitle] — the auto session
///   title/description generator (purely runtime-side, on the first prompt).
/// - [`Narrate`][Purpose::Narrate] — the live action narrator (#635): a short
///   "what the agent is doing now" phrase, set as `Session.action` on every
///   tool call (purely runtime-side, `narrate.rs`).
///
/// Keyed in text by [`as_str`](Self::as_str) / [`parse`](Self::parse)
/// (`summarize`, `session_title`, `narrate`) — `skutter`'s `aux-models.yml`
/// keys through them, so the type carries no serde form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Purpose {
    Summarize,
    SessionTitle,
    Narrate,
}

impl std::fmt::Display for Purpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Purpose {
    /// The stable string key, reachable without
    /// constructing the enum first (e.g. the `/aux-model` parser).
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Summarize => "summarize",
            Purpose::SessionTitle => "session_title",
            Purpose::Narrate => "narrate",
        }
    }

    /// Parse a purpose key, case-insensitively. `None` for an unrecognized
    /// string (the `/aux-model` command surfaces this as a parse error rather
    /// than silently storing an inert pin).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "summarize" => Some(Purpose::Summarize),
            "session_title" | "title" => Some(Purpose::SessionTitle),
            "narrate" => Some(Purpose::Narrate),
            _ => None,
        }
    }
}

/// The per-purpose pins, in memory. A multi-user embedder looks a user's
/// pins up in its own storage (ADR-0181) and wraps them with
/// [`in_memory`](Self::in_memory).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuxModelStore {
    aux: BTreeMap<Purpose, (String, String)>,
}

impl AuxModelStore {
    /// A store holding exactly `pins` (`purpose → (provider, model)`).
    pub fn in_memory(pins: BTreeMap<Purpose, (String, String)>) -> Self {
        Self { aux: pins }
    }

    /// The `(provider, model)` pin for `purpose`, if any.
    pub fn get(&self, purpose: Purpose) -> Option<(&str, &str)> {
        self.aux
            .get(&purpose)
            .map(|(provider, model)| (provider.as_str(), model.as_str()))
    }

    /// Pin `purpose` to `provider`/`model`.
    pub fn set(&mut self, purpose: Purpose, provider: &str, model: &str) {
        self.aux
            .insert(purpose, (provider.to_string(), model.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purpose_roundtrips_through_its_keys() {
        for p in [Purpose::Summarize, Purpose::SessionTitle, Purpose::Narrate] {
            assert_eq!(Purpose::parse(p.as_str()), Some(p));
        }
        // `title` is accepted as an alias of `session_title` (the friendly
        // spelling a user is likeliest to type at `/aux-model`).
        assert_eq!(Purpose::parse("session_title"), Some(Purpose::SessionTitle));
        assert_eq!(Purpose::parse("title"), Some(Purpose::SessionTitle));
        assert_eq!(Purpose::parse("summarize"), Some(Purpose::Summarize));
        assert_eq!(Purpose::parse("narrate"), Some(Purpose::Narrate));
        assert_eq!(Purpose::parse("bogus_purpose"), None);
        assert_eq!(Purpose::parse("Summarize"), Some(Purpose::Summarize));
        assert_eq!(Purpose::parse("  summarize "), Some(Purpose::Summarize));
    }

    #[test]
    fn set_replaces_one_purpose_and_keeps_the_rest() {
        let mut store = AuxModelStore::default();
        assert_eq!(store.get(Purpose::Summarize), None);
        store.set(Purpose::Summarize, "zai", "glm-4.5-flash");
        store.set(Purpose::Narrate, "ollama", "llama3.1");
        store.set(Purpose::Summarize, "openai", "gpt-4o-mini");
        assert_eq!(
            store.get(Purpose::Summarize),
            Some(("openai", "gpt-4o-mini"))
        );
        assert_eq!(store.get(Purpose::Narrate), Some(("ollama", "llama3.1")));
        assert_eq!(store.get(Purpose::SessionTitle), None);
    }
}
