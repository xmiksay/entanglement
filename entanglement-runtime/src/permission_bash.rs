//! Compound `bash` command grading (ADR-0197). Argument-scoped permission
//! rules like `bash(find *)` historically graded the ENTIRE raw `command`
//! string through [`PermissionProfile::resolve_scoped`]'s full-string
//! `glob_match` — over-matching a compound command's trailing segments
//! (`find . && rm -rf /` slipped through `bash(find *)`, since the rule's
//! trailing `*` swallows `&&` and everything after it) and under-matching a
//! compound that doesn't start with an allowed verb (no rule ever matches
//! the whole string, so the same curated read-only allow-list re-prompts on
//! every compound call). [`crate::shell_split`] gives a conservative,
//! quote-aware split into top-level segments; [`resolve_scoped_bash_aware`]
//! grades each one and folds the result, failing closed on anything the
//! splitter can't fully account for.
//!
//! Every runtime call site that resolves a `bash` call's command through
//! [`PermissionProfile::resolve_scoped`] routes through here instead — a
//! drop-in replacement with the identical signature plus the profile
//! receiver. Core's `resolve_scoped`/`glob_match` stay untouched (`make
//! tree` keeps the runtime's policy layer out of core); this is a
//! runtime-only wrapper, applied independently at each remaining call site
//! (the tool-overlay grade, in both `tool_runner::dispatch` and
//! `BindingPolicy::decide`) rather than as one refactored top-level function
//! — `min`-folding a segment grade is associative and commutative, so
//! folding per layer then combining layers with the existing
//! `min_permission` calls gives exactly the same answer as folding once over
//! every (layer, segment) pair would. The config **ceiling** clamp
//! (`permission::clamp_to_base`) no longer routes through here (ADR-0207
//! stage 6c): it grades through `mode::Mode::resolve` instead, which does
//! its own compound-command splitting and additionally understands
//! capability-class ceiling rules (`deny: [write]`), which this
//! `PermissionProfile`-only wrapper has no notion of.

use entanglement_core::{Permission, PermissionProfile};

use crate::permission::min_permission;
use crate::shell_split::{self, SplitOutcome};

/// [`PermissionProfile::resolve_scoped`], but per-segment-aware for `bash`
/// (ADR-0197). Every other tool (including `call`, which execs argv with no
/// shell — ADR-0093 — and so has no compound-command surface to split) is
/// passed straight through, byte-identical to today.
pub fn resolve_scoped_bash_aware(
    profile: &PermissionProfile,
    tool: &str,
    arg: Option<&str>,
    workdir: Option<&str>,
) -> Permission {
    if tool != "bash" {
        return profile.resolve_scoped(tool, arg, workdir);
    }
    match arg {
        Some(command) => resolve_bash(command, |a| profile.resolve_scoped(tool, a, workdir)),
        // No command to split (a workdir-only resolution, #425) — nothing
        // for this wrapper to add.
        None => profile.resolve_scoped(tool, arg, workdir),
    }
}

/// Grade a `bash` `command` through `resolve` — the caller's own
/// single-argument resolution, closing over whichever profile/ceiling layer
/// is being graded — per segment:
///
/// - The **whole raw command is graded first** (the legacy full-string
///   behavior). A `Deny` there short-circuits immediately: deny must never
///   be *weakened* by splitting, and this is also the only way an arg-scoped
///   deny rule fires against an [`SplitOutcome::Opaque`] command, since the
///   opaque path below drops the argument entirely. A non-deny legacy grade
///   is otherwise discarded — the segment fold (or the opaque fallback)
///   decides the real answer.
/// - [`SplitOutcome::Opaque`]: an arg-scoped **Allow** rule must not fire
///   against a construct the splitter can't fully account for (that is
///   exactly the over-match hole this module closes), so this re-resolves
///   with `arg: None` — only the tool's bare/workdir-scoped rules can still
///   grade it.
/// - [`SplitOutcome::Segments`]: every segment is resolved independently and
///   folded with [`min_permission`] (`Deny < Ask < Allow`) — any segment
///   `Deny` denies the whole command; all-`Allow` is `Allow`; otherwise the
///   fold lands on the most restrictive non-deny grade among the segments,
///   which is exactly what an unmatched argument already resolves to today
///   (the profile's own `Ask`-by-default fallthrough). A simple command
///   splits into exactly one segment equal to the whole string, so this
///   reduces to the legacy single `resolve(Some(command))` call — the fold
///   is byte-identical to today for every non-compound call.
pub(crate) fn resolve_bash(
    command: &str,
    mut resolve: impl FnMut(Option<&str>) -> Permission,
) -> Permission {
    let legacy = resolve(Some(command));
    if legacy == Permission::Deny {
        return Permission::Deny;
    }
    match shell_split::split(command) {
        SplitOutcome::Opaque => resolve(None),
        SplitOutcome::Segments(segments) => segments
            .iter()
            .try_fold(Permission::Allow, |acc, seg| {
                let grade = resolve(Some(seg));
                if grade == Permission::Deny {
                    None
                } else {
                    Some(min_permission(acc, grade))
                }
            })
            .unwrap_or(Permission::Deny),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::PermissionProfile;

    #[test]
    fn simple_command_matches_legacy_behavior() {
        let profile =
            PermissionProfile::new(Permission::Ask).with("bash(find *)", Permission::Allow);
        assert_eq!(
            resolve_scoped_bash_aware(&profile, "bash", Some("find ."), None),
            Permission::Allow
        );
    }

    #[test]
    fn compound_pipeline_allowed_when_every_segment_matches() {
        let profile = PermissionProfile::new(Permission::Ask)
            .with("bash(find *)", Permission::Allow)
            .with("bash(grep *)", Permission::Allow)
            .with("bash(wc *)", Permission::Allow);
        assert_eq!(
            resolve_scoped_bash_aware(&profile, "bash", Some("find . | grep x | wc -l"), None),
            Permission::Allow
        );
    }

    #[test]
    fn compound_with_unmatched_segment_asks_not_allows() {
        // The over-match regression: `find . && rm -rf /tmp/x` must never
        // ride a `bash(find *)` allow rule through the `&&`.
        let profile =
            PermissionProfile::new(Permission::Ask).with("bash(find *)", Permission::Allow);
        assert_eq!(
            resolve_scoped_bash_aware(&profile, "bash", Some("find . && rm -rf /tmp/x"), None),
            Permission::Ask
        );
    }

    #[test]
    fn deny_rule_on_any_segment_denies_the_whole_command() {
        let profile = PermissionProfile::new(Permission::Ask)
            .with("bash(find *)", Permission::Allow)
            .with("bash(rm *)", Permission::Deny);
        assert_eq!(
            resolve_scoped_bash_aware(&profile, "bash", Some("find . && rm x"), None),
            Permission::Deny
        );
    }

    #[test]
    fn opaque_command_does_not_ride_an_arg_scoped_allow() {
        let profile =
            PermissionProfile::new(Permission::Ask).with("bash(find *)", Permission::Allow);
        assert_eq!(
            resolve_scoped_bash_aware(&profile, "bash", Some("find . > out.txt"), None),
            Permission::Ask
        );
    }

    #[test]
    fn non_bash_tool_is_unaffected() {
        let profile = PermissionProfile::new(Permission::Ask).with("call(rm *)", Permission::Allow);
        assert_eq!(
            resolve_scoped_bash_aware(&profile, "call", Some("rm -rf / && ls"), None),
            Permission::Allow
        );
    }
}
