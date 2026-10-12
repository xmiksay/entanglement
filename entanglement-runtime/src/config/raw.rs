//! The on-disk `config.yml` shape, deserialized from the merged layers
//! before [`super::parse`] resolves it into a [`super::Config`]. Split out of
//! `mod.rs` (#745).

use std::collections::HashMap;

use entanglement_core::WebSearchConfig;
use serde::Deserialize;

use super::ceiling;
use super::tui::TuiConfig;
use crate::file_backup::BackupLimits;
use crate::hooks::Hooks;
use crate::mcp::McpServerConfig;
use crate::mode;

/// The raw file shape. `deny_unknown_fields` makes a typo'd key a loud error
/// rather than a silently-ignored setting, exactly like the agent/provider files.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawConfig {
    #[serde(default)]
    pub(super) agent: Option<String>,
    /// Default permission mode (ADR-0207 §12): `--mode` > this > the engine's
    /// `DEFAULT_MODE`. Not validated against the table here — parsing has no
    /// registry to build one against ([`ceiling`](super::ceiling)'s doc); an unknown name is
    /// caught where the table is actually built (`services.rs`, alongside
    /// `modes:` below), the same reason `agent` above isn't validated here
    /// either.
    #[serde(default)]
    pub(super) mode: Option<String>,
    #[serde(default)]
    pub(super) provider: Option<String>,
    #[serde(default)]
    pub(super) model: Option<String>,
    #[serde(default)]
    pub(super) verbose: bool,
    /// Parsed into the config ceiling's [`entanglement_core::PermissionProfile`]
    /// via [`ceiling::build_ceiling_profile`](super::ceiling::build_ceiling_profile) (ADR-0207 stage 6c) — the same
    /// `default`/`allow`/`deny`/`prompt` grammar a mode body uses, not the
    /// retired free-form `tool: allow|ask|deny` map. A file still in the old
    /// shape is self-healed by [`migrate_permissions`](super::migrate_permissions) before it ever
    /// reaches this deserialize.
    #[serde(default)]
    pub(super) permissions: Option<ceiling::RawCeiling>,
    /// Per-mode tuning (ADR-0207 §5): each key must name one of the four
    /// built-in modes (`research`/`plan`/`build`/`auto`) — there is no
    /// syntax to define a fifth (§2) — and may only *add* `allow`/`deny`/
    /// `prompt` rules, validated against a live [`crate::capability::Capability`]
    /// resolver once the tool registry exists ([`mode::apply_tuning`](crate::mode::apply_tuning),
    /// `services.rs`). An unrecognized mode name is a loud error there, not
    /// silently ignored here.
    #[serde(default)]
    pub(super) modes: HashMap<String, mode::ModeTuning>,
    /// Lifecycle hooks (#199): external commands run around tool execution and on
    /// prompt ingress. Deserializes straight into [`Hooks`] (plain serde, unlike
    /// `permissions`); absent ⇒ no hooks.
    #[serde(default)]
    pub(super) hooks: Hooks,
    /// External MCP tool servers (#198): a map of server name → spawn config. Each
    /// server's tools are discovered and registered into the runtime tool
    /// registry. Absent ⇒ no servers.
    #[serde(default)]
    pub(super) mcp: HashMap<String, McpServerConfig>,
    /// Hidden `http()` credentials (ADR-0227), host → headers/cookies.
    /// Stripped from the project layer before the merge.
    #[serde(default)]
    pub(super) http_auth: super::http_auth::HttpAuth,
    /// Provider-side web search (#305, ADR-0075): opt-in, bound onto the LLM
    /// client at build time — never seen by core. Absent ⇒ disabled. Enabling
    /// it is consent (the server tool runs provider-side, *outside* the runtime
    /// permission ladder).
    #[serde(default)]
    pub(super) web_search: WebSearchConfig,
    /// Cap on the inner LLM→tool loop within a single turn (#177). Default 200.
    #[serde(default)]
    pub(super) max_turns: Option<usize>,
    /// Auto-hibernate a settled root session (and its spawn sub-tree) after this
    /// many idle seconds (#401, ADR-0090/[ADR-0105]). Absent ⇒ `None` ⇒ the
    /// engine default: no sweep, eviction stays embedder-driven.
    ///
    /// [ADR-0105]: ../../../docs/adr/0105-expose-idle-ttl-via-runtime-config.md
    #[serde(default)]
    pub(super) idle_ttl_secs: Option<u64>,
    /// Try an LLM summary before the prune-only fallback when a turn's history
    /// overflows the context budget (ADR-0103). Absent ⇒ `None` ⇒ the engine
    /// default (`true`).
    #[serde(default)]
    pub(super) auto_compact: Option<bool>,
    /// Editor command for the TUI `$EDITOR` round-trip (`/editor`). When set it
    /// **wins over** `$VISUAL`/`$EDITOR`, so the persisted choice is the default
    /// regardless of the shell env; absent ⇒ fall back to `$VISUAL` → `$EDITOR`
    /// → `vi`. Word-split like a shell command, so `"code --wait"` works.
    #[serde(default)]
    pub(super) editor: Option<String>,
    /// Session-log retention in days (Issue 4, Phase 4.2): on startup the TUI
    /// prunes `.jsonl` files in the cwd's session dir whose mtime is older than
    /// this. The embedded default (`30`) keeps the on-disk footprint bounded
    /// without surprising a user who never configures it. Absent in a layer ⇒
    /// fall through to the lower layer, exactly like every other key.
    #[serde(default)]
    pub(super) session_retention_days: Option<u64>,
    /// Terminal-UI preferences (ADR-0213). Retired keys (`discovery:`,
    /// `tool_advertising:`) never reach this struct: [`super::retired`]
    /// strips them with a warning.
    #[serde(default)]
    pub(super) tui: TuiConfig,
    /// Per-write file backups behind `/undo`/`/rewind` (#787, ADR-0212):
    /// `enabled` + size caps, each field optional ([`super::undo`]).
    #[serde(default)]
    pub(super) undo: BackupLimits,
}
