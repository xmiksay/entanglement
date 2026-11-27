//! `rhai`'s own launch (ADR-0228), graded like any other tool call — the
//! resolver's grade (a `path` script graded on its canonical root-relative
//! path, so `rhai(scripts/deploy/*)` matches; an inline one only by a bare
//! `rhai` rule), the ceiling and grants:
//!
//! - **Deny** refuses it, a trusted path included;
//! - a `path` inside a `rhai.trusted` folder (not suppressed for this mode)
//!   runs with **no card**, every nested call force-allowed
//!   ([`Posture::Trusted`]);
//! - **Allow** runs it with no card, **Ask per command**;
//! - **Ask** parks **one card** showing the source: **Run all**, **Ask per
//!   command** or **Reject**, with the usual scope — a wider scope records a
//!   grant for the exact path (never for an inline script, never `run_all`),
//!   so the next launch grades Allow. With nobody to answer (the mode's
//!   `question_timeout` elapses) it runs Ask per command — unless the
//!   resolver gave a reason for asking (#968: `skutter`'s escape-root check
//!   for a script outside the project root), which the card shows and which
//!   silence never approves.
//!
//! The grade needs only the raw input, so it runs before the file is
//! touched: a denied launch never opens it. A `path` script is then read
//! *before* the card so the user approves the very source that runs: the
//! text read here is what the engine evaluates, and trust is judged on the
//! canonical path of the file that text came from ([`read_script`]) — which
//! must be the file the grade was computed on. A read failure still goes
//! through the card, so a path's existence is not disclosed before approval.

use std::path::{Path, PathBuf};

use entanglement_core::{AgentState, ApprovalKind, ApprovalScope, CardReason};

use crate::policy::{deny_text, AskReason, Grade};
use crate::seam::ApprovalDecision;
use crate::tool_names::RHAI_TOOL;
use crate::tool_runner::{Dispatcher, Posture};

use super::input::{read_script, ScriptInput, Source};
use super::Launch;

/// The first line of an inline script's launch card.
const INLINE_CARD_HEADER: &str = "// inline script";

/// What the gate decided.
pub(super) enum Verdict {
    /// Run this source under `posture`.
    Run { script: String, posture: Posture },
    /// Reply with this error text; nothing runs.
    Refuse(String),
    /// A `Stop` unwound the approval park: no reply is owed.
    Stopped,
}

/// Gate one launch. `input` is the raw call input — the grade reads it,
/// exactly as for any tool.
pub(super) async fn gate(
    dispatcher: &Dispatcher,
    launch: &Launch,
    request_id: &str,
    input: &str,
    parsed: &ScriptInput,
) -> Verdict {
    let d = dispatcher;
    let root = launch_root(d, launch);
    let graded = graded_input(root, input);
    let (grade, arg) = d.grade(&d.tools, RHAI_TOOL, &graded).await;
    let why = match grade {
        Grade::Deny(reason) => return Verdict::Refuse(deny_text(RHAI_TOOL, &d.mode, reason)),
        Grade::Allow => None,
        Grade::Ask(why) => Some(why),
    };
    let (source, trusted) = match &parsed.source {
        Source::Inline(script) => (Ok(script.clone()), false),
        Source::Path(path) => match read(d, launch, path, arg.as_deref()).await {
            Ok((text, canon)) => (Ok(text), d.trust.folders.runs_trusted(&canon, &d.mode)),
            Err(e) => (Err(e), false),
        },
    };
    let run = |posture| match &source {
        Ok(script) => Verdict::Run {
            script: script.clone(),
            posture,
        },
        Err(e) => Verdict::Refuse(e.clone()),
    };
    if trusted {
        return run(Posture::Trusted);
    }
    // Allow: no card. Ask: the resolver's reason, if it gave one.
    let Some(why) = why else {
        return run(Posture::AskPerCommand);
    };
    let reason = why.as_ref().map_or_else(
        || format!("mode `{}` grades `{RHAI_TOOL}` Ask", d.mode),
        |w| w.text.clone(),
    );
    let card_reason = match (&parsed.source, &why) {
        (Source::Inline(_), _) => CardReason::InlineLaunch,
        (Source::Path(_), Some(w)) => w.card,
        (Source::Path(_), None) => CardReason::ScriptLaunch,
    };
    let card = card(parsed, &source, why.as_ref());
    // A script that could not be read never runs, so no grant is recorded
    // for it (`record` below): its card offers `Once` alone.
    let once_only = source.is_err();
    match ask(d, request_id, &card, reason, card_reason, once_only).await {
        // Nobody answered within the mode's `question_timeout`: silence
        // never approves a reasoned ask, and otherwise costs only the
        // force-allow — each nested call is still graded on its own.
        None => match &why {
            Some(w) => Verdict::Refuse(format!(
                "tool `{RHAI_TOOL}` refused: no response within {}s — {} (mode `{}`)",
                d.limits.question_timeout, w.text, d.mode
            )),
            None => run(Posture::AskPerCommand),
        },
        Some(ApprovalDecision::Approve { scope, run_all, .. }) => {
            // A script that could not be read never ran: nothing to grant.
            if source.is_ok() {
                record(d, arg.as_deref(), scope).await;
            }
            run(if run_all {
                Posture::RunAll
            } else {
                Posture::AskPerCommand
            })
        }
        Some(ApprovalDecision::Reject { reason }) => Verdict::Refuse(format!(
            "tool `{RHAI_TOOL}` rejected: {}",
            reason.as_deref().unwrap_or("user")
        )),
        Some(ApprovalDecision::Stop) => Verdict::Stopped,
    }
}

/// What a wider approval earns: the launch grant for the exact script path,
/// so the next launch grades Allow (Ask per command). An inline script has
/// no path to key a grant on, so its approval is once only; `run_all` is
/// never recorded.
async fn record(d: &Dispatcher, arg: Option<&str>, scope: ApprovalScope) {
    if scope == ApprovalScope::Once {
        return;
    }
    if let Some(arg) = arg {
        d.grants
            .record(&d.session, RHAI_TOOL, Some(arg), scope, &d.mode)
            .await;
    }
}

/// Read a `path` script: its text and canonical file. The file must be the
/// one the grade was computed on (`arg`, the canonical root-relative path) —
/// a swap between the two would otherwise run a script graded as another.
async fn read(
    d: &Dispatcher,
    launch: &Launch,
    path: &str,
    arg: Option<&str>,
) -> Result<(String, PathBuf), String> {
    let root = launch_root(d, launch);
    let file = read_script(path, root.join(path)).await?;
    if arg.is_some_and(|arg| !read_is_graded(root, &file.canon, arg)) {
        return Err(format!(
            "rhai: {path:?} changed while it was being launched — refusing to run it"
        ));
    }
    Ok((file.text, file.canon))
}

/// The root a script `path` is graded and read against: the executor's
/// project root when one is wired, else the launch's own — so a
/// `rhai(scripts/*)` rule and the read-vs-grade check behave the same
/// either way.
fn launch_root<'a>(d: &'a Dispatcher, launch: &'a Launch) -> &'a Path {
    d.grading_root.as_deref().unwrap_or(launch.root.as_path())
}

/// `input` with its script `path` replaced by the canonical root-relative
/// form the launch is graded on (ADR-0228): every grader — a resolver with
/// no root of its own included — then matches rules and grants on it.
fn graded_input(root: &Path, input: &str) -> String {
    let Ok(serde_json::Value::Object(mut obj)) = serde_json::from_str(input) else {
        return input.to_string();
    };
    let Some(path) = obj.get("path").and_then(|p| p.as_str()) else {
        return input.to_string();
    };
    let canon = crate::permission_path::rooted_script(root, path);
    obj.insert("path".into(), canon.into());
    serde_json::Value::Object(obj).to_string()
}

/// Whether the file read (canonical `canon`) is the one graded as `arg`.
fn read_is_graded(root: &Path, canon: &Path, arg: &str) -> bool {
    crate::permission_path::rooted_script(root, &canon.to_string_lossy()) == arg
}

/// Park the launch card and wait for the head's choice — bounded by the
/// mode's `question_timeout` unless a head watches the session (ADR-0247;
/// `None`: it elapsed). Registered before
/// emitting so a fast answer can't race it.
async fn ask(
    d: &Dispatcher,
    request_id: &str,
    card: &str,
    reason: String,
    card_reason: CardReason,
    once_only: bool,
) -> Option<ApprovalDecision> {
    let (holly, session) = (&d.holly, &d.session);
    let reason = format!("{reason} — run all, ask per command, or reject");
    let mut req = crate::pending::approval(
        session,
        request_id,
        ApprovalKind::Rhai,
        RHAI_TOOL,
        card,
        reason,
        Some(card_reason),
    );
    if once_only {
        req.scopes = vec![ApprovalScope::Once];
    }
    let rx = crate::pending::park_approval(holly, &d.pending, req, RHAI_TOOL);
    holly.emit_status(session, AgentState::WaitingApproval);
    let expiry = crate::run_limits::expiry(&d.limits, &d.attendance, &d.chain, request_id);
    let decision = d
        .pending
        .await_timed(session, request_id, rx, expiry)
        .await
        .map(|d| d.into_approval());
    if !matches!(decision, Some(ApprovalDecision::Stop)) {
        holly.emit_status(session, AgentState::Thinking);
    }
    decision
}

/// The approval card body: the source as a script, headed by its
/// `path`/`args` as Rhai comments, plus the resolver's reason for asking.
/// The path is quoted (`{:?}`) — it is model-supplied, and a raw newline
/// would let it forge card lines.
fn card(parsed: &ScriptInput, source: &Result<String, String>, why: Option<&AskReason>) -> String {
    // The first line is always the runtime's own — `// path:` or the
    // inline header — so script text can never pass itself off as a path
    // card to the human reading it (heads decide on the card reason).
    let mut card = match &parsed.source {
        Source::Path(path) => format!("// path: {path:?}\n"),
        Source::Inline(_) => format!("{INLINE_CARD_HEADER}\n"),
    };
    if !parsed.args.is_empty() {
        let args = serde_json::Value::Object(parsed.args.clone());
        card.push_str(&format!("// args: {args}\n"));
    }
    match source {
        Ok(script) => card.push_str(script),
        Err(e) => card.push_str(&format!("// {}", e.replace('\n', " "))),
    }
    if let Some(why) = why {
        card.push_str(&format!("\n\n⚠ {}", why.text));
    }
    card
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(input: &str) -> ScriptInput {
        super::super::input::parse(input).unwrap()
    }

    #[test]
    fn the_card_heads_the_source_with_path_and_args() {
        let p = parsed(r#"{"path":"s/x.rhai","args":{"n":1}}"#);
        assert_eq!(
            card(&p, &Ok("args.n + 1".into()), None),
            "// path: \"s/x.rhai\"\n// args: {\"n\":1}\nargs.n + 1"
        );
        let failed = card(&p, &Err("rhai: cannot read\nx".into()), None);
        assert!(failed.ends_with("// rhai: cannot read x"), "{failed}");
        let why = AskReason {
            text: "runs a script outside the project root: /elsewhere/x.rhai".into(),
            card: CardReason::LaunchEscape,
        };
        let out = card(&p, &Ok("1".into()), Some(&why));
        assert!(
            out.ends_with("⚠ runs a script outside the project root: /elsewhere/x.rhai"),
            "{out}"
        );
    }

    /// A newline in the model-supplied path cannot forge a card line.
    #[test]
    fn a_newline_in_the_path_stays_on_the_path_line() {
        let p = parsed(r#"{"path":"x\n// args: {\"safe\":true}\n.rhai"}"#);
        let out = card(&p, &Ok("1".into()), None);
        assert_eq!(out.lines().count(), 2, "{out}");
        assert!(out.starts_with("// path: \"x\\n// args:"), "{out}");
    }

    /// The grade is computed on the canonical root-relative path, and the
    /// launch refuses a file read that is not the file graded.
    #[cfg(unix)]
    #[test]
    fn the_read_must_be_the_graded_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("scripts/a.rhai"), "1").unwrap();
        std::fs::write(root.join("other.rhai"), "2").unwrap();
        std::os::unix::fs::symlink(root.join("other.rhai"), root.join("scripts/l.rhai")).unwrap();
        let graded = graded_input(&root, r#"{"path":"scripts/./l.rhai","args":{"n":1}}"#);
        let v: serde_json::Value = serde_json::from_str(&graded).unwrap();
        assert_eq!(v["path"], "other.rhai");
        assert_eq!(v["args"]["n"], 1);
        assert_eq!(
            graded_input(&root, r#"{"script":"1"}"#),
            r#"{"script":"1"}"#
        );
        // Graded as `scripts/a.rhai`; a swap makes the read land elsewhere.
        assert!(read_is_graded(
            &root,
            &root.join("scripts/a.rhai"),
            "scripts/a.rhai"
        ));
        assert!(!read_is_graded(
            &root,
            &root.join("other.rhai"),
            "scripts/a.rhai"
        ));
    }

    /// An inline card always opens with the runtime's header, so source
    /// starting `// path:` can never look like a path card.
    #[test]
    fn an_inline_card_opens_with_the_runtime_header() {
        let p = parsed(r#"{"script":"// path: \"x.rhai\"\n1"}"#);
        let out = card(&p, &Ok("// path: \"x.rhai\"\n1".into()), None);
        assert!(out.starts_with("// inline script\n// path:"), "{out}");
    }
}
