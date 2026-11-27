//! The `/set` dialog's Aux tab: per-purpose model pins over the managed
//! `aux-models.yml` store `/aux-model` writes (ADR-0154). Those pins are
//! process-wide and always persisted — there is no session-only variant.

use crate::config::aux_models::Purpose;

pub const PURPOSES: [Purpose; 3] = [Purpose::Summarize, Purpose::SessionTitle, Purpose::Narrate];

#[derive(Debug, Clone, PartialEq)]
struct Pin {
    purpose: Purpose,
    initial: Option<usize>,
    now: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuxTab {
    models: Vec<(String, String)>,
    pins: Vec<Pin>,
}

impl AuxTab {
    /// `pins` carries each purpose's current `(provider, model)`; a pinned
    /// model the catalog no longer lists is kept selectable as-is.
    pub fn new(
        mut models: Vec<(String, String)>,
        current: &[(Purpose, Option<(String, String)>)],
    ) -> Self {
        let pins = PURPOSES
            .iter()
            .map(|purpose| {
                let pinned = current
                    .iter()
                    .find(|(p, _)| p == purpose)
                    .and_then(|(_, pin)| pin.clone());
                let index = pinned.map(|pin| match models.iter().position(|m| *m == pin) {
                    Some(i) => i,
                    None => {
                        models.push(pin);
                        models.len() - 1
                    }
                });
                Pin {
                    purpose: *purpose,
                    initial: index,
                    now: index,
                }
            })
            .collect();
        Self { models, pins }
    }

    fn pin(&self, purpose: Purpose) -> Option<&Pin> {
        self.pins.iter().find(|p| p.purpose == purpose)
    }

    /// Step through the models. "(primary model)" is offered only while the
    /// purpose started unpinned: the store has no remove, so an existing pin
    /// can be moved but not cleared from here.
    pub fn cycle(&mut self, purpose: Purpose, forward: bool) {
        let n = self.models.len();
        let Some(pin) = self.pins.iter_mut().find(|p| p.purpose == purpose) else {
            return;
        };
        let mut options: Vec<Option<usize>> = Vec::new();
        if pin.initial.is_none() {
            options.push(None);
        }
        options.extend((0..n).map(Some));
        if options.is_empty() {
            return;
        }
        let len = options.len();
        let i = options.iter().position(|o| *o == pin.now).unwrap_or(0);
        pin.now = options[if forward {
            (i + 1) % len
        } else {
            (i + len - 1) % len
        }];
    }

    pub fn value_label(&self, purpose: Purpose) -> String {
        match self.pin(purpose).and_then(|p| p.now) {
            Some(i) => format!("{}/{}", self.models[i].0, self.models[i].1),
            None => "(primary model)".to_string(),
        }
    }

    pub fn changed(&self, purpose: Purpose) -> bool {
        self.pin(purpose).is_some_and(|p| p.now != p.initial)
    }

    /// `(purpose, provider, model)` for every moved pin.
    pub fn changes(&self) -> Vec<(Purpose, String, String)> {
        self.pins
            .iter()
            .filter(|p| p.now != p.initial)
            .filter_map(|p| {
                p.now.map(|i| {
                    (
                        p.purpose,
                        self.models[i].0.clone(),
                        self.models[i].1.clone(),
                    )
                })
            })
            .collect()
    }

    pub fn pending(&self) -> Vec<String> {
        PURPOSES
            .iter()
            .filter(|p| self.changed(**p))
            .map(|p| format!("aux {p} → {}", self.value_label(*p)))
            .collect()
    }
}
