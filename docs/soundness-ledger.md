# Soundness ledger

Last updated: 2026-09-02

The default for every `unsafe` block in this crate is a machine-checked proof: a Kani harness
over a simulated memory backend that exercises the block's preconditions, or an equivalent.
This ledger is the fallback for the blocks that no such proof fully covers. Each entry records
the exact preconditions the block relies on, the invariants that make them hold, a pen-and-paper
proof sketch, the machine checks that partially cover it (tests, Kani harnesses, Miri), and the
sign-off of a fresh adversarial reviewer who did not write the code. Changing a listed block
means updating its entry and getting a fresh review.

"Partially covered" here means the Kani harness runs the real code on a bounded model: a fixed
bin, a fixed number of operations, or a proof-only `Memory` backend. The generalisation from
that model to every bin, kind and backend is the pen-and-paper part below.

Entries are grouped by module. Page invariants 1 to 5 refer to the numbered list at the top of
`src/page.rs`.

## page

### PAGE-01: `page::init`, the header write

- Preconditions: `page_addr` is a multiple of `kind.page_size()`; `page_addr + page_size` does
  not overflow; the `page_size` bytes at `page_addr` belong to the allocator through `mem` and
  nothing refers to them (no live blocks, no other header); `kind == kind_of_bin(bin)`;
  `bin` in `1..=MAX_BIN`.
- Invariants relied on: `size_of::<Page>() <= PAGE_HEADER_RESERVE <= page_size` (const asserted)
  and `align_of::<Page>() <= WORD` (const asserted), so a 64 KiB-aligned address is aligned for
  `Page`. `Memory::ptr` yields a pointer valid for the allocator's memory (trait contract).
- Proof sketch: `mem.ptr(page_addr)` is valid for `page_size` bytes of writes by the `Memory`
  contract and the ownership precondition; `page.write` touches exactly `size_of::<Page>()` of
  them. The write creates no reference and reads nothing, so dirty memory cannot produce an
  invalid value. Afterwards the header satisfies invariants 2 to 5 with `capacity == used == 0`
  and an empty list: `reserved`, `block_start` and `block_size` are computed by the `bins`
  functions the invariants name, `reserved` fits `u32` and `block_start` fits `u16` (Kani
  `every_block_of_every_bin...`), and `free_is_zero == zeroed` is exactly invariant 5 for a page
  with no linked blocks.
- Machine checks: Kani `four_operations_on_a_page_of_eight_kib_blocks_preserve_invariants`,
  `two_operations_on_a_page_of_four_kib_blocks_preserve_invariants` (concrete bins, proof-only
  backend), `every_block_of_every_bin_lies_inside_its_page_and_is_aligned` (geometry for every
  bin). Tests `init_writes_every_field_on_zeroed_and_dirty_pages`, `kind_bytes_round_trip`.
  Miri (Stacked Borrows with strict provenance, Tree Borrows) over the page tests.
- Changes: 2026-09-01, `used`, `capacity` and `reserved` widened from `u16` to `u32` and
  `block_size` moved ahead of `block_start` (roofline 12.1: a 16-bit store-to-load forward of
  `used` cost about 2 ns per alloc+free pair). The header is 36 bytes on wasm32 and 48 on the
  host, still within `PAGE_HEADER_RESERVE` (const asserted); the write is otherwise the same.
  The four page Kani harnesses and Miri (both aliasing models) were re-run on the new layout.
- Division: `bins::blocks_per_page(kind, block_size)` divides by `block_size == bin_size(bin)`,
  at least `MIN_BLOCK_SIZE`, which the compiler cannot see (`bin_size(0)` would be 0), so the
  release build carried a division-by-zero panic call site in `init`. Since 2026-09-02
  `blocks_per_page` divides by `block_size`, or by 1 when it is zero (a caller bug that its
  debug assertion reports), identical for every bin; the geometry harness
  `every_block_of_every_bin_lies_inside_its_page_and_is_aligned` covers every bin's `reserved`
  and was re-run (see PAGE-04 for the module-level check).
- Reviewer: adversarial-reviewer, 2026-09-02: accepted (the 2026-09-02 division note is new).

### PAGE-02: `page::pop`, the free-list pop

- Preconditions: `page` was returned by `init`; invariants 1 to 5 hold.
- Invariants relied on: 4 (a non-zero `free` is a block of this page with index below
  `capacity`), 2 (block `i` starts at `page + block_start + i * block_size`, inside the page,
  and `block_start` and `block_size` are multiples of `WORD`), 1 (`mem.ptr` valid in the page),
  4 again for `used < capacity <= u32::MAX` whenever the list is non-empty.
- Proof sketch: the header reads and writes are in-bounds and aligned (PAGE-01). If `free == 0`
  nothing else happens. Otherwise `free` is a free block `b` (invariant 4): its first `usize`
  lies inside the block, hence inside the page, and is `WORD`-aligned, so the read is valid.
  After the update the list is the old list minus its head, `used` grew by one, and `b` is live:
  invariant 4 is preserved, 3 holds because `used + |list|` is unchanged, 5 holds because the
  remaining list and the untouched blocks are unchanged. No overflow: `used < capacity` before.
- Machine checks: the two Kani operation-sequence harnesses (every reachable interleaving of
  up to four operations on a 7-block page and up to two on a 15-block page, checked with
  `validate` after each step); tests `pop_with_lazy_extension_hands_out_every_block_exactly_once`
  (every bin, every block distinct, aligned, inside the area),
  `random_operation_sequences_preserve_the_invariants`, `push_is_lifo_in_sequential_and_random_order`.
  Miri as above.
- Changes: 2026-09-01, `used` and `capacity` are `u32` (see PAGE-01); the increment is now a
  32-bit read-modify-write at offset `size_of::<usize>()`. The overflow argument is unchanged
  and holds a fortiori. Harnesses and Miri re-run.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted.

### PAGE-03: `page::push`, the free-list push

- Preconditions: as PAGE-02, plus `block` is live on this page (returned by `pop` and not
  pushed since).
- Invariants relied on: 4 (a live block has index below `capacity` and is not on the list, and
  `used >= 1`), 2 (its first word is inside the page and `WORD`-aligned), 1.
- Proof sketch: the block's first-word write is valid as in PAGE-02. Writing the old head into
  it and making it the head prepends exactly one block that was not on the list, so the list
  stays acyclic with `used - 1 + |list| + 1 == capacity`: invariants 3 and 4 hold. `used >= 1`
  rules out underflow. Clearing `free_is_zero` makes invariant 5 vacuous, which is required
  because the block's payload is arbitrary. In debug builds `block_index` checks the block is a
  block boundary below `capacity` and `used > 0` before anything is written.
- Machine checks: the two Kani operation-sequence harnesses (pushes are drawn from the set of
  live blocks); tests `push_is_lifo_in_sequential_and_random_order`,
  `random_operation_sequences_preserve_the_invariants`, `free_is_zero_holds_until_the_first_push`.
  Miri as above.
- Changes: 2026-09-01, `used` is `u32` (see PAGE-01); the decrement is a 32-bit
  read-modify-write and `free_is_zero` moved from offset 16 to 22 (both targets). The underflow
  argument is unchanged. Harnesses and Miri re-run.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted.

### PAGE-04: `page::extend`, lazy free-list extension

- Preconditions: as PAGE-02.
- Invariants relied on: 3 (`capacity <= reserved`), 2 (blocks `capacity .. reserved` lie inside
  the page; `block_start + reserved * block_size <= page_size` is proved for every bin by Kani
  `every_block_of_every_bin_lies_inside_its_page_and_is_aligned`), 4 (blocks at or above
  `capacity` are untouched, so no live data is overwritten), 1.
- Proof sketch: with `capacity < reserved`, `extend` is in `1 ..= reserved - capacity`, so the
  written blocks have indices in `capacity .. capacity + extend <= reserved`. Each first word is
  inside the page and `WORD`-aligned (invariant 2). The addresses are computed by adding
  offsets below `page_size` to `page_addr`, which cannot overflow given invariant 1. The new
  chain `first -> ... -> last -> old free` visits `extend` new distinct blocks then the old list,
  so `used + |list| == capacity + extend`, the new `capacity`, and every listed block has index
  below it: invariants 3 and 4. Only link words are written, so the payload of every listed block
  and every block at or above the new `capacity` is as before: invariant 5. `capacity + extend
  <= reserved <= u32::MAX`, so the `as u32` store of the new `capacity` is exact.
- Machine checks: the two Kani operation-sequence harnesses (the 15-block page links two blocks
  per call, so the loop body runs); tests `extend_links_at_most_max_extend_size_and_at_least_one_block`
  (every bin), `pop_with_lazy_extension_hands_out_every_block_exactly_once` (extension step
  sizes for every bin), `free_is_zero_holds_until_the_first_push` (only link words written).
  Miri as above.
- Changes: 2026-09-01, `capacity` and `reserved` are `u32` (see PAGE-01); the bound above was
  `u16::MAX`. Harnesses and Miri re-run.
- Division: `MAX_EXTEND_SIZE / block_size.max(1)` divides by a header field. The field is
  non-zero because `init` is the only writer of `block_size` and stores `bin_size(bin)` with
  `bin` in `1..=MAX_BIN`, so at least `MIN_BLOCK_SIZE` (8); `pop`, `push` and `extend` write only
  `free`, `used`, `capacity` and `free_is_zero`, and the heap writes only `next`, `prev`, `flags`
  and `retire_expire` (HEAP-05, HEAP-07). The sentinel `EMPTY_PAGE` has `block_size == 0` but is
  never extended: `extend` is called on `find_page`'s candidate and on `fresh_page`'s new page,
  both queue members, and the sentinel is never linked into a queue (HEAP-01). So `.max(1)`
  changes no reachable value; it is there so that the compiler can drop the division-by-zero
  check, which was a panic call site in the allocator's release code. Kani checks the
  division-by-zero condition on every `extend` in the page harnesses and in the heap harnesses
  `first_allocation_builds_a_valid_heap`, `freeing_any_live_block_preserves_invariants` and the
  full-page pair, where `block_size` is a value read back from the modelled header.
- Changes: 2026-09-02, the divisor is `block_size.max(1)` (the reviewer's proposal). Checked on
  the roofline harness's `wasmalloc` release build for wasm32-unknown-unknown (`wasm-tools
  demangle` then `print`): `call` instructions naming a panic function went from 14 to 11, all
  remaining ones in the harness's std and none in a `wasmalloc::` function (before: one each in
  `page::extend` and `page::init`, the latter through `bins::blocks_per_page`, which got the same
  treatment, see PAGE-01); the string "attempt to divide by zero" is gone from the data
  segments; the module went from 47932 to 47617 bytes raw and from 21418 to 21186 after
  `wasm-opt -O3`. The two page operation-sequence harnesses and
  `every_block_of_every_bin_lies_inside_its_page_and_is_aligned` were re-run.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted, with the caveat that the
  `block_size.max(1)` proposal be applied and this entry updated; applied on 2026-09-02 as
  described under Changes, fresh look pending.

### PAGE-05: header field reads in the predicates and helpers

Blocks in `is_full`, `all_free`, `has_free`, `is_expandable`, `in_full_queue`,
`set_in_full_queue`, `kind`, `block_area` and `block_index`.

- Preconditions: `page` was returned by `init` (and, for `block_area`, invariant 1 so the end
  address does not overflow).
- Invariants relied on: PAGE-01's write made every field a valid value of its type; nothing in
  this module or the heap writes a field through a wider type or leaves one uninitialised.
- Proof sketch: each block reads (and `set_in_full_queue` writes) fields of a header that
  `init` fully initialised, through the raw pointer without creating a reference, so the only
  requirements are in-bounds and alignment, which hold as in PAGE-01. `kind` guards against a
  corrupt byte with a debug assertion and otherwise maps it to a `PageKind` total function.
  `block_index` works in offsets from the page start, so its arithmetic cannot overflow.
- Machine checks: the two Kani operation-sequence harnesses call every predicate after every
  step and compare with the harness's own model; tests `init_writes_every_field_on_zeroed_and_dirty_pages`,
  `random_operation_sequences_preserve_the_invariants`, `full_queue_flag_toggles_without_touching_other_fields`,
  `kind_bytes_round_trip`, `pop_with_lazy_extension_hands_out_every_block_exactly_once`
  (`block_area`). Miri as above.
- Changes: 2026-09-01, `is_full`, `is_expandable`, `block_area` and `block_index` now read
  `u32` counters (see PAGE-01); each read is still of a fully initialised field of its own
  type. Harnesses and Miri re-run.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted.

### PAGE-06: `page::validate` (test and proof infrastructure only)

- Compiled only under `cfg(test)` and `cfg(kani)`; never part of the allocator.
- Preconditions: `page` was returned by `init` inside memory owned through `mem`.
- Proof sketch: the header is read as a whole (`page.read()`, no reference); the list walk reads
  a block's first word only after `block_index` has confirmed the address is a block boundary
  below `capacity`, so a corrupt link yields `Err` rather than an out-of-page read, and the walk
  is bounded by `capacity - used` so a cycle terminates.
- Machine checks: `validate_rejects_corrupt_pages` drives every error path; it is the invariant
  oracle for the Kani operation-sequence harnesses and the randomised test. Miri as above.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted.

## heap

Heap invariants 1 to 5 refer to the numbered list at the top of `src/heap.rs`; page invariants
1 to 5 to `src/page.rs`. "Kani" names harnesses in `heap::verify` unless another module is
given. The structural heap harnesses run the real heap over a proof-only memory of one small
page of bin 36 (`HeapModel`) or of three bare page headers of bin 16 (`QueueModel`); the model
and its limits are described in the `heap::verify` module documentation, and where a block's
proof rests on tests only, the entry says so.

A requirement every entry below relies on and that the `Memory` trait documentation now states
explicitly (2026-09-02): `mem.ptr(a)` returns a pointer whose address is exactly `a`. The heap
turns header pointers back into addresses (`page as usize` for queue links and `free_page`,
`page.addr()` in `page::extend`, `ptr.addr()` before `header_of`), so a backend that mapped
addresses elsewhere would be unsound with this code. Both real backends satisfy it
(`with_exposed_provenance_mut(addr)`, `base.with_addr(addr)`), and the proof-only backends are
built to.

### HEAP-01: `Heap::alloc` and `Heap::alloc_zeroed`, the direct-table fast path

Blocks: the `page::pop` on `self.direct[direct_index(size)]`, the `NonNull::new_unchecked` of
the returned block, the zeroing block in `alloc_zeroed`, and the calls into `alloc_generic`.

- Preconditions: `layout.size() != 0` (the `GlobalAlloc` contract; the code also tolerates 0,
  which `bin` maps to bin 1); heap invariants hold.
- Invariants relied on: heap invariant 3 (every direct entry is the first page of the queue of
  `bin(i * WORD)` or the sentinel); heap invariant 2 and the page invariants for that page;
  `EMPTY_PAGE.free == 0`; `bins::classify`'s alignment-by-construction property.
- Proof sketch. Rounding: `size + align - 1` cannot overflow because `Layout` guarantees the
  rounded size fits `isize` and `align <= MAX_NATURAL_ALIGN`. Index: `direct_index(size) <
  DIRECT_ENTRIES` for every `size <= DIRECT_MAX_SIZE`, and `bin(direct_index(size) * WORD) ==
  bin(size) == classify(layout)`'s bin, so the entry belongs to the request's bin (Kani
  `direct_table_tiles_and_matches_bin`, `dealloc_fast_path_agrees_with_classify`). The entry is
  either the sentinel or a live page. Sentinel: `pop` reads `free`, finds 0 and returns without
  writing; this is the only access ever made through a direct entry that is not a page, it is a
  read of an immutable `static` through a `*mut` obtained from `&raw const`, which is permitted,
  and nothing can make `EMPTY_PAGE.free` non-zero because no code writes through `sentinel()`
  (the only writers of a header are `page::init`, `pop`, `push`, `extend`, which the heap calls
  on queue members or fresh runs, and the queue operations, which take members; the sentinel is
  never linked into a queue). `alloc_zeroed` reads `free_is_zero` only after a successful pop,
  so never from the sentinel. Live page: PAGE-02's preconditions hold, and the popped block is a
  block of a page of `bin(size)`, whose size is at least the rounded size and a multiple of
  `align`, at an offset that is a multiple of `align` from a 64 KiB-aligned page start, so it is
  aligned and large enough (Kani `bins::classify_aligns_by_construction`,
  `page::every_block_of_every_bin_lies_inside_its_page_and_is_aligned`). Non-null: a block lies
  at least `PAGE_HEADER_RESERVE` bytes into a page, so its address is at least 64. Zeroing:
  with `free_is_zero` the block is zero except its link word (page invariant 5), and the block
  is at least `WORD` bytes and `WORD`-aligned, so the single `usize` store is in bounds and
  leaves a zero block; otherwise `write_bytes` clears `layout.size()` bytes, which is at most
  the rounded size, at most `bin_size(bin) == block_size`, inside memory the allocator owns
  (page invariant 1). The `alloc_generic` calls pass the caller's contract through.
- Machine checks: Kani `direct_table_tiles_and_matches_bin`,
  `dealloc_fast_path_agrees_with_classify` (also `bin_size(b) >= size` for every Layout),
  `bins::classify_aligns_by_construction`, `first_allocation_builds_a_valid_heap` (both entry
  points through the slow path onto a fresh page, zeroed or not, from a zero or a dirty slice),
  `two_queue_operations_preserve_the_queues_and_the_direct_table` and
  `three_queue_operations_...` (invariant 3 under every queue operation over three pages).
  Tests `small_alloc_free_reuses_lifo`, `every_bin_allocates_aligned_distinct_blocks_and_recovers_its_page`,
  `alloc_zeroed_is_zero_on_fresh_and_recycled_blocks`, `random_churn_keeps_invariants_and_contents`
  (the validator, including invariant 3, every 997 steps), the model tester
  (`tests/model_heap.rs`, six profiles) and the fuzz targets. Miri over the heap tests (Stacked
  Borrows with strict provenance, Tree Borrows).
- Not machine-checked by Kani: the direct-table pop itself (the harnesses' bin has no direct
  entries; sizes up to 1 KiB reach it only in tests), and `alloc_zeroed`'s `write_bytes` on a
  dirty page (the model stores one word per block); both rest on the arithmetic above and on
  the tests.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted.

### HEAP-02: `Heap::dealloc`, the small-page fast path, and `dealloc_generic`

Blocks: in `dealloc`, the `page::push` on the masked header, `needs_transition` and
`dealloc_transition`; in `dealloc_generic`, the same three plus the `debug_assert!` reading
`(*page).bin`, and the `slices.free` of a run.

- Preconditions: `ptr` was returned by this heap for `layout`, or by `realloc` with a Layout of
  the same alignment and a size that classifies as `realloc` left it (HEAP-03), and has not
  been freed since; heap invariants hold.
- Invariants relied on: the block's page has the kind `classify(layout)` names and the block
  lies inside it (page invariant 2, `bins`); page invariants for that page; heap invariant 1
  (full-queue membership is exactly the flag) for `dealloc_transition`.
- Proof sketch. Rounding as in HEAP-01. Kind: for every Layout with `align <=
  MAX_NATURAL_ALIGN`, `rounded <= SMALL_MAX_OBJ_SIZE` holds exactly when `classify(layout)` is a
  bin of kind `Small` (Kani `dealloc_fast_path_agrees_with_classify`), and Layouts with a larger
  alignment skip the fast path and are classified in `dealloc_generic`; `alloc` served the
  request by the same classification (HEAP-01, HEAP-04), and an in-place `realloc` keeps the
  page kind equal to the kind of the Layout it hands back (HEAP-03). So the mask
  `header_of(kind, addr)` uses the page size of the block's actual page, and because every block
  lies inside its page and pages are aligned to their size, it yields the header (Kani
  `page::header_of_recovers_the_page_from_any_address_inside_it`,
  `page::every_block_of_every_bin_lies_inside_its_page_and_is_aligned`). `push`'s precondition
  (PAGE-03) is the dealloc precondition: the block is live on that page. `needs_transition`
  reads `used`, `retire_expire` and `flags` of that header (HEAP-08). Skipping the transition
  when `used == 0 && retire_expire != 0` leaves a page that already carries a countdown retired
  with that countdown, which is what mimalloc's early return in `_mi_page_retire` does; since
  2026-09-02 this test is the only place that decision is taken (`retire` itself refreshes,
  HEAP-07), and the release before memory growth does not depend on the countdown (HEAP-07).
  Such a page cannot be in the full queue (a full-queue page has `used == reserved >= 6` before
  the free, so `used >= 5` after), so the `flags != 0` test is independent of it.
  `dealloc_transition`: `in_full_queue` is set exactly for full-queue members (only
  `move_to_full` sets it, only `unfull` clears it, both on the corresponding queue move), so
  `unfull`'s precondition holds; after it the page is in its bin queue, so `retire`'s does.
  Huge runs: `classify(layout) == Huge` exactly when the block was served by `alloc_huge` or
  kept by an in-place Huge-to-Huge `realloc`, so `addr` is a run start (a multiple of
  `SLICE_SIZE`) and `huge_slices(layout)` is the run's current length (set at allocation, and
  changed by `realloc` together with the Layout it returns); `slices.free` then releases exactly
  the run's slices, which were handed out (heap invariant 4).
- Machine checks: Kani `dealloc_fast_path_agrees_with_classify`,
  `realloc_in_place_keeps_the_kind_and_fits`, `huge_runs_cover_the_layout_and_its_alignment`,
  `freeing_the_last_block_retires_the_page`, `freeing_any_live_block_preserves_invariants`
  (any of up to three live blocks), `a_free_brings_a_full_page_back_to_its_queue` (the
  `flags != 0` path through `unfull`), plus the page and bins harnesses named above. Tests: every
  heap test frees what it allocates and ends with the validator; `page_exhaustion_adds_pages_and_full_queue_round_trips`,
  `huge_alloc_free_and_realloc_in_place`, `large_alignment_is_honoured` (runs from over-aligned
  requests), the model tester and the fuzz targets. Miri as above.
- Not machine-checked by Kani: medium pages and the run branch of `dealloc_generic` (the model
  has one small page); tests only.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted.

### HEAP-03: `Heap::realloc`

Blocks: `Layout::from_size_align_unchecked`, the `self.alloc(new_layout)` for a move, and the
`copy_nonoverlapping` followed by `self.dealloc(ptr, layout)`.

- Preconditions: as HEAP-02 for `ptr` and `layout`; `new_size != 0` and `new_size` rounded up
  to `layout.align()` does not overflow `isize` (the `GlobalAlloc::realloc` contract).
- Invariants relied on: `fits_in_place`'s guarantee (below); page invariant 4 and heap invariant
  4 (live blocks and handed-out runs are disjoint from everything the allocator can hand out).
- Proof sketch. The unchecked Layout meets `from_size_align`'s requirements exactly by the
  contract (the alignment is a Layout's, hence a power of two). In place within a page:
  `fits_in_place(old, new, new_size)` implies `kind_of_bin(old) == kind_of_bin(new)` and
  `bin_size(old) >= round_up(new_size, align)` (Kani `realloc_in_place_keeps_the_kind_and_fits`,
  over every pair of Layouts with `align <= MAX_NATURAL_ALIGN`), so the block holds every byte
  the caller may use through the new Layout and a later `dealloc` or `realloc` with it masks with
  the same page size (HEAP-02); the alignment is unchanged. In place within a run: the length
  becomes `huge_slices(new_layout)`, which a later `dealloc` with the new Layout recomputes;
  `shrink` releases slices of the run itself, `extend_with_growth` claims only free slices
  directly after it or freshly grown ones (proved in `slices::verify`), and both leave the
  run's own slices claimed. The move: `alloc_huge` or `alloc` returns a block of at least
  `new_size` bytes taken from a free list or from free slices, so disjoint from the live old
  block; `min(layout.size(), new_size)` bytes fit in both; `copy_nonoverlapping` is therefore
  in bounds on both sides and non-overlapping; `dealloc(ptr, layout)` then frees the old block
  under HEAP-02 with the Layout it was allocated with. If the allocation fails, `?` returns
  before any write. The allocation may release retired pages and grow memory: the old block's
  page is not empty (it holds the block), so it is never released, and growth never moves
  memory in either backend.
- Machine checks: Kani `realloc_in_place_keeps_the_kind_and_fits`,
  `huge_runs_cover_the_layout_and_its_alignment`,
  `slices::slices_extend_with_growth_extends_only_a_top_run`,
  `slices::slices_try_extend_claims_exactly_the_tail`; tests
  `realloc_within_a_bin_returns_the_same_block`, `huge_alloc_free_and_realloc_in_place`,
  `realloc_grows_a_top_run_through_memory_growth`, `a_run_that_cannot_extend_moves_to_the_top_of_the_heap`,
  `realloc_preserves_contents_across_classes`, the churn test's realloc step, the model tester
  (content checks across every move). Miri as above.
- Not machine-checked by Kani: the copy on a move (the model has no block bodies); tests and
  the model tester check contents.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted.

### HEAP-04: `alloc_generic`, `alloc_huge` and `acquire_run`, the slow-path hand-out

Blocks: in `alloc_generic`, the `collect_retired(false)`, `find_page`, `page::pop` and the
zeroing block; in `alloc_huge`, the `write_bytes` and `NonNull::new_unchecked`; in
`acquire_run`, the `collect_retired(true)`.

- Preconditions: as HEAP-01; heap invariants hold at entry.
- Invariants relied on: the `slices` contract (a returned run was free, hence owned and
  unreferenced, and is claimed now), whose ownership of the initial free range is the `Memory`
  contract's per-target claim (BACKEND-02: on `wasm32-unknown-unknown` the linker gap is unused
  by anything else; on wasi the range is empty and every slice comes from this heap's own
  `memory.grow`); page invariants of the page `find_page` returns; heap invariants between
  operations for the collections; `heap_base > 0` (below).
- Proof sketch. `find_page` returns a queue member of `bin` whose free list is non-empty (it
  found one, extended an expandable one, or built a fresh one and extended it, HEAP-06), so
  `pop` meets PAGE-02 and returns a block; if it did not, the release build returns `None`
  rather than dereferencing anything. Zeroing as in HEAP-01 with `bin_size(bin) >=
  layout.size()`. `alloc_huge`: `huge_slices(layout) * SLICE_SIZE >= layout.size()` and the run
  alignment `layout.align().div_ceil(SLICE_SIZE).max(1)` is a power of two whose multiples in
  slices are addresses aligned for the Layout (Kani `huge_runs_cover_the_layout_and_its_alignment`),
  so the run covers the request; `write_bytes` clears `layout.size()` bytes of memory the run
  owns; it is skipped only when every slice still had its zero bit, which `slices` sets only for
  regions fresh from `grow` and clears on every hand-out, so the memory is zero by the `Memory`
  contract. Non-null: a run starts at or above the first whole slice at or above `heap_base`,
  so its address is at least `SLICE_SIZE` provided `heap_base > 0`; the `slices` module does not
  exclude slice 0 on its own, so a zero heap base would let a run (or a page, whose blocks
  start 64 bytes in and are non-null regardless) sit at address 0. The assumption holds on
  every target: on `wasm32-unknown-unknown` `__heap_base` follows the shadow stack (1 MiB by
  default, placed first by rustc's `--stack-first`) and the data segments, so it is positive for
  every layout Rust produces; on wasi it is the end of a memory that holds at least the stack
  and data, so at least `SLICE_SIZE`; in the simulation it is a host address inside a live
  region. A structural guard (never adding slice 0 to the map, as slice 65535 is never added)
  would remove the assumption and is noted as a follow-up.
  Both `collect_retired` calls happen with no page pointer held by the caller: `alloc_generic`
  calls it before `find_page`, and `acquire_run` before growing, when `fresh_page` and
  `alloc_huge` have not yet chosen a run; the heap invariants therefore hold at those points.
- Machine checks: Kani `first_allocation_builds_a_valid_heap` (the whole slow path onto a
  fresh page, with the linker gap or through `grow`), `the_search_parks_a_full_page_in_the_full_queue`
  (a failed page supply leaves the heap valid), `huge_runs_cover_the_layout_and_its_alignment`,
  `slices::slices_acquire_stays_inside_memory_and_the_map`, `slices::slices_alloc_*`; tests
  `huge_alloc_free_and_realloc_in_place`, `large_alignment_is_honoured`,
  `alloc_zeroed_is_zero_on_fresh_and_recycled_blocks`, `out_of_memory_returns_none_and_keeps_state`,
  `non_contiguous_growth_is_fine`, `uses_the_linker_gap_before_growing`, the model tester and
  the fuzz targets. Miri as above.
- Not machine-checked by Kani: `alloc_huge` itself (no run fits the one-slice model alongside a
  page); tests only.
- Changes: 2026-09-02, the `heap_base > 0` assumption and the per-target ownership of the
  initial free range are now stated above (R-3, R-4); no code in these blocks changed.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted with two caveats (R-3, R-4), both
  addressed in the text above on 2026-09-02; fresh review of the wording pending.

### HEAP-05: the queue operations and `page_at`

Blocks in `push_front`, `push_back`, `remove`, `move_to_front`, `move_to_full`, `unfull`.

- Preconditions: `page` is a live page of this heap (header written by `init`, slices claimed),
  and its membership matches the operation: in no queue for the pushes; a member of queue `qi`
  for `remove`, `move_to_front` and `move_to_full` (the last with no free and no unextended
  block); a member of the full queue for `unfull`.
- Invariants relied on: heap invariant 1 (queue members are live pages, so the neighbours
  reached through `first`, `last`, `prev` and `next` are valid headers); the links are a
  consistent doubly linked list with `first.prev == 0` and `last.next == 0`.
- Proof sketch: each operation writes only the `next` and `prev` fields of `page` and of its
  neighbours, which are members and hence live, and the queue's `first`, `last` and `count`;
  `count` cannot underflow because `remove` requires membership. The list stays consistent by
  the usual doubly-linked-list argument (checked exhaustively by the harnesses below). The
  pushes set bit `queue_index(qi)` of `occupied` and `remove` clears it when `count` reaches
  zero, so heap invariant 5 holds after every operation (safe code; the shift amount is masked
  below 64). Whenever
  the head of a bin queue may have changed (`push_front` always; `push_back` and `remove` when
  the queue was empty or the page was first; the moves through those), `update_direct` rewrites
  `direct[lo..=hi]` for that bin, with `hi < DIRECT_ENTRIES` by construction (Kani
  `direct_table_tiles_and_matches_bin`), restoring heap invariant 3; the full queue has no
  direct entries and `update_direct` returns for it. `queue_index` masks every queue index, so
  no `queues[..]` access can leave the array even if a header's `bin` byte were corrupt.
- Machine checks: Kani `two_queue_operations_preserve_the_queues_and_the_direct_table` and
  `three_queue_operations_...`: every sequence of two or three of the six operations over three
  pages of bin 16, each drawn from the operations whose precondition the state satisfies, with
  pages switching between "has room" and "all blocks out" as a seventh operation; after each,
  the links, flags and counts of both queues, every other queue's emptiness, one symbolic direct
  entry and one symbolic page's membership are checked, and (2026-09-02) the occupancy bit of
  both queues and of a symbolic other queue. Tests: every heap test ends with the validator over
  all 64 queues and 129 direct entries, which now includes the occupancy bit of every queue;
  `page_exhaustion_adds_pages_and_full_queue_round_trips`,
  `a_forced_collection_reaches_a_retired_page_behind_a_page_in_use`. Miri as above.
- Changes: 2026-09-02, the pushes and `remove` maintain `occupied` (heap invariant 5) for
  `release_empty_pages` (HEAP-07); the unsafe blocks themselves are unchanged, the new lines are
  safe integer operations after them. Queue harnesses re-run.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted; the 2026-09-02 addition awaits a fresh
  look.

### HEAP-06: `find_page` and `fresh_page`, the page search and supply

Blocks: in `find_page`, the header reads of the current member, the `retire_expire` store and
`move_to_full` for a full member, the candidate comparison with `mostly_used` (behind a debug
assertion that the candidate is not empty), the `extend` of the candidate, the
`collect_retired()`, and `move_to_front` with the `retire_expire` store; in `fresh_page`,
`page::init` and the `push_front` and `extend` of the new page.

- Preconditions: heap invariants hold; `bin` in `1..=MAX_BIN` (it comes from `classify`).
- Invariants relied on: heap invariant 1 for the walk; page invariants of every member; the
  `slices` contract for the run of a fresh page; slice `MAX_SLICE_INDEX` is never handed out.
- Proof sketch. The walk starts at `queues[bin].first` and follows `next`, all members; `next`
  is read before the member is touched. `move_to_full` is applied to the current member, just
  read to have neither a free nor an unextended block (HEAP-05), after its `retire_expire`, a
  heap-owned byte of a live header, is cleared: a page with every block out is not retired, and
  the countdown a direct-table drain leaves behind would otherwise survive the round trip
  through the full queue and make the free that empties the page skip `retire` (HEAP-07, R-2).
  A candidate that reaches
  the comparison with a later member was chosen at a member with an empty free list (an
  available member ends the walk) and the walk changes no page's list, so `used == capacity`
  (page invariant 4); `capacity >= 1` for every queue member because `fresh_page` extends a page
  before it is visible and nothing lowers `capacity`; hence the candidate is never empty, which
  a debug assertion states, and mimalloc's release of an empty candidate has no counterpart
  here (R-5). After the walk the
  candidate is a member with a free block or an unextended one; `extend` on it meets PAGE-04
  and, when the list was empty, succeeds because `capacity < reserved` held when it was chosen
  and nothing changes `capacity` but `extend`. `move_to_front` takes a member; the
  `retire_expire` store is a heap-owned byte of a live header. With no candidate, the pages
  retired earlier are collected (HEAP-07) and a fresh page is built: `acquire_run(n, n)`
  returns `n` free slices starting at a multiple of `n` (Kani `slices::slices_alloc_small_page`,
  `slices_alloc_medium_page`, `slices_alloc_large_page`), so `run.start * SLICE_SIZE` is a
  multiple of the page size; the page cannot reach slice 65535 (`add_free` drops it and growth
  stops at `usable_limit`, test `slices::the_last_slice_of_the_address_space_is_never_usable`),
  so `page_addr + page_size` does not overflow a wasm32 `usize`; the slices were free, hence
  owned by the allocator through `mem` and referenced by no page or run (heap invariant 4);
  `kind == kind_of_bin(bin)` by construction. PAGE-01's preconditions hold. The new page is in no
  queue, so `push_front` applies, and has `capacity == 0 < reserved`, so `extend` succeeds.
- Machine checks: Kani `first_allocation_builds_a_valid_heap` (an empty queue through
  `fresh_page`, both memory starts), `the_search_parks_a_full_page_in_the_full_queue` (the walk
  parks a full page, the supply fails cleanly), `a_free_brings_a_full_page_back_to_its_queue`
  (the page returns as the candidate with exactly the freed block), the slices harnesses named
  above. Tests `page_exhaustion_adds_pages_and_full_queue_round_trips` (four pages of one bin,
  the candidate walk over several members), `retired_pages_are_released_before_memory_grows`,
  `a_forced_collection_reaches_a_retired_page_behind_a_page_in_use`,
  `every_bin_allocates_aligned_distinct_blocks_and_recovers_its_page`, the churn test, the model
  tester and the fuzz targets. Miri as above.
- Not machine-checked by Kani: the candidate comparison with two or more members in one queue;
  tests only.
- Changes: 2026-09-02, `find_page` clears the countdown of a page it parks (R-2, HEAP-07) and
  no longer carries the unreachable release of an empty candidate, replaced by the argument
  above and a debug assertion (R-5).
- Reviewer: adversarial-reviewer, 2026-09-02: accepted with a caveat (R-5: the release of an
  empty candidate the sketch treated as live is unreachable), addressed on 2026-09-02 by
  removing the branch; together with the park reset this awaits a fresh look.

### HEAP-07: retirement and release (`dealloc_transition`, `retire`, `collect_retired`, `release_empty_pages`, `free_page`)

- Preconditions: `page` is a live page of this heap; for `retire`, an empty member of its bin
  queue; for `free_page`, empty and a member of queue `qi`; for `collect_retired` and
  `release_empty_pages`, heap invariants hold.
- Invariants relied on: heap invariant 1; a page's slices are exactly `page_size / SLICE_SIZE`
  slices from its address, claimed since `fresh_page` (heap invariant 4).
- Proof sketch. `dealloc_transition` orders `unfull` before `retire`, so `retire` sees a member
  of its bin queue (HEAP-02). `retire` writes `retire_expire` and widens the retired range to
  the page's bin, or calls `free_page(page, bin)`. It no longer returns early for a page that
  already carries a countdown: mimalloc's `_mi_page_retire` does, because mimalloc's free path
  calls it on every free that empties a page and re-arming would undo the aging of a page that
  oscillates around empty; our free path takes that decision in `needs_transition` (HEAP-02),
  so the early return was unreachable, and refreshing instead keeps the range an
  over-approximation of the bins with retired pages by construction. `free_page` removes the
  page from its queue (HEAP-05, which also fixes the direct table) and returns its slices with
  `slices.free`, which requires them to be handed out and inside the map, true since
  `fresh_page`; afterwards nothing in the heap refers to the page: it is in no queue, no direct
  entry (updated by `remove`), and not the full queue's (an empty page is never there,
  HEAP-02), and the caller holds no other pointer to it (`retire` and `dealloc_transition`
  return; both walks read `next` first; `find_page` moves on). `collect_retired`, the aging
  walk, visits at most `RETIRE_MAX_PAGES` members of each bin queue in the retired range, all
  live, reads `next` before any change, decrements `retire_expire` only when non-zero, and
  frees only empty pages; the range is clipped to `MAX_BIN`. `release_empty_pages`, which
  `acquire_run` runs before memory is grown, walks every member of every bin queue that holds a
  page (the set bits of `occupied & BIN_QUEUE_BITS`, heap invariant 5, so the walk costs the
  pages that exist; `queue_index` keeps the queue index in bounds), reads `next` first, frees
  every empty page and clears the countdown of every other member, then empties the range,
  which is exact because no page carries a countdown afterwards. The retired range and the three-page window
  are therefore hints for the aging walk only; the promise that every retired page is released
  before memory grows rests on the full walk and needs no argument about where a retired page
  can sit. Where a stale countdown comes from: the fast paths never touch `retire_expire`, so
  a retired page drained through the direct table keeps its countdown; `find_page` clears it on
  the candidate it returns and, since 2026-09-02, on a page it parks in the full queue, so a
  page that comes back from the full queue and empties goes through `retire` (R-2); a page
  emptied by fast-path frees while it still carries a countdown stays retired with it and is
  reached by the full walk.
- Machine checks: Kani `freeing_the_last_block_retires_the_page`,
  `an_unforced_collection_ages_a_retired_page`, `a_forced_collection_frees_a_retired_page`
  (`release_empty_pages` over the 60 bin queues: slice free, queue and range empty, direct
  entries back to the sentinel), `freeing_any_live_block_preserves_invariants`,
  `first_allocation_builds_a_valid_heap` and `the_search_parks_a_full_page_in_the_full_queue`
  (both reach `release_empty_pages` through `acquire_run` with every queue empty or holding one
  page). Tests `retired_page_is_kept_then_released` (release when the count runs out),
  `retired_pages_are_released_before_memory_grows`,
  `a_forced_collection_reaches_a_retired_page_behind_a_page_in_use`,
  `parking_a_full_page_clears_its_countdown` (the countdown survives a direct-table drain and
  goes when the page is parked; the page is retired properly after it comes back),
  `the_release_before_growth_frees_every_empty_page_whatever_its_position` (an empty page
  beyond the window with a countdown and an empty range, and one with no countdown at all, both
  freed), `retire_refreshes_the_countdown_and_the_range_of_a_retired_page`,
  `page_exhaustion_adds_pages_and_full_queue_round_trips`, the churn test (a full release at
  the end), `tests/review_edge_cases.rs` (`an_emptied_page_behind_three_others_is_released_before_memory_grows`,
  the R-2 reproducer, no longer ignored, and `a_page_reused_through_the_direct_table_is_still_released`),
  the model tester and the fuzz targets. Miri as above.
- Not machine-checked by Kani: `retire`'s immediate release (a queue of more than one page, or
  more than three), which needs a second page, and the full walk over a queue of several pages;
  tests only.
- Changes: 2026-09-02, review finding R-2: `collect_retired(force)` split into the aging walk
  `collect_retired()` and `release_empty_pages()`, which walks every bin queue; `retire`
  refreshes instead of returning early; `find_page` clears the countdown of a page it parks
  (HEAP-06). The roofline's alloc_free_32 and batch_lifo_32 are unchanged within noise (numbers
  in the commit message). The Kani harnesses named above were re-run. A first version walked
  all 60 queue heads in a plain loop; CBMC unrolled the inner queue walk once per queue and the
  three harnesses that reach the walk ran past the 4 GiB cap, which is why the walk iterates the
  occupancy bits (one iteration per queue with pages) and the harnesses keep their unwind bound
  of 2.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted with a caveat (R-2), addressed by the
  change above; fresh review of the changed blocks pending.

### HEAP-08: header reads in `needs_transition` and `mostly_used`

- Preconditions: `page` points at a header written by `init` (the masked header of a live block
  in `dealloc`, a queue member in `find_page`).
- Proof sketch: as PAGE-05, reads of fully initialised fields of their own types through the
  raw pointer; `mostly_used`'s products are of counts at most 8188, far below `usize::MAX`.
- Machine checks: every structural heap harness and every heap test reaches both; Miri as above.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted.

### HEAP-09: `validate_queue_inner`, `validate_direct_entry` and the harness helpers (test and proof infrastructure only)

- Compiled only under `cfg(test)` and `cfg(kani)`; never part of the allocator.
- Proof sketch: the walk follows queue links, relying on the invariant it checks, and stops
  after `count + 1` members so a cycle is reported rather than looped on; `page::validate`
  (PAGE-06) guards every free-list read; the occupancy bit is compared with the count before
  the walk (heap invariant 5, 2026-09-02). The proof-only backends `HeapModel` and `QueueModel`
  assert that every address the heap touches is a header or a modelled block word, so a stray
  access fails the proof instead of reading outside the model.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted.

## global

### GLOBAL-01: `unsafe impl Sync for WasmAlloc`

- Preconditions: the program has one thread.
- Proof sketch: `Sync` is required for a `static` and is the only thing that lets two threads
  call the `GlobalAlloc` methods concurrently. This crate compiles only for wasm32 without the
  `atomics` target feature (`compile_error!` otherwise), and such a module cannot start a second
  thread: there is no shared memory and no thread primitive. Every access to the heap goes
  through `heap()` (GLOBAL-02). The Miri and Kani runs do not reach this block (it exists only
  on wasm32); the wasm32 end-to-end test `tests/global_wasm.rs` runs the static under wasmtime.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted.

### GLOBAL-02: `WasmAlloc::heap` and the four `GlobalAlloc` methods

Blocks: `&mut *self.heap.get()`, and in each method the call into the heap, with
`NonNull::new_unchecked(ptr)` in `dealloc` and `realloc`.

- Preconditions: no other `&mut Heap` is live (calls into the allocator never nest); for
  `dealloc` and `realloc`, `ptr` was returned by this allocator (the `GlobalAlloc` contract).
- Proof sketch: `heap()` creates a `&mut Heap` that lives for one method call. Calls cannot
  nest because the heap allocates nothing (`no_std`, no collections, no formatting) and cannot
  unwind: every target this crate compiles for is `panic-strategy = "abort"` in its target
  specification (`rustc --print target-spec-json` for `wasm32-unknown-unknown`, `wasm32-wasip1`
  and `wasm32-wasip2` all say so), a property of the target that no consumer's build profile
  changes; the crate's own `[profile.release] panic = "abort"` does not carry this, since a
  dependency's profile is never inherited (R-6). In a release build the allocator's own code also
  has no panic call site at all (PAGE-04, PAGE-01, checked on the roofline build on 2026-09-02).
  In a build with debug assertions a failing assertion runs the panic hook before the abort,
  and std's hook formats the message and may allocate (`queue_index`'s `debug_assert!` carries a
  formatted message), which re-enters the allocator while a `&mut Heap` is live; this can only
  happen once an invariant is already broken, and is the reason the allocator's own invariant
  checks are debug-only. The single-thread argument is GLOBAL-01. `ptr` is non-null because
  `alloc` never returns a null block as a success (HEAP-01, HEAP-04) and the contract requires a
  pointer this allocator returned.
- Machine checks: `tests/global_wasm.rs` under wasmtime (std collections, churn, zeroed and
  over-aligned allocations through the static). The heap methods themselves are HEAP-01 to
  HEAP-04.
- Changes: 2026-09-02, the no-unwind argument cites the target specifications and the
  debug-hook allocation is named (R-6); no code changed.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted with a caveat (R-6: cite the targets'
  `panic-strategy`, not the profile, and note the debug-hook allocation), both folded into the
  text above on 2026-09-02.

### GLOBAL-03: `unsafe impl GlobalAlloc for WasmAlloc`

- Contract discharged: blocks are aligned to `layout.align()` and hold `layout.size()` bytes
  (HEAP-01, HEAP-04, `bins`); live blocks never overlap (page invariant 4 within a page, heap
  invariant 4 and the slice bitmap across pages and runs); `alloc_zeroed` returns
  `layout.size()` zero bytes (HEAP-01, HEAP-04); `dealloc` and `realloc` recover the block's
  page or run from the Layout the caller passes back (HEAP-02, HEAP-03); `realloc` preserves
  `min(old, new)` bytes and leaves the old block untouched on failure (HEAP-03); no method
  unwinds (GLOBAL-02). A zero-size request is undefined behaviour for the caller under the
  contract; the implementation still returns a distinct 8-byte block.
- Machine checks: as GLOBAL-02; the model tester (`tests/model_heap.rs`, `tests/model_system.rs`
  against `System` for the tester's own soundness, `tests/model_mutants.rs` for its power).
- Reviewer: adversarial-reviewer, 2026-09-02: accepted. Caveat: discharged among this allocator's
  own blocks; on wasi targets wasi-libc's malloc handed out the same bytes (R-1), resolved on
  2026-09-02 by BACKEND-02's per-target heap base.

## backend

### BACKEND-01: the `Memory` contract

`unsafe trait Memory`: implementors promise fresh, zero-filled, exclusively owned slices from
`grow`, indices at most `MAX_SLICE_INDEX`, a `size_slices` that never decreases, a `heap_base`
from which everything to the end of memory (except what `grow` has not handed out) is the
allocator's, `ptr(addr)` valid with provenance over the owned region, and (stated 2026-09-02,
relied on throughout `heap` and `page`) `ptr(addr).addr() == addr`. Every unsafe block in
`heap` and `page` that dereferences a `Memory::ptr` result cites this contract; the entries
below argue that the implementations meet it.

- Reviewer: adversarial-reviewer, 2026-09-02: accepted.

### BACKEND-02: `unsafe impl Memory for WasmMemory`

- Proof sketch. `grow`: `memory.grow` returns fresh pages that the specification zero-fills,
  contiguous with the previous end unless another party grew memory in between, in which case
  the returned index is still the start of our region and the contract allows the gap
  (`slices::acquire` and `extend_with_growth` handle it); nothing else learns the index, so the
  pages are exclusively ours. `size_slices` is `memory.size`, which never decreases. `ptr` is
  `with_exposed_provenance_mut(addr)`: linear memory is one allocation in Rust's model of wasm,
  every address below `memory.size() * SLICE_SIZE` is valid, and the returned pointer has
  address `addr`. `grow` maps the `usize::MAX` failure code to `None`. `heap_base` is read by
  the heap once, in `ensure_init`, and what it claims for the allocator is per target
  (`wasm_heap_base`):
  - `wasm32-unknown-unknown`: the address of the linker-provided `__heap_base` symbol, taken
    with `addr_of!` and never read. wasm-ld defines it as the end of static data and the shadow
    stack, and nothing in a Rust program on this target allocates linear memory between it and
    the initial `memory.size` except an allocator: std's is `System` (dlmalloc-rs), which this
    crate replaces as the global allocator and which std never calls directly, so the gap is
    unused. Checked against the std of Rust 1.95: its `libdlmalloc` rlib for this target is
    dlmalloc 0.2.11 and contains no reference to `__heap_base`. The one way to break this is a
    program that also allocates through `std::alloc::System` explicitly, since dlmalloc-rs 0.2.13
    and later donate `[__heap_base, __heap_end)` to themselves on their first allocation; that
    combination is unsupported.
  - wasi (`target_os = "wasi"`, so wasip1 and wasip2): `memory.size * SLICE_SIZE` at the time
    of the call, saturating (a 4 GiB memory puts the base in the last slice, which
    `initial_free_range` and `usable_limit` treat as nothing usable). wasi-libc's dlmalloc makes
    `[__heap_base, __heap_end)` its first segment the first time any libc `malloc` runs
    (`try_init_allocator`), and std reaches libc `malloc` even with this crate installed
    (`__wasilibc_populate_preopens`, `__wasilibc_initialize_environ`, `opendir`), so the gap is
    not ours; anything dlmalloc grows memory for afterwards is its own too, and lies below the
    end of memory at our first allocation. With the base at that end the initial free range is
    empty and every slice this heap ever owns comes from its own `memory.grow`, whose pages
    nobody else learns of. Growth by either allocator in between only makes the other's regions
    non-contiguous, which both tolerate.
- Assumption to keep in view: a second allocator or hand-written `memory.grow` in the same
  module is compatible only if it never touches slices this allocator has been given (it may
  grow memory itself); on `wasm32-unknown-unknown` it must also leave the linker gap alone.
- Machine checks: `tests/wasi_libc_gap.rs` under wasmtime, linked with an 8 MiB initial memory
  by `build.rs` (`rustc-link-arg-tests`, so the roofline harness keeps its default layout) so
  that the gap holds about a hundred whole slices: no block or run of this heap lies in
  `[__heap_base, __heap_end)`, a libc `malloc` block covering three quarters of the gap is
  disjoint from every live block, and writes through either leave the other intact. Before the
  change the first assertion failed with a run at `[0x260000, 0x270000)` inside the gap
  `[0x115490, 0x800000)`. `tests/global_wasm.rs` under wasmtime with the same link. The
  wasm32-wasip1 unit tests exercise the slice logic against `SimMemory`, not this backend.
- Changes: 2026-09-02, `heap_base` made target-dependent as above (review finding R-1: the
  previous entry claimed the gap for every target, which is false on wasi, where the two
  allocators handed out the same bytes whenever the gap held a whole slice).
- Reviewer: adversarial-reviewer, 2026-09-02: REJECTED (R-1) before the change above. Fresh
  review of the rewritten entry pending.

### BACKEND-03: `SimMemory::from_region`, `SimMemory::grow` and `unsafe impl Memory for SimMemory`

- Preconditions (`from_region`): `base..base + len` is valid for reads and writes for the life
  of the value and accessed by nobody else; the constructor panics on an unaligned base, a
  length that is not whole slices, an initial size beyond the region or a heap base beyond the
  initial size.
- Proof sketch: `grow` checks `end_slice + slices` against the region's last slice before
  zero-filling the new slices with `write_bytes` from `base.add(offset)`, an in-bounds range of
  the caller's region; each slice is handed out once because `end_slice` only increases.
  `ptr` is `base.with_addr(addr)`, which keeps `base`'s provenance over the whole region and has
  address `addr`; it is valid for every address in the region, and the heap only passes
  addresses of memory it owns (its callers' entries), which lie in `[heap_base, end)`. The
  debug assertions check the range. `skip_slices` models another party's growth and never hands
  those slices to the allocator. On a 64-bit host slice indices exceed `MAX_SLICE_INDEX`; the
  `slices` module's `usable_limit` treats a map entirely above that index as having no hole,
  which is why the assertion on the index is conditional on the pointer width.
- Machine checks: tests `sim_grows_zeroed_and_contiguously_until_skipped`,
  `sim_pointers_round_trip_addresses`; every heap, page and slices test and the model tester run
  over this backend under Miri with strict provenance (Stacked Borrows) and under Tree Borrows,
  which is the check that `with_addr` keeps the right provenance.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted.

### BACKEND-04: `testing::HostRegion` and `testing::Region` (test infrastructure only)

- Compiled only under `cfg(test)` or the `testing` feature; never part of a
  `#[global_allocator]` build.
- Proof sketch: `HostRegion::new` allocates with a non-zero Layout and panics on null; `Drop`
  frees with the same Layout exactly once. `simulate` hands the region to `from_region` with
  the caller promising exclusivity and that the memory dies first; `Region` enforces both by
  owning exactly one `SimMemory` and declaring it before the region so it drops first.
- Machine checks: every test that uses `Region` or `SimHeap`, under Miri as above.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted. Caveat: `HostRegion::new(0)` would hand a
  zero-size Layout to `alloc`; no caller does, but the constructor should refuse it (R-7).

## slices

`slices` has no unsafe code. The bitmap helpers there gained explicit `w < WORDS` tests
(2026-09-02) so that the release build carries no bounds-check panic path; they are safe code
and are covered by the twelve slices Kani harnesses.

Not listed: the unsafe blocks inside `#[cfg(test)]` test code and the proof-only backends
under `#[cfg(kani)]` (`page::verify::PageModel`, `heap::verify::HeapModel` and `QueueModel`),
which never ship; their safety arguments are in their `SAFETY` comments and HEAP-09.

## Review findings (adversarial review, 2026-09-02)

Reviewer: adversarial-reviewer (did not write any of the code). Scope: the 22 entries above,
the unsafe blocks they cover, and the machine checks they cite. Verdict: 21 accepted (8 of them
with a caveat), 1 rejected (BACKEND-02). Ranked by severity.

- R-1 (memory-safety bug, reproduced; wasm32-wasip1 and wasip2): `Heap::ensure_init` reclaims
  every whole slice of the linker gap `[__heap_base, initial memory end)`. wasi-libc's dlmalloc
  does the same: its `try_init_allocator` (present in the wasm32-wasip1 sysroot's `libc.a`,
  referencing `__heap_base` and `__heap_end`) makes that gap its first segment the first time
  any libc `malloc` runs, and std reaches libc malloc through `__wasilibc_populate_preopens`,
  `__wasilibc_initialize_environ` and `opendir` (`std::fs`, `std::env`). With the default link
  the gap is the tail of one slice and this heap uses none of it, so nothing collides by luck;
  with `--initial-memory` widening it, both allocators hand out the same bytes.
  `tests/wasi_libc_gap.rs` shows it: built with
  `RUSTFLAGS="-C link-arg=--initial-memory=8388608" cargo test --target wasm32-wasip1 --test wasi_libc_gap`,
  libc `malloc` returned `[0x114e90, 0x270010)`, which contains this heap's live run
  `[0x260000, 0x270000)`, and a write inside the libc block changed the run. BACKEND-02's
  exclusivity claim for the gap is false on wasi targets. A fix is not the reviewer's to make;
  the obvious one is to start the heap at `max(__heap_base, __heap_end)` (or at the current end
  of memory) on `target_os = "wasi"`, and to say in BACKEND-02 which targets may reclaim the gap.
- R-2 (documented property violated, reproduced; footprint, not memory safety): the heap
  documents that every retired page is released before linear memory grows, and `acquire_run`
  relies on `collect_retired(true)` for it. The fast paths never touch `retire_expire`, the
  collection visits only the first `RETIRE_MAX_PAGES` members of the queues in the retired
  range, and `needs_transition` skips `retire` when `retire_expire != 0`. A page that is
  retired, drained through the direct table, parked in the full queue by the next search,
  brought back to the end of its bin queue by one free and then emptied behind three pages in
  use keeps `retire_expire != 0` with `used == 0` and is never released: memory grows with a
  whole empty slice in a queue. `tests/review_edge_cases.rs`,
  `an_emptied_page_behind_three_others_is_released_before_memory_grows` (ignored; run with
  `-- --ignored`) grows memory by two slices in that state.
- R-3 (proof sketch incomplete, no reproducer): HEAP-04 and the `Memory` contract take the
  linker gap as exclusively the allocator's; on wasi targets that is R-1. The heap's own
  invariants are fine; the precondition is what fails.
- R-4 (proof sketch incomplete): HEAP-04's non-null argument for runs ("positive on wasm")
  depends on `__heap_base > 0`; a run at slice 0 would be address 0 and the
  `NonNull::new_unchecked` would be undefined. Every wasm-ld layout places data or the stack
  below `__heap_base`, so it holds, but it is an assumption the entry should state.
- R-5 (documentation): HEAP-06 describes `find_page`'s `free_page(candidate, qi)` as a live
  path. It is unreachable: a candidate that survives the walk has an empty free list (an
  available page ends the walk), so `used == capacity`, and `capacity >= 1` for every queue
  member (`fresh_page` extends before anything else sees the page), so `all_free` is false.
- R-6 (documentation): GLOBAL-02 credits the release profile's `panic = "abort"` for the
  absence of unwinding; consumers do not inherit a dependency's profile. The property holds
  because both wasm32 targets are `panic-strategy = "abort"` (verified from the target specs).
- R-7 (documentation, test infrastructure): `HostRegion::new(0)` would pass a zero-size Layout
  to `std::alloc::alloc`; no caller does.

Machine checks run for this review (all in the `review` worktree): host tests, wasm32-wasip1
tests under wasmtime, `cargo fmt --check`, `cargo clippy --all-targets -D warnings`; every one
of the 33 Kani harnesses (`scripts/kani`, at most four per invocation, `KANI_MEM=4G`), all
successful, the heap structural harnesses peaking at 3.8 GB; Miri with `-Zmiri-strict-provenance`
over the `page` (11 tests) and `heap` (18 tests) unit tests and with `-Zmiri-tree-borrows` over
the `page` tests, all clean; a 3-minute `model_heap` fuzz run (474k executions), clean; and the
new probes in `tests/review_edge_cases.rs` (every size within an alignment of each class
boundary with every natural alignment and a realloc each way, realloc shrink-then-grow chains
across bins and kinds with frees at the shrunk Layout, runs aligned to 2^16 through 2^30, a
memory too small to serve with refusals interleaved with live blocks, a heap base in the last
slice, and a page reused through the direct table), all passing. Kani harness names, test
names and Miri claims cited by the entries were checked to exist and to cover what they say.
