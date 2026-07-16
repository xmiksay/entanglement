//! Materialize a user-layer override for an agent's `tools`/`disallowed_tools`
//! mask (#330).
//!
//! There is no file to edit for a built-in (`build`/`plan`/`explore`) and
//! hand-authoring a shadowing definition is the only way to change a profile's
//! tool allowlist. The layered loader (embedded < user < project, later wins on
//! `name`, [`super::load_registry`]) already supports shadowing — so an in-app
//! edit just needs to *write* the shadow, not add a new config surface: it lands
//! at `${config_dir}/entanglement/agents/<name>.md`, the native user layer
//! `discover` already reads (or wherever `ENTANGLEMENT_AGENTS_DIR` points, which
//! replaces that whole layer per [`super::AGENTS_DIR_ENV`]).
//!
//! [`rewrite_tools`] is the pure text transform (byte-preserving elsewhere, in
//! the spirit of [`crate::config::env_key::upsert`]): it seeds from the winning
//! definition's *raw* text — a built-in's embedded source, or an existing
//! user/project file's exact text — and rewrites only the `tools:` /
//! `disallowed_tools:` frontmatter keys via a `serde_yaml::Mapping` round-trip
//! (which preserves key order and every untouched key, including nested ones
//! like `permission:`). [`save_tools_override`] wraps it with the I/O: resolve
//! the winning raw text, rewrite it, and write atomically via
//! [`crate::config::atomic::atomic_write`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::atomic::atomic_write;
use crate::frontmatter;

use super::{discover, parse_raw, AGENTS_DIR_ENV};

/// The raw text of the winning definition for `name` — same precedence
/// [`super::load_registry`] applies (built-in < user < project, later wins) — so
/// an override edit starts from what is actually in effect right now: a
/// built-in's embedded source, or an already-materialized user/project file's
/// exact text (comments, key order, everything `parse_raw` doesn't need to
/// touch). `Ok(None)` when no agent named `name` is defined at all.
pub fn winning_raw_text(root: &Path, name: &str) -> Result<Option<String>> {
    let mut winner: Option<String> = None;
    for raw in discover(root)? {
        let Some((def, _body)) = parse_raw(&raw)? else {
            continue;
        };
        if def.name == name {
            winner = Some(raw.content);
        }
    }
    Ok(winner)
}

/// Rewrite only the `tools:` / `disallowed_tools:` frontmatter keys of `raw` —
/// an existing definition's full markdown text — leaving every other key, their
/// order, and the body untouched.
///
/// `allowed: None` means "inherit all": the `tools:` key is dropped entirely.
/// `Some(list)` sets an explicit allowlist (an empty list is a valid "allow
/// nothing" mask, distinct from `None`). `disallowed_tools:` is always dropped —
/// the checklist driving this resolves straight to the final allowed set, so a
/// separate denylist would be redundant and could silently re-subtract from it.
pub fn rewrite_tools(raw: &str, allowed: Option<&[String]>) -> Result<String> {
    let (frontmatter, body) = frontmatter::split(raw)?;
    let mut mapping: serde_yaml::Mapping =
        serde_yaml::from_str(&frontmatter).context("invalid agent frontmatter")?;

    match allowed {
        Some(tools) => {
            let seq = serde_yaml::Value::Sequence(
                tools
                    .iter()
                    .map(|t| serde_yaml::Value::String(t.clone()))
                    .collect(),
            );
            mapping.insert(serde_yaml::Value::String("tools".to_string()), seq);
        }
        None => {
            mapping.remove("tools");
        }
    }
    mapping.remove("disallowed_tools");

    let new_frontmatter = serde_yaml::to_string(&serde_yaml::Value::Mapping(mapping))
        .context("re-serializing agent frontmatter")?;
    Ok(format!("---\n{new_frontmatter}---\n{body}"))
}

/// Resolve the directory a materialized override lands in: the same one
/// [`super::discover`] reads as the native user layer —
/// `ENTANGLEMENT_AGENTS_DIR` if set (which replaces the *whole* user layer with
/// that directory, foreign + native), else
/// `${config_dir}/entanglement/agents`. `None` when neither resolves (no config
/// dir and no override).
fn override_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(AGENTS_DIR_ENV) {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("entanglement").join("agents"))
}

/// Materialize a user-layer override for `name`'s tool allowlist (#330): resolve
/// the winning definition's raw text, rewrite its `tools:`/`disallowed_tools:`
/// keys to `allowed`, and atomically write the result to the native user layer.
/// Returns the written path. A loud error when `name` has no definition at all,
/// or when there is nowhere to write it (set `ENTANGLEMENT_AGENTS_DIR` first).
pub fn save_tools_override(root: &Path, name: &str, allowed: Option<&[String]>) -> Result<PathBuf> {
    let raw = winning_raw_text(root, name)?
        .with_context(|| format!("no agent definition named `{name}`"))?;
    let rewritten = rewrite_tools(&raw, allowed)?;
    let dir = override_dir().context(
        "no config directory for the managed agents dir; set ENTANGLEMENT_AGENTS_DIR to a path first",
    )?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating agents dir {}", dir.display()))?;
    let path = dir.join(format!("{name}.md"));
    atomic_write(&path, &rewritten)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{load_registry, BUILT_INS};
    use crate::skills::SkillRegistry;
    use crate::system_prompt::PromptContext;
    use std::sync::Mutex;

    /// `ENTANGLEMENT_AGENTS_DIR` is process-global; serialize every test that sets
    /// *or* reads it — a reader left unguarded can observe a writer's in-flight
    /// override directory under cargo's parallel test threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn winning_raw_text_seeds_from_built_in_source() {
        // `discover` reads the process-global `ENTANGLEMENT_AGENTS_DIR` env var;
        // take the same lock the mutating tests hold so this can't observe their
        // in-flight override directory (and its written `build.md`) mid-test.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let build_src = BUILT_INS
            .iter()
            .find(|(f, _)| *f == "build.md")
            .map(|(_, c)| *c)
            .unwrap();
        let raw = winning_raw_text(dir.path(), "build").unwrap().unwrap();
        assert_eq!(raw, build_src);
    }

    #[test]
    fn winning_raw_text_is_none_for_unknown_agent() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        assert!(winning_raw_text(dir.path(), "no-such-agent")
            .unwrap()
            .is_none());
    }

    #[test]
    fn rewrite_sets_explicit_allowlist_and_drops_disallowed() {
        let raw = "---\nname: x\ndescription: d\ndisallowed_tools: [bash]\n---\nBody.";
        let rewritten =
            rewrite_tools(raw, Some(&["read".to_string(), "edit".to_string()])).unwrap();
        let (fm, body) = frontmatter::split(&rewritten).unwrap();
        let mapping: serde_yaml::Mapping = serde_yaml::from_str(&fm).unwrap();
        assert!(!mapping.contains_key("disallowed_tools"));
        assert_eq!(
            mapping.get("tools").unwrap(),
            &serde_yaml::Value::Sequence(vec![
                serde_yaml::Value::String("read".into()),
                serde_yaml::Value::String("edit".into()),
            ])
        );
        assert_eq!(body, "Body.");
    }

    #[test]
    fn rewrite_none_drops_tools_key_entirely() {
        let raw = "---\nname: x\ndescription: d\ntools: [read]\n---\nBody.";
        let rewritten = rewrite_tools(raw, None).unwrap();
        let (fm, _) = frontmatter::split(&rewritten).unwrap();
        let mapping: serde_yaml::Mapping = serde_yaml::from_str(&fm).unwrap();
        assert!(!mapping.contains_key("tools"));
    }

    #[test]
    fn rewrite_preserves_unrelated_keys_body_and_order() {
        let raw = "---\nname: x\ndescription: d\nmode: all\npermission:\n  default: ask\n  edit: allow\nskills: [git]\n---\nLine one.\nLine two.";
        let rewritten = rewrite_tools(raw, Some(&["read".to_string()])).unwrap();
        let (fm, body) = frontmatter::split(&rewritten).unwrap();
        let mapping: serde_yaml::Mapping = serde_yaml::from_str(&fm).unwrap();
        assert_eq!(mapping.get("name").unwrap().as_str(), Some("x"));
        assert_eq!(mapping.get("mode").unwrap().as_str(), Some("all"));
        assert_eq!(
            mapping.get("skills").unwrap(),
            &serde_yaml::Value::Sequence(vec![serde_yaml::Value::String("git".into())])
        );
        let permission = mapping.get("permission").unwrap().as_mapping().unwrap();
        assert_eq!(permission.get("default").unwrap().as_str(), Some("ask"));
        assert_eq!(permission.get("edit").unwrap().as_str(), Some("allow"));
        assert_eq!(body, "Line one.\nLine two.");
        // Rewritten frontmatter must still parse cleanly under the strict,
        // deny_unknown_fields agent schema — no stray keys introduced (the
        // preserved `skills: [git]` preload needs a matching registry entry).
        let ctx = PromptContext::default();
        let mut skills = SkillRegistry::default();
        skills.insert(crate::skills::SkillMeta {
            name: "git".to_string(),
            description: "d".to_string(),
            user_only: false,
            allowed_tools: None,
            root_dir: None,
            body: "git body".to_string(),
        });
        crate::agents::parse_definition(&rewritten, &ctx, &skills)
            .expect("rewritten definition must parse");
    }

    #[test]
    fn save_tools_override_writes_user_layer_and_loader_picks_it_up() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let user_dir = tempfile::tempdir().unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        std::env::set_var(AGENTS_DIR_ENV, user_dir.path());

        let path = save_tools_override(
            root_dir.path(),
            "build",
            Some(&["read".to_string(), "grep".to_string()]),
        )
        .unwrap();
        assert_eq!(path, user_dir.path().join("build.md"));
        assert!(path.exists());

        let ctx = PromptContext::default();
        let skills = SkillRegistry::default();
        let reg = load_registry(root_dir.path(), &ctx, &skills).unwrap();
        let build = reg.get("build").expect("build still resolves");
        assert_eq!(
            build.tools.as_deref(),
            Some(&["read".to_string(), "grep".to_string()][..])
        );
        // Every other built-in field survives the round trip unedited.
        assert_eq!(
            build.description,
            "Coding agent — implements changes using the available tools."
        );
        assert!(build.permission.for_tool("edit") == entanglement_core::Permission::Allow);

        std::env::remove_var(AGENTS_DIR_ENV);
    }

    #[test]
    fn save_tools_override_none_reverts_to_inherit_all() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let user_dir = tempfile::tempdir().unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        std::env::set_var(AGENTS_DIR_ENV, user_dir.path());

        save_tools_override(root_dir.path(), "plan", None).unwrap();

        let ctx = PromptContext::default();
        let skills = SkillRegistry::default();
        let reg = load_registry(root_dir.path(), &ctx, &skills).unwrap();
        let plan = reg.get("plan").expect("plan still resolves");
        assert_eq!(plan.tools, None, "an omitted allowlist inherits all");

        std::env::remove_var(AGENTS_DIR_ENV);
    }
}
