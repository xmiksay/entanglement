//! Composes the ADR-0196 §5 `ToolSearch`-mode prompt slimming with the
//! per-session env-date pin ([`crate::env_date`], ADR-0202 §5) into the
//! single `SystemPromptResolver` slot `EngineConfig` exposes — only one may
//! be wired at a time, so this is where the two per-session prompt inputs
//! (tool-advertising mode, pinned calendar date) fold together.
//!
//! `Full` mode is the composed prompt with only its `Date:` line patched to
//! the session's pinned date. `ToolSearch` mode drops the tier-1 skill-index
//! section — the roster that would otherwise list every skill by name,
//! busting nothing on its own but redundant once `explore` already answers
//! "what's searchable" — for one line naming the searchable categories
//! (`tool_search.md` §3's "load-bearing" mitigation: without it, the model
//! has no signal that anything beyond its kernel is reachable at all).

use std::sync::Arc;

use entanglement_core::{
    AgentProfile, Discovery, SessionId, SystemPromptResolver, ToolAdvertising,
};

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

/// The pointer plus, for a session whose tools array never grows (ADR-0204),
/// how a loaded tool is called. Pinned with the strategy, so byte-stable per
/// session.
fn tool_search_note(discovery: Discovery) -> String {
    match discovery {
        Discovery::Append => TOOL_SEARCH_NOTE.to_string(),
        Discovery::NativeFirst => format!(
            "{TOOL_SEARCH_NOTE} Call a loaded tool directly by its name; only if you cannot, \
             call invoke {{\"name\": \"<tool>\", \"args\": {{...}}}}."
        ),
        Discovery::Invoke => format!(
            "{TOOL_SEARCH_NOTE} Call a loaded tool through invoke \
             {{\"name\": \"<tool>\", \"args\": {{...}}}}; it cannot be called directly."
        ),
    }
}

/// `ToolSearch` mode's rendering: strip the skill index, append the one-line
/// category pointer.
fn render_tool_search(prompt: &str, discovery: Discovery) -> String {
    let stripped = strip_skill_index(prompt);
    let note = tool_search_note(discovery);
    if stripped.is_empty() {
        note
    } else {
        format!("{stripped}\n\n{note}")
    }
}

/// Build the combined resolver: the pinned-date patch always applies; the
/// `ToolSearch` transform layers on top for a session pinned to that mode.
/// `advertising` is the same `Arc` the tool executor and `tool_spec_resolver`
/// share (#560, ADR-0196 §2-3), so all three agree on a session's mode — and
/// the executor's session-end arm forgets the date pin held there.
pub fn resolver(advertising: Arc<AdvertisingState>) -> SystemPromptResolver {
    Arc::new(move |session: &SessionId, profile| {
        resolve(&advertising, session, profile, &crate::date::today_utc())
    })
}

/// The resolver body with `today` injected, so a date rollover is testable.
fn resolve(
    advertising: &AdvertisingState,
    session: &SessionId,
    profile: &AgentProfile,
    today: &str,
) -> Option<String> {
    let date = advertising
        .env_dates
        .lock()
        .expect("env-date pin mutex poisoned")
        .pin(session, today);
    let date_fixed = refresh_env_date(&profile.system_prompt, &date);
    match advertising.mode(session) {
        ToolAdvertising::Full => date_fixed,
        ToolAdvertising::ToolSearch => {
            let base = date_fixed.as_deref().unwrap_or(&profile.system_prompt);
            Some(render_tool_search(base, advertising.discovery(session)))
        }
    }
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
        let out = render_tool_search("BODY", Discovery::Append);
        assert!(out.starts_with("BODY"));
        assert!(out.ends_with(TOOL_SEARCH_NOTE));
    }

    #[test]
    fn fixed_array_strategies_say_how_a_loaded_tool_is_called() {
        let nf = render_tool_search("BODY", Discovery::NativeFirst);
        assert!(nf.contains("use explore to search them"), "{nf}");
        assert!(nf.ends_with(
            r#"Call a loaded tool directly by its name; only if you cannot, call invoke {"name": "<tool>", "args": {...}}."#
        ), "{nf}");
        let inv = render_tool_search("BODY", Discovery::Invoke);
        assert!(inv.ends_with(
            r#"Call a loaded tool through invoke {"name": "<tool>", "args": {...}}; it cannot be called directly."#
        ), "{inv}");
    }

    #[test]
    fn render_tool_search_on_an_empty_prompt_is_just_the_note() {
        assert_eq!(render_tool_search("", Discovery::Append), TOOL_SEARCH_NOTE);
    }

    #[test]
    fn full_mode_keeps_the_skill_index_and_only_the_date_ever_changes() {
        use entanglement_core::{AgentMode, AgentProfile};

        let advertising = Arc::new(AdvertisingState::new());
        let session = SessionId::new("s");
        advertising.modes.lock().unwrap().pin(
            session.clone(),
            ToolAdvertising::Full,
            crate::tool_advertising::Encoding::ClientSide,
        );
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
        use entanglement_core::{AgentMode, AgentProfile};

        let advertising = Arc::new(AdvertisingState::new());
        let session = SessionId::new("s");
        advertising.modes.lock().unwrap().pin(
            session.clone(),
            ToolAdvertising::ToolSearch,
            crate::tool_advertising::Encoding::ClientSide,
        );
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
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        };
        let resolve = resolver(advertising);
        let out = resolve(&session, &profile).expect("ToolSearch mode always returns Some");
        assert!(!out.contains(SKILL_INDEX_HEADER));
        assert!(out.contains("use explore to search them"));
    }

    fn full_profile(date: &str) -> AgentProfile {
        use entanglement_core::AgentMode;
        AgentProfile {
            name: "build".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: format!("<env>\nDate: {date}\n</env>"),
            model: None,
            provider: None,
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        }
    }

    fn pin_full(advertising: &AdvertisingState, session: &SessionId) {
        advertising.modes.lock().unwrap().pin(
            session.clone(),
            ToolAdvertising::Full,
            crate::tool_advertising::Encoding::ClientSide,
        );
    }

    #[test]
    fn a_live_repin_changes_the_note_on_the_next_resolution() {
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");
        advertising.modes.lock().unwrap().pin(
            session.clone(),
            ToolAdvertising::ToolSearch,
            crate::tool_advertising::Encoding::ClientSide,
        );
        let profile = full_profile("2026-09-15");
        let before = resolve(&advertising, &session, &profile, "2026-09-15").unwrap();
        assert!(before.ends_with(TOOL_SEARCH_NOTE), "{before}");
        advertising.repin(&session, None, Some(Discovery::Invoke));
        let after = resolve(&advertising, &session, &profile, "2026-09-15").unwrap();
        assert!(after.ends_with("it cannot be called directly."), "{after}");
        advertising.repin(&session, Some(ToolAdvertising::Full), None);
        assert_eq!(
            resolve(&advertising, &session, &profile, "2026-09-15"),
            None
        );
    }

    #[test]
    fn a_session_keeps_its_start_date_across_midnight_even_after_a_rebake() {
        let advertising = AdvertisingState::new();
        let session = SessionId::new("s");
        pin_full(&advertising, &session);
        let day1 = full_profile("2026-09-15");
        assert_eq!(resolve(&advertising, &session, &day1, "2026-09-15"), None);
        // Midnight passes: the baked prompt is unchanged, so still no rewrite.
        assert_eq!(resolve(&advertising, &session, &day1, "2026-09-16"), None);
        // A definitions reload re-baked the new date: patched back to the pin
        // so the cached system block stays byte-identical.
        let rebaked = full_profile("2026-09-16");
        assert_eq!(
            resolve(&advertising, &session, &rebaked, "2026-09-16"),
            Some(day1.system_prompt.clone())
        );
    }

    #[test]
    fn a_new_session_gets_the_new_date_and_a_forgotten_one_re_pins() {
        let advertising = AdvertisingState::new();
        let old = SessionId::new("old");
        let new = SessionId::new("new");
        pin_full(&advertising, &old);
        pin_full(&advertising, &new);
        let day1 = full_profile("2026-09-15");
        resolve(&advertising, &old, &day1, "2026-09-15");
        let patched = resolve(&advertising, &new, &day1, "2026-09-16");
        assert_eq!(patched.as_deref(), Some("<env>\nDate: 2026-09-16\n</env>"));

        advertising.env_dates.lock().unwrap().forget(&old);
        let repinned = resolve(&advertising, &old, &day1, "2026-09-16");
        assert_eq!(repinned.as_deref(), Some("<env>\nDate: 2026-09-16\n</env>"));
    }
}
