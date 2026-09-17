//! The `/set` dialog's Session tab: the (read-only, ADR-0207 §9) agent display
//! plus the provider/model picker `/model` also offers, feeding the "final
//! model" the Generation tab validates against.

use entanglement_provider::Catalog;

use super::generation::ModelCaps;

/// One pickable model with its precomputed capabilities, so a model change
/// refreshes the Generation tab without re-reading the catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelOption {
    pub provider: String,
    pub model: String,
    pub caps: ModelCaps,
}

/// Every catalog model, in catalog order (the `/model` picker's order).
pub fn model_options(catalog: &Catalog) -> Vec<ModelOption> {
    catalog
        .providers
        .iter()
        .flat_map(|p| {
            p.models.iter().map(move |m| ModelOption {
                provider: p.name.clone(),
                model: m.id.clone(),
                caps: ModelCaps::from_entry(p.wire, m),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionTab {
    /// The session's agent — fixed for its whole life (ADR-0207 §9), so this
    /// is display-only: nothing in this tab ever changes it.
    agent: String,
    models: Vec<ModelOption>,
    model: usize,
    initial_model: usize,
}

fn step(i: usize, len: usize, forward: bool) -> usize {
    if len == 0 {
        0
    } else if forward {
        (i + 1) % len
    } else {
        (i + len - 1) % len
    }
}

impl SessionTab {
    pub fn new(agent: String, mut models: Vec<ModelOption>, current_model: (&str, &str)) -> Self {
        let (provider, id) = current_model;
        let model = match models
            .iter()
            .position(|m| m.provider == provider && m.model == id)
        {
            Some(i) => i,
            None => {
                models.push(ModelOption {
                    provider: provider.to_string(),
                    model: id.to_string(),
                    caps: ModelCaps::unknown(),
                });
                models.len() - 1
            }
        };
        Self {
            agent,
            models,
            model,
            initial_model: model,
        }
    }

    pub fn cycle_model(&mut self, forward: bool) {
        self.model = step(self.model, self.models.len(), forward);
    }

    /// Jump to the first model of the next/previous provider — a long flat
    /// model list is otherwise tedious to cycle.
    pub fn jump_provider(&mut self, forward: bool) {
        let len = self.models.len();
        let current = self.models[self.model].provider.clone();
        let mut i = self.model;
        for _ in 0..len {
            i = step(i, len, forward);
            if self.models[i].provider != current {
                break;
            }
        }
        // Land on the provider's first entry when stepping backwards.
        let target = self.models[i].provider.clone();
        while !forward && i > 0 && self.models[i - 1].provider == target {
            i -= 1;
        }
        self.model = i;
    }

    pub fn final_agent(&self) -> &str {
        &self.agent
    }

    pub fn model_change(&self) -> Option<(String, String)> {
        (self.model != self.initial_model).then(|| {
            let m = &self.models[self.model];
            (m.provider.clone(), m.model.clone())
        })
    }

    /// The model the session ends up on: the picked model, or the current one
    /// if untouched.
    pub fn final_model(&self) -> ModelOption {
        self.models[self.model].clone()
    }

    pub fn model_label(&self) -> String {
        let m = self.final_model();
        format!("{}/{}", m.provider, m.model)
    }

    pub fn pending(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some((p, m)) = self.model_change() {
            out.push(format!("model → {p}/{m}"));
        }
        out
    }
}
