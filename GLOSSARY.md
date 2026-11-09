# Glossary

Terms used by SerialNexus. The client tasks (MCP server, terminal) should use
these words in their tool names and user-facing text.

- **Core** (`serial-nexus-core`): the one process per local user that owns
  serial devices and serves clients over a Unix socket. It starts automatically
  when the first client connects and exits after a grace period with no
  clients.
- **Client**: anything connected to the core. An **AI client** is the MCP
  server; a **human client** is the terminal.
- **Device**: one serial port the core has open (e.g. `/dev/ttyUSB0`). The core
  identifies an open device by a numeric **device id**.
- **Open**: the first client to use a device chooses its **baud rate**; later
  clients **join** the open device instead of opening it again.
- **Reader**: an AI client attached to a device without write control. It can
  read history and live output, but not write.
- **Controller**: the one AI client that holds **write control** of a device
  and may write to it.
- **Write control**: the exclusive right of one AI client per device to write.
  Claimed explicitly with `open(..., request_control = true)`, released on
  disconnect, and expired after the **control idle timeout** with no request
  from the holder. A human client writes without holding control.
- **Control holder**: the client currently holding write control, reported in
  `ControlState` and control events.
- **History**: the bounded, in-memory record of **lines** per open device,
  labelled by **source** (device, AI, human). Discarded when the device closes.
- **Line**: bytes ending in `\n` (a trailing `\r` is trimmed). A line with no
  newline yet is **partial** and is marked as such.
- **Partial**: a line-like value without a terminating newline (a prompt, or a
  write that did not end in one). Flagged `partial` and ordered by when it
  started, so an old prompt does not appear newer than later writes.
- **Grace period**: the 30-second default during which the core stays alive
  after its last client disconnects.
- **Close**: detaching the last client. Accepted writes are drained first, then
  the serial handle is released and `DeviceClosed` is announced.
- **Truncated**: a read or waited-write result trimmed to the IPC frame budget.
  The newest lines are kept; `truncated` says older ones were left out.
- **MCP session**: one run of `serial-nexus-mcp`, launched by the AI host and
  spoken to over stdio. It may be attached to several devices at once; each
  device keeps its own baud, history and control.
- **Tool call**: one MCP request from the host to `serial-nexus-mcp`
  (`list_devices`, `connect`, `disconnect`, `read`, `send`). It returns one
  bounded result; there is no continuous stream.
- **Settings**: the human terminal's own preferences, stored per user in
  `terminal.json` (`local_echo` and `scrollback`, both on by default). Not core
  configuration.
- **Local echo**: showing the keys a human types in the **stream** in the
  terminal's own view. On by default; the `local_echo` **setting** turns it off
  for a device that echoes what it receives.
- **Sidebar**: the terminal's list of **devices** and its Settings button,
  shown while a device is chosen. It hides in the **stream**.
- **Stream**: the terminal view that shows a device's output and sends the
  keys typed in it to that device.
- **Scrollback**: the rows a **stream** keeps after they scroll off the top of
  the live view, so a human can read earlier output. Bounded, in memory, per
  session, and kept only while the `scrollback` **setting** is on. The
  **scrollback view** shows those rows in place of the live screen; it is
  opened with `b` from the **sidebar**, navigated with the arrow and page
  keys, and left with `Esc` for the live **stream**. Its keys never reach the
  device.
- **Settings view**: the terminal view opened from the sidebar's Settings
  button, where `local_echo` and `scrollback` are edited and saved with
  `Ctrl+S`.
- **Event**: an unsolicited notification from the core (device output, write
  echo, client joined/left, control granted/released/denied, device closed,
  core shutdown). Clients receive all events from their subscription onward.
