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
//! - [`page`]: the in-band page header and the block free list (pop, push, lazy extend).

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

#[cfg(test)]
extern crate std;

pub mod backend;
pub mod bins;
pub mod page;
