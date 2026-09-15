#!/usr/bin/env bash
#
# deprovision-host.sh — decommission a fleet host's Matrix bot account via
# admin-room automation (issue #94's teardown counterpart to
# provision-host.sh).
#
# Sends `users deactivate <bot>` as the `@safehouse-admin` server-admin bot
# into the Matrix admin room, so ephemeral/decommissioned hosts do not leave
# a dead account behind. Tuwunel mirrors Synapse's admin-API deactivate
# semantics, which force-leave every room the account was a member of as
# part of deactivation — so this also retires the account's fleet-room
# membership, without a separate kick/leave step. Verify against your
# server's actual behavior (e.g. the fleet room's member list) if you rely
# on that for anything more than tidiness.
#
# Credentials are read from the environment at runtime and never baked in:
#
#   SAFEHOUSE_ADMIN_HOMESERVER   homeserver base URL
#   SAFEHOUSE_ADMIN_USERNAME     @safehouse-admin's login
#   SAFEHOUSE_ADMIN_PASSWORD     @safehouse-admin's password
#   SAFEHOUSE_ADMIN_ROOM         admin room id or #alias
#
# Usage:  scripts/deprovision-host.sh --host <name> [--help]
#         scripts/deprovision-host.sh --user <bot-login> [--help]
#
# --host derives the bot login as `safehoused-<name>` (the same convention
# provision-host.sh uses); --user names the bot login directly, for
# accounts that predate that convention or were minted with an explicit
# override.

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

HOST=""
USER_OVERRIDE=""
while [ $# -gt 0 ]; do
	case "$1" in
	-h | --help) usage ;;
	--host)
		HOST=${2:-}
		[ -n "$HOST" ] || die "--host requires a value"
		shift 2
		;;
	--user)
		USER_OVERRIDE=${2:-}
		[ -n "$USER_OVERRIDE" ] || die "--user requires a value"
		shift 2
		;;
	*) die "unknown argument: $1 (try --help)" ;;
	esac
done
[ -n "$HOST" ] || [ -n "$USER_OVERRIDE" ] || die "one of --host <name> or --user <bot-login> is required"

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

TARGET_DESC=${USER_OVERRIDE:-"host '$HOST'"}
step "Deactivating the bot account for $TARGET_DESC via the admin room"

export PROVISION_HOMESERVER="$SAFEHOUSE_ADMIN_HOMESERVER"
export PROVISION_ADMIN_USERNAME="$SAFEHOUSE_ADMIN_USERNAME"
export PROVISION_ADMIN_PASSWORD="$SAFEHOUSE_ADMIN_PASSWORD"
export PROVISION_ADMIN_ROOM="$SAFEHOUSE_ADMIN_ROOM"
export PROVISION_HOST="${HOST:-unused}"
if [ -n "$USER_OVERRIDE" ]; then
	export PROVISION_NEW_USERNAME="$USER_OVERRIDE"
fi

STATUS=0
( cd "$REPO_ROOT" && cargo run --quiet -p provision-host -- deactivate ) || STATUS=$?

unset SAFEHOUSE_ADMIN_PASSWORD PROVISION_ADMIN_PASSWORD

[ "$STATUS" -eq 0 ] || die "deactivation failed — see output above."

printf '\n'
ok "done — the account is deactivated. If it did not already leave the fleet room as part of"
ok "deactivation, remove it from the room's member list by hand."
