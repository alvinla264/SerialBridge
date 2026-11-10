# LAN sharing: internal contract

This page is the contract between the core's sharing foundation and the
follow-on tasks (the terminal's sharing controls and the remote terminal/MCP
clients). Sharing is **off by default** and there is no network listener unless
a local owner starts one.

## Lifecycle

1. A **local owner** connects over the Unix socket (a human terminal) and calls
   `ShareStart` with one concrete `ShareBind` address and port and zero or more
   detected device paths. Wildcard (`0.0.0.0`, `::`), multicast and broadcast
   addresses are refused.
2. The core binds the listener synchronously. A bind failure is returned as an
   error and leaves sharing off; no session, credentials or listener exist.
3. The session lives only while that owner connection lives. Closing it,
   `ShareStop`, a core shutdown or a crash closes the listener, disconnects
   every remote connection and revokes both credentials. Sharing is off until
   started again, with a fresh session id, CA, leaf certificate and credentials.
4. Only the owner may check, uncheck, inspect or stop the session. Other local
   clients can use devices as before but cannot control sharing. Remote
   clients can never control sharing.

## Pairing and roles

`ShareStart` answers with `ShareSessionInfo`, which is the **only** place the
secrets exist on the wire:

- `session_id` (random, 128-bit);
- `bind` (the actual bound `SocketAddr`, including the port when 0 was asked);
- `host`: `address`, `host_name` (the bind IP as text), `ca_cert_der_b64`, and
  `host_cert_fingerprint_sha256`;
- `human` and `ai`: two distinct random 256-bit credentials, each with its
  `PairingRole`.

Each session generates a fresh certificate authority and a leaf certificate
for the bind address. A remote client must:

1. trust only the pairing CA (put it in its root store), verify the TLS name
   against `host_name`, and **additionally** compare the leaf certificate's
   SHA-256 with `host_cert_fingerprint_sha256`; a mismatch is a hard failure;
2. open the TLS connection and send `RemoteAuth { credential, protocol,
   client }` as the first length-prefixed frame. `client` is a display name
   only.

The server derives the role from **which credential matched** (constant-time
digest comparison). Nothing the client sends can select or upgrade a role. A
human credential gets `ClientKind::Human`; the AI credential gets
`ClientKind::Ai`. One human credential may pair several human clients
(several humans may write); the single AI credential may pair several AI
connections, but the engine's existing per-device rule still allows exactly
one local or remote AI to hold write control.

`RemoteAuthResult` carries the engine `client_id`, protocol, core version, the
role, the usual `Limits`, `max_frame_bytes` and `max_pending_writes`. It never
contains a credential.

## Wire format

The Unix socket keeps newline-delimited JSON and `MAX_MESSAGE_BYTES`. Remote
connections use length-prefixed JSON: a 4-byte big-endian length then that many
bytes, bounded by `MAX_REMOTE_FRAME_BYTES` (64 KiB). Zero or oversized lengths
close the connection. The owner control messages extend the existing `Request`
type:

| Request | Answer | Notes |
| --- | --- | --- |
| `ShareStart(ShareStartParams)` | `ShareStarted(ShareSessionInfo)` | owner only, one active session |
| `ShareStop` | `ShareStopped` | owner only |
| `ShareCheck(ShareDevicesParams)` | `Sharing(SharingSnapshot)` | owner only |
| `ShareUncheck(ShareDevicesParams)` | `Sharing(SharingSnapshot)` | owner only, revokes now |
| `ShareStatus` | `Sharing(SharingSnapshot)` | owner only; `active: false` when off |

`SharingSnapshot` has no secrets. Remote requests reuse `Request`, except that
`Hello` and every `Share*` request are refused. `Ping`, `ListDevices`, `Open`,
`Close`, `Read`, `Write` and `Status` are authorized on the request itself.

## Authorization

- **Allowlist.** The checked list is an in-memory, session-scoped map of
  canonical device path to `DeviceIdentity`. A device may be checked when it is
  enumerated now or already open in the core; it does not need to stay open.
- **Identity.** `DeviceIdentity` is the node's `(st_dev, st_ino, st_rdev)`
  plus an `O_PATH` descriptor that pins the node, captured when checked. It is
  revalidated on `Open`, `Read`, `Write`, `Status`, `Close` and on the open
  itself: the serial descriptor's `fstat` and the canonical path must both
  still be that node, and the pinned node must still be linked (`nlink > 0`).
  A changed node or an unplug expires the entry: access stops at once and the
  path stays only long enough for attached remote clients to receive
  `DeviceClosed`. An ordinary last-detach close keeps the checked entry (the
  pinned node proves it is the same device), so reopening the same device does
  not need a recheck. Expired entries are not "checked", are not listed, and
  never grant access; an explicit `ShareCheck` with a live identity is required
  again.
- **Every operation.** `ListDevices` is filtered to checked devices. Remote
  requests for other device ids or paths fail with `PermissionDenied`,
  `NotFound` or `NotAttached`; they are not merely hidden from a UI. Events are
  filtered per remote connection against the allowlist, so unchecked devices
  never appear in device output, writes, client/control events or history.
- **Write control.** Remote AI writes go through the engine's existing control
  rule (`ControlHeld` without control); humans write without control. Control
  is shared across local and remote clients on the same device.
- **Unchecking.** `ShareUncheck` removes the entry, detaches every remote
  client from that device, releases any control it held, fails its pending
  waited writes and bumps that client's write generation so its queued but not
  yet started commands are dropped by the writer. Local clients and other
  shared devices are untouched. A write already in progress can still finish or
  partially reach the device; there is no rollback and no automatic replay.
- **Owner loss / stop.** The listener is closed, every remote socket is shut
  down and every remote client's write generation is bumped, so queued but not
  yet started remote writes are dropped, including writes from clients that
  have since detached (a write already in progress may finish). Remote waits fail and the client reports uncertainty
  (`ClientError::WriteUncertain` / `may_have_reached_device: true`); nothing is
  retried.

## Bound resources

- `MAX_REMOTE_FRAME_BYTES` per frame in either direction. An answer that cannot
  fit becomes a request-level `BadRequest` ("response too large"), not a
  dropped connection; a successful waited write is trimmed and stays a
  successful `WriteResult`, never an error.
- A bounded per-device write queue (64 accepted writes). A full queue answers
  `LimitExceeded` before accepting bytes, so a slow device cannot accumulate
  unbounded work from any client.
- TLS handshake deadline (`share_handshake_timeout`) and pairing deadline
  (`share_auth_timeout`).
- At most `share_max_connections` connections, counting peers still
  handshaking; beyond that, new TCP connections are closed immediately.
- At most `share_max_pending_writes` concurrent waited writes per connection;
  a request over the limit is refused before its bytes are accepted.
- At most `MAX_SHARED_DEVICES` checked devices per session.
- The engine's existing `read_max_lines` and `write_max_timeout` still bound
  result sizes and waits.
- Each remote connection has a bounded event queue (1024 messages). A peer
  that falls behind loses events rather than growing core memory or blocking
  local clients; it can re-read history through `Read`.

## Local compatibility

Local clients keep the Unix socket, framing, hello, open/join, control,
history, close and grace-period behaviour. The uid check, private socket
directory and one-core-per-user lock are unchanged. Sharing is layered on top
and never changes local request or event shapes; the only addition is the five
owner control requests and their answers.

## Test-only adapters

`serial_nexus_client::RemoteClient` is the client-facing remote adapter: it
performs the TLS trust and pin checks, authenticates, then exposes
`list_devices`, `open`, `close`, `read`, `write_bytes`, `status` and
`subscribe`. The terminal and MCP remote clients should use it (or the same
`serial_nexus_protocol` types) rather than re-implementing TLS.

## Board gate

`docs/board-gate.md` is the coordinator's real-hardware, real-LAN acceptance
procedure (no flashing). It runs the ignored `board_gate` integration test
against a real STM32 and one concrete LAN address: local open, sharing start,
remote pair/list/join/read, optional probe and echo, uncheck revocation, and
local access after stop.

## Not covered here

- The terminal UI for choosing an address/devices and showing pairing
  information.
- Remote terminal and MCP client tasks, and any relay/discovery beyond a
  concrete address the owner enters.
- mDNS, NAT traversal, non-Linux platforms and hardware behaviour.
- PTY fixtures are not enumerated by `enumerate::list_ports`, so tests that
  share a PTY open it in the core first; enumerated real devices do not need
  that. The production rule (enumerated **or** open) is unchanged.
