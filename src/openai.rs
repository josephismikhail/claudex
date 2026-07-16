use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::accounts::{AccountProvider, OPENAI_PROFILE_NAME};
use crate::config::ClaudexConfig;

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

pub async fn print_subscription_usage(config: &mut ClaudexConfig) -> Result<()> {
    let store = crate::accounts::apply_to_config(config)?;
    if !store.has_provider(AccountProvider::Openai) {
        anyhow::bail!("/usage is available only when an OpenAI subscription is connected");
    }

    let profile = config
        .find_profile(OPENAI_PROFILE_NAME)
        .cloned()
        .context("connected OpenAI account profile is unavailable")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!("claudex/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let manager = crate::oauth::manager::TokenManager::new(client.clone());
    let mut token = manager.get_token(&profile).await?;
    let mut account_id = subscription_account_id(&token)?;

    let mut response = request_usage(&client, &token.access_token, &account_id).await?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        token = manager.invalidate_and_retry(&profile).await?;
        account_id = subscription_account_id(&token)?;
        response = request_usage(&client, &token.access_token, &account_id).await?;
    }

    let status = response.status();
    let bytes = response.bytes().await?;
    if !status.is_success() {
        let detail = String::from_utf8_lossy(&bytes);
        let detail: String = detail.chars().take(500).collect();
        anyhow::bail!("OpenAI usage request failed (HTTP {status}): {detail}");
    }
    let usage: UsageSnapshot = serde_json::from_slice(&bytes)
        .context("OpenAI returned an invalid subscription usage response")?;
    println!("{}", format_usage(&usage));
    Ok(())
}

fn subscription_account_id(token: &crate::oauth::OAuthToken) -> Result<String> {
    token
        .extra
        .as_ref()
        .and_then(|extra| extra.get("account_id"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            crate::oauth::source::extract_jwt_claim(
                &token.access_token,
                "https://api.openai.com/auth",
                "chatgpt_account_id",
            )
        })
        .context("the OpenAI credential is not a ChatGPT subscription login")
}

async fn request_usage(
    client: &reqwest::Client,
    access_token: &str,
    account_id: &str,
) -> Result<reqwest::Response> {
    request_usage_at(client, USAGE_URL, access_token, account_id).await
}

async fn request_usage_at(
    client: &reqwest::Client,
    url: &str,
    access_token: &str,
    account_id: &str,
) -> Result<reqwest::Response> {
    client
        .get(url)
        .bearer_auth(access_token)
        .header("ChatGPT-Account-Id", account_id)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .context("failed to fetch OpenAI subscription usage")
}

#[derive(Debug, Deserialize)]
struct UsageSnapshot {
    #[serde(default)]
    plan_type: String,
    rate_limit: Option<RateLimit>,
    credits: Option<Credits>,
    additional_rate_limits: Option<Vec<AdditionalRateLimit>>,
}

#[derive(Debug, Deserialize)]
struct RateLimit {
    #[serde(default)]
    allowed: bool,
    #[serde(default)]
    limit_reached: bool,
    primary_window: Option<UsageWindow>,
    secondary_window: Option<UsageWindow>,
}

#[derive(Debug, Deserialize)]
struct UsageWindow {
    used_percent: f64,
    #[serde(default)]
    limit_window_seconds: i64,
    #[serde(default)]
    reset_after_seconds: i64,
    #[serde(default)]
    reset_at: i64,
}

#[derive(Debug, Deserialize)]
struct Credits {
    #[serde(default)]
    has_credits: bool,
    #[serde(default)]
    unlimited: bool,
    balance: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdditionalRateLimit {
    #[serde(default)]
    limit_name: String,
    #[serde(default)]
    metered_feature: String,
    rate_limit: Option<RateLimit>,
}

fn format_usage(usage: &UsageSnapshot) -> String {
    let plan = if usage.plan_type.is_empty() {
        "OpenAI subscription".to_string()
    } else {
        format!("OpenAI {} subscription", title_case(&usage.plan_type))
    };
    let mut lines = vec![format!("{plan} usage (live)")];

    if let Some(rate_limit) = &usage.rate_limit {
        if let Some(window) = &rate_limit.primary_window {
            lines.push(format_window(window, "Primary"));
        }
        if let Some(window) = &rate_limit.secondary_window {
            lines.push(format_window(window, "Secondary"));
        }
        if rate_limit.limit_reached || !rate_limit.allowed {
            lines.push("  Status: limit reached".to_string());
        }
    }

    for additional in usage.additional_rate_limits.as_deref().unwrap_or_default() {
        let Some(rate_limit) = &additional.rate_limit else {
            continue;
        };
        let label = if !additional.limit_name.is_empty() {
            title_case(&additional.limit_name)
        } else {
            title_case(&additional.metered_feature)
        };
        if let Some(window) = &rate_limit.primary_window {
            lines.push(format_window(window, &label));
        }
    }

    if let Some(credits) = &usage.credits {
        if credits.unlimited {
            lines.push("  Credits: unlimited".to_string());
        } else if let Some(balance) = &credits.balance {
            lines.push(format!("  Credit balance: {balance}"));
        } else if credits.has_credits {
            lines.push("  Credits: available".to_string());
        }
    }

    if lines.len() == 1 {
        lines.push("  OpenAI did not return a usage window for this account.".to_string());
    }
    lines.join("\n")
}

fn format_window(window: &UsageWindow, fallback_label: &str) -> String {
    let label = window_label(window.limit_window_seconds, fallback_label);
    let remaining = (100.0 - window.used_percent).clamp(0.0, 100.0);
    let reset_seconds = if window.reset_after_seconds > 0 {
        window.reset_after_seconds
    } else if window.reset_at > 0 {
        (window.reset_at - chrono::Utc::now().timestamp()).max(0)
    } else {
        0
    };
    let reset = if reset_seconds > 0 {
        format!("; resets in {}", format_duration(reset_seconds))
    } else {
        String::new()
    };
    format!("  {label}: {}% left{reset}", format_percent(remaining))
}

fn window_label(seconds: i64, fallback: &str) -> String {
    if seconds > 0 && seconds % 86_400 == 0 {
        let days = seconds / 86_400;
        format!("{days}-day window")
    } else if seconds > 0 && seconds % 3_600 == 0 {
        let hours = seconds / 3_600;
        format!("{hours}-hour window")
    } else if seconds > 0 && seconds % 60 == 0 {
        let minutes = seconds / 60;
        format!("{minutes}-minute window")
    } else {
        fallback.to_string()
    }
}

fn format_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

fn format_percent(value: f64) -> String {
    if value.fract().abs() < f64::EPSILON {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn title_case(value: &str) -> String {
    value
        .split(['_', '-'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_remaining_subscription_windows() {
        let usage: UsageSnapshot = serde_json::from_value(serde_json::json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 42.5,
                    "limit_window_seconds": 18000,
                    "reset_after_seconds": 7200,
                    "reset_at": 0
                },
                "secondary_window": {
                    "used_percent": 75,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 172800,
                    "reset_at": 0
                }
            },
            "credits": {"has_credits": true, "unlimited": false, "balance": "12.50"}
        }))
        .unwrap();

        let rendered = format_usage(&usage);
        assert!(rendered.contains("OpenAI Pro subscription usage (live)"));
        assert!(rendered.contains("5-hour window: 57.5% left; resets in 2h 0m"));
        assert!(rendered.contains("7-day window: 25% left; resets in 2d 0h"));
        assert!(rendered.contains("Credit balance: 12.50"));
    }

    #[tokio::test]
    async fn usage_request_uses_subscription_auth_headers() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/wham/usage"))
            .and(header("authorization", "Bearer access-token"))
            .and(header("chatgpt-account-id", "account-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "plan_type": "plus",
                "rate_limit": null,
                "additional_rate_limits": null
            })))
            .expect(1)
            .mount(&server)
            .await;

        let response = request_usage_at(
            &reqwest::Client::new(),
            &format!("{}/wham/usage", server.uri()),
            "access-token",
            "account-123",
        )
        .await
        .unwrap();
        assert!(response.status().is_success());
        let usage: UsageSnapshot = response.json().await.unwrap();
        assert_eq!(usage.plan_type, "plus");
        assert!(usage.additional_rate_limits.is_none());
    }
}
