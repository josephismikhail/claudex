use std::process::Command;

use anyhow::{bail, Context, Result};

#[cfg(unix)]
use crate::config::HyperlinksConfig;
use crate::config::{ClaudexConfig, ProfileConfig};
use crate::oauth::{AuthType, OAuthProvider};
#[cfg(unix)]
use crate::terminal;

pub fn launch_claude(
    config: &ClaudexConfig,
    profile: &ProfileConfig,
    model_override: Option<&str>,
    extra_args: &[String],
    hyperlinks_override: bool,
) -> Result<()> {
    let proxy_base = format!(
        "http://{}:{}/proxy/{}",
        config.proxy_host, config.proxy_port, profile.name
    );

    let model = model_override
        .map(|m| config.resolve_model(m))
        .unwrap_or_else(|| config.resolve_model(&profile.default_model));

    // 非交互模式检测：含 -p / --print，或首个 arg 不是 flag（裸 prompt）
    let is_noninteractive = extra_args.iter().any(|arg| arg == "-p" || arg == "--print")
        || extra_args.first().is_some_and(|arg| !arg.starts_with('-'));

    let mut cmd = Command::new(&config.claude_binary);

    // 不设 CLAUDE_CONFIG_DIR — 使用全局 ~/.claude，保留用户已有认证和设置。
    // Profile 差异化完全通过环境变量实现。

    // Every profile goes through the local gateway, including Claude
    // subscriptions. This keeps model_routes active when /model switches
    // between providers in the same Claude Code session. The placeholder is
    // accepted only by the loopback proxy and is never forwarded upstream.
    cmd.env("ANTHROPIC_BASE_URL", &proxy_base)
        .env("ANTHROPIC_AUTH_TOKEN", "claudex-passthrough")
        .env("ANTHROPIC_MODEL", &model);

    if !profile.custom_headers.is_empty() {
        let headers: Vec<String> = profile
            .custom_headers
            .iter()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect();
        cmd.env("ANTHROPIC_CUSTOM_HEADERS", headers.join(","));
    }

    // 模型 slot 映射 → Claude Code 的 /model 切换
    if let Some(ref h) = profile.models.haiku {
        cmd.env("ANTHROPIC_DEFAULT_HAIKU_MODEL", config.resolve_model(h));
    }
    if let Some(ref s) = profile.models.sonnet {
        cmd.env("ANTHROPIC_DEFAULT_SONNET_MODEL", config.resolve_model(s));
    }
    if let Some(ref o) = profile.models.opus {
        cmd.env("ANTHROPIC_DEFAULT_OPUS_MODEL", config.resolve_model(o));
    }

    // Claude Code cannot infer gateway model capabilities from arbitrary GPT
    // IDs. Mark ChatGPT subscription slots as effort-capable so /effort and
    // ultracode remain available; the proxy maps the selected level to the
    // Responses API's reasoning.effort field.
    let is_chatgpt_subscription = profile.auth_type == AuthType::OAuth
        && profile.oauth_provider.as_ref().is_some_and(|provider| {
            matches!(
                provider.normalize(),
                OAuthProvider::Chatgpt | OAuthProvider::Openai
            )
        });
    if is_chatgpt_subscription {
        const CAPABILITIES: &str = "effort,xhigh_effort,max_effort";
        cmd.env(
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
            CAPABILITIES,
        )
        .env(
            "ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
            CAPABILITIES,
        )
        .env(
            "ANTHROPIC_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
            CAPABILITIES,
        );
    }

    for (k, v) in &profile.extra_env {
        cmd.env(k, v);
    }

    // Apply after profile variables so telemetry/exporters cannot be
    // accidentally re-enabled by an old profile or inherited shell setting.
    crate::privacy::apply_private_environment(&mut cmd);

    let private_args = crate::privacy::enforce_private_settings(extra_args)?;

    // 自动禁用 Chrome 集成（除非用户显式传了 --chrome）
    if !extra_args.iter().any(|a| a == "--chrome") {
        cmd.arg("--no-chrome");
    }

    cmd.args(&private_args);

    tracing::info!(
        profile = %profile.name,
        model = %model,
        proxy = %proxy_base,
        noninteractive = %is_noninteractive,
        "launching claude"
    );

    // PTY mode (Unix only): 非交互模式跳过 PTY
    #[cfg(unix)]
    let use_pty = !is_noninteractive && should_use_pty(&config.hyperlinks, hyperlinks_override);
    #[cfg(not(unix))]
    let use_pty = {
        let _ = hyperlinks_override;
        false
    };

    #[cfg(unix)]
    let mut resume_session_id: Option<String> = None;
    #[cfg(not(unix))]
    let resume_session_id: Option<String> = None;

    if use_pty {
        #[cfg(unix)]
        {
            tracing::info!("hyperlinks enabled, using PTY proxy mode");
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
            resume_session_id = terminal::pty::spawn_with_pty(cmd, cwd)?;
        }
    } else {
        let mut child = cmd.spawn().context("failed to execute claude binary")?;

        // 转发 SIGINT/SIGTERM 到子进程
        #[cfg(unix)]
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
        }

        let status = child.wait().context("failed to wait for claude")?;

        #[cfg(unix)]
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }

        if !status.success() {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if status.signal().is_some() {
                    std::process::exit(128 + status.signal().unwrap());
                }
            }
            bail!("claude exited with status: {}", status);
        }
    }

    // 追加 claudex resume 命令提示
    if let Some(session_id) = resume_session_id {
        print_claudex_resume_hint(&profile.name, &session_id, extra_args);
    }

    Ok(())
}

/// 在 Claude Code 退出后追加 claudex resume 命令提示
fn print_claudex_resume_hint(profile_name: &str, session_id: &str, extra_args: &[String]) {
    let hint = build_resume_hint(profile_name, session_id, extra_args);
    eprintln!("\nResume this session with claudex:\n  {hint}");
}

/// 构造 claudex resume 命令字符串（纯函数，便于测试）
fn build_resume_hint(profile_name: &str, session_id: &str, extra_args: &[String]) -> String {
    // 过滤掉原始 extra_args 中的 --resume 及其值参数
    let mut args_clean: Vec<&str> = Vec::new();
    let mut skip_next = false;
    for arg in extra_args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--resume" {
            skip_next = true;
            continue;
        }
        args_clean.push(arg);
    }

    let args_str = if args_clean.is_empty() {
        String::new()
    } else {
        format!(" {}", args_clean.join(" "))
    };

    format!("claudex run {profile_name} --resume {session_id}{args_str}")
}

/// Decide whether to use PTY mode based on config + CLI flag.
#[cfg(unix)]
fn should_use_pty(config_hyperlinks: &HyperlinksConfig, cli_override: bool) -> bool {
    if cli_override {
        return true;
    }

    match config_hyperlinks {
        HyperlinksConfig::Enabled => true,
        HyperlinksConfig::Disabled => false,
        HyperlinksConfig::Auto => terminal::detect::terminal_supports_hyperlinks(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_resume_hint_no_extra_args() {
        let hint = build_resume_hint("codex-sub", "abc-123", &[]);
        assert_eq!(hint, "claudex run codex-sub --resume abc-123");
    }

    #[test]
    fn test_build_resume_hint_with_extra_args() {
        let args = vec![
            "--dangerously-skip-permissions".to_string(),
            "--verbose".to_string(),
        ];
        let hint = build_resume_hint("codex-sub", "abc-123", &args);
        assert_eq!(
            hint,
            "claudex run codex-sub --resume abc-123 --dangerously-skip-permissions --verbose"
        );
    }

    #[test]
    fn test_build_resume_hint_filters_existing_resume() {
        let args = vec![
            "--resume".to_string(),
            "old-session-id".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ];
        let hint = build_resume_hint("codex-sub", "new-session-id", &args);
        assert_eq!(
            hint,
            "claudex run codex-sub --resume new-session-id --dangerously-skip-permissions"
        );
    }

    #[test]
    fn test_build_resume_hint_resume_at_end() {
        let args = vec![
            "--verbose".to_string(),
            "--resume".to_string(),
            "old-id".to_string(),
        ];
        let hint = build_resume_hint("my-profile", "new-id", &args);
        assert_eq!(hint, "claudex run my-profile --resume new-id --verbose");
    }

    #[test]
    fn test_build_resume_hint_resume_only() {
        let args = vec!["--resume".to_string(), "old-id".to_string()];
        let hint = build_resume_hint("p", "new-id", &args);
        assert_eq!(hint, "claudex run p --resume new-id");
    }

    #[cfg(windows)]
    #[test]
    fn test_launches_native_windows_command_with_proxy_environment() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-claude.cmd");
        let output = dir.path().join("child-output.txt");
        std::fs::write(
            &script,
            format!(
                "@echo off\r\n> \"{}\" (\r\n  echo BASE=%ANTHROPIC_BASE_URL%\r\n  echo TOKEN=%ANTHROPIC_AUTH_TOKEN%\r\n  echo MODEL=%ANTHROPIC_MODEL%\r\n  echo HAIKU=%ANTHROPIC_DEFAULT_HAIKU_MODEL%\r\n  echo CAPABILITIES=%ANTHROPIC_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES%\r\n  echo PRIVATE=%CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC%\r\n  echo TELEMETRY=%DISABLE_TELEMETRY%\r\n  echo UPDATES=%DISABLE_UPDATES%\r\n  echo OTEL=%OTEL_METRICS_EXPORTER%\r\n  echo ARGS=%*\r\n)\r\n",
                output.display()
            ),
        )
        .unwrap();

        let config = ClaudexConfig {
            claude_binary: script.to_string_lossy().into_owned(),
            proxy_port: 15432,
            model_aliases: std::collections::HashMap::from([(
                "fast".to_string(),
                "provider-fast-model".to_string(),
            )]),
            ..Default::default()
        };
        let profile = ProfileConfig {
            name: "windows-test".to_string(),
            provider_type: crate::config::ProviderType::OpenAICompatible,
            base_url: "https://example.invalid/v1".to_string(),
            default_model: "test-model".to_string(),
            auth_type: AuthType::OAuth,
            oauth_provider: Some(OAuthProvider::Chatgpt),
            models: crate::config::ProfileModels {
                haiku: Some("fast".to_string()),
                ..Default::default()
            },
            extra_env: std::collections::HashMap::from([
                ("DISABLE_TELEMETRY".to_string(), "0".to_string()),
                (
                    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_string(),
                    "0".to_string(),
                ),
            ]),
            ..Default::default()
        };

        launch_claude(
            &config,
            &profile,
            None,
            &["--print".to_string(), "hello".to_string()],
            false,
        )
        .unwrap();

        let child_output = std::fs::read_to_string(output).unwrap();
        assert!(child_output.contains("BASE=http://127.0.0.1:15432/proxy/windows-test"));
        assert!(child_output.contains("TOKEN=claudex-passthrough"));
        assert!(child_output.contains("MODEL=test-model"));
        assert!(child_output.contains("HAIKU=provider-fast-model"));
        assert!(child_output.contains("CAPABILITIES=effort,xhigh_effort,max_effort"));
        assert!(child_output.contains("PRIVATE=1"));
        assert!(child_output.contains("TELEMETRY=1"));
        assert!(child_output.contains("UPDATES=1"));
        assert!(child_output.contains("OTEL=none"));
        assert!(child_output.contains("ARGS=--no-chrome --settings"));
        assert!(child_output.contains("skipWebFetchPreflight"));
        assert!(child_output.contains("--print hello"));
    }
}
