#!/usr/bin/env bash
# bin/_git-sha.sh — this checkout's commit as a build names it in
# MARSPOT_FP: `rev-parse --short=8 HEAD`, plus `-dirty` with uncommitted
# changes.  A runner builds from a synced copy that has no `.git`; the
# scripts that start such a build pass this as MARSPOT_GIT_SHA, which
# build.rs takes only when it finds no repository.
set -euo pipefail
cd "$(dirname "$0")/.."
sha="$(git rev-parse --short=8 HEAD)"
[[ -z "$(git status --porcelain)" ]] || sha+="-dirty"
printf '%s\n' "$sha"
