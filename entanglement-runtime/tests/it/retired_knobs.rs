//! ADR-0213 end to end: a real `skutter` started with retired env vars and a
//! retired `config.yml` key still runs, and prints exactly one warning line
//! per retired knob naming its replacement.

use crate::skutter_cmd::isolated_skutter;

fn warnings(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|l| l.starts_with("skutter: warning:"))
        .collect()
}

#[test]
fn each_retired_knob_warns_once_and_startup_continues() {
    let (mut cmd, _sandbox) = isolated_skutter();
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join(".entanglement");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("config.yml"), "discovery:\n  zai: invoke\n").unwrap();

    let out = cmd
        .args(["run", "hi"])
        .current_dir(root.path())
        .env("ENTANGLEMENT_PROVIDER", "echo")
        .env("ENTANGLEMENT_LOG_BODIES", "1")
        .env("ENTANGLEMENT_TUI_NO_MOUSE", "1")
        .env("ENTANGLEMENT_GRANTS_FILE", root.path().join("g.yml"))
        .env("OLLAMA_BASE", "http://localhost:1/v1")
        .env("ZAI_RPM", "10")
        .env("ENTANGLEMENT_DEBUG", "bodies,bogus")
        .output()
        .expect("failed to spawn skutter");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines = warnings(&stderr);

    for (var, replacement) in [
        ("`ENTANGLEMENT_LOG_BODIES`", "ENTANGLEMENT_DEBUG=bodies"),
        ("`ENTANGLEMENT_TUI_NO_MOUSE`", "tui: {mouse: false}"),
        ("`ENTANGLEMENT_GRANTS_FILE`", "ENTANGLEMENT_STATE_DIR"),
        ("`OLLAMA_BASE`", "OLLAMA_API_BASE"),
        ("`ZAI_RPM`", "providers.yml"),
        ("`discovery:`", "catalog"),
        (
            "ENTANGLEMENT_DEBUG: ignoring unknown token(s) bogus",
            "known:",
        ),
    ] {
        let hits: Vec<_> = lines.iter().filter(|l| l.contains(var)).collect();
        assert_eq!(hits.len(), 1, "{var}: {stderr}");
        assert!(hits[0].contains(replacement), "{var}: {}", hits[0]);
    }
    assert_eq!(lines.len(), 7, "nothing else warns: {stderr}");
}

#[test]
fn a_clean_environment_prints_no_retirement_warning() {
    let (mut cmd, _sandbox) = isolated_skutter();
    let root = tempfile::tempdir().unwrap();
    let out = cmd
        .args(["run", "hi"])
        .current_dir(root.path())
        .env("ENTANGLEMENT_PROVIDER", "echo")
        .output()
        .expect("failed to spawn skutter");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(warnings(&stderr).is_empty(), "{stderr}");
}
