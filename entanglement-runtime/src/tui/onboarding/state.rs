//! The first-run onboarding dialog's state machine (#732): pure, terminal-free,
//! driven one [`KeyEvent`] at a time so the whole flow is unit-testable.
//!
//! Stages: pick a provider (every catalog entry except OAuth-only ones, which
//! need `skutter config connect`) → enter its key (masked) **or**, for a
//! keyless provider, confirm its base URL → pick a model (catalog default
//! preselected). `Esc` on the first stage continues in echo mode; on a later
//! stage it steps back, wiping a typed key.

use entanglement_provider::Catalog;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// One provider the dialog offers.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProviderOption {
    pub name: String,
    /// `None` ⇒ keyless (local Ollama and friends): ask for a base URL instead.
    pub key_env: Option<String>,
    /// The catalog `base_url`, prefilled into the keyless base prompt.
    pub base_url: Option<String>,
    /// Model ids, the catalog default first-class: always present, preselected.
    pub models: Vec<String>,
    pub default_model: String,
}

impl ProviderOption {
    fn default_index(&self) -> usize {
        self.models
            .iter()
            .position(|m| *m == self.default_model)
            .unwrap_or(0)
    }
}

/// The catalog providers the dialog can configure, in catalog order.
pub(crate) fn options(catalog: &Catalog) -> Vec<ProviderOption> {
    options_with(catalog, |name| {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    })
}

/// [`options`] over an injected env lookup. A keyless provider's base
/// prefill follows startup's own precedence: `<NAME>_API_BASE` > catalog
/// (the legacy `<NAME>_BASE` spelling is retired, ADR-0213).
fn options_with(catalog: &Catalog, env: impl Fn(&str) -> Option<String>) -> Vec<ProviderOption> {
    catalog
        .providers
        .iter()
        .filter(|p| p.key_env.is_some() || p.oauth.is_none())
        .map(|p| {
            let mut models: Vec<String> = p.models.iter().map(|m| m.id.clone()).collect();
            if !models.contains(&p.default_model) {
                models.insert(0, p.default_model.clone());
            }
            ProviderOption {
                name: p.name.clone(),
                key_env: p.key_env.clone(),
                base_url: env(&format!("{}_API_BASE", p.name.to_uppercase()))
                    .or_else(|| p.base_url.clone()),
                models,
                default_model: p.default_model.clone(),
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Provider,
    Key,
    Base,
    Model,
}

/// What the user settled on. `Debug` is hand-written to redact the key.
#[derive(Clone, PartialEq)]
pub(crate) struct Setup {
    pub provider: String,
    /// `(KEY_ENV, value)` for a keyed provider.
    pub key: Option<(String, String)>,
    /// `(NAME_API_BASE, url)` for a keyless provider whose URL was edited away
    /// from the catalog's own — `None` keeps the catalog value.
    pub base: Option<(String, String)>,
    pub model: String,
}

impl std::fmt::Debug for Setup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Setup")
            .field("provider", &self.provider)
            .field(
                "key",
                &self.key.as_ref().map(|(env, _)| (env, "<redacted>")),
            )
            .field("base", &self.base)
            .field("model", &self.model)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outcome {
    Configured(Setup),
    /// `Esc` on the first stage: run on the labeled echo stub.
    Echo,
    /// `Ctrl+C`: leave skutter.
    Quit,
}

pub(crate) struct Onboarding {
    providers: Vec<ProviderOption>,
    provider: usize,
    stage: Stage,
    /// The key or base URL being typed; a key never renders in clear.
    buffer: String,
    model: usize,
}

impl Onboarding {
    pub(crate) fn new(providers: Vec<ProviderOption>) -> Self {
        Self {
            providers,
            provider: 0,
            stage: Stage::Provider,
            buffer: String::new(),
            model: 0,
        }
    }

    pub(crate) fn stage(&self) -> Stage {
        self.stage
    }

    pub(crate) fn providers(&self) -> &[ProviderOption] {
        &self.providers
    }

    pub(crate) fn provider_index(&self) -> usize {
        self.provider
    }

    pub(crate) fn model_index(&self) -> usize {
        self.model
    }

    pub(crate) fn selected(&self) -> Option<&ProviderOption> {
        self.providers.get(self.provider)
    }

    /// The key buffer as bullets — the key itself is never drawn.
    pub(crate) fn masked_key(&self) -> String {
        "•".repeat(self.buffer.chars().count())
    }

    /// The base-URL buffer (not secret, shown in clear).
    pub(crate) fn base_text(&self) -> &str {
        &self.buffer
    }

    /// Feed one key press; `Some` once the dialog is finished.
    pub(crate) fn on_key(&mut self, key: KeyEvent) -> Option<Outcome> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.buffer.clear();
            return Some(Outcome::Quit);
        }
        match self.stage {
            Stage::Provider => self.on_provider_key(key.code),
            Stage::Key | Stage::Base => {
                self.on_text_key(key.code);
                None
            }
            Stage::Model => self.on_model_key(key.code),
        }
    }

    fn on_provider_key(&mut self, code: KeyCode) -> Option<Outcome> {
        let len = self.providers.len();
        match code {
            KeyCode::Esc => return Some(Outcome::Echo),
            KeyCode::Down | KeyCode::Char('j') if len > 0 => {
                self.provider = (self.provider + 1) % len
            }
            KeyCode::Up | KeyCode::Char('k') if len > 0 => {
                self.provider = (self.provider + len - 1) % len
            }
            KeyCode::Enter => {
                let option = self.selected()?;
                let (stage, buffer) = match option.key_env {
                    Some(_) => (Stage::Key, String::new()),
                    None => (Stage::Base, option.base_url.clone().unwrap_or_default()),
                };
                self.stage = stage;
                self.buffer = buffer;
            }
            _ => {}
        }
        None
    }

    fn on_text_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.buffer.clear();
                self.stage = Stage::Provider;
            }
            KeyCode::Backspace => {
                self.buffer.pop();
            }
            KeyCode::Char(c) => self.buffer.push(c),
            KeyCode::Enter if !self.buffer.trim().is_empty() => {
                self.model = self.selected().map_or(0, ProviderOption::default_index);
                self.stage = Stage::Model;
            }
            _ => {}
        }
    }

    fn on_model_key(&mut self, code: KeyCode) -> Option<Outcome> {
        let len = self.selected().map_or(0, |p| p.models.len());
        match code {
            KeyCode::Esc => {
                let keyed = self.selected().is_some_and(|p| p.key_env.is_some());
                // Back to the text stage; a key is re-typed rather than kept.
                if keyed {
                    self.buffer.clear();
                }
                self.stage = if keyed { Stage::Key } else { Stage::Base };
            }
            KeyCode::Down | KeyCode::Char('j') if len > 0 => self.model = (self.model + 1) % len,
            KeyCode::Up | KeyCode::Char('k') if len > 0 => {
                self.model = (self.model + len - 1) % len
            }
            KeyCode::Enter => return self.finish().map(Outcome::Configured),
            _ => {}
        }
        None
    }

    fn finish(&mut self) -> Option<Setup> {
        let option = self.selected()?.clone();
        let model = option.models.get(self.model)?.clone();
        let value = std::mem::take(&mut self.buffer).trim().to_string();
        let (key, base) = match &option.key_env {
            Some(env) => (Some((env.clone(), value)), None),
            None if option.base_url.as_deref() == Some(value.as_str()) => (None, None),
            None => {
                let env = format!("{}_API_BASE", option.name.to_uppercase());
                (None, Some((env, value)))
            }
        };
        Some(Setup {
            provider: option.name,
            key,
            base,
            model,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(o: &mut Onboarding, code: KeyCode) -> Option<Outcome> {
        o.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_str(o: &mut Onboarding, s: &str) {
        for c in s.chars() {
            assert_eq!(press(o, KeyCode::Char(c)), None);
        }
    }

    fn dialog() -> Onboarding {
        Onboarding::new(options(&Catalog::builtin()))
    }

    fn select(o: &mut Onboarding, name: &str) {
        while o.selected().unwrap().name != name {
            press(o, KeyCode::Down);
        }
    }

    #[test]
    fn offers_every_catalog_provider_including_keyless_ollama() {
        let names: Vec<String> = options(&Catalog::builtin())
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert!(names.contains(&"zai".to_string()));
        assert!(names.contains(&"anthropic".to_string()));
        assert!(names.contains(&"ollama".to_string()));
    }

    #[test]
    fn keyed_flow_picks_provider_then_masked_key_then_default_model() {
        let mut o = dialog();
        select(&mut o, "anthropic");
        press(&mut o, KeyCode::Enter);
        assert_eq!(o.stage(), Stage::Key);
        type_str(&mut o, "sk-secret");
        assert_eq!(o.masked_key(), "•••••••••", "the key renders as bullets");
        press(&mut o, KeyCode::Enter);
        assert_eq!(o.stage(), Stage::Model);
        let option = o.selected().unwrap().clone();
        assert_eq!(option.models[o.model_index()], option.default_model);
        let Some(Outcome::Configured(setup)) = press(&mut o, KeyCode::Enter) else {
            panic!("expected a configured outcome");
        };
        assert_eq!(setup.provider, "anthropic");
        assert_eq!(
            setup.key,
            Some(("ANTHROPIC_API_KEY".into(), "sk-secret".into()))
        );
        assert_eq!(setup.model, option.default_model);
        assert_eq!(setup.base, None);
    }

    #[test]
    fn an_empty_key_does_not_advance() {
        let mut o = dialog();
        press(&mut o, KeyCode::Enter);
        press(&mut o, KeyCode::Enter);
        assert_eq!(o.stage(), Stage::Key);
    }

    #[test]
    fn keyless_provider_prompts_for_a_prefilled_base_url() {
        let mut o = dialog();
        select(&mut o, "ollama");
        press(&mut o, KeyCode::Enter);
        assert_eq!(o.stage(), Stage::Base);
        let catalog_base = o.selected().unwrap().base_url.clone().unwrap();
        assert_eq!(o.base_text(), catalog_base);
        // Unchanged base: nothing to persist.
        press(&mut o, KeyCode::Enter);
        let Some(Outcome::Configured(setup)) = press(&mut o, KeyCode::Enter) else {
            panic!("expected a configured outcome");
        };
        assert_eq!((setup.key, setup.base), (None, None));
    }

    #[test]
    fn an_edited_base_url_is_persisted_under_the_api_base_var() {
        let mut o = dialog();
        select(&mut o, "ollama");
        press(&mut o, KeyCode::Enter);
        for _ in 0..o.base_text().len() {
            press(&mut o, KeyCode::Backspace);
        }
        type_str(&mut o, "http://gpu-box:11434/v1");
        press(&mut o, KeyCode::Enter);
        press(&mut o, KeyCode::Down);
        let Some(Outcome::Configured(setup)) = press(&mut o, KeyCode::Enter) else {
            panic!("expected a configured outcome");
        };
        assert_eq!(
            setup.base,
            Some(("OLLAMA_API_BASE".into(), "http://gpu-box:11434/v1".into()))
        );
    }

    #[test]
    fn esc_on_the_first_stage_means_echo_and_later_esc_steps_back() {
        let mut o = dialog();
        press(&mut o, KeyCode::Enter);
        type_str(&mut o, "abc");
        press(&mut o, KeyCode::Enter);
        assert_eq!(press(&mut o, KeyCode::Esc), None);
        assert_eq!(o.stage(), Stage::Key);
        assert_eq!(o.masked_key(), "", "stepping back wipes the key");
        press(&mut o, KeyCode::Esc);
        assert_eq!(o.stage(), Stage::Provider);
        assert_eq!(press(&mut o, KeyCode::Esc), Some(Outcome::Echo));
    }

    #[test]
    fn the_base_prefill_prefers_the_api_base_env_var() {
        let env = |name: &str| (name == "OLLAMA_API_BASE").then(|| "http://env:1/v1".to_string());
        let ollama = options_with(&Catalog::builtin(), env)
            .into_iter()
            .find(|p| p.name == "ollama")
            .unwrap();
        assert_eq!(ollama.base_url.as_deref(), Some("http://env:1/v1"));
    }

    #[test]
    fn debug_output_redacts_the_key() {
        let setup = Setup {
            provider: "zai".into(),
            key: Some(("ZAI_API_KEY".into(), "sk-very-secret".into())),
            base: None,
            model: "glm".into(),
        };
        let shown = format!("{:?}", Outcome::Configured(setup));
        assert!(!shown.contains("sk-very-secret"), "{shown}");
        assert!(
            shown.contains("ZAI_API_KEY") && shown.contains("<redacted>"),
            "{shown}"
        );
    }

    #[test]
    fn ctrl_c_quits_from_any_stage() {
        let mut o = dialog();
        press(&mut o, KeyCode::Enter);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(o.on_key(ctrl_c), Some(Outcome::Quit));
    }
}
