# Security policy

crossdrop pairs devices, encrypts their traffic and writes files received
from the network, so security reports are very welcome.

## Reporting a vulnerability

Please **do not open a public issue**. Report it privately through GitHub:
**Security** tab → **Report a vulnerability**
(<https://github.com/root-lr/crossdrop/security/advisories/new>).

Include what you can: the version or commit, the platform, the steps or a
proof of concept, and what an attacker gains. You will get an answer as soon
as possible; once a fix is released, the advisory is published with credit to
you unless you prefer otherwise.

## Supported versions

crossdrop is young (v0.x): fixes go into the next release, built from `main`.
Older releases are not patched.

## Scope

In particular:

- pairing: the short code (SAS), the trust store (`trusted.toml`), and
  anything that lets an unpaired device act as a paired one;
- transport: TLS 1.3 over QUIC, certificate pinning, the device identity;
- permissions: a paired device using a service it was not allowed;
- files: paths escaping the download folder, overwriting existing files,
  resource exhaustion through offers;
- the local API: the daemon socket or named pipe, channel ids;
- interdesk: input injected without the `interdesk/input` permission.

Out of scope: attacks that need control of the user's account on one of the
paired computers, and denial of service by flooding the local network.
