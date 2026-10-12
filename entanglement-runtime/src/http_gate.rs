//! The credentialed-`http()` gate (ADR-0227): which calls carry hidden
//! `http_auth` credentials, and which `(session, host)` pairs the user has
//! already approved. Shared by the `http` tool (which injects) and the
//! dispatcher (which forces the once-per-host approval), so both read one
//! definition of "credentialed".
//!
//! A credentialed call forces one approval per host per session whatever
//! the mode grades — the escape-root pattern: a class-wide `exec` allow
//! must not let a repo's script spend the user's token unseen.

use std::collections::HashSet;
use std::sync::Mutex;

use entanglement_core::SessionId;

use crate::config::http_auth::{HttpAuth, HttpAuthEntry, ProjectHttpRules};
use crate::tool_names::HTTP_TOOL;

#[derive(Default)]
pub struct HttpGate {
    auth: HttpAuth,
    approved: Mutex<HashSet<(SessionId, String)>>,
    /// Per mode, project-layer-only `http(...)` rules: never the explicit
    /// unlock (ADR-0227).
    untrusted_rules: ProjectHttpRules,
}

impl HttpGate {
    /// Building a gate hides every `${VAR}` its credentials reference from
    /// all child processes (`crate::secret_env`) — whoever can resolve the
    /// secret is the only place it may live.
    pub fn new(auth: HttpAuth) -> Self {
        crate::secret_env::hide(auth.referenced_vars());
        Self {
            auth,
            approved: Mutex::default(),
            untrusted_rules: ProjectHttpRules::new(),
        }
    }

    /// Rules that grade but never count as an explicit `http(...)` allow —
    /// per mode, the ones only `config.yml`'s project layer declares.
    pub fn with_untrusted_rules(mut self, rules: ProjectHttpRules) -> Self {
        self.untrusted_rules = rules;
        self
    }

    pub fn is_untrusted_rule(&self, mode: &str, key: &str) -> bool {
        self.untrusted_rules
            .get(mode)
            .is_some_and(|rules| rules.iter().any(|(k, _)| k == key))
    }

    /// The entry credentials for `url` come from: https only, host matched
    /// exactly or at a dot boundary. `None` sends the request bare.
    pub fn entry_for_url(&self, url: &str) -> Option<&HttpAuthEntry> {
        let (scheme, host) = entanglement_core::web_fetch::scheme_and_host(url)?;
        (scheme == "https")
            .then(|| self.auth.entry_for(&host).map(|(_, entry)| entry))
            .flatten()
    }

    /// The host an `http` call would authenticate for, or `None` when it
    /// carries no credentials (another tool, plain http, no matching entry).
    pub fn credentialed_host(&self, tool: &str, input: &str) -> Option<String> {
        if tool != HTTP_TOOL || self.auth.is_empty() {
            return None;
        }
        let value: serde_json::Value = serde_json::from_str(input).ok()?;
        let url = value.get("url")?.as_str()?;
        self.entry_for_url(url)?;
        entanglement_core::web_fetch::scheme_and_host(url).map(|(_, host)| host)
    }

    pub fn is_approved(&self, session: &SessionId, host: &str) -> bool {
        self.approved
            .lock()
            .is_ok_and(|set| set.contains(&(session.clone(), host.to_string())))
    }

    pub fn approve(&self, session: &SessionId, host: &str) {
        if let Ok(mut set) = self.approved.lock() {
            set.insert((session.clone(), host.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> HttpGate {
        HttpGate::new(serde_yaml::from_str("github.com: {headers: {X-Key: k}}").unwrap())
    }

    fn input(url: &str) -> String {
        serde_json::json!({ "method": "GET", "url": url }).to_string()
    }

    #[test]
    fn only_https_http_calls_to_a_matching_host_are_credentialed() {
        let g = gate();
        assert_eq!(
            g.credentialed_host("http", &input("https://API.github.com/x")),
            Some("api.github.com".to_string())
        );
        assert_eq!(
            g.credentialed_host("http", &input("http://api.github.com/")),
            None
        );
        assert_eq!(
            g.credentialed_host("http", &input("https://github.com.evil.io/")),
            None
        );
        assert_eq!(
            g.credentialed_host("web_fetch", &input("https://github.com/")),
            None
        );
    }

    #[test]
    fn approval_is_per_session_and_host() {
        let g = gate();
        let (a, b) = (SessionId::new("a"), SessionId::new("b"));
        g.approve(&a, "api.github.com");
        assert!(g.is_approved(&a, "api.github.com"));
        assert!(!g.is_approved(&b, "api.github.com"));
        assert!(!g.is_approved(&a, "github.com"));
    }
}
