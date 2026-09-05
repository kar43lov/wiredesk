# Security Policy

## Threat model, in one paragraph

WireDesk carries keyboard, mouse, clipboard and shell traffic between two
machines that sit next to each other. **The channel is not authenticated and
not encrypted on any transport.** Whoever reaches the other end of the link
can inject input into the host's session, open a shell on it, and read or
write files through the clipboard. Over a serial cable that is an accepted
trade-off — physical access to the cable already means physical access to the
keyboard. Over Bluetooth LE it is not, which is why the GATT characteristics
are published as `EncryptionRequired` and Windows insists on pairing before
any data flows.

The full reasoning, including what changed in September 2026 and how to opt
out, is in [`README.md`](README.md#security-model) and
[`docs/known-limitations.md`](docs/known-limitations.md).

## What is deliberately not protected

- **The serial link.** No pairing code, no shared secret, no challenge. The
  handshake is a `Hello`/`HelloAck` exchange carrying a name and a protocol
  version.
- **Anything typed while capture is active.** On a Windows client this
  includes password fields: unlike macOS, which disables event taps over
  secure input, a low-level keyboard hook sees everything. Capture is
  explicit and marked with a banner, but there is no second safety net.
- **Files landing in the receive cache** (`~/Library/Caches/WireDesk/`,
  `%TEMP%\WireDesk\`). Names are sanitised against path traversal and
  reserved Windows names; contents are not inspected.

## Supported versions

This is a single-operator tool with no release cadence. Fixes land on `main`;
there are no maintained release branches, and no backports.

## Reporting a vulnerability

Open a [GitHub issue](https://github.com/kar43lov/wiredesk/issues) for
anything that is not itself sensitive — most findings here will be, given
that the design limitations above are public and documented.

For something that should not be public first, use GitHub's private
[security advisory](https://github.com/kar43lov/wiredesk/security/advisories/new)
form. Expect a hobby-project response time: this is not a funded project and
has no SLA.

Please do include the transport (serial / BLE), both operating systems and
their versions, and whether the issue needs physical access to reproduce —
that last one usually decides whether a finding is a bug or a documented
limitation.
