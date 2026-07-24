//! Integration tests: the real `ttyforge sim` binary, all backends.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ttyforge");

struct SimProc {
    child: Child,
    path: String,
}

impl Drop for SimProc {
    fn drop(&mut self) {
        // SAFETY: our own child's pid; SIGKILL as a last resort on test exit.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

/// Start `ttyforge sim` with a unique link path; wait for the ready line.
fn start_sim(tag: &str, args: &[&str]) -> SimProc {
    let path = format!("/tmp/ttyforge-test-sim-{}-{tag}.pty", std::process::id());
    let mut all = vec!["sim", "--link", &path];
    all.extend_from_slice(args);
    let mut child = Command::new(BIN)
        .args(&all)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ttyforge sim");
    let stdout = child.stdout.take().expect("stdout piped");
    let ready = BufReader::new(stdout).lines().next().expect("ready line").expect("read");
    assert_eq!(ready, path);
    SimProc { child, path }
}

fn open_port(path: &str) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path)
        .expect("open sim port")
}

/// Read from `f` until `needle` appears (or 5s passes). Returns everything read.
fn read_until(f: &mut std::fs::File, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut acc = Vec::new();
    let mut byte = [0u8; 256];
    while Instant::now() < deadline {
        let n = f.read(&mut byte).expect("read");
        acc.extend_from_slice(&byte[..n]);
        if String::from_utf8_lossy(&acc).contains(needle) {
            return String::from_utf8_lossy(&acc).into_owned();
        }
    }
    panic!(
        "timed out waiting for {needle:?}; got {:?}",
        String::from_utf8_lossy(&acc)
    );
}

#[test]
fn echo_preset_reflects_bytes() {
    let sim = start_sim("echo", &["--preset", "echo"]);
    let mut port = open_port(&sim.path);
    let payload: Vec<u8> = (0u8..=255).collect();
    port.write_all(&payload).expect("write");
    let mut got = vec![0u8; payload.len()];
    port.read_exact(&mut got).expect("read");
    assert_eq!(got, payload, "echo must be byte-transparent");
}

#[test]
fn uboot_preset_serves_a_console() {
    let sim = start_sim("uboot", &["--preset", "uboot"]);
    let mut port = open_port(&sim.path);

    // The greeting prompt is buffered from before we opened.
    read_until(&mut port, "=> ");

    port.write_all(b"printenv baudrate\r").expect("write");
    let out = read_until(&mut port, "=> ");
    assert!(out.contains("baudrate=115200"), "printenv output: {out:?}");

    port.write_all(b"setenv foo bar; printenv foo\r").expect("write");
    let out = read_until(&mut port, "=> ");
    assert!(out.contains("foo=bar"), "setenv roundtrip: {out:?}");
}

/// The exec backend with the simplest possible device: `cat` (an echo device
/// implemented as an external program).
#[test]
fn exec_backend_bridges_child_stdio() {
    let sim = start_sim("cat", &["--", "cat"]);
    let mut port = open_port(&sim.path);
    port.write_all(b"through the cat").expect("write");
    let mut got = [0u8; 15];
    port.read_exact(&mut got).expect("read");
    assert_eq!(&got, b"through the cat");
}

/// A device program that exits nonzero: sim must pass the status through.
#[test]
fn exec_backend_passes_child_exit_status_through() {
    let path = format!("/tmp/ttyforge-test-sim-{}-exit.pty", std::process::id());
    let mut child = Command::new(BIN)
        .args(["sim", "--link", &path, "--", "sh", "-c", "exit 7"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let status = child.wait().expect("wait");
    assert_eq!(status.code(), Some(7), "child status must pass through");
    assert!(
        std::fs::symlink_metadata(&path).is_err(),
        "link cleaned up after child exit"
    );
}
