#!/usr/bin/env bash
#
# M5 — one port, two tools, and every tool sees the whole stream.
#
#   ttyforge sim --preset uboot     the board
#     -> ttyforge mux                  fanned out to two virtual ttys
#          -> pyserial x2                 a script driving, a human watching
#
# The failure this guards against is subtle: a naive mux *splits* the byte
# stream between consumers rather than copying it, which looks like data loss
# only when two tools are attached at once.

. "$(dirname "$0")/lib.sh"
require_forge
need_python serial "the two consumers"

board=$(mktemp -u /tmp/ttyforge-acc-board.XXXXXX.pty)
mon=$(mktemp -u /tmp/ttyforge-acc-mon.XXXXXX.pty)
scr=$(mktemp -u /tmp/ttyforge-acc-script.XXXXXX.pty)
cleanup() { kill ${SIM_PID:-} ${MUX_PID:-} 2>/dev/null || true; wait 2>/dev/null || true; }
trap cleanup EXIT

start_forge sim --preset uboot --link "$board"; SIM_PID=$FORGE_PID
EXPECT_PORTS=2 start_forge mux "$board" -b 115200 --link "$mon" --link "$scr"; MUX_PID=$FORGE_PID

python3 - "$mon" "$scr" <<'PY' || exit 1
import sys, time, serial
mon = serial.Serial(sys.argv[1], 115200, timeout=1.0)   # a human watching
scr = serial.Serial(sys.argv[2], 115200, timeout=1.0)   # a script driving
time.sleep(0.3); mon.reset_input_buffer(); scr.reset_input_buffer()
scr.write(b"version\r"); scr.flush()
time.sleep(0.6)
a = scr.read(4096).decode(errors="replace")
b = mon.read(4096).decode(errors="replace")
print(f"  script  saw {a!r}")
print(f"  monitor saw {b!r}")
if "U-Boot 2024.01-ttyforge-sim" not in a:
    print("  BAD  the driving consumer missed the reply"); sys.exit(1)
if a != b:
    print("  BAD  the two consumers saw different streams"); sys.exit(1)
print("  ok   both consumers received the identical full stream, echo included")
PY

pass "every consumer gets the whole RX stream, not a share of it"
