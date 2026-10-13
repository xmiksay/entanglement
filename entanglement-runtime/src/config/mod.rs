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
//!   rule — here or in `permissions:` — naming a tool outside the
//!   compile-time vocabulary (a typo, or a name retired by a rename), one
//!   dispatch never grades by name (`mcp`, the discovery tools, the
//!   script-only read variants `read_raw`/`*_json`; #843), or a tool-name
//!   glob (`mcp__gh__*`, `list_?` — rule keys match the tool part exactly,
//!   only a bare `*` is a wildcard), logs a non-fatal startup warning naming the file, the mode (or
//!   `permissions`) and the entry ([`mode_warn`], reviving ADR-0166's
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
//! - `undo` — per-write file backups behind `/undo`/`/rewind` (#787,
//!   ADR-0212): `enabled`, `max_file_bytes`, `max_root_bytes` in the size
//!   grammar of [`undo`]. Live-reloaded by the definitions watcher; caps
//!   apply to the next write, and disabling keeps existing backups
//!   restorable.
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
use entanglement_core::{PermissionProfile, WebSearchConfig};
use serde_yaml::Value;

use crate::hooks::Hooks;
use crate::mcp::McpServerConfig;
use crate::mode;
use layers::default_layer;
use layers::{discover, merge_value, provenance, RawLayer, CONFIG_FILE_ENV, TEMPLATE_YML};
use raw::RawConfig;

pub mod agent_generation;
pub mod agent_models;
pub mod atomic;
pub mod aux_models;
mod ceiling;
mod ceiling_warn;
pub mod env_file;
pub mod env_key;
pub mod http_auth;
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
mod rhai_trusted;
pub mod state_dir;
pub mod statusline;
pub mod theme;
pub mod tui;
pub mod undo;
mod warn_once;
pub mod write_key;

pub use layers::{scaffold_if_missing, user_config_path, ConfigLayer};
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
mod tests_tui;

const DEFAULTS_YML: &str = include_str!("defaults.yml");

/// The embedded default for [`Config::session_retention_days`]: 30 days. A
/// session log untouched for a month is stale enough to prune without surprising
/// a user who never set the key.
const DEFAULT_SESSION_RETENTION_DAYS: u64 = 30;

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
    /// Hidden credentials for the rhai `http()` binding (ADR-0227) — user
    /// layer only ([`http_auth`]). Empty by default.
    pub http_auth: http_auth::HttpAuth,
    /// Per mode, the `http(...)` allow rules only a project layer added —
    /// they grade, but never unlock a credentialed call in an unattended
    /// mode (ADR-0227).
    pub untrusted_http_rules: http_auth::ProjectHttpRules,
    /// `rhai.trusted` folders by layer, canonical (ADR-0228): a
    /// `rhai(path)` inside one runs with no launch card, force-allowed —
    /// a user-layer one not in a mode the project config prompts in.
    pub rhai_trusted: crate::rhai_trust::TrustedFolders,
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
    /// Terminal-UI preferences (`tui:` block, ADR-0213).
    pub tui: TuiConfig,
    /// Per-write file backups (`undo:` block, #787): on/off and size caps,
    /// handed to [`crate::file_backup::FileBackups`] at boot and re-applied
    /// on a config reload.
    pub undo: crate::file_backup::BackupLimits,
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
    /// Each `rhai.trusted` folder (canonical) with the layer that listed it
    /// first — the lists add up across layers, so there is no single winner.
    pub rhai_trusted: Vec<(std::path::PathBuf, ConfigLayer)>,
}

impl Config {
    /// The embedded defaults alone — no user or project layer read. A base an
    /// embedder (or a test) can override field by field without touching any
    /// file on disk.
    pub fn embedded_defaults() -> Result<Config> {
        Ok(parse(&[default_layer()], None)?.config)
    }

    /// Resolve the user config for `root`: embedded defaults, deep-merged with the
    /// user file (if present) and the repository file (if present). A malformed or
    /// unreadable file in any layer is a loud error, never a silent fallback.
    pub fn load(root: &Path) -> Result<Config> {
        Ok(Self::resolve(root)?.config)
    }

    /// [`load`](Self::load) with the user layer at `user_file` instead of the
    /// `ENTANGLEMENT_CONFIG_FILE`/`${config_dir}` resolution — `None` reads no
    /// user layer. The seam tests (and embedders) use to stay off the real
    /// user config and process env (#773).
    pub fn load_with(root: &Path, user_file: Option<&Path>) -> Result<Config> {
        Ok(Self::resolve_with(root, user_file)?.config)
    }

    /// Like [`load`](Self::load) but also returns the per-field provenance and the
    /// discovered layers, for `skutter inspect config`.
    pub fn resolve(root: &Path) -> Result<Resolved> {
        Self::resolve_with(root, user_config_path().as_deref())
    }

    /// [`resolve`](Self::resolve) with an explicit user-layer path, as
    /// [`load_with`](Self::load_with).
    pub fn resolve_with(root: &Path, user_file: Option<&Path>) -> Result<Resolved> {
        let resolved = parse(&discover(root, user_file)?, Some(root))?;
        // Hidden at load, not at gate construction: onboarding and boot
        // already spawn children (a browser, MCP servers) before the gate
        // exists. Kept out of `parse` so it stays pure.
        crate::secret_env::hide(resolved.config.http_auth.referenced_vars());
        Ok(resolved)
    }
}

/// Merge the layers' docs (later wins) and deserialize into a [`Config`], keeping
/// per-field provenance. The merge and the `deny_unknown_fields` validation both
/// run on the combined [`Value`], so a field override keeps its siblings and a
/// typo in any layer is rejected.
///
/// `root` scopes the load-time warnings' dedupe ([`warn_once`]); `None` for
/// the embedded defaults alone.
fn parse(raw_layers: &[RawLayer], root: Option<&Path>) -> Result<Resolved> {
    let mut raw_layers: Vec<RawLayer> = raw_layers.to_vec();
    let mut warnings = retired::strip_retired_keys(&mut raw_layers);
    warnings.extend(http_auth::strip_project_layer(&mut raw_layers));
    let (trusted_layers, trusted_warnings) = rhai_trusted::extract(&mut raw_layers, root);
    let rhai_trusted = rhai_trusted::folders(&trusted_layers, &raw_layers);
    warnings.extend(trusted_warnings);
    let raw_layers = raw_layers.as_slice();
    let (untrusted_http_rules, http_rule_warnings) = http_auth::project_http_rules(raw_layers);
    warnings.extend(http_rule_warnings);
    ceiling_warn::warn_project_permission_overrides(raw_layers, root);
    mode_warn::warn_stale_rule_names(raw_layers, root);
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
        http_auth: raw.http_auth,
        untrusted_http_rules,
        rhai_trusted,
        web_search: raw.web_search,
        max_turns: raw.max_turns,
        idle_ttl: raw.idle_ttl_secs.map(Duration::from_secs),
        auto_compact: raw.auto_compact,
        editor: raw.editor.filter(|s| !s.trim().is_empty()),
        session_retention_days: raw
            .session_retention_days
            .unwrap_or(DEFAULT_SESSION_RETENTION_DAYS),
        tui: raw.tui,
        undo: raw.undo,
    };
    warnings.extend(config.tui.warnings());
    warnings.extend(config.http_auth.key_warnings());
    Ok(Resolved {
        config,
        layers: raw_layers
            .iter()
            .map(|r| (r.layer, r.source.clone()))
            .collect(),
        provenance: provenance(raw_layers),
        warnings,
        rhai_trusted: trusted_layers,
    })
}
