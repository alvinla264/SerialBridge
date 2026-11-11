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

## Final-round security fixes and tests

The last round-capped review (head `c587b01`) confirmed three majors, all fixed
after that round. A further human-approved focused security review was run on
`bda12ac` and confirmed three additional findings, all fixed after that review:
a stop/enqueue race, unchecked-device discovery, and metadata-only oversized
waited-write answers. The fixes are covered by the tests below, but the newest
post-review code has not itself been independently re-reviewed; the
coordinator must assess it before merge, and must repeat the board gate if the
head changes.

- **Remote list/event visibility of a replacement.** Discovery and event
  filtering now require the exact open device instance to have been authorized
  for sharing (`Device.shared`, set at open, at check, and when sharing
  starts), not merely a path present in the allowlist. Tests:
  `an_expired_entry_does_not_leak_events_for_a_later_device_instance`,
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
- Minor fixes from that round: glossary wording now matches the close
  contract; the incremental frame decoder buffers two complete frames so two
  valid pipelined frames are not mistaken for an oversized one (protocol unit
  test `two_pipelined_frames_are_not_mistaken_for_one_oversized_frame`).

Findings fixed after the additional focused review:

- **Stop/enqueue race.** `share_gate` (an `RwLock`) serializes stop/revocation
  with remote write admission; a write admitted concurrently either queued
  before the epoch bump (and is dropped) or waits and then sees the stopped
  session. Test: `a_remote_write_cannot_be_admitted_after_stop_begins`.
- **Unchecked-device discovery.** Open-device discovery now requires the
  instance's share marker *and* current-session allowlist membership; uncheck
  and every new/stopped session clear the markers. Tests:
  `unchecked_and_new_session_devices_are_not_discoverable`.
- **Metadata-only oversized write answers.** Waited-write trimming now budgets
  the serialized response (binary-searching kept newest lines), so many empty
  or short lines cannot turn a successful write into an error. Test:
  `many_empty_waited_lines_keep_a_successful_write`.
- **TLS send boundary.** The transport now writes plaintext in bounded chunks
  and flushes whenever rustls's buffer is full, so a valid near-limit frame
  cannot close the connection after the device write executed. Tests:
  `near_limit_waited_write_answers_survive_the_tls_send`. Zero-write
  recovery accounts for the retry's accepted count exactly once (protocol
  test `zero_write_recovery_never_duplicates_plaintext`).
- **Seam replay observability.** The fake serial port now records every write
  attempt, including failed ones, and the test asserts the exact
  `write_all` remainder sequence with no later attempt.

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
- PTY fixtures are not enumerated by the real `enumerate::list_ports`, so most
  sharing tests open a PTY in the core first. The injected-lister test covers
  the checked-but-never-opened enumerated path; the production rule
  (enumerated **or** open) is unchanged.

## Iterative review fixes (all axes)

After the first full Standards/Spec/security review sweep, these findings were
fixed with tests:

- **An open instance is bound to the node it actually opened.** Opening
  validates the expected identity before `TTYPort::open` configures anything,
  and the engine stores the identity of the opened descriptor. Joining an
  existing device and marking it shared require that exact live instance, so
  checking a replacement cannot authorize an older open descriptor or its
  history. Tests: `an_open_instance_only_matches_its_own_live_node`,
  `a_mismatched_expected_identity_is_rejected_before_configuration`.
- **Discovery and events revalidate the instance.** Remote discovery and event
  delivery revalidate the instance identity, expire the allowlist entry and
  bump its write generation when it no longer matches. Tests:
  `remote_operations_are_limited_to_checked_devices_by_acl`,
  `an_expired_entry_does_not_leak_events_for_a_later_device_instance`.
- **Detached waiters are cancelled.** `Close` fails that client's waited writes
  immediately, so unchecking cannot leave a detached waiter collecting later
  output. Test:
  `a_detached_waited_write_fails_and_never_returns_post_revocation_output`.
- **Unchecking works for vanished devices** by removing the stored canonical
  key, freeing the allowlist slot. Test:
  `a_vanished_checked_device_can_be_unchecked_and_frees_its_slot`.
- **Mapped IPv4 wildcards are refused** and IPv6 binds format with brackets.
  Protocol test `ipv4_mapped_wildcards_are_refused_and_ipv6_formats_with_brackets`.
- **The remote client encodes before registering a pending request**, so a
  refused oversized request retains nothing. Test:
  `refused_oversized_remote_requests_do_not_leak_pending_entries`.
- **The board gate echo check only counts new device-sourced output**, never
  the probe's human write or old history. Unit test
  `a_firmware_echo_must_be_new_device_output_not_the_probe_or_old_history`.
- The serial-queue limit test is named
  `a_slow_remote_peer_hits_the_bounded_serial_queue_and_queued_work_is_revoked`;
  the waited-write admission limit (including a non-waited write that must still
  be admitted) is covered by
  `a_connection_has_a_bounded_number_of_pending_waited_writes`.
- Deadline and connection bounds are covered by
  `a_stalled_tls_handshake_is_closed_after_the_handshake_deadline` and
  `unauthenticated_handshakes_count_toward_the_connection_limit`.
- A draining device's queued remote writes are revoked by
  `a_draining_device_still_revokes_queued_remote_writes`.
- The deterministic recycled-node identity test uses regular files
  (`a_checked_but_never_opened_device_rejects_a_recycled_node`); the engine-level
  PTY-recycling test is `#[ignore]`d because it cannot guarantee the kernel
  reuses a path on every run.

### Second iterative review fixes

- **Open descriptors are validated before configuration.** Opening now creates
  and `fstat`s the descriptor itself, compares it with the expected identity,
  and only then applies 8N1 termios; a replacement at the open boundary is
  never configured. The regression test
  `a_mismatched_expected_identity_is_rejected_before_configuration` checks the
  target baud is unchanged.
- **Draining devices stay revocable.** Write generations of devices whose last
  client detached are kept in a registry until their I/O threads exit, so stop
  and uncheck still drop queued remote writes while the writer drains. Test:
  `a_draining_device_still_revokes_queued_remote_writes`.
- **Timed-out waited writes recheck authorization** before returning a partial
  tail, and a failed wait outcome is honored instead of extracting its lines.
- **Connection registration rejects a stopped session**, clones before
  reserving a slot, and authentication checks the session again before
  registering; a raced accept cannot authenticate with revoked credentials.
- **The pending-wait limit only applies to requests that create a waiter**; a
  non-waited write is admitted while waiter slots are full. Test:
  `a_connection_has_a_bounded_number_of_pending_waited_writes`.
- **The remote adapter carries its authentication decoder into the connection
  loop**, so a frame already buffered during authentication is not lost.
- Added committed tests for a stalled TLS handshake, unauthenticated handshake
  connection slots, and an unchecked later device instance; the engine-level
  PTY-recycling replacement test is `#[ignore]`d because path reuse is not
  guaranteed, with deterministic regular-file identity tests in its place.
