# wasmalloc Design

## Summary
<!-- TO BE WRITTEN after the research phase completes -->

## Definition of Done

Deliverable: a pure-Rust `no_std` crate `wasmalloc` providing a `#[global_allocator]` for
single-threaded wasm32 (`wasm32-unknown-unknown` and `wasm32-wasip1`/`p2`), following mimalloc
v3's design (size-class bins, per-page free lists, lazy page extension, page retirement, 64 KiB
slice management over `memory.grow`), plus:

- a benchmark harness (node/V8 with tier control, wasmtime, native baseline);
- a test suite: model-based property and fuzz tests that run natively and on wasm; Miri-clean core;
- formal verification from the start: Kani (or comparable) harnesses for the core invariants and
  arithmetic, heavier provers where the code is pure, and a soundness ledger in which every
  `unsafe` block not covered by a machine-checked proof carries a pen-and-paper proof of
  correctness that has been adversarially reviewed by a fresh sub-agent.

Success: on the benchmark suite under V8's optimizing tier it beats std's dlmalloc on every
workload and beats talc in aggregate, with the small-object alloc+free fast path within about
1.5x of the measured roofline floor; peak `memory.size` stays within about 1.5x of dlmalloc's so it
is safe as a default; fuzzing and verification find no correctness bugs.

Out of scope: multi-threaded wasm, native production use, a C malloc API, and RSS or code-size
minimization as goals (smaller is welcome, not required).

## Acceptance Criteria
<!-- TO BE WRITTEN -->

## Glossary
<!-- TO BE WRITTEN -->

## Architecture (draft v0, written from the lead's reading of mimalloc v3.5.1; to be revised after research)

### Guiding observations

1. Rust's `GlobalAlloc` passes the `Layout` (size and alignment) to `dealloc` and `realloc`.
   mimalloc's C `free(p)` must recover the page from the pointer via a page map or aligned
   metadata; we can instead derive the page *kind* (small, medium, singleton) from the size and
   find the page header with a single address mask. This removes the page map from every hot
   path and removes headers entirely for huge blocks.
2. There is one thread. Everything mimalloc does for cross-thread frees (`xthread_free`, page
   abandonment, ownership bits, `tld`, `subproc`, atomics, locks, delayed free) is deleted, not
   stubbed.
3. wasm linear memory is flat, starts at 0, only grows, and grows in 64 KiB pages that are
   guaranteed zero. There is no decommit and no unmap, so footprint control is entirely about
   reusing slices and pages. The "OS" is one `memory.grow` instruction.
4. V8 is the primary engine. Fast paths must be tiny, straight-line, and inlinable into the
   `__rg_alloc`/`__rg_dealloc` shims; slow paths are `#[cold] #[inline(never)]`. Under V8's
   Liftoff baseline tier there is no inlining at all, so each fast path must be one function
   with no helper calls, and it must survive a consumer building at `opt-level = "z"` (simlin
   does); the crate documents `[profile.release.package.wasmalloc] opt-level = 3`.
5. The bar, measured under V8 TurboFan (`docs/research/landscape.md` B.5): std's dlmalloc at
   about 44 ns and talc at about 22 ns per random small alloc or free, 5 to 6 ns on pure LIFO
   churn. A size-class fast path of a handful of loads and stores should land in single-digit
   nanoseconds for both LIFO and random order; the roofline harness pins the exact floor.

### Memory layout

- The heap is the linear memory from `align_up(__heap_base, 16)` to `memory.size() * 64 KiB`,
  extended by `memory.grow`. It is managed as an array of 64 KiB *slices* (mimalloc's
  `MI_ARENA_SLICE_SIZE` on 64-bit; we deliberately do not adopt the 32 KiB slices mimalloc
  uses on 32-bit hosts, because the wasm page is 64 KiB).
- A bitmap indexed by absolute slice number (`addr >> 16`; 65536 bits, 8 KiB, static) records
  free slices; a companion bitmap records slices known to be zero (never handed out since
  `memory.grow`). This is mimalloc's `slices_free` / `slices_dirty` pair without atomics.
  Run search must honour alignment: 1 slice anywhere (lowest first), 8-slice runs at 8-slice
  boundaries (a whole byte), 64-slice runs at 64-slice boundaries (a whole `u64` word),
  singleton runs first-fit. mimalloc v3's per-chunk kind binning (`mi_bbitmap_t`, which keeps
  small pages from fragmenting the runs medium and large pages need) is deferred until
  footprint tests show fragmentation; the heap's growing top edge is always available for
  aligned runs, so the failure mode is footprint, not correctness.
- Growth policy: `memory.grow` costs 50 to 75 microseconds per call in V8 regardless of the
  number of pages requested (about 100x wasmtime; measured in `docs/research/landscape.md`
  C.8), so growth is geometric: grow by `max(needed, clamp(heap_size / 2, 1 MiB, 64 MiB))`,
  rounded to whole slices, and reclaim the linker gap between `__heap_base` and the initial
  `memory.size()` as the first free slices instead of paying a grow for the first page (std's
  dlmalloc 0.2.11 wastes that gap). Growth must be sized from the rounded, aligned request
  (talc 5.0.3 undersized growth and spun to the 4 GiB limit) and must tolerate `memory.grow`
  returning `usize::MAX` (failure at the 4 GiB or host limit: return null so std reports OOM)
  and non-contiguous results (something else grew memory in between): the returned page index
  is the start of the new region, never an assumption of contiguity.

### Page kinds (derived from the block size class, hence from the Layout)

| kind      | page size and alignment                | block sizes                     | header        | page lookup at dealloc                 |
|-----------|----------------------------------------|---------------------------------|---------------|----------------------------------------|
| small     | 1 slice (64 KiB), 64 KiB-aligned       | 8 B .. 10240 B                  | at page start | `ptr & !(64 KiB - 1)`                  |
| medium    | 8 slices (512 KiB), 512 KiB-aligned    | 10240 B .. 81920 B              | at page start | `ptr & !(512 KiB - 1)`                 |
| large     | 64 slices (4 MiB), 4 MiB-aligned       | 81920 B .. 512 KiB              | at page start | `ptr & !(4 MiB - 1)`                   |
| singleton | `ceil(size / 64 KiB)` slices, 64 KiB-aligned (or aligned to `align` when `align > 64 KiB`) | > 512 KiB, or any size with `align > 4 KiB` | none | `ptr` is the run start; slice count is recomputed from the Layout |

These are mimalloc's 64-bit constants (`MI_SMALL_PAGE_SIZE`, `MI_MEDIUM_PAGE_SIZE`,
`MI_LARGE_PAGE_SIZE` and the corresponding `*_MAX_OBJ_SIZE` bounds, with the medium bound
snapped down to the 81920-byte bin). mimalloc on 32-bit halves everything to save address
space; wasm32 has a flat 4 GiB space and a 64 KiB grow unit, so the 64-bit values fit better.
Large pages are enabled by default but controlled by one constant: mimalloc's own source
questions them (random sizes between 64 KiB and 512 KiB can leave many partially used 4 MiB
pages), and mimalloc v3.5.1 does not actually align large pages to 4 MiB, so our slice
allocator must search for aligned runs (a large page is exactly one 64-bit word of the slice
bitmap, which makes that search trivial).

Singleton runs carry no header at all. The Layout at `dealloc` and `realloc` gives the run
length, and the slice bitmaps carry the free and known-zero state, so a header would only
duplicate information. This also means a singleton allocation with any alignment up to
64 KiB is satisfied by slice alignment with zero padding.

### Size classes

mimalloc's bins: exact classes at 8-byte granularity up to 64 bytes, then four classes per
power of two (12.5% worst-case internal waste), 73 bins up to the medium limit. Key property
(proved in `bins`): for any size `s` that is a multiple of a power of two `A <= 4096`, the bin
size of `s` is also a multiple of `A`. Combined with placing the first block of a page at
`align_up(header_size, min(4096, largest_power_of_two_dividing(block_size)))`, every block in
a page is aligned to every alignment its size class can be asked for. Therefore:

- `Layout { size, align <= 8 }` maps directly to bin(size);
- `Layout { size, 8 < align <= 4096 }` maps to bin(round_up(size, align)) and is aligned by
  construction, with no over-allocation and no interior pointers;
- `align > 4096` goes to a singleton run (64 KiB-aligned; larger alignments align the run).

The small fast path indexes `pages_direct[(size + 7) >> 3]` (129 entries for sizes up to
1 KiB, mimalloc's `pages_free_direct`) and pops `page.free`. No bin computation on the fast
path; `bin()` (clz-based, as in mimalloc) runs only in the slow path.

### Page header (small, medium and large pages; at the start of the first slice)

About 32 bytes: `free: u32` (intrusive LIFO list head, links stored in the first word of
each free block), `used: u16`, `capacity: u16`, `reserved: u16` (a small page of 8-byte
blocks holds 8188 blocks, so u16 suffices for every kind), `block_size: u32`,
`block_start: u16`, `bin: u8`, `flags: u8` (in-full-queue), `retire_expire: u8`,
`free_is_zero: bool`, and queue links `next: u32`, `prev: u32`. The first block starts at
`block_start`, which is `align_up(header_size, min(4096, block_size & block_size.wrapping_neg()))`
so that every block is aligned to every alignment its size class can be asked for.

Design decision to settle by benchmark (both are cheap to implement behind one interface):
(a) mimalloc's two lists (`free` consumed by malloc, `local_free` receiving frees, migrated
when `free` empties, which also feeds the retire and collection heartbeat), or (b) a single
LIFO `free` list with `free_is_zero` cleared on push. (b) has one fewer branch and reuses the
hottest block first; (a) preserves mimalloc's tested behaviour. Start with (b), measure.

### Fast paths (target: within 1.5x of the roofline free-list floor)

alloc: two compares (size, align), one load from `pages_direct`, one load of `page.free`, a
null test, one load of `block.next`, two stores. Slow path: lazy extension of the page's
never-used region (mimalloc's `mi_page_extend_free`, at most 8 KiB of blocks per extension),
then next-fit search of the bin's page queue with mimalloc's candidate heuristics, then a
fresh page from the slice bitmap, then `memory.grow`.

dealloc: derive kind from the Layout, mask to the page header, push onto `page.free`,
decrement `used`; if `used == 0` retire (mimalloc's `MI_RETIRE_CYCLES` scheme so a page that
oscillates between empty and one block is not freed and re-acquired); if the page was full,
move it back to its bin queue. Singleton: return slices to the bitmap (a shrink that
actually frees memory for reuse).

alloc_zeroed: pop; if `page.free_is_zero` (the page has never had a block freed into it) only
the free-list link word needs clearing, otherwise `memory.fill` exactly `layout.size()` bytes
(not the whole block as C mimalloc must). For singletons, skip the fill when every slice of
the run is still in the known-zero bitmap. Fresh wasm memory is zero, so zeroed buffers from
fresh memory cost nothing extra. Note that C mimalloc's wasi build never learns memory is zero
and always memsets; this is one of the places we can beat it.

realloc: block size is a pure function of the Layout, so shrink and grow-in-place decisions
need no header access; in-place is allowed only when the page kind is unchanged (the next
`dealloc` will recompute the kind from the new Layout). Singleton runs shrink in place by
freeing tail slices and grow in place when the following slices are free.

### Crate structure

- `wasmalloc` (no_std): `bins` (pure math, verified), `slices` (bitmap and growth), `page`,
  `heap` (queues, `pages_direct`, retire), `alloc` (the `GlobalAlloc` impl and fast paths),
  `backend` (a `Memory` trait implemented by `memory.grow` on wasm32 and by a 4 MiB-aligned
  simulated linear memory on the host for tests, Kani, Miri and fuzzing). Addresses are
  `usize` slice arithmetic derived from one base pointer with `wrapping_add`/`map_addr`
  (`memory.grow` returns a page index, not a pointer), so Miri and Kani can follow provenance.
  Edge cases the slice layer must handle: slice index 65535 (the last 64 KiB, where an end
  address would wrap to 0) and `Layout` alignments up to 2^29.

Note for benchmarking: on `wasm32-wasip1` std's default allocator is wasi-libc's C dlmalloc,
not dlmalloc-rs; only `wasm32-unknown-unknown` measures the allocator browsers see.
- `bench/`: the roofline harness grown into the benchmark suite (allocator selected by feature).
- `fuzz/` and `verify/`: differential fuzzing against a model; Kani harnesses.
- `docs/soundness-ledger.md`: the fallback for `unsafe` blocks that formal verification cannot
  reach. The default is a machine-checked proof (Kani harness or equivalent) per unsafe block;
  only blocks without one get a ledger entry with preconditions, pen-and-paper proof, and the
  adversarial reviewer's sign-off.

## Implementation status (2026-09-02)

Everything in the architecture above is implemented on `main` and verified as follows.

| module | what landed | verification |
|---|---|---|
| `bins` | mimalloc's 60 bins with an 8-byte word, `classify(Layout)`, `block_start` alignment rule | exhaustive tests over every size; 4 Kani harnesses (tightness, monotonicity, waste bound, alignment by construction) |
| `backend` | `Memory` trait in slices; `WasmMemory`; `SimMemory` with non-contiguous growth and a 4 MiB-aligned host `Region` | tests; used by every other module's tests, Miri and Kani |
| `slices` | free and known-zero bitmaps, lowest-first aligned run search with dedicated 1/8/64-slice scans, `acquire` with geometric growth sized from the current end, last slice of a 4 GiB memory never used | 18 tests incl. a model check; 9 Kani harnesses (3 in the quick gate) |
| `page` | 32-byte in-band header (48 on the host), `pop`/`push`/`extend`, `header_of` mask | 20 tests; Miri clean under Stacked and Tree Borrows; 4 Kani harnesses over a proof-only memory; ledger PAGE-01..06 |
| `heap` | bin queues, direct table with a read-only sentinel page, candidate search, full queue, retirement and collection, header-less runs, in-place realloc within a kind or a run, OOM collect-and-retry | 14 tests incl. randomised churn with content checks and a full invariant validator; no Kani harnesses yet; no ledger entries yet |
| `global` | `WasmAlloc`: `GlobalAlloc` over a static heap; refuses the `atomics` feature | end-to-end wasm32 test under wasmtime with std collections |
| `testing` | model-based differential tester with six profiles, mutant tests, cargo-fuzz targets for System and the heap | all profiles pass against the heap; 208k fuzz runs clean |

Deviations from the draft: the free list is a single LIFO list (mimalloc's `local_free` was
dropped as planned); the first block of a page starts at 64 bytes, not 32, so the host and
wasm32 share one geometry; large (4 MiB) pages are enabled.

First measurements (node 22, V8 optimizing tier, median ns per operation) against the roofline
harness's size-class floor and the incumbents: alloc+free of 32 bytes 3.73 (floor 1.78, talc
11.8, dlmalloc 8.0); random churn with 10k live objects 7.1 (floor 3.5, talc 31.9, dlmalloc
61.4). Where we lose: a 16 B to 1 MiB realloc chain costs 12.3 us against 0.08 us for talc,
because size-class pages cannot grow in place while boundary-tag allocators extend the top chunk;
and 256 KiB to 4 MiB alloc+touch+free is 25 percent slower than dlmalloc. The full matrix
(engines, tiers, footprint) is being measured; see `docs/research/roofline.md` when it lands.

## Roadmap and research directions

The goal is not to reproduce mimalloc in Rust but to be the best allocator for this target. Ideas
to try, each behind a benchmark and a proof, roughly in order of expected payoff:

1. **Header-less runs above the medium limit.** Disable large pages so every block above 80 KiB
   is a slice run, then grow runs in place: `try_extend` when the following slices are free, and
   when the run sits at the end of the heap, grow linear memory and extend without copying. This
   is how boundary-tag allocators win the realloc chain, and a growing `Vec<u8>` output buffer is
   the most common large-allocation pattern in wasm programs.
2. **Fold `free_is_zero` into the flags byte.** The dealloc fast path currently stores a byte on
   every free; the flags byte is already loaded, so the clear can move to the slow path and run
   once per page.
3. **Shrink the fixed footprint of small heaps.** The batch profiles show peak footprint 18 to
   29x peak live bytes when only 1 MiB is live: one 64 KiB page per touched bin, 512 KiB medium
   pages for a single 20 KiB object, and retired pages kept for 16 collection rounds. Options:
   a 256 KiB medium page (mimalloc's own 32-bit constant), faster release of retired pages while
   the heap is small, and reclaiming the partial slice below the first page for metadata.
4. **Bump allocation in fresh pages.** mimalloc found no gain natively (page.c:627); under V8
   the tradeoff between a free-list pop and a bump-and-compare may differ. Measure.
5. **Liftoff-tier and `opt-level = "z"` behaviour.** Consumers like simlin build at `-Oz`; V8
   runs the first calls in Liftoff with no inlining. Measure both and keep the fast paths a
   single straight-line function.
6. **Zero-tracking beyond first use.** Fresh slices are zero; a freed run whose contents are
   known zero (for example a buffer the program never wrote) is not detectable, but blocks freed
   from `alloc_zeroed` pages that were never written could be. Probably not worth it; note it.
7. **Per-chunk kind binning in the slice map** (mimalloc's `mi_bbitmap_t`) if singleton churn
   is shown to fragment the aligned runs medium and large pages need.
8. **Verification depth.** Kani harnesses for the heap's queue and direct-table invariants over
   a tiny simulated memory, ledger entries for every unsafe block in `heap.rs` and `global.rs`,
   and an adversarial review of all entries.
