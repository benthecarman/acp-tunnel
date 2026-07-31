# Security policy

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use the repository's
private vulnerability-reporting form under **Security → Advisories → Report a
vulnerability**. Include:

- the affected version or commit;
- the deployment model and operating system;
- reproduction steps or a minimal proof of concept;
- the expected and observed security impact;
- any suggested mitigation.

You should receive an acknowledgement within three business days. Maintainers
will coordinate validation, fixes, release timing, and disclosure. Please allow
reasonable time for a patch before public disclosure.

## Supported versions

Until the first stable release, only the latest commit and most recent release
receive security fixes.

## Security boundaries

The remote host and its configured agents are trusted. `acp-tunnel` does not
sandbox agents, synchronize files, inspect prompts, or restrict tools used by an
agent. Use operating-system accounts, filesystem permissions, network policy,
and secret isolation as additional controls.

MCP `passthrough` mode allows client-controlled remote command execution. It is
not suitable for untrusted clients.

In `allowlisted` MCP mode, `client_env_allowlist` grants narrow client control.
Each listed name lets any authenticated and authorized tunnel client select the
value for that allowlisted MCP process. Server `pass_env` and fixed `env` values
take precedence. The server rejects malformed or duplicate entries and removes
all unlisted client environment variables.

An agent `client_env_allowlist` grants the same narrow control for the remote
agent process. The connector sends only names selected with `--client-env`.
The server rejects unlisted, duplicate, and malformed entries. Server
`pass_env` and fixed `env` values take precedence.

An allowlisted agent environment name lets each authorized client select its
value for one new remote process. Do not allowlist a name if that client
control is not acceptable. Resume requests cannot change process environment.

Client-selected values travel in the authenticated opening message. Use TLS
for every non-loopback connection. Environment values have redacted Debug
output and never occur in errors or logs.

Reconnect uses a random, per-session resume capability in addition to the
bearer token. Treat this capability as a secret. It is intentionally memory-only
and expires when the session ends or its reconnect grace period elapses.

An explicit shutdown removes the resume capability before process cleanup.
Unexpected transport loss keeps the capability active for the configured grace
period. A WebSocket close without a `shutdown` envelope is unexpected.

Supervisors must close connector stdin or send SIGTERM before SIGKILL. SIGKILL
cannot be caught, so it cannot request immediate remote cleanup.

Load the bearer token from `--token-file`, `ACP_TUNNEL_TOKEN_FILE`,
`ACP_TUNNEL_TOKEN`, or `$HOME/.config/acp-tunnel/token`. The default file is the
last fallback. A file keeps the token out of process environment listings and
application configuration. Restrict the file permissions when the runtime does
not require group-readable secret mounts.

The `--buzz` option selects three fixed environment names. It does not select a
prefix. The agent `client_env_allowlist` remains authoritative for every name.

The token type has redacted `Debug` output. Authentication uses a constant-time
comparison, and HTTP authorization headers are marked as sensitive.

Do not include bearer tokens, authorization headers, ACP payloads, prompts,
environment values, private keys, or MCP secrets in reports sent through public
channels.
