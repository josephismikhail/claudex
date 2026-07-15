#![allow(dead_code)]

mod cli;
mod config;
mod context;
mod oauth;
mod privacy;
mod process;
mod proxy;
mod router;
mod sets;
mod terminal;
mod tui;
mod util;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use cli::{AuthAction, Cli, Commands, ProfileAction, ProxyAction, SetsAction};
use config::ClaudexConfig;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut config = ClaudexConfig::load(cli.config.as_deref())?;

    // Full-screen terminal sessions must never receive tracing output on stderr.
    // In particular, launching Claude from the dashboard keeps the same tracing
    // subscriber alive after leaving the dashboard. Writing proxy request logs
    // to stderr at that point corrupts Claude Code's terminal renderer.
    let owns_terminal = command_owns_terminal(&cli.command, !config.profiles.is_empty());

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level));

    // 日志文件（所有模式都写）
    let file_layer = proxy::proxy_log_path().and_then(|log_path| {
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok()
            .map(|file| {
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(file))
            })
    });

    // Keep interactive/full-screen terminal output pristine. Logs are still
    // written to the per-process log file configured above.
    let stderr_layer = if owns_terminal {
        None
    } else {
        Some(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    match cli.command {
        Some(Commands::Run {
            profile: profile_name,
            model,
            hyperlinks,
            args,
        }) => {
            run_profile_session(&config, &profile_name, model.as_deref(), &args, hyperlinks)
                .await?;
        }

        Some(Commands::Profile { action }) => match action {
            ProfileAction::List => {
                config::profile::list_profiles(&config).await;
            }
            ProfileAction::Show { name } => {
                config::profile::show_profile(&config, &name).await?;
            }
            ProfileAction::Test { name } => {
                config::profile::test_profile(&config, &name).await?;
            }
            ProfileAction::Add => {
                config::profile::interactive_add(&mut config).await?;
            }
            ProfileAction::Remove { name } => {
                config::profile::remove_profile(&mut config, &name)?;
            }
        },

        Some(Commands::Proxy { action }) => match action {
            ProxyAction::Start {
                port,
                daemon: as_daemon,
            } => {
                if as_daemon {
                    let actual_port = port.unwrap_or(config.proxy_port);
                    if proxy::is_proxy_reachable(&config.proxy_host, actual_port).await {
                        println!(
                            "Proxy is already reachable at {}:{}",
                            config.proxy_host, actual_port
                        );
                    } else {
                        let pid = process::daemon::spawn_proxy_daemon(&config, port)?;
                        if !proxy::wait_for_proxy(
                            &config.proxy_host,
                            actual_port,
                            std::time::Duration::from_secs(5),
                        )
                        .await
                        {
                            anyhow::bail!(
                                "proxy daemon (PID {pid}) failed to start within 5 seconds"
                            );
                        }
                        println!(
                            "Proxy daemon started at {}:{} (PID {pid})",
                            config.proxy_host, actual_port
                        );
                    }
                } else {
                    proxy::start_proxy(config, port).await?;
                }
            }
            ProxyAction::Stop => {
                process::daemon::stop_proxy()?;
            }
            ProxyAction::Status => {
                process::daemon::proxy_status()?;
            }
        },

        Some(Commands::Dashboard) => {
            let config_arc = std::sync::Arc::new(tokio::sync::RwLock::new(config));
            let metrics_store = proxy::metrics::MetricsStore::new();
            let health =
                std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
            tui::run_tui(config_arc, metrics_store, health).await?;
        }

        Some(Commands::Config { action }) => {
            config::cmd::dispatch(action, &mut config).await?;
        }

        Some(Commands::Sets { action }) => match action {
            SetsAction::Add {
                source,
                global,
                r#ref,
            } => {
                sets::add(&source, global, r#ref.as_deref()).await?;
            }
            SetsAction::Remove { name, global } => {
                sets::remove(&name, global).await?;
            }
            SetsAction::List { global } => {
                sets::list(global)?;
            }
            SetsAction::Update { name, global } => {
                sets::update(name.as_deref(), global).await?;
            }
            SetsAction::Show { name, global } => {
                sets::show(&name, global)?;
            }
        },

        Some(Commands::Auth { action }) => match action {
            AuthAction::Login {
                provider,
                profile,
                force,
                headless,
                enterprise_url,
            } => {
                let profile_name = profile.unwrap_or_else(|| provider.clone());
                oauth::providers::login(
                    &mut config,
                    &provider,
                    &profile_name,
                    force,
                    headless,
                    enterprise_url.as_deref(),
                )
                .await?;
            }
            AuthAction::Status { profile } => {
                oauth::providers::status(&config, profile.as_deref()).await?;
            }
            AuthAction::Logout { profile } => {
                oauth::providers::logout(&config, &profile).await?;
            }
            AuthAction::Refresh { profile } => {
                oauth::providers::refresh(&config, &profile).await?;
            }
        },

        None => match default_action(&config) {
            DefaultAction::Welcome => print_welcome()?,
            DefaultAction::AutoRun(profile_name) => {
                run_profile_session(&config, &profile_name, None, &[], false).await?;
            }
            DefaultAction::Dashboard => {
                let config_arc = std::sync::Arc::new(tokio::sync::RwLock::new(config));
                let metrics_store = proxy::metrics::MetricsStore::new();
                let health =
                    std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
                tui::run_tui(config_arc, metrics_store, health).await?;
            }
        },
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum DefaultAction {
    Welcome,
    AutoRun(String),
    Dashboard,
}

fn default_action(config: &ClaudexConfig) -> DefaultAction {
    if config.profiles.is_empty() {
        return DefaultAction::Welcome;
    }

    let enabled = config.enabled_profiles();
    if enabled.len() == 1 {
        DefaultAction::AutoRun(enabled[0].name.clone())
    } else {
        DefaultAction::Dashboard
    }
}

fn print_welcome() -> Result<()> {
    println!("Welcome to Claudex!");
    println!();
    println!("Get started:");
    println!("  1. Create config: claudex config");
    println!(
        "  2. Add a profile: edit {:?}",
        ClaudexConfig::config_path()?
    );
    println!("  3. Run claude:    claudex run <profile>");
    println!();
    println!("Use --help for more options.");
    Ok(())
}

async fn run_profile_session(
    config: &ClaudexConfig,
    profile_name: &str,
    model: Option<&str>,
    args: &[String],
    hyperlinks: bool,
) -> Result<()> {
    let profile = config
        .find_profile(profile_name)
        .ok_or_else(|| anyhow::anyhow!("profile '{}' not found", profile_name))?
        .clone();
    if !profile.enabled {
        anyhow::bail!("profile '{}' is disabled", profile_name);
    }

    if !proxy::is_proxy_reachable(&config.proxy_host, config.proxy_port).await {
        tracing::info!("proxy not running, starting in background...");
        start_proxy_background(config).await?;
    }

    process::launch::launch_claude(config, &profile, model, args, hyperlinks)?;

    if let Some(log_path) = proxy::proxy_log_path() {
        if log_path.exists() {
            eprintln!("\nClaudex proxy log: {}", log_path.display());
        }
    }
    Ok(())
}

fn command_owns_terminal(command: &Option<Commands>, has_profiles: bool) -> bool {
    matches!(command, Some(Commands::Run { .. } | Commands::Dashboard))
        || (command.is_none() && has_profiles)
}

async fn start_proxy_background(config: &ClaudexConfig) -> Result<()> {
    let port = config.proxy_port;
    let host = config.proxy_host.clone();

    if proxy::is_proxy_reachable(&host, port).await {
        return Ok(());
    }

    // Spawn a proxy owned by this interactive process. It is deliberately not
    // registered as a daemon and will shut down with the Claude session.
    let config_clone = config.clone();
    tokio::spawn(async move {
        if let Err(e) = proxy::start_embedded_proxy(config_clone, None).await {
            tracing::error!("proxy failed: {e}");
        }
    });

    if proxy::wait_for_proxy(&host, port, std::time::Duration::from_secs(5)).await {
        tracing::info!("proxy is ready");
        return Ok(());
    }

    anyhow::bail!("proxy failed to start within 5 seconds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_commands_own_stderr() {
        let run = Some(Commands::Run {
            profile: "test".to_string(),
            model: None,
            hyperlinks: false,
            args: Vec::new(),
        });
        assert!(command_owns_terminal(&run, true));
        assert!(command_owns_terminal(&Some(Commands::Dashboard), true));
        assert!(command_owns_terminal(&None, true));
    }

    #[test]
    fn noninteractive_commands_keep_stderr_logging() {
        let command = Some(Commands::Profile {
            action: ProfileAction::List,
        });
        assert!(!command_owns_terminal(&command, true));
        assert!(!command_owns_terminal(&None, false));
    }

    #[test]
    fn bare_command_welcomes_when_no_profiles_exist() {
        assert_eq!(
            default_action(&ClaudexConfig::default()),
            DefaultAction::Welcome
        );
    }

    #[test]
    fn bare_command_runs_the_only_enabled_profile() {
        let config = ClaudexConfig {
            profiles: vec![config::ProfileConfig {
                name: "only".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            default_action(&config),
            DefaultAction::AutoRun("only".to_string())
        );
    }

    #[test]
    fn disabled_profiles_do_not_force_account_selection() {
        let config = ClaudexConfig {
            profiles: vec![
                config::ProfileConfig {
                    name: "only".to_string(),
                    ..Default::default()
                },
                config::ProfileConfig {
                    name: "old".to_string(),
                    enabled: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            default_action(&config),
            DefaultAction::AutoRun("only".to_string())
        );
    }

    #[test]
    fn bare_command_opens_dashboard_for_multiple_or_only_disabled_profiles() {
        let multiple = ClaudexConfig {
            profiles: vec![
                config::ProfileConfig {
                    name: "one".to_string(),
                    ..Default::default()
                },
                config::ProfileConfig {
                    name: "two".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(default_action(&multiple), DefaultAction::Dashboard);

        let disabled = ClaudexConfig {
            profiles: vec![config::ProfileConfig {
                name: "off".to_string(),
                enabled: false,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(default_action(&disabled), DefaultAction::Dashboard);
    }
}
