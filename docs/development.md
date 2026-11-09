# Development and security

## Security

The first version is local-only: a Unix socket, no network listener. The socket
directory is created 0700, the socket file is 0600, and every accepted
connection is checked with `SO_PEERCRED` against the core's effective uid. A
caller-provided socket directory is not chmodded; the uid check still applies.

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
- core grace-period shutdown and client auto-start (including an explicit core
  path, a missing binary, and the started core owning a separate session);
- the terminal's sidebar/prompt/switch/disconnect flows, local echo on by
  default and off after a saved change, the stream owning `Tab` and `Ctrl+Q`
  while the sidebar quits on `Ctrl+Q`, the sidebar sending a literal `Ctrl+]`,
  `b` from the sidebar opening the scrollback and `Esc` returning to the live
  stream, arrow and page scrolling through kept output with no scroll key
  reaching the device, nothing kept with the setting off, settings drafts saved on `Ctrl+S` only when the file was
  written (a forced save failure and a missing settings file leave the options
  unchanged and say so), the settings page's mouse toggles and its
  `Save`/`Cancel` buttons (a mouse save that fails changes nothing and stays on
  the page), and discarded on `Esc`, visible AI writes,
  unplug and shutdown through the same core. The terminal's stream parsing, key
  mapping, layout, cursor positions, mouse hit testing, the scrollback view's
  navigation, the bounded history (rows kept, scrolled views, a resize, an
  erase), local echo next to a half-read device escape or UTF-8 sequence, and
  footer layout are covered by unit tests, and its settings cover defaults, a missing file, round trips,
  invalid and unreadable files left untouched, and save failures.

`serial-nexus-mcp` additionally has end-to-end tests over real MCP stdio framing:
handshake and tool listing, protocol errors, default and joined baud, rejected
conflicts, two independent sessions with read-only takeover, exact writes,
waited sends, partial lines, timeouts, disconnect releasing control, detached
and missing devices, an unplugged device, and auto-start.

Not proven here: real hardware and drivers, PTY behaviour that differs from
USB serial adapters, platforms other than Linux, and `libudev`-based
enumeration (device discovery reads `/dev/serial/by-id` and `/sys/class/tty`).
