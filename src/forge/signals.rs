//! Shutdown signals, installed *before* a forge announces readiness.
//!
//! The readiness line is a promise: the port exists and the forge owns its
//! own lifecycle from here. Registering handlers after printing it leaves a
//! window where Ctrl-C or SIGTERM kills the process by default disposition —
//! symlinks and `.pid` sidecars left behind, and a signal exit status instead
//! of a clean 0. The window is small but perfectly reachable: M5's teardown
//! test signals the instant it reads the last ready line, and hit it every
//! run.
//!
//! Both signals are registered eagerly for the same reason. `ctrl_c()`
//! installs its handler when the future is first polled, which is inside the
//! event loop — too late.

use anyhow::{Context, Result};
use tokio::signal::unix::{signal, Signal, SignalKind};

/// Registered Ctrl-C and SIGTERM handlers.
pub struct Shutdown {
    term: Signal,
    int: Signal,
}

impl Shutdown {
    pub fn install() -> Result<Self> {
        Ok(Self {
            term: signal(SignalKind::terminate()).context("install SIGTERM handler")?,
            int: signal(SignalKind::interrupt()).context("install SIGINT handler")?,
        })
    }

    /// Resolves on the first Ctrl-C or SIGTERM. Cancel-safe: losing a
    /// `select!` race does not lose a signal.
    pub async fn recv(&mut self) {
        tokio::select! {
            _ = self.term.recv() => {}
            _ = self.int.recv() => {}
        }
    }
}
