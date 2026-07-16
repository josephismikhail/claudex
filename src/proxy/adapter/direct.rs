use std::collections::HashMap;

use anyhow::Result;
use reqwest::header::HeaderMap;
use reqwest::RequestBuilder;
use serde_json::Value;

use super::{ByteStream, ProviderAdapter, TranslatedRequest};
use crate::config::ProfileConfig;
use crate::oauth::{AuthType, OAuthProvider};
use crate::proxy::util::ToolNameMap;

pub struct DirectAnthropicAdapter;

impl ProviderAdapter for DirectAnthropicAdapter {
    fn endpoint_path(&self) -> &str {
        "/v1/messages"
    }

    fn translate_request(
        &self,
        body: &Value,
        _profile: &ProfileConfig,
    ) -> Result<TranslatedRequest> {
        Ok(TranslatedRequest {
            body: body.clone(),
            tool_name_map: HashMap::new(),
        })
    }

    fn apply_auth(
        &self,
        builder: RequestBuilder,
        profile: &ProfileConfig,
        inbound_headers: &HeaderMap,
        translated_body: &Value,
    ) -> RequestBuilder {
        let version = inbound_headers
            .get("anthropic-version")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("2023-06-01");
        let is_claude_oauth = profile.auth_type == AuthType::OAuth
            && profile
                .oauth_provider
                .as_ref()
                .is_some_and(|provider| provider.normalize() == OAuthProvider::Claude);
        let is_fast = crate::fast::is_anthropic_console_profile(profile)
            && translated_body.get("speed").and_then(Value::as_str) == Some("fast")
            && translated_body
                .get("model")
                .and_then(Value::as_str)
                .is_some_and(crate::fast::supports_anthropic_fast_model);
        let mut b = builder.header("anthropic-version", version);

        let beta = merged_beta_header(inbound_headers, profile, is_claude_oauth, is_fast);
        if !beta.is_empty() {
            b = b.header("anthropic-beta", beta);
        }

        if !profile.api_key.is_empty() {
            if is_claude_oauth {
                b = b.bearer_auth(&profile.api_key);
            } else {
                b = b.header("x-api-key", &profile.api_key);
            }
        }
        b
    }

    fn passthrough(&self) -> bool {
        true
    }

    fn translate_response(&self, body: &Value, _tool_name_map: &ToolNameMap) -> Result<Value> {
        Ok(body.clone())
    }

    fn translate_stream(&self, stream: ByteStream, _tool_name_map: ToolNameMap) -> ByteStream {
        stream
    }
}

fn merged_beta_header(
    inbound_headers: &HeaderMap,
    profile: &ProfileConfig,
    include_oauth: bool,
    include_fast: bool,
) -> String {
    let mut values = Vec::new();
    for header in inbound_headers.get_all("anthropic-beta") {
        let Ok(header) = header.to_str() else {
            continue;
        };
        for value in header
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if !values.iter().any(|existing| existing == value) {
                values.push(value.to_string());
            }
        }
    }
    for (name, header) in &profile.custom_headers {
        if !name.eq_ignore_ascii_case("anthropic-beta") {
            continue;
        }
        for value in header
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if !values.iter().any(|existing| existing == value) {
                values.push(value.to_string());
            }
        }
    }
    if include_oauth
        && !values
            .iter()
            .any(|value| value == crate::oauth::exchange::CLAUDE_OAUTH_BETA)
    {
        values.push(crate::oauth::exchange::CLAUDE_OAUTH_BETA.to_string());
    }
    if include_fast
        && !values
            .iter()
            .any(|value| value == crate::fast::ANTHROPIC_FAST_BETA)
    {
        values.push(crate::fast::ANTHROPIC_FAST_BETA.to_string());
    }
    values.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_oauth_uses_bearer_and_preserves_anthropic_headers() {
        let profile = ProfileConfig {
            auth_type: AuthType::OAuth,
            oauth_provider: Some(OAuthProvider::Claude),
            api_key: "oauth-access-token".to_string(),
            ..Default::default()
        };
        let inbound = HeaderMap::from_iter([
            (
                "anthropic-version".parse().unwrap(),
                "2025-01-01".parse().unwrap(),
            ),
            (
                "anthropic-beta".parse().unwrap(),
                "tools-2025-01-01".parse().unwrap(),
            ),
        ]);
        let request = DirectAnthropicAdapter
            .apply_auth(
                reqwest::Client::new().post("https://example.test"),
                &profile,
                &inbound,
                &Value::Null,
            )
            .build()
            .unwrap();

        assert_eq!(
            request.headers()["authorization"],
            "Bearer oauth-access-token"
        );
        assert!(!request.headers().contains_key("x-api-key"));
        assert_eq!(request.headers()["anthropic-version"], "2025-01-01");
        assert_eq!(
            request.headers()["anthropic-beta"],
            format!(
                "tools-2025-01-01,{}",
                crate::oauth::exchange::CLAUDE_OAUTH_BETA
            )
        );
    }

    #[test]
    fn claude_api_key_uses_x_api_key_without_oauth_beta() {
        let profile = ProfileConfig {
            auth_type: AuthType::ApiKey,
            api_key: "api-key".to_string(),
            ..Default::default()
        };
        let request = DirectAnthropicAdapter
            .apply_auth(
                reqwest::Client::new().post("https://example.test"),
                &profile,
                &HeaderMap::new(),
                &Value::Null,
            )
            .build()
            .unwrap();

        assert_eq!(request.headers()["x-api-key"], "api-key");
        assert!(!request.headers().contains_key("authorization"));
        assert!(!request.headers().contains_key("anthropic-beta"));
        assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
    }

    #[test]
    fn anthropic_fast_mode_merges_required_and_existing_beta_headers() {
        let profile = ProfileConfig {
            provider_type: crate::config::ProviderType::DirectAnthropic,
            base_url: "https://api.anthropic.com".to_string(),
            auth_type: AuthType::ApiKey,
            api_key: "api-key".to_string(),
            custom_headers: HashMap::from([(
                "Anthropic-Beta".to_string(),
                "custom-beta,tools-2025-01-01".to_string(),
            )]),
            ..Default::default()
        };
        let inbound = HeaderMap::from_iter([(
            "anthropic-beta".parse().unwrap(),
            "tools-2025-01-01".parse().unwrap(),
        )]);
        let body = serde_json::json!({
            "model": "claude-opus-4-8",
            "speed": "fast"
        });

        let request = DirectAnthropicAdapter
            .apply_auth(
                reqwest::Client::new().post("https://example.test"),
                &profile,
                &inbound,
                &body,
            )
            .build()
            .unwrap();

        assert_eq!(
            request.headers()["anthropic-beta"],
            format!(
                "tools-2025-01-01,custom-beta,{}",
                crate::fast::ANTHROPIC_FAST_BETA
            )
        );
    }
}
