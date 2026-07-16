use std::process::Command;

#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

#[cfg(unix)]
use crate::config::HyperlinksConfig;
use crate::config::{ClaudexConfig, ProfileConfig};
use crate::oauth::{AuthType, OAuthProvider};
#[cfg(unix)]
use crate::terminal;

#[cfg(windows)]
fn resolve_windows_command_in(binary: &str, paths: Option<&OsStr>, cwd: &Path) -> Result<PathBuf> {
    let resolved = which::which_in(binary, paths, cwd)
        .with_context(|| format!("Claude Code command `{binary}` was not found in PATH"))?;
    Ok(native_target_from_cmd_shim(&resolved).unwrap_or(resolved))
}

#[cfg(windows)]
fn native_target_from_cmd_shim(shim: &Path) -> Option<PathBuf> {
    let extension = shim.extension()?.to_string_lossy();
    if !extension.eq_ignore_ascii_case("cmd") && !extension.eq_ignore_ascii_case("bat") {
        return None;
    }

    let contents = std::fs::read_to_string(shim).ok()?;
    let parent = shim.parent()?;
    for line in contents.lines() {
        let Some(open_quote) = line.find('"') else {
            continue;
        };
        let remainder = &line[open_quote + 1..];
        let Some(close_quote) = remainder.find('"') else {
            continue;
        };
        let command = &remainder[..close_quote];
        let trailing = &remainder[close_quote + 1..];
        if !trailing.split_whitespace().any(|token| token == "%*") {
            continue;
        }
        let Some(prefix) = command.get(..5) else {
            continue;
        };
        if !prefix.eq_ignore_ascii_case("%dp0%") {
            continue;
        }
        if !command.to_ascii_lowercase().ends_with(".exe") {
            continue;
        }

        let relative = command[5..].trim_start_matches(['\\', '/']);
        let target = parent.join(relative);
        if target.is_file() {
            return Some(target);
        }
    }
    None
}

#[cfg(windows)]
fn create_claude_command(binary: &str) -> Result<Command> {
    let cwd = std::env::current_dir().context("failed to determine the current directory")?;
    let paths = std::env::var_os("PATH");
    let resolved = resolve_windows_command_in(binary, paths.as_deref(), &cwd)?;
    Ok(Command::new(resolved))
}

#[cfg(not(windows))]
fn create_claude_command(binary: &str) -> Result<Command> {
    Ok(Command::new(binary))
}

pub fn launch_claude(
    config: &ClaudexConfig,
    profile: &ProfileConfig,
    model_override: Option<&str>,
    extra_args: &[String],
    hyperlinks_override: bool,
) -> Result<()> {
    let fast_session = crate::fast::FastSession::create()?;
    let model = model_override
        .map(|m| config.resolve_model(m))
        .unwrap_or_else(|| config.resolve_model(&profile.default_model));

    // 非交互模式检测：含 -p / --print，或首个 arg 不是 flag（裸 prompt）
    let is_noninteractive = extra_args.iter().any(|arg| arg == "-p" || arg == "--print")
        || extra_args.first().is_some_and(|arg| !arg.starts_with('-'));

    let mut cmd = configured_claude_command(config, profile, &model, fast_session.id())?;

    let private_args = crate::privacy::enforce_private_settings(extra_args)?;

    // 自动禁用 Chrome 集成（除非用户显式传了 --chrome）
    if !extra_args.iter().any(|a| a == "--chrome") {
        cmd.arg("--no-chrome");
    }

    // Keep Claudex-managed provider commands out of ordinary Claude sessions.
    cmd.arg("--add-dir")
        .arg(crate::integration::claude_integration_root()?);

    cmd.args(&private_args);

    tracing::info!(
        profile = %profile.name,
        model = %model,
        proxy = %format!("http://{}:{}/proxy/{}", config.proxy_host, config.proxy_port, profile.name),
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

/// Build a Claude Code process with Claudex's local gateway, route-aware fast
/// state, and privacy policy applied. Interactive and SDK-style frontends use
/// the same environment so provider behavior cannot drift between surfaces.
pub(crate) fn configured_claude_command(
    config: &ClaudexConfig,
    profile: &ProfileConfig,
    model: &str,
    fast_session_id: &str,
) -> Result<Command> {
    let mut cmd = create_claude_command(&config.claude_binary)?;
    let proxy_base = format!(
        "http://{}:{}/proxy/{}",
        config.proxy_host, config.proxy_port, profile.name
    );

    // Do not set CLAUDE_CONFIG_DIR: project instructions, tools, and existing
    // user settings remain available to the underlying agent harness.
    cmd.env("ANTHROPIC_BASE_URL", proxy_base)
        .env("ANTHROPIC_AUTH_TOKEN", "claudex-passthrough")
        .env("ANTHROPIC_MODEL", model)
        .env("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1");

    configure_openai_picker_entry(&mut cmd, config, profile, model);

    #[cfg(windows)]
    cmd.env("CLAUDE_CODE_USE_POWERSHELL_TOOL", "1");

    for (family, configured) in [
        ("HAIKU", profile.models.haiku.as_deref()),
        ("SONNET", profile.models.sonnet.as_deref()),
        ("OPUS", profile.models.opus.as_deref()),
    ] {
        let Some(configured) = configured else {
            continue;
        };
        let resolved = config.resolve_model(configured);
        cmd.env(format!("ANTHROPIC_DEFAULT_{family}_MODEL"), &resolved)
            .env(
                format!("ANTHROPIC_DEFAULT_{family}_MODEL_NAME"),
                format!(
                    "{resolved} · {}",
                    model_provider_label(config, profile, &resolved)
                ),
            )
            .env(
                format!("ANTHROPIC_DEFAULT_{family}_MODEL_DESCRIPTION"),
                "Connected locally through Claudex",
            );
        if model_uses_chatgpt(config, profile, &resolved) {
            cmd.env(
                format!("ANTHROPIC_DEFAULT_{family}_MODEL_SUPPORTED_CAPABILITIES"),
                "effort,xhigh_effort,max_effort",
            );
        }
    }

    for (key, value) in &profile.extra_env {
        cmd.env(key, value);
    }

    cmd.env("CLAUDE_CODE_DISABLE_FAST_MODE", "1")
        .env(crate::fast::FAST_SESSION_ENV, fast_session_id);
    configure_custom_headers(&mut cmd, profile, fast_session_id);
    crate::privacy::apply_private_environment(&mut cmd);
    Ok(cmd)
}

fn configure_custom_headers(command: &mut Command, profile: &ProfileConfig, session_id: &str) {
    let mut headers: Vec<String> = profile
        .custom_headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case(crate::fast::FAST_SESSION_HEADER))
        .map(|(name, value)| format!("{name}:{value}"))
        .collect();
    headers.push(format!("{}:{session_id}", crate::fast::FAST_SESSION_HEADER));
    command.env("ANTHROPIC_CUSTOM_HEADERS", headers.join(","));
}

fn profile_uses_chatgpt(config: &ClaudexConfig, profile: &ProfileConfig) -> bool {
    is_chatgpt_profile(profile)
        || profile
            .model_routes
            .values()
            .any(|target| config.find_profile(target).is_some_and(is_chatgpt_profile))
}

fn model_uses_chatgpt(config: &ClaudexConfig, profile: &ProfileConfig, model: &str) -> bool {
    profile
        .model_routes
        .get(model)
        .and_then(|target| config.find_profile(target))
        .map_or_else(|| is_chatgpt_profile(profile), is_chatgpt_profile)
}

fn model_provider_label(
    config: &ClaudexConfig,
    profile: &ProfileConfig,
    model: &str,
) -> &'static str {
    let target = profile
        .model_routes
        .get(model)
        .and_then(|name| config.find_profile(name))
        .unwrap_or(profile);
    match target.provider_type {
        crate::config::ProviderType::DirectAnthropic => "Anthropic",
        crate::config::ProviderType::OpenAIResponses => "OpenAI",
        crate::config::ProviderType::OpenAICompatible => "OpenAI-compatible provider",
    }
}

fn is_chatgpt_profile(profile: &ProfileConfig) -> bool {
    profile.auth_type == AuthType::OAuth
        && profile.oauth_provider.as_ref().is_some_and(|provider| {
            matches!(
                provider.normalize(),
                OAuthProvider::Chatgpt | OAuthProvider::Openai
            )
        })
}

fn configure_openai_picker_entry(
    command: &mut Command,
    config: &ClaudexConfig,
    profile: &ProfileConfig,
    model: &str,
) {
    // Claude Code's gateway discovery intentionally filters out model IDs
    // that do not begin with "claude" or "anthropic". Its documented custom
    // entry is therefore used as a compatibility row for the legacy stock
    // Claude surface. Joey's Claudex owns its normal `/model` picker and shows
    // every connected OpenAI model there without inventing Claude-family IDs.
    if !model_uses_chatgpt(config, profile, model) {
        return;
    }

    const CAPABILITIES: &str = "effort,xhigh_effort,max_effort";
    command
        .env("ANTHROPIC_CUSTOM_MODEL_OPTION", model)
        .env(
            "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME",
            format!("{model} via OpenAI"),
        )
        .env(
            "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION",
            "OpenAI through the local Claudex gateway",
        )
        .env(
            "ANTHROPIC_CUSTOM_MODEL_OPTION_SUPPORTED_CAPABILITIES",
            CAPABILITIES,
        );
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

    fn command_env(command: &Command, name: &str) -> Option<String> {
        command.get_envs().find_map(|(key, value)| {
            if key == name {
                value.map(|value| value.to_string_lossy().into_owned())
            } else {
                None
            }
        })
    }

    #[test]
    fn managed_openai_account_gets_picker_entry_and_effort_capabilities() {
        let mut store = crate::accounts::AccountStore::default();
        store.upsert(crate::accounts::AccountProvider::Openai);
        let mut config = ClaudexConfig::default();
        crate::accounts::apply_store_to_config(&mut config, &store);
        let profile = config
            .find_profile(crate::accounts::SESSION_PROFILE_NAME)
            .unwrap();
        let mut command = Command::new("claude");

        configure_openai_picker_entry(&mut command, &config, profile, "gpt-5.6-sol");

        assert!(profile_uses_chatgpt(&config, profile));
        assert_eq!(
            command_env(&command, "ANTHROPIC_CUSTOM_MODEL_OPTION").as_deref(),
            Some("gpt-5.6-sol")
        );
        assert_eq!(
            command_env(
                &command,
                "ANTHROPIC_CUSTOM_MODEL_OPTION_SUPPORTED_CAPABILITIES"
            )
            .as_deref(),
            Some("effort,xhigh_effort,max_effort")
        );
    }

    #[test]
    fn empty_session_does_not_advertise_an_unconnected_openai_model() {
        let mut config = ClaudexConfig::default();
        crate::accounts::apply_store_to_config(
            &mut config,
            &crate::accounts::AccountStore::default(),
        );
        let profile = config
            .find_profile(crate::accounts::SESSION_PROFILE_NAME)
            .unwrap();
        let mut command = Command::new("claude");

        configure_openai_picker_entry(&mut command, &config, profile, "claude-opus-4-8");

        assert!(!profile_uses_chatgpt(&config, profile));
        assert_eq!(command_env(&command, "ANTHROPIC_CUSTOM_MODEL_OPTION"), None);
    }

    #[test]
    fn reserved_fast_header_cannot_be_overridden_by_a_profile() {
        let profile = ProfileConfig {
            custom_headers: std::collections::HashMap::from([
                ("X-Test".to_string(), "ok".to_string()),
                (
                    crate::fast::FAST_SESSION_HEADER.to_string(),
                    "attacker-controlled".to_string(),
                ),
            ]),
            ..Default::default()
        };
        let mut command = Command::new("claude");

        configure_custom_headers(&mut command, &profile, "safe-session");

        let headers = command_env(&command, "ANTHROPIC_CUSTOM_HEADERS").unwrap();
        assert!(headers.contains("X-Test:ok"));
        assert!(headers.contains("x-claudex-fast-session:safe-session"));
        assert!(!headers.contains("attacker-controlled"));
    }

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
    fn resolves_windows_cmd_shim_from_path_without_extension() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-claude.cmd");
        std::fs::write(&script, "@echo off\r\nexit /b 0\r\n").unwrap();

        let resolved =
            resolve_windows_command_in("fake-claude", Some(dir.path().as_os_str()), dir.path())
                .unwrap();

        assert_eq!(
            std::fs::canonicalize(resolved).unwrap(),
            std::fs::canonicalize(script).unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn resolves_native_executable_forwarded_by_npm_cmd_shim() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("claude.cmd");
        let target = dir
            .path()
            .join("node_modules/@anthropic-ai/claude-code/bin/claude.exe");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"not executed by this resolution test").unwrap();
        std::fs::write(
            &script,
            "@echo off\r\n\"%dp0%\\node_modules\\@anthropic-ai\\claude-code\\bin\\claude.exe\" %*\r\n",
        )
        .unwrap();

        let resolved =
            resolve_windows_command_in("claude", Some(dir.path().as_os_str()), dir.path()).unwrap();

        assert_eq!(
            std::fs::canonicalize(resolved).unwrap(),
            std::fs::canonicalize(target).unwrap()
        );
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
                "@echo off\r\n> \"{}\" (\r\n  echo BASE=%ANTHROPIC_BASE_URL%\r\n  echo TOKEN=%ANTHROPIC_AUTH_TOKEN%\r\n  echo MODEL=%ANTHROPIC_MODEL%\r\n  echo HAIKU=%ANTHROPIC_DEFAULT_HAIKU_MODEL%\r\n  echo CAPABILITIES=%ANTHROPIC_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES%\r\n  echo DISCOVERY=%CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY%\r\n  echo CUSTOM=%ANTHROPIC_CUSTOM_MODEL_OPTION%\r\n  echo CUSTOM_CAPABILITIES=%ANTHROPIC_CUSTOM_MODEL_OPTION_SUPPORTED_CAPABILITIES%\r\n  echo FAST_DISABLED=%CLAUDE_CODE_DISABLE_FAST_MODE%\r\n  echo FAST_SESSION=%CLAUDEX_FAST_SESSION%\r\n  echo HEADERS=%ANTHROPIC_CUSTOM_HEADERS%\r\n  echo PRIVATE=%CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC%\r\n  echo TELEMETRY=%DISABLE_TELEMETRY%\r\n  echo UPDATES=%DISABLE_UPDATES%\r\n  echo OTEL=%OTEL_METRICS_EXPORTER%\r\n  echo POWERSHELL=%CLAUDE_CODE_USE_POWERSHELL_TOOL%\r\n  echo ARGS=%*\r\n)\r\n",
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
        assert!(child_output.contains("DISCOVERY=1"));
        assert!(child_output.contains("CUSTOM=test-model"));
        assert!(child_output.contains("CUSTOM_CAPABILITIES=effort,xhigh_effort,max_effort"));
        assert!(child_output.contains("FAST_DISABLED=1"));
        assert!(child_output.contains("FAST_SESSION="));
        assert!(child_output.contains("HEADERS=x-claudex-fast-session:"));
        assert!(child_output.contains("PRIVATE=1"));
        assert!(child_output.contains("TELEMETRY=1"));
        assert!(child_output.contains("UPDATES=1"));
        assert!(child_output.contains("OTEL=none"));
        assert!(child_output.contains("POWERSHELL=1"));
        assert!(child_output.contains("ARGS=--no-chrome --add-dir"));
        assert!(child_output.contains("claude-integration --settings"));
        assert!(child_output.contains("skipWebFetchPreflight"));
        assert!(child_output.contains("--print hello"));
    }
}
