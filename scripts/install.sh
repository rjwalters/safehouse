#!/usr/bin/env bash
#
# install.sh — one-command host setup for safehoused.
#
# Brings a brand-new host into the safehouse fleet with no manual Matrix steps:
#
#   1. Builds `safehoused` from this checked-out repo into ~/.local/bin.
#   2. Writes a 0600 config.toml (prompts for homeserver + bot credentials +
#      recovery passphrase; generates the store passphrase for you).
#   3. Verifies the first boot in the foreground — password login, headless
#      cross-signing, recovery enable — and surfaces the daemon's own error
#      verbatim if anything is wrong (bad password, wrong recovery passphrase,
#      unreachable homeserver).
#   4. Registers a supervised service (launchd LaunchAgent on macOS, systemd
#      --user unit on Linux) that keeps the daemon alive across crashes/reboots.
#   5. Prints the loom-side handoff block to wire loom-daemon at it.
#
# All the Matrix logic already lives in the daemon's cold-start boot path
# (safehoused/src/main.rs::boot). This script is pure host orchestration.
#
# It contains ZERO Matrix logic and stores no secret in any file looser than
# 0600, echoes none to the terminal, and leaves none in shell history.
#
# NON-GOAL: creating the bot's Matrix account on the homeserver. That is an
# admin action on the server — see the pointer printed near the top of a run,
# or docs/research/2026-07-26-homeserver.md#creating-a-user-on-an-already-running-server
#
# Idempotent: re-running on an already-set-up host leaves the config/state
# untouched, warm-starts to re-verify, and refreshes the service definition.
#
# Usage:  scripts/install.sh [--help]

set -euo pipefail

# ---------------------------------------------------------------------------
# Cosmetics + small helpers
# ---------------------------------------------------------------------------

if [ -t 1 ]; then
	C_BOLD=$(printf '\033[1m')
	C_BLUE=$(printf '\033[34m')
	C_GREEN=$(printf '\033[32m')
	C_YELLOW=$(printf '\033[33m')
	C_RED=$(printf '\033[31m')
	C_RESET=$(printf '\033[0m')
else
	C_BOLD=""
	C_BLUE=""
	C_GREEN=""
	C_YELLOW=""
	C_RED=""
	C_RESET=""
fi

step() { printf '%s==>%s %s\n' "$C_BLUE$C_BOLD" "$C_RESET" "$*"; }
info() { printf '    %s\n' "$*"; }
ok() { printf '%s ok %s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn() { printf '%swarn%s %s\n' "$C_YELLOW" "$C_RESET" "$*" >&2; }
die() {
	printf '%sfail%s %s\n' "$C_RED" "$C_RESET" "$*" >&2
	exit 1
}

usage() {
	# Print the header comment block (lines after the shebang, up to the first
	# non-comment line), stripping the leading "# " from each line.
	awk 'NR > 1 { if (!/^#/) exit; sub(/^# ?/, ""); print }' "$0"
	exit 0
}

case "${1:-}" in
-h | --help) usage ;;
"") ;;
*) die "unknown argument: $1 (try --help)" ;;
esac

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

BIN_DIR="$HOME/.local/bin"
BIN="$BIN_DIR/safehoused"
CONFIG_DIR="$HOME/.config/safehoused"
CONFIG="$CONFIG_DIR/config.toml"
STATE_DIR="$HOME/.local/state/safehoused"
SOCKET_PATH="$STATE_DIR/safehoused.sock"
SERVICE_LABEL="com.safehouse.safehoused"

OS=$(uname -s)

# ---------------------------------------------------------------------------
# 0. Prerequisites
# ---------------------------------------------------------------------------

step "Checking prerequisites"

command -v cargo >/dev/null 2>&1 ||
	die "cargo not found — install Rust from https://rustup.rs and re-run."
[ -f "$REPO_ROOT/Cargo.toml" ] ||
	die "run this from a safehouse checkout (no Cargo.toml at $REPO_ROOT)."
[ -f "$REPO_ROOT/safehoused/Cargo.toml" ] ||
	die "safehoused/ crate not found under $REPO_ROOT."

case "$OS" in
Darwin) ok "macOS host — will supervise via launchd" ;;
Linux)
	command -v systemctl >/dev/null 2>&1 ||
		die "systemd (systemctl) not found — this installer supervises the
     daemon with 'systemd --user'. On a non-systemd host, build with
     'cargo build --release -p safehoused' and run it under your own
     supervisor instead."
	ok "Linux host — will supervise via systemd --user"
	;;
*) die "unsupported OS '$OS' — only macOS (launchd) and Linux (systemd) are supported." ;;
esac

printf '\n'
info "Reminder: this installer does NOT create the bot's Matrix account."
info "That is a homeserver-admin action. If the bot user does not exist yet,"
info "create it first — see:"
info "  docs/research/2026-07-26-homeserver.md#creating-a-user-on-an-already-running-server"
printf '\n'

# ---------------------------------------------------------------------------
# 1. Build the binary
# ---------------------------------------------------------------------------

step "Building safehoused (cargo build --release)"
( cd "$REPO_ROOT" && cargo build --release -p safehoused )
mkdir -p "$BIN_DIR"
install -m 0755 "$REPO_ROOT/target/release/safehoused" "$BIN"
ok "installed $BIN"

case ":$PATH:" in
*":$BIN_DIR:"*) ;;
*) warn "$BIN_DIR is not on your PATH — add it so 'safehoused' is runnable directly." ;;
esac

# Single source of truth for the config schema version (issue #101,
# provisioning parity): read from the binary we just built rather than
# hand-duplicating the number here, so this script can never drift from
# safehoused/src/main.rs::CONFIG_SCHEMA_VERSION.
CURRENT_SCHEMA_VERSION=$("$BIN" --schema-version)

# ---------------------------------------------------------------------------
# 2. Config
# ---------------------------------------------------------------------------
#
# No-clobber: an existing config is left exactly as is (idempotent re-run =
# re-verify, not re-login). Secrets are read with `read -s` and written straight
# into a 0600 file; nothing is echoed or command-substituted.

# TOML basic-string escaper (backslash + double-quote only; pure bash, no
# subprocess, so the secret never reaches argv or history).
toml_escape() {
	local s=$1
	s=${s//\\/\\\\}
	s=${s//\"/\\\"}
	printf '%s' "$s"
}

gen_passphrase() {
	if command -v openssl >/dev/null 2>&1; then
		openssl rand -hex 32
	else
		LC_ALL=C tr -dc 'a-f0-9' </dev/urandom | head -c 64
		printf '\n'
	fi
}

if [ -f "$CONFIG" ]; then
	step "Config already present — keeping it (idempotent re-run)"

	# Provisioning-parity check (issue #101): warn — never rewrite — when an
	# existing config predates the binary's current schema. This is the
	# no-clobber block, so the only allowed action here is a warning; a
	# stale config keeps booting exactly as before until an operator
	# chooses to hand-edit it (see safehoused/example-config.toml).
	CONFIG_SCHEMA_VERSION=$(grep -E '^schema_version[[:space:]]*=' "$CONFIG" | head -1 | grep -oE '[0-9]+' || true)
	CONFIG_SCHEMA_VERSION=${CONFIG_SCHEMA_VERSION:-0}
	if [ "$CONFIG_SCHEMA_VERSION" -lt "$CURRENT_SCHEMA_VERSION" ]; then
		warn "$CONFIG has schema_version=$CONFIG_SCHEMA_VERSION, behind the current"
		warn "schema_version=$CURRENT_SCHEMA_VERSION. A newer optional field may exist that this"
		warn "config predates (e.g. [egress], #30) and will never appear on its own —"
		warn "compare against safehoused/example-config.toml and hand-edit $CONFIG"
		warn "if you want it. This installer never rewrites an existing config."
	else
		ok "$CONFIG schema_version=$CONFIG_SCHEMA_VERSION is current"
	fi

	ok "$CONFIG left untouched"
else
	step "Writing config ($CONFIG)"

	printf 'Homeserver base URL [https://matrix.example.com]: '
	read -r HOMESERVER
	HOMESERVER=${HOMESERVER:-https://matrix.example.com}

	printf 'Bot username (login only — no @, no :server) [safehouse-bot]: '
	read -r USERNAME
	USERNAME=${USERNAME:-safehouse-bot}

	printf 'Bot password: '
	read -r -s PASSWORD
	printf '\n'
	[ -n "$PASSWORD" ] || die "password must not be empty."

	info "The recovery passphrase is the ONLY headless way back after a crypto-"
	info "store loss (decisions.md D10). Store it OFF this host. On first-ever"
	info "boot for this account it is minted into secret storage; re-use the"
	info "same value on every future host/reinstall for this account."
	printf 'Recovery passphrase: '
	read -r -s RECOVERY
	printf '\n'
	[ -n "$RECOVERY" ] || die "recovery passphrase must not be empty."

	STORE_PASSPHRASE=$(gen_passphrase)

	# Soft reachability probe — the daemon surfaces the authoritative error, but
	# catching an obviously-wrong URL here saves a confusing boot failure.
	if command -v curl >/dev/null 2>&1; then
		if curl -fsS --max-time 10 "$HOMESERVER/_matrix/client/versions" >/dev/null 2>&1; then
			ok "homeserver reachable"
		else
			warn "could not reach $HOMESERVER/_matrix/client/versions — continuing;"
			warn "the daemon boot below will fail with the real error if it is wrong."
		fi
	fi

	mkdir -p "$CONFIG_DIR" "$STATE_DIR"
	chmod 700 "$STATE_DIR"

	# Create the file empty at 0600 BEFORE writing any secret into it.
	( umask 077 && : >"$CONFIG" )
	chmod 600 "$CONFIG"

	{
		printf '# Generated by scripts/install.sh — see safehoused/example-config.toml\n'
		printf '# for the full documented field reference. Permissions: 0600.\n\n'
		printf 'homeserver = "%s"\n' "$(toml_escape "$HOMESERVER")"
		printf 'username = "%s"\n' "$(toml_escape "$USERNAME")"
		printf 'password = "%s"\n' "$(toml_escape "$PASSWORD")"
		printf 'state_dir = "%s"\n' "$(toml_escape "$STATE_DIR")"
		printf 'store_passphrase = "%s"\n' "$(toml_escape "$STORE_PASSPHRASE")"
		printf 'recovery_passphrase = "%s"\n' "$(toml_escape "$RECOVERY")"
		printf '\n'
		printf '# Personas allowed to attach over the unix socket. The loom-daemon\n'
		printf '# integration attaches as "loom_daemon". The allowlist is read once at\n'
		printf '# boot (not hot-reloaded), so it is baked in here before first start.\n'
		printf 'personas = ["loom_daemon"]\n'
		printf '\n'
		printf '# Config schema this file was written against (issue #101) — lets a\n'
		printf '# future re-run of this installer warn if this config falls behind.\n'
		printf 'schema_version = %s\n' "$CURRENT_SCHEMA_VERSION"
	} >>"$CONFIG"

	# Scrub secrets from the shell as soon as they are on disk.
	unset PASSWORD RECOVERY STORE_PASSPHRASE

	ok "wrote $CONFIG (0600)"
fi

# ---------------------------------------------------------------------------
# 3. First-run / warm-start verification
# ---------------------------------------------------------------------------
#
# Run the daemon in the foreground and watch for its own boot-success line.
# Only one process may hold the encrypted store at a time, so if the service
# is already installed we stop it first, verify, then (re)start it below.

stop_service() {
	case "$OS" in
	Darwin)
		launchctl bootout "gui/$(id -u)/$SERVICE_LABEL" >/dev/null 2>&1 || true
		;;
	Linux)
		systemctl --user stop safehoused.service >/dev/null 2>&1 || true
		;;
	esac
}

step "Verifying boot (login, cross-signing, recovery)"
stop_service

VERIFY_LOG=$(mktemp "${TMPDIR:-/tmp}/safehoused-verify.XXXXXX")
trap 'rm -f "$VERIFY_LOG"' EXIT

"$BIN" "$CONFIG" >"$VERIFY_LOG" 2>&1 &
VERIFY_PID=$!

boot_ok=0
deadline=$((SECONDS + 180))
while kill -0 "$VERIFY_PID" 2>/dev/null; do
	if grep -q "entering sync loop" "$VERIFY_LOG"; then
		boot_ok=1
		break
	fi
	if [ "$SECONDS" -gt "$deadline" ]; then
		break
	fi
	sleep 1
done

if [ "$boot_ok" -eq 1 ]; then
	# Show the load-bearing "device ... cross-signed; backup ..." line.
	grep "cross-signed" "$VERIFY_LOG" | sed 's/^/    /' || true
	kill -INT "$VERIFY_PID" 2>/dev/null || true
	wait "$VERIFY_PID" 2>/dev/null || true
	ok "boot verified (device cross-signed, recovery enabled)"
else
	# Failed or timed out — reproduce the daemon's own error and stop.
	if kill -0 "$VERIFY_PID" 2>/dev/null; then
		kill -INT "$VERIFY_PID" 2>/dev/null || true
		wait "$VERIFY_PID" 2>/dev/null || true
		printf '\n%s--- daemon did not reach the sync loop within 180s ---%s\n' "$C_RED" "$C_RESET" >&2
	else
		printf '\n%s--- daemon exited before boot completed ---%s\n' "$C_RED" "$C_RESET" >&2
	fi
	sed 's/^/    /' "$VERIFY_LOG" >&2
	die "boot verification failed — service NOT installed. Fix the config
     ($CONFIG) and re-run. Common causes: wrong bot password, wrong recovery
     passphrase, unreachable homeserver."
fi

# ---------------------------------------------------------------------------
# 4. Supervised service
# ---------------------------------------------------------------------------

install_launchd() {
	local plist="$HOME/Library/LaunchAgents/$SERVICE_LABEL.plist"
	local logdir="$HOME/Library/Logs"
	mkdir -p "$(dirname "$plist")" "$logdir"

	cat >"$plist" <<-PLIST
		<?xml version="1.0" encoding="UTF-8"?>
		<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
		<plist version="1.0">
		<dict>
		    <key>Label</key>
		    <string>$SERVICE_LABEL</string>
		    <key>ProgramArguments</key>
		    <array>
		        <string>$BIN</string>
		        <string>$CONFIG</string>
		    </array>
		    <key>RunAtLoad</key>
		    <true/>
		    <key>KeepAlive</key>
		    <true/>
		    <key>ThrottleInterval</key>
		    <integer>5</integer>
		    <key>ProcessType</key>
		    <string>Background</string>
		    <key>StandardOutPath</key>
		    <string>$logdir/safehoused.out.log</string>
		    <key>StandardErrorPath</key>
		    <string>$logdir/safehoused.err.log</string>
		</dict>
		</plist>
	PLIST
	chmod 644 "$plist"

	# bootout is a no-op if not loaded (stop_service already ran); bootstrap
	# (re)loads the refreshed definition, kickstart -k forces a fresh start.
	launchctl bootout "gui/$(id -u)/$SERVICE_LABEL" >/dev/null 2>&1 || true
	launchctl bootstrap "gui/$(id -u)" "$plist"
	launchctl kickstart -k "gui/$(id -u)/$SERVICE_LABEL" >/dev/null 2>&1 || true
	ok "launchd agent installed: $plist"
	info "logs: $logdir/safehoused.{out,err}.log"
	info "status: launchctl print gui/$(id -u)/$SERVICE_LABEL | head"
}

install_systemd() {
	local unitdir="$HOME/.config/systemd/user"
	local unit="$unitdir/safehoused.service"
	mkdir -p "$unitdir"

	cat >"$unit" <<-UNIT
		[Unit]
		Description=safehoused — per-host Matrix daemon (safehouse)
		After=network-online.target
		Wants=network-online.target

		[Service]
		Type=simple
		ExecStart=$BIN $CONFIG
		Restart=always
		RestartSec=5
		# Stop with SIGINT so the daemon runs its clean-shutdown path (socket
		# cleanup + room-key backup flush) instead of dying on the default
		# SIGTERM. The daemon also handles SIGTERM identically, so this is
		# belt-and-braces; the generous timeout covers wait_for_steady_state's
		# network I/O before systemd escalates to SIGKILL.
		KillSignal=SIGINT
		TimeoutStopSec=120

		[Install]
		WantedBy=default.target
	UNIT
	chmod 644 "$unit"

	systemctl --user daemon-reload
	systemctl --user enable --now safehoused.service
	ok "systemd --user unit installed: $unit"

	# Without lingering the unit stops when the login session ends.
	if command -v loginctl >/dev/null 2>&1; then
		if ! loginctl show-user "$(id -un)" -p Linger 2>/dev/null | grep -q 'Linger=yes'; then
			warn "user lingering is off — the daemon stops when you log out."
			warn "enable persistence with:  sudo loginctl enable-linger $(id -un)"
		fi
	fi
	info "status: systemctl --user status safehoused.service"
	info "logs:   journalctl --user -u safehoused.service -f"
}

step "Installing supervised service"
case "$OS" in
Darwin) install_launchd ;;
Linux) install_systemd ;;
esac

# ---------------------------------------------------------------------------
# 5. Loom handoff
# ---------------------------------------------------------------------------

printf '\n'
printf 'Fleet room (id or #alias:server) to show in the loom block [optional]: '
read -r FLEET_ROOM
FLEET_ROOM=${FLEET_ROOM:-"!your-fleet-room:your-server"}

printf '\n%s================= safehoused is up — loom handoff =================%s\n\n' "$C_BOLD" "$C_RESET"

printf 'Wire loom-daemon at this host by adding to its .loom/config.json:\n\n'
cat <<-JSON
	  "safehouse": {
	    "enabled": true,
	    "socket": "$SOCKET_PATH",
	    "room": "$FLEET_ROOM",
	    "persona": "loom_daemon"
	  }
JSON

printf '\nEquivalently, loom resolves the socket from (first match wins):\n'
# shellcheck disable=SC2016  # literal env-var names, must not expand
printf '    config safehouse.socket  ->  $LOOM_SAFEHOUSE_SOCKET  ->  $SAFEHOUSED_SOCKET\n'
printf 'so you can instead export:\n\n'
printf '    export SAFEHOUSED_SOCKET=%s\n' "$SOCKET_PATH"

printf '\n%sBefore it works end-to-end:%s\n' "$C_BOLD" "$C_RESET"
printf '  1. The persona allowlist already includes "loom_daemon" in your\n'
printf '     config (it is read once at boot — do not add it after start).\n'
printf '  2. Invite the bot to the fleet room from a human Element session:\n'
printf '     invite  @%s:<your-server>  to  %s\n' "${USERNAME:-safehouse-bot}" "$FLEET_ROOM"
printf '     (the daemon auto-joins invites; it never provisions rooms itself).\n'
printf '  3. Restart loom-daemon so it picks up the new config.\n'
printf '  4. Verify: send a message to the fleet room and watch it arrive on\n'
printf '     the human Element client, then have a loom agent read it back.\n'

printf '\n%s==================================================================%s\n\n' "$C_BOLD" "$C_RESET"
ok "done"
