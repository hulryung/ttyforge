#!/usr/bin/env bash
#
# M4a — a remote port becomes a local tty.
#
#   ttyforge sim --preset uboot        the "board", behind a virtual port
#     -> socat TCP-LISTEN … FILE:,rawer   a third-party serial-over-TCP server
#          -> ttyforge bridge tcp://…        the laptop's local tty
#               -> pyserial                     the terminal tool
#
# socat plays the ser2net role deliberately: the point is that the bridge
# talks to something nobody here wrote. (PLAN's original criterion named
# `tetherd --tcp`, which turned out to speak JSON-RPC, not raw bytes.)

. "$(dirname "$0")/lib.sh"
require_forge
need socat "stands in for ser2net"
need_python serial "the terminal tool"

port=$(free_port)
board=$(port_path board)
local_pty=$(port_path local)
cleanup() { kill ${SIM_PID:-} ${SOCAT_PID:-} ${BR_PID:-} 2>/dev/null || true; wait 2>/dev/null || true; }
trap cleanup EXIT

start_forge sim --preset uboot --link "$board"; SIM_PID=$FORGE_PID
socat "TCP-LISTEN:$port,reuseaddr" "FILE:$board,rawer" 2>/dev/null & SOCAT_PID=$!
sleep 0.4   # socat has no readiness contract to wait on
start_forge bridge "tcp://127.0.0.1:$port" --link "$local_pty"; BR_PID=$FORGE_PID

python3 - "$local_pty" <<'PY' || exit 1
import sys, time, serial
p = serial.Serial(sys.argv[1], 115200, timeout=1.0)
def talk(cmd):
    p.reset_input_buffer(); p.write(cmd.encode() + b"\r"); p.flush()
    time.sleep(0.6); return p.read(4096).decode(errors="replace")
checks = [
    ("version", "U-Boot 2024.01-ttyforge-sim"),
    ("printenv baudrate", "baudrate=115200"),
    ("setenv ipaddr 10.0.0.9; printenv ipaddr", "ipaddr=10.0.0.9"),
]
bad = 0
for cmd, want in checks:
    got = talk(cmd)
    if want in got:
        print(f"  ok   {cmd!r} -> {want!r}")
    else:
        print(f"  BAD  {cmd!r} wanted {want!r}, got {got!r}"); bad += 1
sys.exit(1 if bad else 0)
PY

pass "a remote board reached through socat behaves like a local serial port"
