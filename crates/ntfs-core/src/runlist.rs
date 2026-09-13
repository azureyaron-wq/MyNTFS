use crate::error::{Error, Result};

/// One NTFS mapping-pair (data run).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataRun {
    /// Length in clusters. Never zero.
    pub cluster_count: u64,
    /// Starting logical cluster, or `None` for a sparse hole.
    pub lcn: Option<u64>,
}

/// Decode a mapping-pairs blob. Stops at the first 0x00 header or end of
/// `bytes`. LCN deltas are signed; a negative absolute LCN is corrupt.
pub fn decode_runs(bytes: &[u8]) -> Result<Vec<DataRun>> {
    let mut runs = Vec::new();
    let mut i = 0;
    let mut abs_lcn: i64 = 0;
    while i < bytes.len() {
        let header = bytes[i];
        if header == 0 {
            break;
        }
        i += 1;
        let length_bytes = (header & 0x0F) as usize;
        let lcn_bytes = (header >> 4) as usize;
        if length_bytes == 0 || length_bytes > 8 || lcn_bytes > 8 {
            return Err(Error::Corrupt(format!(
                "bad run header 0x{header:02x}"
            )));
        }
        if i + length_bytes + lcn_bytes > bytes.len() {
            return Err(Error::Corrupt("truncated mapping pairs".into()));
        }
        let cluster_count = read_le_uint(&bytes[i..i + length_bytes]);
        i += length_bytes;
        if cluster_count == 0 {
            return Err(Error::Corrupt("zero-length run".into()));
        }
        let lcn = if lcn_bytes == 0 {
            None
        } else {
            let delta = read_le_sint(&bytes[i..i + lcn_bytes]);
            i += lcn_bytes;
            abs_lcn = abs_lcn.checked_add(delta).ok_or_else(|| {
                Error::Corrupt("LCN delta overflow".into())
            })?;
            if abs_lcn < 0 {
                return Err(Error::Corrupt(format!("negative LCN {abs_lcn}")));
            }
            Some(abs_lcn as u64)
        };
        runs.push(DataRun {
            cluster_count,
            lcn,
        });
    }
    Ok(runs)
}

/// Inverse of [`decode_runs`]. Appends a 0x00 terminator.
pub fn encode_runs(runs: &[DataRun]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut abs_lcn: i64 = 0;
    for run in runs {
        if run.cluster_count == 0 {
            return Err(Error::Format("zero-length run"));
        }
        let length_bytes = uint_size(run.cluster_count);
        let (lcn_bytes, delta) = match run.lcn {
            None => (0usize, 0i64),
            Some(lcn) => {
                let delta = lcn as i64 - abs_lcn;
                abs_lcn = lcn as i64;
                (sint_size(delta), delta)
            }
        };
        out.push(((lcn_bytes as u8) << 4) | (length_bytes as u8));
        write_le_uint(&mut out, run.cluster_count, length_bytes);
        if lcn_bytes > 0 {
            write_le_sint(&mut out, delta, lcn_bytes);
        }
    }
    out.push(0);
    Ok(out)
}

/// Resolve a virtual cluster number to an LCN. `None` means sparse or past end.
pub fn vcn_to_lcn(runs: &[DataRun], vcn: u64) -> Option<u64> {
    let mut cur = 0u64;
    for run in runs {
        let next = cur.saturating_add(run.cluster_count);
        if vcn < next {
            return run.lcn.map(|lcn| lcn + (vcn - cur));
        }
        cur = next;
    }
    None
}

fn read_le_uint(b: &[u8]) -> u64 {
    let mut v = 0u64;
    for (i, byte) in b.iter().enumerate() {
        v |= (*byte as u64) << (8 * i);
    }
    v
}

fn read_le_sint(b: &[u8]) -> i64 {
    let mut v = read_le_uint(b);
    let bits = b.len() * 8;
    if bits < 64 && (v & (1u64 << (bits - 1))) != 0 {
        v |= !0u64 << bits;
    }
    v as i64
}

fn uint_size(v: u64) -> usize {
    let mut n = 1;
    let mut x = v >> 8;
    while x != 0 && n < 8 {
        n += 1;
        x >>= 8;
    }
    n
}

fn sint_size(v: i64) -> usize {
    let mut n = 1;
    while n < 8 {
        let shift = n * 8;
        let truncated = (v << (64 - shift)) >> (64 - shift);
        if truncated == v {
            return n;
        }
        n += 1;
    }
    8
}

fn write_le_uint(out: &mut Vec<u8>, v: u64, n: usize) {
    for i in 0..n {
        out.push(((v >> (8 * i)) & 0xFF) as u8);
    }
}

fn write_le_sint(out: &mut Vec<u8>, v: i64, n: usize) {
    write_le_uint(out, v as u64, n);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_simple() {
        let runs = vec![
            DataRun {
                cluster_count: 4,
                lcn: Some(16),
            },
            DataRun {
                cluster_count: 8,
                lcn: None,
            },
            DataRun {
                cluster_count: 2,
                lcn: Some(100),
            },
        ];
        let bytes = encode_runs(&runs).unwrap();
        let decoded = decode_runs(&bytes).unwrap();
        assert_eq!(decoded, runs);
        assert_eq!(vcn_to_lcn(&decoded, 0), Some(16));
        assert_eq!(vcn_to_lcn(&decoded, 3), Some(19));
        assert_eq!(vcn_to_lcn(&decoded, 4), None); // hole
        assert_eq!(vcn_to_lcn(&decoded, 12), Some(100));
    }

    #[test]
    fn negative_delta() {
        // header 0x21: 1 length byte, 2 lcn bytes; length=8, delta=-4
        // first run lcn=20 (0x14), second delta encoded separately
        let runs = vec![
            DataRun {
                cluster_count: 1,
                lcn: Some(20),
            },
            DataRun {
                cluster_count: 1,
                lcn: Some(16),
            },
        ];
        let bytes = encode_runs(&runs).unwrap();
        assert_eq!(decode_runs(&bytes).unwrap(), runs);
    }

    #[test]
    fn rejects_negative_absolute() {
        // length 1, lcn 1 signed: length=1, delta=-1 → LCN -1
        let bytes = [0x11, 0x01, 0xFF, 0x00];
        assert!(decode_runs(&bytes).is_err());
    }
}
