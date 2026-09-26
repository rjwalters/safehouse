#!/usr/bin/env bash
# Install the safehouse agent skill (skills/safehouse/SKILL.md) for Claude
# Code, Codex, pi and opencode (issue #181).
#
#   scripts/install-skill.sh                 user-level: ~/.agents/skills + ~/.claude/skills
#   scripts/install-skill.sh --repo <dir>    into one repo: <dir>/.agents/skills + <dir>/.claude/skills
#   scripts/install-skill.sh --check [...]   exit 1 if any installed copy is missing or stale; change nothing
#
# Two targets cover all four agents: Codex, pi and opencode all discover
# `.agents/skills/<name>/SKILL.md`; Claude Code discovers
# `.claude/skills/<name>/SKILL.md`. The file is copied byte-for-byte to both.
# It is one canonical body in the Repo Skills shared-body sense, and its
# frontmatter is the portable name/description pair every runtime reads.
#
# Copies rather than symlinks so an installed skill survives this checkout
# moving or being deleted; re-run (or --check) after pulling to pick up edits.
#
# This installs instructions only. The agent still needs a reachable daemon
# and a persona in its allowlist; the MCP registration lines for each agent are
# printed at the end, and live in the skill's own "Setup" section.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SRC="$HERE/../skills/safehouse/SKILL.md"
NAME=safehouse

usage() { sed -n '2,8p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

base="$HOME" check=0
while [ $# -gt 0 ]; do
    case "$1" in
        --repo)
            [ $# -ge 2 ] || { echo "install-skill: --repo needs a directory" >&2; exit 2; }
            [ -d "$2" ] || { echo "install-skill: no such directory: $2" >&2; exit 2; }
            base=$(cd "$2" && pwd); shift 2 ;;
        --check) check=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "install-skill: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

[ -f "$SRC" ] || { echo "install-skill: missing $SRC" >&2; exit 2; }

targets=("$base/.agents/skills/$NAME/SKILL.md" "$base/.claude/skills/$NAME/SKILL.md")

if [ "$check" = 1 ]; then
    stale=0
    for t in "${targets[@]}"; do
        if [ ! -f "$t" ]; then echo "missing  $t"; stale=1
        elif ! cmp -s "$SRC" "$t"; then echo "stale    $t"; stale=1
        else echo "current  $t"; fi
    done
    exit "$stale"
fi

for t in "${targets[@]}"; do
    mkdir -p "$(dirname "$t")"
    # rm first: if $t is a symlink into some other checkout, replace the
    # link rather than writing through it.
    rm -f "$t"
    cp "$SRC" "$t"
    echo "installed $t"
done

cat <<'EOF'

Next, per agent (one persona each, [a-z0-9_], listed in the daemon's `personas`):
  Claude Code  claude mcp add --scope user safehouse -e SAFEHOUSED_SOCKET=<sock> -e SAFEHOUSE_PERSONA=claude_code -- safehouse-mcp
  Codex        codex mcp add safehouse --env SAFEHOUSED_SOCKET=<sock> --env SAFEHOUSE_PERSONA=codex -- safehouse-mcp
  opencode     "mcp": {"safehouse": {"type": "local", "command": ["safehouse-mcp"],
                 "environment": {"SAFEHOUSED_SOCKET": "<sock>", "SAFEHOUSE_PERSONA": "opencode"}}}
  pi           no MCP: export SAFEHOUSED_SOCKET=<sock> SAFEHOUSE_PERSONA=pi where pi runs; it uses the CLI
<sock> is <state_dir>/safehoused.sock from the daemon config. Check with: safehouse-mcp status
EOF
