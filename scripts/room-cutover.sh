#!/usr/bin/env bash
#
# room-cutover.sh — scripted room-DAG-death cutover (issue #172, following #169).
#
# Two narration rooms have now died the same way: a homeserver-side
# "cannot create a non-create event in a room with no forward extremities"
# wedge that no client can repair (see the dead-room registry below). When it
# happens a third time, the fix is not a hand-run incident response — it is
# this script.
#
# This is deliberately NOT the same shape as scripts/create-claims-room.sh
# under the hood: the claims room bypasses safehoused's RPC entirely because
# it needs the unencrypted carve-out (D6 amendment). A cutover replacement
# room wants the normal, always-encrypted room `create_room` already gives
# you, so this script drives that RPC over an already-running daemon's unix
# socket instead of logging in a fresh client — see docs/protocol/envelope-v1.md
# and README "Scripting the socket" for the wire format `safehouse-mcp`
# implements.
#
# What it does, against ONE already-onboarded host's daemon socket:
#   1. create_room — creates the replacement room (always encrypted) and
#      invites every publisher/consumer identity you pass, in the same call
#      (safehoused's create_room op takes an `invite` array — the same
#      mechanism #169's incident response used). Every invited bot's own
#      daemon auto-joins on its next sync.
#   2. Prints the new room ID and the full rollout runbook: per-host env
#      layer instructions (launchd vs systemd), the egress-allowlist step,
#      and the LOOM_SAFEHOUSE_RECONCILE_MAX_AGE_SECS duplicate-burst lever.
#
# It reads NO baked secrets or identities: the hosting persona/socket come
# from the SAFEHOUSED_SOCKET / SAFEHOUSE_PERSONA env vars already documented
# in README "Scripting the socket" (unchanged from how every other
# safehouse-mcp invocation is configured), and the room name / invite list
# are passed explicitly per run — the fleet roster lives with the operator,
# not in this repo.
#
# A user id that fails Matrix user-id validation aborts create_room entirely
# (before any room is created) — safe to just fix the list and re-run.
#
# Usage:
#   scripts/room-cutover.sh --room-name NAME --invite "@bot-a:example.com @bot-b:example.com" [--dry-run]
#   scripts/room-cutover.sh --dry-run                 # renders the full plan with sample data, sends nothing
#
# Flags:
#   --room-name NAME   Name for the replacement room (required unless --dry-run).
#   --invite LIST      Space-separated Matrix user IDs to invite — the known
#                       publisher/consumer identities (required unless --dry-run).
#   --dry-run          Render every command that would run, without opening the
#                       daemon socket. Missing --room-name/--invite fall back to
#                       clearly-marked sample values so this is safe to run with
#                       no arguments at all (this is what CI exercises).
#   -h, --help         Show this help.
#
# Requires: jq (to build/parse the JSON-RPC frames — needed even for
# --dry-run, since it renders the exact frame that would be sent) and, for a
# live run only, cargo (to run the safehouse-mcp shim).

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
warn() { printf '%swarn%s %s\n' "$C_YELLOW" "$C_RESET" "$*"; }
die() {
	printf 'fail: %s\n' "$*" >&2
	exit 1
}

usage() {
	awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "$0"
	exit 0
}

DRY_RUN=0
ROOM_NAME=""
INVITE_LIST=""

while [ $# -gt 0 ]; do
	case "$1" in
	-h | --help) usage ;;
	--dry-run)
		DRY_RUN=1
		shift
		;;
	--room-name)
		[ $# -ge 2 ] || die "--room-name requires a value"
		ROOM_NAME="$2"
		shift 2
		;;
	--invite)
		[ $# -ge 2 ] || die "--invite requires a value"
		INVITE_LIST="$2"
		shift 2
		;;
	*) die "unknown argument: $1 (try --help)" ;;
	esac
done

if [ "$DRY_RUN" -eq 1 ]; then
	if [ -z "$ROOM_NAME" ]; then
		ROOM_NAME="loom-fleet-v4"
		warn "--room-name not given — using sample value '$ROOM_NAME' for this dry run"
	fi
	if [ -z "$INVITE_LIST" ]; then
		INVITE_LIST="@safehouse-bot:example.com @safehoused-worker1:example.com"
		warn "--invite not given — using sample identities for this dry run"
	fi
else
	[ -n "$ROOM_NAME" ] || die "--room-name is required (or pass --dry-run)"
	[ -n "$INVITE_LIST" ] || die "--invite is required (or pass --dry-run)"
fi

command -v jq >/dev/null 2>&1 ||
	die "jq not found — install it (e.g. 'brew install jq' / 'apt install jq') and re-run."

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

# The recommended duplicate-burst lever (see the runbook section below for
# why): well below the 7-day default so a long-dark publisher's reconnect
# reconciliation only re-narrates a bounded recent window instead of days of
# backlog. Matches the value #169's cutover applied on most of the fleet (one
# host was already more conservative at 14400s / 4h, which is also fine — the
# goal is "well below 604800", not this exact number).
RECOMMENDED_RECONCILE_MAX_AGE_SECS=3600

# ---------------------------------------------------------------------------
# Step 1: create_room (with invite) — over the socket.
# ---------------------------------------------------------------------------

# Builds the `invite` array as a JSON value from the space-separated list.
build_invite_json() {
	# shellcheck disable=SC2086 # word-splitting the space-separated list is intended
	jq -n --args '$ARGS.positional' -- $INVITE_LIST
}

# Builds one JSON-RPC 2.0 tools/call frame for safehouse-mcp's stdio MCP
# server (see README "Scripting the socket" / safehouse-mcp/src/main.rs).
build_create_room_call() {
	local args_json
	args_json=$(jq -n --arg name "$ROOM_NAME" --argjson invite "$(build_invite_json)" '{name: $name, invite: $invite}')
	jq -n --argjson args "$args_json" \
		'{jsonrpc: "2.0", id: 1, method: "tools/call", params: {name: "safehouse_create_room", arguments: $args}}'
}

CREATE_FRAME=$(build_create_room_call)
ROOM_ID=""

if [ "$DRY_RUN" -eq 1 ]; then
	step "[dry-run] create_room (with invite) — would send over SAFEHOUSED_SOCKET as \$SAFEHOUSE_PERSONA:"
	printf '%s\n' "$CREATE_FRAME"
	ROOM_ID="!DRY-RUN-PLACEHOLDER-ROOM-ID:example.com"
	ok "[dry-run] would create room '$ROOM_NAME' inviting: $INVITE_LIST"
else
	command -v cargo >/dev/null 2>&1 ||
		die "cargo not found — install Rust from https://rustup.rs and re-run."
	[ -n "${SAFEHOUSED_SOCKET:-}" ] ||
		die "SAFEHOUSED_SOCKET must be set — the running daemon's socket path (see README \"Scripting the socket\")."
	[ -n "${SAFEHOUSE_PERSONA:-}" ] ||
		die "SAFEHOUSE_PERSONA must be set — the persona this script authenticates as."

	step "Creating replacement room '$ROOM_NAME' via $SAFEHOUSED_SOCKET as $SAFEHOUSE_PERSONA, inviting: $INVITE_LIST"
	CREATE_REPLY=$(printf '%s' "$CREATE_FRAME" | (cd "$REPO_ROOT" && cargo run --quiet -p safehouse-mcp))
	CREATE_IS_ERROR=$(printf '%s' "$CREATE_REPLY" | jq -r '.result.isError // false')
	[ "$CREATE_IS_ERROR" = "false" ] ||
		die "create_room failed: $(printf '%s' "$CREATE_REPLY" | jq -r '.result.content[0].text // .')"
	CREATE_TEXT=$(printf '%s' "$CREATE_REPLY" | jq -r '.result.content[0].text')
	ROOM_ID=$(printf '%s' "$CREATE_TEXT" | jq -r '.room_id')
	if [ -z "$ROOM_ID" ] || [ "$ROOM_ID" = "null" ]; then
		die "create_room reply carried no room_id: $CREATE_TEXT"
	fi
	ok "created $ROOM_ID"
fi

# ---------------------------------------------------------------------------
# Step 2: the room ID and the full rollout runbook.
# ---------------------------------------------------------------------------

printf '\n'
step "New room: $ROOM_ID"
printf '\n'

cat <<EOF
${C_BOLD}Rollout — per-host env layer (env overrides config; do the right layer per host)${C_RESET}

Every host addresses the room by ID ONLY. Never an alias or name lookup —
both dead rooms in the registry below can never be left, so name/alias
resolution is permanently ambiguous on this homeserver.

Set on EVERY host in the fleet (publishers AND consumers):

    export LOOM_SAFEHOUSE_ROOM='$ROOM_ID'

${C_BOLD}launchd hosts (macOS)${C_RESET} — a plain \`restart\` does not re-read the plist.
Re-render the plist with the new env first (loom-daemon-start.sh, or hand-edit
the EnvironmentVariables block), THEN reload:

    # preferred, if this daemon build has it (added after #169's incident;
    # older builds — e.g. 0.18.121 — predate the flag entirely):
    loom-daemon restart --reload-supervisor

    # always works, and is what #169 had to fall back to on one host where
    # --reload-supervisor left the OLD process running with stale env:
    launchctl bootout gui/\$(id -u)/com.rjwalters.loom-daemon
    launchctl bootstrap gui/\$(id -u) ~/Library/LaunchAgents/com.rjwalters.loom-daemon.plist

Verify the new env actually took (a bootout is asynchronous — don't trust a
"success" exit alone):

    launchctl print gui/\$(id -u)/com.rjwalters.loom-daemon | grep LOOM_SAFEHOUSE_ROOM

${C_BOLD}systemd hosts (Linux)${C_RESET} — a drop-in, then \`daemon-reload\`, then a DRAINED
restart (the systemd stop job reaps the whole cgroup, unlike launchd's
reparenting — always drain here, never on launchd):

    mkdir -p ~/.config/systemd/user/loom-daemon.service.d
    printf '[Service]\\nEnvironment=LOOM_SAFEHOUSE_ROOM=$ROOM_ID\\n' \\
        > ~/.config/systemd/user/loom-daemon.service.d/safehouse-room.conf
    systemctl --user daemon-reload
    loom-daemon restart --drain

${C_BOLD}Egress allowlist${C_RESET} — on every host with \`[egress]\` publishing enabled, add the
new room id to the \`rooms\` allowlist in safehoused's config and restart
safehoused so it takes effect:

    # safehoused config: [egress] rooms = ["$ROOM_ID", ...]
    systemctl --user restart safehoused   # or the launchd equivalent bootout/bootstrap above

${C_BOLD}Duplicate-burst mitigation${C_RESET} — a long-dark publisher's reconciliation
re-narrates its whole in-window backlog on reconnect. Bound the window well
below the 7-day default BEFORE re-enabling each publisher:

    export LOOM_SAFEHOUSE_RECONCILE_MAX_AGE_SECS=$RECOMMENDED_RECONCILE_MAX_AGE_SECS   # recommended; default is 604800 (7 days)

${C_BOLD}Re-enable order${C_RESET}
  1. Create the room + invite every identity (done above) — verify with
     \`safehouse-mcp list-rooms\` on at least one host that it auto-joined.
  2. Roll out LOOM_SAFEHOUSE_ROOM + LOOM_SAFEHOUSE_RECONCILE_MAX_AGE_SECS on
     every CONSUMER host first (nothing to lose by them being ready early).
  3. Update the egress allowlist + restart safehoused on every PUBLISHER.
  4. Roll out LOOM_SAFEHOUSE_ROOM on every PUBLISHER host last, one at a time,
     watching each daemon's log for its first successful publish into the
     new room before moving to the next host.
  5. Confirm end-to-end: a real completion (not just a chat) lands in the new
     room and reaches every downstream consumer.

${C_BOLD}Traps (from #169's incident)${C_RESET}
  - \`loom-daemon restart --reload-supervisor\` bootstraps the ALREADY-INSTALLED
    plist — it does not re-render it. A brand-new env var (like
    LOOM_SAFEHOUSE_ROOM's new value) only takes effect if the plist was
    re-rendered first; otherwise the daemon reloads with stale env and looks
    fine while quietly still pointed at the dead room. Always verify with
    \`launchctl print\` (see above) after any launchd reload.
  - Older daemon builds predate \`--reload-supervisor\` entirely (seen at
    0.18.121) — for those, the manual bootout+bootstrap (or systemd drop-in)
    path is not a fallback, it's the only path.
  - Daemon logs rotate on restart, and the boot's claim-reconciliation burst
    right after a restart can trip the forge rate-limit breaker for up to 15
    minutes — a wave of failures in that window is expected backoff, not a
    new incident. Capture any log evidence you need BEFORE restarting.

${C_BOLD}Dead-room registry — never reuse or alias-resolve these${C_RESET}
  | Room | ID                   | Died       | Error signature |
  |------|----------------------|------------|------------------|
  | v1   | !FZ8oYmGnszTpYdXUGz  | 2026-08-17 | 500 M_UNKNOWN "cannot create a non-create event in a room with no forward extremities" |
  | v2   | !MQP2aSTA5uDu7czxYZ  | 2026-09-04 | 500 M_UNKNOWN "cannot create a non-create event in a room with no forward extremities" |

Full runbook: docs/runbooks/room-cutover.md
EOF

if [ "$DRY_RUN" -eq 1 ]; then
	printf '\n'
	ok "[dry-run] nothing was sent — no socket was opened. Re-run without --dry-run (with real"
	ok "[dry-run] --room-name/--invite and SAFEHOUSED_SOCKET/SAFEHOUSE_PERSONA set) to cut over for real."
fi
