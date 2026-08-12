//! `cachedContents` resource lifecycle for Gemini context caching (#587):
//! create-or-reuse a cache for the stable system+tools prefix, mirroring the
//! Anthropic `cache_control` breakpoint strategy (#566) — Gemini has no
//! automatic equivalent, so without this the system prompt and every tool
//! schema re-bill at the full input rate on every turn. One resource lives
//! per session (the state sits on the `GeminiLlm` clone that session owns,
//! wrapped in `Arc` so a further clone still shares it rather than creating a
//! duplicate) and is recreated whenever the system prompt or tool set
//! actually changes. Best-effort throughout: a too-small prefix or any
//! creation failure just falls back to inlining `system`/`tools` as before —
//! this never fails the turn itself.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::client::HttpClient;
use crate::ToolSpec;

use super::request::{build_cache_body, cache_prefix_size, MIN_CACHEABLE_CHARS};

/// TTL attached to a created `cachedContents` resource — mirrors Google's own
/// default and comfortably outlives the gap between an agent's turns without
/// re-creating the cache constantly.
const CACHE_TTL: &str = "3600s";

#[derive(Clone)]
enum CacheEntry {
    Ready(String),
    /// The prefix at this key isn't worth (or isn't able to be) cached —
    /// remembered so a stable-but-too-small prefix doesn't retry the create
    /// call on every single turn.
    Skip,
}

struct CacheState {
    key: u64,
    entry: CacheEntry,
}

/// Per-session handle to the resolved `cachedContents` resource. Cheap to
/// clone — the state lives behind the `Arc`.
#[derive(Clone, Default)]
pub(super) struct CacheHandle(Arc<Mutex<Option<CacheState>>>);

impl CacheHandle {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Resolve the `cachedContent` resource name to send with this request,
    /// if any. Reuses the existing resource when `model`/`system`/`tools`
    /// hash the same as what it was created from; creates a new one on first
    /// use or on change; returns `None` (inline as before) when the prefix is
    /// too small or the create call fails.
    pub(super) async fn resolve(
        &self,
        http: &HttpClient,
        base_url: &str,
        // The resolved request auth header — `x-goog-api-key` or an OAuth
        // `authorization: Bearer` (#684), built by `super::auth_header`.
        auth: &(&'static str, String),
        model: &str,
        system: &str,
        tools: &[ToolSpec],
    ) -> Option<String> {
        if system.is_empty() && tools.is_empty() {
            return None;
        }
        let key = cache_key(model, system, tools);
        {
            let guard = self.0.lock().await;
            if let Some(state) = guard.as_ref() {
                if state.key == key {
                    return match &state.entry {
                        CacheEntry::Ready(name) => Some(name.clone()),
                        CacheEntry::Skip => None,
                    };
                }
            }
        }
        if cache_prefix_size(system, tools) < MIN_CACHEABLE_CHARS {
            *self.0.lock().await = Some(CacheState {
                key,
                entry: CacheEntry::Skip,
            });
            return None;
        }
        let entry = match create(http, base_url, auth, model, system, tools).await {
            Some(name) => CacheEntry::Ready(name),
            None => CacheEntry::Skip,
        };
        let name = match &entry {
            CacheEntry::Ready(name) => Some(name.clone()),
            CacheEntry::Skip => None,
        };
        *self.0.lock().await = Some(CacheState { key, entry });
        name
    }
}

/// Hash `model`+`system`+`tools` so a repeat request with the identical
/// prefix reuses the cache and any change gets a fresh one. Collisions would
/// at worst reuse a stale cache for one turn — an acceptable risk for a
/// cost-optimization path that never affects correctness of the reply itself
/// (the resolved `contents` history is always sent in full, uncached).
fn cache_key(model: &str, system: &str, tools: &[ToolSpec]) -> u64 {
    let mut hasher = DefaultHasher::new();
    model.hash(&mut hasher);
    system.hash(&mut hasher);
    for t in tools {
        t.name.hash(&mut hasher);
        t.description.hash(&mut hasher);
        t.schema.to_string().hash(&mut hasher);
    }
    hasher.finish()
}

/// POST the `cachedContents` create body. Returns `None` (never an `Err`) on
/// any transport failure, non-2xx status, or unparsable body — cache creation
/// is strictly best-effort and must not turn into a failed turn.
async fn create(
    http: &HttpClient,
    base_url: &str,
    auth: &(&'static str, String),
    model: &str,
    system: &str,
    tools: &[ToolSpec],
) -> Option<String> {
    // `base_url` is the `models` collection root (e.g.
    // `.../v1beta/models`); `cachedContents` is a sibling collection under
    // the same `v1beta` root.
    let base = base_url.trim_end_matches('/').trim_end_matches("/models");
    let url = format!("{base}/cachedContents");
    let body = build_cache_body(model, system, tools, CACHE_TTL);

    let response = match http
        .client()
        .post(&url)
        .header(auth.0, &auth.1)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "gemini cachedContents create request failed");
            return None;
        }
    };
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        tracing::debug!(status = %status, response = %text, "gemini cachedContents create rejected");
        return None;
    }
    match response.json::<Value>().await {
        Ok(v) => v.get("name").and_then(|n| n.as_str()).map(str::to_string),
        Err(e) => {
            tracing::debug!(error = %e, "gemini cachedContents response unparsable");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_changes_with_system_and_tools() {
        let a = cache_key("gemini-2.5-flash", "sys a", &[]);
        let b = cache_key("gemini-2.5-flash", "sys b", &[]);
        assert_ne!(a, b);

        let with_tool = cache_key("gemini-2.5-flash", "sys a", &[ToolSpec::new("t", "desc")]);
        assert_ne!(a, with_tool);
    }

    #[test]
    fn cache_key_stable_for_identical_input() {
        let spec = ToolSpec::new("t", "desc");
        let a = cache_key("gemini-2.5-flash", "sys", std::slice::from_ref(&spec));
        let b = cache_key("gemini-2.5-flash", "sys", std::slice::from_ref(&spec));
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn resolve_returns_none_for_empty_prefix() {
        let http = HttpClient::new().expect("client");
        let handle = CacheHandle::new();
        let name = handle
            .resolve(
                &http,
                super::super::GEMINI_BASE,
                &("x-goog-api-key", "key".to_string()),
                "model",
                "",
                &[],
            )
            .await;
        assert!(name.is_none());
    }

    #[tokio::test]
    async fn resolve_skips_and_remembers_a_too_small_prefix() {
        let http = HttpClient::new().expect("client");
        let handle = CacheHandle::new();
        // Well under MIN_CACHEABLE_CHARS — never issues a network call.
        let first = handle
            .resolve(
                &http,
                super::super::GEMINI_BASE,
                &("x-goog-api-key", "key".to_string()),
                "model",
                "short system prompt",
                &[],
            )
            .await;
        assert!(first.is_none());
        // Second call with the same too-small prefix hits the remembered
        // `Skip` entry rather than re-attempting.
        let second = handle
            .resolve(
                &http,
                super::super::GEMINI_BASE,
                &("x-goog-api-key", "key".to_string()),
                "model",
                "short system prompt",
                &[],
            )
            .await;
        assert!(second.is_none());
    }
}
