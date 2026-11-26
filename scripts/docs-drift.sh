#!/bin/sh
# Print documentation-drift evidence. Does not edit files and does not fail
# when it finds stale text. Run from anywhere: the script cds to the repo root.
# Usage: just docs-drift
#        sh scripts/docs-drift.sh

set -eu

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
cd "$script_dir/.."

echo "== docs-drift evidence =="
echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "rev: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
echo "branch: $(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
echo

echo "== workspace crates (Cargo.toml) =="
awk '
  /^\[workspace\]/ { in_ws = 1; next }
  in_ws && /^\[/ { exit }
  in_ws { print }
' Cargo.toml
echo

echo "== current guidance files =="
# docs/reviews is historical. Validation notes record a past run.
# Both are listed so the sweep can see them, and marked so they are not
# treated as instructions to modernize.
find . \
  -path './target' -prune -o \
  -path './vendor' -prune -o \
  -path './.git' -prune -o \
  -path './website/node_modules' -prune -o \
  -type f \( -name '*.md' -o -name 'JustFile' -o -name 'justfile' \) -print |
  sed 's#^\./##' |
  sort |
  while IFS= read -r path; do
    case "$path" in
      docs/reviews/*|crates/ferros-tests/*-validation.md)
        printf '%s\thistorical-or-note\n' "$path"
        ;;
      *)
        printf '%s\tcurrent\n' "$path"
        ;;
    esac
  done
echo

echo "== rustdoc entry points (lib.rs, mod.rs) =="
find crates \( -name lib.rs -o -name mod.rs \) -type f | sed 's#^\./##' | sort
echo

echo "== CLI help source =="
grep -n -E 'about = |^/// |^#!\[command' crates/ferros/src/main.rs || true
echo

echo "== integration and CLI test files =="
find crates/ferros-tests/tests crates/ferros/tests \
  -name '*.rs' -type f 2>/dev/null |
  sed 's#^\./##' |
  sort
echo

echo "== stale-pattern hits =="
# Patterns that have gone stale before. A hit is a lead, not a verdict.
pattern='sudo ferros|Run with sudo|with `sudo`|with sudo|72 of 75|still open|gated on|marks the end of Phase 1|tracking epic|Phase 1\.5|hybrid MIR|MIR-Level|interpreted execution|async tasks directly|Memory Safety Visualization|Zero-Cost Breakpoints'
# Current docs plus the website. Historical reviews are skipped here so a
# snapshot of an old tree does not look like a fresh claim.
find . \
  -path './target' -prune -o \
  -path './vendor' -prune -o \
  -path './.git' -prune -o \
  -path './docs/reviews' -prune -o \
  -path './.agents' -prune -o \
  -path './website/node_modules' -prune -o \
  -type f \( -name '*.md' -o -name '*.rs' -o -name '*.html' -o -name 'JustFile' \) -print |
  while IFS= read -r path; do
    # -H keeps the path when grep is given one file. A hit is a lead, not a verdict.
    grep -n -H -E -e "$pattern" "$path" || true
  done
echo

echo "== issue numbers cited by current docs =="
# Markdown only. Rust comments cite the issue that added a line; those are not
# documentation claims, and a bare "#0" also matches assembly immediates.
cited=$(
  find README.md ROADMAP.md AGENTS.md CONTRIBUTING.md docs crates \
    -path 'docs/reviews' -prune -o \
    -path 'crates/ferros-tests/*-validation.md' -prune -o \
    -path '.agents' -prune -o \
    -name '*.md' -type f -print 2>/dev/null |
    while IFS= read -r path; do
      # Issue links and bare "#123" (two or more digits). A single digit is an
      # assembly immediate in these docs (`brk #1`). A match must also end at a
      # non-digit so a CSS-like hex run is not read as an issue number.
      grep -o -E 'issues/[0-9]+|(^|[^A-Za-z0-9_#])#[0-9]{2,}([^0-9A-Fa-f]|$)' "$path" 2>/dev/null || true
    done |
    grep -o -E '[0-9]+' |
    sort -n |
    uniq
)
if [ -n "$cited" ]; then
  echo "$cited" | tr '\n' ' '
  echo
else
  echo "(none)"
fi
echo

# Phase issues the roadmap and planning note depend on, even when a doc has
# dropped one of them. Live `gh` output wins over this list.
watch="26 40 41 182 183 188 210"

echo "== GitHub issue state =="
if ! command -v gh >/dev/null 2>&1; then
  echo "gh: not installed. Issue state was not checked."
  exit 0
fi
if ! gh auth status >/dev/null 2>&1; then
  echo "gh: not authenticated. Issue state was not checked."
  exit 0
fi

repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null || true)
if [ -z "$repo" ]; then
  echo "gh: could not resolve the repository. Issue state was not checked."
  exit 0
fi
echo "repo: $repo"

numbers=$(printf '%s\n%s\n' "$cited" "$watch" | tr ' ' '\n' | grep -E '^[0-9]+$' | sort -n | uniq)

for n in $numbers; do
  if ! gh issue view "$n" --repo "$repo" --json number,title,state,closedAt,stateReason \
    --jq '"\(.number)\t\(.state)\t\(.closedAt // "-")\t\(.stateReason // "-")\t\(.title)"'; then
    echo "MISSING	#$n"
  fi
done
echo

echo "== epic sub-issues =="
for n in $numbers; do
  title=$(gh issue view "$n" --repo "$repo" --json title --jq .title 2>/dev/null || true)
  case "$title" in
    Epic:*) ;;
    *) continue ;;
  esac
  echo "-- #$n $title --"
  if ! rows=$(gh api --paginate "repos/$repo/issues/$n/sub_issues" --jq '.[] | [.number, .state, .title] | @tsv' 2>/dev/null); then
    echo "sub-issues: request failed"
    continue
  fi
  if [ -z "$rows" ]; then
    echo "(no sub-issues)"
    continue
  fi
  printf '%s\n' "$rows" | awk -F '\t' '
    { count[$2]++; total++ }
    $2 != "closed" && $2 != "CLOSED" { print "OPEN\t" $1 "\t" $3 }
    END {
      printf "summary:"
      for (state in count) printf " %s=%d", state, count[state]
      printf " total=%d\n", total
    }
  '
done

echo
echo "== end (evidence only; this script does not edit files) =="
