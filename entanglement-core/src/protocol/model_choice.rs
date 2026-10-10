//! A catalog-qualified model pick carried on [`InMsg::Spawn`] (#725).

use serde::{Deserialize, Serialize};

#[cfg(doc)]
use crate::protocol::InMsg;

/// A catalog-qualified model choice: the same `provider` + `model` pair
/// [`InMsg::SetModel`] carries inline, as a struct so [`InMsg::Spawn`] can
/// make it optional as one unit. Resolved through the runtime-supplied
/// `model_resolver` exactly like a `SetModel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    pub provider: String,
    pub model: String,
}

impl ModelChoice {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ModelChoice;
    use crate::protocol::InMsg;

    fn spawn_model(msg: &InMsg) -> Option<ModelChoice> {
        match msg {
            InMsg::Spawn { model, .. } => model.clone(),
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    /// A pre-#725 `Spawn` frame (and every old log's) has no `model` key: it
    /// must still deserialize, as `None`, and re-serialize without the key.
    #[test]
    fn spawn_without_model_stays_wire_compatible() {
        let json = r#"{"kind":"spawn","session":"c1","parent":"p1","agent":"build","prompt":"go"}"#;
        let msg: InMsg = serde_json::from_str(json).expect("old Spawn frame");
        assert_eq!(spawn_model(&msg), None);
        let back = serde_json::to_value(&msg).expect("serialize");
        assert!(back.get("model").is_none(), "None is omitted: {back}");
    }

    #[test]
    fn spawn_model_round_trips() {
        let json = r#"{"kind":"spawn","session":"c1","agent":"build","prompt":"go","model":{"provider":"zai","model":"glm-b"}}"#;
        let msg: InMsg = serde_json::from_str(json).expect("Spawn with model");
        assert_eq!(spawn_model(&msg), Some(ModelChoice::new("zai", "glm-b")));
        let back: InMsg =
            serde_json::from_value(serde_json::to_value(&msg).expect("serialize")).expect("parse");
        assert_eq!(spawn_model(&back), Some(ModelChoice::new("zai", "glm-b")));
    }
}
