//! The library's default [`GrantStore`]: every grant in memory, gone when the
//! store is dropped. A head that wants `Always` grants to outlive the process
//! supplies its own store (`skutter` keeps them in a managed file).
//!
//! A grant matches **exactly** — the tool, the argument
//! [`grading_arg`][crate::permission_path::grading_arg] extracted (`None`
//! grants every call to a tool with no permission argument) and the **mode**
//! it was earned in (ADR-0207 §8: a grant from `build` never fires in
//! `research`). A child session never inherits a parent's `Session` grants.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use entanglement_core::{ApprovalScope, SessionId};

use super::GrantStore;

/// `(tool, arg, mode)` — compared by exact equality.
type Key = (String, Option<String>, String);

fn key(tool: &str, arg: Option<&str>, mode: &str) -> Key {
    (tool.to_string(), arg.map(str::to_string), mode.to_string())
}

#[derive(Default)]
struct Grants {
    session: HashMap<SessionId, HashSet<Key>>,
    always: HashSet<Key>,
}

/// In-memory `Session` and `Always` grants; see the module docs.
#[derive(Default)]
pub struct InMemoryGrantStore {
    grants: Mutex<Grants>,
}

impl InMemoryGrantStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Grants> {
        self.grants.lock().expect("grants mutex poisoned")
    }
}

#[async_trait]
impl GrantStore for InMemoryGrantStore {
    fn is_granted(&self, session: &SessionId, tool: &str, arg: Option<&str>, mode: &str) -> bool {
        let key = key(tool, arg, mode);
        let grants = self.lock();
        grants.always.contains(&key)
            || grants
                .session
                .get(session)
                .is_some_and(|set| set.contains(&key))
    }

    async fn record(
        &self,
        session: &SessionId,
        tool: &str,
        arg: Option<&str>,
        scope: ApprovalScope,
        mode: &str,
    ) {
        let key = key(tool, arg, mode);
        let mut grants = self.lock();
        match scope {
            ApprovalScope::Once => {}
            ApprovalScope::Session => {
                grants
                    .session
                    .entry(session.clone())
                    .or_default()
                    .insert(key);
            }
            ApprovalScope::Always => {
                grants.always.insert(key);
            }
        }
    }

    fn forget_session(&self, session: &SessionId) {
        self.lock().session.remove(session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn once_records_nothing() {
        let store = InMemoryGrantStore::new();
        let s = SessionId::new("s");
        store
            .record(&s, "bash", Some("ls"), ApprovalScope::Once, "build")
            .await;
        assert!(!store.is_granted(&s, "bash", Some("ls"), "build"));
    }

    #[tokio::test]
    async fn session_grant_is_scoped_to_its_session_and_forgotten() {
        let store = InMemoryGrantStore::new();
        let (a, b) = (SessionId::new("a"), SessionId::new("b"));
        store
            .record(
                &a,
                "bash",
                Some("git status"),
                ApprovalScope::Session,
                "build",
            )
            .await;
        assert!(store.is_granted(&a, "bash", Some("git status"), "build"));
        assert!(!store.is_granted(&a, "bash", Some("git log"), "build"));
        assert!(!store.is_granted(&b, "bash", Some("git status"), "build"));
        store.forget_session(&a);
        assert!(!store.is_granted(&a, "bash", Some("git status"), "build"));
    }

    /// ADR-0207 §8: a grant earned in one mode never fires in another.
    #[tokio::test]
    async fn a_grant_is_scoped_to_its_mode() {
        let store = InMemoryGrantStore::new();
        let s = SessionId::new("s");
        store
            .record(
                &s,
                "bash",
                Some("git status"),
                ApprovalScope::Session,
                "build",
            )
            .await;
        store
            .record(&s, "grep", None, ApprovalScope::Always, "build")
            .await;
        assert!(!store.is_granted(&s, "bash", Some("git status"), "research"));
        assert!(!store.is_granted(&s, "grep", None, "research"));
    }

    #[tokio::test]
    async fn an_always_grant_covers_every_session_and_survives_forget() {
        let store = InMemoryGrantStore::new();
        let s = SessionId::new("s");
        store
            .record(&s, "grep", None, ApprovalScope::Always, "build")
            .await;
        store.forget_session(&s);
        assert!(store.is_granted(&SessionId::new("other"), "grep", None, "build"));
    }
}
