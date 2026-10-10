//! Session logs persisted by entanglement 0.6.0 must load and resume (#720).
//!
//! `tests/fixtures/v0.6.0-session.jsonl` was recorded by the real `v0.6.0`
//! tag — a `Holly` + tool executor driven through the reference persistence
//! tap (`spawn_persistence_subscriber_with_sink`) — so it carries 0.6.0's own
//! serializations: `SessionStarted.profile` (renamed `agent` by ADR-0207),
//! `AgentChanged.profile_detail` (the retired agent mask), two `set_agent`
//! switches (retired by ADR-0207), the `bash_enable`/`bash_disable`/
//! `bash_changed` toggles (retired by ADR-0163; session-less, so the
//! reference tap never logs them, but a sink storing every frame does), a
//! tool round-trip, a spawned child, a manual compaction, usage, and a final
//! turn parked mid-batch (`call-2` answered, `call-3` never).

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, Agent, AgentCatalog, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, Message, MessageRole, OutEvent, SessionId, DEFAULT_AGENT,
};
use entanglement_runtime::session_store::{
    check_for_resume, integrity_gap, pair_records, read_path, LogPayload, LogRecord,
};
use tokio::sync::broadcast::Receiver;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/v0.6.0-session.jsonl"
);

fn fixture() -> Vec<LogRecord> {
    read_path(Path::new(FIXTURE)).expect("a 0.6.0 log must load")
}

#[test]
fn v060_log_loads_with_retired_frames_skipped_not_gapped() {
    let records = fixture();
    assert_eq!(records.len(), 57, "every record decodes; none is dropped");
    assert_eq!(
        integrity_gap(&records),
        None,
        "a retired frame is an intentional retirement, not lost data"
    );

    let retired: Vec<&str> = records
        .iter()
        .filter_map(|r| match &r.payload {
            LogPayload::Retired { kind } => Some(kind.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        retired,
        [
            "set_agent",
            "set_agent",
            "bash_enable",
            "bash_changed",
            "bash_disable",
            "bash_changed"
        ]
    );

    let started: Vec<(&str, &str)> = records
        .iter()
        .filter_map(|r| match &r.payload {
            LogPayload::Out(OutEvent::SessionStarted { session, agent, .. }) => {
                Some((session.0.as_str(), agent.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        [("v060-root", "site-editor"), ("v060-child", "explore")],
        "0.6.0's `profile` field lands in `agent`"
    );
    // The root resumes as its own last agent; only the still-live child,
    // which ran under the read-only `explore`, falls back (ADR-0215).
    let check = check_for_resume(&records, known).expect("no gap");
    let notes: Vec<String> = check.fallbacks.iter().map(|f| f.note(false)).collect();
    assert_eq!(
        notes,
        [
            "note: session `v060-child` used retired agent `explore`; resuming as `general` in \
          `research` mode (ADR-0215)"
        ]
    );
}

/// What one request carried.
#[derive(Clone)]
struct Req {
    system: String,
    messages: Vec<Message>,
    /// The out-of-band mode notice (`[mode: …]`, ADR-0207 §9).
    notice: Option<String>,
}

type Requests = Arc<Mutex<Vec<Req>>>;

struct ScriptLlm {
    responses: Arc<Mutex<VecDeque<LlmResponse>>>,
    requests: Requests,
}

#[async_trait]
impl Llm for ScriptLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.requests.lock().unwrap().push(Req {
            system: req.system.to_string(),
            messages: req.messages.to_vec(),
            notice: req.trailing_notice.clone(),
        });
        let resp = self.responses.lock().unwrap().pop_front();
        Ok(stream_from_response(resp.unwrap_or_else(|| LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
            ..Default::default()
        })))
    }
}

const PLAN_PROMPT: &str = "PLAN PERSONA";
const EDITOR_PROMPT: &str = "SITE EDITOR PERSONA";

fn agent(name: &str, system_prompt: &str) -> Agent {
    Agent {
        name: name.into(),
        description: String::new(),
        system_prompt: system_prompt.into(),
        model: None,
        provider: None,
    }
}

/// The base `general` agent's prompt — what a retired or unknown agent
/// resumes under.
fn general_prompt() -> String {
    AgentCatalog::new()
        .get(DEFAULT_AGENT)
        .expect("the catalog always carries `general`")
        .system_prompt
        .clone()
}

/// The names [`engine`]'s catalog carries.
fn known(name: &str) -> bool {
    matches!(name, "general" | "plan" | "site-editor") || name == DEFAULT_AGENT
}

/// An engine whose catalog carries `plan` and the fixture's `site-editor`
/// (but no retired name), each with a recognizable system prompt.
fn engine() -> (Holly, Requests) {
    let responses = Arc::new(Mutex::new(VecDeque::from([LlmResponse {
        text: "Published.".into(),
        tool_calls: vec![],
        ..Default::default()
    }])));
    let requests: Requests = Arc::default();
    let r = requests.clone();
    let mut agents = AgentCatalog::new();
    agents.insert(agent("plan", PLAN_PROMPT));
    agents.insert(agent("site-editor", EDITOR_PROMPT));
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptLlm {
                responses: responses.clone(),
                requests: r.clone(),
            }) as Box<dyn Llm>
        }),
        agents,
        ..EngineConfig::default()
    };
    (Holly::spawn(cfg), requests)
}

async fn wait_for(sub: &mut Receiver<OutEvent>, sid: &SessionId, pred: impl Fn(&OutEvent) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ev = tokio::time::timeout_at(deadline, sub.recv())
            .await
            .expect("timed out waiting for event")
            .expect("broadcast closed");
        if ev.session() == Some(sid) && pred(&ev) {
            return;
        }
    }
}

#[tokio::test]
async fn v060_log_resumes_its_parked_turn_with_history_intact() {
    let records = fixture();
    let root = SessionId::new("v060-root");
    let (holly, requests) = engine();
    let mut sub = holly.subscribe();

    holly
        .resume(root.clone(), pair_records(&records))
        .await
        .expect("resume a 0.6.0 log");

    // The unanswered half of the parked batch is re-offered (ADR-0061).
    wait_for(
        &mut sub,
        &root,
        |ev| matches!(ev, OutEvent::ToolExec { request_id, .. } if request_id == "call-3"),
    )
    .await;
    holly
        .send(InMsg::tool_result(
            root.clone(),
            "call-3",
            "published draft",
        ))
        .await
        .unwrap();
    wait_for(&mut sub, &root, |ev| matches!(ev, OutEvent::Done { .. })).await;

    let Req {
        system,
        messages: history,
        ..
    } = requests.lock().unwrap()[0].clone();
    // The fixture switched `site-editor` → `plan` → `site-editor`; the
    // retired `set_agent`s' own `AgentChanged` replies fold last-write-wins.
    assert_eq!(system, EDITOR_PROMPT);
    let shape: Vec<(MessageRole, String, Vec<String>, Option<String>)> = history
        .iter()
        .map(|m| {
            let calls = m.tool_calls.iter().map(|c| c.id.clone()).collect();
            (m.role, m.text(), calls, m.tool_call_id.clone())
        })
        .collect();
    let (user, asst, tool) = (MessageRole::User, MessageRole::Assistant, MessageRole::Tool);
    let s = |t: &str| t.to_string();
    // The compaction is a no-op fold (ADR-0205) and the retired frames carry
    // no context, so the resumed turn sees exactly the conversation 0.6.0 had
    // — because this one was *manual* (0.6.0 never rewrote the source). A
    // 0.6.0 `Compacted { auto: true }` did rewrite context in place; master
    // ignores it too, so such a log resumes with the full pre-compaction
    // history instead of the summary 0.6.0 continued from.
    assert_eq!(
        shape,
        [
            (user, s("What does the home page say?"), vec![], None),
            (
                asst,
                s("Let me read the home page."),
                vec![s("call-1")],
                None
            ),
            (
                tool,
                s("page {\"path\":\"home\"}: Welcome to the site."),
                vec![],
                Some(s("call-1"))
            ),
            (
                asst,
                s("The home page says: Welcome to the site."),
                vec![],
                None
            ),
            (user, s("Publish the draft."), vec![], None),
            (
                asst,
                s("Reading the draft, then publishing it."),
                vec![s("call-2"), s("call-3")],
                None
            ),
            (
                tool,
                s("page {\"path\":\"draft\"}: Welcome to the site."),
                vec![],
                Some(s("call-2"))
            ),
            (tool, s("published draft"), vec![], Some(s("call-3"))),
        ]
    );
}

/// Sends `probe` to a resumed `sid` and returns the request it produced.
async fn probe(holly: &Holly, requests: &Requests, sid: &SessionId) -> Req {
    let mut sub = holly.subscribe();
    let before = requests.lock().unwrap().len();
    holly
        .send(InMsg::prompt(sid.clone(), "probe"))
        .await
        .unwrap();
    wait_for(&mut sub, sid, |ev| matches!(ev, OutEvent::Done { .. })).await;
    let req = requests.lock().unwrap()[before].clone();
    req
}

/// A 0.6.0-shaped log (`profile`, a retired `set_agent`, `profile_detail`).
fn v060_lines(sid: &str, profile: &str, switch_to: Option<&str>) -> String {
    let mut lines = vec![format!(
        r#"{{"ts":1,"session":"{sid}","payload":{{"direction":"out","kind":"session_started","session":"{sid}","parent":null,"profile":"{profile}","model":null,"root":true,"ts":1}}}}"#
    )];
    if let Some(to) = switch_to {
        lines.push(format!(
            r#"{{"ts":2,"session":"{sid}","payload":{{"direction":"in","kind":"set_agent","session":"{sid}","agent":"{to}"}}}}"#
        ));
        lines.push(format!(
            r#"{{"ts":2,"session":"{sid}","payload":{{"direction":"out","kind":"agent_changed","session":"{sid}","agent":"{to}","profile_detail":{{"mode":"primary","permission":{{"rules":[],"default":"ask"}}}}}}}}"#
        ));
    }
    lines.join("\n") + "\n"
}

/// A log ending on a retired `set_agent` switch to an agent the catalog
/// still carries resumes under *that* agent: the switch's `AgentChanged`
/// reply is an ordinary record and folds last-write-wins.
#[tokio::test]
async fn retired_set_agent_switch_resumes_under_the_last_agent() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let path = tmp.path().join("log.jsonl");
    std::fs::write(&path, v060_lines("switched", "site-editor", Some("plan"))).unwrap();
    let records = read_path(&path).expect("load");
    let sid = SessionId::new("switched");
    let (holly, requests) = engine();
    holly
        .resume(sid.clone(), pair_records(&records))
        .await
        .unwrap();
    assert_eq!(probe(&holly, &requests, &sid).await.system, PLAN_PROMPT);
}

/// What the CLI's resume path does with a 0.6.0 log rooted under `agent`
/// (`check_for_resume` → note → resume with forced modes seeded, ADR-0215),
/// minus the file read: the log is a temp file, never the real data dir.
/// Returns the notes and the first resumed request.
async fn resume_like_the_cli(agent: &str, explicit_mode: bool) -> (Vec<String>, Req) {
    let tmp = tempfile::tempdir().expect("temp dir");
    let path = tmp.path().join("log.jsonl");
    std::fs::write(&path, v060_lines("retired", agent, None)).unwrap();
    let records = read_path(&path).expect("load");
    let check = check_for_resume(&records, known).expect("a retired agent no longer refuses");
    let notes = check
        .fallbacks
        .iter()
        .map(|f| f.note(explicit_mode))
        .collect();

    let sid = SessionId::new("retired");
    let (holly, requests) = engine();
    holly
        .resume(
            sid.clone(),
            check.paired_for_resume(&records, explicit_mode, None),
        )
        .await
        .unwrap();
    (notes, probe(&holly, &requests, &sid).await)
}

/// A retired `build` log resumes as `general` in the ordinary default mode.
#[tokio::test]
async fn cli_resume_of_a_retired_build_agent_falls_back_to_general() {
    let (notes, req) = resume_like_the_cli("build", false).await;
    assert_eq!(
        notes,
        ["note: session used retired agent `build`; resuming as `general` (ADR-0215)"]
    );
    assert_eq!(req.system, general_prompt());
    assert_eq!(req.notice.as_deref(), Some("[mode: build]"));
}

/// A read-only retired agent never widens on resume: it lands in `research`
/// mode — unless the user passed `--mode` explicitly, which wins.
#[tokio::test]
async fn cli_resume_of_a_retired_explore_agent_narrows_to_research() {
    let (notes, req) = resume_like_the_cli("explore", false).await;
    assert_eq!(
        notes,
        [
            "note: session used retired agent `explore`; resuming as `general` in `research` mode \
          (ADR-0215)"
        ]
    );
    assert_eq!(req.system, general_prompt());
    // Already `research` from the replay itself — never a switch away from
    // `build` that a re-offered call could slip in ahead of.
    assert_eq!(req.notice.as_deref(), Some("[mode: research]"));

    let (notes, req) = resume_like_the_cli("explore", true).await;
    assert_eq!(
        notes,
        ["note: session used retired agent `explore`; resuming as `general` (ADR-0215)"]
    );
    assert_eq!(req.notice.as_deref(), Some("[mode: build]"));
}
