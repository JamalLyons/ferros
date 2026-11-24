#!/bin/sh
# Run the shared platform boundary checks on Linux and macOS.
set -eu

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
cd "$script_dir/.."

exec "${CARGO:-cargo}" test -p ferros-tests --test platform_boundary -- --nocapture
