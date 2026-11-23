#!/usr/bin/env bash
#
# A stand-in for docker, for bin/test-agent-control.sh: linked as
# `docker` first on PATH.  `run` starts the agent of that name on this
# machine, with the mounts undone in its arguments and the env file's
# variables set; a "container" is a file in $FAKE_DOCKER_DIR/ps holding
# its process and its helper label, an image one in $FAKE_DOCKER_DIR/images.
# An image named in $FAKE_DOCKER_DIR/images-used cannot be removed, build-s
# holds how long a build takes, rm-s how long an `rm`, rm-says what a refused `rm` says, and
# ps-fails makes `ps` fail.
# Every call is logged to $FAKE_DOCKER_DIR/log with its arguments.

set -u
D="${FAKE_DOCKER_DIR:?}"
mkdir -p "$D/ps" "$D/images"
printf '%s\n' "$*" | head -1 >> "$D/log"

verb=${1:-}
shift
case $verb in
  version) echo "29.0.0-fake" ;;
  image)
    tag=${*: -1}
    case $1 in
      inspect) [[ -e "$D/images/${tag//:/_}" ]] || { echo "No such image: $tag" >&2; exit 1; }; echo "sha256:fake" ;;
      ls) for f in "$D"/images/*; do [[ -e $f ]] && n=${f##*/} && echo "${n/_/:}"; done; true ;;
      rm)
        grep -qxF "${tag//:/_}" "$D/images-used" 2>/dev/null && { echo "conflict: image is being used" >&2; exit 1; }
        rm "$D/images/${tag//:/_}"
        ;;
    esac
    ;;
  build)
    tag=""
    while (( $# )); do
      [[ $1 == -t ]] && { tag=$2; shift; }
      shift
    done
    cat > "$D/Dockerfile"
    sleep "$(cat "$D/build-s" 2>/dev/null || echo "${FAKE_DOCKER_BUILD_S:-4}")"
    : > "$D/images/${tag//:/_}"
    ;;
  ps)
    [[ -e $D/ps-fails ]] && { echo "cannot connect" >&2; exit 1; }
    for f in "$D"/ps/*; do
      [[ -e $f ]] || continue
      printf '%s\t%s\n' "${f##*/}" "$(sed -n 2p "$f")"
    done
    ;;
  rm)
    name=${*: -1}
    [[ -s $D/rm-s ]] && sleep "$(cat "$D/rm-s")"
    [[ -s $D/rm-says ]] && { cat "$D/rm-says" >&2; exit 1; }
    if [[ -e "$D/ps/$name" ]]; then
      kill "$(sed -n 1p "$D/ps/$name")" 2>/dev/null
      rm -f "$D/ps/$name"
    fi
    ;;
  run)
    name="" sid="" work="" share="" image=""
    while (( $# )); do
      case $1 in
        --name) name=$2; shift 2 ;;
        --label) [[ $2 == marspot.helper=* ]] && sid=${2#*=}; shift 2 ;;
        --mount)
          from=${2#type=bind,source=}
          [[ -e ${from%%,target=*} ]] || { echo "bind source path does not exist: $2" >&2; exit 125; }
          case $2 in
            *,target=/work) work=${from%,target=/work} ;;
            *,target=/marspot) share=${from%,target=/marspot} ;;
          esac
          shift 2
          ;;
        --env-file) set -a; source "$2"; set +a; shift 2 ;;
        -e) export "${2?}"; shift 2 ;;
        -w | --pull | --cap-drop | --security-opt | --pids-limit | --memory) shift 2 ;;
        -*) shift ;;
        *) image=$1; shift; break ;;
      esac
    done
    [[ -e "$D/images/${image//:/_}" ]] || { echo "No such image: $image" >&2; exit 125; }
    args=()
    for a in "$@"; do args+=("${a//\/marspot\//$share/}"); done
    cd "$work" || exit 125
    [[ -n ${CLAUDE_CODE_OAUTH_TOKEN:-} ]] && echo "token of ${#CLAUDE_CODE_OAUTH_TOKEN} bytes in the environment" >> "$D/log"
    "${args[@]}" <&0 &
    pid=$!
    printf '%s\n%s\n' "$pid" "$sid" > "$D/ps/$name"
    wait "$pid"
    rc=$?
    rm -f "$D/ps/$name"
    exit "$rc"
    ;;
  *) echo "fake docker: $verb" >&2; exit 2 ;;
esac
