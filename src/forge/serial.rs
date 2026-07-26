//! The real serial port behind `mux` (M5).
//!
//! Ported from serial-tether's `tetherd/serial.rs` `FdPort`: open the device
//! nonblocking, put it in raw termios, and drive it with the same AsyncFd +
//! manual `libc` loop the pty core uses (rule 3 in `pty.rs`).
//!
//! PLAN pencilled in `tokio-serial` for this. It isn't here: the FdPort
//! pattern already exists in this repo, and opening a tty is `open` plus
//! `tcsetattr` — while `tokio-serial` brings `mio-serial` + `serialport`,
//! which wants libudev on Linux. A single binary heading for homebrew and
//! crates.io in M6 shouldn't grow a system-library dependency to call two
//! syscalls it already knows how to call.
//!
//! One useful consequence: this opens *any* tty path, so `mux` can be tested
//! end-to-end against a pty on a machine with no hardware attached.
//!
//! Deliberately absent: `TIOCEXCL`. Locking a device against other openers is
//! serial-tether's job (sessions, leases, remote clients); `mux` is the quick
//! local splitter and stays out of the way.

use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use anyhow::{bail, Context, Result};
use tokio::io::{unix::AsyncFd, Interest};

use super::pty::{read_fd, write_all_fd, SetupError};

/// A real serial device (or any tty path), owned for the process's lifetime.
pub struct SerialPort {
    fd: AsyncFd<OwnedFd>,
}

impl SerialPort {
    pub fn open(path: &str, baud: u32) -> Result<Self> {
        let speed = speed_of(baud).with_context(|| {
            format!("baud {baud} has no termios constant on this platform")
        })?;

        let c_path = std::ffi::CString::new(path)
            .with_context(|| format!("device path {path:?} contains a NUL"))
            .context(SetupError)?;
        // O_NONBLOCK for rule 3, and because opening a port whose carrier is
        // down would otherwise block forever waiting for DCD. O_NOCTTY so the
        // device never becomes this process's controlling terminal.
        // SAFETY: c_path is a valid NUL-terminated string.
        let raw: RawFd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("open {path}"))
                .context(SetupError);
        }
        // SAFETY: fd was just produced by open() and is owned here — from now
        // on every early return closes it via OwnedFd's Drop.
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };

        configure(&owned, path, speed)?;

        let fd = AsyncFd::with_interest(owned, Interest::READABLE | Interest::WRITABLE)
            .with_context(|| format!("AsyncFd for {path}"))
            .context(SetupError)?;
        Ok(Self { fd })
    }

    pub async fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        read_fd(&self.fd, buf).await
    }

    pub async fn write_all(&self, data: &[u8]) -> std::io::Result<()> {
        write_all_fd(&self.fd, data).await
    }
}

/// Raw termios at the requested speed. Same reasoning as the pty core's rule
/// 1 — a line discipline between the forge and the device would eat 0x11/0x13
/// and rewrite CR/NL, which is fatal for firmware blobs and ZMODEM.
fn configure(fd: &OwnedFd, path: &str, speed: libc::speed_t) -> Result<()> {
    use std::os::fd::AsRawFd as _;
    let raw = fd.as_raw_fd();
    let mut tio: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: raw is a valid tty fd; tio is ours to fill.
    if unsafe { libc::tcgetattr(raw, &mut tio) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("{path} is not a tty"))
            .context(SetupError);
    }
    unsafe { libc::cfmakeraw(&mut tio) };
    // CLOCAL: we are not a modem, so don't let a missing carrier stall reads.
    // CREAD: actually receive. VMIN/VTIME 0: never block in the driver —
    // readiness is AsyncFd's business.
    tio.c_cflag |= libc::CLOCAL | libc::CREAD;
    tio.c_cc[libc::VMIN] = 0;
    tio.c_cc[libc::VTIME] = 0;
    // SAFETY: tio is initialized; speed came from the table below.
    unsafe {
        libc::cfsetispeed(&mut tio, speed);
        libc::cfsetospeed(&mut tio, speed);
    }
    // SAFETY: same fd, fully built termios.
    if unsafe { libc::tcsetattr(raw, libc::TCSANOW, &tio) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("configure {path}"))
            .context(SetupError);
    }

    // tcsetattr reports success even when the driver quietly refused part of
    // the request, so read it back — a port silently running at the wrong
    // baud is a long afternoon.
    let mut check: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: same fd; check is ours to fill.
    if unsafe { libc::tcgetattr(raw, &mut check) } == 0 {
        let got = unsafe { libc::cfgetospeed(&check) };
        if got != speed {
            tracing::warn!(
                path,
                requested = baud_of(speed),
                actual = baud_of(got),
                "device refused the requested baud"
            );
        }
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// Baud <-> speed_t
//
// Linux encodes speeds as small indices (`B9600` == 13); the BSDs — macOS
// included — store the number itself (`B9600` == 9600). Everything that
// touches termios speeds goes through this pair, including `rfc2217`.
// ──────────────────────────────────────────────────────────────────────────

const COMMON: &[(libc::speed_t, u32)] = &[
    (libc::B0, 0),
    (libc::B50, 50),
    (libc::B75, 75),
    (libc::B110, 110),
    (libc::B134, 134),
    (libc::B150, 150),
    (libc::B200, 200),
    (libc::B300, 300),
    (libc::B600, 600),
    (libc::B1200, 1200),
    (libc::B1800, 1800),
    (libc::B2400, 2400),
    (libc::B4800, 4800),
    (libc::B9600, 9600),
    (libc::B19200, 19200),
    (libc::B38400, 38400),
    (libc::B57600, 57600),
    (libc::B115200, 115200),
    (libc::B230400, 230400),
];

#[cfg(target_os = "linux")]
const EXTRA: &[(libc::speed_t, u32)] = &[
    (libc::B460800, 460800),
    (libc::B500000, 500000),
    (libc::B576000, 576000),
    (libc::B921600, 921600),
    (libc::B1000000, 1_000_000),
    (libc::B1152000, 1_152_000),
    (libc::B1500000, 1_500_000),
    (libc::B2000000, 2_000_000),
];
#[cfg(not(target_os = "linux"))]
const EXTRA: &[(libc::speed_t, u32)] = &[];

/// `speed_t` → bits per second. An unknown code is taken at face value,
/// which is right on the BSDs (where arbitrary rates are legal) and
/// unreachable on Linux, where every code is in the table.
pub fn baud_of(speed: libc::speed_t) -> u32 {
    COMMON
        .iter()
        .chain(EXTRA)
        .find(|(code, _)| *code == speed)
        .map(|(_, baud)| *baud)
        .unwrap_or(speed as u32)
}

/// Bits per second → `speed_t`. On the BSDs any rate is legal, so an
/// unlisted one passes straight through; on Linux there is no encoding for
/// it and the caller gets a usage error instead of a silently wrong port.
pub fn speed_of(baud: u32) -> Option<libc::speed_t> {
    if let Some((code, _)) = COMMON.iter().chain(EXTRA).find(|(_, b)| *b == baud) {
        return Some(*code);
    }
    #[cfg(target_os = "linux")]
    {
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        Some(baud as libc::speed_t)
    }
}

/// Reject a device path before anything is built, with a message that says
/// what to do about it.
pub fn check_baud(baud: u32) -> Result<()> {
    if speed_of(baud).is_none() {
        bail!("baud {baud} has no termios encoding on this platform (try 9600, 115200, 921600…)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baud_and_speed_round_trip() {
        for baud in [0, 300, 9600, 19200, 115200, 230400] {
            let code = speed_of(baud).expect("standard rate");
            assert_eq!(baud_of(code), baud, "{baud} must survive the round trip");
        }
        // The rates this project actually types most often.
        assert_eq!(baud_of(speed_of(115200).unwrap()), 115200);
    }

    #[test]
    fn opening_a_non_tty_is_a_setup_error() {
        let err = SerialPort::open("/dev/null", 115200)
            .map(|_| ())
            .expect_err("/dev/null is not a tty");
        assert!(
            err.chain().any(|c| c.to_string().contains("not a tty")),
            "explain what's wrong: {err:#}"
        );
        assert!(err.downcast_ref::<SetupError>().is_some(), "must exit 3");
    }

    #[test]
    fn opening_a_missing_device_is_a_setup_error() {
        let err = SerialPort::open("/dev/definitely-not-here", 115200)
            .map(|_| ())
            .expect_err("no device");
        assert!(err.downcast_ref::<SetupError>().is_some(), "must exit 3");
    }

    /// A pty is a tty, so the real-port code opens one — which is exactly why
    /// `mux` is testable on a machine with no hardware attached.
    #[tokio::test]
    async fn opens_a_pty_and_moves_bytes() {
        let (port, path) = super::super::pty::VirtualPort::create().expect("pty");
        let dev = SerialPort::open(&path, 115200).expect("open pty as a device");
        port.write_all(b"from the device").await.expect("write");
        let mut buf = [0u8; 32];
        let n = dev.read(&mut buf).await.expect("read");
        assert_eq!(&buf[..n], b"from the device");

        dev.write_all(b"to the device").await.expect("write");
        let mut buf = [0u8; 32];
        let n = port.read(&mut buf).await.expect("read");
        assert_eq!(&buf[..n], b"to the device");
    }
}
