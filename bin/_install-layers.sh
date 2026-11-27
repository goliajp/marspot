#!/usr/bin/env bash
#
# Which layers an install swaps, and why.
#
# Sourced by install-local.sh, which sets ROOT, TARGET, TREE and MACOS.
# Run directly, it explains an install between two sets of binaries
# without installing anything:
#
#   bin/_install-layers.sh explain <running-dir> <target-dir> [<source-tree>]
#
# Each dir holds marspot-shell / marspot-core / marspot-session (and
# optionally marspot-peer); the printed INSTALL_LAYERS line is the one
# an install from <running-dir> to <target-dir> would write when the
# target binaries were built from <source-tree> (default: this checkout).

running_equiv() {
  # What the running supervisor came from: current/ slot if a prior
  # update populated it, else the (still-old) bundle binary.
  local bin="$1"
  if [[ -f "$TREE/current/$bin" ]]; then echo "$TREE/current/$bin"; else echo "$MACOS/$bin"; fi
}

# The sha (with optional `-dirty`) in a binary's `MARSPOT_FP=<sha>|<ts>|END`
# rodata marker, written by build.rs; empty when there is none (the
# caller then compares bytes).
binary_git_sha() {
  local bin="$1"
  [[ -f "$bin" ]] || { echo ""; return; }
  strings "$bin" 2>/dev/null \
    | grep -oE 'MARSPOT_FP=[0-9a-f]{8}(-dirty)?\|' \
    | head -1 \
    | sed -E 's/^MARSPOT_FP=//;s/\|$//'
}

# built_from_tree SHA: whether SHA, the commit a target binary names, is
# what the source tree (SRC_TREE, default ROOT) holds: clean and at that
# commit.  build.rs reruns only when git's HEAD, refs or index change (or
# MARSPOT_GIT_SHA, which install-local's build sets), so a binary built
# otherwise after an unstaged edit can still name a clean commit.  The
# binaries `--no-build` installs were built on a runner from a copy of
# this tree; a tree that moved since then only swaps more layers.
_TREE_AT=""  # the tree's commit when clean, "-" otherwise
built_from_tree() {
  if [[ -z "$_TREE_AT" ]]; then
    local src="${SRC_TREE:-$ROOT}" head
    _TREE_AT=-
    if head="$(git -C "$src" rev-parse --short=8 HEAD 2>/dev/null)" \
        && [[ -z "$(git -C "$src" status --porcelain)" ]]; then
      _TREE_AT="$head"
    fi
  fi
  [[ -n "$1" && "$1" == "$_TREE_AT" ]]
}

# Extract the per-layer version (e.g. `0.5.4`) from a binary's
# `MARSPOT_LAYER_VERS=shell:X|core:Y|session:Z|END` rodata marker.
# Empty when the marker isn't present (older binary).
# $1 = binary path, $2 = layer name (shell / core / session).
binary_layer_ver() {
  local bin="$1" layer="$2"
  [[ -f "$bin" && -n "$layer" ]] || { echo ""; return; }
  strings "$bin" 2>/dev/null \
    | grep -oE 'MARSPOT_LAYER_VERS=[^[:cntrl:]]*\|END' \
    | head -1 \
    | grep -oE "${layer}:[0-9A-Za-z._-]+" \
    | head -1 \
    | sed -E "s/^${layer}://"
}

# L2 (marspot-core) is the canonical version carrier: all binaries ride
# on the same git commit, so "is this install a no-op?" is decided by
# L2's fingerprint alone:
#
#   running L2 sha == target L2 sha, no `-dirty` suffix on either side,
#   and the source tree is clean at that commit (built_from_tree)
#   => same commit, nothing to do: skip stage for every binary so no
#      SIGUSR1 fires, no shell self-update execv runs, no window flashes.
#
# Any other state falls back to the per-layer checks in `changed`.
# Computed once per run.
_L2_NOOP=""  # "yes" / "no" once decided
_l2_decision() {
  if [[ -z "$_L2_NOOP" ]]; then
    local ref tgt ref_sha tgt_sha
    ref="$(running_equiv marspot-core)"
    tgt="$TARGET/marspot-core"
    _L2_NOOP=no
    if [[ -f "$ref" && -f "$tgt" ]]; then
      ref_sha="$(binary_git_sha "$ref")"
      tgt_sha="$(binary_git_sha "$tgt")"
      if [[ -n "$ref_sha" && "$ref_sha" == "$tgt_sha" ]] && built_from_tree "$tgt_sha"; then
        _L2_NOOP=yes
      fi
    fi
  fi
  [[ "$_L2_NOOP" == yes ]]
}

layer_of_bin() {
  case "$1" in
    marspot-shell)   echo shell ;;
    marspot-core)    echo core ;;
    marspot-session) echo session ;;
    marspot-peer)    echo peer ;;
  esac
}

# Record why a layer swaps or not, and the sha its running binary was
# built from, for the INSTALL_LAYERS line.
_why() { printf -v "WHY_$1" '%s' "$2"; }
_from() { printf -v "FROM_$1" '%s' "$(binary_git_sha "$2")"; }
# _running_clean LAYER: after _from, its running binary names a clean commit
_running_clean() { local v="FROM_$1"; [[ -n "${!v:-}" && "${!v}" != *-dirty ]]; }

# changed BIN: true when BIN must be staged.
changed() {
  local bin="$1" ref layer
  layer="$(layer_of_bin "$bin")"
  ref="$(running_equiv "$bin")"
  [[ -f "$ref" ]] || { _why "$layer" not-installed; return 0; }
  _from "$layer" "$ref"
  if _l2_decision; then
    _why "$layer" same-commit
    return 1
  fi
  # Per-layer version skip: when both binaries carry
  # MARSPOT_LAYER_VERS and this layer's number is unchanged, the
  # developer declared (in version-vector.toml) that the layer did not
  # change, so a pure-L2 install doesn't restage L1 just because the
  # rebuild bumped the sha and build time embedded in every binary.
  # Not for a build the tree does not confirm: mid-iteration the vector
  # may not reflect uncommitted source, so fall through to byte-compare.
  # Nor when the running binary is a -dirty build: code no commit has.
  local tgt_sha
  tgt_sha="$(binary_git_sha "$TARGET/$bin")"
  if built_from_tree "$tgt_sha" && _running_clean "$layer"; then
    local ref_ver tgt_ver
    ref_ver="$(binary_layer_ver "$ref" "$layer")"
    tgt_ver="$(binary_layer_ver "$TARGET/$bin" "$layer")"
    if [[ -n "$ref_ver" && "$ref_ver" == "$tgt_ver" ]]; then
      _why "$layer" same-build-number
      return 1
    fi
  fi
  if cmp -s "$TARGET/$bin" "$ref"; then
    _why "$layer" bytes-same
    return 1
  fi
  _why "$layer" bytes-differ
  return 0
}

# peer_decision NEW INSTALLED: marspot-peer is not part of the swap; it
# is replaced on PATH when it is from another commit or its bytes
# differ.  Call before installing it; install only on bytes-differ or
# not-installed.
#
# The same-commit step comes first because the bytes of two installs
# of one build never match: install-local signs the binary every time,
# and a Developer ID signature carries its signing time.  It is taken
# only when the source tree confirms the commit the new binary names.
peer_decision() {
  local new="$1" old="$2" new_sha
  if [[ ! -x "$new" ]]; then _why peer not-built
  elif [[ ! -f "$old" ]]; then _why peer not-installed
  else
    _from peer "$old"
    new_sha="$(binary_git_sha "$new")"
    if [[ "$new_sha" == "${FROM_peer:-}" ]] && built_from_tree "$new_sha"; then
      _why peer same-commit
    elif cmp -s "$new" "$old"; then _why peer bytes-same
    else _why peer bytes-differ
    fi
  fi
}

# peer_needs_install: after peer_decision, whether the binary on PATH
# is to be replaced.
peer_needs_install() {
  case "${WHY_peer:-}" in bytes-differ|not-installed) return 0 ;; *) return 1 ;; esac
}

# Which binaries a repository path is built into, in LP.  `other` = none
# (scripts, tests, docs, bundle resources).
layers_of_path() {
  case "$1" in
    tests/*|benches/*|examples/*|crates/*/tests/*|crates/*/benches/*|crates/*/examples/*) LP="other" ;;
    *_tests.rs|*/tests.rs|*/tests/*|*/fixtures/*) LP="other" ;;
    crates/marspot-session/*)                 LP="session" ;;
    crates/marspot-term/*|crates/marspot-linkify/*) LP="shell core session" ;;
    crates/marspot-peer/*)                    LP="shell core peer" ;;
    crates/ui/*)                              LP="shell core" ;;
    src/bin/marspot-shell/*)                  LP="shell" ;;
    src/bin/marspot-core.rs|src/bin/marspot-core/*) LP="core" ;;
    src/*|build.rs)                           LP="shell core" ;;
    assets/skills/*|assets/helper-image/*)    LP="shell" ;;
    version-vector.toml)                      LP="shell core session" ;;
    Cargo.toml|Cargo.lock)                    LP="shell core session peer" ;;
    *)                                        LP="other" ;;
  esac
}

_is_commit() { [[ -n "$1" ]] && git -C "$ROOT" cat-file -e "${1%-dirty}^{commit}" 2>/dev/null; }

# changed_paths FROM HEAD: paths changed from FROM to HEAD, or to the
# working tree when HEAD is a -dirty build.
changed_paths() {
  local from="${1%-dirty}" head="$2"
  if [[ "$head" == *-dirty ]]; then
    git -C "$ROOT" diff --name-only "$from" --
  else
    git -C "$ROOT" diff --name-only "$from" "$head" --
  fi
}

# count_layers FROM HEAD: sets N_shell .. N_other to the number of
# changed paths built into each layer, reading the diff once.
count_layers() {
  local p l
  N_shell=0; N_core=0; N_session=0; N_peer=0; N_other=0
  while IFS= read -r p; do
    [[ -n "$p" ]] || continue
    layers_of_path "$p"
    for l in $LP; do
      case "$l" in
        shell) N_shell=$((N_shell+1)) ;; core) N_core=$((N_core+1)) ;;
        session) N_session=$((N_session+1)) ;; peer) N_peer=$((N_peer+1)) ;;
        *) N_other=$((N_other+1)) ;;
      esac
    done
  done < <(changed_paths "$1" "$2")
}

INSTALL_LAYERS_FILES_CAP=20

# install_layers_line HEAD SWAP: the one-line record of this install.
# HEAD is the target build's sha, SWAP the comma list of layers that
# swap ("none" when nothing does).  Path counts are from the running
# L2's sha; a layer whose running binary is from another commit counts
# from its own and says so in from_<layer>.
install_layers_line() {
  local head="$1" swap="$2" from="${FROM_core:-}" line l lf why=""
  for l in shell core session peer; do
    lf="WHY_$l"; why="$why${why:+,}$l:${!lf:-unknown}"
  done
  line="INSTALL_LAYERS ts=$(date -u +%Y-%m-%dT%H:%M:%SZ) head=${head:-unknown} from=${from:-unknown} swap=$swap running=${RUNNING:-unknown} why=$why"
  if ! _is_commit "$from" || ! _is_commit "$head"; then
    echo "$line paths=unknown"
    return
  fi
  local all n
  all="$(changed_paths "$from" "$head")"
  n=$(printf '%s\n' "$all" | awk 'NF { c++ } END { print c + 0 }')
  line="$line paths=$n"
  count_layers "$from" "$head"
  # shellcheck disable=SC2034 # read back through ${!v}
  local b_shell=$N_shell b_core=$N_core b_session=$N_session b_peer=$N_peer b_other=$N_other v
  for l in shell core session peer; do
    lf="FROM_$l"; lf="${!lf:-}"
    if [[ -n "$lf" && "$lf" != "$from" ]] && _is_commit "$lf"; then
      count_layers "$lf" "$head"
      v="N_$l"; line="$line paths_$l=${!v} from_$l=$lf"
    else
      v="b_$l"; line="$line paths_$l=${!v}"
    fi
  done
  line="$line paths_other=$b_other"
  local files
  files="$(printf '%s\n' "$all" | awk -v cap="$INSTALL_LAYERS_FILES_CAP" \
    'NF && ++c <= cap { gsub(/ /, "%20"); printf "%s%s", (c > 1 ? "," : ""), $0 }')"
  if (( n > INSTALL_LAYERS_FILES_CAP )); then files="$files,+$((n - INSTALL_LAYERS_FILES_CAP))"; fi
  echo "$line files=$files"
}

# swap_list SHELL CORE SESSION: comma list from the three 0/1 flags plus
# the peer decision.
swap_list() {
  local s=""
  (( $1 )) && s="shell"
  (( $2 )) && s="$s${s:+,}core"
  (( $3 )) && s="$s${s:+,}session"
  ! peer_needs_install || s="$s${s:+,}peer"
  echo "${s:-none}"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  set -euo pipefail
  [[ "${1:-}" == explain && ( $# -eq 3 || $# -eq 4 ) ]] \
    || { echo "usage: $0 explain <running-dir> <target-dir> [<source-tree>]" >&2; exit 2; }
  ROOT="$(cd "$(dirname "$0")/.." && pwd)"
  MACOS="$2"; TREE="$2/.no-tree"; TARGET="$3"; SRC_TREE="${4:-$ROOT}"
  sh=0; co=0; se=0
  changed marspot-shell && sh=1
  changed marspot-core && co=1
  changed marspot-session && se=1
  peer_decision "$TARGET/marspot-peer" "$MACOS/marspot-peer"
  install_layers_line "$(binary_git_sha "$TARGET/marspot-core")" "$(swap_list $sh $co $se)"
fi
