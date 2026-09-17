//! [`BindingPolicy`] — the per-run grading snapshot `rhai` bindings resolve
//! through. Split out of `script.rs` (#451 file-cap repayment, ADR-0207 stage
//! 4c) — a self-contained unit with no dependency on the rest of the script
//! engine (parsing, the bridge, `run_rhai`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use entanglement_core::{Permission, PermissionProfile, SessionId, ToolOverlayEntry};

use crate::permission::{
    ancestor_chain, overlay_denies, overlay_entry_grade, overlay_grade_entry, permission_workdir,
};
use crate::permission_bash::resolve_scoped_bash_aware;
use crate::permission_path::grading_arg;
use crate::policy::PermissionResolver;
use crate::subagent::SpawnGuard;
use crate::tool_names::BINDING_TOOLS;

/// The per-run binding policy: grades each binding through the session's
/// permission **mode** and the binding tool's declared [`Capability`] — the
/// exact same pluggable [`PermissionResolver`] + ancestor-chain clamp
/// (ADR-0024) a direct tool call resolves through (ADR-0207 stage 4b), so a
/// script's `bash()` grades identically to a model-issued `bash` call
/// instead of a second, `Agent`-chain grading path that could drift
/// from it. Built once in the executor loop where the resolver/chain/overlay
/// state lives, then moved into the script task so the read stays ordered
/// with lifecycle events.
///
/// The `tools`/`disallowed_tools` mask this policy used to also filter
/// bindings through is retired (ADR-0207 §8, "the mask machinery is
/// deleted") — every binding is graded, never withheld by name.
pub struct BindingPolicy {
    /// The ancestor chain (nearest first, ADR-0024) each binding call's grade
    /// is resolved across and clamped least-privilege over — the same chain
    /// [`crate::tool_runner::resolve_effective`] walks for a direct call.
    chain: Vec<SessionId>,
    /// The same pluggable resolver a direct tool call grades through (#311)
    /// — reused here rather than re-deriving the session's mode table, so a
    /// multi-tenant embedder's own resolver governs script bindings too.
    resolver: Arc<dyn PermissionResolver>,
    /// The overlay entry that overrides a binding's grade, keyed by binding
    /// name (#628, closing the ADR-0149 deferral): the nearest
    /// ancestor-chain link with a live overlay **enable** entry for that
    /// binding, same lookup [`overlay_grade_entry`] gives `tool_runner::dispatch`
    /// — so a script's `bash()` under an overlay (its own session's or an
    /// ancestor's) grades the same way a direct `bash` call would, instead
    /// of always falling through to the resolver below.
    overlay: HashMap<&'static str, ToolOverlayEntry>,
    /// Bindings withdrawn by a matching overlay **deny** entry (#634,
    /// ADR-0207 §8 restore) — the same nearest-opinionated-link chain walk
    /// as `overlay` above, via [`crate::permission::overlay_denies`], so
    /// `/disable tool bash` reaches a script's `bash()` binding exactly like
    /// it reaches a direct call.
    denied: HashSet<&'static str>,
    /// The config permission ceiling (#172) an overlay grade still clamps
    /// against — the resolver already clamps its own result against this
    /// same ceiling (`ModeResolver`), so this is only consulted on the
    /// overlay path, which bypasses the resolver.
    base: PermissionProfile,
    /// The session's permission mode name, captured for the decline message
    /// only (`decide`'s `Deny` case names it, matching `tool_runner::dispatch`'s
    /// wording) — grading itself goes through `resolver`, not this string.
    /// `pub(super)`: `run_rhai` (in the parent `script` module) reads it
    /// directly to build the decline text.
    pub(super) mode: String,
    /// The project root a path-arg binding's argument is normalized relative
    /// to before matching (#485, ADR-0125) — mirrors `tool_runner::dispatch`'s
    /// use of `grading_arg`. `None` keeps the pre-#485 verbatim match.
    /// `pub(super)`: the parent `script` module's escape-root gate
    /// (`approval_cache_key`) reads it directly too.
    pub(super) root: Option<PathBuf>,
}

impl BindingPolicy {
    /// Snapshot each binding's overlay grade/withdrawal and the ancestor
    /// chain for `session` — `resolver` is reused as-is (already clamps to
    /// the config ceiling internally), so unlike the pre-4b policy chain
    /// there is no separate chain to fold `base` into.
    pub fn capture(
        guard: &SpawnGuard,
        overlays: &HashMap<SessionId, Vec<ToolOverlayEntry>>,
        session: &SessionId,
        base: &PermissionProfile,
        resolver: Arc<dyn PermissionResolver>,
        mode: String,
        root: Option<&Path>,
    ) -> Self {
        // The session's live tool overlay (#539, ADR-0149) reaches every
        // binding's Ask/Allow grade override (#628) via `overlay` below, and
        // its withdrawal via `denied` (#634) — mirroring
        // `tool_runner::dispatch`'s per-link chain walk instead of resolving
        // only through the mode.
        let session_chain = ancestor_chain(guard, session);
        let overlay: HashMap<&'static str, ToolOverlayEntry> = BINDING_TOOLS
            .into_iter()
            .filter_map(|tool| {
                overlay_grade_entry(overlays, &session_chain, tool).map(|entry| (tool, entry))
            })
            .collect();
        let denied: HashSet<&'static str> = BINDING_TOOLS
            .into_iter()
            .filter(|tool| overlay_denies(overlays, &session_chain, tool))
            .collect();
        BindingPolicy {
            chain: session_chain,
            resolver,
            overlay,
            denied,
            base: base.clone(),
            mode,
            root: root.map(Path::to_path_buf),
        }
    }

    /// Resolve one binding call's grade: a withdrawn binding (#634) declines
    /// flat, ahead of everything else; a tool with an overlay **enable**
    /// grade (#628) resolves that entry clamped to the config ceiling,
    /// replacing the resolver's grade exactly as `tool_runner::dispatch`
    /// does for a direct call; otherwise the grade is the least-privileged
    /// resolver result across the whole ancestor chain for this tool +
    /// argument (+ `workdir` for `exec`/`bash`, #480) — the resolve can hit a
    /// DB for a pluggable resolver, hence `async`. `read_raw` is graded as an
    /// alias of `read` — it is not in `BINDING_TOOLS` at all (never
    /// advertised, see [`crate::host::ReadRawTool`]), so without this alias a
    /// mode restricting `read` would be silently bypassed by a script
    /// reaching for the unlabeled raw path instead. The same alias applies
    /// to the overlay lookup, for the same reason. `glob_json`/`grep_json`
    /// alias `glob`/`grep` identically (ADR-0206) — a structured-output
    /// escape hatch must not be a permission escape hatch.
    pub(super) async fn decide(&self, tool: &'static str, input: &str) -> Permission {
        let tool = graded_name(tool);
        if self.denied.contains(tool) {
            return Permission::Deny;
        }
        let arg = grading_arg(tool, input, self.root.as_deref());
        let workdir = permission_workdir(tool, input);
        match self.overlay.get(tool) {
            Some(entry) => {
                let overlay_profile = overlay_entry_grade(tool, entry);
                let grade = resolve_scoped_bash_aware(
                    &overlay_profile,
                    tool,
                    arg.as_deref(),
                    workdir.as_deref(),
                );
                // `tool` is always one of the fixed `BINDING_TOOLS` names here
                // (via `graded_name`'s alias resolution above), so the ceiling
                // clamp can use the no-registry static table (ADR-0207 stage
                // 6c) — `BindingPolicy` grades a call before dispatch ever
                // looks a live `ToolRegistry` up for it.
                let capabilities = crate::capability::static_capability_of(tool).unwrap_or(&[]);
                crate::permission::clamp_to_base(
                    grade,
                    &self.base,
                    tool,
                    capabilities,
                    arg.as_deref(),
                    workdir.as_deref(),
                )
            }
            None => {
                crate::tool_runner::resolve_effective(&*self.resolver, &self.chain, tool, input)
                    .await
            }
        }
    }
}

/// The mask/grade identity of a binding call: script-facing variant names
/// resolve to the model-facing tool they ride on — `read_raw` → `read`
/// (ADR-0098) and `glob_json`/`grep_json` → `glob`/`grep` (ADR-0206). A
/// variant is never advertised as its own tool, so a profile can only mean
/// the alias target when it masks or grades its family; the mapping lives
/// here (not in `BINDING_TOOLS`) because the *bridge* still dispatches the
/// literal variant name — [`crate::host::GlobJsonTool`] etc. — this is
/// purely the policy identity.
fn graded_name(tool: &'static str) -> &'static str {
    match tool {
        "read_raw" => "read",
        "glob_json" => "glob",
        "grep_json" => "grep",
        other => other,
    }
}
