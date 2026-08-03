use super::*;

fn sh(script: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", script])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    super::super::exec::own_process_group(&mut cmd);
    cmd
}

fn caller() -> SessionId {
    SessionId::new("test-caller")
}

#[tokio::test]
async fn spawn_poll_captures_output_and_exit() {
    let reg = JobRegistry::new();
    let id = reg
        .spawn(
            "echo hi".into(),
            sh("echo hi; echo boom 1>&2"),
            Duration::from_secs(60),
            None,
        )
        .unwrap();
    // Each poll is destructive (#605): the wait can end on new output alone,
    // *before* exit, so stdout/stderr/exit may arrive split across separate
    // polls — accumulate rather than expecting one poll to see everything.
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for _ in 0..50 {
        let p = reg.poll(&id, &caller(), false, 1).await.unwrap();
        stdout.extend(p.stdout);
        stderr.extend(p.stderr);
        if p.status == JobStatus::Exited(Some(0)) {
            assert_eq!(String::from_utf8_lossy(&stdout).trim(), "hi");
            assert_eq!(String::from_utf8_lossy(&stderr).trim(), "boom");
            return;
        }
    }
    panic!("job never reached Exited(0)");
}

#[tokio::test]
async fn poll_is_incremental_then_drains() {
    let reg = JobRegistry::new();
    let id = reg
        .spawn(
            "echo one; sleep 30".into(),
            sh("echo one; sleep 30"),
            Duration::from_secs(60),
            None,
        )
        .unwrap();
    // The first poll waits (via Notify) for the emitted line instead of
    // busy-looping — a real regression test for #605's wakeup path.
    let p = reg.poll(&id, &caller(), false, 5).await.unwrap();
    assert!(
        String::from_utf8_lossy(&p.stdout).contains("one"),
        "poll should have waited for the emitted line"
    );
    assert_eq!(p.status, JobStatus::Running);
    // A poll drains the buffer, so the immediate next poll (bounded, short)
    // sees no new output and returns once its deadline elapses.
    let p2 = reg.poll(&id, &caller(), false, 1).await.unwrap();
    assert!(p2.stdout.is_empty(), "second poll should be drained");
    // Kill it so the test process group doesn't leak.
    let _ = reg.poll(&id, &caller(), true, 0).await;
}

/// #605: a poll issued before any output exists blocks until the job produces
/// something, instead of returning "no new output" instantly — the busy-wait
/// this ADR exists to close.
#[tokio::test]
async fn poll_waits_for_new_output_instead_of_returning_instantly() {
    let reg = JobRegistry::new();
    let id = reg
        .spawn(
            "sleep 0.2; echo late".into(),
            sh("sleep 0.2; echo late"),
            Duration::from_secs(60),
            None,
        )
        .unwrap();
    let started = Instant::now();
    let p = reg.poll(&id, &caller(), false, 5).await.unwrap();
    assert!(
        String::from_utf8_lossy(&p.stdout).contains("late"),
        "poll should have waited for the delayed output"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "poll returned suspiciously fast — did it actually wait?"
    );
}

/// #605: a poll with nothing to report and a short bound returns once the
/// deadline elapses, rather than hanging forever.
#[tokio::test]
async fn poll_times_out_when_nothing_new() {
    let reg = JobRegistry::new();
    let id = reg
        .spawn(
            "sleep 30".into(),
            sh("sleep 30"),
            Duration::from_secs(60),
            None,
        )
        .unwrap();
    let started = Instant::now();
    let p = reg.poll(&id, &caller(), false, 1).await.unwrap();
    assert_eq!(p.status, JobStatus::Running);
    assert!(p.stdout.is_empty() && p.stderr.is_empty());
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert!(started.elapsed() < Duration::from_secs(5));
    let _ = reg.poll(&id, &caller(), true, 0).await;
}

/// #605: a job's owner is the only session that may poll it; any other caller
/// is treated exactly like an unknown id.
#[tokio::test]
async fn poll_scopes_to_the_owning_session() {
    let reg = JobRegistry::new();
    let owner = SessionId::new("owner");
    let stranger = SessionId::new("stranger");
    let id = reg
        .spawn(
            "true".into(),
            sh("true"),
            Duration::from_secs(60),
            Some(owner.clone()),
        )
        .unwrap();
    assert!(
        reg.poll(&id, &stranger, false, 0).await.is_none(),
        "a non-owning session must see this as unknown"
    );
    assert!(reg.poll(&id, &owner, false, 1).await.is_some());
}

/// A job spawned with no owner (the session-less `Tool::run` path) is visible
/// to any caller — matches pre-#605 unscoped behavior for standalone use.
#[tokio::test]
async fn ownerless_job_is_visible_to_any_caller() {
    let reg = JobRegistry::new();
    let id = reg
        .spawn("true".into(), sh("true"), Duration::from_secs(60), None)
        .unwrap();
    assert!(reg
        .poll(&id, &SessionId::new("anyone"), false, 1)
        .await
        .is_some());
}

/// #617: a background job must be killed once it outlives its timeout,
/// not left running forever just because `background` was set.
#[tokio::test]
async fn spawn_kills_job_that_outlives_timeout() {
    let reg = JobRegistry::new();
    let id = reg
        .spawn(
            "sleep 30".into(),
            sh("sleep 30"),
            Duration::from_millis(200),
            None,
        )
        .unwrap();
    let p = reg.poll(&id, &caller(), false, 5).await.unwrap();
    assert!(p.timed_out);
    assert_eq!(p.status, JobStatus::Exited(None));
}

#[tokio::test]
async fn poll_unknown_job_is_none() {
    let reg = JobRegistry::new();
    assert!(reg
        .poll("j-unknown999", &caller(), false, 0)
        .await
        .is_none());
}

#[test]
fn push_capped_drops_oldest_over_cap() {
    let mut buf = Vec::new();
    let mut dropped = 0;
    let big = vec![b'x'; MAX_JOB_BUFFER + 100];
    push_capped(&mut buf, &mut dropped, &big);
    assert_eq!(buf.len(), MAX_JOB_BUFFER);
    assert_eq!(dropped, 100);
}

fn finished_job(finished_at: Instant) -> Arc<Job> {
    Arc::new(Job {
        command: "true".into(),
        owner: None,
        pgid: None,
        timeout: Duration::from_secs(60),
        state: Mutex::new(JobState {
            finished: Some(Some(0)),
            finished_at: Some(finished_at),
            ..Default::default()
        }),
        notify: Notify::new(),
    })
}

fn running_job() -> Arc<Job> {
    Arc::new(Job {
        command: "sleep 30".into(),
        owner: None,
        pgid: None,
        timeout: Duration::from_secs(60),
        state: Mutex::new(JobState::default()),
        notify: Notify::new(),
    })
}

#[test]
fn sweep_evicts_expired_finished_jobs_but_keeps_running_and_fresh() {
    let t0 = Instant::now();
    let mut jobs = HashMap::new();
    jobs.insert("old".to_string(), finished_job(t0));
    jobs.insert("running".to_string(), running_job());
    let later = t0 + Duration::from_secs(120);
    jobs.insert("fresh".to_string(), finished_job(later));

    sweep(&mut jobs, later, Duration::from_secs(60), 100);

    assert!(!jobs.contains_key("old"), "expired entry should be evicted");
    assert!(jobs.contains_key("fresh"), "fresh entry should survive");
    assert!(
        jobs.contains_key("running"),
        "a running job is never evicted"
    );
}

#[test]
fn sweep_caps_finished_jobs_evicting_oldest_first() {
    let t0 = Instant::now();
    let mut jobs = HashMap::new();
    for i in 0..5u64 {
        jobs.insert(format!("j{i}"), finished_job(t0 + Duration::from_secs(i)));
    }

    // TTL generous enough that nothing expires by age; only the cap bites.
    sweep(
        &mut jobs,
        t0 + Duration::from_secs(100),
        Duration::from_secs(1000),
        3,
    );

    assert_eq!(jobs.len(), 3);
    assert!(!jobs.contains_key("j0"));
    assert!(!jobs.contains_key("j1"));
    assert!(jobs.contains_key("j2"));
    assert!(jobs.contains_key("j3"));
    assert!(jobs.contains_key("j4"));
}

#[tokio::test]
async fn finished_job_is_evicted_after_ttl_via_real_poll() {
    // Exercises the real spawn/poll integration path (not just the pure
    // `sweep` helper) by back-dating `finished_at` rather than waiting out
    // the real 15-minute TTL. `checked_sub` avoids panicking on a host
    // whose monotonic clock has less than `JOB_TTL` of uptime behind it.
    let Some(expired_at) = Instant::now().checked_sub(JOB_TTL + Duration::from_secs(1)) else {
        return;
    };
    let reg = JobRegistry::new();
    let id = reg
        .spawn("true".into(), sh("true"), Duration::from_secs(60), None)
        .unwrap();
    let _ = reg.poll(&id, &caller(), false, 5).await;
    {
        let jobs = reg.inner.jobs.lock().unwrap();
        let job = jobs.get(&id).unwrap();
        let mut st = job.state.lock().unwrap();
        st.finished_at = Some(expired_at);
    }
    let _other = reg
        .spawn("true".into(), sh("true"), Duration::from_secs(60), None)
        .unwrap();
    assert!(
        reg.poll(&id, &caller(), false, 0).await.is_none(),
        "expired job should have been swept on the next spawn"
    );
}
