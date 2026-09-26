#!/usr/bin/env bash
# Tests for scripts/install-skill.sh and the skill it installs (issue #181).
# Hermetic: installs into a temp HOME / temp repo, touches nothing real.
#
#   scripts/test-install-skill.sh
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
INSTALL="$HERE/install-skill.sh"
SRC="$HERE/../skills/safehouse/SKILL.md"
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "ok - $*"; }

# --- the skill itself: portable frontmatter ---------------------------------
# Codex, pi and opencode reject a skill whose `name` is not lowercase
# [a-z0-9-] or does not match its directory, or that has no description.
[ "$(sed -n 1p "$SRC")" = "---" ] || fail "SKILL.md must open with frontmatter"
fm=$(awk 'NR==1{next} /^---$/{exit} {print}' "$SRC")
name=$(sed -n 's/^name: *//p' <<<"$fm")
desc=$(sed -n 's/^description: *//p' <<<"$fm")
[ "$name" = "$(basename "$(dirname "$SRC")")" ] || fail "name '$name' != directory name"
[[ "$name" =~ ^[a-z0-9-]{1,64}$ ]] || fail "name '$name' not [a-z0-9-]{1,64}"
[ -n "$desc" ] && [ "${#desc}" -le 1024 ] || fail "description missing or > 1024 chars"
pass "frontmatter portable (name=$name, description ${#desc} chars)"

# --- every CLI subcommand the skill tells an agent to run exists ------------
for sub in check read send list-rooms status; do
    grep -q "safehouse-mcp $sub" "$SRC" || continue
    grep -q "\"$sub\" => build_" "$HERE/../safehouse-mcp/src/main.rs" || fail "skill documents unknown subcommand: $sub"
done
while read -r tool; do
    grep -q "\"name\": \"$tool\"" "$HERE/../safehouse-mcp/src/main.rs" || fail "skill documents unknown MCP tool: $tool"
done < <(grep -o 'safehouse_[a-z][a-z_]*[a-z]' "$SRC" | sort -u)
pass "documented subcommands and MCP tools exist in safehouse-mcp"

# --- user-level install -----------------------------------------------------
HOME="$T/home" "$INSTALL" >/dev/null
for d in .agents .claude; do
    cmp -s "$SRC" "$T/home/$d/skills/safehouse/SKILL.md" || fail "user install: $d copy differs"
done
HOME="$T/home" "$INSTALL" --check >/dev/null || fail "--check after install should pass"
pass "user-level install to .agents and .claude; --check clean"

# --- drift is detected, and a reinstall fixes it -----------------------------
echo "local edit" >>"$T/home/.claude/skills/safehouse/SKILL.md"
out=$(HOME="$T/home" "$INSTALL" --check) && fail "--check missed a stale copy"
grep -q "^stale .*\.claude" <<<"$out" || fail "--check did not name the stale copy: $out"
HOME="$T/home" "$INSTALL" >/dev/null
HOME="$T/home" "$INSTALL" --check >/dev/null || fail "reinstall did not fix drift"
pass "--check reports a stale copy; reinstall fixes it"

# --- a symlinked target is replaced, not written through ----------------------
echo "someone else's file" >"$T/other.md"
rm "$T/home/.agents/skills/safehouse/SKILL.md"
ln -s "$T/other.md" "$T/home/.agents/skills/safehouse/SKILL.md"
HOME="$T/home" "$INSTALL" >/dev/null
[ "$(cat "$T/other.md")" = "someone else's file" ] || fail "install wrote through a symlink"
[ ! -L "$T/home/.agents/skills/safehouse/SKILL.md" ] || fail "symlink not replaced"
pass "symlinked target replaced, its target untouched"

# --- repo install, and argument errors ---------------------------------------
mkdir -p "$T/repo"
HOME="$T/home2" "$INSTALL" --repo "$T/repo" >/dev/null
[ -f "$T/repo/.agents/skills/safehouse/SKILL.md" ] && [ -f "$T/repo/.claude/skills/safehouse/SKILL.md" ] || fail "--repo install"
[ ! -e "$T/home2/.agents" ] || fail "--repo also wrote to HOME"
if "$INSTALL" --repo "$T/nope" 2>/dev/null; then fail "--repo on a missing dir should fail"; fi
if "$INSTALL" --bogus 2>/dev/null; then fail "unknown flag should fail"; fi
pass "--repo installs only into the repo; bad arguments fail"

echo "all passed"
