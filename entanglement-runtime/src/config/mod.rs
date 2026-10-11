//! User configuration — the layered settings file (#172, ADR-0047).
//!
//! A general user-config file, same defaults+override shape as the provider
//! catalog (#118) and the agent/skill registries (#112/#114): an embedded
//! default is deep-merged with an optional user file and an optional repository
//! file, later layers winning.
//!
//! # Layers & precedence
//!
//! Three layers, later wins:
//!
//! 1. **default** — the embedded [`include_str!`] of `defaults.yml`.
//! 2. **user** — `${config_dir}/entanglement/config.yml`
//!    (override the path via `ENTANGLEMENT_CONFIG_FILE`).
//! 3. **project** — `<root>/.entanglement/config.yml`.
//!
//! The merge is at the [`serde_yaml::Value`] level *before* deserializing, so a
//! layer can override a single field and leave its siblings untouched, and
//! `deny_unknown_fields` still validates the merged result (typos are loud). The
//! project layer is **trusted** (ADR-0047): a repository may override the user's
//! configuration for work in that repository, mirroring git's
//! `system < global < local`.
//!
//! # Sections
//!
//! - `permissions` — the first section: a `default`/`allow`/`deny`/`prompt`
//!   ceiling in the same grammar a permission mode body uses (ADR-0207 §4/§5,
//!   stage 6c) — not the old free-form `tool: allow|ask|deny` map, which a
//!   startup pass ([`migrate_permissions`]) self-heals in place. Graded
//!   least-privilege against the session's own mode via
//!   [`crate::permission::clamp_to_base`], through
//!   [`crate::mode::Mode::from_permission_profile`] so a bare capability-class
//!   key (`deny: [write]`) catches every write-capable tool, not just one
//!   named literally `write`. Argument/path patterns (#173) build on it. It
//!   is a pure *ceiling*; the orthogonal "always allow" grants (#174) that
//!   *raise* an `Ask` live in a separate managed file ([`crate::grants`]), not
//!   here.
//! - `modes` — per-mode tuning (ADR-0207 §5): adds `allow`/`deny`/`prompt`
//!   rules onto one of the four built-in modes (there is no syntax to define
//!   a fifth, or to remove a shipped rule — longest-match already makes a
//!   removal syntax unnecessary). Applied over
//!   [`crate::mode::ModeTable::builtin`] once a live tool registry exists to
//!   validate a tuning rule's capability against ([`mode::apply_tuning`]). A
//!   rule naming a tool outside the compile-time vocabulary (a typo, or a
//!   name retired by a rename) logs a non-fatal startup warning naming the
//!   file, the mode and the entry ([`mode_warn`], reviving ADR-0166's
//!   posture for this rule language).
//! - `hooks` — lifecycle hooks (#199, ADR-0066): external commands run around
//!   tool execution (`pre_`/`post_tool_use`) and on prompt ingress
//!   (`user_prompt_submit`), plus six observe-only lifecycle points
//!   (`session_start`/`session_end`/`stop`/`pre_compact`/`subagent_stop`/
//!   `notification`, #734). See [`crate::hooks`]. Empty by default.
//! - `tui` — terminal-UI preferences ([`tui::TuiConfig`], ADR-0213): one
//!   block every TUI knob joins, live-reloaded by the TUI. Its `statusline`
//!   templates ([`statusline`], #762) warn once per unknown placeholder; its
//!   `theme` ([`theme`]) warns per bad slot/colour and `keybindings`
//!   ([`keybindings`]) once per rejected remap (#735).
//! - general settings — `agent` / `mode` / `provider` / `model` / `verbose` /
//!   `max_turns` / `idle_ttl_secs` / `auto_compact`. Each is a *fallback*: an
//!   explicit CLI flag or environment variable wins over the file (env >
//!   config > embedded default) — `mode`'s precedence is `--mode` > this >
//!   the engine's `DEFAULT_MODE` (ADR-0207 §12), mirroring `agent`'s own
//!   `--agent` > this > `DEFAULT_AGENT`. `idle_ttl_secs` (#401, ADR-0090)
//!   maps onto `EngineConfig::idle_ttl`; `None` (the default) leaves
//!   auto-hibernation off, exactly as before this setting existed.
//!   `auto_compact` (ADR-0103) maps onto `EngineConfig::auto_compact` the
//!   same way — `None` keeps the engine default (`true`), `false` restores
//!   prune-then-refuse.
//!
//! # First-run scaffold (#219)
//!
//! [`scaffold_if_missing`] drops a fully-commented starter template
//! (`template.yml`) at the user path when none exists, so the config dir is a
//! discoverable starting point rather than empty. Because every setting is
//! commented out the file parses to `Null` and is skipped in the merge
//! ([`read_layer`]) — a no-op until a user uncomments a key.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use entanglement_core::{PermissionProfile, ToolAdvertising, WebSearchConfig};
use serde_yaml::Value;

use crate::endpoint::EndpointConfig;
use crate::hooks::Hooks;
use crate::mcp::McpServerConfig;
use crate::mode;
use layers::{
    discover, merge_value, provenance, user_config_path, RawLayer, CONFIG_FILE_ENV, TEMPLATE_YML,
};
use raw::RawConfig;
// Only `bare_config` and the sibling test suites build a bare defaults layer.
#[cfg(test)]
use layers::default_layer;

pub mod agent_generation;
pub mod agent_models;
pub mod atomic;
pub mod aux_models;
mod ceiling;
mod ceiling_warn;
pub mod env_file;
pub mod env_key;
pub mod keybindings;
pub mod keychord;
mod layers;
pub mod lock;
pub mod mcp_persist;
pub mod mcp_tokens;
mod migrate_permissions;
mod mode_warn;
mod raw;
pub mod retired;
pub mod state_dir;
pub mod statusline;
pub mod theme;
pub mod tui;
pub mod write_key;

pub use layers::{scaffold_if_missing, ConfigLayer};
pub use mcp_persist::save_mcp;
pub use mcp_tokens::McpTokenStore;
pub use tui::TuiConfig;

/// The CLI `skutter config set-key` handler (rpassword prompt + catalog lookup).
/// Behind `cli`+`provider`: it prompts (rpassword, a `cli`-feature dep) and looks
/// the provider's key env up in the catalog (`provider` feature).
#[cfg(all(feature = "cli", feature = "provider"))]
pub mod keys;
/// The CLI `skutter config connect|disconnect` handlers (#684 edge d) — the
/// LLM twin of `/mcp connect`. Behind the same `cli`+`provider` gate as
/// `keys`: it resolves the provider from the catalog and prints to a terminal.
#[cfg(all(feature = "cli", feature = "provider"))]
pub mod llm_connect;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_retention;
#[cfg(test)]
mod tests_tool_advertising;
#[cfg(test)]
mod tests_tui;

const DEFAULTS_YML: &str = include_str!("defaults.yml");

/// The embedded default for [`Config::session_retention_days`]: 30 days. A
/// session log untouched for a month is stale enough to prune without surprising
/// a user who never set the key.
const DEFAULT_SESSION_RETENTION_DAYS: u64 = 30;

/// Guards mutation of `ENTANGLEMENT_CONFIG_FILE` — process-global env state —
/// across every test in this crate that points the layered config loader (or
/// [`mcp_persist::save_mcp`]'s surgical `mcp:` writer) at a temp file. Shared by
/// `config::tests` and `config::mcp_persist::tests` (not module-local to
/// either) so the two suites can't race on the same var when `cargo test` runs
/// them in parallel threads.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A parsed embedded-defaults [`Config`], for sibling crates' unit tests
/// (e.g. `tool_advertising`'s precedence tests): constructed through the real
/// `parse` so it can never drift from `Config`'s field list, and overridable
/// field-by-field via struct update syntax.
#[cfg(test)]
pub(crate) fn bare_config() -> Config {
    parse(&[default_layer()])
        .expect("embedded defaults.yml is valid — guarded by test")
        .config
}

/// Resolved user configuration — the merged, validated values every head reads.
#[derive(Debug, Clone)]
pub struct Config {
    /// Default agent profile when the CLI passes none.
    pub agent: Option<String>,
    /// Default permission mode when `--mode` is absent (ADR-0207 §12):
    /// `--mode` > this > the engine's `DEFAULT_MODE` ("build"). `None` when
    /// unset — the CLI falls through to the engine default itself, exactly
    /// like `agent` above.
    pub mode: Option<String>,
    /// Provider name (like `ENTANGLEMENT_PROVIDER`); `None` ⇒ auto-detect.
    pub provider: Option<String>,
    /// Model id override; `None` ⇒ the provider's catalog default.
    pub model: Option<String>,
    /// Log at `debug` by default (like `--verbose`).
    pub verbose: bool,
    /// The global permission ceiling, in the same `default`/`allow`/`deny`/
    /// `prompt` grammar a mode body uses (ADR-0207 stage 6c). Allow-all by
    /// default (a no-op). Graded through [`crate::mode::Mode::resolve`] via
    /// [`crate::mode::Mode::from_permission_profile`], not this type's own
    /// matcher — see [`crate::permission::clamp_to_base`].
    pub permissions: PermissionProfile,
    /// Per-mode tuning (ADR-0207 §5): raw `config.yml` `modes:` data, keyed
    /// by built-in mode name. Applied over [`crate::mode::ModeTable::builtin`]
    /// once a live [`crate::tools::ToolRegistry`] exists to validate against
    /// (`services.rs`) — kept raw here since `Config` resolution has no registry
    /// to build a real [`crate::mode::ModeTable`] with.
    pub modes: HashMap<String, mode::ModeTuning>,
    /// Lifecycle hooks (#199). Empty by default (a no-op).
    pub hooks: Hooks,
    /// External MCP tool servers (#198). Empty by default (a no-op).
    pub mcp: HashMap<String, McpServerConfig>,
    /// Definition-driven HTTP endpoint tools (#560 P8). Empty by default (a
    /// no-op). See [`crate::endpoint`].
    pub endpoints: HashMap<String, EndpointConfig>,
    /// Provider-side web search (#305). Disabled by default (a no-op).
    pub web_search: WebSearchConfig,
    /// Cap on the inner LLM→tool loop within a single turn (#177). `None` ⇒
    /// the engine default (200). User-configurable so a long autonomous run can
    /// be loosened (or a runaway tightened) without a recompile.
    pub max_turns: Option<usize>,
    /// Auto-hibernate a settled root session (and its spawn sub-tree) once idle
    /// this long (#401, ADR-0090). `None` (the default) leaves
    /// `EngineConfig::idle_ttl` unset — the supervisor sweep never arms and
    /// eviction stays embedder-driven, matching every release before this. A
    /// long-lived multi-session embedder like `skutter serve` sets this to cap
    /// memory growth; the CLI/TUI (single session, process-bound) rarely need
    /// it but sharing the one engine-global `EngineConfig` costs them nothing.
    pub idle_ttl: Option<Duration>,
    /// Auto-summarize on context overflow (#398, ADR-0103). `None` (the
    /// default) leaves `EngineConfig::auto_compact` at its engine default
    /// (`true`): an over-budget turn first tries an LLM summary in place, then
    /// falls back to the prune-only `Context::compact`. `Some(false)` restores
    /// the pre-#398 prune-then-refuse behavior — useful when the summarization
    /// round-trip is unwanted (a metered endpoint, an offline model, or a
    /// workload that would rather refuse than continue on a lossy summary).
    pub auto_compact: Option<bool>,
    /// Editor command for the TUI `$EDITOR` round-trip; `Some` wins over
    /// `$VISUAL`/`$EDITOR`. `None` ⇒ resolve from env then `vi`.
    pub editor: Option<String>,
    /// Session-log retention in days (Issue 4, Phase 4.2). The startup prune
    /// deletes `.jsonl` files in the cwd's session dir whose mtime is older
    /// than this. Precedence: config, then the embedded default (30) — no env
    /// override (ADR-0213). Best-effort: a read-only data dir logs and
    /// continues, never fatal.
    pub session_retention_days: u64,
    /// Install-wide tool-advertising override (ADR-0196): `full` |
    /// `tool_search`. `None` (the default) ⇒ per-model resolution: the
    /// catalog entry's `tool_advertising:` preference, else `tool_search`.
    /// No env override since ADR-0213.
    pub tool_advertising: Option<ToolAdvertising>,
    /// Terminal-UI preferences (`tui:` block, ADR-0213).
    pub tui: TuiConfig,
}

/// The resolved config plus the provenance `skutter inspect config` reports:
/// which layer last set each field, and every layer that was present.
pub struct Resolved {
    pub config: Config,
    /// `(layer, source)` for every discovered layer, in precedence order.
    pub layers: Vec<(ConfigLayer, String)>,
    /// The winning layer for each top-level key that any layer defined.
    pub provenance: Vec<(String, ConfigLayer)>,
    /// One line per retired key a layer still carried (ADR-0213) — ignored,
    /// reported by the caller once logging/stderr is ready.
    pub warnings: Vec<String>,
}

impl Config {
    /// Resolve the user config for `root`: embedded defaults, deep-merged with the
    /// user file (if present) and the repository file (if present). A malformed or
    /// unreadable file in any layer is a loud error, never a silent fallback.
    pub fn load(root: &Path) -> Result<Config> {
        Ok(Self::resolve(root)?.config)
    }

    /// Like [`load`](Self::load) but also returns the per-field provenance and the
    /// discovered layers, for `skutter inspect config`.
    pub fn resolve(root: &Path) -> Result<Resolved> {
        parse(&discover(root)?)
    }
}

/// Merge the layers' docs (later wins) and deserialize into a [`Config`], keeping
/// per-field provenance. The merge and the `deny_unknown_fields` validation both
/// run on the combined [`Value`], so a field override keeps its siblings and a
/// typo in any layer is rejected.
fn parse(raw_layers: &[RawLayer]) -> Result<Resolved> {
    let mut raw_layers: Vec<RawLayer> = raw_layers.to_vec();
    let mut warnings = retired::strip_retired_keys(&mut raw_layers);
    let raw_layers = raw_layers.as_slice();
    ceiling_warn::warn_project_permission_overrides(raw_layers);
    mode_warn::warn_stale_mode_tuning_names(raw_layers);
    let mut merged = Value::Null;
    for rl in raw_layers {
        merged = merge_value(merged, rl.doc.clone());
    }
    let raw: RawConfig = serde_yaml::from_value(merged).context("validating merged user config")?;
    let permissions = ceiling::build_ceiling_profile(raw.permissions.as_ref());
    let config = Config {
        agent: raw.agent,
        mode: raw.mode,
        provider: raw.provider,
        model: raw.model,
        verbose: raw.verbose,
        permissions,
        modes: raw.modes,
        hooks: raw.hooks,
        mcp: raw.mcp,
        endpoints: raw.endpoints,
        web_search: raw.web_search,
        max_turns: raw.max_turns,
        idle_ttl: raw.idle_ttl_secs.map(Duration::from_secs),
        auto_compact: raw.auto_compact,
        editor: raw.editor.filter(|s| !s.trim().is_empty()),
        session_retention_days: raw
            .session_retention_days
            .unwrap_or(DEFAULT_SESSION_RETENTION_DAYS),
        tool_advertising: raw.tool_advertising,
        tui: raw.tui,
    };
    warnings.extend(config.tui.warnings());
    Ok(Resolved {
        config,
        layers: raw_layers
            .iter()
            .map(|r| (r.layer, r.source.clone()))
            .collect(),
        provenance: provenance(raw_layers),
        warnings,
    })
}
