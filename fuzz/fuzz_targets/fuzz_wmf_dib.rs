#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../support/wmf_dib.rs"]
mod harness;

fuzz_target!(|data: &[u8]| harness::exercise(data));
