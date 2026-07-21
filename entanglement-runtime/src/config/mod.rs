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
//! - `permissions` — the first section: tool name → `allow | ask | deny`, a
//!   global ceiling combined least-privilege with each agent profile (see
//!   [`crate::permission::clamp_to_base`]). Argument/path patterns (#173) build on
//!   it. It is a pure *ceiling*; the orthogonal "always allow" grants (#174) that
//!   *raise* an `Ask` live in a separate managed file ([`crate::grants`]), not here.
//! - `hooks` — lifecycle hooks (#199, ADR-0066): external commands run around
//!   tool execution (`pre_`/`post_tool_use`) and on prompt ingress
//!   (`user_prompt_submit`). See [`crate::hooks`]. Empty by default.
//! - general settings — `agent` / `provider` / `model` / `verbose` / `max_turns`
//!   / `idle_ttl_secs`. Each is a *fallback*: an explicit CLI flag or
//!   environment variable wins over the file (env > config > embedded
//!   default). `idle_ttl_secs` (#401, ADR-0090) maps onto
//!   `EngineConfig::idle_ttl`; `None` (the default) leaves auto-hibernation
//!   off, exactly as before this setting existed.
//!
//! # First-run scaffold (#219)
//!
//! [`scaffold_if_missing`] drops a fully-commented starter template
//! (`template.yml`) at the user path when none exists, so the config dir is a
//! discoverable starting point rather than empty. Because every setting is
//! commented out the file parses to `Null` and is skipped in the merge
//! ([`read_layer`]) — a no-op until a user uncomments a key.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use entanglement_core::{Permission, PermissionProfile, WebSearchConfig};
use serde::Deserialize;
use serde_yaml::Value;

use crate::agents::permission_from_value;
use crate::hooks::Hooks;
use crate::mcp::McpServerConfig;

pub mod agent_generation;
pub mod agent_models;
pub mod atomic;
pub mod env_file;
pub mod env_key;
pub mod lock;
pub mod mcp_persist;

pub use mcp_persist::save_mcp;

/// The CLI `skutter config set-key` handler (rpassword prompt + catalog lookup).
/// Behind `cli`+`provider`: it prompts (rpassword, a `cli`-feature dep) and looks
/// the provider's key env up in the catalog (`provider` feature).
#[cfg(all(feature = "cli", feature = "provider"))]
pub mod keys;

#[cfg(test)]
mod tests;

const DEFAULTS_YML: &str = include_str!("defaults.yml");

/// The commented starter file written on first run (#219). Distinct from
/// [`DEFAULTS_YML`]: every setting here is commented *out*, so an untouched
/// scaffold is a pure no-op that never pins a default — it only exists to be a
/// discoverable, editable starting point.
const TEMPLATE_YML: &str = include_str!("template.yml");

/// Env var overriding the user config file path (tests + non-XDG setups).
const CONFIG_FILE_ENV: &str = "ENTANGLEMENT_CONFIG_FILE";

/// Guards mutation of `ENTANGLEMENT_CONFIG_FILE` — process-global env state —
/// across every test in this crate that points the layered config loader (or
/// [`mcp_persist::save_mcp`]'s surgical `mcp:` writer) at a temp file. Shared by
/// `config::tests` and `config::mcp_persist::tests` (not module-local to
/// either) so the two suites can't race on the same var when `cargo test` runs
/// them in parallel threads.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The raw file shape. `deny_unknown_fields` makes a typo'd key a loud error
/// rather than a silently-ignored setting, exactly like the agent/provider files.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    verbose: bool,
    /// Parsed into a [`PermissionProfile`] via the same reader agent frontmatter
    /// uses ([`permission_from_value`]).
    #[serde(default)]
    permissions: Option<Value>,
    /// Lifecycle hooks (#199): external commands run around tool execution and on
    /// prompt ingress. Deserializes straight into [`Hooks`] (plain serde, unlike
    /// `permissions`); absent ⇒ no hooks.
    #[serde(default)]
    hooks: Hooks,
    /// External MCP tool servers (#198): a map of server name → spawn config. Each
    /// server's tools are discovered and registered into the runtime tool
    /// registry. Absent ⇒ no servers.
    #[serde(default)]
    mcp: HashMap<String, McpServerConfig>,
    /// Provider-side web search (#305, ADR-0075): opt-in, bound onto the LLM
    /// client at build time — never seen by core. Absent ⇒ disabled. Enabling
    /// it is consent (the server tool runs provider-side, *outside* the runtime
    /// permission ladder).
    #[serde(default)]
    web_search: WebSearchConfig,
    /// Cap on the inner LLM→tool loop within a single turn (#177). Default 200.
    #[serde(default)]
    max_turns: Option<usize>,
    /// Auto-hibernate a settled root session (and its spawn sub-tree) after this
    /// many idle seconds (#401, ADR-0090/[ADR-0105]). Absent ⇒ `None` ⇒ the
    /// engine default: no sweep, eviction stays embedder-driven.
    ///
    /// [ADR-0105]: ../../../docs/adr/0105-expose-idle-ttl-via-runtime-config.md
    #[serde(default)]
    idle_ttl_secs: Option<u64>,
    /// Editor command for the TUI `$EDITOR` round-trip (`/editor`). When set it
    /// **wins over** `$VISUAL`/`$EDITOR`, so the persisted choice is the default
    /// regardless of the shell env; absent ⇒ fall back to `$VISUAL` → `$EDITOR`
    /// → `vi`. Word-split like a shell command, so `"code --wait"` works.
    #[serde(default)]
    editor: Option<String>,
}

/// Resolved user configuration — the merged, validated values every head reads.
#[derive(Debug, Clone)]
pub struct Config {
    /// Default agent profile when the CLI passes none.
    pub agent: Option<String>,
    /// Provider name (like `ENTANGLEMENT_PROVIDER`); `None` ⇒ auto-detect.
    pub provider: Option<String>,
    /// Model id override; `None` ⇒ the provider's catalog default.
    pub model: Option<String>,
    /// Log at `debug` by default (like `--verbose`).
    pub verbose: bool,
    /// The global permission ceiling. Allow-all by default (a no-op).
    pub permissions: PermissionProfile,
    /// Lifecycle hooks (#199). Empty by default (a no-op).
    pub hooks: Hooks,
    /// External MCP tool servers (#198). Empty by default (a no-op).
    pub mcp: HashMap<String, McpServerConfig>,
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
    /// Editor command for the TUI `$EDITOR` round-trip; `Some` wins over
    /// `$VISUAL`/`$EDITOR`. `None` ⇒ resolve from env then `vi`.
    pub editor: Option<String>,
}

/// Which of the three precedence layers a value came from. Ordered low → high so
/// `Default < User < Project` matches discovery order and the later-wins rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfigLayer {
    Default,
    User,
    Project,
}

impl ConfigLayer {
    /// Short label for `skutter inspect config`.
    pub fn label(self) -> &'static str {
        match self {
            ConfigLayer::Default => "default",
            ConfigLayer::User => "user",
            ConfigLayer::Project => "project",
        }
    }
}

/// One discovered config layer *before* merging: its precedence, a display label
/// for its origin (`built-in (defaults.yml)` or a file path), and the parsed doc.
struct RawLayer {
    layer: ConfigLayer,
    source: String,
    doc: Value,
}

/// The resolved config plus the provenance `skutter inspect config` reports:
/// which layer last set each field, and every layer that was present.
pub struct Resolved {
    pub config: Config,
    /// `(layer, source)` for every discovered layer, in precedence order.
    pub layers: Vec<(ConfigLayer, String)>,
    /// The winning layer for each top-level key that any layer defined.
    pub provenance: Vec<(String, ConfigLayer)>,
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

/// Enumerate the present config layers in precedence order — embedded defaults,
/// then the user file, then the project file. Missing files are skipped; an
/// unreadable or malformed file is an error.
fn discover(root: &Path) -> Result<Vec<RawLayer>> {
    let mut layers = vec![default_layer()];
    if let Some(path) = user_config_path() {
        read_layer(ConfigLayer::User, &path, &mut layers)?;
    }
    read_layer(
        ConfigLayer::Project,
        &root.join(".entanglement").join("config.yml"),
        &mut layers,
    )?;
    Ok(layers)
}

/// The always-present embedded default layer.
fn default_layer() -> RawLayer {
    RawLayer {
        layer: ConfigLayer::Default,
        source: "built-in (defaults.yml)".to_string(),
        doc: serde_yaml::from_str(DEFAULTS_YML)
            .expect("embedded defaults.yml is valid — guarded by test"),
    }
}

/// Read + parse the file at `path` (if it exists) as one layer. A missing file is
/// fine; an unreadable file or invalid YAML is a loud error.
fn read_layer(layer: ConfigLayer, path: &Path, layers: &mut Vec<RawLayer>) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading user config {}", path.display()))?;
    let doc: Value = serde_yaml::from_str(&text)
        .with_context(|| format!("parsing user config {}", path.display()))?;
    // A comment-only or empty file parses to `Null`; it sets nothing, so skip it
    // rather than let it wipe the lower layers in the merge (the scaffolded
    // template, #219, is exactly this until a user uncomments a key).
    if doc.is_null() {
        return Ok(());
    }
    layers.push(RawLayer {
        layer,
        source: path.display().to_string(),
        doc,
    });
    Ok(())
}

/// Merge the layers' docs (later wins) and deserialize into a [`Config`], keeping
/// per-field provenance. The merge and the `deny_unknown_fields` validation both
/// run on the combined [`Value`], so a field override keeps its siblings and a
/// typo in any layer is rejected.
fn parse(raw_layers: &[RawLayer]) -> Result<Resolved> {
    let mut merged = Value::Null;
    for rl in raw_layers {
        merged = merge_value(merged, rl.doc.clone());
    }
    let raw: RawConfig = serde_yaml::from_value(merged).context("validating merged user config")?;
    // The ceiling's `permission_from_value` needs the same MCP capability
    // index (#426) `agents::load_registry` uses, built from this same `mcp:`
    // section — `read: allow` in the ceiling should cover an annotated MCP
    // tool exactly like it does in agent frontmatter.
    let mcp_capabilities =
        crate::mcp::capability_index(&raw.mcp).context("in user config `mcp` capabilities")?;
    let permissions = match &raw.permissions {
        Some(v) => {
            permission_from_value(v, &mcp_capabilities).context("in user config `permissions`")?
        }
        None => PermissionProfile::new(Permission::Allow),
    };
    let config = Config {
        agent: raw.agent,
        provider: raw.provider,
        model: raw.model,
        verbose: raw.verbose,
        permissions,
        hooks: raw.hooks,
        mcp: raw.mcp,
        web_search: raw.web_search,
        max_turns: raw.max_turns,
        idle_ttl: raw.idle_ttl_secs.map(Duration::from_secs),
        editor: raw.editor.filter(|s| !s.trim().is_empty()),
    };
    Ok(Resolved {
        config,
        layers: raw_layers
            .iter()
            .map(|r| (r.layer, r.source.clone()))
            .collect(),
        provenance: provenance(raw_layers),
    })
}

/// The winning layer for each top-level key any layer set, in a stable key order.
fn provenance(raw_layers: &[RawLayer]) -> Vec<(String, ConfigLayer)> {
    const KEYS: &[&str] = &[
        "agent",
        "provider",
        "model",
        "verbose",
        "permissions",
        "hooks",
        "mcp",
        "web_search",
        "max_turns",
        "idle_ttl_secs",
        "editor",
    ];
    KEYS.iter()
        .filter_map(|key| {
            // Highest layer that carries this key wins (layers are low→high).
            raw_layers
                .iter()
                .rev()
                .find(|rl| rl.doc.get(*key).is_some())
                .map(|rl| (key.to_string(), rl.layer))
        })
        .collect()
}

/// Deep-merge `over` onto `base`: mappings merge key-wise recursively; scalars
/// and sequences are replaced by `over`. (The config file has no identity-keyed
/// sequences, so the catalog's by-`name`/`id` sequence merge isn't needed.)
fn merge_value(base: Value, over: Value) -> Value {
    match (base, over) {
        (Value::Mapping(mut base_map), Value::Mapping(over_map)) => {
            for (key, over_val) in over_map {
                let merged = match base_map.remove(&key) {
                    Some(base_val) => merge_value(base_val, over_val),
                    None => over_val,
                };
                base_map.insert(key, merged);
            }
            Value::Mapping(base_map)
        }
        (_, over) => over,
    }
}

/// The user config path: `${config_dir}/entanglement/config.yml`, overridable via
/// `ENTANGLEMENT_CONFIG_FILE` (which tests point at a temp file).
fn user_config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(CONFIG_FILE_ENV) {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("entanglement").join("config.yml"))
}

/// First-run scaffold (#219): if the user config file does not exist yet, write a
/// commented starter template ([`TEMPLATE_YML`]) so `${config_dir}/entanglement/`
/// is a discoverable, editable starting point instead of an empty directory.
///
/// Best-effort and non-authoritative: the template is fully commented, so it
/// changes nothing until a user edits it — the embedded defaults still drive
/// behavior either way. Returns the written path on success, `None` if a file was
/// already present (or the config dir is unknown). Callers treat a write error as
/// non-fatal; startup must not fail because the home directory is read-only.
pub fn scaffold_if_missing() -> Result<Option<PathBuf>> {
    let Some(path) = user_config_path() else {
        return Ok(None);
    };
    if path.exists() {
        return Ok(None);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    std::fs::write(&path, TEMPLATE_YML)
        .with_context(|| format!("writing default config {}", path.display()))?;
    Ok(Some(path))
}
