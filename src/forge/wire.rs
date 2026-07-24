//! Wire model: the timing/fault layer spliced between any two forge ends (M3).
//!
//! A real UART is slow and imperfect; a pty is instant and perfect. Tests that
//! pass against a perfect wire routinely fail on hardware. This layer makes
//! the virtual wire honest, per direction:
//!
//!   --baud-sim 9600      throttle throughput to the line rate (8N1: baud/10 B/s)
//!   --latency 5ms        fixed delivery delay per chunk
//!   --jitter 2ms         random extra delay, uniform in [0, jitter]
//!   --drop 0.001         probability a byte vanishes on the wire
//!   --corrupt 0.0001     probability a byte arrives with one bit flipped
//!   --seed 42            make the randomness reproducible
//!
//! Design: `Wire::plan()` transforms bytes and computes a delivery *schedule*
//! (cells with due instants); the caller sleeps and performs the IO. Keeping
//! IO out of the wire makes the timing math unit-testable under tokio's
//! paused clock, and lets the same Wire pace a pty, a child's stdio, or (M4)
//! a TCP stream. Randomness is a built-in xorshift64* — no platform-dependent
//! RNG, so a given `--seed` replays byte-for-byte anywhere.
//!
//! Semantics pinned by tests:
//! - Throughput: a byte takes 10/baud seconds of line time (8N1 framing).
//!   Dropped/corrupted bytes still consume line time — noise doesn't make a
//!   wire faster.
//! - No burst credit: an idle line doesn't bank throughput; pacing restarts
//!   from "now" (`cursor = max(cursor, now)`).
//! - Delivery granularity: ~5ms worth of bytes per cell when throttling, so
//!   consumers see a stream, not one burst per chunk.
//! - Backpressure, not reordering: a paced chunk stalls its direction (like
//!   a slow modem); nothing overtakes anything.

use std::time::Duration;

use tokio::time::Instant;

use super::pty::VirtualPort;

/// Parsed wire configuration, shared by every forge (global CLI flags).
#[derive(Debug, Clone, Default)]
pub struct WireSpec {
    pub baud: Option<u32>,
    pub latency: Option<Duration>,
    pub jitter: Option<Duration>,
    pub drop: Option<f64>,
    pub corrupt: Option<f64>,
    pub seed: Option<u64>,
}

impl WireSpec {
    /// If stochastic features are on but no seed was given, pick one and
    /// announce it on stderr — a failing CI run must always be replayable.
    pub fn resolve_seed(&mut self) {
        let stochastic =
            self.drop.is_some() || self.corrupt.is_some() || self.jitter.is_some();
        if stochastic && self.seed.is_none() {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(0);
            let seed = nanos ^ ((std::process::id() as u64) << 32);
            self.seed = Some(seed);
            eprintln!("ttyforge: wire seed {seed} (pass --seed {seed} to reproduce)");
        }
    }

    /// Two independent wires, one per direction, with decorrelated RNG
    /// streams (same seed still reproduces both).
    pub fn build_pair(&self) -> (Wire, Wire) {
        (Wire::new(self, 0), Wire::new(self, 1))
    }
}

/// One scheduled delivery: write `data` no earlier than `due`.
pub struct Cell {
    pub data: Vec<u8>,
    pub due: Instant,
}

/// One direction of the simulated wire. Owns its pacing cursor and RNG.
pub struct Wire {
    /// Serialization time per byte (None = infinitely fast line).
    byte_time: Option<Duration>,
    /// Bytes per delivery cell (~5ms of line time when throttling).
    cell_bytes: usize,
    latency: Duration,
    jitter: Duration,
    drop_p: f64,
    corrupt_p: f64,
    rng: Rng,
    /// The instant the line finishes transmitting everything scheduled so far.
    cursor: Instant,
}

impl Wire {
    fn new(spec: &WireSpec, direction: u64) -> Self {
        let byte_time = spec
            .baud
            .map(|b| Duration::from_secs_f64(10.0 / b as f64));
        let cell_bytes = match spec.baud {
            // ~5ms of bytes per cell, at least 1.
            Some(b) => std::cmp::max(1, (b as usize / 10) / 200),
            None => usize::MAX,
        };
        Self {
            byte_time,
            cell_bytes,
            latency: spec.latency.unwrap_or(Duration::ZERO),
            jitter: spec.jitter.unwrap_or(Duration::ZERO),
            drop_p: spec.drop.unwrap_or(0.0),
            corrupt_p: spec.corrupt.unwrap_or(0.0),
            rng: Rng::new(
                spec.seed
                    .unwrap_or(0)
                    .wrapping_add(direction.wrapping_mul(0x9E37_79B9_7F4A_7C15)),
            ),
            cursor: Instant::now(),
        }
    }

    /// Transform (drop/corrupt) and schedule one chunk. Cells whose bytes
    /// were all dropped are omitted, but their line time is still consumed.
    pub fn plan(&mut self, data: &[u8]) -> Vec<Cell> {
        let now = Instant::now();
        if self.cursor < now {
            self.cursor = now; // idle line banks no throughput credit
        }
        let lat = self.latency + self.jitter_sample();
        let mut cells = Vec::new();
        for chunk in data.chunks(self.cell_bytes) {
            // Serialization consumes line time for the *original* bytes —
            // noise-eaten bytes still crossed the wire.
            if let Some(bt) = self.byte_time {
                self.cursor += bt * chunk.len() as u32;
            }
            let out = self.transform(chunk);
            if !out.is_empty() {
                cells.push(Cell {
                    data: out,
                    due: self.cursor + lat,
                });
            }
        }
        cells
    }

    fn jitter_sample(&mut self) -> Duration {
        if self.jitter.is_zero() {
            Duration::ZERO
        } else {
            self.jitter.mul_f64(self.rng.f64())
        }
    }

    fn transform(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.drop_p == 0.0 && self.corrupt_p == 0.0 {
            return chunk.to_vec();
        }
        chunk
            .iter()
            .filter_map(|&b| {
                if self.drop_p > 0.0 && self.rng.f64() < self.drop_p {
                    None
                } else if self.corrupt_p > 0.0 && self.rng.f64() < self.corrupt_p {
                    Some(b ^ (1 << (self.rng.next() % 8)))
                } else {
                    Some(b)
                }
            })
            .collect()
    }
}

/// Pump bytes from one port into another through a wire, forever. The wire-
/// aware successor of M1's plain pump (identity `WireSpec` ⇒ byte-for-byte,
/// no added delay). Transient errors are logged and retried; `Ok(0)` (no
/// consumer attached) idles briefly.
pub async fn pump(
    from: std::sync::Arc<VirtualPort>,
    to: std::sync::Arc<VirtualPort>,
    mut wire: Wire,
) {
    let mut buf = [0u8; 8192];
    loop {
        match from.read(&mut buf).await {
            Ok(0) => tokio::time::sleep(Duration::from_millis(50)).await,
            Ok(n) => {
                for cell in wire.plan(&buf[..n]) {
                    tokio::time::sleep_until(cell.due).await;
                    if let Err(e) = to.write_all(&cell.data).await {
                        tracing::warn!(error = %e, "pump write failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "pump read failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Deliver one chunk through a wire into a port (sim's outbound path).
pub async fn deliver(wire: &mut Wire, data: &[u8], port: &VirtualPort) -> std::io::Result<()> {
    for cell in wire.plan(data) {
        tokio::time::sleep_until(cell.due).await;
        port.write_all(&cell.data).await?;
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// CLI value parsers (shared by main.rs)
// ──────────────────────────────────────────────────────────────────────────

/// Parse `250us` / `5ms` / `1.5s` / bare `5` (= ms) into a Duration.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let (num, scale) = if let Some(v) = s.strip_suffix("us") {
        (v, 1e-6)
    } else if let Some(v) = s.strip_suffix("ms") {
        (v, 1e-3)
    } else if let Some(v) = s.strip_suffix('s') {
        (v, 1.0)
    } else {
        (s, 1e-3) // bare number = milliseconds
    };
    let n: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("bad duration {s:?} (try 5ms, 250us, 1.5s)"))?;
    if !n.is_finite() || n < 0.0 {
        return Err(format!("duration {s:?} must be non-negative"));
    }
    Ok(Duration::from_secs_f64(n * scale))
}

/// Parse a probability in [0, 1].
pub fn parse_probability(s: &str) -> Result<f64, String> {
    let p: f64 = s
        .parse()
        .map_err(|_| format!("bad probability {s:?} (0.0..=1.0)"))?;
    if !(0.0..=1.0).contains(&p) {
        return Err(format!("probability {p} out of range 0.0..=1.0"));
    }
    Ok(p)
}

// ──────────────────────────────────────────────────────────────────────────
// Deterministic RNG (xorshift64*, splitmix64-seeded) — no `rand` dependency,
// so a seed replays identically on every platform and toolchain.
// ──────────────────────────────────────────────────────────────────────────

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // splitmix64 turns any seed (including 0) into a well-mixed nonzero
        // xorshift state.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Self(if z == 0 { 1 } else { z })
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    fn f64(&mut self) -> f64 {
        (self.next() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(f: impl FnOnce(&mut WireSpec)) -> WireSpec {
        let mut s = WireSpec::default();
        f(&mut s);
        s
    }

    /// The M3 acceptance number, pinned under tokio's paused clock: 1000
    /// bytes at 9600 baud (8N1) occupy 1000·10/9600 ≈ 1.0417s of line time.
    #[tokio::test(start_paused = true)]
    async fn throttle_schedules_line_rate_exactly() {
        let (mut w, _) = spec(|s| s.baud = Some(9600)).build_pair();
        let start = Instant::now();
        let cells = w.plan(&[0u8; 1000]);
        let last_due = cells.last().unwrap().due;
        let total = last_due - start;
        let expect = Duration::from_secs_f64(1000.0 * 10.0 / 9600.0);
        let err = total.abs_diff(expect);
        assert!(
            err < Duration::from_millis(2),
            "line time {total:?} vs expected {expect:?}"
        );
        // ~5ms cells: 1000 bytes at 960 B/s in 4-byte cells → many cells,
        // a stream rather than one burst.
        assert!(cells.len() > 100, "expected fine-grained cells, got {}", cells.len());
    }

    /// An idle line must not bank credit: after a quiet spell, pacing
    /// restarts from now instead of bursting the backlog instantly.
    #[tokio::test(start_paused = true)]
    async fn idle_line_banks_no_credit() {
        let (mut w, _) = spec(|s| s.baud = Some(9600)).build_pair();
        let _ = w.plan(&[0u8; 96]); // consumes 100ms of line time
        tokio::time::advance(Duration::from_secs(10)).await;
        let now = Instant::now();
        let cells = w.plan(&[0u8; 96]);
        let total = cells.last().unwrap().due - now;
        let expect = Duration::from_millis(100);
        assert!(
            total >= expect && total < expect + Duration::from_millis(2),
            "second batch must take its own 100ms, got {total:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn latency_shifts_delivery_without_eating_throughput() {
        let (mut w, _) = spec(|s| {
            s.baud = Some(9600);
            s.latency = Some(Duration::from_millis(50));
        })
        .build_pair();
        let now = Instant::now();
        let cells = w.plan(&[0u8; 96]); // 100ms line time
        let first = cells.first().unwrap().due - now;
        let last = cells.last().unwrap().due - now;
        assert!(first >= Duration::from_millis(50), "latency floor: {first:?}");
        let expect_last = Duration::from_millis(150);
        let err = last.abs_diff(expect_last);
        assert!(err < Duration::from_millis(3), "last due {last:?} ≈ 150ms");
    }

    #[tokio::test(start_paused = true)]
    async fn same_seed_replays_byte_for_byte() {
        let data: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let collect = |seed: u64| {
            let (mut w, _) = spec(|s| {
                s.drop = Some(0.05);
                s.corrupt = Some(0.05);
                s.seed = Some(seed);
            })
            .build_pair();
            w.plan(&data)
                .into_iter()
                .flat_map(|c| c.data)
                .collect::<Vec<u8>>()
        };
        assert_eq!(collect(42), collect(42), "same seed → identical wire");
        assert_ne!(collect(42), collect(43), "different seed → different wire");
        // And the two directions of one seed must not mirror each other.
        let both = spec(|s| {
            s.drop = Some(0.05);
            s.seed = Some(42);
        });
        let (mut fwd, mut rev) = both.build_pair();
        let f: Vec<u8> = fwd.plan(&data).into_iter().flat_map(|c| c.data).collect();
        let r: Vec<u8> = rev.plan(&data).into_iter().flat_map(|c| c.data).collect();
        assert_ne!(f, r, "directions must be decorrelated");
    }

    #[tokio::test(start_paused = true)]
    async fn drop_rate_lands_near_p() {
        let (mut w, _) = spec(|s| {
            s.drop = Some(0.1);
            s.seed = Some(7);
        })
        .build_pair();
        let n = 100_000usize;
        let out: usize = w.plan(&vec![0u8; n]).into_iter().map(|c| c.data.len()).sum();
        let dropped = n - out;
        assert!(
            (8_000..=12_000).contains(&dropped),
            "≈10% of {n} bytes should drop, got {dropped}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn corrupt_flips_exactly_one_bit() {
        let (mut w, _) = spec(|s| {
            s.corrupt = Some(1.0);
            s.seed = Some(9);
        })
        .build_pair();
        let data: Vec<u8> = (0u8..=255).collect();
        let out: Vec<u8> = w.plan(&data).into_iter().flat_map(|c| c.data).collect();
        assert_eq!(out.len(), data.len(), "corrupt never loses bytes");
        for (a, b) in data.iter().zip(&out) {
            assert_eq!((a ^ b).count_ones(), 1, "{a:#04x} → {b:#04x} must differ by one bit");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn identity_wire_is_transparent_and_immediate() {
        let (mut w, _) = WireSpec::default().build_pair();
        let now = Instant::now();
        let data: Vec<u8> = (0u8..=255).collect();
        let cells = w.plan(&data);
        assert_eq!(cells.len(), 1, "no throttle → one cell");
        assert_eq!(cells[0].data, data);
        assert!(cells[0].due <= now + Duration::from_millis(1), "no added delay");
    }

    #[test]
    fn duration_and_probability_parsers() {
        assert_eq!(parse_duration("5ms").unwrap(), Duration::from_millis(5));
        assert_eq!(parse_duration("250us").unwrap(), Duration::from_micros(250));
        assert_eq!(parse_duration("1.5s").unwrap(), Duration::from_millis(1500));
        assert_eq!(parse_duration("7").unwrap(), Duration::from_millis(7), "bare = ms");
        assert!(parse_duration("-1ms").is_err());
        assert!(parse_duration("fast").is_err());
        assert_eq!(parse_probability("0.25").unwrap(), 0.25);
        assert!(parse_probability("1.5").is_err());
        assert!(parse_probability("-0.1").is_err());
    }
}
