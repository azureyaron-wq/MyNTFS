//! Promote a directory's resident `$INDEX_ROOT` into one `$INDEX_ALLOCATION`
//! leaf when the MFT record cannot grow.
//!
//! am-fs-ntfs 0.4.0 inserts names only into `$INDEX_ROOT`. On volumes with
//! 1024-byte MFT records (typical Windows USB) that overflows after a
//! handful of names: `growing attribute by N bytes exceeds record capacity`.
//!
//! This is the small half of W3.2: move the leaf entries into one INDX
//! block, leave `$INDEX_ROOT` as an interior node that points at VCN 0,
//! and add `$INDEX_ALLOCATION:$I30` + `$BITMAP:$I30`. Full B+ tree split
//! (a second INDX block) is still upstream work.
//!
//! The INDX block size **must** stay the volume's `$INDEX_ROOT` /
//! boot-sector size (almost always 4096). Inflating it to 16 KiB lets
//! macOS list the folder (we brute-force every INDX) but `ntfs.sys`
//! treats the directory as corrupt (Event 55 / unreadable folder).
//!
//! Layout references (no GPL code): Windows Internals 7th ed. NTFS
//! on-disk structure; MS-FSCC index root / INDX; rust-fs-ntfs docs.

use fs_ntfs::attr_io::{self, AttrType};
use fs_ntfs::attr_resize::{allocate_attribute_id, insert_attribute_sorted, set_resident_value};
use fs_ntfs::bitmap;
use fs_ntfs::block_io::BlockIo;
use fs_ntfs::data_runs::{encode_runs, DataRun};
use fs_ntfs::idx_block;
use fs_ntfs::index_io::{self, IH_FLAG_HAS_SUBNODES};
use fs_ntfs::mft_io::{apply_fixup_on_write_magic, read_mft_record_io, update_mft_record_io};
use fs_ntfs::mkfs::stream;
use fs_ntfs::read;
use fs_ntfs::record_build::{align8, build_nonresident_attribute};
use ntfs_core::{Error, Result};

const ATTR_INDEX_ALLOCATION: u32 = 0xA0;
const ATTR_BITMAP: u32 = 0xB0;
const REC_OFF_BYTES_USED: usize = 0x18;
const ROOT_RECORD: u64 = 5;
const IR_HEADER_LEN: usize = 16;
const IR_INDEX_HEADER_OFFSET: usize = 16;
const IH_FIRST_ENTRY: usize = 0;
const IH_TOTAL_SIZE: usize = 4;
const INDX_USA_OFFSET: usize = 0x28;
const INDX_INDEX_HEADER_OFFSET: usize = 0x18;
const IE_FLAG_NODE: u16 = 0x01;
const IE_FLAG_LAST: u16 = 0x02;
const MAX_INDEX_BLOCK: u64 = 65536;

pub fn is_index_capacity_error(err: &Error) -> bool {
    let s = err.to_string();
    s.contains("exceeds record capacity")
        || s.contains("no INDX block with room")
        || s.contains("insert index entry")
        || s.contains("has sub-nodes; inserting into an interior node")
}

pub fn map_index_error(err: Error) -> Error {
    if is_index_capacity_error(&err) {
        Error::Corrupt(
            "this folder is full (NTFS directory index). Create a new folder and copy fewer items."
                .into(),
        )
    } else {
        err
    }
}

/// Promote `parent_path` from a leaf `$INDEX_ROOT` to one INDX block.
///
/// Returns `Ok(true)` if the directory was promoted, `Ok(false)` if it
/// already had `$INDEX_ALLOCATION` (caller should surface a folder-full
/// error).
pub fn promote_leaf_directory<T: BlockIo + ?Sized>(io: &mut T, parent_path: &str) -> Result<bool> {
    let rec_num = read::resolve_path(io, parent_path).map_err(Error::Corrupt)?;
    let (params, record) = read_mft_record_io(io, rec_num).map_err(Error::Corrupt)?;

    let flags = index_io::index_root_flags(&record)
        .ok_or_else(|| Error::Corrupt("parent directory has no $INDEX_ROOT".into()))?;
    if flags & IH_FLAG_HAS_SUBNODES != 0
        || attr_io::find_attribute(&record, AttrType::IndexAllocation, Some(stream::I30)).is_some()
    {
        return Ok(false);
    }

    let ir = attr_io::find_attribute(&record, AttrType::IndexRoot, Some(stream::I30))
        .ok_or_else(|| Error::Corrupt("$INDEX_ROOT:$I30 missing".into()))?;
    let val_off =
        ir.resident_value_offset
            .ok_or_else(|| Error::Corrupt("$INDEX_ROOT has no value".into()))? as usize;
    let val_len = ir
        .resident_value_length
        .ok_or_else(|| Error::Corrupt("$INDEX_ROOT has no value length".into()))?
        as usize;
    let val_start = ir.attr_offset + val_off;
    if val_len < IR_HEADER_LEN + 16 || val_start + val_len > record.len() {
        return Err(Error::Corrupt("$INDEX_ROOT value is truncated".into()));
    }

    let mut ir_header = [0u8; IR_HEADER_LEN];
    ir_header.copy_from_slice(&record[val_start..val_start + IR_HEADER_LEN]);
    let declared_block = u32::from_le_bytes(ir_header[8..12].try_into().unwrap());

    let ih_start = val_start + IR_INDEX_HEADER_OFFSET;
    let first_rel = u32::from_le_bytes(
        record[ih_start + IH_FIRST_ENTRY..ih_start + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let total_size = u32::from_le_bytes(
        record[ih_start + IH_TOTAL_SIZE..ih_start + 8]
            .try_into()
            .unwrap(),
    ) as usize;
    if first_rel > total_size || ih_start + total_size > val_start + val_len {
        return Err(Error::Corrupt(
            "$INDEX_ROOT index header is truncated".into(),
        ));
    }
    let entries = record[ih_start + first_rel..ih_start + total_size].to_vec();
    if entries.len() < 16 {
        return Err(Error::Corrupt("$INDEX_ROOT has no LAST sentinel".into()));
    }

    let cluster = params.cluster_size.max(512);
    let block_size = pick_index_block_size(cluster, declared_block);
    if block_size < 512 || block_size > MAX_INDEX_BLOCK || !block_size.is_power_of_two() {
        return Err(Error::Corrupt(format!(
            "cannot pick an INDX block size (cluster={cluster}, block={block_size})"
        )));
    }
    // Never rewrite INDEX_ROOT's index_block_size / clusters_per_index_block
    // when they already match the volume. chkdsk and ntfs.sys compare them
    // to boot sector +0x44; a 16 KiB leaf on a 4 KiB volume is Event 55.
    if block_size != declared_block as u64 {
        ir_header[8..12].copy_from_slice(&(block_size as u32).to_le_bytes());
        ir_header[12] = clusters_per_index_block(block_size, cluster);
    }
    let n_clusters = if block_size >= cluster {
        if block_size % cluster != 0 {
            return Err(Error::Corrupt(format!(
                "INDX block {block_size} is not a multiple of cluster {cluster}"
            )));
        }
        block_size / cluster
    } else {
        1
    };

    if (INDX_INDEX_HEADER_OFFSET
        + first_entry_rel(block_size, params.bytes_per_sector)
        + entries.len()) as u64
        > block_size
    {
        return Err(Error::Corrupt(
            "directory index is larger than one INDX block; split is not implemented".into(),
        ));
    }

    let vol_bm = bitmap::locate_bitmap_io(io).map_err(Error::Corrupt)?;
    let lcn = bitmap::find_free_run_io(io, &vol_bm, n_clusters, params.mft_lcn)
        .map_err(Error::Corrupt)?
        .ok_or_else(|| {
            Error::Corrupt(format!(
                "no free run of {n_clusters} clusters for directory index"
            ))
        })?;
    bitmap::allocate_io(io, &vol_bm, lcn, n_clusters).map_err(Error::Corrupt)?;

    let mut block = match build_indx_leaf(block_size as usize, params.bytes_per_sector, 0, &entries)
    {
        Ok(b) => b,
        Err(e) => {
            let _ = bitmap::free_io(io, &vol_bm, lcn, n_clusters);
            return Err(Error::Corrupt(e));
        }
    };
    if let Err(e) = apply_fixup_on_write_magic(&mut block, params.bytes_per_sector, b"INDX") {
        let _ = bitmap::free_io(io, &vol_bm, lcn, n_clusters);
        return Err(Error::Corrupt(e));
    }
    let disk_off = lcn * cluster;
    if let Err(e) = io.write_all_at(disk_off, &block) {
        let _ = bitmap::free_io(io, &vol_bm, lcn, n_clusters);
        return Err(Error::Corrupt(format!("write INDX: {e}")));
    }
    if let Err(e) = io.sync() {
        let _ = bitmap::free_io(io, &vol_bm, lcn, n_clusters);
        return Err(Error::Corrupt(format!("fsync INDX: {e}")));
    }

    let mapping = match encode_runs(&[DataRun {
        starting_vcn: 0,
        length: n_clusters,
        lcn: Some(lcn),
    }]) {
        Ok(m) => m,
        Err(e) => {
            let _ = bitmap::free_io(io, &vol_bm, lcn, n_clusters);
            return Err(Error::Corrupt(e));
        }
    };
    let new_ir = build_index_root_node(&ir_header, 0);
    let last_vcn = (n_clusters - 1) as i64;
    let allocated_length = n_clusters * cluster;

    let mft = update_mft_record_io(io, rec_num, |record| {
        let ir = attr_io::find_attribute(record, AttrType::IndexRoot, Some(stream::I30))
            .ok_or_else(|| "$INDEX_ROOT vanished during promote".to_string())?;
        set_resident_value(record, ir.attr_offset, &new_ir)?;
        let ia_id = allocate_attribute_id(record);
        let ia = build_nonresident_attribute(
            ATTR_INDEX_ALLOCATION,
            Some(stream::I30),
            ia_id,
            block_size,
            allocated_length,
            block_size,
            last_vcn,
            &mapping,
        )?;
        insert_attribute_sorted(record, &ia)?;
        let bm_id = allocate_attribute_id(record);
        let bm_attr = build_resident_named(
            ATTR_BITMAP,
            stream::I30,
            bm_id,
            &[0x01, 0, 0, 0, 0, 0, 0, 0],
        )?;
        insert_attribute_sorted(record, &bm_attr)?;
        Ok(())
    });
    if let Err(e) = mft {
        let _ = bitmap::free_io(io, &vol_bm, lcn, n_clusters);
        return Err(Error::Corrupt(format!("promote $INDEX_ROOT: {e}")));
    }
    Ok(true)
}

pub fn is_overflow_rmdir_error(err: &Error) -> bool {
    let s = err.to_string();
    s.contains("$INDEX_ALLOCATION overflow") || s.contains("probably not empty")
}

/// If `dir_path` is an empty overflowed directory, collapse it back to a
/// resident `$INDEX_ROOT` leaf so `rmdir` can free the record.
pub fn demote_empty_directory<T: BlockIo + ?Sized>(io: &mut T, dir_path: &str) -> Result<bool> {
    let rec_num = read::resolve_path(io, dir_path).map_err(Error::Corrupt)?;
    if rec_num == ROOT_RECORD {
        return Ok(false);
    }
    let (_params, record) = read_mft_record_io(io, rec_num).map_err(Error::Corrupt)?;
    let flags = index_io::index_root_flags(&record)
        .ok_or_else(|| Error::Corrupt("directory has no $INDEX_ROOT".into()))?;
    if flags & IH_FLAG_HAS_SUBNODES == 0
        && attr_io::find_attribute(&record, AttrType::IndexAllocation, Some(stream::I30)).is_none()
    {
        return Ok(false);
    }

    let ia = idx_block::load_for_directory_io(io, rec_num).map_err(Error::Corrupt)?;
    for vcn in ia.allocated_block_vcns() {
        let blk = idx_block::read_indx_block_io(io, &ia, vcn).map_err(Error::Corrupt)?;
        let mut ents = Vec::new();
        index_io::collect_indx_block_entries(&blk, &mut ents).map_err(Error::Corrupt)?;
        if !ents.is_empty() {
            return Err(Error::Corrupt(format!("rmdir: '{dir_path}' is not empty")));
        }
    }

    let ir = attr_io::find_attribute(&record, AttrType::IndexRoot, Some(stream::I30))
        .ok_or_else(|| Error::Corrupt("$INDEX_ROOT:$I30 missing".into()))?;
    let val_off =
        ir.resident_value_offset
            .ok_or_else(|| Error::Corrupt("$INDEX_ROOT has no value".into()))? as usize;
    let val_start = ir.attr_offset + val_off;
    if val_start + IR_HEADER_LEN > record.len() {
        return Err(Error::Corrupt("$INDEX_ROOT value is truncated".into()));
    }
    let mut ir_header = [0u8; IR_HEADER_LEN];
    ir_header.copy_from_slice(&record[val_start..val_start + IR_HEADER_LEN]);
    let empty_ir = build_empty_leaf_ir(&ir_header);

    let mut to_free: Vec<(u64, u64)> = Vec::new();
    for run in &ia.runs {
        if let Some(lcn) = run.lcn {
            if run.length > 0 {
                to_free.push((lcn, run.length));
            }
        }
    }

    update_mft_record_io(io, rec_num, |record| {
        let ir = attr_io::find_attribute(record, AttrType::IndexRoot, Some(stream::I30))
            .ok_or_else(|| "$INDEX_ROOT vanished during demote".to_string())?;
        set_resident_value(record, ir.attr_offset, &empty_ir)?;
        let mut offs = Vec::new();
        if let Some(bm) = attr_io::find_attribute(record, AttrType::Bitmap, Some(stream::I30)) {
            offs.push(bm.attr_offset);
        }
        if let Some(ia_attr) =
            attr_io::find_attribute(record, AttrType::IndexAllocation, Some(stream::I30))
        {
            offs.push(ia_attr.attr_offset);
        }
        offs.sort_unstable_by(|a, b| b.cmp(a));
        for off in offs {
            remove_attribute(record, off)?;
        }
        Ok(())
    })
    .map_err(|e| Error::Corrupt(format!("demote $INDEX_ALLOCATION: {e}")))?;

    if !to_free.is_empty() {
        let vol_bm = bitmap::locate_bitmap_io(io).map_err(Error::Corrupt)?;
        for (lcn, n) in to_free {
            bitmap::free_io(io, &vol_bm, lcn, n).map_err(Error::Corrupt)?;
        }
    }
    Ok(true)
}

fn build_empty_leaf_ir(ir_header: &[u8; 16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(48);
    v.extend_from_slice(ir_header);
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&32u32.to_le_bytes());
    v.extend_from_slice(&32u32.to_le_bytes());
    v.extend_from_slice(&[0, 0, 0, 0]);
    v.extend_from_slice(&[0u8; 8]);
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&IE_FLAG_LAST.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v
}

fn remove_attribute(record: &mut [u8], attr_offset: usize) -> std::result::Result<(), String> {
    if attr_offset + 8 > record.len() {
        return Err("attribute header truncated".into());
    }
    let attr_len =
        u32::from_le_bytes(record[attr_offset + 4..attr_offset + 8].try_into().unwrap()) as usize;
    if attr_len == 0 || !attr_len.is_multiple_of(8) || attr_offset + attr_len > record.len() {
        return Err(format!("bad attribute length {attr_len} at {attr_offset}"));
    }
    let bytes_used = u32::from_le_bytes(
        record[REC_OFF_BYTES_USED..REC_OFF_BYTES_USED + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    if attr_offset + attr_len > bytes_used {
        return Err("attribute extends past bytes_used".into());
    }
    record.copy_within(attr_offset + attr_len..bytes_used, attr_offset);
    for b in &mut record[bytes_used - attr_len..bytes_used] {
        *b = 0;
    }
    let new_used = (bytes_used - attr_len) as u32;
    record[REC_OFF_BYTES_USED..REC_OFF_BYTES_USED + 4].copy_from_slice(&new_used.to_le_bytes());
    Ok(())
}

/// Use the directory's existing index record size. Do not inflate: a
/// larger INDX than boot +0x44 / `$INDEX_ROOT` is unreadable on Windows.
fn pick_index_block_size(cluster: u64, declared: u32) -> u64 {
    let cluster = cluster.max(512);
    let declared = declared as u64;
    if declared >= 512 && declared <= MAX_INDEX_BLOCK && declared.is_power_of_two() {
        return declared;
    }
    if cluster <= MAX_INDEX_BLOCK && cluster.is_power_of_two() {
        return cluster;
    }
    4096
}

fn clusters_per_index_block(block_size: u64, cluster: u64) -> u8 {
    if cluster <= block_size {
        (block_size / cluster) as u8
    } else {
        (block_size / 512) as u8
    }
}

fn first_entry_rel(block_size: u64, bytes_per_sector: u16) -> usize {
    let sectors = block_size as usize / bytes_per_sector as usize;
    let usa_count = sectors + 1;
    let usa_end = INDX_USA_OFFSET + usa_count * 2;
    align8(usa_end) - INDX_INDEX_HEADER_OFFSET
}

fn build_indx_leaf(
    block_size: usize,
    bytes_per_sector: u16,
    vcn: u64,
    entries: &[u8],
) -> std::result::Result<Vec<u8>, String> {
    let bps = bytes_per_sector as usize;
    if bps == 0 || block_size < bps || block_size % bps != 0 {
        return Err(format!("bad INDX geometry block={block_size} sector={bps}"));
    }
    let sectors = block_size / bps;
    let usa_count = sectors + 1;
    let first_abs = align8(INDX_USA_OFFSET + usa_count * 2);
    if first_abs + entries.len() > block_size {
        return Err("INDX leaf entries do not fit in the block".into());
    }
    let mut block = vec![0u8; block_size];
    block[0..4].copy_from_slice(b"INDX");
    block[4..6].copy_from_slice(&(INDX_USA_OFFSET as u16).to_le_bytes());
    block[6..8].copy_from_slice(&(usa_count as u16).to_le_bytes());
    block[0x10..0x18].copy_from_slice(&vcn.to_le_bytes());
    block[INDX_USA_OFFSET..INDX_USA_OFFSET + 2].copy_from_slice(&1u16.to_le_bytes());

    let first_rel = first_abs - INDX_INDEX_HEADER_OFFSET;
    let total = (first_rel + entries.len()) as u32;
    let allocated = (block_size - INDX_INDEX_HEADER_OFFSET) as u32;
    let ih = INDX_INDEX_HEADER_OFFSET;
    block[ih..ih + 4].copy_from_slice(&(first_rel as u32).to_le_bytes());
    block[ih + 4..ih + 8].copy_from_slice(&total.to_le_bytes());
    block[ih + 8..ih + 12].copy_from_slice(&allocated.to_le_bytes());
    block[first_abs..first_abs + entries.len()].copy_from_slice(entries);
    Ok(block)
}

/// Interior `$INDEX_ROOT` value: IR header + INDEX_HEADER + LAST+NODE(VCN).
fn build_index_root_node(ir_header: &[u8; 16], leaf_vcn: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(56);
    v.extend_from_slice(ir_header);
    v.extend_from_slice(&16u32.to_le_bytes()); // first_entry
    v.extend_from_slice(&40u32.to_le_bytes()); // total_size (header + 24-byte LAST)
    v.extend_from_slice(&40u32.to_le_bytes()); // allocated_size
    v.push(IH_FLAG_HAS_SUBNODES);
    v.extend_from_slice(&[0, 0, 0]);
    v.extend_from_slice(&[0u8; 8]); // file reference
    v.extend_from_slice(&24u16.to_le_bytes()); // length includes trailing VCN
    v.extend_from_slice(&0u16.to_le_bytes()); // key_length
    v.extend_from_slice(&(IE_FLAG_NODE | IE_FLAG_LAST).to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&leaf_vcn.to_le_bytes());
    v
}

fn build_resident_named(
    type_code: u32,
    name: &str,
    attr_id: u16,
    value: &[u8],
) -> std::result::Result<Vec<u8>, String> {
    let name_u16: Vec<u16> = name.encode_utf16().collect();
    if name_u16.len() > 255 {
        return Err("attribute name too long".into());
    }
    let header = 24usize;
    let name_off = header;
    let name_bytes = name_u16.len() * 2;
    let value_off = align8(name_off + name_bytes);
    let attr_len = align8(value_off + value.len());
    let mut buf = vec![0u8; attr_len];
    buf[0..4].copy_from_slice(&type_code.to_le_bytes());
    buf[4..8].copy_from_slice(&(attr_len as u32).to_le_bytes());
    buf[8] = 0;
    buf[9] = name_u16.len() as u8;
    buf[10..12].copy_from_slice(&(name_off as u16).to_le_bytes());
    buf[14..16].copy_from_slice(&attr_id.to_le_bytes());
    buf[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
    buf[20..22].copy_from_slice(&(value_off as u16).to_le_bytes());
    for (i, c) in name_u16.iter().enumerate() {
        buf[name_off + i * 2..name_off + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
    }
    buf[value_off..value_off + value.len()].copy_from_slice(value);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_block_keeps_volume_index_size() {
        assert_eq!(pick_index_block_size(4096, 4096), 4096);
        assert_eq!(pick_index_block_size(512, 4096), 4096);
        assert_eq!(pick_index_block_size(4096, 0), 4096);
        assert_eq!(pick_index_block_size(4096, 16384), 16384);
        assert!(pick_index_block_size(4096, 4096).is_power_of_two());
    }

    #[test]
    fn index_root_node_is_56_bytes_with_subnodes() {
        let mut hdr = [0u8; 16];
        hdr[8..12].copy_from_slice(&4096u32.to_le_bytes());
        hdr[12] = 1;
        let v = build_index_root_node(&hdr, 0);
        assert_eq!(v.len(), 56);
        assert_eq!(v[28], IH_FLAG_HAS_SUBNODES);
        assert_eq!(&v[48..56], &0u64.to_le_bytes());
    }

    #[test]
    fn promote_keeps_declared_4096_index_block_size() {
        use crate::{format_image, Volume};
        use fs_ntfs::block_io::PathIo;
        use ntfs_core::WritePolicy;

        let dir = std::env::temp_dir().join(format!("myntfs-idxwin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("vol.img");
        format_image(&img, 64 * 1024 * 1024, Some("IdxWin")).unwrap();

        let vol = Volume::mount(&img, WritePolicy::ReadWriteIfSafe).unwrap();
        vol.mkdir("/", "drop").unwrap();
        let pad = "x".repeat(72);
        for i in 0..16 {
            let name = format!("n{i:03}_{pad}.txt");
            vol.create_file("/drop", &name).unwrap();
        }
        drop(vol);

        let mut io = PathIo::open_ro(&img).unwrap();
        let rec_num = read::resolve_path(&mut io, "/drop").unwrap();
        let (_params, record) = read_mft_record_io(&mut io, rec_num).unwrap();
        let flags = index_io::index_root_flags(&record).unwrap();
        assert_ne!(
            flags & IH_FLAG_HAS_SUBNODES,
            0,
            "expected $INDEX_ALLOCATION promotion"
        );
        let ir = attr_io::find_attribute(&record, AttrType::IndexRoot, Some(stream::I30)).unwrap();
        let val_off = ir.resident_value_offset.unwrap() as usize;
        let val_start = ir.attr_offset + val_off;
        let block = u32::from_le_bytes(record[val_start + 8..val_start + 12].try_into().unwrap());
        assert_eq!(
            block, 4096,
            "Windows ntfs.sys rejects INDX larger than the volume index record size"
        );
        assert!(
            attr_io::find_attribute(&record, AttrType::IndexAllocation, Some(stream::I30)).is_some()
        );
    }
}
