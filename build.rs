//! Links this package's test binaries for wasi targets with an 8 MiB initial memory.
//!
//! With the default link the gap between `__heap_base` and the end of the initial linear
//! memory is the tail of one 64 KiB slice, which this heap never used, so the collision that
//! `tests/wasi_libc_gap.rs` checks for (review finding R-1: wasi-libc's dlmalloc and this heap
//! both taking the gap) could not show up in the default test run. `--initial-memory=8388608`
//! widens the gap to about a hundred whole slices.
//!
//! The flag goes through `rustc-link-arg-tests` rather than `.cargo/config.toml` because cargo
//! reads that config for every crate under the repository, including the roofline harness in
//! `bench/roofline`, whose wasi binary must keep the default layout, and target `rustflags`
//! arrays from nested configs are merged, not overridden. A build-script link argument reaches
//! only this package's test targets: neither the library nor any dependent crate sees it.
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_CFG_TARGET_OS").is_some_and(|os| os == "wasi") {
        println!("cargo::rustc-link-arg-tests=--initial-memory=8388608");
    }
}
