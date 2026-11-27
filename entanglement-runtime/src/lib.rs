//! Runtime crate for the entanglement agent engine.
//!
//! The library half of the engine seam: the `Tool` trait + `ToolRegistry`,
//! tool execution, permission dispatch, sub-agent spawn and event-sourced
//! persistence over `entanglement-core`. It ships no host tool: an embedder
//! registers its own (`skutter` brings the file, shell and fetch tools). It carries no binary
//! and no head, and reads no config or env file: the embedder hands it plain
//! values. The `skutter` crate is the CLI built on it (layered `config.yml`,
//! the managed state files, live reload, the key writer).
//!
//! # Feature gates (ADR-0025)
//!
//! - `default = ["provider", "mcp-http", "rhai"]`.
//! - `provider` — the throttle relay, which polls the
//!   `entanglement-provider` endpoint pool directly.
//! - `mcp-http` — the streamable-HTTP MCP transport + OAuth code paths; the
//!   stdio MCP transport stays in the lean library.
//! - `rhai` — the sandboxed `rhai` scripting tool (ADR-0046, #502).
//!
//! No feature pulls a CLI, TUI or web-server crate (`make check-lean` gates
//! both the default and the `--no-default-features` build).

pub mod agent_registry;
pub mod agent_send;
pub mod agents;
// Pre-dispatch argument validation against a tool's advertised `ToolSpec`
// schema (#560, ADR-0196 §6): the three-way error taxonomy (schema
// violation / parameter error / command failure) plus the delivered-schema
// and loop-breaker guards.
pub mod arg_validate;
pub mod ask_user;
// Which sessions a head is watching (ADR-0247): an attended session's
// cards and questions wait for their approver instead of collapsing.
pub mod attendance;
pub mod cancel;
// The capability vocabulary tools declare (#560, ADR-0207 §3), consumed by
// `crate::policy::ModeResolver` (stage 4) to grade a call by its tool's
// declared capabilities under the session's permission mode.
pub mod capability;
mod date;
// The discovery tools (ADR-0226): `list_agents`/`list_models`/`list_skills`/
// `list_mcp`/`mcp_tools` plus the `mcp` envelope spec. Ungated — pure
// state/logic over core + the lean `mcp` module, needed by the lean build's
// executor too.
pub mod discover;
pub mod env_date;
pub mod exec;
// The running call's `ToolExec` seq, read by the per-write backups.
pub mod exec_scope;
pub mod file_backup;
pub mod file_change;
pub mod frontmatter;
pub mod history;
// Host script namespaces for embedders (ADR-0249).
pub mod host_ns;
// The credentialed-`http()` gate (ADR-0227) — shared by the `http` tool and
// the dispatch seam; ungated so the lean executor compiles the same ladder.
pub mod http_gate;
// The credentials that gate injects (ADR-0227), as plain data.
pub mod http_auth;
// The rhai-only `http` tool behind that gate.
#[cfg(feature = "rhai")]
pub mod http_tool;
pub mod jobs;
pub mod layers;
// The identifier mapping for generated rhai namespaces (ADR-0227).
#[cfg(feature = "rhai")]
pub mod rhai_ident;
// `rhai.trusted` folders on disk: containment and the trusted-script write
// floor (ADR-0228).
pub mod rhai_trust;
// Env vars no child process may inherit — `http_auth`'s secrets (ADR-0227).
pub mod secret_env;
// MCP client — attach external tool servers as a runtime-side tool provider
// (#198, #312). The stdio transport lives in the lean library (tokio process +
// serde_json only), so an embedder gets external tools without any
// CLI/TUI/transport dep; the streamable-HTTP transport rides the `mcp-http`
// feature (reqwest), keeping the lean build transport-free (ADR-0025).
pub mod mcp;
// Permission modes (#560, ADR-0207): the mode table and its grade-resolution
// engine, built on `capability`'s vocabulary. Wired into the dispatch ladder
// (`crate::policy::ModeResolver`) as of stage 4. Ungated — pure logic
// over core types + capability + serde_yaml, needed by the lean build too.
pub mod mode;
// Pending-operations listing (#607, ADR-0161 §6): shared by `poll`'s
// no-handle model surface and `InMsg::ListOperations`'s head surface.
pub mod operations;
pub mod output;
// Path grading: the session-workdir rewrite of relative path arguments.
pub mod path_grading;
pub mod pending;
pub mod permission;
mod permission_arg;
// Per-segment `bash` command grading (ADR-0197): the runtime-only wrapper
// every `resolve_scoped` call site for a `bash` argument routes through, so
// a compound command (`&&`/`|`/`;`/...) is graded segment-by-segment instead
// of as one full-string glob match. Ungated — pure logic over core types and
// `shell_split`, needed by the lean build's dispatch path too.
pub mod permission_path;
pub mod persistence;
pub mod plan_files;
pub mod plan_tasks;
pub mod plan_watch;
pub mod policy;
// The `poll` join tool (#605, ADR-0161 §1-4) — replaces `bash_output`/
// `agent_poll` outright.
pub mod poll;
pub mod propose_plan;
pub mod questions;
pub mod request_mode;
pub mod retained_output;
// `question_timeout`/`on_timeout` unattended-run policy (ADR-0207 §11,
// stage 5c): the `Ask`-collapses-to-deny rule, its repeat-denial
// escalation, and a timed-out `ask_user` call's default answers.
pub mod run_limits;
// Sandboxed `rhai` script tool (#122, ADR-0046). Behind the `rhai` feature
// (default-on, #502/ADR-0135) so a lean embedder can drop the dep via
// `--no-default-features`.
#[cfg(feature = "rhai")]
pub mod script;
// Background-script registry (#637, ADR-0185): shared by `rhai`'s
// `background: true` launcher and `poll`'s `x-` handle path. Ungated — it is
// plain state with no rhai dep, so `poll`/`operations` reference it
// unconditionally (empty forever in a lean build without the `rhai` feature).
pub mod script_ops;
pub mod seam;
pub mod session_store;
// Conservative, quote-aware compound-command splitter (ADR-0197) — the
// splitting side a mode's `bash` rules grade against. Ungated — pure `char`
// scanning, no deps.
pub mod shell_split;
pub mod skills;
pub mod subagent;
pub mod system_prompt;
// Composes the ADR-0196 §5 `ToolSearch`-mode prompt slimming with the
// existing env-date freshness patch into the one `SystemPromptResolver`
// slot `EngineConfig` exposes. Ungated — pure string transforms over core
// types, needed by the lean build's `Config`-driven mode too.
pub mod system_prompt_mode;
#[cfg(test)]
mod test_tools;
// Wire-visible LLM-endpoint throttle transitions (#517, ADR-0141). Behind
// `provider` since it polls `entanglement_provider::HttpClient` directly.
#[cfg(feature = "provider")]
pub mod throttle;
pub mod tool_names;
// The one advertised surface (ADR-0226): the one-surface resolver, the
// per-session surface snapshots and delivered-schema set. Ungated — pure state
// over core types, needed by the lean build too.
pub mod tool_advertising;
pub mod tool_runner;
// The three-state (`allowed`/`asks`/`declines`) per-tool posture the profile
// UIs render, now that advertisement no longer varies with the mask.
pub mod tool_state;
// The host-tool vocabulary (`Tool` trait + `ToolRegistry`) lives here, not in
// core: core holds no executable tools, only advertises schemas and round-trips
// each call back to the runtime (#206, ADR-0006/0010/0053).
pub mod tools;
// The debounced file-watch primitive (#329) `plan_watch` and an embedder's
// own reload watcher share.
pub mod watch;

pub use tools::{SharedRegistry, Tool, ToolRegistry};

/// A pool client with cross-process endpoint state disabled (#743): a unit
/// test must never read or write the developer's real endpoint-state dir, and
/// steering it via process env would race every parallel test.
#[cfg(test)]
pub(crate) fn test_http_client() -> entanglement_core::HttpClient {
    entanglement_core::HttpClient::with_shared_state(
        entanglement_core::SharedStateLocation::Disabled,
        entanglement_core::RetryConfig::default(),
    )
    .expect("reqwest client builds in tests")
}
