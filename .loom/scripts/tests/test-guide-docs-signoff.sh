#!/usr/bin/env bash
# test-guide-docs-signoff.sh - Regression test for issue #82
#
# create_docs_pr() in the Guide role's document-maintenance phase commits
# automated WORK_LOG/WORK_PLAN/README changes with a single, hardcoded `git
# commit` call. This repo requires a DCO `Signed-off-by:` trailer on every
# commit (an enforced `sign-off` status check). #71 reported the commit was
# missing `--signoff`; PR #73 fixed it in `.loom/roles/guide.md` only. The very
# next commit, `fa2751d` ("chore(loom): commit resynced installed surfaces"),
# silently clobbered that fix by overwriting the file wholesale from the
# upstream Loom template (which carries no `--signoff` call to preserve) —
# and the same file, `.claude/commands/loom/guide.md`, never got the fix in
# the first place. This is #82: same regression, both installed copies.
#
# This suite is a mechanical backstop against a *future* resync (or any other
# edit) silently dropping `--signoff` again: it greps BOTH installed copies
# and fails loudly if either one's create_docs_pr() commit call lacks it.
#
# Hermetic: pure grep against the repo's own installed files. No forge,
# network, or `gh` calls.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo -e "  ${RED}FAIL${NC}: $1"; }

# Both installed copies must exist and be checked independently: unlike
# defaults/roles/*.md in the Loom SOURCE repo (symlinked to
# defaults/.claude/commands/loom/*.md per #5222), a CONSUMER repo's two
# installed copies are plain, independent files kept byte-identical only by
# convention — a fix applied to one does not propagate to the other (this is
# exactly how #82 diverged: PR #73 only touched .loom/roles/guide.md).
GUIDE_MD_FILES=(
    "$REPO_ROOT/.claude/commands/loom/guide.md"
    "$REPO_ROOT/.loom/roles/guide.md"
)

# extract_commit_call FILE - prints the `git ... commit ...` line inside
# create_docs_pr() that carries the "docs: update WORK_LOG" message, or
# nothing if not found.
extract_commit_call() {
    grep -m1 'commit.*-m "docs: update WORK_LOG' "$1" 2>/dev/null
}

echo "Test: create_docs_pr()'s commit call carries --signoff in both installed copies"

any_found=0
for f in "${GUIDE_MD_FILES[@]}"; do
    rel="${f#"$REPO_ROOT"/}"
    if [[ ! -f "$f" ]]; then
        echo "  SKIP: $rel not present in this checkout"
        continue
    fi
    any_found=1

    line="$(extract_commit_call "$f")"
    if [[ -z "$line" ]]; then
        fail "$rel: could not locate create_docs_pr()'s WORK_LOG commit call"
        continue
    fi

    if [[ "$line" == *"--signoff"* ]]; then
        pass "$rel: commit call carries --signoff"
    else
        fail "$rel: commit call is MISSING --signoff (regression of #71/#73, see #82) -- got: $line"
    fi
done

if [[ "$any_found" -eq 0 ]]; then
    echo "SKIP: neither installed guide.md copy is present in this checkout"
    echo ""
    echo "================================"
    echo "Tests run:    $TESTS_RUN"
    exit 0
fi

# The two installed copies are expected to stay byte-identical (both are
# separate resync targets per resync-installed.sh's surface map, but nothing
# should intentionally diverge them) -- not strictly required for the
# --signoff fix itself, but a divergence here is exactly the shape of bug
# that let #82 happen (one copy fixed, the other never touched), so flag it.
if [[ -f "${GUIDE_MD_FILES[0]}" && -f "${GUIDE_MD_FILES[1]}" ]]; then
    if diff -q "${GUIDE_MD_FILES[0]}" "${GUIDE_MD_FILES[1]}" >/dev/null 2>&1; then
        pass "both installed guide.md copies are byte-identical"
    else
        fail "installed guide.md copies have diverged (.claude/commands/loom/guide.md != .loom/roles/guide.md) -- a fix applied to only one will regress the other, exactly as happened in #82"
    fi
fi

echo ""
echo "================================"
echo "Tests run:    $TESTS_RUN"
echo -e "Tests passed: ${GREEN}${TESTS_PASSED}${NC}"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "Tests failed: ${RED}${TESTS_FAILED}${NC}"
    exit 1
fi
echo "All tests passed"
exit 0
