use crate::error::{Error, Result};

/// NTFS timestamps are 100-nanosecond intervals since 1601-01-01 UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct NtfsTime(pub u64);

const EPOCH_DIFF_SECS: i64 = 11_644_473_600; // 1601 → 1970
const UNITS_PER_SEC: u64 = 10_000_000;

pub fn ntfs_to_unix(t: NtfsTime) -> (i64, u32) {
    let units = t.0;
    let secs = (units / UNITS_PER_SEC) as i64 - EPOCH_DIFF_SECS;
    let nsec = ((units % UNITS_PER_SEC) * 100) as u32;
    (secs, nsec)
}

pub fn unix_to_ntfs(secs: i64, nsec: u32) -> NtfsTime {
    let units = ((secs + EPOCH_DIFF_SECS) as u64)
        .saturating_mul(UNITS_PER_SEC)
        .saturating_add((nsec as u64) / 100);
    NtfsTime(units)
}

pub fn read_time(buf: &[u8]) -> Result<NtfsTime> {
    if buf.len() < 8 {
        return Err(Error::Corrupt("timestamp truncated".into()));
    }
    Ok(NtfsTime(u64::from_le_bytes(buf[..8].try_into().unwrap())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_epoch() {
        let t = unix_to_ntfs(0, 0);
        let (s, n) = ntfs_to_unix(t);
        assert_eq!(s, 0);
        assert_eq!(n, 0);
    }

    #[test]
    fn known_windows_filetime() {
        // 2020-01-01 00:00:00 UTC = 132223104000000000
        let t = NtfsTime(132_223_104_000_000_000);
        let (s, _) = ntfs_to_unix(t);
        assert_eq!(s, 1_577_836_800);
    }
}
