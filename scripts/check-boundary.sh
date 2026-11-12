#!/bin/sh
set -eu

script_dir=$(CDPATH='' cd "$(dirname "$0")" && pwd)
root=$(CDPATH='' cd "$script_dir/.." && pwd)
cd "$root"
failed=0

report() {
    printf '%s:%s: %s: %s\n' "$1" "$2" "$3" "$4" >&2
    failed=1
}

find crates/ferros-core/src -type f -name '*.rs' -print | while IFS= read -r file; do
    awk -v file="$file" '
        index($0, "target_os") { printf "%s:%d: ferros-core must not contain target_os: %s\n", file, NR, $0; bad=1 }
        {
            code=$0
            sub(/^[[:space:]]*/, "", code)
            if (substr(code, 1, 2) != "//" && (index($0,"unsafe {") || index($0,"unsafe fn") || index($0,"unsafe impl") || index($0,"unsafe extern"))) {
                printf "%s:%d: ferros-core must not contain unsafe code: %s\n", file, NR, $0; bad=1
            }
        }
        END { if (bad) exit 1 }
    ' "$file" || exit 1
done || failed=1

awk '
    /^[[:space:]]*#/ { next }
    {
        line=$0
        while (match(line, /[[:alnum:]_-]+/)) {
            word=substr(line, RSTART, RLENGTH)
            if (word == "mach2" || word == "libproc" || word == "libc" || word == "ferros-macos") {
                printf "crates/ferros-core/Cargo.toml:%d: ferros-core must not depend on OS crates: %s\n", NR, $0
                bad=1
            }
            line=substr(line, RSTART+RLENGTH)
        }
    }
    END { if (bad) exit 1 }
' crates/ferros-core/Cargo.toml || failed=1

find crates/ferros/src -type f -name '*.rs' ! -path 'crates/ferros/src/platform.rs' -print | while IFS= read -r file; do
    awk -v file="$file" 'index($0,"ferros_macos") { printf "%s:%d: only platform.rs may name ferros_macos in the CLI: %s\n", file, NR, $0; bad=1 } END { if (bad) exit 1 }' "$file" || exit 1
done || failed=1

if [ "$failed" -ne 0 ]; then exit 1; fi
printf '%s\n' 'Platform boundary check passed.'
