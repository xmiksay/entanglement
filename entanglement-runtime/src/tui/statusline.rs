//! The always-visible top row: where this TUI works (#762).
//!
//! The git facts come from a background probe ([`spawn_probe`], the `@file`
//! index pattern: off the draw path, delivered as [`Event::StatusLine`]).
//! It re-probes when a `bash`/`call`/`rhai` call finishes (those can switch
//! branches or create a repo) and otherwise only stats `HEAD` every 2 s, so
//! an out-of-band `git checkout` shows up without any tree walk.
//!
//! With `tui.statusline.command` set, the left half is that command's
//! output instead of `template` ([`command`], part B); the right half and
//! the counters stay template-driven.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ratatui::text::{Line, Span};
use tokio::sync::Notify;

use crate::config::statusline::{tokenize, StatuslineConfig, Token};
use crate::tui::app::App;
use crate::tui::event::Event;
use crate::tui::format::{format_session_cost, short_id};
use crate::tui::theme::StatusTheme;
use entanglement_core::SessionId;

pub(crate) mod command;
mod git;
pub(crate) mod payload;
mod render;

pub(crate) use git::GitInfo;
use render::{Piece, Slot, Vars};

const HEAD_POLL: Duration = Duration::from_secs(2);

/// Tools whose completion may have moved `HEAD` or created a repo.
pub(crate) fn refreshes_git(tool: &str) -> bool {
    matches!(tool, "bash" | "call" | "rhai")
}

/// The App-held half: the last probe result, the live-reloaded templates,
/// the probe's wake-up handle (`None` in tests — nothing to wake), the
/// `command:` runner's state, and the roots whose session log is currently
/// not being written (#767).
#[derive(Default)]
pub(crate) struct StatusLineState {
    pub git: GitInfo,
    /// The TUI root, canonical (#764): what a session workdir — always
    /// canonical — is compared against without touching the disk per frame.
    pub root: Option<PathBuf>,
    pub config: StatuslineConfig,
    pub refresh: Option<Arc<Notify>>,
    pub command: command::CommandState,
    pub log_outages: HashSet<SessionId>,
}

/// Start the probe for `root`; the returned handle forces a re-probe.
pub(crate) fn spawn_probe(
    root: PathBuf,
    event_tx: tokio::sync::mpsc::Sender<Event>,
) -> Arc<Notify> {
    let wake = Arc::new(Notify::new());
    let notified = wake.clone();
    tokio::spawn(async move {
        let home = dirs::home_dir();
        let mut last: Option<GitInfo> = None;
        loop {
            let (r, h) = (root.clone(), home.clone());
            let Ok(info) = tokio::task::spawn_blocking(move || git::probe(&r, h.as_deref())).await
            else {
                tracing::warn!("status-line git probe failed");
                return;
            };
            let stamp = git::head_stamp(&info);
            if last.as_ref() != Some(&info) {
                if event_tx
                    .send(Event::StatusLine(info.clone()))
                    .await
                    .is_err()
                {
                    return; // the TUI quit
                }
                last = Some(info.clone());
            }
            // Sleep until a tool-driven wake-up or a HEAD mtime change.
            loop {
                tokio::select! {
                    // The TUI quit: stop polling rather than outlive it.
                    _ = event_tx.closed() => return,
                    _ = notified.notified() => break,
                    _ = tokio::time::sleep(HEAD_POLL) => {
                        if git::head_stamp(&info) != stamp {
                            break;
                        }
                    }
                }
            }
        }
    });
    wake
}

/// The placeholder values for the focused session.
fn vars(app: &App) -> Vars {
    let git = &app.status_line().git;
    let mut v = Vars::new();
    v.insert("repo", git.repo.clone());
    v.insert("cwd", git.cwd.clone());
    v.insert("cwd_rel", git.cwd_rel.clone());
    v.insert("branch", git.branch.clone());
    if let Some(name) = &git.worktree {
        v.insert("worktree", format!("[worktree: {name}]"));
    }
    v.insert("session", short_id(app.active_session_id()));
    v.insert("agent", app.agent().to_string());
    v.insert("mode", app.mode().to_string());
    if let Some(dir) = workdir_shown(app) {
        v.insert("workdir", dir);
    }
    let model = app.model_info();
    if !model.id.is_empty() {
        v.insert("model", model.display_name.clone());
    }
    let ledger = app.cost();
    if ledger.rounds() > 0 {
        let cost = format_session_cost(
            ledger.cost_known(),
            ledger.cost_usd(),
            ledger.prompt_total(),
            ledger.output_tokens(),
        );
        v.insert("cost", cost);
    }
    if let Some(pct) = context_pct(app) {
        v.insert("context_pct", format!("{pct:.0}%"));
    }
    if let Some(action) = app.status_action() {
        v.insert("action", action.to_string());
    }
    if let Some((done, total)) = app.task_list().and_then(|t| task_counts(t)) {
        v.insert("tasks", format!("{done}/{total}"));
    }
    v
}

/// The focused session's workdir when it differs from the TUI root (#764):
/// relative to the root, absolute outside it; `None` while they agree.
fn workdir_shown(app: &App) -> Option<String> {
    let dir = std::path::Path::new(app.workdir()?);
    let root = app.status_line().root.as_deref().unwrap_or(app.root());
    match dir.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => None,
        Ok(rel) => Some(rel.display().to_string()),
        Err(_) => Some(dir.display().to_string()),
    }
}

/// The last turn's prompt as a share of the model's context window.
fn context_pct(app: &App) -> Option<f64> {
    let prompt = app.cost().last_turn_prompt()?;
    let window = app.model_info().context_window.filter(|w| *w > 0)?;
    Some(prompt as f64 / f64::from(window) * 100.0)
}

/// `(done, total)` over a markdown checklist's `- [ ]` / `- [x]` lines;
/// `None` when it has none.
fn task_counts(markdown: &str) -> Option<(usize, usize)> {
    let (mut done, mut total) = (0, 0);
    for line in markdown.lines() {
        let item = line
            .trim_start()
            .trim_start_matches(['-', '*', '+'])
            .trim_start();
        match item.get(..3) {
            Some("[ ]") => total += 1,
            Some("[x]" | "[X]") => {
                total += 1;
                done += 1;
            }
            _ => {}
        }
    }
    (total > 0).then_some((done, total))
}

/// The old top bar's counters, kept as fixed chrome after the right template
/// (and shed first): the session count, the waiting badge, and the
/// session-log outage badge (#767; the transcript carries the detail).
fn counters(app: &App) -> Vec<Piece> {
    let mut out = Vec::new();
    let sessions = app.sessions().len();
    if sessions > 1 {
        out.push((format!(" ({sessions} sessions)"), Slot::Dim));
    }
    // Commands and relative paths no longer run where this TUI does (#764).
    if let Some(dir) = workdir_shown(app) {
        out.push((format!(" ⚠ cd {dir}"), Slot::Attention));
    }
    let waiting = crate::tui::ui::alerts::background_attention(app).len();
    if waiting > 0 {
        out.push((format!(" ⚠ {waiting}"), Slot::Attention));
    }
    match app.status_line().log_outages.len() {
        0 => {}
        1 => out.push((" ⚠ log".to_string(), Slot::Attention)),
        n => out.push((format!(" ⚠ log×{n}"), Slot::Attention)),
    }
    out
}

/// The rendered row for `cols` columns: left template, padding, right
/// template + counters.
pub(crate) fn status_line(app: &App, cols: u16) -> Line<'static> {
    let state = app.status_line();
    let mut vars = vars(app);
    // The command's line rides a pseudo-placeholder outside `PLACEHOLDERS`
    // (so no template can name it) that the shed order never empties.
    let left = match (state.config.command(), &state.command.line) {
        (Some(_), Some(line)) => {
            vars.insert("command", line.clone());
            vec![Token::Var("command")]
        }
        _ => tokenize(&state.config.template).0,
    };
    let (left, right) = render::layout(
        &left,
        &tokenize(&state.config.right).0,
        &vars,
        counters(app),
        usize::from(cols),
    );
    let theme = app.theme().status;
    let pad = usize::from(cols).saturating_sub(render::width(&left) + render::width(&right));
    let mut spans: Vec<Span<'static>> = left.into_iter().map(|p| styled(p, &theme)).collect();
    spans.push(Span::raw(" ".repeat(pad)));
    spans.extend(right.into_iter().map(|p| styled(p, &theme)));
    Line::from(spans)
}

fn styled((text, slot): Piece, theme: &StatusTheme) -> Span<'static> {
    let style = match slot {
        Slot::Repo => theme.repo,
        Slot::Path => theme.path,
        Slot::Branch => theme.branch,
        Slot::Worktree => theme.worktree,
        Slot::Text => theme.text,
        Slot::Dim => theme.dim,
        Slot::Attention => theme.attention,
    };
    Span::styled(text, style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::{OutEvent, SessionId};
    use ratatui::{backend::TestBackend, widgets::Paragraph, Terminal};

    fn row(app: &App, cols: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(cols, 1)).unwrap();
        terminal
            .draw(|f| f.render_widget(Paragraph::new(status_line(app, cols)), f.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..cols).map(|x| buffer[(x, 0)].symbol()).collect()
    }

    fn app_in_repo() -> App {
        let mut app = App::new_for_test(SessionId::new("a1b2c3d4-e5f6"));
        app.set_status_git(GitInfo {
            repo: "proj".into(),
            cwd: "~/w/proj/src".into(),
            cwd_rel: "src".into(),
            branch: "main".into(),
            worktree: Some("wt".into()),
            head: None,
            toplevel: Some("/w/proj".into()),
        });
        app
    }

    #[test]
    fn default_row_puts_the_project_left_and_the_session_right() {
        let text = row(&app_in_repo(), 60);
        assert!(
            text.starts_with("proj › src  ⎇ main [worktree: wt]"),
            "{text}"
        );
        assert!(text.trim_end().ends_with("a1b2c3d4"), "{text}");
    }

    #[test]
    fn a_reloaded_template_and_session_facts_render() {
        let mut app = app_in_repo();
        let sid = app.active_session_id().clone();
        app.handle_out_event(OutEvent::TaskList {
            session: sid,
            seq: 1,
            content: "- [x] a\n- [ ] b\n  - [X] c\nnotes".into(),
        });
        app.set_statusline_config(StatuslineConfig {
            template: "{repo} {tasks} {agent}/{mode} {model}".into(),
            right: String::new(),
            ..StatuslineConfig::default()
        });
        let text = row(&app, 60);
        assert!(text.starts_with("proj 2/3 build/build"), "{text}");
    }

    #[test]
    fn counters_ride_the_right_edge_and_go_first() {
        let mut app = app_in_repo();
        app.create_session();
        let wide = row(&app, 80);
        assert!(wide.trim_end().ends_with(" (2 sessions)"), "{wide}");
        let narrow = row(&app, 45);
        assert!(!narrow.contains("sessions"), "{narrow}");
        assert!(narrow.starts_with("proj › src"), "{narrow}");
    }

    #[test]
    fn log_outage_badge_shows_until_recovered() {
        let mut app = app_in_repo();
        let root = SessionId::new("root-1");
        assert!(!row(&app, 80).contains("⚠ log"));
        app.handle_out_event(OutEvent::PersistenceDegraded {
            root: root.clone(),
            error: "disk full".into(),
        });
        let degraded = row(&app, 80);
        assert!(degraded.trim_end().ends_with("⚠ log"), "{degraded}");
        // A second outage on another root counts; a repeat of the first doesn't.
        app.handle_out_event(OutEvent::PersistenceDegraded {
            root: SessionId::new("root-2"),
            error: "disk full".into(),
        });
        app.handle_out_event(OutEvent::PersistenceDegraded {
            root: root.clone(),
            error: "disk full".into(),
        });
        assert!(row(&app, 80).contains("⚠ log×2"));
        app.handle_out_event(OutEvent::PersistenceRecovered {
            root: SessionId::new("root-2"),
            lost: 1,
        });
        app.handle_out_event(OutEvent::PersistenceRecovered { root, lost: 4 });
        let healthy = row(&app, 80);
        assert!(!healthy.contains("⚠ log"), "{healthy}");
    }

    fn log_lines(app: &App, id: &SessionId) -> usize {
        use crate::tui::session_view::TranscriptEntry;
        let sessions = app.sessions();
        let (_, view) = sessions.iter().find(|(s, _)| *s == id).expect("view");
        view.transcript()
            .iter()
            .filter(
                |e| matches!(e, TranscriptEntry::ToolOutput { tool: Some(t), .. } if t == "log"),
            )
            .count()
    }

    #[test]
    fn a_root_that_stops_mid_outage_or_a_lag_clears_the_badge() {
        let mut app = app_in_repo();
        let focused = app.active_session_id().clone();
        let other = SessionId::new("other-root");
        // The other root's view exists (it started), so its notes go there.
        app.handle_out_event(OutEvent::SessionStarted {
            session: other.clone(),
            parent: None,
            predecessor: None,
            agent: "build".into(),
            model: None,
            root: true,
            ts: 0,
            user: None,
        });
        app.handle_out_event(OutEvent::PersistenceDegraded {
            root: other.clone(),
            error: "down".into(),
        });
        assert!(row(&app, 80).contains("⚠ log"));
        assert_eq!((log_lines(&app, &other), log_lines(&app, &focused)), (1, 0));
        app.handle_out_event(OutEvent::SessionEnded {
            session: other.clone(),
            ts: 1,
        });
        assert!(!row(&app, 80).contains("⚠ log"));
        assert_eq!(log_lines(&app, &other), 2, "the stop leaves a note");
        // A late Recovered for it is now a no-op.
        app.handle_out_event(OutEvent::PersistenceRecovered {
            root: other.clone(),
            lost: 1,
        });
        assert_eq!(log_lines(&app, &other), 2);

        // An unknown root's notes land in the focused view; a lag clears.
        app.handle_out_event(OutEvent::PersistenceDegraded {
            root: SessionId::new("unseen"),
            error: "down".into(),
        });
        assert_eq!(log_lines(&app, &focused), 1);
        app.engine_events_lagged();
        assert!(!row(&app, 80).contains("⚠ log"));
        assert_eq!(log_lines(&app, &focused), 2);
        app.engine_events_lagged(); // nothing raised: no further note
        assert_eq!(log_lines(&app, &focused), 2);
    }

    #[test]
    fn a_session_workdir_away_from_the_root_warns() {
        let mut app = app_in_repo();
        app.set_status_root(Some("/w/proj".into()));
        let sid = app.active_session_id().clone();
        assert!(!row(&app, 80).contains('⚠'), "no workdir, no warning");
        app.handle_out_event(OutEvent::WorkdirChanged {
            session: sid.clone(),
            dir: "/w/proj/crates/x".into(),
        });
        let text = row(&app, 80);
        assert!(text.contains("⚠ cd crates/x"), "{text}");
        app.set_statusline_config(StatuslineConfig {
            template: "{repo} [{workdir}]".into(),
            right: String::new(),
            ..StatuslineConfig::default()
        });
        assert!(row(&app, 80).starts_with("proj [crates/x]"));
        app.handle_out_event(OutEvent::WorkdirChanged {
            session: sid.clone(),
            dir: "/elsewhere".into(),
        });
        assert!(row(&app, 80).contains("⚠ cd /elsewhere"));
        app.handle_out_event(OutEvent::WorkdirChanged {
            session: sid,
            dir: "/w/proj".into(),
        });
        let back = row(&app, 80);
        assert!(!back.contains('⚠') && back.starts_with("proj"), "{back}");
    }

    #[test]
    fn task_counts_reads_checkbox_lines_only() {
        assert_eq!(task_counts("- [x] a\n* [ ] b\nplain"), Some((1, 2)));
        assert_eq!(task_counts("no boxes"), None);
    }

    #[test]
    fn only_exec_tools_trigger_a_reprobe() {
        assert!(refreshes_git("bash") && refreshes_git("call") && refreshes_git("rhai"));
        assert!(!refreshes_git("read"));
    }
}
