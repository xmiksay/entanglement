//! #757 (ADR-0216): on a CLI resume, the configured `mode:` must never reach
//! a session forced into `research`. A `SetMode` on the root cascades over the
//! whole tree, so sent as one it would widen the forced child until a
//! re-narrowing landed. `resume_with_mode` instead carries every mode in the
//! replay when any session is forced.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId,
};
use entanglement_runtime::session_store::{check_for_resume, read_path, resume_with_mode};

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

/// A `general` root with one live child running under `kid_agent`.
fn log(kid_agent: &str) -> String {
    [
        r#"{"ts":1,"session":"root","payload":{"direction":"out","kind":"session_started","session":"root","parent":null,"agent":"general","model":null,"root":true,"ts":1}}"#.to_string(),
        format!(
            r#"{{"ts":2,"session":"kid","payload":{{"direction":"out","kind":"session_started","session":"kid","parent":"root","agent":"{kid_agent}","model":null,"root":false,"ts":2}}}}"#
        ),
    ]
    .join("\n")
}

/// Resumes [`log`] the way `skutter run --resume` does under config
/// `mode: build` (no explicit `--mode`), and returns every mode `root` and
/// `kid` announced. A prompt to `kid` is the barrier: its inbox is FIFO, so
/// any cascaded switch was announced before that turn's `Done`.
async fn resume_modes(kid_agent: &str) -> (Vec<String>, Vec<String>) {
    let tmp = tempfile::tempdir().expect("temp dir");
    let path = tmp.path().join("log.jsonl");
    std::fs::write(&path, log(kid_agent)).unwrap();
    let records = read_path(&path).expect("load");

    let agents = entanglement_runtime::agents::built_in_registry().expect("built-in agents");
    let check = check_for_resume(&records, |n| agents.get(n).is_some()).expect("no gap");
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(|| Box::new(DoneLlm) as Box<dyn Llm>),
        agents,
        ..EngineConfig::default()
    });

    let mut sub = holly.subscribe();
    let (root, kid) = (SessionId::new("root"), SessionId::new("kid"));
    resume_with_mode(&holly, &root, &records, &check, false, Some("build".into()))
        .await
        .expect("resume");
    holly
        .send(InMsg::prompt(kid.clone(), "go"))
        .await
        .expect("prompt");

    let (mut root_modes, mut kid_modes) = (Vec::new(), Vec::new());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ev = tokio::time::timeout_at(deadline, sub.recv())
            .await
            .expect("kid's turn never finished")
            .expect("broadcast closed");
        match ev {
            OutEvent::ModeChanged { session, mode } if session == root => root_modes.push(mode),
            OutEvent::ModeChanged { session, mode } if session == kid => kid_modes.push(mode),
            OutEvent::Done { session, .. } if session == kid => break,
            _ => {}
        }
    }
    (root_modes, kid_modes)
}

#[tokio::test]
async fn the_configured_mode_never_reaches_a_forced_read_only_child() {
    let (root_modes, kid_modes) = resume_modes("explore").await;
    assert_eq!(root_modes.last().map(String::as_str), Some("build"));
    assert!(!kid_modes.is_empty(), "the kid never announced its mode");
    assert!(
        kid_modes.iter().all(|m| m == "research"),
        "a forced child observed a wider mode: {kid_modes:?}"
    );
}

/// Control: with nothing forced, the configured mode still reaches the child
/// (here through the ordinary cascading `SetMode` on the root).
#[tokio::test]
async fn without_a_forced_session_the_configured_mode_cascades() {
    let (root_modes, kid_modes) = resume_modes("general").await;
    assert_eq!(root_modes.last().map(String::as_str), Some("build"));
    assert_eq!(kid_modes.last().map(String::as_str), Some("build"));
}
