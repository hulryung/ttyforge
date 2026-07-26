//! The readiness announcement (M6).
//!
//! Every forge tells stdout it is up, exactly once, and flushes. Plain mode
//! prints one port path per line — the contract scripts have read since M1,
//! and it stays byte-for-byte what it was. `--json` swaps that for a single
//! object on the same stream at the same moment, which buys three things the
//! plain form cannot express: which forge is running, its pid, and the wire
//! seed — including one that was *generated* for you, the single value a
//! failing CI run needs to replay a lossy wire.
//!
//! Still exactly one line either way (well, one per port in plain mode), so
//! `read`, `jq`, and `head -1` all work unchanged.

use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Map, Value};

use super::wire::WireSpec;

pub fn announce(
    json_mode: bool,
    forge: &str,
    ports: &[&str],
    details: Value,
    wire: &WireSpec,
) -> Result<()> {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    if !json_mode {
        for p in ports {
            writeln!(out, "{p}")?;
        }
        out.flush()?;
        return Ok(());
    }
    writeln!(out, "{}", object(forge, ports, details, wire))?;
    out.flush()?;
    Ok(())
}

/// Built separately from the IO so the shape is unit-testable.
fn object(forge: &str, ports: &[&str], details: Value, wire: &WireSpec) -> Value {
    let mut obj = Map::new();
    obj.insert("forge".into(), json!(forge));
    obj.insert("pid".into(), json!(std::process::id()));
    obj.insert("ports".into(), json!(ports));
    // Per-forge extras sit at the top level: `.endpoint`, `.device`, `.preset`
    // read better than `.details.endpoint`.
    if let Value::Object(extra) = details {
        obj.extend(extra);
    }
    obj.insert("wire".into(), wire_value(wire));
    Value::Object(obj)
}

/// Only the knobs actually in use, so an untouched wire is `{}` rather than a
/// screenful of nulls.
fn wire_value(w: &WireSpec) -> Value {
    let mut m = Map::new();
    if let Some(b) = w.baud {
        m.insert("baud_sim".into(), json!(b));
    }
    if let Some(d) = w.latency {
        m.insert("latency_ms".into(), json!(millis(d)));
    }
    if let Some(d) = w.jitter {
        m.insert("jitter_ms".into(), json!(millis(d)));
    }
    if let Some(p) = w.drop {
        m.insert("drop".into(), json!(p));
    }
    if let Some(p) = w.corrupt {
        m.insert("corrupt".into(), json!(p));
    }
    if let Some(s) = w.seed {
        m.insert("seed".into(), json!(s));
    }
    Value::Object(m)
}

fn millis(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_forge_reports_ports_and_an_empty_wire() {
        let v = object("pair", &["/tmp/a.pty", "/tmp/b.pty"], Value::Null, &WireSpec::default());
        assert_eq!(v["forge"], "pair");
        assert_eq!(v["ports"], json!(["/tmp/a.pty", "/tmp/b.pty"]));
        assert_eq!(v["wire"], json!({}), "an untouched wire says nothing");
        assert!(v["pid"].as_u64().unwrap() > 0);
    }

    #[test]
    fn details_land_at_the_top_level_and_the_seed_is_reported() {
        let wire = WireSpec {
            baud: Some(9600),
            latency: Some(Duration::from_micros(2500)),
            drop: Some(0.01),
            seed: Some(42),
            ..WireSpec::default()
        };
        let v = object(
            "bridge",
            &["/tmp/b.pty"],
            json!({"endpoint": "tcp://lab:2000",
                     "rfc2217": true}),
            &wire,
        );
        assert_eq!(v["endpoint"], "tcp://lab:2000");
        assert_eq!(v["rfc2217"], true);
        assert_eq!(v["wire"]["baud_sim"], 9600);
        assert_eq!(v["wire"]["latency_ms"], 2.5, "sub-millisecond survives");
        assert_eq!(v["wire"]["drop"], 0.01);
        assert_eq!(v["wire"]["seed"], 42, "the replay value must be machine-readable");
        assert!(v["wire"].get("jitter_ms").is_none(), "unused knobs stay absent");
    }

    /// One line, and a path with characters that would break naive quoting
    /// still round-trips — the reason this uses a JSON writer at all.
    #[test]
    fn output_is_one_parseable_line_even_for_awkward_paths() {
        let path = r#"/tmp/we"ird\path.pty"#;
        let text =
            object("sim", &[path], json!({"preset": "uboot"}), &WireSpec::default()).to_string();
        assert!(!text.contains('\n'), "must stay on one line");
        let back: Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(back["ports"][0], path);
        assert_eq!(back["preset"], "uboot");
    }
}
