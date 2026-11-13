# serial-nexus-mcp

The AI's SerialNexus client: an [MCP](https://modelcontextprotocol.io) server
over stdio, launched by an AI host. It speaks newline-delimited JSON-RPC 2.0 on
stdin/stdout and sends **every** tool call—local or to an explicit remote
endpoint—through the one supported foreground `serial-nexus-terminal` running
on this laptop. The terminal hosts a bounded, versioned, per-user Unix-socket
gateway and owns one upstream core or authenticated remote connection per MCP
session and endpoint.

```
AI host <-- MCP stdio --> serial-nexus-mcp <-- per-user IPC --> serial-nexus-terminal gateway
                                                        <-- core socket or verified TLS --> core / paired remote host
```

The terminal must be running for local **and remote** MCP calls. If it is not,
MCP calls fail clearly: the MCP process does not start the core and does not
make network connections itself. Local-only MCP calls work while LAN sharing is
off; they do not require pairing. Closing the terminal disconnects its local
and outgoing remote MCP sessions and stops any share it owns. A second
supported terminal exits clearly. Sharing is never enabled by MCP.

**Same-user trust:** all programs running as this laptop user are trusted.
Another same-UID program can impersonate the terminal/client role, access local
serial through the core, or enable/change LAN sharing. Both sides verify the
Unix peer UID before exchanging gateway messages, excluding other local OS
users even if they pre-create the fallback socket path. The terminal gateway is
the supported route and lifetime owner, not a security barrier against the
same user. Remote clients still require valid pairing and are restricted to
their role and the session allowlist.

`serial-nexus-mcp` never opens a serial port itself and never owns a local core
connection or remote network connection: the terminal gateway creates a
separate upstream client per MCP session/endpoint. The core still enforces one
AI writer per device, baud conflicts, idle expiry, history, and device
open/close.

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

The terminal and MCP host must use the same gateway socket. Its default is
`$XDG_RUNTIME_DIR/serial-nexus/gateway.sock` (or the per-user temporary runtime
path when no runtime directory exists); set `SERIAL_NEXUS_GATEWAY_SOCKET` in
both environments to override it. The terminal starts the core as needed and
hosts the gateway for its foreground lifetime. `SERIAL_NEXUS_SOCKET` selects
the core socket used by the terminal; it does not let MCP bypass the gateway.
The MCP process has no core binary auto-start path.

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

## Reach a device shared by another host

LAN sharing is opt-in on the host: its human terminal starts a listener for one
concrete address and port, checks the devices to share, and shows pairing
details. This client uses those details to reach the shared devices. It never
starts or stops sharing, and it never falls back to the local core when a
remote endpoint fails.

### Pairing file

The credential is **not** a tool argument and never appears in MCP output,
errors, logs, history or serial writes. The human writes one private JSON file,
readable by the laptop user only. The MCP process sends only the configured
file path through local gateway IPC; the terminal gateway opens and validates
the file, then performs the paired TLS connection itself:

1. On the host, open terminal Settings and choose the detected devices to
   share, save, and start sharing. Press `p` with the sidebar focused to open
   the owner-only Pairing view; read the endpoint, CA certificate,
   certificate fingerprint and **MCP credential** there. These details are
   created for this session only. See
   [`docs/sharing.md`](../../docs/sharing.md) for the core contract.
2. create `~/.config/serial-nexus/mcp-peers.json` (`$XDG_CONFIG_HOME` is
   honoured, and `SERIAL_NEXUS_MCP_PEERS` names another file; if none of these
   is set the server says so instead of reading a relative file) with mode `600`:

```sh
install -d -m 700 ~/.config/serial-nexus
install -m 600 /dev/null ~/.config/serial-nexus/mcp-peers.json
$EDITOR ~/.config/serial-nexus/mcp-peers.json
```

```json
{
  "version": 1,
  "peers": [
    {
      "endpoint": { "address": "192.0.2.10", "port": 7000 },
      "paired": {
        "address": "192.0.2.10:7000",
        "host_name": "192.0.2.10",
        "ca_cert_der_b64": "<pairing CA from the host terminal>",
        "host_cert_fingerprint_sha256": "<64 hex characters from the host terminal>"
      },
      "secret": {
        "role": "ai",
        "credential": "<the AI pairing credential from the host terminal>"
      }
    }
  ]
}
```

- One entry per endpoint; two entries for the same address, scope and port are
  refused as ambiguous, and a file larger than 64 KiB is refused. IPv4 and IPv6
  are accepted (`"address": "::1"`, `"address": "fd00::20"`). For a scoped
  link-local IPv6 host, set `endpoint.scope_id` to the MCP machine's numeric
  interface index and pass the same client-local scope in the tool's `ip`
  argument (for example `"fe80::20%3"`). Keep the host's pairing address and
  `host_name` as shown; their host-local scope is not used to choose the
  connecting interface.
- Use the **AI** credential (`"role": "ai"`) for this server. A human
  credential is refused: the role is enforced by the host, not chosen here.
- The file must be a regular file owned by you with no group or world
  permission bits; a symbolic link, another owner, or mode `644` is refused
  with a message that says which rule failed. The file contents are never
  echoed.
- Keeping the file outside the repo and out of anything an AI reads is the
  point: the credential is a bearer secret for every device the host checks.

Pairing details last only as long as the host's sharing session. When the host
terminal closes, stops sharing, or restarts it (fresh certificate and
credentials), the next remote call fails with a message saying the pairing is
stale. Update the file, then call again; no write is replayed and no local
device is used.

### Tool arguments

Every tool takes the same optional pair:

| Argument | Meaning |
| --- | --- |
| `ip` | The sharing host's IPv4 or IPv6 address; link-local IPv6 may include a numeric interface scope such as `fe80::20%3`. |
| `port` | The sharing listener's TCP port. |

Give both to use that host, or neither for the local core. Giving only one is a
parameter error. `connect` keeps its local `port` (the device path); with `ip`
set, `port` is the host's TCP port and `path` is the device path on that host:

```json
{ "name": "connect", "arguments": { "ip": "192.0.2.10", "port": 7000, "path": "/dev/ttyUSB0", "request_control": true } }
```

A device path for a remote endpoint is the path **on the host**. It is never
resolved against this machine's filesystem, because a client-side symlink of
the same name could point at a different device. A path this client cannot
match to a remote connection is an error, not a guess.

Results from a remote endpoint say so (`on remote 192.0.2.10:7000`) and carry
`structuredContent.remote_endpoint` with the address, port and (for scoped IPv6)
`scope_id`; local results are unchanged. Device ids,
control and history are kept separately per endpoint, so two hosts that both
share `/dev/ttyUSB0` never collide. Only devices the host checked are visible,
and an uncheck or owner termination revokes access immediately.

A failed or uncertain write is reported as such and is never sent again.

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
  is slow every call is slow. MCP keeps per-session device ids and endpoint
  routing; the terminal gateway owns the corresponding upstream clients.

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
and failure without the terminal gateway without auto-starting the core.

`tests/remote_mcp.rs` covers the remote path: local calls unchanged with no
pairing file, a real loopback sharing session reached through the terminal
gateway with `ip` + `port`,
endpoint and address validation, an unknown endpoint, a human credential
refused for the AI role, a host that cannot prove the pinned certificate, an
unreadable pairing file, two hosts with equal device ids kept apart, one AI
writer across local and remote, concurrent whole writes, unchecking a device,
owner termination stopping a write without replay, an abrupt host-process loss
during a remote write (uncertain, never replayed, no local fallback), an IPv6
endpoint, a credential misplaced into a file field and never echoed, a remote
path not resolved locally, and a rotated credential recovered by updating the
file.

The stderr secrecy assertions read the running server's output, so the test
harness publishes stderr as it arrives (proven by a test that writes to the
live process's stderr) rather than only when the server exits.

Two hosts that share the *same* device path are kept apart end to end: each
core runs in a private mount namespace with its own PTY bound over one shared
placeholder, and writes, read history and session state stay with the endpoint
the call named. A separate host-side symlink alias test checks that remote
discovery returns the canonical path the MCP client can use. Where a machine
forbids unprivileged user namespaces, the same-path test says so and skips; the
endpoint-keyed bookkeeping unit test still runs. No physical hardware is used.

The ignored, never-flashing real-board MCP stdio gate is in
[`tests/board_gate.rs`](tests/board_gate.rs); exact-head instructions, isolated
temporary credential/socket setup and safe echo-probe guidance are in
[`docs/board-gate.md`](../../docs/board-gate.md). The gate uses the canonical
path reported by the local open / remote discovery, since `/dev/serial/by-id`
symlinks can appear remotely as `/dev/ttyACM*`.
