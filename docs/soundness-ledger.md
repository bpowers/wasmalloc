# Soundness ledger

Last updated: 2026-09-01

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
  functions the invariants name, `reserved` fits `u16` (Kani `every_block_of_every_bin...`),
  and `free_is_zero == zeroed` is exactly invariant 5 for a page with no linked blocks.
- Machine checks: Kani `four_operations_on_a_page_of_eight_kib_blocks_preserve_invariants`,
  `two_operations_on_a_page_of_four_kib_blocks_preserve_invariants` (concrete bins, proof-only
  backend), `every_block_of_every_bin_lies_inside_its_page_and_is_aligned` (geometry for every
  bin). Tests `init_writes_every_field_on_zeroed_and_dirty_pages`, `kind_bytes_round_trip`.
  Miri (Stacked Borrows with strict provenance, Tree Borrows) over the page tests.
- Reviewer: pending adversarial review. Date: 2026-09-01.

### PAGE-02: `page::pop`, the free-list pop

- Preconditions: `page` was returned by `init`; invariants 1 to 5 hold.
- Invariants relied on: 4 (a non-zero `free` is a block of this page with index below
  `capacity`), 2 (block `i` starts at `page + block_start + i * block_size`, inside the page,
  and `block_start` and `block_size` are multiples of `WORD`), 1 (`mem.ptr` valid in the page),
  4 again for `used < capacity <= u16::MAX` whenever the list is non-empty.
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
- Reviewer: pending adversarial review. Date: 2026-09-01.

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
- Reviewer: pending adversarial review. Date: 2026-09-01.

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
  <= reserved <= u16::MAX`.
- Machine checks: the two Kani operation-sequence harnesses (the 15-block page links two blocks
  per call, so the loop body runs); tests `extend_links_at_most_max_extend_size_and_at_least_one_block`
  (every bin), `pop_with_lazy_extension_hands_out_every_block_exactly_once` (extension step
  sizes for every bin), `free_is_zero_holds_until_the_first_push` (only link words written).
  Miri as above.
- Reviewer: pending adversarial review. Date: 2026-09-01.

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
- Reviewer: pending adversarial review. Date: 2026-09-01.

### PAGE-06: `page::validate` (test and proof infrastructure only)

- Compiled only under `cfg(test)` and `cfg(kani)`; never part of the allocator.
- Preconditions: `page` was returned by `init` inside memory owned through `mem`.
- Proof sketch: the header is read as a whole (`page.read()`, no reference); the list walk reads
  a block's first word only after `block_index` has confirmed the address is a block boundary
  below `capacity`, so a corrupt link yields `Err` rather than an out-of-page read, and the walk
  is bounded by `capacity - used` so a cycle terminates.
- Machine checks: `validate_rejects_corrupt_pages` drives every error path; it is the invariant
  oracle for the Kani operation-sequence harnesses and the randomised test. Miri as above.
- Reviewer: pending adversarial review. Date: 2026-09-01.

Not listed: the unsafe blocks inside `#[cfg(test)]` test code and the proof-only `PageModel`
backend under `#[cfg(kani)]`, which never ship.
