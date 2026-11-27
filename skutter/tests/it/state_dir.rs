//! `skutter`'s managed files follow `ENTANGLEMENT_STATE_DIR` (ADR-0213) and
//! back the live engine (#966). The pin and escape-root stores are checked
//! in-process through their production `managed()` resolution (#967). The stores are seeded at
//! `<dir>/<managed file name>` through their injection seams, then a real
//! `skutter` child — with only the env var pointing at `<dir>`, set on the
//! child alone (#773) — must see them: `inspect mcp` reads the stores
//! directly, and the `pipe` head's `McpList` reply comes from the engine's
//! roster, so an in-memory default wired in by mistake reports `needs auth`.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use std::sync::Arc;

use entanglement_core::{ApprovalScope, StoredAuth, TokenSet, TokenStore};
use entanglement_runtime::extra_roots::{AlwaysRoots, ExtraRootStore};
use entanglement_runtime::mcp::McpToolCache;
use serde_json::Value;
use skutter::config::agent_generation::AgentGenerationStore;
use skutter::config::agent_models::AgentModelStore;
use skutter::config::aux_models::{AuxModelStore, AuxModelsPath, Purpose};
use skutter::config::state_dir::{self, MCP_TOKENS_FILE, MCP_TOOLS_FILE, STATE_DIR_ENV};
use skutter::state::{FileExtraRoots, FileMcpToolCache, McpTokenStore};

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
        .set(
            &mut AuxModelStore::default(),
            Purpose::Narrate,
            "zai",
            PROBE,
        )
        .unwrap();
    FileExtraRoots::at(file(state_dir::EXTRA_ROOTS_FILE))
        .add("read", "/state-dir-probe")
        .unwrap();
}

/// Seeded through each store's injection seam, then re-loaded through the
/// **production** resolution with only the env var pointing at the dir:
/// seeing the probe proves the default went there. The only test in this
/// binary that sets process env (children strip every `ENTANGLEMENT_*`
/// var, `isolated_skutter`), and the probes name nothing else uses.
#[test]
fn the_state_dir_relocates_the_pin_and_escape_root_files() {
    let dir = tempfile::tempdir().unwrap();
    seed_pins(dir.path());

    std::env::set_var(STATE_DIR_ENV, dir.path());
    let agent_models = AgentModelStore::load();
    let agent_generation = AgentGenerationStore::load();
    let aux = AuxModelsPath::managed().load();
    let extra_roots =
        ExtraRootStore::ephemeral().with_always_roots(Arc::new(FileExtraRoots::managed()));
    std::env::remove_var(STATE_DIR_ENV);

    assert_eq!(agent_models.get(PROBE), Some(("zai", "glm-5.3")));
    assert!(agent_generation.get(PROBE).is_some());
    assert_eq!(aux.get(Purpose::Narrate), Some(("zai", PROBE)));
    assert!(extra_roots.is_durably_allowed("read", Path::new("/state-dir-probe")));
    // The store records into the same file.
    extra_roots.record("read", Path::new("/second"), ApprovalScope::Always, "r");
    assert_eq!(
        FileExtraRoots::at(Some(dir.path().join(state_dir::EXTRA_ROOTS_FILE)))
            .load()
            .len(),
        2
    );
}
