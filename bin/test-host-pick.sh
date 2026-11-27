#!/usr/bin/env bash
# bin/test-host-pick.sh — marspot_host against a stub `devops`, locally:
# an answer devops gives, the overrides, and every way to the fallback.
# The real devops is never called.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
S="$(mktemp -d "${TMPDIR:-/tmp}/marspot-host-pick.XXXXXX")"
trap 'rm -rf "${S:?}"' EXIT
mkdir "$S/stubs"
# the stub answers what $S/answer holds and logs its arguments
cat > "$S/stubs/devops" <<STUB
#!/usr/bin/env bash
echo "\$*" >> "$S/calls"
case "\$(cat "$S/mode")" in
  fail) exit 1 ;;
  hang) sleep 30 ;;
esac
cat "$S/answer"
STUB
chmod +x "$S/stubs/devops"

fails=0
answer() {  # answer <device> <its excluded reason or null>
  printf '{"device":"%s","reason":"r","scored":[{"device":"%s","excluded":%s}]}\n' "$1" "$1" "$2" > "$S/answer"
}
check() {  # check <name> <expected host> <kind> [repo]
  local name=$1 want=$2 got
  shift 2
  got="$(cd "$S" && PATH="$S/stubs:${CHECK_PATH:-$PATH}" bash -c 'ROOT=$1; shift; source "$ROOT/bin/_host.sh"; marspot_host "$@"' \
    _ "$ROOT" "$@" 2>"$S/err")"
  if [[ $got == "$want" ]]; then
    echo "ok    $name: $got  ($(tail -1 "$S/err"))"
  else
    echo "FAIL  $name: got '$got', want '$want'; stderr: $(cat "$S/err")"
    fails=$((fails + 1))
  fi
}
unset MARSPOT_TEST_HOST MARSPOT_GUI_HOST MARSPOT_BENCH_HOST MARSPOT_RUNNER_SKIP

echo ok > "$S/mode"
answer mini2 null;            check "test pick" mini2 test test-marspot
grep -q -- '--kind test --platform macos-arm64 --repo test-marspot --json' "$S/calls" \
  || { echo "FAIL  pick args: $(cat "$S/calls")"; fails=$((fails + 1)); }
answer mini4 null;            check "bench pick devops scores as bench" mini4 bench bench-marspot
answer mini2 '"no bench role"'; check "bench pick not a bench runner" mini3 bench bench-marspot
answer mini3 '"busy"';        check "bench pick is the fallback" mini3 bench
answer mini null;             check "skipped runner" mini2 test
answer 'a b' null;            check "malformed device" mini2 gui
echo 'not json' > "$S/answer"; check "not json" mini3 bench
echo fail > "$S/mode";        check "devops fails" mini2 gui
echo hang > "$S/mode";        check "devops hangs" mini2 test
: > "$S/calls"
MARSPOT_BENCH_HOST=elsewhere check "override" elsewhere bench
[[ ! -s $S/calls ]] || { echo "FAIL  override still called devops"; fails=$((fails + 1)); }
# a PATH without the stub and without any real devops
mv "$S/stubs/devops" "$S/devops.off"
CHECK_PATH=/usr/bin:/bin check "devops missing" mini2 gui

(( fails == 0 )) && echo "PASS" || echo "FAIL: $fails"
(( fails == 0 ))
