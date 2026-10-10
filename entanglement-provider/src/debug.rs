//! `ENTANGLEMENT_DEBUG` — the one developer-diagnostics switch (ADR-0213).
//!
//! A comma-separated token list folding three former single-purpose env vars:
//!
//! | token             | effect                                              | replaced                                  |
//! | ----------------- | --------------------------------------------------- | ----------------------------------------- |
//! | `bodies`          | log full LLM request bodies at `debug` (#165)        | `ENTANGLEMENT_LOG_BODIES=1`               |
//! | `echo`            | `EchoLlm` appends the full system text              | `ENTANGLEMENT_ECHO_FULL=1`                |
//! | `no-shared-state` | no cross-process endpoint-state sharing (#523)       | `ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE=1` |
//!
//! Tokens are trimmed and case-insensitive; empty tokens are skipped. An
//! unknown token is reported back by [`DebugFlags::parse`] so the binary can
//! warn **once** at startup ([`DebugFlags::from_env`] itself stays silent —
//! it is read at several call sites, and repeating the warning on every
//! request would drown the log).

use std::ffi::OsString;

/// The env var carrying the debug token list.
pub const DEBUG_ENV: &str = "ENTANGLEMENT_DEBUG";

/// The parsed `ENTANGLEMENT_DEBUG` switches. All off by default.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DebugFlags {
    /// `bodies`: log LLM request bodies (conversation + repo data).
    pub bodies: bool,
    /// `echo`: `EchoLlm` appends the full assembled system text.
    pub echo: bool,
    /// `no-shared-state`: pure in-process endpoint gating.
    pub no_shared_state: bool,
}

impl DebugFlags {
    /// Parse a token list, returning the flags plus every token that matched
    /// nothing (in input order), for the caller's one startup warning.
    pub fn parse(raw: &str) -> (Self, Vec<String>) {
        let mut flags = Self::default();
        let mut unknown = Vec::new();
        for token in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match token.to_ascii_lowercase().as_str() {
                "bodies" => flags.bodies = true,
                "echo" => flags.echo = true,
                "no-shared-state" => flags.no_shared_state = true,
                _ => unknown.push(token.to_string()),
            }
        }
        (flags, unknown)
    }

    /// The flags from the process env (unknown tokens ignored here — see the
    /// module doc for where they are reported).
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    /// [`from_env`][Self::from_env] over an injected env lookup, so callers'
    /// precedence tests never mutate process-global env.
    pub fn from_lookup(env: impl Fn(&str) -> Option<OsString>) -> Self {
        env(DEBUG_ENV)
            .map(|v| Self::parse(&v.to_string_lossy()).0)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_token_sets_exactly_its_flag() {
        let (f, unknown) = DebugFlags::parse("bodies");
        assert_eq!(
            f,
            DebugFlags {
                bodies: true,
                ..Default::default()
            }
        );
        assert!(unknown.is_empty());
        assert!(DebugFlags::parse("echo").0.echo);
        assert!(DebugFlags::parse("no-shared-state").0.no_shared_state);
    }

    #[test]
    fn a_list_is_trimmed_case_insensitive_and_skips_empty_tokens() {
        let (f, unknown) = DebugFlags::parse(" Bodies , ,ECHO,no-shared-state,");
        assert_eq!(
            f,
            DebugFlags {
                bodies: true,
                echo: true,
                no_shared_state: true,
            }
        );
        assert!(unknown.is_empty());
    }

    #[test]
    fn empty_input_is_all_off() {
        assert_eq!(DebugFlags::parse(""), (DebugFlags::default(), Vec::new()));
        assert_eq!(
            DebugFlags::parse(" , "),
            (DebugFlags::default(), Vec::new())
        );
    }

    #[test]
    fn unknown_tokens_are_returned_in_order_and_known_ones_still_apply() {
        let (f, unknown) = DebugFlags::parse("bogus,echo,1");
        assert!(f.echo && !f.bodies && !f.no_shared_state);
        assert_eq!(unknown, vec!["bogus".to_string(), "1".to_string()]);
    }

    #[test]
    fn lookup_reads_only_the_debug_var() {
        let f = DebugFlags::from_lookup(|name| (name == DEBUG_ENV).then(|| "echo".into()));
        assert!(f.echo);
        assert_eq!(DebugFlags::from_lookup(|_| None), DebugFlags::default());
    }
}
