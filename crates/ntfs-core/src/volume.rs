use crate::attribute::{Attribute, AttributeType};
use crate::boot::BootSector;
use crate::device::BlockDevice;
use crate::error::{Error, Result};
use crate::index::IndexRoot;
use crate::lznt1::decompress_lznt1;
use crate::mft::MftRecord;
use crate::runlist::{decode_runs, vcn_to_lcn, DataRun};

#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub serial: u64,
    pub cluster_size: u32,
    pub mft_record_size: u32,
    pub index_block_size: u32,
    pub cluster_count: u64,
    pub mft_lcn: u64,
}

/// A physical extent of a non-resident file, used by FSKit KOIO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub logical_offset: u64,
    pub physical_offset: u64,
    pub length: u64,
    pub sparse: bool,
}

#[derive(Debug, Clone)]
pub struct FileData {
    pub size: u64,
    pub resident: Option<Vec<u8>>,
    pub runs: Vec<DataRun>,
    pub compressed: bool,
    pub encrypted: bool,
    pub sparse: bool,
}

/// A volume opened on a `BlockDevice` using only this crate's parsers.
pub struct NtfsVolume {
    pub boot: BootSector,
    mft_runs: Vec<DataRun>,
}

impl NtfsVolume {
    pub fn open(dev: &dyn BlockDevice) -> Result<Self> {
        let mut buf = vec![0u8; 4096];
        let n = 512.min(buf.len());
        dev.read_at(0, &mut buf[..n])?;
        let boot = BootSector::parse(&buf[..n])?;
        Self::from_boot(dev, boot)
    }

    pub fn from_boot(dev: &dyn BlockDevice, boot: BootSector) -> Result<Self> {
        let rec_size = boot.mft_record_size() as u64;
        let cluster = boot.cluster_size() as u64;
        let mft_off = boot.mft_lcn.saturating_mul(cluster);
        let mut rec_buf = vec![0u8; rec_size as usize];
        dev.read_at(mft_off, &mut rec_buf)?;
        let rec = MftRecord::parse(&rec_buf)?;
        let mft_runs = data_runs_of(&rec, "")?;
        Ok(Self { boot, mft_runs })
    }

    pub fn info(&self) -> VolumeInfo {
        VolumeInfo {
            serial: self.boot.serial,
            cluster_size: self.boot.cluster_size(),
            mft_record_size: self.boot.mft_record_size(),
            index_block_size: self.boot.index_block_size(),
            cluster_count: self.boot.cluster_count(),
            mft_lcn: self.boot.mft_lcn,
        }
    }

    pub fn record_offset(&self, number: u64) -> Result<u64> {
        let rec_size = self.boot.mft_record_size() as u64;
        let cluster = self.boot.cluster_size() as u64;
        let recs_per_cluster = if rec_size == 0 {
            return Err(Error::Corrupt("zero MFT record size".into()));
        } else {
            (cluster / rec_size).max(1)
        };
        let vcn = number / recs_per_cluster;
        let into = (number % recs_per_cluster) * rec_size;
        let lcn = vcn_to_lcn(&self.mft_runs, vcn)
            .ok_or_else(|| Error::Corrupt(format!("MFT VCN {vcn} not mapped")))?;
        Ok(lcn * cluster + into)
    }

    pub fn read_record(&self, dev: &dyn BlockDevice, number: u64) -> Result<MftRecord> {
        let off = self.record_offset(number)?;
        let mut buf = vec![0u8; self.boot.mft_record_size() as usize];
        dev.read_at(off, &mut buf)?;
        MftRecord::parse(&buf)
    }

    pub fn list_root_names(&self, dev: &dyn BlockDevice) -> Result<Vec<String>> {
        let rec = self.read_record(dev, 5)?;
        let mut names = Vec::new();
        for a in rec.attributes()? {
            if a.header().ty == AttributeType::INDEX_ROOT {
                if let Attribute::Resident { value, .. } = a {
                    let root = IndexRoot::parse(&value)?;
                    for e in root.entries {
                        if let Some(n) = e.name {
                            if n != "." && n != ".." {
                                names.push(n);
                            }
                        }
                    }
                }
            }
        }
        Ok(names)
    }

    /// File `$DATA` extents for KOIO. Resident / compressed / encrypted
    /// files return `FileData` flags so the caller can inhibit KOIO.
    pub fn file_data(&self, rec: &MftRecord, stream: &str) -> Result<FileData> {
        for a in rec.attributes()? {
            if a.header().ty != AttributeType::DATA {
                continue;
            }
            if a.header().name != stream {
                continue;
            }
            let compressed = a.header().flags.contains(crate::attribute::AttributeFlags::COMPRESSED);
            let encrypted = a.header().flags.contains(crate::attribute::AttributeFlags::ENCRYPTED);
            let sparse = a.header().flags.contains(crate::attribute::AttributeFlags::SPARSE);
            return match a {
                Attribute::Resident { value, .. } => Ok(FileData {
                    size: value.len() as u64,
                    resident: Some(value),
                    runs: Vec::new(),
                    compressed,
                    encrypted,
                    sparse,
                }),
                Attribute::NonResident {
                    mapping_pairs,
                    data_size,
                    ..
                } => Ok(FileData {
                    size: data_size,
                    resident: None,
                    runs: decode_runs(&mapping_pairs)?,
                    compressed,
                    encrypted,
                    sparse,
                }),
            };
        }
        Err(Error::NotFound(format!("$DATA:{stream}")))
    }

    pub fn extents_for_koio(&self, data: &FileData) -> Vec<Extent> {
        if data.resident.is_some() || data.compressed || data.encrypted {
            return Vec::new();
        }
        let cluster = self.boot.cluster_size() as u64;
        let mut logical = 0u64;
        let mut out = Vec::new();
        for run in &data.runs {
            let length = run.cluster_count.saturating_mul(cluster);
            match run.lcn {
                Some(lcn) => out.push(Extent {
                    logical_offset: logical,
                    physical_offset: lcn.saturating_mul(cluster),
                    length,
                    sparse: false,
                }),
                None => out.push(Extent {
                    logical_offset: logical,
                    physical_offset: 0,
                    length,
                    sparse: true,
                }),
            }
            logical = logical.saturating_add(length);
        }
        out
    }

    pub fn read_data(
        &self,
        dev: &dyn BlockDevice,
        data: &FileData,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize> {
        if let Some(value) = &data.resident {
            if offset as usize >= value.len() {
                return Ok(0);
            }
            let n = buf.len().min(value.len() - offset as usize);
            buf[..n].copy_from_slice(&value[offset as usize..offset as usize + n]);
            return Ok(n);
        }
        if data.encrypted {
            return Err(Error::Unsupported("EFS encrypted $DATA"));
        }
        if data.compressed {
            // Full-stream decompress then slice. Fine for small compressed files.
            let mut whole = vec![0u8; data.size as usize];
            // Read allocated clusters then LZNT1.
            let cluster = self.boot.cluster_size() as u64;
            let mut packed = Vec::new();
            for run in &data.runs {
                if let Some(lcn) = run.lcn {
                    let mut chunk = vec![0u8; (run.cluster_count * cluster) as usize];
                    dev.read_at(lcn * cluster, &mut chunk)?;
                    packed.extend_from_slice(&chunk);
                }
            }
            let dec = decompress_lznt1(&packed)?;
            let n = dec.len().min(whole.len());
            whole[..n].copy_from_slice(&dec[..n]);
            if offset as usize >= whole.len() {
                return Ok(0);
            }
            let n = buf.len().min(whole.len() - offset as usize);
            buf[..n].copy_from_slice(&whole[offset as usize..offset as usize + n]);
            return Ok(n);
        }
        let cluster = self.boot.cluster_size() as u64;
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            if pos >= data.size {
                break;
            }
            let vcn = pos / cluster;
            let into = (pos % cluster) as usize;
            match vcn_to_lcn(&data.runs, vcn) {
                None => {
                    // sparse hole
                    let n = (cluster as usize - into).min(buf.len() - done);
                    buf[done..done + n].fill(0);
                    done += n;
                }
                Some(lcn) => {
                    let phys = lcn * cluster + into as u64;
                    let n = (cluster as usize - into).min(buf.len() - done);
                    let remain = (data.size - pos) as usize;
                    let n = n.min(remain);
                    dev.read_at(phys, &mut buf[done..done + n])?;
                    done += n;
                }
            }
        }
        Ok(done)
    }
}

fn data_runs_of(rec: &MftRecord, stream: &str) -> Result<Vec<DataRun>> {
    for a in rec.attributes()? {
        if a.header().ty == AttributeType::DATA && a.header().name == stream {
            if let Attribute::NonResident { mapping_pairs, .. } = a {
                return decode_runs(&mapping_pairs);
            }
        }
    }
    Err(Error::Corrupt("$MFT has no non-resident $DATA".into()))
}
