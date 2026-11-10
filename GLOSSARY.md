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
- **Sidebar**: the terminal's list of **devices** and its Settings button. At a
  usable width it sits on the left, beside the right pane; a narrow terminal
  shows it only while it has the **focus**.
- **Highlighted row**: the device row the sidebar's arrow keys and commands act
  on. Moving or clicking it never changes the **shown device**, so it never
  redirects typed input.
- **Shown device**: the device whose **stream** (or **scrollback**) occupies the
  right pane. It changes only when a device is opened/joined, `b` shows an
  attached device's scrollback, or the shown device disconnects and another
  session takes over. Row markers tell the two apart: `▸` highlighted and shown,
  `›` only highlighted, `▹` only shown.
- **Stream**: the terminal view that shows the **shown device**'s output and
  sends the keys typed in it to that device.
- **Focus**: which pane receives keys: the **sidebar** or the right pane
  (stream, scrollback or settings). `Ctrl+]` moves it; the header and the
  focused pane's styling show it.
- **Scrollback**: the rows a **stream** keeps after they scroll off the top of
  the live view, so a human can read earlier output. Bounded, in memory, per
  session, and kept only while the `scrollback` **setting** is on. The
  **scrollback view** shows those rows in place of the live screen; it is
  opened with `b` from the **sidebar** for the highlighted attached device,
  navigated with the arrow and page keys, and left with `Esc` for the live
  **stream**. Its keys never reach the device, and each device keeps its own
  kept rows and scrolled position across focus switches and resizes.
- **Settings view**: the terminal view opened from the sidebar's Settings
  button into the right pane, while the sidebar stays visible. `local_echo` and
  `scrollback` are edited there and saved with `Ctrl+S` or the Save button.
  Save and Cancel return to the **shown device**'s stream. While the draft has
  unsaved changes, opening a device is refused and the draft is kept.
- **Event**: an unsolicited notification from the core (device output, write
  echo, client joined/left, control granted/released/denied, device closed,
  core shutdown). Clients receive all events from their subscription onward.
- **Sharing session**: opt-in LAN access to a set of **checked devices**,
  started by a **local owner** and alive only while that owner's local terminal
  connection is. It has its own TLS certificate authority, host certificate and
  pairing credentials; stopping it, or losing the owner connection, closes the
  listener, disconnects every **remote client** and revokes the credentials.
  Off by default; there is no listener until one is started.
- **Local owner**: the local human terminal connection that started a sharing
  session. Only it may check, uncheck, inspect or stop sharing.
- **Checked device**: a detected device the owner added to the session's
  in-memory allowlist. The checked list is the whole of what remote clients may
  see or use; a checked device need not be open locally.
- **Device identity**: the concrete device node behind a path, captured as
  `(st_dev, st_ino, st_rdev)` and an `O_PATH` descriptor that pins the node,
  captured when the device is checked. It is revalidated at authorization and
  open time; a device that closes, is unplugged or is replaced loses its
  authorization (the entry expires and must be checked again) instead of
  letting a replacement inherit it.
- **Unchecked**: a device removed from the allowlist. Remote access to it is
  revoked immediately, remote work pending on it fails, and local clients and
  other checked devices keep working. Checking it again is explicit.
- **Pairing credential**: a random, high-entropy secret generated fresh for each
  sharing session, one for human clients and a distinct one for AI clients. Its
  role is decided by the core from which credential matched, never by the
  client. Returned once, only to the local owner.
- **Paired host**: the host information a remote client needs to authenticate
  the core: the session certificate authority, the host certificate's SHA-256
  fingerprint and the exact address to dial. A different certificate is never
  accepted silently.
- **Remote client**: a client connected over TLS to a sharing session, using a
  pairing credential. It sees only checked devices and cannot control sharing.
- **Remote frame**: one length-prefixed JSON message on a remote connection
  (4-byte big-endian length, then that many bytes), bounded to keep one peer
  from exhausting memory.
