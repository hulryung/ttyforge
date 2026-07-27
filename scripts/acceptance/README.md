# Acceptance scripts

Each milestone in [PLAN.md](../../PLAN.md) had to pass against *someone else's*
software before it counted as done. These scripts are those checks, kept
runnable.

The test suite (`cargo test`) covers the logic; it cannot cover interop,
because everything it talks to is also ours. A telnet codec graded by its own
decoder is graded by nobody. So each script here puts a third-party
implementation on the other end of the wire:

| Script | Milestone | The other end |
|---|---|---|
| `bridge-raw.sh` | M4a | `socat TCP-LISTEN … FILE:,rawer`, standing in for ser2net |
| `bridge-rfc2217.sh` | M4b | pyserial's own RFC2217 server (`serial.rfc2217.PortManager`) |
| `mux-fanout.sh` | M5 | two independent pyserial consumers |

## Running them

```sh
cargo build                 # they use target/debug/ttyforge by default
scripts/acceptance/run-all.sh
```

Set `TTYFORGE=/path/to/ttyforge` to check a different build — an installed
release, say.

Exit codes: `0` passed, `1` a real mismatch, `77` skipped because the
third-party tool isn't installed (`socat`, or Python's `pyserial`). `run-all.sh`
fails only on `1`, so a machine without socat still gets a useful run.

## What each one is really guarding

**`bridge-raw.sh`** — that a remote port becomes an ordinary local tty. A fake
U-Boot board sits behind a virtual port, socat serves that port over TCP the
way ser2net would, the bridge dials in, and pyserial drives the board through
the whole chain. PLAN originally specified `tetherd --tcp` as the far end;
that turned out to speak NDJSON/JSON-RPC rather than raw bytes, so no raw
bridge could ever have interoperated with it — the criterion was wrong, and
socat replaced it.

**`bridge-rfc2217.sh`** — that baud/parity/framing set on a virtual port reach
real hardware. It also documents a platform split by construction: a Linux pty
forces `CS8` and clears parity (`drivers/tty/pty.c`), so only baud, stop bits
and flow control can be relayed there, while macOS keeps all five. The script
asserts what the platform can actually express. Its server stubs pyserial's
modem-line reads — the traceback from *not* stubbing them is what originally
confirmed that DTR/RTS cannot be forwarded through a pty.

**`mux-fanout.sh`** — that every consumer receives the whole RX stream. A naive
mux splits the byte stream between consumers instead of copying it, which is
invisible until two tools are attached at once; here a script drives the board
on one port while a monitor watches on another, and the two must see byte-
identical output, the driving tool's echo included.

## In CI, on request

They need `socat` and `pyserial` installed and they measure real timing, so
they are not part of the per-push suite — `cargo test` is what guards each
commit. A manual workflow runs them on both supported systems:

```sh
gh workflow run acceptance.yml      # or the "Run workflow" button in Actions
```

Worth a run before a release, or whenever something on the other end of the
wire changes — that Linux leg is what catches the differences a macOS desk
never shows you.
