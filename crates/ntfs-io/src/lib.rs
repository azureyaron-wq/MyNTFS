use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ntfs_core::{BlockDevice, Error, Result};

/// File-backed image or raw character device. Regular files use direct
/// `pread`/`pwrite`; macOS raw devices coalesce into logical-block-aligned I/O.
pub struct FileDevice {
    file: std::fs::File,
    size: u64,
    writable: bool,
    path: PathBuf,
    /// When > 0, offsets and lengths must be multiples of this (macOS `/dev/rdisk*`).
    logical_block_size: u32,
}

impl FileDevice {
    pub fn open_ro(path: impl AsRef<Path>) -> Result<Self> {
        let p = path.as_ref();
        let file = std::fs::File::open(p)?;
        Self::from_file(file, false, p)
    }

    pub fn open_rw(path: impl AsRef<Path>) -> Result<Self> {
        let p = path.as_ref();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(p)?;
        Self::from_file(file, true, p)
    }

    /// Wrap an already-open file. Never reopens by pathname.
    pub fn from_file(file: std::fs::File, writable: bool, path: impl AsRef<Path>) -> Result<Self> {
        let size = device_file_size(&file);
        let logical_block_size = detect_logical_block_size(&file, size);
        Ok(Self {
            file,
            size,
            writable,
            path: path.as_ref().to_path_buf(),
            logical_block_size,
        })
    }

    /// Force logical-block alignment (used by tests simulating macOS raw devices).
    #[doc(hidden)]
    pub fn from_file_with_logical_block_size(
        file: std::fs::File,
        writable: bool,
        path: impl AsRef<Path>,
        logical_block_size: u32,
    ) -> Result<Self> {
        let size = device_file_size(&file);
        Ok(Self {
            file,
            size,
            writable,
            path: path.as_ref().to_path_buf(),
            logical_block_size,
        })
    }

    /// Duplicate `fd` and take ownership of the copy. The caller's descriptor stays open.
    #[cfg(unix)]
    pub fn from_raw_fd(fd: i32, writable: bool, path: impl AsRef<Path>) -> Result<Self> {
        let file = dup_owned_file(fd, writable)?;
        let mut probe = [0u8; 512];
        BlockDevice::read_at(&file, 0, &mut probe)?;
        Self::from_file(file, writable, path)
    }

    pub fn create(path: impl AsRef<Path>, size: u64) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path.as_ref())?;
        file.set_len(size)?;
        Ok(Self {
            file,
            size,
            writable: true,
            path: path.as_ref().to_path_buf(),
            logical_block_size: 0,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn inner(&self) -> &std::fs::File {
        &self.file
    }
}

impl BlockDevice for FileDevice {
    fn size(&self) -> u64 {
        self.size
    }

    fn block_size(&self) -> u32 {
        if self.logical_block_size > 0 {
            self.logical_block_size
        } else {
            4096
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if self.logical_block_size == 0 {
            return self.file.read_at(offset, buf);
        }
        aligned_read_at(&self.file, self.logical_block_size, offset, buf)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if !self.writable {
            return Err(Error::WriteDenied("file device is read-only"));
        }
        if self.logical_block_size == 0 {
            return self.file.write_at(offset, buf);
        }
        aligned_write_at(&self.file, self.logical_block_size, offset, buf)
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn flush(&self) -> Result<()> {
        self.file.sync_data().map_err(Error::from)
    }
}

/// Raw character device (`/dev/rdiskNsY` on macOS, `/dev/sdX` on Linux).
pub struct RawDevice {
    inner: FileDevice,
}

impl RawDevice {
    pub fn open(path: impl AsRef<Path>, writable: bool) -> Result<Self> {
        let inner = if writable {
            FileDevice::open_rw(path.as_ref()).map_err(|e| map_raw_open_err(path.as_ref(), e, true))?
        } else {
            FileDevice::open_ro(path.as_ref()).map_err(|e| map_raw_open_err(path.as_ref(), e, false))?
        };
        Ok(Self { inner })
    }

    pub fn path(&self) -> &Path {
        self.inner.path()
    }
}

fn map_raw_open_err(path: &Path, err: Error, writable: bool) -> Error {
    #[cfg(target_os = "macos")]
    if matches!(err, Error::Io(ref e) if e.kind() == std::io::ErrorKind::PermissionDenied) {
        let hint = diskarb::macos_raw_access_hint();
        let mode = if writable { "read-write" } else { "read-only" };
        return Error::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "cannot open {} ({mode}): {hint}. \
                 For USB testing use read-only commands only; do not write to user data.",
                path.display()
            ),
        ));
    }
    err
}

impl BlockDevice for RawDevice {
    fn size(&self) -> u64 {
        if self.inner.size > 0 {
            return self.inner.size;
        }
        // Character devices report size 0 via stat; ioctl DKIOCGETBLOCKCOUNT.
        #[cfg(target_os = "macos")]
        {
            macos_device_size(&self.inner.file).unwrap_or(0)
        }
        #[cfg(not(target_os = "macos"))]
        {
            linux_device_size(&self.inner.file).unwrap_or(0)
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_at(offset, buf)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        self.inner.write_at(offset, buf)
    }

    fn writable(&self) -> bool {
        self.inner.writable()
    }

    fn flush(&self) -> Result<()> {
        self.inner.flush()
    }

    fn block_size(&self) -> u32 {
        4096
    }
}

#[cfg(unix)]
pub fn dup_owned_file(fd: i32, writable: bool) -> Result<std::fs::File> {
    use std::os::unix::io::FromRawFd;
    if fd < 0 {
        return Err(Error::Io(std::io::Error::from_raw_os_error(libc::EBADF)));
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    let acc = flags & libc::O_ACCMODE;
    let fd_writable = acc == libc::O_RDWR || acc == libc::O_WRONLY;
    if writable && !fd_writable {
        return Err(Error::WriteDenied("file descriptor is not writable"));
    }
    let duped = unsafe { libc::dup(fd) };
    if duped < 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(unsafe { std::fs::File::from_raw_fd(duped) })
}

fn detect_logical_block_size(file: &std::fs::File, meta_len: u64) -> u32 {
    if meta_len > 0 {
        return 0;
    }
    #[cfg(target_os = "macos")]
    {
        macos_device_logical_block_size(file).unwrap_or(0)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = file;
        0
    }
}

#[cfg(target_os = "macos")]
fn macos_device_logical_block_size(file: &std::fs::File) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x40046418;
    let fd = file.as_raw_fd();
    let mut bs: u32 = 0;
    unsafe {
        if libc::ioctl(fd, DKIOCGETBLOCKSIZE, &mut bs) != 0 || bs == 0 {
            return None;
        }
    }
    Some(bs)
}

/// Bounce buffer whose payload pointer is aligned to `align` bytes.
/// macOS `/dev/rdisk*` rejects `pread`/`pwrite` unless the user buffer
/// address is a multiple of the logical block size, not only offset/length.
struct AlignedBounce {
    alloc: Vec<u8>,
    offset: usize,
    len: usize,
}

impl AlignedBounce {
    fn new(len: usize, align: usize) -> Self {
        let align = align.max(1);
        let alloc = vec![0u8; len.saturating_add(align)];
        let ptr = alloc.as_ptr() as usize;
        let offset = (align - (ptr % align)) % align;
        debug_assert!(offset + len <= alloc.len());
        debug_assert_eq!((alloc.as_ptr() as usize + offset) % align, 0);
        Self { alloc, offset, len }
    }

    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.alloc[self.offset..self.offset + self.len]
    }
}

fn ptr_aligned(ptr: *const u8, align: usize) -> bool {
    align == 0 || (ptr as usize) % align == 0
}

fn aligned_read_at(
    file: &std::fs::File,
    logical_block_size: u32,
    offset: u64,
    buf: &mut [u8],
) -> Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    let align = logical_block_size as usize;
    let bs = logical_block_size as u64;
    if offset % bs == 0
        && (buf.len() as u64) % bs == 0
        && ptr_aligned(buf.as_ptr(), align)
    {
        return file.read_at(offset, buf);
    }
    let end = offset.saturating_add(buf.len() as u64);
    let block_start = (offset / bs) * bs;
    let block_end = ((end + bs - 1) / bs) * bs;
    let bounce_len = block_end.saturating_sub(block_start) as usize;
    let mut bounce = AlignedBounce::new(bounce_len, align);
    file.read_at(block_start, bounce.as_mut())?;
    let skip = (offset - block_start) as usize;
    buf.copy_from_slice(&bounce.as_mut()[skip..skip + buf.len()]);
    Ok(())
}

fn aligned_write_at(
    file: &std::fs::File,
    logical_block_size: u32,
    offset: u64,
    buf: &[u8],
) -> Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    let align = logical_block_size as usize;
    let bs = logical_block_size as u64;
    if offset % bs == 0
        && (buf.len() as u64) % bs == 0
        && ptr_aligned(buf.as_ptr(), align)
    {
        return file.write_at(offset, buf);
    }

    let end = offset.saturating_add(buf.len() as u64);
    let block_start = (offset / bs) * bs;
    let block_end = ((end + bs - 1) / bs) * bs;
    let bounce_len = block_end.saturating_sub(block_start) as usize;
    let mut bounce = AlignedBounce::new(bounce_len, align);
    let dest = bounce.as_mut();

    let head_partial = offset != block_start;
    let tail_partial = end != block_end;
    if head_partial {
        file.read_at(block_start, &mut dest[..align])?;
    }
    if tail_partial && bounce_len > align {
        let tail_off = bounce_len - align;
        file.read_at(block_start + tail_off as u64, &mut dest[tail_off..])?;
    } else if tail_partial && !head_partial {
        file.read_at(block_start, dest)?;
    }

    let skip = (offset - block_start) as usize;
    dest[skip..skip + buf.len()].copy_from_slice(buf);
    file.write_at(block_start, dest)
}

fn device_file_size(file: &std::fs::File) -> u64 {
    let meta_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if meta_len > 0 {
        return meta_len;
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(n) = macos_device_size(file) {
            return n;
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(n) = linux_device_size(file) {
            return n;
        }
    }
    0
}

#[cfg(target_os = "macos")]
fn macos_device_size(file: &std::fs::File) -> Option<u64> {
    use std::os::unix::io::AsRawFd;
    const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x40086419;
    const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x40046418;
    let fd = file.as_raw_fd();
    let mut count: u64 = 0;
    let mut bs: u32 = 0;
    unsafe {
        if libc::ioctl(fd, DKIOCGETBLOCKCOUNT, &mut count) != 0 {
            return None;
        }
        if libc::ioctl(fd, DKIOCGETBLOCKSIZE, &mut bs) != 0 {
            return None;
        }
    }
    Some(count.saturating_mul(bs as u64))
}

#[cfg(not(target_os = "macos"))]
fn linux_device_size(file: &std::fs::File) -> Option<u64> {
    use std::os::unix::io::AsRawFd;
    const BLKGETSIZE64: libc::c_ulong = 0x80081272;
    let fd = file.as_raw_fd();
    let mut size: u64 = 0;
    unsafe {
        if libc::ioctl(fd, BLKGETSIZE64, &mut size) != 0 {
            return None;
        }
    }
    Some(size)
}

/// Bounded LRU of metadata pages. Hard cap in bytes keeps idle memory flat.
pub struct MetaCache {
    cap_bytes: usize,
    inner: Mutex<lru_impl::Lru>,
}

impl MetaCache {
    pub fn new(cap_bytes: usize) -> Self {
        Self {
            cap_bytes: cap_bytes.max(64 * 1024),
            inner: Mutex::new(lru_impl::Lru::new()),
        }
    }

    pub fn get(&self, offset: u64) -> Option<Vec<u8>> {
        self.inner.lock().ok()?.get(offset)
    }

    pub fn put(&self, offset: u64, data: Vec<u8>) {
        if let Ok(mut g) = self.inner.lock() {
            g.put(offset, data, self.cap_bytes);
        }
    }

    pub fn invalidate(&self, offset: u64) {
        if let Ok(mut g) = self.inner.lock() {
            g.remove(offset);
        }
    }

    pub fn clear(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.clear();
        }
    }
}

mod lru_impl {
    use std::collections::{HashMap, VecDeque};

    pub struct Lru {
        map: HashMap<u64, Vec<u8>>,
        order: VecDeque<u64>,
        bytes: usize,
    }

    impl Lru {
        pub fn new() -> Self {
            Self {
                map: HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
            }
        }

        pub fn get(&mut self, k: u64) -> Option<Vec<u8>> {
            if let Some(v) = self.map.get(&k) {
                if let Some(i) = self.order.iter().position(|x| *x == k) {
                    self.order.remove(i);
                    self.order.push_back(k);
                }
                return Some(v.clone());
            }
            None
        }

        pub fn put(&mut self, k: u64, v: Vec<u8>, cap: usize) {
            self.remove(k);
            self.bytes += v.len();
            self.map.insert(k, v);
            self.order.push_back(k);
            while self.bytes > cap {
                if let Some(old) = self.order.pop_front() {
                    if let Some(d) = self.map.remove(&old) {
                        self.bytes = self.bytes.saturating_sub(d.len());
                    }
                } else {
                    break;
                }
            }
        }

        pub fn remove(&mut self, k: u64) {
            if let Some(d) = self.map.remove(&k) {
                self.bytes = self.bytes.saturating_sub(d.len());
                self.order.retain(|x| *x != k);
            }
        }

        pub fn clear(&mut self) {
            self.map.clear();
            self.order.clear();
            self.bytes = 0;
        }
    }
}

pub mod diskarb;

#[cfg(test)]
mod alignment_tests {
    use std::fs::{self, OpenOptions};
    use std::path::PathBuf;

    use ntfs_core::BlockDevice;

    use super::{aligned_read_at, aligned_write_at, AlignedBounce, FileDevice};

    const BS: u32 = 512;

    fn pattern_file() -> (PathBuf, std::fs::File) {
        let path = std::env::temp_dir().join(format!(
            "myntfs-align-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let mut data = vec![0u8; BS as usize * 4];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        fs::write(&path, &data).unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        (path, file)
    }

    fn cleanup(path: &PathBuf) {
        let _ = fs::remove_file(path);
    }

    #[test]
    fn one_byte_unaligned_read() {
        let (path, file) = pattern_file();
        let mut buf = [0u8; 1];
        aligned_read_at(&file, BS, 1, &mut buf).unwrap();
        assert_eq!(buf[0], 1);
        cleanup(&path);
    }

    #[test]
    fn one_byte_unaligned_write_preserves_neighbors() {
        let (path, file) = pattern_file();
        let mut before = [0u8; BS as usize];
        BlockDevice::read_at(&file, 0, &mut before).unwrap();
        aligned_write_at(&file, BS, 1, &[0xAB]).unwrap();
        let mut after = [0u8; BS as usize];
        BlockDevice::read_at(&file, 0, &mut after).unwrap();
        assert_eq!(after[0], before[0]);
        assert_eq!(after[1], 0xAB);
        assert_eq!(&after[2..], &before[2..]);
        cleanup(&path);
    }

    #[test]
    fn cross_sector_write_rmw() {
        let (path, file) = pattern_file();
        let mut before = [0u8; BS as usize];
        BlockDevice::read_at(&file, 0, &mut before).unwrap();
        let payload = vec![0xCD; 600];
        aligned_write_at(&file, BS, 400, &payload).unwrap();
        assert_eq!(before[399], (399 % 251) as u8);
        let mut got = vec![0u8; 600];
        BlockDevice::read_at(&file, 400, &mut got).unwrap();
        assert!(got.iter().all(|&b| b == 0xCD));
        let mut head = [0u8; 400];
        BlockDevice::read_at(&file, 0, &mut head).unwrap();
        assert_eq!(&head[..399], &before[..399]);
        assert_eq!(head[399], before[399]);
        cleanup(&path);
    }

    #[test]
    fn file_device_forced_alignment_roundtrip() {
        let (path, file) = pattern_file();
        let dev = FileDevice::from_file_with_logical_block_size(file, true, &path, BS).unwrap();
        let mut one = [0u8; 1];
        dev.read_at(777, &mut one).unwrap();
        dev.write_at(777, &[0x42]).unwrap();
        let mut check = [0u8; 1];
        dev.read_at(777, &mut check).unwrap();
        assert_eq!(check, [0x42]);
        cleanup(&path);
    }

    #[test]
    fn aligned_middle_write_uses_direct_io() {
        let (path, file) = pattern_file();
        let chunk = vec![0xEE; BS as usize * 2];
        aligned_write_at(&file, BS, BS as u64, &chunk).unwrap();
        let mut got = vec![0u8; BS as usize * 2];
        BlockDevice::read_at(&file, BS as u64, &mut got).unwrap();
        assert!(got.iter().all(|&b| b == 0xEE));
        cleanup(&path);
    }

    #[test]
    fn bounce_buffer_is_block_aligned() {
        for len in [1usize, 511, 512, 513, 1024, 4097] {
            let mut bounce = AlignedBounce::new(len, BS as usize);
            let ptr = bounce.as_mut().as_ptr() as usize;
            assert_eq!(ptr % BS as usize, 0, "len={len} ptr={ptr:#x}");
            assert_eq!(bounce.as_mut().len(), len);
        }
    }

    #[test]
    fn unaligned_caller_buffer_roundtrip() {
        let (path, file) = pattern_file();
        let mut storage = vec![0u8; BS as usize + 32];
        let mis = (BS as usize - (storage.as_ptr() as usize % BS as usize) + 1) % BS as usize;
        let mis = if mis == 0 { 1 } else { mis };
        {
            let buf = &mut storage[mis..mis + 1];
            assert_ne!(buf.as_ptr() as usize % BS as usize, 0);
            aligned_read_at(&file, BS, 1, buf).unwrap();
            assert_eq!(buf[0], 1);
        }

        storage[mis] = 0xCD;
        {
            let payload = &storage[mis..mis + 1];
            assert_ne!(payload.as_ptr() as usize % BS as usize, 0);
            aligned_write_at(&file, BS, 5, payload).unwrap();
        }

        let mut check_store = vec![0u8; BS as usize + 16];
        let check_off =
            (BS as usize - (check_store.as_ptr() as usize % BS as usize) + 7) % BS as usize;
        let check_off = if check_off == 0 { 1 } else { check_off };
        let check = &mut check_store[check_off..check_off + 1];
        aligned_read_at(&file, BS, 5, check).unwrap();
        assert_eq!(check[0], 0xCD);
        cleanup(&path);
    }
}
