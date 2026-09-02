# wasmalloc

wasmalloc is a memory allocator for Rust programs compiled to WebAssembly. It is a
single-threaded reimplementation of [mimalloc](https://github.com/microsoft/mimalloc) v3's
design in pure Rust, arranged around what Rust's allocator interface knows that C's does not.

## Why

Rust's default allocator on `wasm32-unknown-unknown` is a port of dlmalloc. wasmalloc is
faster, is checked more thoroughly than most unsafe Rust, and adds no dependencies or toolchain
requirements to your build.

**Fast.** Median nanoseconds per operation on V8's optimizing tier (d8 15.2, the engine in
current Chrome), against the default allocator and the fastest existing pure-Rust alternative:

| workload | wasmalloc | dlmalloc (default) | talc |
|---|---:|---:|---:|
| 32-byte allocate and free | 1.1 | 4.1 | 8.4 |
| same, 16-byte aligned (`v128` types) | 1.1 | 7.9 | 8.4 |
| free one and allocate one among 10,000 live objects | 6.5 | 55.8 | 26.1 |
| talc's random allocate/free/realloc mix, sizes 1 to 10,000 bytes | 14.2 | 40.7 | 20.1 |
| grow one buffer from 16 bytes to 1 MiB by doubling (whole chain) | 620 | 40 | 60 |

JavaScriptCore and wasmtime give the same picture within 15 percent; on node 22's older V8
the first row reads 2.5, 8.0 and 11.7. The last row is the one place wasmalloc loses: a
size-class allocator copies at each doubling below 40 KiB, where a boundary-tag allocator
extends the block in place. Above 40 KiB wasmalloc also extends in place. The numbers come
from `bench/roofline/run-all.sh`; the full tables, including memory footprint and the
baseline-compiler tier, are in `docs/research/roofline.md`.

The speed comes from a few facts about this target. Rust hands the allocator the `Layout` on
every `dealloc` and `realloc`, so the size class is known without reading memory, the page
header is found by masking the address, and any alignment up to 4 KiB is satisfied by
construction with no over-allocation. WebAssembly memory is a flat address space of 64 KiB
pages that only grows and arrives zeroed, so pages are placed at addresses that make the mask
trick work, zeroed allocations from fresh pages skip the memset, and blocks above 40 KiB are
header-less runs of pages that grow in place. There is one thread, so nothing is atomic and
nothing is thread-local. The fast paths compile to a handful of loads and stores and are
inlined into the `__rust_alloc` and `__rust_dealloc` shims even at `opt-level = "z"`.

**Safe.** Every `unsafe` block is either covered by a machine-checked proof (33 Kani harnesses;
the fast ones run in the merge gate, all of them before a release) or carries an entry in the
[soundness ledger](docs/soundness-ledger.md) with its preconditions, invariants and a written
proof, adversarially reviewed by someone who did not write the code. The whole allocator runs
natively against a simulated linear memory, which is how it is fuzzed (a model-based
differential tester that checks alignment, overlap, zeroing and contents), tested under Miri
with both aliasing models, and proved.

**Pure Rust, no dependencies.** `no_std`, stable Rust 1.85 or later, no C compiler, no
`wasi-sdk`, nothing to install. Builds for `wasm32-unknown-unknown` and `wasm32-wasip1`/`p2`.

## Usage

```toml
[dependencies]
wasmalloc = "0.1"
```

```rust
#[global_allocator]
static ALLOC: wasmalloc::WasmAlloc = wasmalloc::WasmAlloc::new();
```

That is the whole integration. wasm-bindgen, wasm-pack and std collections use the global
allocator and need no changes.

Two things to know:

- wasmalloc is single-threaded by design. It refuses to compile with the `atomics` target
  feature rather than silently racing.
- It trades some linear memory for speed on small heaps: each size class in use holds at least
  one 64 KiB page, and memory grows in steps of an eighth of the heap because each
  `memory.grow` call costs tens of microseconds in some engines. On the benchmark workloads
  peak `memory.size` is between 0.9x and 2.1x the default allocator's; the details are in
  `docs/research/roofline.md`. Pages are touched 8 KiB at a time, so resident memory is
  lower than `memory.size` suggests.

## Design and development

The design document is `docs/design/2026-09-01-wasmalloc.md`; `CLAUDE.md` describes how the
code is organised, built, benchmarked and verified. The allocator is generic over a `Memory`
backend, so `cargo test` exercises it on the host against a simulated wasm memory and
`cargo test --target wasm32-wasip1` runs the same tests under wasmtime.

## License

Licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE) or the
[MIT license](LICENSE-MIT), at your option. The design follows mimalloc, which is
copyright Microsoft Research and licensed under the MIT license; no code is shared.
