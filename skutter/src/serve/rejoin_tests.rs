use entanglement_core::AgentState;

use super::*;

fn sid() -> SessionId {
    SessionId::new("s")
}

fn started() -> (Option<InMsg>, OutEvent) {
    (
        Some(InMsg::prompt(sid(), "go")),
        OutEvent::SessionStarted {
            session: sid(),
            parent: None,
            predecessor: None,
            agent: "general".into(),
            model: None,
            root: true,
            ts: 0,
            user: None,
        },
    )
}

fn call(id: &str) -> (Option<InMsg>, OutEvent) {
    (
        None,
        OutEvent::ToolCall {
            session: sid(),
            seq: 1,
            request_id: id.into(),
            tool: "bash".into(),
            input: "{}".into(),
            provider_meta: None,
            envelope: None,
        },
    )
}

fn agents() -> AgentCatalog {
    crate::agents::built_in_registry().expect("built-in agents")
}

/// Core re-offers a turn's unresolved batch.
#[test]
fn a_parked_batch_is_reoffered() {
    let ids = reoffered_ids(&[started(), call("a")], &sid(), agents());
    assert_eq!(ids, vec![(sid(), "a".to_string())]);
}

/// A `Stop` ends the turn: nothing is re-offered, so a listing need not
/// wait (review F1: it waited 5s on the stopped call).
#[test]
fn a_stopped_turn_reoffers_nothing() {
    let stopped = [
        started(),
        call("a"),
        (
            Some(InMsg::Stop { session: sid() }),
            OutEvent::Status {
                session: sid(),
                state: AgentState::Done,
            },
        ),
    ];
    assert!(reoffered_ids(&stopped, &sid(), agents()).is_empty());
}
