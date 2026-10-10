//! ADR-0215: a resumed session whose retired agent was read-only is in the
//! `research` mode *before* its parked batch is re-offered. A resumed session
//! announces its replayed mode and re-offers pending calls before it reads
//! its inbox, so the narrowing has to ride the replay stream
//! (`ResumeCheck::paired_for_resume`). A later `SetMode` would let the
//! re-offered call be graded under the wider default mode first.

use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, Llm, LlmRequest, LlmResponse, LlmStream, OutEvent,
    Permission, PermissionProfile, SessionId,
};
use entanglement_runtime::capability::Capability;
use entanglement_runtime::session_store::{check_for_resume, read_path};
use entanglement_runtime::tool_runner::spawn_tool_executor;
use entanglement_runtime::{Tool, ToolRegistry};

struct DoneLlm;
#[async_trait]
impl Llm for DoneLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        Ok(stream_from_response(LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
            ..Default::default()
        }))
    }
}

/// A `Write`-capability `edit` that counts its runs.
struct CountingEdit(Arc<AtomicUsize>);
#[async_trait]
impl Tool for CountingEdit {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("edit")
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(format!("ran: {input}"))
    }
    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::Write]
    }
}

/// A 0.6.0-shaped root (`plan`) whose live `explore` child crashed with an
/// `edit` call parked: offered (`tool_exec`) but never answered.
const LOG: &str = concat!(
    r#"{"ts":1,"session":"root","payload":{"direction":"out","kind":"session_started","session":"root","parent":null,"profile":"plan","model":null,"root":true,"ts":1}}"#,
    "\n",
    r#"{"ts":2,"session":"kid","payload":{"direction":"out","kind":"session_started","session":"kid","parent":"root","profile":"explore","model":null,"root":false,"ts":2}}"#,
    "\n",
    r#"{"ts":2,"session":"kid","payload":{"direction":"in","kind":"prompt","session":"kid","content":[{"type":"text","text":"Fix x."}]}}"#,
    "\n",
    r#"{"ts":3,"session":"kid","payload":{"direction":"out","kind":"tool_call","session":"kid","seq":1,"request_id":"c-1","tool":"edit","input":"{\"path\":\"x\"}"}}"#,
    "\n",
    r#"{"ts":3,"session":"kid","payload":{"direction":"out","kind":"tool_exec","session":"kid","seq":2,"request_id":"c-1","tool":"edit","input":"{\"path\":\"x\"}","agent":"explore"}}"#,
    "\n",
);

/// Resumes [`LOG`] the way the CLI does and returns the child's re-offered
/// call's `(output, is_error)` and how often `edit` actually ran.
async fn resume_and_grade(explicit_mode: bool) -> (String, bool, usize) {
    let tmp = tempfile::tempdir().expect("temp dir");
    let path = tmp.path().join("log.jsonl");
    std::fs::write(&path, LOG).unwrap();
    let records = read_path(&path).expect("load");

    let agents = entanglement_runtime::agents::built_in_registry().expect("built-in agents");
    let check = check_for_resume(&records, |n| agents.get(n).is_some()).expect("no gap");
    let cfg = EngineConfig {
        llm_factory: Arc::new(|| Box::new(DoneLlm) as Box<dyn Llm>),
        agents: agents.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let runs = Arc::new(AtomicUsize::new(0));
    let mut reg = ToolRegistry::new();
    reg.register(CountingEdit(runs.clone()));
    let _executor = spawn_tool_executor(
        &holly,
        reg,
        agents,
        PermissionProfile::new(Permission::Allow),
    );

    let mut sub = holly.subscribe();
    holly
        .resume(
            SessionId::new("root"),
            check.paired_for_resume(&records, explicit_mode, None),
        )
        .await
        .expect("resume");
    let kid = SessionId::new("kid");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ev = tokio::time::timeout_at(deadline, sub.recv())
            .await
            .expect("the re-offered call was never answered")
            .expect("broadcast closed");
        if let OutEvent::ToolOutput {
            session,
            request_id,
            output,
            is_error,
            ..
        } = ev
        {
            if session == kid && request_id == "c-1" {
                return (output, is_error, runs.load(Ordering::SeqCst));
            }
        }
    }
}

#[tokio::test]
async fn a_parked_call_of_a_read_only_retired_child_is_graded_under_research() {
    let (output, is_error, runs) = resume_and_grade(false).await;
    assert!(is_error, "{output}");
    assert!(output.contains("research"), "{output}");
    assert_eq!(runs, 0, "a write must never run under `research`");
}

/// Control: with an explicit `--mode` there is no narrowing, so the same
/// re-offered call is graded under the default `build` mode and runs. This
/// proves the test above really observes the mode.
#[tokio::test]
async fn with_an_explicit_mode_the_same_call_is_graded_under_the_default() {
    let (output, is_error, runs) = resume_and_grade(true).await;
    assert!(!is_error, "{output}");
    assert_eq!(runs, 1, "{output}");
}
