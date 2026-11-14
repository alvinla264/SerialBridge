# Security policy

## Reporting a vulnerability

Please report suspected security vulnerabilities privately through GitHub
Security advisories for
[alvinla264/SerialNexus](https://github.com/alvinla264/SerialNexus): open the
repository's **Security** tab and choose **Report a vulnerability**. Do not open
a public issue, discussion or pull request for a suspected vulnerability, and do
not attach live pairing credentials, tokens or device data.

A useful report includes:

- the affected component (core, terminal, MCP server, client library or LAN
  sharing) and the exact commit you tested;
- your Linux distribution and how the component was built or installed;
- steps to reproduce, the expected and actual behavior, and the impact you
  believe the issue has;
- any suggested fix or mitigation, if you have one.

## Scope and trust model

SerialNexus runs serial devices for one local OS user. The following are in
scope:

- the core's Unix socket, local client protocol and device access rules;
- the terminal's MCP gateway;
- the opt-in LAN sharing listener, TLS host verification, pairing credentials,
  role handling and checked-device allowlist;
- the human terminal and the MCP server;
- packaging and build configuration that affects the shipped binaries.

The same-user trust model is intentional, not a vulnerability: programs running
as the laptop user are trusted and can impersonate the terminal or client,
access local serial devices, and start or change LAN sharing. The private Unix
sockets exclude other local OS users, but they are not an isolation boundary
between programs owned by the same user. Unpaired remote clients and other OS
users are outside that trust and should not be able to access devices or change
sharing.

Linux is the only supported platform. There is no tagged release yet; please
report against `main` or the exact commit you tested and include its hash.

## What to expect

Reports are handled on a best-effort basis by the maintainer. This project does
not promise a response time, a fix deadline, a bug bounty or public credit.
Please give the maintainer time to investigate and fix the issue, and coordinate
public disclosure through the advisory rather than publishing details first.
