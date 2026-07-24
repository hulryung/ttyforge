//! `ttyforge bridge` — a virtual tty backed by a TCP peer (M4).
//!
//! `tcp://HOST:PORT` dials out and bridges; `listen://[HOST]:PORT` accepts one
//! connection at a time (re-listens after a peer drops, port stays alive).
//! Raw byte bridge first. RFC2217 client mode (M4b) adds remote baud/DTR/RTS:
//! termios changes a tool makes on the virtual port get translated into
//! telnet COM-PORT-OPTION commands for a ser2net-style server.
//!
//! Pairs with serial-tether: `tetherd --tcp` on a lab host, `ttyforge bridge
//! tcp://lab:5557` on a laptop — but also works against plain ser2net,
//! ESP-Link, or `nc -l`.

use anyhow::{bail, Result};

use super::wire::WireSpec;

pub async fn run(endpoint: String, link: Option<String>, wire: WireSpec) -> Result<()> {
    let _ = (endpoint, link, wire);
    bail!("`bridge` lands in M4 — see PLAN.md");
}
