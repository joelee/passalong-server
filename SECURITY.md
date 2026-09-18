# Security policy

## Supported versions

Security fixes go into the newest tagged version. Older versions are not
patched; build the newest tag.

## Reporting a vulnerability

Report vulnerabilities privately through GitHub: open the repository's
**Security** tab and choose **Report a vulnerability**. Please do not open a
public issue or pull request for a security problem.

Include the passalong-server version, how it is deployed (Docker or
systemd, built-in TLS or a reverse proxy), and the steps or input that show
the problem.

## Scope

Examples of what counts as a vulnerability in passalong-server:

- a request reading, writing, listing, or learning the existence of
  anything in a workspace other than its API key's;
- an expired, revoked, or read-only key being accepted for an operation it
  must not perform;
- an item id, workspace id, or upload token resolving to a path outside the
  workspace's directory;
- an API key, or anything it can be recovered from, reaching logs, error
  messages, or the control database unhashed;
- item content, names, or previews reaching logs;
- unauthenticated requests exhausting memory or disk, for example through
  body size or abandoned uploads, or a body being read before the key is
  checked;
- a response carrying a field taken from an item's `meta`;
- a write to an encrypted workspace accepted under a key id that is not the
  workspace's current one.

`cargo deny` checks dependencies against the RustSec advisory database on
every CI run.
