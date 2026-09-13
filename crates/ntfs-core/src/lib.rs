//! Clean-room NTFS on-disk parsers, a `BlockDevice` trait, and mount-time
//! safety probes. This crate performs no syscalls of its own.
//!
//! Layout references: Microsoft MS-FSCC, Windows Internals (NTFS on-disk
//! structure). No GPL NTFS source was consulted.

mod attribute;
mod boot;
mod device;
mod error;
mod index;
mod lznt1;
mod mft;
mod runlist;
mod safety;
mod time;
mod utf16;
mod volume;

pub use attribute::{Attribute, AttributeFlags, AttributeHeader, AttributeType};
pub use boot::BootSector;
pub use device::BlockDevice;
pub use error::{Error, Result};
pub use index::{IndexEntry, IndexNodeHeader, IndexRoot};
pub use lznt1::decompress_lznt1;
pub use mft::{MftFlags, MftRecord, MftRecordHeader};
pub use runlist::{DataRun, decode_runs, encode_runs, vcn_to_lcn};
pub use safety::{SafetyReport, VolumeProbe, WritePolicy};
pub use time::{NtfsTime, unix_to_ntfs, ntfs_to_unix};
pub use utf16::{utf16le_to_string, string_to_utf16le};
pub use volume::{Extent, FileData, NtfsVolume, VolumeInfo};

/// NTFS OEM identifier stored at boot-sector offset 3.
pub const NTFS_OEM: &[u8; 8] = b"NTFS    ";
/// BitLocker OEM identifiers that replace "NTFS    " on encrypted volumes.
pub const BITLOCKER_OEM: &[&[u8; 8]] = &[b"-FVE-FS-", b"FVE-FS  "];
/// FILE record magic.
pub const FILE_MAGIC: &[u8; 4] = b"FILE";
/// INDX block magic.
pub const INDX_MAGIC: &[u8; 4] = b"INDX";
/// End-of-attributes marker.
pub const ATTR_END: u32 = 0xFFFF_FFFF;
