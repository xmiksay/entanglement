//! Composes the ADR-0196 §5 `ToolSearch`-mode prompt slimming with the
//! existing env-date freshness patch ([`crate::env_date`]) into the single
//! `SystemPromptResolver` slot `EngineConfig` exposes — only one may be
//! wired at a time, so this is where the two per-session prompt inputs
//! (tool-advertising mode, calendar date) fold together.
//!
//! `Full` mode is untouched (today's composed prompt, date-patched exactly
//! as before this change). `ToolSearch` mode drops the tier-1 skill-index
//! section — the roster that would otherwise list every skill by name,
//! busting nothing on its own but redundant once `explore` already answers
//! "what's searchable" — for one line naming the searchable categories
//! (`tool_search.md` §3's "load-bearing" mitigation: without it, the model
//! has no signal that anything beyond its kernel is reachable at all).

use std::sync::Arc;

use entanglement_core::{SessionId, SystemPromptResolver, ToolAdvertising};

use crate::env_date::refresh_env_date;
use crate::tool_advertising::AdvertisingState;

/// The tier-1 skill-index header `system_prompt::render_skills` emits —
/// matched here to strip that whole section out of a baked prompt.
const SKILL_INDEX_HEADER: &str = "Available skills (load with the `load_skill` tool before use):";

/// The one-line pointer replacing the dropped rosters in `ToolSearch` mode
/// (ADR-0196 §5).
const TOOL_SEARCH_NOTE: &str = "Beyond the tools above, more tools are available — use explore \
     to search them and describe to load one: file/exec built-ins, MCP servers, skills.";

/// Remove the tier-1 skill-index part from a baked, `assemble`-joined prompt.
/// Sections are joined by `"\n\n"` (`system_prompt::assemble`), and no other
/// section's content collides with the skill-index header, so splitting on
/// that separator and dropping the one matching piece is lossless for every
/// other part — `split("\n\n").join("\n\n")` is the identity function, and
/// this only ever removes the one piece that starts with the header.
fn strip_skill_index(prompt: &str) -> String {
    prompt
        .split("\n\n")
        .filter(|part| !part.starts_with(SKILL_INDEX_HEADER))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `ToolSearch` mode's rendering: strip the skill index, append the one-line
/// category pointer.
fn render_tool_search(prompt: &str) -> String {
    let stripped = strip_skill_index(prompt);
    if stripped.is_empty() {
        TOOL_SEARCH_NOTE.to_string()
    } else {
        format!("{stripped}\n\n{TOOL_SEARCH_NOTE}")
    }
}

/// Build the combined resolver: date-refresh always applies; the
/// `ToolSearch` transform layers on top for a session pinned to that mode.
/// `advertising` is the same `Arc` the tool executor and `tool_spec_resolver`
/// share (#560, ADR-0196 §2-3), so all three agree on a session's mode.
pub fn resolver(advertising: Arc<AdvertisingState>) -> SystemPromptResolver {
    Arc::new(move |session: &SessionId, profile| {
        let today = crate::date::today_utc();
        let date_fixed = refresh_env_date(&profile.system_prompt, &today);
        match advertising.mode(session) {
            ToolAdvertising::Full => date_fixed,
            ToolAdvertising::ToolSearch => {
                let base = date_fixed.as_deref().unwrap_or(&profile.system_prompt);
                Some(render_tool_search(base))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_skill_index_removes_only_that_section() {
        let prompt = format!(
            "PREAMBLE\n\nBODY\n\n<env>\nDate: x\n</env>\n\n{SKILL_INDEX_HEADER}\n- git: helpers"
        );
        let out = strip_skill_index(&prompt);
        assert!(!out.contains(SKILL_INDEX_HEADER));
        assert!(!out.contains("- git: helpers"));
        assert!(out.contains("PREAMBLE"));
        assert!(out.contains("BODY"));
        assert!(out.contains("<env>"));
    }

    #[test]
    fn strip_skill_index_is_a_no_op_without_the_section() {
        let prompt = "PREAMBLE\n\nBODY";
        assert_eq!(strip_skill_index(prompt), prompt);
    }

    #[test]
    fn render_tool_search_appends_the_category_note() {
        let out = render_tool_search("BODY");
        assert!(out.starts_with("BODY"));
        assert!(out.contains("use explore to search them"));
    }

    #[test]
    fn render_tool_search_on_an_empty_prompt_is_just_the_note() {
        assert_eq!(render_tool_search(""), TOOL_SEARCH_NOTE);
    }

    #[test]
    fn full_mode_keeps_the_skill_index_and_only_the_date_ever_changes() {
        use entanglement_core::{AgentMode, AgentProfile, Permission, PermissionProfile};

        let advertising = Arc::new(AdvertisingState::new());
        let session = SessionId::new("s");
        advertising
            .modes
            .lock()
            .unwrap()
            .pin(session.clone(), ToolAdvertising::Full);
        let today = crate::date::today_utc();
        let profile = AgentProfile {
            name: "build".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: format!(
                "<env>\nDate: {today}\n</env>\n\n{SKILL_INDEX_HEADER}\n- git: x"
            ),
            model: None,
            provider: None,
            permission: PermissionProfile::new(Permission::Allow),
            tools: None,
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        };
        let resolve = resolver(advertising);
        // Same date, Full mode ⇒ falls back to the unmodified baked prompt.
        assert_eq!(resolve(&session, &profile), None);
    }

    #[test]
    fn tool_search_mode_always_returns_the_slimmed_prompt() {
        use entanglement_core::{AgentMode, AgentProfile, Permission, PermissionProfile};

        let advertising = Arc::new(AdvertisingState::new());
        let session = SessionId::new("s");
        advertising
            .modes
            .lock()
            .unwrap()
            .pin(session.clone(), ToolAdvertising::ToolSearch);
        let today = crate::date::today_utc();
        let profile = AgentProfile {
            name: "build".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: format!(
                "<env>\nDate: {today}\n</env>\n\n{SKILL_INDEX_HEADER}\n- git: x"
            ),
            model: None,
            provider: None,
            permission: PermissionProfile::new(Permission::Allow),
            tools: None,
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        };
        let resolve = resolver(advertising);
        let out = resolve(&session, &profile).expect("ToolSearch mode always returns Some");
        assert!(!out.contains(SKILL_INDEX_HEADER));
        assert!(out.contains("use explore to search them"));
    }
}
