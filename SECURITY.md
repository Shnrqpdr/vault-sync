# Security policy

vault-sync holds people's private notes, so security reports are welcome and taken seriously.

## Reporting a vulnerability

**Please do not open a public issue.** Report it privately through GitHub: **Security → Report a vulnerability** on this repository. Include:

- what an attacker could do, and under which conditions;
- steps or a proof of concept to reproduce it;
- the version (release tag or commit) you tested.

You should get an acknowledgement within a week. Once a fix is released, the advisory will credit you unless you prefer otherwise.

## Scope

In scope, among other things:

- the code in this repository: vault-core, the OAuth and MCP endpoints, the compose files and the deploy scripts;
- ways to read or change notes without the right credentials;
- authentication bypasses, token leaks, ways around the rate limits, and escapes from the CD key's forced command.

Out of scope:

- issues that require root on the server (see the [security model](README.md#security-model): notes are stored unencrypted there by design);
- vulnerabilities in CouchDB, Self-hosted LiveSync, Obsidian, Caddy or Traefik themselves. Report those upstream.

## Supported versions

Only the latest release receives fixes.
