# wasmalloc Design

## Summary

wasmalloc is a pure-Rust, `no_std`, single-threaded `#[global_allocator]` for wasm32 whose
structure follows mimalloc v3: size-class bins, one intrusive free list per page, lazy page
extension, page retirement, and 64 KiB slices managed over `memory.grow`. Three facts about the
target reshape that design. Rust passes the `Layout` to `dealloc` and `realloc`, so the size class
and the page kind are pure functions of the arguments: page headers are found by masking the
address, alignment up to 4 KiB is satisfied by construction, and blocks above the medium limit are
header-less runs of slices. Wasm memory is a flat, zero-filled, grow-only space of 64 KiB pages,
so pages are placed at addresses that make the masks valid, fresh memory skips zeroing, and runs at
the top of the heap grow in place by growing memory. There is exactly one thread, so nothing is
atomic or thread-local. The allocator is generic over a `Memory` backend and runs on the host
against a simulated linear memory for tests, fuzzing, Miri and Kani.

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

- wasmalloc.AC1 Correctness: the model-based differential tester passes every profile against
  the heap on the host; the wasm32 end-to-end test passes under wasmtime; Miri reports nothing on
  the heap, page and backend tests; every Kani harness verifies; every `unsafe` block has a
  passing proof or an accepted ledger entry.
- wasmalloc.AC2 Speed: on the roofline harness's workloads under V8's optimizing tier, wasmalloc
  is faster than std's dlmalloc on every workload except the realloc doubling chain, and faster
  than talc on aggregate; the 32-byte alloc+free pair is within 2x of the free-list floor.
- wasmalloc.AC3 Footprint: peak `memory.size` after each roofline workload is at most 2.2x the
  default allocator's, so the crate is safe as a drop-in default.
- wasmalloc.AC4 Integration: one `#[global_allocator]` line, no dependencies, stable Rust 1.85,
  `wasm32-unknown-unknown`, `wasm32-wasip1` and `wasip2`; refuses to build with `atomics`.

Status against these criteria is recorded in "Implementation status" below.

## Glossary

- **Slice**: one 64 KiB wasm page, the unit of `memory.grow` and of the slice bitmap.
- **Page**: an allocator page holding blocks of one size class, with its header at the start:
  small (64 KiB, blocks up to 10 KiB) or medium (256 KiB, blocks up to 40 KiB). Aligned to its
  own size so `ptr & !(size - 1)` finds the header.
- **Run**: a header-less sequence of whole slices serving one block above the medium limit, or
  any block with alignment above 4 KiB. Its length is recomputed from the Layout on free.
- **Bin / size class**: one of 60 block sizes (8-byte steps to 64 bytes, then four per doubling).
- **Direct table**: `pages_direct[(size + 7) / 8]`, the page to pop from for sizes up to 1 KiB.
- **Retired page**: an empty page kept in its queue for a few collections in case the size class
  is still active; released before memory grows.
- **Full queue**: pages with no free and no unextended blocks, kept out of the search.
- **Floor**: a deliberately minimal allocator in the benchmark harness that bounds what any
  size-class fast path can achieve on an engine.

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

- The heap is the linear memory from the backend's heap base to `memory.size() * 64 KiB`,
  extended by `memory.grow`. The base is `__heap_base` on `wasm32-unknown-unknown`; on wasi it
  is the end of the memory present at the heap's first allocation, because wasi-libc's dlmalloc
  owns `[__heap_base, __heap_end)` and std reaches it even with this crate installed (review
  finding R-1, 2026-09-02). It is managed as an array of 64 KiB *slices* (mimalloc's
  `MI_ARENA_SLICE_SIZE` on 64-bit; we deliberately do not adopt the 32 KiB slices mimalloc
  uses on 32-bit hosts, because the wasm page is 64 KiB).
- A bitmap indexed by absolute slice number (`addr >> 16`; 65536 bits, 8 KiB, static) records
  free slices; a companion bitmap records slices known to be zero (never handed out since
  `memory.grow`). This is mimalloc's `slices_free` / `slices_dirty` pair without atomics.
  Run search must honour alignment: 1 slice anywhere (lowest first), 4-slice medium pages at
  4-slice boundaries (a whole nibble), 8-slice runs at 8-slice boundaries (a whole byte),
  64-slice runs at 64-slice boundaries (a whole `u64` word), singleton runs first-fit. mimalloc v3's per-chunk kind binning (`mi_bbitmap_t`, which keeps
  small pages from fragmenting the runs medium and large pages need) is deferred until
  footprint tests show fragmentation; the heap's growing top edge is always available for
  aligned runs, so the failure mode is footprint, not correctness.
- Growth policy: `memory.grow` costs 50 to 75 microseconds per call in V8 regardless of the
  number of pages requested (about 100x wasmtime; measured in `docs/research/landscape.md`
  C.8), so growth is geometric: grow by `max(needed, clamp(heap_size / 8, 1 MiB, 64 MiB))`
  (an eighth, not a half: the half-heap step overshot the peak by up to 50 percent, tuning log
  2026-09-02), rounded to whole slices, and before growing at all release every retired page
  (a grow is footprint for good; a released page is one page initialisation away). On
  `wasm32-unknown-unknown`, reclaim the linker gap between `__heap_base` and the initial
  `memory.size()` as the first free slices instead of paying a grow for the first page (std's
  dlmalloc 0.2.11 wastes that gap); on wasi the gap is wasi-libc's, so nothing is reclaimed and
  the first page costs a grow. Growth must be sized from the rounded, aligned request
  (talc 5.0.3 undersized growth and spun to the 4 GiB limit) and must tolerate `memory.grow`
  returning `usize::MAX` (failure at the 4 GiB or host limit: return null so std reports OOM)
  and non-contiguous results (something else grew memory in between): the returned page index
  is the start of the new region, never an assumption of contiguity.

### Page kinds (derived from the block size class, hence from the Layout)

| kind      | page size and alignment                | block sizes                     | header        | page lookup at dealloc                 |
|-----------|----------------------------------------|---------------------------------|---------------|----------------------------------------|
| small     | 1 slice (64 KiB), 64 KiB-aligned       | 8 B .. 10240 B                  | at page start | `ptr & !(64 KiB - 1)`                  |
| medium    | 4 slices (256 KiB), 256 KiB-aligned    | 10240 B .. 40960 B              | at page start | `ptr & !(256 KiB - 1)`                 |
| large (off) | 64 slices (4 MiB), 4 MiB-aligned     | 40960 B .. 512 KiB when `bins::LARGE_PAGES` | at page start | `ptr & !(4 MiB - 1)`         |
| singleton | `ceil(size / 64 KiB)` slices, 64 KiB-aligned (or aligned to `align` when `align > 64 KiB`) | > 40960 B, or any size with `align > 4 KiB` | none | `ptr` is the run start; slice count is recomputed from the Layout |

The small page and the large page are mimalloc's 64-bit constants (`MI_SMALL_PAGE_SIZE`,
`MI_LARGE_PAGE_SIZE` and the corresponding `*_MAX_OBJ_SIZE` bounds); the medium page is
mimalloc's 32-bit one (256 KiB, its bound snapped down to the 40960-byte bin), because in a
small heap every touched bin costs a whole page and the 512 KiB page measured as pure footprint
(tuning log, 2026-09-02). Large pages are compiled but off (`bins::LARGE_PAGES = false`):
everything above the medium limit is a header-less run, which can grow in place and costs its
own size instead of a 4 MiB page holding one block. mimalloc's own source questions large pages
(random sizes between 64 KiB and 512 KiB can leave many partially used 4 MiB pages), and mimalloc
v3.5.1 does not actually align large pages to 4 MiB, so with the constant on our slice allocator
searches for aligned runs (a large page is exactly one 64-bit word of the slice bitmap, which
makes that search trivial).

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
freeing tail slices and grow in place when the following slices are free or, when the run
reaches the end of memory, by growing linear memory (`slices::extend_with_growth`). A block
that must move into a run goes to the bottom of the free tail at the top of the heap
(`SliceMap::alloc_tail`, dlmalloc's top chunk) so that its next growth is in place; the lowest
fit would put every doubling of a growing buffer into a new hole and copy it again.

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
| `slices` | free and known-zero bitmaps, lowest-first aligned run search with dedicated 1/4/8/64-slice scans, `acquire` with geometric growth (an eighth of the heap) sized from the current end, `alloc_tail` (bottom of the free tail) and `extend_with_growth` (in-place growth through `memory.grow`) for growing runs, last slice of a 4 GiB memory never used, no bounds-check panic paths | 25 tests incl. a model check; 12 Kani harnesses (4 in the quick gate) |
| `page` | 36-byte in-band header (48 on the host), `pop`/`push`/`extend`, `header_of` mask | 20 tests; Miri clean under Stacked and Tree Borrows; 4 Kani harnesses over a proof-only memory; ledger PAGE-01..06 |
| `heap` | bin queues, direct table with a read-only sentinel page, candidate search, full queue, retirement and collection with every retired page released before memory grows, header-less runs above 40 KiB, in-place realloc within a kind or a run (through memory growth at the top), moved runs placed at the top | 18 tests incl. randomised churn with content checks and a full invariant validator; 13 Kani harnesses (arithmetic ones in the quick gate, lifecycle ones over one-page models); ledger HEAP-01..09 |
| `global` | `WasmAlloc`: `GlobalAlloc` over a static heap; refuses the `atomics` feature | end-to-end wasm32 test under wasmtime with std collections |
| `testing` | model-based differential tester with six profiles, mutant tests, cargo-fuzz targets for System and the heap | all profiles pass against the heap; 208k fuzz runs clean on main, 249k more on tuning-b |

Deviations from the draft: the free list is a single LIFO list (mimalloc's `local_free` was
dropped as planned); the first block of a page starts at 64 bytes, not 32, so the host and
wasm32 share one geometry; large (4 MiB) pages are compiled but off, and the medium page is
256 KiB with a 40 KiB limit (tuning log); the block counters in the page header are `u32`, not
`u16`, because a 16-bit store followed by a 16-bit load of `used` is a slow store-to-load
forward on current x86 cores (`docs/research/roofline.md` section 12.1).

Measurements (median ns per operation; full matrix in `docs/research/roofline.md`, tuning
deltas in the tuning-a commits). After the first tuning pass (u32 counters, inline retire test,
aligned frees on the fast path, `inline(always)`), the 32-byte alloc+free pair costs 1.13 ns on
V8 15.2 (floor 0.55, dlmalloc 4.11, talc 8.92), 1.10 on wasmtime (floor 0.71), 2.50 on node 22
(floor 1.80, dlmalloc 7.99, talc 12.35); the aligned-16 pair costs the same as the unaligned one;
random churn over 10k live objects is 6.4 on V8 15.2 (floor 3.2, talc 26, dlmalloc 56);
talc-style random actions 14.6 (talc 20.1, dlmalloc 40.6). After the second tuning pass
(tuning log, 2026-09-02) the 16 B to 1 MiB realloc chain costs 0.63 us on V8 15.2 and 0.64 on
node 22 (was 12 us; dlmalloc 0.04 and 0.07, talc 0.05 and 0.09), the rest being the copies
below the 40 KiB medium limit, and footprint is 1.0x dlmalloc's on the 1 MiB Vec growth (68
pages against 54, was 288), 1.27x on churn (132 against 104, was 181) and 2.1x on random
actions (68 against 32, was 81), the last being one 64 KiB page per touched bin, which needs a
design change to go below. Note that our pages are extended 8 KiB at a time, so `memory.size`
overstates our resident memory relative to dlmalloc, which touches everything it hands out.

## Roadmap and research directions

The goal is not to reproduce mimalloc in Rust but to be the best allocator for this target. Ideas
to try, each behind a benchmark and a proof, roughly in order of expected payoff:

1. **Header-less runs above the medium limit.** Done (tuning log, 2026-09-02): large pages are
   off, runs grow in place through the free slices after them and through `memory.grow` at the
   top of the heap, and a run that must move goes to the bottom of the free tail. What is left
   of the chain's cost is the copies inside pages, 10 KiB per small-page block and 40 KiB per
   medium block, which `Vec` doubling amortises.
2. **Fold `free_is_zero` into the flags byte.** The dealloc fast path currently stores a byte on
   every free; the flags byte is already loaded, so the clear can move to the slow path and run
   once per page.
3. **Shrink the fixed footprint of small heaps.** Mostly done (tuning log, 2026-09-02): the
   256 KiB medium page, the eighth-heap growth step and releasing retired pages before growth
   took the batch profiles from 18 to 29x peak live bytes to 3.7 to 4.6x. What remains is one
   64 KiB page per touched bin (37 pages before any object is counted in random_actions);
   carving the first page of several bins from one slice would need the page header address to
   stay derivable from the Layout, a design change. Reclaiming the partial slice below the
   first page for metadata is still open.
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
8. **A one-entry hot-block cache per direct index (churn at 2x the floor); deprioritised.**
   The simlin profile (`docs/research/simlin-profile.md`) found that 99.3 percent of that
   workload's allocations already pop the most recently freed block of their class, so the cache
   would buy 0 to 15 ms of a 1900 ms compile there; it still targets random-free patterns and
   must be proved on the roofline churn workload before it earns a place. The rerun in
   `docs/research/roofline.md` section 14 shows churn over 10k live objects at 2x the free-list
   floor on every engine while the hot pair is within 1.4 to 2x. The mechanism: a freed block
   goes onto its own page's list, so the next allocation of that size pops a different, cold
   block from the direct page, one cache miss per operation; a global LIFO list reuses the block
   that was just written. Rust's Layout at dealloc makes a tiny per-size cache trivially correct:
   `hot[direct_index(size)]` holds the most recently freed block of that size; `dealloc` stores
   the block there and pushes the previously cached block (if any) to its page as today; `alloc`
   pops `hot[idx]` when non-empty with no page-header access at all, else takes the direct page.
   A cached block counts as used by its page (retirement is delayed by at most one block per
   index), `alloc_zeroed` must clear it, and `realloc` is unaffected. Expected: the hot pair at
   or below the floor and churn close to it, at the cost of 129 words of state and one branch.
   Needs the model tester, a Kani harness for the used-count invariant with cached blocks, and
   Liftoff measurements.
9. **Zero-initialised heap static: decided against.** The 17.7 KB data segment is almost all
   zero bitmaps, compresses to a few bytes under the gzip or zstd every wasm delivery uses, and
   wasm-opt's memory packing removes it outright; at runtime the pages it initialises are the
   allocator's own state and are touched anyway, so the incremental cost is about 17 KB of
   retained module bytes. Not worth a null test on the allocation fast path (+0.3 ns in Liftoff).
10. **Verification depth.** Kani harnesses for the heap's queue and direct-table invariants over
   a tiny simulated memory, ledger entries for every unsafe block in `heap.rs` and `global.rs`,
   and an adversarial review of all entries.

## Tuning log

Each entry: date, change, before -> after (median ns per operation on node 22 opt / node 22
Liftoff / d8 V8 15.2 opt / wasmtime; footprint in `memory.size` pages after the workload in a
fresh process; `memory.grow` calls in that first call from the harness's `wasmalloc_count`
variant), decision. Full tables are in the commit messages on branch `tuning-b`.

### 2026-09-02, tuning-b (footprint and realloc)

Baseline on `main` after tuning-a: alloc_free_32 2.50/3.66/1.13/1.09 ns (36 pages),
batch_lifo_32 2.99/4.82/2.42/1.81 (36), churn 7.21/10.1/6.77/6.22 (181 pages, 5 grows),
random_actions 15.1/20.1/14.6/14.1 (81, 3), vec_push_growth 399/1116/400/522 us (288, 5),
realloc_doubling 12.2/13.4/12.4/12.2 us (288, 5), large_alloc_free 2.24/1.95/2.18/2.47 us
(288, 3). dlmalloc's footprints: 21/21/104/32/54/54/149.

1. **Large pages off** (`bins::LARGE_PAGES = false`; sizes above the medium limit are runs).
   Footprint 288 -> 81 on vec_push_growth and realloc_doubling, 288 -> 132 on large_alloc_free;
   realloc_doubling only 12.2 -> 11.2 us, because the matrix runs it after churn, whose retired
   pages leave the slice map full of holes, and the lowest fit put every doubling into a hole
   and copied it again at the next step (alone in a fresh process it was 3.4 us). Kept.
2. **Runs grow in place through `memory.grow`, moved runs go to the top**
   (`slices::extend_with_growth`, `SliceMap::alloc_tail`). realloc_doubling 11.2 -> 1.19 us,
   1.37 Liftoff, 1.19 d8, 1.15 wasmtime; footprint unchanged; vec_push_growth 397 -> 387 us
   on node. Placing the moved run at the *highest* fit was tried first and is a trap: with
   nothing free above it every growth is a `memory.grow` of half the heap, 73 grows and
   65535 pages within one call. The bottom of the free tail (dlmalloc's top chunk) keeps the
   tail above the run. Kept. Observation: vec_push_growth on wasmtime swings between 380 and
   590 us for the same code depending on the buffer's address (a Cranelift push loop into a
   1 MiB `Vec` at twelve addresses spans 460 to 589 us with std's own allocator), so its
   wasmtime column is not an allocator measurement.
3. **Growth step an eighth of the heap** (`GrowPolicy::step_divisor = 8`, was a hard-coded
   half). churn 181 -> 132 pages (grows 5 -> 7), random_actions 81 -> 68 (3 -> 3), the Vec
   workloads 81 -> 84 (3 -> 4, step rounding); timings flat. The extra calls are in the first
   call of each workload and cost under 0.3 ms in total even at 60 us per call on node 22.
   Kept.
4. **Retired pages released before any `memory.grow`; forced collection scans past pages in
   use.** Roofline footprints unchanged (no workload changes its bin mix); model tester peak
   footprint over peak live: mixed 1.43/1.36/1.40 -> 1.34/1.15/1.40, batches 6.43/7.39/6.85
   -> 5.71/6.57/6.85, align_heavy 2.01/1.95/1.62 -> 1.94/1.96/1.47. batch_lifo_32 on d8
   2.41 -> 1.69 (three reruns) with `find_page` inlined into `alloc_generic` after the OOM
   retry it subsumed was removed; random_actions 15.0 -> 14.5 on node. Studied and rejected:
   freeing an empty page at once when a sibling has room (no peak-footprint effect once pages
   are released before growth; would churn pages in bins around a page boundary) and
   refreshing the countdown on re-retire (a collection already un-retires a page in use).
   Kept.
5. **256 KiB medium pages, 40 KiB medium limit, nibble scan** (mimalloc's 32-bit constant).
   vec_push_growth and realloc_doubling 84 -> 68 pages, realloc_doubling 1.18 -> 0.64 us
   (0.79/0.63/0.61) because 64 KiB is now a run the 128 KiB step extends; churn and
   random_actions footprint unchanged (they have no medium blocks); model tester batches
   5.71/6.57/6.85 -> 3.65 to 4.07/4.43/4.55, small_churn 1.38/1.47/1.51 -> 1.29/1.39/1.42,
   others within 0.2. Cost: a wasmtime microbenchmark of 1024 medium allocations then 1024
   FIFO frees per round pays 12.7 -> 14.0 ns per pair at 16 KiB, 13.4 -> 14.8 at 24 KiB,
   14.5 -> 15.5 at 32 KiB, 15.1 -> 14.9 at 40 KiB: a page initialisation every 15 blocks
   instead of every 31, not the search (the nibble scan and the general first-fit search
   measured within 0.3 ns). No roofline workload moved. Kept as a footprint knob that one
   constant flips back.
6. **No bounds-check panic paths in the allocator's functions** (bitmap helpers test the word
   index, queue indices are masked over 64 queues, the direct table is written in an index
   loop, the step divisor is clamped). Module panic call sites 29 -> 6 (the harness's std and
   `page::extend`'s `MAX_EXTEND_SIZE / block_size`, an unsafe block under ledger PAGE-04 left
   for a reviewed change, done on 2026-09-02, review fixes item 2 below); `__rust_realloc`
   3 -> 0; raw module 47903 -> 47208 bytes, after
   wasm-opt -O3 20871 -> 20558. Timings unchanged (churn read 8.30 once on node and 7.00,
   7.25, 6.97 in three alternating reruns against 7.10 for the previous build). Kept.

Final state against the baseline: alloc_free_32 2.51/3.68/1.13/1.09 (36 pages),
batch_lifo_32 2.99/4.83/1.66 to 1.88/1.57 to 1.81 (36), churn 7.0/9.8/6.47/6.05 (132, 7
grows), random_actions 14.5/19.5/14.2/14.1 (68, 3), vec_push_growth 386/1106/392/560 us
(68, 3), realloc_doubling 0.64/0.78/0.62/0.61 us (68, 3), large_alloc_free
2.04/2.26/2.08/2.38 us (132, 3). Model tester peak footprint over peak live, main -> tuning-b:
lifo and fifo batches 18.1 to 29.1 -> 3.7 to 4.6, small_churn 1.56 to 2.12 -> 1.29 to 1.42,
align_heavy 2.35 to 3.13 -> 1.40 to 2.03, mixed 1.32 to 1.39 -> 1.23 to 1.37, large_heavy
1.31 to 1.52 -> 1.23 to 1.42. Proofs: 20 Kani harnesses (311 s for the full set, 2.5 GB
peak), 249k model_heap fuzz runs in 60 s clean.

Next: the per-bin page cost (random_actions at 2.1x dlmalloc), the `page::extend` division
(with a PAGE-04 review), a zero-initialised heap static (roadmap 8), heap Kani harnesses and
ledger entries (roadmap 9), and `alloc_zeroed` for runs extended through `memory.grow`, whose
fresh slices are known zero but are not yet reported as such to a zeroing realloc.

### 2026-09-02, review fixes (the adversarial ledger review, `docs/soundness-ledger.md`)

1. **Every empty page is released before memory grows, whatever its countdown** (review
   finding R-2). A page retired, drained through the direct table (the fast paths never touch
   `retire_expire`), parked in the full queue, brought back by a free and emptied behind three
   pages in use kept a stale countdown and was never reached by the collection, so memory grew
   with an empty slice in a queue. Now the search clears the countdown of a page it parks, so
   the page's last free goes through `retire`; `retire` refreshes the countdown and the retired
   range instead of returning early; and the release that precedes a `memory.grow`
   (`Heap::release_empty_pages`) walks every bin queue that holds a page, found through an
   occupancy bitmask the queue operations maintain, rather than the retired range's three-page
   window. A first version scanned all 60 queue heads in a plain loop, which was fine at run
   time but made CBMC unroll the queue walk once per queue and pushed three heap proofs past
   their 4 GiB cap; the bitmask keeps the proofs at an unwind bound of 2. Hot paths untouched;
   measured anyway, three interleaved runs each of the `wasmalloc` variant (median ns per
   pair): alloc_free_32 2.50 to 2.52 -> 2.49 to 2.51 on node 22 `--no-liftoff`, 1.122 to 1.124
   -> 1.122 to 1.123 on d8 (V8 15.2) `--no-liftoff`; batch_lifo_32 2.98 to 3.00 -> 2.96 to 2.98
   on node (one run read 2.54), 1.87 to 1.88 -> 1.83 to 1.88 on d8. Kept.
2. **No panic call site left in the allocator's release code** (ledger PAGE-04 and PAGE-01).
   `page::extend` divides `MAX_EXTEND_SIZE` by the header's `block_size`, and `page::init`
   divides the block area by it through `bins::blocks_per_page`; the compiler cannot see that
   the field is at least 8, so both carried a division-by-zero check and the panic string it
   pulls in. Both now divide by `block_size` or 1, identical for every reachable header. On the
   roofline `wasmalloc` release build for wasm32-unknown-unknown (`wasm-tools demangle`, then
   `print`, counting `call` instructions whose callee is a panic function; tuning-b item 6
   counted differently): 14 -> 11 in the module, all remaining in the harness's std, 2 -> 0 in
   `wasmalloc::` functions; "attempt to divide by zero" gone from the data segments; raw module
   47932 -> 47617 bytes, 21418 -> 21186 after `wasm-opt -O3`. Kept.

### 2026-09-02, tuning-c (the simlin profile's ranked list)

Implements the ranked changes of `docs/research/simlin-profile.md` section 8, each measured at
two levels before it was kept. Roofline: `bench/roofline` at `opt-level = 3` with fat LTO, the
six workloads alloc_free_32, alloc_free_32_align16, batch_lifo_32, churn, random_actions and
realloc_doubling, on node 24 (V8 13.6, `--no-liftoff` and `--liftoff-only`), node 22 (V8 12.4,
`--no-liftoff`) and wasmtime, pinned to CPUs 12-15, three to seven alternating runs per variant,
median of medians (ns per operation). simlin: the C-LEARN compile stage (`clearn-alloc.mjs`,
LTM on) on node 24, the baseline and the candidate bundle interleaved in one process, 10
iterations after 2 warm-ups, two runs with the bundle order swapped, reported as the median of
the 20 per-iteration paired differences. The bundles are built exactly as simlin's
`build.sh` does (`-Oz`, fat LTO, `wasm-opt -O3`); a second build keeps the name section for
instruction counts (`wasm-tools print`) and V8 machine code (`--print-wasm-code` on the tiered
run, the code the bench executes). Baseline: main at afc0e2c (byte-identical to the wasmalloc
0.1.0 release simlin ships).

1. **Lean `realloc` entry** (profile rank 1). `bins::bin`, `bin_size`, `kind_of_bin`,
   `direct_index`, `classify`, `fits_in_place` and `huge_slices` are `#[inline(always)]` (at
   `-Oz` LLVM compiled `classify` out of line and returned its two-byte `Class` through a slot
   on the shadow stack, twice per `realloc`); `realloc` returns the block before classifying
   anything when `align <= WORD`, both sizes are at most `DIRECT_MAX_SIZE` and their direct
   indices agree (one direct index means one bin, Kani `direct_table_tiles_and_matches_bin`;
   the harness `realloc_in_place_keeps_the_kind_and_fits` now also asserts the shortcut agrees
   with `fits_in_place`); and the alloc fast paths take their size from one function,
   `direct_size`, which maps an over-aligned request to `usize::MAX` instead of returning early.
   The last point was forced by the first version of this change, which left `__rust_alloc`
   with two copies of the `alloc_generic` call and its epilogue at `-Oz` (20 bytes more of V8
   code) and measured 25 ms slower on simlin. Shims, wasm instructions after `wasm-opt -O3` in
   the simlin bundle: `__rust_alloc` 63 -> 56, `__rust_dealloc` 68 -> 68, `__rust_realloc`
   493 -> 622 with two `classify` calls and three shadow-stack operations -> none,
   `__rust_alloc_zeroed` 82 -> 77; V8 TurboFan code: `__rust_realloc` 1916 -> 1752 bytes,
   `__rust_alloc` 216 -> 232 (V8 duplicates the cold call for a third branch; the hot path is
   instruction-for-instruction the same), and the alloc path is inlined into 67 callers instead
   of 64. simlin compile: 1934 -> 1927 ms median, paired difference -4.2 ms (IQR -9.1 to -1.6,
   n = 20). Roofline, base -> change (n24 TurboFan / n24 Liftoff / n22 TurboFan / wasmtime):
   alloc_free_32 1.249/4.41/2.599/1.12 -> 1.248/4.39/2.599/1.12 (one wasmtime baseline run
   read 1.30, the address-dependent swing noted in the tuning-b entry), alloc_free_32_align16
   1.248/4.40/2.601/1.122 -> 1.248/4.39/2.599/1.122, batch_lifo_32 1.596/5.41/3.077/1.86 ->
   1.589/5.42/3.078/1.61, churn 6.68/10.98/7.27/6.26 -> 6.73/10.99/7.29/6.25, random_actions
   14.88/21.46/14.86/14.38 -> 15.59/20.95/15.06/14.63, realloc_doubling 640/826/649/622 ->
   631/825/642/628. The random_actions step on n24 (+4.8 percent) is not the change's
   arithmetic: removing either the shortcut or `direct_size` alone puts the workload back at
   14.9, and the TurboFan code of the `random_actions` loop (which inlines the fast paths)
   differs between those builds only in register assignment and spill slots, at 2385 to 3021
   instructions depending on what V8 chose to inline. Kept.

2. **Queue-head service for 1 to 40 KiB requests** (profile rank 2). `alloc_generic` classifies
   first and, for a bin above the direct table's range whose queue's first page has a free
   block, pops that page with none of its bookkeeping (`ensure_init`, the collection counter,
   the candidate walk, the countdown store); the direct table does the same for the sizes it
   covers, since it caches exactly these queue heads. Counters on the simlin bundle: 147,159
   of the 215,291 `alloc_generic` entries per compile take the new path, the periodic
   collections fall from 215 to 68, page supply and `page::extend` counts are unchanged, and
   the shims are byte-for-byte what change 1 left (`alloc_generic` itself 529 -> 504 wasm
   instructions). A first version put the check in a separate `#[inline(never)]` function
   between the fast path and `alloc_generic`, as the profile suggested, and lost: V8 inlines
   any small callee into `__rust_alloc` on its own (TurboFan code 232 -> 372 bytes), under
   `--no-liftoff` the grown shim was no longer inlined into the roofline's batch loop
   (batch_lifo_32 +10 percent on both V8s), and simlin read -0.7 ms (IQR -9.7 to 3.1, n = 20).
   `alloc_generic` is far past V8's inlining size limit, so the check is safe there and the
   only cost is its call, which the request paid before. simlin compile against change 1: 1929
   -> 1926 ms median, paired difference -2.2 ms (IQR -6.2 to 2.8, n = 40): the profile's 5 to
   6 ms assumed the whole 35 ns of a generic entry was bookkeeping, but the queue head's
   header line is cold for these sizes whichever path loads it. Roofline, change 1 -> change 2
   (n24 TurboFan / n24 Liftoff / n22 TurboFan / wasmtime): alloc_free_32 1.249/4.39/2.60/1.12
   -> 1.247/4.38/2.60/1.12, align16 unchanged, batch_lifo_32 1.60/5.41/3.08/1.63 ->
   1.59/5.40/3.09/1.87 (wasmtime's address swing, see tuning-b), churn 6.69/10.84/7.41/6.26
   -> 6.60/11.00/7.38/6.29, random_actions 15.57/20.98/15.10/14.60 ->
   14.50/19.99/14.51/13.97, realloc_doubling 632/827/644/628 -> 640/810/623/609. Kept for the
   4 to 7 percent on the workloads with requests in that range; the simlin gain is real but
   at the resolution limit.

3. **`dealloc` decides the page kind without rounding** (profile rank 3, section 5.2). The fast
   path tested `round_up(size, align) <= SMALL_MAX_OBJ_SIZE`, and LLVM computed the rounding on
   every free and selected between the two sizes: `lea, sub, mov, neg, and, mov, cmp, cmova`
   before the compare, for the 54 aligned requests per simlin compile. For a power-of-two
   alignment the same test is `size <= SMALL_MAX_OBJ_SIZE & -align` (the limit rounded down;
   for `align <= WORD` it is the limit itself), proved equal to the rounded test and to
   `classify`'s kind for every Layout in `dealloc_fast_path_agrees_with_classify`.
   `__rust_dealloc` 68 -> 59 wasm instructions and no `select`; V8 TurboFan 248 -> 228 bytes,
   the predicate `mov, neg, and, cmp, jc` (5 instructions, was 10), the dealloc path inlined
   into 103 callers (was 100). simlin compile against change 2: paired difference +0.3 ms
   (IQR -7.8 to 7.0, n = 20), that is nothing, as the profile predicted (under 0.1 ns per
   free). Roofline flat on every engine (largest move random_actions 14.54 -> 14.40 on n24
   TurboFan, within the run-to-run spread). Kept for the shorter shim.
