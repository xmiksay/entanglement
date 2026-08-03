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

#[tokio::test]
async fn spawn_poll_captures_output_and_exit() {
    let reg = JobRegistry::new();
    let id = reg
        .spawn(
            "echo hi".into(),
            sh("echo hi; echo boom 1>&2"),
            Duration::from_secs(60),
        )
        .unwrap();
    // Give the reaper time to finish and flip status.
    for _ in 0..50 {
        let p = reg.poll(&id, false).unwrap();
        if p.status == JobStatus::Exited(Some(0)) {
            assert_eq!(String::from_utf8_lossy(&p.stdout).trim(), "hi");
            assert_eq!(String::from_utf8_lossy(&p.stderr).trim(), "boom");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
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
        )
        .unwrap();
    // First poll eventually sees "one" while still running.
    let mut seen = false;
    for _ in 0..50 {
        let p = reg.poll(&id, false).unwrap();
        if String::from_utf8_lossy(&p.stdout).contains("one") {
            assert_eq!(p.status, JobStatus::Running);
            seen = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(seen, "first poll never saw the emitted line");
    // A poll drains the buffer, so the immediate next poll has no new output.
    let p2 = reg.poll(&id, false).unwrap();
    assert!(p2.stdout.is_empty(), "second poll should be drained");
    // Kill it so the test process group doesn't leak.
    let _ = reg.poll(&id, true);
}

/// #617: a background job must be killed once it outlives its timeout,
/// not left running forever just because `run_in_background` was set.
#[tokio::test]
async fn spawn_kills_job_that_outlives_timeout() {
    let reg = JobRegistry::new();
    let id = reg
        .spawn(
            "sleep 30".into(),
            sh("sleep 30"),
            Duration::from_millis(200),
        )
        .unwrap();
    for _ in 0..100 {
        let p = reg.poll(&id, false).unwrap();
        if p.timed_out {
            assert_eq!(p.status, JobStatus::Exited(None));
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("job was not killed by its timeout");
}

#[tokio::test]
async fn poll_unknown_job_is_none() {
    let reg = JobRegistry::new();
    assert!(reg.poll("j-unknown999", false).is_none());
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
        pgid: None,
        timeout: Duration::from_secs(60),
        state: Mutex::new(JobState {
            finished: Some(Some(0)),
            finished_at: Some(finished_at),
            ..Default::default()
        }),
    })
}

fn running_job() -> Arc<Job> {
    Arc::new(Job {
        command: "sleep 30".into(),
        pgid: None,
        timeout: Duration::from_secs(60),
        state: Mutex::new(JobState::default()),
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
        .spawn("true".into(), sh("true"), Duration::from_secs(60))
        .unwrap();
    for _ in 0..50 {
        if reg.poll(&id, false).unwrap().status != JobStatus::Running {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    {
        let jobs = reg.inner.jobs.lock().unwrap();
        let job = jobs.get(&id).unwrap();
        let mut st = job.state.lock().unwrap();
        st.finished_at = Some(expired_at);
    }
    let _other = reg
        .spawn("true".into(), sh("true"), Duration::from_secs(60))
        .unwrap();
    assert!(
        reg.poll(&id, false).is_none(),
        "expired job should have been swept on the next spawn"
    );
}
