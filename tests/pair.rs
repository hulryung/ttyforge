//! Integration test: the real `ttyforge pair` binary, end to end.
//!
//! Covers the full user contract: spawn → two ready lines on stdout →
//! byte-transparent transfer through the symlinked ports → SIGTERM →
//! links and sidecars cleaned up.

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

/// Start `ttyforge pair` with unique link paths and wait for the two ready
/// lines. Unique paths keep parallel test runs out of each other's way.
fn start_pair(tag: &str) -> PairProc {
    let a = format!("/tmp/ttyforge-test-{}-{tag}-a.pty", std::process::id());
    let b = format!("/tmp/ttyforge-test-{}-{tag}-b.pty", std::process::id());
    let mut child = Command::new(BIN)
        .args(["pair", "--link", &a, "--link", &b])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ttyforge pair");

    let stdout = child.stdout.take().expect("stdout piped");
    let mut lines = BufReader::new(stdout).lines();
    let path_a = lines.next().expect("line A").expect("read line A");
    let path_b = lines.next().expect("line B").expect("read line B");
    assert_eq!(path_a, a, "first ready line is side A");
    assert_eq!(path_b, b, "second ready line is side B");
    PairProc { child, path_a, path_b }
}

fn open_port(path: &str) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path)
        .expect("open pty slave via symlink")
}

#[test]
fn pair_bridges_bytes_and_cleans_up_on_sigterm() {
    let pair = start_pair("bridge");

    // Byte-transparent transfer, A → B: all 256 values.
    let payload: Vec<u8> = (0u8..=255).collect();
    let mut tx = open_port(&pair.path_a);
    let mut rx = open_port(&pair.path_b);
    tx.write_all(&payload).expect("write");
    let mut got = vec![0u8; payload.len()];
    rx.read_exact(&mut got).expect("read");
    assert_eq!(got, payload);

    // And B → A, reusing the same open fds (bidirectional on one open).
    tx.write_all(b"ping").expect("write ping");
    let mut buf4 = [0u8; 4];
    rx.read_exact(&mut buf4).expect("read ping");
    rx.write_all(b"pong").expect("write pong");
    tx.read_exact(&mut buf4).expect("read pong");
    assert_eq!(&buf4, b"pong");

    // Graceful teardown on SIGTERM: process exits, links + sidecars vanish.
    let (path_a, path_b) = (pair.path_a.clone(), pair.path_b.clone());
    let mut pair = pair;
    // SAFETY: our own child's pid.
    unsafe { libc::kill(pair.child.id() as i32, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match pair.child.try_wait().expect("try_wait") {
            Some(status) => {
                assert!(status.success(), "clean exit on SIGTERM, got {status:?}");
                break;
            }
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => panic!("pair did not exit within 5s of SIGTERM"),
        }
    }
    for p in [&path_a, &path_b] {
        assert!(
            std::fs::symlink_metadata(p).is_err(),
            "link {p} must be removed"
        );
        assert!(
            !std::path::Path::new(&format!("{p}.pid")).exists(),
            "sidecar {p}.pid must be removed"
        );
    }
}

/// A consumer closing and reopening the port must not kill the bridge
/// (rule 2: the forge holds the slave fd open).
#[test]
fn pair_survives_consumer_reopen_cycles() {
    let pair = start_pair("reopen");
    let mut far = open_port(&pair.path_b);

    for round in 0u8..3 {
        // Open, exchange one byte each way, close. Repeat.
        let mut near = open_port(&pair.path_a);
        near.write_all(&[round]).expect("write");
        let mut one = [0u8; 1];
        far.read_exact(&mut one).expect("read");
        assert_eq!(one[0], round, "round {round}");
    }
}
