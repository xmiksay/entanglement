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
//! this never fails the turn itself. A replaced resource is deleted in the
//! background: every tool-set change on the client-side encoding mints a new
//! one, and the old one would otherwise stay billed for storage until its TTL.

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
    /// use or on change (best-effort deleting the resource it replaces);
    /// returns `None` (inline as before) when the prefix is too small or the
    /// create call fails.
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
        let replaced = {
            let guard = self.0.lock().await;
            match guard.as_ref() {
                Some(state) if state.key == key => {
                    return match &state.entry {
                        CacheEntry::Ready(name) => Some(name.clone()),
                        CacheEntry::Skip => None,
                    };
                }
                Some(CacheState {
                    entry: CacheEntry::Ready(name),
                    ..
                }) => Some(name.clone()),
                _ => None,
            }
        };
        if let Some(name) = replaced {
            spawn_delete(http, base_url, auth, name);
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
    let url = format!("{}/cachedContents", api_root(base_url));
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

/// `base_url` is the `models` collection root (e.g. `.../v1beta/models`);
/// `cachedContents` is a sibling collection under the same `v1beta` root,
/// and a resource `name` (`cachedContents/…`) is relative to it too.
fn api_root(base_url: &str) -> &str {
    base_url.trim_end_matches('/').trim_end_matches("/models")
}

/// Fire-and-forget `DELETE {root}/{name}` for a replaced resource. Detached so
/// a slow or dead endpoint never delays the turn; any failure is only logged —
/// the resource still expires at its TTL.
fn spawn_delete(http: &HttpClient, base_url: &str, auth: &(&'static str, String), name: String) {
    let client = http.client().clone();
    let url = format!("{}/{name}", api_root(base_url));
    let (header, value) = (auth.0, auth.1.clone());
    tokio::spawn(async move {
        match client.delete(&url).header(header, value).send().await {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => {
                tracing::debug!(status = %r.status(), %name, "gemini cachedContents delete rejected")
            }
            Err(e) => tracing::debug!(error = %e, %name, "gemini cachedContents delete failed"),
        }
    });
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

    /// A loopback stand-in for the Gemini API: answers a `POST` create with
    /// `cachedContents/<n>` and anything else with `{}`, one request per
    /// connection, reporting `"<METHOD> <path> <x-goog-api-key>"` per request.
    fn mock_gemini() -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let mut created = 0;
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { return };
                let mut reader = BufReader::new(sock.try_clone().expect("clone socket"));
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let (mut len, mut key) = (0usize, String::new());
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                    let lower = h.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                    if lower.starts_with("x-goog-api-key:") {
                        key = h["x-goog-api-key:".len()..].trim().to_string();
                    }
                }
                let _ = reader.read_exact(&mut vec![0u8; len]);
                let mut parts = line.split_whitespace();
                let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let body = if method == "POST" {
                    created += 1;
                    format!(r#"{{"name":"cachedContents/{created}"}}"#)
                } else {
                    "{}".to_string()
                };
                let _ = tx.send(format!("{method} {path} {key}"));
                let _ = write!(
                    sock,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (format!("http://{addr}/v1beta/models"), rx)
    }

    #[tokio::test]
    async fn a_key_change_deletes_the_replaced_resource_and_a_reuse_does_not() {
        let (base, mut seen) = mock_gemini();
        let http = HttpClient::new().expect("client");
        let handle = CacheHandle::new();
        let auth = ("x-goog-api-key", "k1".to_string());
        let sys_a = "a".repeat(MIN_CACHEABLE_CHARS);
        let sys_b = "b".repeat(MIN_CACHEABLE_CHARS);

        for (system, want) in [
            (&sys_a, "cachedContents/1"),
            (&sys_a, "cachedContents/1"),
            (&sys_b, "cachedContents/2"),
        ] {
            let name = handle.resolve(&http, &base, &auth, "m", system, &[]).await;
            assert_eq!(name.as_deref(), Some(want));
        }

        let mut requests = Vec::new();
        for _ in 0..3 {
            let next = tokio::time::timeout(std::time::Duration::from_secs(5), seen.recv()).await;
            requests.push(next.expect("a request within 5s").expect("server alive"));
        }
        assert_eq!(
            requests,
            [
                "POST /v1beta/cachedContents k1",
                "POST /v1beta/cachedContents k1",
                "DELETE /v1beta/cachedContents/1 k1",
            ]
        );
        let extra = tokio::time::timeout(std::time::Duration::from_millis(300), seen.recv()).await;
        assert!(extra.is_err(), "the reuse must not delete: {extra:?}");
    }
}
