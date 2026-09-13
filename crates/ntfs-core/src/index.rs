use crate::error::{Error, Result};
use crate::mft::apply_usa_fixup;
use crate::utf16::utf16le_to_string;
use crate::INDX_MAGIC;

/// Header of an index node (`$INDEX_ROOT` body after the type/collation prefix,
/// or an `INDX` block after the 0x18-byte IND header + USA).
#[derive(Debug, Clone)]
pub struct IndexNodeHeader {
    pub entries_offset: u32,
    pub used_size: u32,
    pub allocated_size: u32,
    pub has_children: bool,
}

impl IndexNodeHeader {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 16 {
            return Err(Error::Corrupt("index node header truncated".into()));
        }
        Ok(Self {
            entries_offset: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            used_size: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            allocated_size: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            has_children: u32::from_le_bytes(buf[12..16].try_into().unwrap()) & 1 != 0,
        })
    }
}

#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub file_reference: u64,
    pub flags: u16,
    pub name: Option<String>,
    pub is_last: bool,
    pub has_subnode: bool,
    pub subnode_vcn: Option<u64>,
    pub is_directory: bool,
}

impl IndexEntry {
    pub const LAST: u16 = 0x0002;
    pub const NODE: u16 = 0x0001;

    pub fn parse(buf: &[u8]) -> Result<(Self, usize)> {
        if buf.len() < 16 {
            return Err(Error::Corrupt("index entry truncated".into()));
        }
        let file_reference = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let entry_size = u16::from_le_bytes(buf[8..10].try_into().unwrap()) as usize;
        let key_size = u16::from_le_bytes(buf[10..12].try_into().unwrap()) as usize;
        let flags = u16::from_le_bytes(buf[12..14].try_into().unwrap());
        if entry_size < 16 || entry_size > buf.len() {
            return Err(Error::Corrupt(format!("bad index entry size {entry_size}")));
        }
        let is_last = flags & Self::LAST != 0;
        let has_subnode = flags & Self::NODE != 0;
        let mut name = None;
        let mut is_directory = false;
        if !is_last && key_size >= 0x42 {
            let key = &buf[0x10..0x10 + key_size.min(buf.len().saturating_sub(0x10))];
            // FILE_NAME: flags at 0x38, name_len at 0x40, namespace 0x41, name at 0x42
            if key.len() >= 0x42 {
                let fn_flags = u32::from_le_bytes(key[0x38..0x3C].try_into().unwrap());
                is_directory = fn_flags & 0x1000_0000 != 0 || fn_flags & 0x2000_0000 != 0;
                let name_units = key[0x40] as usize;
                let name_bytes = name_units.saturating_mul(2);
                if 0x42 + name_bytes <= key.len() {
                    name = utf16le_to_string(&key[0x42..0x42 + name_bytes]).ok();
                }
            }
        }
        let subnode_vcn = if has_subnode && entry_size >= 8 {
            let off = entry_size - 8;
            Some(u64::from_le_bytes(buf[off..off + 8].try_into().unwrap()))
        } else {
            None
        };
        Ok((
            Self {
                file_reference,
                flags,
                name,
                is_last,
                has_subnode,
                subnode_vcn,
                is_directory,
            },
            entry_size,
        ))
    }
}

/// `$INDEX_ROOT` attribute value.
#[derive(Debug, Clone)]
pub struct IndexRoot {
    pub attr_type: u32,
    pub collation: u32,
    pub index_block_size: u32,
    pub clusters_per_block: u8,
    pub header: IndexNodeHeader,
    pub entries: Vec<IndexEntry>,
}

impl IndexRoot {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 0x20 {
            return Err(Error::Corrupt("INDEX_ROOT truncated".into()));
        }
        let attr_type = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let collation = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let index_block_size = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        let clusters_per_block = buf[12];
        let header = IndexNodeHeader::parse(&buf[0x10..])?;
        let entries_start = 0x10 + header.entries_offset as usize;
        let entries_end = 0x10 + header.used_size as usize;
        let entries = parse_entry_list(buf, entries_start, entries_end.min(buf.len()))?;
        Ok(Self {
            attr_type,
            collation,
            index_block_size,
            clusters_per_block,
            header,
            entries,
        })
    }
}

/// Parse an INDX allocation block (already the size of one index block).
pub fn parse_indx_block(bytes: &[u8]) -> Result<Vec<IndexEntry>> {
    if bytes.len() < 0x28 {
        return Err(Error::Corrupt("INDX block truncated".into()));
    }
    if &bytes[0..4] != INDX_MAGIC {
        return Err(Error::Format("not an INDX block"));
    }
    let mut raw = bytes.to_vec();
    apply_usa_fixup(&mut raw)?;
    // INDX: header 0x00 magic, 0x04 USA off, 0x06 USA count, 0x08 lsn,
    // 0x10 vcn, 0x18 node header
    let header = IndexNodeHeader::parse(&raw[0x18..])?;
    let start = 0x18 + header.entries_offset as usize;
    let end = 0x18 + header.used_size as usize;
    parse_entry_list(&raw, start, end.min(raw.len()))
}

fn parse_entry_list(buf: &[u8], mut off: usize, end: usize) -> Result<Vec<IndexEntry>> {
    let mut entries = Vec::new();
    while off + 16 <= end {
        let (entry, size) = IndexEntry::parse(&buf[off..end])?;
        let last = entry.is_last;
        off += size.max(16);
        entries.push(entry);
        if last {
            break;
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_root_has_last_entry() {
        // INDEX_ROOT with only the last (empty) entry.
        let mut buf = vec![0u8; 0x30];
        buf[0..4].copy_from_slice(&0x30u32.to_le_bytes()); // FILE_NAME
        buf[4..8].copy_from_slice(&1u32.to_le_bytes()); // collation
        buf[8..12].copy_from_slice(&4096u32.to_le_bytes());
        buf[12] = 1;
        // node header at 0x10: entries at 0x10, used 0x20, alloc 0x20
        buf[0x10..0x14].copy_from_slice(&0x10u32.to_le_bytes());
        buf[0x14..0x18].copy_from_slice(&0x20u32.to_le_bytes());
        buf[0x18..0x1C].copy_from_slice(&0x20u32.to_le_bytes());
        // last entry at 0x20 (0x10+0x10)
        buf[0x20 + 8..0x20 + 10].copy_from_slice(&16u16.to_le_bytes());
        buf[0x20 + 12..0x20 + 14].copy_from_slice(&IndexEntry::LAST.to_le_bytes());
        let root = IndexRoot::parse(&buf).unwrap();
        assert_eq!(root.entries.len(), 1);
        assert!(root.entries[0].is_last);
    }
}
