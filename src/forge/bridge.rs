//! `ttyforge bridge` — a virtual tty backed by a TCP peer (M4).
//!
//! `tcp://HOST:PORT` dials out and bridges; `listen://[HOST]:PORT` accepts one
//! connection at a time (re-listens after a peer drops, port stays alive).
//! Raw byte bridge first. RFC2217 client mode (M4b) adds remote baud/DTR/RTS:
//! termios changes a tool makes on the virtual port get translated into
//! telnet COM-PORT-OPTION commands for a ser2net-style server.
//!
//! The peer has to speak *raw bytes*: ser2net, ESP-Link, `socat TCP-LISTEN:…
//! FILE:/dev/ttyUSB0,rawer`, `nc -l`, or another ttyforge. Note that
//! serial-tether's `tetherd --tcp` is **not** such a peer — its TCP transport
//! is NDJSON/JSON-RPC 2.0 with token auth (tether `docs/PROTOCOL.md` §1–3), so
//! reaching it needs a tether-protocol client, not a byte pipe. To hand a
//! tether-owned device to a non-tether tool, `tether -D <dev>,pty` already
//! does that job.
//!
//! **The port outlives every peer.** That is the whole point: minicom stays
//! attached to `/tmp/ttyforge-bridge.pty` across lab-host reboots. So a
//! dropped peer is not an error — `listen://` goes back to accepting,
//! `tcp://` redials with backoff, and the pty never closes.
//!
//! **A wire with no peer drops what you write into it**, like a cable with
//! nothing on the far end. `pair` can afford to hold bytes in the kernel pty
//! buffer (both ends exist from process start, so the wait is bounded); an
//! absent TCP peer is an open-ended condition, and holding would eventually
//! block the local tool's writes forever and then flood the next peer with
//! minutes-old keystrokes. So the port is drained while the wire is down.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::AsyncReadExt as _;
use tokio::net::{TcpListener, TcpStream};

use super::pty::{Link, SetupError, VirtualPort};
use super::wire::{deliver, deliver_to, Wire, WireSpec};

/// Idle nap when the port reports "no consumer attached" (`Ok(0)`), and the
/// pause after a transient error — same values the other forges use.
const IDLE: Duration = Duration::from_millis(50);
const RETRY: Duration = Duration::from_millis(100);

pub async fn run(endpoint: String, link: Option<String>, wire: WireSpec) -> Result<()> {
    // Parse before creating anything: a typo'd endpoint should fail with a
    // usage-shaped message, not leave a half-built port behind.
    let endpoint = Endpoint::parse(&endpoint).context(SetupError)?;

    let (port, slave) = VirtualPort::create()?;
    let link = Link::claim(link.as_deref(), "bridge", &slave)?;

    // Bind before announcing readiness: for `listen://` the ready line has to
    // mean "the TCP side is accepting" as well, or a script that connects the
    // instant it reads the path loses the race. For `tcp://` we deliberately
    // do *not* wait for the peer — the port must exist even when the lab host
    // is still booting.
    let peers = Peers::bind(endpoint).await.context(SetupError)?;
    let peer_desc = peers.describe();

    // Readiness contract: exactly one stdout line, flushed.
    {
        use std::io::Write as _;
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{}", link.path())?;
        stdout.flush()?;
    }
    eprintln!(
        "ttyforge: bridge ready: {} <-> {peer_desc} (Ctrl-C to stop)",
        link.path()
    );

    let port = Arc::new(port);
    let mut bridge = tokio::spawn(bridge_loop(port.clone(), peers, wire));

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;
    let mut termios_tick = tokio::time::interval(Duration::from_millis(500));
    termios_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
            // The loop never finishes on its own; if it ever does, the port is
            // live but unpumped — surface that instead of idling silently.
            r = &mut bridge => {
                if let Err(e) = r {
                    bail!("bridge task failed: {e}");
                }
                bail!("bridge loop ended unexpectedly");
            }
            _ = termios_tick.tick() => {
                // Rule 5: a naive consumer may cook the slave; quietly fix.
                if port.reassert_raw_if_needed() {
                    tracing::debug!(port = link.path(), "reasserted raw termios");
                }
            }
        }
    }

    bridge.abort();
    eprintln!("ttyforge: bridge torn down");
    // link drops here → symlink and .pid sidecar removed.
    Ok(())
}

/// Peer after peer, forever: wait for one, bridge it until it goes away, wait
/// for the next. The virtual port survives all of it.
async fn bridge_loop(port: Arc<VirtualPort>, mut peers: Peers, spec: WireSpec) {
    let mut session = 0u64;
    loop {
        // Keep the port drained while the wire is down (see module docs), and
        // stop that reader before a session starts — two concurrent readers on
        // one master would split the byte stream between them.
        let drain = tokio::spawn(drain_while_down(port.clone()));
        let (stream, who) = peers.next().await;
        drain.abort();
        let _ = drain.await;

        // Nagle would batch keystrokes into 40ms clumps and smear the wire
        // model's ~5ms cells into bursts.
        if let Err(e) = stream.set_nodelay(true) {
            tracing::debug!(error = %e, "set_nodelay failed; continuing");
        }

        session += 1;
        eprintln!("ttyforge: peer connected: {who}");
        let (to_peer, from_peer) = spec.build_pair_nth(session);
        run_session(port.clone(), stream, to_peer, from_peer).await;
        eprintln!("ttyforge: peer disconnected: {who} (port stays up)");
    }
}

/// Bridge one connected peer to the port until either direction ends. Returns
/// on peer disconnect — never propagates it as an error.
async fn run_session(port: Arc<VirtualPort>, stream: TcpStream, to_peer: Wire, from_peer: Wire) {
    let (mut peer_rx, mut peer_tx) = stream.into_split();

    // Port → peer. Separate tasks per direction (as in `pair`) so a stalled
    // reader on one side never blocks the other.
    let mut up = {
        let port = port.clone();
        let mut wire = to_peer;
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            loop {
                match port.read(&mut buf).await {
                    Ok(0) => tokio::time::sleep(IDLE).await,
                    Ok(n) => {
                        if deliver_to(&mut wire, &buf[..n], &mut peer_tx).await.is_err() {
                            break; // peer gone; the session ends
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "port read failed");
                        tokio::time::sleep(RETRY).await;
                    }
                }
            }
        })
    };

    // Peer → port.
    let mut down = {
        let mut wire = from_peer;
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            loop {
                match peer_rx.read(&mut buf).await {
                    Ok(0) => break, // peer closed (or half-closed) the socket
                    Ok(n) => {
                        if let Err(e) = deliver(&mut wire, &buf[..n], &port).await {
                            tracing::warn!(error = %e, "port write failed");
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "peer read failed");
                        break;
                    }
                }
            }
        })
    };

    // Whichever direction ends first, the session is over. Only the *other*
    // handle is awaited — a JoinHandle that already yielded panics if polled
    // again — and awaiting it guarantees the next drain/session is the only
    // reader of the port.
    tokio::select! {
        _ = &mut up => {
            down.abort();
            let _ = down.await;
        }
        _ = &mut down => {
            up.abort();
            let _ = up.await;
        }
    }
}

/// Read and discard while no peer is attached, so a local tool writing into a
/// dead bridge never blocks forever and the next peer isn't greeted with a
/// backlog of stale bytes.
async fn drain_while_down(port: Arc<VirtualPort>) {
    let mut buf = [0u8; 4096];
    let mut discarded = 0u64;
    loop {
        match port.read(&mut buf).await {
            Ok(0) => tokio::time::sleep(IDLE).await,
            Ok(n) => {
                if discarded == 0 {
                    eprintln!("ttyforge: no peer attached — bytes written to the port are dropped");
                }
                discarded += n as u64;
                tracing::debug!(discarded, "dropped bytes on a down wire");
            }
            Err(e) => {
                tracing::warn!(error = %e, "port read failed");
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Endpoints
// ──────────────────────────────────────────────────────────────────────────

/// A parsed `endpoint` argument. Pure — [`Peers`] does the IO.
#[derive(Debug, PartialEq)]
enum Endpoint {
    /// `tcp://HOST:PORT` — dial the peer, redialing whenever it goes away.
    Dial(String),
    /// `listen://[HOST]:PORT` — accept one peer at a time. A bare `:PORT`
    /// binds every interface.
    Accept(String),
}

impl Endpoint {
    fn parse(s: &str) -> Result<Self> {
        if let Some(rest) = s.strip_prefix("tcp://") {
            let (host, _) = split_host_port(rest)?;
            if host.is_empty() {
                bail!("tcp://{rest} has no host — dial a peer, e.g. tcp://lab-host:5557");
            }
            Ok(Self::Dial(rest.to_string()))
        } else if let Some(rest) = s.strip_prefix("listen://") {
            let (host, port) = split_host_port(rest)?;
            // `listen://:7000` means every interface, like ser2net's default.
            Ok(Self::Accept(if host.is_empty() {
                format!("0.0.0.0:{port}")
            } else {
                rest.to_string()
            }))
        } else {
            bail!(
                "unknown endpoint {s:?} — use tcp://HOST:PORT to dial a peer \
                 or listen://[HOST]:PORT to accept one"
            )
        }
    }
}

/// Split `HOST:PORT` (or `[::1]:PORT`, or `:PORT`) and validate the port.
/// The host half is returned unvalidated — resolution is the network's job.
fn split_host_port(s: &str) -> Result<(&str, u16)> {
    let (host, port) = s
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("endpoint {s:?} has no :PORT"))?;
    let port: u16 = port
        .parse()
        .map_err(|_| anyhow::anyhow!("endpoint {s:?} has a bad port {port:?} (1..=65535)"))?;
    if port == 0 {
        // Port 0 would bind/dial something arbitrary — never what was meant
        // on a command line (tests use it only via the OS-assigned path).
        bail!("endpoint {s:?} needs a real port, not 0");
    }
    Ok((host, port))
}

/// The IO side of an endpoint: hands out one peer at a time, forever.
enum Peers {
    Dial(String),
    Accept(TcpListener),
}

impl Peers {
    async fn bind(endpoint: Endpoint) -> Result<Self> {
        Ok(match endpoint {
            Endpoint::Dial(addr) => Self::Dial(addr),
            Endpoint::Accept(addr) => Self::Accept(
                TcpListener::bind(&addr)
                    .await
                    .with_context(|| format!("listen on {addr}"))?,
            ),
        })
    }

    fn describe(&self) -> String {
        match self {
            Self::Dial(addr) => format!("tcp://{addr}"),
            Self::Accept(l) => match l.local_addr() {
                Ok(a) => format!("listen://{a}"),
                Err(_) => "listen://?".to_string(),
            },
        }
    }

    /// Block until a peer is available. Transient failures (host down, accept
    /// interrupted) are retried, not reported — the forge outlives them.
    async fn next(&mut self) -> (TcpStream, String) {
        match self {
            Self::Dial(addr) => {
                for attempt in 0u32.. {
                    match TcpStream::connect(addr.as_str()).await {
                        Ok(s) => return (s, addr.clone()),
                        Err(e) => {
                            let wait = backoff(attempt);
                            // Loud once, quiet after: a lab host that is down
                            // for an hour shouldn't fill the terminal.
                            if attempt == 0 {
                                eprintln!(
                                    "ttyforge: connect to {addr} failed ({e}); retrying"
                                );
                            } else {
                                tracing::debug!(%addr, error = %e, ?wait, "connect failed");
                            }
                            tokio::time::sleep(wait).await;
                        }
                    }
                }
                unreachable!("0u32.. is unbounded")
            }
            Self::Accept(listener) => loop {
                match listener.accept().await {
                    Ok((s, addr)) => return (s, addr.to_string()),
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed; retrying");
                        tokio::time::sleep(RETRY).await;
                    }
                }
            },
        }
    }
}

/// Redial backoff: 250ms doubling to a 5s ceiling. Fast enough that a lab
/// host rebooting feels instant, slow enough not to hammer a dead address.
fn backoff(attempt: u32) -> Duration {
    const CAP: Duration = Duration::from_secs(5);
    std::cmp::min(CAP, Duration::from_millis(250 << attempt.min(10)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parses_both_schemes() {
        assert_eq!(
            Endpoint::parse("tcp://lab-host:5557").unwrap(),
            Endpoint::Dial("lab-host:5557".into())
        );
        assert_eq!(
            Endpoint::parse("listen://:7000").unwrap(),
            Endpoint::Accept("0.0.0.0:7000".into()),
            "bare :PORT binds every interface"
        );
        assert_eq!(
            Endpoint::parse("listen://127.0.0.1:7000").unwrap(),
            Endpoint::Accept("127.0.0.1:7000".into())
        );
        // IPv6 literals keep their brackets — rsplit_once(':') lands after ']'.
        assert_eq!(
            Endpoint::parse("tcp://[::1]:23").unwrap(),
            Endpoint::Dial("[::1]:23".into())
        );
    }

    #[test]
    fn endpoint_rejects_the_typos_that_would_silently_misbehave() {
        for bad in [
            "lab-host:5557",          // no scheme
            "serial://lab:5557",      // wrong scheme
            "tcp://lab-host",         // no port
            "tcp://lab-host:",        // empty port
            "tcp://lab-host:0",       // port 0 is never meant literally
            "tcp://lab-host:99999",   // out of range
            "tcp://:5557",            // no host to dial
            "tcp://lab-host:5557/x",  // stray path
        ] {
            assert!(Endpoint::parse(bad).is_err(), "{bad:?} must be rejected");
        }
        // But a listener may legitimately omit the host.
        assert!(Endpoint::parse("listen://:5557").is_ok());
    }

    #[test]
    fn backoff_climbs_then_caps() {
        assert_eq!(backoff(0), Duration::from_millis(250));
        assert_eq!(backoff(1), Duration::from_millis(500));
        assert_eq!(backoff(4), Duration::from_secs(4));
        assert_eq!(backoff(5), Duration::from_secs(5), "capped");
        assert_eq!(backoff(u32::MAX), Duration::from_secs(5), "no shift overflow");
    }
}
