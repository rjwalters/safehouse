#!/usr/bin/env bash
#
# check-advisories.sh — check Cargo.lock against the RustSec advisory database (issue #261).
#
# Exits non-zero on any vulnerability advisory (cargo-audit's default), when cargo-audit is not
# installed, and when the advisory database cannot be fetched — a failed check must never look like
# a pass. Ignored advisories live, with a one-line reason each, in .cargo/audit.toml.
#
# Usage: scripts/check-advisories.sh
# Needs: cargo-audit (`cargo install cargo-audit --locked`).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if ! command -v cargo-audit >/dev/null 2>&1; then
  echo "check-advisories: cargo-audit is not installed (cargo install cargo-audit --locked)" >&2
  exit 2
fi

# cargo-audit reads .cargo/audit.toml itself; --deny warnings is deliberately NOT set, so only
# vulnerabilities (not unmaintained/yanked notices) fail the check.
exec cargo audit --file Cargo.lock
