# ttyforge

> Forge virtual serial ports: null-modem pairs, scripted device simulators,
> network bridges, and port multiplexers — a socat/ser2net replacement built
> for serial work.

Point pyserial, minicom, tio, or any serial tool at a `ttyforge` port and it
behaves like real hardware — full raw termios (binary-safe for ZMODEM and
firmware blobs), stable symlink paths, survives open/close cycles.

```sh
ttyforge pair                                   # virtual null-modem cable
ttyforge sim --preset uboot                     # fake U-Boot behind a port
ttyforge sim -- python3 fake_board.py           # your program IS the device
ttyforge bridge tcp://lab-host:2000             # remote ser2net port as a local tty
ttyforge bridge listen://:7000                  # …or wait for the far side to dial in
ttyforge bridge --rfc2217 tcp://lab-host:2000   # …and let baud/parity reach the real UART
ttyforge mux /dev/ttyUSB0 --link a.pty --link b.pty   # 1 port → N tools

# The wire model — make the virtual wire as slow and dirty as a real one:
ttyforge pair --baud-sim 9600 --latency 5ms --drop 0.001 --seed 42
```

## Install

```sh
brew install hulryung/tap/ttyforge     # macOS / Linuxbrew
cargo install ttyforge                 # anywhere with a Rust toolchain
```

## How a script drives it

Every forge runs in the foreground and prints its port path(s) to stdout the
moment they are usable — one line per port, flushed, then nothing else. That
is the whole readiness protocol:

```sh
{ read A; read B; } < <(ttyforge pair)     # both ports exist by the time read returns
```

`--json` swaps those lines for one object, which adds what the plain form
cannot say — including a seed that was generated for you, the one value that
replays a lossy-wire failure:

```sh
$ ttyforge pair --json --drop 0.001
{"forge":"pair","pid":4711,"ports":["/tmp/ttyforge-a.pty","/tmp/ttyforge-b.pty"],
 "wire":{"drop":0.001,"seed":270278378129984}}
```

Ctrl-C or SIGTERM tears everything down and removes the symlinks. Exit codes:
`0` clean, `1` usage, `2` runtime, `3` setup failed before the port existed —
so a script that got a readiness line knows the port was real.

## Where the ports live

The path a forge publishes is a symlink; what it points at is already a real
device node:

```sh
$ readlink /tmp/ttyforge-a.pty
/dev/ttys019          # /dev/pts/3 on Linux
```

Tools may open either one. The node is the kernel's, and its number changes
every run — which is the whole reason `--link` exists.

`--link` takes any path, so a stable name can live wherever you want it:
`--link ~/board.pty`, `--link /usr/local/var/run/board.pty`. Under `/dev`,
that depends on the system:

- **Linux** — works as root, since `/dev` is a normal filesystem and this is
  the same thing udev does for `/dev/serial/by-id/*`:
  `sudo ttyforge pair --link /dev/ttyforge0 --link /dev/ttyforge1`.
- **macOS** — not possible. `/dev` is devfs, which refuses to create entries
  at all: as root, both `ln -s` and `touch` there fail with `EPERM` rather
  than a permission error, and the only symlinks in `/dev` are the three the
  kernel makes at boot. Put the link anywhere else, or open `/dev/ttysNNN`
  directly.

A genuine macOS `cu.*` / `tty.*` pair is further out of reach: those two nodes
are created together by an IOKit serial-family driver, so producing one means
shipping a DriverKit driver or kext — the same kernel-driver territory that
puts Windows virtual COM ports out of scope. A symlink named `cu.something`
would carry the name without the semantics.

## Recipes

**Test a serial app with no hardware.** One end for your code, one for a fake
device:

```sh
ttyforge pair --link /tmp/app.pty --link /tmp/dev.pty
```

**Make the fake device a program you write.** Its stdin is what the port
receives, its stdout is what the port sends — so a board simulator is 20 lines
of Python (use `python3 -u`; line buffering will otherwise eat your replies):

```sh
ttyforge sim --link /tmp/board.pty -- python3 -u fake_board.py
```

**Prove your retry logic works.** A real cable drops bytes; a pty never does.
Same seed, same failure, every run:

```sh
ttyforge pair --baud-sim 9600 --drop 0.002 --seed 7
```

**Use a lab machine's port from your laptop.** Anything that serves a serial
port as raw TCP works — ser2net, esp-link, `socat TCP-LISTEN:2000
FILE:/dev/ttyUSB0,rawer`:

```sh
ttyforge bridge tcp://lab-host:2000 --link /tmp/board.pty
```

The port outlives the peer: reboot the lab machine and the bridge redials
while minicom stays attached to the same path. Add `--rfc2217` and the
baud/parity/framing your tool sets locally is applied to the *real* UART.

**Watch a port while a script drives it.** Every consumer gets the whole RX
stream, not a share of it:

```sh
ttyforge mux /dev/ttyUSB0 -b 115200 --link /tmp/monitor.pty --link /tmp/script.pty
```

## Verifying it against other people's tools

`cargo test` covers the logic. It cannot cover interop, because everything it
talks to is also ours — so each milestone's acceptance check lives in
[`scripts/acceptance/`](scripts/acceptance) with a third-party implementation
on the other end of the wire: socat standing in for ser2net, pyserial's own
RFC2217 server, pyserial as the terminal tool.

```sh
cargo build && scripts/acceptance/run-all.sh
```

## Status

**v0.1.0 released — every forge works.** `pair`, `sim`, `bridge` (raw TCP *and*
RFC2217), `mux`, and the wire model (`--baud-sim` / `--latency` / `--jitter` /
`--drop` / `--corrupt` / `--seed`). ZMODEM survives a seeded lossy wire via
retransmission; RFC2217 is verified against pyserial's RFC2217 server; the
mux fan-out is verified against the exact 200-byte case that splits under a
naive implementation. See [PLAN.md](PLAN.md) for the design log — every
milestone records what it was measured against and what it got wrong.

Sibling project: [serial-tether](https://github.com/hulryung/serial-tether)
(daemon-based sharing of *real* ports; ttyforge forges the *virtual* side).
Rule of thumb: if you need daemons, sessions, locking or remote clients, use
tether; if you need a port that does not exist yet, use ttyforge.

## Limits

- **Unix only** (macOS + Linux). Windows virtual COM ports are a kernel-driver
  problem (com0com territory), deliberately out of scope.
- **A pty has no UART.** A baud rate set on a virtual port is a no-op locally —
  `--baud-sim` simulates the timing, and `bridge --rfc2217` forwards the real
  thing to a remote UART.
- **DTR/RTS cannot be forwarded.** They are modem lines, and a pty has none to
  observe.
- **No `/dev` name of your choosing on macOS**, and no real `cu.*`/`tty.*`
  pair on any system — see [Where the ports live](#where-the-ports-live).
- **`--rfc2217` relays less on Linux.** Its pty driver forces `CS8` and clears
  parity on every `tcsetattr`, so data bits and parity never reach the forge to
  be forwarded; baud, stop bits and flow control do. macOS keeps all five.

MIT OR Apache-2.0.
