//! `ttyforge mux` — one real serial port, N virtual ttys (M5).
//!
//! RX fan-out needs serial-tether's ring-buffer-with-per-consumer-cursors
//! pattern (`tetherd/buffer.rs`): a naive shared read splits the byte stream
//! between consumers (serial-tether measured 128/72 of 200 bytes). Each
//! virtual port keeps its own cursor into one ring; TX from any port is
//! merged (serialised) into the real port.
//!
//! Here that ring is a `tokio::sync::broadcast` channel, which *is* the
//! pattern — one buffer, an independent cursor per receiver, and an explicit
//! "you fell behind" signal — with the wakeup bookkeeping already written and
//! tested. Capacity is counted in reads rather than bytes (see [`BACKLOG`]).
//!
//! Two rules the shape encodes:
//!
//! 1. **Exactly one reader of the device.** The whole point of a mux is that
//!    every consumer sees every byte; two tasks reading one fd would race and
//!    split the stream, which is the bug being designed out.
//! 2. **A slow consumer must never stall the device.** If one tool stops
//!    draining, its own copy is dropped from the oldest end and it is told;
//!    the device keeps flowing for everyone else. The alternative —
//!    backpressure all the way to the UART — loses data nobody asked to lose.
//!
//! Deliberately daemonless: one foreground process, config on the command
//! line, dies with Ctrl-C. For locking, sessions, remote clients, and agent
//! RPCs, use serial-tether — this is the quick local splitter.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{broadcast, mpsc};

use super::pty::{Link, VirtualPort};
use super::serial::{check_baud, SerialPort};
use super::signals::Shutdown;
use super::wire::{deliver, Wire, WireSpec};

/// Idle nap when a port reports "no consumer attached" (`Ok(0)`), and the
/// pause after a transient error — same values the other forges use.
const IDLE: Duration = Duration::from_millis(50);
const RETRY: Duration = Duration::from_millis(100);

/// How many device reads the fan-out buffer holds. Counted in reads, not
/// bytes: one read is at most 8 KiB, so a consumer may fall ~2 MB behind
/// before it starts losing the oldest data — far more slack than a human
/// tool needs, and bounded so a wedged consumer can't grow memory forever.
const BACKLOG: usize = 256;

/// Bytes queued from consumers toward the device before their writers wait.
/// Small on purpose: TX is keystrokes and commands, and a deep queue would
/// only delay the moment a stuck device becomes visible.
const TX_QUEUE: usize = 64;

pub async fn run(device: String, baud: u32, link: Vec<String>, wire: WireSpec) -> Result<()> {
    // Check the baud before opening anything, so a typo fails as usage rather
    // than as a port that quietly runs at the wrong speed.
    check_baud(baud)?;
    let dev = Arc::new(SerialPort::open(&device, baud)?);

    // One virtual port per --link, in the order given.
    let mut ports = Vec::with_capacity(link.len());
    let mut links = Vec::with_capacity(link.len());
    for (i, path) in link.iter().enumerate() {
        let (port, slave) = VirtualPort::create().with_context(|| format!("port {}", i + 1))?;
        links.push(Link::claim(Some(path.as_str()), "mux", &slave)?);
        ports.push(Arc::new(port));
    }

    // Signals before readiness — see `signals`.
    let mut shutdown = Shutdown::install()?;

    // Readiness contract: one stdout line per --link, in the order given,
    // flushed — so a script can read them positionally.
    {
        use std::io::Write as _;
        let mut stdout = std::io::stdout().lock();
        for l in &links {
            writeln!(stdout, "{}", l.path())?;
        }
        stdout.flush()?;
    }
    eprintln!("ttyforge: mux ready: {device} @ {baud} -> {} port(s) (Ctrl-C to stop)", links.len());

    // Device → every consumer (one copy each), and every consumer → device
    // (merged). `Arc<[u8]>` so the fan-out clones a refcount, not the bytes.
    let (fanout, _) = broadcast::channel::<Arc<[u8]>>(BACKLOG);
    let (to_device, from_consumers) = mpsc::channel::<Vec<u8>>(TX_QUEUE);

    let mut tasks = vec![
        tokio::spawn(device_reader(dev.clone(), fanout.clone())),
        tokio::spawn(device_writer(dev.clone(), from_consumers)),
    ];
    for (i, port) in ports.iter().enumerate() {
        // Each consumer gets its own wire, so `--baud-sim` paces every copy
        // independently — as N real cables would.
        let (out, back) = wire.build_pair_nth(i as u64);
        tasks.push(tokio::spawn(to_consumer(
            port.clone(),
            fanout.subscribe(),
            out,
            links[i].path().to_string(),
        )));
        tasks.push(tokio::spawn(from_consumer(port.clone(), to_device.clone(), back)));
    }
    // Our own handle would keep the device writer alive forever; the
    // consumers' clones are the ones that matter.
    drop(to_device);

    let mut termios_tick = tokio::time::interval(Duration::from_millis(500));
    termios_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = termios_tick.tick() => {
                // Rule 5, once per virtual port.
                for (port, l) in ports.iter().zip(&links) {
                    if port.reassert_raw_if_needed() {
                        tracing::debug!(port = l.path(), "reasserted raw termios");
                    }
                }
            }
        }
    }

    for t in &tasks {
        t.abort();
    }
    tasks.clear();
    eprintln!("ttyforge: mux torn down");
    // links drop here → every symlink and .pid sidecar removed.
    Ok(())
}

/// The single reader of the device. Every byte it reads is published once and
/// copied to each consumer's cursor.
async fn device_reader(dev: Arc<SerialPort>, fanout: broadcast::Sender<Arc<[u8]>>) {
    let mut buf = [0u8; 8192];
    loop {
        match dev.read(&mut buf).await {
            // No writer on the other side right now (a pty device between
            // openers, or a quiet UART): idle, don't treat it as EOF.
            Ok(0) => tokio::time::sleep(IDLE).await,
            Ok(n) => {
                // Err only means "no consumers", which cannot happen while a
                // port exists — and would be nothing to do about anyway.
                let _ = fanout.send(Arc::from(&buf[..n]));
            }
            Err(e) => {
                tracing::warn!(error = %e, "device read failed");
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// The single writer to the device: whatever the consumers sent, in arrival
/// order, one whole chunk at a time. Merging is expected — two tools typing
/// at once interleave on a real cable too — but a chunk is never split.
async fn device_writer(dev: Arc<SerialPort>, mut from_consumers: mpsc::Receiver<Vec<u8>>) {
    while let Some(chunk) = from_consumers.recv().await {
        if let Err(e) = dev.write_all(&chunk).await {
            tracing::warn!(error = %e, "device write failed");
            tokio::time::sleep(RETRY).await;
        }
    }
}

/// One consumer's copy of the device stream.
async fn to_consumer(
    port: Arc<VirtualPort>,
    mut rx: broadcast::Receiver<Arc<[u8]>>,
    mut wire: Wire,
    path: String,
) {
    loop {
        match rx.recv().await {
            Ok(chunk) => {
                if let Err(e) = deliver(&mut wire, &chunk, &port).await {
                    tracing::warn!(port = path, error = %e, "consumer write failed");
                    tokio::time::sleep(RETRY).await;
                }
            }
            // This consumer stopped draining and lost the oldest reads. Say
            // so — silent truncation on a console log is a debugging trap —
            // and carry on from what is still buffered.
            Err(broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("ttyforge: {path} fell behind; dropped {n} read(s) for that port only");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// One consumer's writes, on their way to the device.
async fn from_consumer(port: Arc<VirtualPort>, to_device: mpsc::Sender<Vec<u8>>, mut wire: Wire) {
    let mut buf = [0u8; 8192];
    loop {
        match port.read(&mut buf).await {
            Ok(0) => tokio::time::sleep(IDLE).await,
            Ok(n) => {
                for cell in wire.plan(&buf[..n]) {
                    tokio::time::sleep_until(cell.due).await;
                    if to_device.send(cell.data).await.is_err() {
                        return; // device writer gone
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "port read failed");
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}
