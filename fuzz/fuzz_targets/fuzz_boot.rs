#![no_main]

use libfuzzer_sys::fuzz_target;
use ntfs_core::BootSector;

fuzz_target!(|data: &[u8]| {
    let _ = BootSector::parse(data);
});
