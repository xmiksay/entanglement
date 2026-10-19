//! A background script's nested `Ask` outlives its session's end (#849,
//! ADR-0232): the executor unwinds a closed or hibernated session's parked
//! `ToolExec` cards, but a detached script's card is no `ToolExec` — nothing
//! would ever re-offer it — so it stays answerable after a compaction (the
//! successor inherits the script, ADR-0231) or a hibernate.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmFactory, LlmRequest, LlmResponse,
    LlmStream, MessageRole, OutEvent, PendingApproval, Permission, PermissionProfile, SessionId,
    ToolCall,
};
use entanglement_runtime::host::{host_tools, BashTool};
use entanglement_runtime::policy::{DefaultGrantStore, ModeResolver};
use entanglement_runtime::script_ops::ScriptRegistry;
use entanglement_runtime::tool_names::RHAI_TOOL;
use entanglement_runtime::tool_runner::ToolExecutor;

use super::{answer_launch_cards, is_launch_card, mode_table_for, perm_modes, TempDir};

const WAIT: Duration = Duration::from_secs(5);

/// A turn prompted `go` launches the background script; one prompted `edit`
/// makes a direct (attached) `edit` call; one prompted `poll` makes a
/// handle-less `poll`; every other turn, or a later round of any, just
/// answers.
struct LaunchOnGo;

#[async_trait]
impl Llm for LaunchOnGo {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let last_user = req
            .messages
            .iter()
            .rposition(|m| m.role == MessageRole::User && !m.text().starts_with("[mode: "));
        let fresh_turn = last_user.is_some_and(|i| {
            !req.messages[i..]
                .iter()
                .any(|m| m.role == MessageRole::Tool)
        });
        let prompt = last_user.map(|i| req.messages[i].text());
        let call = |name: &str, input: serde_json::Value| ToolCall {
            id: "t1".into(),
            name: name.into(),
            input: input.to_string(),
            provider_meta: None,
        };
        let next = match prompt.as_deref() {
            Some("go") if fresh_turn => Some(call(
                RHAI_TOOL,
                serde_json::json!({
                    "script": r#"edit("f.txt", "", "approved\n"); "finished""#,
                    "background": true,
                }),
            )),
            Some("edit") if fresh_turn => Some(call(
                "edit",
                serde_json::json!({"path": "g.txt", "oldString": "", "newString": "x"}),
            )),
            Some("poll") if fresh_turn => Some(call("poll", serde_json::json!({}))),
            _ => None,
        };
        Ok(stream_from_response(LlmResponse {
            text: if next.is_some() { "" } else { "ok" }.into(),
            tool_calls: next.into_iter().collect(),
            ..Default::default()
        }))
    }
}

/// `edit` asks; everything else (the launch included) is allowed.
pub(super) fn engine(root: &std::path::Path, scripts: ScriptRegistry) -> Holly {
    engine_with(
        root,
        scripts,
        Arc::new(|| Box::new(LaunchOnGo) as Box<dyn Llm>),
    )
}

/// [`engine`] over another scripted model. The host set carries `bash` too.
pub(super) fn engine_with(
    root: &std::path::Path,
    scripts: ScriptRegistry,
    llm_factory: LlmFactory,
) -> Holly {
    let agents =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let holly = Holly::spawn(EngineConfig {
        llm_factory,
        agents: agents.clone(),
        ..EngineConfig::default()
    });
    answer_launch_cards(&holly);
    let base = PermissionProfile::new(Permission::Allow);
    let table = mode_table_for(&base.clone().with("edit", Permission::Ask));
    let mut tools = host_tools(root.to_path_buf());
    // Lets a script wait on the filesystem without a busy spin (#884).
    tools.register(BashTool::new(root.to_path_buf()));
    let tools = tools.shared();
    let modes = perm_modes();
    let resolver = Arc::new(ModeResolver::new(
        modes.clone(),
        table.clone(),
        tools.clone(),
        base.clone(),
        None,
    ));
    let _executor = ToolExecutor::builder(&holly, tools)
        .agents(agents)
        .base(base)
        .perm_modes(modes)
        .resolver(resolver)
        .grants(Arc::new(DefaultGrantStore::load_from(None)))
        .mode_table(table)
        .scripts(scripts)
        .spawn();
    holly
}

/// Launch the script on `pred` and return the nested `edit` card's id.
pub(super) async fn park_nested_edit(holly: &Holly, pred: &SessionId) -> String {
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(pred.clone(), "go")).await.unwrap();
    loop {
        match tokio::time::timeout(WAIT, sub.recv()).await {
            Ok(Ok(ev)) if is_launch_card(&ev) => {}
            Ok(Ok(OutEvent::ToolRequest {
                request_id, tool, ..
            })) if tool.contains("edit") => return request_id,
            Ok(Ok(_)) => {}
            other => panic!("expected the script's nested edit card: {other:?}"),
        }
    }
}

async fn await_gone(sub: &mut tokio::sync::broadcast::Receiver<OutEvent>, sid: &SessionId) {
    loop {
        match tokio::time::timeout(WAIT, sub.recv()).await {
            Ok(Ok(OutEvent::SessionEnded { session, .. }))
            | Ok(Ok(OutEvent::SessionHibernated { session, .. }))
                if &session == sid =>
            {
                return
            }
            Ok(Ok(_)) => {}
            other => panic!("expected {sid} to end: {other:?}"),
        }
    }
}

/// Approve the card (keyed on the predecessor, which parked it) and wait
/// for the script, polled as `owner`, to finish.
pub(super) async fn approve_and_finish(
    holly: &Holly,
    scripts: &ScriptRegistry,
    pred: &SessionId,
    owner: &SessionId,
    request_id: String,
    root: &std::path::Path,
) {
    let handle = tokio::time::timeout(WAIT, async {
        loop {
            if let Some(op) = scripts.snapshot_ops(Some(owner)).into_iter().next() {
                return op.handle;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{owner} never owned the background script"));
    holly
        .send(InMsg::Approve {
            session: pred.clone(),
            request_id,
            scope: Default::default(),
            mode: None,
            run_all: false,
        })
        .await
        .unwrap();
    let mut output = Vec::new();
    for _ in 0..20 {
        let polled = scripts
            .poll(&handle, owner, false, 1)
            .await
            .expect("the owner can poll the script");
        output.extend(polled.output);
        if !polled.running {
            assert!(!polled.is_error, "{}", String::from_utf8_lossy(&output));
            break;
        }
    }
    assert_eq!(
        std::fs::read_to_string(root.join("f.txt")).unwrap(),
        "approved\n",
        "the approved nested edit ran"
    );
}

#[tokio::test]
async fn a_background_scripts_card_survives_a_compaction() {
    let dir = TempDir::new("bg-compact");
    let scripts = ScriptRegistry::new();
    let holly = engine(&dir.path, scripts.clone());
    let (pred, succ) = (SessionId::new("pred"), SessionId::new("succ"));
    let request_id = park_nested_edit(&holly, &pred).await;

    // The exact pair a compaction fork sends (ADR-0205/0231).
    let mut sub = holly.subscribe();
    holly
        .send(InMsg::Spawn {
            session: succ.clone(),
            parent: None,
            predecessor: Some(pred.clone()),
            agent: "general".into(),
            prompt: "continue".into(),
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
    await_gone(&mut sub, &pred).await;

    approve_and_finish(&holly, &scripts, &pred, &succ, request_id, &dir.path).await;
}

#[tokio::test]
async fn a_background_scripts_card_survives_a_hibernate() {
    let dir = TempDir::new("bg-hibernate");
    let scripts = ScriptRegistry::new();
    let holly = engine(&dir.path, scripts.clone());
    let pred = SessionId::new("pred");
    let request_id = park_nested_edit(&holly, &pred).await;

    let mut sub = holly.subscribe();
    holly
        .send(InMsg::HibernateSession {
            session: pred.clone(),
        })
        .await
        .unwrap();
    await_gone(&mut sub, &pred).await;

    approve_and_finish(&holly, &scripts, &pred, &pred, request_id, &dir.path).await;
}

/// The pending listing's request ids for `sid` (`ListPending`, #726).
async fn pending_ids(holly: &Holly, sid: &SessionId, correlation: &str) -> Vec<String> {
    pending_list(holly, sid, correlation)
        .await
        .into_iter()
        .map(|a| a.request_id)
        .collect()
}

/// The pending listing's entries for `sid` (`ListPending`, #726).
pub(super) async fn pending_list(
    holly: &Holly,
    sid: &SessionId,
    correlation: &str,
) -> Vec<PendingApproval> {
    let mut sub = holly.subscribe();
    holly
        .send_from_wire(InMsg::ListPending {
            correlation_id: correlation.into(),
            root: None,
        })
        .await
        .expect("ListPending is wire-allowed");
    loop {
        match tokio::time::timeout(WAIT, sub.recv()).await {
            Ok(Ok(OutEvent::PendingList {
                correlation_id,
                approvals,
            })) if correlation_id == correlation => {
                return approvals
                    .into_iter()
                    .filter(|a| &a.session == sid)
                    .collect()
            }
            Ok(Ok(_)) => {}
            other => panic!("expected a PendingList reply: {other:?}"),
        }
    }
}

/// A `Stop` is the user's escape hatch (ADR-0232): it unwinds the turn's own
/// card *and* a background script's — the TUI clears its card view on Stop
/// and an attended park has no timeout, so a card left parked would hang the
/// script for good. The script itself is not aborted (ADR-0185): its nested
/// call fails with "rhai run stopped" and the run ends.
#[tokio::test]
async fn a_stop_unwinds_the_turns_card_and_a_background_scripts() {
    let dir = TempDir::new("bg-stop");
    let scripts = ScriptRegistry::new();
    let holly = engine(&dir.path, scripts.clone());
    let sid = SessionId::new("s1");
    let script_card = park_nested_edit(&holly, &sid).await;
    let handle = scripts
        .snapshot_ops(Some(&sid))
        .into_iter()
        .next()
        .expect("the background script is registered")
        .handle;

    let mut sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "edit"))
        .await
        .unwrap();
    let turn_card = loop {
        match tokio::time::timeout(WAIT, sub.recv()).await {
            Ok(Ok(OutEvent::ToolRequest {
                request_id, tool, ..
            })) if tool == "edit" => break request_id,
            Ok(Ok(_)) => {}
            other => panic!("expected the turn's own edit card: {other:?}"),
        }
    };
    let mut both = pending_ids(&holly, &sid, "before").await;
    both.sort();
    let mut want = vec![script_card, turn_card];
    want.sort();
    assert_eq!(both, want);

    holly
        .send(InMsg::Stop {
            session: sid.clone(),
        })
        .await
        .unwrap();

    let mut output = Vec::new();
    let mut finished = false;
    for _ in 0..20 {
        let polled = scripts
            .poll(&handle, &sid, false, 1)
            .await
            .expect("the owner can poll the script");
        output.extend(polled.output);
        if !polled.running {
            finished = true;
            break;
        }
    }
    let output = String::from_utf8_lossy(&output);
    assert!(finished, "the script must end after the Stop: {output}");
    assert!(output.contains("rhai run stopped"), "{output}");
    assert!(pending_ids(&holly, &sid, "after").await.is_empty());
    assert!(
        !dir.path.join("f.txt").exists(),
        "the script's edit never ran"
    );
    assert!(
        !dir.path.join("g.txt").exists(),
        "the stopped edit never ran"
    );
}
