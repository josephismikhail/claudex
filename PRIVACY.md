# Privacy and network behavior

Claudex has no maintainer-operated collection service and does not send
analytics, crash reports, usage metrics, configuration, prompts, or terminal
output anywhere for telemetry. It forwards model content only to the provider
profiles you configure. It has no telemetry or crash-reporting SDK and performs
no automatic update checks.

## Default runtime guarantees

- Starting the dashboard or proxy does not contact configured providers.
- Provider health checks run only when you explicitly request a connectivity
  test.
- Every Claude Code process launched by Claudex has telemetry, Sentry error
  reporting, feedback, surveys, feature-flag fetching, automatic updates,
  official marketplace installation, Claude.ai MCP connectors, hosted
  artifacts, and OpenTelemetry exporters disabled.
- Claude Code's WebFetch hostname preflight to `api.anthropic.com` is disabled
  with a command-line settings overlay. User settings are preserved, but the
  privacy keys are enforced.
- Logs remain local. Request bodies are excluded unless you explicitly set
  `CLAUDEX_LOG_REQUEST_BODIES=1`.

## Explicit network operations

Claudex is an API proxy, so model use is not offline unless your configured
provider is local (for example Ollama, LM Studio, or vLLM). Network access can
still occur when you explicitly:

- send a model request to a configured remote provider;
- use an OAuth-backed provider, including login and required token refreshes;
- add or update a configuration set from a Git or HTTP URL;
- run a connectivity test;
- run a Claude Code tool that accesses the network, such as WebFetch or a
  remote MCP server; or
- install Claudex from GitHub Releases.

The removed `claudex update` command cannot check GitHub from the installed
binary. Updating requires deliberately running an installer again or replacing
the binary yourself.

For a hard offline boundary, use only loopback provider URLs and enforce an OS
firewall allowlist. Application-level switches cannot stop a shell command,
plugin, hook, or MCP server chosen by the user from opening its own connection.
