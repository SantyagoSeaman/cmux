# Embedded tailnet controls

The daemon-owned embedded node is opt-in. Until a person calls
`_acpmux/tailnet/start`, no node is constructed, no state directory is made,
and no tailnet listener is opened. A start resumes the saved login when one
exists; a new node reports `state: "needs_login"` and keeps its browser URL in
memory for the person who requested it.

The controls are accepted only on a local, person-proven connection:

- `_acpmux/tailnet/start` starts the node without waiting for sign-in.
- `_acpmux/tailnet/login` asks a running node for a fresh sign-in URL.
- `_acpmux/tailnet/state` returns `state`, optional `dns_name`, `ips`, and
  `error`. The sign-in URL is included only for the person response.
- `_acpmux/tailnet/logout` signs the node out and closes ingress connections.

The state stream is event-driven from the node's IPN bus. It has no polling
timer. A failed watch clears the node so a later explicit start can retry; a
generation fence prevents an old watch from changing a replacement node's
state. Explicit start saves an opt-in alongside the node's private state. Daemon
startup resumes only that opt-in; an untouched installation creates no state or
node. Logout removes the opt-in before remote cleanup, so a failed sign-out
cannot silently reconnect on the next launch. Daemon shutdown preserves it.

The daemon supervises one WSS listener on the live node's port 443. It drains
connections before replacing a node or DNS name. A failed listener reports an
error and waits for explicit Start to retry. Shutdown bounds listener drainage
and node closure; no polling timer runs in the background.

Tailnet ingress receives the controller's live `Node` and the daemon's same
`Arc<Hub>`. Pairing tickets are minted and redeemed by that Hub's one pairing
store, so the authenticated tailnet identity and acpmux identity cannot drift
into separate stores.
