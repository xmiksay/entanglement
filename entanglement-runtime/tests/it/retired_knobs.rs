//! ADR-0213 + ADR-0226 end to end: a real `skutter` started with retired env
//! vars and retired `config.yml` keys still runs, and prints exactly one
//! warning line per retired knob naming its replacement.

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
    // The `mcp:` entry also pins "nothing connects at startup" (ADR-0226):
    // its unreachable URL would otherwise cost a connect attempt or a warning.
    std::fs::write(
        project.join("config.yml"),
        "discovery:\n  zai: invoke\ntool_advertising: full\n\
         mcp:\n  kb:\n    url: http://127.0.0.1:1/mcp\n    state: allowed\n",
    )
    .unwrap();

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
        ("`discovery:`", "one advertised surface"),
        ("`tool_advertising:`", "ADR-0226"),
        ("`mcp.kb.state:`", "disabled: true"),
        (
            "ENTANGLEMENT_DEBUG: ignoring unknown token(s) bogus",
            "known:",
        ),
    ] {
        let hits: Vec<_> = lines.iter().filter(|l| l.contains(var)).collect();
        assert_eq!(hits.len(), 1, "{var}: {stderr}");
        assert!(hits[0].contains(replacement), "{var}: {}", hits[0]);
    }
    assert_eq!(lines.len(), 9, "nothing else warns: {stderr}");
}

/// #785: a retired var parked in the managed `.env` (not the shell) is
/// warned about too — the file feeds the same process env.
#[test]
fn a_retired_var_in_the_managed_env_file_warns() {
    let (mut cmd, _sandbox) = isolated_skutter();
    let root = tempfile::tempdir().unwrap();
    let env_file = root.path().join("managed.env");
    std::fs::write(&env_file, "ENTANGLEMENT_ECHO_FULL=1\n").unwrap();
    let out = cmd
        .args(["run", "hi"])
        .current_dir(root.path())
        .env("ENTANGLEMENT_PROVIDER", "echo")
        .env("ENTANGLEMENT_ENV_FILE", &env_file)
        .output()
        .expect("failed to spawn skutter");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines = warnings(&stderr);
    assert_eq!(lines.len(), 1, "{stderr}");
    assert!(
        lines[0].contains("`ENTANGLEMENT_ECHO_FULL`"),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains("ENTANGLEMENT_DEBUG=echo"), "{}", lines[0]);
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

/// ADR-0226: two configured MCP servers whose tool names would collide
/// (`my.docs` / `my_docs` sanitize identically) keep only the first, and the
/// dropped one is a startup warning, not just a log line.
#[test]
fn a_clashing_mcp_server_name_warns_at_startup() {
    let (mut cmd, _sandbox) = isolated_skutter();
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join(".entanglement");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("config.yml"),
        "mcp:\n  my.docs:\n    url: http://127.0.0.1:1/a\n  my_docs:\n    url: http://127.0.0.1:1/b\n",
    )
    .unwrap();
    let out = cmd
        .args(["run", "hi"])
        .current_dir(root.path())
        .env("ENTANGLEMENT_PROVIDER", "echo")
        .output()
        .expect("failed to spawn skutter");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let hits: Vec<_> = warnings(&stderr)
        .into_iter()
        .filter(|l| l.contains("`my_docs` ignored") && l.contains("`my.docs`"))
        .collect();
    assert_eq!(hits.len(), 1, "{stderr}");
}
