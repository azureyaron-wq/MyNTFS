use crate::error::{Error, Result};

/// NTFS attribute type codes (MS-FSCC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributeType(pub u32);

impl AttributeType {
    pub const STANDARD_INFORMATION: Self = Self(0x10);
    pub const ATTRIBUTE_LIST: Self = Self(0x20);
    pub const FILE_NAME: Self = Self(0x30);
    pub const OBJECT_ID: Self = Self(0x40);
    pub const SECURITY_DESCRIPTOR: Self = Self(0x50);
    pub const VOLUME_NAME: Self = Self(0x60);
    pub const VOLUME_INFORMATION: Self = Self(0x70);
    pub const DATA: Self = Self(0x80);
    pub const INDEX_ROOT: Self = Self(0x90);
    pub const INDEX_ALLOCATION: Self = Self(0xA0);
    pub const BITMAP: Self = Self(0xB0);
    pub const REPARSE_POINT: Self = Self(0xC0);
    pub const EA_INFORMATION: Self = Self(0xD0);
    pub const EA: Self = Self(0xE0);
    pub const LOGGED_UTILITY_STREAM: Self = Self(0x100);
    pub const END: Self = Self(0xFFFF_FFFF);

    pub fn from_u32(v: u32) -> Self {
        Self(v)
    }

    pub fn as_u32(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributeFlags(pub u16);

impl AttributeFlags {
    pub const COMPRESSED: Self = Self(0x0001);
    pub const ENCRYPTED: Self = Self(0x4000);
    pub const SPARSE: Self = Self(0x8000);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

#[derive(Debug, Clone)]
pub struct AttributeHeader {
    pub ty: AttributeType,
    pub length: u32,
    pub non_resident: bool,
    pub name: String,
    pub flags: AttributeFlags,
    pub id: u16,
}

#[derive(Debug, Clone)]
pub enum Attribute {
    Resident {
        header: AttributeHeader,
        value: Vec<u8>,
    },
    NonResident {
        header: AttributeHeader,
        lowest_vcn: u64,
        highest_vcn: u64,
        mapping_pairs: Vec<u8>,
        allocated_size: u64,
        data_size: u64,
        initialized_size: u64,
        compression_unit: u8,
    },
}

impl Attribute {
    pub fn header(&self) -> &AttributeHeader {
        match self {
            Attribute::Resident { header, .. } | Attribute::NonResident { header, .. } => header,
        }
    }

    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 16 {
            return Err(Error::Corrupt("attribute header truncated".into()));
        }
        let ty = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if ty == 0xFFFF_FFFF {
            return Err(Error::Format("end marker"));
        }
        let length = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        if length < 16 || length as usize > buf.len() {
            return Err(Error::Corrupt(format!("bad attribute length {length}")));
        }
        let non_resident = buf[8] != 0;
        let name_len = buf[9] as usize;
        let name_off = u16::from_le_bytes(buf[10..12].try_into().unwrap()) as usize;
        let flags = AttributeFlags(u16::from_le_bytes(buf[12..14].try_into().unwrap()));
        let id = u16::from_le_bytes(buf[14..16].try_into().unwrap());
        let name = if name_len > 0 {
            let start = name_off;
            let end = start.saturating_add(name_len * 2);
            if end > buf.len() {
                return Err(Error::Corrupt("attribute name truncated".into()));
            }
            crate::utf16::utf16le_to_string(&buf[start..end]).unwrap_or_default()
        } else {
            String::new()
        };
        let header = AttributeHeader {
            ty: AttributeType::from_u32(ty),
            length,
            non_resident,
            name,
            flags,
            id,
        };
        if !non_resident {
            if buf.len() < 0x18 {
                return Err(Error::Corrupt("resident attribute truncated".into()));
            }
            let value_len = u32::from_le_bytes(buf[0x10..0x14].try_into().unwrap()) as usize;
            let value_off = u16::from_le_bytes(buf[0x14..0x16].try_into().unwrap()) as usize;
            let end = value_off.saturating_add(value_len);
            if end > length as usize || end > buf.len() {
                return Err(Error::Corrupt("resident value out of range".into()));
            }
            Ok(Attribute::Resident {
                header,
                value: buf[value_off..end].to_vec(),
            })
        } else {
            if buf.len() < 0x40 {
                return Err(Error::Corrupt("non-resident attribute truncated".into()));
            }
            let lowest_vcn = u64::from_le_bytes(buf[0x10..0x18].try_into().unwrap());
            let highest_vcn = u64::from_le_bytes(buf[0x18..0x20].try_into().unwrap());
            let mp_off = u16::from_le_bytes(buf[0x20..0x22].try_into().unwrap()) as usize;
            let compression_unit = buf[0x22];
            let allocated_size = u64::from_le_bytes(buf[0x28..0x30].try_into().unwrap());
            let data_size = u64::from_le_bytes(buf[0x30..0x38].try_into().unwrap());
            let initialized_size = u64::from_le_bytes(buf[0x38..0x40].try_into().unwrap());
            if mp_off >= length as usize {
                return Err(Error::Corrupt("mapping pairs offset out of range".into()));
            }
            let mapping_pairs = buf[mp_off..length as usize].to_vec();
            Ok(Attribute::NonResident {
                header,
                lowest_vcn,
                highest_vcn,
                mapping_pairs,
                allocated_size,
                data_size,
                initialized_size,
                compression_unit,
            })
        }
    }
}

/// Iterate attributes inside a post-fixup MFT record.
pub fn iter_attributes(record: &[u8], first_offset: usize) -> Result<Vec<Attribute>> {
    let mut attrs = Vec::new();
    let mut off = first_offset;
    let used = if record.len() >= 0x1C {
        u32::from_le_bytes(record[0x18..0x1C].try_into().unwrap()) as usize
    } else {
        record.len()
    };
    let limit = used.min(record.len());
    while off + 8 <= limit {
        let ty = u32::from_le_bytes(record[off..off + 4].try_into().unwrap());
        if ty == 0xFFFF_FFFF {
            break;
        }
        let length = u32::from_le_bytes(record[off + 4..off + 8].try_into().unwrap()) as usize;
        if length < 16 || off + length > record.len() {
            return Err(Error::Corrupt(format!(
                "attribute at {off} has bad length {length}"
            )));
        }
        attrs.push(Attribute::parse(&record[off..off + length])?);
        off += length;
        // Attributes are 8-byte aligned; length already includes padding.
    }
    Ok(attrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_resident_data() {
        // Minimal resident $DATA: type=0x80, length=0x20, value "hi"
        let mut buf = vec![0u8; 0x20];
        buf[0..4].copy_from_slice(&0x80u32.to_le_bytes());
        buf[4..8].copy_from_slice(&0x20u32.to_le_bytes());
        buf[8] = 0;
        buf[0x10..0x14].copy_from_slice(&2u32.to_le_bytes());
        buf[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        buf[0x18] = b'h';
        buf[0x19] = b'i';
        match Attribute::parse(&buf).unwrap() {
            Attribute::Resident { value, header } => {
                assert_eq!(value, b"hi");
                assert_eq!(header.ty, AttributeType::DATA);
            }
            _ => panic!("expected resident"),
        }
    }
}
