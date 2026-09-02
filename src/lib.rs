//! wasmalloc: a mimalloc-v3-style `#[global_allocator]` for single-threaded wasm32.
//!
//! The design document lives in `docs/design/`. In short: size-class bins, per-page intrusive
//! free lists, lazy page extension, page retirement, and 64 KiB slices over `memory.grow`.
//! Because Rust's `GlobalAlloc` hands the `Layout` back at `dealloc`, the page kind and page
//! header address are pure functions of the Layout, so no hot path consults a page map.
//!
//! Module map (each module's doc comment explains its contract):
//! - [`bins`]: size classes and page geometry, pure arithmetic.
//! - [`backend`]: the [`backend::Memory`] trait over linear memory (wasm or simulated).
//! - [`slices`]: the free-slice bitmap and the memory growth policy.
//! - [`page`]: the in-band page header and block free-list operations.
//! - [`heap`]: bin queues, the direct table, page lifecycle, and alloc/dealloc/realloc.
//! - [`global`] (wasm32 only): [`WasmAlloc`], the `#[global_allocator]` over a static heap.
//! - [`testing`] (feature `testing`): the model-based tester every allocator change is judged
//!   against, plus the simulated-memory helpers integration tests and fuzz targets share.
//!
//! Threads are not supported: the crate assumes wasm32 without the `atomics` target feature and
//! refuses to build with it.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

#[cfg(any(test, feature = "testing"))]
extern crate std;

pub mod backend;
pub mod bins;
#[cfg(target_arch = "wasm32")]
pub mod global;
pub mod heap;
pub mod page;
pub mod slices;
#[cfg(feature = "testing")]
pub mod testing;

#[cfg(target_arch = "wasm32")]
pub use global::WasmAlloc;

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
compile_error!("wasmalloc is single-threaded and does not support the wasm atomics feature");
