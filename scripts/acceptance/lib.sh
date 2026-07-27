# Shared helpers for the acceptance scripts.
#
# These scripts exist to check the one thing the test suite structurally
# cannot: that ttyforge interoperates with *other people's* implementations —
# socat standing in for ser2net, pyserial's RFC2217 server, pyserial as the
# terminal tool. A codec graded only by its own decoder is graded by nobody.

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
TTYFORGE=${TTYFORGE:-$ROOT/target/debug/ttyforge}

# Missing tooling is a skip (77), not a failure — these are interop checks
# against software that may simply not be installed. A real mismatch exits 1.
need() {
  command -v "$1" >/dev/null 2>&1 || { echo "SKIP: $1 is not installed ($2)"; exit 77; }
}

need_python() {
  python3 -c "import $1" 2>/dev/null || { echo "SKIP: python3 -m $1 is missing ($2)"; exit 77; }
}

require_forge() {
  [ -x "$TTYFORGE" ] || {
    echo "FAIL: no binary at $TTYFORGE — run 'cargo build' first, or set TTYFORGE" >&2
    exit 1
  }
}

free_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

# Start a forge and wait for its readiness line rather than sleeping — which
# is itself part of what these scripts check, since that line *is* ttyforge's
# contract with scripts. Sets FORGE_PID and FORGE_PORTS (one path per line).
start_forge() {
  local expect=${EXPECT_PORTS:-1} out
  out=$(mktemp)
  "$TTYFORGE" "$@" >"$out" 2>/dev/null &
  FORGE_PID=$!
  local waited=0
  while [ "$(wc -l <"$out")" -lt "$expect" ]; do
    kill -0 "$FORGE_PID" 2>/dev/null || { echo "FAIL: forge exited before readiness: $*" >&2; exit 1; }
    sleep 0.1
    waited=$((waited + 1))
    [ "$waited" -lt 100 ] || { echo "FAIL: forge never became ready: $*" >&2; exit 1; }
  done
  FORGE_PORTS=$(cat "$out")
  rm -f "$out"
}

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1" >&2; exit 1; }
