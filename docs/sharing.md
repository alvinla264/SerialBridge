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
  pins the node, captured when checked. It also has a process-local opaque
  generation token; that token, not the stat tuple, is the identity label
  exchanged by discovery, snapshots and checklist requests. A new capture
  gets a fresh token, so a recycled inode cannot inherit the old selection.
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

These tests cover security and lifecycle properties that were established in
iterative local reviews; the coordinator's current integrated-head review and
board-gate status are tracked separately from this protocol contract.

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

## Iterative review fixes (all axes)

After the first full Standards/Spec/security review sweep, these findings were
fixed with tests:

- **An open instance is bound to the node it actually opened.** Opening
  validates the expected identity before the descriptor is configured, and the
  engine stores the identity of the opened descriptor. Joining an
  existing device and marking it shared require that exact live instance, so
  checking a replacement cannot authorize an older open descriptor or its
  history. Tests: `an_open_instance_only_matches_its_own_live_node`,
  `a_mismatched_expected_identity_is_rejected_before_configuration`.
- **Discovery and events revalidate the instance.** Remote discovery and event
  delivery revalidate the instance identity, expire the allowlist entry and
  bump its write generation when it no longer matches. Two deterministic seam
  tests split the paths: `discovery_and_events_revalidate_a_stale_shared_identity`
  delivers output before discovery and checks event suppression, expiry,
  `DeviceClosed`, waiter cancellation and later write denial;
  `discovery_revalidation_expires_without_any_event` replaces a pinned node and
  calls discovery first. `remote_operations_are_limited_to_checked_devices_by_acl`
  covers checked versus unchecked devices.
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
- **The board gate echo check only accepts fresh device-output events**, never
  the probe's human write or a sliding history window.
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

### Twenty-first iterative review fixes

- **Discovery expiry of a draining device bumps its live-writes epoch**, so
  queued remote commands are revoked even when no open instance remains.
- **Randomness failures return errors** instead of panicking while engine locks
  are held.
- **ShareCheck has only the live-entry check it needs**, with no dead
  make-room retain.
- **Board-gate and authorize documentation now state only what the gate and
  tombstone lifetime actually establish.**

### Twentieth iterative review fixes

- **Remote authorization failures are uniform.** Expired, unchecked,
  nonexistent paths and unknown/open device ids all return the same
  `PermissionDenied "device is not open or not shared"`, removing remote
  host-state and open-device-count oracles. Local owners keep descriptive
  errors.

### Nineteenth iterative review fixes

- **Write-triggered identity expiry runs full revalidation** before detaching,
  so queued remote commands and waiters are revoked even when the failing
  remote already closed its attachment.

### Eighteenth iterative review fixes

- **ShareCheck returns the paths it expires** and the engine revokes each via
  revalidation plus draining-registry epoch bumps.
- **ShareStop while off returns `was_active: false`** as documented.

### Seventeenth iterative review fixes

- **Request-triggered expiry bumps `live_writes` even when the device already
  left the open map**, so draining queued remote writes are dropped.
- **Queued local requests are processed after a write-half close**, restoring
  main's half-close compatibility; a queued `ShareStart` is still refused by
  the dead-connection guard.
- **`ShareStop` when sharing is off returns `was_active: false`.** Docs clarify
  inactive `ShareStatus` visibility.

### Sixteenth iterative review fixes

- **Writes refresh the AI control lease again**, restoring main's
  `control_idle` semantics.
- **ShareCheck expires and drops invalid entries before counting**, bounding
  the checked list and pinned descriptors.
- **The oversized-echo test now subscribes and asserts `truncated: true`** and
  shortened bytes.
- **`Event::Write.truncated` is omitted when false**, preserving the old local
  JSON shape; only a trimmed remote echo carries the additive field.
- The `authorize` rustdoc now matches the expired-entry retention behavior.

### Fifteenth iterative review fixes

- **Writes are never rejected for echo size.** The full write is accepted and
  executed; a remote `Write` echo that exceeds a frame is trimmed with
  `truncated: true`. This preserves local compatibility and avoids silent
  event loss.
- **ShareStatus expiry revokes draining devices** through `live_writes`, not
  only devices still in the open map.
- **A failed wait outcome racing a timeout is uncertainty**, never a
  successful timed-out write.
- **Break regression now uses a pipe**, so TIOCSBRK/TIOCCBRK no-ops fail it.

### Fourteenth iterative review fixes

- **ShareStatus expiry also bumps epochs in the draining-device registry**, so
  queued remote writes on a device already removed from the open map are
  dropped.
- **A failed wait outcome racing the timeout is returned as uncertainty**, not
  converted into a successful timed-out write.
- **Expiry announced for an already-expired entry with attached remotes uses a
  forced vacated mark**, so `DeviceClosed` actually reaches those remotes.

### Thirteenth iterative review fixes

- **Owner status expiry revokes queued remote work.** `ShareStatus` expiration
  revalidates affected open instances, bumping epochs, detaching remotes and
  failing waiters instead of only hiding the entry.
- **Request-triggered expiry announces `DeviceClosed`** to other attached
  remotes by routing through the same revalidation path.
- The write-generation documentation now consistently says per-device.

### Twelfth iterative review fixes

- **Expiry cleanup is asynchronous**, including when identity expiry is
  discovered from a device event, so a stalled writer no longer freezes the
  dispatcher or healthy devices.
- **Write echoes are trimmed, never refused.** Local writes keep their full
  budget; only a remote `Write` echo that exceeds a frame is trimmed with
  `truncated: true`, so remote subscribers never silently lose it. Tests:
  `a_large_local_write_still_succeeds_while_sharing`,
  `a_remote_write_too_large_to_echo_is_accepted_and_trimmed`.
- **Discovery expiry has its own event-free regression test**, so removing
  list revalidation cannot hide behind event-time expiry.

### Eleventh iterative review fixes

- **Expiry discovered by discovery/events now detaches remotes and announces
  `DeviceClosed`** (or lets normal last-detach cleanup do so), releasing stale
  control; the stale-identity test covers this.
- **A host-only `/32` interface address is no longer mistaken for a
  broadcast**, while directed broadcasts still derive from wider masks.
- **Remote `Write` echoes that cannot fit a frame are trimmed and marked
  `truncated`**, while the write itself is accepted.
- **Rejected/failed remote connections shut their TCP transport before
  blocking device cleanup**, so stop always closes the socket.
- Documentation now states the exact link-count identity predicate.

### Tenth iterative review fixes

- **Remote last-detach Close returns without joining the writer.** The device is
  removed, revoked and reported, then its I/O threads are joined on a cleanup
  thread, so a disconnected peer's connection slot is released even while
  accepted writes drain. Test:
  `a_disconnected_remote_close_releases_its_slot_before_serial_drain`.

### Ninth iterative review fixes

- **Remote Open expiry revokes like discovery/event expiry.** A failed
  `session.authorize` in the Open branch detaches the instance, bumps its
  write epoch, cancels remote waiters and drops queued writes.
- **A straddling remote write cannot enqueue after `device.shared` is
  cleared.** `begin_write` refuses remote writes when the shared marker is
  gone, closing the authorize-then-enqueue race with identity expiry.
- **Local-first opens validate the opened descriptor too**, against the node
  captured before the open, so the documented identity guarantee applies to
  every open path.
- **A refused post-insert remote open removes the just-opened device**, so no
  zero-client instance is left holding the serial port.
- **The failed-device revocation test waits for the writer to resume** before
  asserting the queued command was dropped.
- Documentation no longer cites a removed firmware-echo unit test.

### Eighth iterative review fixes

- **A failed write authorization no longer holds `share_gate` across a
  blocking last-detach drain.** The gate is dropped before detaching, so
  owner-loss/stop can revoke the listener and credentials immediately.
- **Reader failure bumps the device write epoch** before async cleanup, so
  queued remote commands are dropped instead of running after revocation.
- **Remote `Read` answers are fitted to the remote frame.** Oversized lines
  and partial tails are trimmed (with `truncated` set) instead of failing with
  an impossible "request fewer lines" remedy.
- **The post-open re-authorization error is fixed text**, with no canonical
  path.
- **`dead_connections` entries are pruned on unregister.**
- **The stale-identity test delivers output before discovery**, so event-time
  revalidation is independently exercised.
- **Board-gate echo detection subscribes to fresh `DeviceOutput` events**,
  removing the sliding-window false negative and the occurrence helper.

### Seventh iterative review fixes

- **Expiry cancels pending remote waiters.** Discovery/event revalidation now
  fails remote waiters when it expires an instance, matching the documented
  uncheck behavior. The stale-identity seam test holds a waited write across
  replacement and asserts it is cancelled promptly.
- **Authorization errors no longer reveal canonical paths.** Remote callers
  receive fixed "device is not shared" / "not available" messages; the
  canonical key stays internal. The recycled-node test asserts no path leak.
- **Open/uncheck race refuses a remote attach.** If an existing device's
  re-authorization fails for a remote caller, the open is refused instead of
  attaching it (or granting control) after the uncheck removed it.
- **Directed broadcast binds are refused.** Validation derives interface
  broadcasts from address/netmask as well as `IFF_BROADCAST`, covering
  `127.255.255.255`. Test: `directed_broadcast_addresses_are_refused`.
- **Write uncertainty has one client check.** `ClientError::is_write_uncertain`
  covers both transport uncertainty and server answers with
  `may_have_reached_device`; server answers map to `WriteUncertain`.

### Sixth iterative review fixes

- **Legacy local pipelining is restored.** Local request queues are unbounded
  again, so a 256-request burst is answered instead of disconnecting the
  client. Test: `local_clients_can_pipeline_more_requests_than_the_old_queue_bound`.
- **Local writes keep the historical unbounded queue.** Only remote writes are
  bounded by `MAX_REMOTE_SERIAL_QUEUE`; local writes cannot produce the new
  `LimitExceeded` variant that protocol-v1 clients cannot decode. Test:
  `local_writes_keep_the_legacy_unbounded_queue`.
- **Failed-device cleanup is off the dispatcher.** A failed device is removed,
  reported and revoked immediately, then its I/O threads are joined on a
  dedicated cleanup thread, so a stalled writer does not block healthy-device
  data or timers. Test:
  `a_failed_devices_stalled_writer_does_not_block_healthy_output`.
- **Expiry marks the path vacated only while remotes are attached and the
  entry is live**, so those remotes see `DeviceClosed`, while a later local
  open/close of a replacement at an expired path does not leak an event.
- **Board-gate echo checks use fresh device-output events**, not a sliding
  history window, so rollover cannot hide a genuine echo.

### Fifth iterative review fixes

- **Connection EOF is remembered, not only acted on.** Every local connection
  whose reader ends is recorded; queued requests are refused and `ShareStart`
  cannot register a session after EOF. The liveness guard is held through
  session registration, so a late notification stops the new session.
- **Exclusive cleanup owns its own descriptor.** The shared token duplicates
  the descriptor, so either drop order clears `TIOCEXCL` on the right tty and
  never on a reused descriptor. The regression test covers both orders.
- **Changing speed preserves framing and flow control**, and break start/stop
  use `TIOCSBRK`/`TIOCCBRK` rather than a pulse and a no-op. Tests extend
  `framing_setters_are_reflected_by_getters`.

### Fourth iterative review fixes

- **Owner liveness no longer depends on request-queue backpressure.** Local
  request pipelining keeps the historical unbounded queue, so the reader can
  always reach EOF; no valid local burst is disconnected.
- **Batch uncheck revokes every requested device before blocking cleanup.**
  `begin_detach_remote_clients` performs authorization, waiter and detach
  changes for the whole batch, then `finish_devices` joins I/O threads once.
  Test: `a_stalled_uncheck_still_revokes_later_devices_in_the_batch`.
- **Failed serial acquisition has no side effects.** `flock` and `TIOCEXCL`
  are taken before termios changes, and a failed configure clears exclusive
  mode. Test: `failed_opening_leaves_other_settings_and_exclusivity_untouched`.
- **Disconnected remotes release their connection slot before device
  draining**, so a stalled writer cannot keep the authenticated limit full.
- **Exclusive ownership follows clones** through a shared token rather than a
  per-handle `Arc` count. Tests: `dropping_one_clone_keeps_exclusive_mode`,
  `reopening_after_drop_clears_exclusive_mode`.
- The owner-loss regression test now blocks the owner's own handler and forces
  EOF on that connection; it fails when the reader-thread notification is
  removed.
- Serial framing getters read effective termios settings instead of returning
  constants.

### Third iterative review fixes

- **Owner loss is observed independently of the request handler.** The reader
  thread tells the engine when the owning connection reaches EOF, so sharing
  stops even while that connection's handler is blocked draining a device.
  Test: `owner_loss_revokes_sharing_even_while_a_device_close_is_draining`.
- **Serial opening normalizes inherited framing.** `cfmakeraw` leaves
  `CSTOPB`, `PARENB`, `PARODD` and `CRTSCTS` untouched, so opening now clears
  them explicitly and restores exclusive ownership with `TIOCEXCL`, `flock`
  and `TIOCNXCL` on the last handle. Tests:
  `opening_normalizes_framing_and_flow_control`,
  `opening_keeps_exclusive_ownership` and
  `reopening_after_drop_clears_exclusive_mode`.
- **Non-table baud rates use `termios2`/`BOTHER`** instead of failing, and the
  `SerialPort` setters and modem-control getters perform their documented
  operations. Test: `opening_supports_arbitrary_baud_rates`.
- **Expiring an instance fails its pending remote waiters** and a successful
  waited write rechecks authorization before returning device output.
- **Generated host certificates include the CA Authority Key Identifier**, as
  required by strict OpenSSL/Python verifiers.
- Fixed the later-instance test to close and reopen a genuinely new instance,
  the firmware-echo helper test to assert freshness, and removed the redundant
  raw-TCP oversized test.
