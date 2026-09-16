//! Definition-driven HTTP endpoint tools (#560, P8 of the tool-search plan):
//! `config.yml`'s `endpoints:` map, each entry an HTTP call tool registered
//! as `endpoint__<name>` — method, a URL template with `{{param}}`
//! substitution from the tool call's arguments, optional static headers
//! (`${VAR}`-expanded), an optional static body template, and a 32 KiB
//! response cap (truncated with a marker, never an error). The same
//! [`EndpointConfig`]/[`EndpointTool`] machinery backs a skill's inline
//! `tools:` endpoint entries (`skills::tools::SkillToolDef::Endpoint`),
//! registered under `skill__<skill>__<name>` instead — see that module.
//!
//! Schema derivation ([`config::build_schema`]) means P4's pre-dispatch
//! argument validation and `describe()` work for an endpoint tool exactly
//! like any other registered tool, with zero extra wiring — both already
//! operate generically over the [`crate::tools::ToolRegistry`]. Likewise
//! `explore`'s index and `describe`'s by-name lookup need no endpoint-
//! specific code beyond a distinct `source` label
//! ([`crate::discover::explore`]) — registration into the shared registry is
//! the whole integration surface.
//!
//! Execution rides `entanglement_core::call_endpoint` — the shared
//! per-endpoint pool/retry/rate-limit machinery ([`entanglement_provider::client::HttpClient`]),
//! not a bespoke `reqwest::Client` (ADR-0053: `reqwest` is provider-owned).
//!
//! Registration is **startup-only**, not live-reloaded like skills/agents/MCP
//! servers: `config.yml` already reloads other sections live via
//! [`crate::watch`], but wiring a debounced re-diff specifically for
//! `endpoints:` (add/remove/redefine an `EndpointTool` mid-session) is
//! disproportionate to a feature with no evidence anyone needs to edit an
//! endpoint definition without restarting — unlike skills/agents, which
//! already had the watcher plumbing this would have had to duplicate.

mod config;
mod tool;

pub use config::{EndpointConfig, EndpointParam};
pub use tool::{call_capability_names, register_endpoints, EndpointTool};
