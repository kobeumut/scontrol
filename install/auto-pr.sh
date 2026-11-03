#!/usr/bin/env bash
# Copy a non-secret, managed panel config snapshot into the deployment Git
# repository, commit it on a dedicated branch, push it, and create a PR.
#
# This intentionally stages exactly one allow-listed file. It never commits
# /etc/scontrol/panel-api.env, database URLs, cookie keys, passwords, or any
# arbitrary change already present in the checkout.
set -Eeuo pipefail
IFS=$'\n\t'

readonly REPO_DIR="/opt/scontrol"
readonly SOURCE_CONFIG="/etc/scontrol/agent.toml"
readonly SNAPSHOT_REL="deploy/managed-config/agent.toml"

log() { logger -t scontrol-auto-pr -- "$*"; }
fail_soft() { log "skipped: $*"; exit 0; }

reason="${1:-config-change}"
[[ "$reason" =~ ^[a-z0-9-]{1,48}$ ]] || fail_soft "invalid reason"
[[ -d "${REPO_DIR}/.git" ]] || fail_soft "repository unavailable"
[[ -f "$SOURCE_CONFIG" ]] || fail_soft "agent configuration unavailable"
command -v gh >/dev/null 2>&1 || fail_soft "GitHub CLI unavailable"
gh auth status >/dev/null 2>&1 || fail_soft "GitHub CLI is not authenticated"

# Do not accidentally include an operator's unrelated work in the automated
# commit. A production checkout must be clean before this script changes it.
git -C "$REPO_DIR" status --porcelain --untracked-files=all | grep -q . && \
  fail_soft "repository has unrelated changes"

mkdir -p "${REPO_DIR}/$(dirname "$SNAPSHOT_REL")"
if [[ -f "${REPO_DIR}/${SNAPSHOT_REL}" ]] && cmp -s "$SOURCE_CONFIG" "${REPO_DIR}/${SNAPSHOT_REL}"; then
  exit 0
fi

# Validate the source before copying it into Git. Values are never logged.
if grep -Eiq '(^|_)(password|secret|token|cookie[_-]?key)[[:space:]]*=' "$SOURCE_CONFIG"; then
  fail_soft "source configuration appeared to contain a secret"
fi

branch="scontrol/${reason}-$(date -u +%Y%m%d%H%M%S)"
git -C "$REPO_DIR" switch -c "$branch" >/dev/null 2>&1 || fail_soft "could not create branch"
install -m 0644 "$SOURCE_CONFIG" "${REPO_DIR}/${SNAPSHOT_REL}"
git -C "$REPO_DIR" add -- "$SNAPSHOT_REL"
git -C "$REPO_DIR" commit -m "chore(config): sync managed agent configuration" >/dev/null
git -C "$REPO_DIR" push --set-upstream origin "$branch" >/dev/null

git -C "$REPO_DIR" -c "credential.helper=" status >/dev/null
git -C "$REPO_DIR" rev-parse --show-toplevel >/dev/null
gh pr create \
  --repo "$(git -C "$REPO_DIR" config --get remote.origin.url | sed -E 's#(git@github.com:|https://github.com/)([^/]+/[^/.]+)(\.git)?#\2#')" \
  --head "$branch" \
  --base "${SCONTROL_PR_BASE:-main}" \
  --title "chore(config): sync managed agent configuration" \
  --body "Automated, non-secret snapshot of /etc/scontrol/agent.toml after a panel configuration change." \
  >/dev/null || log "commit pushed but PR creation failed"

log "created configuration review branch ${branch}"
