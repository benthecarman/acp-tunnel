# Run an ACP agent with Docker Compose

This guide adds `acp-tunnel` to an existing ACP agent image. The same container
runs the tunnel server and the allowlisted agent.

The repository Dockerfile builds a small tunnel-only image. It does not contain
an ACP agent. The pattern below starts from the existing agent image instead.

The server starts the agent as a direct child process. This design preserves
stdio transport, process cleanup, environment rules, and reconnect behavior.

Do not mount the Docker socket in the tunnel container. Docker socket access
gives the container extensive control of the host.

## Deployment layout

The example uses this layout:

```text
acp-tunnel/
├── Dockerfile.agent
├── compose.yaml
├── deploy/
│   └── config.toml
├── Cargo.toml
├── Cargo.lock
└── src/
```

The existing agent image must contain each configured ACP agent binary. The
configuration must use the path or executable name from that image.

## 1. Extend the agent image

Create `Dockerfile.agent` in the repository root:

```dockerfile
# syntax=docker/dockerfile:1

ARG AGENT_IMAGE

FROM rust:1.88-bookworm AS tunnel-builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM ${AGENT_IMAGE}
COPY --from=tunnel-builder \
    /build/target/release/acp-tunnel \
    /usr/local/bin/acp-tunnel
ENTRYPOINT ["/usr/local/bin/acp-tunnel"]
```

Set `AGENT_IMAGE` to the image that your team already uses. This image must
contain `codex-acp`, `claude-agent-acp`, `goose`, or another ACP agent.

The final stage inherits the user, filesystem, and installed tools from the
agent image. Make sure that this user can read and write the workspace.

## 2. Create the server configuration

Create `deploy/config.toml`:

```toml
listen = "0.0.0.0:8787"
keepalive_interval_seconds = 15
keepalive_timeout_seconds = 45
reconnect_grace_seconds = 35

[agents.codex]
command = "codex-acp"
args = []
workspaces = ["project-a"]
pass_env = ["PATH", "HOME", "OPENAI_API_KEY"]
env = { NO_BROWSER = "1" }
mcp_policy = "allowlisted"

[workspaces.project-a]
path = "/srv/workspaces/project-a"
```

Change the agent ID, command, and environment names for your agent image. Keep
all commands, arguments, and environment rules in this server-owned file.

The workspace path is a path inside the container. The Compose file must mount
the remote repository at this exact path.

## 3. Create the Compose file

Create `compose.yaml`:

```yaml
services:
  acp-tunnel:
    build:
      context: .
      dockerfile: Dockerfile.agent
      args:
        AGENT_IMAGE: ${AGENT_IMAGE:?Set AGENT_IMAGE}
    command:
      - serve
      - --token-file
      - /run/secrets/acp-tunnel-token
      - --config
      - /etc/acp-tunnel/config.toml
      - --insecure-listen
    environment:
      OPENAI_API_KEY: ${OPENAI_API_KEY:-}
      RUST_LOG: ${RUST_LOG:-acp_tunnel=info}
    secrets:
      - acp-tunnel-token
    volumes:
      - ./deploy/config.toml:/etc/acp-tunnel/config.toml:ro
      - /srv/workspaces/project-a:/srv/workspaces/project-a:rw
    ports:
      - "127.0.0.1:8787:8787"
    init: true
    restart: unless-stopped
    stop_signal: SIGTERM
    stop_grace_period: 30s
    security_opt:
      - no-new-privileges:true
    cap_drop:
      - ALL
    pids_limit: 512

secrets:
  acp-tunnel-token:
    file: ./deploy/acp-tunnel-token
```

This example publishes the server only on the host loopback interface. A host
reverse proxy can connect to `127.0.0.1:8787`.

`--insecure-listen` permits plaintext traffic on the container interface. The
host port remains private because the `ports` entry uses `127.0.0.1`.

If the proxy runs in Compose, remove `ports` and add `expose: ["8787"]`. Connect
the proxy and tunnel services to one private Compose network.

Replace `OPENAI_API_KEY` with the credential names for the selected agent.
Each name must also occur in the agent `pass_env` list.

## 4. Build the image

Set the existing agent image:

```sh
export AGENT_IMAGE='your-registry.example/team/codex-acp:approved-tag'
```

Create a file that contains a long random bearer token:

```sh
openssl rand -hex 32 > deploy/acp-tunnel-token
chmod 0600 deploy/acp-tunnel-token
```

Set the agent credential:

```sh
export OPENAI_API_KEY='replace-with-the-agent-credential'
```

Build the image:

```sh
docker compose build
```

Make sure that the server configuration is valid:

```sh
docker compose run --rm acp-tunnel \
  check-config --config /etc/acp-tunnel/config.toml
```

Do not commit bearer tokens, API keys, OAuth files, or generated `.env` files.
Use the secret manager of your team for production deployment.

## 5. Start the service

Start the tunnel server:

```sh
docker compose up -d
```

Read the startup logs:

```sh
docker compose logs acp-tunnel
```

Make sure that both health endpoints return HTTP status 200:

```sh
curl --fail http://127.0.0.1:8787/healthz
curl --fail http://127.0.0.1:8787/readyz
```

If the agent binary is missing, the server reports an agent-start error when a
client opens a tunnel. The `check-config` command does not start the agent.

## 6. Configure TLS

The local connector requires `wss://` for a non-loopback destination. Put a TLS
reverse proxy in front of the loopback port.

The proxy must preserve these properties:

- Forward the `Authorization` header unchanged.
- Forward the WebSocket `Upgrade` and `Connection` headers.
- Use HTTP/1.1 for the upstream connection.
- Disable response buffering.
- Set proxy timeouts longer than `keepalive_timeout_seconds`.

Use the [Nginx example](../examples/nginx.conf) as a starting point. Replace the
hostname and certificate paths before deployment.

## 7. Connect an ACP client

Install `acp-tunnel` on the client host or in the ACP client container. Configure
the ACP client to run:

```sh
ACP_TUNNEL_TOKEN_FILE="$HOME/.config/acp-tunnel/token" \
  acp-tunnel connect \
  --url wss://agents.example.com/v1/tunnel \
  --agent codex \
  --workspace project-a
```

The local connector writes only ACP messages to stdout. It writes connection
status, remote stderr, and errors to stderr.

## Claude and other agent images

Use one configuration entry for each agent binary in the image. For Claude,
replace the sample agent section:

```toml
[agents.claude]
command = "claude-agent-acp"
args = []
workspaces = ["project-a"]
pass_env = ["PATH", "HOME", "ANTHROPIC_API_KEY"]
env = { NO_BROWSER = "1" }
mcp_policy = "allowlisted"
```

Then replace `OPENAI_API_KEY` with `ANTHROPIC_API_KEY` in `compose.yaml`.

If one image contains both agent binaries, one tunnel service can expose both
agent IDs. If your team uses separate images, run one tunnel service per image.

Use a different host port or hostname for each separate service. Each service
can keep an independent bearer token and workspace allowlist.

## Credential files and writable home directories

Some agent images store credentials or caches below the home directory. Mount
only the required directory, and use the narrowest filesystem permissions.

If an agent refreshes its credential, the credential mount must be writable.
Otherwise, mount the credential file or directory as read-only.

The tunnel server clears the child environment before it adds allowlisted
values. Add `HOME`, `PATH`, and required credentials to `pass_env`.

## Security notes

- Pin the agent base image by digest for production builds.
- Keep the tunnel port on loopback or a private Compose network.
- Terminate TLS before traffic leaves the trusted host or network.
- Do not mount `/var/run/docker.sock`.
- Do not use privileged containers.
- Mount only allowlisted workspaces.
- Keep `mcp_policy = "allowlisted"` or use `deny`.
- Do not use MCP passthrough for untrusted clients.
- Protect the bearer token separately from agent credentials.

The remote container is part of the trusted computing base. It can read ACP
traffic, prompts, credentials, and all mounted workspace files.

Compose sends SIGTERM and waits for `stop_grace_period`. Keep this period longer
than the server shutdown timeout. SIGKILL cannot run the explicit shutdown
exchange or flush agent stdin.

Container isolation does not synchronize files. The selected workspace must
already exist on the remote host.

## Troubleshooting

**The client cannot connect:** Make sure that the proxy uses WebSocket upgrade
headers. Make sure that the proxy forwards the `Authorization` header.

**The container rejects the listener:** Include `--insecure-listen` for
plaintext `0.0.0.0` behind the TLS proxy.

**The agent executable is missing:** Use an agent image that contains the
configured command. Use an absolute command path if `PATH` does not contain it.

**The agent cannot read the repository:** Make sure that the container user can
access the mounted workspace. Make sure that the container path matches
`[workspaces]`.

**Authentication fails:** Load the same token for the service and local
connector. Make sure that the proxy forwards the header unchanged.

**The agent loses its login:** Mount the required credential directory. Make
the mount writable only when the agent must refresh its credential.

**Reconnect stops too early:** Set `reconnect_grace_seconds` slightly higher
than the connector value for `--reconnect-timeout-seconds`.

**Container shutdown leaves work running:** Keep `stop_signal: SIGTERM`. Set
`stop_grace_period` long enough for stdin closure, SIGTERM, and final cleanup.
