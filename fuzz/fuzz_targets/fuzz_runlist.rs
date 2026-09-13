#![no_main]

use libfuzzer_sys::fuzz_target;
use ntfs_core::decode_runs;

fuzz_target!(|data: &[u8]| {
    let _ = decode_runs(data);
});
