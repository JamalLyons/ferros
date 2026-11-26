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
# When the signature needs to change, the runner signs a private copy and caches
# it alongside the original executable. Nothing is printed on success. The
# script never prompts and never uses sudo.
#
# Environment:
#   FERROS_TEST_UNSIGNED=1  Run without the debugger entitlement. If the Cargo
#                           artifact already has it, make a separate ad-hoc
#                           signed copy without it.

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
    codesign -d --entitlements - "$1" 2>/dev/null | grep -Fq "$entitlement_key"
}

cache_matches_mode() {
    [ -x "$1" ] && embedded=$(codesign -d --entitlements - "$1" 2>/dev/null) || return 1
    if printf '%s\n' "$embedded" | grep -Fq "$entitlement_key"; then
        [ "$mode" = signed ]
    else
        [ "$mode" = unsigned ]
    fi
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

sha256() {
    digest=$(/usr/bin/openssl dgst -sha256 -r) || return 1
    printf '%s\n' "${digest%% *}"
}

cache_path() {
    artifact_hash=$(sha256 < "$1") || return 1
    key=$(printf '%s\n' "$artifact_hash" "$entitlements_hash" | sha256) || return 1
    printf '%s/%s\n' "$cache_dir" "$key"
}

sweep_stale_staging() {
    find "$1" -mindepth 1 -maxdepth 1 -type d -name '.ferros-signing.*' -mmin +60 \
        -exec rm -rf -- {} + 2>/dev/null || true
}

# Called with the cache directory locked until the selected binary execs.
prepare_executable() {
    basename=${exe##*/}
    sweep_stale_staging "$cache_dir"
    entitlements_hash=
    if [ "$mode" = signed ]; then
        entitlements_hash=$(sha256 < "$entitlements") || return 1
    fi

    cached=$(cache_path "$exe") || return 1
    if ! cache_matches_mode "$cached/$basename"; then
        temp_dir=$(mktemp -d -- "$cache_dir/.ferros-signing.XXXXXXXX") || return 1
        trap 'rm -rf -- "$temp_dir"' EXIT
        trap 'exit 1' HUP INT TERM
        mkdir "$temp_dir/bin" || return 1
        candidate="$temp_dir/bin/$basename"
        cp -cp -- "$exe" "$candidate" || return 1
        if [ "$mode" = signed ]; then
            cp -- "$entitlements" "$temp_dir/entitlements.plist" || return 1
            entitlements_hash=$(sha256 < "$temp_dir/entitlements.plist") || return 1
            set -- --entitlements "$temp_dir/entitlements.plist"
        fi

        # Cargo can rebuild the source after the initial cache lookup. Key the
        # published copy by the exact bytes that will be signed.
        cached=$(cache_path "$candidate") || return 1
        if ! cache_matches_mode "$cached/$basename"; then
            if ! run_codesign "$@" --force --sign - "$candidate"; then
                printf '%s\n' "$exe"
                return
            fi
            rm -rf -- "$cached" && mv -f -- "$temp_dir/bin" "$cached" || return 1
        fi
    fi

    for previous in "$cache_dir"/*; do
        if [ "$previous" != "$cached" ] && [ -d "$previous" ]; then
            rm -rf -- "$previous" 2>/dev/null || true
        fi
    done
    printf '%s/%s\n' "$cached" "$basename"
}

if [ "$exe" = --prepare ]; then
    mode=$1
    cache_dir=$2
    exe=$3
    prepare_executable
    exit
fi

exe_dir=$(dirname -- "$exe")
sweep_stale_staging "$exe_dir"

# A codesign failure does not stop the run: this runner applies to every
# executable cargo runs on this target, most of which (unit tests, the CLI)
# do not need the entitlement, and refusing to run them would turn a signing
# problem into unrelated failures. Tests that do need the entitlement fail on
# their own with guidance that names this signing step.
mode=
if ! command -v codesign >/dev/null 2>&1; then
    echo "cargo-test-runner: warning: codesign not found; running $exe unsigned" >&2
elif [ "${FERROS_TEST_UNSIGNED:-}" = "1" ]; then
    if has_debugger_entitlement "$exe"; then
        mode=unsigned
    fi
elif ! has_debugger_entitlement "$exe"; then
    if [ ! -f "$entitlements" ]; then
        echo "cargo-test-runner: warning: entitlements file not found: $entitlements" >&2
    else
        mode=signed
    fi
fi

if [ -n "$mode" ]; then
    cache_dir="$exe_dir/.ferros-runner/${exe##*/}/$mode"
    if mkdir -p -- "$cache_dir" && [ -x /usr/bin/perl ]; then
        # Perl keeps the directory lock open until exec closes it. A later
        # runner can then prune this copy even while it is still running.
        exec /usr/bin/perl -MFcntl=:flock,F_SETFD,FD_CLOEXEC -MErrno=ENOENT -e '
            my ($runner, $mode, $cache, $original, @args) = @ARGV;
            my ($exe, $lock);
            # The preparer inherits the lock in case this process is killed.
            if (open($lock, "<", $cache) && flock($lock, LOCK_EX) && fcntl($lock, F_SETFD, 0)) {
                if (open(my $prepare, "-|", $runner, "--prepare", $mode, $cache, $original)) {
                    local $/;
                    $exe = <$prepare>;
                    undef $exe unless close $prepare;
                }
                undef $exe unless fcntl($lock, F_SETFD, FD_CLOEXEC);
            }
            if (defined $exe) {
                chomp $exe;
            } else {
                close $lock if defined fileno($lock);
                warn "cargo-test-runner: warning: could not prepare $mode copy of $original; running original\n";
                $exe = $original;
            }
            exec { $exe } $exe, @args or do {
                my $status = $! == ENOENT ? 127 : 126;
                warn "cargo-test-runner: $exe: $!\n";
                exit $status;
            };
        ' -- "$script_dir/cargo-test-runner.sh" "$mode" "$cache_dir" "$exe" "$@"
    fi
    echo "cargo-test-runner: warning: could not prepare $mode copy of $exe; running original" >&2
fi

exec "$exe" "$@"
