#!/usr/bin/env bash
#
# create-claims-room.sh — documented, repeatable creation of the fleet's
# UNENCRYPTED claims room (issue #101).
#
# safehoused's own `create_room` RPC op unconditionally enables
# `m.room.encryption` for every non-space room (D6: "every meaningful
# message goes through the encrypted room") — deliberately, and that
# default is unchanged by this script. The claims room is a narrow,
# documented carve-out: claim payloads (issue numbers, hostnames, TTLs)
# are coordination metadata, not secrets, and E2EE contributes only a
# failure class here — see docs/decisions.md D6's amendment note.
#
# This script bypasses the daemon's RPC entirely: it logs the invoking bot
# account in fresh (no state written to disk — a one-shot admin operation,
# not a daemon), creates a room with no m.room.encryption state, invites
# every fleet bot you list, and prints the resulting room ID for you to
# paste into each host's config explicitly (LOOM_SAFEHOUSE_ROOM_CLAIMS /
# rooms.claims — no alias-resolution magic).
#
# It contains no persistent secrets: credentials are read once (never
# echoed, never left in shell history) and passed to the one-shot Rust
# binary via environment variables, then unset immediately after use.
#
# Run this once per fleet (or whenever recovering from a lost crypto
# store that black-holed the existing claims room) — not on every host.
# Every invited bot's own daemon auto-joins the invite on its next sync
# (see README "Running it" > "Invite the bot to a room").
#
# Usage:  scripts/create-claims-room.sh [--help]

set -euo pipefail

if [ -t 1 ]; then
	C_BOLD=$(printf '\033[1m')
	C_BLUE=$(printf '\033[34m')
	C_GREEN=$(printf '\033[32m')
	C_RESET=$(printf '\033[0m')
else
	C_BOLD=""
	C_BLUE=""
	C_GREEN=""
	C_RESET=""
fi

step() { printf '%s==>%s %s\n' "$C_BLUE$C_BOLD" "$C_RESET" "$*"; }
ok() { printf '%s ok %s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
die() {
	printf 'fail: %s\n' "$*" >&2
	exit 1
}

usage() {
	awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "$0"
	exit 0
}

case "${1:-}" in
-h | --help) usage ;;
"") ;;
*) die "unknown argument: $1 (try --help)" ;;
esac

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

command -v cargo >/dev/null 2>&1 ||
	die "cargo not found — install Rust from https://rustup.rs and re-run."
[ -f "$REPO_ROOT/spikes/create-claims-room/Cargo.toml" ] ||
	die "spikes/create-claims-room/ not found under $REPO_ROOT — run this from a safehouse checkout."

step "Claims room account (the bot account that will own/host the room)"

printf 'Homeserver base URL [https://matrix.example.com]: '
read -r HOMESERVER
HOMESERVER=${HOMESERVER:-https://matrix.example.com}

printf 'Bot username (login only — no @, no :server): '
read -r USERNAME
[ -n "$USERNAME" ] || die "username must not be empty."

printf 'Bot password: '
read -r -s PASSWORD
printf '\n'
[ -n "$PASSWORD" ] || die "password must not be empty."

printf 'Room name [safehouse-claims]: '
read -r ROOM_NAME
ROOM_NAME=${ROOM_NAME:-safehouse-claims}

printf 'Fleet bot Matrix user IDs to invite, space-separated\n'
printf '  (e.g. "@bot-a:example.com @bot-b:example.com"; leave blank to invite nobody now): '
read -r INVITE_LIST

step "Creating unencrypted claims room via a one-shot client (no daemon/RPC involved)"

export CLAIMS_HOMESERVER="$HOMESERVER"
export CLAIMS_USERNAME="$USERNAME"
export CLAIMS_PASSWORD="$PASSWORD"
export CLAIMS_ROOM_NAME="$ROOM_NAME"
export CLAIMS_INVITE="$INVITE_LIST"

STATUS=0
( cd "$REPO_ROOT" && cargo run --quiet -p create-claims-room ) || STATUS=$?

# Scrub secrets from the shell as soon as the binary is done with them.
unset CLAIMS_PASSWORD PASSWORD

[ "$STATUS" -eq 0 ] || die "room creation failed — see output above."

printf '\n'
ok "done — see ROOM_ID above. Paste it into each fleet host's config as"
ok "LOOM_SAFEHOUSE_ROOM_CLAIMS (or rooms.claims), then restart each daemon."
