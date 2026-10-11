//! Tests for the status-line command runner (#762 part B): the line
//! cleaning, one run end to end, timeout containment, coalescing, and the
//! App's success/failure/reload handling.

use super::*;
use crate::config::statusline::StatuslineConfig;
use crate::tui::app::App;
use crate::tui::statusline::payload::{Payload, VERSION};
use entanglement_core::SessionId;

fn payload(session: &str) -> Payload {
    Payload {
        v: VERSION,
        session: session.to_string(),
        cwd: "/w".into(),
        root: None,
        repo: None,
        branch: None,
        worktree: None,
        agent: "build".into(),
        mode: "build".into(),
        model: None,
        cost_usd: None,
        context_pct: None,
        action: None,
        tasks: None,
    }
}

fn job(command: &str, timeout_ms: u64) -> Job {
    Job {
        command: command.to_string(),
        timeout: Duration::from_millis(timeout_ms),
        payload: payload("s1"),
    }
}

#[test]
fn only_the_first_line_survives_stripped_of_escapes_and_controls() {
    assert_eq!(first_line(b"one\ntwo\n"), "one");
    assert_eq!(
        first_line(b"\x1b[1;31mred\x1b[0m \x1b]8;;http://x\x07link\x1b]8;;\x1b\\ end"),
        "red link end"
    );
    assert_eq!(first_line(b"a\tb\x07c\rd\x1bMe  "), "a bcde");
    assert_eq!(first_line("wide 漢字 ✓".as_bytes()), "wide 漢字 ✓");
    assert_eq!(first_line(b"\xffbad utf8"), "\u{fffd}bad utf8");
    assert_eq!(first_line(b""), "");
}

#[cfg(unix)]
mod unix {
    use super::*;

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only probes for existence; no memory effects.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    async fn gone_within(pid: i32, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if !alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        !alive(pid)
    }

    #[tokio::test]
    async fn the_payload_arrives_on_stdin_and_the_first_line_is_kept() {
        let out = run(&job("head -n 1; echo second", 5000), &[])
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            (json["v"].as_u64(), json["session"].as_str()),
            (Some(1), Some("s1"))
        );
        assert!(json["root"].is_null());
    }

    #[tokio::test]
    async fn output_is_read_through_a_bounded_window() {
        // 1 MiB on one line: the run still finishes (the rest is drained)
        // and the line is capped.
        let out = run(&job("head -c 1048576 /dev/zero | tr '\\0' x", 5000), &[])
            .await
            .unwrap();
        assert_eq!(out.len(), READ_CAP);
    }

    #[tokio::test]
    async fn provider_keys_are_scrubbed_from_the_child() {
        // HOME stands in for a key var: always set, never mutated by tests.
        let out = run(&job("echo \"${HOME-scrubbed}\"", 5000), &["HOME".into()])
            .await
            .unwrap();
        assert_eq!(out, "scrubbed");
    }

    #[tokio::test]
    async fn a_failing_command_reports_its_code_and_stderr() {
        let err = run(&job("echo out; echo 'no jq' >&2; exit 3", 5000), &[])
            .await
            .unwrap_err();
        assert_eq!(err, "exited with code 3: no jq");
    }

    #[tokio::test]
    async fn a_timeout_kills_the_whole_group_and_leaves_no_zombie() {
        let dir = tempfile::tempdir().unwrap();
        let pids = dir.path().join("pids");
        let cmd = format!(
            "echo $$ > {p}; sleep 30 & echo $! >> {p}; wait",
            p = pids.display()
        );
        let started = Instant::now();
        let err = run(&job(&cmd, 300), &[]).await.unwrap_err();
        assert_eq!(err, "timed out after 300 ms");
        assert!(started.elapsed() < Duration::from_secs(5));
        let text = std::fs::read_to_string(&pids).unwrap();
        let pids: Vec<i32> = text.lines().map(|l| l.trim().parse().unwrap()).collect();
        assert_eq!(pids.len(), 2, "{text}");
        // The leader was reaped by `run` (a zombie would still answer
        // signal 0); the backgrounded grandchild died with the group.
        assert!(!alive(pids[0]), "leader left as a zombie");
        assert!(gone_within(pids[1], Duration::from_secs(3)).await);
    }

    async fn next_outcome(rx: &mut mpsc::Receiver<Event>) -> Outcome {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(Event::StatusCommand(o))) => o,
            other => panic!("expected an outcome, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_burst_of_changes_coalesces_into_one_follow_up_run() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let cmd = format!("cat >> {l}; echo >> {l}; echo ok", l = log.display());
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let jobs = spawn_with(event_tx, Vec::new(), Duration::from_millis(400));
        let send = |i: usize| {
            let mut j = job(&cmd, 5000);
            j.payload = payload(&format!("burst-{i}"));
            jobs.send_replace(Some(j));
        };
        let started = Instant::now();
        send(0);
        assert_eq!(next_outcome(&mut event_rx).await.result, Ok("ok".into()));
        // The runner now sits out the rest of its interval; every change in
        // it folds into one follow-up run of the latest payload.
        for i in 1..20 {
            send(i);
            tokio::task::yield_now().await;
        }
        assert_eq!(next_outcome(&mut event_rx).await.result, Ok("ok".into()));
        assert!(
            started.elapsed() >= Duration::from_millis(400),
            "rate limited"
        );
        // No third run: nothing changed after the follow-up was taken.
        let extra = tokio::time::timeout(Duration::from_millis(900), event_rx.recv()).await;
        assert!(extra.is_err(), "unexpected extra run: {extra:?}");
        let runs = std::fs::read_to_string(&log).unwrap();
        let runs: Vec<&str> = runs.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert!(
            runs[1].contains("burst-19"),
            "the follow-up sees the latest payload"
        );
    }

    #[tokio::test]
    async fn a_forced_resend_reruns_an_unchanged_job() {
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let jobs = spawn_with(event_tx, Vec::new(), Duration::from_millis(10));
        jobs.send_replace(Some(job("echo hi", 5000)));
        assert_eq!(next_outcome(&mut event_rx).await.result, Ok("hi".into()));
        jobs.send_modify(|_| {});
        assert_eq!(next_outcome(&mut event_rx).await.result, Ok("hi".into()));
    }

    #[tokio::test]
    async fn the_runner_stops_with_the_tui_and_kills_a_run_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let (event_tx, event_rx) = mpsc::channel(16);
        let jobs = spawn_with(event_tx, Vec::new(), Duration::from_millis(10));
        let cmd = format!("echo $$ > {}; sleep 30", pid_file.display());
        jobs.send_replace(Some(job(&cmd, 60_000)));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pid_file.exists() || std::fs::read_to_string(&pid_file).unwrap().is_empty() {
            assert!(Instant::now() < deadline, "the run never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        drop(event_rx); // the TUI quit
        tokio::time::timeout(Duration::from_secs(5), jobs.closed())
            .await
            .expect("the runner task ended");
        assert!(gone_within(pid, Duration::from_secs(3)).await);
    }
}

fn app_with_command(command: &str) -> App {
    let mut app = App::new_for_test(SessionId::new("s1"));
    app.set_statusline_config(StatuslineConfig {
        command: Some(command.into()),
        ..StatuslineConfig::default()
    });
    app
}

fn outcome(command: &str, result: Result<&str, &str>) -> Outcome {
    Outcome {
        command: command.into(),
        result: result.map(str::to_string).map_err(str::to_string),
    }
}

#[test]
fn success_shows_the_line_and_failures_fall_back_with_one_toast_per_streak() {
    let mut app = app_with_command("x");
    app.on_status_command(outcome("x", Ok("from script")));
    assert_eq!(
        app.status_line().command.line.as_deref(),
        Some("from script")
    );
    assert!(app.toast().is_none());

    app.on_status_command(outcome("x", Err("timed out after 1000 ms")));
    assert_eq!(app.status_line().command.line, None, "template again");
    assert!(app.toast().unwrap().contains("timed out"));
    app.clear_toast();
    app.on_status_command(outcome("x", Err("exited with code 1")));
    assert!(app.toast().is_none(), "one toast per failure streak");

    app.on_status_command(outcome("x", Ok("back")));
    assert_eq!(app.status_line().command.line.as_deref(), Some("back"));
    app.on_status_command(outcome("x", Err("exited with code 1")));
    assert!(app.toast().is_some(), "a new streak toasts again");
}

#[test]
fn the_last_good_line_survives_until_a_run_reports() {
    let mut app = app_with_command("x");
    app.on_status_command(outcome("x", Ok("good")));
    // An unrelated reload and a stale result for a replaced command leave
    // the last good line alone.
    app.set_statusline_config(StatuslineConfig {
        command: Some("x".into()),
        right: String::new(),
        ..StatuslineConfig::default()
    });
    app.on_status_command(outcome("old", Err("exited with code 1")));
    assert_eq!(app.status_line().command.line.as_deref(), Some("good"));
    assert!(app.toast().is_none());
}

#[test]
fn reloading_a_different_command_or_none_resets_to_the_template() {
    let mut app = app_with_command("x");
    app.on_status_command(outcome("x", Err("boom")));
    app.set_statusline_config(StatuslineConfig {
        command: Some("y".into()),
        ..StatuslineConfig::default()
    });
    let state = &app.status_line().command;
    assert!(state.line.is_none() && !state.failing, "fresh streak");
    app.on_status_command(outcome("y", Ok("new")));
    app.set_statusline_config(StatuslineConfig::default());
    assert!(app.status_line().command.line.is_none());
}

#[tokio::test]
async fn sync_publishes_only_on_change() {
    let mut app = app_with_command("echo hi");
    let (tx, mut rx) = tokio::sync::watch::channel(None);
    app.set_status_command_runner(tx);
    app.sync_status_command();
    assert!(rx.has_changed().unwrap());
    let sent = rx.borrow_and_update().clone().unwrap();
    assert_eq!(
        (sent.command.as_str(), sent.timeout),
        ("echo hi", Duration::from_secs(1))
    );
    app.sync_status_command();
    assert!(!rx.has_changed().unwrap(), "unchanged payload: no wake-up");
    app.refresh_status_line();
    assert!(rx.has_changed().unwrap(), "an exec tool forces a re-run");
    rx.borrow_and_update();
    app.set_statusline_config(StatuslineConfig::default());
    app.sync_status_command();
    assert!(
        rx.borrow_and_update().is_none(),
        "no command: the runner idles"
    );
}

#[test]
fn the_command_line_replaces_only_the_left_template() {
    let text = |app: &App| -> String {
        let line = crate::tui::statusline::status_line(app, 60);
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    };
    let mut app = app_with_command("x");
    assert!(
        !text(&app).contains("from script"),
        "template until a run lands"
    );
    app.on_status_command(outcome("x", Ok("from script")));
    let row = text(&app);
    assert!(row.starts_with("from script"), "{row}");
    assert!(row.trim_end().ends_with("s1"), "right template kept: {row}");
}
