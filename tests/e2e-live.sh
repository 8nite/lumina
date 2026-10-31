#!/usr/bin/env bash
# Lumina Rust port — live end-to-end suite (real BYO-LLM backend).
# Usage: ./tests/e2e-live.sh [--base URL] [--workdir DIR]
# Requires: daemon up, network + configured provider (opencode_go default).
# All workspaces are scratch dirs under /tmp; router/model state is restored.
set -u
BASE="http://127.0.0.1:7432"
REPOROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LUMINA_BIN="${LUMINA_BIN:-lumina-rs}"
command -v "$LUMINA_BIN" >/dev/null || { echo "need $LUMINA_BIN on PATH"; exit 2; }
ROOT="$(mktemp -d /tmp/e2e-ws-XXXXXX)"
PASS=0; FAIL=0; SECTIONS=()
CFGBAK=""
cleanup_root() { rm -rf "$ROOT"; if [ -n "$CFGBAK" ] && [ -f "$CFGBAK" ]; then cp "$CFGBAK" "${LUMINA_CONFIG:-$HOME/.config/lumina/config.toml}"; fi; }
trap cleanup_root EXIT

for a in "$@"; do case "$a" in --base=*) BASE="${a#--base=}";; esac; done

pass() { PASS=$((PASS+1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1${2:+ — $2}"; }
section() { echo "== $1"; SECTIONS+=("$1"); }

api() { # method path [data]
  local m="$1" p="$2" d="${3:-}";
  if [ -n "$d" ]; then curl -s -X "$m" "$BASE$p" -H 'Content-Type: application/json' -d "$d";
  else curl -s -X "$m" "$BASE$p"; fi
}
jget() { python3 -c 'import sys,json;print(json.load(sys.stdin).get("'"$2"'",""))' <<<"$1"; }
new_session() { # workspace [agent] [mode] -> id
  local r; r=$(api POST /api/v1/sessions "{\"workspace_dir\":\"$1\",\"agent\":\"${2:-build}\",\"permission_mode\":\"${3:-bypass}\"}");
  jget "$r" id;
}
# wait_for <session> <timeout>: block until session not running (poll state)
wait_done() {
  local id="$1" t="${2:-150}" i=0;
  while [ $i -lt "$t" ]; do
    local st; st=$(api GET "/api/v1/sessions/$id/state");
    if [ "$(echo "$st" | python3 -c 'import sys,json;print(json.load(sys.stdin).get("running",True))')" = "False" ]; then return 0; fi;
    sleep 3; i=$((i+3));
  done; return 1;
}
export LUMINA_DB="${LUMINA_DB:-$HOME/.local/share/lumina-rs/lumina.db}"

section "S0 preflight"
code=$(curl -s -o /dev/null -w "%{http_code}" "$BASE/openapi.json")
[ "$code" = "200" ] && pass "daemon up" || fail "daemon up" "$code"
npaths=$(curl -s "$BASE/openapi.json" | python3 -c 'import sys,json;print(len(json.load(sys.stdin)["paths"]))')
[ "$npaths" -ge 20 ] && pass "spec has $npaths paths" || fail "spec paths" "$npaths"

section "S1 sessions CRUD"
WS1="$ROOT/ws1"; mkdir -p "$WS1"
S1=$(new_session "$WS1")
[ -n "$S1" ] && pass "create" || fail "create"
echo "$(api GET "/api/v1/sessions?directory=$WS1" | grep -c "$S1") Sessions listed" | grep -q "1 Sessions" && pass "list" || fail "list"
[ "$(api GET "/api/v1/sessions/$S1" | python3 -c 'import sys,json;print(json.load(sys.stdin)["agent"])')" = "build" ] && pass "get" || fail "get"
api PATCH "/api/v1/sessions/$S1" '{"agent":"plan"}' | grep -q '"agent":"plan"' && pass "patch agent" || fail "patch agent"
api PATCH "/api/v1/sessions/$S1" '{"agent":"nope"}' | grep -q 'must be plan' && pass "patch rejects bad agent" || fail "patch validation"
api PATCH "/api/v1/sessions/$S1" '{"agent":"build"}' >/dev/null
api GET "/api/v1/sessions/$S1/export?sanitize=1" | head -1 | grep -q "# Session" && pass "export" || fail "export"
api POST "/api/v1/sessions/$S1/fork" '{}' | grep -q '"parent_id":"'$S1'"' && pass "fork" || fail "fork"
[ "$(api GET "/api/v1/sessions/$S1/state" | python3 -c 'import sys,json;print(json.load(sys.stdin)["running"])')" = "False" ] && pass "state idle" || fail "state idle"

section "S2 exact-reply turn (no tools)"
S2=$(new_session "$ROOT/ws2")
OUT=$(timeout 150 curl -s -X POST "$BASE/api/v1/sessions/$S2/prompt" -H 'Content-Type: application/json' -d '{"text":"reply with exactly E2E-OK and nothing else"}')
echo "$OUT" | grep -q '"text":"E2E-OK"' && pass "exact reply" || fail "exact reply" "$(echo "$OUT" | head -c 150)"

section "S3 tool turn (write + bash)"
S3=$(new_session "$ROOT/ws3"); mkdir -p "$ROOT/ws3"
OUT=$(timeout 180 curl -s -X POST "$BASE/api/v1/sessions/$S3/prompt" -H 'Content-Type: application/json' -d '{"text":"create hello.txt containing exactly the word Hi, then run the shell command cat hello.txt using the bash tool, then reply with the word TOOL-DONE"}')
[ "$(cat "$ROOT/ws3/hello.txt" 2>/dev/null)" = "Hi" ] && pass "write tool made file" || fail "write tool" "$(cat "$ROOT/ws3/hello.txt" 2>/dev/null | head -c 60)"
echo "$OUT" | grep -q "TOOL-DONE" && pass "final text" || fail "final text" "$(echo "$OUT" | head -c 200)"

section "S4 approval gate (ask mode + allow_once)"
S4=$(new_session "$ROOT/ws4" build ask); mkdir -p "$ROOT/ws4"
timeout 240 curl -s -X POST "$BASE/api/v1/sessions/$S4/prompt" -H 'Content-Type: application/json' -d '{"text":"run the shell command echo APPROVAL-TEST using the bash tool, then reply with what it printed"}' > "$ROOT/s4.out" 2>&1 &
P4=$!
REQ=""; for i in $(seq 1 70); do
  ST=$(api GET "/api/v1/sessions/$S4/state")
  REQ=$(echo "$ST" | python3 -c 'import sys,json;p=json.load(sys.stdin).get("permission");print(p["req_id"] if p else "")');
  [ -n "$REQ" ] && break; sleep 3;
done
[ -n "$REQ" ] && pass "permission.asked raised" || fail "permission.asked"
if [ -n "$REQ" ]; then
  api POST "/api/v1/permissions/$REQ" '{"decision":"allow_once"}' | grep -q '"ok":true' && pass "allow_once accepted" || fail "allow_once"
fi
wait $P4 2>/dev/null
# The blocking curl may time out on a slow model; poll the session instead.
for i in $(seq 1 60); do
  [ "$(api GET "/api/v1/sessions/$S4/state" | python3 -c 'import sys,json;print(json.load(sys.stdin)["running"])')" = "False" ] && break
  sleep 5
done
api GET "/api/v1/sessions/$S4/export" | grep -q "APPROVAL-TEST" && pass "turn completed after approval" || fail "approval completion" "$(api GET "/api/v1/sessions/$S4/export" | head -c 200)"

section "S5 question gate"
S5=$(new_session "$ROOT/ws5")
QPROBE='{"text":"use the question tool to ask me which color I prefer with options red and blue, then stop and wait for my answer"}'
QNUDGE='{"text":"you did not use the question tool. Call the question tool right now with my color preference question (options red and blue), then stop and wait."}'
timeout 240 curl -s -X POST "$BASE/api/v1/sessions/$S5/prompt" -H 'Content-Type: application/json' -d "$QPROBE" > "$ROOT/s5.out" 2>&1 &
P5=$!
# Answer every pending question (up to 3 rounds): the model must see its
# answers land before it stops asking.
ASKED=0; ANSWERED=0
for round in 1 2 3; do
  REQ=""
  for i in $(seq 1 40); do
    ST=$(api GET "/api/v1/sessions/$S5/state");
    if [ "$(echo "$ST" | python3 -c 'import sys,json;print(json.load(sys.stdin)["running"])')" = "False" ]; then break 2; fi
    REQ=$(echo "$ST" | python3 -c 'import sys,json;q=json.load(sys.stdin).get("question");print(q["req_id"] if q else "")');
    [ -n "$REQ" ] && break; sleep 3;
  done
  [ -z "$REQ" ] && break
  ASKED=$((ASKED+1))
  if api POST "/api/v1/questions/$REQ" '{"answers":["red"]}' | grep -q '"ok":true'; then ANSWERED=$((ANSWERED+1)); fi
done
# Retry once with a nudge if the model skipped the tool entirely.
if [ "$ASKED" = "0" ]; then
  timeout 240 curl -s -X POST "$BASE/api/v1/sessions/$S5/prompt" -H 'Content-Type: application/json' -d "$QNUDGE" > "$ROOT/s5b.out" 2>&1 &
  P5B=$!
  for round in 1 2 3; do
    REQ=""
    for i in $(seq 1 40); do
      ST=$(api GET "/api/v1/sessions/$S5/state");
      if [ "$(echo "$ST" | python3 -c 'import sys,json;print(json.load(sys.stdin)["running"])')" = "False" ]; then break 2; fi
      REQ=$(echo "$ST" | python3 -c 'import sys,json;q=json.load(sys.stdin).get("question");print(q["req_id"] if q else "")');
      [ -n "$REQ" ] && break; sleep 3;
    done
    [ -z "$REQ" ] && break
    ASKED=$((ASKED+1))
    if api POST "/api/v1/questions/$REQ" '{"answers":["red"]}' | grep -q '"ok":true'; then ANSWERED=$((ANSWERED+1)); fi
  done
  wait $P5B 2>/dev/null
fi
[ "$ASKED" -ge 1 ] && pass "question.asked raised" || fail "question.asked"
[ "$ANSWERED" -ge 1 ] && pass "answer accepted" || fail "answer"
wait $P5 2>/dev/null
# The blocking curl may time out on a slow model; poll the session instead.
for i in $(seq 1 60); do
  [ "$(api GET "/api/v1/sessions/$S5/state" | python3 -c 'import sys,json;print(json.load(sys.stdin)["running"])')" = "False" ] && break
  sleep 5
done
api GET "/api/v1/sessions/$S5/export" | python3 -c '
import sys
t = sys.stdin.read()
parts = t.split("## assistant")
ok = any(p.strip() and not p.strip().startswith("##") for p in parts[1:])
sys.exit(0 if ok else 1)' && pass "turn completed after answer" || fail "question completion" "empty assistant turn"

section "S6 undo/redo (shadow git)"
S6=$(new_session "$ROOT/ws6"); mkdir -p "$ROOT/ws6"; (cd "$ROOT/ws6" && git init -q && git config user.email t@t && git config user.name t && echo base > t.txt && git add -A && git commit -qm init)
timeout 240 curl -s -X POST "$BASE/api/v1/sessions/$S6/prompt" -H 'Content-Type: application/json' -d '{"text":"use the edit tool to append LINE1 to t.txt, then reply DONE6"}' > "$ROOT/s6.out" 2>&1
grep -q "LINE1" "$ROOT/ws6/t.txt" && pass "turn mutated file" || fail "mutating turn"
api POST "/api/v1/sessions/$S6/revert" '{}' | grep -q '"commit"' && pass "revert ok" || fail "revert"
[ "$(cat "$ROOT/ws6/t.txt" | tr '\n' '|')" = "base|" ] && pass "revert restored pre-turn tree" || fail "revert content" "$(cat "$ROOT/ws6/t.txt" | tr '\n' '|')"
api POST "/api/v1/sessions/$S6/unrevert" '{}' | grep -q '"commit"' && pass "unrevert ok" || fail "unrevert"
grep -q "LINE1" "$ROOT/ws6/t.txt" && pass "unrevert restored turn output" || fail "unrevert content"

section "S7 compaction (20 micro-turns + compact)"
S7=$(new_session "$ROOT/ws7")
ok=1; for i in $(seq 1 20); do
  timeout 240 curl -s -X POST "$BASE/api/v1/sessions/$S7/prompt" -H 'Content-Type: application/json' -d '{"text":"reply with exactly T'$i'"}' | grep -q "T$i" || { ok=0; echo "  turn $i deviated"; };
done
[ $ok = 1 ] && pass "20 micro-turns" || fail "20 micro-turns"
CR=$(timeout 180 curl -s -X POST "$BASE/api/v1/sessions/$S7/compact" -H 'Content-Type: application/json' -d '{}')
echo "$CR" | grep -q "compacted" && pass "compact ran" || fail "compact" "$(echo "$CR" | head -c 150)"
N=$(api GET "/api/v1/sessions/$S7/export" | grep -c "^## ")
[ "$N" = "9" ] && pass "9 turns remain (2+6+summary)" || fail "compact count" "$N"

section "S8 memory (kb update + kb_query tool)"
mkdir -p "$ROOT/ws8"
(cd "$ROOT/ws8" && timeout 60 "$LUMINA_BIN" kb update e2emagic "the magic fruit is kiwi [[e2e-fruit]]" >/dev/null)
S8=$(new_session "$ROOT/ws8")
OUT=$(timeout 240 curl -s -X POST "$BASE/api/v1/sessions/$S8/prompt" -H 'Content-Type: application/json' -d '{"text":"use the kb_query tool to find the magic fruit, then reply with only its name"}')
echo "$OUT" | grep -qi "kiwi" && pass "kb_query answered" || fail "kb_query" "$(echo "$OUT" | head -c 200)"

section "S9 SSE fanout (2 subscribers)"
S9=$(new_session "$ROOT/ws9")
(timeout 60 curl -s -N "$BASE/api/v1/events" > "$ROOT/ev-a.cap" 2>&1 &)
(timeout 60 curl -s -N "$BASE/api/v1/events" > "$ROOT/ev-b.cap" 2>&1 &)
sleep 2
timeout 120 curl -s -X POST "$BASE/api/v1/sessions/$S9/prompt" -H 'Content-Type: application/json' -d '{"text":"reply with exactly FANOUT"}' >/dev/null
sleep 2
A=$(grep -c "turn.completed" "$ROOT/ev-a.cap"); B=$(grep -c "turn.completed" "$ROOT/ev-b.cap")
[ "$A" -ge 1 ] && [ "$B" -ge 1 ] && pass "both subscribers got completed" || fail "fanout" "$A/$B"

section "S10 pty websocket"
python3 - "$BASE" <<'PYEOF' > "$ROOT/pty.out" 2>&1
import sys, socket, base64, os, struct
base = sys.argv[1]
host = base.split("//")[1].split(":")[0]
port = int(base.split(":")[2]) if base.count(":") > 2 else 7432
s = socket.create_connection((host, port), timeout=10)
key = base64.b64encode(os.urandom(16)).decode()
s.sendall(f"GET /api/v1/pty HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode())
resp = s.recv(1024).decode(errors="replace")
assert "101" in resp, resp.split("\r\n")[0]
def frame(t):
    b = t.encode(); m = os.urandom(4)
    return bytes([0x81, 0x80 | len(b)]) + m + bytes(c ^ m[i % 4] for i, c in enumerate(b))
s.sendall(frame("echo pty-e2e"))
hdr = s.recv(2); ln = hdr[1] & 0x7F
if ln == 126: ln = struct.unpack(">H", s.recv(2))[0]
out = b""
while len(out) < ln: out += s.recv(ln - len(out))
assert b"pty-e2e" in out, out
print("PTY-OK")
PYEOF
grep -q "PTY-OK" "$ROOT/pty.out" && pass "pty exec round-trip" || fail "pty" "$(head -c 200 "$ROOT/pty.out")"

section "S11 providers/models/router (+persist round-trip)"
api GET /api/v1/providers | grep -q "opencode_go" && pass "providers listed" || fail "providers"
api GET "/api/v1/models?provider=local_ollama" | grep -q '"models"' && pass "models endpoint" || fail "models"
BEFORE_ROUTER=$(api GET /api/v1/router)
CFG="${LUMINA_CONFIG:-$HOME/.config/lumina/config.toml}"; cp "$CFG" "$ROOT/config.bak"; CFGBAK="$ROOT/config.bak"
api PUT /api/v1/router '{"role":"fast","model":"e2e/probe"}' | grep -q '"ok":true' && pass "router hot-swap" || fail "router put"
api GET /api/v1/router | grep -q "e2e/probe" && pass "router reflects" || fail "router get"
grep -q 'fast = "e2e/probe"' "$CFG" && pass "model persisted to file" || fail "persist"
ORIG_FAST=$(echo "$BEFORE_ROUTER" | python3 -c 'import sys,json;print(json.load(sys.stdin)["fast"])')
api PUT /api/v1/router "{\"role\":\"fast\",\"model\":\"$ORIG_FAST\"}" >/dev/null
cp "$ROOT/config.bak" "$CFG"; CFGBAK=""
grep -q "e2e/probe" "$CFG" && fail "config restored" || pass "config restored"

section "S12 harness evolve (scratch repo)"
S12="$ROOT/ws12"; mkdir -p "$S12"; (cd "$S12" && git init -q && git config user.email t@t && git config user.name t && echo x > f && git add -A && git commit -qm init)
(cd "$S12" && timeout 120 "$LUMINA_BIN" evolve 2>&1 | tail -1 | grep -q "evolve landed" && echo EVOLVE-OK > "$ROOT/evo.out" || echo EVOLVE-EMPTY > "$ROOT/evo.out")
grep -q "EVOLVE-OK\|EVOLVE-EMPTY" "$ROOT/evo.out" && pass "evolve ran ($(cat "$ROOT/evo.out"))" || fail "evolve"
git -C "$S12" rev-parse refs/lumina/harness >/dev/null 2>&1 && pass "harness ref committed" || fail "harness ref"
ls ~/.local/share/lumina/library/evolve-*.md >/dev/null 2>&1 && pass "library promoted" || fail "library"

section "S13 plugin hook blocks (bypass still fenced)"
S13="$ROOT/ws13"; mkdir -p "$S13/.lumina/plugins"
cat > "$S13/.lumina/plugins/guard.mjs" <<'EOF'
export default async () => ({
  "tool.execute.before": async (input) => {
    if (input.tool === "bash" && JSON.stringify(input.args || {}).includes("FORBIDDEN")) {
      return { block: "e2e-hook: forbidden command" };
    }
    return {};
  },
});
EOF
S13ID=$(new_session "$S13")
OUT=$(timeout 150 curl -s -X POST "$BASE/api/v1/sessions/$S13ID/prompt" -H 'Content-Type: application/json' -d '{"text":"run the shell command echo FORBIDDEN-XYZ using the bash tool"}')
echo "$OUT" | grep -qi "forbidden" && pass "plugin blocked the call" || fail "plugin hook" "$(echo "$OUT" | head -c 200)"

section "S14 vision attach (1x1 PNG)"
python3 -c "import base64;open('$ROOT/dot.png','wb').write(base64.b64decode('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=='))"
mkdir -p "$ROOT/ws14"
S14=$(new_session "$ROOT/ws14")
OUT=$(cd "$ROOT/ws14" && timeout 240 "$LUMINA_BIN" run -s "$S14" --attach "$ROOT/dot.png" "describe this image in three words" 2>&1)
[ -n "$OUT" ] && [ ${#OUT} -ge 3 ] && pass "vision turn completed ($(echo "$OUT" | head -c 60))" || fail "vision" "$(echo "$OUT" | head -c 150)"

section "S15 web SPA"
code=$(curl -s -o /tmp/e2e-spa.html -w "%{http_code}" "$BASE/")
[ "$code" = "200" ] && grep -q 'id="root"' /tmp/e2e-spa.html && pass "SPA served" || fail "SPA" "$code"
curl -s -D - -o /dev/null "$BASE/" | grep -qi "content-security-policy" && pass "CSP header" || fail "CSP"

section "S16 TUI pty smoke"
echo | timeout 20 script -qec "timeout 10 $LUMINA_BIN" /dev/null > "$ROOT/tui.out" 2>&1
grep -q $'\x1b\[?1049h' "$ROOT/tui.out" && pass "TUI boots (altscreen)" || fail "TUI boot"

section "S17 queue + 409 (doze backend)"
cat > "$ROOT/doze.py" <<'PYEOF'
from http.server import BaseHTTPRequestHandler, HTTPServer
import time
class H(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get('Content-Length', 0)); self.rfile.read(n); time.sleep(8)
        b = b'{"done": true}\n'
        self.send_response(200); self.send_header('Content-Type: application/json')
        self.send_header('Content-Length', str(len(b))); self.end_headers(); self.wfile.write(b)
    def log_message(self, *a): pass
import sys
HTTPServer(('127.0.0.1', int(sys.argv[1])), H).serve_forever()
PYEOF
printf '[providers.o]\nprotocol="ollama"\nbase_url="http://127.0.0.1:18991"\n[router]\nexecutor="o/q"\n' > "$ROOT/doze.toml"
# Dynamic ports: another suite may run concurrently — never hardcode.
DPORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
ZPORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
printf '[providers.o]\nprotocol="ollama"\nbase_url="http://127.0.0.1:%s"\n[router]\nexecutor="o/q"\n' "$ZPORT" > "$ROOT/doze.toml"
LUMINA_CONFIG="$ROOT/doze.toml" LUMINA_DB="$ROOT/doze.db" nohup "$REPOROOT/target/debug/lumina" serve --port "$DPORT" >"$ROOT/doze-srv.log" 2>&1 &
SRVPID=$!; sleep 2
nohup python3 "$ROOT/doze.py" "$ZPORT" >"$ROOT/doze.log" 2>&1 & DOZEPID=$!
sleep 1
DB="http://127.0.0.1:$DPORT"
DS=$(curl -s -X POST $DB/api/v1/sessions -H 'Content-Type: application/json' -d '{"workspace_dir":"/tmp/dz"}'); DID=$(echo "$DS" | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
timeout 60 curl -s -X POST $DB/api/v1/sessions/$DID/prompt -H 'Content-Type: application/json' -d '{"text":"hi"}' > "$ROOT/q1.out" 2>&1 &
sleep 4
C2=$(timeout 10 curl -s -o /dev/null -w "%{http_code}" -X POST $DB/api/v1/sessions/$DID/prompt -H 'Content-Type: application/json' -d '{"text":"q2"}')
[ "$C2" = "202" ] && pass "queued while running (202)" || fail "queue 202" "$C2"
grep -q '"queued":true' <(timeout 10 curl -s -X POST $DB/api/v1/sessions/$DID/prompt -H 'Content-Type: application/json' -d '{"text":"q3"}') && pass "queue body" || fail "queue body"
# Poll for the full drain (3 user + 3 assistant turns).
N=0; for i in $(seq 1 60); do
  N=$(curl -s $DB/api/v1/sessions/$DID/export 2>/dev/null | grep -c "^## ");
  [ "$N" = "6" ] && break;
  sleep 5;
done
if [ "$N" = "6" ]; then
  pass "all 3 turns drained"
else
  fail "drain count" "$N"
  echo "--- S17 forensics (doze hits / daemon state):"
  grep -c "got request" "$ROOT/doze.log" 2>/dev/null || echo "doze log missing"
  tail -c 400 "$ROOT/doze-srv.log" 2>/dev/null; echo
  curl -s "$DB/api/v1/sessions/$DID/state" 2>/dev/null | head -c 200; echo
fi
kill $SRVPID $DOZEPID 2>/dev/null; wait 2>/dev/null

echo
echo "=== e2e-live: $PASS passed, $FAIL failed (${#SECTIONS[@]} sections)"
[ "$FAIL" = "0" ]
