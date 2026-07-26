//! Integration tests: the real `ttyforge bridge` binary against a real socket.
//!
//! The test process plays the remote end — a TCP listener for `tcp://`, a
//! client for `listen://` — which is exactly the raw byte stream ser2net,
//! esp-link and `socat TCP-LISTEN:… FILE:/dev/tty…,rawer` serve.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt as _;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ttyforge");

struct BridgeProc {
    child: Child,
    path: String,
}

impl Drop for BridgeProc {
    fn drop(&mut self) {
        // SAFETY: our own child's pid; SIGKILL as a last resort on test exit.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGKILL) };
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}.pid", self.path));
    }
}

/// Start `ttyforge bridge ENDPOINT` with a unique link path; wait for the
/// ready line. `None` if the forge died before announcing readiness — for a
/// `listen://` bridge that means the port we picked was taken meanwhile.
fn try_start_bridge(tag: &str, endpoint: &str, extra: &[&str]) -> Option<BridgeProc> {
    let path = format!("/tmp/ttyforge-test-bridge-{}-{tag}.pty", std::process::id());
    let mut args = vec!["bridge", endpoint, "--link", &path];
    args.extend_from_slice(extra);
    let mut child = Command::new(BIN)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ttyforge bridge");
    let stdout = child.stdout.take().expect("stdout piped");
    let ready = BufReader::new(stdout).lines().next().transpose().expect("read ready line");
    match ready {
        Some(line) => {
            assert_eq!(line, path, "the ready line is the port path");
            Some(BridgeProc { child, path })
        }
        // Setup failed (exit 3): no ready line, just EOF on the pipe.
        None => {
            let _ = child.wait();
            None
        }
    }
}

fn start_bridge(tag: &str, endpoint: &str, extra: &[&str]) -> BridgeProc {
    try_start_bridge(tag, endpoint, extra).expect("bridge became ready")
}

/// A listening bridge on a port nobody else holds. Picking a port and then
/// binding it is inherently racy — the OS may hand the same freed port to
/// another binder in between — so retry instead of flaking.
fn start_listen_bridge(tag: &str) -> (BridgeProc, u16) {
    for _ in 0..8 {
        let port = free_port();
        if let Some(b) = try_start_bridge(tag, &format!("listen://127.0.0.1:{port}"), &[]) {
            return (b, port);
        }
    }
    panic!("no free port for a listening bridge after 8 tries");
}

fn open_port(path: &str) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path)
        .expect("open pty slave via symlink")
}

/// A port the OS just handed out and nobody holds — i.e. one that is reliably
/// *closed* right now. Only sound for "nothing is listening here"; anything
/// that must actually bind it has to cope with losing the race.
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = l.local_addr().expect("local_addr").port();
    drop(l);
    port
}

/// Rebind one specific address, tolerating the window where the kernel still
/// holds it (a TIME_WAIT connection, or the bridge's in-flight redial).
fn bind_retry(addr: std::net::SocketAddr, within: Duration) -> TcpListener {
    let deadline = Instant::now() + within;
    loop {
        match TcpListener::bind(addr) {
            Ok(l) => return l,
            Err(e) => {
                assert!(Instant::now() < deadline, "rebind {addr}: {e}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn accept_within(listener: &TcpListener, within: Duration) -> TcpStream {
    listener.set_nonblocking(true).expect("nonblocking listener");
    let deadline = Instant::now() + within;
    loop {
        match listener.accept() {
            Ok((sock, _)) => {
                listener.set_nonblocking(false).expect("blocking listener");
                // BSD/macOS accepted sockets inherit O_NONBLOCK; Linux's don't.
                sock.set_nonblocking(false).expect("blocking socket");
                sock.set_read_timeout(Some(Duration::from_secs(5))).expect("read timeout");
                return sock;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "no connection within {within:?}");
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("accept: {e}"),
        }
    }
}

/// `read_exact` with a deadline: a broken bridge should fail the test, not
/// hang it (a pty read blocks forever when nothing arrives).
fn read_port_within(port: &std::fs::File, n: usize, within: Duration) -> Vec<u8> {
    let mut fd = port.try_clone().expect("dup pty fd");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut got = vec![0u8; n];
        let _ = tx.send(fd.read_exact(&mut got).map(|()| got));
    });
    rx.recv_timeout(within).expect("timed out reading from the port").expect("read from port")
}

/// Prove the session is fully up before measuring anything: the peer sends a
/// sync byte, and seeing it at the port means both directions are pumping
/// (the bridge drops what it reads while no peer is attached, so a naive
/// port-first write would race the connect).
fn sync(peer: &mut TcpStream, port: &std::fs::File) {
    peer.write_all(b"\x01").expect("peer sync write");
    peer.flush().expect("flush");
    assert_eq!(read_port_within(port, 1, Duration::from_secs(5)), b"\x01");
}

/// Every byte value, both directions, through a dialed peer — plus the
/// teardown contract (`SIGTERM` → clean exit, link and sidecar removed).
#[test]
fn bridge_dials_a_peer_and_is_byte_transparent() {
    let remote = TcpListener::bind("127.0.0.1:0").expect("bind remote");
    let addr = remote.local_addr().expect("addr");
    let bridge = start_bridge("dial", &format!("tcp://{addr}"), &[]);

    let mut peer = accept_within(&remote, Duration::from_secs(5));
    let port = open_port(&bridge.path);
    sync(&mut peer, &port);

    // peer → port
    let payload: Vec<u8> = (0u8..=255).collect();
    peer.write_all(&payload).expect("peer write");
    peer.flush().expect("flush");
    assert_eq!(
        read_port_within(&port, payload.len(), Duration::from_secs(5)),
        payload,
        "peer → port must be byte-transparent"
    );

    // port → peer, on the same open fds
    let mut tx = open_port(&bridge.path);
    tx.write_all(&payload).expect("port write");
    tx.flush().expect("flush");
    let mut got = vec![0u8; payload.len()];
    peer.read_exact(&mut got).expect("peer read");
    assert_eq!(got, payload, "port → peer must be byte-transparent");

    // Graceful teardown: process exits, link + sidecar vanish.
    let path = bridge.path.clone();
    let mut bridge = bridge;
    // SAFETY: our own child's pid.
    unsafe { libc::kill(bridge.child.id() as i32, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match bridge.child.try_wait().expect("try_wait") {
            Some(status) => {
                assert!(status.success(), "clean exit on SIGTERM, got {status:?}");
                break;
            }
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => panic!("bridge did not exit within 5s of SIGTERM"),
        }
    }
    assert!(std::fs::symlink_metadata(&path).is_err(), "link {path} must be removed");
    assert!(
        !std::path::Path::new(&format!("{path}.pid")).exists(),
        "sidecar {path}.pid must be removed"
    );
}

/// The M4 requirement: a peer dropping does not take the port down — the
/// bridge goes back to accepting, and the same virtual port serves the next
/// peer (minicom stays attached across lab-host reboots).
#[test]
fn bridge_listens_and_reaccepts_after_a_peer_drops() {
    let (bridge, port_no) = start_listen_bridge("listen");
    let port = open_port(&bridge.path);

    for round in 1u8..=3 {
        let mut peer = TcpStream::connect(("127.0.0.1", port_no))
            .unwrap_or_else(|e| panic!("connect round {round}: {e}"));
        peer.set_read_timeout(Some(Duration::from_secs(5))).expect("read timeout");
        sync(&mut peer, &port);

        // Round-trip a round-specific byte each way.
        peer.write_all(&[round]).expect("peer write");
        assert_eq!(read_port_within(&port, 1, Duration::from_secs(5)), vec![round]);

        let mut tx = open_port(&bridge.path);
        tx.write_all(&[round ^ 0xff]).expect("port write");
        let mut one = [0u8; 1];
        peer.read_exact(&mut one).expect("peer read");
        assert_eq!(one[0], round ^ 0xff, "round {round}");

        drop(peer); // peer vanishes; the port must survive
    }
}

/// Connect mode redials: the remote can disappear entirely and come back, and
/// the bridge reattaches on its own (backoff caps at 5s).
#[test]
fn bridge_redials_when_the_remote_returns() {
    // Bind first and take the address from the socket: picking a port and
    // binding it afterwards races every other binder on the machine.
    let remote = TcpListener::bind("127.0.0.1:0").expect("bind remote");
    let addr = remote.local_addr().expect("addr");
    let bridge = start_bridge("redial", &format!("tcp://{addr}"), &[]);
    let port = open_port(&bridge.path);

    let mut peer = accept_within(&remote, Duration::from_secs(5));
    sync(&mut peer, &port);

    // The whole remote goes away — connection and listener both — long enough
    // for the bridge to see the disconnect and start redialing into nothing.
    drop(peer);
    drop(remote);
    std::thread::sleep(Duration::from_millis(300));

    // …and comes back on the same address.
    let remote = bind_retry(addr, Duration::from_secs(5));
    let mut peer = accept_within(&remote, Duration::from_secs(15));
    sync(&mut peer, &port);

    peer.write_all(b"back").expect("peer write");
    assert_eq!(read_port_within(&port, 4, Duration::from_secs(5)), b"back");
}

/// A failure before the port is ready exits 3 (main.rs's documented code), and
/// nothing is published on stdout — a script capturing the ready line must see
/// EOF rather than hang waiting for a port that will never exist.
#[test]
fn setup_failures_exit_three_with_no_ready_line() {
    for endpoint in ["lab-host:5557", "tcp://lab-host", "listen://:0"] {
        let out =
            Command::new(BIN).args(["bridge", endpoint]).output().expect("run ttyforge bridge");
        assert_eq!(
            out.status.code(),
            Some(3),
            "{endpoint:?} must exit 3 (setup error), stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty(), "{endpoint:?} must publish no path");
    }
}

/// The wire model is a *global* flag, so it must reach the bridge too: 1000
/// bytes at `--baud-sim 9600` take ≈1000·10/9600 ≈ 1.04s to reach the peer,
/// not the microseconds a pty-plus-loopback-socket would manage.
#[test]
fn bridge_applies_the_wire_model() {
    let remote = TcpListener::bind("127.0.0.1:0").expect("bind remote");
    let addr = remote.local_addr().expect("addr");
    let bridge = start_bridge("wire", &format!("tcp://{addr}"), &["--baud-sim", "9600"]);

    let mut peer = accept_within(&remote, Duration::from_secs(5));
    let port = open_port(&bridge.path);
    sync(&mut peer, &port);

    let mut tx = open_port(&bridge.path);
    let start = Instant::now();
    // From a thread: the throttled bridge drains the port slowly, so a
    // sequential write-then-read could wedge on kernel-buffer backpressure.
    let writer = std::thread::spawn(move || {
        tx.write_all(&vec![0xA5u8; 1000]).expect("write");
        tx.flush().expect("flush");
    });
    let mut got = vec![0u8; 1000];
    peer.read_exact(&mut got).expect("peer read");
    let elapsed = start.elapsed();
    writer.join().expect("writer thread");

    assert!(
        elapsed >= Duration::from_millis(900) && elapsed <= Duration::from_millis(2500),
        "1000 B at 9600 baud should take ≈1.04s over the bridge, took {elapsed:?}"
    );
    assert_eq!(got, vec![0xA5u8; 1000], "throttling must not alter bytes");
}

/// The port exists and is usable before (and without) any peer: `tcp://` must
/// not block startup on a lab host that is still booting, and writing into a
/// bridge whose wire is down must drop the bytes rather than wedge the writer
/// once the kernel pty buffer fills.
#[test]
fn bridge_port_is_ready_and_writable_with_no_peer() {
    let bridge = start_bridge("nopeer", &format!("tcp://127.0.0.1:{}", free_port()), &[]);
    let mut tx = open_port(&bridge.path);

    // 32 KB is an order of magnitude past the pty kernel buffer (~2-3 KB), so
    // this only completes if the forge is actively draining the down wire.
    let writer = std::thread::spawn(move || {
        tx.write_all(&vec![0x5Au8; 32 * 1024]).expect("write");
        tx.flush().expect("flush");
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !writer.is_finished() {
        assert!(
            Instant::now() < deadline,
            "writing into a peerless bridge blocked — the down wire must drop"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    writer.join().expect("writer thread");
}
