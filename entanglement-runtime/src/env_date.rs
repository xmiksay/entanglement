//! Keeps the `<env>` block's baked date accurate across a long-lived process
//! (#566, rider to the prompt-caching audit).
//!
//! [`system_prompt::EnvBlock`][crate::system_prompt::EnvBlock] is generated
//! once, at process start, and baked verbatim into every
//! [`AgentProfile::system_prompt`][entanglement_core::AgentProfile] — the
//! system block otherwise stays byte-stable across rounds, which is exactly
//! what makes it cacheable, but it also means a process that outlives
//! midnight UTC keeps sending a stale `Date:` line until an unrelated
//! definitions reload or a restart happens to re-bake it. [`date_resolver`]
//! fixes that without giving up the byte-stability: wired as the engine's
//! [`SystemPromptResolver`][entanglement_core::SystemPromptResolver], it is
//! consulted once per turn and only produces a *different* string on the one
//! turn where the calendar date has actually rolled over — every other turn
//! it returns `None`, so the engine falls back to the same baked (and
//! therefore still cached) prompt.

/// Patch a baked system prompt's `<env>` date line to `today`. Returns `None`
/// — falling back to the unmodified baked prompt — when there's no `<env>`
/// block (a subagent's prompt omits it) or the date already matches, so the
/// prompt stays byte-identical for as long as it's accurate.
fn refresh_env_date(system_prompt: &str, today: &str) -> Option<String> {
    let marker = "\nDate: ";
    let start = system_prompt.find(marker)? + marker.len();
    let end = system_prompt[start..]
        .find('\n')
        .map(|i| start + i)
        .unwrap_or(system_prompt.len());
    if &system_prompt[start..end] == today {
        return None;
    }
    let mut out = String::with_capacity(system_prompt.len());
    out.push_str(&system_prompt[..start]);
    out.push_str(today);
    out.push_str(&system_prompt[end..]);
    Some(out)
}

/// Builds the [`entanglement_core::SystemPromptResolver`] the runtime wires
/// onto `EngineConfig` so every turn re-checks the env-block date. Cheap
/// (string search, no I/O), so there's no reason to gate it to session start
/// only — a session that spans midnight UTC picks up the new date on its very
/// next turn instead of waiting for a restart.
pub fn date_resolver() -> entanglement_core::SystemPromptResolver {
    std::sync::Arc::new(|_session, profile| {
        refresh_env_date(&profile.system_prompt, &crate::date::today_utc())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_date_returns_none() {
        let prompt = "before\n<env>\nWorking directory: /work\nPlatform: linux\nDate: 2026-08-02\n</env>\nafter";
        assert_eq!(refresh_env_date(prompt, "2026-08-02"), None);
    }

    #[test]
    fn different_date_patches_only_the_date_line() {
        let prompt = "before\n<env>\nWorking directory: /work\nPlatform: linux\nDate: 2026-08-02\n</env>\nafter";
        let out = refresh_env_date(prompt, "2026-08-03").unwrap();
        assert_eq!(
            out,
            "before\n<env>\nWorking directory: /work\nPlatform: linux\nDate: 2026-08-03\n</env>\nafter"
        );
    }

    #[test]
    fn date_as_the_last_line_with_no_trailing_content_is_handled() {
        let prompt = "<env>\nDate: 2026-08-02";
        let out = refresh_env_date(prompt, "2026-08-03").unwrap();
        assert_eq!(out, "<env>\nDate: 2026-08-03");
    }

    #[test]
    fn no_env_block_returns_none() {
        let prompt = "just a plain prompt with no env block";
        assert_eq!(refresh_env_date(prompt, "2026-08-03"), None);
    }

    #[test]
    fn resolver_falls_back_to_none_within_the_same_day() {
        use entanglement_core::{
            AgentMode, AgentProfile, Permission, PermissionProfile, SessionId,
        };

        let resolver = date_resolver();
        let today = crate::date::today_utc();
        let profile = AgentProfile {
            name: "build".into(),
            description: String::new(),
            mode: AgentMode::Primary,
            system_prompt: format!("<env>\nDate: {today}\n</env>"),
            model: None,
            provider: None,
            permission: PermissionProfile::new(Permission::Allow),
            tools: None,
            disallowed_tools: Vec::new(),
            can_spawn: None,
            spawnable_agents: None,
            sandbox: None,
        };
        let session = SessionId::new("s1");
        assert_eq!(resolver(&session, &profile), None);
    }
}
