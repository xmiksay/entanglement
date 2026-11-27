//! `skutter`'s managed files follow `ENTANGLEMENT_STATE_DIR` (ADR-0213) and
//! back the live engine (#966). The stores are seeded at
//! `<dir>/<managed file name>` through their injection seams, then a real
//! `skutter` child — with only the env var pointing at `<dir>`, set on the
//! child alone (#773) — must see them: `inspect mcp` reads the stores
//! directly, and the `pipe` head's `McpList` reply comes from the engine's
//! roster, so an in-memory default wired in by mistake reports `needs auth`.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use entanglement_core::{StoredAuth, TokenSet, TokenStore};
use entanglement_runtime::config::state_dir::{MCP_TOKENS_FILE, MCP_TOOLS_FILE, STATE_DIR_ENV};
use entanglement_runtime::mcp::McpToolCache;
use serde_json::Value;
use skutter::state::{FileMcpToolCache, McpTokenStore};

use crate::skutter_cmd::isolated_skutter;

const URL: &str = "http://127.0.0.1:1/mcp";

fn auth() -> StoredAuth {
    StoredAuth {
        client_id: "cid".into(),
        client_secret: None,
        token_endpoint: "https://as.example/token".into(),
        revocation_endpoint: None,
        resource: None,
        tokens: TokenSet {
            access_token: "a".into(),
            refresh_token: None,
            token_type: "Bearer".into(),
            expires_at: None,
            scope: None,
        },
    }
}

/// A `skutter` child whose user config registers one OAuth server, `kb`,
/// and whose state dir holds a credential and a last-seen tool list for it.
fn seeded_skutter(dir: &Path) -> (Command, tempfile::TempDir) {
    let state = dir.join("state");
    McpTokenStore::at(Some(state.join(MCP_TOKENS_FILE)))
        .save("kb", &auth())
        .unwrap();
    FileMcpToolCache::at(Some(state.join(MCP_TOOLS_FILE)))
        .record("kb", URL, &["search".to_string()])
        .unwrap();
    let config = dir.join("config.yml");
    std::fs::write(
        &config,
        format!("mcp:\n  kb:\n    url: {URL}\n    oauth: {{}}\n"),
    )
    .unwrap();
    let (mut cmd, sandbox) = isolated_skutter();
    cmd.current_dir(dir)
        .env(STATE_DIR_ENV, &state)
        .env("ENTANGLEMENT_CONFIG_FILE", &config);
    (cmd, sandbox)
}

#[test]
fn the_state_dir_relocates_skutters_managed_files() {
    let dir = tempfile::tempdir().unwrap();
    let (mut cmd, _sandbox) = seeded_skutter(dir.path());
    let out = cmd
        .args(["inspect", "mcp"])
        .output()
        .expect("spawn skutter");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("kb (http) [oauth: authenticated]"),
        "{stdout}"
    );
    assert!(stdout.contains("tools (last seen): search"), "{stdout}");
}

#[test]
fn the_live_engine_reads_the_managed_token_file() {
    let dir = tempfile::tempdir().unwrap();
    let (mut cmd, _sandbox) = seeded_skutter(dir.path());
    let mut child = cmd
        .arg("pipe")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn skutter pipe");
    let mut stdin = child.stdin.take().expect("piped stdin");
    writeln!(stdin, r#"{{"kind":"mcp_list","correlation_id":"c1"}}"#).unwrap();
    // A watchdog: no reply in time closes stdin, the relay ends, and the read
    // below hits EOF instead of hanging the suite.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(60));
        drop(stdin);
    });

    let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let reply = stdout
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .find(|ev| ev["kind"] == "mcp_list" && ev["correlation_id"] == "c1");
    let _ = child.kill();
    let _ = child.wait();

    let reply = reply.expect("an McpList reply");
    let servers = reply["servers"].as_array().expect("servers");
    assert_eq!(servers.len(), 1, "{reply}");
    assert_eq!(servers[0]["name"], "kb");
    assert_eq!(servers[0]["auth"], "authenticated", "{reply}");
}
