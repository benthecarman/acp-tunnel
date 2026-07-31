# Tailscale Serve

This guide exposes the loopback tunnel listener to devices in one Tailscale
tailnet. Tailscale terminates TLS and proxies the WebSocket connection.

Tailscale Serve is private to the tailnet. Do not use Tailscale Funnel unless
you intend to expose the endpoint to the public internet.

The commands in this guide follow the current [Tailscale Serve CLI
reference](https://tailscale.com/docs/reference/tailscale-cli/serve).

## Requirements

- The server and client are connected to the same tailnet.
- The tailnet access policy permits the client to reach the server.
- HTTPS certificates are enabled for the tailnet.
- `acp-tunnel serve` listens on `127.0.0.1:8787`.

Make sure that the local server is ready:

```sh
curl --fail http://127.0.0.1:8787/healthz
curl --fail http://127.0.0.1:8787/readyz
```

## Configure Tailscale Serve

Run this command on the server:

```sh
tailscale serve --bg http://127.0.0.1:8787
```

Tailscale can open a consent page if HTTPS requires tailnet approval. Complete
that approval, and then run the command again.

Read the active mapping and HTTPS hostname:

```sh
tailscale serve status
```

The output contains a hostname similar to this value:

```text
https://acp-tunnel.example-tailnet.ts.net
```

The connector URL adds the tunnel path and uses the WebSocket TLS scheme:

```text
wss://acp-tunnel.example-tailnet.ts.net/v1/tunnel
```

The `--bg` option keeps the mapping active after the command exits. Tailscale
restores this background mapping after a reboot or daemon restart.

## Examine the endpoint

Run this command on the client machine:

```sh
acp-tunnel doctor \
  --url wss://acp-tunnel.example-tailnet.ts.net/v1/tunnel
```

The public diagnostic sends no bearer token. A successful diagnostic receives
`401 Unauthorized` from the tunnel route.

This response proves that the client reached the protected route through TLS.
It does not prove that the client token matches the server token.

## Configure the connector

Use the same URL in the ACP client:

```sh
acp-tunnel connect \
  --url wss://acp-tunnel.example-tailnet.ts.net/v1/tunnel \
  --agent codex \
  --workspace project-a
```

The Tailscale access policy and the tunnel bearer token provide separate access
controls. Keep both controls active.

## Troubleshooting

### Tailscale Serve has no mapping

Run `tailscale serve status`. If no HTTPS mapping exists, run the Serve command
again and complete any approval prompt.

### The diagnostic cannot resolve the hostname

Make sure that the client is connected to Tailscale. Then make sure that
MagicDNS and HTTPS are active for the tailnet.

### The connector receives 401 Unauthorized

The route is reachable, but the bearer token is missing or incorrect. Copy the
same server token to `$HOME/.config/acp-tunnel/token` on the client.

### The connector disconnects during idle sessions

Make sure that the connector and server remain connected to Tailscale. Then
read the connector stderr output for resume messages.
