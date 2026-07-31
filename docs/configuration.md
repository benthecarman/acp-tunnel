# Configuration reference

The server reads `$HOME/.config/acp-tunnel/config.toml` by default. Use
`--config PATH` to select a different file. Run `acp-tunnel check-config`
before deployment. Unknown configuration keys are errors.

## Generate a configuration

Run `acp-tunnel init` for an interactive setup. The command creates a token and
one agent with one workspace. It resolves the agent executable and makes the
generated files private.

The initializer uses these defaults:

- Listener: `127.0.0.1:8787`
- Inherited agent environment: `HOME` and `PATH`
- MCP policy: `passthrough`
- Configuration: `$HOME/.config/acp-tunnel/config.toml`
- Token: `$HOME/.config/acp-tunnel/token`

CAUTION: MCP passthrough lets an authenticated client supply remote commands.
Use `--mcp-policy allowlisted` or `--mcp-policy deny` to prevent this control.

Use `--buzz` to add the three fixed Buzz session names. The server still
enforces `client_env_allowlist` for each opening request.

The command refuses to replace an existing configuration. Use `--force` only
when you intend to replace that exact file. The command refuses configuration
symlinks, including with `--force`.

Run `acp-tunnel doctor` after initialization. Use `--url` to examine the public
URL scheme, DNS result, TCP connection, and WebSocket route. The route must
reject the doctor's unauthenticated request. The doctor does not send a
credential or start an agent.

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
client_env_allowlist = ["SESSION_ENDPOINT", "SESSION_ACCESS_TOKEN"]
mcp_policy = "allowlisted"
```

`command`, `args`, `pass_env`, and `env` are entirely server-owned. Commands are
invoked directly, never concatenated and never passed to a shell. The child
environment is cleared before the allowlisted variables and fixed values are
added. A variable cannot appear in both `pass_env` and `env`.

When `command` is not absolute, include `PATH` in `pass_env`. Include platform
variables such as `HOME` only when the configured agent needs them.

Agent and workspace IDs match `[a-z0-9][a-z0-9_-]*`.

### Client-selected agent environment

Use `--client-env NAME` on the connector to select one local variable. Repeat
the option to select more variables. The connector reads each selected value
once during startup. A missing variable is an error.

For a Buzz custom harness, `--buzz` selects `BUZZ_RELAY_URL`,
`BUZZ_PRIVATE_KEY`, and `BUZZ_AUTH_TAG`. The preset is equivalent to three
`--client-env` options. It does not select variables by prefix. The server
agent must allowlist all three names.

The agent `client_env_allowlist` controls the names that the server accepts.
The server rejects unlisted, duplicate, and malformed names. The opening
request does not accept commands, arguments, working directories, or other
environment variables.

The server constructs the agent environment in this order:

1. It copies available host values selected by `pass_env`.
2. It applies fixed server `env` values.
3. It adds accepted client values.

The first two server-owned sources take precedence over client values. The
server uses client values only when it starts a new agent. Resume requests
cannot add or change environment values.

Each allowlisted name lets an authenticated and authorized client select its
value for that agent process. Add a name only when this client control is
acceptable. Environment values never occur in errors, logs, or Debug output.

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
args = ["serve"]
pass_env = ["PATH"]
env = { LOG_FORMAT = "json" }
client_env_allowlist = [
  "PROJECT_URL",
  "PROJECT_TOKEN",
  "AGENT_DISPLAY_NAME",
]
```

In `allowlisted` mode, the incoming MCP `name` selects this table entry. All
incoming executable, argument, and path fields are discarded. The server parses
the incoming environment as an ACP object or a `{name,value}` array.

The server constructs the MCP environment in this order:

1. It copies available host values selected by `pass_env`.
2. It applies fixed server `env` values.
3. It adds client values whose names occur in `client_env_allowlist`.

The first two server-owned sources take precedence over client values. The
server rejects duplicate client names and malformed entries. It does not copy
unlisted names. Environment values never occur in errors or logs.

An allowlisted name lets any authenticated and authorized tunnel client select
its value for that allowlisted MCP process. Add a name only when every client
that can select the agent and workspace can also select this value.

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

The connector uses `--shutdown-timeout-seconds 10` by default. During
intentional shutdown, it waits for `shutdown_complete` for this duration. A
timeout closes the transport and produces a nonzero exit.

The connector sends client-selected agent environment only in the initial
`open` message. Use `wss://` for all non-loopback connections. The server
removes the retained values after it starts the agent.

Set the server `shutdown_timeout_seconds` high enough for the agent and its
process group. The server closes agent stdin before it sends SIGTERM. It sends
SIGKILL only after the shutdown timeout expires.
