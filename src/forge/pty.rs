//! Virtual-port core: create one pty end, publish it, pump bytes through it.
//!
//! Ported from serial-tether (`tether.rs` `create_client_pty` /
//! `run_pty_bridge`, `tetherd/pty.rs` `create_pty`). The four hard-won rules
//! that implementation encodes — every one the answer to a real bug:
//!
//! 1. **Full raw termios on the slave** (`cfmakeraw`, not just `echo=0`).
//!    Anything less and the line discipline eats binary streams: IXON
//!    swallows 0x11/0x13, ICRNL/ONLCR rewrite CR/NL, and ZMODEM dies with
//!    Bad CRC. (Exactly the `socat raw` vs `rawer` failure serial-tether hit
//!    while validating `tether zmodem`.)
//! 2. **Keep the slave fd open in this process.** Otherwise the master hits
//!    EIO the moment a consumer closes the port, and open/close cycles
//!    (minicom restarts, pyserial scripts) kill the bridge.
//! 3. **Nonblocking master + tokio AsyncFd** with manual `libc::read`/`write`
//!    loops — regular File I/O blocks the runtime.
//! 4. **Publish via symlink, RAII-clean it.** Consumers get a stable path
//!    (`--link /tmp/x.pty`) independent of which `/dev/ptys*` slot was free;
//!    a Drop guard removes the link on every exit path.
//!
//! Planned surface (M1):
//!   pub struct VirtualPort { master: AsyncFd<OwnedFd>, slave_keep: OwnedFd, link: PathBuf }
//!   pub fn create(link: Option<&str>) -> Result<VirtualPort>;
//!   impl VirtualPort { async fn read(&self, buf) -> ...; async fn write_all(&self, data) -> ...; }
//!   plus a ready-line printer (one stdout line per port, flushed).
