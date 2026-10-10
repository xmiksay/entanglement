//! Wire `kind` tags of [`InMsg`]/[`OutEvent`] variants removed since 0.6.0 —
//! the first release embedders persisted session logs with (#720).
//!
//! WHY a list rather than a catch-all variant: a `#[serde(other)]` arm on the
//! live enums would also swallow a typo'd or future frame on the wire, and
//! every head would have to match a variant that never means anything. A
//! log reader instead checks a frame that fails to decode against this list
//! and skips it as an intentional retirement — never as lost data, so it is
//! not a gap — while any other undecodable frame still fails loudly.
//! Removing a variant from either enum means appending its tag here.

#[cfg(doc)]
use super::{InMsg, OutEvent};

/// `InMsg` kinds retired since 0.6.0: the live persona switch (`set_agent`,
/// ADR-0207 §9 — an agent is fixed for a session's life) and the session-less
/// bash toggles (ADR-0163 — live bash enablement is a tool-overlay entry).
pub const RETIRED_IN_MSG_KINDS: &[&str] = &["set_agent", "bash_enable", "bash_disable"];

/// `OutEvent` kinds retired since 0.6.0: the bash toggle's acknowledgement.
pub const RETIRED_OUT_EVENT_KINDS: &[&str] = &["bash_changed"];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{InMsg, OutEvent, SessionInfo};
    use serde_json::json;

    /// Every listed tag really is gone — a variant re-added under the same
    /// tag would otherwise be silently dropped from replayed logs. Matching
    /// the error text keeps a re-added variant with *required* fields from
    /// passing vacuously on a missing-field error.
    #[test]
    fn retired_kinds_no_longer_decode() {
        for kind in RETIRED_IN_MSG_KINDS {
            let frame = json!({"kind": kind, "session": "s", "agent": "plan"});
            let err = serde_json::from_value::<InMsg>(frame).expect_err(kind);
            assert!(err.to_string().contains("unknown variant"), "{kind}: {err}");
        }
        for kind in RETIRED_OUT_EVENT_KINDS {
            let frame = json!({"kind": kind, "enabled": true});
            let err = serde_json::from_value::<OutEvent>(frame).expect_err(kind);
            assert!(err.to_string().contains("unknown variant"), "{kind}: {err}");
        }
    }

    /// A 0.6.0 `SessionStarted` (`profile`, not `agent`) still decodes.
    #[test]
    fn session_started_accepts_the_pre_adr_0207_profile_field() {
        let frame = json!({
            "kind": "session_started", "session": "s", "parent": null,
            "profile": "site-editor", "model": null, "root": true, "ts": 1
        });
        let ev: OutEvent = serde_json::from_value(frame).expect("0.6.0 SessionStarted");
        assert!(matches!(ev, OutEvent::SessionStarted { agent, .. } if agent == "site-editor"));
    }

    /// The same rename on the `SessionList` row type.
    #[test]
    fn session_info_accepts_the_pre_adr_0207_profile_field() {
        let row = json!({"session": "s", "profile": "plan", "root": true});
        let info: SessionInfo = serde_json::from_value(row).expect("0.6.0 SessionInfo");
        assert_eq!(info.agent, "plan");
    }

    /// 0.6.0's `AgentChanged.profile_detail` (the retired agent mask) is
    /// ignored, not an error — no protocol type denies unknown fields.
    #[test]
    fn agent_changed_ignores_the_retired_profile_detail() {
        let frame = json!({
            "kind": "agent_changed", "session": "s", "agent": "plan",
            "profile_detail": {"mode": "primary", "tools": ["read"],
                               "permission": {"rules": [], "default": "ask"}}
        });
        let ev: OutEvent = serde_json::from_value(frame).expect("0.6.0 AgentChanged");
        assert!(matches!(ev, OutEvent::AgentChanged { agent, .. } if agent == "plan"));
    }
}
