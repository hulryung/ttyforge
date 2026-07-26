//! ttyforge — forge virtual serial ports.
//!
//! Four forges, one binary (see PLAN.md for the roadmap):
//!   pair    two linked virtual ttys (a virtual null-modem cable)
//!   sim     a virtual tty backed by a simulated device (exec'd child or preset)
//!   bridge  a virtual tty backed by a TCP peer (raw now, RFC2217 later)
//!   mux     one real serial port fanned out to N virtual ttys
//!
//! Design lineage: the pty core (openpty + full cfmakeraw + slave-fd-keepalive +
//! nonblocking master on tokio AsyncFd + symlink publishing) is ported from
//! serial-tether's battle-tested `tether pty` / `tetherd pty=` implementation.
//!
//! Exit codes:
//!   0  ok            1  usage error (clap)
//!   2  runtime error 3  setup error (pty/link/port creation failed)

use clap::{Parser, Subcommand};
use std::process::ExitCode;

mod forge;

#[derive(Parser, Debug)]
#[command(
    name = "ttyforge",
    version,
    about = "ttyforge — forge virtual serial ports (pairs, simulators, bridges, muxes)",
    after_help = "Every forge runs in the foreground: the virtual port lives exactly as long\n\
                  as the process (Ctrl-C to tear it down). Paths are printed to stdout once\n\
                  ready — one line per port, flushed — so scripts can capture them."
)]
struct Cli {
    #[command(flatten)]
    wire: WireArgs,
    #[command(subcommand)]
    cmd: Cmd,
}

/// The wire model — timing and fault injection applied to every forge, per
/// direction. A pty is instant and perfect; a real UART is neither. These
/// flags make the virtual wire honest so tests fail here instead of on
/// hardware.
#[derive(clap::Args, Debug)]
#[command(next_help_heading = "Wire model (all forges)")]
struct WireArgs {
    /// Throttle throughput to this line rate (8N1 framing: baud/10 bytes/s).
    #[arg(long, global = true, value_name = "BAUD")]
    baud_sim: Option<u32>,
    /// Fixed delivery delay per chunk (e.g. 5ms, 250us, 1.5s; bare = ms).
    #[arg(long, global = true, value_name = "DUR", value_parser = forge::wire::parse_duration)]
    latency: Option<std::time::Duration>,
    /// Random extra delay, uniform in [0, JITTER].
    #[arg(long, global = true, value_name = "DUR", value_parser = forge::wire::parse_duration)]
    jitter: Option<std::time::Duration>,
    /// Probability [0..=1] that a byte vanishes on the wire.
    #[arg(long, global = true, value_name = "PROB", value_parser = forge::wire::parse_probability)]
    drop: Option<f64>,
    /// Probability [0..=1] that a byte arrives with one bit flipped.
    #[arg(long, global = true, value_name = "PROB", value_parser = forge::wire::parse_probability)]
    corrupt: Option<f64>,
    /// RNG seed for --drop/--corrupt/--jitter; same seed replays the same
    /// wire byte-for-byte. Auto-generated (and printed) if omitted.
    #[arg(long, global = true, value_name = "N")]
    seed: Option<u64>,
}

impl WireArgs {
    fn to_spec(&self) -> forge::wire::WireSpec {
        forge::wire::WireSpec {
            baud: self.baud_sim,
            latency: self.latency,
            jitter: self.jitter,
            drop: self.drop,
            corrupt: self.corrupt,
            seed: self.seed,
        }
    }
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Create two linked virtual ttys — a virtual null-modem cable.
    ///
    /// Whatever one side writes, the other reads, byte-for-byte (full raw
    /// termios: no echo, no CR/NL translation, no flow-control characters —
    /// binary-safe for ZMODEM/XMODEM and firmware blobs, unlike `socat
    /// pty,raw`). Point pyserial at one end and minicom at the other.
    Pair {
        /// Publish the two ends at these paths (repeat twice: side A, then
        /// side B); default: /tmp/ttyforge-a.pty and /tmp/ttyforge-b.pty
        /// (numbered -2, -3… if busy).
        #[arg(long, value_name = "PATH")]
        link: Vec<String>,
    },
    /// Create a virtual tty backed by a simulated device.
    ///
    /// The device side is either a built-in preset or your own program
    /// (`-- CMD`): the child's stdin receives every byte written to the port,
    /// and everything it prints to stdout comes back out of the port. Write
    /// the fake device in Python, the consumer never knows.
    Sim {
        /// Built-in device behavior.
        #[arg(long, conflicts_with = "exec", value_parser = ["echo", "shell", "uboot", "at"])]
        preset: Option<String>,
        /// Publish the port at this path; default: /tmp/ttyforge-sim.pty
        /// (numbered -2, -3… if busy).
        #[arg(long, value_name = "PATH")]
        link: Option<String>,
        /// Your device program; its stdio is the device side of the wire
        /// (stdin ← port, stdout → port; line-buffer beware: use `python3 -u`).
        /// The port path is exported as $TTYFORGE_PTY. Exit status passes
        /// through when the program ends.
        #[arg(last = true)]
        exec: Vec<String>,
    },
    /// Create a virtual tty bridged to a TCP peer.
    ///
    /// `tcp://host:port` dials out; `listen://:port` waits for a connection.
    /// The port outlives every peer: a drop means re-accept (listen) or
    /// redial (tcp), never a closed path. Raw bytes by default, telnet
    /// COM-PORT-OPTION with `--rfc2217`.
    Bridge {
        /// Peer: tcp://HOST:PORT (connect) or listen://[HOST]:PORT (accept).
        endpoint: String,
        /// Publish the port at this path; default: /tmp/ttyforge-bridge.pty
        /// (numbered -2, -3… if busy).
        #[arg(long, value_name = "PATH")]
        link: Option<String>,
        /// Speak RFC2217 (telnet COM-PORT-OPTION) instead of raw bytes, so
        /// the baud/parity/data/stop/flow a tool sets on the virtual port
        /// retunes the *real* UART at the far end. For ser2net-style servers;
        /// tcp:// only. DTR/RTS still cannot be forwarded — a pty has no
        /// modem lines to observe.
        #[arg(long)]
        rfc2217: bool,
    },
    /// Fan one real serial port out to N virtual ttys.
    ///
    /// Every consumer gets a full copy of the RX stream; TX is merged.
    /// The no-daemon little sibling of serial-tether's `tether pty`.
    Mux {
        /// The real serial device, e.g. /dev/ttyUSB0
        device: String,
        /// Baud rate for the real port.
        #[arg(short, long, default_value_t = 115200)]
        baud: u32,
        /// Publish a virtual tty at each of these paths (repeatable).
        #[arg(long, value_name = "PATH", required = true)]
        link: Vec<String>,
    },
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let mut wire = cli.wire.to_spec();
    wire.resolve_seed();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let result = rt.block_on(async {
        match cli.cmd {
            Cmd::Pair { link } => forge::pair::run(link, wire).await,
            Cmd::Sim { preset, link, exec } => forge::sim::run(preset, link, exec, wire).await,
            Cmd::Bridge { endpoint, link, rfc2217 } => {
                forge::bridge::run(endpoint, link, rfc2217, wire).await
            }
            Cmd::Mux { device, baud, link } => forge::mux::run(device, baud, link, wire).await,
        }
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ttyforge: {e:#}");
            // A sim device program's own exit status passes through verbatim;
            // failures before the port was ready (pty/link creation) exit 3;
            // anything after is a runtime error, exit 2.
            if let Some(forge::sim::ChildExit(code)) = e
                .chain()
                .find_map(|c| c.downcast_ref::<forge::sim::ChildExit>())
            {
                ExitCode::from((*code).clamp(0, 255) as u8)
            // `downcast_ref`, not `chain().any(|c| c.is::<_>())`: a chain
            // frame's concrete type is anyhow's context wrapper, never the
            // context value, so the `is::<SetupError>()` form never matched
            // and every setup failure exited 2.
            } else if e.downcast_ref::<forge::pty::SetupError>().is_some() {
                ExitCode::from(3)
            } else {
                ExitCode::from(2)
            }
        }
    }
}
