//! Wire model: the timing/fault layer spliced between any two forge ends (M3).
//!
//! A real UART is slow and imperfect; a pty is instant and perfect. Tests that
//! pass against a perfect wire routinely fail on hardware. This layer makes
//! the virtual wire honest, per direction:
//!
//!   --baud-sim 115200     throttle throughput to the real line rate
//!   --latency 5ms         fixed per-chunk delay
//!   --jitter 2ms          random extra delay
//!   --drop 0.001          probability a byte vanishes
//!   --corrupt 0.0001      probability a byte is bit-flipped
//!
//! Planned surface: `Wire::new(WireSpec)` wrapping an async byte pump;
//! identity (zero-cost) when no spec is given. Deterministic with `--seed`
//! so CI failures reproduce.
