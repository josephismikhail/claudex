//! Layer 2: Token Exchange
//!
//! 所有 token 交换和刷新逻辑: PKCE、Headless Device Auth、refresh_token、Copilot bearer 交换。

use anyhow::{Context, Result};

use super::source;
use super::OAuthToken;

// ── Constants ────────────────────────────────────────────────────────────

pub const CHATGPT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CHATGPT_ISSUER: &str = "https://auth.openai.com";
pub const CHATGPT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

pub const CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const CLAUDE_OAUTH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";
pub const CLAUDE_OAUTH_DEFAULT_SCOPES: &[&str] = &[
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

pub const GITHUB_COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
pub const GITHUB_COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";

// ── ChatGPT Token Refresh ────────────────────────────────────────────────

/// ChatGPT refresh_token 错误分类
#[derive(Debug)]
pub enum RefreshError {
    Expired,
    Reused,
    Revoked,
    Other(String),
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expired => write!(f, "refresh token expired, please re-login"),
            Self::Reused => write!(f, "refresh token reused (concurrent refresh detected)"),
            Self::Revoked => write!(f, "refresh token revoked, please re-login"),
            Self::Other(msg) => write!(f, "refresh failed: {msg}"),
        }
    }
}

impl std::error::Error for RefreshError {}

// ── Claude Subscription Token Refresh ───────────────────────────────────

/// Refresh the short-lived access token stored by Claude Code. Claude may
/// rotate the refresh token, so callers must persist the returned token.
pub async fn refresh_claude_token(
    client: &reqwest::Client,
    refresh_token: &str,
    scopes: Option<&[String]>,
) -> Result<OAuthToken> {
    refresh_claude_token_at(client, CLAUDE_OAUTH_TOKEN_URL, refresh_token, scopes).await
}

async fn refresh_claude_token_at(
    client: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
    scopes: Option<&[String]>,
) -> Result<OAuthToken> {
    let scope = scopes
        .filter(|scopes| !scopes.is_empty())
        .map(|scopes| scopes.join(" "))
        .unwrap_or_else(|| CLAUDE_OAUTH_DEFAULT_SCOPES.join(" "));
    let response = client
        .post(token_url)
        .header("Content-Type", "application/json")
        .header("anthropic-beta", CLAUDE_OAUTH_BETA)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLAUDE_OAUTH_CLIENT_ID,
            "scope": scope,
        }))
        .send()
        .await
        .context("Claude OAuth token refresh request failed")?;

    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .context("invalid JSON from Claude OAuth token refresh")?;
    if !status.is_success() {
        let error = body
            .get("error_description")
            .or_else(|| body.get("error"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown OAuth error");
        anyhow::bail!("Claude OAuth token refresh failed (HTTP {status}): {error}");
    }

    let mut token = OAuthToken::from_token_response(&body)
        .context("failed to parse Claude OAuth refresh response")?;
    if token.refresh_token.is_none() {
        token.refresh_token = Some(refresh_token.to_string());
    }
    if token.scopes.is_none() {
        token.scopes = scopes.map(ToOwned::to_owned);
    }
    Ok(token)
}

/// 使用 refresh_token 刷新 ChatGPT token
pub async fn refresh_chatgpt_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<OAuthToken> {
    let resp = client
        .post(CHATGPT_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}",
            encode(refresh_token),
            CHATGPT_CLIENT_ID
        ))
        .send()
        .await
        .context("ChatGPT token refresh request failed")?;

    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .context("invalid JSON from ChatGPT token refresh")?;

    if !status.is_success() {
        let error = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let err = match error {
            "invalid_grant" => {
                let desc = body
                    .get("error_description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if desc.contains("expired") {
                    RefreshError::Expired
                } else if desc.contains("reused") {
                    RefreshError::Reused
                } else if desc.contains("revoked") || desc.contains("invalidated") {
                    RefreshError::Revoked
                } else {
                    RefreshError::Other(format!("{error}: {desc}"))
                }
            }
            _ => RefreshError::Other(format!("HTTP {status}: {error}")),
        };
        return Err(err.into());
    }

    let mut token = OAuthToken::from_token_response(&body)
        .context("failed to parse ChatGPT refresh response")?;

    // 保留原 refresh_token 如果响应没返回新的
    if token.refresh_token.is_none() {
        token.refresh_token = Some(refresh_token.to_string());
    }

    // 从 JWT 提取 expires_at
    if token.expires_at.is_none() {
        token.expires_at = source::extract_jwt_exp(&token.access_token);
    }

    // 提取 account_id
    let account_id = source::extract_account_id(&body);
    let mut extra = serde_json::json!({"auth_mode": "chatgpt"});
    if let Some(ref aid) = account_id {
        extra["account_id"] = serde_json::json!(aid);
    }
    token.extra = Some(extra);

    tracing::info!("ChatGPT token refreshed successfully");
    Ok(token)
}

// ── ChatGPT Browser PKCE ─────────────────────────────────────────────────

/// 构造 ChatGPT PKCE authorize URL
pub fn build_chatgpt_authorize_url(
    redirect_port: u16,
    pkce: &super::server::PkceChallenge,
    state: &str,
) -> String {
    let params = [
        ("response_type", "code".to_string()),
        ("client_id", CHATGPT_CLIENT_ID.to_string()),
        (
            "redirect_uri",
            format!("http://localhost:{redirect_port}/auth/callback"),
        ),
        ("scope", "openid profile email offline_access".to_string()),
        ("code_challenge", pkce.code_challenge.clone()),
        ("code_challenge_method", "S256".to_string()),
        ("id_token_add_organizations", "true".to_string()),
        ("codex_cli_simplified_flow", "true".to_string()),
        ("state", state.to_string()),
    ];
    let qs = params
        .iter()
        .map(|(k, v)| format!("{k}={}", encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{CHATGPT_ISSUER}/oauth/authorize?{qs}")
}

/// 用 authorization_code + code_verifier 换取 ChatGPT tokens
pub async fn exchange_chatgpt_code(
    client: &reqwest::Client,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<OAuthToken> {
    let resp = client
        .post(CHATGPT_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
            encode(code),
            encode(redirect_uri),
            CHATGPT_CLIENT_ID,
            encode(code_verifier),
        ))
        .send()
        .await
        .context("ChatGPT code exchange failed")?;

    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .context("invalid JSON from ChatGPT code exchange")?;

    if !status.is_success() {
        tracing::error!(
            status = %status,
            body = %body,
            "ChatGPT code exchange failed"
        );
        let error = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let desc = body
            .get("error_description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        anyhow::bail!("ChatGPT code exchange failed (HTTP {status}): {error} - {desc}");
    }

    let mut token = OAuthToken::from_token_response(&body)
        .context("failed to parse ChatGPT code exchange response")?;

    if token.expires_at.is_none() {
        token.expires_at = source::extract_jwt_exp(&token.access_token);
    }

    let account_id = source::extract_account_id(&body);
    let mut extra = serde_json::json!({"auth_mode": "chatgpt"});
    if let Some(ref aid) = account_id {
        extra["account_id"] = serde_json::json!(aid);
    }
    token.extra = Some(extra);

    Ok(token)
}

// ── ChatGPT Headless Device Auth ─────────────────────────────────────────

/// Device Auth 初始响应
#[derive(Debug)]
pub struct DeviceAuthResponse {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval: u64,
}

/// 请求 ChatGPT device auth code
pub async fn chatgpt_device_auth_request(client: &reqwest::Client) -> Result<DeviceAuthResponse> {
    let resp = client
        .post(format!("{CHATGPT_ISSUER}/api/accounts/deviceauth/usercode"))
        .json(&serde_json::json!({"client_id": CHATGPT_CLIENT_ID}))
        .send()
        .await
        .context("ChatGPT device auth request failed")?;

    let body: serde_json::Value = resp
        .json()
        .await
        .context("invalid JSON from ChatGPT device auth")?;

    Ok(DeviceAuthResponse {
        device_auth_id: body
            .get("device_auth_id")
            .and_then(|v| v.as_str())
            .context("missing device_auth_id")?
            .to_string(),
        user_code: body
            .get("user_code")
            .and_then(|v| v.as_str())
            .context("missing user_code")?
            .to_string(),
        interval: body.get("interval").and_then(|v| v.as_u64()).unwrap_or(5),
    })
}

/// 轮询 ChatGPT device auth token
/// Codex CLI 协议: HTTP 200 = 成功 (返回 authorization_code + code_verifier)
///                  HTTP 403/404 = 等待中
///                  其他 = 错误
pub async fn chatgpt_device_auth_poll(
    client: &reqwest::Client,
    device_auth_id: &str,
    user_code: &str,
) -> Result<OAuthToken> {
    let interval = std::time::Duration::from_secs(5);
    let max_wait = std::time::Duration::from_secs(15 * 60);
    let start = std::time::Instant::now();

    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\nInterrupted.");
                std::process::exit(130);
            }
        }

        let resp = client
            .post(format!("{CHATGPT_ISSUER}/api/accounts/deviceauth/token"))
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "device_auth_id": device_auth_id,
                "user_code": user_code,
            }))
            .send()
            .await
            .context("ChatGPT device auth poll failed")?;

        let http_status = resp.status();

        if http_status.is_success() {
            // 200: 用户已授权，解析 authorization_code + code_verifier
            let body: serde_json::Value = resp
                .json()
                .await
                .context("invalid JSON from device auth success response")?;

            let auth_code = body
                .get("authorization_code")
                .and_then(|v| v.as_str())
                .context("missing authorization_code in device auth response")?;
            let code_verifier = body
                .get("code_verifier")
                .and_then(|v| v.as_str())
                .context("missing code_verifier in device auth response")?;

            let redirect_uri = format!("{CHATGPT_ISSUER}/deviceauth/callback");
            let token =
                exchange_chatgpt_code(client, auth_code, &redirect_uri, code_verifier).await?;

            return Ok(token);
        }

        if http_status == reqwest::StatusCode::FORBIDDEN
            || http_status == reqwest::StatusCode::NOT_FOUND
        {
            // 403/404: 用户尚未授权，继续等待
            if start.elapsed() >= max_wait {
                anyhow::bail!("device auth timed out after 15 minutes");
            }
            continue;
        }

        // 其他错误
        let err_text = resp.text().await.unwrap_or_default();
        anyhow::bail!("device auth poll failed (HTTP {http_status}): {err_text}");
    }
}

// ── GitHub Copilot Token Exchange ────────────────────────────────────────

/// Copilot bearer token（短生命周期，约 30 分钟）
#[derive(Debug, Clone)]
pub struct CopilotBearerToken {
    pub token: String,
    pub expires_at: i64, // Unix seconds
}

/// Copilot 伪装 headers
fn copilot_headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("User-Agent", "GitHubCopilotChat/0.32.4"),
        ("Editor-Version", "vscode/1.105.1"),
        ("Editor-Plugin-Version", "copilot-chat/0.32.4"),
        ("Copilot-Integration-Id", "vscode-chat"),
    ]
}

/// 用 GitHub OAuth token 交换 Copilot bearer token
pub async fn exchange_github_for_copilot(
    client: &reqwest::Client,
    github_token: &str,
) -> Result<CopilotBearerToken> {
    let mut req = client
        .get(GITHUB_COPILOT_TOKEN_URL)
        .header("Authorization", format!("token {github_token}"));

    for (k, v) in copilot_headers() {
        req = req.header(k, v);
    }

    let resp = req.send().await.context("Copilot token exchange failed")?;
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .context("invalid JSON from Copilot token exchange")?;

    if !status.is_success() {
        let msg = body
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("Copilot token exchange failed (HTTP {status}): {msg}");
    }

    let token = body
        .get("token")
        .and_then(|v| v.as_str())
        .context("missing 'token' in Copilot response")?
        .to_string();

    let expires_at = body
        .get("expires_at")
        .and_then(|v| v.as_i64())
        .context("missing 'expires_at' in Copilot response")?;

    Ok(CopilotBearerToken { token, expires_at })
}

/// 返回 Copilot 请求所需的额外 headers
pub fn copilot_extra_headers() -> Vec<(&'static str, &'static str)> {
    let mut headers = copilot_headers();
    headers.push(("Openai-Intent", "conversation-edits"));
    headers
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn encode(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refresh_claude_token_uses_official_oauth_contract() {
        use std::sync::{Arc, Mutex};

        use axum::extract::State;
        use axum::http::HeaderMap;
        use axum::routing::post;
        use axum::{Json, Router};

        type Capture = Arc<Mutex<Option<(HeaderMap, serde_json::Value)>>>;
        let capture: Capture = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route(
                "/v1/oauth/token",
                post(
                    |State(capture): State<Capture>,
                     headers: HeaderMap,
                     Json(body): Json<serde_json::Value>| async move {
                        *capture.lock().unwrap() = Some((headers, body));
                        Json(serde_json::json!({
                            "access_token": "new-access",
                            "refresh_token": "new-refresh",
                            "expires_in": 3600,
                            "scope": ["user:inference"]
                        }))
                    },
                ),
            )
            .with_state(capture.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let token = refresh_claude_token_at(
            &reqwest::Client::new(),
            &format!("http://{address}/v1/oauth/token"),
            "old-refresh",
            Some(&["user:inference".to_string()]),
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(token.access_token, "new-access");
        assert_eq!(token.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(token.scopes, Some(vec!["user:inference".to_string()]));
        let (headers, body) = capture.lock().unwrap().take().unwrap();
        assert_eq!(headers["anthropic-beta"], CLAUDE_OAUTH_BETA);
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "old-refresh");
        assert_eq!(body["client_id"], CLAUDE_OAUTH_CLIENT_ID);
        assert_eq!(body["scope"], "user:inference");
    }

    // ── encode ────────────────────────────────────────────────

    #[test]
    fn test_encode_spaces_as_percent20() {
        // urlencoding::encode 编码空格为 %20（与 Codex CLI 一致）
        assert_eq!(encode("openid profile"), "openid%20profile");
        // 不应该编码为 +
        assert!(!encode("a b").contains('+'));
    }

    #[test]
    fn test_encode_special_chars() {
        assert_eq!(
            encode("http://localhost:1455/auth/callback"),
            "http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"
        );
    }

    #[test]
    fn test_encode_passthrough_safe_chars() {
        assert_eq!(encode("abc-123_test.value~ok"), "abc-123_test.value~ok");
    }

    // ── authorize URL ────────────────────────────────────────

    #[test]
    fn test_build_chatgpt_authorize_url() {
        let pkce = super::super::server::PkceChallenge::generate();
        let url = build_chatgpt_authorize_url(1455, &pkce, "test-state");
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
        assert!(url.contains("state=test-state"));
    }

    #[test]
    fn test_authorize_url_redirect_uri_format() {
        let pkce = super::super::server::PkceChallenge::generate();
        let url = build_chatgpt_authorize_url(1455, &pkce, "s");
        // redirect_uri 应该 percent-encode 为 http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
    }

    #[test]
    fn test_authorize_url_scope_uses_percent20() {
        let pkce = super::super::server::PkceChallenge::generate();
        let url = build_chatgpt_authorize_url(1455, &pkce, "s");
        // scope 中空格应编码为 %20
        assert!(url.contains("scope=openid%20profile%20email%20offline_access"));
        // 不应含有 + 编码
        assert!(!url.contains("openid+profile"));
    }

    #[test]
    fn test_authorize_url_dynamic_port() {
        let pkce = super::super::server::PkceChallenge::generate();
        let url = build_chatgpt_authorize_url(53020, &pkce, "s");
        assert!(url.contains("localhost%3A53020"));
    }

    // ── device auth redirect_uri ─────────────────────────────

    #[test]
    fn test_device_auth_redirect_uri_no_api_accounts() {
        // device code 的 redirect_uri 应该是 {ISSUER}/deviceauth/callback
        // 不是 {ISSUER}/api/accounts/deviceauth/callback
        let expected = format!("{CHATGPT_ISSUER}/deviceauth/callback");
        assert_eq!(expected, "https://auth.openai.com/deviceauth/callback");
        assert!(!expected.contains("/api/accounts/"));
    }

    // ── constants ────────────────────────────────────────────

    #[test]
    fn test_chatgpt_client_id_matches_codex_cli() {
        assert_eq!(CHATGPT_CLIENT_ID, "app_EMoamEEZ73f0CkXaXp7hrann");
    }

    #[test]
    fn test_github_copilot_client_id_is_official() {
        assert_eq!(GITHUB_COPILOT_CLIENT_ID, "Iv1.b507a08c87ecfe98");
    }

    // ── copilot headers ──────────────────────────────────────

    #[test]
    fn test_copilot_headers() {
        let headers = copilot_headers();
        assert!(headers.iter().any(|(k, _)| *k == "User-Agent"));
        assert!(headers.iter().any(|(k, _)| *k == "Editor-Version"));
    }

    #[test]
    fn test_copilot_extra_headers_include_intent() {
        let headers = copilot_extra_headers();
        assert!(headers
            .iter()
            .any(|(k, v)| *k == "Openai-Intent" && *v == "conversation-edits"));
    }

    // ── refresh error ────────────────────────────────────────

    #[test]
    fn test_refresh_error_display() {
        assert!(RefreshError::Expired.to_string().contains("expired"));
        assert!(RefreshError::Reused.to_string().contains("reused"));
        assert!(RefreshError::Revoked.to_string().contains("revoked"));
        assert!(RefreshError::Other("test".to_string())
            .to_string()
            .contains("test"));
    }
}
