#!/usr/bin/env bash
#
# M4b — line settings reach the real UART.
#
#   pyserial's RFC2217 server  <-  ttyforge bridge --rfc2217  <-  pyserial
#
# A pty has no UART, so baud/parity set on a virtual port is inert. RFC2217 is
# how that intent travels: the bridge polls the port's termios and turns each
# change into a COM-PORT-OPTION subnegotiation. The server here is pyserial's
# own implementation, so the codec is graded by someone else's parser.
#
# Note the platform split this checks by construction: a Linux pty forces CS8
# and clears parity (drivers/tty/pty.c), so only baud, stop bits and flow
# control can be relayed there. The expectations below follow suit.

. "$(dirname "$0")/lib.sh"
require_forge
need_python serial.rfc2217 "the third-party RFC2217 server"

port=$(free_port)
link=$(mktemp -u /tmp/ttyforge-acc-r2217.XXXXXX.pty)
log=$(mktemp)
cleanup() { kill ${SRV_PID:-} ${BR_PID:-} 2>/dev/null || true; wait 2>/dev/null || true; rm -f "$log"; }
trap cleanup EXIT

python3 -u "$(dirname "$0")/rfc2217_server.py" "$port" >"$log" 2>&1 & SRV_PID=$!
for _ in $(seq 1 50); do grep -q READY "$log" && break; sleep 0.1; done
grep -q READY "$log" || fail "the RFC2217 server never started: $(cat "$log")"

start_forge bridge --rfc2217 "tcp://127.0.0.1:$port" --link "$link"; BR_PID=$FORGE_PID

python3 - "$link" <<'PY' || exit 1
import sys, time, serial
p = serial.Serial(sys.argv[1], 9600, bytesize=7, parity='O', stopbits=2, timeout=1.5)
time.sleep(0.5)
payload = bytes([0xFF, 0x01]) + b"hello" + bytes([0xFF, 0xFF, 0x00])
p.write(payload); p.flush()
echo = p.read(len(payload))
if echo != payload:
    print(f"  BAD  telnet framing lost data: sent {payload!r}, got {echo!r}"); sys.exit(1)
print("  ok   0xFF-heavy payload round-tripped through the telnet stream")
p.baudrate = 115200
time.sleep(0.5)
PY
sleep 0.4

python3 - "$log" <<'PY' || exit 1
import re, sys
rows = re.findall(r"SETTINGS \((.*?)\)", open(sys.argv[1]).read())
print("  server observed:", *rows, sep="\n    ")
if not any(r.startswith("9600,") for r in rows):
    print("  BAD  the server never saw the 9600 baud request"); sys.exit(1)
if not any(r.startswith("115200,") for r in rows):
    print("  BAD  the later change to 115200 was not relayed"); sys.exit(1)
# Data bits and parity only survive where the pty keeps them (macOS).
if any(r.startswith("9600, 7, 'O'") for r in rows):
    print("  ok   data bits and parity relayed too (this pty keeps them)")
else:
    print("  ok   baud relayed; data bits/parity not expressible on this pty")
PY

pass "termios changes on a virtual port retune a third-party RFC2217 server"
