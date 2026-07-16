# Claudex

Claudex is a local multi-provider coding-agent terminal. It owns the interface
you see while retaining Claude Code's installed tool, skill, MCP, and subagent
harness underneath. Connect OpenAI and Anthropic in one session, then switch
between every model exposed to those accounts with `/model`.

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

That command opens **Joey's Claudex** and displays the Claudex version. It does
not display Claude Code's logo, version, model aliases, or billing label. A new
installation starts with no preset models and the status `/model to
authenticate`.

1. Type `/model`.
2. Choose OpenAI or Anthropic.
3. Complete authentication in the browser window that opens.
4. Choose one of the models returned for that account.

After the first account is connected, `/model` stacks every connected
provider's models in one picker. **Authenticate another LLM provider** at the
bottom returns to the provider list. The chosen default and connected accounts
persist across exits; bare `claudex` never asks a one-account user to select an
account or profile.

Other in-session commands are provider-aware:

- `/effort` selects `low`, `medium`, `high`, `ultracode`, or `max`.
  `ultracode` registers proactive researcher, worker, and reviewer subagents
  that inherit the selected provider model, adds parallel-delegation guidance,
  and maps API reasoning effort to `xhigh` where supported.
- `/fast` is available when OpenAI is connected or the Anthropic account exposes
  Claude Opus 4.8. It is one session toggle, but the gateway chooses the fast
  implementation for each selected model: OpenAI routes use
  `service_tier: "priority"` (about 1.5x), while Anthropic Opus 4.8 routes use
  `speed: "fast"` with Anthropic's required research-preview beta header (up
  to 2.5x output speed). Other Anthropic models remain at standard speed.
- `/usage` fetches a live OpenAI subscription snapshot and shows the percentage
  remaining in each returned usage window and when it resets.

`/usage` becomes unavailable when the OpenAI account is removed. `/fast`
remains available if an eligible Anthropic route is still connected.
Anthropic fast mode requires provider access and premium billing. See the
[OpenAI fast-mode documentation](https://learn.chatgpt.com/docs/agent-configuration/speed.md)
and [Anthropic fast-mode documentation](https://platform.claude.com/docs/en/build-with-claude/fast-mode).

Connected accounts survive exits and restarts. Bare `claudex` always returns to
the unified session, whether one provider or several are connected.

## Provider authentication

| Provider | Setup | Models |
|---|---|---|
| OpenAI | Browser OAuth using ChatGPT sign-in | Picker-visible models returned for that ChatGPT account |
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
- Account metadata is saved atomically in the platform Claudex configuration
  directory as `accounts.json`. That file contains provider names and model
  IDs, never tokens or API keys.
- Tokens and API keys are stored in Windows Credential Manager, macOS Keychain,
  or the platform keyring on Linux.
- `/model`, `/effort`, `/fast`, and `/usage` are handled by the local Claudex
  terminal. Compatibility skills loaded into the underlying harness do not
  replace commands in ordinary Claude Code sessions.
- `/fast` state is a small per-session JSON file under
  `~/.config/claudex/sessions/`. It contains only a version and an on/off value,
  is selected through a random loopback-only ID, and is removed when the Claude
  process exits.
- The proxy and browser manager bind to loopback by default. Mutating browser
  requests require the exact same origin.
- Provider endpoints are not probed at startup. A provider's model catalog is
  requested only when the user explicitly connects that provider.

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

The local terminal talks to the installed agent harness, and model traffic goes
through the loopback Claudex gateway:

```text
Joey's Claudex terminal
    │  streaming agent protocol
    ▼
installed Claude agent harness
    │  Anthropic Messages API
    ▼
127.0.0.1:13456
    ├── selected GPT model ───────► OpenAI Responses API
    └── selected Claude model ────► Anthropic Messages API
```

The catalog is rebuilt in memory when an account is added or removed. Requests
are routed by exact model ID, so the selected model can change providers while
the same conversation and subagent harness remain active. `ultracode` is sent
to the harness as `xhigh` and translated to OpenAI
`reasoning.effort = "xhigh"`.

When `/fast` is on, the same translation path adds
`service_tier = "priority"` to connected OpenAI subscription routes. Official
Anthropic Console routes receive `speed = "fast"` plus
`anthropic-beta: fast-mode-2026-02-01` only for Claude Opus 4.8. The local
gateway strips client-supplied fast fields, so unsupported providers and Claude
models cannot opt themselves into premium processing.

The `/model` picker is owned by Claudex and refreshes while authentication is in
progress. It never synthesizes fake Opus, Sonnet, or Haiku rows for non-Claude
models. Claude Code 2.1.129 or newer is required for the streaming harness.

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

Legacy profiles stay out of the account-first `/model` picker and can be
launched explicitly with `claudex run <legacy-profile>`. Claude subscription
OAuth profiles are rejected; use an Anthropic Console API key instead. See
[config.example.toml](./config.example.toml) for advanced provider
configuration.

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
