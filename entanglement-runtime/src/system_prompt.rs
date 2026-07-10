//! Deterministic system-prompt assembly (#113, epic #111).
//!
//! The system prompt a session sends to the model is **composed**, not a single
//! opaque string on the agent definition. [`assemble`] folds up to five parts in
//! a fixed order — each optional, none model-guessed:
//!
//! 1. **shared preamble** — invariants every agent must honour (safety, output,
//!    tool-use). Claude Code does *not* apply a shared preamble to subagents
//!    automatically; we make it an explicit opt-in so shared rules are never
//!    silently dropped when an agent supplies its own body.
//! 2. **agent body** — the markdown body of the agent definition.
//! 3. **project brief** — a project-instructions file (`.entanglement/BRIEF.md`
//!    or `AGENTS.md`), included only when the agent definition sets
//!    `include_brief: true`.
//! 4. **environment block** — cwd/root, platform, date; *generated* here so the
//!    model never has to guess them.
//! 5. **skill index** — tier-1 disclosure lines (`name` + `description` only)
//!    generated from the skill registry, never hand-written into an agent body.
//!
//! A **subagent** (`AgentMode::Subagent`) gets `preamble + body (+ brief)` only —
//! the env block and skill index are omitted, and it never inherits the parent's
//! assembled prompt (each agent is composed independently from its own body and
//! its own `include_brief` flag).
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
#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    pub preamble: Option<String>,
    pub brief: Option<String>,
    pub env: Option<EnvBlock>,
    pub skills: Vec<SkillDisclosure>,
}

impl PromptContext {
    /// Discover the composition inputs for `root`: the shared preamble (built-in
    /// default unless a file overrides it), the project brief (if a brief file
    /// exists), and the generated environment block. The skill index stays empty
    /// until the skill registry lands (#115); once it does, filter by the agent's
    /// tool mask (#116) before handing the list here.
    pub fn load(root: &Path) -> Self {
        Self {
            preamble: load_preamble(root),
            brief: load_brief(root),
            env: Some(EnvBlock::detect(root)),
            skills: Vec::new(),
        }
    }
}

/// Compose one agent's system prompt deterministically (#113).
///
/// Order: preamble, body, brief (only if `include_brief`), env, skills — each
/// emitted only when present/non-empty, joined by blank lines. A `Subagent`
/// agent gets `preamble + body (+ brief)` only; the env block and skill index
/// are reserved for primary/`all` sessions.
pub fn assemble(body: &str, include_brief: bool, mode: AgentMode, ctx: &PromptContext) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(preamble) = non_empty(ctx.preamble.as_deref()) {
        parts.push(preamble.to_string());
    }
    if let Some(body) = non_empty(Some(body)) {
        parts.push(body.to_string());
    }
    if include_brief {
        if let Some(brief) = non_empty(ctx.brief.as_deref()) {
            parts.push(brief.to_string());
        }
    }
    // Subagents are composed from their own body only (#113): no env, no skill
    // index, and never the parent's assembled prompt.
    if mode != AgentMode::Subagent {
        if let Some(env) = &ctx.env {
            parts.push(env.render());
        }
        if !ctx.skills.is_empty() {
            parts.push(render_skills(&ctx.skills));
        }
    }

    parts.join("\n\n")
}

/// Render the tier-1 skill index: a header plus one `name: description` line per
/// skill. Only the two disclosed fields appear — no bodies, no tool lists.
fn render_skills(skills: &[SkillDisclosure]) -> String {
    let mut out = String::from("Available skills (load with the `load_skill` tool before use):");
    for s in skills {
        out.push_str(&format!("\n- {}: {}", s.name, s.description));
    }
    out
}

/// `Some(trimmed)` if the input is present and non-empty after trimming.
fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|t| !t.is_empty())
}

/// Resolve the shared preamble: `ENTANGLEMENT_PREAMBLE_FILE`, then a project
/// `<root>/.entanglement/preamble.md`, then the user
/// `${config_dir}/entanglement/preamble.md`, else the built-in default. A file
/// that exists but is empty disables the preamble (`None`).
fn load_preamble(root: &Path) -> Option<String> {
    if let Some(path) = std::env::var_os(PREAMBLE_FILE_ENV) {
        return read_non_empty(Path::new(&path));
    }
    let project = root.join(".entanglement").join("preamble.md");
    if project.exists() {
        return read_non_empty(&project);
    }
    if let Some(user) = dirs::config_dir().map(|d| d.join("entanglement").join("preamble.md")) {
        if user.exists() {
            return read_non_empty(&user);
        }
    }
    Some(DEFAULT_PREAMBLE.to_string())
}

/// Resolve the project brief: `ENTANGLEMENT_BRIEF_FILE`, then
/// `<root>/.entanglement/BRIEF.md`, then `<root>/AGENTS.md`. Missing ⇒ `None`
/// (the `include_brief` flag then becomes a no-op).
fn load_brief(root: &Path) -> Option<String> {
    if let Some(path) = std::env::var_os(BRIEF_FILE_ENV) {
        return read_non_empty(Path::new(&path));
    }
    for candidate in [
        root.join(".entanglement").join("BRIEF.md"),
        root.join("AGENTS.md"),
    ] {
        if candidate.exists() {
            return read_non_empty(&candidate);
        }
    }
    None
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
        }
    }

    #[test]
    fn primary_assembly_is_ordered_preamble_body_brief_env_skills() {
        let out = assemble("BODY", true, AgentMode::Primary, &ctx_full());
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
        assert!(!assemble("BODY", true, AgentMode::Primary, &ctx).contains("PREAMBLE"));

        // Brief present but flag off ⇒ omitted.
        let out = assemble("BODY", false, AgentMode::Primary, &ctx_full());
        assert!(
            !out.contains("BRIEF"),
            "brief must be gated by include_brief:\n{out}"
        );

        // No env.
        let ctx = PromptContext {
            env: None,
            ..ctx_full()
        };
        assert!(!assemble("BODY", true, AgentMode::Primary, &ctx).contains("<env>"));

        // No skills.
        let ctx = PromptContext {
            skills: vec![],
            ..ctx_full()
        };
        assert!(!assemble("BODY", true, AgentMode::Primary, &ctx).contains("Available skills"));
    }

    #[test]
    fn subagent_gets_preamble_body_brief_but_not_env_or_skills() {
        let out = assemble("BODY", true, AgentMode::Subagent, &ctx_full());
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
        let out = assemble("BODY", false, AgentMode::All, &ctx_full());
        assert!(out.contains("<env>"));
        assert!(out.contains("Available skills"));
    }

    #[test]
    fn empty_context_is_identity_on_the_trimmed_body() {
        let out = assemble(
            "  BODY  ",
            true,
            AgentMode::Primary,
            &PromptContext::default(),
        );
        assert_eq!(out, "BODY");
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // 1704067200 / 86400
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
