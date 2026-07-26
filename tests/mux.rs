//! Integration tests: `ttyforge mux` — one device, N virtual ttys.
//!
//! No hardware required: the test makes a pty and hands `mux` its slave path,
//! which the forge opens exactly as it would `/dev/ttyUSB0`. Driving the
//! master is then indistinguishable from a board talking.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::fd::FromRawFd as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ttyforge");

/// The stand-in device. Holds its own slave fd open for the same reason the
/// forge does (rule 2): the pty must survive `mux` opening and closing it.
struct FakeDevice {
    master: std::fs::File,
    _slave: std::fs::File,
    path: String,
}

fn fake_device() -> FakeDevice {
    let (mut m, mut s): (libc::c_int, libc::c_int) = (-1, -1);
    let mut tio: libc::termios = unsafe { std::mem::zeroed() };
    unsafe { libc::cfmakeraw(&mut tio) };
    // SAFETY: out-params are valid; termios is initialized; win size null.
    let rc = unsafe {
        libc::openpty(&mut m, &mut s, std::ptr::null_mut(), &mut tio, std::ptr::null_mut())
    };
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    let mut name = [0 as libc::c_char; 256];
    // SAFETY: s is a valid tty fd; the buffer is sized.
    assert_eq!(unsafe { libc::ttyname_r(s, name.as_mut_ptr(), name.len()) }, 0);
    // SAFETY: ttyname_r NUL-terminated the buffer on success.
    let path = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: both fds come from openpty and are owned here.
    unsafe {
        FakeDevice {
            master: std::fs::File::from_raw_fd(m),
            _slave: std::fs::File::from_raw_fd(s),
            path,
        }
    }
}

struct MuxProc {
    child: Child,
    links: Vec<String>,
}

impl Drop for MuxProc {
    fn drop(&mut self) {
        // SAFETY: our own child's pid.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGKILL) };
        let _ = self.child.wait();
        for l in &self.links {
            let _ = std::fs::remove_file(l);
            let _ = std::fs::remove_file(format!("{l}.pid"));
        }
    }
}

/// Start `ttyforge mux DEVICE --link … --link …` and wait for one ready line
/// per link.
fn start_mux(tag: &str, device: &str, count: usize) -> MuxProc {
    let links: Vec<String> = (0..count)
        .map(|i| format!("/tmp/ttyforge-test-mux-{}-{tag}-{i}.pty", std::process::id()))
        .collect();
    let mut args = vec!["mux".to_string(), device.to_string()];
    for l in &links {
        args.push("--link".into());
        args.push(l.clone());
    }
    let mut child = Command::new(BIN)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ttyforge mux");
    let stdout = child.stdout.take().expect("stdout piped");
    let mut lines = BufReader::new(stdout).lines();
    for (i, want) in links.iter().enumerate() {
        let got = lines.next().expect("ready line").expect("read ready line");
        assert_eq!(&got, want, "ready line {i} is the link path, in order");
    }
    MuxProc { child, links }
}

fn open_port(path: &str) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path)
        .expect("open pty slave via symlink")
}

/// Read exactly `n` bytes with a deadline, on a thread — a mux that drops
/// bytes should fail the test, not hang it.
fn read_within(f: &std::fs::File, n: usize, within: Duration) -> Vec<u8> {
    let mut fd = f.try_clone().expect("dup fd");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut got = vec![0u8; n];
        let _ = tx.send(fd.read_exact(&mut got).map(|()| got));
    });
    rx.recv_timeout(within)
        .expect("timed out reading")
        .expect("read failed")
}

/// The M5 acceptance gate: **every consumer gets a full copy**. The 200-byte
/// payload is the exact case serial-tether measured splitting 128/72 across
/// two consumers when they shared one read of the device.
#[test]
fn every_consumer_receives_the_whole_rx_stream() {
    let mut dev = fake_device();
    let mux = start_mux("fanout", &dev.path, 2);
    let a = open_port(&mux.links[0]);
    let b = open_port(&mux.links[1]);

    let payload: Vec<u8> = (0u8..=199).collect();
    dev.master.write_all(&payload).expect("device write");
    dev.master.flush().expect("flush");

    let got_a = read_within(&a, payload.len(), Duration::from_secs(5));
    let got_b = read_within(&b, payload.len(), Duration::from_secs(5));
    assert_eq!(got_a, payload, "consumer A must see all 200 bytes");
    assert_eq!(got_b, payload, "consumer B must see all 200 bytes");
}

/// Same guarantee under volume, with every byte value and more data than a
/// pty buffer holds — so the copies are really independent, not one buffer
/// two readers happen to share.
#[test]
fn fan_out_survives_more_data_than_a_pty_buffer() {
    let dev = fake_device();
    let mux = start_mux("volume", &dev.path, 3);
    let ports: Vec<_> = mux.links.iter().map(|l| open_port(l)).collect();

    let payload: Vec<u8> = (0u8..=255).cycle().take(16 * 1024).collect();
    // Readers first: 16 KiB is far past the ~2-3 KiB kernel pty buffer, so a
    // write-then-read would deadlock on backpressure.
    let readers: Vec<_> = ports
        .iter()
        .map(|p| {
            let mut fd = p.try_clone().expect("dup");
            let n = payload.len();
            std::thread::spawn(move || {
                let mut got = vec![0u8; n];
                fd.read_exact(&mut got).map(|()| got)
            })
        })
        .collect();

    let to_send = payload.clone();
    let mut master = dev.master.try_clone().expect("dup master");
    let writer = std::thread::spawn(move || {
        master.write_all(&to_send).expect("device write");
        master.flush().expect("flush");
    });

    for (i, r) in readers.into_iter().enumerate() {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !r.is_finished() {
            assert!(Instant::now() < deadline, "consumer {i} never got its copy");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(r.join().expect("reader thread").expect("read"), payload, "consumer {i}");
    }
    writer.join().expect("writer thread");
    drop(dev.master);
}

/// TX merges toward the device, and a consumer's write arrives whole: two
/// tools typing at once interleave (as they would on a real cable), but never
/// mid-chunk.
#[test]
fn tx_from_every_consumer_reaches_the_device_intact() {
    let dev = fake_device();
    let mux = start_mux("tx", &dev.path, 2);
    let mut a = open_port(&mux.links[0]);
    let mut b = open_port(&mux.links[1]);

    let from_a = vec![b'A'; 64];
    let from_b = vec![b'B'; 64];
    a.write_all(&from_a).expect("A writes");
    a.flush().expect("flush");
    b.write_all(&from_b).expect("B writes");
    b.flush().expect("flush");

    let got = read_within(&dev.master, 128, Duration::from_secs(5));
    let (first, second) = got.split_at(64);
    assert!(
        (first == from_a && second == from_b) || (first == from_b && second == from_a),
        "each consumer's chunk must arrive contiguously, got {:?}",
        String::from_utf8_lossy(&got)
    );
}

/// A consumer closing and reopening its port must not disturb the others —
/// rule 2, now with siblings watching.
#[test]
fn one_consumer_reopening_does_not_disturb_the_others() {
    let mut dev = fake_device();
    let mux = start_mux("reopen", &dev.path, 2);
    let steady = open_port(&mux.links[1]);

    for round in 0u8..3 {
        let transient = open_port(&mux.links[0]);
        dev.master.write_all(&[round]).expect("device write");
        dev.master.flush().expect("flush");
        assert_eq!(read_within(&transient, 1, Duration::from_secs(5)), vec![round]);
        assert_eq!(
            read_within(&steady, 1, Duration::from_secs(5)),
            vec![round],
            "round {round}: the steady consumer keeps receiving"
        );
        drop(transient);
    }
}

/// Teardown contract: clean exit on SIGTERM, every link and sidecar removed.
#[test]
fn sigterm_cleans_up_every_link() {
    let dev = fake_device();
    let mut mux = start_mux("teardown", &dev.path, 3);

    // SAFETY: our own child's pid.
    unsafe { libc::kill(mux.child.id() as i32, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match mux.child.try_wait().expect("try_wait") {
            Some(status) => {
                assert!(status.success(), "clean exit on SIGTERM, got {status:?}");
                break;
            }
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => panic!("mux did not exit within 5s of SIGTERM"),
        }
    }
    for l in &mux.links {
        assert!(std::fs::symlink_metadata(l).is_err(), "link {l} must be removed");
        assert!(
            !std::path::Path::new(&format!("{l}.pid")).exists(),
            "sidecar {l}.pid must be removed"
        );
    }
}

/// A device that isn't a tty, or a baud with no termios encoding, must fail
/// as setup (exit 3) with nothing published.
#[test]
fn bad_device_or_baud_exits_three() {
    let out = Command::new(BIN)
        .args(["mux", "/dev/null", "--link", "/tmp/ttyforge-test-mux-never.pty"])
        .output()
        .expect("run ttyforge");
    assert_eq!(out.status.code(), Some(3), "not a tty is a setup error");
    assert!(out.stdout.is_empty(), "no port published");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not a tty"),
        "say what's wrong: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
