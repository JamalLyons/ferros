#!/bin/sh
# Platform boundary check (`just check-boundary`, also run in CI on Linux and macOS).
#
# Fails when OS-specific code could leak out of the platform crates:
#   1. crates/ferros-core/src must not contain `target_os` (comments included).
#   2. crates/ferros-core/src must not contain `unsafe` code, and lib.rs must
#      declare `#![forbid(unsafe_code)]`.
#   3. crates/ferros-core/Cargo.toml must not name an OS crate.
#   4. The CLI (crates/ferros/src) may name `ferros_macos` only in its factory
#      module, platform.rs.
#
# Every offending line is printed as `file:line: rule: text` on stderr and the
# exit status is 1. The rules match crates/ferros-tests/tests/platform_boundary.rs.
# POSIX sh and awk only, so it behaves the same on both CI runners.

set -eu

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
cd "$script_dir/.."

failed=0

# Rules 1 and 2: source lines of ferros-core. A line whose first non-blank
# characters are `//` is a comment and may mention unsafe.
core_sources=$(find crates/ferros-core/src -type f -name '*.rs' | sort)
if [ -z "$core_sources" ]; then
    echo "check-boundary: no .rs files found under crates/ferros-core/src" >&2
    exit 1
fi
# shellcheck disable=SC2086 # the file list is split on purpose; paths have no spaces
awk '
    index($0, "target_os") {
        printf "%s:%d: ferros-core must not contain target_os: %s\n", FILENAME, FNR, $0
        bad = 1
    }
    {
        code = $0
        sub(/^[[:space:]]*/, "", code)
        if (substr(code, 1, 2) != "//" &&
            (index($0, "unsafe {") || index($0, "unsafe fn") || index($0, "unsafe impl") || index($0, "unsafe extern"))) {
            printf "%s:%d: ferros-core must not contain unsafe code: %s\n", FILENAME, FNR, $0
            bad = 1
        }
    }
    END { exit bad }
' $core_sources >&2 || failed=1

if ! grep -q '^#!\[forbid(unsafe_code)\]' crates/ferros-core/src/lib.rs; then
    echo "crates/ferros-core/src/lib.rs:1: ferros-core must declare #![forbid(unsafe_code)]" >&2
    failed=1
fi

# Rule 3: any whole word of a non-comment manifest line that is an OS crate name.
awk '
    /^[[:space:]]*#/ { next }
    {
        line = $0
        while (match(line, /[A-Za-z0-9_-]+/)) {
            word = substr(line, RSTART, RLENGTH)
            if (word == "mach2" || word == "libproc" || word == "libc" || word == "ferros-macos") {
                printf "%s:%d: ferros-core must not depend on OS crates: %s\n", FILENAME, FNR, $0
                bad = 1
            }
            line = substr(line, RSTART + RLENGTH)
        }
    }
    END { exit bad }
' crates/ferros-core/Cargo.toml >&2 || failed=1

# Rule 4: the CLI names the macOS crate only in platform.rs.
cli_sources=$(find crates/ferros/src -type f -name '*.rs' ! -path 'crates/ferros/src/platform.rs' | sort)
if [ -n "$cli_sources" ]; then
    # shellcheck disable=SC2086
    awk '
        index($0, "ferros_macos") {
            printf "%s:%d: only platform.rs may name ferros_macos in the CLI: %s\n", FILENAME, FNR, $0
            bad = 1
        }
        END { exit bad }
    ' $cli_sources >&2 || failed=1
fi

if [ "$failed" -ne 0 ]; then
    echo "check-boundary: the platform boundary is broken (see the lines above)" >&2
    exit 1
fi
echo "Platform boundary check passed."
