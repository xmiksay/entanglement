//! Slash-command senders that need `holly` and the raw command text — the
//! commands the sync `execute_command` dispatch can't serve — plus the
//! `!bash` passthrough.

use entanglement_core::{Holly, InMsg};

use crate::tui::app::App;

/// Issue 2: `/cmd -h`/`--help` and `/help <cmd>` both render the clap-generated
/// help for the command. Returns `true` if the input was a help request (so the
/// caller short-circuits the normal dispatch), `false` to let dispatch proceed.
/// For `/cmd -h`, `cmd` is the parsed command; for `/help <cmd>`, `cmd` is
/// `Command::Help` and the target is parsed from the trailing text.
pub(super) async fn send_help_if_requested(
    app: &mut App,
    text: &str,
    cmd: &crate::tui::commands::Command,
) -> bool {
    use crate::tui::commands::Command;
    // `/help <cmd>`: look up the named command and render its help_text.
    if *cmd == Command::Help {
        let target = text
            .trim()
            .strip_prefix("/help")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|name| app.slash().roster().into_iter().find(|c| c.name() == name));
        if let Some(target) = target {
            let help = target.help_text();
            app.record_notice("help", help);
            return true;
        }
        return false;
    }
    // `/cmd -h`/`--help`: any arg-bearing command accepts the help flag.
    if cmd.has_args() && has_help_flag(text) {
        let help = cmd.help_text();
        app.record_notice(cmd.name(), help);
        return true;
    }
    false
}

/// Whether the raw `/cmd …` text contains a `-h` or `--help` token (Issue 2).
pub(super) fn has_help_flag(text: &str) -> bool {
    text.split_whitespace()
        .any(|tok| tok == "-h" || tok == "--help")
}

/// Send `/compact [--keep N] [instructions]` as an [`InMsg::Oneshot`]
/// `"compact"` op (#324, ADR-0082; `--keep`, #397/ADR-0102): an optional
/// leading `--keep N` becomes `args.kept`, any remaining text becomes
/// `args.instructions`. A parse error (bad `--keep` value) is rendered as a
/// status line instead — no engine traffic. The reducer renders the result
/// (`Compacted`) as a transcript notice once it arrives; nothing is recorded
/// here — unlike a prompt, a oneshot op has no user-authored message to echo
/// locally.
pub(super) async fn send_compact(app: &mut App, holly: &Holly, text: &str) {
    let (kept, instructions) = match crate::tui::commands::parse_compact_args(text) {
        Ok(parsed) => parsed,
        Err(e) => {
            app.record_compact_error(e);
            return;
        }
    };
    let mut args = serde_json::Map::new();
    if kept > 0 {
        args.insert("kept".to_string(), serde_json::Value::from(kept));
    }
    if let Some(instructions) = instructions {
        args.insert(
            "instructions".to_string(),
            serde_json::Value::String(instructions),
        );
    }
    let _ = holly
        .send(InMsg::Oneshot {
            session: app.active_session_id().clone(),
            op: "compact".to_string(),
            args: serde_json::Value::Object(args),
        })
        .await;
}

/// Send `/show` as a no-override [`InMsg::SetGeneration`] query (#376): the
/// engine's merge is a no-op for an all-`None` override but still emits
/// [`OutEvent::GenerationChanged`][entanglement_core::OutEvent::GenerationChanged]
/// with the current effective params, which `App::handle_generation_changed`
/// renders as a status line — no pending persist is recorded, so this can never
/// be mistaken for a `/set` confirmation.
pub(super) async fn send_show(app: &App, holly: &Holly) {
    let _ = holly
        .send(InMsg::SetGeneration {
            session: app.active_session_id().clone(),
            overrides: entanglement_core::GenerationParams::default(),
        })
        .await;
}

/// Send `/name <text>` as an [`InMsg::SetSessionMeta`] naming the active
/// session (same raw-text re-parse pattern as [`send_compact`]). The
/// confirming `SessionMetaChanged` folds into the session view, so the sidebar
/// title updating *is* the confirmation — no status line. Bare `/name`
/// renders a one-line usage toast instead; no engine traffic.
pub(super) async fn send_name(app: &mut App, holly: &Holly, text: &str) {
    let Some(name) = crate::tui::commands::parse_name_args(text) else {
        app.set_toast("usage: /name <text>".to_string());
        return;
    };
    let _ = holly
        .send(InMsg::SetSessionMeta {
            session: app.active_session_id().clone(),
            name: Some(name),
            action: None,
            if_unset: false,
        })
        .await;
}

/// Send `/stop [--all]` (#6): the bare form routes through
/// `stop_command::request_stop`; `--all` fans the same call out to every
/// live session. A parse error (unknown argument) is rendered
/// as a status line instead.
pub(super) async fn send_stop(app: &mut App, holly: &Holly, text: &str) {
    let all = match crate::tui::commands::parse_all_flag(text, crate::tui::commands::Command::Stop)
    {
        Ok(all) => all,
        Err(e) => {
            app.record_status("stop", e);
            return;
        }
    };
    if all {
        let ids: Vec<_> = app
            .sessions()
            .into_iter()
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            crate::tui::stop_command::request_stop(app, holly, id).await;
        }
    } else {
        let target = app.active_session_id().clone();
        crate::tui::stop_command::request_stop(app, holly, target).await;
    }
}

/// Send `/pause [--all]` as one [`InMsg::PauseSession`] per live session (#6):
/// holds the session at `AgentState::Paused` without cancelling the turn or
/// evicting memory. `--all` fans out to every live session.
pub(super) async fn send_pause(app: &mut App, holly: &Holly, text: &str) {
    let all = match crate::tui::commands::parse_all_flag(text, crate::tui::commands::Command::Pause)
    {
        Ok(all) => all,
        Err(e) => {
            app.record_status("pause", e);
            return;
        }
    };
    if all {
        for (id, _) in app.sessions() {
            let _ = holly
                .send(InMsg::PauseSession {
                    session: id.clone(),
                })
                .await;
        }
    } else {
        let _ = holly
            .send(InMsg::PauseSession {
                session: app.active_session_id().clone(),
            })
            .await;
    }
}

/// Send `/continue [--all]` as one [`InMsg::ResumeSession`] per live session
/// (#6): lifts a hold placed by `/pause`. Idempotent on a non-paused session.
/// `--all` fans out to every live session.
pub(super) async fn send_resume(app: &mut App, holly: &Holly, text: &str) {
    let all =
        match crate::tui::commands::parse_all_flag(text, crate::tui::commands::Command::Continue) {
            Ok(all) => all,
            Err(e) => {
                app.record_status("resume", e);
                return;
            }
        };
    if all {
        for (id, _) in app.sessions() {
            let _ = holly
                .send(InMsg::ResumeSession {
                    session: id.clone(),
                })
                .await;
        }
    } else {
        let _ = holly
            .send(InMsg::ResumeSession {
                session: app.active_session_id().clone(),
            })
            .await;
    }
}

/// Ctrl+Space toggle (#6): pause the active session if it is currently running,
/// resume it if it is paused. Mirrors the idempotent wire semantics — the engine
/// treats `PauseSession` on an idle session and `ResumeSession` on a non-paused
/// one as no-ops, so toggling by observed state is safe.
pub(super) async fn send_pause_resume_toggle(app: &mut App, holly: &Holly) {
    use entanglement_core::AgentState;
    let msg = if app.state() == AgentState::Paused {
        InMsg::ResumeSession {
            session: app.active_session_id().clone(),
        }
    } else {
        InMsg::PauseSession {
            session: app.active_session_id().clone(),
        }
    };
    let _ = holly.send(msg).await;
}

/// Runs a `!bash` passthrough command head-side and injects the output into the
/// transcript (ADR-0030). `bash` is registered at startup like every other
/// built-in (ADR-0195), so the passthrough is always available — what it runs
/// is still the user's own hand-typed command, which is the consent the gate
/// was for. Honors `ENTANGLEMENT_SANDBOX` (the same opt-in as the model-facing
/// tool, #399/ADR-0104) so a passthrough command gets the same confinement.
pub(super) async fn run_bash_passthrough(app: &mut App, command: &str) {
    use entanglement_runtime::Tool;
    // The user's own command runs where the focused session works (#764).
    let cwd = app
        .workdir()
        .map_or_else(|| app.root().to_path_buf(), Into::into);
    let tool = crate::host::bash::BashTool::new(cwd)
        .with_sandbox(crate::host::sandbox::SandboxPolicy::from_env());
    let input = serde_json::json!({ "command": command }).to_string();
    let output = match tool.run(&input).await {
        Ok(out) => out,
        Err(e) => format!("[bash error] {e:#}"),
    };
    app.record_bash_passthrough(command.to_string(), output);
}

/// Expand and send a user slash command (#733) as an ordinary `Prompt`.
/// Returns `false` (nothing sent) when `text` names no user command, so an
/// unknown `/word` still goes to the model as typed. The transcript shows a
/// one-line notice naming the command's file, then the **expanded** prompt —
/// the exact text the model receives and the session log replays, so a
/// resumed session reads the same as the live one.
pub(super) async fn send_user_command(app: &mut App, holly: &Holly, text: &str) -> bool {
    let Some((command, prompt)) = app.slash().expand_user(text) else {
        return false;
    };
    app.record_notice(
        &format!("/{}", command.name),
        format!(
            "expanded from {} ({})",
            command.source,
            command.layer.label()
        ),
    );
    app.record_user_message(prompt.clone());
    if let Err(e) = holly
        .send(InMsg::prompt(app.active_session_id().clone(), prompt))
        .await
    {
        tracing::debug!("Failed to send user command prompt: {e}");
    }
    true
}
