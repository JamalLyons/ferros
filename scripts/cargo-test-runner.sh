#!/bin/sh
# Cargo target runner for aarch64-apple-darwin (configured in .cargo/config.toml).
#
# Cargo calls this for every executable it runs on arm64 macOS (`cargo test`,
# `cargo run`, doctests): `$1` is the executable, the remaining arguments are
# its own arguments. The script ad-hoc signs the executable with the debugger
# entitlement (crates/ferros-macos/ferros.entitlements) so the integration tests
# in crates/ferros-tests can call task_for_pid() without sudo, then execs it so
# the exit status and signals pass through unchanged.
#
# Signing is skipped when the executable already carries the entitlement, and
# nothing is printed on success. The script never prompts and never uses sudo.
#
# Environment:
#   FERROS_TEST_UNSIGNED=1  Re-sign the executable ad hoc WITHOUT the debugger
#                           entitlement before running it. This makes the
#                           permission-failure path of the integration tests
#                           reproducible even after an earlier signed run. The
#                           next run without the switch signs it again.

set -eu

if [ "$#" -lt 1 ]; then
    echo "cargo-test-runner: usage: $0 <executable> [args...]" >&2
    exit 2
fi

exe=$1
shift

# Cargo runs each test binary with the package directory as cwd, so resolve the
# entitlements file relative to this script, not to the current directory.
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
entitlements="$script_dir/../crates/ferros-macos/ferros.entitlements"
entitlement_key="com.apple.security.cs.debugger"

has_debugger_entitlement() {
    codesign -d --entitlements - "$exe" 2>/dev/null | grep -q "$entitlement_key"
}

# Runs codesign quietly. On failure it reports the problem on stderr and
# returns non-zero; the caller still runs the executable (see below).
run_codesign() {
    if ! output=$(codesign "$@" 2>&1); then
        echo "cargo-test-runner: warning: codesign failed for $exe:" >&2
        printf '%s\n' "$output" >&2
        return 1
    fi
}

# A codesign failure does not stop the run: this runner applies to every
# executable cargo runs on this target, most of which (unit tests, the CLI)
# do not need the entitlement, and refusing to run them would turn a signing
# problem into unrelated failures. Tests that do need the entitlement fail on
# their own with guidance that names this signing step.
if ! command -v codesign >/dev/null 2>&1; then
    echo "cargo-test-runner: warning: codesign not found; running $exe unsigned" >&2
elif [ "${FERROS_TEST_UNSIGNED:-}" = "1" ]; then
    if has_debugger_entitlement; then
        run_codesign --force --sign - "$exe" || true
    fi
elif ! has_debugger_entitlement; then
    if [ ! -f "$entitlements" ]; then
        echo "cargo-test-runner: warning: entitlements file not found: $entitlements" >&2
    else
        run_codesign --entitlements "$entitlements" --force --sign - "$exe" || true
    fi
fi

exec "$exe" "$@"
