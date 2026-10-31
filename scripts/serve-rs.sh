#!/bin/bash
# Lumina Rust daemon supervisor stub: exactly one instance on 7432.
PIDFILE="$HOME/.local/share/lumina-rs/serve.pid"
LOG="$HOME/.local/share/lumina-rs/serve.log"
BIN="$HOME/.local/bin/lumina-rs"
# Kill any previous instances (exact comm match only; never pattern-match our own cmdline).
for p in $(pgrep -x lumina-rs); do kill "$p" 2>/dev/null; done
sleep 1
for p in $(pgrep -x lumina-rs); do kill -9 "$p" 2>/dev/null; done
sleep 1
mkdir -p "$(dirname "$LUMINA_DB")" "$(dirname "$LOG")"
cd "$HOME" || exit 1
# NOTE: keep `nohup … &` as its own simple command so $! is the daemon PID
# (chaining with && would capture a subshell instead).
LUMINA_DB="${LUMINA_DB:-$HOME/.local/share/lumina-rs/lumina.db}" \
LUMINA_DEBUG_SSE="${LUMINA_DEBUG_SSE:-}" \
  nohup "$BIN" serve --port 7432 >>"$LOG" 2>&1 &
echo $! > "$PIDFILE"
sleep 2
curl -s -o /dev/null -w "daemon:%{http_code}\n" http://127.0.0.1:7432/openapi.json
readlink "/proc/$(cat "$PIDFILE")/exe"
ls -l "$BIN" | awk '{print "installed:", $6, $7, $8}'
