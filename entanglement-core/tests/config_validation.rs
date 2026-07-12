//! Config validation + graceful profile fallback (issue #106 part 2).
//!
//! A custom [`ProfileRegistry`] without the required `build` profile must be a
//! clean construction-time error via [`EngineConfig::validate`], and — should an
//! embedder skip that check — the supervisor must fall back to a synthesized
//! default rather than panicking and taking down every session.

use std::time::Duration;

use entanglement_core::{
    AgentMode, AgentProfile, ConfigError, EngineConfig, Holly, InMsg, OutEvent, Permission,
    PermissionProfile, ProfileRegistry, SessionId,
};

fn custom_profile(name: &str) -> AgentProfile {
    AgentProfile {
        name: name.to_string(),
        description: String::new(),
        mode: AgentMode::Primary,
        system_prompt: "custom".to_string(),
        model: None,
        permission: PermissionProfile::new(Permission::Deny),
        tools: None,
        disallowed_tools: Vec::new(),
        owns_plan: false,
        owns_tasks: false,
        can_spawn: None,
        spawnable_agents: None,
    }
}

/// A registry an embedder assembled without the built-in `build` profile.
fn registry_without_build() -> ProfileRegistry {
    let mut reg = ProfileRegistry::default();
    reg.insert(custom_profile("reviewer"));
    reg
}

#[test]
fn default_config_validates() {
    assert_eq!(EngineConfig::default().validate(), Ok(()));
    assert_eq!(ProfileRegistry::new().validate(), Ok(()));
}

#[test]
fn registry_missing_build_is_a_construction_error() {
    let reg = registry_without_build();
    assert_eq!(reg.validate(), Err(ConfigError::MissingDefaultProfile));

    let cfg = EngineConfig {
        profiles: reg,
        ..EngineConfig::default()
    };
    assert_eq!(cfg.validate(), Err(ConfigError::MissingDefaultProfile));
}

#[tokio::test]
async fn supervisor_falls_back_when_build_missing() {
    // An unvalidated registry without `build` used to panic the supervisor on
    // the first session spawn (`.expect`). It must now degrade gracefully: the
    // session starts under a synthesized `build` profile.
    let cfg = EngineConfig {
        profiles: registry_without_build(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut sub = holly.subscribe();
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "hi".to_string(),
        })
        .await
        .expect("send prompt");

    // A running supervisor emits SessionStarted; a panicked one never would.
    let started = loop {
        let ev = tokio::time::timeout(Duration::from_secs(3), sub.recv())
            .await
            .expect("supervisor did not start the session (likely panicked)")
            .expect("event stream closed");
        if let OutEvent::SessionStarted {
            session, profile, ..
        } = &ev
        {
            if session == &sid {
                break profile.clone();
            }
        }
    };
    assert_eq!(
        started, "build",
        "fallback should synthesize the build profile"
    );
}
