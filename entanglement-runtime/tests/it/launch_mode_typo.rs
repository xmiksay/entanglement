//! A typo'd launch mode stops `skutter` before any session exists (#781
//! review, ADR-0218). A refused start must never fall open: if a prompt
//! followed it, ADR-0210's lazy create would run the turn as `general` in
//! `build`, wider than what was asked for, with the agent dropped.

use std::path::Path;
use std::process::Output;

use crate::skutter_cmd::isolated_skutter;

/// Runs `skutter <args>` on the echo backend, optionally with a
/// `config.yml` holding `config`, and returns the output plus whether any
/// session log was written under the sandbox's data dir.
fn launch(args: &[&str], config: Option<&str>) -> (Output, bool) {
    let (mut cmd, sandbox) = isolated_skutter();
    if let Some(config) = config {
        let path = sandbox.path().join("config.yml");
        std::fs::write(&path, config).expect("write config");
        cmd.env("ENTANGLEMENT_CONFIG_FILE", &path);
    }
    let out = cmd
        .args(args)
        .env("ENTANGLEMENT_PROVIDER", "echo")
        .output()
        .expect("failed to spawn skutter");
    (out, any_jsonl(sandbox.path()))
}

fn any_jsonl(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            let p = e.path();
            if p.is_dir() {
                any_jsonl(&p)
            } else {
                p.extension().is_some_and(|x| x == "jsonl")
            }
        })
}

fn assert_refused(out: &Output, wrote_log: bool, needle: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "must exit non-zero; stderr: {stderr}"
    );
    assert!(stderr.contains(needle), "stderr: {stderr}");
    assert!(
        stderr.contains("research | plan | build | auto"),
        "{stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
    // The echo backend would print the prompt back: nothing ran.
    assert!(out.stdout.is_empty(), "a turn ran: {:?}", out.stdout);
    assert!(!wrote_log, "a session log was written");
}

/// Control: the same launch with a real mode runs a turn and logs it, so the
/// refusal assertions above are not vacuous.
#[test]
fn a_valid_mode_runs_and_logs() {
    let (out, log) = launch(
        &["run", "--mode", "research", "--agent", "plan", "hi"],
        None,
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(!out.stdout.is_empty(), "stderr: {stderr}");
    assert!(log, "no session log written");
}

#[test]
fn a_typo_in_the_mode_flag_stops_run() {
    let (out, log) = launch(&["run", "--mode", "reserch", "--agent", "plan", "hi"], None);
    assert_refused(&out, log, "`reserch` from --mode");
}

#[test]
fn a_typo_in_config_mode_stops_run() {
    let (out, log) = launch(&["run", "hi"], Some("mode: bild\n"));
    assert_refused(&out, log, "`bild` from config.yml");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("config.yml) — fix or remove it"),
        "{stderr}"
    );
}

#[test]
fn a_typo_in_the_mode_flag_stops_the_tui_before_it_starts() {
    let (out, log) = launch(&["tui", "--mode", "reserch"], None);
    assert_refused(&out, log, "`reserch` from --mode");
}
