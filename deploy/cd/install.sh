#!/bin/sh
# Installs or updates dash-nodes-update (update.sh) and its systemd units, as root on the stack's
# host. `--enable` also enables the timer, whose first pass then runs right away: check the
# fingerprints this prints first. Safe to re-run; update.sh re-runs it (without --enable) when a
# deploy changes deploy/cd/. Files holding this host's settings and trust are created once and then
# left alone:
#   /etc/dash-nodes-cd/env              settings (from env.example)
#   /etc/dash-nodes-cd/allowed_signers  SSH keys whose commits deploy: GITHUB_USER's signing keys
#                                       on GitHub at install time
#   /etc/dash-nodes-cd/gnupg/           GPG keyring: GitHub's web-flow key (squash merges), pinned
#   /var/lib/dash-nodes-cd/             mirror of the repo; app-commit and worker-commit, what's
#                                       deployed: APP_COMMIT and WORKER_COMMIT if given (e.g. when
#                                       the live Worker is older), else APP_DIR/DEPLOYED_COMMIT
# The running stack's images are tagged :good (what a failed deploy goes back to) if they aren't yet.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
CONF=${DASH_NODES_CD_CONF:-/etc/dash-nodes-cd}
GITHUB_USER=${GITHUB_USER:-PastaPastaPasta}
# GitHub's web-flow commit signing key since 2024-01-16 (https://github.com/web-flow.gpg).
WEB_FLOW_FPR=968479A1AFF927E37D1A566BB5690EEEBB952194

# Copies $1 to $2 with mode $3, replacing $2 in one rename (a running update.sh keeps its old copy).
put() { install -m "$3" "$1" "$2.new" && mv -f "$2.new" "$2"; }

install -d -m 0700 "$CONF"
[ -e "$CONF/env" ] || put "$here/env.example" "$CONF/env" 0600
# shellcheck source=/dev/null
. "$CONF/env"
STATE=${STATE:-/var/lib/dash-nodes-cd}
APP_DIR=${APP_DIR:-/opt/dash-nodes}
LIB=${LIB:-/usr/local/lib/dash-nodes-cd}
UNITS=${UNITS:-/etc/systemd/system}
BIN=${BIN:-/usr/local/sbin}
REPO=$STATE/repo.git
REPO_URL=${REPO_URL:-https://github.com/PastaPastaPasta/dash-nodes.git}
BRANCH=${BRANCH:-main}
install -d -m 0700 "$STATE"
install -d -m 0755 "$LIB"

# Root runs init.sh, the build and this script from APP_DIR's tree: nobody else may be able to change it.
if [ "$(id -u)" = 0 ]; then
  bad=$(find "$APP_DIR" -path "$APP_DIR/deploy/secrets/*" -prune -o \( ! -user 0 -o -perm -o+w -o \( ! -group 0 -perm -g+w \) \) -print | head -5)
  [ -z "$bad" ] || { echo "$APP_DIR must be root's alone; not so for: $bad" >&2; exit 1; }
fi

if [ ! -s "$CONF/allowed_signers" ]; then
  curl -fsS "https://api.github.com/users/$GITHUB_USER/ssh_signing_keys" \
    | python3 -c 'import json,sys; print("".join("* namespaces=\"git\" " + k["key"] + "\n" for k in json.load(sys.stdin)), end="")' \
    > "$CONF/allowed_signers.new"
  [ -s "$CONF/allowed_signers.new" ] || { echo "no SSH signing keys for $GITHUB_USER" >&2; exit 1; }
  mv -f "$CONF/allowed_signers.new" "$CONF/allowed_signers"
  echo "allowed_signers ($GITHUB_USER's SSH signing keys):"
  cut -d' ' -f3- "$CONF/allowed_signers" | ssh-keygen -lf -
fi

export GNUPGHOME="$CONF/gnupg"
install -d -m 0700 "$GNUPGHOME"
if ! gpg --batch --export-ownertrust 2>/dev/null | grep -q "^$WEB_FLOW_FPR:6:"; then
  gpg --batch --list-keys "$WEB_FLOW_FPR" >/dev/null 2>&1 || curl -fsS https://github.com/web-flow.gpg | gpg --batch --quiet --import
  gpg --batch --list-keys "$WEB_FLOW_FPR" >/dev/null
  echo "$WEB_FLOW_FPR:6:" | gpg --batch --quiet --import-ownertrust
  echo "gnupg: trusts GitHub web-flow $WEB_FLOW_FPR"
fi

if [ ! -d "$REPO" ]; then
  git init --quiet --bare "$REPO"
  git -C "$REPO" remote add origin "$REPO_URL"
fi
# Only needed for a first install; a re-run can do without.
git -C "$REPO" fetch --quiet origin "+refs/heads/$BRANCH:refs/remotes/origin/$BRANCH" \
  || { [ -s "$STATE/app-commit" ] && [ -s "$STATE/worker-commit" ]; }

# Records the deployed commit $2 (default: APP_DIR/DEPLOYED_COMMIT) as $1-commit, unless it's set.
deployed() {
  [ ! -s "$STATE/$1-commit" ] || return 0
  git -C "$REPO" rev-parse --verify "${2:-$(cat "$APP_DIR/DEPLOYED_COMMIT")}^{commit}" > "$STATE/$1-commit"
  git -C "$REPO" update-ref "refs/deployed/$1" "$(cat "$STATE/$1-commit")"
  echo "$1-commit: $(cat "$STATE/$1-commit")"
}
deployed app "${APP_COMMIT:-}"
deployed worker "${WORKER_COMMIT:-}"
# The commit this updater comes from: update.sh replaces it only with one from a newer deploy.
[ -s "$STATE/updater-commit" ] || cp "$STATE/app-commit" "$STATE/updater-commit"

for img in $(cd "$APP_DIR/deploy" && docker compose config --images | grep -E '^dash-nodes(-[a-z0-9]+)?:' || true); do
  if ! docker image inspect "${img%:*}:good" >/dev/null 2>&1 && docker image inspect "$img" >/dev/null 2>&1; then
    docker tag "$img" "${img%:*}:good" && echo "tagged $img as ${img%:*}:good"
  fi
done

put "$here/update.sh" "$LIB/update.sh" 0755
ln -sfn "$LIB/update.sh" "$BIN/dash-nodes-update"
timer_changed=; cmp -s "$here/dash-nodes-update.timer" "$UNITS/dash-nodes-update.timer" || timer_changed=1
put "$here/dash-nodes-update.service" "$UNITS/dash-nodes-update.service" 0644
put "$here/dash-nodes-update.timer" "$UNITS/dash-nodes-update.timer" 0644
systemctl daemon-reload
if [ "${1:-}" = --enable ]; then
  systemctl enable --now dash-nodes-update.timer
  echo "enabled dash-nodes-update.timer"
elif [ -n "$timer_changed" ]; then
  # Picks up a new schedule, if the timer runs at all (a disabled one stays off).
  systemctl try-restart dash-nodes-update.timer
fi
echo "ok: dash-nodes-update status; journalctl -u dash-nodes-update"
