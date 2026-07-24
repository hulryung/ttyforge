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
    #[command(subcommand)]
    cmd: Cmd,
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
        /// Publish the two ends at these paths (repeat twice); default:
        /// /tmp/ttyforge-<pid>-{a,b}.pty
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
        /// Built-in device behavior: echo | shell | uboot | at
        #[arg(long, conflicts_with = "exec")]
        preset: Option<String>,
        /// Publish the port at this path; default: /tmp/ttyforge-<pid>.pty
        #[arg(long, value_name = "PATH")]
        link: Option<String>,
        /// Your device program; its stdio is the device side of the wire.
        #[arg(last = true)]
        exec: Vec<String>,
    },
    /// Create a virtual tty bridged to a TCP peer.
    ///
    /// `tcp://host:port` dials out; `listen://:port` waits for a connection.
    /// Raw byte bridge first; RFC2217 (remote baud/DTR control) later.
    Bridge {
        /// Peer: tcp://HOST:PORT (connect) or listen://[HOST]:PORT (accept).
        endpoint: String,
        /// Publish the port at this path; default: /tmp/ttyforge-<pid>.pty
        #[arg(long, value_name = "PATH")]
        link: Option<String>,
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
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let result = rt.block_on(async {
        match cli.cmd {
            Cmd::Pair { link } => forge::pair::run(link).await,
            Cmd::Sim { preset, link, exec } => forge::sim::run(preset, link, exec).await,
            Cmd::Bridge { endpoint, link } => forge::bridge::run(endpoint, link).await,
            Cmd::Mux { device, baud, link } => forge::mux::run(device, baud, link).await,
        }
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ttyforge: {e:#}");
            ExitCode::from(2)
        }
    }
}
