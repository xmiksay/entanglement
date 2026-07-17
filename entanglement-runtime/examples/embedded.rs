//! Minimal multi-tenant embedding example (issue #315, `docs/embedding.md`).
//!
//! Compiled (not just documented) so the guide can't silently drift from the
//! real API: `make lint`/`make check-lean` run `clippy --all-targets` over
//! this crate with and without `--no-default-features`, which includes this
//! file. Run it directly with `cargo run -p entanglement-runtime --example
//! embedded --no-default-features`.
//!
//! Demonstrates, against the lean embeddable library (no CLI/TUI/transport
//! deps):
//! - one `Holly` serving two tenants, sessions namespaced `{tenant}:{uuid}`;
//! - ownership filtering on the `subscribe()` fan-out so tenant B never sees
//!   tenant A's events;
//! - a custom `PermissionResolver`/`GrantStore` pair (the #311 pluggable
//!   policy seam) deciding tool grades per tenant instead of the CLI's
//!   file-backed defaults.
//!
//! Uses `EchoLlm` (the `EngineConfig` default) so the example needs no
//! provider key and produces deterministic output.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use entanglement_core::{
    ApprovalScope, EngineConfig, Holly, InMsg, OutEvent, Permission, PermissionProfile,
    ProfileRegistry, SessionId,
};
use entanglement_runtime::hooks::Hooks;
use entanglement_runtime::permission::permission_arg;
use entanglement_runtime::policy::{GrantStore, PermissionResolver};
use entanglement_runtime::skills::SkillRegistry;
use entanglement_runtime::{host, tool_runner};
use tokio::sync::broadcast::error::RecvError;

/// Grades each tool call by the calling session's tenant — the part of the
/// #311 seam a multi-tenant embedder swaps in place of the CLI's
/// `ProfileResolver`. A real embedder looks `tenant_of(session)` up in its own
/// DB instead of this static map.
struct TenantResolver {
    tenants: HashMap<&'static str, PermissionProfile>,
}

#[async_trait]
impl PermissionResolver for TenantResolver {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Permission {
        let arg = permission_arg(tool, input);
        self.tenants
            .get(tenant_of(session))
            .map(|profile| profile.resolve(tool, arg.as_deref()))
            .unwrap_or(Permission::Deny)
    }
}

/// No persisted "always allow" grants in this example — every call re-asks
/// per its tenant's grade. A real embedder backs this with a per-tenant table
/// in its own store (the write side of the same #311 seam).
struct NoGrants;

#[async_trait]
impl GrantStore for NoGrants {
    fn is_granted(&self, _session: &SessionId, _tool: &str, _arg: Option<&str>) -> bool {
        false
    }

    async fn record(
        &self,
        _session: &SessionId,
        _tool: &str,
        _arg: Option<&str>,
        _scope: ApprovalScope,
    ) {
    }

    fn forget_session(&self, _session: &SessionId) {}
}

/// The `{tenant}:{uuid}` convention: the tenant is whatever precedes the first
/// `:`. `SessionId`'s inner string is public, so this is the whole "filter"
/// idiom — no dedicated core API needed for it.
fn tenant_of(session: &SessionId) -> &str {
    session.0.split_once(':').map_or(&session.0[..], |(t, _)| t)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let tools = host::host_tools(std::env::temp_dir());
    let tool_specs = tools.specs();
    let profiles = ProfileRegistry::new(); // just the built-in `build` profile

    let holly = Holly::spawn(EngineConfig {
        tool_specs,
        profiles: profiles.clone(),
        ..EngineConfig::default() // EchoLlm — no provider key, deterministic
    });

    let mut tenants = HashMap::new();
    tenants.insert("acme", PermissionProfile::new(Permission::Allow));
    tenants.insert("globex", PermissionProfile::new(Permission::Deny));
    let resolver: Arc<dyn PermissionResolver> = Arc::new(TenantResolver { tenants });
    let grants: Arc<dyn GrantStore> = Arc::new(NoGrants);

    let _executor = tool_runner::spawn_tool_executor_with_policy(
        &holly,
        tools.shared(),
        Arc::new(RwLock::new(profiles)),
        Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))),
        PermissionProfile::new(Permission::Allow),
        Arc::new(Mutex::new(HashMap::new())),
        resolver,
        grants,
        Hooks::default(),
        None,
    );

    let acme = SessionId::new(format!("acme:{}", SessionId::new_uuid()));
    let globex = SessionId::new(format!("globex:{}", SessionId::new_uuid()));

    run_one_turn(&holly, &acme, "hello from acme").await?;
    run_one_turn(&holly, &globex, "hello from globex").await?;

    Ok(())
}

/// Send one prompt and relay only `session`'s own events — the ownership
/// filter every multi-tenant head applies before forwarding to that tenant's
/// transport (a WS socket, an SSE stream, …).
async fn run_one_turn(holly: &Holly, session: &SessionId, prompt: &str) -> anyhow::Result<()> {
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(session.clone(), prompt.to_string()))
        .await?;
    loop {
        let ev = match sub.recv().await {
            Ok(ev) => ev,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break,
        };
        if ev.session() != Some(session) {
            continue; // another tenant's event on the shared fan-out
        }
        if let OutEvent::TextDelta { text, .. } = &ev {
            println!("[{session}] {text}");
        }
        if matches!(ev, OutEvent::Done { .. }) {
            break;
        }
    }
    Ok(())
}
