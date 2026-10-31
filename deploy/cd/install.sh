#!/bin/sh
# Installs or updates dash-nodes-update (update.sh) and its systemd timer, as root on the stack's host.
# Safe to re-run; update.sh runs it itself when a deploy changes deploy/cd/. Files that hold this
# host's settings and trust are created once and then left alone:
#   /etc/dash-nodes-cd/env              settings (from env.example)
#   /etc/dash-nodes-cd/allowed_signers  SSH keys whose commits deploy: GITHUB_USER's signing keys
#                                       on GitHub at install time
#   /etc/dash-nodes-cd/gnupg/           GPG keyring: GitHub's web-flow key (squash merges), pinned
#   /var/lib/dash-nodes-cd/             mirror of the repo; app-commit and worker-commit, what's
#                                       deployed (from APP_DIR/DEPLOYED_COMMIT, or APP_COMMIT and
#                                       WORKER_COMMIT)
# Check the fingerprints it prints. The timer is enabled on the first install only, so a later run
# doesn't undo `systemctl disable`.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
CONF=/etc/dash-nodes-cd
LIB=/usr/local/lib/dash-nodes-cd
UNITS=/etc/systemd/system
GITHUB_USER=${GITHUB_USER:-PastaPastaPasta}
# GitHub's web-flow commit signing key since 2024-01-16 (https://github.com/web-flow.gpg).
WEB_FLOW_FPR=968479A1AFF927E37D1A566BB5690EEEBB952194

# Copies $1 to $2 with mode $3, replacing $2 in one rename (a running update.sh keeps its old copy).
put() { cp "$1" "$2.new" && chmod "$3" "$2.new" && mv -f "$2.new" "$2"; }

install -d -m 0700 "$CONF"
install -d -m 0755 "$LIB"
[ -e "$CONF/env" ] || put "$here/env.example" "$CONF/env" 0600
# shellcheck source=/dev/null
. "$CONF/env"
STATE=${STATE:-/var/lib/dash-nodes-cd}
APP_DIR=${APP_DIR:-/opt/dash-nodes}
install -d -m 0700 "$STATE"

if [ ! -s "$CONF/allowed_signers" ]; then
  curl -fsS "https://api.github.com/users/$GITHUB_USER/ssh_signing_keys" \
    | python3 -c 'import json,sys; print("".join("* namespaces=\"git\" " + k["key"] + "\n" for k in json.load(sys.stdin)), end="")' \
    > "$CONF/allowed_signers.new"
  [ -s "$CONF/allowed_signers.new" ] || { echo "no SSH signing keys for $GITHUB_USER" >&2; exit 1; }
  mv -f "$CONF/allowed_signers.new" "$CONF/allowed_signers"
  echo "allowed_signers ($GITHUB_USER's SSH signing keys):"
  cut -d' ' -f3- "$CONF/allowed_signers" | while read -r k; do echo "$k" | ssh-keygen -lf -; done
fi

if ! GNUPGHOME="$CONF/gnupg" gpg --batch --list-keys "$WEB_FLOW_FPR" >/dev/null 2>&1; then
  install -d -m 0700 "$CONF/gnupg"
  curl -fsS https://github.com/web-flow.gpg | GNUPGHOME="$CONF/gnupg" gpg --batch --quiet --import
  GNUPGHOME="$CONF/gnupg" gpg --batch --list-keys "$WEB_FLOW_FPR" >/dev/null
  echo "$WEB_FLOW_FPR:6:" | GNUPGHOME="$CONF/gnupg" gpg --batch --quiet --import-ownertrust
  echo "gnupg: trusts GitHub web-flow $WEB_FLOW_FPR"
fi

if [ ! -d "$STATE/repo.git" ]; then
  git init --quiet --bare "$STATE/repo.git"
  git -C "$STATE/repo.git" remote add origin "${REPO_URL:-https://github.com/PastaPastaPasta/dash-nodes.git}"
fi
git -C "$STATE/repo.git" fetch --quiet origin "+refs/heads/${BRANCH:-main}:refs/remotes/origin/${BRANCH:-main}"
# What's deployed: $2, else APP_DIR/DEPLOYED_COMMIT, into state file $1 (if it isn't set yet).
deployed() {
  [ ! -s "$STATE/$1" ] || return 0
  git -C "$STATE/repo.git" rev-parse --verify "${2:-$(cat "$APP_DIR/DEPLOYED_COMMIT")}^{commit}" > "$STATE/$1"
  echo "$1: $(cat "$STATE/$1")"
}
deployed app-commit "${APP_COMMIT:-}"
deployed worker-commit "${WORKER_COMMIT:-}"

put "$here/update.sh" "$LIB/update.sh" 0755
ln -sfn "$LIB/update.sh" /usr/local/sbin/dash-nodes-update
first=; [ -e "$UNITS/dash-nodes-update.timer" ] || first=1
put "$here/dash-nodes-update.service" "$UNITS/dash-nodes-update.service" 0644
put "$here/dash-nodes-update.timer" "$UNITS/dash-nodes-update.timer" 0644
systemctl daemon-reload
if [ -n "$first" ]; then
  systemctl enable --now dash-nodes-update.timer
  echo "enabled dash-nodes-update.timer"
fi
echo "ok: dash-nodes-update status; journalctl -u dash-nodes-update"
