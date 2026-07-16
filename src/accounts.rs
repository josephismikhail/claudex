//! Persistent provider accounts and the runtime-only session profile.
//!
//! `accounts.json` contains labels, provider kinds, and model IDs only. Secrets
//! live in the operating system credential store and are loaded only when a
//! user actually sends a request to that provider.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{ClaudexConfig, ProfileConfig, ProfileModels, ProviderType};
use crate::oauth::{AuthType, OAuthProvider};

pub const SESSION_PROFILE_NAME: &str = "__claudex_session";
pub const ONBOARDING_MODEL: &str = "claudex-onboarding";
pub const OPENAI_PROFILE_NAME: &str = "__claudex_account_openai";
pub const ANTHROPIC_PROFILE_NAME: &str = "__claudex_account_anthropic";

const STORE_VERSION: u32 = 1;
const OPENAI_ACCOUNT_ID: &str = "openai";
const ANTHROPIC_ACCOUNT_ID: &str = "anthropic";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountProvider {
    Openai,
    Anthropic,
}

impl AccountProvider {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Openai => "OpenAI",
            Self::Anthropic => "Anthropic API",
        }
    }

    fn account_id(&self) -> &'static str {
        match self {
            Self::Openai => OPENAI_ACCOUNT_ID,
            Self::Anthropic => ANTHROPIC_ACCOUNT_ID,
        }
    }

    fn credential_key(&self) -> &'static str {
        match self {
            Self::Openai => "managed-openai",
            Self::Anthropic => "managed-anthropic-api-key",
        }
    }

    pub fn default_models(&self) -> Vec<String> {
        match self {
            Self::Openai => vec!["gpt-5.6".to_string()],
            Self::Anthropic => vec![
                "claude-opus-4-8".to_string(),
                "claude-sonnet-5".to_string(),
                "claude-haiku-4-5".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountRecord {
    pub id: String,
    pub provider: AccountProvider,
    pub label: String,
    pub credential_key: String,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountStore {
    #[serde(default = "store_version")]
    pub version: u32,
    #[serde(default)]
    pub accounts: Vec<AccountRecord>,
    #[serde(default)]
    pub default_model: Option<String>,
}

impl Default for AccountStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            accounts: Vec::new(),
            default_model: None,
        }
    }
}

fn store_version() -> u32 {
    STORE_VERSION
}

impl AccountStore {
    pub fn path() -> Result<PathBuf> {
        Ok(ClaudexConfig::config_dir()?.join("accounts.json"))
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path()?)
    }

    fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(path)
            .with_context(|| format!("failed to read account store at {}", path.display()))?;
        let store: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid account store at {}", path.display()))?;
        if store.version > STORE_VERSION {
            anyhow::bail!(
                "account store version {} is newer than this Claudex supports ({STORE_VERSION})",
                store.version
            );
        }
        Ok(store)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self).context("failed to serialize account store")?;
        crate::config::write_file_atomically(path, &bytes)
            .with_context(|| format!("failed to save account store at {}", path.display()))
    }

    pub fn upsert(&mut self, provider: AccountProvider) -> AccountRecord {
        let models = provider.default_models();
        self.upsert_with_models(provider, models)
    }

    pub fn upsert_with_models(
        &mut self,
        provider: AccountProvider,
        models: Vec<String>,
    ) -> AccountRecord {
        let record = AccountRecord {
            id: provider.account_id().to_string(),
            label: provider.label().to_string(),
            credential_key: provider.credential_key().to_string(),
            models,
            provider,
        };
        if let Some(existing) = self.accounts.iter_mut().find(|item| item.id == record.id) {
            *existing = record.clone();
        } else {
            self.accounts.push(record.clone());
        }
        if self
            .default_model
            .as_ref()
            .is_none_or(|model| !self.has_model(model))
        {
            self.default_model = record.models.first().cloned();
        }
        record
    }

    pub fn remove(&mut self, id: &str) -> Option<AccountRecord> {
        let index = self.accounts.iter().position(|item| item.id == id)?;
        let removed = self.accounts.remove(index);
        if self
            .default_model
            .as_ref()
            .is_some_and(|model| removed.models.contains(model))
        {
            self.default_model = self
                .accounts
                .iter()
                .flat_map(|account| account.models.iter())
                .next()
                .cloned();
        }
        Some(removed)
    }

    pub fn set_default_model(&mut self, model: &str) -> Result<()> {
        if !self.has_model(model) {
            anyhow::bail!("model '{model}' is not connected");
        }
        self.default_model = Some(model.to_string());
        Ok(())
    }

    pub fn has_model(&self, model: &str) -> bool {
        self.accounts
            .iter()
            .any(|account| account.models.iter().any(|candidate| candidate == model))
    }

    pub fn has_provider(&self, provider: AccountProvider) -> bool {
        self.accounts
            .iter()
            .any(|account| account.provider == provider)
    }
}

/// Rebuild the account-backed portion of a loaded config without persisting
/// any synthesized profiles to the legacy config file.
pub fn apply_to_config(config: &mut ClaudexConfig) -> Result<AccountStore> {
    let store = AccountStore::load()?;
    apply_store_to_config(config, &store);
    Ok(store)
}

pub fn apply_store_to_config(config: &mut ClaudexConfig, store: &AccountStore) {
    config.profiles.retain(|profile| !profile.runtime_managed);

    let legacy_profiles = config.profiles.clone();
    let mut root = ProfileConfig {
        name: SESSION_PROFILE_NAME.to_string(),
        provider_type: ProviderType::DirectAnthropic,
        base_url: "http://127.0.0.1/claudex-onboarding".to_string(),
        default_model: ONBOARDING_MODEL.to_string(),
        runtime_managed: true,
        ..Default::default()
    };

    // Existing explicit profiles remain usable after upgrading, but the bare
    // command no longer asks the user to select one.
    for profile in legacy_profiles.iter().filter(|profile| profile.enabled) {
        let mut model_ids = vec![profile.default_model.clone()];
        model_ids.extend(
            [
                profile.models.haiku.clone(),
                profile.models.sonnet.clone(),
                profile.models.opus.clone(),
            ]
            .into_iter()
            .flatten(),
        );
        model_ids.extend(profile.model_routes.keys().cloned());
        for model in model_ids.into_iter().filter(|model| !model.is_empty()) {
            let target = profile
                .model_routes
                .get(&model)
                .cloned()
                .unwrap_or_else(|| profile.name.clone());
            root.model_routes.entry(model).or_insert(target);
        }
    }

    for account in &store.accounts {
        let profile = account_profile(account);
        for model in &account.models {
            root.model_routes
                .insert(model.clone(), profile.name.clone());
        }
        config.profiles.push(profile);
    }

    let mut available: Vec<String> = root.model_routes.keys().cloned().collect();
    available.sort();
    if !available.is_empty() {
        root.default_model = store
            .default_model
            .as_ref()
            .filter(|model| root.model_routes.contains_key(*model))
            .cloned()
            .or_else(|| available.first().cloned())
            .unwrap_or_else(|| ONBOARDING_MODEL.to_string());
    }

    root.models = model_slots(&available, &root.default_model);
    config.profiles.push(root);
}

fn account_profile(account: &AccountRecord) -> ProfileConfig {
    let default_model = account.models.first().cloned().unwrap_or_default();
    let models = model_slots(&account.models, &default_model);
    match account.provider {
        AccountProvider::Openai => ProfileConfig {
            name: OPENAI_PROFILE_NAME.to_string(),
            provider_type: ProviderType::OpenAIResponses,
            base_url: "https://chatgpt.com/backend-api/codex".to_string(),
            api_key_keyring: Some(account.credential_key.clone()),
            default_model,
            auth_type: AuthType::OAuth,
            oauth_provider: Some(OAuthProvider::Chatgpt),
            models,
            runtime_managed: true,
            ..Default::default()
        },
        AccountProvider::Anthropic => ProfileConfig {
            name: ANTHROPIC_PROFILE_NAME.to_string(),
            provider_type: ProviderType::DirectAnthropic,
            base_url: "https://api.anthropic.com".to_string(),
            api_key_keyring: Some(account.credential_key.clone()),
            default_model,
            auth_type: AuthType::ApiKey,
            models,
            runtime_managed: true,
            ..Default::default()
        },
    }
}

fn model_slots(models: &[String], default_model: &str) -> ProfileModels {
    let find = |needle: &str| {
        models
            .iter()
            .find(|model| model.to_ascii_lowercase().contains(needle))
            .cloned()
            .or_else(|| (!default_model.is_empty()).then(|| default_model.to_string()))
    };
    ProfileModels {
        haiku: find("haiku"),
        sonnet: find("sonnet"),
        opus: find("opus"),
    }
}

pub fn store_api_key(entry_name: &str, value: &str) -> Result<()> {
    let entry = keyring::Entry::new("claudex", entry_name)
        .context("failed to access the OS credential store")?;
    entry
        .set_password(value)
        .context("failed to store API key in the OS credential store")
}

pub fn load_api_key(entry_name: &str) -> Result<String> {
    let entry = keyring::Entry::new("claudex", entry_name)
        .context("failed to access the OS credential store")?;
    entry
        .get_password()
        .context("API key is missing from the OS credential store")
}

pub fn delete_api_key(entry_name: &str) -> Result<()> {
    let entry = keyring::Entry::new("claudex", entry_name)
        .context("failed to access the OS credential store")?;
    entry
        .delete_credential()
        .context("failed to remove API key from the OS credential store")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_round_trips_without_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accounts.json");
        let mut store = AccountStore::default();
        store.upsert(AccountProvider::Openai);
        store.save_to(&path).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("access_token"));
        assert!(!raw.contains("api_key"));
        assert_eq!(
            AccountStore::load_from(&path).unwrap().accounts,
            store.accounts
        );
    }

    #[test]
    fn runtime_session_starts_with_no_provider_models() {
        let mut config = ClaudexConfig::default();
        apply_store_to_config(&mut config, &AccountStore::default());
        let root = config.find_profile(SESSION_PROFILE_NAME).unwrap();
        assert_eq!(root.default_model, ONBOARDING_MODEL);
        assert!(root.model_routes.is_empty());
        assert!(root.runtime_managed);
    }

    #[test]
    fn accounts_become_live_routes_and_are_not_serialized() {
        let mut store = AccountStore::default();
        store.upsert(AccountProvider::Openai);
        store.upsert(AccountProvider::Anthropic);
        let mut config = ClaudexConfig::default();
        apply_store_to_config(&mut config, &store);

        let root = config.find_profile(SESSION_PROFILE_NAME).unwrap();
        assert_eq!(root.model_routes["gpt-5.6"], OPENAI_PROFILE_NAME);
        assert_eq!(root.model_routes["claude-sonnet-5"], ANTHROPIC_PROFILE_NAME);
        let serialized = toml::to_string(&config).unwrap();
        assert!(!serialized.contains("runtime_managed"));
        assert!(!serialized.contains(SESSION_PROFILE_NAME));
        assert!(!serialized.contains(OPENAI_PROFILE_NAME));
        assert!(!serialized.contains(ANTHROPIC_PROFILE_NAME));
    }

    #[test]
    fn legacy_profiles_are_available_without_account_migration() {
        let mut config = ClaudexConfig {
            profiles: vec![ProfileConfig {
                name: "legacy".to_string(),
                base_url: "https://example.invalid".to_string(),
                default_model: "legacy-model".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        apply_store_to_config(&mut config, &AccountStore::default());
        let root = config.find_profile(SESSION_PROFILE_NAME).unwrap();
        assert_eq!(root.model_routes["legacy-model"], "legacy");
    }
}
