//! Decode a raw terminal byte stream into key events.
//!
//! `decode` is pure and incremental: it consumes a prefix of the buffer and
//! reports how many bytes it used, or that it needs more bytes to finish an
//! escape sequence. This keeps the editor's I/O loop thin and lets the decoder
//! be unit-tested without a TTY.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    WordLeft,
    WordRight,
    DeleteWord,
    /// Ctrl-A — start of line.
    LineStart,
    /// Ctrl-E — end of line.
    LineEnd,
    /// Ctrl-K — kill to end of line.
    KillToEnd,
    /// Ctrl-U — kill to start of line.
    KillToStart,
    /// Ctrl-W — kill previous word.
    KillWord,
    /// Ctrl-L — clear screen.
    Clear,
    /// Ctrl-R — reverse history search.
    ReverseSearch,
    /// Ctrl-S — cycle search mode inside the history picker.
    SearchMode,
    /// Alt-1..9 — quick-select a visible history result or completion candidate.
    AltDigit(u8),
    /// Ctrl-C — abort the current line.
    Interrupt,
    /// Ctrl-D — EOF on empty line / delete char otherwise.
    Eof,
    /// Bracketed-paste content (already unwrapped).
    Paste(String),
    /// Escape key alone.
    Escape,
    /// An unrecognized sequence (ignored).
    Unknown,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decoded {
    /// A key plus the number of bytes consumed.
    Key(Key, usize),
    /// The buffer ends mid-sequence; read more bytes.
    Incomplete,
}

/// Decode the next key from the front of `buf`.
pub fn decode(buf: &[u8]) -> Decoded {
    let Some(&b0) = buf.first() else {
        return Decoded::Incomplete;
    };
    match b0 {
        b'\r' | b'\n' => Decoded::Key(Key::Enter, 1),
        b'\t' => Decoded::Key(Key::Tab, 1),
        0x7f | 0x08 => Decoded::Key(Key::Backspace, 1),
        0x01 => Decoded::Key(Key::LineStart, 1),
        0x02 => Decoded::Key(Key::Left, 1),
        0x03 => Decoded::Key(Key::Interrupt, 1),
        0x04 => Decoded::Key(Key::Eof, 1),
        0x05 => Decoded::Key(Key::LineEnd, 1),
        0x06 => Decoded::Key(Key::Right, 1),
        0x0b => Decoded::Key(Key::KillToEnd, 1),
        0x0c => Decoded::Key(Key::Clear, 1),
        0x0e => Decoded::Key(Key::Down, 1), // Ctrl-N
        0x10 => Decoded::Key(Key::Up, 1),   // Ctrl-P
        0x12 => Decoded::Key(Key::ReverseSearch, 1),
        0x13 => Decoded::Key(Key::SearchMode, 1),
        0x15 => Decoded::Key(Key::KillToStart, 1),
        0x17 => Decoded::Key(Key::KillWord, 1),
        0x1b => decode_escape(buf),
        _ => decode_utf8(buf),
    }
}

fn decode_escape(buf: &[u8]) -> Decoded {
    // buf[0] == ESC
    match buf.get(1) {
        None => Decoded::Incomplete,
        Some(b'[') => decode_csi(buf),
        Some(b'O') => decode_ss3(buf),
        Some(b'b') => Decoded::Key(Key::WordLeft, 2),
        Some(b'f') => Decoded::Key(Key::WordRight, 2),
        Some(b'd') => Decoded::Key(Key::DeleteWord, 2),
        Some(0x7f) => Decoded::Key(Key::KillWord, 2),
        Some(d @ b'1'..=b'9') => Decoded::Key(Key::AltDigit(d - b'0'), 2),
        // A lone ESC (no following byte yet would be Incomplete above); treat a
        // standalone ESC followed by a non-sequence byte as Escape consuming 1.
        Some(_) => Decoded::Key(Key::Escape, 1),
    }
}

/// CSI: ESC `[` ... final byte in 0x40..=0x7e.
fn decode_csi(buf: &[u8]) -> Decoded {
    let mut i = 2;
    while i < buf.len() {
        let b = buf[i];
        if (0x40..=0x7e).contains(&b) {
            let params = &buf[2..i];
            let consumed = i + 1;
            return csi_key(params, b, consumed);
        }
        i += 1;
    }
    Decoded::Incomplete
}

fn csi_key(params: &[u8], final_byte: u8, consumed: usize) -> Decoded {
    // Bracketed paste start/end: ESC [ 200~ / 201~
    if final_byte == b'~' {
        return match params {
            b"3" => Decoded::Key(Key::Delete, consumed),
            b"1" | b"7" => Decoded::Key(Key::Home, consumed),
            b"4" | b"8" => Decoded::Key(Key::End, consumed),
            b"200" => Decoded::Key(Key::Unknown, consumed), // handled by editor as paste-begin
            b"201" => Decoded::Key(Key::Unknown, consumed),
            _ => Decoded::Key(Key::Unknown, consumed),
        };
    }
    // Modified keys like ESC[1;5C (Ctrl-Right) carry a ";5"/";3" parameter.
    let modified = params.contains(&b';');
    let key = match final_byte {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => {
            if modified {
                Key::WordRight
            } else {
                Key::Right
            }
        }
        b'D' => {
            if modified {
                Key::WordLeft
            } else {
                Key::Left
            }
        }
        b'H' => Key::Home,
        b'F' => Key::End,
        b'Z' => Key::BackTab, // Shift-Tab
        _ => Key::Unknown,
    };
    Decoded::Key(key, consumed)
}

/// SS3: ESC `O` then a single final byte (application cursor keys).
fn decode_ss3(buf: &[u8]) -> Decoded {
    match buf.get(2) {
        None => Decoded::Incomplete,
        Some(b'C') => Decoded::Key(Key::Right, 3),
        Some(b'D') => Decoded::Key(Key::Left, 3),
        Some(b'A') => Decoded::Key(Key::Up, 3),
        Some(b'B') => Decoded::Key(Key::Down, 3),
        Some(b'H') => Decoded::Key(Key::Home, 3),
        Some(b'F') => Decoded::Key(Key::End, 3),
        Some(_) => Decoded::Key(Key::Unknown, 3),
    }
}

/// Decode a UTF-8 scalar (1–4 bytes), or `Incomplete` if truncated.
fn decode_utf8(buf: &[u8]) -> Decoded {
    let b0 = buf[0];
    let len = if b0 < 0x80 {
        1
    } else if b0 >> 5 == 0b110 {
        2
    } else if b0 >> 4 == 0b1110 {
        3
    } else if b0 >> 3 == 0b11110 {
        4
    } else {
        // Invalid lead byte; skip it.
        return Decoded::Key(Key::Unknown, 1);
    };
    if buf.len() < len {
        return Decoded::Incomplete;
    }
    match std::str::from_utf8(&buf[..len]) {
        Ok(s) => match s.chars().next() {
            Some(c) => Decoded::Key(Key::Char(c), len),
            None => Decoded::Key(Key::Unknown, len),
        },
        Err(_) => Decoded::Key(Key::Unknown, 1),
    }
}

/// The bracketed-paste start/end byte sequences, for the editor's paste handling.
pub const PASTE_START: &[u8] = b"\x1b[200~";
pub const PASTE_END: &[u8] = b"\x1b[201~";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_plain_chars() {
        assert_eq!(decode(b"a"), Decoded::Key(Key::Char('a'), 1));
        assert_eq!(decode("é".as_bytes()), Decoded::Key(Key::Char('é'), 2));
    }

    #[test]
    fn decodes_control_keys() {
        assert_eq!(decode(b"\r"), Decoded::Key(Key::Enter, 1));
        assert_eq!(decode(b"\x7f"), Decoded::Key(Key::Backspace, 1));
        assert_eq!(decode(b"\x01"), Decoded::Key(Key::LineStart, 1));
        assert_eq!(decode(b"\x12"), Decoded::Key(Key::ReverseSearch, 1));
        assert_eq!(decode(b"\x13"), Decoded::Key(Key::SearchMode, 1));
        assert_eq!(decode(b"\x03"), Decoded::Key(Key::Interrupt, 1));
    }

    #[test]
    fn decodes_arrows_and_csi() {
        assert_eq!(decode(b"\x1b[C"), Decoded::Key(Key::Right, 3));
        assert_eq!(decode(b"\x1b[D"), Decoded::Key(Key::Left, 3));
        assert_eq!(decode(b"\x1b[3~"), Decoded::Key(Key::Delete, 4));
        assert_eq!(decode(b"\x1b[1;5C"), Decoded::Key(Key::WordRight, 6));
        assert_eq!(decode(b"\x1bOH"), Decoded::Key(Key::Home, 3));
        assert_eq!(decode(b"\x1b3"), Decoded::Key(Key::AltDigit(3), 2));
    }

    #[test]
    fn reports_incomplete_sequences() {
        assert_eq!(decode(b"\x1b["), Decoded::Incomplete);
        assert_eq!(decode(b"\x1b"), Decoded::Incomplete);
        // truncated 2-byte utf8
        assert_eq!(decode(&[0xc3]), Decoded::Incomplete);
    }
}
