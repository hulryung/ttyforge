//! `ttyforge mux` — one real serial port, N virtual ttys (M5).
//!
//! RX fan-out needs serial-tether's ring-buffer-with-per-consumer-cursors
//! pattern (`tetherd/buffer.rs`): a naive shared read splits the byte stream
//! between consumers (serial-tether measured 128/72 of 200 bytes). Each
//! virtual port keeps its own cursor into one ring; TX from any port is
//! merged (serialised) into the real port.
//!
//! Deliberately daemonless: one foreground process, config on the command
//! line, dies with Ctrl-C. For locking, sessions, remote clients, and agent
//! RPCs, use serial-tether — this is the quick local splitter.

use anyhow::{bail, Result};

pub async fn run(device: String, baud: u32, link: Vec<String>) -> Result<()> {
    let _ = (device, baud, link);
    bail!("`mux` lands in M5 — see PLAN.md");
}
