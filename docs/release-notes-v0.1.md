# SerialNexus v0.1.0 release notes (draft)

**Status: draft for the planned first release.** No tag or GitHub release has
been created, and no release date is set. This document describes the intended
v0.1.0 contents; it is not a stability promise.

## Highlights

- **One local core owns the serial devices.** The core enforces one AI writer
  per device while allowing multiple readers and human writes. Devices are
  discovered under `/dev/serial/by-id` and `/sys/class/tty`; the first client to
  open a device chooses its baud rate.
- **MCP for AI hosts.** `serial-nexus-mcp` exposes `list_devices`, `connect`,
  `disconnect`, `read` and `send` over MCP stdio. Every call, local or to a
  paired remote host, goes through the foreground terminal's gateway; MCP never
  starts the core or opens a network connection itself.
- **Interactive human terminal.** `serial-nexus-terminal` shows a device sidebar
  beside the live stream, with local echo, bounded scrollback, settings, device
  sharing controls and a remote Connect form.
- **Opt-in LAN sharing.** Sharing is off by default and has no listener until the
  terminal starts a session for one concrete IPv4 or IPv6 address. Remote
  clients must verify the host certificate and pin its fingerprint, present a
  pairing credential, and use only the devices the owner checked. Stopping
  sharing or closing the owning terminal revokes the session.
- **Client library and protocol.** `serial-nexus-client` and
  `serial-nexus-protocol` provide the local client API and wire types used by
  the terminal, MCP server and trusted same-user programs.

## Installation

Linux is required. macOS and Windows are not supported.

With Nix flakes enabled:

```sh
nix run github:alvinla264/SerialNexus              # human terminal
nix run github:alvinla264/SerialNexus#serial-nexus-mcp
nix profile install github:alvinla264/SerialNexus  # all three binaries
```

With Rust 1.85 or newer:

```sh
cargo build --release
./target/release/serial-nexus-terminal
```

Start the terminal before using MCP; it hosts the gateway and starts the core
automatically. See [Installation and device behavior](getting-started.md) for
serial permissions and all installation options, and
[Clients and interfaces](clients.md) for MCP and LAN sharing setup.

## Verification status

Checked for this release:

- the workspace's automated tests on Linux, using PTY-based simulated serial
  devices and loopback TLS for LAN sharing;
- the x86_64-linux Nix package, built and checked locally;
- local serial I/O with an STM32 Nucleo board through the terminal.

Not verified:

- the sharing path between two physical computers, or a real LAN peer. Sharing
  tests use loopback TLS on one machine;
- aarch64-linux at runtime. The flake declares `x86_64-linux` and
  `aarch64-linux`; a build or evaluation for the other architecture does not
  prove that it runs there;
- any platform other than Linux.

## Known limitations

- **Linux only**, including Linux serial discovery and the Unix-socket IPC.
- **Same-user trust, not a sandbox.** Programs running as the laptop user can
  impersonate the terminal or client, access local serial devices, and start or
  change LAN sharing. Other OS users are excluded by the private sockets; a
  same-UID program is not.
- **MCP is text and polling.** It exposes tools only, with no notifications,
  resources or prompts, no continuous stream, and bounded reads and waits.
  Binary protocols and terminal emulation are not provided.
- **Writes are not transactional.** A write may partially reach a device on
  unplug, uncheck or connection loss. Nothing is retried or replayed, and a
  write whose answer was lost is reported as uncertain.
- **LAN reachability is external.** Firewalls and router/NAT configuration decide
  who can reach a sharing listener; forwarding a port can expose it beyond the
  LAN.
- **Pre-1.0.** Interfaces and behavior may change before a stable release.

## Reporting security issues

Report suspected vulnerabilities privately through GitHub Security advisories,
as described in [SECURITY.md](../SECURITY.md). Do not open a public issue for a
security problem.

## License

SerialNexus is licensed under the
[Apache License, Version 2.0](../LICENSE).
