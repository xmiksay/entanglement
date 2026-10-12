//! Opt-in live tool-use matrix against z.ai (`make test-live-matrix`, #826).
//!
//! WHY: #818's choice of a single `mcp(server, tool, args)` envelope plus
//! `rhai` as the pluggable surface, and of `list_mcp`/`mcp_tools` over
//! `explore`/`describe`, rests on how the models actually behave, not on an
//! estimate. Every GLM model in the catalog × thinking on/off × six call
//! encodings × four tasks, plus the `mcp` encoding × two discovery shapes
//! from the bare task; `ZAI_PROBE_RUNS` runs each. Results land as JSONL
//! (appended live) and a markdown summary under `scratch/`.
//!
//! Env: `ZAI_API_KEY` (required, else skipped with a note), `ZAI_API_BASE`,
//! `ZAI_PROBE_MODELS` (comma list; default: every catalog `zai` model),
//! `ZAI_PROBE_RUNS` (default 3), `ZAI_PROBE_THINKING` (`both` default |
//! `enabled` | `disabled`; a `thinking_required` model only ever runs
//! enabled), `ZAI_PROBE_VARIANTS` / `ZAI_PROBE_TASKS` (comma lists of names),
//! `ZAI_PROBE_DISCOVERY=0` (skip the unseeded discovery-shape cells),
//! `ZAI_PROBE_OUT` (path stem, default `<repo>/scratch/tool-matrix`),
//! `ZAI_PROBE_REPORT_ONLY=1` (rebuild the markdown from the JSONL, no calls).

mod live_matrix;

use std::path::PathBuf;

use live_matrix::probe::Probe;
use live_matrix::report::{append_jsonl, load_jsonl, markdown, Record};
use live_matrix::runner::{run_cell, Cell};
use live_matrix::tasks::Task;
use live_matrix::variants::{Discovery, Variant};

/// Every `zai` model in `defaults.yml` at the time of writing, newest first.
const DEFAULT_MODELS: &[&str] = &[
    "glm-5.3",
    "glm-5.3-flash",
    "glm-5.2",
    "glm-5.1",
    "glm-5",
    "glm-4.7",
    "glm-4.7-flashx",
    "glm-4.7-flash",
    "glm-4.6",
    "glm-4.5",
    "glm-4.5-x",
    "glm-4.5-air",
    "glm-4.5-airx",
    "glm-4.5-flash",
    "glm-4-32b-0414-128k",
];

/// Catalog `thinking_required: true` — these 400 on `thinking: disabled`.
const THINKING_REQUIRED: &[&str] = &["glm-5.3", "glm-5.3-flash"];

fn env_list(name: &str) -> Option<Vec<String>> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
}

fn env_flag_off(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "0")
}

fn out_stem() -> PathBuf {
    std::env::var("ZAI_PROBE_OUT")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("scratch")
                .join("tool-matrix")
        })
}

fn thinking_modes(model: &str) -> Vec<bool> {
    if THINKING_REQUIRED.contains(&model) {
        return vec![true];
    }
    match std::env::var("ZAI_PROBE_THINKING").as_deref() {
        Ok("enabled") => vec![true],
        Ok("disabled") => vec![false],
        _ => vec![false, true],
    }
}

/// The cells one model × thinking setting runs: every encoding seeded with
/// the `list` shape, then the `mcp` encoding unseeded under both shapes.
fn cells(variants: &[Variant], tasks: &[Task], with_discovery: bool) -> Vec<Cell> {
    let mut cells = Vec::new();
    for &variant in variants {
        for &task in tasks {
            cells.push(Cell {
                variant,
                task,
                discovery: Discovery::ListTools,
                seeded: true,
            });
        }
    }
    if with_discovery && variants.contains(&Variant::Mcp) {
        for discovery in [Discovery::Explore, Discovery::ListTools] {
            for &task in tasks {
                cells.push(Cell {
                    variant: Variant::Mcp,
                    task,
                    discovery,
                    seeded: false,
                });
            }
        }
    }
    cells
}

#[tokio::test]
#[ignore = "live z.ai calls; run via `make test-live-matrix`"]
async fn tool_use_matrix() {
    let stem = out_stem();
    let jsonl = stem.with_extension("jsonl");
    let md = stem.with_extension("md");
    if let Some(dir) = stem.parent() {
        std::fs::create_dir_all(dir).expect("output dir");
    }

    let variants: Vec<Variant> = env_list("ZAI_PROBE_VARIANTS")
        .map(|l| l.iter().filter_map(|s| Variant::from_name(s)).collect())
        .unwrap_or_else(|| Variant::ALL.to_vec());
    let tasks: Vec<Task> = env_list("ZAI_PROBE_TASKS")
        .map(|l| l.iter().filter_map(|s| Task::from_name(s)).collect())
        .unwrap_or_else(|| Task::ALL.to_vec());
    let variant_names: Vec<&str> = variants.iter().map(|v| v.name()).collect();
    let task_names: Vec<&str> = tasks.iter().map(|t| t.name()).collect();

    if std::env::var("ZAI_PROBE_REPORT_ONLY").is_ok_and(|v| v == "1") {
        let records = load_jsonl(&jsonl);
        // A rebuild renders what the JSONL actually holds unless the env
        // narrows it — otherwise every task absent from the file would come
        // out as an all-"—" table.
        let variant_names: Vec<&str> = variant_names
            .into_iter()
            .filter(|v| {
                env_list("ZAI_PROBE_VARIANTS").is_some() || records.iter().any(|r| r.variant == *v)
            })
            .collect();
        let task_names: Vec<&str> = task_names
            .into_iter()
            .filter(|t| {
                env_list("ZAI_PROBE_TASKS").is_some() || records.iter().any(|r| r.task == *t)
            })
            .collect();
        std::fs::write(&md, markdown(&records, &variant_names, &task_names))
            .expect("write markdown");
        eprintln!(
            "report rebuilt from {} records → {}",
            records.len(),
            md.display()
        );
        return;
    }

    let Some(probe) = Probe::from_env() else {
        eprintln!("skipping live matrix: ZAI_API_KEY is unset");
        return;
    };
    let models = env_list("ZAI_PROBE_MODELS")
        .unwrap_or_else(|| DEFAULT_MODELS.iter().map(|s| s.to_string()).collect());
    let runs: u32 = std::env::var("ZAI_PROBE_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let plan = cells(&variants, &tasks, !env_flag_off("ZAI_PROBE_DISCOVERY"));

    let settings: usize = models.iter().map(|m| thinking_modes(m).len()).sum();
    let total = settings * plan.len() * runs as usize;
    eprintln!(
        "matrix: {} models ({settings} model×thinking settings), {} cells each, {runs} runs → {total} runs",
        models.len(),
        plan.len()
    );

    let mut done = 0usize;
    for model in &models {
        for thinking in thinking_modes(model) {
            for &cell in &plan {
                for run in 1..=runs {
                    let o = run_cell(&probe, model, thinking, cell).await;
                    done += 1;
                    eprintln!(
                        "[{done}/{total}] {model} think={} {:<11} {:<7} {:<8} run {run}: reached={} args_ok={} d={} r={} t={}{}",
                        if thinking { "on " } else { "off" },
                        cell.variant.name(),
                        if cell.seeded { "seeded" } else { cell.discovery.name() },
                        cell.task.name(),
                        o.reached,
                        o.args_ok,
                        o.discover_calls,
                        o.rounds,
                        o.completion_tokens,
                        o.error.as_ref().map(|e| format!(" ERROR {e}")).unwrap_or_default()
                    );
                    let rec = Record {
                        model: model.clone(),
                        thinking,
                        variant: cell.variant.name().into(),
                        discovery: cell.discovery.name().into(),
                        seeded: cell.seeded,
                        task: cell.task.name().into(),
                        run,
                        reached: o.reached,
                        args_ok: o.args_ok,
                        discover_calls: o.discover_calls,
                        rounds: o.rounds,
                        completion_tokens: o.completion_tokens,
                        cached_first: o.cached_first,
                        error: o.error,
                    };
                    append_jsonl(&jsonl, &rec).expect("append jsonl");
                }
            }
        }
    }

    let records = load_jsonl(&jsonl);
    std::fs::write(&md, markdown(&records, &variant_names, &task_names)).expect("write markdown");
    eprintln!("wrote {} ({} records)", md.display(), records.len());
}
