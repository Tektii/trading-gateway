#!/usr/bin/env bash
# Waits up to 15 minutes for Copilot's review of a pull request's current head.
# Exits 0 once it exists, 1 if it never came.
set -euo pipefail
N=$1
REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner)
HEAD=$(gh pr view "$N" --json headRefOid -q .headRefOid)
for _ in $(seq 15); do
  if gh api "repos/$REPO/pulls/$N/reviews?per_page=100" \
    --jq "[.[] | select(.user.login == \"copilot-pull-request-reviewer[bot]\" and .commit_id == \"$HEAD\")] | length > 0" \
    | grep -q true; then
    echo "Copilot reviewed $HEAD"
    exit 0
  fi
  sleep 60
done
echo "No Copilot review of $HEAD after 15 minutes"
exit 1
