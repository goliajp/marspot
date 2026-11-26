#!/usr/bin/env bash
# bin/_git-sha.sh [BEFORE] — this checkout's commit as a build names it
# in MARSPOT_FP: `rev-parse --short=8 HEAD`, plus `-dirty` with
# uncommitted changes.  A runner builds from a synced copy that has no
# `.git`; the scripts that start such a build pass this as
# MARSPOT_GIT_SHA, which build.rs takes only when it finds no repository.
#
# Those scripts run it twice, before and after the sync, and pass the
# first answer as BEFORE to the second: if the checkout moved while the
# sync ran (a commit, an edit), the copy may hold files of either state,
# so the answer is marked `-dirty` rather than naming a clean commit the
# copy may not be.
set -euo pipefail
cd "$(dirname "$0")/.."
sha="$(git rev-parse --short=8 HEAD)"
[[ -z "$(git status --porcelain)" ]] || sha+="-dirty"
if [[ $# -ge 1 && "$1" != "$sha" && "$sha" != *-dirty ]]; then
  echo "_git-sha: the checkout moved during the sync ($1 -> $sha); the build is marked -dirty" >&2
  sha+="-dirty"
fi
printf '%s\n' "$sha"
