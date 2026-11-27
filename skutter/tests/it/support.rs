//! The engine fixture the `serve` suites drive: a scripted LLM, an echoing
//! `bash`, and the real `ToolExecutor` graded against a one-mode table. A
//! trimmed copy of `entanglement-runtime`'s own `unattended_mode`/
//! `attended_mode` helpers — those are private to that crate's test harness.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, ApprovalScope, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::attendance::Attendance;
use entanglement_runtime::capability::Capability;
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, ModeResolver, PermissionResolver,
};
use entanglement_runtime::tool_names::ASK_USER_TOOL;
use entanglement_runtime::tool_runner::ToolExecutor;
use entanglement_runtime::{Tool, ToolRegistry};
use tokio::sync::broadcast::Receiver;

/// Replays scripted responses in order, then plain text (so the turn ends
/// instead of looping once the script runs out).
struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
}

#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(done_text);
        Ok(stream_from_response(resp))
    }
}

/// A trivial `bash` declaring `Capability::Exec` (the `Tool` default is
/// `Write`, which would mis-grade an exec-class rule).
pub(crate) struct EchoBash;

#[async_trait]
impl Tool for EchoBash {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("bash")
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran: {input}"))
    }
    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::Exec]
    }
}

/// A single-mode table named `"build"` (the default mode, so a fresh session
/// resolves against it without `SetMode`).
pub(crate) fn one_mode_table(default: Permission, limits: Limits) -> Arc<ModeTable> {
    let mode = Mode {
        name: "build".to_string(),
        default,
        rules: Rules::default(),
        limits,
        sandbox: None,
        sandbox_network: false,
    };
    Arc::new(ModeTable::new(vec![mode]).expect("single-mode table is valid"))
}

/// A mode that asks for every call and gives up after `question_timeout`s.
pub(crate) fn asking(question_timeout: u64) -> Arc<ModeTable> {
    let limits = Limits {
        question_timeout,
        ..Limits::default()
    };
    one_mode_table(Permission::Ask, limits)
}

fn call(id: &str, name: &str, input: &str) -> LlmResponse {
    LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            input: input.into(),
            provider_meta: None,
        }],
        ..Default::default()
    }
}

pub(crate) fn bash_call(id: &str, input: &str) -> LlmResponse {
    call(id, "bash", input)
}

pub(crate) fn ask_user_call(id: &str, input: &str) -> LlmResponse {
    call(id, ASK_USER_TOOL, input)
}

pub(crate) fn done_text() -> LlmResponse {
    LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
        ..Default::default()
    }
}

/// The real executor against `mode_table`, replaying `scripted`, reading
/// `attendance` (ADR-0247) so a test can register a watching head.
pub(crate) fn spawn_attended(
    reg: ToolRegistry,
    mode_table: Arc<ModeTable>,
    scripted: Vec<LlmResponse>,
    attendance: Attendance,
) -> Holly {
    spawn_wrapped(reg, mode_table, scripted, attendance, |r| r)
}

/// [`spawn_attended`] whose mode resolver `wrap` decorates (a slow one, say).
pub(crate) fn spawn_wrapped(
    reg: ToolRegistry,
    mode_table: Arc<ModeTable>,
    scripted: Vec<LlmResponse>,
    attendance: Attendance,
    wrap: impl FnOnce(Arc<dyn PermissionResolver>) -> Arc<dyn PermissionResolver>,
) -> Holly {
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let scripted = Arc::new(scripted);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            let mut responses = (*scripted).clone();
            responses.reverse();
            Box::new(ScriptedLlm {
                responses: Mutex::new(responses),
            }) as Box<dyn Llm>
        }),
        agents: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let shared_tools = reg.shared();
    // `ModeResolver` folds `ModeChanged` in live; core emits it before the
    // first `ToolExec`, so an empty map is fine.
    let modes = Arc::new(Mutex::new(HashMap::new()));
    let resolver: Arc<dyn PermissionResolver> = wrap(Arc::new(ModeResolver::new(
        modes.clone(),
        mode_table.clone(),
        shared_tools.clone(),
        PermissionProfile::new(Permission::Allow),
        None,
    )));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load_from(None));
    let _executor = ToolExecutor::builder(&holly, shared_tools)
        .agents(profiles)
        .base(PermissionProfile::new(Permission::Allow))
        .active(Arc::new(Mutex::new(HashMap::new())))
        .perm_modes(modes)
        .resolver(resolver)
        .grants(grants)
        .mode_table(mode_table)
        .attendance(attendance)
        .spawn();
    holly
}

pub(crate) fn bash_engine(
    table: Arc<ModeTable>,
    calls: Vec<LlmResponse>,
    attendance: Attendance,
) -> Holly {
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    spawn_attended(reg, table, calls, attendance)
}

/// An engine whose one `bash` call parks a card for 1s-timeout approval.
pub(crate) fn engine(attendance: Attendance) -> Holly {
    bash_engine(
        asking(1),
        vec![bash_call("t1", "echo hi"), done_text()],
        attendance,
    )
}

/// `sid`'s next `ToolRequest` id.
pub(crate) async fn card(sub: &mut Receiver<OutEvent>, sid: &SessionId) -> String {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Ok(OutEvent::ToolRequest {
                session,
                request_id,
                ..
            })) if &session == sid => return request_id,
            Ok(Ok(_)) => {}
            other => panic!("expected a card on {sid}: {other:?}"),
        }
    }
}

pub(crate) fn approve(sid: &SessionId, request_id: &str) -> InMsg {
    InMsg::Approve {
        session: sid.clone(),
        request_id: request_id.into(),
        scope: ApprovalScope::Once,
        mode: None,
        run_all: false,
    }
}
