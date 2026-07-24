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
ttyforge bridge tcp://lab-host:5557             # remote port as a local tty
ttyforge mux /dev/ttyUSB0 --link a.pty --link b.pty   # 1 port → N tools
```

**Status: M1 done** — `pair` works (pyserial- and ZMODEM-verified). See [PLAN.md](PLAN.md) for the roadmap.
Sibling project: [serial-tether](https://github.com/hulryung/serial-tether)
(daemon-based sharing of *real* ports; ttyforge forges the *virtual* side).

Unix only (macOS + Linux). MIT OR Apache-2.0.
