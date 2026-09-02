//! Test infrastructure shared by unit tests, integration tests and fuzz targets.
//!
//! Only compiled with the `testing` feature, which pulls in `std`. Nothing here may be reachable
//! from a `#[global_allocator]` build.
//!
//! - [`rng`]: dependency-free deterministic entropy (a PRNG and a fuzzer byte stream) behind one
//!   trait so the same driver is reproducible from a seed and mutable by a fuzzer.
//! - [`model`]: the model-based tester: a table of live blocks with recomputable contents, driven
//!   through an allocator by a weighted random operation stream, checking the `GlobalAlloc`
//!   contract on every step.
//! - [`sim`]: [`sim::SimHeap`], the crate's own heap over a simulated linear memory in a host
//!   region, and the [`model::RawAlloc`] impl that lets the model drive it.

pub mod model;
pub mod rng;
pub mod sim;
