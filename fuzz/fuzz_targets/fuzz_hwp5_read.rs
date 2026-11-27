#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../support/hwp5.rs"]
mod harness;

fuzz_target!(|data: &[u8]| harness::exercise(data));
