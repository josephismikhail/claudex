use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::ClaudexConfig;

const LEGACY_MODELS_MARKER: &str = "<!-- claudex-managed-models-skill:v1 -->";
const OPENAI_FAST_MARKER: &str = "<!-- claudex-managed-openai-fast-skill:v1 -->";
const OPENAI_USAGE_MARKER: &str = "<!-- claudex-managed-openai-usage-skill:v1 -->";

const ROUTE_AWARE_FAST_SKILL: &str = r#"---
name: fast
description: Toggle provider-aware fast processing for this Claudex session.
disable-model-invocation: true
---
<!-- claudex-managed-openai-fast-skill:v1 -->

Toggle this Claudex session's provider-aware fast mode now:

!`claudex fast`

Repeat the command output verbatim and do nothing else. While fast mode is on,
the local gateway chooses the connected provider's supported fast path for each
request and leaves unsupported provider or model routes at standard speed.
"#;

const OPENAI_USAGE_SKILL: &str = r#"---
name: usage
description: Show the connected OpenAI subscription's live remaining usage.
disable-model-invocation: true
---
<!-- claudex-managed-openai-usage-skill:v1 -->

Fetch the connected OpenAI subscription's usage now:

!`claudex usage`

Repeat the command output verbatim and do nothing else. The local command
queries OpenAI at invocation time, so its values are authoritative.
"#;

// Claude Code ships its own `/usage` command. A hidden skill with the same
// name shadows that built-in while no OpenAI subscription is connected, so
// the command is genuinely absent instead of showing unrelated Claude usage.
const HIDDEN_OPENAI_USAGE_SKILL: &str = r#"---
name: usage
description: Reserved for connected OpenAI subscription usage in Claudex.
disable-model-invocation: true
user-invocable: false
---
<!-- claudex-managed-openai-usage-skill:v1 -->

OpenAI subscription usage is unavailable because no OpenAI account is connected.
"#;

/// Remove the retired Claudex `/models` skill while preserving any
/// user-authored skill that happens to use the same name.
pub fn remove_legacy_models_skills() -> Result<()> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    remove_legacy_models_skills_at(&home)
}

fn remove_legacy_models_skills_at(home: &std::path::Path) -> Result<()> {
    let skills = home.join(".claude").join("skills");
    for name in ["models", "claudex-models"] {
        sync_managed_skill(
            &skills.join(name).join("SKILL.md"),
            "",
            LEGACY_MODELS_MARKER,
            false,
        )?;
    }
    Ok(())
}

fn write_managed_skill(path: &PathBuf, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() && std::fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(());
    }
    crate::config::write_file_atomically(path, content.as_bytes())
        .with_context(|| format!("failed to install Claude Code skill at {}", path.display()))
}

/// Directory passed to Claude Code with `--add-dir`. Keeping provider-specific
/// commands here makes them visible only in Claudex sessions, while the
/// always-present parent directory lets Claude Code detect account changes
/// without a restart.
pub fn claude_integration_root() -> Result<PathBuf> {
    let root = ClaudexConfig::config_dir()?.join("claude-integration");
    std::fs::create_dir_all(root.join(".claude").join("skills"))?;
    Ok(root)
}

pub fn sync_account_skills(store: &crate::accounts::AccountStore) -> Result<()> {
    let root = claude_integration_root()?;
    let fast_available = crate::fast::FastAvailability::from_store(store).any();
    let openai_connected = store.has_provider(crate::accounts::AccountProvider::Openai);
    sync_provider_skills_at(&root, fast_available, openai_connected)
}

fn sync_provider_skills_at(
    root: &std::path::Path,
    fast_available: bool,
    openai_connected: bool,
) -> Result<()> {
    let skills = root.join(".claude").join("skills");
    std::fs::create_dir_all(&skills)?;
    sync_managed_skill(
        &skills.join("fast").join("SKILL.md"),
        ROUTE_AWARE_FAST_SKILL,
        OPENAI_FAST_MARKER,
        fast_available,
    )?;
    sync_managed_skill(
        &skills.join("usage").join("SKILL.md"),
        if openai_connected {
            OPENAI_USAGE_SKILL
        } else {
            HIDDEN_OPENAI_USAGE_SKILL
        },
        OPENAI_USAGE_MARKER,
        true,
    )?;
    Ok(())
}

fn sync_managed_skill(path: &PathBuf, content: &str, marker: &str, enabled: bool) -> Result<()> {
    if enabled {
        if path.exists() {
            let existing = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            if !existing.contains(marker) {
                anyhow::bail!(
                    "refusing to replace non-Claudex skill at {}",
                    path.display()
                );
            }
        }
        return write_managed_skill(path, content);
    }

    if !path.exists() {
        return Ok(());
    }
    let existing = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if !existing.contains(marker) {
        return Ok(());
    }
    std::fs::remove_file(path)
        .with_context(|| format!("failed to remove Claude Code skill at {}", path.display()))?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

pub async fn open_model_manager(config: &mut ClaudexConfig) -> Result<()> {
    crate::accounts::apply_to_config(config)?;
    let setup_url = format!(
        "http://{}:{}/setup",
        browser_host(&config.proxy_host),
        config.proxy_port
    );

    if crate::proxy::is_proxy_reachable(&config.proxy_host, config.proxy_port).await {
        if !setup_is_available(&setup_url).await {
            anyhow::bail!(
                "a proxy is already using {}:{}, but it does not expose the Claudex model manager; restart the current Claudex session",
                config.proxy_host,
                config.proxy_port
            );
        }
    } else {
        let pid = crate::process::daemon::spawn_proxy_daemon(config, None)?;
        if !crate::proxy::wait_for_proxy(
            &config.proxy_host,
            config.proxy_port,
            Duration::from_secs(5),
        )
        .await
        {
            anyhow::bail!("model manager proxy (PID {pid}) failed to start within 5 seconds");
        }
        if !setup_is_available(&setup_url).await {
            anyhow::bail!("model manager proxy started but its setup page is unavailable");
        }
    }

    open::that(&setup_url).context("failed to open the local model manager in a browser")?;
    println!("Claudex model manager opened in your browser.");
    Ok(())
}

fn browser_host(configured: &str) -> String {
    if configured.eq_ignore_ascii_case("localhost") {
        return "localhost".to_string();
    }
    match configured {
        "0.0.0.0" | "::" => "127.0.0.1".to_string(),
        "::1" => "[::1]".to_string(),
        other => other.to_string(),
    }
}

async fn setup_is_available(url: &str) -> bool {
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };
    client
        .get(url)
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_bind_uses_loopback_browser_url() {
        assert_eq!(browser_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(browser_host("::1"), "[::1]");
        assert_eq!(browser_host("LOCALHOST"), "localhost");
        assert_eq!(browser_host("localhost"), "localhost");
    }

    #[test]
    fn retired_models_skills_are_removed_without_touching_user_skills() {
        let home = tempfile::tempdir().unwrap();
        let managed = home.path().join(".claude/skills/models/SKILL.md");
        let managed_fallback = home.path().join(".claude/skills/claudex-models/SKILL.md");
        let user_owned = home.path().join(".claude/skills/custom/SKILL.md");
        for path in [&managed, &managed_fallback, &user_owned] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(&managed, LEGACY_MODELS_MARKER).unwrap();
        std::fs::write(&managed_fallback, LEGACY_MODELS_MARKER).unwrap();
        std::fs::write(&user_owned, "user content").unwrap();

        remove_legacy_models_skills_at(home.path()).unwrap();

        assert!(!managed.exists());
        assert!(!managed_fallback.exists());
        assert_eq!(std::fs::read_to_string(user_owned).unwrap(), "user content");
    }

    #[test]
    fn fast_and_usage_skills_follow_their_provider_availability() {
        let root = tempfile::tempdir().unwrap();
        let fast = root.path().join(".claude/skills/fast/SKILL.md");
        let usage = root.path().join(".claude/skills/usage/SKILL.md");

        sync_provider_skills_at(root.path(), false, false).unwrap();
        assert!(!fast.exists());
        assert!(std::fs::read_to_string(&usage)
            .unwrap()
            .contains("user-invocable: false"));

        // An eligible Anthropic account exposes /fast without exposing the
        // OpenAI-only /usage command.
        sync_provider_skills_at(root.path(), true, false).unwrap();
        assert!(std::fs::read_to_string(&fast)
            .unwrap()
            .contains("!`claudex fast`"));
        assert!(std::fs::read_to_string(&usage)
            .unwrap()
            .contains("user-invocable: false"));

        sync_provider_skills_at(root.path(), true, true).unwrap();
        assert!(std::fs::read_to_string(&usage)
            .unwrap()
            .contains("!`claudex usage`"));

        sync_provider_skills_at(root.path(), false, false).unwrap();
        assert!(!fast.exists());
        assert!(std::fs::read_to_string(&usage)
            .unwrap()
            .contains("user-invocable: false"));
        assert!(root.path().join(".claude/skills").exists());
    }
}
