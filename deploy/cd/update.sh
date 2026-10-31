#!/usr/bin/env bash
# Continuous deployment: pulls `main` and deploys it to this host's compose stack, then the Worker.
# Installed by install.sh and run every few minutes by dash-nodes-update.timer; README "Deploy notes"
# has the whole picture.
#
#   dash-nodes-update [run]         one pass, what the timer runs; does nothing while paused
#   dash-nodes-update deploy <rev>  deploy this commit now: also while paused, an older commit (a
#                                   rollback: passes then skip main's head until it moves) or one
#                                   that failed; it must still be signed
#   dash-nodes-update status        what's deployed, where main is, pause and failure markers
#
# A pass, when main has moved:
#  1. The new head must be signed by a trusted key: git's %G? must be exactly "G", meaning a key in
#     $CONF/allowed_signers (SSH) or one with ultimate trust in $CONF/gnupg (GitHub's web-flow key,
#     which signs squash merges). An SSH key that isn't listed shows "U" and is refused. In a pass
#     (not `deploy`) it must also descend from the deployed commit, so a force-push can't roll back.
#  2. App: unpack the tree over APP_DIR (deleting files the commit removed; deploy/.env and
#     deploy/secrets/ aren't tracked and stay), run init.sh, build, `up -d`, restart services whose
#     bind-mounted files changed, then check that the app answers HEALTH_URL from the new image and
#     still does SETTLE_SECS later, with every service that was running still running and none
#     restarted in between. Otherwise the last
#     good tree and images (tagged :good) go back. A commit that fails ATTEMPTS times waits for a
#     newer one or `deploy`. Changes that don't touch the stack (docs) only update the tree.
#  3. Worker, if worker/ or web/ changed since its last deploy: once every network's published
#     summary is from a crawl that finished after the running app started (so pages never need
#     fields the data doesn't have yet; going back, the Worker goes first instead), build and test
#     in a NODE_IMAGE container, then `wrangler deploy` with the token in CF_TOKEN_FILE.
set -euo pipefail

CONF=${DASH_NODES_CD_CONF:-/etc/dash-nodes-cd}
# shellcheck source=/dev/null
[ -r "$CONF/env" ] && . "$CONF/env"
BRANCH=${BRANCH:-main}
APP_DIR=${APP_DIR:-/opt/dash-nodes}
STATE=${STATE:-/var/lib/dash-nodes-cd}
LIB=${LIB:-/usr/local/lib/dash-nodes-cd}
UNITS=${UNITS:-/etc/systemd/system}
HEALTH_URL=${HEALTH_URL:-http://127.0.0.1:8080/healthz}
HEALTH_TIMEOUT=${HEALTH_TIMEOUT:-180}
SETTLE_SECS=${SETTLE_SECS:-60}
ATTEMPTS=${ATTEMPTS:-3}
PUBLIC_URL=${PUBLIC_URL:-https://nodes.dashhq.org}
NETWORKS=${NETWORKS:-mainnet testnet}
SERVICES=${SERVICES:-}
CF_TOKEN_FILE=${CF_TOKEN_FILE:-$CONF/cf-token}
NODE_IMAGE=${NODE_IMAGE:-node:24-alpine}
REPO=$STATE/repo.git

# Paths that change what the compose stack runs (the app image also carries web/; the publisher
# runs worker/src and worker/scripts from the tree), and what the Worker is built from.
STACK_RE='^(src/|Cargo\.(toml|lock)$|Dockerfile$|\.dockerignore$|web/|deploy/|worker/(src|scripts)/)'
NOT_STACK_RE='^deploy/(cd|bench-vantage)/'
WORKER_RE='^(web/|worker/(src/|scripts/build\.mjs$|package(-lock)?\.json$|wrangler\.jsonc$))'
BENCH_RE='^src/(bench/|p2p\.rs$|transport\.rs$)'

die() { echo "error: $*" >&2; exit 1; }
# No X.509 (gpgsm): only the SSH allowed signers and the dedicated GPG keyring can say "G".
git_() {
  GNUPGHOME="$CONF/gnupg" git -C "$REPO" -c gpg.ssh.allowedSignersFile="$CONF/allowed_signers" \
    -c gpg.x509.program=false "$@"
}
short() { git_ rev-parse --short=12 "$1"; }
describe() { git_ log -1 --format='%h "%s"' "$1"; }
get() { cat "$STATE/$1" 2>/dev/null || true; }
put() { printf '%s\n' "$2" > "$STATE/$1.tmp" && mv -f "$STATE/$1.tmp" "$STATE/$1"; }
is_ancestor() { git_ merge-base --is-ancestor "$1" "$2"; }
# Not grep -q: exiting early would SIGPIPE git diff, which pipefail reports as "no match".
changed() { git_ diff --name-only --no-renames "$1" "$2" | grep -E "$3" >/dev/null; }
stack_changed() { git_ diff --name-only --no-renames "$1" "$2" | grep -Ev "$NOT_STACK_RE" | grep -E "$STACK_RE" >/dev/null; }
compose() { (cd "$APP_DIR/deploy" && docker compose "$@"); }
init() { (cd "$APP_DIR/deploy" && sh scripts/init.sh >/dev/null); }

fetch() { git_ fetch --quiet --prune origin "+refs/heads/$BRANCH:refs/remotes/origin/$BRANCH"; }
signed() { [ "$(git_ log -1 --format=%G? "$1")" = G ]; }

# Records what's deployed, also as a ref in the mirror so the commit outlives a force-push and gc.
mark() { put "$1-commit" "$2" && git_ update-ref "refs/deployed/$1" "$2"; }

# How often commit $1 failed: the `failed` file holds "<commit> <count>".
failures() {
  local c n
  read -r c n 2>/dev/null < "$STATE/failed" || { echo 0; return 0; }
  if [ "$c" = "$1" ]; then echo "${n:-1}"; else echo 0; fi
}
fail() { put failed "$1 $(( $(failures "$1") + 1 ))"; }
# After a rollback, `held` names main's head at the time: passes skip it until main moves on.
held() { [ "$(get held)" = "$1" ]; }

# Makes APP_DIR's tracked files those of commit $2, coming from commit $1.
sync_tree() {
  git_ diff --name-only --no-renames --diff-filter=D -z "$1" "$2" | (cd "$APP_DIR" && xargs -0r rm -f --) \
    && git_ -c tar.umask=022 archive "$2" | tar -x -C "$APP_DIR" --no-same-owner \
    && short "$2" > "$APP_DIR/DEPLOYED_COMMIT"
}

# The images this tree's stack builds (dash-nodes:local, dash-nodes-tor:local, ...). Each also has
# :good (the last that passed a deploy) and :previous (the one before that).
built_images() { compose config --images | grep -E '^dash-nodes(-[a-z0-9]+)?:' || true; }
revert_images() { local img; for img in $(built_images); do docker tag "${img%:*}:good" "$img" 2>/dev/null || true; done; }
promote_images() {
  local img id
  for img in $(built_images); do
    id=$(docker image inspect -f '{{.Id}}' "$img")
    [ "$id" != "$(docker image inspect -f '{{.Id}}' "${img%:*}:good" 2>/dev/null)" ] || continue
    docker tag "${img%:*}:good" "${img%:*}:previous" 2>/dev/null || true
    docker tag "$img" "${img%:*}:good" || return 1
  done
}

running_services() { compose ps --status running --format '{{.Service}}' | sort; }
# Each container's name, restart count and start time, to see whether anything restarted.
restarts() { compose ps -q | xargs -r docker inspect -f '{{.Name}} {{.RestartCount}} {{.State.StartedAt}}' | sort; }

# Services with a bind mount over a file or directory that changed from commit $1 to $2: tar
# replaces the files, but running containers keep the old ones and `up -d` sees no config change.
bind_changed() {
  local files
  files=$(git_ diff --name-only --no-renames "$1" "$2")
  compose config --format json | FILES="$files" APP="$APP_DIR" python3 -c '
import json, os, sys
app = os.path.realpath(os.environ["APP"])
files = [os.path.join(app, f) for f in os.environ["FILES"].splitlines() if f]
for name, svc in json.load(sys.stdin).get("services", {}).items():
    srcs = [os.path.realpath(v["source"]) for v in svc.get("volumes", []) if v.get("type") == "bind"]
    if any(f == s or f.startswith(s + "/") for s in srcs for f in files):
        print(name)'
}

# Starts the stack from APP_DIR's tree, coming from commit $1 to $2.
start() {
  local svcs svc
  svcs=$(bind_changed "$1" "$2") || { echo "can't tell which services mount changed files"; return 1; }
  # shellcheck disable=SC2086 # a list of service names
  compose up -d --quiet-pull $SERVICES || return 1
  for svc in $svcs; do compose restart "$svc" || return 1; done
}

app_container() { compose ps -aq app; }

# The app answers from the image it was just started with and still does SETTLE_SECS later, with
# every service in $1 (those running before the deploy) running and no container restarted meanwhile.
healthy() {
  local deadline=$((SECONDS + HEALTH_TIMEOUT)) cid before missing
  until curl -fs -m 5 -o /dev/null "$HEALTH_URL"; do
    [ "$SECONDS" -lt "$deadline" ] || { echo "no answer from $HEALTH_URL after ${HEALTH_TIMEOUT}s"; return 1; }
    sleep 5
  done
  cid=$(app_container)
  [ -n "$cid" ] || { echo "no app container"; return 1; }
  [ "$(docker inspect -f '{{.Image}}' "$cid")" = "$(docker image inspect -f '{{.Id}}' "$(docker inspect -f '{{.Config.Image}}' "$cid")")" ] \
    || { echo "the app isn't running the image just built"; return 1; }
  before=$(restarts)
  sleep "$SETTLE_SECS"
  [ "$(restarts)" = "$before" ] || { echo "a container restarted within ${SETTLE_SECS}s:" \
    "$(comm -13 <(echo "$before") <(restarts) | cut -d' ' -f1 | tr '\n' ' ')"; return 1; }
  curl -fs -m 5 -o /dev/null "$HEALTH_URL" || { echo "$HEALTH_URL stopped answering"; return 1; }
  missing=$(comm -23 <(printf '%s\n' "$1" | sed '/^$/d') <(running_services) | tr '\n' ' ')
  [ -z "$missing" ] || { echo "not running after the deploy: $missing"; return 1; }
}

# Puts back the deployed commit $2's tree and :good images after a deploy of $1, restarts, checks.
restore() { sync_tree "$1" "$2" && revert_images && init && start "$1" "$2" && healthy "$3"; }

# Deploys the app side of commit $2 over commit $1. On failure, puts $1 back and returns 1.
deploy_app() {
  local from=$1 to=$2 before
  if ! stack_changed "$from" "$to"; then
    sync_tree "$from" "$to" || return 1
    echo "$(short "$to"): nothing the stack runs changed; tree updated"
    return 0
  fi
  before=$(running_services)
  put attempt "$to"
  : > "$STATE/build.log"
  if ! { sync_tree "$from" "$to" && init && compose build > "$STATE/build.log" 2>&1; }; then
    echo "unpacking or building $(short "$to") failed; back to $(short "$from") (nothing was restarted)"
    tail -n 30 "$STATE/build.log"
    { sync_tree "$to" "$from" && revert_images; } || echo "putting back $(short "$from")'s tree failed too"
    rm -f "$STATE/attempt"
    return 1
  fi
  if ! { start "$from" "$to" && healthy "$before"; }; then
    echo "$(short "$to") isn't healthy; rolling back to $(short "$from")"
    if restore "$to" "$from" "$before"; then echo "rolled back to $(short "$from")"
    else echo "after rolling back to $(short "$from") the stack still isn't healthy"; fi
    rm -f "$STATE/attempt"
    return 1
  fi
  promote_images || echo "warning: tagging the new images :good failed; a later rollback may go further back"
  # From here on the deploy counts as done, even if this pass dies before deploy() records it.
  put attempt "$to promoted"
  docker image prune -f >/dev/null
  echo "$(short "$to"): app healthy"
}

# A pass that died mid-deploy (timeout, reboot) left `attempt`: if the deploy had passed, record it;
# otherwise count it as a failure and put back what's recorded as deployed.
recover() {
  local a phase app
  read -r a phase 2>/dev/null < "$STATE/attempt" || return 0
  app=$(get app-commit)
  if [ "$phase" = promoted ]; then
    [ "$a" = "$app" ] || { mark app "$a"; rm -f "$STATE/failed"; echo "a pass died after deploying $(short "$a"); recorded it"; }
  elif [ "$a" != "$app" ]; then
    echo "a pass died while deploying $(short "$a"); restoring $(short "$app")"
    fail "$a"
    restore "$a" "$app" "$(running_services)" || echo "after restoring $(short "$app") the stack still isn't healthy"
  fi
  rm -f "$STATE/attempt"
}

# True once every network's published summary comes from a crawl that finished after the app started.
published_since_start() {
  local cid started net finished
  cid=$(app_container)
  [ -n "$cid" ] || return 1
  started=$(date -d "$(docker inspect -f '{{.State.StartedAt}}' "$cid")" +%s) || return 1
  for net in $NETWORKS; do
    finished=$(curl -fsS -m 20 "$PUBLIC_URL/api/v1/$net/summary" \
      | python3 -c 'import json,sys; print(json.load(sys.stdin)["crawl"]["finished"])') || return 1
    [ "$finished" -ge "$started" ] || return 1
  done
}

# Runs a NODE_IMAGE container (docker run options, the image, the command) on the Worker's sources.
in_node() {
  docker run --rm -e WRANGLER_SEND_METRICS=false -e CI=1 -v "$WORK:/src" -v "$STATE/npm:/root/.npm" \
    -w /src/worker "$@"
}

deploy_worker() {
  local to=$1 force=$2 from rc=0
  from=$(get worker-commit)
  [ "$from" != "$to" ] || return 0
  if [ -n "$from" ] && ! changed "$from" "$to" "$WORKER_RE"; then mark worker "$to"; return 0; fi
  if [ -z "$force" ] && [ "$(get worker-failed)" = "$to" ]; then
    echo "Worker: $(short "$to") failed to deploy before; waiting for a newer commit or \`deploy\`"
    return 1
  fi
  [ -s "$CF_TOKEN_FILE" ] || { echo "Worker: $(short "$to") waits for a Cloudflare token in $CF_TOKEN_FILE"; return 0; }
  # Older pages work on newer data, so going back needs no wait.
  if ! { [ -n "$from" ] && is_ancestor "$to" "$from"; } && ! published_since_start; then
    echo "Worker: $(short "$to") waits for the publisher to upload a crawl from the running app"
    return 0
  fi
  echo "Worker: deploying $(describe "$to")"
  WORK=$(mktemp -d)
  trap 'rm -rf "$WORK" || true' EXIT  # if this pass dies; otherwise removed below
  git_ archive "$to" worker web | tar -x -C "$WORK"
  mkdir -p "$STATE/npm"
  # No install scripts, and the token only for wrangler (which comes from the lockfile, like the rest).
  in_node "$NODE_IMAGE" sh -c 'npm ci --ignore-scripts --no-audit --no-fund --loglevel=error \
      && { npm test --silent > /tmp/test.log 2>&1 || { cat /tmp/test.log; exit 1; }; } && npm run build --silent' \
    && CLOUDFLARE_API_TOKEN=$(cat "$CF_TOKEN_FILE") in_node -e CLOUDFLARE_API_TOKEN \
      ${CLOUDFLARE_ACCOUNT_ID:+-e CLOUDFLARE_ACCOUNT_ID="$CLOUDFLARE_ACCOUNT_ID"} "$NODE_IMAGE" \
      node_modules/.bin/wrangler deploy \
    || rc=$?
  rm -rf "$WORK" || true
  if [ "$rc" -ne 0 ]; then
    put worker-failed "$to"
    echo "Worker: deploying $(short "$to") failed ($rc); the previous version keeps serving"
    return 1
  fi
  mark worker "$to"
  rm -f "$STATE/worker-failed"
  echo "Worker: $(short "$to") deployed; $PUBLIC_URL/api/v1/health answers $(curl -s -o /dev/null -w '%{http_code}' -m 20 "$PUBLIC_URL/api/v1/health")"
}

# Installs the deployed tree's updater and units if they differ from what's installed, unless the
# deployed commit is older than the one the installed updater came from (a rollback keeps it).
self_update() {
  local d=$APP_DIR/deploy/cd app u
  app=$(get app-commit)
  u=$(get updater-commit)
  [ -f "$d/install.sh" ] || return 0
  if [ -n "$u" ] && ! is_ancestor "$u" "$app"; then return 0; fi
  cmp -s "$d/update.sh" "$LIB/update.sh" && cmp -s "$d/dash-nodes-update.service" "$UNITS/dash-nodes-update.service" \
    && cmp -s "$d/dash-nodes-update.timer" "$UNITS/dash-nodes-update.timer" && return 0
  sh "$d/install.sh" >/dev/null || die "installing the updater from $(short "$app") failed; retrying next pass"
  put updater-commit "$app"
  echo "updated dash-nodes-update from $(short "$app")"
}

# Deploys commit $1; $2 is "force" for `deploy` (older or previously failed commits allowed).
deploy() {
  local to=$1 force=${2:-} app wk
  app=$(get app-commit)
  wk=$(get worker-commit)
  [ -n "$app" ] || die "no $STATE/app-commit: run install.sh first"
  if [ "$app" != "$to" ]; then
    if [ -z "$force" ] && [ "$(failures "$to")" -ge "$ATTEMPTS" ]; then
      die "$(describe "$to") failed $ATTEMPTS times; waiting for a newer commit or \`dash-nodes-update deploy\`"
    fi
    signed "$to" || die "$(describe "$to") isn't signed by a trusted key ($(git_ log -1 --format='%G? %GK' "$to")); not deploying"
    if [ -z "$force" ] && ! is_ancestor "$app" "$to"; then
      die "$(describe "$to") doesn't descend from the deployed $(short "$app"); deploy it by hand if that's intended"
    fi
    echo "deploying $(describe "$to") over $(short "$app"), signed by $(git_ log -1 --format='%GK %GS' "$to")"
    if [ -n "$wk" ] && is_ancestor "$to" "$wk"; then
      deploy_worker "$to" "$force" || echo "Worker: going back failed; the app goes back anyway"
    fi
    if ! deploy_app "$app" "$to"; then
      fail "$to"
      die "$(short "$to") not deployed (attempt $(failures "$to") of $ATTEMPTS)"
    fi
    mark app "$to"
    rm -f "$STATE/failed" "$STATE/attempt"
    if [ -n "$force" ] && is_ancestor "$to" "$app"; then
      put held "$(git_ rev-parse "refs/remotes/origin/$BRANCH")"
      echo "rolled back: passes skip $BRANCH at $(short "$(get held)") until it moves on"
    fi
    if changed "$app" "$to" "$BENCH_RE"; then
      echo "bench code changed: bench vantages on other hosts (deploy/bench-vantage) need the new image by hand"
    fi
  fi
  local rc=0
  deploy_worker "$to" "$force" || rc=$?
  # Even after a Worker failure: the fix for it may be in the updater.
  self_update
  return "$rc"
}

status() {
  local head c
  # No fetch: a pass may be using the mirror. This shows main as of the last pass.
  head=$(git_ rev-parse --verify -q "refs/remotes/origin/$BRANCH" || true)
  echo "$BRANCH:   ${head:+$(describe "$head") signature $(git_ log -1 --format='%G? %GK' "$head")}"
  c=$(get app-commit); echo "app:    ${c:+$(describe "$c")}"
  c=$(get worker-commit); echo "worker: ${c:+$(describe "$c")}"
  [ ! -e "$CONF/paused" ] || echo "PAUSED ($CONF/paused exists)"
  if read -r c _ 2>/dev/null < "$STATE/failed"; then echo "failed or held: $(describe "$c") ($(failures "$c") of $ATTEMPTS)"; fi
  c=$(get held); [ -z "$c" ] || echo "held after a rollback: $(describe "$c") (skipped until $BRANCH moves)"
  c=$(get worker-failed); [ -z "$c" ] || echo "Worker failed: $(describe "$c")"
  if read -r c _ 2>/dev/null < "$STATE/attempt"; then echo "deploying now (or a pass died deploying): $(describe "$c")"; fi
}

main() {
  local head
  [ -d "$REPO" ] || die "no $REPO: run install.sh first"
  case "${1:-run}" in
    status) status; return ;;
    run|deploy) ;;
    *) die "usage: dash-nodes-update [run | deploy <commit> | status]" ;;
  esac
  exec 9> "$STATE/lock"
  if [ "${1:-run}" = run ]; then
    flock -n 9 || { echo "another run is in progress"; return; }
    [ ! -e "$CONF/paused" ] || { echo "paused ($CONF/paused exists)"; return; }
    recover
    fetch
    head=$(git_ rev-parse "refs/remotes/origin/$BRANCH")
    if held "$head"; then echo "$(short "$head") is held after a rollback; waiting for $BRANCH to move"; return; fi
    rm -f "$STATE/held"
    deploy "$head"
  else
    [ -n "${2:-}" ] || die "usage: dash-nodes-update deploy <commit>"
    flock -n 9 || die "another run is in progress; try again when it's done"
    recover
    fetch
    deploy "$(git_ rev-parse --verify "$2^{commit}")" force
  fi
}

# Everything runs from main, so bash has read the whole file before a deploy replaces it.
main "$@"
exit
