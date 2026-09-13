# Connect

DevManager remains a local-first product. Connect is the optional hosted product and realtime transport. Connect never becomes the execution authority and cannot decrypt raw task content by default.

## Local host and remote clients

The durable `devmanager-host` process owns runtime truth. Direct browser/PWA access to the host listener and optional Connect relay clients are projections of that host. Closing a remote client does not stop host-owned tasks, terminals, providers, or browsers.

Operational detail for the embedded mobile web surface, HTTPS proxy headers, notifications, and pairing cookie scope lives in [REMOTE_MOBILE_WEB.md](REMOTE_MOBILE_WEB.md).

## Pairing versus task invitations

Persistent paired owner devices and task invitations are different mechanisms:

- **Device pairing** uses the long-lived pairing/device records in `remote.json`. Application upgrades must not rotate that pairing code or invalidate a previously paired device. Rotation and revocation are explicit user actions.
- **Task invitations** are scoped grants with nickname, expiry, individual revocation, and separate view versus collaborate/write capabilities. They never reuse or reveal the long-lived pairing code, do not inherit full owner authority, and do not replace personal pairing.

## Privacy boundary

By default Connect must not receive raw prompts, responses, terminal output, browser content, recordings, file bodies, or full diffs. Provider and Connect secrets remain host-side in the OS credential vault. Viewer/collaborator/owner capabilities are enforced by the host, not trusted from UI state.

Organization accounts and deliberately published organization prompts may live on Connect; personal work remains local-only unless deliberately enrolled. Personal prompt libraries stay host-authoritative even when a paired client reads them over an end-to-end encrypted channel.
