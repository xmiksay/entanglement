//! The always-visible top row: where this TUI works (#762).
//!
//! The git facts come from a background probe ([`spawn_probe`], the `@file`
//! index pattern: off the draw path, delivered as [`Event::StatusLine`]).
//! It re-probes when a `bash`/`call`/`rhai` call finishes (those can switch
//! branches or create a repo) and otherwise only stats `HEAD` every 2 s, so
//! an out-of-band `git checkout` shows up without any tree walk.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ratatui::text::{Line, Span};
use tokio::sync::Notify;

use crate::config::statusline::{tokenize, StatuslineConfig};
use crate::tui::app::App;
use crate::tui::event::Event;
use crate::tui::format::{format_session_cost, short_id};
use crate::tui::theme::StatusTheme;

mod git;
mod render;

pub(crate) use git::GitInfo;
use render::{Piece, Slot, Vars};

const HEAD_POLL: Duration = Duration::from_secs(2);

/// Tools whose completion may have moved `HEAD` or created a repo.
pub(crate) fn refreshes_git(tool: &str) -> bool {
    matches!(tool, "bash" | "call" | "rhai")
}

/// The App-held half: the last probe result, the live-reloaded templates,
/// and the probe's wake-up handle (`None` in tests — nothing to wake).
#[derive(Default)]
pub(crate) struct StatusLineState {
    pub git: GitInfo,
    pub config: StatuslineConfig,
    pub refresh: Option<Arc<Notify>>,
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
    if let (Some(prompt), Some(window)) = (ledger.last_turn_prompt(), model.context_window) {
        if window > 0 {
            let pct = prompt as f64 / f64::from(window) * 100.0;
            v.insert("context_pct", format!("{pct:.0}%"));
        }
    }
    if let Some(action) = app.status_action() {
        v.insert("action", action.to_string());
    }
    if let Some(tasks) = app.task_list().and_then(|t| task_counts(t)) {
        v.insert("tasks", tasks);
    }
    v
}

/// `done/total` over a markdown checklist's `- [ ]` / `- [x]` lines; `None`
/// when it has none.
fn task_counts(markdown: &str) -> Option<String> {
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
    (total > 0).then(|| format!("{done}/{total}"))
}

/// The old top bar's counters, kept as fixed chrome after the right template
/// (and shed first): the session count and the waiting badge.
fn counters(app: &App) -> Vec<Piece> {
    let mut out = Vec::new();
    let sessions = app.sessions().len();
    if sessions > 1 {
        out.push((format!(" ({sessions} sessions)"), Slot::Dim));
    }
    let waiting = crate::tui::ui::alerts::background_attention(app).len();
    if waiting > 0 {
        out.push((format!(" ⚠ {waiting}"), Slot::Attention));
    }
    out
}

/// The rendered row for `cols` columns: left template, padding, right
/// template + counters.
pub(crate) fn status_line(app: &App, cols: u16) -> Line<'static> {
    let config = &app.status_line().config;
    let (left, right) = render::layout(
        &tokenize(&config.template).0,
        &tokenize(&config.right).0,
        &vars(app),
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
    fn task_counts_reads_checkbox_lines_only() {
        assert_eq!(
            task_counts("- [x] a\n* [ ] b\nplain").as_deref(),
            Some("1/2")
        );
        assert_eq!(task_counts("no boxes"), None);
    }

    #[test]
    fn only_exec_tools_trigger_a_reprobe() {
        assert!(refreshes_git("bash") && refreshes_git("call") && refreshes_git("rhai"));
        assert!(!refreshes_git("read"));
    }
}
