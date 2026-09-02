//! The model tester driven by fuzzer bytes, against the crate's heap over a simulated memory.
//!
//! The first byte picks a profile, the rest is the operation stream (see
//! `wasmalloc::testing::model`). Every run starts from a fresh heap, so a crashing input
//! reproduces on its own. The host region under the simulated memory is reused across runs:
//! under AddressSanitizer a fresh 256 MiB allocation per run costs more than the run itself
//! (the sanitizer poisons and unpoisons 32 MiB of shadow memory each time), and the heap never
//! reads a slice it has not grown into, which the simulated memory zero-fills.
#![no_main]

use libfuzzer_sys::fuzz_target;
use wasmalloc::backend::SimMemory;
use wasmalloc::backend::testing::HostRegion;
use wasmalloc::heap::Heap;
use wasmalloc::slices::GrowPolicy;
use wasmalloc::testing::model::{self, Profile};
use wasmalloc::testing::rng::ByteSource;

/// Live bytes per run. Keeps each run fast and the process far below libFuzzer's RSS limit.
const MAX_LIVE: usize = 16 << 20;
/// 256 MiB region: sixteen times the live cap, committed lazily by the OS.
const REGION_SLICES: usize = 4096;

thread_local! {
    static REGION: HostRegion = HostRegion::new(REGION_SLICES);
}

fuzz_target!(|data: &[u8]| {
    let Some((&which, rest)) = data.split_first() else {
        return;
    };
    let profiles = Profile::all();
    let mut profile = profiles[which as usize % profiles.len()];
    profile.max_live_bytes = MAX_LIVE;
    let mut source = ByteSource::new(rest);
    REGION.with(|region| {
        // SAFETY: the previous run's heap, and with it the previous SimMemory, was dropped when
        // its closure returned; this one is dropped before the closure returns too, so exactly
        // one memory is live over the region at a time and none outlives it.
        let mem = unsafe { region.simulate(4, 100) };
        let mut heap = Heap::<SimMemory, 1024>::new(mem);
        // Small growth steps so short runs still cross memory.grow boundaries.
        heap.set_grow_policy(GrowPolicy {
            min_grow: 2,
            max_grow: 64,
            ..GrowPolicy::DEFAULT
        });
        if let Err(failure) = model::run_with(&mut heap, &mut source, usize::MAX, profile) {
            panic!("{failure}");
        }
    });
});
