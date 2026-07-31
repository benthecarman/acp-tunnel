# acp-tunnel protocol v3

The tunnel protocol is independent of the ACP protocol version. It uses one
authenticated WebSocket at `GET /v1/tunnel`. Every application message is a
WebSocket text message containing one JSON object. Binary messages are invalid.

Protocol v3 includes authenticated transport resumption, ordered ACP replay,
and explicit remote-agent shutdown. Reconnect state is held in memory. It is
not a durable session store.

The server rejects all other tunnel versions. It does not negotiate or fall
back to protocol v2. Update the connector and server together.

## Authentication and upgrade

Every initial or resumed WebSocket sends:

```http
Authorization: Bearer <ACP_TUNNEL_TOKEN>
```

The server returns a generic `401 Unauthorized` for a missing or incorrect
bearer credential. Authentication occurs before the server reads an opening
message or starts or resumes an agent. Browser `Origin` is absent for the CLI
client and rejected by default.

## Initial handshake

The first message on a new tunnel is:

```json
{
  "type": "open",
  "tunnelVersion": 3,
  "agent": "codex",
  "workspace": "project-a",
  "clientInfo": {
    "name": "acp-tunnel",
    "version": "0.1.0"
  },
  "clientEnvironment": [
    {
      "name": "SESSION_ENDPOINT",
      "value": "https://session.example"
    },
    {
      "name": "SESSION_ACCESS_TOKEN",
      "value": "connection-secret"
    }
  ]
}
```

`clientEnvironment` is optional. The connector includes only variables that
the user selected with `--client-env`. The server accepts only names in the
selected agent `client_env_allowlist`.

The server rejects duplicate, malformed, and unlisted entries. Server
`pass_env` and fixed `env` values take precedence. Errors, logs, and Debug
output do not contain environment values.

After validating the version and allowlisted identifiers and starting exactly
one process, the server returns:

```json
{
  "type": "ready",
  "tunnelVersion": 3,
  "connectionId": "opaque-id",
  "resumeToken": "single-session-secret",
  "resumed": false
}
```

The resume token is a secret capability in addition to the bearer credential.
It is held only in memory, is never written to ACP stdout, and must not be
logged.

## Resumed handshake

After an unexpected socket failure, the connector opens a new authenticated
WebSocket and sends:

```json
{
  "type": "open",
  "tunnelVersion": 3,
  "agent": "codex",
  "workspace": "project-a",
  "clientInfo": {
    "name": "acp-tunnel",
    "version": "0.1.0"
  },
  "resume": {
    "connectionId": "opaque-id",
    "resumeToken": "single-session-secret"
  }
}
```

The server compares the resume token in constant time and verifies that the
agent and workspace IDs match the original session. A successful reattachment
returns the same connection ID and token with `"resumed": true`. A rejected or
expired credential returns a generic `resume_rejected` error.

A resume request must not contain `clientEnvironment`. The original agent
keeps the environment that the server applied during process creation.

Only one transport is active for a session. A newly authenticated transport
replaces an older transport with the same resume capability.

## Ordered ACP data

Each direction has an independent sequence beginning at 1:

```json
{
  "type": "acp",
  "sequence": 1,
  "payload": "{\"jsonrpc\":\"2.0\",...}"
}
```

The line terminator is not part of `payload`. Each receiver restores `\n`.
Messages can travel concurrently in both directions; the tunnel makes no
assumption about JSON-RPC request and response ordering.

After flushing an ACP line to its next local pipe, the receiver sends a
cumulative acknowledgement:

```json
{
  "type": "ack",
  "stream": "client_to_server",
  "sequence": 1
}
```

or:

```json
{
  "type": "ack",
  "stream": "server_to_client",
  "sequence": 1
}
```

The sender retains frames until acknowledged. On resume it replays retained
frames in sequence order. A receiver acknowledges replayed sequences below its
next expected sequence without writing them to the pipe again. A sequence gap
is a protocol error.

This gives exactly-once delivery to each local pipe across transient WebSocket
failures while both tunnel processes remain alive. It does not make the ACP
agent, ACP client, or their side effects transactional.

## Diagnostics

Remote standard error remains isolated:

```json
{"type":"stderr","payload":"diagnostic text"}
```

Diagnostics are not sequenced or replayed. The connector writes them only to
stderr. The server may drop stderr lines while detached or when its bounded
diagnostic queue is full. ACP replay traffic takes priority.

## Lifecycle and errors

An intentional connector shutdown starts with:

```json
{
  "type": "shutdown",
  "reason": "stdin_eof"
}
```

The initial reasons are `stdin_eof`, `sigterm`, `interrupt`, and
`client_shutdown`. A protocol v3 receiver accepts unknown reason strings for
future extensions.

The server removes the resume capability before it starts process cleanup. It
then flushes and closes the agent stdin. This gives the agent a bounded period
to exit. If the process remains alive, the server sends SIGTERM to the process
group. It sends SIGKILL after the configured shutdown timeout.

After it reaps the direct child, the server replies:

```json
{
  "type": "shutdown_complete",
  "code": 0,
  "signal": null
}
```

The connector waits for this envelope for its configured shutdown timeout.
Then both peers send a normal WebSocket close frame. A confirmed shutdown is
successful even when the final agent status is nonzero.

Process termination:

```json
{"type":"exit","code":0,"signal":null}
```

`code` can be `null` when a Unix signal terminated the process. `signal` is
`null` on platforms without Unix signals. If the child exits while detached,
the server retains pending ACP frames and the exit status until the resume grace
period expires.

Tunnel error:

```json
{"type":"error","code":"resume_rejected","message":"..."}
```

Error messages are diagnostics, not a stable programmatic API.

Keepalive:

```json
{"type":"ping","nonce":"opaque"}
{"type":"pong","nonce":"opaque"}
```

The peer copies the nonce unchanged. Native WebSocket ping/pong frames can also
be used by intermediaries.

## Client-selected agent environment

The connector reads selected local variables once during startup. It sends the
selected names and values only in the first `open` message. TLS protects these
values in transit on non-loopback connections.

The server builds the process environment from three sources:

1. The server selects host values through agent `pass_env`.
2. The server applies fixed agent `env` values.
3. The server adds allowlisted client values without replacing server values.

The server clears the inherited process environment before it applies these
sources. The client cannot supply a command, arguments, or a working directory.

## Limits and closure

Both sides enforce the configured WebSocket message and ACP line limit before
unbounded buffering; the default is 10 MiB. Replay queues have independent
frame-count and byte limits. When a queue fills, reading from the producing pipe
stops until acknowledgements provide space.

Local stdin EOF sends `shutdown` with reason `stdin_eof`. SIGTERM sends reason
`sigterm`. SIGINT and Ctrl-C send reason `interrupt`. An embedding application
can use the shutdown handle and reason `client_shutdown`.

If shutdown starts while the connector is detached, normal reconnect attempts
stop. The connector makes a bounded resume attempt only to send `shutdown`. It
does not start a new agent.

All WebSocket closes without a preceding `shutdown` are unexpected. An I/O
failure or keepalive expiry also detaches the transport.

The server retains the process for `reconnect_grace_seconds`. The connector
retries with exponential backoff until `--reconnect-timeout-seconds` expires.
Grace expiration or server shutdown terminates and reaps the complete remote
process group.

## ACP payload handling

Ordinary ACP payloads are forwarded as strings without deserializing or
reserializing them. When enabled, the server parses only:

- `session/new.params.cwd`
- `session/load.params.cwd`
- `session/resume.params.cwd`
- `session/new.params.mcpServers`

Those edits use a generic JSON tree around the typed
`agent-client-protocol` MCP definition, preserving unrelated fields and `_meta`.
