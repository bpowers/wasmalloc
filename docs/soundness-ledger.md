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

Running the harnesses an entry names: only through `scripts/kani` (never bare `cargo kani`), and
the structural heap harnesses one per invocation, as `scripts/kani-full` runs every harness. One
`cargo kani` invocation runs its harnesses sequentially in one scope, and CBMC's residue
accumulates across them, so two structural heap harnesses that each verify alone at 4 GiB in
under a minute are OOM-killed together at the same cap (R2-2, below). The arithmetic, bins, page
and slices harnesses are small enough to share an invocation, which is how `scripts/kani-quick`
runs the gate's set.

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
- Reviewer: adversarial-reviewer-2, 2026-09-02: accepted. The divisor is `bin_size(bin)` with `bin`
  in `1..=MAX_BIN`, never zero, so the `1` branch of `blocks_per_page` is dead for every caller; the
  geometry harness and the page tests were re-run.

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
- Reviewer: adversarial-reviewer-2, 2026-09-02: accepted. Re-derived: `init` is the only writer of
  `block_size` and stores `bin_size(bin) >= 8`; `extend` is called on `find_page`'s candidate and
  `fresh_page`'s new page, both queue members, and the sentinel (`block_size == 0`) is never linked
  into a queue, so `.max(1)` selects no reachable value. Both page operation-sequence harnesses, the
  geometry harness and Miri (strict provenance) over the page tests re-run.

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
proof rests on tests only, the entry says so. Where an entry says a structural harness was
re-run, it was run in its own `scripts/kani` invocation (`scripts/kani --harness <name>`); see
the note at the top of this ledger and R2-2 for why two of them in one invocation do not fit
under the 4 GiB cap.

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
- Proof sketch. Size: `direct_size(layout)` is `layout.size()` for `align <= WORD`, the size
  rounded up to `align` for `WORD < align <= MAX_NATURAL_ALIGN` (the rounding `bins::classify`
  does; `size + align - 1` cannot overflow because `Layout` guarantees the rounded size fits
  `isize` and `align <= MAX_NATURAL_ALIGN`), and `usize::MAX` for a larger alignment, which
  fails the `<= DIRECT_MAX_SIZE` test so that such a request reaches `alloc_generic` without a
  second return path (it classifies as `Huge` there, HEAP-04). Index: `direct_index(size) <
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
- Changes: 2026-09-02, tuning-c: the two fast paths take their size from `direct_size` (safe
  code) instead of rounding inline with an early return for over-aligned requests; the unsafe
  blocks are unchanged and reached under the same condition (`size <= DIRECT_MAX_SIZE` after
  rounding, never for `align > MAX_NATURAL_ALIGN`). The three arithmetic harnesses re-run.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted; the tuning-c change awaits a fresh look.
- Reviewer: adversarial-reviewer-2, 2026-09-02: accepted. `direct_size` is the size for `align <=
  WORD`, the rounding `classify` performs for `WORD < align <= MAX_NATURAL_ALIGN` (no overflow: a
  Layout's rounded size fits `isize`), and `usize::MAX` above it, which fails the table test, so the
  pop is reached exactly when `classify` yields a bin whose direct range holds the index (Kani
  `dealloc_fast_path_agrees_with_classify`, re-run). One note outside the contract (R2-4): a
  zero-size request with an alignment above `WORD` rounds to size 0, indexes slot 0 and gets a block
  aligned to 8 only; nothing else changes, and `dealloc` with that Layout finds the page.

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
- Proof sketch. Kind: the fast path tests `layout.size() <= SMALL_MAX_OBJ_SIZE &
  align.wrapping_neg()`, the small limit rounded down to the alignment, which for a power-of-two
  `align` holds exactly when the size rounded up to `align` (the rounding `alloc` and
  `bins::classify` perform, HEAP-01) is at most `SMALL_MAX_OBJ_SIZE`, and that holds exactly
  when `classify(layout)` is a bin of kind `Small`, for every Layout with `align <=
  MAX_NATURAL_ALIGN` (both equivalences in Kani `dealloc_fast_path_agrees_with_classify`);
  Layouts with a larger alignment skip the fast path and are classified in `dealloc_generic`; `alloc` served the
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
- Changes: 2026-09-02, tuning-c: the fast-path condition compares the size against the masked
  limit instead of rounding the size (safe arithmetic before the unsafe block, proved equal in
  the harness above); the unsafe blocks are unchanged. `freeing_any_live_block_preserves_invariants`
  and `a_free_brings_a_full_page_back_to_its_queue` re-run.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted; the tuning-c change awaits a fresh look.
- Reviewer: adversarial-reviewer-2, 2026-09-02: accepted. The mask identity re-derived by hand: for
  a power of two `a`, `round_up(s, a) <= L` holds exactly when a multiple of `a` lies in `[s, L]`,
  that is when `s <= round_down(L, a) == L & -a`; for `a <= WORD` the mask leaves
  `SMALL_MAX_OBJ_SIZE` (a multiple of 8) intact, matching `classify`'s unrounded size. The harness
  models the code as written (`size <= (SMALL_MAX_OBJ_SIZE & align.wrapping_neg())`) for every
  Layout with `shift <= 12`; re-run. `tests/review_edge_cases.rs` now probes every bin edge and both
  run boundaries with every natural alignment on real memory (see R2 test speed note below).

### HEAP-03: `Heap::realloc`

Blocks: `Layout::from_size_align_unchecked`, the `release_empty_pages()` before a run is grown
through memory or moved, the `self.alloc(new_layout)` for a move, and the `copy_nonoverlapping`
followed by `self.dealloc(ptr, layout)`.

- Preconditions: as HEAP-02 for `ptr` and `layout`; `new_size != 0` and `new_size` rounded up
  to `layout.align()` does not overflow `isize` (the `GlobalAlloc::realloc` contract).
- Invariants relied on: `fits_in_place`'s guarantee (below); page invariant 4 and heap invariant
  4 (live blocks and handed-out runs are disjoint from everything the allocator can hand out).
- Proof sketch. The shortcut before any classification returns `ptr` when `align <= WORD`,
  both sizes are at most `DIRECT_MAX_SIZE` and `direct_index` agrees on them: sizes with one
  direct index have one bin (Kani `direct_table_tiles_and_matches_bin`: `bin(direct_index(s) *
  WORD) == bin(s)`), `classify` with `align <= WORD` is `Bin(bin(size))`, and
  `fits_in_place(b, b, new_size)` holds, so this is exactly the decision the general path takes
  for such a pair (asserted for every such pair in `realloc_in_place_keeps_the_kind_and_fits`).
  The unchecked Layout meets `from_size_align`'s requirements exactly by the
  contract (the alignment is a Layout's, hence a power of two). In place within a page:
  `fits_in_place(old, new, new_size)` implies `kind_of_bin(old) == kind_of_bin(new)` and
  `bin_size(old) >= round_up(new_size, align)` (Kani `realloc_in_place_keeps_the_kind_and_fits`,
  over every pair of Layouts with `align <= MAX_NATURAL_ALIGN`), so the block holds every byte
  the caller may use through the new Layout and a later `dealloc` or `realloc` with it masks with
  the same page size (HEAP-02); the alignment is unchanged. In place within a run: the length
  becomes `huge_slices(new_layout)`, which a later `dealloc` with the new Layout recomputes;
  `shrink` releases slices of the run itself, `try_extend` and `extend_with_growth` claim only
  free slices directly after it or freshly grown ones (proved in `slices::verify`), and all
  three leave the run's own slices claimed. When `try_extend` cannot serve a growth from the
  map, `release_empty_pages` runs before `extend_with_growth` grows memory or the block moves
  (R2-1): its precondition (heap invariants, no page pointer held, HEAP-07) holds because the
  block is a run, not a page, and `realloc` refers to no page at that point; the walk frees the
  slices of empty pages and clears countdowns, touching neither the run nor its Layout, so
  `extend_with_growth`'s arguments still describe a handed-out run, and a released page's
  slices right after the run are exactly free slices `try_extend` may then claim. The move:
  `alloc_huge` or `alloc` returns a block of at least
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
  `a_run_grows_in_place_into_an_empty_page_released_before_growth` (the walk frees the page in
  the run's way and the run grows into it without a copy or a grow),
  `realloc_preserves_contents_across_classes`, the churn test's realloc step, the model tester
  (content checks across every move), and in `tests/review_edge_cases.rs`
  `in_place_run_growth_releases_every_empty_page_before_memory_grows` (the R2-1 reproducer,
  asserting the property). Miri as above.
- Not machine-checked by Kani: the copy on a move (the model has no block bodies); tests and
  the model tester check contents.
- Changes: 2026-09-02, tuning-c: the direct-index shortcut above (safe code before the unsafe
  blocks), the new Layout classified once and reused for the move, and `#[inline(always)]` on
  `bins::classify`, `fits_in_place` and `huge_slices` (no semantic change; at `opt-level = "z"`
  they were out-of-line calls with an sret round trip through the shadow stack, see
  `docs/research/simlin-profile.md` section 6). The unsafe blocks are unchanged.
  2026-09-02, R2-1: the Huge-to-Huge growth tries the map alone first (`try_extend`), then
  releases every empty page (`release_empty_pages`, a new unsafe call in this function) before
  `extend_with_growth` grows memory or the block moves; the other unsafe blocks are unchanged.
  The heap harnesses named under HEAP-07 were re-run, one per invocation. Awaits a fresh look.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted; the tuning-c change awaits a fresh look.
- Reviewer: adversarial-reviewer-2, 2026-09-02: accepted. On the shortcut: it fires for a proper
  subset of the pairs whose block size is unchanged (two sizes in adjacent slots of one bin, such as
  65 and 80, take the general path and get the same answer); a zero `new_size` or a zero old size,
  both outside the contract, miss the shortcut and are served by the general path without any
  out-of-bounds access; an old alignment above `WORD` is excluded by the first test, so over-aligned
  blocks (runs) never reach it. The in-place argument is inductive over chains of reallocs: each
  in-place step keeps `kind_of_bin` of the held Layout's bin equal to the page's kind and
  `block_size` at least the held size, and the entry's precondition ("a size that classifies as
  `realloc` left it") is exactly that induction hypothesis. Because the half bound is taken from the
  held Layout's bin rather than the block, a chain of shrinks can walk a 1 KiB block down to an
  8-byte Layout of the same kind (footprint only, R2-3; test
  `realloc_shortcut_after_an_in_place_shrink_keeps_contents` shows the chain with contents checked
  and the block freed through the final Layout). Kani `realloc_in_place_keeps_the_kind_and_fits`
  re-run.

### HEAP-04: `alloc_generic`, `alloc_huge` and `acquire_run`, the slow-path hand-out

Blocks: in `alloc_generic`, the `page::has_free` read of the bin queue's first page, the
`collect_retired(false)`, `find_page`, `page::pop` and the zeroing block; in `alloc_huge`, the
`write_bytes` and `NonNull::new_unchecked`; in `acquire_run`, the `collect_retired(true)`.

- Preconditions: as HEAP-01; heap invariants hold at entry.
- Invariants relied on: the `slices` contract (a returned run was free, hence owned and
  unreferenced, and is claimed now), whose ownership of the initial free range is the `Memory`
  contract's per-target claim (BACKEND-02: on `wasm32-unknown-unknown` the linker gap is unused
  by anything else; on wasi the range is empty and every slice comes from this heap's own
  `memory.grow`); page invariants of the page `find_page` returns; heap invariants between
  operations for the collections; `heap_base > 0` (below).
- Proof sketch. The page popped is either the first page of the queue of `bin`, taken when
  `bin > DIRECT_MAX_BIN` and its free list is non-empty, or what `find_page` returns. Queue
  head: `queues[bin].first` is 0 or a queue member (heap invariant 1), hence a live page of
  `bin` whose header `has_free` may read (PAGE-05), and it is 0 before `ensure_init` has run
  because every queue starts empty and only `fresh_page`, after `ensure_init`, adds pages; the
  bin's blocks are `bin_size(bin) >= rounded size` bytes and aligned for the Layout exactly as
  the direct table's are (HEAP-01, and the table is a cache of these very heads by heap
  invariant 3; Kani `dealloc_fast_path_agrees_with_classify` pins `bin > DIRECT_MAX_BIN` to
  the sizes the table does not cover). `find_page` returns a queue member of `bin` whose free
  list is non-empty (it found one, extended an expandable one, or built a fresh one and
  extended it, HEAP-06). Either way `pop` meets PAGE-02 and returns a block; if it did not, the
  release build returns `None` rather than dereferencing anything. Zeroing as in HEAP-01 with
  `bin_size(bin) >= layout.size()`. `alloc_huge`: `huge_slices(layout) * SLICE_SIZE >= layout.size()` and the run
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
  fresh page, with the linker gap or through `grow`),
  `an_allocation_above_the_direct_table_pops_the_queue_head` (the queue-head path on a
  prepared page, the generic counter untouched), `the_search_parks_a_full_page_in_the_full_queue`
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
  2026-09-02, tuning-c: `alloc_generic` classifies first, then serves a request above the
  direct table from the bin queue's first page when that page has a free block, and runs
  `ensure_init`, the collection counter and `find_page` only otherwise (the `has_free` read
  is a new unsafe block, the `pop` has the second source above); test
  `requests_above_the_direct_table_pop_from_the_queue_head`, the harnesses named re-run.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted with two caveats (R-3, R-4), both
  addressed in the text above on 2026-09-02; fresh review of the wording and of the tuning-c
  change pending.
- Reviewer: adversarial-reviewer-2, 2026-09-02: accepted, wording and the queue-head path. The head
  of `queues[bin]` is a member of that bin queue, never of the full queue (a different index), of
  bin `bin` and hence of its kind (heap invariant 1), possibly retired: the pop then leaves a stale
  countdown exactly as the direct table does, which `collect_retired` (un-retires a page in use),
  `find_page` (clears it on the candidate and on a parked page) and `release_empty_pages` (clears or
  frees) all tolerate. Bins above `DIRECT_MAX_BIN` have no direct entries, so heap invariant 3 is
  untouched, and nothing on the path changes a queue. Test
  `a_retired_page_at_the_queue_head_is_reused_and_still_released`. A consequence for the
  documentation of `GENERIC_COLLECT_PERIOD`: it now counts page searches, not slow-path allocations
  (R2-3). The non-null and ownership wording (R-3, R-4) is accurate. Kani
  `an_allocation_above_the_direct_table_pops_the_queue_head`,
  `first_allocation_builds_a_valid_heap`, `the_search_parks_a_full_page_in_the_full_queue` re-run,
  one per invocation (R2-2).

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
- Reviewer: adversarial-reviewer-2, 2026-09-02: accepted. The six operations are the only writers of
  `queues`, `occupied` and the links; both pushes set the bit after incrementing `count`, `remove`
  clears it when `count` reaches zero, the three moves are compositions of those, `free_page` is a
  `remove`, and `Heap::new` starts with every queue empty and no bit set, so a queue with pages
  always has its bit and an empty queue never does. The shift amount is `queue_index(qi) < 64`. The
  queue harnesses check the bit of both queues through `validate_queue_inner` and of a symbolic
  other queue; re-run.

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
- Reviewer: adversarial-reviewer-2, 2026-09-02: accepted. The candidate argument re-derived: a
  candidate that reaches the comparison was chosen with `free == 0` (an available page ends the
  walk) and the walk changes no list, so `used == capacity >= 1`. One addition to the sketch: when
  `fresh_page` is reached, every member of queue `bin` was parked (the walk only ends early with a
  candidate), so the `release_empty_pages` inside `acquire_run` walks other queues only and cannot
  free a page `find_page` refers to. The park reset is a store to a heap-owned byte of a live
  member. Kani `the_search_parks_a_full_page_in_the_full_queue`,
  `a_free_brings_a_full_page_back_to_its_queue`, `first_allocation_builds_a_valid_heap` re-run.

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
  `acquire_run` runs before memory is grown for a page or a fresh run and `Heap::realloc` runs
  before a run at the top of the heap is grown in place through memory or moved (HEAP-03),
  walks every member of every bin queue that holds a
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
  `page_exhaustion_adds_pages_and_full_queue_round_trips`,
  `a_run_grows_in_place_into_an_empty_page_released_before_growth` (the walk from `realloc`
  frees the page in a run's way), the churn test (a full release at the end),
  `tests/review_edge_cases.rs` (`an_emptied_page_behind_three_others_is_released_before_memory_grows`,
  the R-2 reproducer, no longer ignored, `a_page_reused_through_the_direct_table_is_still_released`,
  and `in_place_run_growth_releases_every_empty_page_before_memory_grows`, the R2-1 reproducer,
  asserting that the in-place growth of a run releases the page before memory grows), the
  model tester and the fuzz targets. Miri as above.
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
  of 2. 2026-09-02, review finding R2-1: `Heap::realloc` runs the walk when the free slices
  after a run cannot serve its growth, before `slices::extend_with_growth` grows memory or the
  block moves, so the promise covers both roads that grow memory; the walk itself is unchanged
  and HEAP-03 lists the new call. The harnesses `freeing_the_last_block_retires_the_page`,
  `an_unforced_collection_ages_a_retired_page`, `a_forced_collection_frees_a_retired_page` and
  `freeing_any_live_block_preserves_invariants` were re-run, one per invocation.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted with a caveat (R-2), addressed by the
  change above; fresh review of the changed blocks pending.
- Reviewer: adversarial-reviewer-2, 2026-09-02: accepted for the blocks, with a caveat on the
  promise (R2-1): the walk runs in `acquire_run`, that is before memory grows for a page or a new
  run, but not before the in-place growth of a run at the top of the heap (`Heap::realloc`, Huge to
  Huge, through `slices::extend_with_growth`), which grows memory while empty pages sit in their
  queues (test `in_place_run_growth_grows_memory_while_an_empty_page_is_kept`). Footprint wording,
  not soundness. Addressed on 2026-09-02 by the walk in `Heap::realloc` (Changes above); the test
  is now `in_place_run_growth_releases_every_empty_page_before_memory_grows` and asserts the
  property. The walk itself re-derived: `pending` is a snapshot of the bits, each queue is
  walked from `first` with `next` read before `free_page`, `free_page` is a `remove` on the page's
  own queue (invariant 1) plus `slices.free` of exactly the page's slices, and a full-queue page
  never carries a countdown (only `find_page` parks, after clearing it), so emptying the range
  afterwards is exact. Kani `a_forced_collection_frees_a_retired_page`,
  `an_unforced_collection_ages_a_retired_page`, `freeing_the_last_block_retires_the_page`,
  `freeing_any_live_block_preserves_invariants` re-run, one per invocation (R2-2).

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
- Reviewer: adversarial-reviewer-2, 2026-09-02: accepted (the reworded no-unwind argument).

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
- Reviewer: adversarial-reviewer-2, 2026-09-02: accepted. R2-4: the zero-size remark should add that
  such a block is aligned to 8 whatever the Layout's alignment (HEAP-01), still outside the
  contract.

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
- Reviewer: adversarial-reviewer-2, 2026-09-02: accepted. Re-derived for wasi: the base is read
  once, in `ensure_init`, so growth by anyone before the first allocation lies below it and growth
  afterwards only makes the regions non-contiguous, which `slices::acquire` sizes from the current
  end each time (in a single-threaded module nothing can grow memory between its `size_slices` read
  and its `grow`, so its retry branch is defensive); `initial_free_range` yields zero slices, and a
  first allocation of any size, huge included, takes the `acquire` path whose room is bounded by
  `usable_limit`; a memory already at 4 GiB saturates to a base in the last slice, which serves
  nothing. Claims checked against the toolchains: Rust 1.95.0's `libdlmalloc-*.rlib` for
  `wasm32-unknown-unknown` names dlmalloc 0.2.11 and holds no `__heap_base` string; the stable
  wasm32-wasip1 sysroot's `libc.a` exports `try_init_allocator`, `__heap_base`, `__heap_end`,
  `__wasilibc_populate_preopens` and `__wasilibc_initialize_environ`, and its `libstd` rlib imports
  `malloc`, `calloc` and `opendir`. `tests/wasi_libc_gap.rs` and `tests/global_wasm.rs` pass under
  wasmtime with the 8 MiB link.

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
- Proof sketch: `HostRegion::new` refuses zero slices with an assertion (R-7), so the Layout it
  allocates with is non-zero, and panics on null; `Drop` frees with the same Layout exactly
  once. `simulate` hands the region to `from_region` with
  the caller promising exclusivity and that the memory dies first; `Region` enforces both by
  owning exactly one `SimMemory` and declaring it before the region so it drops first.
- Machine checks: every test that uses `Region` or `SimHeap`, under Miri as above.
- Changes: 2026-09-02, `HostRegion::new` asserts `total_slices >= 1` (R-7); test
  `a_host_region_refuses_zero_slices`.
- Reviewer: adversarial-reviewer, 2026-09-02: accepted with a caveat (R-7), addressed on
  2026-09-02 by the assertion.
- Reviewer: adversarial-reviewer-2, 2026-09-02: accepted.

## slices

`slices` has no unsafe code. The bitmap helpers there gained explicit `w < WORDS` tests
(2026-09-02) so that the release build carries no bounds-check panic path; they are safe code
and are covered by the twelve slices Kani harnesses.

Growth floor (adversarial-reviewer-2, 2026-09-02): `GrowPolicy::DEFAULT.min_grow` went from 16 to 2
slices. In `grow_and_alloc` the request is `want = max(pad + count, step).min(room)` with `room >=
pad + count` checked first, so a request larger than the step is never cut, the fresh region `[end,
end + want)` holds the aligned run at `end + pad`, and `add_region` never clips it (`want <= room =
usable_limit - end`); `grow_and_extend` has the same shape with `need`. The twelve slices harnesses
(`slices_acquire_stays_inside_memory_and_the_map` and
`slices_extend_with_growth_extends_only_a_top_run` quantify over `min_grow` and `max_grow` below 8)
were re-run.

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
successful, the heap structural harnesses peaking at 3.8 GB (a recipe that no longer reproduces
for the structural heap harnesses since the retirement rework: run each of them in its own
invocation, as `scripts/kani-full` does, or the second in a scope is OOM-killed at 4 GiB, R2-2);
Miri with `-Zmiri-strict-provenance`
over the `page` (11 tests) and `heap` (18 tests) unit tests and with `-Zmiri-tree-borrows` over
the `page` tests, all clean; a 3-minute `model_heap` fuzz run (474k executions), clean; and the
new probes in `tests/review_edge_cases.rs` (every size within an alignment of each class
boundary with every natural alignment and a realloc each way, realloc shrink-then-grow chains
across bins and kinds with frees at the shrunk Layout, runs aligned to 2^16 through 2^30, a
memory too small to serve with refusals interleaved with live blocks, a heap base in the last
slice, and a page reused through the direct table), all passing. Kani harness names, test
names and Miri claims cited by the entries were checked to exist and to cover what they say.

Resolutions (branch `review-fixes`, 2026-09-02): R-1 fixed in `WasmMemory::heap_base`
(BACKEND-02 rewritten; `tests/wasi_libc_gap.rs` with `build.rs` makes the collision part of the
default wasi test run); R-2 fixed by the countdown reset on park, the refreshing `retire` and
`Heap::release_empty_pages` (HEAP-07, HEAP-06, HEAP-02, HEAP-05, HEAP-09; the reproducer is no
longer ignored); R-3 and R-4 stated in HEAP-04; R-5 removed from `find_page` (HEAP-06); R-6
reworded in GLOBAL-02; R-7 guarded in `HostRegion::new` (BACKEND-04); the PAGE-04 caveat applied
(PAGE-04, and PAGE-01 for the same division in `init`). Every entry touched says so under
"Changes" and awaits the reviewer's fresh look.

## Review findings (second adversarial review, 2026-09-02)

Reviewer: adversarial-reviewer-2 (did not write any of the code). Scope: the thirteen entries
whose Reviewer line said a change awaited a fresh look or was rejected before a change (PAGE-01,
PAGE-04, HEAP-01 to HEAP-07, GLOBAL-02, GLOBAL-03, BACKEND-02, BACKEND-04), and every unsafe
block changed since commit 3eacf62: the wasi heap base, the retirement rework and the release
walk with its occupancy bitmask, the realloc entry and its direct-index shortcut, the queue-head
service above the direct table, the dealloc kind test without rounding, the `max(1)` divisions,
and the growth floor. Verdict: 13 accepted (HEAP-07 with a caveat, R2-1), 0 rejected. No
memory-safety bug and no invariant violation was found. Ranked by severity.

- R2-1 (documented property violated, reproduced; footprint, not memory safety): the heap
  module documentation and HEAP-07 promise that every empty page is released before linear
  memory grows. The promise holds on the `acquire_run` road (pages and new runs) but not for the
  in-place growth of a run at the top of the heap: `Heap::realloc` (Huge to Huge) calls
  `slices::extend_with_growth`, whose `grow_and_extend` grows memory without the walk.
  `tests/review_edge_cases.rs`, `in_place_run_growth_grows_memory_while_an_empty_page_is_kept`:
  a retired page in the first slice and a three-slice run above it fill the initial memory; a
  realloc of the run to four slices grows memory and the map holds one free slice afterwards,
  not the two it would hold had the page been released first. Either word the promise as
  "before memory is grown for a page or a fresh run", or release before `grow_and_extend`,
  which would also let an extension succeed when the slices in its way are an empty page.
- R2-2 (machine check as recorded not reproducible): the first review's note says the Kani
  harnesses were run "at most four per invocation, `KANI_MEM=4G`". After the retirement rework
  two heap structural harnesses in one invocation exceed the cap: with
  `scripts/kani --harness a_free_brings_a_full_page_back_to_its_queue --harness
  the_search_parks_a_full_page_in_the_full_queue` the second is OOM-killed at 4 GiB, while each
  verifies alone at 4 GiB in at most 47 s (the residue effect `scripts/kani-full` documents).
  The ledger's recipe should say one heap structural harness per invocation; `kani-full`
  already does that and `kani-quick` excludes them.
- R2-3 (documentation, two nits with a footprint side): `fits_in_place` bounds an in-place
  shrink by half of the held Layout's bin, not by half of the block (mimalloc uses the block's
  usable size), so a chain of shrinks each above half of the current bin walks a block down
  through every bin of its kind: `realloc_shortcut_after_an_in_place_shrink_keeps_contents`
  takes a 1 KiB block to an 8-byte Layout in twelve steps, contents intact, and frees it through
  the final Layout. Sound (HEAP-03's argument is inductive), but a block can hold a Layout far
  below its size until it is freed. And `GENERIC_COLLECT_PERIOD` is documented as "every this
  many slow-path allocations"; since the queue-head service it counts page searches only, so the
  aging walk runs less often under workloads served from queue heads (the release before growth
  is what bounds footprint, so nothing else changes).
- R2-4 (documentation nit): GLOBAL-03 says a zero-size request "still returns a distinct 8-byte
  block"; with an alignment above `WORD` that block is aligned to 8, not to the Layout
  (`direct_size` rounds 0 to 0 and slot 0 is bin 1). Outside the contract either way; `dealloc`
  with the same Layout finds the page.

Answers to the questions the review was asked, in brief: (a) the realloc shortcut fires for a
proper subset of the same-bin pairs and never for an old alignment above `WORD`; zero sizes
miss it and are served by the general path. (b) The queue head is a bin-queue member of the
request's bin and kind, never a full-queue page, possibly retired; the pop keeps the direct
table (the bin has no entries) and leaves a countdown the collections tolerate. (c) The
unrounded test equals the rounded one for every power-of-two alignment (mask identity, Kani
harness models the code as written, every bin edge probed on real memory). (d) The occupancy
bit is maintained by the six queue operations, the only writers of the queues; the harnesses
check it for both queues. (e) The wasi base is read once at first use; earlier growth lies
below it, later growth only makes regions non-contiguous, a huge first request takes the
`acquire` road bounded by `usable_limit`, and a 4 GiB memory saturates to nothing usable.
(f) The growth request is at least the padded need and never clipped below it, so the aligned
run is always inside the fresh region.

Machine checks run for this review (all in the `review2` worktree): host `cargo test` (80 unit
tests, the model tester, the mutants, `review_edge_cases`), `cargo test --target wasm32-wasip1`
under wasmtime 48 (the wasi-libc gap test and the end-to-end test included), `cargo fmt --check`
and `cargo clippy --all-targets -D warnings`; every one of the 33 Kani harnesses at `KANI_MEM=4G`
(arithmetic, bins, page and slices harnesses in groups, each heap structural harness alone),
all successful; Miri with `-Zmiri-strict-provenance` over the `heap` (22 tests, the 100 000-step
census test skipped) and `page` (11 tests) unit tests, clean; a 3-minute `model_heap` fuzz run
seeded with the mixed and align-heavy profiles (68 159 executions, 901 edges), clean; the new
probes in `tests/review_edge_cases.rs` (the shrink chain across every direct slot edge, a
retired page reused through the queue head and released before growth, the R2-1 reproducer, and
a model-tester profile over sizes to 100 KiB with alignments 16 to 4096 and a fifth reallocs,
90 000 operations over three memory starts), all passing.

Resolutions (branch `release-0.1.1`, 2026-09-02): R2-1 fixed in `Heap::realloc`, which runs
`release_empty_pages` when the map alone cannot extend a run, before `slices::extend_with_growth`
grows memory or the block moves (HEAP-03, HEAP-07; the reproducer is now
`in_place_run_growth_releases_every_empty_page_before_memory_grows` and asserts the property, and
`a_run_grows_in_place_into_an_empty_page_released_before_growth` in `heap::tests` shows a run
growing into a released page instead of moving). HEAP-03 awaits the reviewer's fresh look. R2-2
stated at the top of the ledger, in the heap section's introduction and next to the first
review's recipe: structural heap harnesses run one per `scripts/kani` invocation, which is what
`scripts/kani-full` does and why.

Test speed (`class_boundaries_with_every_natural_alignment`): it scanned every size within
`align + 1` of seven boundaries, 37 s here and up to 145 s on the CI runner. It now takes, for
every alignment, the sizes where the rounded size crosses each boundary (`b - a`, `b`, `b + a`
and their neighbours) for every bin edge and both run boundaries (47 boundaries instead of 7):
0.7 s alone, 2.7 s for the whole file, with the exhaustive scan over all 47 boundaries behind
`WASMALLOC_EXHAUSTIVE=1` (107 s).
