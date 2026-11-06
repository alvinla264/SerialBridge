# SerialBridge

One local core owns the serial devices; AI clients and the human terminal share
them through it. This repository currently contains the **foundation**: the
Cargo workspace, `serial-core`, and the reusable Rust client/IPC library. The
MCP server, the human terminal and the Nix packaging are separate follow-up
tasks that build on the client library documented here.

```
            serialbridge-client  (this repo: shared Rust API + auto-start)
               /            \
   serial-mcp (follow-up)   serial-terminal (follow-up)
               \            /
                Unix socket (one per user)
                     |
                 serial-core  -- one process per local user
                     |
              /dev/ttyUSB0, /dev/ttyACM0, ...
```

## What exists now

| Crate | Purpose |
| --- | --- |
| `serialbridge-protocol` | Wire types and newline-delimited JSON framing shared by the core and clients. |
| `serialbridge-client` | Thread-safe client: discovery, open/join, write control, read, write, live events, lifecycle, core auto-start. |
| `serial-core` | The `serial-core` binary and its engine: one core per user, one writer per device, history, timers. |

One `cargo build --release` produces `serial-core` (the only binary in this
slice; the client crates are libraries for the next tasks).

## Behaviour and defaults

All defaults are bounded and reported to clients in the `hello` response
(`Limits`), so client tools and docs never hard-code different numbers.

| Setting | Default | Notes |
| --- | --- | --- |
| Baud | `115200` | Used when a new device is opened without a rate. |
| Framing | 8N1, no flow control | Baud rate is the only configurable serial setting. |
| Core exit grace | 30 s | The core exits 30 s after its last client disconnects. |
| AI control idle expiry | 5 min | Any request from the holder on that device refreshes it. |
| History | 2000 lines / 256 KiB per open device | In memory only; discarded when the device closes. Drops are counted and reported. |
| Read lines | 50 default, 2000 max | The AI chooses per call. |
| Write wait | 1000 ms default, 30 s max | The AI chooses per call. |
| Default socket | `$SERIALBRIDGE_SOCKET`, else `$XDG_RUNTIME_DIR/serialbridge/core.sock`, else `$TMPDIR/serialbridge-<uid>/core.sock` | Directory 0700, socket 0600, peer uid checked. |

Device rules:

- **Opening.** The first opener chooses the baud rate. A later client may join
  with no rate (the active rate) or the same rate; a different rate is rejected
  with `BaudConflict` and the active rate in the error details. Paths are
  resolved (`fs::canonicalize`) before lookup, so a symlink such as
  `/dev/serial/by-id/...` and `/dev/ttyUSB0` name the same open device, and
  discovery canonicalizes `by-id` aliases so each device is listed once.
- **Closing.** The last client disconnecting closes the device and releases its
  rate. Every write already accepted is drained to the device before the close
  completes, and the core waits for the device's I/O threads to release the
  serial file before it reports the close (or broadcasts `DeviceClosed`), so an
  immediate reopen works from any client.
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

## Using the client library

Both follow-up clients should use `serialbridge-client` rather than speaking
the socket themselves. The quickest path:

```rust
use serialbridge_client::{Client, ClientConfig, ClientInfo, WaitSpec};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Starts serial-core if it is not already running, then says hello.
    let client = Client::connect(ClientConfig::new(ClientInfo::ai("my-agent")))?;

    // Live updates for a human-facing view.
    let events = client.subscribe();

    for device in client.list_devices()?.devices {
        println!("{} open={} baud={:?}", device.path, device.open, device.baud);
    }

    // Open and claim write control.
    let opened = client.open("/dev/ttyUSB0", Some(115_200), true)?;
    println!("role {:?}, control {:?}", opened.role, opened.control);

    // Recent history, then send exactly "version\r" and wait for one line.
    let history = client.read(opened.device_id, Some(20))?;
    let written = client.write_bytes(
        opened.device_id,
        b"version\r",
        Some(WaitSpec { lines: 1, timeout_ms: Some(2_000) }),
    )?;
    println!("{} lines, timed out: {}", written.lines.len(), written.timed_out);

    client.close(opened.device_id)?;
    Ok(())
}
```

### API surface

| Call | What it does |
| --- | --- |
| `Client::connect(config)` | Connect, starting the core when needed. |
| `Client::connect_to_socket(path, info)` | Connect to a running core without auto-start (tests, embedding). |
| `hello()` | Core version, client id, socket and `Limits`. |
| `list_devices()` | `Vec<DeviceSummary>`: every detected device plus open devices (baud, client count, control holder). |
| `open(port, baud: Option<u32>, request_control)` | Open or join; returns `OpenResult { device_id, port, baud, role, control }`. |
| `close(device_id)` | Detach; the last client closes the device. |
| `read(device_id, lines: Option<usize>)` | `ReadResult { lines, dropped_lines, truncated }`; newest lines last; a partial tail is included and marked. |
| `write_bytes(device_id, bytes, wait: Option<WaitSpec>)` | Send exact bytes; optionally wait for post-write device lines. `WriteResult { bytes_written, lines, timed_out, truncated }`. |
| `status(device_id)` | `DeviceStatus` with control holder and history counts. |
| `subscribe()` | `Receiver<Event>`; each subscriber gets every future event. |
| `is_connected()` | False after the connection closes (a `TransportClosed` event is also sent). |
| `socket()` | The socket this client is using. |

`Event` variants cover raw device output, write echoes with source, client
join/leave, control granted/released/denied, device closed and core shutdown.
`DeviceOutput` and `Write` carry the original bytes (base64 on the wire),
decoded with `event.bytes()`.

Errors are `ClientError`. Use `error.code()` to match the core's `ErrorCode`
(`BaudConflict`, `ControlHeld`, `NotAttached`, `DeviceClosed`, `DeviceFailed`,
`NotFound`, `PermissionDenied`, ...); `is_control_held()` and
`is_device_gone()` cover the two common cases.

### Auto-start and packaging

`Client::connect` tries the socket first. If nothing answers and auto-start is
on, it starts `serial-core` detached (its own session, so Ctrl-C in the client
that launched it does not kill the shared core) and polls until the socket
accepts. The core locks its socket directory, so clients racing to start it
still end up with one core. `spawn_core()` returns the new process id.

The executable is found in this order:

1. `ClientConfig::core_binary` (an explicit path, for packaging);
2. `$SERIALBRIDGE_CORE`;
3. a `serial-core` next to the current executable (what `cargo build` gives);
4. `serial-core` on `PATH`.

A packaged client that keeps the core elsewhere should set either the explicit
path or `SERIALBRIDGE_CORE`; `find_core_binary()` and `spawn_core()` are public
for wrappers with their own launch strategy.

### Core CLI

```
serial-core [--socket PATH] [--grace-secs N] [--control-idle-secs N]
            [--history-lines N] [--default-baud N]
```

The same settings can come from `SERIALBRIDGE_SOCKET`,
`SERIALBRIDGE_GRACE_SECS`, `SERIALBRIDGE_CONTROL_IDLE_SECS`,
`SERIALBRIDGE_HISTORY_LINES` and `SERIALBRIDGE_DEFAULT_BAUD`. Clients normally
never run it by hand.

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
  path, a missing binary, and the started core owning a separate session).

Not proven here: real hardware and drivers, PTY behaviour that differs from
USB serial adapters, platforms other than Linux, and `libudev`-based
enumeration (device discovery reads `/dev/serial/by-id` and `/sys/class/tty`).
