# serial-nexus-mcp

The AI's SerialNexus client: an [MCP](https://modelcontextprotocol.io) server
over stdio, launched by an AI host. It speaks newline-delimited JSON-RPC 2.0 on
stdin/stdout and reaches the serial devices through the one local
`serial-nexus-core` (using `serial-nexus-client`), starting the core when nothing
answers.

```
AI host  <-- MCP over stdio -->  serial-nexus-mcp  <-- Unix socket -->  serial-nexus-core
```

`serial-nexus-mcp` never opens a serial port itself and never takes ownership:
the core still enforces one AI writer per device, baud conflicts, idle expiry,
the human terminal's writes, and device close/open.

## Configure the host

Any MCP host that can launch a stdio server works. For example, a client
configuration entry:

```json
{
  "mcpServers": {
    "serial": {
      "command": "/path/to/serial-nexus-mcp"
    }
  }
}
```

A Nix host can run it straight from the flake instead:

```json
{
  "mcpServers": {
    "serial": { "command": "nix", "args": ["run", "github:alvinla264/SerialNexus#serial-nexus-mcp"] }
  }
}
```

The core is found like every other SerialNexus client:

1. `SERIAL_NEXUS_SOCKET` (or the standard per-user socket path);
2. `SERIAL_NEXUS_CORE` for an explicit `serial-nexus-core` binary, else the path
   the build compiled in (`SERIAL_NEXUS_CORE_DEFAULT`, set by the Nix flake),
   else a `serial-nexus-core` next to `serial-nexus-mcp`, else one on `PATH`.

With no core running, the first tool call starts one detached; it exits again
after its grace period once no clients remain.

## Tools

| Tool | Arguments | What it does |
| --- | --- | --- |
| `list_devices` | - | Every device the core sees: open/closed, active baud, client count, AI control holder. |
| `connect` | `port`, `baud?`, `request_control?` | Open or join a device; returns the device id, active baud and either granted AI control or read-only with the holder's name. |
| `disconnect` | `device` | Detach; the last client closes the device and releases baud and control. |
| `read` | `device`, `lines?` | The newest buffered lines (default 50, max 2000), each labelled `device`, `ai` or `human`; partial lines are marked. |
| `send` | `device`, `text`, `lines?`, `timeout_ms?` | Write the exact text (no newline added), then wait for `lines` device-output lines received after the send (default 1; 0 skips the wait) up to `timeout_ms` (default 1000 ms, core max 30000 ms). |

`device` is the id `connect` returned, or an open device's path (canonicalized
the way the core does, so a `/dev/serial/by-id/...` alias keeps working). A
reconnect to the same path replaces the old device id. One MCP session may
connect to several devices; ids, control and history are per device.

`connect` follows the core's rules:

- a device another client already opened keeps its **active baud** when `baud`
  is omitted, and an explicit different rate is rejected with the active rate;
- an unopened device opens at 115200 when no rate is given;
- repeating `connect` claims available control without disconnecting first;
- when another AI holds write control, the call succeeds **read-only** and
  names the holder and its remaining idle time. `send` then fails with a clear
  "write control" error until control is free or `connect` with
  `request_control: true` succeeds.

`send` returns what arrived when it times out, including a partial line (output
with no newline yet) marked `(partial)`, and reports the timeout in the text.
On timeout the core also returns the device's current partial tail, which may
predate the send; such a line is marked `(partial, present before this send)`
and carries `predates_send: true`. More lines than the core's maximum are
capped and the reply says so. There is no continuous stream: each call returns
one bounded batch. Poll `read` for more history.

Every tool returns human-readable text plus `structuredContent` with the same
facts (`device_id`, `baud`, `role`, `write_control`, `lines`, `timed_out`,
`truncated`, ...). Failures come back as `isError: true` tool results with a
message that says what to do next. Malformed calls are JSON-RPC errors
(`-32602`): unknown tools, unknown or misspelled arguments, and wrong types
(including `null` for a typed argument) are refused before any device action,
so a typo cannot open a device at the wrong rate.

## Protocol notes

- Framing: one JSON-RPC message per line. Incoming messages are bounded to
  1 MiB, like every SerialNexus message. JSON-RPC batches (arrays) are
  supported and answered as one array in request order; a notification-only
  batch gets no reply.
- `initialize` negotiates `2024-11-05`, `2025-03-26` or `2025-06-18` (echoing
  the host's version when it is one of those, else the latest). Capabilities:
  `tools` only.
- `ping`, `tools/list`, `tools/call` and the `notifications/*` a host sends are
  handled; notifications are never answered. Envelopes JSON-RPC forbids (a
  null, boolean, object or fractional `id`, a scalar `params`, a missing
  method) are rejected with `-32600` before dispatch.
- Integer arguments follow JSON Schema: `1.0` is accepted as `1`, `1.5` is
  refused.
- Requests are handled one at a time, in arrival order. That keeps serial
  writes in the order the host sent them; the cost is that a `send` waiting for
  device output delays the next request on the same session. Run a second
  session if a host needs concurrent work on other devices.

## Limitations

- **Text, not bytes.** `send` takes JSON text; invalid UTF-8 from a device is
  shown as U+FFFD, and binary protocols are not first-class.
- **No notifications, resources or prompts.** Poll `read`; only the `tools`
  capability is advertised.
- **No terminal emulation.** The core splits history into lines and trims a
  trailing `\r`; ANSI escape sequences pass through untouched and are not
  interpreted or stripped.
- **Bounded waiting.** `send` waits at most the core's 30 s maximum, for at
  most 2000 device-output lines (AI and human writes are not counted), so a
  device that never answers stays a timeout. A session handles one request at a
  time, so a long wait also holds up that session's next call.
- **Scanning is bounded.** `read` returns at most 2000 lines and the core may
  trim a response to its frame budget (`truncated: true`). The MCP wrapper adds
  its text and structured copy of that bounded reply, and JSON escaping can
  expand control characters, so an outgoing response is not itself capped at
  1 MiB (only incoming messages are).
- **Linux only**, like the core: Unix socket, `SO_PEERCRED`, PTY tests.
- **A tool call is bounded by the core's per-request handling**, so if the core
  is slow every call is slow; the MCP server itself keeps no state beyond the
  client connection and the device ids it has seen.

## Building and testing

The workspace checks cover this crate:

```sh
nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#rustfmt nixpkgs#clippy -c sh -c '
  cargo fmt --all --check &&
  cargo clippy --workspace --all-targets -- -D warnings &&
  cargo test --workspace
'
```

`tests/mcp_stdio.rs` drives the built binary over real stdio framing against a
real core and simulated serial devices (PTY pairs): handshake and tool listing,
protocol errors, batched requests, pipelined sends keeping wire order, default
baud, joins at the active rate, rejected baud conflicts, two independent
sessions where the second is read-only, exact writes without an appended
newline, waited sends returning device lines, partial lines (new and
pre-existing), timeouts, capped line counts, disconnect releasing control,
detached-session errors, path aliases, a missing device, an unplugged device,
and auto-start with an explicit core path.
