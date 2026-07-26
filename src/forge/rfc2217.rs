//! RFC2217 client: telnet framing + COM-PORT-OPTION (M4b).
//!
//! A pty has no UART, so the baud/parity/framing a tool sets on a ttyforge
//! port is inert (PLAN §5). RFC2217 is how that intent reaches a *real* UART
//! at the far end: the virtual port's termios changes become telnet
//! COM-PORT-OPTION subnegotiations, and a ser2net-style server retunes the
//! hardware.
//!
//! Two consequences shape this module:
//!
//! 1. **The peer stream stops being raw.** RFC2217 rides on telnet, so a
//!    0xFF data byte must go out doubled (`IAC IAC`) and come back collapsed,
//!    and negotiation/subnegotiation bytes are interleaved with data. Getting
//!    this wrong corrupts binary transfers exactly like a non-raw pty does —
//!    the same class of bug as the ZMODEM Bad CRC that started this project.
//!    Hence a codec with a byte-at-a-time state machine, unit-tested against
//!    inputs split at every boundary.
//! 2. **We must notice termios changes without being told.** A pty master
//!    gets no notification when the slave is reconfigured, so [`PortSettings`]
//!    is polled and diffed (the "A" of the polling-vs-TIOCPKT decision in
//!    PLAN §6): state-based, self-healing, identical on macOS and Linux, and
//!    no surgery on `VirtualPort::read` — the one function every forge's
//!    binary transparency rests on.
//!
//! Out of scope, and not fixable here: DTR/RTS. Those are modem lines, not
//! termios state, and a pty has none — a consumer's `TIOCMSET` is invisible
//! to the master, so RFC2217's SET-CONTROL 8..=12 has no local trigger.

// ── telnet (RFC 854) ──────────────────────────────────────────────────────
const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;

const OPT_BINARY: u8 = 0;
const OPT_SGA: u8 = 3;
const OPT_COM_PORT: u8 = 44;

// ── COM-PORT-OPTION (RFC2217) client→server commands ──────────────────────
const SET_BAUDRATE: u8 = 1;
const SET_DATASIZE: u8 = 2;
const SET_PARITY: u8 = 3;
const SET_STOPSIZE: u8 = 4;
const SET_CONTROL: u8 = 5;
/// Server→client answers are the client command + 100.
const SERVER_OFFSET: u8 = 100;

/// Everything one `decode()` call produced: payload for the port, bytes owed
/// to the peer, and human-readable server answers worth logging.
#[derive(Debug, Default, PartialEq)]
pub struct Decoded {
    pub data: Vec<u8>,
    pub reply: Vec<u8>,
    pub notices: Vec<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Data,
    Iac,
    /// Saw IAC WILL/WONT/DO/DONT; the next byte is the option.
    Negotiate(u8),
    Sub,
    SubIac,
}

/// Telnet codec for one peer session. Owns the negotiation state, so a
/// reconnect starts from a clean slate (as it must — the new server has
/// agreed to nothing).
pub struct Telnet {
    state: State,
    sub: Vec<u8>,
    /// Options we have offered/accepted, tracked so we never acknowledge an
    /// acknowledgement (RFC 854's negotiation loop trap).
    binary_us: bool,
    binary_them: bool,
    sga_us: bool,
    sga_them: bool,
    /// False once the server refuses COM-PORT-OPTION — then forwarding
    /// settings is pointless and we say so once instead of every tick.
    com_port: bool,
}

impl Default for Telnet {
    fn default() -> Self {
        Self::new()
    }
}

impl Telnet {
    pub fn new() -> Self {
        Self {
            state: State::Data,
            sub: Vec::new(),
            // Optimistic: we send these offers in `start()`, so an incoming
            // confirmation must not draw another reply.
            binary_us: true,
            binary_them: true,
            sga_us: true,
            sga_them: true,
            com_port: true,
        }
    }

    /// The opening offers, sent before any data: 8-bit clean both ways
    /// (BINARY), no line-at-a-time turn taking (SGA), and COM-PORT-OPTION.
    pub fn start(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(15);
        for (cmd, opt) in [
            (WILL, OPT_BINARY),
            (DO, OPT_BINARY),
            (WILL, OPT_SGA),
            (DO, OPT_SGA),
            (WILL, OPT_COM_PORT),
        ] {
            out.extend_from_slice(&[IAC, cmd, opt]);
        }
        out
    }

    /// True until the server refuses COM-PORT-OPTION.
    pub fn com_port_ok(&self) -> bool {
        self.com_port
    }

    /// Feed bytes from the peer; get back port payload plus anything we owe
    /// the peer. Safe to call with input split at any boundary — the state
    /// machine spans calls.
    pub fn decode(&mut self, input: &[u8]) -> Decoded {
        let mut out = Decoded::default();
        for &b in input {
            match self.state {
                State::Data => {
                    if b == IAC {
                        self.state = State::Iac;
                    } else {
                        out.data.push(b);
                    }
                }
                State::Iac => match b {
                    IAC => {
                        // Escaped 0xFF: one data byte, not a command.
                        out.data.push(IAC);
                        self.state = State::Data;
                    }
                    WILL | WONT | DO | DONT => self.state = State::Negotiate(b),
                    SB => {
                        self.sub.clear();
                        self.state = State::Sub;
                    }
                    // NOP, Data Mark, Break, … — nothing we act on.
                    _ => self.state = State::Data,
                },
                State::Negotiate(cmd) => {
                    if let Some(reply) = self.negotiate(cmd, b) {
                        out.reply.extend_from_slice(&[IAC, reply, b]);
                    }
                    self.state = State::Data;
                }
                State::Sub => {
                    if b == IAC {
                        self.state = State::SubIac;
                    } else {
                        self.sub.push(b);
                    }
                }
                State::SubIac => match b {
                    IAC => {
                        self.sub.push(IAC); // escaped 0xFF inside a subneg
                        self.state = State::Sub;
                    }
                    SE => {
                        if let Some(n) = self.subnegotiation() {
                            out.notices.push(n);
                        }
                        self.sub.clear();
                        self.state = State::Data;
                    }
                    // Malformed (IAC <cmd> inside SB): drop the subneg rather
                    // than let its bytes leak into the data stream.
                    _ => {
                        self.sub.clear();
                        self.state = State::Data;
                    }
                },
            }
        }
        out
    }

    /// Answer one WILL/WONT/DO/DONT. `None` means "already in that state" —
    /// replying anyway is what makes two polite implementations loop forever.
    fn negotiate(&mut self, cmd: u8, opt: u8) -> Option<u8> {
        let known = matches!(opt, OPT_BINARY | OPT_SGA | OPT_COM_PORT);
        if !known {
            // Refuse everything else, once: the peer must not re-offer after
            // a refusal, so this cannot loop.
            return Some(match cmd {
                WILL | WONT => DONT,
                _ => WONT,
            });
        }
        match cmd {
            // The peer offers to do it themselves.
            WILL | WONT => {
                let agreed = cmd == WILL;
                let slot = match opt {
                    OPT_BINARY => &mut self.binary_them,
                    OPT_SGA => &mut self.sga_them,
                    _ => &mut self.com_port,
                };
                if *slot == agreed {
                    return None;
                }
                *slot = agreed;
                Some(if agreed { DO } else { DONT })
            }
            // The peer asks us to do it.
            _ => {
                let agreed = cmd == DO;
                let slot = match opt {
                    OPT_BINARY => &mut self.binary_us,
                    OPT_SGA => &mut self.sga_us,
                    _ => &mut self.com_port,
                };
                if *slot == agreed {
                    return None;
                }
                *slot = agreed;
                Some(if agreed { WILL } else { WONT })
            }
        }
    }

    /// Describe a server answer (`SET-x + 100`) for the log. We do not adopt
    /// it: the consumer's termios stays the source of truth, and the answer
    /// is how the user learns the far end could not honour a request.
    fn subnegotiation(&self) -> Option<String> {
        let (&opt, rest) = self.sub.split_first()?;
        if opt != OPT_COM_PORT {
            return None;
        }
        let (&cmd, params) = rest.split_first()?;
        let name = match cmd.checked_sub(SERVER_OFFSET)? {
            SET_BAUDRATE => "baudrate",
            SET_DATASIZE => "datasize",
            SET_PARITY => "parity",
            SET_STOPSIZE => "stopsize",
            SET_CONTROL => "control",
            _ => return None,
        };
        let value = if params.len() == 4 {
            u32::from_be_bytes([params[0], params[1], params[2], params[3]])
        } else {
            *params.first()? as u32
        };
        Some(format!("peer {name} = {value}"))
    }
}

/// Double every 0xFF so the peer reads it as data, not as IAC.
pub fn escape(data: &[u8]) -> Vec<u8> {
    if !data.contains(&IAC) {
        return data.to_vec(); // the overwhelmingly common case
    }
    let mut out = Vec::with_capacity(data.len() + 8);
    for &b in data {
        out.push(b);
        if b == IAC {
            out.push(IAC);
        }
    }
    out
}

/// `IAC SB COM-PORT-OPTION <cmd> <params> IAC SE`, with params escaped —
/// a baud rate whose big-endian encoding contains 0xFF is still a baud rate.
fn subneg(cmd: u8, params: &[u8]) -> Vec<u8> {
    let mut out = vec![IAC, SB, OPT_COM_PORT, cmd];
    out.extend(escape(params));
    out.extend_from_slice(&[IAC, SE]);
    out
}

// ──────────────────────────────────────────────────────────────────────────
// termios → RFC2217
// ──────────────────────────────────────────────────────────────────────────

/// The line parameters RFC2217 can carry, in RFC2217's own encoding.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PortSettings {
    /// Bits per second. 0 means "unset" — RFC2217 reads it as *query the
    /// current value*, which is exactly right for a port nobody configured.
    pub baud: u32,
    pub data_bits: u8,
    /// 1 none, 2 odd, 3 even.
    pub parity: u8,
    /// 1 one stop bit, 2 two.
    pub stop_bits: u8,
    /// 1 none, 2 XON/XOFF, 3 RTS/CTS.
    pub flow: u8,
}

impl PortSettings {
    /// A freshly forged raw port: 8N1, no flow control, no speed chosen.
    pub const UNSET: Self = Self {
        baud: 0,
        data_bits: 8,
        parity: 1,
        stop_bits: 1,
        flow: 1,
    };

    pub fn from_termios(tio: &libc::termios) -> Self {
        // SAFETY: reads a field of a caller-owned, initialized termios.
        let speed = unsafe { libc::cfgetospeed(tio) };
        let data_bits = match tio.c_cflag & libc::CSIZE {
            libc::CS5 => 5,
            libc::CS6 => 6,
            libc::CS7 => 7,
            _ => 8,
        };
        let parity = if tio.c_cflag & libc::PARENB == 0 {
            1
        } else if tio.c_cflag & libc::PARODD != 0 {
            2
        } else {
            3
        };
        Self {
            baud: super::serial::baud_of(speed),
            data_bits,
            parity,
            stop_bits: if tio.c_cflag & libc::CSTOPB != 0 { 2 } else { 1 },
            flow: if tio.c_cflag & libc::CRTSCTS != 0 {
                3
            } else if tio.c_iflag & (libc::IXON | libc::IXOFF) != 0 {
                2
            } else {
                1
            },
        }
    }

    /// The subnegotiations that turn `self` into `new` — only what changed,
    /// so an idle port stays silent on the wire.
    pub fn commands_to(&self, new: &Self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if new.baud != self.baud {
            out.push(subneg(SET_BAUDRATE, &new.baud.to_be_bytes()));
        }
        if new.data_bits != self.data_bits {
            out.push(subneg(SET_DATASIZE, &[new.data_bits]));
        }
        if new.parity != self.parity {
            out.push(subneg(SET_PARITY, &[new.parity]));
        }
        if new.stop_bits != self.stop_bits {
            out.push(subneg(SET_STOPSIZE, &[new.stop_bits]));
        }
        if new.flow != self.flow {
            out.push(subneg(SET_CONTROL, &[new.flow]));
        }
        out
    }

    /// One-line summary for the log when settings move.
    pub fn describe(&self) -> String {
        let p = match self.parity {
            2 => 'O',
            3 => 'E',
            _ => 'N',
        };
        let f = match self.flow {
            2 => " xon/xoff",
            3 => " rts/cts",
            _ => "",
        };
        format!("{} {}{}{}{}", self.baud, self.data_bits, p, self.stop_bits, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_termios() -> libc::termios {
        let mut tio: libc::termios = unsafe { std::mem::zeroed() };
        unsafe { libc::cfmakeraw(&mut tio) };
        tio
    }

    #[test]
    fn escaping_is_transparent_for_every_byte_value() {
        let all: Vec<u8> = (0u8..=255).collect();
        let wire = escape(&all);
        assert_eq!(wire.len(), all.len() + 1, "exactly one 0xFF gets doubled");
        let mut t = Telnet::new();
        assert_eq!(t.decode(&wire).data, all, "escape → decode is identity");
    }

    /// A TCP read can split anywhere, including between IAC and its command
    /// byte. Feeding one byte at a time must produce the identical result.
    #[test]
    fn decoding_survives_a_split_at_every_boundary() {
        let mut stream = escape(&[0x00, 0xFF, 0x41]);
        stream.extend_from_slice(&[IAC, DO, OPT_COM_PORT]); // a negotiation
        stream.extend_from_slice(&escape(&[0xFF])); // …then more data
        stream.extend_from_slice(&[IAC, SB, OPT_COM_PORT, SET_BAUDRATE + SERVER_OFFSET]);
        stream.extend_from_slice(&[0, 0, 0xE1, 0x00, IAC, SE]); // 57600

        let whole = Telnet::new().decode(&stream);
        let mut t = Telnet::new();
        let mut piecemeal = Decoded::default();
        for b in &stream {
            let d = t.decode(&[*b]);
            piecemeal.data.extend(d.data);
            piecemeal.reply.extend(d.reply);
            piecemeal.notices.extend(d.notices);
        }
        assert_eq!(whole, piecemeal, "byte-at-a-time must match one big read");
        assert_eq!(whole.data, vec![0x00, 0xFF, 0x41, 0xFF], "data only, unescaped");
        assert_eq!(whole.notices, vec!["peer baudrate = 57600"]);
        assert!(whole.reply.is_empty(), "DO for an option we already offered");
    }

    #[test]
    fn negotiation_refuses_the_unknown_and_never_acks_an_ack() {
        let mut t = Telnet::new();
        // An option we don't implement: refuse in the matching direction.
        assert_eq!(t.decode(&[IAC, WILL, 24]).reply, vec![IAC, DONT, 24]);
        assert_eq!(t.decode(&[IAC, DO, 24]).reply, vec![IAC, WONT, 24]);
        // Confirmations of our own offers draw no reply — that is the loop.
        assert!(t.decode(&[IAC, DO, OPT_BINARY]).reply.is_empty());
        assert!(t.decode(&[IAC, WILL, OPT_BINARY]).reply.is_empty());
        // A refusal is a state change, so it is answered once, and it turns
        // COM-PORT forwarding off.
        assert!(t.com_port_ok());
        assert_eq!(
            t.decode(&[IAC, DONT, OPT_COM_PORT]).reply,
            vec![IAC, WONT, OPT_COM_PORT]
        );
        assert!(!t.com_port_ok(), "a refused option must stop the poller");
    }

    #[test]
    fn subnegotiation_params_are_escaped() {
        // 0x00FF0000 = 16711680: the encoding contains an IAC byte that must
        // be doubled, or the peer sees the subneg end early.
        let cmds = PortSettings {
            baud: 0,
            ..settings(9600)
        }
        .commands_to(&PortSettings {
            baud: 0x00FF_0000,
            ..settings(9600)
        });
        assert_eq!(
            cmds,
            vec![vec![IAC, SB, OPT_COM_PORT, SET_BAUDRATE, 0x00, 0xFF, 0xFF, 0x00, 0x00, IAC, SE]]
        );
    }

    fn settings(baud: u32) -> PortSettings {
        PortSettings { baud, data_bits: 8, parity: 1, stop_bits: 1, flow: 1 }
    }

    #[test]
    fn termios_maps_to_rfc2217_values() {
        let mut tio = raw_termios();
        // A pristine raw pty: 8N1, no flow control, speed unset (= query).
        let base = PortSettings::from_termios(&tio);
        assert_eq!(base, PortSettings::UNSET, "cfmakeraw leaves 8N1 and no speed");
        assert_eq!(base, settings(0));

        unsafe { libc::cfsetospeed(&mut tio, libc::B9600) };
        unsafe { libc::cfsetispeed(&mut tio, libc::B9600) };
        tio.c_cflag = (tio.c_cflag & !libc::CSIZE) | libc::CS7 | libc::PARENB | libc::PARODD | libc::CSTOPB;
        tio.c_cflag |= libc::CRTSCTS;
        let s = PortSettings::from_termios(&tio);
        assert_eq!(
            s,
            PortSettings { baud: 9600, data_bits: 7, parity: 2, stop_bits: 2, flow: 3 }
        );
        assert_eq!(s.describe(), "9600 7O2 rts/cts");

        // Even parity, software flow control.
        tio.c_cflag &= !(libc::PARODD | libc::CRTSCTS);
        tio.c_iflag |= libc::IXON;
        let s = PortSettings::from_termios(&tio);
        assert_eq!(s.parity, 3, "PARENB without PARODD is even");
        assert_eq!(s.flow, 2, "IXON is XON/XOFF");
    }

    #[test]
    fn only_changed_fields_are_sent() {
        let a = settings(9600);
        assert!(a.commands_to(&a).is_empty(), "an idle port says nothing");

        let b = PortSettings { baud: 115200, ..a };
        let cmds = a.commands_to(&b);
        assert_eq!(cmds.len(), 1, "only the baud moved");
        assert_eq!(
            cmds[0],
            vec![IAC, SB, OPT_COM_PORT, SET_BAUDRATE, 0, 1, 0xC2, 0x00, IAC, SE],
            "115200 big-endian"
        );

        let c = PortSettings { data_bits: 7, parity: 3, stop_bits: 2, flow: 2, ..b };
        assert_eq!(b.commands_to(&c).len(), 4, "four fields moved");
    }

    /// Every constant this module can emit must survive its own decoder as a
    /// subnegotiation — a cheap guard against a mistyped command number.
    #[test]
    fn emitted_commands_round_trip_through_the_decoder() {
        let cmds = settings(0).commands_to(&PortSettings {
            baud: 921_600,
            data_bits: 7,
            parity: 2,
            stop_bits: 2,
            flow: 3,
        });
        let mut t = Telnet::new();
        for c in &cmds {
            let d = t.decode(c);
            assert!(d.data.is_empty(), "a subneg must not leak into the data stream");
            assert!(d.reply.is_empty());
        }
    }
}
