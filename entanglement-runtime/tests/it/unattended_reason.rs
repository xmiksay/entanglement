//! An unattended collapse of a resolver's reasoned `Ask` tells the model
//! why the call needed asking at all (#968 review: an escape-root ask must
//! not read as a bare mode denial).

use std::sync::Arc;

use async_trait::async_trait;
use entanglement_core::{CardReason, InMsg, OutEvent, Permission, SessionId};
use entanglement_runtime::mode::Limits;
use entanglement_runtime::policy::{Grade, PermissionResolver};
use entanglement_runtime::ToolRegistry;

use crate::unattended_mode::{
    bash_call, collect, done_text, one_mode_table, spawn_wrapped, EchoBash,
};

/// Asks for every call with a reason, whatever the inner grade.
struct Reasoned(Arc<dyn PermissionResolver>);

#[async_trait]
impl PermissionResolver for Reasoned {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Grade {
        match self.0.resolve(session, tool, input).await {
            Grade::Deny(why) => Grade::Deny(why),
            _ => Grade::ask(
                "accesses a path outside the project root: /elsewhere",
                CardReason::RootEscape,
            ),
        }
    }
}

#[tokio::test]
async fn an_unattended_collapse_names_the_resolver_reason() {
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    let limits = Limits {
        question_timeout: 30,
        ..Limits::default()
    };
    let holly = spawn_wrapped(
        reg,
        one_mode_table(Permission::Allow, limits),
        vec![bash_call("t1", r#"{"command":"ls"}"#), done_text()],
        entanglement_runtime::attendance::Attendance::new(),
        |inner| Arc::new(Reasoned(inner)),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;
    let refused = events.iter().find_map(|e| match e {
        OutEvent::ToolOutput {
            output,
            is_error: true,
            ..
        } => Some(output.clone()),
        _ => None,
    });
    let refused = refused.unwrap_or_else(|| panic!("a collapsed refusal: {events:?}"));
    assert!(
        refused.contains("outside the project root: /elsewhere") && refused.contains("unattended"),
        "{refused}"
    );
}
