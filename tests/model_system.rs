//! Every profile against `std::alloc::System`.
//!
//! The model must accept a correct allocator on every step, so a failure here is a bug in the
//! tester, not in an allocator. The counters are checked too: every block the model created must
//! have been freed exactly once by the end of the run.

use std::alloc::System;

use wasmalloc::testing::model::{self, Profile};

/// wasm32 runs single-threaded under wasmtime, so it gets a fifth of the operations.
fn ops(host: usize) -> usize {
    if cfg!(target_arch = "wasm32") {
        host / 5
    } else {
        host
    }
}

fn exercise(profile: Profile, host_ops: usize) {
    let ops = ops(host_ops);
    for seed in 1..=2 {
        let stats = model::check(&mut System, seed, ops, profile);
        assert_eq!(stats.ops, ops, "{}: {stats:?}", profile.name);
        assert!(
            stats.allocs > 0 && stats.zeroed_allocs > 0,
            "{}: {stats:?}",
            profile.name
        );
        assert_eq!(
            stats.deallocs,
            stats.allocs + stats.zeroed_allocs,
            "{}: every block is freed exactly once: {stats:?}",
            profile.name
        );
        assert!(stats.peak_live_bytes <= profile.max_live_bytes, "{stats:?}");
        assert_eq!(
            stats.peak_footprint_bytes, None,
            "System reports no footprint"
        );
        eprintln!("{} seed {seed}: {stats:?}", profile.name);
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
