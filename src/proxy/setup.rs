use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::accounts::{AccountProvider, AccountStore};
use crate::proxy::ProxyState;

const CHATGPT_CALLBACK_PORT: u16 = 1455;
const SETUP_HTML: &str = include_str!("setup.html");

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Idle,
    Pending,
    Connected,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthAttempt {
    pub state: AttemptState,
    pub message: String,
}

impl Default for AuthAttempt {
    fn default() -> Self {
        Self {
            state: AttemptState::Idle,
            message: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SetupStatus {
    pub openai: AuthAttempt,
}

pub fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

pub async fn page(State(state): State<Arc<ProxyState>>) -> Response {
    if !state.setup_enabled {
        return setup_disabled();
    }
    (
        StatusCode::OK,
        [
            ("content-type", "text/html; charset=utf-8"),
            ("cache-control", "no-store"),
            ("x-content-type-options", "nosniff"),
            ("x-frame-options", "DENY"),
            (
                "content-security-policy",
                "default-src 'none'; connect-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            ),
        ],
        Html(SETUP_HTML),
    )
        .into_response()
}

pub async fn state(State(state): State<Arc<ProxyState>>) -> Response {
    if !state.setup_enabled {
        return setup_disabled();
    }
    match AccountStore::load() {
        Ok(store) => {
            let status = state.setup_status.read().await.clone();
            (
                StatusCode::OK,
                [("cache-control", "no-store")],
                Json(json!({
                    "accounts": store.accounts,
                    "default_model": store.default_model,
                    "auth": status,
                })),
            )
                .into_response()
        }
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

pub async fn connect_openai(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(error) = require_same_origin(&state, &headers) {
        return origin_error_response(error);
    }

    {
        let status = state.setup_status.read().await;
        if matches!(status.openai.state, AttemptState::Pending) {
            return api_error(
                StatusCode::CONFLICT,
                "OpenAI authorization is already in progress",
            );
        }
    }

    let oauth_state = uuid::Uuid::new_v4().to_string();
    let pkce = crate::oauth::server::PkceChallenge::generate();
    let callback = match crate::oauth::server::CallbackServer::bind(
        CHATGPT_CALLBACK_PORT,
        Some(oauth_state.clone()),
    )
    .await
    {
        Ok(callback) => callback,
        Err(error) => {
            return api_error(
                StatusCode::CONFLICT,
                format!("could not start the local OpenAI callback: {error}"),
            )
        }
    };
    let authorization_url = crate::oauth::exchange::build_chatgpt_authorize_url(
        CHATGPT_CALLBACK_PORT,
        &pkce,
        &oauth_state,
    );

    {
        let mut status = state.setup_status.write().await;
        status.openai = AuthAttempt {
            state: AttemptState::Pending,
            message: "Waiting for browser authorization…".to_string(),
        };
    }

    let completion_state = state.clone();
    tokio::spawn(async move {
        let result = complete_openai_connection(&completion_state, callback, pkce).await;
        let mut status = completion_state.setup_status.write().await;
        status.openai = match result {
            Ok(()) => AuthAttempt {
                state: AttemptState::Connected,
                message: "OpenAI is connected. Use /model in Claude Code to switch.".to_string(),
            },
            Err(error) => {
                tracing::warn!(%error, "OpenAI account connection failed");
                AuthAttempt {
                    state: AttemptState::Error,
                    message: error.to_string(),
                }
            }
        };
    });

    (
        StatusCode::OK,
        Json(json!({"authorization_url": authorization_url})),
    )
        .into_response()
}

async fn complete_openai_connection(
    state: &Arc<ProxyState>,
    callback: crate::oauth::server::CallbackServer,
    pkce: crate::oauth::server::PkceChallenge,
) -> anyhow::Result<()> {
    let code = callback.wait().await?;
    let redirect_uri = format!("http://localhost:{CHATGPT_CALLBACK_PORT}/auth/callback");
    let token = crate::oauth::exchange::exchange_chatgpt_code(
        &state.http_client,
        &code,
        &redirect_uri,
        &pkce.code_verifier,
    )
    .await?;

    let _guard = state.account_store_lock.lock().await;
    let mut store = AccountStore::load()?;
    let record = store.upsert(AccountProvider::Openai);
    crate::oauth::source::store_keyring(&record.credential_key, &token)?;
    state
        .token_manager
        .invalidate(crate::accounts::OPENAI_PROFILE_NAME)
        .await;
    store.save()?;
    crate::integration::sync_openai_skills(true)?;
    let mut config = state.config.write().await;
    crate::accounts::apply_store_to_config(&mut config, &store);
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct AnthropicRequest {
    api_key: String,
}

pub async fn connect_anthropic(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(request): Json<AnthropicRequest>,
) -> Response {
    if let Err(error) = require_same_origin(&state, &headers) {
        return origin_error_response(error);
    }
    let api_key = request.api_key.trim();
    if api_key.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "Anthropic API key cannot be empty");
    }

    let result = async {
        let models = discover_anthropic_models(&state.http_client, api_key).await?;
        let _guard = state.account_store_lock.lock().await;
        let mut store = AccountStore::load()?;
        let record = store.upsert_with_models(AccountProvider::Anthropic, models);
        crate::accounts::store_api_key(&record.credential_key, api_key)?;
        store.save()?;
        let mut config = state.config.write().await;
        crate::accounts::apply_store_to_config(&mut config, &store);
        Ok::<(), anyhow::Error>(())
    }
    .await;

    match result {
        Ok(()) => (StatusCode::OK, Json(json!({"connected": true}))).into_response(),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

async fn discover_anthropic_models(
    client: &reqwest::Client,
    api_key: &str,
) -> anyhow::Result<Vec<String>> {
    let response = client
        .get("https://api.anthropic.com/v1/models?limit=100")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("Anthropic rejected the API key (HTTP {status})");
    }
    let body: serde_json::Value = response.json().await?;
    parse_anthropic_models(&body)
}

fn parse_anthropic_models(body: &serde_json::Value) -> anyhow::Result<Vec<String>> {
    let mut models: Vec<String> = body
        .get("data")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| model.get("id").and_then(serde_json::Value::as_str))
        .filter(|model| model.starts_with("claude-"))
        .map(ToOwned::to_owned)
        .collect();
    models.dedup();
    if models.is_empty() {
        anyhow::bail!("Anthropic returned no available Claude models for this account");
    }
    Ok(models)
}

pub async fn remove_account(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(error) = require_same_origin(&state, &headers) {
        return origin_error_response(error);
    }

    let result = async {
        let _guard = state.account_store_lock.lock().await;
        let mut store = AccountStore::load()?;
        let record = store
            .remove(&id)
            .ok_or_else(|| anyhow::anyhow!("account '{id}' is not connected"))?;
        let delete_result = match record.provider {
            AccountProvider::Openai => {
                crate::oauth::source::delete_keyring(&record.credential_key)
            }
            AccountProvider::Anthropic => {
                crate::accounts::delete_api_key(&record.credential_key)
            }
        };
        if let Err(error) = delete_result {
            tracing::warn!(account = %record.id, %error, "credential was already absent or could not be removed");
        }
        store.save()?;
        crate::integration::sync_openai_skills(
            store.has_provider(AccountProvider::Openai),
        )?;
        let mut config = state.config.write().await;
        crate::accounts::apply_store_to_config(&mut config, &store);
        Ok::<(), anyhow::Error>(())
    }
    .await;

    match result {
        Ok(()) => {
            if id == "openai" {
                state
                    .token_manager
                    .invalidate(crate::accounts::OPENAI_PROFILE_NAME)
                    .await;
                state.setup_status.write().await.openai = AuthAttempt::default();
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) if error.to_string().contains("is not connected") => {
            api_error(StatusCode::NOT_FOUND, error.to_string())
        }
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct DefaultModelRequest {
    model: String,
}

pub async fn set_default_model(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(request): Json<DefaultModelRequest>,
) -> Response {
    if let Err(error) = require_same_origin(&state, &headers) {
        return origin_error_response(error);
    }

    let result = async {
        let _guard = state.account_store_lock.lock().await;
        let mut store = AccountStore::load()?;
        store.set_default_model(request.model.trim())?;
        store.save()?;
        let mut config = state.config.write().await;
        crate::accounts::apply_store_to_config(&mut config, &store);
        Ok::<(), anyhow::Error>(())
    }
    .await;

    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => api_error(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

enum OriginError {
    Disabled,
    Forbidden,
}

fn require_same_origin(state: &ProxyState, headers: &HeaderMap) -> Result<(), OriginError> {
    if !state.setup_enabled {
        return Err(OriginError::Disabled);
    }
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if origin != Some(state.setup_origin.as_str()) {
        return Err(OriginError::Forbidden);
    }
    Ok(())
}

fn origin_error_response(error: OriginError) -> Response {
    match error {
        OriginError::Disabled => setup_disabled(),
        OriginError::Forbidden => api_error(StatusCode::FORBIDDEN, "same-origin request required"),
    }
}

fn api_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

fn setup_disabled() -> Response {
    api_error(
        StatusCode::NOT_FOUND,
        "the model manager is available only when proxy_host is loopback",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_is_restricted_to_loopback_hosts() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("localhost"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("192.168.1.10"));
    }

    #[test]
    fn account_manager_has_no_remote_assets() {
        assert!(!SETUP_HTML.contains("<script src="));
        assert!(!SETUP_HTML.contains("<link rel="));
        assert!(!SETUP_HTML.contains("analytics"));
        assert!(!SETUP_HTML.contains("telemetry"));
    }

    #[test]
    fn anthropic_model_catalog_uses_models_returned_for_the_account() {
        let models = parse_anthropic_models(&json!({
            "data": [
                {"id": "claude-sonnet-5"},
                {"id": "claude-haiku-4-5-20251001"},
                {"id": "not-a-claude-model"}
            ]
        }))
        .unwrap();
        assert_eq!(models, vec!["claude-sonnet-5", "claude-haiku-4-5-20251001"]);
    }
}
