use super::*;

fn chain(ids: &[&str]) -> Vec<SessionId> {
    ids.iter().map(|s| SessionId::new(*s)).collect()
}

fn expiry(ms: u64, attendance: &Attendance, ids: &[&str], request_id: &str) -> Expiry {
    Expiry::unless_attended(
        Some(Duration::from_millis(ms)),
        attendance,
        &chain(ids),
        request_id,
    )
}

/// Whether `expiry` is still waiting after `ms`.
async fn still_waiting(expiry: Expiry, ms: u64) -> bool {
    tokio::time::timeout(Duration::from_millis(ms), expiry.elapsed())
        .await
        .is_err()
}

#[test]
fn nobody_watches_by_default() {
    assert!(!Attendance::new().attends_any(&chain(&["s"])));
}

#[test]
fn a_watched_ancestor_attends_its_descendants() {
    let attendance = Attendance::new();
    let watcher = attendance.watcher();
    watcher.attend(&SessionId::new("root"));
    assert!(attendance.attends_any(&chain(&["child", "root"])));
    assert!(!attendance.attends_any(&chain(&["other"])));
    drop(watcher);
    assert!(!attendance.attends_any(&chain(&["child", "root"])));
}

#[test]
fn dropping_the_watcher_unregisters_it() {
    let attendance = Attendance::new();
    let all = attendance.watch_all();
    assert!(attendance.attends_any(&chain(&["anything"])));
    drop(all);
    assert!(!attendance.attends_any(&chain(&["anything"])));
}

/// The TUI's seam: every session is watched for exactly the head's run.
#[tokio::test]
async fn watching_all_holds_the_watch_for_the_whole_run() {
    let attendance = Attendance::new();
    let inside = attendance
        .watching_all(async {
            tokio::task::yield_now().await;
            attendance.attends_any(&chain(&["s"]))
        })
        .await;
    assert!(inside, "watched while the head runs");
    assert!(!attendance.attends_any(&chain(&["s"])), "released after");
}

#[tokio::test]
async fn an_attended_session_never_expires() {
    let attendance = Attendance::new();
    let _all = attendance.watch_all();
    let expiry = expiry(10, &attendance, &["s"], "r");
    assert!(still_waiting(expiry, 100).await);
}

#[tokio::test]
async fn an_unattended_session_expires() {
    let expiry = expiry(10, &Attendance::new(), &["s"], "r");
    assert!(!still_waiting(expiry, 5000).await);
}

#[tokio::test]
async fn a_watcher_arriving_waives_the_deadline_for_good() {
    let attendance = Attendance::new();
    let expiry = expiry(200, &attendance, &["child", "root"], "r");
    let again = expiry.clone();
    let wait = tokio::spawn(expiry.elapsed());
    tokio::time::sleep(Duration::from_millis(20)).await;
    let watcher = attendance.watcher();
    watcher.attend(&SessionId::new("root"));
    tokio::time::sleep(Duration::from_millis(20)).await;
    // The watcher leaves: the waived wait still never elapses.
    drop(watcher);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!wait.is_finished(), "a waived deadline must never fire");
    wait.abort();
    // A clone (an `ask_user` re-park) shares the waiver.
    assert!(still_waiting(again, 400).await, "the waiver is shared");
}

/// `watch_all` arriving (the TUI starting) wakes a running deadline too.
#[tokio::test]
async fn a_watch_all_arriving_waives_the_deadline() {
    let attendance = Attendance::new();
    let wait = tokio::spawn(expiry(200, &attendance, &["s"], "r").elapsed());
    tokio::time::sleep(Duration::from_millis(20)).await;
    let all = attendance.watch_all();
    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(all);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!wait.is_finished(), "the TUI's arrival waived it");
    wait.abort();
}

/// A hibernate unwinds a waived card and resume re-offers it under the same
/// id: it waits again with nobody watching — until it is answered, or its
/// session ends.
#[tokio::test]
async fn a_waiver_outlives_its_park_until_answered_or_ended() {
    let attendance = Attendance::new();
    let all = attendance.watch_all();
    drop(expiry(2000, &attendance, &["s"], "r"));
    drop(all);
    let reoffer = expiry(2000, &attendance, &["s"], "r");
    assert_eq!(reoffer.secs(), 0, "the re-offer is still waived");
    assert_eq!(expiry(2000, &attendance, &["s"], "other").secs(), 2);
    reoffer.settle();
    assert_eq!(expiry(2000, &attendance, &["s"], "r").secs(), 2, "spent");

    let all = attendance.watch_all();
    drop(expiry(2000, &attendance, &["t"], "r"));
    drop(all);
    attendance.forget_session(&SessionId::new("t"));
    assert_eq!(expiry(2000, &attendance, &["t"], "r").secs(), 2, "ended");
}

/// Paused clock: a watcher registering at the deadline instant wins — the
/// deadline re-checks attendance before it gives up.
#[tokio::test(start_paused = true)]
async fn a_watcher_at_the_deadline_instant_still_wins() {
    let attendance = Attendance::new();
    let wait = tokio::spawn(expiry(100, &attendance, &["s"], "r").elapsed());
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    let _all = attendance.watch_all();
    tokio::time::advance(Duration::from_millis(100)).await;
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    assert!(!wait.is_finished(), "the re-check waived it");
    wait.abort();
    let _ = wait.await;
}

/// Paused clock: a watcher arriving mid-timer records the waiver, so the
/// card re-offered after a hibernate still waits with nobody watching.
#[tokio::test(start_paused = true)]
async fn a_mid_timer_arrival_survives_a_reoffer() {
    let attendance = Attendance::new();
    let wait = tokio::spawn(expiry(2000, &attendance, &["s"], "r").elapsed());
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(1000)).await;
    let watcher = attendance.watcher();
    watcher.attend(&SessionId::new("s"));
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    drop(watcher);
    // The hibernate unwinds the park; resume re-offers it, unwatched.
    wait.abort();
    let _ = wait.await;
    let reoffer = expiry(2000, &attendance, &["s"], "r");
    assert_eq!(reoffer.secs(), 0, "the arrival's waiver was recorded");
    let wait = tokio::spawn(reoffer.elapsed());
    tokio::time::advance(Duration::from_millis(5000)).await;
    tokio::task::yield_now().await;
    assert!(!wait.is_finished(), "the re-offered card still waits");
    wait.abort();
}
