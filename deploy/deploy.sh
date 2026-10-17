#!/usr/bin/env bash
# Ships a vault-sync tag to your server. Used by the CD workflow
# (.github/workflows/cd.yml) and for manual deploys:
#
#   DEPLOY_TARGET=user@host deploy/deploy.sh v1.2.3
#
# The vault-core image is built here (on the CI runner or your machine) from
# the tag's content, and travels to the server together with the code in a
# single SSH stream. The server builds nothing and needs no GitHub access.
#
# Variables:
#   DEPLOY_TARGET  user@host of the server (required)
#   DEPLOY_SSH     ssh command (default: ssh), e.g. "ssh -i ~/.ssh/deploy"
#   DEPLOY_PATH    remote script path, relative to the home directory
#                  (default: apps/vault-sync/bin/remote-deploy.sh)
set -euo pipefail

tag="${1:?usage: deploy/deploy.sh <tag>}"
target="${DEPLOY_TARGET:?set DEPLOY_TARGET=user@host}"
ssh_cmd="${DEPLOY_SSH:-ssh}"
remote_path="${DEPLOY_PATH:-apps/vault-sync/bin/remote-deploy.sh}"

log() { printf '[deploy] %s\n' "$*" >&2; }

root="$(git rev-parse --show-toplevel)"
git -C "$root" rev-parse -q --verify "refs/tags/$tag^{commit}" >/dev/null ||
    {
        log "tag $tag not found here (missing git fetch --tags?)"
        exit 1
    }
# Only ship what is already on main.
git -C "$root" fetch -q origin main
git -C "$root" merge-base --is-ancestor "$tag" origin/main ||
    {
        log "tag $tag is not on main"
        exit 1
    }

work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT
git -C "$root" archive --format=tar.gz -o "$work/release.tar.gz" "$tag"
mkdir "$work/src"
tar -xzf "$work/release.tar.gz" -C "$work/src"

image="vault-sync/vault-core:$tag"
log "building $image"
docker build -q -t "$image" "$work/src/vault-core" >/dev/null
docker save "$image" | gzip -1 >"$work/image.tar.gz"
log "bundle: $(du -h "$work/release.tar.gz" | cut -f1) of code, $(du -h "$work/image.tar.gz" | cut -f1) of image"

log "sending to $target"
# shellcheck disable=SC2086
tar -cf - -C "$work" release.tar.gz image.tar.gz |
    $ssh_cmd "$target" "$remote_path deploy $tag"
