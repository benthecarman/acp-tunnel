# Upgrade both tunnel endpoints

The tunnel protocol does not negotiate older versions. Install the same
`acp-tunnel` commit on the server and client before you resume normal use.

Before the first stable release, `acp-tunnel --version` can be identical for
different commits. Use the source commit to identify an exact build.

## Stop active connectors

Close Buzz or the other ACP client. The connector sends the explicit shutdown
request, and the server terminates the remote agent.

Do not use SIGKILL for a normal upgrade. SIGKILL cannot start the shutdown
exchange.

## Select one source commit

In the source checkout, update the branch and record the commit:

```sh
git pull --ff-only
git rev-parse HEAD
```

Use this exact commit on both machines.

## Upgrade the server

Install the selected checkout:

```sh
cargo install --path . --locked --force
```

Restart the server service:

```sh
systemctl --user restart acp-tunnel.service
systemctl --user status acp-tunnel.service
```

If you use another supervisor, restart its `acp-tunnel serve` process instead.

## Upgrade the client

Install the same checkout on the machine that runs the ACP client:

```sh
cargo install --path . --locked --force
command -v acp-tunnel
acp-tunnel --version
```

Restart Buzz or the other ACP client after installation.

## Examine the upgraded connection

Run the public diagnostic from the client:

```sh
acp-tunnel doctor --url wss://agents.example.com/v1/tunnel
```

Then start one ACP session. A tunnel-version rejection means that the server
and connector use different builds. Install the selected commit on both
machines, and then restart both processes.
