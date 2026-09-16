//! The `/set` dialog's Session tab: agent profile + provider/model pickers
//! over the same data `/agent` and `/model` offer, plus the "final model"
//! the Generation tab validates against.

use std::collections::HashMap;

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
    agents: Vec<String>,
    agent: usize,
    initial_agent: usize,
    models: Vec<ModelOption>,
    model: usize,
    initial_model: usize,
    /// Each profile's persisted model pin: switching agent rebinds to it
    /// (ADR-0081), so it is the "final model" unless a model is picked too.
    agent_pins: HashMap<String, (String, String)>,
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
    pub fn new(
        mut agents: Vec<String>,
        current_agent: &str,
        mut models: Vec<ModelOption>,
        current_model: (&str, &str),
        agent_pins: HashMap<String, (String, String)>,
    ) -> Self {
        let agent = match agents.iter().position(|a| a == current_agent) {
            Some(i) => i,
            None => {
                agents.push(current_agent.to_string());
                agents.len() - 1
            }
        };
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
            agents,
            agent,
            initial_agent: agent,
            models,
            model,
            initial_model: model,
            agent_pins,
        }
    }

    pub fn cycle_agent(&mut self, forward: bool) {
        self.agent = step(self.agent, self.agents.len(), forward);
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
        &self.agents[self.agent]
    }

    pub fn agent_change(&self) -> Option<String> {
        (self.agent != self.initial_agent).then(|| self.final_agent().to_string())
    }

    pub fn model_change(&self) -> Option<(String, String)> {
        (self.model != self.initial_model).then(|| {
            let m = &self.models[self.model];
            (m.provider.clone(), m.model.clone())
        })
    }

    /// The model the session ends up on: an explicit pick, else the new
    /// agent's pin when the agent changes, else the current model. The bool
    /// says whether it came from an agent pin.
    pub fn final_model(&self) -> (ModelOption, bool) {
        if self.model == self.initial_model && self.agent != self.initial_agent {
            if let Some((p, m)) = self.agent_pins.get(self.final_agent()) {
                let option = self
                    .models
                    .iter()
                    .find(|o| &o.provider == p && &o.model == m)
                    .cloned()
                    .unwrap_or_else(|| ModelOption {
                        provider: p.clone(),
                        model: m.clone(),
                        caps: ModelCaps::unknown(),
                    });
                return (option, true);
            }
        }
        (self.models[self.model].clone(), false)
    }

    pub fn model_label(&self) -> String {
        let (m, pinned) = self.final_model();
        let suffix = if pinned { "  (agent pin)" } else { "" };
        format!("{}/{}{suffix}", m.provider, m.model)
    }

    pub fn pending(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(a) = self.agent_change() {
            out.push(format!("agent → {a}"));
        }
        if let Some((p, m)) = self.model_change() {
            out.push(format!("model → {p}/{m}"));
        }
        out
    }
}
