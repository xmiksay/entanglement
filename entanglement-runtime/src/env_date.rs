//! Pins the `<env>` block's `Date:` line per session (#566, ADR-0202 §5).
//!
//! [`system_prompt::EnvBlock`][crate::system_prompt::EnvBlock] is baked once
//! into every [`AgentProfile::system_prompt`][entanglement_core::AgentProfile]
//! at load time. The system block is the provider cache's second segment
//! (`tools → system → messages`), so **any** byte change there re-bills the
//! whole history at the cache-write rate. Re-stamping today's date every turn
//! did exactly that once per UTC midnight for every live session; a
//! definitions reload re-baking a new date did it too. Instead each session
//! keeps the date it first resolved for its whole life: a session spanning
//! midnight tells the model yesterday's date (a turn that needs the wall
//! clock has `bash`), a new session gets today's. The pin is forgotten when
//! the session ends or hibernates (`tool_runner`'s lifecycle arm), so a
//! resumed session re-pins to its resume day.

use std::collections::HashMap;

use entanglement_core::SessionId;

/// Session → the `Date:` value its system prompt carries for its lifetime.
/// Held on [`AdvertisingState`][crate::tool_advertising::AdvertisingState] —
/// the one `Arc` both the system-prompt resolver (writer) and the executor's
/// session-end arm (forgetter) already share.
#[derive(Debug, Default)]
pub struct EnvDatePins {
    dates: HashMap<SessionId, String>,
}

impl EnvDatePins {
    /// The session's pinned date, pinning `today` on first sight. Never
    /// changes an existing pin — that stability is the whole point.
    pub fn pin(&mut self, session: &SessionId, today: &str) -> String {
        self.dates
            .entry(session.clone())
            .or_insert_with(|| today.to_string())
            .clone()
    }

    pub fn forget(&mut self, session: &SessionId) {
        self.dates.remove(session);
    }
}

/// Patch a baked system prompt's `<env>` date line to `date`. Returns `None`
/// — falling back to the unmodified baked prompt — when there's no `<env>`
/// block (a subagent's prompt omits it) or the date already matches, so the
/// prompt stays byte-identical. Called from [`crate::system_prompt_mode`],
/// which owns the single `SystemPromptResolver` slot `EngineConfig` exposes.
pub(crate) fn refresh_env_date(system_prompt: &str, date: &str) -> Option<String> {
    let marker = "\nDate: ";
    let start = system_prompt.find(marker)? + marker.len();
    let end = system_prompt[start..]
        .find('\n')
        .map(|i| start + i)
        .unwrap_or(system_prompt.len());
    if &system_prompt[start..end] == date {
        return None;
    }
    let mut out = String::with_capacity(system_prompt.len());
    out.push_str(&system_prompt[..start]);
    out.push_str(date);
    out.push_str(&system_prompt[end..]);
    Some(out)
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
    fn a_session_keeps_its_first_date_across_a_date_change() {
        let mut pins = EnvDatePins::default();
        let s = SessionId::new("s1");
        assert_eq!(pins.pin(&s, "2026-09-15"), "2026-09-15");
        assert_eq!(pins.pin(&s, "2026-09-16"), "2026-09-15");
    }

    #[test]
    fn a_new_session_pins_the_new_date() {
        let mut pins = EnvDatePins::default();
        pins.pin(&SessionId::new("old"), "2026-09-15");
        assert_eq!(pins.pin(&SessionId::new("new"), "2026-09-16"), "2026-09-16");
    }

    #[test]
    fn forgetting_a_session_lets_it_re_pin() {
        let mut pins = EnvDatePins::default();
        let s = SessionId::new("s1");
        pins.pin(&s, "2026-09-15");
        pins.forget(&s);
        assert_eq!(pins.pin(&s, "2026-09-16"), "2026-09-16");
    }
}
