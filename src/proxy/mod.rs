pub mod adapter;
pub mod context_engine;
pub mod error;
pub mod fallback;
pub mod handler;
pub mod health;
pub mod metrics;
pub mod models;
pub mod setup;
pub mod translate;
pub mod util;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use axum::routing::{delete, get, post};
use axum::Router;
use tokio::sync::RwLock;

use crate::config::ClaudexConfig;
use crate::context::rag::RagIndex;
use crate::context::sharing::SharedContext;
use metrics::MetricsStore;

pub struct ProxyState {
    pub config: Arc<RwLock<ClaudexConfig>>,
    pub metrics: MetricsStore,
    pub http_client: reqwest::Client,
    pub health_status: Arc<RwLock<health::HealthMap>>,
    pub circuit_breakers: fallback::CircuitBreakerMap,
    pub shared_context: SharedContext,
    pub rag_index: Option<RagIndex>,
    pub token_manager: crate::oauth::manager::TokenManager,
    pub setup_status: Arc<RwLock<setup::SetupStatus>>,
    pub account_store_lock: Arc<tokio::sync::Mutex<()>>,
    pub setup_enabled: bool,
    pub setup_origin: String,
}

static PROXY_LOG_PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();

/// Return the stable log path for this process.
///
/// The previous implementation regenerated the timestamp on every call, so
/// the path printed after a long session could differ from the file opened at
/// startup. Cache it once so diagnostics always point to the actual log.
pub fn proxy_log_path() -> Option<std::path::PathBuf> {
    PROXY_LOG_PATH
        .get_or_init(|| {
            dirs::cache_dir().map(|d| {
                let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
                let pid = std::process::id();
                d.join("claudex").join(format!("proxy-{ts}-{pid}.log"))
            })
        })
        .clone()
}

pub async fn start_proxy(config: ClaudexConfig, port_override: Option<u16>) -> Result<()> {
    start_proxy_inner(config, port_override, true).await
}

/// Start a proxy owned by the current interactive session.
///
/// Embedded proxies intentionally do not create the daemon PID file. A PID
/// file that points at the dashboard or Claude wrapper causes `proxy stop` to
/// terminate the entire terminal-owning process instead of only the proxy.
pub async fn start_embedded_proxy(config: ClaudexConfig, port_override: Option<u16>) -> Result<()> {
    start_proxy_inner(config, port_override, false).await
}

async fn start_proxy_inner(
    config: ClaudexConfig,
    port_override: Option<u16>,
    register_daemon_pid: bool,
) -> Result<()> {
    let port = port_override.unwrap_or(config.proxy_port);
    let host = config.proxy_host.clone();

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    // RAG indexing is lazy. Building here would contact an embedding provider
    // merely because the proxy started, before the user made a model request.
    let rag_index = config
        .context
        .rag
        .enabled
        .then(|| RagIndex::new(config.context.rag.clone()));

    let token_manager = crate::oauth::manager::TokenManager::new(http_client.clone());

    let setup_origin_host = if host.eq_ignore_ascii_case("localhost") {
        "localhost".to_string()
    } else if host == "::1" {
        "[::1]".to_string()
    } else {
        host.clone()
    };
    let state = Arc::new(ProxyState {
        config: Arc::new(RwLock::new(config)),
        metrics: MetricsStore::new(),
        http_client,
        health_status: Arc::new(RwLock::new(health::HealthMap::new())),
        circuit_breakers: fallback::new_circuit_breaker_map(),
        shared_context: SharedContext::new(),
        rag_index,
        token_manager,
        setup_status: Arc::new(RwLock::new(setup::SetupStatus::default())),
        account_store_lock: Arc::new(tokio::sync::Mutex::new(())),
        setup_enabled: setup::is_loopback_host(&host),
        setup_origin: format!("http://{setup_origin_host}:{port}"),
    });

    // Do not probe provider endpoints in the background. A provider is
    // contacted only for an actual proxied request or an explicit connectivity
    // test requested by the user.

    let app = Router::new()
        .route("/v1/models", get(models::list_models))
        .route(
            "/proxy/{profile}/v1/models",
            get(models::list_profile_models),
        )
        .route(
            "/proxy/{profile}/v1/messages",
            post(handler::handle_messages),
        )
        .route(
            "/health",
            get(|| async { ([("x-claudex-proxy", "1")], "ok") }),
        )
        .route("/setup", get(setup::page))
        .route("/setup/api/state", get(setup::state))
        .route("/setup/api/connect/openai", post(setup::connect_openai))
        .route(
            "/setup/api/connect/anthropic",
            post(setup::connect_anthropic),
        )
        .route("/setup/api/accounts/{id}", delete(setup::remove_account))
        .route("/setup/api/default", post(setup::set_default_model))
        .with_state(state);

    let bind_addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;

    tracing::info!("proxy listening on {bind_addr}");

    let _pid_guard = if register_daemon_pid {
        crate::process::daemon::write_pid(std::process::id())?;
        Some(PidFileGuard)
    } else {
        None
    };

    axum::serve(listener, app).await?;
    Ok(())
}

struct PidFileGuard;

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        if let Err(error) = crate::process::daemon::remove_pid() {
            tracing::warn!(%error, "failed to remove proxy PID file");
        }
    }
}

pub async fn is_proxy_reachable(host: &str, port: u16) -> bool {
    let browser_host = match host {
        "0.0.0.0" | "::" => "127.0.0.1".to_string(),
        "::1" => "[::1]".to_string(),
        other => other.to_string(),
    };
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(500))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };
    client
        .get(format!("http://{browser_host}:{port}/health"))
        .send()
        .await
        .is_ok_and(|response| {
            response.status().is_success()
                && response
                    .headers()
                    .get("x-claudex-proxy")
                    .and_then(|value| value.to_str().ok())
                    == Some("1")
        })
}

pub async fn wait_for_proxy(host: &str, port: u16, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if is_proxy_reachable(host, port).await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_log_path_is_stable_for_process_lifetime() {
        assert_eq!(proxy_log_path(), proxy_log_path());
    }

    #[tokio::test]
    async fn proxy_reachability_detects_a_listening_socket() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/health",
                    get(|| async { ([("x-claudex-proxy", "1")], "ok") }),
                ),
            )
            .await
            .unwrap();
        });
        assert!(is_proxy_reachable("127.0.0.1", port).await);
        server.abort();
    }

    #[tokio::test]
    async fn proxy_reachability_rejects_an_unrelated_listener() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/health", get(|| async { "ok" })),
            )
            .await
            .unwrap();
        });
        assert!(!is_proxy_reachable("127.0.0.1", port).await);
        server.abort();
    }

    #[tokio::test]
    async fn proxy_startup_does_not_contact_enabled_profiles() {
        let provider_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let provider_addr = provider_listener.local_addr().unwrap();

        let port_probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let proxy_port = port_probe.local_addr().unwrap().port();
        drop(port_probe);

        let dir = tempfile::tempdir().unwrap();
        let indexed_file = dir.path().join("private.md");
        std::fs::write(&indexed_file, "this content must not leave during startup").unwrap();

        let mut config = ClaudexConfig {
            proxy_host: "127.0.0.1".to_string(),
            proxy_port,
            profiles: vec![crate::config::ProfileConfig {
                name: "remote".to_string(),
                base_url: format!("http://{provider_addr}"),
                default_model: "model".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        config.context.rag.enabled = true;
        config.context.rag.profile = "remote".to_string();
        config.context.rag.model = "embedding-model".to_string();
        config.context.rag.index_paths = vec![indexed_file.to_string_lossy().into_owned()];

        let proxy = tokio::spawn(start_embedded_proxy(config, None));
        assert!(wait_for_proxy("127.0.0.1", proxy_port, Duration::from_secs(2)).await);

        let unexpected_connection =
            tokio::time::timeout(Duration::from_millis(250), provider_listener.accept()).await;
        proxy.abort();

        assert!(
            unexpected_connection.is_err(),
            "proxy startup contacted a configured provider"
        );
    }

    #[tokio::test]
    async fn empty_session_is_served_locally_with_an_empty_model_catalog() {
        let port_probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = port_probe.local_addr().unwrap().port();
        drop(port_probe);
        let mut config = ClaudexConfig {
            proxy_host: "127.0.0.1".to_string(),
            proxy_port,
            ..Default::default()
        };
        crate::accounts::apply_store_to_config(
            &mut config,
            &crate::accounts::AccountStore::default(),
        );

        let proxy = tokio::spawn(start_embedded_proxy(config, None));
        assert!(wait_for_proxy("127.0.0.1", proxy_port, Duration::from_secs(2)).await);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();

        let models: serde_json::Value = client
            .get(format!(
                "http://127.0.0.1:{proxy_port}/proxy/{}/v1/models",
                crate::accounts::SESSION_PROFILE_NAME
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(models["data"], serde_json::json!([]));

        let setup_page = client
            .get(format!("http://127.0.0.1:{proxy_port}/setup"))
            .send()
            .await
            .unwrap();
        assert!(setup_page.status().is_success());
        assert_eq!(
            setup_page
                .headers()
                .get("x-frame-options")
                .and_then(|value| value.to_str().ok()),
            Some("DENY")
        );

        let rejected = client
            .post(format!(
                "http://127.0.0.1:{proxy_port}/setup/api/connect/openai"
            ))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), reqwest::StatusCode::FORBIDDEN);

        let response: serde_json::Value = client
            .post(format!(
                "http://127.0.0.1:{proxy_port}/proxy/{}/v1/messages",
                crate::accounts::SESSION_PROFILE_NAME
            ))
            .json(&serde_json::json!({
                "model": crate::accounts::ONBOARDING_MODEL,
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        proxy.abort();

        assert_eq!(response["model"], crate::accounts::ONBOARDING_MODEL);
        assert!(response["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("/model"));
    }
}
