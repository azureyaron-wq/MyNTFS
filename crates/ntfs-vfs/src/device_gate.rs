//! Shared refuse rules for block-device format/write — used by CLI, FFI, and FUSE.

use std::path::Path;

use ntfs_core::{Error, Result};

/// Explicit confirmation that the caller accepts mutating a block device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeviceWriteConfirm {
    pub allow_block_device: bool,
}

/// True when `path` resolves to a block or character device (follows symlinks).
pub fn is_block_device(path: &Path) -> Result<bool> {
    match std::fs::metadata(path) {
        Ok(meta) => device_meta(&meta),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::Io(e)),
    }
}

fn device_meta(meta: &std::fs::Metadata) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        return Ok(meta.file_type().is_block_device() || meta.file_type().is_char_device());
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        Ok(false)
    }
}

pub fn refuse_block_device_format(path: &Path) -> Result<()> {
    if is_block_device(path)? {
        return Err(Error::WriteDenied(
            "refusing to format block device; use a disk image file",
        ));
    }
    Ok(())
}

pub fn refuse_block_device_write(path: &Path, confirm: DeviceWriteConfirm) -> Result<()> {
    if is_block_device(path)? && !confirm.allow_block_device {
        return Err(Error::WriteDenied(
            "refusing write on block device; use disk images or pass explicit device-write confirmation",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_null_is_block_device() {
        if Path::new("/dev/null").exists() {
            assert!(is_block_device(Path::new("/dev/null")).unwrap());
        }
    }
}
