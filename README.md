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

Install stable Rust, then:

```sh
cargo build --release
install -m 0755 target/release/acp-tunnel ~/.local/bin/acp-tunnel
```

On the remote host, copy and edit
[`examples/config.toml`](examples/config.toml), then validate it:

```sh
acp-tunnel check-config --config /etc/acp-tunnel/config.toml
```

Start a loopback server behind a TLS reverse proxy:

```sh
acp-tunnel serve \
  --token-file /run/secrets/acp-tunnel-token \
  --listen 127.0.0.1:8787 \
  --config /etc/acp-tunnel/config.toml
```

Configure the local ACP client to launch one of these commands as its agent:

```sh
# Claude ACP
ACP_TUNNEL_TOKEN='the-same-token' \
  acp-tunnel connect \
  --url wss://agents.example.com/v1/tunnel \
  --agent claude \
  --workspace project-a

# Codex ACP
ACP_TUNNEL_TOKEN='the-same-token' \
  acp-tunnel connect \
  --url wss://agents.example.com/v1/tunnel \
  --agent codex \
  --workspace project-a

# Goose
ACP_TUNNEL_TOKEN='the-same-token' \
  acp-tunnel connect \
  --url wss://agents.example.com/v1/tunnel \
  --agent goose \
  --workspace project-a
```

The connector reads `$HOME/.config/acp-tunnel/token` by default. Protect this
file so that only your account can read it:

```sh
install -d -m 0700 "$HOME/.config/acp-tunnel"
install -m 0600 /secure/path/acp-tunnel-token \
  "$HOME/.config/acp-tunnel/token"

acp-tunnel connect \
  --url wss://agents.example.com/v1/tunnel \
  --agent codex \
  --workspace project-a
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

### Buzz preset

Buzz supplies three session variables to its custom harness. Use `--buzz` to
select these exact names:

```sh
acp-tunnel connect \
  --url wss://agents.example.com/v1/tunnel \
  --agent codex \
  --workspace project-a \
  --buzz
```

The preset selects `BUZZ_RELAY_URL`, `BUZZ_PRIVATE_KEY`, and `BUZZ_AUTH_TAG`.
The server agent must allowlist these names. The command fails if Buzz does not
supply one of these variables. You can add other names with `--client-env`.

```toml
[agents.codex]
client_env_allowlist = [
  "BUZZ_RELAY_URL",
  "BUZZ_PRIVATE_KEY",
  "BUZZ_AUTH_TAG",
]
```

The tunnel protocol and server policy remain generic. The `--buzz` option is a
client-side shortcut for the three names. It does not change their values or
bypass the server allowlist.

## Security model

The server authenticates the HTTP upgrade with a bearer token before it accepts
an opening request or starts a process. Both commands use `--token-file`,
`ACP_TUNNEL_TOKEN_FILE`, `ACP_TUNNEL_TOKEN`, or the default token file. The
default file is `$HOME/.config/acp-tunnel/token`. Explicit sources take
precedence. Tokens use a constant-time comparison. Tokens and authorization
headers are never logged. The client does not follow redirects.

Credential sources use this order:

1. `--token-file`
2. `ACP_TUNNEL_TOKEN_FILE`
3. `ACP_TUNNEL_TOKEN`
4. `$HOME/.config/acp-tunnel/token`

The loader rejects an explicit token file with `ACP_TUNNEL_TOKEN`. The direct
token takes precedence over the implicit default file.

The client supplies an agent ID and workspace ID. It can also offer variables
that the connector selects with `--client-env`. The server owns every
executable, argument, environment rule, and filesystem path. It clears the
agent environment and copies only server-approved names. It invokes the
executable directly without a shell.

Agent `pass_env` and fixed `env` values take precedence over client values. An
agent `client_env_allowlist` grants connection-specific control to authorized
clients. The server rejects unlisted, duplicate, and malformed entries. It
does not log environment values.

Browser `Origin` headers are rejected unless their exact value appears in
`allowed_origins`. Plaintext servers are restricted to loopback unless
`--insecure-listen` is explicitly supplied. A client requires `wss://` except
for loopback destinations.

The connector uses an explicit shutdown exchange for stdin EOF, SIGTERM,
SIGINT, Ctrl-C, and library shutdown requests. The server first closes the
agent stdin. It then escalates to SIGTERM and SIGKILL when the agent does not
exit. The connector waits for confirmation and sends a normal WebSocket close
frame.

On an unexpected network loss, the server keeps the complete agent process
group alive for `reconnect_grace_seconds`. The authenticated connector resumes
the same process and replays only unacknowledged ACP frames.

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

See [SECURITY.md](SECURITY.md) and the configuration
[security notes](docs/configuration.md#security-sensitive-options).

## MCP policy

Each agent selects one server-owned policy:

- `deny` replaces `params.mcpServers` with an empty array.
- `allowlisted` (default) matches each incoming `name` against `[mcp_servers]`
  and replaces client command and argument data. It keeps only client
  environment names in the server `client_env_allowlist`.
- `passthrough` forwards client MCP definitions unchanged.

Unknown allowlist names produce a JSON-RPC error with the original request ID.
Server `pass_env` and fixed `env` values override client values. An allowlisted
name lets any authenticated and authorized tunnel client select its value for
that allowlisted MCP process. Do not allowlist a name unless this control is
acceptable.
`passthrough` permits remote command execution and is unsafe for untrusted
clients. It requires `allow_insecure_mcp_passthrough = true` and emits a startup
warning.

## Workspace mapping

By default, the selected remote workspace path replaces `params.cwd` in
`session/new`, `session/load`, and `session/resume`. JSON-RPC IDs, `_meta`, and
unknown fields survive the edit. Set `rewrite_cwd = false` when local and remote
absolute paths are identical.

Workspace selection is path mapping, not synchronization. The repository must
already exist at the configured remote path.

## Reliability and privacy

The default ACP line and WebSocket message limit is 10 MiB. Bounded channels and
bounded line codecs apply backpressure. Each direction assigns sequence numbers
and acknowledges a frame only after flushing it to the next local pipe. During
reconnect, unacknowledged frames are replayed and duplicates are acknowledged
without being delivered twice. Replay storage is bounded by
`max_replay_frames` and `max_replay_bytes`.

Tunnel ping/pong messages detect dead peers. Agent stderr uses a bounded,
nonblocking path: when the diagnostic queue is full or the tunnel is detached,
stderr lines are dropped and the dropped count is logged at connection close.
ACP traffic is retained or backpressured instead.

Server logs include connection, agent, workspace, process, exit, frame, byte,
and error-category fields. They never include ACP payloads, prompts, token
values, authorization headers, or environment values.

Reconnect state is memory-only. It survives transient WebSocket and proxy
failures while both `acp-tunnel` processes remain alive, but it does not survive
a connector or server process restart.

The connector waits 10 seconds for `shutdown_complete` by default. Set
`--shutdown-timeout-seconds` to change this limit. If the timeout expires, the
connector closes the transport and exits nonzero. It does not resume normal
reconnect attempts.

## TLS and reverse proxies

For direct TLS, set `[tls].cert_path` and `[tls].key_path`. For a reverse proxy,
bind plain HTTP to loopback and terminate TLS at the proxy. The proxy must:

- use HTTP/1.1 to the upstream;
- forward WebSocket `Upgrade` and `Connection` headers;
- forward `Authorization` unchanged;
- disable response buffering;
- use an idle timeout longer than `keepalive_timeout_seconds`.

See [`examples/nginx.conf`](examples/nginx.conf).

## Troubleshooting

**ACP client reports malformed JSON:** Verify that only remote ACP lines reach
local stdout. Put client diagnostics and wrapper output on stderr. Do not add
shell startup messages around `acp-tunnel connect`.

**401 Unauthorized:** Load the same nonempty token in the connector and remote
service. Make sure that the proxy forwards `Authorization`.

**Token file fails to load:** Make sure that the file is readable and 16 KiB or
smaller. The default path is `$HOME/.config/acp-tunnel/token`. The file can end
with one LF or CRLF.

**Unknown or missing workspace:** Check the requested ID, the agent's
`workspaces` list, and the remote path. No files are copied by the tunnel.

**Agent fails to start:** Confirm `command` resolves under the configured
`pass_env` policy, the binary is executable, and the service user can enter the
workspace. Include `PATH` in `pass_env` when using an executable name.

**MCP policy rejection:** Add the incoming MCP `name` under `[mcp_servers]`,
choose `deny`, or use the explicitly insecure passthrough mode only for trusted
clients.

**Client environment rejection:** Add the selected name to the agent
`client_env_allowlist`. Remove the corresponding `--client-env` option if the
remote agent does not need the variable.

**The Buzz preset reports a missing variable:** Start the connector as the Buzz
custom harness. The preset fails when its process environment lacks a required
Buzz variable.

**Proxy closes long sessions:** The connector should report a successful
reconnect on stderr. Raise proxy read/send timeouts above the server keepalive
timeout, confirm WebSocket upgrades are forwarded, and ensure the connector's
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
