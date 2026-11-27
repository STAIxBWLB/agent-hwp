#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../support/hwpx.rs"]
mod harness;

fuzz_target!(|data: &[u8]| harness::exercise(data));
