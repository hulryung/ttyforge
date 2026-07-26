//! Integration tests: the `--json` readiness contract (M6).
//!
//! The plain contract (one path per line) is exercised by every other test
//! file, since they all parse it to find the port. These cover the machine-
//! readable form: still one line, still the first thing on stdout, and it
//! carries what the plain form cannot say.

use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_ttyforge");

struct Forge {
    child: Child,
    links: Vec<String>,
}

impl Drop for Forge {
    fn drop(&mut self) {
        // SAFETY: our own child's pid.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGKILL) };
        let _ = self.child.wait();
        for l in &self.links {
            let _ = std::fs::remove_file(l);
            let _ = std::fs::remove_file(format!("{l}.pid"));
        }
    }
}

/// Run a forge with `--json` and return its parsed announcement.
fn announce(args: &[&str], links: Vec<String>) -> (serde_json::Value, Forge) {
    let mut child = Command::new(BIN)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ttyforge");
    let stdout = child.stdout.take().expect("stdout piped");
    let line = BufReader::new(stdout)
        .lines()
        .next()
        .expect("a readiness line")
        .expect("read readiness line");
    let value = serde_json::from_str(&line)
        .unwrap_or_else(|e| panic!("readiness line must be JSON ({e}): {line:?}"));
    (value, Forge { child, links })
}

/// A lossy wire generates a seed for you; `--json` is how a harness captures
/// it, which is the difference between a reproducible CI failure and a story.
#[test]
fn json_readiness_names_the_forge_and_reports_the_generated_seed() {
    let a = format!("/tmp/ttyforge-test-json-{}-a.pty", std::process::id());
    let b = format!("/tmp/ttyforge-test-json-{}-b.pty", std::process::id());
    let (v, forge) = announce(
        &["pair", "--json", "--link", &a, "--link", &b, "--drop", "0.01"],
        vec![a.clone(), b.clone()],
    );

    assert_eq!(v["forge"], "pair");
    assert_eq!(v["ports"], serde_json::json!([a, b]), "in the order given");
    assert_eq!(v["pid"].as_u64().expect("pid"), forge.child.id() as u64);
    assert_eq!(v["wire"]["drop"], 0.01);
    assert!(
        v["wire"]["seed"].as_u64().is_some(),
        "a generated seed must be machine-readable, got {}",
        v["wire"]
    );
    // The ports it claims must actually be there.
    for p in [&a, &b] {
        assert!(std::fs::symlink_metadata(p).is_ok(), "{p} should exist");
    }
}

/// Each forge names its own device side, at the top level.
#[test]
fn json_readiness_describes_the_device_side() {
    let p = format!("/tmp/ttyforge-test-json-{}-sim.pty", std::process::id());
    let (v, _forge) =
        announce(&["sim", "--json", "--preset", "uboot", "--link", &p], vec![p.clone()]);
    assert_eq!(v["forge"], "sim");
    assert_eq!(v["preset"], "uboot");
    assert_eq!(v["ports"], serde_json::json!([p]));
    assert_eq!(v["wire"], serde_json::json!({}), "an untouched wire says nothing");
}
