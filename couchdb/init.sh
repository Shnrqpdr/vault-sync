#!/bin/sh
# CouchDB setup for Self-hosted LiveSync.
# Adapted from docker/scripts/couchdb-init.sh in vrtmrz/obsidian-livesync (MIT).
#
#   init.sh           one-time setup (the couchdb-init service). Idempotent:
#                     safe to run on every `docker compose up`.
#   init.sh --watch   keeps the vault database restricted to the sync user (the
#                     couchdb-guard service). LiveSync's "Rebuild everything"
#                     deletes and recreates the database, and CouchDB recreates
#                     it admin-only, which locks out the sync user and vault-core.
#                     This restores the permissions within COUCH_GUARD_INTERVAL.
#
# Besides the LiveSync settings, it creates a non-admin sync user that owns the
# vault database. Obsidian and vault-core use that user; the admin credentials
# never leave the server.
set -eu

url="${COUCH_INTERNAL_URL:-http://couchdb:5984}"
admin="${COUCH_USER:?COUCH_USER is required}"
admin_pass="${COUCH_PASSWORD:?COUCH_PASSWORD is required}"
sync_user="${COUCH_SYNC_USER:?COUCH_SYNC_USER is required}"
sync_pass="${COUCH_SYNC_PASSWORD:?COUCH_SYNC_PASSWORD is required}"
db="${COUCH_DB:-obsidian}"
interval="${COUCH_GUARD_INTERVAL:-30}"
node="_local"

# These values go into JSON bodies below; keep them to safe characters.
for value in "$admin" "$admin_pass" "$sync_user" "$sync_pass" "$db"; do
  case "$value" in
    *[!A-Za-z0-9._~-]*)
      echo "CouchDB users, passwords and database names may only use letters, digits and . _ ~ -" >&2
      exit 1
      ;;
  esac
done
if [ "$sync_user" = "$admin" ]; then
  echo "COUCH_SYNC_USER must differ from COUCH_USER (the admin)" >&2
  exit 1
fi

echo "==> Waiting for CouchDB at ${url} ..."
# With credentials: after the first run require_valid_user is on and an
# unauthenticated /_up answers 401 forever.
until curl -sf -u "${admin}:${admin_pass}" "${url}/_up" >/dev/null 2>&1; do sleep 2; done

api() {
  # api <method> <path> [JSON body]
  if [ $# -ge 3 ]; then
    curl -sf -X "$1" "${url}$2" -u "${admin}:${admin_pass}" \
      -H "Content-Type: application/json" -d "$3" >/dev/null
  else
    curl -sf -X "$1" "${url}$2" -u "${admin}:${admin_pass}" >/dev/null
  fi
}

cfg() {
  # cfg <section/key> <JSON value>
  api PUT "/_node/${node}/_config/$1" "$2"
  echo "[ok] $1"
}

db_status() {
  curl -s -o /dev/null -w "%{http_code}" -u "${admin}:${admin_pass}" "${url}/${db}"
}

# Only the sync user (and server admins) can read or write the vault.
members="\"members\":{\"names\":[\"${sync_user}\"],\"roles\":[]}"
restrict_db() {
  api PUT "/${db}/_security" "{\"admins\":{\"names\":[],\"roles\":[]},${members}}"
}

if [ "${1:-}" = "--watch" ]; then
  echo "==> Keeping '${db}' restricted to '${sync_user}' (checking every ${interval}s)"
  while :; do
    # Missing (404) means a device is in the middle of rebuilding it: wait.
    if [ "$(db_status)" = "200" ]; then
      security=$(curl -s -u "${admin}:${admin_pass}" "${url}/${db}/_security" || true)
      case "$security" in
        *"$members"*) ;;
        *)
          if restrict_db; then
            echo "[fixed] '${db}' was recreated or changed; restricted to '${sync_user}' again"
          else
            echo "[warn] could not restore the permissions of '${db}'; retrying" >&2
          fi
          ;;
      esac
    fi
    sleep "$interval"
  done
fi

api POST /_cluster_setup \
  "{\"action\":\"enable_single_node\",\"username\":\"${admin}\",\"password\":\"${admin_pass}\",\"bind_address\":\"0.0.0.0\",\"port\":5984,\"singlenode\":true}" \
  && echo "[ok] cluster_setup" || echo "[info] cluster_setup already applied"

cfg chttpd/require_valid_user '"true"'
cfg chttpd_auth/require_valid_user '"true"'
cfg httpd/WWW-Authenticate '"Basic realm=\"couchdb\""'
cfg httpd/enable_cors '"true"'
cfg chttpd/enable_cors '"true"'
cfg chttpd/max_http_request_size '"4294967296"'
cfg couchdb/max_document_size '"50000000"'
cfg cors/credentials '"true"'
cfg cors/origins '"app://obsidian.md,capacitor://localhost,http://localhost"'

if [ "$(db_status)" = "200" ]; then
  echo "[ok] database '${db}' already exists"
else
  api PUT "/${db}" && echo "[ok] database '${db}' created"
fi

# Sync user: created, or its password updated to match the .env.
user_doc="/_users/org.couchdb.user:${sync_user}"
rev=$(curl -s -u "${admin}:${admin_pass}" "${url}${user_doc}" | sed -n 's/.*"_rev":"\([^"]*\)".*/\1/p')
api PUT "$user_doc" \
  "{\"name\":\"${sync_user}\",\"password\":\"${sync_pass}\",\"roles\":[],\"type\":\"user\"${rev:+,\"_rev\":\"${rev}\"}}"
echo "[ok] sync user '${sync_user}'"

restrict_db
echo "[ok] '${db}' restricted to '${sync_user}'"

echo "==> CouchDB ready."
