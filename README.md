# Claudex

Claudex is a local multi-provider model gateway for Claude Code. Start one
Claude Code session, connect providers from `/models`, and switch between their
models with `/model` without restarting or choosing a profile.

This is a Windows-first stability fork of
[StringKe/claudex](https://github.com/StringKe/claudex). It retains the upstream
MIT license and adds native PowerShell support, crash-safe local state, bounded
terminal buffering, strict privacy defaults, and in-session provider setup.

## Install

Windows PowerShell (x64):

```powershell
irm https://raw.githubusercontent.com/josephismikhail/claudex/main/install.ps1 | iex
```

Linux or macOS:

```bash
curl -fsSL https://raw.githubusercontent.com/josephismikhail/claudex/main/install.sh | bash
```

Claude Code must also be installed and available as `claude` in `PATH`.

## First run

```powershell
claudex
```

That command immediately opens Claude Code. There is no setup wizard, profile
picker, or required model selection. A new installation starts with an empty
provider catalog; the temporary onboarding response is generated inside the
loopback proxy and makes no provider request.

Inside Claude Code:

1. Run `/models`.
2. Pick OpenAI or Anthropic in the local browser page.
3. Finish authentication.
4. Copy the displayed `/model <model-id>` command to switch immediately in
   the same session. On later launches, bare `/model` lists the persisted
   models in its picker.

Connected accounts survive exits and restarts. Bare `claudex` always returns to
the unified session, whether one provider or several are connected.

## Provider authentication

| Provider | Setup | Models |
|---|---|---|
| OpenAI | Browser OAuth using ChatGPT sign-in | GPT-5.6 |
| Anthropic API | Opens Anthropic Console; paste an API key into the localhost page | Discovered from Anthropic's Models API for that key |

OpenAI credentials are stored in the operating system credential store and
refreshed when required. This follows the browser sign-in and persistent local
credential behavior described in the
[official OpenAI authentication documentation](https://learn.chatgpt.com/docs/auth).

Anthropic does not permit third-party gateways to route requests through Claude
Free, Pro, or Max subscription credentials. Claudex therefore supports
Anthropic through a Console API key only; see Anthropic's
[authentication and credential-use policy](https://code.claude.com/docs/en/legal-and-compliance).

## What is local

- The account manager is served from the Claudex loopback proxy. It contains no
  remote scripts, fonts, images, or analytics.
- Account metadata is saved atomically in
  `~/.config/claudex/accounts.json`. That file contains provider names and model
  IDs, never tokens or API keys.
- Tokens and API keys are stored in Windows Credential Manager, macOS Keychain,
  or the platform keyring on Linux.
- The `/models` command is installed as a managed personal Claude Code skill at
  `~/.claude/skills/models/SKILL.md`. An existing user-authored `/models` skill
  is preserved; Claudex installs `/claudex-models` instead.
- The proxy and browser manager bind to loopback by default. Mutating browser
  requests require the exact same origin.
- Provider endpoints are not probed at startup. Anthropic's model catalog is
  requested only when the user explicitly connects an Anthropic API key.

Model prompts necessarily leave the machine when a remote model is selected.
OAuth also contacts the chosen provider. Claudex itself has no analytics,
crash-reporting service, background update check, or maintainer-operated
collection endpoint. See [PRIVACY.md](./PRIVACY.md) for the exact boundary.

## Windows behavior

Claudex runs natively in PowerShell; WSL is not required. Claude Code processes
launched on Windows receive `CLAUDE_CODE_USE_POWERSHELL_TOOL=1`, and the
PowerShell installer:

- downloads the matching GitHub Release asset;
- verifies its SHA-256 checksum;
- installs `claudex.exe` under the current user; and
- adds the install directory to the user's `PATH` unless disabled.

## How model switching works

Claude Code talks only to the loopback Claudex gateway:

```text
Claude Code
    │  Anthropic Messages API
    ▼
127.0.0.1:13456
    ├── GPT model ───────► OpenAI Responses API
    └── Claude model ────► Anthropic Messages API
```

The catalog exposed to Claude Code is rebuilt in memory when an account is
added or removed. Requests are routed by exact model ID, so the selected model
can change providers while Claude Code keeps the same conversation and
subagent harness. Claude Code's `ultracode` effort is translated to GPT-5.6
`reasoning.effort = "xhigh"`.

Claudex enables Claude Code's gateway model discovery automatically. Claude
Code refreshes that picker at process startup, so a provider added during the
very first empty session is selected immediately with `/model <model-id>`; its
clickable picker entry is present on the next `claudex` launch. Gateway picker
discovery requires Claude Code 2.1.129 or newer.

## Privacy enforcement

Every Claude Code child process launched by Claudex disables nonessential
traffic, feedback and bug commands, surveys, automatic updates, marketplace
auto-installation, hosted artifacts, OpenTelemetry exporters, and the WebFetch
hostname preflight. Request bodies are excluded from local logs unless
`CLAUDEX_LOG_REQUEST_BODIES=1` is explicitly set.

## Advanced compatibility

The old profile engine remains available for custom local endpoints, OpenAI-
compatible APIs, routers, and existing installations. Its CLI commands are
hidden from normal help so new users do not have to understand profiles:

```powershell
claudex run <legacy-profile>
claudex profile list
claudex config show
```

Existing enabled profiles automatically appear in the unified bare session.
Claude subscription OAuth profiles are rejected; use an Anthropic Console API
key instead. See [config.example.toml](./config.example.toml) for advanced
provider configuration.

## Development

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

CI runs the Rust checks on Windows and Ubuntu and builds the documentation site
without analytics or update checks.

## License

[MIT](./LICENSE)
