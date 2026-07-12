//! Deterministic system-prompt assembly (#113, epic #111).
//!
//! The system prompt a session sends to the model is **composed**, not a single
//! opaque string on the agent definition. [`assemble`] folds up to six parts in
//! a fixed order — each optional, none model-guessed:
//!
//! 1. **shared preamble** — invariants every agent must honour (safety, output,
//!    tool-use). Claude Code does *not* apply a shared preamble to subagents
//!    automatically; we make it an explicit opt-in so shared rules are never
//!    silently dropped when an agent supplies its own body.
//! 2. **agent body** — the markdown body of the agent definition.
//! 3. **project brief** — a standard project-instructions file (`AGENTS.md` /
//!    `.agents/AGENTS.md`, or Anthropic's `.claude/CLAUDE.md` / `CLAUDE.md`),
//!    included only when the agent definition sets `include_brief: true`. We
//!    read whatever the ecosystem already puts in the repo — no bespoke file.
//! 4. **environment block** — cwd/root, platform, date; *generated* here so the
//!    model never has to guess them.
//! 5. **skill index** — tier-1 disclosure lines (`name` + `description` only)
//!    generated from the skill registry, never hand-written into an agent body.
//! 6. **preloaded skill bodies** — full bodies of the skills an agent names in its
//!    `skills:` frontmatter (#117), resolved through the same substitution
//!    pipeline as `load_skill`. Preload only, never an allowlist; runtime skill
//!    *access* is the orthogonal `load_skill` tool mask.
//!
//! A **subagent** (`AgentMode::Subagent`) gets `preamble + body (+ brief)` plus
//! any preloaded bodies — the env block and tier-1 skill index are omitted (but
//! preload is not, being author-requested), and it never inherits the parent's
//! assembled prompt (each agent is composed independently from its own body and
//! its own `include_brief`/`skills` frontmatter).
//!
//! Composition is a pure function so it is unit-testable with no model in the
//! loop. The runtime bakes the assembled prompt into each [`AgentProfile`] at
//! load time (see [`crate::agents::load_registry`]); core stays a pass-through
//! that ships `system_prompt` verbatim as `LlmRequest.system`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use entanglement_core::AgentMode;

/// Env var pointing at an explicit shared-preamble file (overrides discovery).
const PREAMBLE_FILE_ENV: &str = "ENTANGLEMENT_PREAMBLE_FILE";
/// Env var pointing at an explicit project-brief file (overrides discovery).
const BRIEF_FILE_ENV: &str = "ENTANGLEMENT_BRIEF_FILE";

/// Built-in shared preamble applied to every agent unless a file overrides it.
/// Deliberately terse: invariants only, no task specifics.
const DEFAULT_PREAMBLE: &str = "\
You are an AI coding agent that acts through a fixed set of tools. These rules \
hold for every task, always:
- Safety: tool calls are real side effects (files, shell, network). Never take a \
destructive or irreversible action unless explicitly asked to.
- Tool use: prefer the provided tools over guessing; read before you edit and \
verify before you report success.
- Output: be concise and direct. State what you did, what failed, and what \
remains — never claim a result you have not verified.";

/// A tier-1 skill disclosure: the two fields exposed to the model (`name` +
/// one-line `description`). Generated from the skill registry, never authored
/// into an agent body (#113). Filtering by tool mask / `user_only` is the
/// caller's job (#115/#116); [`assemble`] renders whatever list it is handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDisclosure {
    pub name: String,
    pub description: String,
}

/// Generated environment facts, rendered into the `<env>` block. Never
/// model-guessed: the harness fills these in at composition time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvBlock {
    pub root: PathBuf,
    pub platform: String,
    pub date: String,
}

impl EnvBlock {
    /// Snapshot the environment for `root`: platform from the compile target,
    /// date from the wall clock (UTC, `YYYY-MM-DD`).
    pub fn detect(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            platform: std::env::consts::OS.to_string(),
            date: today_utc(),
        }
    }

    fn render(&self) -> String {
        format!(
            "<env>\nWorking directory: {}\nPlatform: {}\nDate: {}\n</env>",
            self.root.display(),
            self.platform,
            self.date,
        )
    }
}

/// Shared inputs for [`assemble`], loaded once at startup. Each part is optional;
/// the [`Default`] value is an identity composition (`assemble` returns just the
/// trimmed body) which the parsing tests rely on.
///
/// `preamble_source`/`brief_path` carry *where* the corresponding content came
/// from, for `skutter inspect prompt --parts` and the load-time `debug!` — never
/// consumed by composition itself. Each is `Some` only when the matching content
/// is present (an empty override file yields no content and no source).
#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    pub preamble: Option<String>,
    pub brief: Option<String>,
    pub env: Option<EnvBlock>,
    pub skills: Vec<SkillDisclosure>,
    /// File the shared preamble was read from; `None` ⇒ the built-in default.
    pub preamble_source: Option<PathBuf>,
    /// File the project brief was read from; `None` ⇒ no brief was found.
    pub brief_path: Option<PathBuf>,
}

/// One composed slice of the system prompt, with the source it came from — the
/// structured view behind `skutter inspect prompt --parts`. [`assemble`] joins
/// these `content`s; [`assemble_parts`] hands the same slices back for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptPart {
    /// Human label for the slice (`preamble`, `agent body`, …).
    pub label: &'static str,
    /// Where it came from (a file path, `built-in default`, `generated`, …).
    pub source: String,
    /// The rendered text of this slice.
    pub content: String,
}

impl PromptContext {
    /// Discover the composition inputs for `root`: the shared preamble (built-in
    /// default unless a file overrides it), the project brief (if a brief file
    /// exists), and the generated environment block. The skill index is left
    /// empty here; the head populates it from
    /// [`crate::skills::SkillRegistry::disclosures`] (#114) before handing the
    /// context to the agent loader. Per-agent tool-mask filtering of that list is
    /// deferred (#116).
    pub fn load(root: &Path) -> Self {
        let preamble = load_preamble();
        let brief = load_brief(root);
        // Tie each source to actual presence: an override file that read empty
        // yields no content, so it should not claim to be the source either.
        let preamble_source = preamble
            .as_ref()
            .and_then(|_| std::env::var_os(PREAMBLE_FILE_ENV).map(PathBuf::from));
        let brief_path = brief.as_ref().and_then(|_| brief_file(root));
        Self {
            preamble,
            brief,
            env: Some(EnvBlock::detect(root)),
            skills: Vec::new(),
            preamble_source,
            brief_path,
        }
    }
}

/// Compose one agent's system prompt deterministically (#113).
///
/// Order: preamble, body, brief (only if `include_brief`), env, tier-1 skill
/// index, preloaded skill bodies — each emitted only when present/non-empty,
/// joined by blank lines. A `Subagent` agent gets `preamble + body (+ brief)`
/// only for the env block and tier-1 index, which are reserved for primary/`all`
/// sessions.
///
/// `preloaded` (#117) are full skill bodies resolved from the agent definition's
/// `skills:` frontmatter (same substitution pipeline as `load_skill`). Preload is
/// a distinct mechanism from the tier-1 index and from runtime access (the
/// `load_skill` tool mask): it is **mode-independent** — a spawned subagent that
/// preloads a skill gets its body even though the tier-1 index is withheld — and
/// **additive**, never an allowlist, so the index still discloses every other
/// skill.
pub fn assemble(
    body: &str,
    include_brief: bool,
    mode: AgentMode,
    ctx: &PromptContext,
    preloaded: &[String],
) -> String {
    assemble_parts(body, include_brief, mode, ctx, preloaded)
        .into_iter()
        .map(|p| p.content)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The same composition as [`assemble`], but returning each included slice with
/// its source instead of one joined string — the structured view behind
/// `skutter inspect prompt --parts`. Only the parts that make it into the final
/// prompt are returned, in prompt order, so `assemble` is exactly the join of
/// these `content`s and the two can never drift.
///
/// The `agent body` part is labelled with a generic source here; a caller that
/// knows the winning definition file (`inspect`) overwrites it with that path.
pub fn assemble_parts(
    body: &str,
    include_brief: bool,
    mode: AgentMode,
    ctx: &PromptContext,
    preloaded: &[String],
) -> Vec<PromptPart> {
    let mut parts: Vec<PromptPart> = Vec::new();

    if let Some(preamble) = non_empty(ctx.preamble.as_deref()) {
        let source = ctx
            .preamble_source
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "built-in default".to_string());
        parts.push(PromptPart {
            label: "preamble",
            source,
            content: preamble.to_string(),
        });
    }
    if let Some(body) = non_empty(Some(body)) {
        parts.push(PromptPart {
            label: "agent body",
            source: "agent definition".to_string(),
            content: body.to_string(),
        });
    }
    if include_brief {
        if let Some(brief) = non_empty(ctx.brief.as_deref()) {
            let source = ctx
                .brief_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "discovered project brief".to_string());
            parts.push(PromptPart {
                label: "project brief",
                source,
                content: brief.to_string(),
            });
        }
    }
    // Subagents are composed from their own body only (#113): no env, no skill
    // index, and never the parent's assembled prompt.
    if mode != AgentMode::Subagent {
        if let Some(env) = &ctx.env {
            parts.push(PromptPart {
                label: "environment",
                source: "generated".to_string(),
                content: env.render(),
            });
        }
        if !ctx.skills.is_empty() {
            parts.push(PromptPart {
                label: "skill index",
                source: format!(
                    "skill registry ({} skill{})",
                    ctx.skills.len(),
                    if ctx.skills.len() == 1 { "" } else { "s" }
                ),
                content: render_skills(&ctx.skills),
            });
        }
    }
    // Preload is author-requested (#117), so it is not gated by mode — the
    // subagent spawn case is precisely what it is for.
    if !preloaded.is_empty() {
        parts.push(PromptPart {
            label: "preloaded skills",
            source: "skills: frontmatter".to_string(),
            content: render_preloaded(preloaded),
        });
    }

    parts
}

/// Render the tier-1 skill index: a header plus one `name: description` line per
/// skill. Only the two disclosed fields appear — no bodies, no tool lists.
pub(crate) fn render_skills(skills: &[SkillDisclosure]) -> String {
    let mut out = String::from("Available skills (load with the `load_skill` tool before use):");
    for s in skills {
        out.push_str(&format!("\n- {}: {}", s.name, s.description));
    }
    out
}

/// Render the preloaded-skill section (#117): a header explaining these bodies
/// are already loaded (so the model need not call `load_skill` for them), then
/// each rendered skill body separated by blank lines.
fn render_preloaded(bodies: &[String]) -> String {
    let mut out = String::from(
        "Preloaded skills — the full instructions below are already loaded; you do \
         not need to call `load_skill` for them:",
    );
    for body in bodies {
        out.push_str("\n\n");
        out.push_str(body.trim());
    }
    out
}

/// `Some(trimmed)` if the input is present and non-empty after trimming.
fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|t| !t.is_empty())
}

/// Resolve the shared preamble: an explicit `ENTANGLEMENT_PREAMBLE_FILE`, else
/// the built-in default. Unlike the brief there is no cross-vendor file
/// convention for a shared preamble — it is a harness invariant, not a
/// project-authored doc — so it ships embedded and is only overridable by the
/// env var (an empty override file disables it, `None`).
fn load_preamble() -> Option<String> {
    if let Some(path) = std::env::var_os(PREAMBLE_FILE_ENV) {
        return read_non_empty(Path::new(&path));
    }
    Some(DEFAULT_PREAMBLE.to_string())
}

/// Standard project-instruction files, in precedence order (first found wins):
/// the cross-vendor `AGENTS.md` convention (root + `.agents/`) then Anthropic's
/// `CLAUDE.md` (`.claude/CLAUDE.md` preferred over the repo-root `CLAUDE.md`, per
/// the workspace rule). No bespoke `.entanglement/BRIEF.md` — the brief is
/// whatever the ecosystem already puts in the repo.
const BRIEF_FILES: &[&str] = &[
    "AGENTS.md",
    ".agents/AGENTS.md",
    ".claude/CLAUDE.md",
    "CLAUDE.md",
];

/// Resolve the project brief: an explicit `ENTANGLEMENT_BRIEF_FILE`, else the
/// first existing [`BRIEF_FILES`] entry under `root`. Missing ⇒ `None` (the
/// `include_brief` flag then becomes a no-op).
fn load_brief(root: &Path) -> Option<String> {
    if let Some(path) = std::env::var_os(BRIEF_FILE_ENV) {
        return read_non_empty(Path::new(&path));
    }
    for rel in BRIEF_FILES {
        let candidate = root.join(rel);
        if candidate.exists() {
            return read_non_empty(&candidate);
        }
    }
    None
}

/// The brief file [`load_brief`] would pick for `root` (env override, else the
/// first existing [`BRIEF_FILES`] entry), for source reporting — mirrors the
/// selection precedence without reading the file.
fn brief_file(root: &Path) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(BRIEF_FILE_ENV) {
        return Some(PathBuf::from(path));
    }
    BRIEF_FILES
        .iter()
        .map(|rel| root.join(rel))
        .find(|c| c.exists())
}

/// Read a file, returning `Some(trimmed)` if it is readable and non-empty. An
/// unreadable path is a warning, not a hard failure — composition degrades to
/// dropping that part rather than aborting startup.
fn read_non_empty(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let trimmed = contents.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Err(e) => {
            tracing::warn!("could not read {}: {e}", path.display());
            None
        }
    }
}

/// Current UTC date as `YYYY-MM-DD`. Uses the wall clock; a system clock before
/// the epoch degrades to `1970-01-01` rather than panicking.
fn today_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Convert a count of days since 1970-01-01 into `(year, month, day)`.
/// Howard Hinnant's `civil_from_days` — exact, no leap-second/timezone handling
/// (UTC calendar date), and no external date crate.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_full() -> PromptContext {
        PromptContext {
            preamble: Some("PREAMBLE".into()),
            brief: Some("BRIEF".into()),
            env: Some(EnvBlock {
                root: PathBuf::from("/work"),
                platform: "linux".into(),
                date: "2026-07-10".into(),
            }),
            skills: vec![
                SkillDisclosure {
                    name: "git".into(),
                    description: "commit and branch helpers".into(),
                },
                SkillDisclosure {
                    name: "review".into(),
                    description: "review a diff".into(),
                },
            ],
            preamble_source: None,
            brief_path: None,
        }
    }

    #[test]
    fn primary_assembly_is_ordered_preamble_body_brief_env_skills() {
        let out = assemble("BODY", true, AgentMode::Primary, &ctx_full(), &[]);
        let p = out.find("PREAMBLE").unwrap();
        let b = out.find("BODY").unwrap();
        let br = out.find("BRIEF").unwrap();
        let env = out.find("<env>").unwrap();
        let sk = out.find("Available skills").unwrap();
        assert!(
            p < b && b < br && br < env && env < sk,
            "order wrong:\n{out}"
        );
        assert!(out.contains("Working directory: /work"));
        assert!(out.contains("- git: commit and branch helpers"));
    }

    #[test]
    fn each_part_is_individually_toggled() {
        // No preamble.
        let ctx = PromptContext {
            preamble: None,
            ..ctx_full()
        };
        assert!(!assemble("BODY", true, AgentMode::Primary, &ctx, &[]).contains("PREAMBLE"));

        // Brief present but flag off ⇒ omitted.
        let out = assemble("BODY", false, AgentMode::Primary, &ctx_full(), &[]);
        assert!(
            !out.contains("BRIEF"),
            "brief must be gated by include_brief:\n{out}"
        );

        // No env.
        let ctx = PromptContext {
            env: None,
            ..ctx_full()
        };
        assert!(!assemble("BODY", true, AgentMode::Primary, &ctx, &[]).contains("<env>"));

        // No skills.
        let ctx = PromptContext {
            skills: vec![],
            ..ctx_full()
        };
        assert!(!assemble("BODY", true, AgentMode::Primary, &ctx, &[]).contains("Available skills"));
    }

    #[test]
    fn subagent_gets_preamble_body_brief_but_not_env_or_skills() {
        let out = assemble("BODY", true, AgentMode::Subagent, &ctx_full(), &[]);
        assert!(out.contains("PREAMBLE"));
        assert!(out.contains("BODY"));
        assert!(out.contains("BRIEF"));
        assert!(
            !out.contains("<env>"),
            "subagent must not get the env block:\n{out}"
        );
        assert!(
            !out.contains("Available skills"),
            "subagent must not get the skill index:\n{out}"
        );
    }

    #[test]
    fn all_mode_is_composed_like_a_primary() {
        let out = assemble("BODY", false, AgentMode::All, &ctx_full(), &[]);
        assert!(out.contains("<env>"));
        assert!(out.contains("Available skills"));
    }

    #[test]
    fn preloaded_bodies_render_after_the_skill_index_and_survive_subagent_mode() {
        // Preload (#117) is mode-independent: a subagent (which drops env + the
        // tier-1 index) still gets the preloaded body — the spawn case it is for.
        let sub = assemble(
            "BODY",
            false,
            AgentMode::Subagent,
            &ctx_full(),
            &["SKILL_BODY".into()],
        );
        assert!(!sub.contains("<env>"));
        assert!(!sub.contains("Available skills"));
        assert!(sub.contains("Preloaded skills"), "{sub}");
        assert!(sub.contains("SKILL_BODY"), "{sub}");

        // For a primary the tier-1 index still renders (preload is additive, not
        // an allowlist) and the preloaded body comes after it.
        let prim = assemble(
            "BODY",
            false,
            AgentMode::Primary,
            &ctx_full(),
            &["SKILL_BODY".into()],
        );
        let idx = prim.find("Available skills").unwrap();
        let pre = prim.find("Preloaded skills").unwrap();
        assert!(
            idx < pre,
            "preloaded body must follow the tier-1 index:\n{prim}"
        );
    }

    #[test]
    fn empty_preload_adds_no_section() {
        let out = assemble("BODY", false, AgentMode::Primary, &ctx_full(), &[]);
        assert!(!out.contains("Preloaded skills"), "{out}");
    }

    #[test]
    fn empty_context_is_identity_on_the_trimmed_body() {
        let out = assemble(
            "  BODY  ",
            true,
            AgentMode::Primary,
            &PromptContext::default(),
            &[],
        );
        assert_eq!(out, "BODY");
    }

    #[test]
    fn brief_discovers_standard_files_with_agents_md_first() {
        // Only Anthropic's `.claude/CLAUDE.md` present → found.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(root.join(".claude/CLAUDE.md"), "claude brief").unwrap();
        assert_eq!(load_brief(root).as_deref(), Some("claude brief"));

        // The cross-vendor `AGENTS.md` wins over `CLAUDE.md`.
        std::fs::write(root.join("AGENTS.md"), "agents brief").unwrap();
        assert_eq!(load_brief(root).as_deref(), Some("agents brief"));

        // No standard file ⇒ no brief (the include_brief flag is then a no-op).
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(load_brief(empty.path()), None);
    }

    #[test]
    fn assemble_parts_is_the_join_behind_assemble_with_sources() {
        let ctx = PromptContext {
            brief_path: Some(PathBuf::from("/work/CLAUDE.md")),
            ..ctx_full()
        };
        let parts = assemble_parts("BODY", true, AgentMode::Primary, &ctx, &["PRE".into()]);
        // Joining the parts reproduces `assemble` exactly (no drift).
        let joined = parts
            .iter()
            .map(|p| p.content.clone())
            .collect::<Vec<_>>()
            .join("\n\n");
        assert_eq!(
            joined,
            assemble("BODY", true, AgentMode::Primary, &ctx, &["PRE".into()])
        );
        // Sources are annotated: built-in preamble, brief path, generated env.
        let src = |label| {
            parts
                .iter()
                .find(|p| p.label == label)
                .map(|p| p.source.as_str())
        };
        assert_eq!(src("preamble"), Some("built-in default"));
        assert_eq!(src("project brief"), Some("/work/CLAUDE.md"));
        assert_eq!(src("environment"), Some("generated"));
        assert_eq!(src("preloaded skills"), Some("skills: frontmatter"));
    }

    #[test]
    fn assemble_parts_labels_preamble_override_file() {
        let ctx = PromptContext {
            preamble_source: Some(PathBuf::from("/etc/preamble.md")),
            ..ctx_full()
        };
        let parts = assemble_parts("BODY", false, AgentMode::Primary, &ctx, &[]);
        let preamble = parts.iter().find(|p| p.label == "preamble").unwrap();
        assert_eq!(preamble.source, "/etc/preamble.md");
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // 1704067200 / 86400
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
