# Use acp-tunnel with Buzz

This guide configures `acp-tunnel` as a custom agent harness in Buzz Desktop.
Buzz runs the connector locally. The selected ACP agent runs on the server.

```text
Buzz Desktop
    → buzz-acp
    → acp-tunnel connect on the Buzz machine
    → acp-tunnel serve on the server
    → configured remote ACP agent
```

Complete the [server setup](server-setup.md) before you configure Buzz.

## 1. Configure the server for Buzz

For a new server configuration, use the initializer preset:

```sh
acp-tunnel init \
  --agent codex \
  --agent-command codex-acp \
  --workspace project-a \
  --workspace-path /srv/workspaces/project-a \
  --buzz
```

The example uses `codex` as the public agent ID. The command and workspace must
exist on the server.

If the configuration already exists, add these names to the selected agent:

```toml
[agents.codex]
client_env_allowlist = [
  "BUZZ_RELAY_URL",
  "BUZZ_PRIVATE_KEY",
  "BUZZ_AUTH_TAG",
]
```

Add this list to the agent section, not an MCP server section. Buzz selects the
three values for each managed-agent connection.

Make sure that the edited configuration is valid:

```sh
acp-tunnel check-config
```

Then restart the server:

```sh
systemctl --user restart acp-tunnel.service
systemctl --user status acp-tunnel.service
```

## 2. Prepare the Buzz machine

Install the published connector on the machine that runs Buzz:

```sh
cargo install acp-tunnel --locked
```

Get its absolute path:

```sh
command -v acp-tunnel
```

The result usually resembles this path on Linux:

```text
/home/alice/.cargo/bin/acp-tunnel
```

You will enter the exact result in Buzz.

### Install the tunnel token

The Buzz machine and the server must use the same tunnel token. Copy the token
through SSH or another approved secret-transfer system:

```sh
install -d -m 0700 "$HOME/.config/acp-tunnel"
scp user@server.example:~/.config/acp-tunnel/token \
  "$HOME/.config/acp-tunnel/token"
chmod 0600 "$HOME/.config/acp-tunnel/token"
```

Make sure that the token is readable:

```sh
test -r "$HOME/.config/acp-tunnel/token" && echo "token readable"
```

Do not paste the token into Buzz. Do not add `--token-file` unless you use a
nondefault path.

### Examine the endpoint

Run the unauthenticated endpoint diagnostic from the Buzz machine:

```sh
acp-tunnel doctor \
  --url wss://agents.example.com/v1/tunnel
```

A successful public diagnostic receives `401 Unauthorized`. This response
proves that the protected WebSocket route is reachable. It does not test the
token.

## 3. Open the Buzz custom-harness form

You can open the form while you create an agent:

1. Open **Agents**.
2. Select **New agent**.
3. Select **Create agent**.
4. Open **Customize for this agent**.
5. Open **Agent harness**.
6. Select **Add custom harness…**.

You can also register the harness before you create an agent:

1. Open **Settings**.
2. Open **Agents**.
3. Select **Add runtimes**.
4. Select **Custom harness**.

Both routes open the same form.

## 4. Enter the harness definition

Enter these values:

| Buzz field | Value |
|---|---|
| **Name** | `ACP Tunnel` |
| **ID** | `acp-tunnel` (Buzz derives this value from the name) |
| **Command** | The absolute result from `command -v acp-tunnel` |
| **Env vars** | Leave empty |
| **Docs URL** | `https://github.com/benthecarman/acp-tunnel` (optional) |
| **Install hint** | `cargo install acp-tunnel --locked` (optional) |

Do not put the complete connector command in **Command**. This field contains
only the executable path.

Under **Arguments**, select **Add argument** eight times. Enter one value in
each row, in this exact order:

| Row | Value |
|---:|---|
| 1 | `connect` |
| 2 | `--url` |
| 3 | `wss://agents.example.com/v1/tunnel` |
| 4 | `--agent` |
| 5 | `codex` |
| 6 | `--workspace` |
| 7 | `project-a` |
| 8 | `--buzz` |

Replace row 3 with your tunnel URL. Replace rows 5 and 7 with the IDs from the
server configuration.

Leave **Env vars** empty. Buzz injects its managed session variables when it
starts the connector.

Select **Save**. If you opened the form from **Create agent**, Buzz selects the
new harness automatically.

If you registered the harness through **Settings**, select **ACP Tunnel** when
you create or edit the agent.

## 5. Start the Buzz agent

Save the Buzz agent and start its managed harness. Buzz launches the connector
with its relay URL, private key, and authorization tag.

The `--buzz` option selects these exact names for the tunnel:

```text
BUZZ_RELAY_URL
BUZZ_PRIVATE_KEY
BUZZ_AUTH_TAG
```

The connector does not read other Buzz variables. The server accepts these
values only because the selected agent allowlists their names.

Do not run the `--buzz` connector command directly in a terminal. A terminal
does not receive the managed session variables from Buzz.

## Model selection notice

Buzz can display this notice during agent setup:

```text
Using built-in model options. Could not load live models for this provider.
```

This notice does not mean that the tunnel failed. It means that live model
discovery did not return a catalog that Buzz can use.

Select a built-in model option and start the agent. Then use the first managed
session to examine the complete tunnel path.

## Troubleshooting

### Buzz reports `Not found on PATH`

Use the absolute result from `command -v acp-tunnel` in **Command**. Desktop
applications often use a smaller `PATH` than terminal applications.

### The Save button is disabled

Remove each empty argument or environment row. Buzz requires a value in every
row that you add.

### The connector rejects `--token-file`

The Buzz machine has an old `acp-tunnel` binary. Upgrade it and restart Buzz:

```sh
cargo install acp-tunnel --locked --force
acp-tunnel --version
```

### The connector receives `401 Unauthorized`

Make sure that the client and server use the same token. Then make sure that
the TLS proxy forwards the `Authorization` header.

### The remote agent reports a missing Buzz variable

Make sure that row 8 contains `--buzz`. Then make sure that the selected server
agent allowlists all three Buzz names.

Restart `acp-tunnel serve` after you edit its configuration. Also restart the
Buzz agent so that Buzz starts a new connector process.

### The server rejects the agent or workspace

Make sure that argument rows 5 and 7 match the server IDs. These values are
identifiers, not filesystem paths or display names.

### The public diagnostic returns `401 Unauthorized`

This result is correct for `doctor --url`. The diagnostic intentionally sends
no bearer token.
