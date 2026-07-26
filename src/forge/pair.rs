//! `ttyforge pair` — a virtual null-modem cable (M1).
//!
//! Two `pty::VirtualPort`s cross-connected by two independent pump tasks
//! (a→b and b→a — separate tasks so a stalled reader on one side never
//! blocks the other direction). Prints both paths to stdout (A then B, one
//! per line, flushed), then runs until Ctrl-C / SIGTERM.
//!
//! Kernel pty buffering gives the pair a friendly test property: bytes
//! written before the far side opens are *held*, not lost — no startup race
//! between the two consumers.

use std::sync::Arc;

use anyhow::{bail, Context, Result};

use super::pty::{Link, VirtualPort};
use super::signals::Shutdown;
use super::wire::{pump, WireSpec};

pub async fn run(link: Vec<String>, wire: WireSpec) -> Result<()> {
    if !(link.is_empty() || link.len() == 2) {
        bail!("--link must be given exactly twice (side A, then side B), or not at all");
    }

    let (port_a, slave_a) = VirtualPort::create().context("side A")?;
    let (port_b, slave_b) = VirtualPort::create().context("side B")?;
    // `Link::claim` tags its own failures as SetupError (exit 3).
    let link_a = Link::claim(link.first().map(|s| s.as_str()), "a", &slave_a).context("side A")?;
    let link_b = Link::claim(link.get(1).map(|s| s.as_str()), "b", &slave_b).context("side B")?;

    // Signals before readiness: once the paths are out, a script may kill
    // us immediately, and that must be a clean teardown (see `signals`).
    let mut shutdown = Shutdown::install()?;

    // Readiness contract: exactly two stdout lines (A then B), flushed, so
    // scripts can `{ read A; read B; } < <(ttyforge pair)`.
    {
        use std::io::Write as _;
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{}", link_a.path())?;
        writeln!(stdout, "{}", link_b.path())?;
        stdout.flush()?;
    }
    eprintln!(
        "ttyforge: pair ready: {} <-> {} (Ctrl-C to stop)",
        link_a.path(),
        link_b.path()
    );

    let a = Arc::new(port_a);
    let b = Arc::new(port_b);
    let (wire_ab, wire_ba) = wire.build_pair();
    let ab = tokio::spawn(pump(a.clone(), b.clone(), wire_ab));
    let ba = tokio::spawn(pump(b.clone(), a.clone(), wire_ba));

    let mut termios_tick = tokio::time::interval(std::time::Duration::from_millis(500));
    termios_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = termios_tick.tick() => {
                // Rule 5: a naive consumer may cook the slave; quietly fix.
                if a.reassert_raw_if_needed() {
                    tracing::debug!(port = link_a.path(), "reasserted raw termios");
                }
                if b.reassert_raw_if_needed() {
                    tracing::debug!(port = link_b.path(), "reasserted raw termios");
                }
            }
        }
    }

    ab.abort();
    ba.abort();
    eprintln!("ttyforge: pair torn down");
    // link_a / link_b drop here → symlinks and .pid sidecars removed.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::OpenOptionsExt as _;

    fn open_port(path: &str) -> std::fs::File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(path)
            .expect("open pty slave")
    }

    /// The M1 acceptance gate from PLAN.md: every byte value survives the
    /// wire, both directions. This is exactly what `socat pty,raw` failed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn all_256_byte_values_roundtrip_both_ways() {
        let (port_a, slave_a) = VirtualPort::create().expect("a");
        let (port_b, slave_b) = VirtualPort::create().expect("b");
        let a = Arc::new(port_a);
        let b = Arc::new(port_b);
        let (wire_ab, wire_ba) = WireSpec::default().build_pair();
        let ab = tokio::spawn(pump(a.clone(), b.clone(), wire_ab));
        let ba = tokio::spawn(pump(b.clone(), a.clone(), wire_ba));

        let payload: Vec<u8> = (0u8..=255).cycle().take(1024).collect();

        for (tx_path, rx_path) in [(&slave_a, &slave_b), (&slave_b, &slave_a)] {
            let mut tx = open_port(tx_path);
            let mut rx = open_port(rx_path);
            let want = payload.clone();

            let writer = tokio::task::spawn_blocking(move || {
                tx.write_all(&want).expect("write payload");
                tx.flush().expect("flush");
            });
            let expect_len = payload.len();
            let reader = tokio::task::spawn_blocking(move || {
                let mut got = vec![0u8; expect_len];
                rx.read_exact(&mut got).expect("read payload");
                got
            });

            writer.await.expect("writer task");
            let got = tokio::time::timeout(std::time::Duration::from_secs(5), reader)
                .await
                .expect("roundtrip timed out")
                .expect("reader task");
            assert_eq!(got, payload, "byte stream must survive unmodified");
        }

        ab.abort();
        ba.abort();
    }
}
