#!/usr/bin/env bash
# Prepares a server for vault-sync. Run it on the server, next to remote-deploy.sh:
#
#   scp deploy/bootstrap.sh deploy/remote-deploy.sh user@host:
#   ssh user@host ./bootstrap.sh --domain vault.example.com --proxy caddy --acme-email you@example.com
#
# Safe to re-run: it never overwrites settings or keys. Missing .env variables
# are added (secrets are generated randomly); existing ones are kept.
#
# Options:
#   --domain D                public domain for Obsidian sync and MCP (first run)
#   --proxy P                 caddy (bundled, owns ports 80/443), traefik (an
#                             existing Traefik) or external (your own proxy);
#                             default: caddy
#   --acme-email E            Let's Encrypt e-mail (caddy)
#   --traefik-network N       Docker network Traefik watches (traefik)
#   --traefik-entrypoint E    Traefik's HTTPS entrypoint (traefik)
#   --traefik-certresolver R  Traefik's certificate resolver (traefik)
#   --backup-remote URL       private repo for the notes' Git history
#   --cd-key "ssh-ed25519 …"  authorize the CD's public key, restricted to
#                             running remote-deploy.sh
#   --home DIR                install dir (default: ~/apps/vault-sync)
set -euo pipefail

APP="$HOME/apps/vault-sync"
domain="" proxy="" acme_email="" backup_remote="" cd_key=""
traefik_network="" traefik_entrypoint="" traefik_certresolver=""

log() { printf '[bootstrap] %s\n' "$*" >&2; }
die() {
    log "error: $*"
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
    --domain) domain="$2" ;;
    --proxy) proxy="$2" ;;
    --acme-email) acme_email="$2" ;;
    --traefik-network) traefik_network="$2" ;;
    --traefik-entrypoint) traefik_entrypoint="$2" ;;
    --traefik-certresolver) traefik_certresolver="$2" ;;
    --backup-remote) backup_remote="$2" ;;
    --cd-key) cd_key="$2" ;;
    --home) APP="$2" ;;
    -h | --help)
        sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'
        exit 0
        ;;
    *) die "unknown option $1 (see --help)" ;;
    esac
    shift 2 || die "missing value for $1"
done

for cmd in docker openssl ssh-keygen flock tar gzip; do
    command -v "$cmd" >/dev/null || die "$cmd is not installed"
done
docker compose version >/dev/null 2>&1 || die "the Docker Compose plugin is not installed"
docker info >/dev/null 2>&1 || die "cannot talk to Docker (is this user in the docker group?)"

here="$(cd "$(dirname "$0")" && pwd)"
[[ -f "$here/remote-deploy.sh" ]] || die "remote-deploy.sh must sit next to bootstrap.sh"

umask 077
mkdir -p "$APP/shared/secrets" "$APP/releases" "$APP/bin"
install -m 755 "$here/remote-deploy.sh" "$APP/bin/remote-deploy.sh"
log "deploy script installed at $APP/bin/remote-deploy.sh"

# --- .env -------------------------------------------------------------------
env_file="$APP/shared/.env"
if [[ ! -f "$env_file" ]]; then
    printf '# vault-sync settings. Created by bootstrap.sh on %s.\n' "$(date -u +%F)" >"$env_file"
fi
chmod 600 "$env_file"

current() { sed -n "s/^$1=//p" "$env_file" | tail -n 1; }
added=()
ensure() {
    # ensure KEY VALUE [asked]: adds KEY if missing. With "asked" (the value
    # came from an option), warns when the existing value differs.
    local key="$1" value="$2" existing
    if grep -q "^$key=" "$env_file"; then
        existing="$(current "$key")"
        if [[ "${3:-}" == asked && -n "$value" && "$value" != "$existing" ]]; then
            log "kept $key=$existing (edit $env_file to change it)"
        fi
        return
    fi
    printf '%s=%s\n' "$key" "$value" >>"$env_file"
    added+=("$key")
}
secret() { openssl rand -hex "$1"; }

[[ -n "$domain" || -n "$(current VAULT_DOMAIN)" ]] || die "--domain is required on the first run"
proxy="${proxy:-$(current VAULT_PROXY)}"
proxy="${proxy:-caddy}"
case "$proxy" in caddy | traefik | external) ;; *) die "--proxy must be caddy, traefik or external" ;; esac

ensure VAULT_DOMAIN "$domain" asked
ensure VAULT_PROXY "$proxy" asked
ensure COUCH_USER admin
ensure COUCH_PASSWORD "$(secret 24)"
ensure COUCH_SYNC_USER obsidian
ensure COUCH_SYNC_PASSWORD "$(secret 24)"
ensure COUCH_DB obsidian
ensure RUST_LOG vault_core=info
ensure BACKUP_INTERVAL_SECS 600
ensure BACKUP_REMOTE "$backup_remote" asked
ensure BACKUP_BRANCH main
ensure MCP_TOKEN "$(secret 32)"
ensure MCP_PASSWORD "$(secret 12)"

case "$proxy" in
caddy)
    [[ -n "$acme_email" || -n "$(current ACME_EMAIL)" ]] || die "--acme-email is required with --proxy caddy"
    ensure ACME_EMAIL "$acme_email" asked
    if ss -ltnH 2>/dev/null | awk '{print $4}' | grep -qE '(^|:)(80|443)$'; then
        log "warning: something already listens on port 80 or 443; the bundled Caddy needs both"
        log "         (an existing Traefik? use --proxy traefik)"
    fi
    ;;
traefik)
    for spec in "TRAEFIK_NETWORK:--traefik-network:$traefik_network" \
        "TRAEFIK_ENTRYPOINT:--traefik-entrypoint:$traefik_entrypoint" \
        "TRAEFIK_CERTRESOLVER:--traefik-certresolver:$traefik_certresolver"; do
        IFS=: read -r key flag value <<<"$spec"
        [[ -n "$value" || -n "$(current "$key")" ]] || die "$flag is required with --proxy traefik"
        ensure "$key" "$value" asked
    done
    docker network inspect "$(current TRAEFIK_NETWORK)" >/dev/null 2>&1 ||
        log "warning: Docker network '$(current TRAEFIK_NETWORK)' does not exist"
    ;;
external)
    log "point your proxy at http://couchdb:5984 and, for MCP, /mcp /oauth/ /.well-known/oauth- at"
    log "http://vault-core:8081, joining the vault-sync_edge network (see the README)"
    ;;
esac
if [[ ${#added[@]} -gt 0 ]]; then
    log "added to .env: ${added[*]}"
else
    log ".env already complete"
fi

# --- Git backup deploy key ----------------------------------------------------
key="$APP/shared/secrets/backup_deploy_key"
if [[ ! -f "$key" ]]; then
    ssh-keygen -q -t ed25519 -N "" -C "vault-sync backup ($(current VAULT_DOMAIN))" -f "$key"
    log "backup deploy key created"
fi
# The container runs as uid 10001. Docker does the chown, so no sudo is needed.
docker run --rm -v "$APP/shared/secrets:/s" alpine:3 \
    sh -c 'chown 10001:10001 /s/backup_deploy_key && chmod 600 /s/backup_deploy_key'
chmod 755 "$APP/shared/secrets"

# --- CD key -------------------------------------------------------------------
if [[ -n "$cd_key" ]]; then
    [[ "$cd_key" =~ ^(ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp[0-9]+)\ [A-Za-z0-9+/=]+(\ .*)?$ ]] ||
        die "--cd-key does not look like an SSH public key"
    body="$(awk '{print $2}' <<<"$cd_key")"
    auth="$HOME/.ssh/authorized_keys"
    mkdir -p "$HOME/.ssh"
    touch "$auth"
    if grep -qF "$body" "$auth"; then
        log "CD key already authorized"
    else
        cp -p "$auth" "$auth.bak-$(date +%Y%m%d%H%M%S)"
        printf 'command="%s",restrict %s\n' "$APP/bin/remote-deploy.sh" "$cd_key" >>"$auth"
        log "CD key authorized (it can only run remote-deploy.sh)"
    fi
fi

# --- Next steps ----------------------------------------------------------------
cat <<EOF

vault-sync is ready to receive deploys at $APP.

1. Settings and secrets: $env_file
   Generated passwords are not printed. Read them on the server when needed.

2. Notes backup: add this public key as a deploy key WITH WRITE ACCESS to the
   private repository in BACKUP_REMOTE (never the vault-sync repo):

   $(cat "$key.pub")

3. Continuous deployment, in your fork's Settings → Environments → production:
   secret DEPLOY_SSH_KEY      the CD private key (its public half goes in --cd-key)
   secret DEPLOY_KNOWN_HOSTS  "<host> $(awk '{print $1, $2}' /etc/ssh/ssh_host_ed25519_key.pub 2>/dev/null || echo '<this server ed25519 host key>')"
   variable DEPLOY_TARGET     $(id -un)@<host>

4. Deploy: publish a release, or run deploy/deploy.sh from a clone.
EOF
