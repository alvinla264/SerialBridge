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

This release's MCP server is local-only: it reaches the core on this machine and
has no remote pairing option yet. A sharing session still issues a separate MCP
credential for the follow-on remote MCP client. Keep it private — a file only
your user can read (mode `0600`), outside the repository and never in shell
history or a tool call — and do not add it to the local MCP host configuration,
which would send it nowhere useful.

### Sharing devices on the LAN from the terminal

Sharing is opt-in and off by default. The sidebar has `[ Share: off ]` between
the device list and the Settings button; `s` does the same from the sidebar.
Nothing listens on the network until it is started deliberately.

1. Open Settings and type the interface address and port the host should listen
   on: one concrete IPv4 or IPv6 address (for example `192.168.1.20` or `::1`).
   A wildcard such as `0.0.0.0` or `::` is refused. The address and port are
   saved preferences; the device checklist is not.
2. Tick the detected devices to share. A device does not need to be open by the
   host, and at least one must be ticked before sharing can start. `Space` or
   `Enter` toggles a checkbox; clicking it does the same.
3. `Ctrl+S`/`[ Save ]` writes the preferences and applies the checklist. `Esc`/
   `[ Cancel ]` throws both away. A save that fails — a refused checklist change
   or a settings file that cannot be written — applies neither the checklist nor
   the preferences and keeps the draft.
4. Start sharing with the Share button or `s`. A bind failure is shown in the
   footer and sharing stays off. Once on, the pairing view shows the bound
   address and what a remote client needs.

The pairing view (`p`, or `Enter` on the running Share button) shows the host
name, the host certificate's SHA-256 fingerprint, the session's CA certificate
(base64 DER) and two separate credentials: one for the human terminal, one for
MCP/AI clients. The credentials are shown only to the owner, are never written
to the settings file or a device, and disappear when sharing stops. Use `↑`/`↓`
to scroll the long certificate.

While sharing is on, saving a checklist that unchecks a device tells the core
to revoke it at once: remote clients on that device are detached and their
queued writes dropped. Local clients and other checked devices are untouched.
Unplugging a device, or replacing it with a different device at the same path,
clears its checkbox; a replacement must be ticked again. The core's checked
list is authoritative, never the UI's idea of it.

Stopping sharing (`s` again, the Share button, or leaving the terminal) closes
the listener, disconnects every remote client and revokes both credentials. A
crashed terminal is noticed by the core when its owner connection closes, with
the same effect. Restarting is off and creates fresh credentials.

**Firewall and router exposure.** Binding an address only makes the process
listen on it. Whether another machine can reach it is decided by the host
firewall and by router/NAT configuration; opening a home router port exposes
the service beyond the LAN. Share only on a trusted network, share only the
devices needed, and stop sharing when finished.

**A write is not transactional.** Accepting a write means it may reach the
device; a USB unplug, an uncheck or a lost connection can leave a partially
written command. The terminal never retries or replays a write, and a write
whose answer was lost is reported as uncertain so the human can decide what to
do.

### Connecting to a shared host

The sidebar's `[ Connect ]` button (or `c`) opens a form for the host address,
port, CA certificate, host fingerprint and human pairing credential. Every value
is entered by hand or pasted and lives only in memory: it is not saved, not
logged and never sent to a serial device. The terminal verifies the host's
certificate against the entered CA and pinned fingerprint before sending the
credential; a mismatch is refused and the form stays open with the reason.

After a successful pair, the connected host's approved devices appear in the
sidebar under a `Remote <address:port>` heading (the local devices stay under
`Local`). Identical device paths on two hosts are separate rows with separate
sessions, and only devices the host checked are listed. Selecting or
highlighting a row never changes the shown stream; only `Enter`, a double-click
or an explicit open does. A device the host allows but has not opened asks for a
baud rate; later clients join at the active rate. Remote devices keep their own
scrollback, settings and local echo like local ones.

A remote write failure, disconnect or unplug is shown in the footer. It affects
only that device (or that host's sessions on disconnect); local devices and
other remote hosts keep working. The terminal never retries a remote write. An
MCP client and a human may use the same shared device concurrently, and the
core's one-AI-writer rule still applies across local and remote AI clients.
Reconnecting after quitting the terminal requires entering the pairing
information again.
