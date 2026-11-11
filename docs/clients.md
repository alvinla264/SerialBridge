# Clients and interfaces

## Using the client library

The MCP server and the terminal both use `serial-nexus-client` rather than
speaking the socket themselves. The quickest path:

```rust
use serial_nexus_client::{Client, ClientConfig, ClientInfo, WaitSpec};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Starts serial-nexus-core if it is not already running, then says hello.
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

### Remote (LAN) transport

`RemoteClient` reaches devices a host shares on the local network. It is the
same transport the MCP server uses today and the terminal will use for its
sharing controls. It never talks to the Unix socket, and a network failure is
an error rather than a quiet local retry.

```rust
use std::net::{IpAddr, Ipv4Addr};
use serial_nexus_client::{RemoteClient, RemoteEndpoint};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
// The exact address and port, plus the pairing information and credential the
// host terminal showed. In a real program the pairing comes from configuration,
// not from a tool argument.
let endpoint = RemoteEndpoint::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)), 7000);
let paired = /* PairedHost from the host's pairing details */;
let secret = /* PairingSecret: role plus credential */;
let client = RemoteClient::connect_to(endpoint, &paired, &secret, Some("my-client".into()), std::time::Duration::from_secs(5))?;
let devices = client.list_devices()?;
# Ok(())
# }
```

| Call | What it does |
| --- | --- |
| `RemoteEndpoint::new(ip, port)` | The address and port to dial. `validate()` refuses port 0, wildcards, multicast and broadcast; IPv4-mapped IPv6 is the same endpoint as its IPv4 form. |
| `RemoteClient::connect_to(endpoint, paired, secret, name, timeout)` | Dial exactly that endpoint. Refuses an endpoint the pairing does not describe (`ClientError::EndpointMismatch`). |
| `RemoteClient::connect(paired, secret, name, timeout)` | The same, dialling the pairing's own address. |
| `list_devices`, `open`, `close`, `read`, `write_bytes`, `status` | Same shapes as `Client`. |
| `role()`, `auth()`, `limits()` | What the host authenticated this connection as, and its bounds. |
| `is_connected()`, `subscribe()` | Connection state and events; a close also arrives as `Event::TransportClosed`. |

Authentication happens before any request:

1. the host must present a certificate chain that verifies under the pairing's
   certificate authority **and** a leaf certificate whose SHA-256 equals
   `host_cert_fingerprint_sha256`; a different certificate is refused and the
   credential is not sent;
2. the pairing credential is then sent as the first frame, and the host derives
   the role from it. The client checks that the role the host reports is the
   role the pairing promised, so a credential used under the wrong role is
   refused.

Frames are bounded by `MAX_REMOTE_FRAME_BYTES`, the handshake and pairing have
deadlines, and a write whose answer is lost becomes `ClientError::WriteUncertain`
(`may_have_reached_device`): the transport never replays it. `is_write_uncertain()`
recognizes both that and a host answer carrying the same flag.

### The private pairing file

An MCP host cannot be trusted to keep a credential out of AI-visible tool
arguments or history, so the human configures one private file instead.
`Peers` loads it:

- path: `SERIAL_NEXUS_MCP_PEERS`, else `$XDG_CONFIG_HOME/serial-nexus/mcp-peers.json`,
  else `~/.config/serial-nexus/mcp-peers.json`;
- shape: `{ "version": 1, "peers": [ { "endpoint": {"address", "port"}, "paired": {...}, "secret": {"role", "credential"} } ] }`;
- it must be a regular file owned by the current user with no group or world
  permission bits; a symlink, another owner or mode `644` is refused
  (`PeerError`), and no error message contains the credential;
- one entry per endpoint; duplicate endpoints are refused as ambiguous;
- the entry's endpoint, the pairing's address and the pairing's TLS name must
  all be the same endpoint, and the certificate fingerprint must be 64
  lowercase hex characters.

`Peers::find(endpoint)` returns the one entry for exactly that address and
port. [`crates/serial-nexus-mcp/README.md`](../crates/serial-nexus-mcp/README.md)
shows the file a user writes.

### Auto-start and packaging

`Client::connect` tries the socket first. If nothing answers and auto-start is
on, it starts `serial-nexus-core` detached (its own session, so Ctrl-C in the
client that launched it does not kill the shared core) and polls until the
socket accepts. The core locks its socket directory, so clients racing to start
it still end up with one core. `spawn_core()` returns the new process id.

The executable is found in this order:

1. `ClientConfig::core_binary` (an explicit path, for embedding);
2. `$SERIAL_NEXUS_CORE`;
3. the path compiled in with `SERIAL_NEXUS_CORE_DEFAULT` at build time (the
   Nix flake points this at its `serial-nexus-core` store path, so a client
   installed on its own still starts the core it was built against);
4. a `serial-nexus-core` next to the current executable (what `cargo build`
   gives);
5. `serial-nexus-core` on `PATH`.

A packaged client that keeps the core elsewhere sets either the explicit path
or `SERIAL_NEXUS_CORE` at runtime; `find_core_binary()` and `spawn_core()` are
public for wrappers with their own launch strategy.

### Human terminal

`serial-nexus-terminal` is the human client. It connects with `Client::connect`
(so a missing core is started automatically) as a human client and never takes
AI write control.

- At a usable width the terminal shows the device sidebar on the left and the
  right pane beside it: the shown device's stream, its scrollback, or the
  Settings view. A narrow terminal cannot fit both, so it shows the focused
  pane only. `Ctrl+]` moves the keyboard focus between the sidebar and the right
  pane; the header names the focused pane, the focused pane's title (or the
  pane edge) is highlighted, and the visible cursor follows the focus — the
  device's own cursor in the stream, the highlighted row in the sidebar, and
  the selected flag in the settings. While the sidebar has the focus, the
  stream still marks the device's live cursor with a reversed cell. The focus
  can move to the sidebar even while the settings are open; the draft is kept
  either way.
- The sidebar's highlighted row and the device shown in the right pane are
  separate. A single click (or arrow key) only moves the highlight; it never
  changes the stream, so choosing a row never redirects a write. `Enter` or a
  double-click opens a closed device (after the baud prompt, default `115200`)
  or joins an open one at its active rate and shows it. Other attached devices
  stay attached. The row markers tell them apart: `▸` is both highlighted and
  shown, `›` only highlighted, `▹` only shown.
- If the shown device disconnects, a notice says so, the terminal stays open,
  and the other attached sessions stay attached: the right pane shows the next
  one and the keyboard focus returns to the sidebar (scrollback keeps its
  navigation keys). Opening or joining a device takes typing again.
- In the sidebar, `↑`/`↓` (or `j`/`k`) highlight, `Enter` (or `o`) opens or
  joins the highlighted device, `b` shows the highlighted attached device's
  scrollback (or, for a closed device that has none, the baud prompt) and `d`
  disconnects the highlighted device. The bottom row is the Settings button:
  highlight it with the arrow keys and open it with `Enter`, or click it with
  the mouse. `Ctrl+Q` quits from here, even while the baud prompt is open. `]`
  sends a literal `Ctrl+]` to the shown device while that device's stream is in
  the right pane; it writes nothing while the settings or scrollback are shown,
  and no other key typed in the sidebar reaches a device. `Ctrl+]` during the
  baud prompt drops the prompt and moves the focus. Opening the settings
  abandons an open baud prompt, and clicking the visible right pane takes the
  keys back from a pending prompt.
- In the focused stream, keys are sent to the shown device as-is: arrows are
  ANSI sequences, Enter is CR and Backspace is DEL, and `Tab`, `Ctrl+Q` and
  every printable key go to the device. `Ctrl+]` is the only plain key the
  terminal keeps, to move the focus back to the sidebar; that is why the
  sidebar's `]` sends the literal byte. The header always names the shown
  device, its active baud, the AI control holder, whether this terminal is
  attached and which pane has the focus.
- The stream keeps its scrolled-off rows (up to 1000) while the `scrollback`
  setting is on, so earlier output can be read again. `Ctrl+]` focuses the
  sidebar and `b` there opens the scrollback for the attached device under the
  highlight; `↑`/`↓` move a line, `PageUp`/`PageDown` move a page, the mouse
  wheel moves three lines a notch, and `Esc` returns to that device's live
  stream (as does opening the device again). The scrollback view starts at the
  live screen; its status line says how far back the view is, and its keys are
  never sent to the device. The device cursor is hidden there. With the setting
  off nothing is kept, `b` says so in the footer instead of opening an empty
  view, and turning the setting off frees what was kept. Each device keeps its
  own scrollback and scrolled position, which a focus switch or a resize never
  resets.
- Device output is rendered through a small ANSI screen (cursor movement,
  erase, SGR colours/attributes, OSC titles ignored). AI writes appear inline in
  a distinct colour, without labels; human writes are not added to the stream as
  events.
- Local echo is on by default: the keys a person types in the focused stream are
  shown in the screen's default style once the device accepted the write, so
  typing is visible on a device that does not echo. Turn it off in the settings
  when the device echoes what it receives.
- The settings open in the right pane while the sidebar stays visible. The view
  lists both flags, `local_echo` and `scrollback`. `Tab`, `Shift+Tab` and
  `↑`/`↓` move between them, `Space` (or `Enter`) toggles the selection, `Ctrl+S`
  saves the file and only then puts the changes into effect, and `Esc` throws
  every change away. With the mouse, a flag row toggles on click, and the
  `[ Save ]` and `[ Cancel ]` buttons on the page do what they say — a mouse
  save uses the same save-then-apply path as `Ctrl+S`. Save and Cancel both
  return to the shown device's live stream. A save that fails — including a
  launch with no settings file to write — leaves the options as they were, says
  so in the footer, and stays in the settings so the draft is not lost; the
  terminal never reports a save it did not make. While the settings hold an
  unsaved draft, a device opening from the sidebar (Enter or a double-click) is
  refused with a footer message and the draft is kept, even if the sidebar was
  clicked; Save or Cancel is the way out. Scrollback is saved like the other
  flag: turning it off frees every session's kept rows and brings the view back
  to live, and turning it on starts keeping rows from then on.
- The footer has a status line (errors, unplug notices, setting changes, the
  baud prompt) and a line of key hints for the focused pane. A short terminal
  keeps the status line and hides the hints.
- Settings are stored per user in `$XDG_CONFIG_HOME/serial-nexus/terminal.json`
  (`~/.config/serial-nexus/terminal.json` when `XDG_CONFIG_HOME` is unset and
  `HOME` is set). The file holds `local_echo` and `scrollback`, both on by
  default; the terminal loads them at launch and writes the defaults on the
  first launch. An invalid or unreadable file is reported in the footer and
  left untouched, never silently replaced, and settings changed in that launch
  are not saved; saves go to a temporary file and are renamed into place, and a
  failed save is reported and changes nothing.
- `Ctrl+Q` in the sidebar (or `F10`) quits. Normal exit, I/O errors and panics
  restore raw mode, the alternate screen and mouse capture, and explicitly
  disconnect every device. Catchable termination signals (SIGTERM, SIGHUP) are
  not handled and may leave the terminal in raw mode; SIGKILL cannot be
  handled.

Tests drive the real client/core seam through PTY-backed simulated devices:
opening by prompt, Enter and double-click, joining without reopening, a single
click only highlighting a row and never redirecting a write, focus switching
with `Ctrl+]`, the stream owning `Tab` and `Ctrl+Q` while the sidebar quits on
`Ctrl+Q`, the sidebar sending a literal `Ctrl+]` to the shown device, `b` from
the sidebar opening the scrollback and `Esc` returning to the live stream,
scrolling with the arrow and page keys through output kept by the `scrollback`
setting (and nothing kept when it is off), scrollback keys not reaching the
device, settings drafts saved on `Ctrl+S` and discarded on `Esc`, an unsaved
draft refusing device opening, a resize keeping the session, draft and scroll
position, a disconnect keeping the settings and other sessions, device echo,
visible AI writes, baud conflicts, unplug notices, explicit disconnect and
shutdown.
State/input/render and the terminal cleanup guard are tested without a real
terminal.

### Core CLI

```
serial-nexus-core [--socket PATH] [--grace-secs N] [--control-idle-secs N]
                  [--history-lines N] [--default-baud N]
```

The same settings can come from `SERIAL_NEXUS_SOCKET`,
`SERIAL_NEXUS_GRACE_SECS`, `SERIAL_NEXUS_CONTROL_IDLE_SECS`,
`SERIAL_NEXUS_HISTORY_LINES` and `SERIAL_NEXUS_DEFAULT_BAUD`. Clients normally
never run it by hand.

## MCP server (`serial-nexus-mcp`)

`serial-nexus-mcp` is an MCP server over stdio: the AI host launches it and it
reaches the devices through `serial-nexus-client` and the core, starting the
core when needed. It never opens a serial port itself, so the core's rules (one
AI writer per device, baud conflicts, idle expiry, human writes) still apply.

```json
{
  "mcpServers": {
    "serial": { "command": "/path/to/serial-nexus-mcp" }
  }
}
```

or, without installing anything, point the host at the flake (some hosts want
one command and no arguments):

```json
{
  "mcpServers": {
    "serial": {
      "command": "nix",
      "args": ["run", "github:alvinla264/SerialNexus#serial-nexus-mcp"]
    }
  }
}
```

After `nix profile install github:alvinla264/SerialNexus`, or `cargo install` of
the three crates, `"command": "serial-nexus-mcp"` works too. The core is started
by the first tool call; the host does not need to launch `serial-nexus-core`
itself.

Tools: `list_devices` (paths, open state, baud, control holder), `connect`
(`port`, optional `baud` and `request_control`; reports active baud and either
granted AI control or read-only with the holder), `disconnect`, `read` (recent
history lines labelled `device`/`ai`/`human`, partial lines marked, truncation
and drops reported) and `send` (exact text, no newline added; waits for the
requested device-output lines or a bounded timeout and returns what arrived,
including a marked partial line). One MCP session can connect to several
devices. There is no continuous stream: poll `read`.

See [`crates/serial-nexus-mcp/README.md`](../crates/serial-nexus-mcp/README.md)
for the tool reference, protocol notes and limitations (text only, one request at
a time per session, bounded waits and reads, Linux only).
