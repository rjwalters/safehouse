#!/usr/bin/env bash
#
# provision-host.sh — unattended fleet-host onboarding via Matrix
# admin-room automation (issue #94).
#
# Mints a new bot account for a fleet host with NO human action on the
# homeserver: it sends `users create_user <bot> <generated-password>` as
# the `@safehouse-admin` server-admin bot into the Matrix admin room (the
# zero-downtime "run it in the admin room" path documented in
# docs/research/2026-07-26-homeserver.md), waits best-effort for an ack,
# then (if an already-onboarded host's daemon socket is available) sends
# the existing `invite` op so the new bot's daemon auto-joins the fleet
# room on its next sync.
#
# `allow_registration = false` stays untouched — this never opens
# registration; it drives the same admin-room command an operator would
# type by hand.
#
# Credentials are read from the environment at runtime and never baked in,
# committed, or left in shell history:
#
#   SAFEHOUSE_ADMIN_HOMESERVER   homeserver base URL (e.g. https://matrix.example.com)
#   SAFEHOUSE_ADMIN_USERNAME     @safehouse-admin's login (no @, no :server)
#   SAFEHOUSE_ADMIN_PASSWORD     @safehouse-admin's password
#   SAFEHOUSE_ADMIN_ROOM         admin room id or #alias (e.g. #admins:example.com)
#
# Optional, to also complete the room-join half in one pass (requires an
# already-onboarded host's daemon socket — this cannot come from the new
# host itself, which is not a room member yet):
#
#   SAFEHOUSE_INVITE_SOCKET      an onboarded host's SAFEHOUSED_SOCKET
#   SAFEHOUSE_INVITE_PERSONA     persona to send the invite op as (default: operator)
#   SAFEHOUSE_FLEET_ROOM         fleet room id/alias to invite the new bot into
#
# If SAFEHOUSE_INVITE_SOCKET/SAFEHOUSE_FLEET_ROOM are not set, the account
# is still minted — this script prints the manual `safehouse-mcp invite`
# command to finish the room-join step (README "Running it" step 4).
#
# Usage:  scripts/provision-host.sh --host <name> [--help]

set -euo pipefail

if [ -t 1 ]; then
	C_BOLD=$(printf '\033[1m')
	C_BLUE=$(printf '\033[34m')
	C_GREEN=$(printf '\033[32m')
	C_YELLOW=$(printf '\033[33m')
	C_RESET=$(printf '\033[0m')
else
	C_BOLD=""
	C_BLUE=""
	C_GREEN=""
	C_YELLOW=""
	C_RESET=""
fi

step() { printf '%s==>%s %s\n' "$C_BLUE$C_BOLD" "$C_RESET" "$*"; }
ok() { printf '%s ok %s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn() { printf '%swarn%s %s\n' "$C_YELLOW" "$C_RESET" "$*" >&2; }
die() {
	printf 'fail: %s\n' "$*" >&2
	exit 1
}

usage() {
	awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "$0"
	exit 0
}

HOST=""
while [ $# -gt 0 ]; do
	case "$1" in
	-h | --help) usage ;;
	--host)
		HOST=${2:-}
		[ -n "$HOST" ] || die "--host requires a value"
		shift 2
		;;
	*) die "unknown argument: $1 (try --help)" ;;
	esac
done
[ -n "$HOST" ] || die "--host <name> is required (e.g. --host studio)"

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

command -v cargo >/dev/null 2>&1 ||
	die "cargo not found — install Rust from https://rustup.rs and re-run."
[ -f "$REPO_ROOT/spikes/provision-host/Cargo.toml" ] ||
	die "spikes/provision-host/ not found under $REPO_ROOT — run this from a safehouse checkout."

: "${SAFEHOUSE_ADMIN_HOMESERVER:?SAFEHOUSE_ADMIN_HOMESERVER must be set}"
: "${SAFEHOUSE_ADMIN_USERNAME:?SAFEHOUSE_ADMIN_USERNAME must be set}"
: "${SAFEHOUSE_ADMIN_PASSWORD:?SAFEHOUSE_ADMIN_PASSWORD must be set}"
: "${SAFEHOUSE_ADMIN_ROOM:?SAFEHOUSE_ADMIN_ROOM must be set}"

step "Minting a bot account for host '$HOST' via the admin room (no human on the homeserver)"

export PROVISION_HOMESERVER="$SAFEHOUSE_ADMIN_HOMESERVER"
export PROVISION_ADMIN_USERNAME="$SAFEHOUSE_ADMIN_USERNAME"
export PROVISION_ADMIN_PASSWORD="$SAFEHOUSE_ADMIN_PASSWORD"
export PROVISION_ADMIN_ROOM="$SAFEHOUSE_ADMIN_ROOM"
export PROVISION_HOST="$HOST"

OUT=$(mktemp "${TMPDIR:-/tmp}/provision-host.XXXXXX")
trap 'rm -f "$OUT"' EXIT

STATUS=0
( cd "$REPO_ROOT" && cargo run --quiet -p provision-host -- create ) | tee "$OUT" || STATUS=$?

unset SAFEHOUSE_ADMIN_PASSWORD PROVISION_ADMIN_PASSWORD

[ "$STATUS" -eq 0 ] || die "account creation failed — see output above."

NEW_USERNAME=$(grep -E '^USERNAME=' "$OUT" | tail -1 | cut -d= -f2-)
[ -n "$NEW_USERNAME" ] || die "could not find USERNAME in provision-host output."

if [ -n "${SAFEHOUSE_INVITE_SOCKET:-}" ] && [ -n "${SAFEHOUSE_FLEET_ROOM:-}" ]; then
	step "Inviting @$NEW_USERNAME to the fleet room via an already-onboarded host's socket"
	SERVER=${SAFEHOUSE_ADMIN_HOMESERVER#*://}
	NEW_USER_ID="@${NEW_USERNAME}:${SERVER}"
	INVITE_STATUS=0
	(
		export SAFEHOUSED_SOCKET="$SAFEHOUSE_INVITE_SOCKET"
		export SAFEHOUSE_PERSONA="${SAFEHOUSE_INVITE_PERSONA:-operator}"
		cd "$REPO_ROOT" && cargo run --quiet -p safehouse-mcp -- invite \
			--room "$SAFEHOUSE_FLEET_ROOM" --user "$NEW_USER_ID"
	) || INVITE_STATUS=$?
	if [ "$INVITE_STATUS" -eq 0 ]; then
		ok "invited $NEW_USER_ID to $SAFEHOUSE_FLEET_ROOM"
	else
		warn "invite failed — invite $NEW_USER_ID to the fleet room manually (see below)."
	fi
else
	warn "SAFEHOUSE_INVITE_SOCKET/SAFEHOUSE_FLEET_ROOM not set — account minted, but the new"
	warn "host still needs an invite. From an already-onboarded host's socket, run:"
	warn "  safehouse-mcp invite --room <fleet-room> --user @${NEW_USERNAME}:<your-server>"
fi

printf '\n'
ok "done — deliver USERNAME=$NEW_USERNAME / PASSWORD=<see above> to host '$HOST' out of band."
ok "That host's scripts/install.sh consumes them as its username/password config fields."
