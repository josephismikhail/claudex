use std::path::PathBuf;

use anyhow::{Context, Result};
use http::HeaderMap;
use serde::{Deserialize, Serialize};

use crate::accounts::{AccountProvider, AccountStore};
use crate::config::{ClaudexConfig, ProfileConfig, ProviderType};
use crate::oauth::{AuthType, OAuthProvider};

pub const FAST_SESSION_ENV: &str = "CLAUDEX_FAST_SESSION";
pub const FAST_SESSION_HEADER: &str = "x-claudex-fast-session";
pub const ANTHROPIC_FAST_BETA: &str = "fast-mode-2026-02-01";

const FAST_STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FastAvailability {
    pub openai: bool,
    pub anthropic: bool,
}

impl FastAvailability {
    pub fn from_store(store: &AccountStore) -> Self {
        let openai = store.has_provider(AccountProvider::Openai);
        let anthropic = store.accounts.iter().any(|account| {
            account.provider == AccountProvider::Anthropic
                && account
                    .models
                    .iter()
                    .any(|model| supports_anthropic_fast_model(model))
        });
        Self { openai, anthropic }
    }

    pub fn any(self) -> bool {
        self.openai || self.anthropic
    }
}

pub fn is_openai_subscription_profile(profile: &ProfileConfig) -> bool {
    profile.auth_type == AuthType::OAuth
        && profile.oauth_provider.as_ref().is_some_and(|provider| {
            matches!(
                provider.normalize(),
                OAuthProvider::Chatgpt | OAuthProvider::Openai
            )
        })
}

pub fn is_anthropic_console_profile(profile: &ProfileConfig) -> bool {
    profile.provider_type == ProviderType::DirectAnthropic
        && profile.auth_type == AuthType::ApiKey
        && profile
            .base_url
            .trim_end_matches('/')
            .eq_ignore_ascii_case("https://api.anthropic.com")
}

/// Anthropic rejects `speed: "fast"` on unsupported models. Opus 4.7 is
/// intentionally excluded because Anthropic has announced its removal on
/// July 24, 2026; Opus 4.8 is the durable supported route.
pub fn supports_anthropic_fast_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model == "claude-opus-4-8" || model.starts_with("claude-opus-4-8-")
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct FastState {
    version: u32,
    enabled: bool,
}

/// Per-Claude-process state used by the managed `/fast` skill and the local
/// gateway. The random ID is sent to the loopback proxy, never upstream.
pub struct FastSession {
    id: String,
    path: PathBuf,
}

impl FastSession {
    pub fn create() -> Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        let path = fast_state_path(&id)?;
        write_fast_state(&path, false)?;
        Ok(Self { id, path })
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for FastSession {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(path = %self.path.display(), %error, "failed to clean fast-mode session state");
            }
        }
    }
}

pub fn run_fast_command(action: Option<crate::cli::FastAction>) -> Result<()> {
    let store = AccountStore::load()?;
    let available = FastAvailability::from_store(&store);
    if !available.any() {
        anyhow::bail!(
            "/fast requires an OpenAI subscription or an Anthropic Console account with Claude Opus 4.8"
        );
    }

    let id = std::env::var(FAST_SESSION_ENV)
        .context("/fast is available only inside a running Claudex session")?;
    let path = fast_state_path(&id)?;
    if !path.exists() {
        anyhow::bail!("this Claudex session is no longer active");
    }

    let current = read_fast_state_path(&path).unwrap_or(false);
    let enabled = match action {
        Some(crate::cli::FastAction::On) => true,
        Some(crate::cli::FastAction::Off) => false,
        Some(crate::cli::FastAction::Status) => current,
        None => !current,
    };
    if !matches!(action, Some(crate::cli::FastAction::Status)) {
        write_fast_state(&path, enabled)?;
    }

    if enabled {
        println!("Fast mode ON - provider-aware for this Claudex session.");
        if available.openai {
            println!(
                "  OpenAI routes: priority access (about 1.5x faster; accelerated subscription-credit use)."
            );
        }
        if available.anthropic {
            println!(
                "  Anthropic Opus 4.8 routes: fast inference (up to 2.5x output speed; premium research-preview access required)."
            );
        }
        println!("  Other provider and model routes remain at standard speed.");
    } else {
        println!("Fast mode OFF - all provider routes use standard speed.");
    }
    Ok(())
}

/// Read the state selected by a request from Claude Code. Invalid IDs and
/// missing files are treated as disabled, preventing arbitrary filesystem
/// reads through the loopback HTTP endpoint.
pub fn fast_enabled(headers: &HeaderMap) -> bool {
    let Some(id) = headers
        .get(FAST_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Ok(path) = fast_state_path(id) else {
        return false;
    };
    read_fast_state_path(&path).unwrap_or(false)
}

fn fast_state_path(id: &str) -> Result<PathBuf> {
    let parsed = uuid::Uuid::parse_str(id).context("invalid Claudex fast-mode session ID")?;
    Ok(ClaudexConfig::config_dir()?
        .join("sessions")
        .join(format!("{}.json", parsed.hyphenated())))
}

fn write_fast_state(path: &std::path::Path, enabled: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let state = FastState {
        version: FAST_STATE_VERSION,
        enabled,
    };
    let bytes = serde_json::to_vec(&state).context("failed to serialize fast-mode state")?;
    crate::config::write_file_atomically(path, &bytes)
        .with_context(|| format!("failed to save fast-mode state at {}", path.display()))
}

fn read_fast_state_path(path: &std::path::Path) -> Result<bool> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read fast-mode state at {}", path.display()))?;
    let state: FastState = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid fast-mode state at {}", path.display()))?;
    if state.version > FAST_STATE_VERSION {
        anyhow::bail!("fast-mode state is newer than this Claudex version");
    }
    Ok(state.enabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_tracks_each_supported_provider() {
        let mut store = AccountStore::default();
        assert!(!FastAvailability::from_store(&store).any());

        store.upsert_with_models(
            AccountProvider::Anthropic,
            vec!["claude-sonnet-5".to_string()],
        );
        assert!(!FastAvailability::from_store(&store).any());

        store.upsert_with_models(
            AccountProvider::Anthropic,
            vec!["claude-opus-4-8".to_string()],
        );
        let anthropic = FastAvailability::from_store(&store);
        assert!(!anthropic.openai);
        assert!(anthropic.anthropic);

        store.upsert(AccountProvider::Openai);
        let both = FastAvailability::from_store(&store);
        assert!(both.openai);
        assert!(both.anthropic);
    }

    #[test]
    fn only_current_durable_anthropic_fast_models_are_accepted() {
        assert!(supports_anthropic_fast_model("claude-opus-4-8"));
        assert!(supports_anthropic_fast_model("claude-opus-4-8-20260701"));
        assert!(!supports_anthropic_fast_model("claude-opus-4-7"));
        assert!(!supports_anthropic_fast_model("claude-sonnet-5"));
    }

    #[test]
    fn rejects_fast_state_path_traversal() {
        assert!(fast_state_path("../../accounts.json").is_err());
    }

    #[test]
    fn request_header_reads_only_its_session_state() {
        let session = FastSession::create().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            FAST_SESSION_HEADER,
            session.id().parse().expect("valid header value"),
        );
        assert!(!fast_enabled(&headers));

        write_fast_state(&session.path, true).unwrap();
        assert!(fast_enabled(&headers));
    }
}
