//! ADR-0230: every non-ordinary card carries a structured `reason`, and its
//! `scopes` advertise only what an `Approve` honors (#836, #846) — a head
//! decides on data, never on the card text.

use entanglement_core::{ApprovalScope, CardReason, Permission};
use serde_json::json;

use super::launch_harness::{fixture, run, run_calls, table, Card, Launch, Setup, ADD};

const FULL: [ApprovalScope; 3] = [
    ApprovalScope::Once,
    ApprovalScope::Session,
    ApprovalScope::Always,
];

fn card(tool: &str, reason: Option<CardReason>, scopes: &[ApprovalScope]) -> Card {
    Card {
        tool: tool.into(),
        reason,
        scopes: scopes.to_vec(),
    }
}

/// `skutter run`'s answerer starts an in-root launch and refuses an
/// out-of-root one on `reason` alone.
#[tokio::test]
async fn the_headless_head_refuses_an_escape_launch_on_its_reason() {
    let (_tmp, root) = fixture();
    let setup = Setup::new(table(Permission::Ask, &[], &[], 0), Launch::Headless);
    let escape = r#"{"path":"../evil.rhai","args":{"a":1,"b":1}}"#;
    let seen = run(&root, &[escape, ADD], setup).await;
    assert_eq!(
        seen.cards,
        [
            card("rhai", Some(CardReason::LaunchEscape), &FULL),
            card("rhai", Some(CardReason::ScriptLaunch), &FULL),
        ]
    );
    assert!(seen.outputs[0].contains("rejected"), "{:?}", seen.outputs);
    assert!(seen.outputs[1].contains("42"), "{:?}", seen.outputs);
}

/// An inline script has no path to key a grant on: its card offers `Once`.
#[tokio::test]
async fn an_inline_launch_card_offers_only_once() {
    let (_tmp, root) = fixture();
    let setup = Setup::new(table(Permission::Ask, &[], &[], 0), Launch::AskPerCommand);
    let seen = run(&root, &[r#"{"script":"40 + 2"}"#], setup).await;
    let once = [ApprovalScope::Once];
    assert_eq!(
        seen.cards,
        [card("rhai", Some(CardReason::InlineLaunch), &once)]
    );
    assert!(seen.outputs[0].contains("42"), "{:?}", seen.outputs);
}

/// A floored write always asks, so no grant is ever recorded — its card
/// offers `Once` alone, even where the mode allows `write`.
#[tokio::test]
async fn a_floored_write_card_offers_only_once() {
    let (_tmp, root) = fixture();
    let mut setup = Setup::new(table(Permission::Ask, &["write"], &[], 0), Launch::Reject);
    setup.trusted = vec![root.join("scripts")].into();
    let write = json!({"path": "scripts/new.rhai", "content": "1"}).to_string();
    let seen = run_calls(&root, &[("write", write.as_str())], setup).await;
    let once = [ApprovalScope::Once];
    assert_eq!(
        seen.cards,
        [card("write", Some(CardReason::FlooredWrite), &once)]
    );
}

/// An ordinary graded `Ask` carries no reason and every scope; an
/// out-of-root `read` is a `RootEscape` card — without the triad's
/// `SessionDir`, since an escape grant is exact-path.
#[tokio::test]
async fn ordinary_and_escape_cards_keep_every_scope() {
    let (_tmp, root) = fixture();
    let setup = Setup::new(table(Permission::Ask, &["read"], &[], 0), Launch::Reject);
    let write = json!({"path": "a.txt", "content": "1"}).to_string();
    let read = json!({"path": "../out.txt"}).to_string();
    let seen = run_calls(
        &root,
        &[("write", write.as_str()), ("read", read.as_str())],
        setup,
    )
    .await;
    assert_eq!(
        seen.cards,
        [
            card("write", None, &FULL),
            card("read", Some(CardReason::RootEscape), &FULL),
        ]
    );
}

/// A wire `Always` on a `[once]` card is clamped to `Once`: a floored write
/// outside the root runs once and leaves no durable escape grant.
#[tokio::test]
async fn a_wire_scope_the_card_did_not_offer_is_clamped_to_once() {
    let (_tmp, root) = fixture();
    let ext = root.parent().unwrap().join("ext");
    std::fs::create_dir_all(&ext).unwrap();
    let mut setup = Setup::new(table(Permission::Ask, &["write"], &[], 0), Launch::Reject);
    setup.trusted = vec![ext.clone()].into();
    setup.scope = ApprovalScope::Always;
    let target = ext.join("x.rhai");
    let write = json!({"path": target, "content": "1"}).to_string();
    let seen = run_calls(&root, &[("write", write.as_str())], setup).await;
    let once = [ApprovalScope::Once];
    assert_eq!(
        seen.cards,
        [card("write", Some(CardReason::FlooredWrite), &once)]
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "1",
        "it ran once"
    );
    assert!(
        !seen.store.is_durably_allowed("write", &target),
        "no durable escape grant from an unoffered scope"
    );
}
