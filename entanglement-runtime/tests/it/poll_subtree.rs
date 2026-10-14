//! Integration tests for the subtree section of a handle-less `poll` (#829):
//! a sub-agent's open `ask_user` question and parked approval, plus retained
//! outputs, surface in the parent's own `poll {}` — and nothing from a
//! sibling tree does. Driven end to end through the executor, so the
//! registries are the real ones the ladder hands `poll`.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, ApprovalScope, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, MessageRole, OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::host::jobs::JobRegistry;
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::policy::{GrantStore, ModeResolver, PermissionResolver};
use entanglement_runtime::retained_output::RetainedOutputRegistry;
use entanglement_runtime::tool_runner::ToolExecutor;
use entanglement_runtime::{Tool, ToolRegistry};
use tokio::sync::broadcast::Receiver;

use crate::mode_support::perm_modes;

const WAIT: Duration = Duration::from_secs(5);
const ASK: &str = r#"{"questions":[{"question":"Which DB?","options":[{"label":"Postgres"}]}]}"#;

fn calls(calls: &[(&str, &str, &str)]) -> LlmStream {
    stream_from_response(LlmResponse {
        tool_calls: calls
            .iter()
            .map(|(id, name, input)| ToolCall {
                id: (*id).into(),
                name: (*name).into(),
                input: (*input).into(),
                provider_meta: None,
            })
            .collect(),
        ..Default::default()
    })
}

/// The last real user message — skipping the per-round mode/workdir notices
/// the engine appends to every request.
fn last_user(req: &LlmRequest<'_>) -> String {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User && !m.text().starts_with('['))
        .map(|m| m.text())
        .unwrap_or_default()
}

/// Scripted by the last user message: `spawn` launches a background child
/// (prompt `child-task`), which parks one `ask_user` and one `bash` in a
/// single batch; `poll` calls a handle-less `poll`. Every turn ends once its
/// tool results are back.
struct SubtreeLlm;

#[async_trait]
impl Llm for SubtreeLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let answered = req
            .messages
            .last()
            .is_some_and(|m| m.role == MessageRole::Tool);
        if answered {
            return Ok(stream_from_response(LlmResponse {
                text: "done".into(),
                ..Default::default()
            }));
        }
        let user = last_user(&req);
        Ok(if user.contains("child-task") {
            calls(&[
                ("q1", "ask_user", ASK),
                ("b1", "bash", r#"{"command":"ls"}"#),
            ])
        } else if user.contains("spawn") {
            let input = r#"{"agent":"general","prompt":"child-task","background":true}"#;
            calls(&[("a1", "agent", input)])
        } else if let Some(handle) = user.strip_prefix("poll-handle ") {
            let input = format!(r#"{{"handle":"{handle}","timeout_secs":1}}"#);
            calls(&[("p2", "poll", &input)])
        } else {
            calls(&[("p1", "poll", "{}")])
        })
    }
}

struct EchoBash;
#[async_trait]
impl Tool for EchoBash {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("bash")
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran: {input}"))
    }
    fn capabilities(&self) -> &'static [entanglement_runtime::capability::Capability] {
        &[entanglement_runtime::capability::Capability::Exec]
    }
}

struct NoGrants;
#[async_trait]
impl GrantStore for NoGrants {
    fn is_granted(&self, _: &SessionId, _: &str, _: Option<&str>, _: &str) -> bool {
        false
    }
    async fn record(&self, _: &SessionId, _: &str, _: Option<&str>, _: ApprovalScope, _: &str) {}
    fn forget_session(&self, _: &SessionId) {}
}

/// An engine whose one mode grades every graded tool `Ask`, so the child's
/// `bash` parks; `retained` is the registry the executor's `poll` reads.
fn spawn_engine(retained: RetainedOutputRegistry, jobs: JobRegistry) -> Holly {
    let agents =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(|| Box::new(SubtreeLlm) as Box<dyn Llm>),
        agents: agents.clone(),
        ..EngineConfig::default()
    });
    let mode_table = Arc::new(
        ModeTable::new(vec![Mode {
            name: "build".into(),
            default: Permission::Ask,
            // Spawning is graded (ADR-0232); allow it like the built-ins do,
            // so the child's own calls are what park.
            rules: Rules::from_lists(&[], &["control".to_string()], &[]),
            limits: Limits::default(),
            sandbox: None,
            sandbox_network: false,
        }])
        .expect("mode table is valid"),
    );
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    let tools = reg.shared();
    let modes = perm_modes();
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        modes.clone(),
        mode_table.clone(),
        tools.clone(),
        PermissionProfile::new(Permission::Allow),
        None,
    ));
    ToolExecutor::builder(&holly, tools)
        .agents(agents)
        .base(PermissionProfile::new(Permission::Allow))
        .active(Default::default())
        .perm_modes(modes)
        .resolver(resolver)
        .grants(Arc::new(NoGrants))
        .mode_table(mode_table)
        .retained(retained)
        .jobs(jobs)
        .spawn();
    holly
}

/// Launch `root`'s background child and wait until it has parked both its
/// question and its `bash` approval, and `root`'s own turn has ended (so its
/// next prompt starts a fresh turn); returns the child's session.
async fn spawn_parked_child(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    root: &SessionId,
) -> SessionId {
    holly
        .send(InMsg::prompt(root.clone(), "spawn"))
        .await
        .unwrap();
    let (mut asked, mut parked, mut done) = (None, None, false);
    while asked.is_none() || parked.is_none() || !done {
        let Ok(Ok(ev)) = tokio::time::timeout(WAIT, sub.recv()).await else {
            panic!("{root}: asked {asked:?}, parked {parked:?}, done {done}");
        };
        match ev {
            OutEvent::UserQuestion { session, .. } if &session != root => asked = Some(session),
            OutEvent::ToolRequest { session, tool, .. } if tool == "bash" && &session != root => {
                parked = Some(session)
            }
            OutEvent::Done { session, .. } if &session == root => done = true,
            _ => {}
        }
    }
    assert_eq!(asked, parked, "one child parks both");
    asked.expect("loop exits only once set")
}

/// Prompt `root` to call a handle-less `poll` and return its output.
async fn poll_output(holly: &Holly, sub: &mut Receiver<OutEvent>, root: &SessionId) -> String {
    prompt_poll(holly, sub, root, "poll").await.0
}

/// Prompt `root` with `prompt` (`poll`, or `poll-handle <h>`) and return its
/// `poll` output and `is_error` flag.
async fn prompt_poll(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    root: &SessionId,
    prompt: &str,
) -> (String, bool) {
    holly
        .send(InMsg::prompt(root.clone(), prompt))
        .await
        .unwrap();
    next_poll(sub, root).await
}

async fn next_poll(sub: &mut Receiver<OutEvent>, root: &SessionId) -> (String, bool) {
    while let Ok(Ok(ev)) = tokio::time::timeout(WAIT, sub.recv()).await {
        if let OutEvent::ToolOutput {
            session,
            tool,
            output,
            is_error,
            ..
        } = ev
        {
            if &session == root && tool == "poll" {
                return (output, is_error);
            }
        }
    }
    panic!("expected {root}'s poll output");
}

#[tokio::test]
async fn idle_subtree_leaves_the_handle_less_poll_unchanged() {
    let holly = spawn_engine(RetainedOutputRegistry::new(), JobRegistry::new());
    let mut sub = holly.subscribe();
    let root = SessionId::new("idle-root");
    let out = poll_output(&holly, &mut sub, &root).await;
    assert_eq!(out, "poll: no pending operations for this session.");
}

#[tokio::test]
async fn childs_question_approval_and_retained_output_show_but_a_siblings_do_not() {
    let retained = RetainedOutputRegistry::new();
    let holly = spawn_engine(retained.clone(), JobRegistry::new());
    let mut sub = holly.subscribe();
    let (root, other) = (SessionId::new("tree-root"), SessionId::new("other-root"));

    let child = spawn_parked_child(&holly, &mut sub, &root).await;
    let sibling = spawn_parked_child(&holly, &mut sub, &other).await;
    let own = retained.register_text(Some(root.clone()), "root's own".into());
    let childs = retained.register_text(Some(child.clone()), "child's".into());
    let siblings = retained.register_text(Some(sibling.clone()), "sibling's".into());

    let out = poll_output(&holly, &mut sub, &root).await;
    let (listing, section) = out
        .split_once("\n\nsubtree (this session and its sub-agents):\n")
        .unwrap_or_else(|| panic!("subtree section missing: {out}"));
    // Own operations first, unchanged in shape: the one background child.
    assert!(
        listing.starts_with("poll: 1 pending operation(s):"),
        "{out}"
    );
    assert!(
        listing.contains(&format!("[agent running] {child}")),
        "{out}"
    );

    assert!(
        section.contains(&format!("q1 — session {child}, 1 question(s)")),
        "{out}"
    );
    assert!(
        section.contains(&format!("b1 — tool approval for `bash`, session {child}")),
        "{out}"
    );
    assert!(
        section.contains(&format!("{own} — retained text output, yours")),
        "{out}"
    );
    assert!(
        section.contains(&format!(
            "{childs} — retained text output, owned by {child}"
        )),
        "{out}"
    );

    for leaked in [sibling.to_string(), siblings, other.to_string()] {
        assert!(
            !out.contains(&leaked),
            "{leaked} leaked into {root}'s poll: {out}"
        );
    }
}

fn sleeper() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.args(["-c", "sleep 30"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    entanglement_runtime::host::exec::own_process_group(&mut cmd);
    cmd
}

/// ADR-0231: the exact `Spawn{predecessor}` + `CloseSession` pair a
/// compaction fork sends hands the predecessor's running job and retained
/// output to the successor — listed by its handle-less `poll`, resolvable by
/// handle, and the old id loses them.
#[tokio::test]
async fn a_compaction_successor_takes_over_its_predecessors_job_and_retained_output() {
    let (retained, jobs) = (RetainedOutputRegistry::new(), JobRegistry::new());
    let holly = spawn_engine(retained.clone(), jobs.clone());
    let mut sub = holly.subscribe();
    let (pred, succ) = (SessionId::new("pred"), SessionId::new("succ"));
    // A live predecessor, at rest, as at the end of a compacting turn.
    poll_output(&holly, &mut sub, &pred).await;
    let job = jobs
        .spawn("sleep 30".into(), sleeper(), WAIT * 4, Some(pred.clone()))
        .expect("spawn sleeper");
    let out = retained.register_text(Some(pred.clone()), "pred's output".into());

    holly
        .send(InMsg::Spawn {
            session: succ.clone(),
            parent: None,
            predecessor: Some(pred.clone()),
            agent: "general".into(),
            prompt: "poll".into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    holly
        .send(InMsg::CloseSession {
            session: pred.clone(),
        })
        .await
        .unwrap();

    let (listing, _) = next_poll(&mut sub, &succ).await;
    assert!(
        listing.contains(&format!("[job running] {job}")),
        "{listing}"
    );
    assert!(
        listing.contains(&format!("{out} — retained text output, yours")),
        "{listing}"
    );

    let (paged, err) = prompt_poll(&holly, &mut sub, &succ, &format!("poll-handle {out}")).await;
    assert!(!err && paged.contains("pred's output"), "{paged}");
    let (joined, err) = prompt_poll(&holly, &mut sub, &succ, &format!("poll-handle {job}")).await;
    assert!(!err && joined.contains(&job), "{joined}");

    assert!(jobs.snapshot(Some(&pred)).is_empty());
    assert!(retained.page(&out, &pred, 0, 30).is_none());
    let _ = jobs.poll(&job, &succ, true, 0).await;
}

/// A `/fork` copy (ADR-0223) leaves its source alive, so the source keeps
/// its job: the fork's `SessionStarted` carries no predecessor.
#[tokio::test]
async fn a_fork_copy_does_not_take_its_sources_job() {
    let jobs = JobRegistry::new();
    let holly = spawn_engine(RetainedOutputRegistry::new(), jobs.clone());
    let mut sub = holly.subscribe();
    let src = SessionId::new("fork-src");
    poll_output(&holly, &mut sub, &src).await;
    let job = jobs
        .spawn("sleep 30".into(), sleeper(), WAIT * 4, Some(src.clone()))
        .expect("spawn sleeper");

    holly
        .send(InMsg::ForkSession {
            session: src.clone(),
            at_turn: None,
        })
        .await
        .unwrap();
    let fork = loop {
        let Ok(Ok(ev)) = tokio::time::timeout(WAIT, sub.recv()).await else {
            panic!("no SessionForked for {src}");
        };
        if let OutEvent::SessionForked {
            session,
            forked_from,
            ..
        } = ev
        {
            assert_eq!(forked_from, src);
            break session;
        }
    };

    // The lifecycle loop folds the fork's `SessionStarted` before it routes
    // this later `ToolExec`, so these polls observe the settled ownership.
    let mine = poll_output(&holly, &mut sub, &src).await;
    assert!(mine.contains(&format!("[job running] {job}")), "{mine}");
    let theirs = poll_output(&holly, &mut sub, &fork).await;
    assert!(!theirs.contains(&job), "{theirs}");
    let _ = jobs.poll(&job, &src, true, 0).await;
}

/// The lossless self-heal: work still owned by the predecessor *after* the
/// successor's `SessionStarted` was already folded (a lagged drop looks the
/// same to the registries) is adopted by the successor's next `poll`, read
/// from the supervisor's directory rather than the broadcast.
#[tokio::test]
async fn a_successor_poll_adopts_work_the_lifecycle_fold_missed() {
    let (retained, jobs) = (RetainedOutputRegistry::new(), JobRegistry::new());
    let holly = spawn_engine(retained.clone(), jobs.clone());
    let mut sub = holly.subscribe();
    let (pred, succ) = (SessionId::new("late-pred"), SessionId::new("late-succ"));
    poll_output(&holly, &mut sub, &pred).await;
    holly
        .send(InMsg::Spawn {
            session: succ.clone(),
            parent: None,
            predecessor: Some(pred.clone()),
            agent: "general".into(),
            prompt: "poll".into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    holly
        .send(InMsg::CloseSession {
            session: pred.clone(),
        })
        .await
        .unwrap();
    // The fold has run by now: the successor's first poll was routed after it.
    let (first, _) = next_poll(&mut sub, &succ).await;
    assert_eq!(first, "poll: no pending operations for this session.");

    let job = jobs
        .spawn("sleep 30".into(), sleeper(), WAIT * 4, Some(pred.clone()))
        .expect("spawn sleeper");
    let out = retained.register_text(Some(pred.clone()), "late output".into());

    let listing = poll_output(&holly, &mut sub, &succ).await;
    assert!(
        listing.contains(&format!("[job running] {job}")),
        "{listing}"
    );
    assert!(
        listing.contains(&format!("{out} — retained text output, yours")),
        "{listing}"
    );
    let (paged, err) = prompt_poll(&holly, &mut sub, &succ, &format!("poll-handle {out}")).await;
    assert!(!err && paged.contains("late output"), "{paged}");
    let _ = jobs.poll(&job, &succ, true, 0).await;
}
