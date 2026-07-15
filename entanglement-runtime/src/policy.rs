//! Pluggable policy seams for the runtime tool executor (#311).
//!
//! [`spawn_tool_executor_with_policy`][crate::tool_runner::spawn_tool_executor_with_policy]
//! hard-codes nothing about *where* an allow/deny/ask decision or an "always
//! allow" grant comes from: it drives two trait objects, a [`PermissionResolver`]
//! and a [`GrantStore`]. The single-user CLI plugs in the defaults below — the
//! agent-profile chain clamped by the config ceiling ([`ProfileResolver`]) and
//! the managed grants file ([`DefaultGrantStore`]) — so its behavior is
//! byte-identical. A multi-tenant embedder that stores rules per user in its own
//! DB swaps both without forking the ~350-line executor, keeping the shared
//! interception ladder, spawn/mask gating, hooks, rhai, and plan/tasks tools.
//!
//! ## Where the seams sit in the ladder
//!
//! The executor asks the resolver for the grade of a *single* session, then takes
//! the least-privileged grade across the session's ancestor chain
//! ([`ancestor_chain`][crate::permission::ancestor_chain]) — so the sub-agent
//! privilege ceiling (ADR-0024) and spawn/mask gating stay in the ladder **on top
//! of** the resolver result. A tenant rule can widen or narrow a session's own
//! grade, but can never widen a child beyond its parent. The `GrantStore` only
//! ever upgrades a resolved `Ask` to `Allow`; a multi-tenant store's "always
//! allow" write lands in its own DB and surfaces on the *next* call through its
//! resolver, so the trait's read side is deliberately the resolver's job — the
//! store's own [`is_granted`][GrantStore::is_granted] covers only the default
//! file/session grants the CLI needs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use entanglement_core::{AgentProfile, ApprovalScope, Permission, PermissionProfile, SessionId};

use crate::grants::FileGrantStore;
use crate::permission::{clamp_to_base, permission_arg, permission_for};

/// Decide the `Allow | Ask | Deny` grade for one concrete tool call. `session`
/// lets a multi-tenant embedder derive the tenant; `input` (the raw JSON tool
/// input) enables argument-scoped rules. Called once per session in a call's
/// ancestor chain — the executor clamps the results least-privilege, so a
/// resolver need only decide a single session's own grade. Async because a real
/// embedder hits a DB; the ladder already runs in a detached task.
#[async_trait]
pub trait PermissionResolver: Send + Sync {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Permission;
}

/// Persist and read "always allow" grants (#174). A grant only ever upgrades a
/// resolved `Ask` to `Allow`. The write side ([`record`][GrantStore::record])
/// is async because an [`ApprovalScope::Always`] grant may hit a DB; the read
/// side ([`is_granted`][GrantStore::is_granted]) is a fast in-memory/cached check
/// the executor consults synchronously before prompting. A multi-tenant store
/// writes an "always" rule to its DB and resolves later reads through its
/// [`PermissionResolver`] instead, so its `is_granted` can simply return `false`.
#[async_trait]
pub trait GrantStore: Send + Sync {
    /// Whether `(tool, arg)` from `session` is already granted (session or
    /// always), upgrading a resolved `Ask` to `Allow`.
    fn is_granted(&self, session: &SessionId, tool: &str, arg: Option<&str>) -> bool;
    /// Record an approval per its scope. `Once` records nothing; `Session` is
    /// in-memory; `Always` persists (a file for the default, a DB row for a
    /// multi-tenant store).
    async fn record(
        &self,
        session: &SessionId,
        tool: &str,
        arg: Option<&str>,
        scope: ApprovalScope,
    );
    /// Release a session's in-memory grants when it ends.
    fn forget_session(&self, session: &SessionId);
}

/// The single-user CLI resolver: the executor's live active-profile map plus the
/// config permission ceiling (#172). Resolves a session's *own* profile grade
/// clamped by the base ceiling; the executor mins this across the ancestor chain
/// for the sub-agent clamp, so the pair reproduces `effective_permission` +
/// `clamp_to_base` exactly (the clamp is monotonic, so min-of-clamped equals
/// clamp-of-min). Shares the same `Arc<Mutex<..>>` the executor folds lifecycle
/// events into, so it always reads the current profile view.
pub struct ProfileResolver {
    active: Arc<Mutex<HashMap<SessionId, AgentProfile>>>,
    base: PermissionProfile,
}

impl ProfileResolver {
    pub fn new(
        active: Arc<Mutex<HashMap<SessionId, AgentProfile>>>,
        base: PermissionProfile,
    ) -> Self {
        Self { active, base }
    }
}

#[async_trait]
impl PermissionResolver for ProfileResolver {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Permission {
        let arg = permission_arg(tool, input);
        // Read the folded profile view without holding the lock across an await
        // (there is none here) — the executor's single-threaded loop is the sole
        // writer, so this brief lock never contends.
        let own = {
            let active = self.active.lock().unwrap();
            permission_for(&active, session, tool, arg.as_deref())
        };
        clamp_to_base(own, &self.base, tool, arg.as_deref())
    }
}

/// The single-user CLI grant store: the managed [`FileGrantStore`] behind a
/// `Mutex` so the shared trait object can record and read grants. An `Always`
/// grant persists to `${config_dir}/entanglement/grants.yml`.
pub struct DefaultGrantStore {
    inner: Mutex<FileGrantStore>,
}

impl DefaultGrantStore {
    /// Load the persisted `Always` grants from the managed file.
    pub fn load() -> Self {
        Self {
            inner: Mutex::new(FileGrantStore::load()),
        }
    }
}

#[async_trait]
impl GrantStore for DefaultGrantStore {
    fn is_granted(&self, session: &SessionId, tool: &str, arg: Option<&str>) -> bool {
        self.inner.lock().unwrap().is_granted(session, tool, arg)
    }

    async fn record(
        &self,
        session: &SessionId,
        tool: &str,
        arg: Option<&str>,
        scope: ApprovalScope,
    ) {
        self.inner.lock().unwrap().record(session, tool, arg, scope);
    }

    fn forget_session(&self, session: &SessionId) {
        self.inner.lock().unwrap().forget_session(session);
    }
}
