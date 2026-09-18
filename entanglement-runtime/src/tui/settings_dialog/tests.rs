use entanglement_provider::{
    Catalog, Discovery, GenerationParams, ReasoningEffort, ToolAdvertising,
};

use super::generation::{GenerationTab, ModelCaps};
use super::session::ModelOption;
use super::*;
use crate::tui::session_tools_dialog::SessionToolRow;

fn options() -> Vec<ModelOption> {
    model_options(&Catalog::builtin())
}

fn caps(provider: &str, model: &str) -> ModelCaps {
    options()
        .into_iter()
        .find(|o| o.provider == provider && o.model == model)
        .unwrap_or_else(|| panic!("{provider}/{model} in the builtin catalog"))
        .caps
}

fn tool(name: &str, profile_default: bool) -> SessionToolRow {
    SessionToolRow {
        name: name.to_string(),
        profile_default,
        checked: profile_default,
        allow: false,
    }
}

fn dialog_on(provider: &str, model: &str, adv: AdvertisingRows) -> SettingsDialog {
    let session = SessionTab::new(
        "general".to_string(),
        options(),
        (provider, model),
        mode_names(),
        entanglement_core::DEFAULT_MODE,
    );
    let tools = ToolsTab::new(
        vec![
            tool("read", true),
            tool("mcp__docs__search", false),
            tool("mcp__docs__*", false),
        ],
        adv,
    );
    let models = options()
        .into_iter()
        .map(|o| (o.provider, o.model))
        .collect();
    SettingsDialog::new(
        session,
        GenerationParams::default(),
        tools,
        AuxTab::new(models, &[]),
    )
}

fn live_adv() -> AdvertisingRows {
    AdvertisingRows::new(
        true,
        true,
        ToolAdvertising::ToolSearch,
        Discovery::Append,
        "zai".to_string(),
    )
}

fn dialog() -> SettingsDialog {
    dialog_on("zai", "glm-5.3", live_adv())
}

fn focus(d: &mut SettingsDialog, id: RowId) {
    let index = d
        .rows()
        .iter()
        .position(|r| r.id == id)
        .expect("row present");
    d.focus_row(index);
}

#[test]
fn tabs_cycle_forward_and_back_with_wrap() {
    let mut d = dialog();
    let mut seen = vec![d.tab()];
    for _ in 0..4 {
        d.next_tab();
        seen.push(d.tab());
    }
    assert_eq!(
        seen,
        [
            Tab::Session,
            Tab::Generation,
            Tab::Tools,
            Tab::Aux,
            Tab::Session
        ]
    );
    d.prev_tab();
    assert_eq!(d.tab(), Tab::Aux);
}

#[test]
fn pending_tracks_changes_and_reverting_clears_them() {
    let mut d = dialog();
    assert!(d.pending().is_empty());
    focus(&mut d, RowId::Model);
    d.activate(true);
    assert_eq!(d.pending().len(), 1);
    assert!(d.pending()[0].starts_with("model → "), "{:?}", d.pending());
    d.activate(false);
    assert!(d.pending().is_empty(), "back on the starting model");
    assert!(matches!(d.confirm(), Confirm::Close));
}

#[test]
fn persist_flags_are_per_tab_and_aux_is_always_on() {
    let mut d = dialog();
    focus(&mut d, RowId::Persist);
    d.activate(true);
    assert!(d.persist(Tab::Session));
    assert!(!d.persist(Tab::Generation));
    assert!(!d.persist(Tab::Tools));
    d.set_tab(Tab::Aux);
    focus(&mut d, RowId::Persist);
    d.activate(true);
    assert!(d.persist(Tab::Aux), "aux persist can't be switched off");
    let aux_persist = d
        .rows()
        .into_iter()
        .find(|r| r.id == RowId::Persist)
        .unwrap();
    assert_eq!(aux_persist.disabled, Some(AUX_PERSIST_REASON));

    // Only the Session tab's flag reaches the model step.
    d.set_tab(Tab::Session);
    focus(&mut d, RowId::Model);
    d.activate(true);
    d.set_tab(Tab::Generation);
    focus(&mut d, RowId::Gen(GenField::Effort));
    d.activate(true);
    let Confirm::Apply(plan) = d.confirm() else {
        panic!("a plan");
    };
    assert!(matches!(&plan[0], ApplyStep::Model { persist_for: Some(a), .. } if a == "general"));
    assert!(matches!(
        &plan[1],
        ApplyStep::Generation {
            persist_for: None,
            ..
        }
    ));
}

#[test]
fn adaptive_model_without_temperature_hides_temperature_and_budget() {
    for model in ["claude-fable-5", "claude-sonnet-5"] {
        let c = caps("anthropic", model);
        assert_eq!(
            c.visible_fields(),
            vec![GenField::Effort, GenField::MaxTokens],
            "{model}"
        );
        assert_eq!(c.effort_tiers.len(), 5);
    }
}

#[test]
fn glm_5_3_offers_only_its_tiers_and_requires_thinking() {
    let c = caps("zai", "glm-5.3");
    assert_eq!(
        c.visible_fields(),
        vec![GenField::Temperature, GenField::Effort, GenField::MaxTokens]
    );
    assert_eq!(
        c.effort_tiers,
        vec![
            ReasoningEffort::Low,
            ReasoningEffort::High,
            ReasoningEffort::Max
        ]
    );
    assert!(c.thinking_required);
}

#[test]
fn glm_4_7_hides_effort() {
    let c = caps("zai", "glm-4.7");
    assert_eq!(
        c.visible_fields(),
        vec![GenField::Temperature, GenField::MaxTokens]
    );
}

#[test]
fn budget_style_sonnet_4_5_shows_the_thinking_budget_and_all_tiers() {
    let c = caps("anthropic", "claude-sonnet-4-5");
    assert_eq!(
        c.visible_fields(),
        vec![
            GenField::Temperature,
            GenField::Effort,
            GenField::ThinkingBudget,
            GenField::MaxTokens
        ]
    );
    assert_eq!(c.effort_tiers, ReasoningEffort::ALL.to_vec());
}

#[test]
fn changing_the_model_refreshes_generation_fields_live() {
    let mut d = dialog();
    d.set_tab(Tab::Generation);
    focus(&mut d, RowId::Gen(GenField::Effort));
    d.activate(true);
    assert_eq!(d.pending(), vec!["reasoning effort → low"]);

    d.set_tab(Tab::Session);
    focus(&mut d, RowId::Model);
    for _ in 0..20 {
        if d.rows()
            .iter()
            .any(|r| r.id == RowId::Model && r.value == "zai/glm-4.7")
        {
            break;
        }
        d.activate(true);
    }
    d.set_tab(Tab::Generation);
    let ids: Vec<RowId> = d.rows().iter().map(|r| r.id).collect();
    assert!(
        !ids.contains(&RowId::Gen(GenField::Effort)),
        "glm-4.7 takes no effort"
    );
    assert_eq!(
        d.pending(),
        vec!["model → zai/glm-4.7"],
        "the effort edit is dropped"
    );
}

#[test]
fn model_default_resolves_to_the_catalog_value_or_is_reported() {
    let current = GenerationParams {
        reasoning_effort: Some(ReasoningEffort::Low),
        thinking_budget_tokens: Some(4096),
        ..GenerationParams::default()
    };
    let mut glm = GenerationTab::new(current, caps("zai", "glm-5.3"));
    glm.cycle(GenField::Effort, false); // low → model default
    let (overrides, unresettable) = glm.overrides();
    assert_eq!(overrides.reasoning_effort, Some(ReasoningEffort::High));
    assert!(unresettable.is_empty());

    let mut sonnet = GenerationTab::new(current, caps("anthropic", "claude-sonnet-4-5"));
    sonnet.cycle(GenField::ThinkingBudget, false); // 4096 → 1024
    sonnet.cycle(GenField::ThinkingBudget, false); // → model default
    let (overrides, unresettable) = sonnet.overrides();
    assert_eq!(overrides.thinking_budget_tokens, None);
    assert_eq!(unresettable, vec![GenField::ThinkingBudget]);
}

fn change_mode(d: &mut SettingsDialog) {
    d.set_tab(Tab::Tools);
    focus(d, RowId::Mode);
    d.activate(true);
}

#[test]
fn repin_needs_its_own_confirmation_step() {
    let mut d = dialog();
    change_mode(&mut d);
    assert!(matches!(d.confirm(), Confirm::NeedsRepinConfirmation));
    assert_eq!(d.stage(), Stage::ConfirmRepin);
    d.back();
    assert!(
        matches!(d.confirm(), Confirm::NeedsRepinConfirmation),
        "back re-arms it"
    );
    let Confirm::Apply(plan) = d.confirm() else {
        panic!("confirmed plan");
    };
    assert_eq!(
        plan,
        vec![ApplyStep::Repin {
            mode: Some(ToolAdvertising::Full),
            discovery: None
        }]
    );
}

#[test]
fn discovery_is_disabled_with_a_reason_off_client_side_or_under_full() {
    let native = AdvertisingRows::new(
        true,
        false,
        ToolAdvertising::ToolSearch,
        Discovery::Append,
        "anthropic".to_string(),
    );
    assert!(native.discovery_disabled().is_some());
    let mut d = dialog_on("anthropic", "claude-sonnet-5", native);
    d.set_tab(Tab::Tools);
    focus(&mut d, RowId::Discovery);
    d.activate(true);
    assert!(d.pending().is_empty(), "a disabled row doesn't change");

    let mut full = AdvertisingRows::new(
        true,
        true,
        ToolAdvertising::Full,
        Discovery::Append,
        "zai".to_string(),
    );
    assert_eq!(
        full.discovery_disabled(),
        Some("only applies under tool_search")
    );
    full.cycle_mode();
    assert_eq!(full.discovery_disabled(), None);
}

fn persist_row(d: &SettingsDialog) -> RowView {
    d.tools_rows()
        .into_iter()
        .find(|r| r.id == RowId::AdvertisingPersist)
        .expect("the advertising persist row")
}

#[test]
fn the_persist_checkbox_is_live_and_only_a_stateless_head_disables_it() {
    // The managed writer exists now, so a normal session can save.
    assert_eq!(persist_row(&dialog()).disabled, None);

    let headless = AdvertisingRows::new(
        false,
        false,
        ToolAdvertising::ToolSearch,
        Discovery::Append,
        String::new(),
    );
    assert_eq!(
        headless.persist_disabled(),
        Some(ADVERTISING_PERSIST_REASON)
    );
    let d = dialog_on("zai", "glm-5.3", headless);
    assert_eq!(persist_row(&d).disabled, Some(ADVERTISING_PERSIST_REASON));
}

fn toggle_persist(d: &mut SettingsDialog) {
    d.set_tab(Tab::Tools);
    focus(d, RowId::AdvertisingPersist);
    d.activate(true);
}

#[test]
fn persisting_saves_the_shown_mode_and_strategy_without_a_repin() {
    let mut d = dialog();
    toggle_persist(&mut d);
    // Saving what is already pinned changes nothing live, so no confirmation
    // step — the write is the whole plan.
    let Confirm::Apply(plan) = d.confirm() else {
        panic!("a plan");
    };
    assert_eq!(
        plan,
        vec![ApplyStep::PersistAdvertising {
            mode: ToolAdvertising::ToolSearch,
            provider: "zai".to_string(),
            discovery: Some(Discovery::Append),
        }]
    );
    assert!(
        plan[0].label().contains("discovery[zai] → append"),
        "{}",
        plan[0].label()
    );
}

#[test]
fn persist_off_writes_nothing_and_full_mode_saves_no_strategy() {
    // Off: a re-pin alone never touches config.yml.
    let mut d = dialog();
    change_mode(&mut d);
    let _ = d.confirm();
    let Confirm::Apply(plan) = d.confirm() else {
        panic!("confirmed plan");
    };
    assert!(
        !plan
            .iter()
            .any(|s| matches!(s, ApplyStep::PersistAdvertising { .. })),
        "{plan:?}"
    );

    // On, with the mode flipped to `full`: the strategy no longer applies, so
    // only the mode is written — the re-pin still goes first.
    let mut d = dialog();
    toggle_persist(&mut d);
    focus(&mut d, RowId::Mode);
    d.activate(true);
    let _ = d.confirm();
    let Confirm::Apply(plan) = d.confirm() else {
        panic!("confirmed plan");
    };
    assert_eq!(
        plan,
        vec![
            ApplyStep::Repin {
                mode: Some(ToolAdvertising::Full),
                discovery: None
            },
            ApplyStep::PersistAdvertising {
                mode: ToolAdvertising::Full,
                provider: "zai".to_string(),
                discovery: None,
            }
        ]
    );
}

#[test]
fn an_unknown_provider_saves_the_mode_but_not_the_strategy() {
    // `active_provider` is empty until a `ModelChanged` names one; the mode is
    // provider-independent, the `discovery:` map key is not.
    let rows = AdvertisingRows::new(
        true,
        true,
        ToolAdvertising::ToolSearch,
        Discovery::Invoke,
        String::new(),
    );
    let mut d = dialog_on("zai", "glm-5.3", rows);
    toggle_persist(&mut d);
    let Confirm::Apply(plan) = d.confirm() else {
        panic!("a plan");
    };
    assert_eq!(
        plan,
        vec![ApplyStep::PersistAdvertising {
            mode: ToolAdvertising::ToolSearch,
            provider: String::new(),
            discovery: None,
        }]
    );
}

#[test]
fn tools_changes_become_one_overlay_step_with_server_enables() {
    let mut d = dialog();
    d.set_tab(Tab::Tools);
    focus(&mut d, RowId::Tool(2)); // mcp__docs__* server row
    d.activate(true);
    let Confirm::Apply(plan) = d.confirm() else {
        panic!("a plan");
    };
    let [ApplyStep::Tools(change)] = plan.as_slice() else {
        panic!("one tools step: {plan:?}");
    };
    assert_eq!(change.enable_servers, vec!["docs"]);
    assert_eq!(change.entries.as_ref().map(Vec::len), Some(1));
}

#[derive(Default)]
struct Recorder {
    calls: Vec<ApplyStep>,
    fail_tools: bool,
}

impl SettingsEffects for Recorder {
    async fn apply(&mut self, step: &ApplyStep) -> Result<(), String> {
        self.calls.push(step.clone());
        match step {
            ApplyStep::Tools(_) if self.fail_tools => Err("connect refused".into()),
            _ => Ok(()),
        }
    }
}

fn everything_changed() -> SettingsDialog {
    let mut d = dialog();
    focus(&mut d, RowId::Model);
    d.activate(true);
    d.set_tab(Tab::Generation);
    focus(&mut d, RowId::Gen(GenField::MaxTokens));
    d.activate(true);
    d.set_tab(Tab::Aux);
    focus(
        &mut d,
        RowId::Aux(crate::config::aux_models::Purpose::Narrate),
    );
    d.activate(true);
    d.set_tab(Tab::Tools);
    focus(&mut d, RowId::Tool(0));
    d.activate(true);
    change_mode(&mut d);
    d
}

fn kind(step: &ApplyStep) -> &'static str {
    match step {
        ApplyStep::Model { .. } => "model",
        ApplyStep::PermMode { .. } => "perm-mode",
        ApplyStep::Generation { .. } => "generation",
        ApplyStep::Tools(_) => "tools",
        ApplyStep::Repin { .. } => "repin",
        ApplyStep::PersistAdvertising { .. } => "persist-advertising",
        ApplyStep::Aux { .. } => "aux",
    }
}

#[tokio::test]
async fn apply_runs_in_the_settled_order_and_repin_waits_for_confirmation() {
    let mut d = everything_changed();
    let mut fx = Recorder::default();
    // First Enter only arms the confirmation — nothing may run yet.
    if let Confirm::Apply(plan) = d.confirm() {
        run_plan(&plan, &mut fx).await;
    }
    assert!(
        fx.calls.is_empty(),
        "no side effect before the re-pin is confirmed"
    );

    let Confirm::Apply(plan) = d.confirm() else {
        panic!("confirmed plan");
    };
    let report = run_plan(&plan, &mut fx).await;
    let order: Vec<_> = fx.calls.iter().map(kind).collect();
    assert_eq!(order, ["model", "generation", "tools", "repin", "aux"]);
    assert_eq!(report.applied.len(), 5);
    assert!(report.failed.is_none());
}

#[tokio::test]
async fn a_failing_step_stops_the_plan_and_the_summary_says_what_applied() {
    let mut d = everything_changed();
    let _ = d.confirm();
    let Confirm::Apply(plan) = d.confirm() else {
        panic!("confirmed plan");
    };
    let mut fx = Recorder {
        fail_tools: true,
        ..Recorder::default()
    };
    let report = run_plan(&plan, &mut fx).await;
    let order: Vec<_> = fx.calls.iter().map(kind).collect();
    assert_eq!(
        order,
        ["model", "generation", "tools"],
        "repin/aux never attempted"
    );
    assert_eq!(report.skipped.len(), 2);
    let summary = report.summary(&[]);
    assert!(summary.starts_with("applied: model → "), "{summary}");
    assert!(summary.contains("FAILED: tool overlay"), "{summary}");
    assert!(summary.contains("connect refused"), "{summary}");
    assert!(
        summary.contains("not applied: tool advertising → full; aux narrate"),
        "{summary}"
    );
}
