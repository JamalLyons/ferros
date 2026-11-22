#!/bin/sh
# Kills the ferros fixture processes started from one worktree's target directory,
# leaving fixtures that belong to other worktrees running.
#
# `pkill -9 -f ferros-fixture-` kills every agent's fixtures on the machine, so an
# agent cleaning up after itself could fail another agent's integration run. This
# script matches only executables under `<worktree>/target/`, where the test harness
# stages fixtures (target/tmp/ferros-fixtures/...).
#
# Usage: scripts/kill-worktree-fixtures.sh [worktree]   (default: this repository)
# Prints each killed process and exits 0, including when nothing matched.

set -eu

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
worktree=$(CDPATH='' cd -- "${1:-$script_dir/..}" && pwd -P)
prefix="$worktree/target/"

# Fixed-string prefix match on the executable path (the first word of the command),
# so characters such as `.` in the worktree path are not read as a pattern.
ps -axo pid=,command= | awk -v prefix="$prefix" '
    index($2, prefix) == 1 && index($2, "/ferros-fixture-") > 0 { print $1, $2 }
' | while read -r pid exe; do
    if kill -9 "$pid" 2>/dev/null; then
        echo "killed $pid $exe"
    fi
done
