//! The model tester driven by fuzzer bytes, against `std::alloc::System`.
//!
//! This target exists to prove the model itself: a correct allocator must never make it fail,
//! whatever the operation stream. `model_heap` is the same stream against the crate's heap.
#![no_main]

use libfuzzer_sys::fuzz_target;
use wasmalloc::testing::model::{self, Profile};
use wasmalloc::testing::rng::ByteSource;

/// Live bytes per run. Keeps each run fast and the process far below libFuzzer's RSS limit.
const MAX_LIVE: usize = 16 << 20;

fuzz_target!(|data: &[u8]| {
    let Some((&which, rest)) = data.split_first() else {
        return;
    };
    let profiles = Profile::all();
    let mut profile = profiles[which as usize % profiles.len()];
    profile.max_live_bytes = MAX_LIVE;
    let mut source = ByteSource::new(rest);
    let mut alloc = std::alloc::System;
    if let Err(failure) = model::run_with(&mut alloc, &mut source, usize::MAX, profile) {
        panic!("{failure}");
    }
});
