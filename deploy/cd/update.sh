#!/usr/bin/env bash
# Continuous deployment: pulls `main` and deploys it to this host's compose stack, then the Worker.
# Installed by install.sh and run every few minutes by dash-nodes-update.timer (README "Deploy notes").
#
#   dash-nodes-update [run]         one pass, what the timer runs; does nothing while paused
#   dash-nodes-update deploy <rev>  deploy this commit now: also while paused, an older commit (a
#                                   rollback) or one that failed before; it must still be signed
#   dash-nodes-update status        what's deployed, where main is, pause and failure markers
#
# A pass, when main has moved:
#  1. The new head must be signed by a trusted key: git's %G? must be exactly "G", meaning a key in
#     $CONF/allowed_signers (SSH) or one with ultimate trust in $CONF/gnupg (GitHub's web-flow key,
#     which signs squash merges). An SSH key that isn't listed shows "U" and is refused. In a pass
#     (not `deploy`) it must also descend from the deployed commit, so a force-push can't roll back.
#  2. App: unpack the tree over APP_DIR (deleting files the commit removed; deploy/.env and
#     deploy/secrets/ aren't tracked and stay), run init.sh, build, `up -d`, then wait until the
#     app's health URL answers and it hasn't restarted SETTLE_SECS later. If the build fails or the
#     app doesn't come up, the previous tree and images go back and the commit is marked failed: it
#     isn't tried again until a newer commit or `deploy`. Changes that don't touch the stack (docs)
#     only update the tree.
#  3. Worker, if worker/ or web/ changed since its last deploy: once every network's published
#     summary is from a crawl that finished after the running app started (so pages never need
#     fields the data doesn't have yet), `npm ci && npm test && npm run deploy` in a NODE_IMAGE
#     container, with the Cloudflare token in CF_TOKEN_FILE. Until then each pass just says it waits.
set -euo pipefail

CONF=${DASH_NODES_CD_CONF:-/etc/dash-nodes-cd}
# shellcheck source=/dev/null
[ -r "$CONF/env" ] && . "$CONF/env"
REPO_URL=${REPO_URL:-https://github.com/PastaPastaPasta/dash-nodes.git}
BRANCH=${BRANCH:-main}
APP_DIR=${APP_DIR:-/opt/dash-nodes}
STATE=${STATE:-/var/lib/dash-nodes-cd}
HEALTH_URL=${HEALTH_URL:-http://127.0.0.1:8080/healthz}
HEALTH_TIMEOUT=${HEALTH_TIMEOUT:-180}
SETTLE_SECS=${SETTLE_SECS:-60}
PUBLIC_URL=${PUBLIC_URL:-https://nodes.dashhq.org}
NETWORKS=${NETWORKS:-mainnet testnet}
SERVICES=${SERVICES:-}
CF_TOKEN_FILE=${CF_TOKEN_FILE:-$CONF/cf-token}
NODE_IMAGE=${NODE_IMAGE:-node:24-alpine}
REPO=$STATE/repo.git

# Paths that change what the compose stack runs (the app image also carries web/); the publisher
# runs worker/src and worker/scripts from the tree; the Worker is built from worker/ and web/.
STACK_RE='^(src/|Cargo\.(toml|lock)$|Dockerfile$|\.dockerignore$|web/|deploy/|worker/(src|scripts)/)'
PUBLISHER_RE='^worker/(src|scripts)/'
WORKER_RE='^(web/|worker/(src/|scripts/build\.mjs$|package(-lock)?\.json$|wrangler\.jsonc$))'
BENCH_RE='^src/(bench/|p2p\.rs$|transport\.rs$)'

log() { echo "$*"; }
die() { echo "error: $*" >&2; exit 1; }
git_() { GNUPGHOME="$CONF/gnupg" git -C "$REPO" -c gpg.ssh.allowedSignersFile="$CONF/allowed_signers" "$@"; }
short() { git_ rev-parse --short=12 "$1"; }
describe() { git_ log -1 --format='%h "%s"' "$1"; }
get() { cat "$STATE/$1" 2>/dev/null || true; }
put() { printf '%s\n' "$2" > "$STATE/$1.tmp" && mv -f "$STATE/$1.tmp" "$STATE/$1"; }
# Not grep -q: exiting early would SIGPIPE git diff, which pipefail reports as "no match".
changed() { git_ diff --name-only --no-renames "$1" "$2" | grep -E "$3" >/dev/null; }
compose() { (cd "$APP_DIR/deploy" && docker compose "$@"); }

fetch() { git_ fetch --quiet --prune origin "+refs/heads/$BRANCH:refs/remotes/origin/$BRANCH"; }

signed() { [ "$(git_ log -1 --format=%G? "$1")" = G ]; }

# Makes APP_DIR's tracked files those of commit $2, coming from commit $1.
sync_tree() {
  git_ diff --name-only --no-renames --diff-filter=D -z "$1" "$2" | (cd "$APP_DIR" && xargs -0r rm -f --)
  git_ archive "$2" | tar -x -C "$APP_DIR" --no-same-owner
  short "$2" > "$APP_DIR/DEPLOYED_COMMIT"
}

# The images this stack builds (dash-nodes:local, dash-nodes-tor:local, ...).
built_images() { compose config --images | grep -E '^dash-nodes(-[a-z0-9]+)?:' || true; }

# Starts the stack from APP_DIR's tree; restarts the publisher if its scripts changed from $1 to $2.
start() {
  # shellcheck disable=SC2086 # a list of service names
  compose up -d --quiet-pull $SERVICES
  if changed "$1" "$2" "$PUBLISHER_RE"; then compose restart publisher; fi
}

app_container() { compose ps -aq app; }

healthy() {
  local deadline=$((SECONDS + HEALTH_TIMEOUT)) cid started
  until curl -fs -m 5 -o /dev/null "$HEALTH_URL"; do
    [ "$SECONDS" -lt "$deadline" ] || { log "no answer from $HEALTH_URL after ${HEALTH_TIMEOUT}s"; return 1; }
    sleep 5
  done
  cid=$(app_container)
  started=$(docker inspect -f '{{.State.StartedAt}}' "$cid")
  sleep "$SETTLE_SECS"
  [ "$(docker inspect -f '{{.State.StartedAt}} {{.State.Running}}' "$cid")" = "$started true" ] \
    || { log "the app restarted within ${SETTLE_SECS}s"; return 1; }
  curl -fsS -m 5 -o /dev/null "$HEALTH_URL" || { log "$HEALTH_URL stopped answering"; return 1; }
}

# Deploys the app side of commit $2 over commit $1. On failure, puts $1 back and returns 1.
deploy_app() {
  local from=$1 to=$2 img
  if ! changed "$from" "$to" "$STACK_RE"; then
    sync_tree "$from" "$to" || return 1
    log "$(short "$to"): nothing the stack runs changed; tree updated"
    return 0
  fi
  for img in $(built_images); do
    docker image inspect "$img" >/dev/null 2>&1 && docker tag "$(docker image inspect -f '{{.Id}}' "$img")" "${img%:*}:previous"
  done
  if ! sync_tree "$from" "$to"; then
    log "unpacking $(short "$to") failed; back to the tree of $(short "$from")"
    sync_tree "$to" "$from"
    return 1
  fi
  if ! (cd "$APP_DIR/deploy" && sh scripts/init.sh >/dev/null && docker compose build --quiet); then
    log "build failed; back to the tree of $(short "$from") (nothing was restarted)"
    sync_tree "$to" "$from"
    return 1
  fi
  start "$from" "$to"
  if healthy; then
    docker image prune -f >/dev/null
    log "$(short "$to"): app healthy"
    return 0
  fi
  log "$(short "$to") isn't healthy; rolling back to $(short "$from")"
  for img in $(built_images); do
    docker image inspect "${img%:*}:previous" >/dev/null 2>&1 && docker tag "${img%:*}:previous" "$img"
  done
  sync_tree "$to" "$from"
  (cd "$APP_DIR/deploy" && sh scripts/init.sh >/dev/null)
  start "$to" "$from"
  if healthy; then log "rolled back to $(short "$from")"; else log "after rolling back to $(short "$from") the app still isn't healthy"; fi
  return 1
}

# True once every network's published summary comes from a crawl that finished after the app started.
published_since_start() {
  local started net finished
  started=$(date -d "$(docker inspect -f '{{.State.StartedAt}}' "$(app_container)")" +%s)
  for net in $NETWORKS; do
    finished=$(curl -fsS -m 20 "$PUBLIC_URL/api/v1/$net/summary" \
      | python3 -c 'import json,sys; print(json.load(sys.stdin)["crawl"]["finished"])') || return 1
    [ "$finished" -ge "$started" ] || return 1
  done
}

deploy_worker() {
  local to=$1 force=$2 from tmp rc=0
  from=$(get worker-commit)
  [ "$from" != "$to" ] || return 0
  if [ -n "$from" ] && ! changed "$from" "$to" "$WORKER_RE"; then put worker-commit "$to"; return 0; fi
  if [ -z "$force" ] && [ "$(get worker-failed)" = "$to" ]; then
    log "Worker: $(short "$to") failed to deploy before; waiting for a newer commit or \`deploy\`"
    return 1
  fi
  [ -s "$CF_TOKEN_FILE" ] || { log "Worker: $(short "$to") waits for a Cloudflare token in $CF_TOKEN_FILE"; return 0; }
  published_since_start || { log "Worker: $(short "$to") waits for the publisher to upload a crawl from the running app"; return 0; }
  log "Worker: deploying $(describe "$to")"
  tmp=$(mktemp -d)
  git_ archive "$to" worker web | tar -x -C "$tmp"
  mkdir -p "$STATE/npm"
  CLOUDFLARE_API_TOKEN=$(cat "$CF_TOKEN_FILE") docker run --rm \
    -e CLOUDFLARE_API_TOKEN ${CLOUDFLARE_ACCOUNT_ID:+-e CLOUDFLARE_ACCOUNT_ID="$CLOUDFLARE_ACCOUNT_ID"} \
    -e WRANGLER_SEND_METRICS=false -e CI=1 -v "$tmp:/src" -v "$STATE/npm:/root/.npm" -w /src/worker \
    "$NODE_IMAGE" sh -c 'npm ci --no-audit --no-fund --loglevel=error \
      && { npm test --silent > /tmp/test.log 2>&1 || { cat /tmp/test.log; exit 1; }; } && npm run deploy' || rc=$?
  rm -rf "$tmp"
  if [ "$rc" -ne 0 ]; then
    put worker-failed "$to"
    log "Worker: deploying $(short "$to") failed ($rc); the previous version keeps serving"
    return 1
  fi
  put worker-commit "$to"
  rm -f "$STATE/worker-failed"
  log "Worker: $(short "$to") deployed; $PUBLIC_URL/api/v1/health answers $(curl -s -o /dev/null -w '%{http_code}' -m 20 "$PUBLIC_URL/api/v1/health")"
}

# Deploys commit $1; $2 is "force" for `deploy` (paused, older or previously failed commits allowed).
deploy() {
  local to=$1 force=${2:-} app
  app=$(get app-commit)
  [ -n "$app" ] || die "no $STATE/app-commit: run install.sh first"
  if [ "$app" != "$to" ]; then
    if [ -z "$force" ] && [ "$(get failed)" = "$to" ]; then
      die "$(describe "$to") failed before; waiting for a newer commit or \`dash-nodes-update deploy\`"
    fi
    signed "$to" || die "$(describe "$to") isn't signed by a trusted key ($(git_ log -1 --format='%G? %GK' "$to")); not deploying"
    if [ -z "$force" ] && ! git_ merge-base --is-ancestor "$app" "$to"; then
      die "$(describe "$to") doesn't descend from the deployed $(short "$app"); deploy it by hand if that's intended"
    fi
    log "deploying $(describe "$to") over $(short "$app"), signed by $(git_ log -1 --format='%GK %GS' "$to")"
    if ! deploy_app "$app" "$to"; then
      put failed "$to"
      die "$(short "$to") not deployed"
    fi
    put app-commit "$to"
    rm -f "$STATE/failed"
    if changed "$app" "$to" "$BENCH_RE"; then
      log "bench code changed: bench vantages on other hosts (deploy/bench-vantage) need the new image by hand"
    fi
    if changed "$app" "$to" '^deploy/cd/'; then
      sh "$APP_DIR/deploy/cd/install.sh" >/dev/null && log "updated the updater from $(short "$to")"
    fi
  fi
  deploy_worker "$to" "$force"
}

status() {
  local head
  fetch || log "(fetch failed; showing the last fetched main)"
  head=$(git_ rev-parse --verify -q "refs/remotes/origin/$BRANCH" || true)
  log "main:   ${head:+$(describe "$head") signature $(git_ log -1 --format='%G? %GK' "$head")}"
  log "app:    $(c=$(get app-commit); [ -n "$c" ] && describe "$c")"
  log "worker: $(c=$(get worker-commit); [ -n "$c" ] && describe "$c")"
  [ ! -e "$CONF/paused" ] || log "PAUSED ($CONF/paused exists)"
  [ -z "$(get failed)" ] || log "failed: $(describe "$(get failed)")"
  [ -z "$(get worker-failed)" ] || log "worker failed: $(describe "$(get worker-failed)")"
}

main() {
  [ -d "$REPO" ] || die "no $REPO: run install.sh first"
  exec 9> "$STATE/lock"
  flock -n 9 || { log "another run is in progress"; exit 0; }
  case "${1:-run}" in
    run)
      [ ! -e "$CONF/paused" ] || { log "paused ($CONF/paused exists)"; exit 0; }
      fetch
      deploy "$(git_ rev-parse "refs/remotes/origin/$BRANCH")"
      ;;
    deploy)
      [ -n "${2:-}" ] || die "usage: dash-nodes-update deploy <commit>"
      fetch
      deploy "$(git_ rev-parse --verify "${2}^{commit}")" force
      ;;
    status) status ;;
    *) die "usage: dash-nodes-update [run | deploy <commit> | status]" ;;
  esac
}

# Everything runs from main, so bash has read the whole file before a deploy replaces it.
main "$@"
exit
