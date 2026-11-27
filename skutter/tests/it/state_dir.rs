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

use entanglement_core::{ApprovalScope, StoredAuth, TokenSet, TokenStore};
use entanglement_runtime::mcp::McpToolCache;
use serde_json::Value;
use skutter::aux_llm::Purpose;
use skutter::config::agent_generation::AgentGenerationStore;
use skutter::config::agent_models::AgentModelStore;
use skutter::config::aux_models::AuxModelsPath;
use skutter::config::state_dir::{self, MCP_TOKENS_FILE, MCP_TOOLS_FILE, STATE_DIR_ENV};
use skutter::state::{ExtraRootStore, FileMcpToolCache, McpTokenStore};

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

const PROBE: &str = "state-dir-probe";

/// Write one probe into each pin/escape-root file under `dir`, by file name.
fn seed_pins(dir: &Path) {
    let file = |name: &str| Some(dir.join(name));
    AgentModelStore::load_from(file(state_dir::AGENT_MODELS_FILE))
        .set(PROBE, "zai", "glm-5.3")
        .unwrap();
    AgentGenerationStore::load_from(file(state_dir::AGENT_GENERATION_FILE))
        .set(PROBE, Default::default())
        .unwrap();
    AuxModelsPath::at(file(state_dir::AUX_MODELS_FILE))
        .record(Purpose::Narrate, "zai", PROBE)
        .unwrap();
    ExtraRootStore::at(file(state_dir::EXTRA_ROOTS_FILE)).record(
        "read",
        Path::new("/state-dir-probe"),
        ApprovalScope::Always,
    );
}

/// The pin and escape-root stores (#967), seeded through each store's
/// injection seam, then re-loaded through the **production** resolution by
/// [`pins_follow_the_state_dir`] in a child run of this test binary with
/// only the env var pointing at the dir — set on the child alone, so no
/// test here races a process-env change (#773).
#[test]
fn the_state_dir_relocates_the_pin_and_escape_root_files() {
    let dir = tempfile::tempdir().unwrap();
    seed_pins(dir.path());
    let out = Command::new(std::env::current_exe().expect("test binary path"))
        .args([
            "--exact",
            "state_dir::pins_follow_the_state_dir",
            "--ignored",
            "--test-threads=1",
        ])
        .env(STATE_DIR_ENV, dir.path())
        .output()
        .expect("spawn the test binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("1 passed"),
        "the child ran no test: {stdout}"
    );
    // The store recorded into the same file.
    let file = ExtraRootStore::at(Some(dir.path().join(state_dir::EXTRA_ROOTS_FILE)));
    assert!(file.is_durably_allowed("read", Path::new("/second")));
}

/// The child half of the test above: only meaningful with the state dir set.
#[test]
#[ignore = "run by the_state_dir_relocates_the_pin_and_escape_root_files with the env set"]
fn pins_follow_the_state_dir() {
    assert!(
        std::env::var_os(STATE_DIR_ENV).is_some(),
        "run through the_state_dir_relocates_the_pin_and_escape_root_files"
    );
    let agent_models = AgentModelStore::load();
    let agent_generation = AgentGenerationStore::load();
    let aux = AuxModelsPath::managed().load();
    let extra_roots = ExtraRootStore::managed();

    assert_eq!(agent_models.get(PROBE), Some(("zai", "glm-5.3")));
    assert!(agent_generation.get(PROBE).is_some());
    assert_eq!(aux.get(Purpose::Narrate), Some(("zai", PROBE)));
    assert!(extra_roots.is_durably_allowed("read", Path::new("/state-dir-probe")));
    extra_roots.record("read", Path::new("/second"), ApprovalScope::Always);
}
