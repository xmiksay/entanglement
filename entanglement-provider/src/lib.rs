pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub context_window: Option<u32>,
}

pub mod anthropic;
pub mod client;
pub mod openai;

pub use anthropic::{anthropic_factory, AnthropicLlm};
pub use client::HttpClient;
pub use openai::{
    openai_factory, OpenAiLlm, OLLAMA_BASE, OPENAI_BASE, ZAI_CODING_PLAN_BASE, ZAI_GENERAL_BASE,
};

fn zai_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "glm-5.2".to_string(),
            display_name: "GLM-5.2".to_string(),
            context_window: Some(128000),
        },
        ModelInfo {
            id: "glm-4.7".to_string(),
            display_name: "GLM-4.7".to_string(),
            context_window: Some(128000),
        },
    ]
}

fn openai_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "gpt-4o".to_string(),
            display_name: "GPT-4o".to_string(),
            context_window: Some(128000),
        },
        ModelInfo {
            id: "gpt-4-turbo".to_string(),
            display_name: "GPT-4 Turbo".to_string(),
            context_window: Some(128000),
        },
        ModelInfo {
            id: "gpt-3.5-turbo".to_string(),
            display_name: "GPT-3.5 Turbo".to_string(),
            context_window: Some(16385),
        },
    ]
}

fn ollama_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "llama3.1".to_string(),
            display_name: "Llama 3.1".to_string(),
            context_window: Some(128000),
        },
        ModelInfo {
            id: "llama3".to_string(),
            display_name: "Llama 3".to_string(),
            context_window: Some(8192),
        },
        ModelInfo {
            id: "mistral".to_string(),
            display_name: "Mistral".to_string(),
            context_window: Some(32768),
        },
    ]
}

fn anthropic_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "claude-sonnet-4-5".to_string(),
            display_name: "Claude Sonnet 4.5".to_string(),
            context_window: Some(200000),
        },
        ModelInfo {
            id: "claude-3-5-sonnet-20241022".to_string(),
            display_name: "Claude 3.5 Sonnet".to_string(),
            context_window: Some(200000),
        },
    ]
}

pub fn models_for(provider: &str) -> Vec<ModelInfo> {
    match provider {
        "zai" => zai_models(),
        "openai" => openai_models(),
        "ollama" => ollama_models(),
        "anthropic" => anthropic_models(),
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_models_for_known_provider() {
        let models = models_for("zai");
        assert!(!models.is_empty());
        assert_eq!(models[0].id, "glm-5.2");
    }

    #[test]
    fn test_models_for_unknown_provider() {
        let models = models_for("unknown");
        assert!(models.is_empty());
    }

    #[test]
    fn test_model_info_fields() {
        let info = ModelInfo {
            id: "test-model".to_string(),
            display_name: "Test Model".to_string(),
            context_window: Some(42000),
        };
        assert_eq!(info.id, "test-model");
        assert_eq!(info.display_name, "Test Model");
        assert_eq!(info.context_window, Some(42000));
    }

    #[test]
    fn test_all_providers_have_models() {
        for provider in &["zai", "openai", "ollama", "anthropic"] {
            let models = models_for(provider);
            assert!(!models.is_empty(), "Provider {provider} should have models");
            for model in &models {
                assert!(!model.id.is_empty());
                assert!(!model.display_name.is_empty());
            }
        }
    }
}
