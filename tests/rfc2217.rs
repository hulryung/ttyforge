//! Integration tests: `ttyforge bridge --rfc2217` against a telnet peer.
//!
//! The fake server here decodes telnet with its own small state machine
//! rather than reusing the crate's — a codec graded by itself grades nothing.
//! It negotiates, records COM-PORT-OPTION subnegotiations, answers them the
//! way RFC2217 says a server does (command + 100), and echoes data back, so
//! one connection exercises escape, decode, negotiation and the settings
//! poller at once.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpListener};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ttyforge");

const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;
const OPT_BINARY: u8 = 0;
const OPT_SGA: u8 = 3;
const OPT_COM_PORT: u8 = 44;

const SET_BAUDRATE: u8 = 1;
const SET_DATASIZE: u8 = 2;
const SET_PARITY: u8 = 3;
const SET_STOPSIZE: u8 = 4;
const SET_CONTROL: u8 = 5;

// ── the fake ser2net ──────────────────────────────────────────────────────

#[derive(Default)]
struct Seen {
    /// COM-PORT-OPTION commands the client sent, in order.
    subnegs: Vec<(u8, Vec<u8>)>,
    /// Payload the client sent, after un-escaping.
    data: Vec<u8>,
}

struct Server {
    addr: SocketAddr,
    seen: Arc<Mutex<Seen>>,
    connected: mpsc::Receiver<()>,
}

impl Server {
    /// Bind and start accepting in the background; call before the bridge is
    /// started so the address exists to dial.
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
        let addr = listener.local_addr().expect("addr");
        let seen = Arc::new(Mutex::new(Seen::default()));
        let (tx, connected) = mpsc::channel();
        let their_seen = seen.clone();
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().expect("accept");
            let mut reader = sock.try_clone().expect("clone");
            let mut writer = sock;
            let _ = tx.send(());
            let mut dec = Decoder::default();
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let out = dec.feed(&buf[..n]);
                        let mut reply = out.reply;
                        // Answer each setting the way a real server does, so
                        // the client's notice path runs; the answer must not
                        // surface as data on the far side.
                        for (cmd, params) in &out.subnegs {
                            reply.extend_from_slice(&[IAC, SB, OPT_COM_PORT, cmd + 100]);
                            for &b in params {
                                reply.push(b);
                                if b == IAC {
                                    reply.push(IAC);
                                }
                            }
                            reply.extend_from_slice(&[IAC, SE]);
                        }
                        // Echo the payload back, escaped.
                        for &b in &out.data {
                            reply.push(b);
                            if b == IAC {
                                reply.push(IAC);
                            }
                        }
                        if !reply.is_empty() && writer.write_all(&reply).is_err() {
                            break;
                        }
                        let mut s = their_seen.lock().expect("lock");
                        s.data.extend(out.data);
                        s.subnegs.extend(out.subnegs);
                    }
                }
            }
        });
        Self { addr, seen, connected }
    }

    fn wait_connected(&self) {
        self.connected
            .recv_timeout(Duration::from_secs(5))
            .expect("bridge never connected");
    }

    /// Poll the recorded state until `f` is satisfied, then return it.
    fn wait_until<T>(&self, what: &str, f: impl Fn(&Seen) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(v) = f(&self.seen.lock().expect("lock")) {
                return v;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[derive(Default)]
struct DecodeOut {
    data: Vec<u8>,
    reply: Vec<u8>,
    subnegs: Vec<(u8, Vec<u8>)>,
}

/// Deliberately a second, independent telnet decoder.
#[derive(Default)]
struct Decoder {
    state: u8, // 0 data, 1 iac, 2 negotiate, 3 sub, 4 sub-iac
    cmd: u8,
    sub: Vec<u8>,
}

impl Decoder {
    fn feed(&mut self, input: &[u8]) -> DecodeOut {
        let mut out = DecodeOut::default();
        for &b in input {
            match self.state {
                0 if b == IAC => self.state = 1,
                0 => out.data.push(b),
                1 => match b {
                    IAC => {
                        out.data.push(IAC);
                        self.state = 0;
                    }
                    WILL | WONT | DO | DONT => {
                        self.cmd = b;
                        self.state = 2;
                    }
                    SB => {
                        self.sub.clear();
                        self.state = 3;
                    }
                    _ => self.state = 0,
                },
                2 => {
                    let ok = matches!(b, OPT_BINARY | OPT_SGA | OPT_COM_PORT);
                    let answer = match (self.cmd, ok) {
                        (WILL, true) => DO,
                        (WILL, false) => DONT,
                        (DO, true) => WILL,
                        (DO, false) => WONT,
                        _ => 0, // WONT/DONT need no answer
                    };
                    if answer != 0 {
                        out.reply.extend_from_slice(&[IAC, answer, b]);
                    }
                    self.state = 0;
                }
                3 if b == IAC => self.state = 4,
                3 => self.sub.push(b),
                _ => match b {
                    IAC => {
                        self.sub.push(IAC);
                        self.state = 3;
                    }
                    SE => {
                        if self.sub.len() >= 2 && self.sub[0] == OPT_COM_PORT {
                            out.subnegs.push((self.sub[1], self.sub[2..].to_vec()));
                        }
                        self.sub.clear();
                        self.state = 0;
                    }
                    _ => {
                        self.sub.clear();
                        self.state = 0;
                    }
                },
            }
        }
        out
    }
}

// ── the forge under test ──────────────────────────────────────────────────

struct BridgeProc {
    child: Child,
    path: String,
}

impl Drop for BridgeProc {
    fn drop(&mut self) {
        // SAFETY: our own child's pid.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGKILL) };
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}.pid", self.path));
    }
}

fn start_bridge(tag: &str, addr: SocketAddr) -> BridgeProc {
    let path = format!("/tmp/ttyforge-test-r2217-{}-{tag}.pty", std::process::id());
    let mut child = Command::new(BIN)
        .args(["bridge", &format!("tcp://{addr}"), "--rfc2217", "--link", &path])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ttyforge bridge");
    let stdout = child.stdout.take().expect("stdout piped");
    let ready = BufReader::new(stdout).lines().next().expect("ready").expect("read");
    assert_eq!(ready, path);
    BridgeProc { child, path }
}

fn open_port(path: &str) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path)
        .expect("open pty slave via symlink")
}

fn read_port_within(port: &std::fs::File, n: usize, within: Duration) -> Vec<u8> {
    let mut fd = port.try_clone().expect("dup pty fd");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut got = vec![0u8; n];
        let _ = tx.send(fd.read_exact(&mut got).map(|()| got));
    });
    rx.recv_timeout(within)
        .expect("timed out reading from the port")
        .expect("read from port")
}

/// Configure the virtual port the way pyserial or minicom would.
fn set_line_params(port: &std::fs::File, baud: libc::speed_t, cflag_bits: libc::tcflag_t) {
    let fd = port.as_raw_fd();
    let mut tio: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::tcgetattr(fd, &mut tio) }, 0, "tcgetattr");
    unsafe {
        libc::cfsetispeed(&mut tio, baud);
        libc::cfsetospeed(&mut tio, baud);
    }
    tio.c_cflag = (tio.c_cflag & !libc::CSIZE) | cflag_bits;
    assert_eq!(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &tio) }, 0, "tcsetattr");
}

/// The M4b contract: what a tool sets on the virtual port becomes
/// COM-PORT-OPTION on the wire — noticed by polling, since a pty master is
/// never told.
#[test]
fn rfc2217_relays_termios_changes_to_the_peer() {
    let server = Server::start();
    let bridge = start_bridge("settings", server.addr);
    server.wait_connected();

    let port = open_port(&bridge.path);
    set_line_params(
        &port,
        libc::B9600,
        libc::CS7 | libc::PARENB | libc::PARODD | libc::CSTOPB | libc::CRTSCTS,
    );

    let subnegs = server.wait_until("all five settings", |s| {
        (s.subnegs.len() >= 5).then(|| s.subnegs.clone())
    });
    let get = |cmd: u8| {
        subnegs
            .iter()
            .find(|(c, _)| *c == cmd)
            .unwrap_or_else(|| panic!("no command {cmd} in {subnegs:?}"))
            .1
            .clone()
    };
    assert_eq!(get(SET_BAUDRATE), 9600u32.to_be_bytes(), "baud, big-endian");
    assert_eq!(get(SET_DATASIZE), vec![7]);
    assert_eq!(get(SET_PARITY), vec![2], "2 = odd");
    assert_eq!(get(SET_STOPSIZE), vec![2]);
    assert_eq!(get(SET_CONTROL), vec![3], "3 = hardware flow control");

    // An unchanged port must go quiet: no re-sending the same settings every
    // 100ms tick.
    let count = server.seen.lock().unwrap().subnegs.len();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        server.seen.lock().unwrap().subnegs.len(),
        count,
        "an idle port must stop talking"
    );

    // And a later change sends only the field that moved.
    set_line_params(
        &port,
        libc::B19200,
        libc::CS7 | libc::PARENB | libc::PARODD | libc::CSTOPB | libc::CRTSCTS,
    );
    let extra = server.wait_until("the baud change", |s| {
        (s.subnegs.len() > count).then(|| s.subnegs[count..].to_vec())
    });
    assert_eq!(extra.len(), 1, "only the baud moved, got {extra:?}");
    assert_eq!(extra[0], (SET_BAUDRATE, 19200u32.to_be_bytes().to_vec()));
}

/// Telnet framing must not cost binary transparency: 0xFF goes out doubled
/// and comes back single, and the server's own subnegotiation answers never
/// surface as data.
#[test]
fn rfc2217_stream_stays_binary_transparent() {
    let server = Server::start();
    let bridge = start_bridge("binary", server.addr);
    server.wait_connected();

    let port = open_port(&bridge.path);
    // Force settings traffic to interleave with the payload — the answers
    // come back mid-stream and must be invisible to the port.
    set_line_params(&port, libc::B115200, libc::CS8);

    // Every byte value, with a run of IAC to catch off-by-one escaping.
    let mut payload: Vec<u8> = (0u8..=255).collect();
    payload.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0x00, 0xFF]);

    let mut tx = open_port(&bridge.path);
    tx.write_all(&payload).expect("write");
    tx.flush().expect("flush");

    let got = server.wait_until("the payload", |s| {
        (s.data.len() >= payload.len()).then(|| s.data.clone())
    });
    assert_eq!(got, payload, "peer must see the bytes un-escaped");

    // The server echoed it back escaped; the port must see it collapsed
    // again, with no telnet bytes mixed in.
    let echoed = read_port_within(&port, payload.len(), Duration::from_secs(5));
    assert_eq!(echoed, payload, "round trip must be byte-identical");
}

/// `--rfc2217` is a client mode; accepting would mean being the ser2net-style
/// server. Refuse at setup rather than half-work.
#[test]
fn rfc2217_refuses_listen_mode() {
    let out = Command::new(BIN)
        .args(["bridge", "listen://:7000", "--rfc2217"])
        .output()
        .expect("run ttyforge");
    assert_eq!(out.status.code(), Some(3), "setup error");
    assert!(out.stdout.is_empty(), "no port published");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("client mode"), "explain why: {err}");
}
