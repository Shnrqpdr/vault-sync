#!/usr/bin/env bash
# Server side of the vault-sync deploy. Installed at $APP/bin/ by bootstrap.sh
# and used as the forced command of the CD's SSH key, so that key can run
# nothing but this:
#
#   deploy <tag>   reads a tar from stdin with release.tar.gz (the tag's code)
#                  and image.tar.gz (the prebuilt vault-core image), starts the
#                  release, waits for it to become healthy and, if it doesn't,
#                  rolls back to the previous one.
#   status         shows the active release and the containers' state.
#
# Layout under $APP (default ~/apps/vault-sync):
#   shared/.env        configuration and secrets (kept across releases)
#   shared/secrets/    deploy key for the Git backup
#   releases/<tag>/    each release's code
#   current            link to the active release
set -euo pipefail

APP="${VAULT_SYNC_HOME:-$HOME/apps/vault-sync}"
PROJECT="${VAULT_SYNC_PROJECT:-vault-sync}"
KEEP_RELEASES=5
HEALTH_TIMEOUT="${VAULT_SYNC_HEALTH_TIMEOUT:-240}"

log() { printf '[deploy] %s\n' "$*" >&2; }
die() {
    log "error: $*"
    exit 1
}

# Through the CD key the command arrives in SSH_ORIGINAL_COMMAND; by hand, as
# arguments. A leading path to this script (as deploy.sh sends it) is ignored.
if [[ -n "${SSH_ORIGINAL_COMMAND:-}" ]]; then
    read -r -a args <<<"$SSH_ORIGINAL_COMMAND"
else
    args=("$@")
fi
if [[ ${#args[@]} -gt 0 && "${args[0]}" == *remote-deploy.sh ]]; then
    args=("${args[@]:1}")
fi
verb="${args[0]:-}"
tag="${args[1]:-}"

# How the instance is published (VAULT_PROXY in shared/.env) decides the
# compose files. The .env is read, never sourced.
env_value() {
    sed -n "s/^$1=//p" "$APP/shared/.env" 2>/dev/null | tail -n 1
}
case "$(env_value VAULT_PROXY)" in
traefik) export COMPOSE_FILE=docker-compose.yml:docker-compose.traefik.yml ;;
caddy)
    export COMPOSE_FILE=docker-compose.yml
    export COMPOSE_PROFILES=proxy
    ;;
external | "") export COMPOSE_FILE=docker-compose.yml ;;
*) die "unknown VAULT_PROXY '$(env_value VAULT_PROXY)' (use traefik, caddy or external)" ;;
esac
# Escape hatch for tests and unusual setups.
COMPOSE_FILE="${VAULT_SYNC_COMPOSE_FILE:-$COMPOSE_FILE}"

compose() {
    # compose <release dir> <tag> <args...>
    local dir="$1" release_tag="$2"
    shift 2
    (cd "$dir" &&
        VAULT_CORE_IMAGE="vault-sync/vault-core:$release_tag" \
            SECRETS_DIR="$APP/shared/secrets" \
            docker compose -p "$PROJECT" --env-file "$APP/shared/.env" "$@")
}

health_of() {
    local id
    id="$(compose "$1" "$2" ps -q "$3")"
    [[ -n "$id" ]] || {
        echo "missing"
        return
    }
    docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "$id"
}

wait_healthy() {
    local dir="$1" release_tag="$2" deadline=$((SECONDS + HEALTH_TIMEOUT)) couch core
    while ((SECONDS < deadline)); do
        couch="$(health_of "$dir" "$release_tag" couchdb)"
        core="$(health_of "$dir" "$release_tag" vault-core)"
        if [[ "$couch" == healthy && "$core" == healthy ]]; then
            return 0
        fi
        sleep 5
    done
    log "not healthy after ${HEALTH_TIMEOUT}s (couchdb=$couch, vault-core=$core)"
    compose "$dir" "$release_tag" logs --tail 40 vault-core couchdb-init >&2 || true
    return 1
}

up() {
    compose "$1" "$2" up -d --no-build --remove-orphans
}

prune() {
    local current old
    current="$(readlink -f "$APP/current")"
    # Newest first; keeps KEEP_RELEASES and never the active one. Release dirs
    # are validated tags, so parsing ls is safe here.
    # shellcheck disable=SC2012
    mapfile -t old < <(ls -1dt "$APP"/releases/*/ 2>/dev/null | tail -n +$((KEEP_RELEASES + 1)))
    for dir in "${old[@]}"; do
        dir="${dir%/}"
        [[ "$(readlink -f "$dir")" == "$current" ]] && continue
        log "removing old release $(basename "$dir")"
        docker image rm "vault-sync/vault-core:$(basename "$dir")" >/dev/null 2>&1 || true
        rm -rf -- "$dir"
    done
}

cmd_status() {
    if [[ ! -L "$APP/current" ]]; then
        echo "no active release"
        return
    fi
    local dir release_tag
    dir="$(readlink -f "$APP/current")"
    release_tag="$(basename "$dir")"
    echo "active release: $release_tag"
    compose "$dir" "$release_tag" ps --format 'table {{.Service}}\t{{.Status}}'
}

cmd_deploy() {
    [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]] || die "invalid tag: '$tag'"
    [[ -f "$APP/shared/.env" ]] || die "$APP/shared/.env is missing (run bootstrap.sh first)"
    mkdir -p "$APP/releases"

    exec 9>"$APP/.deploy.lock"
    flock -n 9 || die "another deploy is in progress"

    local incoming
    incoming="$(mktemp -d "$APP/.incoming.XXXXXX")"
    # shellcheck disable=SC2064
    trap "rm -rf -- '$incoming'" EXIT

    log "receiving $tag"
    tar -xf - -C "$incoming"
    [[ -f "$incoming/release.tar.gz" && -f "$incoming/image.tar.gz" ]] || die "incomplete bundle"

    local release="$APP/releases/$tag" image="vault-sync/vault-core:$tag"
    mkdir -p "$release"
    tar -xzf "$incoming/release.tar.gz" -C "$release"
    gunzip -c "$incoming/image.tar.gz" | docker load -q >&2
    docker image inspect "$image" >/dev/null 2>&1 || die "the bundle did not contain $image"

    local previous="" previous_tag=""
    if [[ -L "$APP/current" ]]; then
        previous="$(readlink -f "$APP/current")"
        previous_tag="$(basename "$previous")"
    fi

    log "starting $tag${previous_tag:+ (previous: $previous_tag)}"
    if up "$release" "$tag" && wait_healthy "$release" "$tag"; then
        ln -sfn "$release" "$APP/current"
        # Update this script from the release (atomic rename: safe even while
        # it is running).
        cp "$release/deploy/remote-deploy.sh" "$APP/bin/.remote-deploy.sh.new"
        chmod 755 "$APP/bin/.remote-deploy.sh.new"
        mv -f "$APP/bin/.remote-deploy.sh.new" "$APP/bin/remote-deploy.sh"
        prune
        log "$tag is live"
        return 0
    fi

    if [[ -n "$previous" && "$previous" != "$(readlink -f "$release")" ]]; then
        log "failed; rolling back to $previous_tag"
        if up "$previous" "$previous_tag" && wait_healthy "$previous" "$previous_tag"; then
            log "rolled back to $previous_tag"
        else
            log "the rollback failed too; check by hand"
        fi
    fi
    die "deploy of $tag failed"
}

case "$verb" in
deploy) cmd_deploy ;;
status) cmd_status ;;
*) die "usage: remote-deploy.sh deploy <tag> | status" ;;
esac
