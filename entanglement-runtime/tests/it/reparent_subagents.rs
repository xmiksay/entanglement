//! Live sub-agents survive compaction (ADR-0234), end to end through the
//! executor: a background child of a compacting parent moves under the
//! successor — listed by its `poll`, resolvable by handle, reachable by
//! `agent_send`, its answer delivered there — and a sub-agent that compacts
//! keeps its place under its parent, behind the handle the parent holds.
//! Compaction is driven by the exact `Spawn { predecessor }` + `CloseSession`
//! pair the fork sends (`entanglement-core`'s `fork.rs`).

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, ApprovalScope, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, MessageRole, OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::policy::{GrantStore, ModeResolver, PermissionResolver};
use entanglement_runtime::retained_output::RetainedOutputRegistry;
use entanglement_runtime::tool_runner::ToolExecutor;
use entanglement_runtime::{Tool, ToolRegistry};
use tokio::sync::broadcast::Receiver;
use tokio::sync::Semaphore;

use crate::mode_support::{allow_all_table, perm_modes};

const WAIT: Duration = Duration::from_secs(5);

fn call(id: &str, name: &str, input: &str) -> LlmStream {
    stream_from_response(LlmResponse {
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            input: input.into(),
            provider_meta: None,
        }],
        ..Default::default()
    })
}

fn text(t: &str) -> LlmStream {
    stream_from_response(LlmResponse {
        text: t.into(),
        ..Default::default()
    })
}

/// The last real user message — skipping the per-round notices.
fn last_user(req: &LlmRequest<'_>) -> String {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User && !m.text().starts_with('['))
        .map(|m| m.text())
        .unwrap_or_default()
}

/// Scripted by the last user message. `spawn` launches a background child
/// (`child-wait`), which blocks in a gated `bash` and answers `child-answer`
/// once released; `poll` / `poll-handle <h>` / `send <h>` call `poll` or
/// `agent_send`; `followup` and `successor-task` answer with text.
struct Script;

#[async_trait]
impl Llm for Script {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let user = last_user(&req);
        let answered = req
            .messages
            .last()
            .is_some_and(|m| m.role == MessageRole::Tool);
        if answered {
            return Ok(text(if user.contains("child-wait") {
                "child-answer"
            } else {
                "done"
            }));
        }
        Ok(if user.contains("child-wait") {
            call("b1", "bash", r#"{"command":"wait"}"#)
        } else if user.contains("spawn") {
            let input = r#"{"agent":"general","prompt":"child-wait","background":true}"#;
            call("a1", "agent", input)
        } else if let Some(h) = user.strip_prefix("poll-handle ") {
            call(
                "p2",
                "poll",
                &format!(r#"{{"handle":"{h}","timeout_secs":5}}"#),
            )
        } else if let Some(h) = user.strip_prefix("send ") {
            let input = format!(r#"{{"agent_id":"{h}","prompt":"followup"}}"#);
            call("s1", "agent_send", &input)
        } else if user.contains("followup") {
            text("followed-up")
        } else if user.contains("successor-task") {
            text("successor-answer")
        } else {
            call("p1", "poll", "{}")
        })
    }
}

/// A `bash` that blocks until the test releases one permit.
struct GatedBash(Arc<Semaphore>);

#[async_trait]
impl Tool for GatedBash {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("bash")
    }
    async fn run(&self, _input: &str) -> anyhow::Result<String> {
        self.0.acquire().await?.forget();
        Ok("released".into())
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

fn spawn_engine(gate: Arc<Semaphore>, retained: RetainedOutputRegistry) -> Holly {
    let agents =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(|| Box::new(Script) as Box<dyn Llm>),
        agents: agents.clone(),
        ..EngineConfig::default()
    });
    let mode_table = allow_all_table();
    let mut reg = ToolRegistry::new();
    reg.register(GatedBash(gate));
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
        .spawn();
    holly
}

/// Launch `root`'s background child and wait until it is blocked in its
/// gated `bash` and `root`'s own turn has ended; returns the child.
async fn spawn_blocked_child(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    root: &SessionId,
) -> SessionId {
    holly
        .send(InMsg::prompt(root.clone(), "spawn"))
        .await
        .unwrap();
    let (mut child, mut blocked, mut done) = (None, false, false);
    while !(blocked && done) {
        let Ok(Ok(ev)) = tokio::time::timeout(WAIT, sub.recv()).await else {
            panic!("{root}: child {child:?}, blocked {blocked}, done {done}");
        };
        match ev {
            OutEvent::SessionStarted {
                session,
                parent: Some(p),
                ..
            } if &p == root => child = Some(session),
            OutEvent::ToolExec { session, tool, .. }
                if tool == "bash" && Some(&session) == child.as_ref() =>
            {
                blocked = true
            }
            OutEvent::Done { session, .. } if &session == root => done = true,
            _ => {}
        }
    }
    child.expect("set before its bash ran")
}

/// The exact frame pair a compaction fork sends for `source`.
async fn compact(holly: &Holly, source: &SessionId, successor: &SessionId, prompt: &str) {
    holly
        .send(InMsg::Spawn {
            session: successor.clone(),
            parent: None,
            predecessor: Some(source.clone()),
            agent: "general".into(),
            prompt: prompt.into(),
            user: None,
            model: None,
            mode: None,
            workdir: None,
        })
        .await
        .unwrap();
    holly
        .send(InMsg::CloseSession {
            session: source.clone(),
        })
        .await
        .unwrap();
}

/// `session`'s next output of `tool`, failing the test if `ended` shows up
/// in a `SessionEnded` first.
async fn next_output(
    sub: &mut Receiver<OutEvent>,
    session: &SessionId,
    tool: &str,
    ended: &SessionId,
) -> (String, bool) {
    while let Ok(Ok(ev)) = tokio::time::timeout(WAIT, sub.recv()).await {
        match ev {
            OutEvent::SessionEnded { session: s, .. } if &s == ended => {
                panic!("{ended} was closed")
            }
            OutEvent::ToolOutput {
                session: s,
                tool: t,
                output,
                is_error,
                ..
            } if &s == session && t == tool => return (output, is_error),
            _ => {}
        }
    }
    panic!("expected {session}'s {tool} output");
}

async fn prompt_for(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    session: &SessionId,
    prompt: &str,
    tool: &str,
    watched: &SessionId,
) -> (String, bool) {
    holly
        .send(InMsg::prompt(session.clone(), prompt))
        .await
        .unwrap();
    next_output(sub, session, tool, watched).await
}

#[tokio::test]
async fn a_compacting_parents_background_child_moves_to_the_successor() {
    let gate = Arc::new(Semaphore::new(0));
    let holly = spawn_engine(gate.clone(), RetainedOutputRegistry::new());
    let mut sub = holly.subscribe();
    let (pred, succ) = (SessionId::new("rp-pred"), SessionId::new("rp-succ"));
    let child = spawn_blocked_child(&holly, &mut sub, &pred).await;

    compact(&holly, &pred, &succ, "poll").await;

    // The successor's first turn lists the still-running child as its own.
    let (listing, _) = next_output(&mut sub, &succ, "poll", &child).await;
    assert!(
        listing.contains(&format!("[agent running] {child}")),
        "{listing}"
    );
    assert_eq!(
        holly.session_info(&child).and_then(|i| i.parent),
        Some(succ.clone()),
        "core re-linked the child"
    );

    // Released, the child answers; the successor collects it by handle.
    gate.add_permits(1);
    let (answer, err) = prompt_for(
        &holly,
        &mut sub,
        &succ,
        &format!("poll-handle {child}"),
        "poll",
        &child,
    )
    .await;
    assert!(!err && answer.contains("child-answer"), "{answer}");

    // And can steer it.
    let (reply, err) = prompt_for(
        &holly,
        &mut sub,
        &succ,
        &format!("send {child}"),
        "agent_send",
        &child,
    )
    .await;
    assert!(!err && reply.contains("followed-up"), "{reply}");
}

#[tokio::test]
async fn a_compacting_sub_agent_stays_behind_its_parents_handle() {
    let gate = Arc::new(Semaphore::new(0));
    let retained = RetainedOutputRegistry::new();
    let holly = spawn_engine(gate, retained.clone());
    let mut sub = holly.subscribe();
    let root = SessionId::new("sa-root");
    let child = spawn_blocked_child(&holly, &mut sub, &root).await;
    let child_succ = SessionId::new("sa-child-succ");

    compact(&holly, &child, &child_succ, "successor-task").await;

    // The parent's original handle resolves to the successor's answer.
    let (answer, err) = prompt_for(
        &holly,
        &mut sub,
        &root,
        &format!("poll-handle {child}"),
        "poll",
        &root,
    )
    .await;
    assert!(!err && answer.contains("successor-answer"), "{answer}");
    let info = holly.session_info(&child_succ).expect("successor is live");
    assert_eq!(info.parent, Some(root.clone()));
    assert!(!info.root);

    // The successor is inside the parent's subtree walk.
    let out = retained.register_text(Some(child_succ.clone()), "successor's".into());
    let (listing, _) = prompt_for(&holly, &mut sub, &root, "poll", "poll", &root).await;
    assert!(
        listing.contains(&format!(
            "{out} — retained text output, owned by {child_succ}"
        )),
        "{listing}"
    );
}

/// A `/fork` copy (ADR-0223) leaves its source alive: the source keeps its
/// child and the copy gets nothing.
#[tokio::test]
async fn a_fork_copy_does_not_take_its_sources_children() {
    let gate = Arc::new(Semaphore::new(0));
    let holly = spawn_engine(gate, RetainedOutputRegistry::new());
    let mut sub = holly.subscribe();
    let src = SessionId::new("fk-src");
    let child = spawn_blocked_child(&holly, &mut sub, &src).await;

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
        match ev {
            OutEvent::ParentChanged { .. } => panic!("a fork moved a child: {ev:?}"),
            OutEvent::SessionForked { session, .. } => break session,
            _ => {}
        }
    };

    let (mine, _) = prompt_for(&holly, &mut sub, &src, "poll", "poll", &child).await;
    assert!(mine.contains(&format!("[agent running] {child}")), "{mine}");
    let (theirs, _) = prompt_for(&holly, &mut sub, &fork, "poll", "poll", &child).await;
    assert!(!theirs.contains(&child.0), "{theirs}");
    assert_eq!(holly.session_info(&child).and_then(|i| i.parent), Some(src));
}
