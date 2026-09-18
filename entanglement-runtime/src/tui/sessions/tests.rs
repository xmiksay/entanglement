use super::*;
use crate::tui::session_view::{ApprovalMode, TranscriptEntry};

fn event(session: &SessionId, seq: u64, text: &str) -> OutEvent {
    OutEvent::TextDelta {
        session: session.clone(),
        seq,
        text: text.to_string(),
    }
}

#[test]
fn routes_events_to_the_right_session_without_cross_pollution() {
    let a = SessionId::new("a");
    let b = SessionId::new("b");
    let mut reg = SessionRegistry::new(a.clone());

    reg.handle_out_event(event(&a, 1, "hello-a"));
    reg.handle_out_event(event(&b, 1, "hello-b"));

    assert_eq!(reg.active_view().transcript().len(), 1);
    assert!(matches!(
        &reg.active_view().transcript()[0],
        TranscriptEntry::TextDelta { text } if text == "hello-a"
    ));

    let all = reg.all();
    assert_eq!(all.len(), 2);
    let b_view = all.iter().find(|(id, _)| **id == b).unwrap().1;
    assert_eq!(b_view.transcript().len(), 1);
}

#[test]
fn per_session_seq_dedupe_is_independent() {
    let a = SessionId::new("a");
    let b = SessionId::new("b");
    let mut reg = SessionRegistry::new(a.clone());

    reg.handle_out_event(event(&a, 1, "a1"));
    reg.handle_out_event(event(&b, 1, "b1"));
    reg.switch_to(b);
    assert_eq!(reg.active_view().transcript().len(), 1);
}

#[test]
fn background_approval_is_isolated_and_visible_in_sessions_list() {
    let a = SessionId::new("a");
    let b = SessionId::new("b");
    let mut reg = SessionRegistry::new(a.clone());

    reg.handle_out_event(OutEvent::ToolRequest {
        session: b.clone(),
        seq: 1,
        request_id: "t1".to_string(),
        tool: "read".to_string(),
        input: "{}".to_string(),
    });

    assert!(matches!(
        reg.active_view().approval_mode(),
        ApprovalMode::Normal
    ));

    let all = reg.all();
    let b_view = all.iter().find(|(id, _)| **id == b).unwrap().1;
    assert!(b_view.is_waiting_approval());

    reg.switch_to(b);
    assert!(matches!(
        reg.active_view().approval_mode(),
        ApprovalMode::WaitingForApproval { request_id } if request_id == "t1"
    ));
}

#[test]
fn propose_plan_request_renders_accept_prompt_and_handoff_switches_session() {
    let plan_session = SessionId::new("plan-s");
    let mut reg = SessionRegistry::new(plan_session.clone());

    // A `propose_plan` ToolRequest surfaces the standard approval prompt and
    // exposes the plan input so the head can hand it off on approve (#141).
    reg.handle_out_event(OutEvent::ToolRequest {
        session: plan_session.clone(),
        seq: 1,
        request_id: "pp1".to_string(),
        tool: crate::tool_names::PROPOSE_PLAN_TOOL.to_string(),
        input: serde_json::json!({
            "content": "# Do it",
            "path": ".entanglement/plans/plan-s.md",
        })
        .to_string(),
    });
    assert!(matches!(
        reg.active_view().approval_mode(),
        ApprovalMode::WaitingForApproval { request_id } if request_id == "pp1"
    ));
    let (_, tool, input) = reg
        .active_view()
        .pending_tool_request()
        .expect("pending propose_plan request");
    assert_eq!(tool, crate::tool_names::PROPOSE_PLAN_TOOL);
    let v: serde_json::Value = serde_json::from_str(input).unwrap();
    assert_eq!(v["content"], "# Do it");

    // The handoff mints a fresh root build session and switches to it.
    let build_session = SessionId::new("build-fresh");
    reg.ensure(&build_session);
    reg.switch_to(build_session.clone());
    assert_eq!(reg.active_id(), &build_session);
    // The plan session stays alive after accept (a later re-propose mints
    // another fresh build session).
    assert!(reg.all().iter().any(|(id, _)| **id == plan_session));
}

#[test]
fn switch_round_trip_preserves_scroll_and_agent() {
    let a = SessionId::new("a");
    let mut reg = SessionRegistry::new(a.clone());
    let b = reg.create();

    reg.switch_to(a.clone());
    // Scroll is now clamped against draw-time metrics, so give session `a`
    // headroom (20 lines of content in a 10-row viewport) before freezing
    // it at a manual offset by scrolling up from the bottom.
    {
        let view = reg.active_view_mut();
        view.set_viewport_metrics(20, 10);
        view.scroll_up(3);
    }
    assert_eq!(reg.active_view().scroll_offset(), 7);
    assert!(!reg.active_view().auto_follow());

    reg.switch_to(b.clone());
    assert_eq!(reg.active_view().scroll_offset(), 0);
    assert!(reg.active_view().auto_follow());

    reg.switch_to(a);
    assert_eq!(reg.active_view().scroll_offset(), 7);
    assert!(!reg.active_view().auto_follow());
}

#[test]
fn create_generates_unique_kind_tagged_ids() {
    let base = SessionId::new("tui");
    let mut reg = SessionRegistry::new(base.clone());
    let s2 = reg.create();
    let s3 = reg.create();
    // Each new session is a fresh id — no `{base}-{ordinal}` suffix.
    assert_ne!(s2, s3);
    assert_ne!(s2, base);
    assert!(
        !s2.0.starts_with("tui-"),
        "no human-readable suffix: {}",
        s2.0
    );
    // ADR-0164 shape: `s-<epoch-seconds hex><salt><counter>`, 15 chars.
    for id in [&s2, &s3] {
        assert!(id.0.starts_with("s-"), "kind-tagged: {}", id.0);
        assert_eq!(id.0.len(), 15, "id length: {}", id.0);
    }
    assert_eq!(reg.active_id(), &s3);
}

#[test]
fn acceptance_multiple_sessions_visible_in_modal_switching_renders_right_transcript() {
    let a = SessionId::new("a");
    let b = SessionId::new("b");
    let c = SessionId::new("c");
    let mut reg = SessionRegistry::new(a.clone());

    reg.handle_out_event(event(&a, 1, "hello-a"));
    reg.handle_out_event(event(&b, 1, "hello-b"));
    reg.handle_out_event(event(&c, 1, "hello-c"));

    let all = reg.all();
    assert_eq!(all.len(), 3, "All sessions should be visible");

    assert_eq!(
        reg.active_view().transcript().len(),
        1,
        "Active session has 1 entry"
    );
    assert!(
        matches!(
            &reg.active_view().transcript()[0],
            crate::tui::session_view::TranscriptEntry::TextDelta { text } if text == "hello-a"
        ),
        "Active session 'a' shows correct transcript"
    );

    reg.switch_to(b.clone());
    assert_eq!(
        reg.active_view().transcript().len(),
        1,
        "After switch, active session has 1 entry"
    );
    assert!(
        matches!(
            &reg.active_view().transcript()[0],
            crate::tui::session_view::TranscriptEntry::TextDelta { text } if text == "hello-b"
        ),
        "After switch, session 'b' shows correct transcript"
    );

    reg.switch_to(c.clone());
    assert!(
        matches!(
            &reg.active_view().transcript()[0],
            crate::tui::session_view::TranscriptEntry::TextDelta { text } if text == "hello-c"
        ),
        "After switch to 'c', shows correct transcript"
    );

    reg.switch_to(a.clone());
    assert!(
        matches!(
            &reg.active_view().transcript()[0],
            crate::tui::session_view::TranscriptEntry::TextDelta { text } if text == "hello-a"
        ),
        "Switching back to 'a' still shows correct transcript"
    );
}

#[test]
fn acceptance_new_session_created_on_first_prompt_and_appears_in_list() {
    let initial = SessionId::new("initial");
    let mut reg = SessionRegistry::new(initial.clone());

    reg.handle_out_event(event(&initial, 1, "first message"));

    let new_session = reg.create();
    // A new session is a fresh kind-tagged id (ADR-0164), distinct from the
    // initial id.
    assert_ne!(new_session, initial);
    assert_eq!(new_session.to_string().len(), 15);

    let all = reg.all();
    assert_eq!(all.len(), 2, "New session appears in list");

    assert!(
        all.iter().any(|(id, _)| *id == &new_session),
        "New session ID is in the list"
    );

    reg.switch_to(new_session.clone());
    reg.handle_out_event(event(&new_session, 1, "new session message"));

    let all = reg.all();
    assert!(
        all.iter()
            .find(|(id, _)| **id == new_session)
            .map(|(_, view)| !view.transcript().is_empty())
            .unwrap_or(false),
        "New session transcript exists"
    );
}

#[test]
fn restore_from_records_rebuilds_transcript_and_switches() {
    use crate::session_store::{LogPayload, LogRecord};

    let initial = SessionId::new("live");
    let restored = SessionId::new("old");
    let mut reg = SessionRegistry::new(initial.clone());

    let prompt = LogRecord::new(
        restored.clone(),
        LogPayload::In(InMsg::prompt(restored.clone(), "My name is Miksa")),
    );
    let reply = LogRecord::new(
        restored.clone(),
        LogPayload::Out(OutEvent::TextDelta {
            session: restored.clone(),
            seq: 1,
            text: "Hello Miksa".to_string(),
        }),
    );
    // Approve is a non-Prompt inbound record — it must not enter the transcript.
    let approve = LogRecord::new(
        restored.clone(),
        LogPayload::In(InMsg::Approve {
            session: restored.clone(),
            request_id: "r1".to_string(),
            scope: Default::default(),
            mode: None,
        }),
    );

    reg.restore_from_records(restored.clone(), &[prompt, reply, approve]);

    assert_eq!(
        reg.active_id(),
        &restored,
        "restored session becomes active"
    );
    let transcript = reg.active_view().transcript();
    assert_eq!(transcript.len(), 2);
    assert!(matches!(
        &transcript[0],
        TranscriptEntry::User { text, pending } if text == "My name is Miksa" && !pending
    ));
    assert!(matches!(
        &transcript[1],
        TranscriptEntry::TextDelta { text } if text == "Hello Miksa"
    ));

    // The restored id appears exactly once in the tab order.
    assert_eq!(
        reg.all().iter().filter(|(id, _)| **id == restored).count(),
        1
    );

    // The replayed Prompt doubles as the sidebar description.
    assert_eq!(
        reg.active_view().first_prompt(),
        Some("My name is Miksa"),
        "restore derives first_prompt from the replayed Prompt record"
    );
}

#[test]
fn first_prompt_is_set_once_and_snippeted() {
    let sid = SessionId::new("s1");
    let mut reg = SessionRegistry::new(sid);
    reg.active_view_mut()
        .record_user_message("fix the login bug\nwith full detail below".to_string());
    reg.active_view_mut()
        .record_user_message("second prompt".to_string());

    // First line wins, ellipsized because more content followed; a later
    // prompt never overwrites it.
    assert_eq!(reg.active_view().first_prompt(), Some("fix the login bug…"));
}

#[test]
fn acceptance_events_from_inactive_sessions_dont_pollute_active_view() {
    let active = SessionId::new("active");
    let background = SessionId::new("background");
    let mut reg = SessionRegistry::new(active.clone());

    reg.handle_out_event(event(&active, 1, "active-1"));

    reg.handle_out_event(event(&background, 1, "background-1"));
    reg.handle_out_event(event(&background, 2, "background-2"));

    assert_eq!(
        reg.active_view().transcript().len(),
        1,
        "Active session only has its own events"
    );
    assert!(
        matches!(
            &reg.active_view().transcript()[0],
            crate::tui::session_view::TranscriptEntry::TextDelta { text } if text == "active-1"
        ),
        "Active session not polluted by background events"
    );

    reg.switch_to(background.clone());
    assert_eq!(
        reg.active_view().transcript().len(),
        2,
        "Background session has its own events"
    );

    reg.handle_out_event(event(&active, 2, "active-2"));

    assert_eq!(
        reg.active_view().transcript().len(),
        2,
        "Background session not polluted by active events"
    );

    reg.switch_to(active.clone());
    assert_eq!(
        reg.active_view().transcript().len(),
        2,
        "Active session now has both its events"
    );
}

#[test]
fn restore_from_records_rebuilds_token_totals() {
    // Regression: Usage events were folded into head-global `App` state (never
    // per-view), so a resumed session always showed 0 in / 0 out. Token totals
    // now live per-view, and the resume path replays persisted `Usage` records
    // through `apply_event`, so the restored session carries its real totals.
    use crate::session_store::{LogPayload, LogRecord};

    let live = SessionId::new("live");
    let old = SessionId::new("old");
    let mut reg = SessionRegistry::new(live.clone());

    let usage = LogRecord::new(
        old.clone(),
        LogPayload::Out(OutEvent::Usage {
            session: old.clone(),
            seq: 1,
            input_tokens: 2_500,
            output_tokens: 900,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: Some(0.0123),
            purpose: entanglement_core::UsagePurpose::Turn,
        }),
    );

    reg.restore_from_records(old.clone(), &[usage]);

    assert_eq!(reg.active_id(), &old);
    let view = reg.active_view();
    assert_eq!(view.input_tokens(), 2_500);
    assert_eq!(view.output_tokens(), 900);
    assert!((view.cost_usd() - 0.0123).abs() < 1e-9);
}

#[test]
fn modal_selected_id_tracks_the_highlight_and_navigation() {
    // #6: the sessions-modal quick keys (`s`/`p`/`r`) act on the highlighted
    // session, so `modal_selected_id` must return exactly what the modal's
    // `ListState` has selected — the active session on open, and whatever
    // `modal_next`/`modal_prev`/`modal_page_*` moves it to afterwards.
    let a = SessionId::new("a");
    let mut reg = SessionRegistry::new(a.clone());
    let b = reg.create();
    let c = reg.create();
    // `create` switches active to the newest, so switch back to `a` first to
    // make the open-modal highlight deterministic (it seeds from the active id).
    reg.switch_to(a.clone());

    reg.toggle_modal();
    assert_eq!(reg.modal_selected_id().as_ref(), Some(&a));

    reg.modal_next();
    assert_eq!(reg.modal_selected_id().as_ref(), Some(&b));

    reg.modal_next();
    assert_eq!(reg.modal_selected_id().as_ref(), Some(&c));

    // Wrap-around back to the first session (modal_next is modular).
    reg.modal_next();
    assert_eq!(reg.modal_selected_id().as_ref(), Some(&a));

    reg.modal_prev();
    assert_eq!(reg.modal_selected_id().as_ref(), Some(&c));

    // Page down clamps at the last session rather than wrapping.
    reg.modal_page_down(10);
    assert_eq!(reg.modal_selected_id().as_ref(), Some(&c));

    // Closing the modal does not clear the underlying selection — the next
    // open re-seeds from the then-active id.
    reg.close_modal();
    reg.switch_to(b.clone());
    reg.toggle_modal();
    assert_eq!(reg.modal_selected_id().as_ref(), Some(&b));
}

fn started(id: &SessionId, parent: Option<&SessionId>) -> OutEvent {
    OutEvent::SessionStarted {
        session: id.clone(),
        parent: parent.cloned(),
        predecessor: None,
        agent: "build".to_string(),
        model: None,
        root: parent.is_none(),
        ts: 1,
        user: None,
    }
}

fn usage(id: &SessionId, seq: u64, input: u64, output: u64, cost_usd: Option<f64>) -> OutEvent {
    OutEvent::Usage {
        session: id.clone(),
        seq,
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: 0,
        cache_write_tokens: 0,
        cost_usd,
        purpose: entanglement_core::UsagePurpose::Turn,
    }
}

#[test]
fn usage_rollup_sums_two_levels_of_descendants() {
    // parent -> child -> grandchild, each spawned via agent/agent_send (#560):
    // the parent's own view never sees the descendants' usage, so the rollup
    // has to walk the spawn tree to add it back.
    let parent = SessionId::new("parent");
    let child = SessionId::new("child");
    let grandchild = SessionId::new("grandchild");
    let mut reg = SessionRegistry::new(parent.clone());

    reg.handle_out_event(usage(&parent, 1, 1_000, 100, Some(0.01)));
    reg.handle_out_event(started(&child, Some(&parent)));
    reg.handle_out_event(usage(&child, 1, 2_000, 200, Some(0.02)));
    reg.handle_out_event(started(&grandchild, Some(&child)));
    reg.handle_out_event(usage(&grandchild, 1, 3_000, 300, Some(0.03)));

    let rollup = reg.usage_rollup(&parent);
    assert_eq!(rollup.input_tokens, 6_000);
    assert_eq!(rollup.output_tokens, 600);
    assert!((rollup.cost_usd.unwrap() - 0.06).abs() < 1e-9);

    // The child's own rollup only picks up its own + the grandchild's usage.
    let child_rollup = reg.usage_rollup(&child);
    assert_eq!(child_rollup.input_tokens, 5_000);
    assert_eq!(child_rollup.output_tokens, 500);

    // A leaf with no descendants rolls up to exactly its own usage.
    let leaf_rollup = reg.usage_rollup(&grandchild);
    assert_eq!(leaf_rollup.input_tokens, 3_000);
}

#[test]
fn usage_rollup_falls_back_to_tokens_when_any_descendant_lacks_pricing() {
    // A partial dollar sum that silently drops an unpriced child's cost would
    // understate the real total, so the whole rollup must go token-only
    // instead of guessing (#560).
    let parent = SessionId::new("parent");
    let child = SessionId::new("child");
    let mut reg = SessionRegistry::new(parent.clone());

    reg.handle_out_event(usage(&parent, 1, 1_000, 100, Some(0.01)));
    reg.handle_out_event(started(&child, Some(&parent)));
    // Child's model has no catalog pricing — `cost_usd: None` on the wire.
    reg.handle_out_event(usage(&child, 1, 2_000, 200, None));

    let rollup = reg.usage_rollup(&parent);
    assert_eq!(rollup.input_tokens, 3_000);
    assert_eq!(rollup.output_tokens, 300);
    assert_eq!(rollup.cost_usd, None);
}

#[test]
fn usage_rollup_of_a_childless_session_matches_its_own_totals() {
    let solo = SessionId::new("solo");
    let mut reg = SessionRegistry::new(solo.clone());
    reg.handle_out_event(usage(&solo, 1, 500, 50, Some(0.001)));

    let rollup = reg.usage_rollup(&solo);
    assert_eq!(rollup.input_tokens, 500);
    assert_eq!(rollup.output_tokens, 50);
    assert!((rollup.cost_usd.unwrap() - 0.001).abs() < 1e-9);
}

#[test]
fn restore_routes_each_record_to_its_own_sessions_view() {
    // A root's log interleaves its sub-agents' records. Folding them all into
    // the root mixed a child's stream into the root transcript, and ran the
    // child's own `seq` counter through the root's dedupe guard.
    use crate::session_store::{LogPayload, LogRecord};

    let root = SessionId::new("root");
    let child = SessionId::new("child");
    let mut reg = SessionRegistry::new(SessionId::new("live"));
    let delta = |session: &SessionId, seq, text: &str| {
        LogRecord::new(
            session.clone(),
            LogPayload::Out(OutEvent::TextDelta {
                session: session.clone(),
                seq,
                text: text.to_string(),
            }),
        )
    };
    let records = [
        LogRecord::new(
            root.clone(),
            LogPayload::In(InMsg::prompt(root.clone(), "go")),
        ),
        delta(&root, 5, "root says"),
        LogRecord::new(
            child.clone(),
            LogPayload::In(InMsg::prompt(child.clone(), "child task")),
        ),
        // Lower seq than the root's: a shared dedupe guard dropped this.
        delta(&child, 1, "child says"),
    ];

    reg.restore_from_records(root.clone(), &records);

    assert_eq!(reg.active_id(), &root);
    let text_of = |id: &SessionId| {
        reg.view_for(id)
            .expect("view restored")
            .transcript()
            .iter()
            .filter_map(|e| match e {
                TranscriptEntry::User { text, .. } => Some(format!("user:{text}")),
                TranscriptEntry::TextDelta { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(text_of(&root), ["user:go", "root says"]);
    assert_eq!(text_of(&child), ["user:child task", "child says"]);
}
