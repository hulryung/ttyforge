//! `ttyforge pair` — a virtual null-modem cable (M1).
//!
//! Two `pty::VirtualPort`s cross-connected: A's reads pump into B's writes
//! and vice versa, through a `wire::Wire` each way. Prints both paths to
//! stdout (A then B, one per line), then runs until Ctrl-C.
//!
//! Acceptance test: replace `socat pty,rawer,echo=0 pty,rawer,echo=0` in
//! serial-tether's ZMODEM loopback test and transfer 200 KB byte-identical
//! both ways.

use anyhow::{bail, Result};

pub async fn run(link: Vec<String>) -> Result<()> {
    let _ = link;
    bail!("`pair` lands in M1 — see PLAN.md");
}
