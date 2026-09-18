//! Mode-scoped run limits (ADR-0207 §6/§11): spawn nesting depth, spawn
//! fan-out, and the bounds an unattended (`auto`) run needs that an
//! attended one doesn't. Every numeric field is `Option` — undefined means
//! unlimited, never a hidden default nobody chose (ADR-0207 §6: "Undefined
//! means unlimited").

use serde::Deserialize;

/// One mode's run limits, deserialized flat alongside its `default`/`deny`/
/// `allow` keys (ADR-0207 §4's YAML shape has no nested `limits:` block).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Spawn nesting depth; the root session is depth 0 (ADR-0207 §6).
    /// `None` = unbounded.
    #[serde(default)]
    pub max_depth: Option<u32>,
    /// Concurrent child sessions per root (ADR-0207 §6). `None` = unbounded.
    #[serde(default)]
    pub max_agents: Option<u32>,
    /// Seconds before a parked question or approval expires; `0` (the
    /// default) means infinite — every built-in mode but `auto` leaves this
    /// at `0`, since only an unattended run has no one to time out on
    /// (ADR-0207 §11).
    #[serde(default)]
    pub question_timeout: u64,
    /// What a parked approval does when `question_timeout` elapses. Inert
    /// while `question_timeout` is `0`.
    #[serde(default)]
    pub on_timeout: OnTimeout,
    /// Turn budget for an unattended run (ADR-0207 §11). `None` = unbounded.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Wall-clock budget in seconds for an unattended run (ADR-0207 §11).
    /// `None` = unbounded.
    #[serde(default)]
    pub max_duration: Option<u64>,
}

/// What happens when `question_timeout` elapses (ADR-0207 §11): an options
/// question expires to its default option, a free-text question expires as
/// an `is_error`, and a parked *approval* expires as a denial — silence is
/// never consent for a privileged action. `Deny` is the only posture any
/// built-in mode needs; kept as an enum rather than folded into a bool so an
/// embedder's own mode table has somewhere to add a different rule later
/// without a breaking change to [`Limits`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnTimeout {
    #[default]
    Deny,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undefined_limits_mean_unlimited() {
        let limits = Limits::default();
        assert_eq!(limits.max_depth, None);
        assert_eq!(limits.max_agents, None);
        assert_eq!(limits.max_turns, None);
        assert_eq!(limits.max_duration, None);
    }

    #[test]
    fn question_timeout_defaults_to_infinite() {
        assert_eq!(Limits::default().question_timeout, 0);
    }

    #[test]
    fn on_timeout_defaults_to_deny() {
        assert_eq!(Limits::default().on_timeout, OnTimeout::Deny);
    }

    #[test]
    fn deserializes_from_the_flat_mode_yaml_shape() {
        let limits: Limits = serde_yaml::from_str(
            "max_depth: 2\nmax_agents: 4\nquestion_timeout: 60\non_timeout: deny\n",
        )
        .expect("valid limits YAML");
        assert_eq!(limits.max_depth, Some(2));
        assert_eq!(limits.max_agents, Some(4));
        assert_eq!(limits.question_timeout, 60);
    }
}
