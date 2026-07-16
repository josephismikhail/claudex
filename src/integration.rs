use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::ClaudexConfig;

const MANAGED_MARKER: &str = "<!-- claudex-managed-models-skill:v1 -->";
const MODELS_SKILL: &str = r#"---
name: models
description: Open the local Claudex provider and model manager.
disable-model-invocation: true
---
<!-- claudex-managed-models-skill:v1 -->

Open the local Claudex provider and model manager now:

!`claudex models open`

Tell the user that the local account manager is open in their browser. Once a
provider is connected, they can use `/model` to switch models without leaving
this session.
"#;

const FALLBACK_SKILL: &str = r#"---
name: claudex-models
description: Open the local Claudex provider and model manager.
disable-model-invocation: true
---
<!-- claudex-managed-models-skill:v1 -->

Open the local Claudex provider and model manager now:

!`claudex models open`

Tell the user that the local account manager is open in their browser. Once a
provider is connected, they can use `/model` to switch models without leaving
this session.
"#;

/// Install the personal skill before Claude starts so it is discovered during
/// the current session. Existing user-authored `/models` skills are preserved.
pub fn ensure_models_skill() -> Result<&'static str> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    ensure_models_skill_at(&home)
}

fn ensure_models_skill_at(home: &std::path::Path) -> Result<&'static str> {
    let skills = home.join(".claude").join("skills");
    let preferred = skills.join("models").join("SKILL.md");

    if preferred.exists() {
        let existing = std::fs::read_to_string(&preferred)
            .with_context(|| format!("failed to read {}", preferred.display()))?;
        if !existing.contains(MANAGED_MARKER) {
            let fallback = skills.join("claudex-models").join("SKILL.md");
            write_managed_skill(&fallback, FALLBACK_SKILL)?;
            tracing::warn!(
                path = %preferred.display(),
                "kept user-authored /models skill; installed Claudex as /claudex-models"
            );
            return Ok("/claudex-models");
        }
    }

    write_managed_skill(&preferred, MODELS_SKILL)?;
    Ok("/models")
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
    fn managed_skill_invokes_local_cli() {
        assert!(MODELS_SKILL.contains("disable-model-invocation: true"));
        assert!(MODELS_SKILL.contains("!`claudex models open`"));
        assert!(MODELS_SKILL.contains(MANAGED_MARKER));
    }

    #[test]
    fn wildcard_bind_uses_loopback_browser_url() {
        assert_eq!(browser_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(browser_host("::1"), "[::1]");
        assert_eq!(browser_host("LOCALHOST"), "localhost");
        assert_eq!(browser_host("localhost"), "localhost");
    }

    #[test]
    fn skill_is_installed_before_launch_and_upgrades_atomically() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(ensure_models_skill_at(home.path()).unwrap(), "/models");
        let path = home.path().join(".claude/skills/models/SKILL.md");
        assert_eq!(std::fs::read_to_string(path).unwrap(), MODELS_SKILL);
    }

    #[test]
    fn user_owned_models_skill_is_preserved_with_fallback() {
        let home = tempfile::tempdir().unwrap();
        let preferred = home.path().join(".claude/skills/models/SKILL.md");
        std::fs::create_dir_all(preferred.parent().unwrap()).unwrap();
        std::fs::write(&preferred, "user content").unwrap();

        assert_eq!(
            ensure_models_skill_at(home.path()).unwrap(),
            "/claudex-models"
        );
        assert_eq!(std::fs::read_to_string(preferred).unwrap(), "user content");
        let fallback = home.path().join(".claude/skills/claudex-models/SKILL.md");
        assert_eq!(std::fs::read_to_string(fallback).unwrap(), FALLBACK_SKILL);
    }
}
