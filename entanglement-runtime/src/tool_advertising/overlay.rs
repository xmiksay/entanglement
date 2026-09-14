//! Overlay-enable ⇒ advertisement (#560 P9, ADR-0199 part 2): when a
//! session's live tool overlay (ADR-0149) gains a new **enable** entry, and
//! the session is `ToolSearch`-mode with `client_side` encoding, the newly
//! enabled tool name(s) join the session's discovered set
//! ([`super::AdvertisingState::discovered`]) — the exact set `describe()`
//! already writes into (ADR-0196 §3) — so the next resolver round advertises
//! them directly, with no extra `describe` round-trip. This is simply
//! another writer of that append-only set, not a new mechanism.
//!
//! `Full`-mode sessions are a no-op: advertisement is already universal
//! there, so there is nothing to add. `anthropic_native`/`responses_native`
//! encodings have their own `defer_loading`/`tool_reference` mechanism for
//! getting a schema in front of the model without a roster mutation — this
//! module only ever touches the `client_side` discovered-tail. A provider
//! that opted out of growing that array (`advertise_discovered: false`,
//! ADR-0200) is a no-op too — enable still unmasks the tool for dispatch,
//! only this append effect is suppressed.
//!
//! Wildcard expansion is a one-time snapshot against the registry at the
//! moment the overlay changes: a tool registered *later* that happens to
//! match an already-enabled pattern is **not** retro-advertised. Documented,
//! deliberate — it stays discoverable via `explore`/`describe` either way,
//! and teaching the discovered set to track live pattern subscriptions (versus
//! a one-shot name expansion) is more machinery than this feature needs.

use entanglement_core::{SessionId, ToolAdvertising, ToolOverlayEntry};

use super::{AdvertisingState, Encoding};

/// Fold one `SetToolOverlay` confirmation (`OutEvent::ToolOverlayChanged`)
/// into the discovered set. Called from the tool executor's fold *before*
/// `entries` overwrites its `overlays` mirror — `previous` is that
/// about-to-be-replaced list, needed to tell a genuinely new enable entry
/// apart from a re-send of one already in effect (e.g. a grade-only change
/// to an unrelated pattern still re-sends the whole list, ADR-0149's
/// full-replacement semantics).
///
/// - A **deny** entry never contributes: withdrawing a tool has no
///   advertisement effect (mirrors [`ToolOverlayEntry::disposition`]'s
///   deny-first read — existence is not the same question as advertisement).
/// - An enable entry already present in `previous` (byte-for-byte — same
///   pattern, grade, and arg-scope) is skipped: nothing new to advertise.
/// - `registered_names` is the tool registry's name snapshot *at the moment
///   of the change* — the wildcard-expansion snapshot the module doc
///   describes.
pub fn advertise_new_overlay_enables(
    state: &AdvertisingState,
    registered_names: &[String],
    session: &SessionId,
    previous: &[ToolOverlayEntry],
    entries: &[ToolOverlayEntry],
) {
    if state.mode(session) != ToolAdvertising::ToolSearch {
        return;
    }
    if state.encoding(session) != Encoding::ClientSide {
        return;
    }
    // ADR-0200: `advertise_discovered: false` freezes the advertised array —
    // enable still unmasks the tool (dispatch's own concern), only this
    // append-to-the-tail effect becomes a no-op.
    if !state.advertise_discovered(session) {
        return;
    }
    let new_enables = entries.iter().filter(|e| !e.deny && !previous.contains(e));
    let mut discovered = state
        .discovered
        .lock()
        .expect("discovered-tool mutex poisoned");
    for entry in new_enables {
        for name in registered_names.iter().filter(|n| entry.matches(n)) {
            discovered.mark(session, name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// Pin a session's mode/encoding directly through the public `modes`
    /// field — the executor loop normally does this via `AdvertisingInputs`
    /// off a `SessionStarted`, which is more machinery than this module's
    /// own tests need.
    fn pin(
        state: &AdvertisingState,
        session: &SessionId,
        mode: ToolAdvertising,
        encoding: Encoding,
    ) {
        state
            .modes
            .lock()
            .expect("tool-advertising mode mutex poisoned")
            .pin(session.clone(), mode, encoding);
    }

    #[test]
    fn a_new_enable_entry_marks_matching_registered_tools_discovered() {
        let state = AdvertisingState::new();
        let session = SessionId::new("s");
        pin(
            &state,
            &session,
            ToolAdvertising::ToolSearch,
            Encoding::ClientSide,
        );
        let registered = names(&["mcp__docs__search", "mcp__docs__read", "bash"]);

        advertise_new_overlay_enables(
            &state,
            &registered,
            &session,
            &[],
            &[ToolOverlayEntry::allow("mcp__docs__*")],
        );

        let discovered = state.discovered.lock().unwrap().names(&session);
        assert!(discovered.contains(&"mcp__docs__search".to_string()));
        assert!(discovered.contains(&"mcp__docs__read".to_string()));
        assert!(!discovered.contains(&"bash".to_string()));
    }

    #[test]
    fn full_mode_session_is_a_no_op() {
        let state = AdvertisingState::new();
        let session = SessionId::new("s");
        pin(
            &state,
            &session,
            ToolAdvertising::Full,
            Encoding::ClientSide,
        );
        let registered = names(&["bash"]);

        advertise_new_overlay_enables(
            &state,
            &registered,
            &session,
            &[],
            &[ToolOverlayEntry::allow("bash")],
        );

        assert!(state.discovered.lock().unwrap().names(&session).is_empty());
    }

    #[test]
    fn a_non_client_side_encoding_is_a_no_op() {
        let state = AdvertisingState::new();
        let session = SessionId::new("s");
        pin(
            &state,
            &session,
            ToolAdvertising::ToolSearch,
            Encoding::AnthropicNative,
        );
        let registered = names(&["bash"]);

        advertise_new_overlay_enables(
            &state,
            &registered,
            &session,
            &[],
            &[ToolOverlayEntry::allow("bash")],
        );

        assert!(state.discovered.lock().unwrap().names(&session).is_empty());
    }

    #[test]
    fn a_deny_entry_never_advertises() {
        let state = AdvertisingState::new();
        let session = SessionId::new("s");
        pin(
            &state,
            &session,
            ToolAdvertising::ToolSearch,
            Encoding::ClientSide,
        );
        let registered = names(&["bash"]);

        advertise_new_overlay_enables(
            &state,
            &registered,
            &session,
            &[],
            &[ToolOverlayEntry::deny("bash")],
        );

        assert!(state.discovered.lock().unwrap().names(&session).is_empty());
    }

    #[test]
    fn advertise_discovered_false_makes_the_overlay_enable_a_no_op() {
        // ADR-0200: a `client_side` `ToolSearch` session that opted out of
        // growing its advertised array must not have an overlay-enable grow
        // it either.
        let state = AdvertisingState::new();
        let session = SessionId::new("s");
        pin(
            &state,
            &session,
            ToolAdvertising::ToolSearch,
            Encoding::ClientSide,
        );
        state
            .modes
            .lock()
            .unwrap()
            .set_advertise_discovered(&session, false);
        let registered = names(&["mcp__docs__search"]);

        advertise_new_overlay_enables(
            &state,
            &registered,
            &session,
            &[],
            &[ToolOverlayEntry::allow("mcp__docs__*")],
        );

        assert!(state.discovered.lock().unwrap().names(&session).is_empty());
    }

    #[test]
    fn an_entry_already_present_in_previous_is_not_re_marked_but_a_genuinely_new_one_is() {
        let state = AdvertisingState::new();
        let session = SessionId::new("s");
        pin(
            &state,
            &session,
            ToolAdvertising::ToolSearch,
            Encoding::ClientSide,
        );
        let registered = names(&["bash", "mcp__docs__search"]);
        let previous = vec![ToolOverlayEntry::allow("bash")];
        let entries = vec![
            ToolOverlayEntry::allow("bash"),
            ToolOverlayEntry::allow("mcp__docs__search"),
        ];

        advertise_new_overlay_enables(&state, &registered, &session, &previous, &entries);

        // `bash` was already enabled (present in `previous`) — this call must
        // not be the reason it's marked; only the genuinely new entry is.
        let discovered = state.discovered.lock().unwrap().names(&session);
        assert_eq!(discovered, vec!["mcp__docs__search".to_string()]);
    }

    #[test]
    fn a_tool_registered_after_the_overlay_change_is_not_retro_advertised() {
        let state = AdvertisingState::new();
        let session = SessionId::new("s");
        pin(
            &state,
            &session,
            ToolAdvertising::ToolSearch,
            Encoding::ClientSide,
        );
        // The pattern is enabled before `mcp__docs__read` exists in the
        // registry snapshot handed in.
        let registered = names(&["mcp__docs__search"]);

        advertise_new_overlay_enables(
            &state,
            &registered,
            &session,
            &[],
            &[ToolOverlayEntry::allow("mcp__docs__*")],
        );

        let discovered = state.discovered.lock().unwrap().names(&session);
        assert_eq!(discovered, vec!["mcp__docs__search".to_string()]);
        assert!(!discovered.contains(&"mcp__docs__read".to_string()));
    }
}
