#!/usr/bin/env bash
# Waits up to 15 minutes for Copilot's review of a pull request's current head.
# Exits 0 once it exists, 1 if it never came. A failed `gh` call aborts at once
# with gh's own error, so an API failure is never mistaken for a missing review.
set -euo pipefail
N=$1
REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner)
for _ in $(seq 15); do
  # Read the head each time: a push during the wait moves it.
  HEAD=$(gh pr view "$N" --json headRefOid -q .headRefOid)
  # One line per page of reviews: true if that page holds a review of HEAD.
  found=$(gh api --paginate "repos/$REPO/pulls/$N/reviews?per_page=100" \
    --jq "[.[] | select(.user.login == \"copilot-pull-request-reviewer[bot]\" and .commit_id == \"$HEAD\")] | length > 0")
  if grep -q true <<<"$found"; then
    echo "Copilot reviewed $HEAD"
    exit 0
  fi
  sleep 60
done
echo "No Copilot review of the head ($HEAD) after 15 minutes"
exit 1
