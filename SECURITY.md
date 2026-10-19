# Security policy

Braid stores other people's code, so a bug in it can be a security bug: a
checkout that writes outside the working copy, a crafted object that crashes or
exhausts the reader, a merge that silently drops a change, or a token that
grants more than it should.

## Reporting a vulnerability

Please report it privately through GitHub:
[**Report a vulnerability**](https://github.com/KikoCis/elffuss-braid/security/advisories/new).
Do not open a public issue for it.

Include what you can: the version or commit, the steps or a minimal repository
that reproduces it, and what an attacker gains. We will confirm that we
received it, keep you informed while we fix it, and credit you in the advisory
unless you prefer otherwise.

## Supported versions

Fixes go to the latest release and the `main` branch.

## Scope

In scope:

- Path handling when writing a working copy: traversal, symlinks, names that
  collide on case-insensitive or normalising file systems, and aliases of
  `.braid` or `.git`.
- Decoding objects, records, indexes and anything received over the network.
- Importing from or exporting to git.
- Authentication and authorisation in the server.
- Any way to make braid lose or corrupt data that it acknowledged as stored.
