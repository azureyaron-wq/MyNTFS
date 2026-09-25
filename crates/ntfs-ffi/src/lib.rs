//! C ABI for Swift / FSKit consumers.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::path::Path;
use std::ptr;

use ntfs_core::{Result as CoreResult, WritePolicy};
use ntfs_io::diskarb;
use ntfs_vfs::{copy_in, copy_out, device_gate::DeviceWriteConfirm, format_image, Volume};

thread_local! {
    static LAST_ERR: RefCell<CString> = RefCell::new(CString::new("").unwrap());
}

pub struct MyNtfsVolume {
    vol: Volume,
}

fn set_err(msg: impl AsRef<str>) {
    if let Ok(c) = CString::new(msg.as_ref()) {
        LAST_ERR.with(|e| *e.borrow_mut() = c);
    }
}

fn write_errbuf(errbuf: *mut c_char, errbuf_len: usize, msg: &str) {
    if errbuf.is_null() || errbuf_len == 0 {
        return;
    }
    let bytes = msg.as_bytes();
    let n = bytes.len().min(errbuf_len - 1);
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), errbuf as *mut u8, n);
        *errbuf.add(n) = 0;
    }
}

#[repr(C)]
pub struct MyNtfsSafetyReport {
    pub verified: c_int,
    pub dirty: c_int,
    pub hibernated: c_int,
    pub bitlocker: c_int,
    pub efs_present: c_int,
    pub writable_safe: c_int,
}

fn fill_safety(out: &mut MyNtfsSafetyReport, report: &ntfs_core::SafetyReport) {
    out.verified = report.verified.into();
    out.dirty = report.dirty.into();
    out.hibernated = report.hibernated.into();
    out.bitlocker = report.bitlocker.into();
    out.efs_present = report.efs_present.into();
    out.writable_safe = report
        .writable(ntfs_core::WritePolicy::ReadWriteIfSafe)
        .into();
}

#[no_mangle]
pub extern "C" fn myntfs_probe(
    path: *const c_char,
    out: *mut MyNtfsSafetyReport,
    errbuf: *mut c_char,
    errbuf_len: usize,
) -> c_int {
    if path.is_null() || out.is_null() {
        write_errbuf(errbuf, errbuf_len, "null argument");
        return -1;
    }
    let Ok(cpath) = (unsafe { CStr::from_ptr(path) }).to_str() else {
        write_errbuf(errbuf, errbuf_len, "invalid path");
        return -1;
    };
    match Volume::probe_path(cpath) {
        Ok(report) => {
            unsafe { fill_safety(&mut *out, &report) };
            0
        }
        Err(e) => {
            let msg = e.to_string();
            set_err(&msg);
            write_errbuf(errbuf, errbuf_len, &msg);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn myntfs_mount(
    path: *const c_char,
    writable: c_int,
    errbuf: *mut c_char,
    errbuf_len: usize,
) -> *mut MyNtfsVolume {
    myntfs_mount_ex(path, writable, 0, errbuf, errbuf_len)
}

#[no_mangle]
pub extern "C" fn myntfs_mount_ex(
    path: *const c_char,
    writable: c_int,
    allow_device_write: c_int,
    errbuf: *mut c_char,
    errbuf_len: usize,
) -> *mut MyNtfsVolume {
    let Ok(cpath) = (unsafe { CStr::from_ptr(path) }).to_str() else {
        set_err("invalid path");
        write_errbuf(errbuf, errbuf_len, "invalid path");
        return ptr::null_mut();
    };
    let policy = if writable != 0 {
        WritePolicy::ReadWriteIfSafe
    } else {
        WritePolicy::ReadOnly
    };
    let confirm = DeviceWriteConfirm {
        allow_block_device: allow_device_write != 0,
    };
    match Volume::mount_with_confirm(cpath, policy, confirm) {
        Ok(vol) => Box::into_raw(Box::new(MyNtfsVolume { vol })),
        Err(e) => {
            let msg = e.to_string();
            set_err(&msg);
            write_errbuf(errbuf, errbuf_len, &msg);
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn myntfs_mount_fd(
    fd: c_int,
    display_path: *const c_char,
    writable: c_int,
    allow_device_write: c_int,
    errbuf: *mut c_char,
    errbuf_len: usize,
) -> *mut MyNtfsVolume {
    let display = if display_path.is_null() {
        "<fd>"
    } else {
        match unsafe { CStr::from_ptr(display_path) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                set_err("invalid path");
                write_errbuf(errbuf, errbuf_len, "invalid path");
                return ptr::null_mut();
            }
        }
    };
    let policy = if writable != 0 {
        WritePolicy::ReadWriteIfSafe
    } else {
        WritePolicy::ReadOnly
    };
    let confirm = DeviceWriteConfirm {
        allow_block_device: allow_device_write != 0,
    };
    match Volume::mount_from_raw_fd(fd, display, policy, confirm) {
        Ok(vol) => Box::into_raw(Box::new(MyNtfsVolume { vol })),
        Err(e) => {
            let msg = e.to_string();
            set_err(&msg);
            write_errbuf(errbuf, errbuf_len, &msg);
            ptr::null_mut()
        }
    }
}

/// Destroys the volume. Best-effort `commit_for_unmount` first (no-op if
/// read-only), then drop. Never leaks the Box. `myntfs_sync` is the same
/// commit without destroying the handle.
#[no_mangle]
pub extern "C" fn myntfs_umount(vol: *mut MyNtfsVolume) {
    if vol.is_null() {
        return;
    }
    unsafe {
        if let Err(e) = (*vol).vol.commit_for_unmount() {
            set_err(e.to_string());
        }
        drop(Box::from_raw(vol));
    }
}

#[no_mangle]
pub extern "C" fn myntfs_sync(vol: *mut MyNtfsVolume) -> c_int {
    if vol.is_null() {
        set_err("null volume");
        return -1;
    }
    match unsafe { (*vol).vol.commit_for_unmount() } {
        Ok(()) => 0,
        Err(e) => {
            let msg = e.to_string();
            set_err(&msg);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn myntfs_is_writable(vol: *const MyNtfsVolume) -> c_int {
    if vol.is_null() {
        return 0;
    }
    unsafe { (*vol).vol.writable().into() }
}

#[no_mangle]
pub extern "C" fn myntfs_volume_serial(vol: *const MyNtfsVolume, out_serial: *mut u64) -> c_int {
    if vol.is_null() || out_serial.is_null() {
        return -1;
    }
    if let Some(serial) = unsafe { (*vol).vol.serial() } {
        unsafe {
            *out_serial = serial;
        }
        return 0;
    }
    -1
}

#[no_mangle]
pub extern "C" fn myntfs_volume_space(
    vol: *mut MyNtfsVolume,
    out_total: *mut u64,
    out_free: *mut u64,
) -> c_int {
    if vol.is_null() || out_total.is_null() || out_free.is_null() {
        return -1;
    }
    match unsafe { (*vol).vol.volume_space() } {
        Ok((total, free)) => {
            unsafe {
                *out_total = total;
                *out_free = free;
            }
            0
        }
        Err(e) => {
            set_err(e.to_string());
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn myntfs_volume_safety(
    vol: *const MyNtfsVolume,
    out: *mut MyNtfsSafetyReport,
) -> c_int {
    if vol.is_null() || out.is_null() {
        return -1;
    }
    unsafe { fill_safety(&mut *out, (*vol).vol.safety()) };
    0
}

#[no_mangle]
pub extern "C" fn myntfs_listdir(
    vol: *mut MyNtfsVolume,
    path: *const c_char,
    buf: *mut c_char,
    buf_len: usize,
    is_dir: *mut u8,
    sizes: *mut u64,
    max_entries: c_int,
) -> c_int {
    if vol.is_null() || path.is_null() {
        set_err("null argument");
        return -1;
    }
    let Ok(dir) = (unsafe { CStr::from_ptr(path) }).to_str() else {
        set_err("invalid path");
        return -1;
    };
    let entries = match unsafe { (*vol).vol.list(dir) } {
        Ok(e) => e,
        Err(e) => {
            set_err(e.to_string());
            return -1;
        }
    };
    let mut off = 0usize;
    let mut count = 0i32;
    for (i, e) in entries.iter().enumerate() {
        if i >= max_entries as usize {
            break;
        }
        if !is_dir.is_null() {
            unsafe {
                *is_dir.add(i) = u8::from(e.is_dir);
            }
        }
        if !sizes.is_null() {
            unsafe {
                *sizes.add(i) = e.size;
            }
        }
        if !buf.is_null() && buf_len > 0 {
            let name = e.name.as_bytes();
            if off + name.len() + 1 >= buf_len {
                break;
            }
            unsafe {
                ptr::copy_nonoverlapping(name.as_ptr(), buf.add(off) as *mut u8, name.len());
                *buf.add(off + name.len()) = 0;
            }
            off += name.len() + 1;
        }
        count += 1;
    }
    count
}

#[no_mangle]
pub extern "C" fn myntfs_read(
    vol: *mut MyNtfsVolume,
    path: *const c_char,
    offset: u64,
    buf: *mut std::ffi::c_void,
    length: usize,
) -> i64 {
    if vol.is_null() || path.is_null() || buf.is_null() {
        set_err("null argument");
        return -1;
    }
    let Ok(p) = (unsafe { CStr::from_ptr(path) }).to_str() else {
        set_err("invalid path");
        return -1;
    };
    let slice = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, length) };
    match unsafe { (*vol).vol.read(p, offset, slice) } {
        Ok(n) => n as i64,
        Err(e) => {
            set_err(e.to_string());
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn myntfs_stat_size(
    vol: *mut MyNtfsVolume,
    path: *const c_char,
    is_dir: *mut c_int,
) -> i64 {
    if vol.is_null() || path.is_null() {
        set_err("null argument");
        return -1;
    }
    let Ok(p) = (unsafe { CStr::from_ptr(path) }).to_str() else {
        set_err("invalid path");
        return -1;
    };
    match unsafe { (*vol).vol.stat(p) } {
        Ok(st) => {
            if !is_dir.is_null() {
                unsafe {
                    *is_dir = c_int::from(st.is_dir);
                }
            }
            st.size as i64
        }
        Err(e) => {
            set_err(e.to_string());
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn myntfs_mkdir(
    vol: *mut MyNtfsVolume,
    parent: *const c_char,
    name: *const c_char,
) -> c_int {
    ffi_write(vol, parent, name, |v, p, n| v.mkdir(p, n).map(|_| ()))
}

#[no_mangle]
pub extern "C" fn myntfs_create(
    vol: *mut MyNtfsVolume,
    parent: *const c_char,
    name: *const c_char,
) -> c_int {
    ffi_write(vol, parent, name, |v, p, n| v.create_file(p, n).map(|_| ()))
}

#[no_mangle]
pub extern "C" fn myntfs_write_contents(
    vol: *mut MyNtfsVolume,
    path: *const c_char,
    data: *const std::ffi::c_void,
    len: usize,
) -> i64 {
    if vol.is_null() || path.is_null() || data.is_null() {
        set_err("null argument");
        return -1;
    }
    let Ok(p) = (unsafe { CStr::from_ptr(path) }).to_str() else {
        set_err("invalid path");
        return -1;
    };
    let slice = unsafe { std::slice::from_raw_parts(data as *const u8, len) };
    match unsafe { (*vol).vol.write_contents(p, slice) } {
        Ok(n) => n as i64,
        Err(e) => {
            set_err(e.to_string());
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn myntfs_unlink(vol: *mut MyNtfsVolume, path: *const c_char) -> c_int {
    if vol.is_null() || path.is_null() {
        return -1;
    }
    let Ok(p) = (unsafe { CStr::from_ptr(path) }).to_str() else {
        return -1;
    };
    match unsafe { (*vol).vol.unlink(p) } {
        Ok(()) => 0,
        Err(e) => {
            set_err(e.to_string());
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn myntfs_rmdir(vol: *mut MyNtfsVolume, path: *const c_char) -> c_int {
    if vol.is_null() || path.is_null() {
        set_err("null argument");
        return -1;
    }
    let Ok(p) = (unsafe { CStr::from_ptr(path) }).to_str() else {
        set_err("invalid path");
        return -1;
    };
    match unsafe { (*vol).vol.rmdir(p) } {
        Ok(()) => 0,
        Err(e) => {
            set_err(e.to_string());
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn myntfs_remove(vol: *mut MyNtfsVolume, path: *const c_char) -> c_int {
    if vol.is_null() || path.is_null() {
        set_err("null argument");
        return -1;
    }
    let Ok(p) = (unsafe { CStr::from_ptr(path) }).to_str() else {
        set_err("invalid path");
        return -1;
    };
    match unsafe { (*vol).vol.remove(p) } {
        Ok(()) => 0,
        Err(e) => {
            set_err(e.to_string());
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn myntfs_claim(
    bsd: *const c_char,
    errbuf: *mut c_char,
    errbuf_len: usize,
) -> c_int {
    let Ok(name) = (unsafe { CStr::from_ptr(bsd) }).to_str() else {
        write_errbuf(errbuf, errbuf_len, "invalid bsd name");
        return -1;
    };
    match diskarb::unmount_and_claim(name) {
        Ok(()) => 0,
        Err(e) => {
            let msg = e.to_string();
            set_err(&msg);
            write_errbuf(errbuf, errbuf_len, &msg);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn myntfs_rename(
    vol: *mut MyNtfsVolume,
    old_path: *const c_char,
    new_basename: *const c_char,
) -> c_int {
    if vol.is_null() || old_path.is_null() || new_basename.is_null() {
        return -1;
    }
    let Ok(old) = (unsafe { CStr::from_ptr(old_path) }).to_str() else {
        return -1;
    };
    let Ok(new) = (unsafe { CStr::from_ptr(new_basename) }).to_str() else {
        return -1;
    };
    match unsafe { (*vol).vol.rename(old, new) } {
        Ok(()) => 0,
        Err(e) => {
            set_err(e.to_string());
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn myntfs_copy_out(
    vol: *mut MyNtfsVolume,
    ntfs_path: *const c_char,
    dest_host_path: *const c_char,
) -> i64 {
    if vol.is_null() || ntfs_path.is_null() || dest_host_path.is_null() {
        return -1;
    }
    let Ok(src) = (unsafe { CStr::from_ptr(ntfs_path) }).to_str() else {
        return -1;
    };
    let Ok(dest) = (unsafe { CStr::from_ptr(dest_host_path) }).to_str() else {
        return -1;
    };
    match copy_out(
        unsafe { &(*vol).vol },
        src,
        Path::new(dest),
        4 * 1024 * 1024,
    ) {
        Ok(s) => s.bytes as i64,
        Err(e) => {
            set_err(e.to_string());
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn myntfs_copy_in(
    vol: *mut MyNtfsVolume,
    src_host_path: *const c_char,
    parent: *const c_char,
    name: *const c_char,
) -> i64 {
    if vol.is_null() || src_host_path.is_null() || parent.is_null() || name.is_null() {
        return -1;
    }
    let Ok(src) = (unsafe { CStr::from_ptr(src_host_path) }).to_str() else {
        return -1;
    };
    let Ok(par) = (unsafe { CStr::from_ptr(parent) }).to_str() else {
        return -1;
    };
    let Ok(n) = (unsafe { CStr::from_ptr(name) }).to_str() else {
        return -1;
    };
    match copy_in(unsafe { &(*vol).vol }, Path::new(src), par, n) {
        Ok(s) => s.bytes as i64,
        Err(e) => {
            set_err(e.to_string());
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn myntfs_format(
    image_path: *const c_char,
    size_bytes: u64,
    label: *const c_char,
    errbuf: *mut c_char,
    errbuf_len: usize,
) -> c_int {
    let Ok(path) = (unsafe { CStr::from_ptr(image_path) }).to_str() else {
        write_errbuf(errbuf, errbuf_len, "invalid path");
        return -1;
    };
    let lbl = if label.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(label) }.to_str().ok()
    };
    match format_image(Path::new(path), size_bytes, lbl) {
        Ok(()) => 0,
        Err(e) => {
            let msg = e.to_string();
            set_err(&msg);
            write_errbuf(errbuf, errbuf_len, &msg);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn myntfs_list_disks(buf: *mut c_char, buf_len: usize) -> c_int {
    if buf.is_null() || buf_len == 0 {
        set_err("null buffer");
        return -1;
    }
    let disks = match diskarb::list_external_volumes() {
        Ok(d) => d,
        Err(e) => {
            set_err(e.to_string());
            return -1;
        }
    };
    let mut lines = Vec::new();
    for disk in &disks {
        let bsd = &disk.bsd;
        let info = diskarb::disk_summary(bsd).ok();
        let mount = info
            .as_ref()
            .and_then(|i| i.mount_point.clone())
            .filter(|m| m != "Not applicable" && m != "/")
            .unwrap_or_default();
        let name = info
            .as_ref()
            .and_then(|i| i.volume_name.clone())
            .filter(|n| n != "Not applicable" && !n.is_empty())
            .unwrap_or_else(|| {
                diskarb::friendly_volume_name(
                    None,
                    if mount.is_empty() { None } else { Some(&mount) },
                    bsd,
                )
            });
        let rd = diskarb::rdisk_path(bsd);
        // Never open /dev/rdisk during scan — it can block while FSKit holds the node.
        let raw_ok = "0";
        let fs = if disk.fs_kind.is_empty() {
            "Unknown"
        } else {
            disk.fs_kind.as_str()
        };
        let ntfs = if disk.is_ntfs { "1" } else { "0" };
        lines.push(format!("{bsd}|{name}|{mount}|{rd}|{raw_ok}|{fs}|{ntfs}"));
    }
    let joined = lines.join("\n");
    write_errbuf(buf, buf_len, &joined);
    lines.len() as c_int
}

#[no_mangle]
pub extern "C" fn myntfs_last_error() -> *const c_char {
    LAST_ERR.with(|e| e.borrow().as_ptr())
}

fn ffi_write<F>(vol: *mut MyNtfsVolume, parent: *const c_char, name: *const c_char, f: F) -> c_int
where
    F: FnOnce(&Volume, &str, &str) -> CoreResult<()>,
{
    if vol.is_null() || parent.is_null() || name.is_null() {
        set_err("null argument");
        return -1;
    }
    let Ok(p) = (unsafe { CStr::from_ptr(parent) }).to_str() else {
        set_err("invalid parent");
        return -1;
    };
    let Ok(n) = (unsafe { CStr::from_ptr(name) }).to_str() else {
        set_err("invalid name");
        return -1;
    };
    match f(unsafe { &(*vol).vol }, p, n) {
        Ok(()) => 0,
        Err(e) => {
            set_err(e.to_string());
            -1
        }
    }
}
