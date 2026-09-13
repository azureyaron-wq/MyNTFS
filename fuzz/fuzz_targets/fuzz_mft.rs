#![no_main]

use libfuzzer_sys::fuzz_target;
use ntfs_core::MftRecord;

fuzz_target!(|data: &[u8]| {
    let _ = MftRecord::parse(data);
});
