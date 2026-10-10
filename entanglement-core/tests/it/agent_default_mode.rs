//! An agent's `default_mode` (#763, ADR-0218) seeds a fresh session's
//! starting mode: a root starts in it, a later `SetMode` (the head's
//! `--mode`/config `mode:`) still wins, and a spawned child takes it only
//! when the engine's mode ladder proves it no wider than its parent's mode.

use std::time::Duration;

use entanglement_core::{Agent, EngineConfig, Holly, InMsg, OutEvent, SessionId, DEFAULT_AGENT};

fn agent(name: &str, default_mode: Option<&str>) -> Agent {
    Agent {
        name: name.into(),
        description: String::new(),
        system_prompt: "x".into(),
        model: None,
        provider: None,
        default_mode: default_mode.map(String::from),
    }
}

fn ladder() -> Vec<String> {
    ["research", "plan", "build", "auto"]
        .map(String::from)
        .to_vec()
}

fn engine(ladder: Vec<String>) -> Holly {
    let mut cfg = EngineConfig {
        mode_ladder: ladder,
        ..EngineConfig::default()
    };
    cfg.agents.insert(agent(DEFAULT_AGENT, None));
    cfg.agents.insert(agent("planner", Some("plan")));
    cfg.agents.insert(agent("wide", Some("auto")));
    Holly::spawn(cfg)
}

fn spawn(session: &str, parent: Option<&str>, agent: &str) -> InMsg {
    InMsg::Spawn {
        session: SessionId::new(session),
        parent: parent.map(SessionId::new),
        predecessor: None,
        agent: agent.into(),
        prompt: String::new(),
        user: None,
        model: None,
    }
}

/// The next mode `session` announces.
async fn next_mode(sub: &mut tokio::sync::broadcast::Receiver<OutEvent>, session: &str) -> String {
    let sid = SessionId::new(session);
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(3), sub.recv())
            .await
            .expect("timed out waiting for ModeChanged")
            .expect("event stream closed");
        if let OutEvent::ModeChanged { session, mode } = ev {
            if session == sid {
                return mode;
            }
        }
    }
}

/// Starts `child` under `child_agent` beneath a `general` root switched to
/// `parent_mode`, returning the child's starting mode.
async fn child_mode(ladder: Vec<String>, parent_mode: &str, child_agent: &str) -> String {
    let holly = engine(ladder);
    let mut sub = holly.subscribe();
    holly
        .send(spawn("root", None, DEFAULT_AGENT))
        .await
        .unwrap();
    assert_eq!(next_mode(&mut sub, "root").await, "build");
    holly
        .send(InMsg::SetMode {
            session: SessionId::new("root"),
            mode: parent_mode.into(),
        })
        .await
        .unwrap();
    if parent_mode != "build" {
        assert_eq!(next_mode(&mut sub, "root").await, parent_mode);
    }
    holly
        .send(spawn("kid", Some("root"), child_agent))
        .await
        .unwrap();
    next_mode(&mut sub, "kid").await
}

#[tokio::test]
async fn a_root_starts_in_its_agents_default_mode() {
    let holly = engine(ladder());
    let mut sub = holly.subscribe();
    holly.send(spawn("root", None, "planner")).await.unwrap();
    assert_eq!(next_mode(&mut sub, "root").await, "plan");
}

#[tokio::test]
async fn a_later_set_mode_out_ranks_the_agents_default() {
    // How a head lands `--mode`/config `mode:` over the hint.
    let holly = engine(ladder());
    let mut sub = holly.subscribe();
    holly.send(spawn("root", None, "planner")).await.unwrap();
    assert_eq!(next_mode(&mut sub, "root").await, "plan");
    holly
        .send(InMsg::SetMode {
            session: SessionId::new("root"),
            mode: "build".into(),
        })
        .await
        .unwrap();
    assert_eq!(next_mode(&mut sub, "root").await, "build");
}

#[tokio::test]
async fn a_child_takes_a_narrower_default() {
    assert_eq!(child_mode(ladder(), "build", "planner").await, "plan");
}

#[tokio::test]
async fn a_child_is_clamped_to_its_parent_when_its_default_is_wider() {
    assert_eq!(
        child_mode(ladder(), "research", "planner").await,
        "research"
    );
    assert_eq!(child_mode(ladder(), "build", "wide").await, "build");
}

#[tokio::test]
async fn without_a_ladder_a_child_just_inherits() {
    assert_eq!(child_mode(Vec::new(), "build", "planner").await, "build");
}
