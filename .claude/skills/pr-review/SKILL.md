---
name: pr-review
description: Copilot's review on a pull request — waiting for it, fixing or declining each finding, answering and resolving every thread, re-requesting once, then checking the pull request merges cleanly onto main. Use straight after `gh pr create`.
---

# Working Copilot's review

The ruleset on `main` requests Copilot's review when the pull request opens.
Copilot only ever comments — it never approves — so the pull request is ready
for the maintainer when it merges cleanly onto `main`, its checks are green,
every review thread is resolved and the closing comment is posted. Never
merge, never enable auto-merge.

At most two rounds. Round 1 is the review the ruleset requested. Round 2 runs
only if round 1 pushed code and its status line was not 🟢 "Approval
recommended". Both rounds fix and push; what bounds the loop is that round 2's
fixes get no third review.

## 1. Wait for the review of the head commit

A review counts only if its commit is the pull request's current head. The
wait is a script, because a session's permission check refuses the same loop
typed inline. Run it in the background, never behind a foreground sleep:

```bash
.claude/skills/pr-review/wait-for-review.sh <pr number>
```

Shell variables do not outlive a call, so each command below sets its own `N`
and `REPO`.

It exits 0 once the review exists and 1 with "No Copilot review" when 15
minutes pass without one. Any other failure is a `gh` error (authentication,
rate limit, network), not a missing review: fix that and run it again rather
than re-requesting. If nothing has come after the 15 minutes, re-request once
(`gh pr edit $N --add-reviewer @copilot`) and run it again. Still nothing:
post a pull request comment saying Copilot never reviewed, tell the
maintainer, stop. That re-request retries a request that went unanswered; it
is allowed once per round and is not a further round. The bound is two
reviews, not two requests.

## 2. Read it

- The review body, whole. Print it and read every line — never grep it, never
  `head` it:

```bash
N=<pr number>; REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner)
HEAD=$(gh pr view $N --json headRefOid -q .headRefOid)
gh api --paginate --slurp "repos/$REPO/pulls/$N/reviews?per_page=100" \
  | jq -r "[.[][] | select(.user.login == \"copilot-pull-request-reviewer[bot]\" and .commit_id == \"$HEAD\")] | last | .body" \
  | perl -pe 's/<picture>.*?alt="([^"]*)".*?<\/picture>/[$1]/g'   # severity icons down to their label
```

  A finding linked as `#discussion_r…` has a thread below. One given as a
  `path:line` with no such link has none and lives only here; a round 2 body
  files these under a collapsed "Previously missed". The "Findings:" line
  is no count of them — it has read "None" above a real finding. The status
  line is the cross-check: under anything but 🟢 there is a finding, so if you
  hold none, read again. "Resolved since last review" needs nothing. List
  every body-only finding before going on; step 8 accounts for each.
- The unresolved threads:

```bash
N=<pr number>; REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner)
gh api graphql --paginate -f query='query($owner:String!,$name:String!,$n:Int!,$endCursor:String){repository(owner:$owner,name:$name){pullRequest(number:$n){reviewThreads(first:100,after:$endCursor){pageInfo{hasNextPage endCursor} nodes{id isResolved path line comments(first:20){nodes{databaseId author{login} body}}}}}}}' \
  -f owner=${REPO%/*} -f name=${REPO#*/} -F n=$N \
  --jq '.data.repository.pullRequest.reviewThreads.nodes[] | select(.isResolved | not)'
```

## 3. Fix or decline each finding

Copilot's review is an opinion, not a verdict. It reads the diff without the
decisions behind it — and, when the pull request carries an OpenSpec change,
without weighing its proposal, design or specs — so it will flag deliberate
choices as mistakes and miss what the change was for. You have that context;
weigh each finding against it and the code, and act only on what survives.
The bias runs the other way too: you just wrote this code, and a finding that
stings is not wrong for stinging. Decline for a reason you can state, never
to defend the work.

Fix a real defect, or a breach of `openspec/config.yaml` `context:`,
`CLAUDE.md` or `.claude/rules/`. Decline when the finding:

- contradicts a decision in the change's `design.md` — cite the decision;
- asks for style the house rules don't hold, or work outside the pull
  request's scope;
- would change behaviour a spec records. That is a change to the change, not a
  review fix: decline it and name it in the closing comment for the
  maintainer.

## 4. Push the fixes

Run the gate as for any commit — `make check` green when the fix touches
code, the manifests or `deny.toml` — then commit with a Conventional Commits
message, staging files by name, and push. New commits only; the one force
push this skill allows is the rebase in step 7.

`main` merges by squash, so the pull request title becomes the commit
release-please reads. If a fix changes what the pull request does, update the
title to match; the `Validate PR title` check holds it to a lowercase
Conventional Commits subject.

## 5. Answer and resolve every thread

Reply to the thread's first comment, then resolve it — one call per thread,
never a shell loop over them, which the permission check refuses:

```bash
N=<pr number>; REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner)
gh api repos/$REPO/pulls/$N/comments/<databaseId>/replies \
  -f body='Fixed in <sha>: <what changed>.'   # or 'Not changing: <reason>.'
gh api graphql -f query='mutation($id:ID!){resolveReviewThread(input:{threadId:$id}){thread{isResolved}}}' -f id=<thread id>
```

Findings in the review body have no thread: answer them together in one pull
request comment.

## 6. Round 2

Skip it when round 1 pushed nothing, or when its status line read 🟢 "Approval
recommended": what that review left open was minor, so its fixes go unreviewed
and the closing comment says one round. Otherwise re-request
(`gh pr edit $N --add-reviewer @copilot`) and repeat steps 1–5 against the new
head: fix what survives step 3, run the gate, push. Request no third review —
round 2's fixes land unreviewed, and the closing comment lists them under
Fixed like any other.

"Left for the maintainer" is not a round-2 verdict. It is only for a finding
step 3 says would change behaviour a spec records: reply "Left for the
maintainer: <why>", resolve, and list it in the closing comment.

## 7. Check it merges

First the delta specs, conflict or not — skip this when the pull request
carries no change under `openspec/changes/`. A change's artifacts ship in the
same pull request as its code, and its delta is applied to `openspec/specs/`
only at archive, after merge. A change archived since this branch was cut may
have rewritten a requirement this change's `MODIFIED` delta copies; the two
live in different files, so git merges them cleanly and the stale copy reverts
the requirement at archive.

```bash
git fetch origin && git diff --stat HEAD...origin/main -- openspec/specs/
```

Nothing listed: nothing to do. Otherwise, for each spec listed that this
change has a delta for, rebuild the delta's `MODIFIED` requirements from
`origin/main`'s current text: start from what `main` now holds, every
scenario included, and re-make this change's own edits on top. Then
`openspec validate <change>`, commit and push as in step 4. If `main` and
this change altered the same behaviour in ways that cannot both hold, that is
a decision: stop and ask the maintainer.

`main` merges by squash, so a branch that is only behind needs nothing more.
A conflict does:

```bash
gh pr view $N --json mergeable,mergeStateStatus   # UNKNOWN means GitHub is still computing: ask again
```

On `CONFLICTING`, rebase onto `origin/main` and resolve the conflicts, repeat
the delta-spec check above, run `make check`, then push with
`git push --force-with-lease` — never a bare `--force`. If a conflict cannot
be resolved without choosing between two behaviours, stop and tell the
maintainer which files conflict.

Wait for the checks to finish on the head (`gh pr checks $N --watch`; without
`--watch` it exits 8 while any check is pending). A red check is yours to fix
as in step 4.

## 8. Close

Confirm no thread is unresolved (the step 2 query returns nothing), every
body-only finding listed in step 2 appears under a heading below, and
`gh pr view $N --json mergeStateStatus` reads `CLEAN`, then post one pull
request comment:

```
Copilot review — <1|2> round(s)
Fixed: <finding> (<sha>) …
Declined: <finding> — <reason> …
Left for the maintainer: <finding> — <why> …
```

Omit an empty heading. Then stop.
