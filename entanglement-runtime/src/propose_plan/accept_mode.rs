//! The two modes a `propose_plan` approval may land on (#560, ADR-0207 §7
//! extension) — split out of the parent module to stay under the file-size
//! cap, mirroring the sibling `resolve` module's split. See
//! `propose_plan`'s own module doc for the full feature overview; this one
//! owns only the closed accept-mode set: parsing the tool's optional `mode`
//! suggestion and resolving the approver's actual choice.

/// The bounded, unattended posture (ADR-0207 §11) an approved plan's session
/// switches to by default — a bare accept with no mode named lands here; see
/// [`resolve_accept_mode`].
pub(super) const AUTO_MODE: &str = "auto";
/// The supervised posture (ADR-0207 §7's original destination) an approved
/// plan's session switches to when the approver explicitly opts in — the same
/// name the built-in mode table calls its ordinary implementation posture.
pub(super) const BUILD_MODE: &str = "build";
/// The closed set both the tool's own `mode` suggestion and the approver's
/// `InMsg::Approve::mode` choice are validated against. Kept as one array so
/// [`is_accept_mode`] and `propose_plan_spec`'s schema `enum` (main module)
/// can't silently drift apart.
const ACCEPT_MODES: [&str; 2] = [AUTO_MODE, BUILD_MODE];

fn is_accept_mode(m: &str) -> bool {
    ACCEPT_MODES.contains(&m)
}

/// Parse `propose_plan`'s optional `mode` argument: the model's own
/// suggestion for which acceptance option the prompt pre-selects. Validated
/// against the same closed [`ACCEPT_MODES`] set an approval itself resolves
/// into — a retired mode name (`research`/`plan`), an unknown string, or a
/// non-string value is refused with a clear message, since the model is
/// choosing how its own plan executes and the accepted set stays closed
/// (mirrors `request_mode`'s own tight validation). Absent field, or an input
/// that isn't a JSON object at all (`parse_plan_input` degrades that case to
/// inline `content`), both mean "no suggestion".
pub(super) fn parse_suggested_mode(input: &str) -> Result<Option<String>, String> {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(input)
    else {
        return Ok(None);
    };
    let Some(value) = map.get("mode") else {
        return Ok(None);
    };
    let Some(m) = value.as_str() else {
        return Err(format!(
            "propose_plan `mode` must be a string, one of `build`/`auto` (got {value})"
        ));
    };
    if is_accept_mode(m) {
        Ok(Some(m.to_string()))
    } else {
        Err(format!(
            "propose_plan `mode` must be `build` or `auto` (got `{m}`) — it only suggests which \
             option the approval prompt pre-selects; the user's own choice always decides"
        ))
    }
}

/// Resolve the approver's chosen mode from the parked
/// `seam::Decision::Approve`'s `mode` field into one of the two closed
/// [`ACCEPT_MODES`]. Anything but the literal `"build"` — `None` (a bare
/// accept, or an older/foreign head that never sends the field) or an
/// unrecognized string (a raw wire client sending garbage) — degrades to
/// [`AUTO_MODE`], the documented default: the mode choice is advisory data on
/// an approval that has already happened, not a second gate that could refuse
/// it.
pub(super) fn resolve_accept_mode(chosen: Option<String>) -> &'static str {
    match chosen.as_deref() {
        // A bare accept is the user's chosen default: accepting a plan means
        // "go do it", and auto is bounded by its budgets and deny list.
        None | Some(AUTO_MODE) => AUTO_MODE,
        Some(BUILD_MODE) => BUILD_MODE,
        // Anything unrecognised fails *safe*, not open. Auto is the most
        // permissive unattended mode there is, and the likeliest garbage here
        // is a mistyped attempt at the cautious option — `"buld"` must not
        // silently run a plan unattended.
        Some(_) => BUILD_MODE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_suggested_mode_reads_build_or_auto() {
        assert_eq!(
            parse_suggested_mode(r#"{"content":"x","mode":"build"}"#).unwrap(),
            Some("build".to_string())
        );
        assert_eq!(
            parse_suggested_mode(r#"{"content":"x","mode":"auto"}"#).unwrap(),
            Some("auto".to_string())
        );
    }

    #[test]
    fn parse_suggested_mode_is_none_when_absent() {
        assert_eq!(parse_suggested_mode(r#"{"content":"x"}"#).unwrap(), None);
        // A bare-string input (degrades to inline content elsewhere) also
        // suggests nothing.
        assert_eq!(parse_suggested_mode("just a plan").unwrap(), None);
    }

    /// Only the two real accepted modes — a retired mode name, an unknown
    /// string, or a non-string value are all refused, mirroring
    /// `request_mode`'s own closed-set validation.
    #[test]
    fn parse_suggested_mode_rejects_anything_outside_build_or_auto() {
        for bad in [
            r#"{"content":"x","mode":"research"}"#,
            r#"{"content":"x","mode":"plan"}"#,
            r#"{"content":"x","mode":"bogus"}"#,
            r#"{"content":"x","mode":42}"#,
        ] {
            let err = parse_suggested_mode(bad).unwrap_err();
            assert!(
                err.contains("build") && err.contains("auto"),
                "{bad} -> {err}"
            );
        }
    }

    #[test]
    fn resolve_accept_mode_defaults_a_bare_accept_to_auto() {
        assert_eq!(resolve_accept_mode(None), AUTO_MODE);
    }

    #[test]
    fn resolve_accept_mode_honors_an_explicit_build_choice() {
        assert_eq!(resolve_accept_mode(Some("build".to_string())), BUILD_MODE);
    }

    /// Garbage in `Approve::mode` fails safe to `build`, not open to `auto`.
    /// Auto is the *permissive* unattended mode, so it is the right answer for
    /// a bare accept (the user's own default) but the wrong fallback for a
    /// malformed value — which is most likely a mistyped cautious choice.
    #[test]
    fn resolve_accept_mode_fails_an_unrecognized_choice_safe_to_build() {
        assert_eq!(resolve_accept_mode(Some("buld".to_string())), BUILD_MODE);
        assert_eq!(resolve_accept_mode(Some("bogus".to_string())), BUILD_MODE);
        assert_eq!(resolve_accept_mode(Some("plan".to_string())), BUILD_MODE);
    }

    #[test]
    fn resolve_accept_mode_honors_an_explicit_auto_choice() {
        assert_eq!(resolve_accept_mode(Some("auto".to_string())), AUTO_MODE);
    }
}
