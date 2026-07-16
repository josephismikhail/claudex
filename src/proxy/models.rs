use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::config::{ClaudexConfig, ProfileConfig, ProviderType};
use crate::proxy::ProxyState;

pub async fn list_models(State(state): State<Arc<ProxyState>>) -> Response {
    let config = state.config.read().await;
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for profile in config.enabled_profiles() {
        for model in models_for_profile(&config, profile) {
            let id = model["id"].as_str().unwrap_or_default().to_string();
            if seen.insert(id) {
                models.push(model);
            }
        }
    }
    model_list_response(models)
}

pub async fn list_profile_models(
    State(state): State<Arc<ProxyState>>,
    Path(profile_name): Path<String>,
) -> Response {
    let config = state.config.read().await;
    let Some(profile) = config.find_profile(&profile_name) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "type": "error",
                "error": {
                    "type": "not_found_error",
                    "message": format!("profile '{profile_name}' not found")
                }
            })),
        )
            .into_response();
    };
    model_list_response(models_for_profile(&config, profile))
}

fn models_for_profile(config: &ClaudexConfig, profile: &ProfileConfig) -> Vec<Value> {
    let mut candidates = vec![config.resolve_model(&profile.default_model)];
    for model in [
        profile.models.haiku.as_deref(),
        profile.models.sonnet.as_deref(),
        profile.models.opus.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        candidates.push(config.resolve_model(model));
    }

    let mut routes: Vec<_> = profile.model_routes.keys().cloned().collect();
    routes.sort();
    candidates.extend(routes);

    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|model| {
            !model.is_empty()
                && model != crate::accounts::ONBOARDING_MODEL
                && seen.insert(model.clone())
        })
        .filter_map(|model| {
            let target = profile
                .model_routes
                .get(&model)
                .and_then(|name| config.find_profile(name))
                .unwrap_or(profile);
            target.enabled.then(|| model_entry(&model, target))
        })
        .collect()
}

fn model_entry(model: &str, target: &ProfileConfig) -> Value {
    let provider = provider_label(target);
    json!({
        "id": model,
        "type": "model",
        "object": "model",
        "display_name": format!("{model} · {provider}"),
        "created_at": "1970-01-01T00:00:00Z",
        "created": 0,
        "owned_by": target.name,
        "x-claudex-profile": target.name,
        "x-claudex-provider": match target.provider_type {
            ProviderType::DirectAnthropic => "anthropic",
            ProviderType::OpenAICompatible => "openai-compatible",
            ProviderType::OpenAIResponses => "openai-responses",
        },
    })
}

fn provider_label(target: &ProfileConfig) -> &'static str {
    match target.provider_type {
        ProviderType::DirectAnthropic => "Anthropic",
        ProviderType::OpenAIResponses => "OpenAI",
        ProviderType::OpenAICompatible => "OpenAI-compatible provider",
    }
}

fn model_list_response(models: Vec<Value>) -> Response {
    let first_id = models.first().and_then(|model| model["id"].as_str());
    let last_id = models.last().and_then(|model| model["id"].as_str());
    (
        StatusCode::OK,
        Json(json!({
            "object": "list",
            "data": models,
            "has_more": false,
            "first_id": first_id,
            "last_id": last_id,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProfileModels;

    #[test]
    fn profile_catalog_includes_slots_and_cross_provider_routes() {
        let launch = ProfileConfig {
            name: "ultracode".to_string(),
            default_model: "gpt-default".to_string(),
            models: ProfileModels {
                haiku: Some("fast".to_string()),
                sonnet: Some("gpt-default".to_string()),
                opus: None,
            },
            model_routes: std::collections::HashMap::from([
                ("claude-opus".to_string(), "claude-max".to_string()),
                ("claude-sonnet".to_string(), "claude-max".to_string()),
            ]),
            ..Default::default()
        };
        let claude = ProfileConfig {
            name: "claude-max".to_string(),
            provider_type: ProviderType::DirectAnthropic,
            default_model: "claude-sonnet".to_string(),
            ..Default::default()
        };
        let config = ClaudexConfig {
            profiles: vec![launch, claude],
            model_aliases: std::collections::HashMap::from([(
                "fast".to_string(),
                "gpt-fast".to_string(),
            )]),
            ..Default::default()
        };

        let models = models_for_profile(&config, &config.profiles[0]);
        let ids: Vec<_> = models
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec!["gpt-default", "gpt-fast", "claude-opus", "claude-sonnet"]
        );
        let routed = models
            .iter()
            .find(|model| model["id"] == "claude-opus")
            .unwrap();
        assert_eq!(routed["x-claudex-profile"], "claude-max");
        assert_eq!(routed["x-claudex-provider"], "anthropic");
        assert_eq!(routed["display_name"], "claude-opus · Anthropic");
    }

    #[test]
    fn profile_catalog_hides_routes_to_disabled_profiles() {
        let source = ProfileConfig {
            name: "source".to_string(),
            model_routes: std::collections::HashMap::from([(
                "disabled-model".to_string(),
                "disabled".to_string(),
            )]),
            ..Default::default()
        };
        let disabled = ProfileConfig {
            name: "disabled".to_string(),
            enabled: false,
            ..Default::default()
        };
        let config = ClaudexConfig {
            profiles: vec![source, disabled],
            ..Default::default()
        };

        let models = models_for_profile(&config, &config.profiles[0]);
        assert!(!models.iter().any(|model| model["id"] == "disabled-model"));
    }

    #[test]
    fn empty_runtime_catalog_does_not_expose_onboarding_as_a_provider_model() {
        let mut config = ClaudexConfig::default();
        crate::accounts::apply_store_to_config(
            &mut config,
            &crate::accounts::AccountStore::default(),
        );
        let root = config
            .find_profile(crate::accounts::SESSION_PROFILE_NAME)
            .unwrap();
        assert!(models_for_profile(&config, root).is_empty());
    }
}
