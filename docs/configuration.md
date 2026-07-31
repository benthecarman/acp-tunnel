# Configuration reference

The server reads TOML. Run `acp-tunnel check-config --config PATH` before
deployment. Unknown configuration keys are errors.

## Top-level fields

| Field | Default | Meaning |
|---|---:|---|
| `listen` | `127.0.0.1:8787` | HTTP/HTTPS socket address |
| `max_frame_bytes` | `10485760` | ACP line and WebSocket message limit |
| `connection_timeout_seconds` | `10` | TCP/WebSocket opening timeout |
| `keepalive_interval_seconds` | `15` | Tunnel ping interval |
| `keepalive_timeout_seconds` | `45` | Maximum interval without peer traffic |
| `shutdown_timeout_seconds` | `5` | Grace period before force-kill |
| `reconnect_grace_seconds` | `30` | Time a detached agent waits for resume |
| `max_replay_frames` | `256` | Retained unacknowledged frames per direction |
| `max_replay_bytes` | `20971520` | Retained unacknowledged payload bytes |
| `channel_capacity` | `32` | Backpressured ACP outbound queue |
| `diagnostic_channel_capacity` | `64` | Reserved diagnostic capacity |
| `diagnostic_line_bytes` | `65536` | Maximum remote stderr line |
| `rewrite_cwd` | `true` | Map lifecycle cwd to remote path |
| `allowed_origins` | `[]` | Exact allowed browser Origin values |
| `allow_insecure_mcp_passthrough` | `false` | Acknowledge MCP RCE risk |

`keepalive_timeout_seconds` must exceed the interval. Limits and timeouts must be
positive. `max_replay_bytes` must be at least `max_frame_bytes`.

## Agents

```toml
[agents.codex]
command = "codex-acp"
args = []
workspaces = ["project-a"]
pass_env = ["PATH", "HOME", "OPENAI_API_KEY"]
env = { NO_BROWSER = "1" }
mcp_policy = "allowlisted"
```

`command`, `args`, `pass_env`, and `env` are entirely server-owned. Commands are
invoked directly, never concatenated and never passed to a shell. The child
environment is cleared before the allowlisted variables and fixed values are
added. A variable cannot appear in both `pass_env` and `env`.

When `command` is not absolute, include `PATH` in `pass_env`. Include platform
variables such as `HOME` only when the configured agent needs them.

Agent and workspace IDs match `[a-z0-9][a-z0-9_-]*`.

## Workspaces

```toml
[workspaces.project-a]
path = "/srv/workspaces/project-a"
```

Paths must be absolute and must exist before a tunnel opens. The service account
must have the required access. Configuration validation does not create, clone,
or synchronize the directory.

## MCP servers

```toml
[mcp_servers.developer-tools]
command = "/usr/local/bin/developer-mcp"
args = []
pass_env = ["API_TOKEN"]
env = { LOG_FORMAT = "json" }
```

In `allowlisted` mode, the incoming MCP `name` selects this table entry. All
incoming executable, argument, path, and environment fields are discarded.
`pass_env` values are copied by the server into ACP's `{name,value}` environment
array. These values are sensitive and are never logged.

## Direct TLS

```toml
[tls]
cert_path = "/etc/acp-tunnel/tls/fullchain.pem"
key_path = "/etc/acp-tunnel/tls/privkey.pem"
```

The certificate must cover the hostname used by clients. The private key must be
readable only by the service account. Without this section, bind to loopback
behind a TLS reverse proxy. Plaintext on a non-loopback address requires
`--insecure-listen`.

## Security-sensitive options

`allowed_origins` should remain empty for non-browser ACP clients. If needed,
entries are exact strings such as `https://trusted.example`; no wildcard or
suffix matching occurs.

`allow_insecure_mcp_passthrough = true` only acknowledges the risk. An agent
must also select `mcp_policy = "passthrough"`. Passthrough allows the client to
provide commands and environment values that the remote agent may execute.

The bearer token is not part of TOML. Both `connect` and `serve` use these
credential sources, in this order:

1. The CLI `--token-file` path.
2. The `ACP_TUNNEL_TOKEN_FILE` path.
3. The direct `ACP_TUNNEL_TOKEN` value.

Do not provide a token file and `ACP_TUNNEL_TOKEN` together. The commands reject
missing credentials and empty credentials. A token file must be 16 KiB or
smaller. It can end with one LF or CRLF. Other spaces remain part of the token.
Embedded newlines are invalid.

On Unix, the command warns when a token file is group-readable or
world-readable. It does not reject the file because container secret mounts can
require group-readable permissions.

The CLI accepts only a token-file path. It never accepts a token value.

Resume credentials are generated per tunnel, kept only in server and connector
memory, compared in constant time, and never configured or logged. Set
`reconnect_grace_seconds` slightly longer than the connector's
`--reconnect-timeout-seconds` to leave room for the final connection attempt.
