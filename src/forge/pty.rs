//! Virtual-port core: create one pty end, publish it, pump bytes through it.
//!
//! Ported from serial-tether (`tether.rs` `create_client_pty` /
//! `run_pty_bridge`, `tetherd/pty.rs` `create_pty`). The five hard-won rules
//! that implementation encodes — every one the answer to a real bug:
//!
//! 1. **Full raw termios on the slave** (`cfmakeraw`, not just `echo=0`).
//!    Anything less and the line discipline eats binary streams: IXON
//!    swallows 0x11/0x13, ICRNL/ONLCR rewrite CR/NL, and ZMODEM dies with
//!    Bad CRC. (Exactly the `socat raw` vs `rawer` failure serial-tether hit
//!    while validating `tether zmodem`.)
//! 2. **Keep the slave fd open in this process.** Otherwise the master hits
//!    EOF/EIO the moment a consumer closes the port, and open/close cycles
//!    (minicom restarts, pyserial scripts) kill the bridge.
//! 3. **Nonblocking master + tokio AsyncFd** with manual `libc::read`/`write`
//!    loops — regular File I/O blocks the runtime.
//! 4. **Publish via symlink, RAII-clean it.** Consumers get a stable path
//!    (`--link /tmp/x.pty`) independent of which `/dev/ptys*` slot was free;
//!    a `.pid` sidecar lets a later invocation reclaim a link whose owner
//!    died, and a Drop guard removes both on every exit path.
//! 5. **Periodically reassert raw termios.** A consumer that opens the port
//!    and leaves it cooked (naive `open()` without termios setup) turns on
//!    ECHO — device bytes reflect back over the bridge as an echo storm —
//!    and ICRNL/ONLCR mangle binary data. Check every ~500ms, quietly fix.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use anyhow::{Context, Result};
use tokio::io::{unix::AsyncFd, Interest};

// The `libc` crate declares `ptsname_r` only for Linux, but the function
// exists with this exact signature in libSystem on macOS 10.13.4+ as well —
// and it is the only reliable way to name a pty slave (see `create`).
extern "C" {
    fn ptsname_r(fd: libc::c_int, buf: *mut libc::c_char, buflen: libc::size_t) -> libc::c_int;
}

/// Marker attachable to an anyhow chain (`.context(SetupError)`) so `main`
/// can map pre-ready failures to exit 3 and everything else to exit 2.
#[derive(Debug)]
pub struct SetupError;

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("setup failed")
    }
}

impl std::error::Error for SetupError {}

/// One end of a virtual wire: the pty master we pump bytes through, plus the
/// slave fd we hold open for the process's lifetime (rule 2). Consumers open
/// the slave path (via a [`Link`]) like a real serial device.
pub struct VirtualPort {
    master: AsyncFd<OwnedFd>,
    /// Never read from or written to — held so the pty survives consumer
    /// open/close cycles, and used to re-check termios (rule 5).
    slave_keep: OwnedFd,
}

impl VirtualPort {
    /// `openpty()` with raw termios applied atomically at creation, resolve
    /// the slave's `/dev/...` path, make the master nonblocking, wrap it in
    /// an AsyncFd. Returns the port and the slave device path.
    pub fn create() -> Result<(Self, String)> {
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        // Raw line discipline from byte one: pass bytes through verbatim, no
        // echo/canonical cooking (rule 1). Passing the termios to openpty
        // itself (rather than tcsetattr afterwards) leaves no window where a
        // fast consumer could open a cooked slave.
        let mut tio: libc::termios = unsafe { std::mem::zeroed() };
        unsafe { libc::cfmakeraw(&mut tio) };
        // SAFETY: out-params are valid; termios is initialized; win size null.
        let r = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                &mut tio,
                std::ptr::null_mut(),
            )
        };
        if r != 0 {
            // openpty() does not reliably set errno on failure — macOS turns a
            // bare last_os_error() into nonsense like "Unknown error: -6" —
            // and the overwhelmingly likely cause is simply running out of
            // ptys, which the system caps (macOS: kern.tty.ptmx_max, 511 by
            // default). Say that instead of printing a garbage errno.
            let os = std::io::Error::last_os_error();
            let detail = match os.raw_os_error() {
                Some(code) if code > 0 => format!(" ({os})"),
                _ => String::new(),
            };
            return Err(anyhow::anyhow!(
                "openpty failed{detail} — the system may be out of ptys \
                 (macOS: sysctl kern.tty.ptmx_max)"
            ))
            .context(SetupError);
        }

        // Resolve the slave device path (e.g. /dev/ttys012) to publish.
        //
        // `ptsname_r(master)`, not the more obvious `ttyname_r(slave)`: on
        // macOS `ttyname_r` resolves the name by looking the device up in
        // /dev, and *every* failure of that lookup is reported as ERANGE — a
        // "buffer too small" errno that has nothing to do with the buffer.
        // Under concurrent pty creation that lookup fails constantly:
        // measured here at 3648 ERANGEs in 4000 calls across ten processes,
        // at every buffer size from 32 bytes to 4 KiB, while `ptsname_r` on
        // the same ptys returned the identical names 4000/4000. It asks the
        // kernel (TIOCPTYGNAME) instead of searching a directory.
        //
        // This is not hypothetical for a tool whose whole job is forging
        // ptys: a `mux` beside a `pair`, or a CI job running both, is exactly
        // the workload that trips it.
        //
        // Needs macOS 10.13.4+ for `ptsname_r`; glibc and musl have long had
        // it. Both platforms yield a path under /dev.
        let mut name_buf = [0 as libc::c_char; 256];
        // SAFETY: master is a valid pty master fd; buffer is sized and
        // outlives the call.
        let rc = unsafe { ptsname_r(master, name_buf.as_mut_ptr(), name_buf.len()) };
        if rc != 0 {
            // Clean up the fds we just opened before bailing.
            // SAFETY: both fds were just produced by openpty above.
            unsafe {
                libc::close(master);
                libc::close(slave);
            }
            return Err(std::io::Error::from_raw_os_error(rc))
                .with_context(|| format!("ptsname_r on pty master (errno {rc})"))
                .context(SetupError);
        }
        // SAFETY: ptsname_r NUL-terminated the buffer on success.
        let slave_name =
            unsafe { std::ffi::CStr::from_ptr(name_buf.as_ptr()) }.to_string_lossy().into_owned();

        // Non-blocking master so AsyncFd can drive it (rule 3).
        // SAFETY: master is a valid fd.
        unsafe {
            let flags = libc::fcntl(master, libc::F_GETFL);
            libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        // SAFETY: master/slave were just produced by openpty and are owned here.
        let master_owned = unsafe { OwnedFd::from_raw_fd(master) };
        let slave_owned = unsafe { OwnedFd::from_raw_fd(slave) };
        let afd = AsyncFd::with_interest(master_owned, Interest::READABLE | Interest::WRITABLE)
            .context("AsyncFd for pty master")
            .context(SetupError)?;
        Ok((Self { master: afd, slave_keep: slave_owned }, slave_name))
    }

    /// Read whatever the consumer wrote into the port. `Ok(0)` means no
    /// consumer is attached right now — callers should idle briefly, not
    /// treat it as EOF (the port outlives consumers; rule 2).
    pub async fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        read_fd(&self.master, buf).await
    }

    /// Write bytes out of the port toward the consumer. Blocks (async) when
    /// the kernel pty buffer is full — natural backpressure when the consumer
    /// isn't draining.
    pub async fn write_all(&self, data: &[u8]) -> std::io::Result<()> {
        write_all_fd(&self.master, data).await
    }

    /// Rule 5: if the attached consumer switched the slave out of raw mode
    /// (`ICANON`/`ECHO`, or `ICRNL`/`ONLCR` munging), reassert `cfmakeraw`
    /// and report `true`. Call on a ~500ms tick.
    pub fn reassert_raw_if_needed(&self) -> bool {
        self.reassert(true)
    }

    /// Rule 5 for `bridge --rfc2217`: fix the line discipline, but leave the
    /// `c_cflag` line parameters (speed, `CSIZE`, `PARENB`/`PARODD`,
    /// `CSTOPB`, `CRTSCTS`) exactly as the consumer set them. Those are the
    /// settings RFC2217 mode exists to relay to the far-end UART, and
    /// `cfmakeraw` resets them to 8N1 — so a consumer that both cooks the
    /// terminal *and* asks for 7O2 would silently lose the 7O2.
    ///
    /// Byte transparency is unaffected: a pty ignores `CSIZE` (a CS7 slave
    /// still passes 0xFF through untouched), so these bits are inert locally
    /// and meaningful only at the real UART on the other end.
    pub fn reassert_line_discipline_if_needed(&self) -> bool {
        self.reassert(false)
    }

    fn reassert(&self, reset_line_params: bool) -> bool {
        let fd = self.slave_keep.as_raw_fd();
        let mut tio: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: slave fd is valid and open for the port's whole life.
        if unsafe { libc::tcgetattr(fd, &mut tio) } != 0 {
            return false; // transient failure; we'll check again next tick
        }
        let cooked = tio.c_lflag & (libc::ICANON | libc::ECHO) != 0
            || tio.c_iflag & libc::ICRNL != 0
            || tio.c_oflag & libc::ONLCR != 0;
        if !cooked {
            return false;
        }
        // SAFETY: reads of an initialized termios we own.
        let (cflag, ispeed, ospeed) =
            unsafe { (tio.c_cflag, libc::cfgetispeed(&tio), libc::cfgetospeed(&tio)) };
        unsafe { libc::cfmakeraw(&mut tio) };
        if !reset_line_params {
            // Linux keeps the speed inside c_cflag, the BSDs in their own
            // fields — restore both so this is one behavior on both.
            tio.c_cflag = cflag;
            // SAFETY: same initialized termios; speeds came from it.
            unsafe {
                libc::cfsetispeed(&mut tio, ispeed);
                libc::cfsetospeed(&mut tio, ospeed);
            }
        }
        // SAFETY: same fd; termios now holds the raw settings we just built.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &tio) == 0 }
    }

    /// Snapshot the slave's termios. A pty master gets no notification when
    /// the slave is reconfigured, so polling this is how a consumer's
    /// baud/parity/framing intent reaches the forge at all (see `rfc2217`).
    pub fn termios(&self) -> Option<libc::termios> {
        let mut tio: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: slave fd is valid and open for the port's whole life.
        if unsafe { libc::tcgetattr(self.slave_keep.as_raw_fd(), &mut tio) } != 0 {
            return None;
        }
        Some(tio)
    }

    /// The raw slave fd, for tests that inspect termios directly.
    #[cfg(test)]
    pub fn slave_raw_fd(&self) -> RawFd {
        self.slave_keep.as_raw_fd()
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Rule 3, once: nonblocking fd + tokio AsyncFd + a manual libc loop. Shared
// by the pty master here and by the real serial port in `serial.rs` — the
// retry-on-EWOULDBLOCK dance is subtle enough to deserve one implementation.
// ──────────────────────────────────────────────────────────────────────────

pub(super) async fn read_fd(afd: &AsyncFd<OwnedFd>, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        let mut guard = afd.readable().await?;
        let raw = afd.get_ref().as_raw_fd();
        // SAFETY: raw fd is valid; buf is a writable slice.
        let n = unsafe { libc::read(raw, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::WouldBlock {
            guard.clear_ready();
            continue;
        }
        return Err(e);
    }
}

pub(super) async fn write_all_fd(afd: &AsyncFd<OwnedFd>, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        let mut guard = afd.writable().await?;
        let raw = afd.get_ref().as_raw_fd();
        // SAFETY: raw fd is valid; data is a readable slice.
        let n = unsafe { libc::write(raw, data.as_ptr() as *const _, data.len()) };
        if n >= 0 {
            data = &data[n as usize..];
            continue;
        }
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::WouldBlock {
            guard.clear_ready();
            continue;
        }
        return Err(e);
    }
    Ok(())
}

/// A claimed symlink (`/tmp/…​.pty` → `/dev/ttysNNN`) plus its `.pid`
/// sidecar; both removed on Drop, every exit path (rule 4).
pub struct Link {
    path: String,
}

impl Link {
    /// Claim a stable path for the port: `--link PATH` verbatim, or the
    /// default sequence `/tmp/ttyforge-<tag>.pty`, `-2`, `-3`, … A busy path
    /// whose recorded owner is confirmed dead is reclaimed transparently.
    pub fn claim(explicit: Option<&str>, tag: &str, slave: &str) -> Result<Self> {
        let path = match explicit {
            Some(p) => {
                if !try_claim_link(p, slave)? {
                    // Tagged like every other claim failure so callers don't
                    // have to re-tag (and double up the "setup failed:" text).
                    return Err(anyhow::anyhow!("--link {p} is busy (claimed by a live process)"))
                        .context(SetupError);
                }
                p.to_string()
            }
            None => {
                let mut claimed = None;
                for n in 1..=9999u32 {
                    let candidate = default_link(tag, n);
                    if try_claim_link(&candidate, slave)? {
                        claimed = Some(candidate);
                        break;
                    }
                }
                claimed
                    .ok_or_else(|| {
                        anyhow::anyhow!("no free /tmp/ttyforge-{tag}*.pty slot (9999 in use?)")
                    })
                    .context(SetupError)?
            }
        };
        if let Err(e) = std::fs::write(format!("{path}.pid"), std::process::id().to_string()) {
            let _ = std::fs::remove_file(&path);
            return Err(e).with_context(|| format!("write {path}.pid")).context(SetupError);
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}.pid", self.path));
    }
}

/// Default symlink path for the `n`th port of a tag; `n=1` gets the clean
/// unnumbered form (`/tmp/ttyforge-a.pty`), later slots `-2`, `-3`, …
fn default_link(tag: &str, n: u32) -> String {
    if n == 1 {
        format!("/tmp/ttyforge-{tag}.pty")
    } else {
        format!("/tmp/ttyforge-{tag}-{n}.pty")
    }
}

/// `kill(pid, 0)` liveness probe: `true` unless the pid is confirmed gone
/// (`ESRCH`). `EPERM` (exists, owned by someone else) and any other
/// unexpected errno are treated as "alive" — never guess a link is free.
fn process_alive(pid: i32) -> bool {
    // SAFETY: signal 0 probes existence; it delivers nothing.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Pure decision behind the `.pid` sidecar staleness check: given the
/// sidecar's raw contents (`None` if missing/unreadable) and a liveness
/// probe, decide whether the recorded holder is confirmed gone. `None`
/// means "can't tell" — the caller must treat that the same as "alive".
fn pid_sidecar_is_stale(contents: Option<&str>, probe: impl Fn(i32) -> bool) -> Option<bool> {
    let pid: i32 = contents?.trim().parse().ok()?;
    Some(!probe(pid))
}

/// If `<link>.pid` names a process that's confirmed gone, remove the stale
/// link + sidecar and report `true` (caller should retry the claim).
fn reclaim_if_stale(link: &str) -> bool {
    let sidecar = format!("{link}.pid");
    let contents = std::fs::read_to_string(&sidecar).ok();
    if pid_sidecar_is_stale(contents.as_deref(), process_alive) == Some(true) {
        let _ = std::fs::remove_file(link);
        let _ = std::fs::remove_file(&sidecar);
        true
    } else {
        false
    }
}

/// Atomically claim `link` -> `slave`. A live collision reports `Ok(false)`
/// ("busy, try something else"); a stale one is reclaimed and retried
/// transparently. Never removes a link without first proving its holder is
/// dead — a stale link can point at a pty slave the OS has since recycled
/// to an unrelated process.
fn try_claim_link(link: &str, slave: &str) -> Result<bool> {
    loop {
        match std::os::unix::fs::symlink(slave, link) {
            Ok(()) => return Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if reclaim_if_stale(link) {
                    continue; // same path, now clear — retry once
                }
                return Ok(false);
            }
            Err(e) => return Err(e).with_context(|| format!("symlink {link}")).context(SetupError),
        }
    }
}

// NOTE: the byte pump lives in `wire.rs` (`wire::pump`) — it is wire-aware
// since M3, and an identity `WireSpec` degrades to the plain M1 behavior:
// byte-for-byte, no added delay, kernel-buffer backpressure (never drops).

#[cfg(test)]
mod tests {
    use super::*;

    /// Rule 1, pinned: the slave must be fully raw from the moment it exists —
    /// every flag here is one that mangled real traffic somewhere.
    #[tokio::test]
    async fn slave_termios_is_fully_raw() {
        let (port, _name) = VirtualPort::create().expect("create");
        let mut tio: libc::termios = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::tcgetattr(port.slave_raw_fd(), &mut tio) };
        assert_eq!(rc, 0, "tcgetattr");
        assert_eq!(tio.c_lflag & libc::ICANON, 0, "ICANON must be off (line buffering)");
        assert_eq!(tio.c_lflag & libc::ECHO, 0, "ECHO must be off (echo storm)");
        assert_eq!(tio.c_iflag & libc::IXON, 0, "IXON must be off (eats 0x11/0x13)");
        assert_eq!(tio.c_iflag & libc::ICRNL, 0, "ICRNL must be off (rewrites CR)");
        assert_eq!(tio.c_iflag & libc::INLCR, 0, "INLCR must be off (rewrites NL)");
        assert_eq!(tio.c_oflag & libc::OPOST, 0, "OPOST must be off (output cooking)");
        assert_eq!(tio.c_oflag & libc::ONLCR, 0, "ONLCR must be off (NL→CRNL)");
    }

    /// Rule 5: a consumer flipping the slave to cooked mode gets corrected.
    #[tokio::test]
    async fn reassert_raw_restores_cooked_slave() {
        let (port, _name) = VirtualPort::create().expect("create");
        // Sabotage: turn echo/canonical back on, as a naive tool would.
        let fd = port.slave_raw_fd();
        let mut tio: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut tio) }, 0);
        tio.c_lflag |= libc::ICANON | libc::ECHO;
        assert_eq!(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &tio) }, 0);

        assert!(port.reassert_raw_if_needed(), "should detect + fix cooked slave");
        assert!(!port.reassert_raw_if_needed(), "second check: already raw");
    }

    /// Rule 5 in RFC2217 mode: the line discipline is forced back to raw, but
    /// the line parameters the far-end UART needs survive.
    #[tokio::test]
    async fn reassert_line_discipline_keeps_line_params() {
        let (port, _name) = VirtualPort::create().expect("create");
        let fd = port.slave_raw_fd();
        let mut tio: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut tio) }, 0);
        // A consumer asking for 9600 7O2 — and cooking the terminal on the way.
        unsafe {
            libc::cfsetispeed(&mut tio, libc::B9600);
            libc::cfsetospeed(&mut tio, libc::B9600);
        }
        tio.c_cflag =
            (tio.c_cflag & !libc::CSIZE) | libc::CS7 | libc::PARENB | libc::PARODD | libc::CSTOPB;
        tio.c_lflag |= libc::ICANON | libc::ECHO;
        assert_eq!(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &tio) }, 0);

        assert!(port.reassert_line_discipline_if_needed(), "cooked slave is fixed");
        let after = port.termios().expect("termios");
        assert_eq!(after.c_lflag & (libc::ICANON | libc::ECHO), 0, "raw again");
        assert_eq!(after.c_cflag & libc::CSIZE, libc::CS7, "CS7 kept");
        assert!(after.c_cflag & libc::PARODD != 0, "odd parity kept");
        assert!(after.c_cflag & libc::CSTOPB != 0, "two stop bits kept");
        assert_eq!(unsafe { libc::cfgetospeed(&after) }, libc::B9600, "speed kept");

        // …while plain rule 5 resets them to 8N1, which is what pair/sim want.
        assert_eq!(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &tio) }, 0);
        assert!(port.reassert_raw_if_needed());
        let after = port.termios().expect("termios");
        assert_eq!(after.c_cflag & libc::CSIZE, libc::CS8, "plain rule 5 → CS8");
    }

    /// Regression: resolving the slave path must survive several threads
    /// forging ports at once — `ttyforge mux` next to a `ttyforge pair`, or
    /// one CI job doing both.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn slave_paths_resolve_under_concurrent_churn() {
        // Concurrency is what reproduces the bug, not volume: ttyname_r failed
        // ~90% of the time under it. Kept small on purpose — the system caps
        // ptys (macOS: 511), and a greedier test starves the rest of the suite
        // instead of testing anything.
        let mut tasks = Vec::new();
        for _ in 0..4 {
            // Report rather than panic: a panicking blocking task tears the
            // runtime down and the *next* port creation fails with a confusing
            // "context is shutting down" instead of the real cause.
            tasks.push(tokio::task::spawn_blocking(|| {
                for _ in 0..12 {
                    match VirtualPort::create() {
                        Ok((_port, name)) if name.starts_with("/dev/") => {}
                        Ok((_port, name)) => return Err(format!("implausible path {name:?}")),
                        Err(e) => return Err(format!("{e:#}")),
                    }
                }
                Ok(())
            }));
        }
        let mut failures = Vec::new();
        for t in tasks {
            if let Err(e) = t.await.expect("churn task") {
                failures.push(e);
            }
        }
        assert!(failures.is_empty(), "forging ports concurrently failed: {failures:#?}");
    }

    #[test]
    fn sidecar_staleness_decisions() {
        let alive = |_pid: i32| true;
        let dead = |_pid: i32| false;
        assert_eq!(pid_sidecar_is_stale(None, dead), None, "missing sidecar → can't tell");
        assert_eq!(pid_sidecar_is_stale(Some("junk"), dead), None, "garbage → can't tell");
        assert_eq!(pid_sidecar_is_stale(Some("123\n"), alive), Some(false));
        assert_eq!(pid_sidecar_is_stale(Some(" 123 "), dead), Some(true));
    }

    #[test]
    fn link_claim_writes_sidecar_and_drop_cleans_up() {
        let dir = std::env::temp_dir().join(format!("ttyforge-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("port.pty");
        let path_s = path.to_str().unwrap();

        let link = Link::claim(Some(path_s), "x", "/dev/null").expect("claim");
        assert_eq!(link.path(), path_s);
        assert!(path.symlink_metadata().is_ok(), "symlink exists");
        let pid: i32 =
            std::fs::read_to_string(format!("{path_s}.pid")).unwrap().trim().parse().unwrap();
        assert_eq!(pid as u32, std::process::id());

        // A second claim on the same path must refuse — we're alive.
        assert!(Link::claim(Some(path_s), "x", "/dev/null").is_err());

        drop(link);
        assert!(path.symlink_metadata().is_err(), "symlink removed");
        assert!(!std::path::Path::new(&format!("{path_s}.pid")).exists(), "sidecar removed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
