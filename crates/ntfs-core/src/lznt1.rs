use crate::error::{Error, Result};

const CHUNK: usize = 4096;

/// Decompress an NTFS LZNT1 stream (MS-XCA). Used for compressed `$DATA`.
pub fn decompress_lznt1(input: &[u8]) -> Result<Vec<u8>> {
    let mut src = 0usize;
    let mut out = Vec::new();
    while src + 2 <= input.len() {
        let header = u16::from_le_bytes(input[src..src + 2].try_into().unwrap());
        src += 2;
        if header == 0 {
            break;
        }
        let chunk_len = ((header & 0x0FFF) as usize) + 1;
        let compressed = (header & 0x8000) != 0;
        if src + chunk_len > input.len() {
            return Err(Error::Corrupt("LZNT1 chunk truncated".into()));
        }
        let chunk = &input[src..src + chunk_len];
        src += chunk_len;
        if !compressed {
            out.extend_from_slice(chunk);
            continue;
        }
        decompress_chunk(chunk, &mut out)?;
    }
    Ok(out)
}

fn decompress_chunk(chunk: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let start = out.len();
    let mut i = 0usize;
    while i < chunk.len() {
        if out.len() - start >= CHUNK {
            break;
        }
        if i + 2 > chunk.len() {
            break;
        }
        let flags = u16::from_le_bytes(chunk[i..i + 2].try_into().unwrap());
        i += 2;
        for bit in 0..16 {
            if out.len() - start >= CHUNK {
                return Ok(());
            }
            if i >= chunk.len() {
                return Ok(());
            }
            if flags & (1 << bit) == 0 {
                out.push(chunk[i]);
                i += 1;
            } else {
                if i + 2 > chunk.len() {
                    return Err(Error::Corrupt("LZNT1 backref truncated".into()));
                }
                let info = u16::from_le_bytes(chunk[i..i + 2].try_into().unwrap());
                i += 2;
                let pos = out.len() - start;
                let mut length_bits = 12u32;
                let mut threshold = 0x10usize;
                while threshold <= pos {
                    length_bits = length_bits.saturating_sub(1);
                    threshold <<= 1;
                    if length_bits == 0 {
                        break;
                    }
                }
                if length_bits < 4 {
                    length_bits = 4;
                }
                let length_mask = (1u16 << length_bits) - 1;
                let length = ((info & length_mask) as usize) + 3;
                let offset = ((info >> length_bits) as usize) + 1;
                if offset == 0 || offset > pos {
                    return Err(Error::Corrupt(format!(
                        "LZNT1 bad offset {offset} at pos {pos}"
                    )));
                }
                for _ in 0..length {
                    let b = out[out.len() - offset];
                    out.push(b);
                    if out.len() - start >= CHUNK {
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncompressed_chunk() {
        // header: size-1 = 4, not compressed
        let mut src = Vec::new();
        src.extend_from_slice(&4u16.to_le_bytes()); // length 5
        src.extend_from_slice(b"hello");
        assert_eq!(decompress_lznt1(&src).unwrap(), b"hello");
    }

    #[test]
    fn empty() {
        assert!(decompress_lznt1(&[]).unwrap().is_empty());
    }
}
