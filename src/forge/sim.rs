//! `ttyforge sim` — a simulated device behind a virtual tty (M2).
//!
//! Two device backends:
//!
//! **Exec** (`-- CMD ...`): spawn the child with piped stdio; port→child-stdin,
//! child-stdout→port (child stderr stays on the terminal). The fake device is
//! just a program — write it in Python:
//!
//! ```text
//! ttyforge sim --link /tmp/fake-board.pty -- python3 fake_board.py
//! # fake_board.py: read stdin, write responses to stdout. That's the device.
//! ```
//!
//! **Presets** (`--preset`): built-in behaviors for instant test rigs:
//!   echo    every byte straight back
//!   shell   minimal POSIX-ish prompt: echoes the line, prints `$ `, answers
//!           `echo ...`; enough for serial-tether's `exec` tests
//!   uboot   `=> ` prompt, CR-only line endings, `printenv`/`setenv` stubs
//!   at      `AT...` → `OK` / `ERROR`, the modem classic
//!
//! The exec pattern mirrors `tether pty -- CMD` but inverted: there the child
//! *consumes* the port; here the child *is* the device.

use anyhow::{bail, Result};

pub async fn run(preset: Option<String>, link: Option<String>, exec: Vec<String>) -> Result<()> {
    let _ = (preset, link, exec);
    bail!("`sim` lands in M2 — see PLAN.md");
}
