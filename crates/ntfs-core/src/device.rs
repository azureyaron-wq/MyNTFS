use crate::error::{Error, Result};

/// Byte-addressable block device. Implementors perform the only syscalls
/// in the stack. Offset and length must be respected exactly.
pub trait BlockDevice {
    /// Total size in bytes.
    fn size(&self) -> u64;

    /// Preferred I/O alignment (physical sector or 4096).
    fn block_size(&self) -> u32 {
        4096
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let _ = (offset, buf);
        Err(Error::WriteDenied("device is read-only"))
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }

    fn writable(&self) -> bool {
        false
    }
}

impl BlockDevice for std::fs::File {
    fn size(&self) -> u64 {
        self.metadata().map(|m| m.len()).unwrap_or(0)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            let mut filled = 0;
            while filled < buf.len() {
                let n = FileExt::read_at(self, &mut buf[filled..], offset + filled as u64)?;
                if n == 0 {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "short read",
                    )));
                }
                filled += n;
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (offset, buf);
            Err(Error::Unsupported("pread"))
        }
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            let mut filled = 0;
            while filled < buf.len() {
                let n = FileExt::write_at(self, &buf[filled..], offset + filled as u64)?;
                if n == 0 {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "short write",
                    )));
                }
                filled += n;
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (offset, buf);
            Err(Error::Unsupported("pwrite"))
        }
    }

    fn writable(&self) -> bool {
        true
    }
}
