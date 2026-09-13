#![no_main]

use libfuzzer_sys::fuzz_target;
use ntfs_core::decompress_lznt1;

fuzz_target!(|data: &[u8]| {
    let _ = decompress_lznt1(data);
});
