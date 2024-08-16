#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = hardy_cbor::decode::parse::<hardy_bpv7::prelude::Eid>(data);
});
