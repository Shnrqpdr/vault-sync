# vault-sync

Self-hosted sync for your Obsidian vault, plus a copy of it you actually own: Git history, full-text search, and an MCP server so Claude can read your notes. It runs as one Docker Compose stack on your own server.

- **Sync:** the [Self-hosted LiveSync](https://github.com/vrtmrz/obsidian-livesync) plugin on each device, talking to a **CouchDB** on your server.
- **`vault-core` (Rust):** follows CouchDB's `_changes` feed, reassembles notes from LiveSync's chunks and keeps a **plain `.md` mirror** on disk. On top of the mirror:
  - **Git backups:** periodic commits, pushed to a private repository you choose.
  - **Search:** a full-text index with tags, frontmatter and backlinks.
  - **MCP server:** read-only tools for Claude (Claude Code, claude.ai, desktop and mobile apps), protected by a token or OAuth.

```
  laptop                 desktop                phone
  Obsidian+LiveSync      Obsidian+LiveSync      Obsidian+LiveSync
          \                    |                    /
           \                HTTPS                  /
            ▼                  ▼                  ▼
   ┌──────────────── your server (Docker Compose) ────────────────┐
   │  reverse proxy (bundled Caddy, an existing Traefik, or yours) │
   │     │ /                         │ /mcp, /oauth/*              │
   │     ▼                           ▼                             │
   │  CouchDB ◄──── _changes ──── vault-core ◄────── Claude        │
   │                                ├─ mirror: *.md                │
   │                                ├─ search index                │
   │                                └─ git ──push──► private repo  │
   └───────────────────────────────────────────────────────────────┘
```

## Security model

Read this before you deploy. It explains what vault-sync protects, and what it can't.

- **Your notes are stored unencrypted on your server.** That holds for CouchDB, the mirror and the Git backup. vault-core has to read them to build the mirror, search and MCP, so **anyone with root on the server can read your vault**. For the same reason, LiveSync's end-to-end encryption must stay off: with it on, vault-core sees only ciphertext (`/health` reports `encrypted_vault: true`).
- **Only two things face the internet**, both through the reverse proxy over HTTPS:
  - CouchDB, behind HTTP basic auth;
  - the MCP and OAuth endpoints, behind a bearer token.

  The search API (port 8080) has no authentication and is never routed by the proxy.
- **The CouchDB admin stays on the server.** The one exception is LiveSync's first-device setup, which needs it once (see [Connect Obsidian](#3-connect-obsidian)). Devices and vault-core log in as a non-admin *sync user* that can only access the vault database, and `couchdb-guard` keeps that restriction in place when a device rebuilds the database.
- **MCP access is read-only.** Claude Code uses a static token. The Claude apps use OAuth 2.1 (PKCE, rotating refresh tokens), and the authorization page asks for your `MCP_PASSWORD`. Tokens are stored only as hashes.
- **Brute force is contained.** Wrong passwords and client registrations are rate limited **per client IP**, so an attacker can only lock themselves out. Changing `MCP_PASSWORD` revokes every OAuth grant on the next start.
- **Backups go to a repository you control.** Use a deploy key that can write to that one repository and nothing else.
- **Continuous deployment can't open a shell.** The CI's SSH key may only run the deploy script (an `authorized_keys` forced command). Images are built on CI, never on the server, and only `v*` tags can deploy.

Found a vulnerability? See [SECURITY.md](SECURITY.md).

## Quick start

You need a Linux server with Docker and the Compose plugin, a user in the `docker` group, and a domain whose DNS points to the server. Also decide how HTTPS reaches the stack:

| `--proxy` | When to use it |
|---|---|
| `caddy` | Nothing else uses ports 80/443. The bundled Caddy gets certificates from Let's Encrypt. |
| `traefik` | A Traefik with the Docker provider already owns 80/443. vault-sync joins it through labels, without touching its config. |
| `external` | You run another proxy (nginx, etc.) and route to vault-sync yourself. |

### 1. Prepare the server

From a clone of this repository (ideally your fork):

```bash
scp deploy/bootstrap.sh deploy/remote-deploy.sh user@server:
ssh user@server ./bootstrap.sh --domain vault.example.com --proxy caddy --acme-email you@example.com
```

With an existing Traefik, use `--proxy traefik` plus `--traefik-network`, `--traefik-entrypoint` and `--traefik-certresolver`, matching that Traefik's setup.

`bootstrap.sh` sets up `~/apps/vault-sync` on the server:

- **`shared/.env`:** settings, with random passwords and tokens (permissions `600`, never printed);
- **a deploy key** for the Git backup;
- **`bin/remote-deploy.sh`.**

It is safe to re-run. It never overwrites existing settings or keys, and only adds what is missing.

### 2. Deploy

Tag a commit that is on `main`, then ship it. The image is built on your machine; the server only receives it:

```bash
git tag v1.0.0 && git push origin v1.0.0
DEPLOY_TARGET=user@server deploy/deploy.sh v1.0.0
ssh user@server apps/vault-sync/bin/remote-deploy.sh status
```

Once [continuous deployment](#continuous-deployment) is set up, publishing a GitHub release does this for you.

### 3. Connect Obsidian

The passwords live on the server: `ssh user@server cat apps/vault-sync/shared/.env`. Back up your vault folder before the first sync.

**First device (the one with your complete vault).** LiveSync's first-device setup deletes and recreates the database on the server, and only the CouchDB admin may do that. So this one setup uses the admin, and nothing else ever does:

1. In Obsidian, install and enable **Self-hosted LiveSync**. Open its settings and run the setup wizard (**Rerun Wizard** if it already ran).
2. Choose **I am setting this up for the first time**, then manual configuration with CouchDB:
   - **URI:** `https://<VAULT_DOMAIN>`
   - **Username:** `COUCH_USER` (the admin, for this setup only)
   - **Password:** `COUCH_PASSWORD`
   - **Database:** `COUCH_DB`

   If it offers to check or fix the database configuration, you can skip that step, because `couchdb-init` already applied those settings.
3. On the **End-to-End Encryption** step, turn encryption **off**, leave the passphrase empty, and leave property obfuscation off (see [Security model](#security-model)).
4. Choose **Restart and Initialise Server**. After the restart, confirm the rebuild. The plugin uploads the vault, and its log ends with "Rebuild everything operation completed".
5. In the plugin's settings, change the username and password to `COUCH_SYNC_USER` and `COUCH_SYNC_PASSWORD`. Don't run the wizard again, or it would rebuild again.

The rebuild leaves the recreated database admin-only. `couchdb-guard` notices this within `COUCH_GUARD_INTERVAL` seconds, restricts the database to the sync user again, and vault-core resumes on its own.

**Other devices.** Choose **I am adding a device to an existing synchronisation setup** and use the sync user. Start from an empty vault so the device downloads everything:

1. On the CouchDB step, the optional **Check server requirements** button reports "Access forbidden" with the sync user. That diagnostic reads the server settings, which only the admin may read, so the error is expected. Skip it and choose **Connect to existing database and continue**.
2. Keep end-to-end encryption off, as on the first device, and choose **Restart and Fetch Data**.
3. After the restart, on **Reset Synchronisation on This Device**, choose **This Vault is empty**. You can also copy a **Setup URI** from the first device after step 5, so it carries the sync user rather than the admin.

Later, use **Rebuild everything** only to repair a broken server copy. It needs the admin credentials again, and every other device has to reset its sync afterwards.

### 4. Connect Claude

- **Claude Code:**

  ```bash
  claude mcp add --transport http vault https://<VAULT_DOMAIN>/mcp \
    --header "Authorization: Bearer <MCP_TOKEN>"
  ```

- **claude.ai, desktop and mobile apps:** add a custom connector with the URL `https://<VAULT_DOMAIN>/mcp`. Claude registers itself and opens vault-sync's authorization page, which asks for `MCP_PASSWORD`. Connectors added on the web are also available in the apps.

The tools are all read-only:

| Tool | What it does |
|---|---|
| `search_notes` | Full-text search, filterable by tag and folder. |
| `read_note` | A whole note, with its links and backlinks. |
| `list_notes` | Recent notes, or those under a tag or folder. |
| `list_tags` | Every tag, with how many notes use it. |

## Git backups

vault-core commits the mirror every `BACKUP_INTERVAL_SECS` (10 minutes by default) when something changed. The local repository lives in vault-core's state volume, outside the mirror.

To push the history somewhere, create a **private** repository for your notes. That is a separate repository, never your vault-sync fork. Then:

1. Add the public key that `bootstrap.sh` printed (`shared/secrets/backup_deploy_key.pub`) to that repository as a **deploy key with write access**.
2. Set `BACKUP_REMOTE=git@github.com:<you>/<notes-repo>.git` in `shared/.env` and redeploy.

You can use an HTTPS URL with a fine-grained token (Contents: read and write) instead. The token is masked in logs and in `/health`.

A failed push (network, credentials) keeps the commits local and is retried on the next cycle. If the state volume is ever lost, the new repository adopts the remote history before its first commit, so nothing is rewritten.

## Reverse proxy

| Path | Goes to |
|---|---|
| `/mcp`, `/mcp/*`, `/oauth/*`, `/.well-known/oauth-*` | `vault-core:8081` (authenticated) |
| everything else | `couchdb:5984` |

- **Caddy (`VAULT_PROXY=caddy`):** the bundled `Caddyfile` already does this and gets certificates automatically.
- **Traefik (`VAULT_PROXY=traefik`):** `docker-compose.traefik.yml` adds the labels and joins `TRAEFIK_NETWORK`. In that shared network, the search API listens only inside its container.
- **Your own proxy (`VAULT_PROXY=external`):** connect it to the `vault-sync_edge` network, route the paths above, and keep the original `Host` header. Do not add CORS headers, because CouchDB handles them. Never route to port 8080.

## Continuous deployment

The `CD` workflow deploys every release published from `main`. To redeploy a tag by hand, use **Actions → CD → Run workflow**. Forks without the setup below just skip the deploy.

1. Create an SSH key for the CI and authorize it on the server. The key can only run the deploy script:

   ```bash
   ssh-keygen -t ed25519 -N '' -C vault-sync-cd -f cd_key
   ssh user@server ./bootstrap.sh --cd-key "$(cat cd_key.pub)"
   ```

2. In your fork, go to **Settings → Environments** and create `production`. Under *Deployment branches and tags*, allow only tags matching `v*`. Then add:

   | Kind | Name | Value |
   |---|---|---|
   | Secret | `DEPLOY_SSH_KEY` | contents of `cd_key` (then delete the local file) |
   | Secret | `DEPLOY_KNOWN_HOSTS` | the server's host key, e.g. `ssh-keyscan -t ed25519 <server>` (compare it with what `bootstrap.sh` printed) |
   | Variable | `DEPLOY_TARGET` | `user@server` |

3. Publish a release from `main`.

Each deploy goes through these steps:

1. The runner builds the vault-core image from the tag.
2. Code (`git archive`) and image (`docker save`) travel in one SSH stream to `remote-deploy.sh`.
3. The script unpacks the release into `releases/<tag>`, starts it with `docker compose up --no-build`, and waits for CouchDB and vault-core to report healthy.
4. Only then does it point `current` at the new release. If the release doesn't become healthy, it rolls back to the previous one. It keeps the last five releases.

To change settings, edit `shared/.env` on the server and redeploy the current tag.

## Configuration

All settings live in `.env` (`shared/.env` on a server set up with `bootstrap.sh`). [`.env.example`](.env.example) documents them.

| Variable | Default | Description |
|---|---|---|
| `VAULT_DOMAIN` | — | Public domain for sync and MCP |
| `VAULT_PROXY` | `caddy` | `caddy`, `traefik` or `external` (used by the deploy script) |
| `COUCH_USER` / `COUCH_PASSWORD` | — | CouchDB admin (stays on the server) |
| `COUCH_SYNC_USER` / `COUCH_SYNC_PASSWORD` | — | Non-admin user for devices and vault-core |
| `COUCH_GUARD_INTERVAL` | `30` | Seconds between `couchdb-guard` checks of the database permissions |
| `COUCH_DB` | `obsidian` | Database used by LiveSync |
| `BACKUP_INTERVAL_SECS` | `600` | Seconds between Git snapshots; `0` disables them |
| `BACKUP_REMOTE` | — | Remote for the notes' history; empty keeps it local |
| `BACKUP_BRANCH` | `main` | Branch pushed to the remote |
| `MCP_TOKEN` | — | Static bearer token (Claude Code); 16+ characters |
| `MCP_PASSWORD` | — | OAuth authorization password (Claude apps); 16+ characters |
| `MCP_PUBLIC_URL` | `https://<VAULT_DOMAIN>` | Public MCP URL, used in OAuth metadata |
| `ACME_EMAIL` | — | Let's Encrypt e-mail (Caddy) |
| `TRAEFIK_NETWORK` / `TRAEFIK_ENTRYPOINT` / `TRAEFIK_CERTRESOLVER` | — | Existing Traefik (with `VAULT_PROXY=traefik`) |
| `RUST_LOG` | `vault_core=info` | Log level |

The MCP server only starts when `MCP_TOKEN` or `MCP_PASSWORD` is set.

## How vault-core works

1. It long-polls `_changes?include_docs=true` from its last checkpoint (in SQLite).
2. It classifies each document: text note, binary, deleted, chunk, internal, or encrypted.
3. It fetches missing chunks through `_all_docs` and assembles the file. Text chunks are concatenated; binary files are base64 per chunk.
4. It writes atomically to the mirror, keeping the note's original modification time, and only then advances the checkpoint. Notes that arrive before their chunks stay pending and are retried.
5. If the database disappears or turns admin-only (a device's "Rebuild everything"), vault-core keeps retrying with backoff. Once `couchdb-guard` restores access, CouchDB replays the changes of the new database from the start.
6. Every changed path goes to the indexer. The search index is in memory, rebuilt from the mirror on each start, so there is no second checkpoint that could drift.
7. The Git snapshot takes a lock shared with the syncer, so it never captures half a batch.

Hidden files (`.obsidian/`) and LiveSync's internal documents are not mirrored.

### Search

Search ignores case and accents, uses Portuguese stemming (`nao` finds "não"), and ANDs the terms together. It supports `"exact phrase"`, `-exclude` and `title:word`.

Links resolve the way Obsidian resolves them:

- `[[Name]]` and `[[folder/Name#heading|alias]]`;
- embeds such as `![[file.png]]`;
- relative Markdown links.

Tags come from the frontmatter and from inline `#tags` outside code. Tag and folder filters include nested levels. The internal API serves the same data:

| Route | Returns |
|---|---|
| `GET /search?q=&tag=&folder=&limit=20` | Best matches, with highlighted snippets |
| `GET /notes?tag=&folder=&limit=50` | Note metadata, most recent first |
| `GET /notes/<path or name>` | Content, frontmatter, links and backlinks |
| `GET /tags` | Tags and how many notes use each |
| `GET /health` | Sync, index and backup status |

It is reachable only from inside the stack, for example `docker compose exec couchdb curl -s 'http://vault-core:8080/search?q=meeting'`.

## Development

```bash
cd vault-core
cargo test                 # the backup tests need `git` installed
cargo clippy --all-targets -- -D warnings
COUCH_USER=... COUCH_PASSWORD=... cargo run
```

To run the whole stack locally: `cp .env.example .env && docker compose up -d --build`. CouchDB has no published port; uncomment `ports` in `docker-compose.yml` to reach it.

Contributions are welcome. Please open an issue first for larger changes.

## Roadmap

- [x] CouchDB + LiveSync
- [x] `.md` mirror with checkpoint
- [x] Git snapshots pushed to a private repository
- [x] Full-text index, tags, frontmatter, backlinks and search API
- [x] MCP server with OAuth
- [x] Release-based deployment with health checks and rollback
- [ ] Writing: an inbox (HTTP/Telegram) that creates notes in LiveSync's format
- [ ] Reading E2EE vaults with the passphrase

## Credits

The CouchDB configuration is adapted from [vrtmrz/obsidian-livesync](https://github.com/vrtmrz/obsidian-livesync) (MIT). The document format was checked against `livesync-commonlib`.

## License

[MIT](LICENSE)
