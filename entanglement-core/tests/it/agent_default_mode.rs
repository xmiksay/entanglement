//! A fresh session's starting mode (#763, ADR-0218): a `Spawn`'s own `mode`
//! (a head's `--mode`/config `mode:`) wins for a root and is its one
//! announced mode; else the agent's `default_mode` hint, clamped to
//! `DEFAULT_MODE`; a child never starts wider than its parent on the
//! engine's mode ladder; an off-ladder `Spawn.mode` refuses the spawn.

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
    // Bypasses the runtime's parse-time `auto` refusal on purpose: core's
    // own clamp is the defense in depth under test here.
    cfg.agents.insert(agent("wide", Some("auto")));
    Holly::spawn(cfg)
}

fn spawn(session: &str, parent: Option<&str>, agent: &str, mode: Option<&str>) -> InMsg {
    InMsg::Spawn {
        session: SessionId::new(session),
        parent: parent.map(SessionId::new),
        predecessor: None,
        agent: agent.into(),
        prompt: String::new(),
        user: None,
        model: None,
        mode: mode.map(String::from),
        workdir: None,
    }
}

async fn next_event(sub: &mut tokio::sync::broadcast::Receiver<OutEvent>) -> OutEvent {
    tokio::time::timeout(Duration::from_secs(3), sub.recv())
        .await
        .expect("timed out waiting for an event")
        .expect("event stream closed")
}

/// The next mode `session` announces.
async fn next_mode(sub: &mut tokio::sync::broadcast::Receiver<OutEvent>, session: &str) -> String {
    let sid = SessionId::new(session);
    loop {
        if let OutEvent::ModeChanged { session, mode } = next_event(sub).await {
            if session == sid {
                return mode;
            }
        }
    }
}

/// Every mode `session` announces until its barrier turn is `Done`.
async fn modes_until_done(holly: &Holly, session: &str) -> Vec<String> {
    let sid = SessionId::new(session);
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let mut modes = Vec::new();
    loop {
        match next_event(&mut sub).await {
            OutEvent::ModeChanged { session, mode } if session == sid => modes.push(mode),
            OutEvent::Done { session, .. } if session == sid => return modes,
            _ => {}
        }
    }
}

/// Starts a `kid` (under `child_agent`, asking for `mode`) beneath a root
/// started in `parent_mode`, returning the kid's starting mode.
async fn child_mode(
    ladder: Vec<String>,
    parent_mode: &str,
    child_agent: &str,
    mode: Option<&str>,
) -> String {
    let holly = engine(ladder);
    let mut sub = holly.subscribe();
    holly
        .send(spawn("root", None, DEFAULT_AGENT, Some(parent_mode)))
        .await
        .unwrap();
    assert_eq!(next_mode(&mut sub, "root").await, parent_mode);
    holly
        .send(spawn("kid", Some("root"), child_agent, mode))
        .await
        .unwrap();
    next_mode(&mut sub, "kid").await
}

#[tokio::test]
async fn a_root_starts_in_its_agents_default_mode() {
    let holly = engine(ladder());
    let mut sub = holly.subscribe();
    holly
        .send(spawn("root", None, "planner", None))
        .await
        .unwrap();
    assert_eq!(next_mode(&mut sub, "root").await, "plan");
}

#[tokio::test]
async fn a_root_hint_never_starts_wider_than_the_default() {
    let holly = engine(ladder());
    let mut sub = holly.subscribe();
    holly.send(spawn("root", None, "wide", None)).await.unwrap();
    assert_eq!(next_mode(&mut sub, "root").await, "build");
}

#[tokio::test]
async fn a_spawn_mode_is_the_roots_one_starting_mode() {
    // How a head lands `--mode`/config `mode:` over the hint: in the spawn
    // itself, so the root announces one mode and no switch from the hint.
    let holly = engine(ladder());
    holly
        .send(spawn("root", None, "planner", Some("research")))
        .await
        .unwrap();
    assert_eq!(modes_until_done(&holly, "root").await, ["research"]);
}

#[tokio::test]
async fn an_explicit_auto_is_a_humans_choice_and_kept() {
    let holly = engine(ladder());
    holly
        .send(spawn("root", None, DEFAULT_AGENT, Some("auto")))
        .await
        .unwrap();
    assert_eq!(modes_until_done(&holly, "root").await, ["auto"]);
}

#[tokio::test]
async fn an_unknown_spawn_mode_refuses_the_spawn() {
    let holly = engine(ladder());
    let mut sub = holly.subscribe();
    holly
        .send(spawn("root", None, DEFAULT_AGENT, Some("yolo")))
        .await
        .unwrap();
    loop {
        match next_event(&mut sub).await {
            OutEvent::Error { message, .. } => {
                assert!(message.contains("unknown mode `yolo`"), "{message}");
                break;
            }
            OutEvent::SessionStarted { .. } => panic!("spawned under an unknown mode"),
            _ => {}
        }
    }
    assert!(!holly.is_live(&SessionId::new("root")));
}

#[tokio::test]
async fn a_child_takes_a_narrower_default() {
    assert_eq!(child_mode(ladder(), "build", "planner", None).await, "plan");
}

#[tokio::test]
async fn a_child_is_clamped_to_its_parent_when_wider() {
    assert_eq!(
        child_mode(ladder(), "research", "planner", None).await,
        "research"
    );
    assert_eq!(child_mode(ladder(), "build", "wide", None).await, "build");
    // An explicit mode on a child spawn is clamped the same way.
    assert_eq!(
        child_mode(ladder(), "plan", DEFAULT_AGENT, Some("auto")).await,
        "plan"
    );
}

#[tokio::test]
async fn without_a_ladder_a_child_just_inherits() {
    assert_eq!(
        child_mode(Vec::new(), "build", "planner", None).await,
        "build"
    );
}
