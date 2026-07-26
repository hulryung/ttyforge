//! The four forges plus the shared plumbing they stand on.
//!
//!   pty    the virtual-port core every forge uses (M1)
//!   wire   timing/fault layer between any two ends (M3)
//!   pair   virtual null-modem cable (M1)
//!   sim    simulated device behind a port (M2)
//!   bridge TCP peer behind a port (M4)
//!   rfc2217 telnet COM-PORT-OPTION client for bridge (M4b)
//!   mux    real port fanned out to N virtual ports (M5)

pub mod bridge;
pub mod mux;
pub mod pair;
pub mod pty;
pub mod rfc2217;
pub mod sim;
pub mod wire;
