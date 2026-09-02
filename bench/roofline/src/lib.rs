//! Allocation-cost roofline harness for single-threaded wasm32.
//!
//! Built as a `cdylib` for `wasm32-unknown-unknown` this exports plain C ABI
//! functions that a JS driver times; built as an `rlib` it backs the
//! `roofline-wasi` binary that times itself with `std::time::Instant`.
//! The allocator under test is chosen with a cargo feature (see Cargo.toml).

pub mod alloc;
pub mod rng;
pub mod workloads;

use workloads as w;

/// Empty function: measures the host-to-wasm call overhead so it can be subtracted.
#[no_mangle]
pub extern "C" fn noop() {}

/// Rewind allocators that support it (the floors). Only valid when nothing is live.
#[no_mangle]
pub extern "C" fn reset() {
    alloc::reset();
}

#[no_mangle]
pub extern "C" fn variant_name_ptr() -> *const u8 {
    alloc::NAME.as_ptr()
}

#[no_mangle]
pub extern "C" fn variant_name_len() -> u32 {
    alloc::NAME.len() as u32
}

#[no_mangle]
pub extern "C" fn alloc_free_32(iters: u32) -> u32 {
    w::alloc_free_fixed(iters as usize, 32)
}

#[no_mangle]
pub extern "C" fn batch_lifo_32(rounds: u32) -> u32 {
    w::batch_alloc_free(rounds as usize, 32, true)
}

#[no_mangle]
pub extern "C" fn batch_fifo_32(rounds: u32) -> u32 {
    w::batch_alloc_free(rounds as usize, 32, false)
}

#[no_mangle]
pub extern "C" fn churn_init() -> u32 {
    w::churn_init()
}

#[no_mangle]
pub extern "C" fn churn(steps: u32) -> u32 {
    w::churn(steps as usize)
}

#[no_mangle]
pub extern "C" fn churn_fini() -> u32 {
    w::churn_fini()
}

#[no_mangle]
pub extern "C" fn vec_push_growth(rounds: u32) -> u32 {
    w::vec_push_growth(rounds as usize)
}

#[no_mangle]
pub extern "C" fn realloc_doubling(rounds: u32) -> u32 {
    w::realloc_doubling(rounds as usize)
}

#[no_mangle]
pub extern "C" fn large_alloc_free(iters: u32) -> u32 {
    w::large_alloc_free(iters as usize)
}

#[no_mangle]
pub extern "C" fn memory_grow_touch(iters: u32) -> u32 {
    w::memory_grow_touch(iters as usize)
}

#[no_mangle]
pub extern "C" fn memory_grow_only(iters: u32) -> u32 {
    w::memory_grow_only(iters as usize)
}
