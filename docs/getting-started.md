# Installation and device behavior

## Installing

### Prerequisites

- **Linux.** The core uses a Unix socket and `SO_PEERCRED` uid checks, reads
  `/dev/serial/by-id` and `/sys/class/tty`, and talks to ports through
  `serialport`. macOS and Windows are not supported.
- **A serial adapter** (or an on-board port) and access to it: see
  [Linux serial permissions](#linux-serial-permissions).
- **Rust 1.85 or newer** to build from source, or **Nix** with flakes enabled
  to use the flake.
- Linux header/tooling for `serialport` is not needed: this workspace builds
  it with `default-features = false`, so no `libudev` is required.

### Linux serial permissions

A serial port belongs to the `dialout` group on Debian and Ubuntu, `uucp` on
Arch, and a distro-specific group elsewhere. Add your user to it, then log out
and back in:

```sh
ls -l /dev/ttyUSB0                 # who owns the device
sudo usermod -aG dialout "$USER"   # Debian/Ubuntu
# sudo usermod -aG uucp "$USER"    # Arch
id -nG                             # confirm the group after logging in again
```

On NixOS the same in `configuration.nix`:

```nix
users.users.<name>.extraGroups = [ "dialout" ];
```

Everything runs as your user; no root, setuid or system service is involved.
The core itself never needs the port to be world-writable, and the socket
directory is private (0700, socket 0600).

### Cargo

```sh
cargo build --release
# target/release/serial-nexus-{core,mcp,terminal}
```

To put all three on `PATH`, install them next to each other:

```sh
cargo install --path crates/serial-nexus-core
cargo install --path crates/serial-nexus-mcp
cargo install --path crates/serial-nexus-terminal
```

They land in `~/.cargo/bin` together, so a client installed this way finds its
core as a sibling. A single `cargo build --release` is also fine: the clients
look for `serial-nexus-core` next to themselves and on `PATH`, and the second
client to start reuses the running core.

### Nix

The flake builds `serial-nexus-core`, the MCP server (`serial-nexus-mcp`), the
human terminal (`serial-nexus-terminal`), and a `default`/`serial-nexus` package
containing all three. Use `.` instead of the GitHub URL inside a checkout.

```sh
nix run github:alvinla264/SerialNexus  # the human TUI (default)
nix run github:alvinla264/SerialNexus#serial-nexus-mcp  # MCP server over stdio
nix run github:alvinla264/SerialNexus#serial-nexus-core -- --help
nix build github:alvinla264/SerialNexus  # ./result/bin/serial-nexus-{core,mcp,terminal}
nix profile install github:alvinla264/SerialNexus  # all three on PATH
nix develop  # cargo, rustc, rustfmt, clippy
```

Local checkout: the same with `.` instead of `github:alvinla264/SerialNexus`
(`nix run .`, `nix build .#serial-nexus-mcp`, ...). The flake supports
`x86_64-linux` and `aarch64-linux`. `nix flake check` builds and checks the
current system only; `nix flake check --all-systems --no-build` evaluates every
system without building it. Building for the other architecture needs a native
builder or an emulator.

The terminal package is built against the `serial-nexus-core` it ships with:
the flake compiles that store path in as `SERIAL_NEXUS_CORE_DEFAULT` (see
[Auto-start and packaging](clients.md#auto-start-and-packaging)), so the
terminal can start its matching core. MCP does not start a core; all MCP calls
require the foreground terminal gateway. No separate core install is needed;
set `SERIAL_NEXUS_CORE` to make the terminal use a different one.

## Running

- **The core has no service.** The foreground terminal starts it detached
  (own session, stdio discarded) on the standard per-user socket; the core exits
  30 seconds after its last client disconnects. The terminal also hosts the
  local MCP gateway. Local and remote MCP calls require the terminal to be
  running; MCP never auto-starts the core or dials remote endpoints directly.
- **One core per user.** Clients racing to start it still end up sharing one.
  `SERIAL_NEXUS_SOCKET` (or `--socket`) in a fresh parent directory isolates a
  second core for testing; the lock is `core.lock` in that directory, so two
  sockets in one directory still mean one core.
- **Stopping.** Close the terminal (`Ctrl+Q`) to stop its share, disconnect
  local and outgoing remote MCP sessions, and release its core connection. MCP
  can no longer make local or remote calls until the terminal is running again.
  The core goes away after its grace period when no clients remain. To stop it
  sooner, terminate `serial-nexus-core`; devices close and attached clients see
  the connection end.
- **Human access.** Run `serial-nexus-terminal` whenever you want: it lists the
  same devices as the AI, joins an open one at its active baud, follows the live
  stream from then on, and can always write. It does not replay earlier
  history; the AI's `read` tool does.
- **AI access.** Configure your MCP host with `serial-nexus-mcp` (below). One AI
  client per device may write; others join read-only.

## Behaviour and defaults

All core defaults are bounded and reported to clients in the `hello` response
(`Limits`), so client tools and docs never hard-code different numbers. Gateway
IPC is separately versioned and bounded; all programs running as the laptop
user are trusted, while different OS users remain excluded by the private
sockets.

| Setting | Default | Notes |
| --- | --- | --- |
| Baud | `115200` | Used when a new device is opened without a rate. |
| Framing | 8N1, no flow control | Baud rate is the only configurable serial setting. |
| Core exit grace | 30 s | The core exits 30 s after its last client disconnects. |
| AI control idle expiry | 5 min | Any request from the holder on that device refreshes it. |
| History | 2000 lines / 256 KiB per open device | In memory only; discarded when the device closes. Drops are counted and reported. |
| Read lines | 50 default, 2000 max | The AI chooses per call. |
| Write wait | 1000 ms default, 30 s max | The AI chooses per call. |
| Core socket | `$SERIAL_NEXUS_SOCKET`, else `$XDG_RUNTIME_DIR/serial-nexus/core.sock`, else `$TMPDIR/serial-nexus-<uid>/core.sock` | Directory 0700, socket 0600, peer uid checked. |
| Terminal gateway socket | `$SERIAL_NEXUS_GATEWAY_SOCKET`, else `$XDG_RUNTIME_DIR/serial-nexus/gateway.sock`, else `$TMPDIR/serial-nexus-<uid>/gateway.sock` | Per-user IPC, socket 0600, same-UID peer checked; terminal lifetime. |

Device rules:

- **Opening.** The first opener chooses the baud rate. A later client may join
  with no rate (the active rate) or the same rate; a different rate is rejected
  with `BaudConflict` and the active rate in the error details. Paths are
  resolved (`fs::canonicalize`) before lookup, so a symlink such as
  `/dev/serial/by-id/...` and `/dev/ttyUSB0` name the same open device, and
  discovery canonicalizes `by-id` aliases so each device is listed once.
- **Closing.** The last client disconnecting closes the device and releases its
  rate. For a local close, accepted writes drain and the serial file is released
  before `DeviceClosed` is reported, so immediate reopen is a drain barrier.
  Remote close returns before asynchronous drain and is not a release barrier;
  see [the client and interface guide](clients.md#remote-lan-transport).
- **Write control.** One AI client per device may write. It asks for control
  with `open(..., request_control = true)`. If another AI holds it, the open
  succeeds read-only and reports the holder; the holder is also told via a
  `ControlDenied` event. Writing without control fails with `ControlHeld`.
  Human clients may always write and never take control.
- **Expiry.** Control expires after 5 idle minutes. The holder calls `open`
  again (same call, `request_control = true`) to reclaim it. When the holder
  disconnects, control is released immediately.
- **History.** Device output and AI/human writes are kept, labelled by source,
  in a bounded per-device buffer. New joiners can read it. Invalid UTF-8 keeps
  its bytes internally and appears as U+FFFD in text.
- **Lines.** `read` returns the newest lines across device output and
  AI/human writes in arrival order, and includes the open partial tail (marked
  `partial`) in its chronological place. A `write` optionally waits for
  device-output lines received after the write, returning early when enough
  arrive (including a partial tail that started after the write) or returning
  what arrived plus the current partial tail on timeout. Writes send exactly
  the supplied bytes; no line ending is added.
- **Failures.** An unplugged or failed device is closed and every attached
  client gets `DeviceClosed`. Reconnecting is manual.
- **Ordering.** Writes from all clients are serialized in arrival order; bytes
  from two writes never interleave. Multiple devices are independent, including
  their control timers and history. A write that waits for response lines does
  not block other requests, live events or disconnect handling on the same
  connection. Waited writes still enqueue synchronously in arrival order; only
  waiting for their response lines is offloaded.
- **Response bounds.** Reads and waited writes keep the newest lines that fit a
  96 KiB text budget and report `truncated`; a response is never allowed to
  exceed the IPC frame limit or drop the connection. Incoming frames are read
  with a hard byte bound, so an unfinished line cannot grow memory without
  limit.

## LAN sharing and remote terminals

Sharing is off by default and needs no configuration until it is used. The
human terminal is the owner: open Settings, set a concrete IPv4 or IPv6 address
and port, tick the devices to share, Save, and start sharing from the sidebar's
Share button (or `s`). See
[Sharing devices on the LAN from the terminal](clients.md#sharing-devices-on-the-lan-from-the-terminal)
for the controls.

The pairing view shows two separate credentials:

- the **human credential** goes into another terminal's Connect form, together
  with the host address, port, CA certificate and certificate fingerprint;
- the **MCP credential** is for `serial-nexus-mcp` running on the laptop whose
  terminal owns the gateway. The MCP server uses that local gateway to connect
  onward to the sharing host. Add the remote endpoint, CA certificate,
  fingerprint and credential to the local MCP process's private `mcp-peers.json`
  file with mode `0600`; follow the setup and tool-call examples in
  [`serial-nexus-mcp/README.md`](../crates/serial-nexus-mcp/README.md#reach-a-device-shared-by-another-host).

Do not paste either credential into shell history, a repository, an issue or a
tool call. If you write the MCP credential down, use a file only your user can
read (mode `0600`) outside the repository, for example
`~/.config/serial-nexus/remote-mcp-credential`. Credentials are per sharing
session: stopping or restarting sharing invalidates the old ones and new ones
must be paired.

**Exposure.** The listener exists only on the chosen address, and only while
the terminal that started sharing is running. Firewall rules and router/NAT
configuration decide who can actually reach it; forwarding a port can expose it
outside the LAN. Prefer loopback (`127.0.0.1` or `::1`) for same-machine use and
a trusted LAN address only when another device must connect.

**Unplug and revocation.** Unchecking a device while sharing revokes remote
access immediately; unplugging it, or replacing it at the same path, clears its
checkbox and requires a recheck. A write already in progress may finish, and a
write whose answer is lost is reported as uncertain: nothing is retried or
rolled back.
