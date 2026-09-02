# Allocator landscape for single-threaded wasm32 Rust

Research notes for a pure-Rust `#[global_allocator]` for `wasm32-unknown-unknown`
modelled on mimalloc v3. Written 2026-09-01 against Rust 1.95.0, wasmtime 48.0.1,
Node 22.22.2 (V8 12.4.254.21), wasm-bindgen 0.2.108, wasm-opt 125. Every source
tree referenced below was cloned and read; numbers marked "measured" come from a
benchmark crate built for this survey (design in Part C) and were run once on an
AMD Ryzen 9 9950X. Treat them as indicative, not as a published benchmark.

Contents

- Part A: allocators usable from Rust on wasm32, plus non-Rust designs worth copying
- Part B: how allocators get benchmarked, and which of that ports to wasm
- Part C: running and timing wasm on this machine, and a harness design
- What it takes to be the best

## Summary of conclusions

- Rust 1.95 std on `wasm32-unknown-unknown` uses `dlmalloc` 0.2.11 (pinned in
  `library/Cargo.lock`), wired through `library/std/src/sys/alloc/wasm.rs` with a
  no-op lock when the `atomics` feature is off. On `wasm32-wasip1` std does not use
  dlmalloc-rs at all: `target_os = "wasi"` selects the `unix` path, so the default
  allocator there is wasi-libc's C dlmalloc. Any wasip1 benchmark of "the default"
  is measuring a different binary than the browser sees.
- Nothing published is a pure-Rust mimalloc. The closest is `ferroc` 1.0.0-pre.4
  (mimalloc-inspired, nightly-only, mmap or static base, no wasm support). Every
  crate with "mimalloc" in the name is an FFI binding to the C library, and the C
  `mimalloc` crate does not compile for either wasm target here (missing C sysroot).
- talc 5.1.0 is the strongest existing pure-Rust option: measured roughly 2x fewer
  ns per random alloc/free than dlmalloc-rs under both wasmtime and V8, at 1.6 KiB
  of code versus 5.4 KiB. rlsf (TLSF) sits in between; lol_alloc is 2x slower than
  dlmalloc; wee_alloc is unmaintained with an unbounded leak.
- None of the Rust allocators use size-class pages, none exploit the `Layout` at
  `dealloc` to skip header reads (talc uses size to locate its tail tag, rlsf uses
  align to locate its header, dlmalloc-rs only uses size for a runtime assert), and
  none avoid the memset in `alloc_zeroed` on fresh zero pages.
- `memory.grow` in V8 costs 50 to 75 microseconds per call regardless of how many
  pages are requested (measured), about 100x wasmtime. Every existing Rust
  allocator grows by exactly the pages the failing request needs. Geometric growth
  is a free win in browsers.
- V8 tiering flags: on Node 22 `--no-wasm-tier-up` alone does not disable dynamic
  tiering. Use `--liftoff-only` for baseline-compiler numbers and `--no-liftoff` for
  optimizing-compiler numbers. Wasmtime's Cranelift tracks TurboFan within about
  10 percent on these workloads and ranks allocators identically, so it is a fine
  CI runner; V8 is still needed for the browser truth (grow cost, Liftoff).

## Part A: existing allocators

### A.1 dlmalloc-rs (the std default on wasm32-unknown-unknown)

Repository: https://github.com/alexcrichton/dlmalloc-rs (crate `dlmalloc`, latest
0.2.14, 2026-05-16). Rust 1.95.0 std depends on `dlmalloc = "0.2.10"` with the
`rustc-dep-of-std` feature and the lockfile resolves 0.2.11 (2025-08-20). The
differences between 0.2.11 and 0.2.14 matter for wasm: 0.2.13 (2026-03) added
donation of the linker's `__heap_base..__heap_end` gap as the first chunk before
any `memory.grow`, and 0.2.14 added a configurable growth granularity and a C-style
`c_malloc`/`c_free` API. Neither is in the std that ships today.

How std wires it (`library/std/src/sys/alloc/wasm.rs`, 173 lines): a
`static DLMALLOC: SyncUnsafeCell<SyncDlmalloc>` and `impl GlobalAlloc for System`
that forwards `alloc` to `malloc(size, align)`, `alloc_zeroed` to `calloc`,
`dealloc` to `free(ptr, size, align)`, and `realloc` to
`realloc(ptr, size, align, new_size)`. With `target_feature = "atomics"` the lock is
a spin on an `AtomicI32` with a long comment explaining that the browser main thread
cannot block; without atomics `lock()` is an empty function. The module comment says
outright that the choice was made because wasm could not link C at the time.

Algorithm (`src/dlmalloc.rs`, 2117 lines, a line-by-line port of dlmalloc.c):

- Boundary tags. Every chunk has a `head` word (size plus PINUSE/CINUSE bits) and
  free chunks also write a `prev_foot`. `Chunk` is 4 words; `min_chunk_size` is 16
  bytes on wasm32 and `chunk_overhead` is 4 bytes, so requests of 1 to 12 bytes each
  cost 16 bytes.
- 32 small bins spaced 8 bytes apart (chunk sizes 16 to 248), each a circular
  doubly-linked list, indexed by `size >> 3`, with a 32-bit `smallmap` occupancy
  bitmap. 32 tree bins (bitwise tries keyed on size) for chunks of 256 bytes and up,
  with a `treemap`. A designated victim `dv` and the `top` chunk absorb splits.
- `malloc`: exact small bin or the next bin up; else the smallest larger small bin
  with a split into `dv`; else `tmalloc_small` (walk a tree); for large requests
  `tmalloc_large` (trie descent plus leftmost walk); then `dv`, then `top`, then
  `sys_alloc`.
- `free`: read `head`, read the next chunk's head, if the previous chunk is free
  read `prev_foot`, unlink one or two neighbours from their bins, coalesce, then
  insert (small list push or tree insert). Every free does this work; there is no
  deferred or batched coalescing.
- `malloc_alignment()` is `2 * size_of::<usize>()`, which is 8 on wasm32. Any
  `Layout` with align 16 or more takes `memalign`, which over-allocates by
  `align + min_chunk_size`, then splits off a leader and a trailer with two calls
  to `dispose_chunk`. On wasm32 `u128` has align 8 (rust-lang/rust#133991), so this
  bites mainly on `v128` SIMD types and explicit `repr(align(16))`.
- `realloc` with align 8 or less tries `try_realloc_chunk` in place (shrink, grow
  into `top`, into `dv`, or into a free next chunk), else malloc plus copy plus free.
  With align above 8 it always allocates, copies and frees.
- `calloc` calls `calloc_must_clear`, which returns true unless the chunk is
  "mmapped". The Rust port never creates mmapped chunks (there is no mmap fast path
  for large requests; everything goes through the bins and `top`), so
  `alloc_zeroed` always memsets, including memory fresh from `memory.grow` that is
  already zero.
- `free(ptr, size, align)` ignores `align` and uses `size` only for
  `validate_size`, two `assert!`s that fire on every deallocation in release
  builds. The Layout is a cost here, not a benefit.

Memory acquisition (`src/wasm.rs`, 202 lines): `System::alloc(size)` calls
`memory_grow(0, size.div_ceil(65536))` and returns exactly those pages.
`sys_alloc` asks for `align_up(size + top_foot_size + 8, granularity)` with a 64
KiB granularity, so the heap grows by the minimum number of pages that satisfies
the current request, never geometrically. If the grown region is contiguous with
the current segment (always, unless something else grew memory) `top` is extended
in place; otherwise a new segment record is pushed and `add_segment` writes
fenceposts. `free`, `free_part`, `remap` and `can_release_part` all return false or
null, so memory is never returned and `trim` is a no-op. There is a special case
for growing into the very last 64 KiB of the 4 GiB address space that shaves 16
bytes so a pointer never wraps to zero.

Size: 5410 bytes of wasm after `wasm-opt -Oz` in an isolated no_std probe
(measured), 29 functions. talc's own table lists 5539 and lol_alloc's README lists
5034 for the built-in allocator.

Maintenance: active (four releases in the last year), CI runs `cargo test` on
`wasm32-wasip1` under wasmtime (`CARGO_TARGET_WASM32_WASIP1_RUNNER=wasmtime`),
builds `wasm32-unknown-unknown`, runs Miri under both Stacked and Tree Borrows,
and builds cargo-fuzz targets. The README says it "is not the most performant by a
longshot" and exists so that wasm had a pure-Rust allocator.

Published numbers: talc's wasm-perf CSV gives DLMalloc 11.91 actions per
microsecond without realloc and 6.35 with, against talc 28.24 and 12.23. Measured
here: 43.9 ns per random action in V8 TurboFan, 41.2 under wasmtime.

### A.2 talc 5.1.0

Repository: https://github.com/SFBdragon/talc (crate `talc` 5.1.0, 2026-08-24,
MSRV 1.64, MIT). Version history that matters for wasm: 5.0.0 (2026-03-19)
introduced per-target binning, `TalcCell`, and the `Source` trait; 5.0.4
(2026-06-18) fixed two wasm bugs found in long-running applications (issue #49): the
allocated-chunk tag byte shared a byte with the trailing size of 16 MiB and larger
gaps on little-endian, misclassifying gaps as allocations and corrupting the heap;
and `WasmGrowAndClaim` undersized its growth for requests just below a page
multiple, claiming heaps forever until `memory.grow` failed at 4 GiB. The fix
widened the per-allocation tag from a byte to a word. 5.1.0 switched the default
wasm source from `WasmGrowAndClaim` to `WasmGrowAndExtend` after issue #51 showed
the claim strategy using 10x the memory in a "ratchet" pattern (1705 pages versus
171) because a freed heap tail could never coalesce with the next grown region.

Algorithm (`talc/src/base/mod.rs` 1298 lines, `chunk.rs`, `tag.rs`, `binning.rs`,
`bitfield.rs`, `node.rs`): the README describes it as "a dlmalloc-style linked list
allocator with boundary tagging and binning" that "shares a lot of similarities
with the TLSF algorithm".

- `CHUNK_UNIT` is `4 * size_of::<usize>()`, 16 bytes on wasm32; every chunk is a
  multiple of it and aligned to it. Each chunk ends in one word (`TAIL_SIZE`,
  4 bytes on wasm32): for allocations it holds a `Tag` (bits ALLOCATED,
  ABOVE_FREE, HEAP_BASE, HEAP_END), for gaps it holds the gap size, whose low bits
  are zero, which is how the two are told apart. A gap additionally stores at its
  base a `Node { next, next_of_prev }`, its bin index, and its size. So an
  allocation costs 4 bytes of metadata plus rounding to 16.
- Bins: `WasmBinning` uses a `u64` availability bitfield (the docs note wasm32 has
  64-bit instructions so one `u64` beats two `usize`) and
  `linear_extent_then_linearly_divided_exponential_binning::<2, 8>`: one bin per 16
  bytes up to 256 bytes, then 2 linear subdivisions per power of two. The last bin
  is searched exhaustively.
- `try_allocate`: compute the required chunk size, `size_to_bin_ceil` of
  `max(size, align)` (the `.max(layout.align())` is the fix for issue #44, where a
  1-byte allocation with 4096 alignment was 1650x slower than dlmalloc), scan the
  bitfield forward, pop the head of that list, `deregister_gap`, clear the
  ABOVE_FREE bit of the chunk below, and `register_gap` the remainder if any. For
  align above 16 it walks the list (`full_search_bin`) checking each gap.
- `deallocate(ptr, layout)`: computes the chunk end from `layout.size()`, reads the
  tag, and coalesces with the gap below (reading the word below the chunk) and the
  gap above (if ABOVE_FREE), each coalesce being a `deregister_gap`; then one
  `register_gap`. `register_gap` writes four words plus a bitfield update;
  `deregister_gap` unlinks and reads the bin index back. Two comments in
  `chunk.rs` and `base/mod.rs` are telling: "WASM perf tanks if these #[inline]'s
  are not present" and `#[cfg_attr(not(target_family = "wasm"), inline)]` on
  `register_gap`/`deregister_gap`, so on wasm those two are deliberately kept
  out-of-line for size.
- `realloc`: `try_realloc_in_place` (grow into the gap above, or shrink and
  register the tail) else allocate, copy, deallocate. Features
  `disable-grow-in-place` and `disable-realloc-in-place` trade this for size.
- `alloc_zeroed`: `alloc` then `write_bytes`, always.
- Single-threaded: `TalcSyncCell` (an `unsafe impl Sync` around `TalcCell`) is what
  `talc::wasm::new_wasm_dynamic_allocator()` returns; it panics at construction if
  built for a non-wasm target or with atomics. `TalcLock<RawMutex>` for threads.
- Memory: `WasmGrowAndExtend::acquire` grows by `delta_pages_for(layout)`, the
  minimum pages that fit the failing request plus a chunk of slack (and the
  alignment if above 16), then `extend`s the heap if the new pages are contiguous
  with the previous end, else `claim`s a new heap. No geometric growth, no release
  (`Source::resize` is only invoked if `TRACK_HEAP_END` is true, which the wasm
  sources leave false). An arena mode (`WasmArenaTalc`) allocates out of a `static`
  array instead.

Size: 1616 bytes after `wasm-opt -Oz` (1403 with `disable-realloc-in-place`),
measured; talc's own table: 1625 / 1415.

Benchmarks (repository `benches/`, `wasm-perf/`, `wasm-size/`, results in
`results/*.csv`, method in `BENCHMARKS.md` and `BENCHMARKS_WASM.md`):

- Random actions: with a 300-allocation floor, each step is 1/7 realloc to a random
  size up to 3x max, 3/7 free a random live allocation, 3/7 allocate with size from
  `generate_size` (biased small: `usize(4..usize(16..max))`) and alignment from
  `generate_align` (75 percent pointer size, 19 percent 2x, 4 percent 4x). Score is
  actions completed in 200 ms, 7 trials, max sizes 200 to 30000. Native results
  (Linux x86_64 arena mode): Talc 8.7M, RLSF 7.7M, Buddy 7.3M, DLmalloc 6.5M at
  max size 200; the gap widens at larger sizes.
- Heap efficiency: fill a 128 MiB arena until OOM, average fraction used over 300
  rounds. DLmalloc 97.7 percent, RLSF 97.3, Talc v4 96.6, Talc 95.2, Galloc 81.5,
  Buddy 58.9. The wasm README warns to expect 10 to 15 percent higher memory use
  than dlmalloc with the wasm defaults.
- Microbench: `rdtsc` around single alloc and dealloc calls with 600 live
  allocations, quartiles reported; Talc median 168 ticks versus DLmalloc 326.
- wasm-perf (`wasm-pack build --target deno`, `performance.now()`, 100k actions
  times 100 iterations, sizes 1 to 10000, align `8 << tz(u16)/2`): Talc (Dynamic)
  28.24 / 12.23 actions per microsecond (no realloc / realloc), Talc (Arena) 29.00 /
  12.91, RLSF 19.52 / 9.61, RLSF (Small) 14.70 / 8.33, DLMalloc 11.91 / 6.35,
  lol_alloc 4.07 / 3.05.
- The author states the benchmarks are "mildly fitted to Talc" because its design
  and the size/alignment distributions share the same intuitions.

Independent report: nickb.dev ("Avoiding allocations in Rust to shrink Wasm
modules", 2025 edit) tried talc in a real wasm-bindgen library (highwayhasher) and
found "the size and performance differences weren't large enough to make the change
permanent" across Chrome and Firefox.

CI: `cargo check` on wasm32-unknown-unknown in all feature combinations, Miri on
x86_64 and i686, a native `high_alignment_test` regression guard (issue #44), and
doc checks. The wasm regression tests in `talc/tests/wasm_sources.rs` run through
`CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner
WASM_BINDGEN_TEST_ONLY_NODE=1` but are not in CI. `wasm-perf` uses deno.

### A.3 lol_alloc 0.4.1

Repository: https://github.com/Craig-Macomber/lol_alloc (0.4.1, 2024-02-24, MIT).
Written to learn about allocators and to replace wee_alloc, which the author "failed
to understand or fix". `FreeListAllocator` (593 lines including tests) keeps a
single free list sorted by descending address; each node is `{ next, size }` (two
words) and allocations are rounded up to a multiple of 8. `alloc` walks the list and
carves from the end of the first block that fits, splitting if the aligned position
leaves room; `dealloc` walks to the insertion point and coalesces with both
neighbours. Both are O(length of the free list). No `realloc` override, so std's
default allocate-copy-free applies. Out of memory grows by exactly
`round_up(size, 64 KiB)` pages, then `dealloc`s the new region into the list and
retries. `AssumeSingleThreaded<T>` is the `unsafe impl Sync` wrapper;
`LockedAllocator<T>` uses `spin`. Also ships `FailAllocator`, `LeakingPageAllocator`
and a bump `LeakingAllocator`. The README states these are "optimized for
simplicity ... and not runtime performance" and that no performance data was
collected. It raises the pointer-provenance question for `memory.grow` (no
"original pointer" exists) and notes coalescing across grow calls may be unsound
under strict provenance.

Size: 709 bytes (measured, `AssumeSingleThreaded<FreeListAllocator>`). Measured
speed: 78 to 83 ns per random action, about 1.9x slower than dlmalloc-rs, but its
degenerate LIFO behaviour makes the 48-byte churn case the fastest of all (1 ns per
op) because alloc and free both touch only the head node. Memory: 280 pages versus
247 for dlmalloc in the harness.

### A.4 wee_alloc 0.4.5 and why it was abandoned

Repository: https://github.com/rustwasm/wee_alloc (0.4.5, 2019-08-22, MPL-2.0;
last commit 2021-04-29; archived 2025-08-25). Design: a first-fit main free list of
`FreeCell`s with a `Neighbors` intrusive doubly-linked physical list, two words of
overhead per allocation, plus (default feature `size_classes`) 256 size-class free
lists for allocations of 1 to 256 words, each refilled with cells of at least 8 KiB
carved from the main list (`size_classes.rs`, `MIN_NEW_CELL_SIZE`). Size-class
cells never merge. In the main list, merging with the previous neighbour is
immediate; merging with the next neighbour is deferred via a `NEXT_FREE_CELL_CAN_MERGE`
bit that is honoured on the next free-list walk. Pages are obtained with
`memory_grow` of exactly the rounded size and never returned. Target code size
under 1 KiB (1.2 KiB with size classes).

Why it was abandoned: RUSTSEC-2022-0054 (published 2022-09-08) records that two of
the maintainers said the crate may not be maintained. Open issue #106 "Unbounded
Memory Leak" (2022-03-24): allocating two large blocks (85196 and 80000 bytes) and
freeing them in allocation order in a loop consumes gigabytes per second natively
and grows the wasm heap by 131 MB per click; issue #85 "wee_alloc leaks memory" and
#105 report related leaks; the deferred-merge scheme is the suspected culprit and
nobody fixed it. wasm-pack removed it from its templates (drager/wasm-pack#1258),
and Dependabot flags any dependent. rlsf's author could not even benchmark it on a
microcontroller because it ran out of memory (rlsf README, referencing #85).

### A.5 rlsf 0.2.3

Repository: https://github.com/yvt/rlsf (0.2.3, 2026-07-27; MIT/Apache-2.0;
MSRV 1.61). A faithful TLSF (Masmano et al. 2004) with one change: pools end in a
permanently-used sentinel block instead of a last-block flag.

- `GRANULARITY = 4 * size_of::<usize>()` (16 bytes on wasm32). `BlockHdr { size,
  prev_phys_block }` is 8 bytes and precedes the payload; free blocks add
  `next_free`/`prev_free`. Size field bit 0 is USED, bit 1 is SENTINEL.
- `Tlsf<FLBitmap, SLBitmap, FLLEN, SLLEN>`: first-level index is the size's
  floor(log2), second-level index is the next `log2(SLLEN)` bits; `first_free` is a
  `[[Option<NonNull>; SLLEN]; FLLEN]` array. `GlobalTlsf` on wasm32 instantiates
  `FLLEN = SLLEN = usize::BITS = 32`, so 1024 list heads (4 KiB of static) plus a
  `u32` first-level bitmap and 32 second-level bitmaps.
- `allocate`: `map_ceil`, one `bit_scan_forward` in the second-level bitmap, else
  one in the first-level bitmap; pop the head; split the remainder into a new free
  block. Constant time, and the README's headline is real-time guarantees.
- `deallocate(ptr, align)`: if `align >= 16` a `UsedBlockPad` pointer stored just
  below the payload locates the header, otherwise the header is at `ptr - 8`. This
  is the one place a Rust allocator here uses the Layout's alignment structurally.
  Coalesces with next and previous physical blocks immediately, then links.
- `reallocate` tries in place (`reallocate_inplace`) then falls back to
  allocate-copy-free; `SmallGlobalTlsfOptions` sets `ENABLE_REALLOCATION = false`
  and `COALESCE_POOLS = false` for size.
- wasm32 source (`global/wasm32.rs`): grows exactly `ceil(min_size / 64 KiB)`
  pages; `realloc_inplace_grow` extends the pool if `ptr` is at the current
  `memory_size`, otherwise a new pool is inserted. Never releases. Mutex is a no-op
  when `atomics` is off; the type does not exist with atomics on.
- Drawbacks the README admits: no concurrency, internal fragmentation proportional
  to free block size (`SLLEN` tunes it), "no special handling for small allocations
  (one algorithm for all sizes)".

Size: 2108 bytes (`GlobalTlsf`), 1202 bytes (`SmallGlobalTlsf`), measured. Talc's
table: 2259 / 1345. Speed measured here: 27 to 28 ns per random action (about 1.6x
faster than dlmalloc, 1.3x slower than talc), 6 to 7 ns on 48-byte churn (slowest
of the group), and 322 memory pages against 247 for dlmalloc, the highest memory use
in the harness. Published: on an STM32F401 rlsf takes 260 to 320 cycles per
operation versus dlmalloc 450 to 750 (FarCri), and code size on wasm 1267 bytes
versus dlmalloc 9613 in the author's older measurement. CI runs `cargo test
--target wasm32-wasi` under wasmtime.

### A.6 emballoc 0.3.0

Repository: https://github.com/jfrimmel/emballoc (0.3.0, 2024-09-17). A
`Allocator<const N: usize>` over a static `[u8; N]` buffer with 4-byte entry headers
and a linear scan for both allocation and free (free scans every entry to find the
one containing the pointer, then merges only with the following free entry). Spin
lock via the `spin` crate. Correct and well tested (Miri, big-endian, 96 percent
coverage, a ripgrep experiment) but designed for embedded heaps of a few KiB; no
`memory.grow`, O(n) in the number of live allocations. Not a candidate for wasm.

### A.7 frusa 0.1.3

Repository: https://github.com/moturus/motor-os (crate `frusa` 0.1.3, 2025-11-22;
MIT/Apache-2.0; nightly, `#![feature(test)]`). A slab allocator: power-of-two size
classes from 16 bytes up to 4 KiB (`Frusa4K`) or 2 MiB (`Frusa2M`), 64-entry blocks
tracked by an `AtomicU64` bitmap, block metadata in a separate 64-byte-cell
metadata slab, everything else forwarded to a `&'static dyn GlobalAlloc` fallback.
Lock-free with CAS on the bitmaps plus a reclaim read-write lock. Its constructor
asserts `size_of::<usize>() == 8`, so it does not build for wasm32, and it needs a
fallback allocator anyway. Its own README measures it at 59 ns per alloc/dealloc
versus 20 ns for glibc single-threaded. Talc's benchmarks include it under "system
allocators". Useful only as a design reference for bitmap slabs.

### A.8 bumpalo 3.21

Repository: https://github.com/fitzgen/bumpalo. A bump arena: a pointer within
the current chunk moves on each allocation; chunks come from the global allocator,
starting at 512 bytes and doubling, with a footer per chunk; individual `dealloc`
only works for the most recent allocation; `reset()` frees everything at once
without running `Drop` (hence `bumpalo::boxed::Box` and `bumpalo::collections`).
It is not a general answer because it cannot be a `#[global_allocator]` for a
program with arbitrary lifetimes; it is a tool for phase-oriented allocation
(parse a document, use the tree, drop the arena). It matters here as a competitor
for specific workloads (nickb.dev's serde gauntlet uses arenas to beat any malloc)
and as an idea: a general allocator can still get bump-like behaviour for fresh
pages by carving a new page sequentially before consulting free lists, which is
exactly what mimalloc's page `free` list plus `local_free` split achieves.

### A.9 C wrappers: mimalloc, rpmalloc-rs, snmalloc-rs

`mimalloc` 0.1.52 / `libmimalloc-sys` 0.1.49 (https://github.com/purpleprotocol/mimalloc_rust,
2026-05-22) vendors upstream mimalloc v2 and v3 as git submodules and compiles
`src/static.c` with the `cc` crate; v3 is the default and `v2` is a feature. Tested
here: `cargo build --target wasm32-unknown-unknown` and `--target wasm32-wasip1`
both fail in the build script with `fatal error: 'wchar.h' file not found`, because
clang 22 is invoked with `--target=wasm32-...` and no C sysroot. The `cc` crate
honours `WASI_SYSROOT`, so with wasi-sdk installed the wasip1 build might link
(upstream has `src/prim/wasi/prim.c`: 64 KiB pages, grows with
`__builtin_wasm_memory_grow` or `sbrk`, never frees, `is_zero = false`, and
CMake enables `MI_FREE_USE_PAGEMAP` on WASI because "the target does not support
large OS aligned allocations well"). For `wasm32-unknown-unknown` there is no libc,
no `stdio.h`, no `pthread`, and no sysroot to point at, so the crate cannot work
without a freestanding shim that nobody has written. Users avoid it on wasm for
those reasons plus binary size and the C-toolchain requirement; the emscripten
world gets mimalloc a different way (`-sMALLOC=mimalloc`, layered on emmalloc as
the "OS", per `src/prim/emscripten/prim.c`). `mimalloc-safe` 0.1.65 is a fork of
the same wrapper. `mimalloc-rust` 0.2.1 (LemonHX) binds mimalloc 1.7.9 or 2.1.2 via
a hand-written sys crate. `mimalloc-rs` 0.1.0 (playXE, 2019) is a 56-line
`extern "C"` binding. All FFI, none wasm.

`rpmalloc` 0.2.2 (https://github.com/EmbarkStudios/rpmalloc-rs, 2021-05-17)
compiles `rpmalloc.c` with `cc`; the README lists exactly three supported targets
(x86_64 Windows, macOS, Linux). rpmalloc needs 64 KiB spans from mmap, thread-local
heaps and OS TLS. Not wasm.

`snmalloc-rs` 0.7.5 (2026-08-12): the standalone repo says "Prepare to archive
repo" (2026-01-29) and development moved into microsoft/snmalloc. It drives CMake
to build C++17 snmalloc; the build script has no wasm branch. snmalloc's design
(16 MiB chunks, a pagemap over the address space, message-passing frees between
threads) assumes a large virtual address space. Not wasm.

### A.10 Pure-Rust mimalloc ports and mimalloc-inspired Rust allocators

Search results (crates.io API for "mimalloc", "wasm allocator", "global_allocator
wasm32", "segregated allocator wasm"; GitHub; lib.rs memory-management category):

- ferroc 1.0.0-pre.4 (https://github.com/js2xxx/ferroc, last commit 2025-09-18,
  MIT/Apache-2.0). "A lock-free concurrent memory allocator written in Rust,
  primarily inspired by mimalloc." Nightly-only (`allocator_api`,
  `alloc_layout_extra`, `pointer_is_aligned_to`, `ptr_as_uninit`). Structure:
  `Arenas<B: BaseAlloc>` hand out `Slab`s of `SLAB_SIZE = 4 MiB`
  (`FE_SLAB_SHIFT=22`), each split into `SHARD_SIZE = 64 KiB` shards
  (`FE_SHARD_SHIFT=16`) with the slab header in the first shard(s); `Context` and
  `Heap` are thread-local and `!Sync`; bins by `obj_size_index`: 16-byte
  granularity to 128 bytes then 8 linear steps per power of two; small up to 1 KiB
  through a direct table, medium up to 32 KiB, large up to about 1.9 MiB, huge
  direct from the arena; `finer-grained` feature drops granularity to 8. Base
  allocators are `Mmap` (memmap2 + libc) and `Static` (caller-provided 4 MiB-aligned
  chunks). No wasm mentions anywhere; it benchmarks with a subset of mimalloc-bench.
  This is the only substantial mimalloc-shaped Rust code base and is worth reading
  for its slab/shard/bin layout, but it cannot be used as-is (nightly, no
  `memory.grow` base, 4 MiB alignment).
- rusch95/mimalloc-rs (2019, 30 commits): scaffolding for a function-by-function
  port with the C library dynamically linked so functions could be swapped one at a
  time. Nothing beyond the scaffold was ported.
- vporton/mimalloc-rs ("Going to port mimalloc to pure Rust", last commit
  2025-01-24 adding FUNDING.yml): the repository is a fork of the C sources with
  zero `.rs` files.
- microsoft/mimalloc#1167 "Pure Rust implementation of mimalloc" (2025-11-04): a
  proposal to contribute output from an LLM-based C-to-Rust transpiler; no
  maintainer response as of this writing.
- Conclusion: no pure-Rust mimalloc has been published, for any target.

Other small wasm-oriented allocators found:

- mini-alloc 1.0.0 (Offchain Labs, for Arbitrum Stylus): 84-line bump allocator,
  `dealloc` is a no-op, starts at `__heap_base`, and its `alloc_zeroed` returns
  fresh memory without memset because wasm pages start zeroed (README: 329 gas
  versus 48 million for std's `alloc_zeroed`). Leaks by design.
- alloc_cat 1.1.1: bump pointer plus free list, metrics, a wee_alloc replacement
  for "small-to-tiny" modules.
- lite-alloc 0.1.0 (2025-12-19, "experimental"): three variants, bump plus
  unsorted free list, segregated bins for 16/32/64/128 bytes with bump fallback (no
  reuse of large), and an address-sorted coalescing list.
- rustix-dlmalloc 0.2.2: dlmalloc-rs on rustix, not wasm-specific.
- Excluded by talc as far slower: `linked_list_allocator`, `simple_chunk_allocator`;
  also `buddy-alloc` (59 percent heap efficiency) and `good_memory_allocator`.

### A.11 Non-Rust wasm allocators worth learning from

emmalloc (emscripten `system/lib/emmalloc.c`, 1433 lines, read at main):

- Regions carry their size at both ends (`[size][payload][size]`), 8 bytes of
  overhead, payloads a multiple of 4 and at least 8, `FREE_REGION_FLAG` in the low
  bit of the trailing size. Free regions are in 64 circular doubly-linked buckets
  with a `uint64_t` occupancy mask: 8-byte-wide buckets up to 128 bytes, then two
  per power of two up to 128 MiB (table in the source). `compute_free_list_bucket`
  is a `clz` plus a few shifts.
- `allocate_memory` scans buckets with `ctz`, tries only the first region in a
  bucket and rotates it to the back on failure ("constant time" guarantee), and
  only when all buckets fail examines up to 99 regions of the largest bucket before
  calling `sbrk`. `free` coalesces both sides immediately. `realloc` grows into a
  free neighbour or shrinks in place. `calloc` always memsets.
- Memory comes from `sbrk` in increments of exactly `size + 3 * sizeof(Region)`,
  relying on emscripten's `sbrk` to over-allocate geometrically (`MEMORY_GROWTH_
  GEOMETRIC_STEP`, default 20 percent) so `memory.grow` is not called per request.
  It also implements `malloc_trim` with a negative `sbrk`. Emscripten's default is
  still dlmalloc; emmalloc is `-sMALLOC=emmalloc` and mimalloc is
  `-sMALLOC=mimalloc`.

Zig `std.heap.WasmAllocator` (lib/std/heap/WasmAllocator.zig, 326 lines with
tests; PR ziglang/zig#13513 by Andrew Kelley): comptime-restricted to wasm and
single-threaded. Size classes are powers of two from 8 bytes (`min_class`, room
for the free-list pointer) to 64 KiB (13 classes); each class has a bump pointer
`next_addrs[class]` refilled with one fresh 64 KiB "bigpage" from `memory.grow(1)`
when exhausted, and an intrusive singly-linked free list `frees[class]` whose next
pointer is stored in the last word of the slot. `free(buf, alignment)` recomputes
the class from the slice length and alignment, so there are no headers at all;
`resize` succeeds only within the same power-of-two slot. Allocations above 64 KiB
get power-of-two runs of bigpages with their own free lists. Nothing is ever
coalesced or returned. It is the cleanest example of exploiting the Layout at free
in a wasm allocator, at the cost of up to 2x internal fragmentation.

Go runtime on wasm (`runtime/mem_wasm.go`, `mem_sbrk.go`, `os_wasm.go`,
`internal/runtime/gc/sizeclasses.go`): the OS layer is a linear break (`bloc`,
`blocMax`) grown with `growMemory(pages)` at `physPageSize = 64 KiB`, plus an
address-ordered free list for interior frees; `sysFreeOS` at the top of memory
just lowers `bloc`. After every grow the js port calls `resetMemoryDataView` so the
JavaScript side rebuilds its `DataView`, which is the same view-invalidation
problem wasm-bindgen users hit. On top sits the normal tcmalloc-derived allocator:
68 size classes up to 32 KiB (8, 16, 24, 32, 48, 64, 80, 96, 112, 128, 144, ...,
32768; `SizeToSizeClass128` lookup tables; per-class "max waste" kept under about
12 percent for classes above 256 bytes), 8 KiB pages grouped into spans with
per-span allocation bitmaps, a per-P `mcache` of spans, `mcentral` per class,
`mheap` for pages, and `needzero` tracking so fresh spans are not re-zeroed. The
size-class table and the needzero flag are directly reusable ideas.

## Part B: benchmarking

### B.1 mimalloc-bench (daanx/mimalloc-bench, HEAD 2026-08-06)

`bench.sh` runs each program under `LD_PRELOAD` of the allocator and records
elapsed time, max RSS, user/sys time and page faults via GNU `time` into
`benchres.csv`; `graphs.py` normalizes against a baseline allocator. Allocators are
pinned in `build-bench-env.sh` (mimalloc v1.8.2 / v2.1.2 / v3.4.5, jemalloc 5.3.1,
snmalloc 0.7.5, tcmalloc gperftools-2.18, rpmalloc 2.0.1, ...). The `allt` set is:

| Benchmark | What it exercises | Threads | Rust/wasm port |
|---|---|---|---|
| cfrac | continued-fraction factorisation of a 44-digit number; millions of tiny short-lived bignum limbs | 1 | Sources are 40 C files; porting is real work. A `num-bigint` factorisation loop is a fair stand-in. |
| espresso | PLA logic minimiser on `largest.espresso`; many small set/cube structs | 1 | Large C code base, not worth porting; the pattern (lots of small structs with irregular lifetimes) is what a Rust parser or compiler exhibits anyway. |
| barnes | n-body, few allocations | N | Skip. |
| gs, lua, lean, z3, redis, rocksdb, linux | real programs | mixed | Not portable; simlin's compile is the local analogue of lean/z3. |
| larson, larson-sized | server simulation with objects freed by other threads | 100 threads | Only as a 1-thread degenerate case. |
| alloc-test (alloc-test1) | 100M iterations, up to 512k live objects, Pareto 80/20 size distribution with `maxItemSizeExp = 10` (up to 1 KiB), writes to every allocated byte | 1 (the `alloc-test1` row) | Straightforward: a few hundred lines (PRNG, Pareto tables, random position/random size loop). |
| sh6bench | SmartHeap: batches of malloc/realloc/free, half freed LIFO and half in reverse | 1 when invoked with 1 thread | Portable, ~400 lines. |
| sh8bench, xmalloc-testN, cache-scratch/thrash, mstress, rptest, glibc-thread, mleak | cross-thread frees, false sharing, producer/consumer | N | Skip; single-thread variants of mstress exist but its header says "do not use this test as a benchmark". |
| glibc-simple | malloc/free of 25, 100, 400 and 1600 blocks of 16, 32, 64 bytes, half FIFO half LIFO, 2M iterations | 1 for the main-arena part | Trivial to port (the C is 200 lines). This is the tcache/fastbin fast-path test. |
| malloc-large | 20 live buffers of 5 to 25 MiB, 2000 replacements, value-initialised (zeroed) | 1 | Trivial; on wasm it measures `memory.grow` and zeroing policy. |
| rbstress | Ruby string churn | N | Skip. |
| security | overflow/UAF detection | 1 | Not a performance benchmark. |

Ports of `cfrac`, `espresso` and `alloc-test` to Rust do not exist publicly.

### B.2 talc's methodology

Described in A.2. What is good: cheap to run, single-threaded, portable to wasm
as-is (the `wasm-perf` crate is exactly this with `std::alloc::alloc` calls), and
it isolates allocator cost from application cost. What to be careful about: the
score counts actions per second so it is dominated by whichever operation is most
common; realloc to a random size up to 3x is far more aggressive than `Vec`
doubling; the alignment distribution (25 percent above pointer size) is much
heavier than real Rust code; the microbenchmark uses `rdtsc` around single calls,
which does not exist on wasm; and heap efficiency measured to OOM in a fixed arena
does not translate to wasm's grow-only memory, where the right metric is peak
`memory.size()` after a workload (which the harness in Part C reports).

### B.3 glibc and jemalloc microbenchmarks

glibc's `benchtests/bench-malloc-simple.c` and `bench-malloc-thread.c` are the two
that mimalloc-bench imports (above). jemalloc has no upstream microbenchmark suite
worth porting; its published comparisons use the same mimalloc-bench programs.
rpmalloc's `rptest` (in mimalloc-bench) is a threaded stress with size ranges 16 to
16000 and is not single-threaded. The rlsf repository has a Criterion/FarCri stress
(`stress_common.rs`): fill a fixed array of live allocations, then repeatedly free
one and allocate a new one with size in `[min, min + mask]` and alignment
`4 << (rng & 3)`, ten size ranges from 1..8 to 128..255. That is a compact, wasm-
portable microbenchmark shape.

### B.4 Realistic Rust workloads that make good wasm benchmarks

- serde_json: `serde_json::from_str::<Value>` of a multi-megabyte document builds
  a tree of `Vec`, `String`, `Map` (BTreeMap by default) nodes; then `to_string`.
  Sizes are small and irregular, lifetimes are tree-shaped, frees come in bulk on
  drop. Also available as `nickb.dev`'s "serde optimization gauntlet" style loops.
- regex: compiling a large alternation (`regex::Regex::new` with hundreds of
  alternatives) allocates thousands of NFA/DFA states, then matching is nearly
  allocation-free; good for "compile then run" phases.
- resvg/usvg: `resvg-wasm` (`/home/bpowers/src/resvg-wasm/src/lib.rs`) does, per
  `render` call, `fontdb.load_font_data(font.clone())` for every registered font
  (a full copy of each font file: large `Vec<u8>` allocations plus memcpy), `usvg::
  Tree::from_str` (a tree of `Rc` nodes, path segment `Vec`s, text shaping through
  rustybuzz/ttf-parser), `tiny_skia::Pixmap::new(w, h)` (one `alloc_zeroed` of
  `w * h * 4` bytes), rasterisation, and `encode_png` (growing `Vec<u8>` through a
  deflate encoder). It is built with `-Zbuild-std` on nightly and `+simd128`, so
  16-byte-aligned `v128` allocations appear, which is the alignment that pushes
  dlmalloc-rs into `memalign`.
- simlin (`/home/bpowers/src/simlin`): `docs/design/engine-performance.md`
  measured the C-LEARN model (1.4 MB of MDL, 53k lines): parse 0.82M allocations,
  salsa compile 73M allocations churning 8.9 GiB while retaining only 3.3 MiB, with
  about 30 percent of compile instructions inside glibc malloc/free; the simulation
  loop was brought to zero allocations. Switching the native build to mimalloc cut
  compile from 2450 ms to 1459 ms (40 percent). The wasm bundle
  (`src/engine/build.sh`, `cargo build -p simlin --lib --release --target
  wasm32-unknown-unknown`, `--no-default-features` for the browser artifact) uses
  std's dlmalloc, is compiled at `opt-level = "z"` via
  `.cargo/config.toml` target rustflags, and is post-processed with `wasm-opt -O3`.
  Its JS side (`src/engine/src/internal/memory.ts`) creates a fresh view of
  `memory.buffer` for each access, which is the correct pattern given
  `memory.grow`. The engine has Criterion benches (`benches/compiler.rs`, backed
  by mimalloc natively) and a Node VM-versus-wasm eval benchmark
  (`src/engine/tests/backend-bench.ts`, gated by `RUN_BENCH=1`, median of warm
  iterations with `performance.now()`), which is the closest thing to an existing
  wasm allocator benchmark in the user's projects: the compile stage of that
  pipeline is the allocator-bound part.
- Other portable candidates: pulldown-cmark rendering a large Markdown file, `syn`
  parsing a large Rust file, `HashMap<u64, u64>` insert/remove churn (hashbrown
  reallocates its table on growth), `Vec<u64>` growth by push (amortised
  `realloc` doubling plus memcpy), and `String` building through `format!`.

### B.5 Baseline numbers from the survey harness

Random-actions workloads follow talc's wasm-perf shape (100k actions, sizes 1 to
10000, align `8 << min(tz/2, 3)`, 100-allocation floor). Figures are ns per
operation, 3 timed iterations after one warmup, one run each. "default" on wasip1 is
wasi-libc's C dlmalloc; on `unknown` it is dlmalloc-rs via std. `dlrs` is the
`dlmalloc` crate with the `global` feature.

| ns/op, V8 TurboFan (`node --no-liftoff`), wasm32-unknown-unknown | dlmalloc-rs (std) | dlrs | talc | rlsf | lol_alloc |
|---|---|---|---|---|---|
| random actions, no realloc | 43.9 | 43.1 | 22.2 | 27.9 | 77.9 |
| random actions, with realloc | 55.0 | 54.1 | 26.2 | 34.0 | 90.0 |
| 48-byte alloc+free LIFO churn (per call) | 6.5 | 4.9 | 5.1 | 7.1 | 1.3 |
| Vec<u64> push to 1M (per push) | 0.5 | 0.6 | 0.6 | 0.5 | 0.6 |
| HashMap<u64,u64> 100k insert+remove (per op) | 25.6 | 25.2 | 25.3 | 25.3 | 25.6 |
| 100k `format!` strings + join (per string) | 56.4 | 54.9 | 55.8 | 58.5 | 48.3 |
| `vec![0u8; 8 MiB]` (per alloc, memset-bound) | 62165 | 61796 | 59371 | 62425 | 62688 |
| peak `memory.size()` (64 KiB pages) | 247 | 247 | 251 | 322 | 280 |

| ns/op, wasmtime 48 Cranelift, wasm32-wasip1 | wasi-libc dlmalloc | dlrs | talc | rlsf | lol_alloc |
|---|---|---|---|---|---|
| random actions, no realloc | 41.2 | 42.1 | 18.0 | 26.8 | 82.6 |
| random actions, with realloc | 48.3 | 52.7 | 23.2 | 32.8 | 94.7 |
| 48-byte churn | 3.7 | 3.5 | 3.7 | 6.1 | 0.9 |

Native x86_64 glibc malloc for reference: 32.6 / 42.2 ns for the two random-action
cases, 6.0 ns churn. V8 Liftoff-only (`node --liftoff-only`): dlmalloc-rs 51.4 /
64.3, talc 28.0 / 32.9, and HashMap churn jumps from 25 to 169 ns per op, so the
baseline compiler penalises the application far more than the allocator. Wasmtime's
Winch baseline compiler gives 51.1 / 65.7 (dlmalloc) and 26.6 / 32.8 (talc),
essentially Liftoff's numbers.

Reading the table: the two dlmalloc implementations (C and Rust) perform the same;
talc halves the random-action cost everywhere; HashMap and string workloads are
insensitive to the allocator at these sizes because hashing and formatting
dominate; the 8 MiB zeroed allocation costs the same for everyone because everyone
memsets. The `small_churn` case is degenerate (pure LIFO) and rewards allocators
whose head-of-list path is shortest; mimalloc's page-local free list is that path
generalised to every size class and to non-LIFO order.

## Part C: running and timing wasm locally

### C.1 Toolchain inventory (verified)

- `rustc 1.95.0` with targets `wasm32-unknown-unknown`, `wasm32-wasip1`,
  `x86_64-unknown-linux-gnu`; no `rust-src` component (fetched std sources from
  GitHub tag `1.95.0` for this survey). Default wasm32 target features in 1.95:
  `bulk-memory`, `multivalue`, `mutable-globals`, `nontrapping-fptoint`,
  `reference-types`, `sign-ext`. `wasm-opt` therefore needs
  `--enable-bulk-memory --enable-bulk-memory-opt --enable-sign-ext
  --enable-mutable-globals --enable-nontrapping-float-to-int --enable-multivalue
  --enable-reference-types` or it rejects `memory.fill`/`memory.copy`.
- `~/.wasmtime/bin/wasmtime` 48.0.1 (Cranelift default; `-C compiler=winch` for the
  baseline compiler; `-O opt-level=0|1|2|s`; `wasmtime compile` produces `.cwasm`
  in 11 ms for a 100 KiB module; `wasmtime explore` and `wasmtime hot-blocks` for
  looking at generated code).
- `node` v22.22.2 (V8 12.4.254.21). `node:wasi` exists (Stability 1, experimental;
  `version: 'preview1'` is mandatory; `returnOnExit` defaults to true; prints an
  ExperimentalWarning unless `--no-warnings`; the docs warn it is not a security
  sandbox).
- `~/.cargo/bin/wasm-bindgen` and `wasm-bindgen-test-runner` 0.2.108,
  `~/.cargo/bin/wasm-tools`, `~/bin/wasm-opt` 125 (Binaryen), `clang` 22.1.8
  (no wasi-sdk sysroot installed).

### C.2 `cargo test --target wasm32-wasip1`

Set the runner and cargo does the rest; both of these were verified to run the
survey crate's test to completion:

```
CARGO_TARGET_WASM32_WASIP1_RUNNER="$HOME/.wasmtime/bin/wasmtime run" \
  cargo test --release --target wasm32-wasip1
```

```
# node's built-in WASI via a 7-line wrapper (see C.3 for the file)
CARGO_TARGET_WASM32_WASIP1_RUNNER="node --no-warnings $PWD/wasi.mjs" \
  cargo test --release --target wasm32-wasip1
```

Or put `[target.wasm32-wasip1] runner = ["/home/.../wasmtime", "run"]` in
`.cargo/config.toml` (rlsf's CI does exactly this). `std::time::Instant` works
under wasip1, so a `main.rs` benchmark can time itself and print. Caveat repeated
from the summary: the std allocator on wasip1 is wasi-libc's C dlmalloc, so
benchmark the `dlmalloc` crate explicitly (`features = ["global"]`,
`#[global_allocator] static A: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;`)
when dlmalloc-rs is the comparison target.

### C.3 Running the same wasip1 binary under node

```
// wasi.mjs
import { readFileSync } from 'node:fs';
import { WASI } from 'node:wasi';
const [file, ...args] = process.argv.slice(2);
const wasi = new WASI({ version: 'preview1', args: [file, ...args], env: process.env, returnOnExit: true });
const mod = await WebAssembly.compile(readFileSync(file));
const instance = await WebAssembly.instantiate(mod, wasi.getImportObject());
process.exitCode = wasi.start(instance);
```

`node --no-warnings wasi.mjs target/wasm32-wasip1/release/bench.wasm 3` ran the
survey benchmark and gave numbers within noise of the wasm32-unknown-unknown build
under the same V8 (talc 18.4 versus 22.2 ns; the wasip1 build uses the C dlmalloc
and a different std, so it is not an exact A/B). Node's WASI is fine for timing and
for tests; it is not a sandbox.

### C.4 wasm32-unknown-unknown with a tiny JS harness

The module imports one function for timing and exports plain `extern "C"`
entry points; no wasm-bindgen needed:

```rust
#[link(wasm_import_module = "env")]
extern "C" { fn now_ms() -> f64; }
#[no_mangle] pub extern "C" fn run_case(idx: usize, iters: usize, ops_out: *mut u64) -> f64 { ... }
#[no_mangle] pub extern "C" fn case_count() -> usize { ... }
#[no_mangle] pub extern "C" fn case_name(idx: usize, len_out: *mut usize) -> *const u8 { ... }
#[no_mangle] pub extern "C" fn memory_pages() -> usize { core::arch::wasm32::memory_size(0) }
```

```
// bench.mjs
const { instance } = await WebAssembly.instantiate(bytes, { env: { now_ms: () => performance.now() } });
```

Build with `cargo build --release --target wasm32-unknown-unknown --lib` and a
`crate-type = ["cdylib"]`. Strings come back as (ptr, len) into `memory.buffer`;
re-create the `Uint8Array` view after every call into wasm because any allocation
may have grown memory and detached the old view (measured below).

For `#[test]`s on this target use wasm-bindgen-test. Verified command (a
`wasm-bindgen = "=0.2.108"` pin is required so the crate matches the installed
CLI; `wasm-bindgen-test` resolved to 0.3.58):

```
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
WASM_BINDGEN_TEST_ONLY_NODE=1 cargo test --target wasm32-unknown-unknown
```

Do not pass `-q`/`--quiet` to cargo here; cargo forwards it to the runner, which
rejects it.

### C.5 Forcing V8 tiers

Defaults on this V8: `--liftoff`, `--wasm-tier-up`, `--wasm-dynamic-tiering`
(budget 13,000,000), `--wasm-lazy-compilation`, `--no-turboshaft-wasm`. Verified
with `--trace-wasm-compilation-times`:

- Optimizing compiler only: `node --no-liftoff bench.mjs ...` (every function is
  compiled by TurboFan, lazily on first call; add `--no-wasm-lazy-compilation` to
  compile eagerly, which moved instantiate from 0.3 ms to 4.4 ms for the 23 KiB
  module and changed no steady-state number).
- Baseline only: `node --liftoff-only bench.mjs ...` (36 Liftoff compilations, 0
  TurboFan). `--liftoff --no-wasm-dynamic-tiering --no-wasm-tier-up` is
  equivalent.
- `--liftoff --no-wasm-tier-up` alone does NOT give baseline numbers on Node 22:
  dynamic tiering still promotes hot functions, and the run matched TurboFan to the
  decimal. The v8.dev "compilation pipeline" page still documents the older
  two-flag recipe; on this version `--liftoff-only` is the reliable one.
- Default (dynamic tiering) reaches TurboFan numbers within the first warmup
  iteration of any workload here, so for steady-state benchmarking the default and
  `--no-liftoff` agree; `--liftoff-only` is the number that matters for
  short-lived modules and for the first ~13 MB of executed code.
- `--predictable` and `--single-threaded` exist for determinism; not needed for
  these microbenchmarks.

### C.6 Is wasmtime a reasonable proxy for browser JITs?

For code quality, yes. Cranelift's generated code is generally within a few percent
of TurboFan (Frank Denis' 2023 runtime comparison put it about 2 percent slower);
in this survey wasmtime was 5 to 15 percent faster than V8 TurboFan on the
allocator loops and ranked the five allocators identically in every case. Winch
behaves like Liftoff. Where wasmtime is not a proxy: `memory.grow` cost (next
section), JS boundary costs (irrelevant to the allocator itself), and lazy/tiered
compilation effects. Recommendation: wasmtime as the deterministic, fast CI
benchmark runner (AOT `.cwasm`, no warmup, no JIT noise) with a periodic V8 run in
both `--no-liftoff` and `--liftoff-only` modes.

### C.7 How the existing crates run wasm CI and benchmarks

- dlmalloc-rs: `cargo test --target wasm32-wasip1` under wasmtime
  (`bytecodealliance/actions/wasmtime/setup`), `cargo build --target
  wasm32-unknown-unknown` in debug and release, Miri, fuzz build.
- rlsf: `cargo test --target wasm32-wasi` under wasmtime via `.cargo/config.toml`
  runner, including with the `std` feature.
- talc: `cargo check` on wasm32 in CI only; wasm functional tests via
  wasm-bindgen-test-runner run manually; benchmarks via `wasm-pack build --target
  deno` plus `deno run` with `performance.now()`; size via `RUSTFLAGS="-C lto -C
  embed-bitcode=yes -C linker-plugin-lto" cargo +nightly build --release` then
  `wasm-opt -Oz` and `wc -c`.
- lol_alloc: `wasm-pack test --node` (wasm-bindgen-test) plus native tests with a
  fake `MemoryGrower`; size via `wasm-pack build --release example && wc -c`.

### C.8 The cost of `memory.grow`, measured

From JavaScript (`WebAssembly.Memory.prototype.grow`) and from inside wasm (a
`.wat` loop calling `memory.grow` n times), Node 22:

| V8, per `memory.grow` call | microseconds |
|---|---|
| grow by 1 page, from 1 page, 4000 times (JS) | 74.3 |
| grow by 16 pages, 1000 times (JS) | 71.3 |
| grow by 256 pages, 100 times (JS) | 77.6 |
| grow by 1 page from 4096 pages (JS) | 72.9 |
| in-wasm `memory.grow 1`, no JS view ever created | 71.1 |
| in-wasm `memory.grow 1`, after a JS view was created | 54.6 |
| in-wasm under `--no-liftoff` | 52.4 to 55.1 |

The cost is flat in the number of pages requested, so it is per-call overhead
(backing-store growth, detaching the `ArrayBuffer`, runtime call), not copying.
Wasmtime: about 0.5 microseconds per grow (4000 grows added 2 ms to a 2.4 ms
process). Every Rust allocator surveyed grows by exactly the pages the failing
request needs, so a program whose live set climbs from 1 to 64 MiB pays roughly
1000 grows, about 70 ms of pure `memory.grow` in a browser. Doubling (or growing
by 50 percent with a 1 MiB floor) turns that into about 10 calls. `memory.grow`
also detaches every `Uint8Array`/`DataView` over `memory.buffer`; wasm-bindgen's
generated glue re-fetches `memory.buffer` on each access for this reason, but user
code that caches a view (or a `TypedArray::view` created before an allocation) reads
zeros afterwards (wasm-bindgen issue #4395, discussion #3802).

### C.9 Recommended harness design for this project

One benchmark crate (`bench/`, `publish = false`) with:

- `lib.rs`: workloads as `fn() -> usize` returning an op count, in the shape of
  the survey harness: talc-style random actions with and without realloc; the
  `glibc-simple` FIFO/LIFO batches at 16/32/64 bytes; `alloc-test`'s Pareto
  80/20 loop with up to 512k live objects; `malloc-large`'s 5 to 25 MiB
  replacement with zeroing; `Vec<u64>` doubling; `HashMap` churn; `String`
  building; a serde_json `Value` round-trip of a bundled 1 to 4 MB document; a
  regex compile; and, behind a feature, a resvg render of a bundled SVG and a
  simlin C-LEARN parse plus compile (the latter two as separate crates that depend
  on the real libraries). Each records ns per op and peak `memory.size()`.
- Allocator selection by mutually exclusive cargo features (`alloc-dlmalloc`,
  `alloc-talc`, `alloc-rlsf`, `alloc-ours`, and no feature for the std default),
  each a `#[global_allocator]` static gated on `target_arch = "wasm32"` so the
  native build always uses the system allocator as the sanity baseline.
- Two entry points: `main.rs` for native and wasip1 (times with `Instant`, prints
  a table) and a `cdylib` export surface for wasm32-unknown-unknown timed through an
  imported `now_ms`. A `bench.mjs` and `wasi.mjs` as above; a `run.sh` that builds
  every feature for every target, runs wasmtime (`-O opt-level=2`, and `-C
  compiler=winch` as the baseline-compiler proxy), `node --no-liftoff`,
  `node --liftoff-only`, and the native binary, and emits one CSV row per
  (allocator, runtime, workload) with ops, ns per op, ops per second, and peak
  pages. Report the median of 5 timed iterations after 1 warmup per case; wasmtime
  runs are stable enough that a single process per configuration suffices.
- Code size as a first-class metric via a no_std `sizeprobe` cdylib (exports
  `alloc`, `alloc_zeroed`, `dealloc`, `realloc`; `opt-level = "z"`, `lto`,
  `panic = "abort"`, `strip = true`) built per allocator and passed through
  `wasm-opt -Oz` with the feature flags listed in C.1. Measured today: none 156,
  lol_alloc 709, rlsf-small 1202, talc (no realloc) 1403, talc 1616, rlsf 2108,
  dlmalloc 5410 bytes.
- Build the allocator at `opt-level = 3` even when the consumer builds at `-Oz`
  (simlin does) by documenting `[profile.release.package.<crate>] opt-level = 3`;
  the benchmark should cover both to make sure the fast path survives `-Oz`.
- Correctness gates alongside: `cargo test --target wasm32-wasip1` under wasmtime
  for the allocator's own tests, a wasm-bindgen-test suite on wasm32-unknown-unknown
  with talc-style randomised fill/verify (`talc/tests/wasm_sources.rs` is a good
  template, including its page-boundary regression tests), Miri on the native build
  with a fake `memory.grow`, and a fuzz target in the style of dlmalloc-rs and talc.

## What it takes to be the best

### Where dlmalloc-rs loses time (from the code)

1. Every `free` coalesces immediately: read the freed chunk's head, read the next
   chunk's head, possibly read `prev_foot`, unlink one or two neighbours from
   doubly-linked lists (or from a trie for chunks over 256 bytes), write the new
   head and foot, then insert into a bin and update a bitmap. That is 6 to 12
   dependent memory operations before the allocator has done anything useful, and
   it destroys the locality that would let the next allocation of the same size
   reuse the same bytes.
2. `malloc` for small sizes hits the exact bin only when the previous free of that
   exact size has not already been coalesced away; otherwise it splits `dv` or
   `top`, writing two headers, and pays the trie walk (`tmalloc_small`) when the
   small bins are empty.
3. `validate_size` runs two asserts on every `free` and `realloc` in release.
4. The 8-byte natural alignment sends every 16-byte-aligned request through
   `memalign` (over-allocate, two `dispose_chunk` calls).
5. `calloc` always memsets, even fresh pages.
6. Growth is in 64 KiB steps sized to the request, so a growing heap calls
   `memory.grow` hundreds of times (C.8).
7. The whole thing is 5.4 KiB of generic pointer-chasing code ported from C, with
   `Chunk::*` helpers that compile to loads of `head` words scattered across the
   heap rather than reading one page header.

### Where talc loses time (from the code)

1. Allocation computes a bin (ilog2 plus shifts), scans a `u64`, pops a node, then
   `register_gap`s the remainder: four word writes plus a bitfield update, and on
   wasm those two helpers are out-of-line calls by design.
2. Deallocation reads the chunk's tail tag, reads the word below the chunk (to see
   if a gap ends there) and, if ABOVE_FREE, the gap above; each coalesce is an
   unlink plus a bin read. Then one `register_gap`. Like dlmalloc it immediately
   coalesces and so never keeps a free block of the right size ready for the next
   request of that size.
3. No size classes: a request for 24 bytes and one for 40 bytes are first-fit in
   the same 16-byte-granular bins and split remainders back into the lists, which
   is where the 10 to 15 percent memory overhead over dlmalloc comes from.
4. Alignment above 16 falls back to a linear scan of a bin (issue #44 made this
   pathological; the fix only narrows it).
5. `alloc_zeroed` always memsets; growth is exactly-sized; nothing is ever
   returned.

Talc is a very good malloc in the dlmalloc/TLSF family and is currently the bar:
roughly 20 ns per random small allocation or free in V8 TurboFan, 18 under
Cranelift, at 1.6 KiB.

### Theoretical headroom for a mimalloc-style design on wasm

mimalloc's fast path is: `size -> bin` through a direct table for sizes up to 1 KiB
(one shift and one load), pop the page-local free list (one load, one store), and
on `free`, mask the pointer to find the page (segments are aligned so this is an
`and`), then push onto the page's local free list (two stores) with a periodic
`collect` that moves `local_free` to `free`. There is no coalescing on the hot
path, no neighbour reads, and no header on small blocks. Natively that is 10 to 20
instructions per operation and the reason simlin's compile got 40 percent faster
just by swapping allocators. On wasm the following are specifically favourable:

- Single thread means no thread-local storage lookup and no atomic
  `thread_free` list: the heap is a `static`, the free path is the local one, and
  the `thread_id` check disappears.
- Wasm pages are 64 KiB, exactly mimalloc's small page size, and `memory.grow`
  returns page-aligned memory, so page-header lookup by masking needs no
  over-allocation for alignment; a "segment" can simply be a run of pages with a
  side table indexed by `addr >> 16` (4 GiB / 64 KiB = 65536 entries of a small
  integer or pointer, allocated lazily) instead of mimalloc's 4 MiB-aligned
  segments, which on wasm would otherwise waste up to 4 MiB on alignment (the
  emscripten and WASI `prim.c` files both live with that waste).
- Rust hands the allocator the `Layout` on `dealloc` and `realloc`. Size selects
  the bin without touching the page header (as Zig's WasmAllocator does), and
  alignment tells the allocator whether a block could have been over-aligned. The
  header read can be skipped entirely on the free fast path; the page is still
  needed to find the free list, but that is an address computation, not a load of
  hot heap data.
- `alloc_zeroed` can skip the memset for blocks carved from never-used pages, as
  mimalloc does with `is_zero` and Go with `needzero`. Fresh `memory.grow` pages
  are zero by specification, which is why mini-alloc's `alloc_zeroed` is 100x
  cheaper than std's in the Stylus measurements.
- Geometric `memory.grow` (double up to some cap, then fixed large steps) removes
  most of the 50 to 75 microsecond grow calls in browsers, and reserving the
  `__heap_base..memory.size()` gap as the first page run (dlmalloc-rs 0.2.13 and
  mini-alloc do this) avoids the first grow altogether.

The measured gap to close: talc at about 20 ns and dlmalloc at about 45 ns per
random action in TurboFan versus a plausible 8 to 12 ns for a size-class allocator
whose fast path is a handful of loads and stores; on the pure LIFO churn case the
existing allocators already reach 4 to 6 ns, and lol_alloc's degenerate 1 ns shows
what an inlined bump-like path costs, so the target for small sizes is single-digit
nanoseconds in both LIFO and random order. Medium sizes (1 KiB to 64 KiB) need
mimalloc's medium pages or a segregated run allocator; a `Vec` doubling past 64 KiB
will still be realloc-copy dominated (0.5 ns per push in every allocator here).

### Pitfalls others have hit

- `memory.grow` cost and frequency (C.8); growth policy must be geometric and the
  allocator should expect `memory.grow` to fail (return null so std can call
  `handle_alloc_error`) at the 4 GiB limit or a host-imposed maximum.
- View invalidation after growth for any JS holding `memory.buffer` views
  (wasm-bindgen #4395, Go's `resetMemoryDataView`); document it for users of the
  allocator, since an allocator that grows more aggressively makes it fire earlier
  (though less often).
- Alignment handling: dlmalloc's 8-byte natural alignment slow path; talc's #44
  pathological over-aligned search; talc 5.0.4's tag/size byte collision on gaps
  over 16 MiB; rlsf's `UsedBlockPad` needing 16 extra bytes for align 16. Rust
  `Layout` alignments go up to 2^29; SIMD `v128` is the common 16-byte case;
  `u128` is align 8 on wasm32 (rust-lang/rust#133991) and could change.
- Realloc: a size-class design cannot grow in place except within the class, so
  the realloc-heavy talc benchmark will favour boundary-tag allocators; mimalloc
  handles this by returning the same pointer when `new_size <= usable_size` and by
  cheap copies otherwise, and Rust's `Vec` doubling amortises the copies. Consider
  in-place growth only for the last block of the heap or for huge blocks that own
  whole page runs.
- Exactly-sized growth interacting with fragmentation: talc's `WasmGrowAndClaim`
  used 10x memory in a ratchet pattern because new pages could not merge with a
  freed tail (#51); any design with separate page runs must be able to reuse freed
  runs for larger requests or grow contiguously.
- Undersized growth loops: talc 5.0.3 requested one page for a 65520-byte
  allocation whose chunk needed two, claimed 65k heaps and hit the 4 GiB limit.
  Always size growth from the rounded, aligned request plus metadata.
- Zero-size allocations are never passed to `GlobalAlloc::alloc`, but
  `alloc_zeroed` and `realloc` to sizes below the block's class and back are.
- Wasm has no `memmove` cheaper than `memory.copy`, and `memory.fill` is the
  memset; both are bulk-memory instructions (default on in Rust 1.95, and the
  reason `wasm-opt` needs the feature flags).
- Code size discipline: talc keeps two helpers out of line on wasm because the
  inlined version was larger; simlin builds at `-Oz`, which will compile the
  allocator's fast path without inlining unless the crate is pinned to
  `opt-level = 3` in the consumer's profile. Under Liftoff there is no inlining at
  all, so the fast path should be a single function with no helper calls.
- The last 64 KiB of the address space: dlmalloc-rs fakes it as 16 bytes short so a
  chunk end never wraps to address 0; any page table indexed by `addr >> 16` must
  handle page index 65535 and `memory.grow` returning `usize::MAX`.
- Provenance: `memory.grow` returns a page index, not a pointer; lol_alloc's README
  flags that strict-provenance rules cannot be followed. Use `ptr::with_exposed_
  provenance` or `wrapping_add` from a base pointer consistently, and run Miri on
  the native build with a fake grower, as dlmalloc-rs and talc do.

## Links

- dlmalloc-rs: https://github.com/alexcrichton/dlmalloc-rs ; std wiring:
  https://github.com/rust-lang/rust/blob/1.95.0/library/std/src/sys/alloc/wasm.rs
- talc: https://github.com/SFBdragon/talc (BENCHMARKS.md, BENCHMARKS_WASM.md,
  talc/README_WASM.md, issues #44, #49, #51)
- lol_alloc: https://github.com/Craig-Macomber/lol_alloc
- wee_alloc: https://github.com/rustwasm/wee_alloc ; RUSTSEC-2022-0054:
  https://rustsec.org/advisories/RUSTSEC-2022-0054.html ; issue #106
- rlsf: https://github.com/yvt/rlsf
- emballoc: https://github.com/jfrimmel/emballoc
- frusa: https://github.com/moturus/motor-os (crate frusa)
- bumpalo: https://github.com/fitzgen/bumpalo
- mimalloc crate: https://github.com/purpleprotocol/mimalloc_rust ; upstream:
  https://github.com/microsoft/mimalloc (src/prim/wasi/prim.c,
  src/prim/emscripten/prim.c); issue #1167
- rpmalloc-rs: https://github.com/EmbarkStudios/rpmalloc-rs ; snmalloc-rs:
  https://github.com/SchrodingerZhu/snmalloc-rs (archived; upstream microsoft/snmalloc)
- ferroc: https://github.com/js2xxx/ferroc
- mini-alloc: https://crates.io/crates/mini-alloc ; alloc_cat:
  https://crates.io/crates/alloc_cat ; lite-alloc: https://crates.io/crates/lite-alloc
- emmalloc: https://github.com/emscripten-core/emscripten/blob/main/system/lib/emmalloc.c
- Zig WasmAllocator: https://github.com/ziglang/zig/blob/master/lib/std/heap/WasmAllocator.zig ;
  PR https://github.com/ziglang/zig/pull/13513
- Go runtime: https://github.com/golang/go/blob/master/src/runtime/mem_sbrk.go ,
  mem_wasm.go , internal/runtime/gc/sizeclasses.go
- mimalloc-bench: https://github.com/daanx/mimalloc-bench
- V8 compilation pipeline: https://v8.dev/docs/wasm-compilation-pipeline ;
  Liftoff: https://v8.dev/blog/liftoff
- Node WASI: https://nodejs.org/docs/latest-v22.x/api/wasi.html
- wasm-bindgen view invalidation: https://github.com/wasm-bindgen/wasm-bindgen/issues/4395
- u128 alignment on wasm: https://github.com/rust-lang/rust/issues/133991
- nickb.dev on allocators and wasm size:
  https://nickb.dev/blog/avoiding-allocations-in-rust-to-shrink-wasm-modules/
- Survey harness sources (scratchpad, not committed):
  /tmp/claude-1000/-home-bpowers-src-wasm-clalloc/87b4b542-caa2-4000-a444-e794ecee204d/scratchpad/wasmbench
  and .../sizeprobe
