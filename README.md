# wasmalloc [![CI](https://github.com/bpowers/wasmalloc/actions/workflows/ci.yml/badge.svg)](https://github.com/bpowers/wasmalloc/actions/workflows/ci.yml)

wasmalloc is a memory allocator for Rust programs compiled to WebAssembly: a single-threaded
reimplementation of [mimalloc](https://github.com/microsoft/mimalloc) v3's design in pure Rust.

## Why

Rust's default allocator on `wasm32-unknown-unknown` is a port of dlmalloc. wasmalloc is
faster, every one of its `unsafe` blocks is proved or reviewed, and it adds nothing to your
build.

**Fast.** Median nanoseconds per operation, V8 15.2 optimizing tier (d8), Zen 5 host. dlmalloc
is the std default; talc is the pure-Rust allocator most often recommended for wasm.

| workload | wasmalloc | dlmalloc | talc |
|---|---:|---:|---:|
| 32-byte allocate and free | 1.1 | 4.1 | 8.4 |
| same, 16-byte aligned (`v128` types) | 1.1 | 7.9 | 8.4 |
| free one and allocate one among 10,000 live objects | 6.5 | 55.8 | 26.1 |
| talc's random allocate/free/realloc mix, sizes 1 to 10,000 bytes | 14.2 | 40.7 | 20.1 |
| grow one buffer from 16 bytes to 1 MiB by doubling (ns per chain) | 620 | 40 | 60 |

On JavaScriptCore and wasmtime wasmalloc's figures are within 15 percent of these and the
ranking is unchanged; on V8 12.4 (node 22) the first row reads 2.5, 8.0 and 11.7. The last row
is where wasmalloc loses: a size-class allocator copies at each doubling below 40 KiB, where a
boundary-tag allocator extends the block in place. Above 40 KiB wasmalloc can extend in place
too. The numbers come from `bench/roofline/run-all.sh`; the full tables, including memory
footprint and V8's baseline tier, are in `docs/research/roofline.md`.

The speed comes from the target. Rust passes the `Layout` to `dealloc`, so the size class is
known without reading memory and the page header is one address mask away; alignment up to
4 KiB costs nothing. Wasm memory arrives zeroed and only grows, so fresh pages skip the memset
and blocks above 40 KiB are header-less runs of 64 KiB slices. One thread: no atomics, no
thread-local state.

**Safe.** Every `unsafe` block is either covered by a Kani proof (the quick set gates every
merge, the full set runs before a release) or carries an entry in the
[soundness ledger](docs/soundness-ledger.md) with its preconditions, invariants and a written
proof, adversarially reviewed by someone who did not write the code. The allocator also runs
natively against a simulated linear memory, which is how it is fuzzed (a model-based
differential tester that checks alignment, overlap, zeroing and contents) and tested under Miri.

**Pure Rust, no dependencies.** `no_std`, stable Rust 1.85 or later, no C compiler, no
`wasi-sdk`. Builds for `wasm32-unknown-unknown`, `wasm32-wasip1` and `wasm32-wasip2`.

## Usage

```toml
[dependencies]
wasmalloc = "0.1"
```

```rust
#[cfg(target_arch = "wasm32")]
#[global_allocator]
static ALLOC: wasmalloc::WasmAlloc = wasmalloc::WasmAlloc::new();
```

wasm-bindgen, wasm-pack and std collections use the global allocator and need no changes.

Things to know:

- Single-threaded means no shared memory: the crate refuses to compile with the `atomics`
  target feature, so `wasm-bindgen-rayon` and other shared-memory threading are out. Web Workers
  each running their own instance are fine.
- Build with LTO (`lto = "thin"` or `"fat"`) so the allocator's fast paths inline into their
  callers, where LLVM specialises them for each constant `Layout`. Without LTO every allocation
  pays one extra call.
- On `wasm32-wasip1` and `wasip2`, wasi-libc keeps its own malloc for libc-internal
  allocations. wasmalloc leaves the initial linear memory to it and grows its own.
- It trades some linear memory for speed on small heaps: each size class in use holds at least
  one page (64 KiB, or 256 KiB for blocks over 10 KiB). On the benchmark workloads peak
  `memory.size` is between 1.0x and 2.0x the default allocator's; `docs/research/roofline.md`
  has the table and the design document's tuning log the latest numbers.

Problems and questions: <https://github.com/bpowers/wasmalloc/issues>.

## Design and development

`docs/design/2026-09-01-wasmalloc.md` explains the design and the reasoning behind each
constant. `bench/roofline/README.md` explains how to reproduce the measurements. The allocator
is generic over a `Memory` backend, so `cargo test` exercises it on the host against a simulated
wasm memory and `cargo test --target wasm32-wasip1` runs the wasm32 tests under wasmtime.

## License

Licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE) or the
[MIT license](LICENSE-MIT), at your option. The design follows mimalloc, copyright Microsoft
Corporation and Daan Leijen, licensed under the MIT license; no code is copied.
