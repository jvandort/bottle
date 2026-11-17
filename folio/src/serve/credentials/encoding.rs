//! Small encodings: base64, and globs as fnmatch has them.

/// Whether `name` matches `pattern`, a shell's glob: `*` anything, `?` one character.
pub(super) fn fnmatch(name: &str, pattern: &str) -> bool {
    let (n, p): (Vec<char>, Vec<char>) = (name.chars().collect(), pattern.chars().collect());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while i < n.len() {
        if j < p.len() && (p[j] == '?' || p[j] == n[i]) {
            i += 1;
            j += 1;
        } else if j < p.len() && p[j] == '*' {
            star = Some(j);
            mark = i;
            j += 1;
        } else if let Some(s) = star {
            j = s + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    while j < p.len() && p[j] == '*' {
        j += 1;
    }
    j == p.len()
}

pub(super) const B64: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64, padded or not.
pub(super) fn base64(bytes: &[u8], padded: bool) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else if padded {
                out.push('=');
            }
        }
    }
    out
}

/// Standard base64, strictly: None if it isn't.
pub(super) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    let mut out = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0);
    for c in text.bytes() {
        let v = B64.iter().position(|&b| b == c)? as u32;
        buffer = (buffer << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}
