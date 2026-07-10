//! File-based agent definitions (#112, ADR-0034).
//!
//! An agent is a markdown file with YAML frontmatter: the frontmatter is the
//! config bundle (`name`/`description`/`mode`/`model`/`permission`/…), the body
//! below the closing `---` is the agent's system prompt. Definitions are
//! discovered at startup and folded into a core [`ProfileRegistry`].
//!
//! # Layers & precedence
//!
//! Three layers, later wins on a `name` collision:
//!
//! 1. **built-in** — embedded [`include_str!`] files ([`build`], [`plan`],
//!    [`explore`]), parsed through the *same* loader. Editing a built-in is just
//!    dropping a same-`name` file in a higher layer; there is no special
//!    "edit built-ins" code path.
//! 2. **user** — `${config_dir}/entanglement/agents/*.md`.
//! 3. **project** — `<root>/.entanglement/agents/*.md`.
//!
//! Same defaults+override shape as the provider catalog (#118): a malformed
//! user/project file is a loud error, never a silent fallback; the embedded
//! built-ins are guarded by a unit test so their parse is provably infallible.
//!
//! # Deferred frontmatter
//!
//! `tools`/`disallowed_tools` (tool mask) and `can_spawn`/`spawnable_agents`
//! (spawn control) are parsed and validated here so a definition file is a
//! stable contract, but their **enforcement** is tracked in follow-up sub-issues
//! of the agents/skills epic (#111) — per-session tool specs (#116/#119) are the
//! prerequisite. They do not yet reach the core [`AgentProfile`].

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use entanglement_core::{AgentMode, AgentProfile, Permission, PermissionProfile, ProfileRegistry};
use serde::Deserialize;

/// Embedded built-in definitions, parsed through the same loader as user/project
/// files. `(filename, contents)` — the filename only feeds parse-error messages;
/// the agent's identity is its frontmatter `name`.
const BUILT_INS: &[(&str, &str)] = &[
    ("build.md", include_str!("build.md")),
    ("plan.md", include_str!("plan.md")),
    ("explore.md", include_str!("explore.md")),
];

/// Env var overriding the user agents directory (tests + non-XDG setups).
const AGENTS_DIR_ENV: &str = "ENTANGLEMENT_AGENTS_DIR";

/// One parsed agent definition (frontmatter + body). The `deny_unknown_fields`
/// makes a typo'd key a loud error rather than a silently-ignored field.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentDefinition {
    /// Unique id; what `agent_spawn { agent }` / `SetAgent` reference.
    name: String,
    /// One-line summary; the only field disclosed to a spawning model.
    description: String,
    /// `primary` / `subagent` / `all`. Defaults to `primary`.
    #[serde(default = "default_mode")]
    mode: AgentMode,
    /// Provider model override, or `inherit` / omitted for the session default.
    #[serde(default)]
    model: Option<String>,
    /// Per-tool `Allow | Ask | Deny` rules. Omitted ⇒ allow-all.
    #[serde(default)]
    permission: Option<serde_yaml::Value>,
    // ── declared here, enforcement deferred (see module docs) ──────────────
    /// Tool allowlist; omitted ⇒ inherit all.
    #[serde(default)]
    #[allow(dead_code)]
    tools: Option<Vec<String>>,
    /// Tool denylist, applied before the allowlist.
    #[serde(default)]
    #[allow(dead_code)]
    disallowed_tools: Vec<String>,
    /// Whether this profile may spawn sub-agents.
    #[serde(default)]
    #[allow(dead_code)]
    can_spawn: Option<bool>,
    /// Which agents this profile may spawn (by name).
    #[serde(default)]
    #[allow(dead_code)]
    spawnable_agents: Option<Vec<String>>,
}

fn default_mode() -> AgentMode {
    AgentMode::Primary
}

/// Load the agent registry for `root`: embedded built-ins, then the user dir,
/// then the project dir — later layers replace earlier ones on a `name`
/// collision (project > user > built-in). A malformed file in any layer aborts.
pub fn load_registry(root: &Path) -> Result<ProfileRegistry> {
    let mut reg = ProfileRegistry::default();
    for (file, contents) in BUILT_INS {
        // Embedded built-ins are guarded by `built_ins_parse`, so a failure here
        // is a build-time bug, not a runtime condition — surface it loudly.
        let profile = parse_definition(contents)
            .with_context(|| format!("parsing built-in agent `{file}`"))?;
        reg.insert(profile);
    }
    if let Some(dir) = user_agents_dir() {
        load_dir(&dir, &mut reg)?;
    }
    load_dir(&root.join(".entanglement").join("agents"), &mut reg)?;
    Ok(reg)
}

/// Parse every `*.md` file in `dir` (if it exists) into `reg`, replacing any
/// same-`name` entry already present. A missing dir is fine (no definitions);
/// an unreadable dir or a malformed file is an error.
fn load_dir(dir: &Path, reg: &mut ProfileRegistry) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("reading agents dir {}", dir.display()))?;
    // Sort for deterministic collision resolution within a single directory.
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md"))
        .collect();
    files.sort();
    for path in files {
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("reading agent definition {}", path.display()))?;
        let profile = parse_definition(&contents)
            .with_context(|| format!("parsing agent definition {}", path.display()))?;
        reg.insert(profile);
    }
    Ok(())
}

/// Split frontmatter from body, parse the frontmatter as YAML, and build a core
/// [`AgentProfile`] (the body becomes its `system_prompt`).
fn parse_definition(content: &str) -> Result<AgentProfile> {
    let (frontmatter, body) = split_frontmatter(content)?;
    let def: AgentDefinition =
        serde_yaml::from_str(&frontmatter).context("invalid agent frontmatter")?;
    if def.name.trim().is_empty() {
        bail!("agent frontmatter `name` must not be empty");
    }
    let permission = match &def.permission {
        Some(v) => permission_from_value(v)?,
        None => PermissionProfile::new(Permission::Allow),
    };
    Ok(AgentProfile {
        name: def.name,
        description: def.description,
        mode: def.mode,
        system_prompt: body.trim().to_string(),
        model: def.model.filter(|m| m != "inherit"),
        permission,
    })
}

/// Split a `---`-delimited YAML frontmatter block from the body below it.
/// Requires the file to open with a `---` line and to carry a closing `---`.
fn split_frontmatter(content: &str) -> Result<(String, String)> {
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        bail!("missing YAML frontmatter: the file must start with a `---` line");
    }
    let mut frontmatter = String::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        frontmatter.push_str(line);
        frontmatter.push('\n');
    }
    if !closed {
        bail!("unterminated frontmatter: missing the closing `---` line");
    }
    let body = lines.collect::<Vec<_>>().join("\n");
    Ok((frontmatter, body))
}

/// Convert a frontmatter `permission` mapping into a core [`PermissionProfile`].
/// Keys are tool patterns (`"*"` or a tool name); the reserved `default` key sets
/// the fallback permission. Rules preserve file order (last match wins,
/// ADR-0003). An omitted `default` ⇒ allow.
fn permission_from_value(value: &serde_yaml::Value) -> Result<PermissionProfile> {
    let map = value
        .as_mapping()
        .context("`permission` must be a mapping of tool → allow|ask|deny")?;
    let mut default = Permission::Allow;
    let mut rules = Vec::new();
    for (key, val) in map {
        let key = key
            .as_str()
            .context("`permission` keys must be strings (a tool name or `*`)")?;
        let perm: Permission = serde_yaml::from_value(val.clone())
            .with_context(|| format!("invalid permission for `{key}` (expected allow|ask|deny)"))?;
        if key == "default" {
            default = perm;
        } else {
            rules.push((key.to_string(), perm));
        }
    }
    Ok(PermissionProfile { rules, default })
}

/// The user agents dir: `${config_dir}/entanglement/agents`, overridable via
/// `ENTANGLEMENT_AGENTS_DIR` (which tests point at a temp dir).
fn user_agents_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(AGENTS_DIR_ENV) {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("entanglement").join("agents"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_ins_parse_with_expected_shape() {
        // The embedded built-ins must parse — this is what lets `load_registry`
        // treat their parse as infallible.
        let mut reg = ProfileRegistry::default();
        for (file, contents) in BUILT_INS {
            let p = parse_definition(contents).unwrap_or_else(|e| panic!("{file}: {e}"));
            reg.insert(p);
        }
        let build = reg.get("build").expect("build built-in");
        assert_eq!(build.mode, AgentMode::Primary);
        assert_eq!(build.permission.for_tool("edit"), Permission::Allow);
        assert!(build.system_prompt.starts_with("You are a coding agent"));

        let plan = reg.get("plan").expect("plan built-in");
        assert_eq!(plan.permission.for_tool("read"), Permission::Allow);
        assert_eq!(plan.permission.for_tool("edit"), Permission::Ask);

        let explore = reg.get("explore").expect("explore built-in");
        assert_eq!(explore.mode, AgentMode::Subagent);
        assert_eq!(explore.permission.for_tool("read"), Permission::Allow);
        assert_eq!(explore.permission.for_tool("edit"), Permission::Deny);
    }

    #[test]
    fn missing_frontmatter_is_an_error() {
        let err = parse_definition("no frontmatter here").unwrap_err();
        assert!(err.to_string().contains("frontmatter"), "got: {err}");
    }

    #[test]
    fn unterminated_frontmatter_is_an_error() {
        let err = parse_definition("---\nname: x\ndescription: y\n").unwrap_err();
        assert!(err.to_string().contains("unterminated"), "got: {err}");
    }

    #[test]
    fn missing_required_field_is_an_error() {
        // `description` is required; the serde detail rides the error's cause chain.
        let err = parse_definition("---\nname: x\n---\nbody").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("description"), "got: {msg}");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err =
            parse_definition("---\nname: x\ndescription: y\ntypo_field: 1\n---\nbody").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("typo_field"), "got: {msg}");
    }

    #[test]
    fn bad_yaml_is_an_error() {
        let err = parse_definition("---\nname: [unclosed\n---\nbody").unwrap_err();
        assert!(err.to_string().contains("frontmatter"), "got: {err}");
    }

    #[test]
    fn body_becomes_system_prompt_and_model_inherit_is_none() {
        let p =
            parse_definition("---\nname: x\ndescription: d\nmodel: inherit\n---\nDo the thing.\n")
                .unwrap();
        assert_eq!(p.system_prompt, "Do the thing.");
        assert_eq!(p.model, None);
        // Omitted permission ⇒ allow-all.
        assert_eq!(p.permission.for_tool("edit"), Permission::Allow);
    }

    #[test]
    fn explicit_model_is_kept() {
        let p =
            parse_definition("---\nname: x\ndescription: d\nmodel: glm-4.7\n---\nbody").unwrap();
        assert_eq!(p.model.as_deref(), Some("glm-4.7"));
    }

    #[test]
    fn mode_all_and_deferred_fields_parse() {
        let p = parse_definition(
            "---\nname: x\ndescription: d\nmode: all\ntools: [read, grep]\n\
             disallowed_tools: [bash]\ncan_spawn: true\nspawnable_agents: [explore]\n---\nbody",
        )
        .unwrap();
        assert_eq!(p.mode, AgentMode::All);
    }
}
