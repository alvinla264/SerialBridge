# LAN sharing: internal contract

This page documents the core contract used by the terminal's LAN sharing
controls, remote terminal, and remote MCP client. Sharing is **off by default**
and there is no network listener unless a local owner starts one.

## Lifecycle

1. A **local owner** connects over the Unix socket and calls `ShareStart` with
   one concrete `ShareBind` address and port and zero or more detected device
   paths. The supported human workflow is the terminal. Wildcard (`0.0.0.0`,
   `::`), multicast and broadcast addresses are refused. The core's local
   client kind is self-asserted; it does not authenticate the executable.
2. The core binds the listener synchronously. A bind failure is returned as an
   error and leaves sharing off; no session, credentials or listener exist.
3. The session lives only while that owner connection lives. Closing it,
   `ShareStop`, a core shutdown or a crash closes the listener, disconnects
   every remote connection and revokes both credentials. Sharing is off until
   started again, with a fresh session id, CA, leaf certificate and credentials.
4. Only the owner connection may check, uncheck, inspect or stop its active
   session. This is connection ownership, not terminal-process authentication:
   every program running as the laptop user is trusted and can impersonate a
   human client, access local serial, and start or change sharing. Other OS
   users cannot connect through the private per-user socket; paired remote
   clients can use only their role and checked-device permissions and cannot
   control sharing.

## Pairing and roles

`ShareStart` answers with `ShareSessionInfo`, the only server response that
issues pairing secrets; the authenticated client later sends its credential in
a `RemoteAuth` frame inside TLS:

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
| `ShareReplace(ShareDevicesParams)` | `Sharing(SharingSnapshot)` | owner only, atomic whole-list replacement |
| `SharePrepare(ShareDevicesParams)` | `SharePrepared(SharePrepared)` | owner only, validates without changing |
| `ShareCommit(ShareCommitParams)` | `Sharing(SharingSnapshot)` | owner only, applies a prepared list |
| `ShareUncheck(ShareDevicesParams)` | `Sharing(SharingSnapshot)` | owner only, revokes now |
| `ShareStatus` | `Sharing(SharingSnapshot)` | owner only; `active: false` when off |

`SharingSnapshot` has no secrets. Remote requests reuse `Request`, except that
`Hello` and every `Share*` request are refused. `Ping`, `ListDevices`, `Open`,
`Close`, `Read`, `Write` and `Status` are authorized on the request itself.

## Authorization

### Local trust boundary

All programs running as the laptop user are trusted. The Unix socket restricts
connections to that user's effective UID, but does not distinguish the
terminal from another same-UID program. `Hello.client.kind` is supplied by the
client, not OS-attested. Therefore a same-user program can impersonate the
terminal, access local serial, and enable/modify sharing. The terminal is the
supported human control path; its controls are not a security boundary against
the same user. This trust does not extend to another local OS user or to an
unpaired remote client. Remote access still requires TLS host verification,
a valid paired credential and role, and the session's checked-device
allowlist.

- **Allowlist.** The checked list is an in-memory, session-scoped map of
  canonical device path to `DeviceIdentity`. A device may be checked when it is
  enumerated now or already open in the core; it does not need to stay open.
- **Identity.** `DeviceIdentity` internally records the node's
  `(st_dev, st_ino, st_rdev)` and link count plus an `O_PATH` descriptor that
  pins the node, captured when checked. The protocol uses two distinct opaque
  generation-token forms: discovery returns an `snx-*` token, while an open
  device or sharing snapshot returns an `identity-*` token. They identify the
  same pinned node through separate APIs and are not interchangeable; sharing
  requests accept either only when it matches the current pinned discovery
  entry. A new capture gets fresh tokens, so a recycled inode cannot inherit
  the old selection.
  The node is revalidated on `Open`, `Read`, `Write`, `Status`, `Close` and on
  the open itself: the serial descriptor's `fstat` and canonical path must
  still be that node, and the pinned node's link count must still equal the
  captured value (so an added hard link also expires it).
  A changed node or an unplug expires the entry: access stops at once and
  attached remote clients receive `DeviceClosed`. The expired path remains as
  a non-authorizing tombstone, retaining its pinned descriptor, until the next
  `ShareCheck` prunes it, the owner unchecks it, or the session ends. An
  ordinary last-detach close keeps the checked entry (the pinned node proves it
  is the same device), so reopening the same device does not need a recheck. Expired entries are not "checked", are not listed, and
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
  waited writes and bumps the device's write generation so queued but not yet
  started remote commands are dropped by the writer. Local clients and other
  shared devices are untouched. A write already in progress can still finish or
  partially reach the device; there is no rollback and no automatic replay.
- **ShareStatus when off.** With no active session, `ShareStatus` returns the
  inactive snapshot to a same-UID local client; when a session is active, only
  its owner connection may inspect it. A different same-UID process can claim
  human kind when creating its own connection and is trusted under this model. `ShareStop` when off returns `was_active: false`.
- **Owner loss / stop.** The listener is closed, every remote socket is shut
  down and every device's write generation is bumped, so queued but not yet
  started remote writes are dropped, including writes from clients that have
  since detached (a write already in progress may finish). Remote waits fail and the client reports uncertainty
  (`ClientError::WriteUncertain` / `may_have_reached_device: true`); nothing is
  retried.

## Bound resources

- `MAX_REMOTE_FRAME_BYTES` per frame in either direction. An answer that cannot
  fit becomes a request-level `BadRequest` ("response too large"), not a
  dropped connection; a successful waited write is trimmed and stays a
  successful `WriteResult`, never an error.
- A bounded per-device remote write queue (64 accepted remote writes). A full
  queue answers `LimitExceeded` before accepting bytes, so a slow device cannot
  accumulate
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

## Local compatibility and terminal gateway

Local core clients keep the Unix socket, framing, hello, open/join, control,
history, close and grace-period behavior. The UID check, per-user socket and
one-core-per-user lock separate OS users; they do not isolate same-UID
programs. The foreground terminal hosts a bounded/versioned per-user MCP
gateway. All MCP calls, including calls to paired remote endpoints, use this
gateway; one upstream core or remote client is kept per MCP session and
endpoint. MCP exposes no sharing administration. Local MCP needs a running
terminal but not an active LAN share. A second supported terminal exits clearly.
On terminal/gateway exit its local MCP clients and outgoing remote MCP clients
disconnect, its owner connection stops its share, inbound remote clients are
closed, and the credentials are revoked. A new share is opt-in and generates
fresh credentials. The gateway is a supported routing/lifetime rule, not a
security boundary against trusted same-UID programs.

## Test seams

Two `#[doc(hidden)]` process-wide hooks exist so automated tests can exercise
real authorization paths deterministically. Production code never installs
them; they only replace discovery or the I/O implementation:

- `enumerate::install_port_lister`: makes an existing real device node appear
  in detection, so a checked-but-never-opened device can be shared and opened
  remotely. Authorization still captures and validates the real node, the
  real open, and the same allowlist/identity checks.
- `device::install_serial_factory`: after the real device is opened and its
  identity validated, replaces only the read/write implementation. Tests use
  it to fail after a known prefix and prove uncertainty reporting and no
  replay.

`crates/serial-nexus-core/tests/seams.rs` runs both seams and holds a lock while
installing them. They are test infrastructure, not a configuration surface.

## Security regression coverage

These tests cover the security and lifecycle invariants implemented by the
core and terminal. The real-hardware acceptance procedure is documented in
[`board-gate.md`](board-gate.md).

- **Remote list/event visibility of a replacement.** Discovery and event
  filtering now require the exact open device instance to have been authorized
  for sharing (`Device.shared`, set at open, at check, and when sharing
  starts), not merely a path present in the allowlist. Tests:
  `an_unchecked_entry_does_not_leak_events_for_a_later_instance`,
  `a_replacement_at_the_same_path_does_not_inherit_authorization`,
  `a_checked_device_survives_an_ordinary_close_and_reopen`,
  `a_checked_but_never_opened_device_rejects_a_recycled_node`.
- **Queued remote writes from detached clients.** Each device has a remote-write
  generation; uncheck, stop and owner loss bump it, and the writer drops any
  queued remote command with an old generation, independent of whether its
  client is still attached. Local queued writes are unaffected. Tests:
  `queued_writes_from_a_detached_remote_client_are_revoked`,
  `stopping_sharing_drops_queued_remote_writes_before_the_writer_gets_them`,
  `a_slow_remote_peer_hits_the_bounded_serial_queue_and_queued_work_is_revoked`.
- **Oversized waited-write answer.** A successful waited write is trimmed to a
  frame-safe budget and stays a successful `WriteResult` (never downgraded to
  an error), preserving write-outcome semantics. Test:
  `an_oversized_waited_write_answer_stays_a_successful_write`.
- The incremental frame decoder handles two valid pipelined frames without
  mistaking them for one oversized frame (`two_pipelined_frames_are_not_mistaken_for_one_oversized_frame`).
- **Stop/enqueue race.** `share_gate` (an `RwLock`) serializes stop/revocation
  with remote write admission; a write admitted concurrently either queued
  before the epoch bump (and is dropped) or waits and then sees the stopped
  session. Test: `a_remote_write_cannot_be_admitted_after_stop_begins`.
- **Unchecked-device discovery.** Open-device discovery requires the instance's
  share marker *and* current-session allowlist membership; uncheck and every
  new/stopped session clear the markers. Test:
  `unchecked_and_new_session_devices_are_not_discoverable`.
- **Waited-write size and TLS framing.** Responses are trimmed to the serialized
  frame budget, and the TLS transport flushes bounded chunks before a valid
  near-limit write answer can be lost. Tests:
  `many_empty_waited_lines_keep_a_successful_write`,
  `near_limit_waited_write_answers_survive_the_tls_send`, and
  `zero_write_recovery_never_duplicates_plaintext`.
- **Write-attempt observability.** The serial test seam records failed attempts
  and verifies that recovery writes only the unaccepted remainder.

## Client adapter

`serial_nexus_client::RemoteClient` is the client-facing production remote
adapter used by both the terminal and MCP server. It performs the TLS trust and
pin checks, authenticates, then exposes `list_devices`, `open`, `close`, `read`,
`write_bytes`, `status` and `subscribe`; these clients do not re-implement TLS.

## Board gate

`docs/board-gate.md` is the coordinator's real-hardware, real-LAN acceptance
procedure (no flashing). It runs the ignored `board_gate` integration test
against a real STM32 and one concrete LAN address: local open, sharing start,
remote pair/list/join/read, optional probe and echo, uncheck revocation, and
local access after stop.

## Not covered here

- mDNS, NAT traversal, non-Linux platforms and hardware behaviour.
- PTY fixtures are not enumerated by the real `enumerate::list_ports`, so most
  sharing tests open a PTY in the core first. The injected-lister test covers
  the checked-but-never-opened enumerated path; the production rule
  (enumerated **or** open) is unchanged.
