//! The JSON a `tui.statusline.command` reads on stdin (#762 part B).
//!
//! Versioned (`"v": 1`): a field is only ever *added* under the same `v`;
//! renaming or removing one bumps it, so a script can check `v` and keep
//! working across upgrades. Raw values, not the template's formatted
//! strings — a script formats them itself. Absent facts are `null`, never
//! an empty string:
//!
//! ```json
//! {"v": 1, "session": "<full session id>", "cwd": "/abs/workdir",
//!  "root": "/abs/checkout" | null, "repo": "name" | null,
//!  "branch": "main" | "0123abc" | "?" | null, "worktree": "name" | null,
//!  "agent": "build", "mode": "build",
//!  "model": {"id": "glm-5.3", "name": "GLM-5.3"} | null,
//!  "cost_usd": 0.0123 | null, "context_pct": 42 | null,
//!  "action": "running tests" | null, "tasks": {"done": 1, "total": 3} | null,
//!  "workdir": "/abs/session/workdir" | null}
//! ```

use serde::Serialize;

use super::{context_pct, task_counts};
use crate::tui::app::App;

pub(crate) const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Payload {
    pub v: u32,
    pub session: String,
    /// The TUI's working directory (absolute).
    pub cwd: String,
    /// The enclosing checkout's top level; `null` outside git.
    pub root: Option<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub worktree: Option<String>,
    pub agent: String,
    pub mode: String,
    pub model: Option<Model>,
    /// `null` until a priced round lands.
    pub cost_usd: Option<f64>,
    /// Rounded to a whole percent so token jitter doesn't re-run the command.
    pub context_pct: Option<u32>,
    pub action: Option<String>,
    pub tasks: Option<Tasks>,
    /// The focused session's working directory (#764), `null` until one is set.
    pub workdir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Model {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Tasks {
    pub done: usize,
    pub total: usize,
}

/// The focused session's payload.
pub(crate) fn payload(app: &App) -> Payload {
    let git = &app.status_line().git;
    let in_git = git.toplevel.is_some();
    let model = app.model_info();
    let ledger = app.cost();
    Payload {
        v: VERSION,
        session: app.active_session_id().0.clone(),
        cwd: app.root().to_string_lossy().into_owned(),
        root: git
            .toplevel
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        repo: in_git.then(|| git.repo.clone()),
        branch: (!git.branch.is_empty()).then(|| git.branch.clone()),
        worktree: git.worktree.clone(),
        agent: app.agent().to_string(),
        mode: app.mode().to_string(),
        model: (!model.id.is_empty()).then(|| Model {
            id: model.id.clone(),
            name: model.display_name.clone(),
        }),
        cost_usd: (ledger.rounds() > 0 && ledger.cost_known()).then(|| ledger.cost_usd()),
        context_pct: context_pct(app).map(|p| p.round() as u32),
        action: app.status_action().map(str::to_string),
        tasks: app
            .task_list()
            .and_then(|t| task_counts(t))
            .map(|(done, total)| Tasks { done, total }),
        workdir: app.workdir().map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::statusline::GitInfo;
    use entanglement_core::{OutEvent, SessionId};

    #[test]
    fn payload_carries_every_documented_field() {
        let mut app = App::new_for_test(SessionId::new("a1b2c3d4-e5f6"));
        app.init_head_context("/w/proj/src".into());
        app.set_status_git(GitInfo {
            repo: "proj".into(),
            cwd: "~/w/proj/src".into(),
            cwd_rel: "src".into(),
            branch: "main".into(),
            worktree: Some("wt".into()),
            head: None,
            toplevel: Some("/w/proj".into()),
        });
        let sid = app.active_session_id().clone();
        app.handle_out_event(OutEvent::TaskList {
            session: sid,
            seq: 1,
            content: "- [x] a\n- [ ] b".into(),
        });
        let json = serde_json::to_value(payload(&app)).unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut expected = vec![
            "v",
            "session",
            "cwd",
            "root",
            "repo",
            "branch",
            "worktree",
            "agent",
            "mode",
            "model",
            "cost_usd",
            "context_pct",
            "action",
            "tasks",
            "workdir",
        ];
        expected.sort_unstable();
        let mut keys = keys;
        keys.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(json["v"], 1);
        assert_eq!(json["session"], "a1b2c3d4-e5f6");
        assert_eq!(json["cwd"], "/w/proj/src");
        assert_eq!(json["root"], "/w/proj");
        assert_eq!(json["repo"], "proj");
        assert_eq!(json["branch"], "main");
        assert_eq!(json["worktree"], "wt");
        assert_eq!(json["tasks"], serde_json::json!({"done": 1, "total": 2}));
        // Facts not yet known are null, not empty strings.
        assert!(json["cost_usd"].is_null() && json["action"].is_null());
        assert!(json["workdir"].is_null(), "no workdir set yet");
    }

    #[test]
    fn outside_git_the_repo_facts_are_null() {
        let mut app = App::new_for_test(SessionId::new("s"));
        app.set_status_git(GitInfo {
            repo: "~/notes".into(),
            cwd: "~/notes".into(),
            ..GitInfo::default()
        });
        let p = payload(&app);
        assert_eq!(
            (p.root, p.repo, p.branch, p.worktree),
            (None, None, None, None)
        );
    }
}
