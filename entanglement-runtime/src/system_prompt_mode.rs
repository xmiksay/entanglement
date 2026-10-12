//! The runtime's `SystemPromptResolver`: pins the `<env>` block's `Date:`
//! line per session ([`crate::env_date`], ADR-0202 §5) so the cached system
//! block stays byte-stable across a UTC midnight. The rest of the prompt is
//! baked at load time and identical for every session (ADR-0226 removed the
//! per-mode prompt slimming along with the advertising modes).

use std::sync::Arc;

use entanglement_core::{Agent, SessionId, SystemPromptResolver};

use crate::env_date::refresh_env_date;
use crate::tool_advertising::AdvertisingState;

/// Build the resolver. `advertising` is the same `Arc` the tool executor
/// shares, so the executor's session-end arm forgets the date pin held there.
pub fn resolver(advertising: Arc<AdvertisingState>) -> SystemPromptResolver {
    Arc::new(move |session: &SessionId, profile| {
        resolve(&advertising, session, profile, &crate::date::today_utc())
    })
}

/// The resolver body with `today` injected, so a date rollover is testable.
fn resolve(
    advertising: &AdvertisingState,
    session: &SessionId,
    agent: &Agent,
    today: &str,
) -> Option<String> {
    let date = advertising
        .env_dates
        .lock()
        .expect("env-date pin mutex poisoned")
        .pin(session, today);
    refresh_env_date(&agent.system_prompt, &date)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(prompt: &str) -> Agent {
        Agent {
            name: "general".into(),
            description: String::new(),
            system_prompt: prompt.into(),
            model: None,
            provider: None,
            default_mode: None,
        }
    }

    #[test]
    fn a_session_keeps_its_first_date_across_a_rollover() {
        let advertising = AdvertisingState::new();
        let s = SessionId::new("s");
        let a = agent("BODY\n\n<env>\nDate: 2026-01-01\n</env>");
        let first = resolve(&advertising, &s, &a, "2026-10-11");
        assert!(first.as_deref().unwrap().contains("Date: 2026-10-11"));
        let next_day = resolve(&advertising, &s, &a, "2026-10-12");
        assert_eq!(first, next_day, "the pin holds for the session's life");
        let fresh = resolve(&advertising, &SessionId::new("t"), &a, "2026-10-12");
        assert!(fresh.as_deref().unwrap().contains("Date: 2026-10-12"));
    }

    #[test]
    fn a_prompt_without_an_env_block_is_left_alone() {
        let advertising = AdvertisingState::new();
        assert_eq!(
            resolve(
                &advertising,
                &SessionId::new("s"),
                &agent("BODY"),
                "2026-10-12"
            ),
            None
        );
    }
}
