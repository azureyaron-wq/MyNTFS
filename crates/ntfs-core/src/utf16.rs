use crate::error::{Error, Result};

/// Decode a little-endian UTF-16 slice into a Rust `String`.
pub fn utf16le_to_string(bytes: &[u8]) -> Result<String> {
    if bytes.len() % 2 != 0 {
        return Err(Error::Corrupt("odd UTF-16 length".into()));
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|u| *u != 0)
        .collect();
    String::from_utf16(&units).map_err(|_| Error::Corrupt("invalid UTF-16".into()))
}

pub fn string_to_utf16le(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 2);
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let s = "Hello 世界";
        let b = string_to_utf16le(s);
        assert_eq!(utf16le_to_string(&b).unwrap(), s);
    }

    #[test]
    fn stops_at_nul() {
        let mut b = string_to_utf16le("ab");
        b.extend_from_slice(&[0, 0, b'c' as u8, 0]);
        assert_eq!(utf16le_to_string(&b).unwrap(), "ab");
    }
}
