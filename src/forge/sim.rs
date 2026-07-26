//! `ttyforge sim` — a simulated device behind a virtual tty (M2).
//!
//! Two device backends:
//!
//! **Exec** (`-- CMD ...`): spawn the child with piped stdio; port→child-stdin,
//! child-stdout→port (child stderr stays on the terminal). The fake device is
//! just a program — write it in Python:
//!
//! ```text
//! ttyforge sim --link /tmp/fake-board.pty -- python3 -u fake_board.py
//! # fake_board.py: read stdin, write responses to stdout. That's the device.
//! ```
//!
//! **Presets** (`--preset`): built-in behaviors for instant test rigs:
//!   echo    every byte straight back
//!   shell   POSIX-ish console: echoes input, `$ ` prompt, `echo`/`true`/
//!           `false`, `;` chains, `$?` — enough for serial-tether's `exec`
//!   uboot   U-Boot hush: `=> ` prompt, CR line endings, `printenv`/`setenv`/
//!           `version`, adjacent-quote concatenation (`"BE""G"` → `BEG`)
//!   at      `AT...` → `OK` / `ERROR`, the modem classic
//!
//! The preset shells implement exactly the slice of hush/POSIX that
//! serial-tether's `exec` wrapper requires: input echo (marker extraction
//! assumes the typed line is reflected), `;`-separated commands, double-quote
//! words with *adjacent-string concatenation* (the `BE""G` echo-split trick),
//! and `$?` expanded at execution time of the final `echo`.

use std::sync::Arc;

use anyhow::{bail, Context, Result};

use super::pty::{Link, SetupError, VirtualPort};
use super::signals::Shutdown;
use super::wire::{deliver, WireSpec};

/// Root error carrying an exec-backend child's nonzero exit status, so `main`
/// can pass it through as the process exit code (like `tether pty -- CMD`).
#[derive(Debug)]
pub struct ChildExit(pub i32);

impl std::fmt::Display for ChildExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "device program exited with status {}", self.0)
    }
}

impl std::error::Error for ChildExit {}

pub async fn run(
    preset: Option<String>,
    link: Option<String>,
    exec: Vec<String>,
    wire: WireSpec,
) -> Result<()> {
    if preset.is_none() && exec.is_empty() {
        bail!("pick a device: --preset echo|shell|uboot|at, or `-- CMD ...` to run your own");
    }

    let (port, slave) = VirtualPort::create()?;
    let link = Link::claim(link.as_deref(), "sim", &slave)?;

    // Signals before readiness — see `signals`.
    let mut shutdown = Shutdown::install()?;

    // Readiness contract: exactly one stdout line, flushed.
    {
        use std::io::Write as _;
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{}", link.path())?;
        stdout.flush()?;
    }

    let port = Arc::new(port);
    match preset.as_deref() {
        Some(name) => {
            eprintln!("ttyforge: sim ready: {} (preset {name}, Ctrl-C to stop)", link.path());
            preset_loop(port, &link, make_device(name)?, wire, &mut shutdown).await
        }
        None => {
            eprintln!(
                "ttyforge: sim ready: {} (device: {}, Ctrl-C to stop)",
                link.path(),
                exec.join(" ")
            );
            exec_loop(port, &link, exec, wire, &mut shutdown).await
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Preset backend
// ──────────────────────────────────────────────────────────────────────────

/// A simulated device as a pure state machine: bytes in, bytes out. Pure so
/// every preset is unit-testable without a pty in sight.
trait Device: Send {
    /// Written once at startup (lands in the pty buffer, greets the first
    /// consumer — like a board that printed its prompt before you attached).
    fn greeting(&mut self) -> Vec<u8> {
        Vec::new()
    }
    fn feed(&mut self, input: &[u8]) -> Vec<u8>;
}

fn make_device(name: &str) -> Result<Box<dyn Device>> {
    Ok(match name {
        "echo" => Box::new(EchoDevice),
        "at" => Box::new(LineDevice::new(AtHandler)),
        "shell" => Box::new(LineDevice::new(ShellHandler::posix())),
        "uboot" => Box::new(LineDevice::new(ShellHandler::uboot())),
        other => bail!("unknown preset {other:?} (echo|shell|uboot|at)"),
    })
}

async fn preset_loop(
    port: Arc<VirtualPort>,
    link: &Link,
    mut device: Box<dyn Device>,
    wire: WireSpec,
    shutdown: &mut Shutdown,
) -> Result<()> {
    // Two wire directions: consumer→device and device→consumer.
    let (mut wire_in, mut wire_out) = wire.build_pair();
    deliver(&mut wire_out, &device.greeting(), &port).await.context("write greeting")?;

    let mut termios_tick = tokio::time::interval(std::time::Duration::from_millis(500));
    termios_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut buf = [0u8; 4096];

    loop {
        tokio::select! {
            r = port.read(&mut buf) => match r {
                Ok(0) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                Ok(n) => {
                    // Inbound bytes cross the wire (drop/corrupt/pacing)
                    // before the device sees them; responses cross it again
                    // on the way out.
                    for cell in wire_in.plan(&buf[..n]) {
                        tokio::time::sleep_until(cell.due).await;
                        let out = device.feed(&cell.data);
                        deliver(&mut wire_out, &out, &port)
                            .await
                            .context("write response")?;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "port read failed");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            },
            _ = shutdown.recv() => break,
            _ = termios_tick.tick() => {
                if port.reassert_raw_if_needed() {
                    tracing::debug!(port = link.path(), "reasserted raw termios");
                }
            }
        }
    }
    eprintln!("ttyforge: sim torn down");
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────
// Exec backend
// ──────────────────────────────────────────────────────────────────────────

/// Bridge the port to a child process's stdio. Mirrors serial-tether's
/// `tether zmodem` lrzsz bridge with the direction inverted: there the child
/// consumes the port, here the child *is* the device. Exits when the child
/// does, passing its status through.
async fn exec_loop(
    port: Arc<VirtualPort>,
    link: &Link,
    exec: Vec<String>,
    wire: WireSpec,
    shutdown: &mut Shutdown,
) -> Result<()> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let (mut wire_in, mut wire_out) = wire.build_pair();

    let (prog, args) = exec.split_first().expect("exec non-empty (validated)");
    let mut child = tokio::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .env("TTYFORGE_PTY", link.path())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn device program `{prog}`"))
        .context(SetupError)?;

    let mut child_stdin = child.stdin.take().expect("stdin piped");
    let mut child_stdout = child.stdout.take().expect("stdout piped");

    // Port → child stdin.
    let to_child = {
        let port = port.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match port.read(&mut buf).await {
                    Ok(0) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                    Ok(n) => {
                        let mut dead = false;
                        for cell in wire_in.plan(&buf[..n]) {
                            tokio::time::sleep_until(cell.due).await;
                            if child_stdin.write_all(&cell.data).await.is_err() {
                                dead = true; // child gone; wait() below reports it
                                break;
                            }
                            let _ = child_stdin.flush().await;
                        }
                        if dead {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
    };

    // Child stdout → port.
    let from_child = {
        let port = port.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match child_stdout.read(&mut buf).await {
                    Ok(0) => break, // child closed stdout
                    Ok(n) => {
                        if deliver(&mut wire_out, &buf[..n], &port).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
    };

    let mut termios_tick = tokio::time::interval(std::time::Duration::from_millis(500));
    termios_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let status = loop {
        tokio::select! {
            s = child.wait() => break Some(s.context("wait on device program")?),
            _ = shutdown.recv() => break None,
            _ = termios_tick.tick() => {
                if port.reassert_raw_if_needed() {
                    tracing::debug!(port = link.path(), "reasserted raw termios");
                }
            }
        }
    };
    to_child.abort();
    from_child.abort();
    eprintln!("ttyforge: sim torn down");

    match status {
        None => Ok(()), // signal — child killed on drop
        Some(s) if s.success() => Ok(()),
        Some(s) => Err(anyhow::Error::new(ChildExit(s.code().unwrap_or(-1)))),
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Byte-level presets
// ──────────────────────────────────────────────────────────────────────────

struct EchoDevice;

impl Device for EchoDevice {
    fn feed(&mut self, input: &[u8]) -> Vec<u8> {
        input.to_vec()
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Line discipline shared by the line-oriented presets
// ──────────────────────────────────────────────────────────────────────────

/// What a line-oriented device does once a full line has been entered.
trait LineHandler: Send {
    fn prompt(&self) -> &[u8] {
        b""
    }
    fn on_line(&mut self, line: &str) -> Vec<u8>;
}

/// Serial-console line editing the way real firmware does it: echo typed
/// characters back, CR (or LF; CRLF counts once) ends the line, backspace
/// rubs out, Ctrl-C abandons the line.
struct LineDevice<H: LineHandler> {
    line: Vec<u8>,
    last_was_cr: bool,
    handler: H,
}

impl<H: LineHandler> LineDevice<H> {
    fn new(handler: H) -> Self {
        Self { line: Vec::new(), last_was_cr: false, handler }
    }
}

impl<H: LineHandler> Device for LineDevice<H> {
    fn greeting(&mut self) -> Vec<u8> {
        self.handler.prompt().to_vec()
    }

    fn feed(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len() + 16);
        for &b in input {
            let was_cr = std::mem::replace(&mut self.last_was_cr, false);
            match b {
                b'\n' if was_cr => {} // LF of a CRLF pair — already handled
                b'\r' | b'\n' => {
                    out.extend_from_slice(b"\r\n");
                    let line = std::mem::take(&mut self.line);
                    out.extend(self.handler.on_line(&String::from_utf8_lossy(&line)));
                    out.extend_from_slice(self.handler.prompt());
                    self.last_was_cr = b == b'\r';
                }
                0x08 | 0x7f => {
                    if self.line.pop().is_some() {
                        out.extend_from_slice(b"\x08 \x08");
                    }
                }
                0x03 => {
                    self.line.clear();
                    out.extend_from_slice(b"^C\r\n");
                    out.extend_from_slice(self.handler.prompt());
                }
                _ => {
                    self.line.push(b);
                    out.push(b); // echo, like a real console
                }
            }
        }
        out
    }
}

// ──────────────────────────────────────────────────────────────────────────
// AT modem preset
// ──────────────────────────────────────────────────────────────────────────

struct AtHandler;

impl LineHandler for AtHandler {
    fn on_line(&mut self, line: &str) -> Vec<u8> {
        let t = line.trim();
        if t.is_empty() {
            Vec::new()
        } else if t.len() >= 2 && t[..2].eq_ignore_ascii_case("at") {
            b"OK\r\n".to_vec()
        } else {
            b"ERROR\r\n".to_vec()
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Shell presets (posix / uboot)
// ──────────────────────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum ShellKind {
    Posix,
    Uboot,
}

struct ShellHandler {
    kind: ShellKind,
    env: std::collections::BTreeMap<String, String>,
    last_status: i32,
}

impl ShellHandler {
    fn posix() -> Self {
        Self { kind: ShellKind::Posix, env: std::collections::BTreeMap::new(), last_status: 0 }
    }

    fn uboot() -> Self {
        let env = [
            ("arch", "sandbox"),
            ("baudrate", "115200"),
            ("board", "ttyforge-sim"),
            ("bootcmd", "echo Hello from ttyforge"),
            ("bootdelay", "2"),
            ("stdin", "serial"),
            ("stdout", "serial"),
            ("stderr", "serial"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        Self { kind: ShellKind::Uboot, env, last_status: 0 }
    }

    fn run_one(&mut self, cmd: &str) -> Vec<u8> {
        // Expansion happens here, per command, *at execution time* — so the
        // `$?` in serial-tether's trailing `echo "...=$?"` sees the status of
        // the user command that ran just before it.
        let words = expand_words(cmd, &self.env, self.last_status);
        let Some(head) = words.first() else {
            return Vec::new();
        };
        match head.as_str() {
            "echo" => {
                self.last_status = 0;
                let mut out = words[1..].join(" ").into_bytes();
                out.extend_from_slice(b"\r\n");
                out
            }
            "true" => {
                self.last_status = 0;
                Vec::new()
            }
            "false" => {
                self.last_status = 1;
                Vec::new()
            }
            "printenv" if self.kind == ShellKind::Uboot => {
                if words.len() == 1 {
                    let mut out = Vec::new();
                    let mut size = 0usize;
                    for (k, v) in &self.env {
                        out.extend_from_slice(format!("{k}={v}\r\n").as_bytes());
                        size += k.len() + v.len() + 2;
                    }
                    out.extend_from_slice(
                        format!("\r\nEnvironment size: {size}/8188 bytes\r\n").as_bytes(),
                    );
                    self.last_status = 0;
                    out
                } else {
                    let mut out = Vec::new();
                    self.last_status = 0;
                    for name in &words[1..] {
                        match self.env.get(name) {
                            Some(v) => out.extend_from_slice(format!("{name}={v}\r\n").as_bytes()),
                            None => {
                                out.extend_from_slice(
                                    format!("## Error: \"{name}\" not defined\r\n").as_bytes(),
                                );
                                self.last_status = 1;
                            }
                        }
                    }
                    out
                }
            }
            "setenv" if self.kind == ShellKind::Uboot => {
                match words.get(1) {
                    Some(name) if words.len() == 2 => {
                        self.env.remove(name);
                    }
                    Some(name) => {
                        self.env.insert(name.clone(), words[2..].join(" "));
                    }
                    None => {}
                }
                self.last_status = 0;
                Vec::new()
            }
            "version" if self.kind == ShellKind::Uboot => {
                self.last_status = 0;
                b"U-Boot 2024.01-ttyforge-sim\r\n\r\n".to_vec()
            }
            other => match self.kind {
                ShellKind::Posix => {
                    self.last_status = 127;
                    format!("sh: {other}: command not found\r\n").into_bytes()
                }
                ShellKind::Uboot => {
                    self.last_status = 1;
                    format!("Unknown command '{other}' - try 'help'\r\n").into_bytes()
                }
            },
        }
    }
}

impl LineHandler for ShellHandler {
    fn prompt(&self) -> &[u8] {
        match self.kind {
            ShellKind::Posix => b"$ ",
            ShellKind::Uboot => b"=> ",
        }
    }

    fn on_line(&mut self, line: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for cmd in split_commands(line) {
            out.extend(self.run_one(&cmd));
        }
        out
    }
}

/// Split a line on `;` outside quotes. Quotes are preserved in the pieces —
/// expansion is `expand_words`' job, later, at execution time.
fn split_commands(line: &str) -> Vec<String> {
    let mut cmds = Vec::new();
    let mut cur = String::new();
    let (mut in_s, mut in_d) = (false, false);
    for c in line.chars() {
        match c {
            '\'' if !in_d => {
                in_s = !in_s;
                cur.push(c);
            }
            '"' if !in_s => {
                in_d = !in_d;
                cur.push(c);
            }
            ';' if !in_s && !in_d => cmds.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    cmds.push(cur);
    cmds.retain(|c| !c.trim().is_empty());
    cmds
}

/// Tokenize one command into words with POSIX-ish quoting:
/// - double quotes group and *concatenate adjacently* (`"BE""G"` → `BEG` —
///   the trick serial-tether's exec wrapper depends on),
/// - single quotes are literal,
/// - `$?` expands to the last status, `$NAME` to the env (empty if unset),
///   in bare words and inside double quotes.
fn expand_words(
    cmd: &str,
    env: &std::collections::BTreeMap<String, String>,
    last_status: i32,
) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut has_content = false; // distinguishes `""` (empty word) from nothing
    let mut chars = cmd.chars().peekable();

    let expand_dollar =
        |chars: &mut std::iter::Peekable<std::str::Chars>, cur: &mut String| match chars.peek() {
            Some('?') => {
                chars.next();
                cur.push_str(&last_status.to_string());
            }
            Some(c) if c.is_ascii_alphanumeric() || *c == '_' => {
                let mut name = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        name.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if let Some(v) = env.get(&name) {
                    cur.push_str(v);
                }
            }
            _ => cur.push('$'),
        };

    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if has_content {
                    words.push(std::mem::take(&mut cur));
                    has_content = false;
                }
            }
            '\'' => {
                has_content = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    cur.push(c);
                }
            }
            '"' => {
                has_content = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '$' => expand_dollar(&mut chars, &mut cur),
                        _ => cur.push(c),
                    }
                }
            }
            '$' => {
                // Bare expansion: only counts as word content if it actually
                // produced characters ($missing → empty → no word, like sh).
                let before = cur.len();
                expand_dollar(&mut chars, &mut cur);
                if cur.len() > before {
                    has_content = true;
                }
            }
            _ => {
                has_content = true;
                cur.push(c);
            }
        }
    }
    if has_content {
        words.push(cur);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_str(d: &mut dyn Device, s: &str) -> String {
        String::from_utf8_lossy(&d.feed(s.as_bytes())).into_owned()
    }

    #[test]
    fn expand_words_concatenates_adjacent_quotes() {
        let env = Default::default();
        assert_eq!(
            expand_words(r#"echo "TETHEREXECBE""Gabc123""#, &env, 0),
            vec!["echo", "TETHEREXECBEGabc123"],
            "the echo-split trick must reassemble contiguously"
        );
        assert_eq!(expand_words(r#"echo "a b" 'c d' e"#, &env, 0), vec!["echo", "a b", "c d", "e"]);
        assert_eq!(expand_words(r#"echo """#, &env, 0), vec!["echo", ""]);
    }

    #[test]
    fn expand_words_expands_status_and_env() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("baudrate".to_string(), "115200".to_string());
        assert_eq!(expand_words(r#"echo "st=$?""#, &env, 42), vec!["echo", "st=42"]);
        assert_eq!(expand_words("echo $baudrate $missing", &env, 0), vec!["echo", "115200"]);
        assert_eq!(expand_words("echo '$?'", &env, 7), vec!["echo", "$?"], "single quotes literal");
    }

    #[test]
    fn split_commands_respects_quotes() {
        assert_eq!(split_commands(r#"echo "a;b"; true"#), vec![r#"echo "a;b""#, " true"]);
        assert_eq!(split_commands("  ;; echo x ;"), vec![" echo x "]);
    }

    /// The full serial-tether exec wrapper, exactly as typed at the device,
    /// against the uboot preset: begin marker on its own line, command output,
    /// end marker carrying the *user command's* status.
    #[test]
    fn uboot_runs_the_tether_exec_wrapper() {
        let mut d = LineDevice::new(ShellHandler::uboot());
        let line = "echo \"TETHEREXECBE\"\"Gtag1\"; printenv baudrate; echo \"TETHEREXECEN\"\"Dtag1=$?\"\r";
        let out = feed_str(&mut d, line);
        assert!(out.contains("TETHEREXECBEGtag1\r\n"), "begin marker line: {out:?}");
        assert!(out.contains("baudrate=115200\r\n"), "command output: {out:?}");
        assert!(out.contains("TETHEREXECENDtag1=0\r\n"), "end marker status 0: {out:?}");
        assert!(out.ends_with("=> "), "prompt reprinted: {out:?}");
        // The echoed command must NOT contain the contiguous begin marker —
        // that's the whole point of the typed `BE""G` split.
        let echoed = &out[..out.find("\r\n").unwrap()];
        assert!(!echoed.contains("TETHEREXECBEGtag1"), "echo must stay split: {echoed:?}");

        // Failing command → its status lands in the end marker.
        let out = feed_str(&mut d, "echo \"B\"\"EGx\"; false; echo \"E\"\"NDx=$?\"\r");
        assert!(out.contains("ENDx=1\r\n"), "false → status 1: {out:?}");
        let out = feed_str(&mut d, "echo \"B\"\"EGy\"; nosuchcmd; echo \"E\"\"NDy=$?\"\r");
        assert!(out.contains("Unknown command 'nosuchcmd'"), "{out:?}");
        assert!(out.contains("ENDy=1\r\n"), "unknown → status 1: {out:?}");
    }

    #[test]
    fn posix_shell_reports_127_for_unknown() {
        let mut d = LineDevice::new(ShellHandler::posix());
        let out = feed_str(&mut d, "nosuchcmd; echo \"rc=$?\"\r");
        assert!(out.contains("sh: nosuchcmd: command not found\r\n"), "{out:?}");
        assert!(out.contains("rc=127\r\n"), "{out:?}");
        assert!(out.ends_with("$ "), "{out:?}");
    }

    #[test]
    fn uboot_setenv_printenv_roundtrip() {
        let mut d = LineDevice::new(ShellHandler::uboot());
        feed_str(&mut d, "setenv ipaddr 192.168.1.7\r");
        let out = feed_str(&mut d, "printenv ipaddr\r");
        assert!(out.contains("ipaddr=192.168.1.7\r\n"), "{out:?}");
        let out = feed_str(&mut d, "setenv ipaddr; printenv ipaddr\r");
        assert!(out.contains("## Error: \"ipaddr\" not defined\r\n"), "{out:?}");
    }

    #[test]
    fn at_modem_ok_and_error() {
        let mut d = LineDevice::new(AtHandler);
        assert!(feed_str(&mut d, "AT\r").contains("OK\r\n"));
        assert!(feed_str(&mut d, "ati\r").contains("OK\r\n"), "case-insensitive");
        assert!(feed_str(&mut d, "hello\r").contains("ERROR\r\n"));
        assert_eq!(feed_str(&mut d, "\r"), "\r\n", "empty line → no response");
    }

    #[test]
    fn line_editing_backspace_and_crlf() {
        let mut d = LineDevice::new(LineHandlerEcho);
        // "ab" + backspace + "c" → line is "ac".
        let out = feed_str(&mut d, "ab\x7fc\r");
        assert!(out.contains("line:ac"), "{out:?}");
        // CRLF must execute once, not twice.
        let mut d = LineDevice::new(LineHandlerEcho);
        let out = feed_str(&mut d, "x\r\ny\r");
        assert_eq!(out.matches("line:").count(), 2, "CRLF is one line end: {out:?}");
        assert!(out.contains("line:x") && out.contains("line:y"), "{out:?}");
    }

    #[test]
    fn ctrl_c_abandons_the_line() {
        let mut d = LineDevice::new(LineHandlerEcho);
        let out = feed_str(&mut d, "junk\x03ok\r");
        assert!(out.contains("^C"), "{out:?}");
        assert!(out.contains("line:ok"), "{out:?}");
        assert!(!out.contains("line:junkok"), "{out:?}");
    }

    /// Minimal handler for line-discipline tests.
    struct LineHandlerEcho;
    impl LineHandler for LineHandlerEcho {
        fn on_line(&mut self, line: &str) -> Vec<u8> {
            format!("line:{line}\r\n").into_bytes()
        }
    }
}
