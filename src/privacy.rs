use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};

/// Environment policy enforced for every Claude Code process Claudex launches.
///
/// Keep the individual switches as well as Anthropic's aggregate switch. The
/// redundancy covers older Claude Code releases and prevents an inherited
/// shell or profile setting from silently re-enabling an exporter.
pub const PRIVATE_ENVIRONMENT: &[(&str, &str)] = &[
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ("DISABLE_TELEMETRY", "1"),
    ("DO_NOT_TRACK", "1"),
    ("DISABLE_ERROR_REPORTING", "1"),
    ("DISABLE_FEEDBACK_COMMAND", "1"),
    ("DISABLE_BUG_COMMAND", "1"),
    ("CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY", "1"),
    ("DISABLE_GROWTHBOOK", "1"),
    ("DISABLE_AUTOUPDATER", "1"),
    ("DISABLE_UPDATES", "1"),
    ("DISABLE_UPGRADE_COMMAND", "1"),
    ("CLAUDE_CODE_ENABLE_TELEMETRY", "0"),
    ("OTEL_METRICS_EXPORTER", "none"),
    ("OTEL_LOGS_EXPORTER", "none"),
    ("OTEL_TRACES_EXPORTER", "none"),
    ("CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL", "1"),
    ("FORCE_AUTOUPDATE_PLUGINS", "0"),
    ("ENABLE_CLAUDEAI_MCP_SERVERS", "false"),
    ("CLAUDE_CODE_DISABLE_ARTIFACT", "1"),
    ("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1"),
    ("CLAUDE_CODE_IDE_SKIP_AUTO_INSTALL", "1"),
    ("CLAUDE_CODE_PACKAGE_MANAGER_AUTO_UPDATE", "0"),
    ("CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION", "false"),
    ("CLAUDE_CODE_ENABLE_AWAY_SUMMARY", "0"),
];

/// Apply the privacy environment after profile-provided variables so a stale
/// profile cannot accidentally turn telemetry back on.
pub fn apply_private_environment(command: &mut Command) {
    for (name, value) in PRIVATE_ENVIRONMENT {
        command.env(name, value);
    }
}

/// Ensure Claude Code receives a command-line settings overlay that disables
/// its WebFetch hostname preflight to api.anthropic.com.
///
/// Existing `--settings` JSON or files are preserved and augmented. Every
/// occurrence is rewritten so normal last-argument precedence cannot bypass
/// the privacy policy.
pub fn enforce_private_settings(args: &[String]) -> Result<Vec<String>> {
    let mut rewritten = Vec::with_capacity(args.len() + 2);
    let mut found_settings = false;
    let mut options_enabled = true;
    let mut index = 0;

    while index < args.len() {
        let arg = &args[index];

        if options_enabled && arg == "--" {
            options_enabled = false;
            rewritten.push(arg.clone());
            index += 1;
            continue;
        }

        if options_enabled && arg == "--settings" {
            let raw = args
                .get(index + 1)
                .context("--settings requires a JSON object or file path")?;
            rewritten.push(arg.clone());
            rewritten.push(private_settings_json(raw)?);
            found_settings = true;
            index += 2;
            continue;
        }

        if options_enabled {
            if let Some(raw) = arg.strip_prefix("--settings=") {
                rewritten.push(format!("--settings={}", private_settings_json(raw)?));
                found_settings = true;
                index += 1;
                continue;
            }
        }

        rewritten.push(arg.clone());
        index += 1;
    }

    if !found_settings {
        rewritten.insert(0, private_settings_json("{}")?);
        rewritten.insert(0, "--settings".to_string());
    }

    Ok(rewritten)
}

fn private_settings_json(raw: &str) -> Result<String> {
    let mut settings = load_settings(raw)?;
    let root = settings
        .as_object_mut()
        .context("Claude Code settings must be a JSON object")?;

    // Claude Code documents this as the separate opt-out for the WebFetch
    // safety preflight, which is not covered by its nonessential-traffic flag.
    root.insert("skipWebFetchPreflight".to_string(), Value::Bool(true));

    let env = root
        .entry("env")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .context("Claude Code settings 'env' value must be a JSON object")?;
    for (name, value) in PRIVATE_ENVIRONMENT {
        env.insert((*name).to_string(), Value::String((*value).to_string()));
    }

    serde_json::to_string(&settings).context("failed to serialize private Claude Code settings")
}

fn load_settings(raw: &str) -> Result<Value> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed).context("invalid inline Claude Code settings JSON");
    }

    if trimmed.is_empty() {
        bail!("Claude Code settings path cannot be empty");
    }

    let path = Path::new(trimmed);
    let contents = fs::read_to_string(path).with_context(|| {
        format!(
            "failed to read Claude Code settings from {}",
            path.display()
        )
    })?;
    // Windows editors commonly preserve a UTF-8 BOM in JSON files. Accept it
    // here even though serde_json (correctly) treats it as non-JSON input.
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(&contents);
    serde_json::from_str(contents)
        .with_context(|| format!("invalid Claude Code settings JSON in {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_value(args: &[String]) -> Value {
        let index = args.iter().position(|arg| arg == "--settings").unwrap();
        serde_json::from_str(&args[index + 1]).unwrap()
    }

    #[test]
    fn inserts_private_settings_when_missing() {
        let args = vec!["--print".to_string(), "hello".to_string()];
        let rewritten = enforce_private_settings(&args).unwrap();
        let settings = settings_value(&rewritten);

        assert_eq!(settings["skipWebFetchPreflight"], true);
        assert_eq!(settings["env"]["DISABLE_TELEMETRY"], "1");
        assert_eq!(settings["env"]["OTEL_METRICS_EXPORTER"], "none");
        assert!(rewritten.ends_with(&args));
    }

    #[test]
    fn preserves_inline_settings_and_overrides_privacy_keys() {
        let args = vec![
            "--settings".to_string(),
            r#"{"theme":"dark","skipWebFetchPreflight":false,"env":{"DISABLE_TELEMETRY":"0","CUSTOM":"yes"}}"#.to_string(),
        ];
        let rewritten = enforce_private_settings(&args).unwrap();
        let settings = settings_value(&rewritten);

        assert_eq!(settings["theme"], "dark");
        assert_eq!(settings["skipWebFetchPreflight"], true);
        assert_eq!(settings["env"]["DISABLE_TELEMETRY"], "1");
        assert_eq!(settings["env"]["CUSTOM"], "yes");
    }

    #[test]
    fn augments_settings_files_without_mutating_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"theme":"light"}"#).unwrap();
        let args = vec![
            "--settings".to_string(),
            path.to_string_lossy().into_owned(),
        ];

        let rewritten = enforce_private_settings(&args).unwrap();
        let settings = settings_value(&rewritten);
        assert_eq!(settings["theme"], "light");
        assert_eq!(settings["skipWebFetchPreflight"], true);
        assert_eq!(fs::read_to_string(path).unwrap(), r#"{"theme":"light"}"#);
    }

    #[test]
    fn accepts_utf8_bom_in_windows_settings_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, "\u{feff}{\"theme\":\"light\"}").unwrap();
        let args = vec![
            "--settings".to_string(),
            path.to_string_lossy().into_owned(),
        ];

        let rewritten = enforce_private_settings(&args).unwrap();
        assert_eq!(settings_value(&rewritten)["theme"], "light");
    }

    #[test]
    fn rewrites_equals_form() {
        let args = vec![r#"--settings={"theme":"dark"}"#.to_string()];
        let rewritten = enforce_private_settings(&args).unwrap();
        let json = rewritten[0].strip_prefix("--settings=").unwrap();
        let settings: Value = serde_json::from_str(json).unwrap();
        assert_eq!(settings["theme"], "dark");
        assert_eq!(settings["skipWebFetchPreflight"], true);
    }

    #[test]
    fn does_not_treat_prompt_content_after_separator_as_settings() {
        let args = vec![
            "--".to_string(),
            "--settings".to_string(),
            "not-a-file".to_string(),
        ];
        let rewritten = enforce_private_settings(&args).unwrap();
        assert_eq!(&rewritten[rewritten.len() - args.len()..], args);
        assert_eq!(settings_value(&rewritten)["skipWebFetchPreflight"], true);
    }

    #[test]
    fn command_environment_overrides_inherited_values() {
        let mut command = Command::new("claude");
        command.env("DISABLE_TELEMETRY", "0");
        apply_private_environment(&mut command);

        let value = command
            .get_envs()
            .find_map(|(name, value)| {
                (name == "DISABLE_TELEMETRY").then(|| value.unwrap().to_owned())
            })
            .unwrap();
        assert_eq!(value, "1");
    }
}
