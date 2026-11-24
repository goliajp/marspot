#!/usr/bin/env bash
#
# Two Codex homes laid out by marspot exclude each other on a thread:
# with `thread-writer-locks` shared, home B cannot resume a thread
# home A's app-server daemon has loaded, and can once A lets it go.
#
# The homes are throwaway, under a temporary HOME: profiles.sh lays
# them out, `marspot-shell --codex-locks` gives them their own lock
# directories back and shares them again (the migration the shell runs
# on the real homes).  No login and no credentials: a thread is made
# with `thread/start` and `thread/inject_items`, which need no model.
#
# Runs the real codex CLI, so it runs on the machine that has one and
# never on a runner.  Every codex command is under a hard timeout, every
# process it starts is recorded by pid and ended by that pid.
#
#   MARSPOT_SHELL_BIN=<marspot-shell> bin/test-codex-thread-locks.sh
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SHELL_BIN="${MARSPOT_SHELL_BIN:-$ROOT/target/debug/marspot-shell}"
PROFILES="$ROOT/assets/skills/marspot-profiles/profiles.sh"
CODEX="$(command -v codex)" || { echo "no codex on PATH"; exit 2; }
[[ -x "$SHELL_BIN" ]] || { echo "no $SHELL_BIN: build marspot-shell or set MARSPOT_SHELL_BIN"; exit 2; }

SB="$(cd "$(mktemp -d /tmp/marspot-cxlocks.XXXXXX)" && pwd -P)"
PIDS="$SB/pids"
REAL_HOME="$HOME"
export HOME="$SB/home"
A="$HOME/.codex-profile-1"
B="$HOME/.codex-profile-2"
SHARED="$HOME/.codex-shared/thread-writer-locks"
mkdir -p "$HOME/.codex/sessions" "$SB/work"
printf 'cli_auth_credentials_store = "file"\n' >"$HOME/.codex/config.toml"

FAILED=0
fail() { echo "FAIL: $*"; FAILED=1; }
ok() { echo "  ok: $*"; }

# pid and start time, so that cleanup ends only the process it started
started() { ps -o lstart= -p "$1" 2>/dev/null | sed 's/ *$//'; }
record() { echo "$1 $(started "$1")" >>"$PIDS"; }
end_recorded() {
  local pid start
  [[ -f "$PIDS" ]] || return 0
  while read -r pid start; do
    [[ "$(started "$pid")" == "$start" ]] && kill "$pid"
  done <"$PIDS"
}
# the managed daemon a TUI starts outlives it; its pid files name it
end_home_daemons() {
  local f pid
  for f in "$1"/app-server-daemon/daemon.pid "$1"/app-server-daemon/daemon-updater.pid; do
    [[ -f "$f" ]] || continue
    pid="$(sed -E 's/^\{"pid":([0-9]+).*/\1/' "$f")"
    if [[ "$(ps -o command= -p "$pid" 2>/dev/null)" == *"$SB"* ]]; then
      echo "$pid (from $f)" >>"$SB/ended"
      kill "$pid"
    fi
  done
}
cleanup() {
  end_recorded
  end_home_daemons "$A"
  end_home_daemons "$B"
  sleep 1
  if (( FAILED == 0 )); then rm -rf "${SB:?}"; else echo "kept $SB"; fi
}
trap cleanup EXIT

cat >"$SB/ws.py" <<'PY'
# JSON-RPC over the WebSocket on an app-server control socket
import base64, json, os, socket, struct, sys, time
class Ws:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX); self.s.connect(os.path.realpath(path)); self.s.settimeout(30)
        self.s.sendall(b"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                       b"Sec-WebSocket-Key: " + base64.b64encode(os.urandom(16)) + b"\r\nSec-WebSocket-Version: 13\r\n\r\n")
        self.buf = b""
        while b"\r\n\r\n" not in self.buf: self.buf += self.s.recv(4096)
        head, self.buf = self.buf.split(b"\r\n\r\n", 1); assert head.startswith(b"HTTP/1.1 101"), head
        self.n = 0
    def need(self, k):
        while len(self.buf) < k: self.buf += self.s.recv(65536)
        out, self.buf = self.buf[:k], self.buf[k:]; return out
    def recv(self):
        b0, b1 = self.need(2); k = b1 & 0x7F
        if k == 126: k = struct.unpack(">H", self.need(2))[0]
        elif k == 127: k = struct.unpack(">Q", self.need(8))[0]
        return json.loads(self.need(k))
    def send(self, obj):
        p = json.dumps(obj).encode(); m = os.urandom(4); k = len(p)
        h = bytes([0x81]) + (bytes([0x80 | k]) if k < 126 else bytes([0xFE]) + struct.pack(">H", k))
        self.s.sendall(h + m + bytes(c ^ m[i % 4] for i, c in enumerate(p)))
    def call(self, method, params):
        self.n += 1; self.send({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params})
        while True:
            r = self.recv()
            if r.get("id") == self.n and "method" not in r: return r
    def init(self):
        self.call("initialize", {"clientInfo": {"name": "lock-probe", "version": "0"}, "capabilities": {"experimentalApi": True}})
        self.send({"jsonrpc": "2.0", "method": "initialized"})
op, sock = sys.argv[1], sys.argv[2]
w = Ws(sock); w.init()
if op == "call":
    print(json.dumps(w.call(sys.argv[3], json.loads(sys.argv[4]))))
elif op == "hold":  # start a thread, keep it, let go when <idfile>.release appears
    cwd, idfile = sys.argv[3], sys.argv[4]
    tid = w.call("thread/start", {"cwd": cwd})["result"]["thread"]["id"]
    w.call("thread/inject_items", {"threadId": tid, "items": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "lock probe"}]}]})
    open(idfile, "w").write(tid)
    while not os.path.exists(idfile + ".release"): time.sleep(0.5)
    print("unsubscribe:", json.dumps(w.call("thread/unsubscribe", {"threadId": tid})))
PY
ws() { /usr/bin/timeout 40 python3 -I "$SB/ws.py" "$@"; }

answers() { ws call "$1/app-server-control/app-server-control.sock" thread/loaded/list '{}' >/dev/null 2>&1; }
# a fresh home's first start takes about twenty seconds
wait_answers() {
  local i
  for (( i = 0; i < 90; i++ )); do
    answers "$1" && return 0
    sleep 1
  done
  return 1
}
start_daemon() { # home -> sets DAEMON (the timeout process; it forwards a TERM)
  CODEX_HOME="$1" /usr/bin/timeout 600 "$CODEX" app-server --listen unix:// \
    </dev/null >>"$SB/daemon.log" 2>&1 &
  DAEMON=$!
  record "$DAEMON"
  wait_answers "$1"
}
stop_daemon() { kill "$DAEMON"; wait "$DAEMON" 2>/dev/null; }

echo "==> layout: profiles.sh init + add, under HOME=$HOME"
MARSPOT_PROFILES_SKIP_RUNNING_CHECK=1 /bin/bash "$PROFILES" init codex >/dev/null || fail "profiles.sh init"
/bin/bash "$PROFILES" add codex 2 >/dev/null || fail "profiles.sh add"
[[ "$(readlink "$B/thread-writer-locks")" == "$SHARED" ]] && ok "a new profile links the shared lock directory"

echo "==> both homes get their own directory back (restore), A has an old lock file"
"$SHELL_BIN" --codex-locks restore "$A" "$B"
[[ -d "$A/thread-writer-locks" && ! -L "$A/thread-writer-locks" ]] || fail "restore left no real directory in A"
: >"$A/thread-writer-locks/old.lock"

echo "==> A's daemon is alive: sharing A is refused, its directory untouched"
start_daemon "$A" || fail "home A's daemon did not come up"
out="$("$SHELL_BIN" --codex-locks share "$A")"; rc=$?
echo "  $out"
(( rc == 1 )) && [[ "$out" == *"answers on"* || "$out" == *"daemon (pid"* ]] && [[ ! -L "$A/thread-writer-locks" ]] \
  && ok "busy home skipped" || fail "a home with a live daemon was changed (rc=$rc)"
stop_daemon

echo "==> A's daemon is gone: both homes are shared"
"$SHELL_BIN" --codex-locks check "$A"
"$SHELL_BIN" --codex-locks share "$A" "$B" || fail "share A B"
[[ "$(readlink "$A/thread-writer-locks")" == "$SHARED" && "$(readlink "$B/thread-writer-locks")" == "$SHARED" ]] \
  && [[ -f "$SHARED/old.lock" ]] && ok "both link $SHARED, A's old.lock moved there" || fail "not shared"

echo "==> A's daemon loads thread T"
start_daemon "$A" || fail "home A's daemon did not come up again"
ADAEMON=$DAEMON
/usr/bin/timeout 300 python3 -I "$SB/ws.py" hold "$A/app-server-control/app-server-control.sock" "$SB/work" "$SB/tid" >"$SB/hold.log" 2>&1 &
HOLD=$!; record "$HOLD"
for (( i = 0; i < 40; i++ )); do [[ -s "$SB/tid" ]] && break; sleep 0.25; done
T="$(cat "$SB/tid" 2>/dev/null)"
[[ -n "$T" ]] || fail "no thread from A"
echo "  T=$T; held: $(lsof -t "$SHARED/$T.lock" 2>/dev/null | tr '\n' ' ')(A's codex is a child of $ADAEMON)"

echo "==> B: codex resume T (the TUI, not logged in), 45 s"
CODEX_HOME="$B" /usr/bin/timeout 70 python3 -I - 45 /usr/bin/timeout 60 "$CODEX" resume "$T" >"$SB/tui.txt" 2>&1 <<'PY'
import os, pty, re, select, struct, subprocess, sys, time, fcntl, termios
m, s = pty.openpty(); fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
p = subprocess.Popen(sys.argv[2:], stdin=s, stdout=s, stderr=s, start_new_session=True); os.close(s)
buf = b""; t0 = time.time()
while time.time() - t0 < float(sys.argv[1]) and p.poll() is None:
    if select.select([m], [], [], 0.2)[0]:
        try: buf += os.read(m, 65536)
        except OSError: break
        if b"\x1b[6n" in buf[-64:]: os.write(m, b"\x1b[1;1R")
print("tui pid", p.pid, "exit", p.poll())
if p.poll() is None: p.terminate(); p.wait(timeout=5)
t = re.sub(rb"\x1b\[[0-9;?<>=]*[ -/]*[@-~]|\x1b[\]P][^\x07\x1b]*(\x07|\x1b\\)?|\x1b.", b" ", buf).decode("utf-8", "replace")
print(re.sub(r"\s+", " ", t)[-1500:])
PY
sed 's/^/  tui: /' "$SB/tui.txt" | cut -c1-400
BSOCK="$B/app-server-control/app-server-control.sock"
if [[ -e "$B/app-server-daemon/daemon.pid" ]] && wait_answers "$B"; then
  echo "  B's daemon (started by the TUI) has loaded: $(ws call "$BSOCK" thread/loaded/list '{}')"
  grep -h -i "active writer" "$B"/app-server-daemon/*.log 2>/dev/null | tail -2 | cut -c1-300 | sed 's/^/  B daemon log: /'
else
  echo "  the TUI left no daemon in B"
  start_daemon "$B" || fail "home B's daemon did not come up"
fi
r="$(ws call "$BSOCK" thread/resume "{\"threadId\":\"$T\"}" 2>&1)"
echo "  B thread/resume T: ${r:0:300}"
[[ "$r" == *"already has an active writer"* ]] && ok "B is refused while A holds T" || fail "B was not refused"
loaded="$(ws call "$BSOCK" thread/loaded/list '{}')"
[[ "$loaded" == *'"data"'* && "$loaded" != *"$T"* ]] && ok "T is not loaded in B" || fail "T is loaded in B: $loaded"
echo "  processes with T's lock open: $(lsof -t "$SHARED/$T.lock" 2>/dev/null | tr '\n' ' ')"

echo "==> A lets T go (unsubscribe); B retries every 5 s, 120 s at most"
: >"$SB/tid.release"
t0=$SECONDS; got=""
while (( SECONDS - t0 < 120 )); do
  r="$(ws call "$BSOCK" thread/resume "{\"threadId\":\"$T\"}")"
  [[ "$r" == *'"thread"'* ]] && { got=$(( SECONDS - t0 )); break; }
  sleep 5
done
[[ -n "$got" ]] && ok "B resumed T ${got} s after A unsubscribed" || fail "B never got T: ${r:0:300}"

(( FAILED == 0 )) && echo "test-codex-thread-locks: PASS" || echo "test-codex-thread-locks: FAIL"
[[ "$HOME" != "$REAL_HOME" ]]
exit "$FAILED"
