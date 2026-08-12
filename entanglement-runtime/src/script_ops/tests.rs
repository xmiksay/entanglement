use super::*;

fn stop_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn register(
    reg: &ScriptRegistry,
    owner: &SessionId,
    stop: Arc<AtomicBool>,
) -> (String, Arc<ScriptOp>) {
    reg.register(
        "let x = 1".to_string(),
        Some(owner.clone()),
        Duration::from_secs(120),
        stop,
    )
}

/// Handles carry ADR-0164's `x-` script prefix.
#[tokio::test]
async fn handles_are_x_prefixed() {
    let reg = ScriptRegistry::new();
    let session = SessionId::new("s1");
    let (id, _op) = register(&reg, &session, stop_flag());
    assert!(id.starts_with("x-"), "{id}");
}

/// A poll drains the buffer (destructive delta): the second poll sees only
/// what arrived after the first — the same contract as a job handle.
#[tokio::test]
async fn poll_is_a_destructive_delta() {
    let reg = ScriptRegistry::new();
    let session = SessionId::new("s1");
    let (id, op) = register(&reg, &session, stop_flag());

    op.append_output("first\n");
    let p1 = reg.poll(&id, &session, false, 1).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&p1.output), "first\n");
    assert!(p1.running);

    op.append_output("second\n");
    let p2 = reg.poll(&id, &session, false, 1).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&p2.output), "second\n");
}

/// `finish` appends the final line, flips the status, and wakes a parked poll.
#[tokio::test]
async fn finish_reports_terminal_state_and_wakes_a_parked_poll() {
    let reg = ScriptRegistry::new();
    let session = SessionId::new("s1");
    let (id, op) = register(&reg, &session, stop_flag());

    let reg2 = reg.clone();
    let session2 = session.clone();
    let id2 = id.clone();
    let waiter = tokio::spawn(async move { reg2.poll(&id2, &session2, false, 0).await });
    // Give the waiter a beat to park before finishing.
    tokio::task::yield_now().await;
    op.finish("=> 42", false);

    let p = waiter.await.unwrap().unwrap();
    assert!(!p.running);
    assert!(!p.is_error);
    assert!(String::from_utf8_lossy(&p.output).contains("=> 42"));
}

/// A wrong-owner poll is indistinguishable from an unknown handle.
#[tokio::test]
async fn wrong_owner_reads_as_unknown() {
    let reg = ScriptRegistry::new();
    let owner = SessionId::new("owner");
    let stranger = SessionId::new("stranger");
    let (id, _op) = register(&reg, &owner, stop_flag());
    assert!(reg.poll(&id, &stranger, false, 1).await.is_none());
    assert!(reg.poll("x-nonexistent", &owner, false, 1).await.is_none());
}

/// `kill: true` trips the cooperative stop flag and returns immediately —
/// the script itself terminates later, at its next engine operation.
#[tokio::test]
async fn kill_trips_the_stop_flag_and_returns_buffered_output() {
    let reg = ScriptRegistry::new();
    let session = SessionId::new("s1");
    let stop = stop_flag();
    let (id, op) = register(&reg, &session, stop.clone());
    op.append_output("partial\n");

    let p = reg.poll(&id, &session, true, 60).await.unwrap();
    assert!(stop.load(Ordering::SeqCst));
    assert!(p.running, "kill returns before the engine notices the flag");
    assert!(String::from_utf8_lossy(&p.output).contains("partial"));

    // The engine notices the flag: `finish` derives the `stopped` cause.
    op.finish("rhai error: script stopped", true);
    let p2 = reg.poll(&id, &session, false, 1).await.unwrap();
    assert!(!p2.running);
    assert!(p2.is_error);
    assert!(p2.stopped);
    assert!(!p2.timed_out);
}

/// The deadline cause wins over the stop flag when both are set.
#[tokio::test]
async fn timed_out_takes_precedence_over_stopped() {
    let reg = ScriptRegistry::new();
    let session = SessionId::new("s1");
    let stop = stop_flag();
    let (id, op) = register(&reg, &session, stop.clone());

    op.mark_timed_out();
    stop.store(true, Ordering::SeqCst);
    op.finish("rhai error: script exceeded the 120s time limit", true);

    let p = reg.poll(&id, &session, false, 1).await.unwrap();
    assert!(p.timed_out);
    assert!(!p.stopped);
}

/// Unpolled output is capped, dropping the oldest bytes and counting them.
#[tokio::test]
async fn output_is_capped_keeping_the_tail() {
    let reg = ScriptRegistry::new();
    let session = SessionId::new("s1");
    let (id, op) = register(&reg, &session, stop_flag());

    let chunk = "y".repeat(200 * 1024);
    op.append_output(&chunk);
    op.append_output("TAIL");
    op.append_output(&chunk);

    let p = reg.poll(&id, &session, false, 1).await.unwrap();
    assert!(p.dropped > 0);
    assert!(p.output.len() <= 256 * 1024);
}

/// Finished entries are listed until evicted; running ones always; other
/// sessions' never.
#[tokio::test]
async fn snapshot_ops_scopes_to_the_session() {
    let reg = ScriptRegistry::new();
    let mine = SessionId::new("mine");
    let theirs = SessionId::new("theirs");
    let (id, _op) = register(&reg, &mine, stop_flag());
    register(&reg, &theirs, stop_flag());

    let ops = reg.snapshot_ops(Some(&mine));
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].handle, id);
    assert_eq!(ops[0].label, "let x = 1");
    assert!(ops[0].running);

    assert_eq!(reg.snapshot_ops(None).len(), 2);
}
