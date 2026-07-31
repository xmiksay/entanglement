//! Resolving a `propose_plan` call's `content`/`path` input to a file (#513,
//! ADR-0145) — split out of `propose_plan.rs` to stay under the 400-line file
//! cap. See that module's doc comment for the feature overview; this one owns
//! parsing, materializing/binding the file, and the staleness guard.

use std::path::Path;

use entanglement_core::SessionId;
use sha2::{Digest, Sha256};

use crate::permission_path;
use crate::plan_files::PlanFileRegistry;

/// Root-relative folder a `content` submission materializes a plan file into.
pub(super) const PLANS_DIR: &str = ".entanglement/plans";

/// One `propose_plan` call's parsed input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PlanInput {
    /// Inline plan markdown to materialize as a fresh (or overwritten) file.
    Content(String),
    /// A path to an existing plan file to submit as-is.
    Path(String),
}

/// Parse a `propose_plan` tool input into exactly one of `content`/`path`. A
/// malformed or bare-string input degrades to inline `content` (mirrors
/// `ask_user`'s tolerance for a scripted backend). A structured object naming
/// both or neither field is a validation error.
pub(super) fn parse_plan_input(input: &str) -> Result<PlanInput, String> {
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(serde_json::Value::Object(map)) => {
            let content = map
                .get("content")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let path = map.get("path").and_then(|v| v.as_str()).map(str::to_string);
            match (content, path) {
                (Some(c), None) => Ok(PlanInput::Content(c)),
                (None, Some(p)) => Ok(PlanInput::Path(p)),
                (Some(_), Some(_)) => Err(
                    "propose_plan requires exactly one of `content` or `path`, not both"
                        .to_string(),
                ),
                (None, None) => {
                    Err("propose_plan requires exactly one of `content` or `path`".to_string())
                }
            }
        }
        _ => Ok(PlanInput::Content(input.to_string())),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// A resolved plan: its markdown content plus the root-relative file location
/// it now lives at.
#[derive(Debug)]
pub(super) struct PlanResolution {
    pub(super) content: String,
    pub(super) rel_path: String,
}

/// Root-relative, lexically clean, and inside `root` — rejects an absolute
/// out-of-root path (`permission_path::rooted_arg` leaves those verbatim) and a
/// relative path that walks above root via leading `..` components.
fn escapes_root(rel: &str) -> bool {
    Path::new(rel).is_absolute() || rel == ".." || rel.starts_with("../")
}

/// Human-scale session label used for a materialized plan file's name: the
/// first 8 chars of the session id (mirrors `tui::format::short_id`,
/// duplicated rather than shared since that helper is TUI-feature-gated and
/// this module must work in a lean, TUI-less build).
fn short_session_id(session: &SessionId) -> String {
    session.to_string().chars().take(8).collect()
}

/// Resolve a parsed [`PlanInput`] against the project `root`: materialize a
/// fresh file for `Content`, or bind to (and staleness-check) an existing one
/// for `Path`. Records the session's new knowledge of the file into
/// `plan_files` on success. Returns a user-facing error string on any failure.
pub(super) fn resolve_plan(
    root: &Path,
    session: &SessionId,
    plan_files: &PlanFileRegistry,
    input: PlanInput,
) -> Result<PlanResolution, String> {
    match input {
        PlanInput::Content(content) => {
            let dir = root.join(PLANS_DIR);
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("failed to create the plans folder: {e}"))?;
            let rel_path = format!("{PLANS_DIR}/{}.md", short_session_id(session));
            let abs_path = root.join(&rel_path);
            std::fs::write(&abs_path, &content)
                .map_err(|e| format!("failed to write the plan file `{rel_path}`: {e}"))?;
            let hash = sha256_hex(content.as_bytes());
            plan_files.record(session.clone(), rel_path.clone(), hash);
            Ok(PlanResolution { content, rel_path })
        }
        PlanInput::Path(raw_path) => {
            let rel_path = permission_path::rooted_arg(root, "write", &raw_path);
            if !rel_path.ends_with(".md") {
                return Err(format!(
                    "propose_plan `path` must name a `.md` file: `{raw_path}`"
                ));
            }
            if escapes_root(&rel_path) {
                return Err(format!(
                    "propose_plan `path` must be inside the project root: `{raw_path}`"
                ));
            }
            let abs_path = root.join(&rel_path);
            if !abs_path.is_file() {
                return Err(format!(
                    "plan file not found: `{rel_path}` — write it first (with `write`) or pass \
                     `content` instead"
                ));
            }
            let content = std::fs::read_to_string(&abs_path)
                .map_err(|e| format!("failed to read the plan file `{rel_path}`: {e}"))?;
            let hash = sha256_hex(content.as_bytes());
            if let Some(binding) = plan_files.get(session) {
                if binding.rel_path == rel_path && binding.hash != hash {
                    return Err(format!(
                        "the plan file `{rel_path}` has changed since you last read or edited \
                         it — re-read it (with `read`) before calling propose_plan again"
                    ));
                }
            }
            plan_files.record(session.clone(), rel_path.clone(), hash);
            Ok(PlanResolution { content, rel_path })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("entanglement-propose-plan-unit-")
            .tempdir()
            .unwrap()
    }

    #[test]
    fn parse_plan_input_reads_content() {
        assert_eq!(
            parse_plan_input(r##"{"content":"# Do X\n1. step"}"##).unwrap(),
            PlanInput::Content("# Do X\n1. step".to_string())
        );
    }

    #[test]
    fn parse_plan_input_reads_path() {
        assert_eq!(
            parse_plan_input(r#"{"path":".entanglement/plans/x.md"}"#).unwrap(),
            PlanInput::Path(".entanglement/plans/x.md".to_string())
        );
    }

    #[test]
    fn parse_plan_input_degrades_bare_string_to_content() {
        assert_eq!(
            parse_plan_input("just a plan").unwrap(),
            PlanInput::Content("just a plan".to_string())
        );
    }

    #[test]
    fn parse_plan_input_rejects_both() {
        let err = parse_plan_input(r#"{"content":"a","path":"b.md"}"#).unwrap_err();
        assert!(err.contains("exactly one"));
    }

    #[test]
    fn parse_plan_input_rejects_neither() {
        let err = parse_plan_input(r#"{}"#).unwrap_err();
        assert!(err.contains("exactly one"));
    }

    #[test]
    fn resolve_plan_materializes_content_under_the_plans_folder() {
        let dir = tempdir();
        let root = dir.path();
        let plan_files = PlanFileRegistry::new();
        let session = SessionId::new("s1");
        let res = resolve_plan(
            root,
            &session,
            &plan_files,
            PlanInput::Content("# Plan\n- [ ] a".to_string()),
        )
        .unwrap();
        assert_eq!(res.rel_path, format!("{PLANS_DIR}/{}.md", "s1"));
        assert_eq!(
            std::fs::read_to_string(root.join(&res.rel_path)).unwrap(),
            "# Plan\n- [ ] a"
        );
        let binding = plan_files.get(&session).unwrap();
        assert_eq!(binding.rel_path, res.rel_path);
    }

    #[test]
    fn resolve_plan_path_requires_md_extension() {
        let dir = tempdir();
        let root = dir.path();
        let plan_files = PlanFileRegistry::new();
        let err = resolve_plan(
            root,
            &SessionId::new("s1"),
            &plan_files,
            PlanInput::Path("notes.txt".to_string()),
        )
        .unwrap_err();
        assert!(err.contains(".md"));
    }

    #[test]
    fn resolve_plan_path_requires_the_file_to_exist() {
        let dir = tempdir();
        let root = dir.path();
        let plan_files = PlanFileRegistry::new();
        let err = resolve_plan(
            root,
            &SessionId::new("s1"),
            &plan_files,
            PlanInput::Path(".entanglement/plans/missing.md".to_string()),
        )
        .unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn resolve_plan_path_rejects_an_out_of_root_escape() {
        let dir = tempdir();
        let root = dir.path();
        let plan_files = PlanFileRegistry::new();
        let err = resolve_plan(
            root,
            &SessionId::new("s1"),
            &plan_files,
            PlanInput::Path("../outside.md".to_string()),
        )
        .unwrap_err();
        assert!(err.contains("project root"));
    }

    #[test]
    fn resolve_plan_path_binds_a_pre_existing_file_with_no_staleness_check() {
        let dir = tempdir();
        let root = dir.path();
        std::fs::create_dir_all(root.join(PLANS_DIR)).unwrap();
        std::fs::write(root.join(PLANS_DIR).join("seed.md"), "# Seeded").unwrap();
        let plan_files = PlanFileRegistry::new();
        let res = resolve_plan(
            root,
            &SessionId::new("s1"),
            &plan_files,
            PlanInput::Path(format!("{PLANS_DIR}/seed.md")),
        )
        .unwrap();
        assert_eq!(res.content, "# Seeded");
    }

    #[test]
    fn resolve_plan_path_refuses_a_file_changed_out_of_band() {
        let dir = tempdir();
        let root = dir.path();
        let plan_files = PlanFileRegistry::new();
        let session = SessionId::new("s1");
        // First bind via content.
        let res = resolve_plan(
            root,
            &session,
            &plan_files,
            PlanInput::Content("# v1".to_string()),
        )
        .unwrap();
        // The "user" edits the file directly, bypassing any tool the runtime
        // would have seen execute (no FileChange, so plan_files stays stale).
        std::fs::write(root.join(&res.rel_path), "# v2 (user edit)").unwrap();
        let err = resolve_plan(
            root,
            &session,
            &plan_files,
            PlanInput::Path(res.rel_path.clone()),
        )
        .unwrap_err();
        assert!(err.contains("changed since"), "{err}");
    }

    #[test]
    fn resolve_plan_path_accepts_a_file_the_registry_already_knows_the_new_hash_of() {
        // Mirrors an in-session `write`/`edit` on the bound file: the
        // `FileChange` listener already refreshed the registry's hash, so
        // resolve_plan sees no staleness even though the file changed.
        let dir = tempdir();
        let root = dir.path();
        let plan_files = PlanFileRegistry::new();
        let session = SessionId::new("s1");
        let res = resolve_plan(
            root,
            &session,
            &plan_files,
            PlanInput::Content("# v1".to_string()),
        )
        .unwrap();
        let new_content = "# v2 (agent edit)";
        std::fs::write(root.join(&res.rel_path), new_content).unwrap();
        plan_files.note_file_change(&session, &res.rel_path, &sha256_hex(new_content.as_bytes()));
        let res2 = resolve_plan(
            root,
            &session,
            &plan_files,
            PlanInput::Path(res.rel_path.clone()),
        )
        .unwrap();
        assert_eq!(res2.content, new_content);
    }

    #[test]
    fn resolve_plan_path_first_bind_never_stale() {
        // A session's very first touch of a pre-existing file has no prior
        // binding to compare against, so it's accepted regardless of history.
        let dir = tempdir();
        let root = dir.path();
        std::fs::create_dir_all(root.join(PLANS_DIR)).unwrap();
        std::fs::write(root.join(PLANS_DIR).join("seed.md"), "# v3").unwrap();
        let plan_files = PlanFileRegistry::new();
        let res = resolve_plan(
            root,
            &SessionId::new("fresh"),
            &plan_files,
            PlanInput::Path(format!("{PLANS_DIR}/seed.md")),
        )
        .unwrap();
        assert_eq!(res.content, "# v3");
    }
}
