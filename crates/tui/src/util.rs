pub(crate) fn count_chars(s: &str) -> usize {
    s.chars().count()
}

pub(crate) fn take_start(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub(crate) fn take_end(s: &str, n: usize) -> String {
    let len = count_chars(s);
    s.chars().skip(len.saturating_sub(n)).collect()
}

pub(crate) fn truncate_end(s: &str, width: usize) -> String {
    let len = count_chars(s);
    if len <= width {
        return s.to_string();
    }
    if width <= 3 {
        return take_start(s, width);
    }
    format!("{}...", take_start(s, width - 3))
}

/// Write `text` to the terminal's system clipboard via the OSC 52 escape
/// sequence. Works over SSH on any terminal that supports it (iTerm2,
/// WezTerm, kitty, Alacritty, Ghostty, tmux with set-clipboard on, etc.).
pub(crate) fn osc52_copy(text: &str) {
    use std::io::Write;
    // Skip pathologically large yanks. Most terminals cap OSC 52 around
    // 8-100KB; blasting a huge sequence can jam the terminal.
    if text.len() > 100_000 {
        return;
    }
    let b64 = base64_encode(text.as_bytes());
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{b64}\x07");
    let _ = out.flush();
}

/// Minimal RFC 4648 base64 encoder. Used for OSC 52 clipboard writes.
pub(crate) fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for chunk in chunks.by_ref() {
        let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        0 => {}
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push_str("==");
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => unreachable!(),
    }
    out
}

/// Pad a string out to `width` columns (assuming 1 char = 1 col). Truncates
/// with `…` when the string would overflow.
pub(crate) fn pad_or_trunc(s: &str, width: usize) -> String {
    let n = count_chars(s);
    if n == width {
        s.to_string()
    } else if n < width {
        format!("{}{}", s, " ".repeat(width - n))
    } else if width <= 1 {
        take_start(s, width)
    } else {
        format!("{}…", take_start(s, width - 1))
    }
}

/// Map a byte offset within a UTF-8 string to its char index. Clamps to
/// `s.chars().count()` if the byte is at or past the end.
pub(crate) fn char_index_in_byte_range(s: &str, byte_offset: usize) -> usize {
    if byte_offset >= s.len() {
        return s.chars().count();
    }
    s[..byte_offset].chars().count()
}
