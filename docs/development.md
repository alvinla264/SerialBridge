# Development and security

## Security

Local clients use a Unix socket: the socket directory is created 0700, the
socket file is 0600, and every accepted connection is checked with `SO_PEERCRED`
against the core's effective uid. A caller-provided socket directory is not
chmodded; the uid check still applies.

LAN sharing is opt-in and off by default: no network listener exists until the
local owner terminal starts a sharing session for one concrete address. Each
session generates a fresh certificate authority, host certificate and two
distinct high-entropy pairing credentials with `rustls` (ring provider) and
`rcgen`. Remote clients trust only the pairing CA and pin the host leaf
certificate's SHA-256, so a different certificate is refused. The core derives
the remote role from the matched credential, applies the checked-device
allowlist and device identity to every request and event, and enforces bounded
frames, handshake/pairing deadlines, connection counts and pending-write
limits. Stopping sharing or losing the owner connection closes the listener,
disconnects every remote client and revokes credentials. See
[LAN sharing](sharing.md) for the internal contract.

## Building and testing

```sh
nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#rustfmt nixpkgs#clippy -c sh -c '
  cargo fmt --all --check &&
  cargo clippy --workspace --all-targets -- -D warnings &&
  cargo test --workspace
'
```

or, with a Rust toolchain on `PATH`, the same `cargo` commands. The workspace
declares Rust 1.85 as its minimum (the locked dependencies need it).

GitHub Actions runs the same checks on every pull request and on each push to
`main` ([`.github/workflows/ci.yml`](../.github/workflows/ci.yml)), with
`--locked` so a stale `Cargo.lock` fails the pull request instead of being
silently updated, and with every action pinned to a release commit. Each check
is its own job, named after the command it runs, so a pull request shows:

- `cargo fmt --all --check` (stable)
- `cargo clippy --workspace --all-targets -- -D warnings` (stable)
- `cargo test --workspace (stable)` and `cargo test --workspace (1.85.0)`
- `cargo build --workspace --release (stable)` and `cargo build --workspace --release (1.85.0)`

The tests and the release build run on both, so the declared minimum is checked
rather than assumed.

Packaging:

```sh
cargo build --release        # all three binaries
nix build .#default          # the same three, from the flake, under ./result/bin
nix flake check              # build the packages/checks for this system
nix flake check --all-systems --no-build   # evaluate every system, build none
```

The Nix build skips the Rust test suite (`doCheck = false`): those tests open
PTY pairs and sockets, and belong to `cargo test`, not to a sandboxed build.

Tests use simulated serial devices: a PTY pair where the core opens the slave
path as a real serial port and the test drives the master. Covered end to end:

- open/join, baud conflicts, join at the active rate, default baud;
- one AI writer, read-only joins, control denial, `ControlHeld` on write;
- idle expiry, per-device timers, reclaim with `open`, release on disconnect;
- read history, dropped-line reporting, partial lines, invalid UTF-8, the
  partial tail counting toward the requested line limit;
- fragmented device output, chronological ordering of an old partial tail
  against newer writes, partial tails in successful and timed-out writes,
  timed-out writes keeping the shared tail, bounded (truncated) large reads,
  oversized unfinished frames, alias paths joining one device;
- exact writes, waited writes, concurrent writes not interleaving, a waited
  write not blocking other requests, close and immediate reopen;
- multiple devices per client, last-client close, close draining accepted
  writes, pipelined writes keeping arrival order, unplug notification;
- LAN sharing over real loopback TLS: default off, fresh per-session human and
  AI credentials only to the local owner, owner-only control, ACL on list/open/
  read/write/status/close and on events, role from credential (AI role spoofing
  refused, one AI controller across local and remote, humans writing beside it),
  loopback IPv4 and IPv6 where available, host certificate pinning and wrong-CA
  refusal, wrong credentials, oversized frames, stalled TLS handshake deadline,
  unauthenticated handshake connection slots, waited-write admission limits,
  an expected device identity enforced by the core at ShareStart/ShareCheck/
  ShareReplace (atomic whole-list replacement),
  bind failure leaving sharing off, unchecking
  revoking access and failing pending work while local and other devices keep
  working, owner loss stopping the listener and revoking credentials, stopping
  during a waited write without replay, path replacement not inheriting
  authorization, secrets absent from responses/events/history, and existing
  Unix-socket clients still working while sharing is active;
- core grace-period shutdown and supported client auto-start (including an
  explicit core path, a missing binary, and the started core owning a separate
  session);
- the terminal's side-by-side layout at a usable width and focused-pane layout
  when narrow, single-click highlight never changing the shown stream,
  Enter/double-click opening, focus switching with `Ctrl+]` without redirecting
  writes, local echo on by default and off after a saved change, the stream
  owning `Tab` and `Ctrl+Q` while the sidebar quits on `Ctrl+Q`, the sidebar
  sending a literal `Ctrl+]` to the shown device, `b` from the sidebar opening
  the scrollback and `Esc` returning to the live stream, arrow and page
  scrolling through kept output with no scroll key reaching the device, nothing
  kept with the setting off, an unsaved settings draft refusing device opening,
  a resize keeping the active session, draft and per-device scroll position, a
  disconnect keeping settings and other sessions, settings drafts saved on
  `Ctrl+S` only when the file was written (a forced save failure and a missing settings file leave the options
  unchanged and say so), the settings page's mouse toggles and its
  `Save`/`Cancel` buttons (a mouse save that fails changes nothing and stays on
  the page), and discarded on `Esc`, visible AI writes,
  unplug and shutdown through the same core. The terminal's stream parsing, key
  mapping, layout, cursor positions, mouse hit testing, the scrollback view's
  navigation, the bounded history (rows kept, scrolled views, a resize, an
  erase), local echo next to a half-read device escape or UTF-8 sequence, and
  footer layout are covered by unit tests, and its settings cover defaults, a missing file, round trips,
  invalid and unreadable files left untouched, and save failures.

The real `serial-nexus-terminal` binary is also driven in a pseudo-terminal
(`crates/serial-nexus-terminal/tests/pty_tui.rs`): a real core, two simulated
devices, raw mode and mouse capture, and a terminal emulator reading back the
drawn screen. It checks the sidebar beside the stream at a usable width, two
devices' independent streams and scrollback, a single click highlighting
without switching or redirecting input, Enter and double-click switching, keys
reaching only the shown device, `Ctrl+]` focus switching, SIGWINCH resize to a
narrow focused pane and back without losing sessions or the scroll position, the
settings page on the right with mouse and keyboard Save/Cancel, a failed save
leaving the options unchanged, a dirty draft surviving a resize and refusing
device opening, explicit disconnect and an unplug leaving the other session
usable, literal `Ctrl+]`, `Tab`/arrows/`Ctrl+Q` forwarding, an AI client writing
beside the human, local echo off, scrollback off clearing every session's kept
rows (also while it is off) without them returning when it is turned back on,
and `Ctrl+Q` quitting. It joins both devices before resizing, so both sessions
are shown to survive with their own output and keys.

The PTY fixtures pre-open their devices, so opening a closed device through the
baud prompt, device discovery, and a hand-edited invalid settings file staying
untouched are covered at the app and client/core seams, not through this PTY.

Further terminal coverage uses real core/client seams and dedicated PTY flows:
- `sharing_controls.rs`: checklist Save/Cancel, bind failures, session-only
  device selection, identity replacement, pairing credentials, revocation and
  cleanup on shutdown.
- `remote_controls.rs`: authenticated Connect, certificate mismatch, several
  endpoint groups, same-path isolation, a remote session's focus, scrollback
  and resize, unplug/disconnect notices, and remote write failures.
- `pty_sharing.rs` and `pty_remote.rs`: the actual rendered and keyboard-driven
  Share/Pairing and Connect workflows, including device access on the host.
- `pty_remote_mcp.rs`: the real MCP binary reaching a remote host through the
  real terminal's gateway, beside a second and third human client on the same
  device, with whole non-retried writes, secret-free output, uncheck revocation,
  credential rotation and no fallback when the gateway exits.

`serial-nexus-mcp` additionally has end-to-end tests over real MCP stdio framing:
handshake and tool listing, protocol errors, default and joined baud, rejected
conflicts, two independent sessions with read-only takeover, exact writes,
waited sends, partial lines, timeouts, disconnect releasing control, detached
and missing devices and an unplugged device. Separate tests require the local
terminal gateway and prove MCP never auto-starts the core. Remote MCP tests cover
private pairing-file permissions/redaction, per-endpoint sessions and device
identity, explicit remote pairing and transport, and isolation between local
and remote endpoints.

Not proven here: real hardware and drivers, PTY behaviour that differs from
USB serial adapters, platforms other than Linux, terminals other than what the
`vt100` emulator models in the PTY tests (truecolour, unusual key encodings),
`libudev`-based enumeration (device discovery reads `/dev/serial/by-id` and
`/sys/class/tty`), and real LAN peers (sharing tests use loopback). PTY fixtures
are not enumerated, so sharing tests open them locally first; real enumerated
devices need not be open locally.
