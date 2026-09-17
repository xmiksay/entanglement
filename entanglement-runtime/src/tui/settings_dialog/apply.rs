//! The `/set` dialog's apply pipeline: a plan of steps in the settled order
//! (model → generation → tools/MCP → re-pin → aux — no `agent` step, ADR-0207
//! §9: the agent row is read-only display, there is no live switch), executed
//! through a side-effect seam so the order and stop-on-failure rule are
//! testable without an engine. Each live effect reuses the single-purpose
//! command's own message/store path (`app/settings.rs`).

use entanglement_core::ToolOverlayEntry;
use entanglement_provider::{Discovery, GenerationParams, ToolAdvertising};

use super::tools::discovery_label;
use crate::config::aux_models::Purpose;

/// Section (a)/(b) of the Tools tab, as one step.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolsChange {
    /// The full-replacement overlay.
    pub entries: Option<Vec<ToolOverlayEntry>>,
    /// Servers newly switched on (lazily connected when `allowed`, #542).
    pub enable_servers: Vec<String>,
    /// Servers newly switched off (session enablement mark withdrawn).
    pub disable_servers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApplyStep {
    /// `persist_for` = the agent whose model pin the confirming
    /// `ModelChanged` writes (ADR-0081); `None` = session only.
    Model {
        provider: String,
        model: String,
        persist_for: Option<String>,
    },
    /// Permission mode (#560 P12, ADR-0207 §12) — session-only, no persist
    /// flag: unlike a model pin, a mode has nothing to save per agent (it
    /// isn't a profile fact any more, ADR-0207 §9).
    PermMode {
        mode: String,
    },
    /// Persisted on the confirming `GenerationChanged` (ADR-0095).
    Generation {
        overrides: GenerationParams,
        persist_for: Option<String>,
    },
    Tools(ToolsChange),
    Repin {
        mode: Option<ToolAdvertising>,
        discovery: Option<Discovery>,
    },
    /// Write the shown advertising facts into `config.yml` as the default for
    /// **new** sessions (`config::write_key`, comment-preserving). Orthogonal
    /// to [`Repin`][ApplyStep::Repin], which changes the running session only.
    /// `discovery` is `None` when the strategy doesn't apply to this session
    /// (`full` mode, a native wire) or its provider isn't known yet.
    PersistAdvertising {
        mode: ToolAdvertising,
        provider: String,
        discovery: Option<Discovery>,
    },
    Aux {
        purpose: Purpose,
        provider: String,
        model: String,
    },
}

fn saved(agent: &Option<String>, what: &str) -> String {
    agent
        .as_ref()
        .map(|a| format!(" (saving {what} for '{a}')"))
        .unwrap_or_default()
}

impl ApplyStep {
    pub fn label(&self) -> String {
        match self {
            ApplyStep::Model {
                provider,
                model,
                persist_for,
            } => format!("model → {provider}/{model}{}", saved(persist_for, "pin")),
            ApplyStep::PermMode { mode } => format!("mode → {mode}"),
            ApplyStep::Generation {
                overrides: g,
                persist_for,
            } => {
                let mut parts = Vec::new();
                if let Some(t) = g.temperature {
                    parts.push(format!("temperature={t}"));
                }
                if let Some(e) = g.reasoning_effort {
                    parts.push(format!("effort={}", e.as_str()));
                }
                if let Some(b) = g.thinking_budget_tokens {
                    parts.push(format!("thinking_budget={b}"));
                }
                if let Some(m) = g.max_output_tokens {
                    parts.push(format!("max_tokens={m}"));
                }
                format!(
                    "generation {}{}",
                    parts.join(" "),
                    saved(persist_for, "defaults")
                )
            }
            ApplyStep::Tools(t) => {
                let mut parts = Vec::new();
                if let Some(entries) = &t.entries {
                    parts.push(format!("tool overlay ({} entries)", entries.len()));
                }
                if !t.enable_servers.is_empty() {
                    parts.push(format!("mcp on: {}", t.enable_servers.join(", ")));
                }
                if !t.disable_servers.is_empty() {
                    parts.push(format!("mcp off: {}", t.disable_servers.join(", ")));
                }
                parts.join(", ")
            }
            ApplyStep::Repin { mode, discovery } => {
                let mut parts = Vec::new();
                if let Some(m) = mode {
                    parts.push(format!("tool advertising → {}", m.label()));
                }
                if let Some(d) = discovery {
                    parts.push(format!("discovery → {}", discovery_label(*d)));
                }
                parts.join(", ")
            }
            ApplyStep::PersistAdvertising {
                mode,
                provider,
                discovery,
            } => {
                let mut parts = vec![format!("tool_advertising → {}", mode.label())];
                if let Some(d) = discovery {
                    parts.push(format!("discovery[{provider}] → {}", discovery_label(*d)));
                }
                format!("saved for new sessions: {}", parts.join(", "))
            }
            ApplyStep::Aux {
                purpose,
                provider,
                model,
            } => format!("aux {purpose} → {provider}/{model}"),
        }
    }
}

/// The side effects the plan drives — the live implementation sends engine
/// messages and writes stores; tests record the calls.
pub trait SettingsEffects {
    async fn apply(&mut self, step: &ApplyStep) -> Result<(), String>;
}

#[derive(Debug, Default, PartialEq)]
pub struct ApplyReport {
    pub applied: Vec<String>,
    pub failed: Option<(String, String)>,
    pub skipped: Vec<String>,
}

/// Run `plan` in order, stopping at the first failure: later steps are
/// reported as not applied rather than attempted against a half-changed
/// session.
pub async fn run_plan<E: SettingsEffects>(plan: &[ApplyStep], fx: &mut E) -> ApplyReport {
    let mut report = ApplyReport::default();
    for step in plan {
        if report.failed.is_some() {
            report.skipped.push(step.label());
            continue;
        }
        match fx.apply(step).await {
            Ok(()) => report.applied.push(step.label()),
            Err(e) => report.failed = Some((step.label(), e)),
        }
    }
    report
}

impl ApplyReport {
    /// The one transcript status line for the whole dialog.
    pub fn summary(&self, notes: &[String]) -> String {
        let mut out = if self.applied.is_empty() {
            "nothing applied".to_string()
        } else {
            format!("applied: {}", self.applied.join("; "))
        };
        if let Some((step, error)) = &self.failed {
            out.push_str(&format!(" | FAILED: {step}: {error}"));
            if !self.skipped.is_empty() {
                out.push_str(&format!(" | not applied: {}", self.skipped.join("; ")));
            }
        }
        for note in notes {
            out.push_str(&format!(" | {note}"));
        }
        out
    }
}
