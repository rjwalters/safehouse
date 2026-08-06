#!/usr/bin/env bash
# check-work-log-diff.sh - Mechanically block a Guide docs-maintenance commit
# that stages an excluded WORK_LOG.md entry (issue #76).
#
# #5454 fixed update_work_log()'s `new_prs` query with a GUIDE_DOCS_PR_EXCLUDE
# jq filter so the phase's own `docs: Guide document maintenance update` PRs
# never became "new content" for the next tick (an unbounded self-referential
# loop otherwise). PR #75 (issue #76) proved that filter alone is not enough:
# it only runs inside `update_work_log()`'s query. An agent reasoning at the
# semantic level ("this PR couldn't log itself last tick, I'll add it now")
# can still hand-stage an excluded entry into WORK_LOG.md through a different
# code path, entirely bypassing the generator's jq filter.
#
# This script is the structural fix: it inspects the FINAL STAGED diff of
# WORK_LOG.md — not the generator's internal query output — for every newly
# added `- **PR #N**: ...` line, re-fetches PR #N's actual headRefName/title
# via `gh pr view` (the rendered WORK_LOG.md line only has title text, not the
# branch name the exclusion also matches on), and evaluates the SAME
# GUIDE_DOCS_PR_EXCLUDE expression against it. Any match aborts the check
# (exit 1) regardless of how the line got staged.
#
# Usage:
#   check-work-log-diff.sh <worktree-path> <exclude-jq-expr>
#
# <worktree-path>    path to the git worktree with WORK_LOG.md staged
#                     (git -C <worktree-path> diff --cached -- WORK_LOG.md)
# <exclude-jq-expr>  the GUIDE_DOCS_PR_EXCLUDE jq boolean expression, passed in
#                     verbatim by the caller (guide.md's create_docs_pr()) so
#                     this script never carries its own copy to drift out of
#                     sync with the single-line assignment in guide.md that
#                     the regression suite also extracts verbatim.
#
# Exit codes:
#   0 - staged WORK_LOG.md diff is clean: no excluded PR entry found
#   1 - an excluded PR entry is staged (or a PR's status could not be
#       verified) — the caller MUST NOT commit, push, or create a PR
#   2 - usage/environment error (missing args, jq/gh unavailable)
#
# Fail-closed on `gh pr view` failure: if a staged PR number can't be
# re-verified (API error, rate limit, PR not found), this script treats it as
# excluded rather than silently letting an unverifiable entry through. A
# false positive here costs one skipped docs-maintenance tick; a false
# negative reintroduces the #5454/#76 self-referential loop.

set -uo pipefail

WT="${1:-}"
EXCLUDE_EXPR="${2:-}"

if [[ -z "$WT" || -z "$EXCLUDE_EXPR" ]]; then
  echo "usage: check-work-log-diff.sh <worktree-path> <exclude-jq-expr>" >&2
  exit 2
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "check-work-log-diff.sh: jq not found" >&2
  exit 2
fi

GH_BIN="${CHECK_WORK_LOG_DIFF_GH:-gh}"
if ! command -v "$GH_BIN" >/dev/null 2>&1; then
  echo "check-work-log-diff.sh: gh not found" >&2
  exit 2
fi

if [[ ! -d "$WT" ]]; then
  echo "check-work-log-diff.sh: worktree path not found: $WT" >&2
  exit 2
fi

# Newly-added `- **PR #N**: ...` lines from the STAGED diff of WORK_LOG.md.
# `git diff --cached` prefixes added lines with a single `+`; the `+++
# b/WORK_LOG.md` file-header line also starts with `+` but never matches the
# `- **PR #N**` body pattern, so no extra exclusion is needed for it.
ADDED_PR_NUMBERS="$(git -C "$WT" diff --cached -- WORK_LOG.md 2>/dev/null \
  | grep -E '^\+- \*\*PR #[0-9]+\*\*' \
  | grep -oE 'PR #[0-9]+' \
  | grep -oE '[0-9]+' \
  | sort -un)"

if [[ -z "$ADDED_PR_NUMBERS" ]]; then
  echo "check-work-log-diff.sh: no newly-added PR lines in staged WORK_LOG.md diff — clean"
  exit 0
fi

FOUND_EXCLUDED=0
while IFS= read -r pr_num; do
  [[ -z "$pr_num" ]] && continue

  pr_json="$("$GH_BIN" pr view "$pr_num" --json headRefName,title 2>/dev/null)"
  if [[ -z "$pr_json" ]]; then
    echo "check-work-log-diff.sh: EXCLUDED_PR #$pr_num — could not verify via 'gh pr view' (failing closed)" >&2
    FOUND_EXCLUDED=1
    continue
  fi

  if printf '%s\n' "$pr_json" | jq -e "($EXCLUDE_EXPR)" >/dev/null 2>&1; then
    title="$(printf '%s' "$pr_json" | jq -r '.title // ""')"
    echo "check-work-log-diff.sh: EXCLUDED_PR #$pr_num matches GUIDE_DOCS_PR_EXCLUDE (title: \"$title\")" >&2
    FOUND_EXCLUDED=1
  fi
done <<<"$ADDED_PR_NUMBERS"

if [[ "$FOUND_EXCLUDED" -eq 1 ]]; then
  echo "check-work-log-diff.sh: staged WORK_LOG.md diff contains an excluded entry — abort" >&2
  exit 1
fi

echo "check-work-log-diff.sh: all staged PR entries verified clean"
exit 0
