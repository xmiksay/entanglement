//! `skutter inspect modes [name]` (#560, ADR-0207 stage 6c).
//!
//! Replaces the mask columns `inspect agents` lost in stage 4c (ADR-0207
//! §12): "what actually grades a call" moved off the agent and onto the
//! session's independent permission mode, so this is where that answer
//! lives now. With no `name`, lists the four built-in modes; with one, shows
//! its **resolved** rules — the built-in shape plus any `config.yml`
//! `modes:` tuning, i.e. what actually grades a live session — plus its run
//! limits, sandbox posture, and the outcome for a known tool roster.
//!
//! Runs with **no engine**, like every other `inspect` subcommand, so the
//! roster is the fixed, always-registered host quintet-plus-two, `apply_patch`/
//! `load_skill`, and the runtime-owned pseudo-tools
//! ([`crate::capability::static_capability_of`]) rather than a live
//! [`crate::tools::ToolRegistry`] — an MCP/endpoint tool is dynamic and
//! environment-dependent, out of scope for a static report.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::capability::{static_capability_of, Capability};
use crate::config::Config;
use crate::mode::{self, Mode, ModeTable};

/// The known roster this view grades against, in a stable display order.
const ROSTER: &[&str] = &[
    "read",
    "glob",
    "grep",
    "edit",
    "write",
    "apply_patch",
    "bash",
    "call",
    "load_skill",
    "propose_plan",
    "request_mode",
    "agent",
    "agent_send",
    "poll",
    "ask_user",
    "explore",
    "describe",
    "update_tasks",
];

/// Build the resolved mode table exactly as `main.rs` does at real startup
/// (built-ins + `config.yml` `modes:` tuning, validated against a real
/// capability resolver) — `inspect` has no live `ToolRegistry`, so it uses
/// the same no-registry static resolver `BindingPolicy` grades through.
fn resolved_table(root: &Path) -> Result<ModeTable> {
    let config = Config::load(root).context("loading user config")?;
    mode::build_table(&config.modes, &static_capability_of)
        .context("building the permission mode table from config.yml `modes:`")
}

pub fn inspect_modes(root: &Path, name: Option<&str>) -> Result<()> {
    let table = resolved_table(root)?;
    match name {
        Some(name) => {
            let m = table.get(name).with_context(|| {
                format!("unknown mode `{name}` (one of: research | plan | build | auto)")
            })?;
            print!("{}", render_mode_detail(m));
        }
        None => print!("{}", render_mode_table(&table)),
    }
    Ok(())
}

/// One-line-per-mode table: name, default grade, sandbox posture.
pub(super) fn render_mode_table(table: &ModeTable) -> String {
    let mut names: Vec<&str> = table.names().collect();
    // Fixed, meaningful order rather than alphabetical — matches how a user
    // thinks about escalation (research → plan → build → auto).
    const ORDER: [&str; 4] = ["research", "plan", "build", "auto"];
    names.sort_by_key(|n| ORDER.iter().position(|o| o == n).unwrap_or(usize::MAX));

    let rows: Vec<Vec<String>> = names
        .iter()
        .filter_map(|n| table.get(n))
        .map(|m| {
            vec![
                m.name.clone(),
                format!("{:?}", m.default),
                m.sandbox.clone().unwrap_or_else(|| "(none)".to_string()),
            ]
        })
        .collect();
    super::render_table(&["MODE", "DEFAULT", "SANDBOX"], &rows)
}

/// Full detail for one mode: resolved rules (built-in + tuning), limits,
/// sandbox, and the graded outcome for [`ROSTER`].
pub(super) fn render_mode_detail(m: &Mode) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "name:    {}", m.name);
    let _ = writeln!(out, "default: {:?}", m.default);

    let _ = writeln!(out, "\nrules (longest matching key wins):");
    let mut any = false;
    for (key, grade) in m.rules.iter() {
        any = true;
        let _ = writeln!(out, "  {key}: {grade:?}");
    }
    if !any {
        let _ = writeln!(out, "  (none)");
    }

    let _ = writeln!(out, "\nlimits:");
    let _ = writeln!(
        out,
        "  max_depth:        {}",
        m.limits
            .max_depth
            .map_or("unlimited".to_string(), |v| v.to_string())
    );
    let _ = writeln!(
        out,
        "  max_agents:       {}",
        m.limits
            .max_agents
            .map_or("unlimited".to_string(), |v| v.to_string())
    );
    let _ = writeln!(
        out,
        "  question_timeout: {}",
        if m.limits.question_timeout == 0 {
            "infinite".to_string()
        } else {
            format!("{}s", m.limits.question_timeout)
        }
    );
    let _ = writeln!(
        out,
        "  max_turns:        {}",
        m.limits
            .max_turns
            .map_or("unlimited".to_string(), |v| v.to_string())
    );
    let _ = writeln!(
        out,
        "  max_duration:     {}",
        m.limits
            .max_duration
            .map_or("unlimited".to_string(), |v| format!("{v}s"))
    );

    let _ = writeln!(
        out,
        "\nsandbox: {}{}",
        m.sandbox.as_deref().unwrap_or("(none)"),
        if m.sandbox.is_some() && m.sandbox_network {
            " (network allowed)"
        } else {
            ""
        }
    );

    let _ = writeln!(out, "\nper-tool outcome (known roster, no argument):");
    let rows: Vec<Vec<String>> = ROSTER
        .iter()
        .map(|tool| {
            let capabilities = static_capability_of(tool).unwrap_or(&[]);
            let outcome = if capabilities.contains(&Capability::Control) {
                "control (never graded)".to_string()
            } else {
                format!("{:?}", m.resolve(tool, capabilities, None, None))
            };
            vec![(*tool).to_string(), outcome]
        })
        .collect();
    out.push_str(&super::render_table(&["TOOL", "OUTCOME"], &rows));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_lists_the_four_built_in_modes_in_escalation_order() {
        let table = ModeTable::builtin().expect("built-ins parse");
        let rendered = render_mode_table(&table);
        let lines: Vec<&str> = rendered.lines().collect();
        // Header + 4 rows, research before plan before build before auto.
        assert_eq!(lines.len(), 5, "{rendered}");
        assert!(lines[1].starts_with("research"), "{rendered}");
        assert!(lines[2].starts_with("plan"), "{rendered}");
        assert!(lines[3].starts_with("build"), "{rendered}");
        assert!(lines[4].starts_with("auto"), "{rendered}");
    }

    #[test]
    fn detail_shows_rules_limits_sandbox_and_roster_outcomes() {
        let table = ModeTable::builtin().expect("built-ins parse");
        let research = table.get("research").expect("research exists");
        let detail = render_mode_detail(research);
        assert!(detail.contains("name:    research"), "{detail}");
        assert!(
            detail.contains("rules (longest matching key wins):"),
            "{detail}"
        );
        assert!(detail.contains("write: Deny"), "{detail}");
        assert!(detail.contains("limits:"), "{detail}");
        assert!(detail.contains("sandbox:"), "{detail}");
        assert!(detail.contains("per-tool outcome"), "{detail}");
        // `edit` is Write-capable and research class-denies write — this is
        // the same worked example the tuning guard tests, now via the
        // per-tool outcome column.
        assert!(detail.contains("edit"), "{detail}");
        assert!(detail.contains("Deny"), "{detail}");
        // A Control tool never grades.
        assert!(detail.contains("control (never graded)"), "{detail}");
    }

    #[test]
    fn detail_shows_a_tuned_mode_resolved_rule() {
        use crate::mode::ModeTuning;
        let mut tuning = std::collections::HashMap::new();
        tuning.insert(
            "research".to_string(),
            ModeTuning {
                allow: vec!["bash(cargo check)".to_string()],
                ..Default::default()
            },
        );
        let table = mode::build_table(&tuning, &static_capability_of).expect("valid tuning");
        let research = table.get("research").expect("research exists");
        let detail = render_mode_detail(research);
        assert!(
            detail.contains("bash(cargo check): Allow"),
            "the tuned rule must show up in the resolved detail: {detail}"
        );
    }

    #[test]
    fn unknown_mode_name_is_a_loud_error() {
        // Point the user-config path at a file that doesn't exist so this
        // test's `Config::load` sees only the embedded defaults, never a
        // real `~/.config/entanglement/config.yml` on the machine running
        // it — the same isolation `config::tests` uses, guarded by the same
        // process-global env lock so a concurrent config test can't race it.
        let _g = crate::config::ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ENTANGLEMENT_CONFIG_FILE", dir.path().join("nope.yml"));
        let err = inspect_modes(dir.path(), Some("researchx")).unwrap_err();
        std::env::remove_var("ENTANGLEMENT_CONFIG_FILE");
        assert!(err.to_string().contains("researchx"), "got: {err}");
    }
}
