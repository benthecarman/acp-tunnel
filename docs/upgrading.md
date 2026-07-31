# Upgrade both tunnel endpoints

The tunnel protocol does not negotiate older versions. Install the same
published `acp-tunnel` version on the server and client.

## Stop active connectors

Close Buzz or the other ACP client. The connector sends the explicit shutdown
request, and the server terminates the remote agent.

Do not use SIGKILL for a normal upgrade. SIGKILL cannot start the shutdown
exchange.

## Upgrade the server

Install the latest published version:

```sh
cargo install acp-tunnel --locked --force
acp-tunnel --version
```

Restart the server service:

```sh
systemctl --user restart acp-tunnel.service
systemctl --user status acp-tunnel.service
```

If you use another supervisor, restart its `acp-tunnel serve` process instead.

## Upgrade the client

Install the published package on the machine that runs the ACP client:

```sh
cargo install acp-tunnel --locked --force
command -v acp-tunnel
acp-tunnel --version
```

Make sure that this version matches the server version. Then restart Buzz or
the other ACP client.

## Examine the upgraded connection

Run the public diagnostic from the client:

```sh
acp-tunnel doctor --url wss://agents.example.com/v1/tunnel
```

Then start one ACP session. A tunnel-version rejection means that the server
and connector use different versions. Install the same published version on
both machines. Then restart both processes.
