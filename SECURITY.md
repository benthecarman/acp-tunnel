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

Reconnect uses a random, per-session resume capability in addition to the
bearer token. Treat this capability as a secret. It is intentionally memory-only
and expires when the session ends or its reconnect grace period elapses.

Do not include bearer tokens, authorization headers, ACP payloads, prompts,
environment values, private keys, or MCP secrets in reports sent through public
channels.
