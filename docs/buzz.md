# Buzz custom harness

This guide connects Buzz on a client machine to an agent on the remote server.
Complete the [server setup](server-setup.md) before you configure Buzz.

## Configure the server for Buzz

The easiest method is the initializer preset:

```sh
acp-tunnel init \
  --agent codex \
  --agent-command codex-acp \
  --workspace project-a \
  --workspace-path /srv/workspaces/project-a \
  --buzz
```

For an existing configuration, add the Buzz names to the selected agent:

```toml
[agents.codex]
client_env_allowlist = [
  "BUZZ_RELAY_URL",
  "BUZZ_PRIVATE_KEY",
  "BUZZ_AUTH_TAG",
]
```

Buzz selects the values for each new connection. The server accepts only the
listed names. Server `pass_env` and fixed `env` values take precedence.

Restart the server after a configuration change:

```sh
systemctl --user restart acp-tunnel.service
```

## Prepare the client

Install `acp-tunnel` on the machine that runs Buzz. Then make sure that the
command and token are available:

```sh
command -v acp-tunnel
test -r "$HOME/.config/acp-tunnel/token" && echo "token readable"
```

If the token is absent, copy the same token from the server:

```sh
install -d -m 0700 "$HOME/.config/acp-tunnel"
scp user@server.example:~/.config/acp-tunnel/token \
  "$HOME/.config/acp-tunnel/token"
chmod 0600 "$HOME/.config/acp-tunnel/token"
```

Do not add the token to the Buzz configuration. The connector reads the
default token file.

## Enter the custom-harness fields

Create a custom harness in Buzz. Use the absolute path from
`command -v acp-tunnel` as the executable.

For example:

```text
Executable
/home/ben/.cargo/bin/acp-tunnel
```

Add each command argument as a separate item, in this order:

```text
connect
--url
wss://agents.example.com/v1/tunnel
--agent
codex
--workspace
project-a
--buzz
```

Replace the URL, agent ID, and workspace ID with the server values. Do not put
the complete command in the executable field.

Do not add `BUZZ_RELAY_URL`, `BUZZ_PRIVATE_KEY`, or `BUZZ_AUTH_TAG` manually.
Buzz supplies these values to the custom-harness process. The `--buzz` option
selects their names for transport.

## Examine the connection

Start a new Buzz session. The connector writes connection messages to stderr.
It reserves stdout for ACP messages.

The following Buzz notice alone does not prove that the tunnel failed:

```text
Using built-in model options. Could not load live models for this provider.
```

Live model discovery depends on the remote ACP agent. Start a session to
examine the complete connection path.

## Troubleshooting

### Buzz reports a missing session variable

Make sure that Buzz starts the connector as its custom harness. A terminal
process does not receive the session variables that Buzz creates.

Make sure that the command arguments include `--buzz`. Then make sure that the
server agent allowlists all three Buzz names.

### The server rejects a client environment name

Restart the server after you edit its configuration. Then make sure that Buzz
uses the agent ID that contains the matching `client_env_allowlist`.

### The connector receives 401 Unauthorized

Make sure that the client and server use the same token. Then make sure that
the TLS proxy forwards the `Authorization` header.
