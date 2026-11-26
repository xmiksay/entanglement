//! Startup warnings a real `skutter` prints on stderr, end to end, and the
//! pre-0.8 config keys that are now a plain parse error instead (#920).

use crate::skutter_cmd::isolated_skutter;

fn warnings(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|l| l.starts_with("skutter: warning:"))
        .collect()
}

#[test]
fn an_unknown_debug_token_warns_once_and_startup_continues() {
    let (mut cmd, _sandbox) = isolated_skutter();
    let root = tempfile::tempdir().unwrap();
    let out = cmd
        .args(["run", "hi"])
        .current_dir(root.path())
        .env("ENTANGLEMENT_PROVIDER", "echo")
        .env("ENTANGLEMENT_DEBUG", "bodies,bogus")
        .output()
        .expect("failed to spawn skutter");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines = warnings(&stderr);
    assert_eq!(lines.len(), 1, "{stderr}");
    assert!(
        lines[0].contains("ENTANGLEMENT_DEBUG: ignoring unknown token(s) bogus"),
        "{}",
        lines[0]
    );
}

/// #920: no migration shim — a key an older release read is an unknown
/// field like any typo, and startup fails naming it.
#[test]
fn a_pre_0_8_config_key_is_a_parse_error() {
    for yaml in [
        "tool_advertising: full\n",
        "permissions:\n  bash: ask\n",
        "mcp:\n  kb:\n    url: http://127.0.0.1:1/mcp\n    state: allowed\n",
    ] {
        let (mut cmd, _sandbox) = isolated_skutter();
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join(".entanglement");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("config.yml"), yaml).unwrap();
        let out = cmd
            .args(["run", "hi"])
            .current_dir(root.path())
            .env("ENTANGLEMENT_PROVIDER", "echo")
            .output()
            .expect("failed to spawn skutter");
        assert_ne!(out.status.code(), Some(0), "{yaml}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("unknown field"), "{yaml}: {stderr}");
        let kept = std::fs::read_to_string(project.join("config.yml")).unwrap();
        assert_eq!(kept, yaml, "the file is never rewritten");
    }
}

#[test]
fn a_clean_environment_prints_no_warning() {
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
