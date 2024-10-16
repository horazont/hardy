#![no_main]

use hardy_bpv7::prelude::*;
use hardy_cbor::decode::*;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match parse::<(ValidBundle, bool, usize)>(data) {
        Ok((ValidBundle::Valid(mut bundle), true, _)) => {
            let new_data = bundle.canonicalise(data).unwrap();

            assert_eq!(data, new_data.as_ref());
        }
        Ok((ValidBundle::Valid(mut bundle), false, _)) => {
            let data = bundle.canonicalise(data).unwrap();

            let Ok((ValidBundle::Valid(_), true, _)) = parse::<(ValidBundle, bool, usize)>(&data)
            else {
                panic!("Rewrite borked");
            };
        }
        _ => {}
    }
});

// llvm-cov show --format=html  -instr-profile ./fuzz/coverage/bundle/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/bundle -o ./fuzz/coverage/bundle/ -ignore-filename-regex='/.cargo/|rustc/'
