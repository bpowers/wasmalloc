//! Every profile against the crate's own heap over a simulated linear memory.
//!
//! Host only: the simulated memory is a lazily committed 1 GiB host region, which a wasm32 test
//! process cannot provide. `tests/global_wasm.rs` covers the heap on wasm32 end to end.
#![cfg(not(target_arch = "wasm32"))]

use wasmalloc::bins::SLICE_SIZE;
use wasmalloc::slices::GrowPolicy;
use wasmalloc::testing::model::{self, Profile};
use wasmalloc::testing::sim::SimHeap;

/// Four times the largest profile's live-byte cap: room for page granularity, retirement and
/// growth steps without ever giving the heap a legitimate reason to refuse a request.
const REGION_SLICES: usize = 16 * 1024;

/// Start conditions to vary per seed: how much initial memory there is and where the heap base
/// falls in it (mid-slice, slice-aligned, and past a whole slice).
const STARTS: [(usize, usize); 3] = [(4, 100), (1, 0), (64, SLICE_SIZE + 8)];

fn heap(initial_slices: usize, heap_base_offset: usize) -> SimHeap {
    let mut h = SimHeap::new(REGION_SLICES, initial_slices, heap_base_offset);
    // Small growth steps so a run crosses many memory.grow boundaries and non-trivial slice
    // map states, not just one big initial grow.
    h.set_grow_policy(GrowPolicy {
        min_grow: 2,
        max_grow: 256,
        ..GrowPolicy::DEFAULT
    });
    h
}

fn exercise(profile: Profile, ops: usize) {
    for (i, (initial, offset)) in STARTS.iter().enumerate() {
        let seed = i as u64 + 1;
        let mut h = heap(*initial, *offset);
        let stats = model::check(&mut h, seed, ops, profile);
        assert_eq!(stats.ops, ops, "{}: {stats:?}", profile.name);
        assert_eq!(
            stats.deallocs,
            stats.allocs + stats.zeroed_allocs,
            "{}: every block is freed exactly once: {stats:?}",
            profile.name
        );
        let footprint = stats
            .peak_footprint_bytes
            .expect("the heap reports its footprint");
        // A loose bound that only a gross leak (pages never reused) could break; the exact
        // ratio is workload dependent and is printed for the record.
        assert!(
            footprint <= 4 * stats.peak_live_bytes + 128 * 1024 * 1024,
            "{}: footprint {footprint} for peak live {}: {stats:?}",
            profile.name,
            stats.peak_live_bytes
        );
        eprintln!(
            "{} seed {seed} start {:?}: fragmentation {:.2} ({stats:?})",
            profile.name,
            (initial, offset),
            stats.fragmentation().unwrap()
        );
    }
}

#[test]
fn small_churn() {
    exercise(Profile::SMALL_CHURN, 50_000);
}

#[test]
fn mixed() {
    exercise(Profile::MIXED, 30_000);
}

#[test]
fn large_heavy() {
    exercise(Profile::LARGE_HEAVY, 3_000);
}

#[test]
fn align_heavy() {
    exercise(Profile::ALIGN_HEAVY, 30_000);
}

#[test]
fn lifo_batches() {
    exercise(Profile::LIFO_BATCHES, 50_000);
}

#[test]
fn fifo_batches() {
    exercise(Profile::FIFO_BATCHES, 50_000);
}
