//! Integration tests: the wire model through the real binary, wall-clock.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ttyforge");

struct PairProc {
    child: Child,
    path_a: String,
    path_b: String,
}

impl Drop for PairProc {
    fn drop(&mut self) {
        // SAFETY: our own child's pid; SIGKILL as a last resort on test exit.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

fn start_pair(tag: &str, extra: &[&str]) -> PairProc {
    let a = format!("/tmp/ttyforge-test-wire-{}-{tag}-a.pty", std::process::id());
    let b = format!("/tmp/ttyforge-test-wire-{}-{tag}-b.pty", std::process::id());
    let mut args = vec!["pair", "--link", &a, "--link", &b];
    args.extend_from_slice(extra);
    let mut child = Command::new(BIN)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ttyforge pair");
    let stdout = child.stdout.take().expect("stdout piped");
    let mut lines = BufReader::new(stdout).lines();
    let path_a = lines.next().expect("line A").expect("read A");
    let path_b = lines.next().expect("line B").expect("read B");
    PairProc { child, path_a, path_b }
}

fn open_port(path: &str) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path)
        .expect("open pty slave")
}

/// Drain `rx` until `idle` passes with no new bytes (reader thread + channel,
/// so the blocking read never wedges the test).
fn read_until_silent(rx: std::fs::File, idle: Duration) -> Vec<u8> {
    let (tx_ch, rx_ch) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut rx = rx;
    std::thread::spawn(move || {
        let mut buf = [0u8; 512];
        while let Ok(n) = rx.read(&mut buf) {
            if n == 0 || tx_ch.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut got = Vec::new();
    while let Ok(chunk) = rx_ch.recv_timeout(idle) {
        got.extend(chunk);
    }
    got
}

/// The M3 acceptance number on a real clock: 1000 bytes at --baud-sim 9600
/// must take ≈ 1000·10/9600 ≈ 1.04s — not milliseconds (pty speed), not
/// multiple seconds.
#[test]
fn baud_sim_9600_paces_1000_bytes_to_about_a_second() {
    let pair = start_pair("baud", &["--baud-sim", "9600"]);
    let mut tx = open_port(&pair.path_a);
    let mut rx = open_port(&pair.path_b);

    let payload = vec![0xA5u8; 1000];
    let start = Instant::now();
    let writer = std::thread::spawn(move || {
        tx.write_all(&payload).expect("write");
        tx.flush().expect("flush");
    });
    let mut got = vec![0u8; 1000];
    rx.read_exact(&mut got).expect("read");
    let elapsed = start.elapsed();
    writer.join().expect("writer thread");

    assert!(
        elapsed >= Duration::from_millis(900) && elapsed <= Duration::from_millis(1500),
        "1000 B at 9600 baud should take ≈1.04s, took {elapsed:?}"
    );
    assert_eq!(got, vec![0xA5u8; 1000], "throttling must not alter bytes");
}

/// A lossy wire drops a deterministic subset under a fixed seed; the stream
/// keeps flowing, and surviving bytes arrive in order (the ramp payload makes
/// reordering or duplication detectable as a non-subsequence).
#[test]
fn seeded_drop_loses_some_bytes_but_keeps_order() {
    let pair = start_pair("drop", &["--drop", "0.05", "--seed", "1234"]);
    let mut tx = open_port(&pair.path_a);
    let rx = open_port(&pair.path_b);

    let payload: Vec<u8> = (0u8..=255).cycle().take(2000).collect();
    // Write from a thread: 2000 bytes exceed the pty kernel buffers, so a
    // sequential write-then-read would deadlock on backpressure.
    let to_send = payload.clone();
    let writer = std::thread::spawn(move || {
        tx.write_all(&to_send).expect("write");
        tx.flush().expect("flush");
    });

    let got = read_until_silent(rx, Duration::from_millis(400));
    writer.join().expect("writer thread");
    assert!(
        got.len() < payload.len(),
        "5% drop must lose something: got {}/{}",
        got.len(),
        payload.len()
    );
    assert!(
        got.len() > payload.len() * 8 / 10,
        "5% drop must not lose ~20%: got {}/{}",
        got.len(),
        payload.len()
    );
    // Survivors must be a subsequence of the payload (order kept, nothing
    // invented, nothing duplicated).
    let mut it = payload.iter();
    for b in &got {
        assert!(
            it.any(|p| p == b),
            "received stream is not an in-order subsequence of the payload"
        );
    }
}
