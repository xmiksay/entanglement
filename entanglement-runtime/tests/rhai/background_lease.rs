//! A background script's seq lease follows its compaction (#891, ADR-0244):
//! once the script switched to the successor (ADR-0241) it pins the
//! successor's counter too, so a card it parks on the successor after that
//! session retired — and after more retirements than the cap — still gets a
//! real seq.

use std::sync::Arc;

use entanglement_core::{InMsg, Llm, OutEvent, SessionId};
use entanglement_runtime::script_ops::ScriptRegistry;
use serde_json::json;

use super::background_follow::{compact, edit_card_on, executor_caught_up, until, Follow};
use super::background_park::engine_with;
use super::background_retired::{retire_storm, STORM};
use super::TempDir;

fn wait_for(gate: &std::path::Path) -> String {
    format!(
        "bash(\"while [ ! -f '{}' ]; do sleep 0.05; done\");",
        gate.display()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_followed_successors_counter_stays_pinned_past_the_cap() {
    let dir = TempDir::new("bg-lease-follow");
    let (g1, g2) = (dir.path.join("g1"), dir.path.join("g2"));
    // Waits on pred, switches to succ on the quick second call, waits again,
    // then asks.
    let script = format!(
        r#"{} bash("true"); {} edit("f.txt", "", "approved\n"); "finished""#,
        wait_for(&g1),
        wait_for(&g2)
    );
    let llm = Arc::new(move || {
        Box::new(Follow {
            script: script.clone(),
            direct: json!({}),
        }) as Box<dyn Llm>
    });
    let holly = engine_with(&dir.path, ScriptRegistry::new(), llm);
    let (pred, succ) = (SessionId::new("pred"), SessionId::new("succ"));
    let mut sub = holly.subscribe();

    holly.send(InMsg::prompt(pred.clone(), "go")).await.unwrap();
    until(
        &mut sub,
        "the launch's turn",
        |_| false,
        |ev| matches!(ev, OutEvent::Done { session, .. } if session == &pred),
    )
    .await;
    compact(&holly, &mut sub, &pred, &succ).await;
    executor_caught_up(&holly, &mut sub, &succ).await;

    std::fs::write(&g1, "").unwrap();
    until(
        &mut sub,
        "the switched call on succ",
        |_| false,
        |ev| {
            matches!(ev, OutEvent::NestedCall { session, tool, index: 2, .. }
            if session == &succ && tool == "bash")
        },
    )
    .await;

    // The successor goes away too; only the script's lease keeps its counter.
    holly.hibernate(succ.clone()).await.unwrap();
    until(
        &mut sub,
        "succ's hibernate",
        |_| false,
        |ev| matches!(ev, OutEvent::SessionHibernated { session, .. } if session == &succ),
    )
    .await;
    drop(sub);
    retire_storm(&holly, STORM).await;
    let mut sub = holly.subscribe();

    std::fs::write(&g2, "").unwrap();
    let card = until(
        &mut sub,
        "the card on succ",
        |ev| edit_card_on(ev, &succ),
        |ev| edit_card_on(ev, &succ),
    )
    .await;
    // Evicted, succ's counter would be gone and the card minted seq 0; the
    // pinned counter continues succ's own sequence (unique by construction).
    let seq = card.seq().unwrap_or(0);
    assert!(seq > 0, "the card on succ got the seq-0 sentinel");
}
