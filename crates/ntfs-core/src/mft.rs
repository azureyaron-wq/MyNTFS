use crate::attribute::{iter_attributes, Attribute};
use crate::error::{Error, Result};
use crate::FILE_MAGIC;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MftFlags(pub u16);

impl MftFlags {
    pub const IN_USE: Self = Self(0x0001);
    pub const DIRECTORY: Self = Self(0x0002);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

#[derive(Debug, Clone)]
pub struct MftRecordHeader {
    pub usa_offset: u16,
    pub usa_count: u16,
    pub lsn: u64,
    pub sequence: u16,
    pub link_count: u16,
    pub first_attr_offset: u16,
    pub flags: MftFlags,
    pub used_size: u32,
    pub allocated_size: u32,
    pub base_record: u64,
    pub next_attr_id: u16,
    pub record_number: u32,
}

#[derive(Debug, Clone)]
pub struct MftRecord {
    pub header: MftRecordHeader,
    pub raw: Vec<u8>,
}

impl MftRecord {
    /// Parse an MFT FILE record, applying the update-sequence array fixup.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 0x30 {
            return Err(Error::Format("MFT record too short"));
        }
        if &bytes[0..4] != FILE_MAGIC {
            if &bytes[0..4] == b"BAAD" {
                return Err(Error::Corrupt("BAAD MFT record".into()));
            }
            return Err(Error::Format("not a FILE record"));
        }
        let mut raw = bytes.to_vec();
        apply_usa_fixup(&mut raw)?;
        let header = parse_header(&raw)?;
        if header.used_size as usize > raw.len() || header.used_size < 0x30 {
            return Err(Error::Corrupt("MFT used_size out of range".into()));
        }
        Ok(Self { header, raw })
    }

    pub fn attributes(&self) -> Result<Vec<Attribute>> {
        iter_attributes(&self.raw, self.header.first_attr_offset as usize)
    }

    pub fn in_use(&self) -> bool {
        self.header.flags.contains(MftFlags::IN_USE)
    }

    pub fn is_directory(&self) -> bool {
        self.header.flags.contains(MftFlags::DIRECTORY)
    }
}

fn parse_header(raw: &[u8]) -> Result<MftRecordHeader> {
    Ok(MftRecordHeader {
        usa_offset: u16::from_le_bytes(raw[0x04..0x06].try_into().unwrap()),
        usa_count: u16::from_le_bytes(raw[0x06..0x08].try_into().unwrap()),
        lsn: u64::from_le_bytes(raw[0x08..0x10].try_into().unwrap()),
        sequence: u16::from_le_bytes(raw[0x10..0x12].try_into().unwrap()),
        link_count: u16::from_le_bytes(raw[0x12..0x14].try_into().unwrap()),
        first_attr_offset: u16::from_le_bytes(raw[0x14..0x16].try_into().unwrap()),
        flags: MftFlags(u16::from_le_bytes(raw[0x16..0x18].try_into().unwrap())),
        used_size: u32::from_le_bytes(raw[0x18..0x1C].try_into().unwrap()),
        allocated_size: u32::from_le_bytes(raw[0x1C..0x20].try_into().unwrap()),
        base_record: u64::from_le_bytes(raw[0x20..0x28].try_into().unwrap()),
        next_attr_id: u16::from_le_bytes(raw[0x28..0x2A].try_into().unwrap()),
        record_number: if raw.len() >= 0x30 {
            u32::from_le_bytes(raw[0x2C..0x30].try_into().unwrap())
        } else {
            0
        },
    })
}

/// Apply the NTFS update-sequence array. Each sector's last two bytes are
/// replaced with the saved original from the USA; the on-disk copies must
/// match the USN (first USA entry).
pub fn apply_usa_fixup(record: &mut [u8]) -> Result<()> {
    if record.len() < 8 {
        return Err(Error::Format("record too short for USA"));
    }
    let usa_off = u16::from_le_bytes(record[0x04..0x06].try_into().unwrap()) as usize;
    let usa_count = u16::from_le_bytes(record[0x06..0x08].try_into().unwrap()) as usize;
    if usa_count == 0 {
        return Ok(());
    }
    let usa_bytes = usa_count * 2;
    if usa_off + usa_bytes > record.len() {
        return Err(Error::Corrupt("USA out of range".into()));
    }
    let sector_size = 512usize;
    let expected_sectors = record.len() / sector_size;
    // usa_count = 1 (USN) + number of sectors.
    if usa_count.saturating_sub(1) != expected_sectors && expected_sectors > 0 {
        // Still apply what we can; some 1024-byte records have usa_count=3
        // (USN + 2 sectors) which is the usual case.
    }
    let usn = [record[usa_off], record[usa_off + 1]];
    for s in 1..usa_count {
        let sector_end = s * sector_size;
        if sector_end > record.len() {
            break;
        }
        let pos = sector_end - 2;
        if record[pos..pos + 2] != usn {
            return Err(Error::Corrupt(format!(
                "USA mismatch at sector {s}: got {:02x}{:02x} expected {:02x}{:02x}",
                record[pos], record[pos + 1], usn[0], usn[1]
            )));
        }
        let src = usa_off + s * 2;
        record[pos] = record[src];
        record[pos + 1] = record[src + 1];
    }
    Ok(())
}

/// Inverse of [`apply_usa_fixup`] for writers: stamp the USN onto each
/// sector end and save the original bytes into the USA.
pub fn stamp_usa(record: &mut [u8], usn: u16) -> Result<()> {
    if record.len() < 8 {
        return Err(Error::Format("record too short for USA"));
    }
    let usa_off = u16::from_le_bytes(record[0x04..0x06].try_into().unwrap()) as usize;
    let usa_count = u16::from_le_bytes(record[0x06..0x08].try_into().unwrap()) as usize;
    if usa_count == 0 {
        return Ok(());
    }
    if usa_off + usa_count * 2 > record.len() {
        return Err(Error::Corrupt("USA out of range".into()));
    }
    let usn_bytes = usn.to_le_bytes();
    record[usa_off] = usn_bytes[0];
    record[usa_off + 1] = usn_bytes[1];
    let sector_size = 512usize;
    for s in 1..usa_count {
        let sector_end = s * sector_size;
        if sector_end > record.len() {
            break;
        }
        let pos = sector_end - 2;
        let src = usa_off + s * 2;
        record[src] = record[pos];
        record[src + 1] = record[pos + 1];
        record[pos] = usn_bytes[0];
        record[pos + 1] = usn_bytes[1];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record() -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[0..4].copy_from_slice(b"FILE");
        r[0x04..0x06].copy_from_slice(&0x30u16.to_le_bytes()); // USA at 0x30
        r[0x06..0x08].copy_from_slice(&3u16.to_le_bytes()); // USN + 2 sectors
        r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        r[0x16..0x18].copy_from_slice(&1u16.to_le_bytes()); // in use
        r[0x18..0x1C].copy_from_slice(&0x40u32.to_le_bytes());
        r[0x1C..0x20].copy_from_slice(&1024u32.to_le_bytes());
        // USA: USN=0x0001, orig last-two of sector0 = AB CD, sector1 = EF 00
        r[0x30] = 0x01;
        r[0x31] = 0x00;
        r[0x32] = 0xAB;
        r[0x33] = 0xCD;
        r[0x34] = 0xEF;
        r[0x35] = 0x00;
        // Place USN at end of each 512-byte sector
        r[510] = 0x01;
        r[511] = 0x00;
        r[1022] = 0x01;
        r[1023] = 0x00;
        r
    }

    #[test]
    fn usa_fixup_restores_bytes() {
        let rec = MftRecord::parse(&make_record()).unwrap();
        assert_eq!(rec.raw[510], 0xAB);
        assert_eq!(rec.raw[511], 0xCD);
        assert_eq!(rec.raw[1022], 0xEF);
        assert_eq!(rec.raw[1023], 0x00);
        assert!(rec.in_use());
    }

    #[test]
    fn usa_mismatch_is_corrupt() {
        let mut r = make_record();
        r[510] = 0xFF;
        assert!(MftRecord::parse(&r).is_err());
    }
}
