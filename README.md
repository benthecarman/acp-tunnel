# acp-tunnel

`acp-tunnel` makes a local-command ACP agent available on another machine. It
does not require changes to the ACP client or agent, and it does not use SSH.

```text
Any ACP client
      │ ACP over local stdio
      ▼
acp-tunnel connect
      │ authenticated WebSocket
      ▼
acp-tunnel serve
      │ ACP over remote stdio
      ▼
Configured ACP agent
```

One binary provides:

```text
acp-tunnel connect
acp-tunnel serve
acp-tunnel check-config
acp-tunnel init
acp-tunnel doctor
acp-tunnel service generate --user
```

ACP messages are carried as complete NDJSON lines. Ordinary messages remain
opaque. The server inspects only `session/new`, `session/load`, and
`session/resume` when configured path or MCP policy requires it. The project
uses the same `agent-client-protocol` Rust dependency used by Goose to construct
canonical server-owned MCP stdio definitions.

This project tunnels ACP. It does not synchronize repositories or files. Agents,
binaries, credentials, and workspaces must already exist on the remote host.
This is not the official ACP remote HTTP transport.

## Quick start

Install stable Rust on the server and the client. Run this command from the
same `acp-tunnel` checkout on both machines:

```sh
cargo install --path . --locked
```

Make sure that the installed command is available:

```sh
command -v acp-tunnel
acp-tunnel --version
```

### 1. Configure the server

On the remote host, change to the workspace directory. Then start the
initializer:

```sh
cd /srv/workspaces/project-a
acp-tunnel init
```

The initializer prompts for the agent ID, executable, workspace, environment,
and MCP policy. It creates these files:

```text
~/.config/acp-tunnel/config.toml
~/.config/acp-tunnel/token
```

The token and configuration use private file permissions. The initializer
refuses to replace an existing configuration unless you use `--force`.

CAUTION: The initializer selects MCP passthrough for compatibility. An
authenticated client can supply MCP commands for remote execution. Select
`allowlisted` or `deny` if clients must not have this control.

For a noninteractive Buzz setup, run:

```sh
acp-tunnel init \
  --agent codex \
  --agent-command codex-acp \
  --workspace project-a \
  --workspace-path /srv/workspaces/project-a \
  --buzz
```

This command resolves the executable to an absolute path. It also allowlists
the three fixed Buzz session variables for the agent.

Run the server diagnostics before you start the listener:

```sh
acp-tunnel doctor
```

The listener check reports that the address is available. This result is
expected before the service starts.

### 2. Start the server service

Generate and start a systemd user service:

```sh
install -d -m 0700 "$HOME/.config/systemd/user"
acp-tunnel service generate --user \
  > "$HOME/.config/systemd/user/acp-tunnel.service"
systemctl --user daemon-reload
systemctl --user enable --now acp-tunnel.service
```

Do not also run `acp-tunnel serve` in a terminal. Both processes use the same
listener address.

For a headless server, keep the user service active after logout:

```sh
sudo loginctl enable-linger "$USER"
```

The generated unit sends SIGTERM and waits 30 seconds before systemd can use
SIGKILL. Read the complete [server setup guide](docs/server-setup.md) for
foreground tests, system services, and diagnostic results.

### 3. Add a secure WebSocket endpoint

The server listens on `127.0.0.1:8787` by default. Add a TLS reverse proxy
before you connect from another machine.

For a private tailnet endpoint, use the
[Tailscale Serve guide](docs/tailscale.md). For a conventional reverse proxy,
use the [Nginx example](examples/nginx.conf).

### 4. Copy the token to the client

The client and server must use the same token. On the client, copy the token
through SSH or another approved secret-transfer system:

```sh
install -d -m 0700 "$HOME/.config/acp-tunnel"
scp user@server.example:~/.config/acp-tunnel/token \
  "$HOME/.config/acp-tunnel/token"
chmod 0600 "$HOME/.config/acp-tunnel/token"
```

Do not paste the token into the ACP client configuration. The connector reads
`$HOME/.config/acp-tunnel/token` by default.

### 5. Examine the public endpoint

Run this command on the client:

```sh
acp-tunnel doctor --url wss://agents.example.com/v1/tunnel
```

The public test does not send the token. A successful test receives
`401 Unauthorized`. This response proves that DNS, TLS, routing, and
authentication enforcement work. It does not prove that authenticated
requests work.

### 6. Configure the ACP client

Configure the local ACP client to run this command as its agent:

```sh
acp-tunnel connect \
  --url wss://agents.example.com/v1/tunnel \
  --agent codex \
  --workspace project-a
```

For Buzz, add `--buzz` and use the exact field layout in the
[Buzz custom-harness guide](docs/buzz.md).

The connector writes only ACP messages to stdout. It writes connection status,
remote stderr, and errors to stderr.

## Manual server command

The `serve` and `check-config` commands read the default configuration file.
Use `--config PATH` to select a different file.

To use explicit paths, first edit the
[example configuration](examples/config.toml). Then run these commands:

```sh
acp-tunnel check-config --config /etc/acp-tunnel/config.toml
acp-tunnel serve \
  --token-file /run/secrets/acp-tunnel-token \
  --listen 127.0.0.1:8787 \
  --config /etc/acp-tunnel/config.toml
```

Some agents need values that the local ACP client selects for each connection.
Select each local variable by name on the connector:

```sh
SESSION_ENDPOINT=https://session.example \
SESSION_ACCESS_TOKEN=connection-secret \
  acp-tunnel connect \
    --url wss://agents.example.com/v1/tunnel \
    --agent codex \
    --workspace project-a \
    --client-env SESSION_ENDPOINT \
    --client-env SESSION_ACCESS_TOKEN
```

The selected agent configuration must list the same names in
`client_env_allowlist`. The connector does not send other local variables.

## Security model

The server authenticates the HTTP upgrade with a bearer token before it accepts
an opening request or starts a process. Tokens use a constant-time comparison.
Tokens and authorization headers are never logged.

Credential sources use this order:

1. `--token-file`
2. `ACP_TUNNEL_TOKEN_FILE`
3. `ACP_TUNNEL_TOKEN`
4. `$HOME/.config/acp-tunnel/token`

The loader rejects an explicit token file with `ACP_TUNNEL_TOKEN`. The direct
token takes precedence over the implicit default file.

The server owns agent commands, arguments, environment rules, and filesystem
paths. It clears the agent environment and copies only approved names. It never
runs an agent command through a shell.

```text
explicit shutdown
    → remote process terminates immediately

unexpected network failure
    → remote process remains resumable during the grace period
```

This process-group cleanup covers grandchildren on Linux and macOS. Windows
supports the local `connect` command. The server targets Linux and macOS.

The remote host is part of the trusted computing base. It sees ACP traffic,
prompts, workspace files, agent credentials, and tool activity. Protect the
host, configuration, token, TLS keys, logs, and reverse proxy accordingly.

Read [SECURITY.md](SECURITY.md) and the configuration
[security notes](docs/configuration.md#security-sensitive-options) for the
complete trust model.

## MCP policy

Each agent selects one server-owned policy. If `mcp_policy` is absent, the
configuration field defaults to `allowlisted`. The initializer explicitly
writes `passthrough` for compatibility.

- `deny` replaces `params.mcpServers` with an empty array.
- `allowlisted` (default) matches each incoming `name` against `[mcp_servers]`
  and replaces client command and argument data. It keeps only client
  environment names in the server `client_env_allowlist`.
- `passthrough` forwards client MCP definitions unchanged.

Unknown allowlist names produce a JSON-RPC error with the original request ID.
Server `pass_env` and fixed `env` values override client values. An allowlisted
name lets any authenticated and authorized tunnel client select its value for
that allowlisted MCP process.

CAUTION: `passthrough` permits remote command execution. Use it only for trusted
clients. Read the [MCP configuration](docs/configuration.md#mcp-servers) before
you select or define an allowlist.

## Workspace mapping

By default, the selected remote workspace path replaces `params.cwd` in
`session/new`, `session/load`, and `session/resume`. JSON-RPC IDs, `_meta`, and
unknown fields survive the edit. Set `rewrite_cwd = false` when local and remote
absolute paths are identical.

Workspace selection is path mapping, not synchronization. The repository must
already exist at the configured remote path.

## Reliability and privacy

Reconnect state is memory-only. It survives transient WebSocket and proxy
failures while both `acp-tunnel` processes remain alive, but it does not survive
a connector or server process restart. Replay storage, messages, lines, and
channels have configured limits.

Logs never include ACP payloads, prompts, token values, authorization headers,
or environment values. Read the [protocol guide](docs/protocol.md) for replay,
shutdown, message limits, and diagnostic behavior.

## TLS and reverse proxies

For direct TLS, set `[tls].cert_path` and `[tls].key_path`. For a reverse proxy,
bind plain HTTP to loopback and terminate TLS at the proxy. The proxy must:

- use HTTP/1.1 to the upstream
- forward WebSocket `Upgrade` and `Connection` headers
- forward `Authorization` unchanged
- disable response buffering
- use an idle timeout longer than `keepalive_timeout_seconds`

See [`examples/nginx.conf`](examples/nginx.conf).

## Troubleshooting

**ACP client reports malformed JSON:** Make sure that only remote ACP lines reach
local stdout. Put client diagnostics and wrapper output on stderr. Do not add
shell startup messages around `acp-tunnel connect`.

**401 Unauthorized:** Load the same nonempty token in the connector and remote
service. Make sure that the proxy forwards `Authorization`.

**Token file fails to load:** Make sure that the file is readable and 16 KiB or
smaller. The default path is `$HOME/.config/acp-tunnel/token`. The file can end
with one LF or CRLF.

**Server setup is incomplete:** Run `acp-tunnel doctor`. Add `--url` to examine
the public hostname, DNS result, TCP connection, and protected WebSocket route.

**Unknown or missing workspace:** Make sure that the requested ID matches the
configuration. Make sure that the agent lists the workspace. Make sure that the
remote path exists. No files are copied by the tunnel.

**Agent fails to start:** Make sure that `command` resolves under the configured
`pass_env` policy. Make sure that the binary is executable. Make sure that the
service user can enter the workspace. Include `PATH` in `pass_env` when you use
an executable name.

**MCP policy rejection:** Select one response:

- Add the incoming MCP `name` under `[mcp_servers]`.
- Select `deny` to disable client MCP servers.
- Select passthrough only for trusted clients.

**Client environment rejection:** Add the selected name to the agent
`client_env_allowlist`. Remove the corresponding `--client-env` option if the
remote agent does not need the variable.

**The Buzz preset reports a missing variable:** Start the connector as the Buzz
custom harness. The preset fails when its process environment lacks a required
Buzz variable.

**Proxy closes long sessions:** The connector reports a successful reconnect on
stderr. Increase the proxy timeouts above the server keepalive timeout. Make
sure that the proxy forwards WebSocket upgrades. Make sure that the connector
reconnect timeout does not exceed the server grace period.

**A supervisor leaves a remote agent running:** Configure the supervisor to
close connector stdin or send SIGTERM. Wait longer than the connector shutdown
timeout before you send SIGKILL. SIGKILL cannot start the shutdown exchange.

**Shutdown confirmation times out:** Make sure that the connector can reach the
server during the full shutdown timeout. The server reconnect grace remains the
final cleanup fallback after a failed cleanup resume.

## Documentation

- [Tunnel protocol](docs/protocol.md)
- [Configuration reference](docs/configuration.md)
- [Server setup](docs/server-setup.md)
- [Buzz custom harness](docs/buzz.md)
- [Tailscale Serve](docs/tailscale.md)
- [Upgrade both endpoints](docs/upgrading.md)
- [Docker Compose deployment](docs/docker-compose.md)
- [Example configuration](examples/config.toml)
- [systemd service](examples/acp-tunnel.service)
- [Nginx reverse proxy](examples/nginx.conf)

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Tests use only local sockets and the binary's hidden fake ACP agent. They do not
need vendor agents, containers, databases, or external network access.

Licensed under the MIT License.
