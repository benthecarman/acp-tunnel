# Server setup

This guide configures `acp-tunnel` for one user on a Linux server. The server
keeps its listener on loopback. A separate TLS proxy provides remote access.

## Install the command

Install stable Rust. Then install the published package:

```sh
cargo install acp-tunnel --locked
```

Make sure that the installed command has the expected path:

```sh
command -v acp-tunnel
acp-tunnel --version
```

The generated user service records this executable path. Do not generate the
service from a temporary build directory.

## Generate the configuration

Change to the workspace that the remote agent will use:

```sh
cd /srv/workspaces/project-a
acp-tunnel init
```

The initializer creates these private files:

```text
~/.config/acp-tunnel/config.toml
~/.config/acp-tunnel/token
```

The initializer does not replace an existing token. It requires `--force` to
replace an existing configuration.

For a noninteractive Buzz configuration, run:

```sh
acp-tunnel init \
  --agent codex \
  --agent-command codex-acp \
  --workspace project-a \
  --workspace-path /srv/workspaces/project-a \
  --buzz
```

The `--buzz` option allowlists the three Buzz session variables for the agent.
It does not change the tunnel protocol.

### Select an MCP policy

The configuration field defaults to `allowlisted` when the field is absent.
The initializer explicitly writes `passthrough` for client compatibility.
These defaults apply in different situations.

CAUTION: MCP passthrough lets authenticated clients supply remote MCP commands.
Select `allowlisted` or `deny` if clients must not have this control.

Use this option during noninteractive initialization:

```sh
acp-tunnel init --mcp-policy allowlisted
```

An allowlisted policy also requires server definitions under `[mcp_servers]`.
Read the [configuration reference](configuration.md#mcp-servers) for details.

## Examine the server configuration

Run the diagnostic before you start the server:

```sh
acp-tunnel doctor
```

The listener result is successful when the address is available. The command
also examines the token, workspace, and agent executable.

The command reports MCP passthrough as a warning. It does not start the agent or
print credentials.

## Do a foreground test

Start the server in one terminal:

```sh
acp-tunnel serve
```

Examine the health endpoints from a second terminal:

```sh
curl --fail http://127.0.0.1:8787/healthz
curl --fail http://127.0.0.1:8787/readyz
```

Press Ctrl-C in the first terminal after the test. Stop this process before you
start a service on the same listener address.

## Install a systemd user service

Generate the service from the installed executable:

```sh
install -d -m 0700 "$HOME/.config/systemd/user"
acp-tunnel service generate --user \
  > "$HOME/.config/systemd/user/acp-tunnel.service"
systemctl --user daemon-reload
systemctl --user enable --now acp-tunnel.service
```

Keep the user service active after logout on a headless server:

```sh
sudo loginctl enable-linger "$USER"
```

Read the service status and recent logs:

```sh
systemctl --user status acp-tunnel.service
journalctl --user -u acp-tunnel.service -n 100 --no-pager
```

The generated service sends SIGTERM and waits 30 seconds before systemd can
send SIGKILL. SIGKILL cannot start the explicit tunnel shutdown exchange.

For a system service, adapt the hardened
[service example](../examples/acp-tunnel.service). That unit uses explicit
configuration and token paths.

## Interpret later diagnostic results

The listener diagnostic reports a warning after the service starts. The
warning means that another process uses the configured address. This result is
expected when the service is active.

Use `systemctl` and the health endpoints to examine a running local service.
Use `doctor --url` from the client to examine the public endpoint.

The public diagnostic sends no credential. It expects `401 Unauthorized` from
the protected WebSocket route. This response does not prove that the proxy
forwards authenticated requests.

## Next steps

1. Configure a [Tailscale Serve endpoint](tailscale.md) or another TLS proxy.
2. Copy the token to the client as described in the
   [README](../README.md#4-copy-the-token-to-the-client).
3. Configure the [Buzz custom harness](buzz.md) or another ACP client.
