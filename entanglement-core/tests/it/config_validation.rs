//! Config validation + graceful profile fallback (issue #106 part 2).
//!
//! A custom [`AgentCatalog`] without the required `general` profile must be a
//! clean construction-time error via [`EngineConfig::validate`], and — should an
//! embedder skip that check — the supervisor must fall back to a synthesized
//! default rather than panicking and taking down every session.

use std::time::Duration;

use entanglement_core::{
    Agent, AgentCatalog, ConfigError, EngineConfig, Holly, InMsg, OutEvent, SessionId,
};

fn custom_profile(name: &str) -> Agent {
    Agent {
        name: name.to_string(),
        description: String::new(),
        system_prompt: "custom".to_string(),
        model: None,
        provider: None,
    }
}

/// A registry an embedder assembled without the built-in `general` profile.
fn registry_without_general() -> AgentCatalog {
    let mut reg = AgentCatalog::default();
    reg.insert(custom_profile("reviewer"));
    reg
}

#[test]
fn default_config_validates() {
    assert_eq!(EngineConfig::default().validate(), Ok(()));
    assert_eq!(AgentCatalog::new().validate(), Ok(()));
}

#[test]
fn registry_missing_general_is_a_construction_error() {
    let reg = registry_without_general();
    assert_eq!(reg.validate(), Err(ConfigError::MissingDefaultAgent));

    let cfg = EngineConfig {
        agents: reg,
        ..EngineConfig::default()
    };
    assert_eq!(cfg.validate(), Err(ConfigError::MissingDefaultAgent));
}

#[tokio::test]
async fn supervisor_falls_back_when_general_missing() {
    // An unvalidated registry without `general` used to panic the supervisor on
    // the first session spawn (`.expect`). It must now degrade gracefully: the
    // session starts under a synthesized `general` profile.
    let cfg = EngineConfig {
        agents: registry_without_general(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut sub = holly.subscribe();
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::prompt(sid.clone(), "hi".to_string()))
        .await
        .expect("send prompt");

    // A running supervisor emits SessionStarted; a panicked one never would.
    let started = loop {
        let ev = tokio::time::timeout(Duration::from_secs(3), sub.recv())
            .await
            .expect("supervisor did not start the session (likely panicked)")
            .expect("event stream closed");
        if let OutEvent::SessionStarted { session, agent, .. } = &ev {
            if session == &sid {
                break agent.clone();
            }
        }
    };
    assert_eq!(
        started, "general",
        "fallback should synthesize the general profile"
    );
}
