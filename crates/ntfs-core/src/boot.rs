use crate::error::{Error, Result};
use crate::{BITLOCKER_OEM, NTFS_OEM};

/// NTFS boot sector (first 512+ bytes of the volume).
#[derive(Debug, Clone)]
pub struct BootSector {
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub mft_lcn: u64,
    pub mft_mirr_lcn: u64,
    pub clusters_per_mft_record: i8,
    pub clusters_per_index_block: i8,
    pub serial: u64,
    pub total_sectors: u64,
    pub oem: [u8; 8],
}

impl BootSector {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 0x54 {
            return Err(Error::Format("boot sector too short"));
        }
        let mut oem = [0u8; 8];
        oem.copy_from_slice(&buf[3..11]);
        if BITLOCKER_OEM.iter().any(|id| *id == &oem) {
            return Err(Error::Safety("BitLocker-encrypted volume".into()));
        }
        if &oem != NTFS_OEM {
            return Err(Error::Format("not an NTFS boot sector"));
        }
        let bytes_per_sector = u16::from_le_bytes(buf[0x0B..0x0D].try_into().unwrap());
        if bytes_per_sector == 0 || !bytes_per_sector.is_power_of_two() {
            return Err(Error::Corrupt("invalid bytes_per_sector".into()));
        }
        let sectors_per_cluster = buf[0x0D];
        if sectors_per_cluster == 0 {
            return Err(Error::Corrupt("invalid sectors_per_cluster".into()));
        }
        let total_sectors = u64::from_le_bytes(buf[0x28..0x30].try_into().unwrap());
        let mft_lcn = u64::from_le_bytes(buf[0x30..0x38].try_into().unwrap());
        let mft_mirr_lcn = u64::from_le_bytes(buf[0x38..0x40].try_into().unwrap());
        let clusters_per_mft_record = buf[0x40] as i8;
        let clusters_per_index_block = buf[0x44] as i8;
        let serial = u64::from_le_bytes(buf[0x48..0x50].try_into().unwrap());
        Ok(Self {
            bytes_per_sector,
            sectors_per_cluster,
            mft_lcn,
            mft_mirr_lcn,
            clusters_per_mft_record,
            clusters_per_index_block,
            serial,
            total_sectors,
            oem,
        })
    }

    pub fn cluster_size(&self) -> u32 {
        self.bytes_per_sector as u32 * self.sectors_per_cluster as u32
    }

    pub fn volume_size(&self) -> u64 {
        self.total_sectors.saturating_mul(self.bytes_per_sector as u64)
    }

    /// MFT record size in bytes. Negative `clusters_per_mft_record` means
    /// 2^abs(value) bytes (the usual 1024-byte record is encoded as -10).
    pub fn mft_record_size(&self) -> u32 {
        signed_cluster_count_to_bytes(self.clusters_per_mft_record, self.cluster_size())
    }

    pub fn index_block_size(&self) -> u32 {
        signed_cluster_count_to_bytes(self.clusters_per_index_block, self.cluster_size())
    }

    pub fn cluster_count(&self) -> u64 {
        let cs = self.cluster_size() as u64;
        if cs == 0 {
            0
        } else {
            self.volume_size() / cs
        }
    }
}

fn signed_cluster_count_to_bytes(value: i8, cluster_size: u32) -> u32 {
    if value < 0 {
        1u32 << ((-value) as u32)
    } else {
        cluster_size.saturating_mul(value as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_boot() -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[0] = 0xEB;
        b[1] = 0x52;
        b[2] = 0x90;
        b[3..11].copy_from_slice(b"NTFS    ");
        b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        b[0x0D] = 8; // 4 KiB clusters
        b[0x28..0x30].copy_from_slice(&0x0001_0000u64.to_le_bytes());
        b[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
        b[0x38..0x40].copy_from_slice(&0x8000u64.to_le_bytes());
        b[0x40] = (-10i8) as u8; // 1024-byte MFT records
        b[0x44] = 1; // 1 cluster index blocks
        b[0x48..0x50].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        b
    }

    #[test]
    fn parses_boot() {
        let boot = BootSector::parse(&sample_boot()).unwrap();
        assert_eq!(boot.cluster_size(), 4096);
        assert_eq!(boot.mft_record_size(), 1024);
        assert_eq!(boot.mft_lcn, 4);
        assert_eq!(boot.serial, 0x1122_3344_5566_7788);
    }

    #[test]
    fn rejects_bitlocker() {
        let mut b = sample_boot();
        b[3..11].copy_from_slice(b"-FVE-FS-");
        match BootSector::parse(&b) {
            Err(Error::Safety(_)) => {}
            // covered above
            other => panic!("expected Safety, got {other:?}"),
        }
    }

    #[test]
    fn rejects_fat() {
        let mut b = sample_boot();
        b[3..11].copy_from_slice(b"MSDOS5.0");
        assert!(BootSector::parse(&b).is_err());
    }
}
