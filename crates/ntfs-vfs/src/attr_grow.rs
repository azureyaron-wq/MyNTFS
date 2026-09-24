//! Grow / preallocate non-resident `$DATA` and resize the attribute when
//! mapping-pairs no longer fit the original 8-byte slot (am-fs-ntfs W2.1).
//!
//! Large copies used to fail with:
//! `new mapping_pairs (14 bytes) exceed attr capacity (8)`.
//! A 300 MB installer on a fragmented USB typically needs a second data
//! run; this rebuilds `$DATA` with `replace_attribute` so the run list
//! can grow. Mapping-pairs are padded so later grows usually fit in place.

use fs_ntfs::attr_io::{self, AttrType};
use fs_ntfs::attr_resize::replace_attribute;
use fs_ntfs::bitmap::{self, BitmapLocation};
use fs_ntfs::block_io::BlockIo;
use fs_ntfs::data_runs::{encode_runs, DataRun};
use fs_ntfs::mft_io::{read_mft_record_io, update_mft_record_io};
use fs_ntfs::read;
use fs_ntfs::record_build::build_nonresident_data_attribute;
use ntfs_core::{Error, Result};

const MAPPING_PAD: usize = 64;
const NONRES_INITIALIZED_LENGTH: usize = 0x38;

pub fn map_alloc_error(msg: impl AsRef<str>) -> Error {
    let s = msg.as_ref();
    if s.contains("no contiguous free")
        || s.contains("no free")
        || s.contains("exceeds record capacity")
        || s.contains("no room for new attribute")
    {
        Error::Corrupt(
            "not enough free space (or the file record is full) to store this file. Free some space and try again."
                .into(),
        )
    } else if s.contains("Attribute resize") || s.contains("mapping_pairs") {
        Error::Corrupt(
            "could not grow the file’s cluster map. Close the volume and try again, or copy a smaller file."
                .into(),
        )
    } else {
        Error::Corrupt(s.to_string())
    }
}

pub fn preallocate_io<T: BlockIo + ?Sized>(io: &mut T, path: &str, size: u64) -> Result<()> {
    if size == 0 {
        return Ok(());
    }
    let rec = read::resolve_path(io, path).map_err(map_alloc_error)?;
    let (params, record) = read_mft_record_io(io, rec).map_err(map_alloc_error)?;
    let loc = attr_io::find_attribute(&record, AttrType::Data, None)
        .ok_or_else(|| Error::Corrupt("unnamed $DATA missing".into()))?;
    if loc.is_resident {
        let cluster_size = params.cluster_size;
        let n_clusters = size.div_ceil(cluster_size).max(1);
        let bm = bitmap::locate_bitmap_io(io).map_err(map_alloc_error)?;
        let (runs, allocated) =
            allocate_runs(io, &bm, n_clusters, params.mft_lcn.saturating_add(32))
                .map_err(map_alloc_error)?;
        let last_vcn = n_clusters as i64 - 1;
        let allocated_bytes = n_clusters * cluster_size;
        if let Err(e) = commit_data_runs(
            io,
            rec,
            loc.attribute_id,
            size,
            allocated_bytes,
            0,
            last_vcn,
            &runs,
        ) {
            free_runs(io, &bm, &allocated);
            return Err(map_alloc_error(e));
        }
        return Ok(());
    }
    let current = loc.non_resident_value_length.unwrap_or(0);
    if size <= current {
        return Ok(());
    }
    grow_io(io, path, size)?;
    Ok(())
}

pub fn grow_io<T: BlockIo + ?Sized>(io: &mut T, path: &str, new_size: u64) -> Result<u64> {
    let rec = read::resolve_path(io, path).map_err(map_alloc_error)?;
    grow_by_record_io(io, rec, new_size)
}

fn grow_by_record_io<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
    new_size: u64,
) -> Result<u64> {
    let (params, record) = read_mft_record_io(io, record_number).map_err(map_alloc_error)?;
    let cluster_size = params.cluster_size;
    let loc = attr_io::find_attribute(&record, AttrType::Data, None)
        .ok_or_else(|| Error::Corrupt("unnamed $DATA missing".into()))?;
    if loc.is_resident {
        return Err(Error::Corrupt(
            "file is still small; write some data before growing it".into(),
        ));
    }
    let flags = u16::from_le_bytes([
        record[loc.attr_offset + attr_io::attr_off::FLAGS],
        record[loc.attr_offset + attr_io::attr_off::FLAGS + 1],
    ]);
    if flags & 0x00FF != 0 {
        return Err(Error::Corrupt(format!(
            "compressed/sparse/encrypted $DATA (flags={flags:#06x})"
        )));
    }
    let current_len = loc.non_resident_value_length.unwrap_or(0);
    if new_size <= current_len {
        return Ok(current_len);
    }

    let mapping_offset = loc
        .non_resident_mapping_pairs_offset
        .ok_or_else(|| Error::Corrupt("missing mapping_pairs_offset".into()))?
        as usize;
    let mapping_start = loc.attr_offset + mapping_offset;
    let mapping_end = loc.attr_offset + loc.attr_length;
    let runs = fs_ntfs::data_runs::decode_runs(&record[mapping_start..mapping_end])
        .map_err(map_alloc_error)?;
    let initialized = if loc.attr_offset + NONRES_INITIALIZED_LENGTH + 8 <= record.len() {
        u64::from_le_bytes(
            record[loc.attr_offset + NONRES_INITIALIZED_LENGTH
                ..loc.attr_offset + NONRES_INITIALIZED_LENGTH + 8]
                .try_into()
                .unwrap(),
        )
    } else {
        current_len
    };

    let new_last_vcn = (new_size - 1) / cluster_size;
    let new_allocated = (new_last_vcn + 1) * cluster_size;
    let current_end_vcn: u64 = runs
        .iter()
        .map(|r| r.starting_vcn + r.length)
        .max()
        .unwrap_or(0);
    let need_clusters = (new_last_vcn + 1).saturating_sub(current_end_vcn);
    if need_clusters == 0 {
        commit_data_runs(
            io,
            record_number,
            loc.attribute_id,
            new_size,
            new_allocated,
            initialized.min(new_size),
            new_last_vcn as i64,
            &runs,
        )
        .map_err(map_alloc_error)?;
        return Ok(new_size);
    }

    let bm = bitmap::locate_bitmap_io(io).map_err(map_alloc_error)?;
    let hint = runs
        .iter()
        .rev()
        .find_map(|r| r.lcn.map(|lcn| lcn + r.length))
        .unwrap_or(params.mft_lcn.saturating_add(32));
    let (extra, allocated) =
        allocate_runs(io, &bm, need_clusters, hint).map_err(map_alloc_error)?;

    let mut new_runs = runs;
    for add in extra {
        if let Some(last) = new_runs.last_mut() {
            if last
                .lcn
                .zip(add.lcn)
                .is_some_and(|(a, b)| a + last.length == b)
            {
                last.length += add.length;
                continue;
            }
        }
        new_runs.push(add);
    }
    let mut vcn = 0u64;
    for r in &mut new_runs {
        r.starting_vcn = vcn;
        vcn += r.length;
    }

    if let Err(e) = commit_data_runs(
        io,
        record_number,
        loc.attribute_id,
        new_size,
        new_allocated,
        initialized.min(new_size),
        new_last_vcn as i64,
        &new_runs,
    ) {
        free_runs(io, &bm, &allocated);
        return Err(map_alloc_error(e));
    }
    Ok(new_size)
}

fn allocate_runs<T: BlockIo + ?Sized>(
    io: &mut T,
    bm: &BitmapLocation,
    need: u64,
    hint: u64,
) -> std::result::Result<(Vec<DataRun>, Vec<(u64, u64)>), String> {
    let mut remaining = need;
    let mut vcn = 0u64;
    let mut scan = hint;
    let mut runs = Vec::new();
    let mut allocated = Vec::new();
    while remaining > 0 {
        let mut ask = remaining;
        let lcn = loop {
            match bitmap::find_free_run_io(io, bm, ask, scan)? {
                Some(lcn) => break lcn,
                None => {
                    if ask == 1 {
                        free_runs(io, bm, &allocated);
                        return Err(format!(
                            "no free clusters for {need} (stuck with {remaining} left)"
                        ));
                    }
                    ask = (ask / 2).max(1);
                }
            }
        };
        if let Err(e) = bitmap::allocate_io(io, bm, lcn, ask) {
            free_runs(io, bm, &allocated);
            return Err(e);
        }
        allocated.push((lcn, ask));
        runs.push(DataRun {
            starting_vcn: vcn,
            length: ask,
            lcn: Some(lcn),
        });
        vcn += ask;
        remaining -= ask;
        scan = lcn.saturating_add(ask);
    }
    Ok((runs, allocated))
}

fn free_runs<T: BlockIo + ?Sized>(io: &mut T, bm: &BitmapLocation, allocated: &[(u64, u64)]) {
    for (lcn, n) in allocated {
        let _ = bitmap::free_io(io, bm, *lcn, *n);
    }
}

fn commit_data_runs<T: BlockIo + ?Sized>(
    io: &mut T,
    record_number: u64,
    attr_id: u16,
    data_length: u64,
    allocated_length: u64,
    initialized_length: u64,
    last_vcn: i64,
    runs: &[DataRun],
) -> std::result::Result<(), String> {
    let mut mapping = encode_runs(runs)?;
    if mapping.len() < MAPPING_PAD {
        mapping.resize(MAPPING_PAD, 0);
    }
    let new_attr = build_nonresident_data_attribute(
        attr_id,
        data_length,
        allocated_length,
        initialized_length,
        last_vcn,
        &mapping,
    )?;
    update_mft_record_io(io, record_number, |record| {
        let loc = attr_io::find_attribute(record, AttrType::Data, None)
            .ok_or_else(|| "$DATA vanished during resize".to_string())?;
        replace_attribute(record, loc.attr_offset, &new_attr)
    })
}
