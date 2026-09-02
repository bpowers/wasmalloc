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
   `__rg_alloc`/`__rg_dealloc` shims; slow paths are `#[cold] #[inline(never)]`.

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
- Growth policy: when no run of the needed length exists, grow by
  `max(needed, quantum)` where `quantum` grows geometrically with the heap (bounded above);
  exact constants come from the memory.grow cost measurements in `docs/research/`.

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
  `backend` (a `Memory` trait implemented by `memory.grow` on wasm32 and by a 64 KiB- and
  512 KiB-aligned simulated linear memory on the host for tests, Kani, Miri and fuzzing).
- `bench/`: the roofline harness grown into the benchmark suite (allocator selected by feature).
- `fuzz/` and `verify/`: differential fuzzing against a model; Kani harnesses.
- `docs/soundness-ledger.md`: the fallback for `unsafe` blocks that formal verification cannot
  reach. The default is a machine-checked proof (Kani harness or equivalent) per unsafe block;
  only blocks without one get a ledger entry with preconditions, pen-and-paper proof, and the
  adversarial reviewer's sign-off.
