pub mod adapter;
pub mod context_engine;
pub mod error;
pub mod fallback;
pub mod handler;
pub mod health;
pub mod metrics;
pub mod models;
pub mod translate;
pub mod util;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use axum::routing::{get, post};
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

    // Build RAG index if enabled
    let rag_index = if config.context.rag.enabled {
        let index = RagIndex::new(config.context.rag.clone());
        if let Some((base_url, api_key, _)) = crate::context::resolve_profile_endpoint(
            &config,
            &config.context.rag.profile,
            &config.context.rag.model,
        ) {
            if let Err(e) = index.build_index(&http_client, &base_url, &api_key).await {
                tracing::warn!("failed to build RAG index: {e}");
            }
        } else {
            tracing::warn!(
                profile = %config.context.rag.profile,
                "RAG profile not found, skipping index build"
            );
        }
        Some(index)
    } else {
        None
    };

    let token_manager = crate::oauth::manager::TokenManager::new(http_client.clone());

    let state = Arc::new(ProxyState {
        config: Arc::new(RwLock::new(config)),
        metrics: MetricsStore::new(),
        http_client,
        health_status: Arc::new(RwLock::new(health::HealthMap::new())),
        circuit_breakers: fallback::new_circuit_breaker_map(),
        shared_context: SharedContext::new(),
        rag_index,
        token_manager,
    });

    health::spawn_health_checker(state.clone());

    let app = Router::new()
        .route("/v1/models", get(models::list_models))
        .route(
            "/proxy/{profile}/v1/messages",
            post(handler::handle_messages),
        )
        .route("/health", get(|| async { "ok" }))
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
    matches!(
        tokio::time::timeout(
            Duration::from_millis(500),
            tokio::net::TcpStream::connect((host, port)),
        )
        .await,
        Ok(Ok(_))
    )
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
        assert!(is_proxy_reachable("127.0.0.1", port).await);
    }
}
