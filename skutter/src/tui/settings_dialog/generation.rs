//! The `/set` dialog's Generation tab: which knobs the *selected* model
//! accepts (catalog capability flags), and the per-field choice state. Every
//! field offers "model default" as an explicit choice; the tab re-derives its
//! visible fields whenever the Session tab's model changes.

use entanglement_provider::{GenerationParams, ModelEntry, ReasoningEffort, ThinkingStyle, Wire};

/// One generation knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenField {
    Temperature,
    Effort,
    ThinkingBudget,
    MaxTokens,
}

impl GenField {
    pub fn label(self) -> &'static str {
        match self {
            GenField::Temperature => "temperature",
            GenField::Effort => "reasoning effort",
            GenField::ThinkingBudget => "thinking budget",
            GenField::MaxTokens => "max output tokens",
        }
    }
}

/// A field's value: the model's own default, or an explicit value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Choice<T> {
    ModelDefault,
    Value(T),
}

/// What the selected model accepts, read off its catalog entry.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCaps {
    pub temperature: bool,
    /// The tiers offered; empty hides the effort field.
    pub effort_tiers: Vec<ReasoningEffort>,
    pub thinking_budget: bool,
    pub thinking_required: bool,
    pub max_output_tokens: Option<u32>,
    /// The catalog defaults a "model default" choice resolves to.
    pub defaults: GenerationParams,
}

impl ModelCaps {
    pub fn from_entry(wire: Wire, entry: &ModelEntry) -> Self {
        let effort_tiers = match entry.effort_tiers {
            None => ReasoningEffort::ALL.to_vec(),
            Some(tiers) => tiers.iter().collect(),
        };
        // Only the fixed-budget request shapes read an explicit budget: the
        // OpenAI-compat/Responses wires drop the field, and adaptive-style
        // Anthropic models reject `budget_tokens` outright.
        let budget_wire = matches!(wire, Wire::Anthropic | Wire::Gemini);
        Self {
            temperature: entry.supports_temperature,
            effort_tiers,
            thinking_budget: entry.supports_thinking
                && budget_wire
                && entry.resolved_thinking_style() == ThinkingStyle::Budget,
            thinking_required: entry.thinking_required,
            max_output_tokens: entry.max_output_tokens,
            defaults: entry.generation_params(),
        }
    }

    /// A model the catalog doesn't know: offer the portable knobs only.
    pub fn unknown() -> Self {
        Self {
            temperature: true,
            effort_tiers: ReasoningEffort::ALL.to_vec(),
            thinking_budget: false,
            thinking_required: false,
            max_output_tokens: None,
            defaults: GenerationParams::default(),
        }
    }

    pub fn visible_fields(&self) -> Vec<GenField> {
        let mut fields = Vec::new();
        if self.temperature {
            fields.push(GenField::Temperature);
        }
        if !self.effort_tiers.is_empty() {
            fields.push(GenField::Effort);
        }
        if self.thinking_budget {
            fields.push(GenField::ThinkingBudget);
        }
        fields.push(GenField::MaxTokens);
        fields
    }
}

const TEMPERATURES: [f32; 5] = [0.0, 0.3, 0.7, 1.0, 1.5];
const BUDGETS: [u32; 4] = [1024, 4096, 16_000, 32_000];
const MAX_TOKENS: [u32; 6] = [4096, 8192, 16_000, 32_000, 64_000, 128_000];

/// The four choices, one per [`GenField`].
#[derive(Debug, Clone, Copy, PartialEq)]
struct Choices {
    temperature: Choice<f32>,
    effort: Choice<ReasoningEffort>,
    budget: Choice<u32>,
    max_tokens: Choice<u32>,
}

/// The Generation tab's state: the choices the session started from, the
/// current choices, and the caps of the model they're validated against.
#[derive(Debug, Clone, PartialEq)]
pub struct GenerationTab {
    initial: Choices,
    now: Choices,
    caps: ModelCaps,
}

fn seed<T: PartialEq + Copy>(current: Option<T>, default: Option<T>) -> Choice<T> {
    match current {
        Some(v) if Some(v) != default => Choice::Value(v),
        _ => Choice::ModelDefault,
    }
}

/// Step `current` through `[ModelDefault, options…]`, wrapping; a current
/// value outside `options` (hand-set via `/set`) steps to the nearest end.
fn cycle<T: PartialEq + Copy>(current: Choice<T>, options: &[T], forward: bool) -> Choice<T> {
    let mut all = vec![Choice::ModelDefault];
    all.extend(options.iter().map(|v| Choice::Value(*v)));
    let len = all.len();
    let next = match all.iter().position(|c| *c == current) {
        Some(i) if forward => (i + 1) % len,
        Some(i) => (i + len - 1) % len,
        None if forward => 0,
        None => len - 1,
    };
    all[next]
}

fn label<T: ToString>(choice: Choice<T>, default: Option<T>) -> String {
    match (choice, default) {
        (Choice::Value(v), _) => v.to_string(),
        (Choice::ModelDefault, Some(d)) => format!("model default ({})", d.to_string()),
        (Choice::ModelDefault, None) => "model default".to_string(),
    }
}

/// `Value(v)` → `Some(v)`; "model default" → the catalog's concrete default.
fn resolve<T>(choice: Choice<T>, default: Option<T>) -> Option<T> {
    match choice {
        Choice::Value(v) => Some(v),
        Choice::ModelDefault => default,
    }
}

impl GenerationTab {
    pub fn new(current: GenerationParams, caps: ModelCaps) -> Self {
        let d = caps.defaults;
        let seeded = Choices {
            temperature: seed(current.temperature, d.temperature),
            effort: seed(current.reasoning_effort, d.reasoning_effort),
            budget: seed(current.thinking_budget_tokens, d.thinking_budget_tokens),
            max_tokens: seed(current.max_output_tokens, d.max_output_tokens),
        };
        Self {
            initial: seeded,
            now: seeded,
            caps,
        }
    }

    pub fn caps(&self) -> &ModelCaps {
        &self.caps
    }

    /// Re-derive against a newly selected model: a field the model doesn't
    /// accept (or a value outside its tiers/cap) drops back to "model default".
    pub fn set_caps(&mut self, caps: ModelCaps) {
        if !caps.temperature {
            self.now.temperature = Choice::ModelDefault;
        }
        if matches!(self.now.effort, Choice::Value(e) if !caps.effort_tiers.contains(&e)) {
            self.now.effort = Choice::ModelDefault;
        }
        if !caps.thinking_budget {
            self.now.budget = Choice::ModelDefault;
        }
        let cap = caps.max_output_tokens.unwrap_or(u32::MAX);
        if matches!(self.now.max_tokens, Choice::Value(v) if v > cap) {
            self.now.max_tokens = Choice::ModelDefault;
        }
        self.caps = caps;
    }

    pub fn cycle(&mut self, field: GenField, forward: bool) {
        let n = &mut self.now;
        match field {
            GenField::Temperature => n.temperature = cycle(n.temperature, &TEMPERATURES, forward),
            GenField::Effort => n.effort = cycle(n.effort, &self.caps.effort_tiers, forward),
            GenField::ThinkingBudget => n.budget = cycle(n.budget, &BUDGETS, forward),
            GenField::MaxTokens => {
                let cap = self.caps.max_output_tokens.unwrap_or(u32::MAX);
                let opts: Vec<u32> = MAX_TOKENS.into_iter().filter(|v| *v <= cap).collect();
                n.max_tokens = cycle(n.max_tokens, &opts, forward);
            }
        }
    }

    pub fn value_label(&self, field: GenField) -> String {
        let (n, d) = (self.now, self.caps.defaults);
        match field {
            GenField::Temperature => label(n.temperature, d.temperature),
            GenField::Effort => label(
                match n.effort {
                    Choice::Value(e) => Choice::Value(e.as_str()),
                    Choice::ModelDefault => Choice::ModelDefault,
                },
                d.reasoning_effort.map(|e| e.as_str()),
            ),
            GenField::ThinkingBudget => label(n.budget, d.thinking_budget_tokens),
            GenField::MaxTokens => label(n.max_tokens, d.max_output_tokens),
        }
    }

    pub fn changed(&self, field: GenField) -> bool {
        let (n, i) = (self.now, self.initial);
        match field {
            GenField::Temperature => n.temperature != i.temperature,
            GenField::Effort => n.effort != i.effort,
            GenField::ThinkingBudget => n.budget != i.budget,
            GenField::MaxTokens => n.max_tokens != i.max_tokens,
        }
    }

    /// The partial `SetGeneration` override for every visible field changed
    /// from the session's starting choice. `SetGeneration` merges and cannot
    /// clear a field, so "model default" is sent as the catalog's concrete
    /// default; a field with no catalog default to reset to is returned in
    /// the second slot instead of being sent.
    pub fn overrides(&self) -> (GenerationParams, Vec<GenField>) {
        let (n, d) = (self.now, self.caps.defaults);
        let mut out = GenerationParams::default();
        let mut unresettable = Vec::new();
        for field in self.caps.visible_fields() {
            if !self.changed(field) {
                continue;
            }
            let sent = match field {
                GenField::Temperature => {
                    out.temperature = resolve(n.temperature, d.temperature);
                    out.temperature.is_some()
                }
                GenField::Effort => {
                    out.reasoning_effort = resolve(n.effort, d.reasoning_effort);
                    out.reasoning_effort.is_some()
                }
                GenField::ThinkingBudget => {
                    out.thinking_budget_tokens = resolve(n.budget, d.thinking_budget_tokens);
                    out.thinking_budget_tokens.is_some()
                }
                GenField::MaxTokens => {
                    out.max_output_tokens = resolve(n.max_tokens, d.max_output_tokens);
                    out.max_output_tokens.is_some()
                }
            };
            if !sent {
                unresettable.push(field);
            }
        }
        (out, unresettable)
    }

    /// Human-readable pending changes for the footer.
    pub fn pending(&self) -> Vec<String> {
        self.caps
            .visible_fields()
            .into_iter()
            .filter(|f| self.changed(*f))
            .map(|f| format!("{} → {}", f.label(), self.value_label(f)))
            .collect()
    }
}
