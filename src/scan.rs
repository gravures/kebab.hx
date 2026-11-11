//! Incremental scanner that swallows image escape sequences before they reach
//! the terminal emulator.
//!
//! `wezterm-term` divides by zero when it places an image: our `TerminalSize`
//! carries `pixel_width = pixel_height = 0`, so the cell pixel size is 0
//! (`term/src/terminalstate/image.rs`). Kitty graphics (APC `ESC _ …`) and
//! sixel (DCS `ESC P q …`) both reach that code path. Swallowing the
//! sequences keeps the hosted tool alive; a later stage will decode and render
//! them instead.

/// Payload bytes swallowed from one sequence before the scanner gives up.
/// kitty caps payloads at 400 MB; 4 MiB is generous for on-screen art and
/// keeps a corrupt or hostile stream from eating the rest of the output.
const MAX_SWALLOWED: usize = 4 * 1024 * 1024;

/// Longest DCS introducer (`ESC P <params> q`) buffered while deciding
/// whether the sequence is sixel.
const MAX_DCS_PARAMS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Between sequences: every byte is forwarded.
    Ground,
    /// Saw `ESC`, waiting to learn what kind of sequence this is.
    Escape,
    /// Inside a DCS introducer, deciding between sixel and another DCS.
    Dcs,
    /// Inside a kitty graphics APC payload (swallowed).
    Apc,
    /// Inside a sixel DCS payload (swallowed).
    Sixel,
    /// Saw `ESC` inside an APC payload: it may be the string terminator.
    ApcEscape,
    /// Saw `ESC` inside a sixel payload: it may be the string terminator.
    SixelEscape,
}

/// Swallows kitty-graphics and sixel escape sequences from a byte stream.
///
/// The state lives across calls: a PTY can split a sequence at any byte
/// boundary, and a scanner that forgets its state between chunks leaks
/// payload bytes onto the screen as text.
pub struct EscapeScanner {
    state: State,
    swallowed: usize,
    dcs_params: Vec<u8>,
    dcs_intermediate: bool,
}

impl Default for EscapeScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl EscapeScanner {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            swallowed: 0,
            dcs_params: Vec::new(),
            dcs_intermediate: false,
        }
    }

    /// Feed a chunk of bytes, return the bytes to forward to the emulator.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            self.feed_byte(byte, &mut out);
        }
        out
    }

    fn feed_byte(&mut self, byte: u8, out: &mut Vec<u8>) {
        match self.state {
            State::Ground => match byte {
                0x1b => self.state = State::Escape,
                _ => out.push(byte),
            },
            State::Escape => match byte {
                b'_' => {
                    self.state = State::Apc;
                    self.swallowed = 0;
                }
                b'P' => {
                    self.state = State::Dcs;
                    self.dcs_params.clear();
                    self.dcs_intermediate = false;
                }
                0x1b => out.push(0x1b),
                _ => {
                    out.push(0x1b);
                    out.push(byte);
                    self.state = State::Ground;
                }
            },
            State::Dcs => self.feed_dcs(byte, out),
            State::Apc => match byte {
                0x07 => self.state = State::Ground, // BEL terminates the APC
                0x18 | 0x1a => self.state = State::Ground, // CAN / SUB abort it
                0x1b => {
                    self.state = State::ApcEscape;
                    self.swallowed += 1;
                }
                _ => self.swallow(),
            },
            State::Sixel => match byte {
                0x18 | 0x1a => self.state = State::Ground, // CAN / SUB abort it
                0x1b => {
                    self.state = State::SixelEscape;
                    self.swallowed += 1;
                }
                _ => self.swallow(),
            },
            State::ApcEscape => match byte {
                b'\\' => self.state = State::Ground, // ST
                0x1b => {}
                _ => {
                    self.state = State::Apc;
                    self.swallow();
                }
            },
            State::SixelEscape => match byte {
                b'\\' => self.state = State::Ground, // ST
                0x1b => {}
                _ => {
                    self.state = State::Sixel;
                    self.swallow();
                }
            },
        }
    }

    /// Inside a DCS introducer: `ESC P <params> <final byte>`.
    /// Sixel is the DCS whose final byte is `q` and which carries no
    /// intermediate bytes (`ESC P $q`, DECRQSS, is a different DCS).
    fn feed_dcs(&mut self, byte: u8, out: &mut Vec<u8>) {
        if byte == 0x1b {
            // Not sixel: flush what we have and treat the ESC as pending.
            out.push(0x1b);
            out.push(b'P');
            out.append(&mut self.dcs_params);
            self.dcs_intermediate = false;
            self.state = State::Escape;
        } else if (0x20..=0x3f).contains(&byte) {
            if byte <= 0x2f {
                self.dcs_intermediate = true;
            }
            if self.dcs_params.len() >= MAX_DCS_PARAMS {
                self.flush_dcs(byte, out);
            } else {
                self.dcs_params.push(byte);
            }
        } else if byte == b'q' && !self.dcs_intermediate {
            self.state = State::Sixel;
            self.swallowed = 0;
            self.dcs_params.clear();
        } else {
            self.flush_dcs(byte, out);
        }
    }

    /// Forward a DCS that is not sixel, unchanged.
    fn flush_dcs(&mut self, byte: u8, out: &mut Vec<u8>) {
        out.push(0x1b);
        out.push(b'P');
        out.append(&mut self.dcs_params);
        out.push(byte);
        self.dcs_intermediate = false;
        self.state = State::Ground;
    }

    /// Drop one payload byte, watching the cap.
    fn swallow(&mut self) {
        self.swallowed += 1;
        if self.swallowed > MAX_SWALLOWED {
            // ponytail: 4 MiB cap — a payload this large is corrupt or hostile;
            // resync and let the rest of the stream through as text.
            self.state = State::Ground;
            self.swallowed = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESC: u8 = 0x1b;

    fn feed(scanner: &mut EscapeScanner, bytes: &[u8]) -> Vec<u8> {
        scanner.feed(bytes)
    }

    /// ISS-039: a kitty graphics APC must vanish, with no reply and no leak.
    #[test]
    fn swallows_kitty_graphics_apc() {
        let mut scanner = EscapeScanner::new();
        let mut input = vec![ESC, b'_'];
        input.extend_from_slice(b"G a=T,f=24,s=80,v=40;AAAA");
        input.extend_from_slice(&[ESC, b'\\']);

        assert!(feed(&mut scanner, &input).is_empty());
        assert_eq!(feed(&mut scanner, b"after"), b"after".to_vec());
    }

    /// ISS-039: an APC may also end with BEL.
    #[test]
    fn swallows_apc_terminated_by_bel() {
        let mut scanner = EscapeScanner::new();
        let mut input = vec![ESC, b'_'];
        input.extend_from_slice(b"G a=q;");
        input.push(0x07);

        assert!(feed(&mut scanner, &input).is_empty());
        assert_eq!(feed(&mut scanner, b"after"), b"after".to_vec());
    }

    /// ISS-039: sixel reaches the same divide-by-zero, so it is swallowed too.
    #[test]
    fn swallows_sixel_dcs() {
        let mut scanner = EscapeScanner::new();
        let mut input = vec![ESC, b'P', b'q'];
        input.extend_from_slice(b"#0;2;0;0;0");
        input.extend_from_slice(&[ESC, b'\\']);

        assert!(feed(&mut scanner, &input).is_empty());
        assert_eq!(feed(&mut scanner, b"after"), b"after".to_vec());
    }

    /// A PTY can split a sequence anywhere; the scanner must not desync.
    #[test]
    fn swallows_apc_split_across_chunks() {
        let mut scanner = EscapeScanner::new();

        assert!(feed(&mut scanner, &[ESC, b'_', b'G', b' ']).is_empty());
        assert!(feed(&mut scanner, b"a=T,f=24;AAAA").is_empty());
        assert!(feed(&mut scanner, &[ESC]).is_empty());
        assert_eq!(feed(&mut scanner, b"\\after"), b"after".to_vec());
    }

    #[test]
    fn swallows_sixel_split_across_chunks() {
        let mut scanner = EscapeScanner::new();

        assert!(feed(&mut scanner, &[ESC, b'P']).is_empty());
        assert!(feed(&mut scanner, b"q#0;2;0;0;0").is_empty());
        assert!(feed(&mut scanner, &[ESC, b'\\']).is_empty());
        assert_eq!(feed(&mut scanner, b"after"), b"after".to_vec());
    }

    /// An unterminated sequence must not eat the rest of the output forever.
    #[test]
    fn unterminated_apc_resyncs_after_cap() {
        let mut scanner = EscapeScanner::new();

        assert!(feed(&mut scanner, &[ESC, b'_']).is_empty());
        feed(&mut scanner, &vec![b'A'; MAX_SWALLOWED + 1]);

        assert_eq!(feed(&mut scanner, b"text"), b"text".to_vec());
    }

    /// Everything that is not an image sequence must reach the emulator
    /// byte for byte.
    #[test]
    fn forwards_non_image_escapes_unchanged() {
        let mut scanner = EscapeScanner::new();
        let input =
            b"\x1b[2J\x1b[1;31m\x1b]0;title\x07\x1b]8;;http://x\x1b\\\x1b[?25l\x1bP$q\x1b\\text";

        assert_eq!(feed(&mut scanner, input), input.to_vec());
    }

    /// ISS-001: bytes stay bytes, a split multibyte glyph is not mangled.
    #[test]
    fn keeps_multibyte_utf8_across_chunks() {
        let mut scanner = EscapeScanner::new();
        let bytes = "é€".as_bytes();

        let mut out = feed(&mut scanner, &bytes[..1]);
        out.extend_from_slice(&feed(&mut scanner, &bytes[1..]));

        assert_eq!(out, bytes.to_vec());
    }

    /// A trailing ESC is held back until the next chunk tells us what it is.
    #[test]
    fn holds_a_trailing_escape_until_the_next_chunk() {
        let mut scanner = EscapeScanner::new();

        assert_eq!(feed(&mut scanner, b"text\x1b"), b"text".to_vec());
        assert_eq!(feed(&mut scanner, b"[2J"), b"\x1b[2J".to_vec());
    }
}
