//! #838: the shipped `rhai` skill's worked example runs verbatim through the
//! real `rhai` tool and host tools, so a doc edit that reintroduces the
//! `for f in glob(...)` character-iteration bug (or any other non-running
//! snippet) fails here instead of silently misleading the model.

use entanglement_core::{Permission, PermissionProfile, SessionId};

use super::{collect, one_profile, prompt, rhai_output, spawn_with_rhai, TempDir};

/// The ```rhai block under the skill doc's `## Worked example` heading.
fn worked_example() -> &'static str {
    let doc = include_str!("../../src/skills/rhai.md");
    let (_, section) = doc.split_once("## Worked example").expect(
        "rhai.md lost its `## Worked example` heading — update the marker in \
         skill_worked_example_runs",
    );
    // Stop at the next heading so a later section's rhai block is never the
    // one silently run in place of the worked example.
    let section = section.split_once("\n## ").map_or(section, |(s, _)| s);
    section
        .split_once("```rhai\n")
        .and_then(|(_, rest)| rest.split_once("```"))
        .map(|(body, _)| body)
        .expect(
            "`## Worked example` has no ```rhai fenced block — the `rhai` tag \
             is required for this test to find it",
        )
}

#[tokio::test]
async fn skill_worked_example_runs() {
    let dir = TempDir::new("skill-doc");
    std::fs::write(dir.path.join("lib.rs"), "fn a() {}\n").unwrap();
    std::fs::write(dir.path.join("tests.rs"), "fn t() {}\n").unwrap();
    let holly = spawn_with_rhai(
        worked_example(),
        &dir.path,
        one_profile("build", PermissionProfile::new(Permission::Allow)),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    prompt(&holly, &sid, "build").await;
    let events = collect(sub, &sid).await;

    let out = rhai_output(&events).expect("rhai.md `## Worked example` no longer runs");
    assert!(
        out.contains("tests.rs") && !out.contains("lib.rs"),
        "the worked example keeps only the test file: {out}"
    );
}
