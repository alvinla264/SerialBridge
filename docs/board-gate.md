# Board gate: real STM32 + real LAN seam (no flashing)

The coordinator runs this gate on the machine with the existing STM32 test
firmware attached. It exercises the exact PR head over the real serial device
and one real LAN address, and never flashes the board or changes firmware.

## What it proves

1. The core starts beside the normal one on a private socket.
2. A local terminal client opens the real board (local behaviour intact).
3. Sharing starts on one concrete LAN IPv4/IPv6 address; the owner gets a
   session, host pin and fresh human/AI credentials.
4. A remote human client pairs over TLS, verifies the host CA and pinned leaf,
   authenticates with the human credential and derives its role from it.
5. Remote discovery lists the checked board, and the remote client joins it.
6. Remote history reads real firmware output; an optional probe can be written
   and an optional firmware echo confirmed.
7. Unchecking revokes the remote read immediately, and local serial access
   still works after sharing stops.

It does not test firmware behaviour beyond what the existing firmware prints or
echoes, the terminal sharing UI (a follow-on task), remote MCP (a follow-on
task), or NAT/firewalls.

## Prerequisites

- The board is connected and its existing test firmware is running; do not
  flash it. `SER13_BOARD_SERIAL` names its stable path (prefer
  `/dev/serial/by-id/...`).
- The coordinator's user can open that device.
- A concrete LAN address of this machine, for example `192.168.1.20:0`
  (port 0 lets the kernel pick an unused port and the test reports it). Do not
  use `0.0.0.0` or `::`.
- The machine's firewall allows inbound TLS to the chosen port from the remote
  peer; for a pure local seam check, pairing from the same machine over the LAN
  IP already exercises the interface and TLS.
- Port 22/other services are irrelevant; the core binds only the address given.

## Run

Use the exact PR head, never a local edit:

```sh
git fetch origin pull/9/head:pr-9
git checkout pr-9
git rev-parse HEAD                      # record this as the gated SHA

nix shell nixpkgs#cargo nixpkgs#rustc -c \
  cargo build --release -p serial-nexus-core -p serial-nexus-client
```

Read-only gate (sends nothing to the board):

```sh
nix shell nixpkgs#cargo nixpkgs#rustc -c sh -c '
  SER13_BOARD_SERIAL=/dev/serial/by-id/usb-STMicroelectronics_... \
  SER13_BOARD_BIND=192.168.1.20:0 \
  SER13_BOARD_BAUD=115200 \
  cargo test -p serial-nexus-core --test board_gate -- --ignored --nocapture
'
```

Optional probe and firmware echo (only if the existing firmware echoes it):

```sh
  SER13_BOARD_PROBE=$'\r' \
  SER13_BOARD_EXPECT_ECHO=OK \
  ...
```

Expected lines: `1/7 local open`, `2/7 sharing: bind=...`, `3/7 remote pair`,
`4/7 remote list`, `5/7 remote join`, `6/7 remote read`, `7/7 uncheck revoked
remote access`, then `sharing stopped; local history ... lines`, and finally
`test result: ok. 1 passed`.

## If it fails

- `local open` fails: check permissions/cabling; this is not a sharing failure.
- `start sharing` fails: the address is not local or the port is taken; try
  `:0` or another concrete LAN IP. A bind failure must leave sharing off (the
  test would stop there).
- `remote pair` fails: TLS/host-auth or credential failure; record the error.
  Do not weaken certificate checks.
- `remote list`/`join` fails: the board is not checked or the remote credential
  was refused.
- `remote read` is empty: the firmware printed nothing since boot; power-cycle
  the board (never flash), wait for boot output, rerun.
- `probe`/echo fails: the firmware did not echo the probe; report the bytes
  written and the last lines read.
- `uncheck` does not revoke: stop; this is a security failure.

## After the gate

Record: gated SHA, LAN address/port used, board serial path, whether a probe
was sent and its echo, and the test output. Then the coordinator performs the
Conventional Commit squash merge. Do not merge before this gate passes.
