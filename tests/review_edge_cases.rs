//! Adversarial-review probes over the heap on a simulated memory: the corners the ledger's
//! proof sketches lean on hardest. Every test drives the heap through the public `RawAlloc`
//! surface with the model tester's contract checks where a stream of operations fits, and with
//! hand-written sequences where a precise shape is needed (Layouts around the class boundaries
//! with every natural alignment, realloc shrink-then-grow chains that cross bins and page kinds,
//! runs with alignments far above a slice, a memory too small to serve, a heap base that leaves
//! no usable slice).
#![cfg(not(target_arch = "wasm32"))]
// Every unsafe block here calls the allocator under test with a Layout the test built and a
// block it just received for that Layout; the heap's own tests carry the same allowance.
#![allow(clippy::undocumented_unsafe_blocks)]

use core::alloc::Layout;
use core::ptr::NonNull;

use wasmalloc::backend::Memory;
use wasmalloc::bins::{
    self, Class, MAX_BINNED_OBJ_SIZE, MAX_NATURAL_ALIGN, MEDIUM_MAX_OBJ_SIZE, MEDIUM_PAGE_SIZE,
    PageKind, SLICE_SIZE, SMALL_MAX_OBJ_SIZE, WORD,
};
use wasmalloc::slices::GrowPolicy;
use wasmalloc::testing::model::{self, Order, Profile, RawAlloc};
use wasmalloc::testing::sim::SimHeap;

fn heap(total: usize, initial: usize, offset: usize) -> SimHeap {
    let mut h = SimHeap::new(total, initial, offset);
    h.set_grow_policy(GrowPolicy {
        min_grow: 2,
        max_grow: 64,
        ..GrowPolicy::DEFAULT
    });
    h
}

fn layout(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align).unwrap()
}

/// Fill `size` bytes with a pattern that depends on the position and `tag`, so a copy from the
/// wrong offset or another block never matches.
unsafe fn fill(p: NonNull<u8>, size: usize, tag: u8) {
    for i in 0..size {
        // SAFETY: inside the block, which the caller owns.
        unsafe { p.as_ptr().add(i).write(tag ^ (i as u8).wrapping_mul(31)) };
    }
}

unsafe fn check(p: NonNull<u8>, size: usize, tag: u8) {
    for i in 0..size {
        // SAFETY: inside the block.
        let b = unsafe { p.as_ptr().add(i).read() };
        assert_eq!(
            b,
            tag ^ (i as u8).wrapping_mul(31),
            "byte {i} of a {size}-byte block"
        );
    }
}

/// Sizes around every bin boundary and every page-kind boundary, with every alignment up to
/// the natural cap: the block is aligned, holds its bytes, survives a realloc by one byte in
/// each direction (which may cross the boundary and change the bin or the page kind) and frees
/// cleanly. This is the `dealloc` fast-path condition against `classify` on real memory, and
/// `realloc`'s direct-index shortcut at every slot edge of the direct table.
///
/// For a boundary `b` and an alignment `a` the class of a request changes exactly where the
/// size rounded up to `a` crosses `b`: at `b - a + 1`, the first size that rounds to `b`, and
/// at `b + 1`, the first that rounds past it. The probe takes `b - a`, `b` and `b + a` with
/// their neighbours, which covers both edges and the sizes on either side of them; every other
/// size within `a + 1` of `b` rounds to the same value as one of these. The exhaustive scan of
/// that whole range, which took 37 s here and up to 145 s on the CI runner, is kept behind
/// `WASMALLOC_EXHAUSTIVE=1`.
#[test]
fn class_boundaries_with_every_natural_alignment() {
    let exhaustive = std::env::var_os("WASMALLOC_EXHAUSTIVE").is_some();
    let mut h = heap(4096, 4, 100);
    // Every bin edge (the eight-byte classes, the direct-table limit, the small and the medium
    // limit among them), then the run boundaries where `huge_slices` changes.
    let mut boundaries: Vec<usize> = (1..=bins::MAX_BINNED_BIN).map(bins::bin_size).collect();
    boundaries.extend([SLICE_SIZE, 2 * SLICE_SIZE]);
    for &b in [
        WORD,
        8 * WORD,
        1024,
        SMALL_MAX_OBJ_SIZE,
        MEDIUM_MAX_OBJ_SIZE,
        MAX_BINNED_OBJ_SIZE,
    ]
    .iter()
    {
        assert!(boundaries.contains(&b), "{b} is a bin edge");
    }
    let mut shift = 0;
    while (1usize << shift) <= MAX_NATURAL_ALIGN {
        let align = 1usize << shift;
        for &b in &boundaries {
            let sizes: Vec<usize> = if exhaustive {
                ((b.saturating_sub(align + 1)).max(1)..=b + align + 1).collect()
            } else {
                let mut v: Vec<usize> = [b.saturating_sub(align), b, b + align]
                    .into_iter()
                    .flat_map(|c| [c.saturating_sub(1), c, c + 1])
                    .filter(|&s| s >= 1)
                    .collect();
                v.sort_unstable();
                v.dedup();
                v
            };
            for size in sizes {
                let l = layout(size, align);
                unsafe {
                    let p = h.alloc(l).expect("memory");
                    assert_eq!(p.addr().get() % align, 0, "size {size} align {align}");
                    fill(p, size, 0x5A);
                    let q = h.realloc(p, l, size + 1).expect("memory");
                    assert_eq!(q.addr().get() % align, 0);
                    check(q, size, 0x5A);
                    fill(q, size + 1, 0x3C);
                    let r = h
                        .realloc(q, layout(size + 1, align), size.max(2) - 1)
                        .expect("memory");
                    assert_eq!(r.addr().get() % align, 0);
                    check(r, size.max(2) - 1, 0x3C);
                    h.dealloc(r, layout(size.max(2) - 1, align));
                }
            }
        }
        shift += 1;
    }
}

/// Shrink in place, free with the shrunk Layout, and grow again from the shrunk Layout, at
/// every point where the in-place decision flips: the block keeps its bytes, and the Layout the
/// caller holds is always enough to find the page again.
#[test]
fn realloc_shrink_then_grow_chains_keep_contents() {
    let mut h = heap(4096, 4, 0);
    for align in [1usize, 16, 4096] {
        // Start at the largest medium block and walk down through every bin below it, then
        // back up, deallocating with the Layout realloc handed back each time.
        let mut size = MAX_BINNED_OBJ_SIZE;
        let mut l = layout(size, align);
        unsafe {
            let mut p = h.alloc(l).unwrap();
            let mut step = 0u8;
            fill(p, size, step);
            // Below the alignment (or a word) every size rounds to the same block and the
            // three targets no longer descend, so stop there.
            while size > align.max(WORD) {
                // Just above half, exactly half, and just below half of the current block:
                // the in-place bound, the boundary, and a forced move.
                let block = match bins::classify(l) {
                    Class::Bin(b) => bins::bin_size(b),
                    Class::Huge => size.div_ceil(SLICE_SIZE) * SLICE_SIZE,
                };
                for target in [block / 2 + 1, block / 2, block / 2 - 1] {
                    let target = target.clamp(1, size);
                    let q = h.realloc(p, l, target).unwrap();
                    assert_eq!(q.addr().get() % align, 0);
                    check(q, target, step);
                    step = step.wrapping_add(1);
                    fill(q, target, step);
                    l = layout(target, align);
                    size = target;
                    p = q;
                }
            }
            // Back up in doublings to well past the medium limit.
            while size < 3 * SLICE_SIZE {
                let target = size * 2;
                let q = h.realloc(p, l, target).unwrap();
                assert_eq!(q.addr().get() % align, 0);
                check(q, size, step);
                step = step.wrapping_add(1);
                fill(q, target, step);
                l = layout(target, align);
                size = target;
                p = q;
            }
            h.dealloc(p, l);
        }
        // A second pass frees with the resized Layout right after each realloc. A target above
        // `from` is a growth inside the same block (1000 bytes at alignment 4096 occupy a 4 KiB
        // block), and only the bytes the test wrote are defined afterwards.
        for from in [SMALL_MAX_OBJ_SIZE, MEDIUM_MAX_OBJ_SIZE, 1000, 96] {
            let l = layout(from, align);
            let block = match bins::classify(l) {
                Class::Bin(b) => bins::bin_size(b),
                Class::Huge => unreachable!(),
            };
            for target in [block / 2 + 1, block / 2, block / 2 - 1, 1] {
                unsafe {
                    let p = h.alloc(l).unwrap();
                    fill(p, from, 7);
                    let q = h.realloc(p, l, target).unwrap();
                    check(q, target.min(from), 7);
                    h.dealloc(q, layout(target, align));
                }
            }
        }
    }
}

/// Runs whose alignment exceeds a slice, up to the largest alignment a non-empty Layout
/// allows on wasm32 (2^30): the run is aligned, holds its bytes, shrinks and grows in place or
/// moves without losing them, and frees cleanly. The region is 4 GiB of lazily committed
/// address space, so an alignment of 1 GiB costs nothing until it is touched.
#[test]
fn runs_with_alignments_far_above_a_slice() {
    let mut h: SimHeap = SimHeap::new(65536, 4, 100);
    for shift in 16..=30 {
        let align = 1usize << shift;
        for size in [1usize, SLICE_SIZE + 1, 3 * SLICE_SIZE] {
            let l = layout(size, align);
            unsafe {
                let p = h.alloc(l).expect("a 4 GiB region fits any one aligned run");
                assert_eq!(p.addr().get() % align, 0, "size {size} align {align}");
                fill(p, size.min(1 << 20), 0x11);
                let bigger = size + SLICE_SIZE;
                let q = h.realloc(p, l, bigger).expect("memory");
                assert_eq!(q.addr().get() % align, 0);
                check(q, size.min(1 << 20), 0x11);
                let r = h.realloc(q, layout(bigger, align), 1).expect("memory");
                assert_eq!(r.addr().get() % align, 0);
                check(r, 1, 0x11);
                h.dealloc(r, layout(1, align));
            }
        }
    }
    // The heap gave the slices back: a plain allocation still works and memory is reused.
    let z = unsafe { h.alloc_zeroed(layout(100, 8)) }.unwrap();
    unsafe { h.dealloc(z, layout(100, 8)) };
}

/// A memory too small for the request: every refusal leaves the heap usable, the blocks that
/// exist keep their bytes, retired pages are released before memory would grow, and a heap
/// base in the last slice yields a heap that refuses everything without faulting.
#[test]
fn exhausting_a_tiny_memory_is_clean() {
    // Eight slices in total, two present, heap base mid-slice: six more slices can be grown.
    let mut h = heap(8, 2, 4096);
    unsafe {
        let a = h.alloc(layout(100, 8)).unwrap();
        fill(a, 100, 1);
        // A medium page needs four aligned slices; the region may or may not have them.
        let per_page = bins::blocks_per_page(PageKind::Medium, MEDIUM_MAX_OBJ_SIZE);
        let most = 8 * SLICE_SIZE / MEDIUM_PAGE_SIZE * per_page;
        let mut mediums = Vec::new();
        while let Some(p) = h.alloc(layout(MEDIUM_MAX_OBJ_SIZE, 8)) {
            fill(p, MEDIUM_MAX_OBJ_SIZE, 2);
            mediums.push(p);
            assert!(
                mediums.len() <= most,
                "more medium blocks than eight slices can hold"
            );
        }
        // Everything else that could possibly fit is refused now or soon.
        let mut runs = Vec::new();
        while let Some(p) = h.alloc(layout(SLICE_SIZE + 1, 8)) {
            fill(p, SLICE_SIZE + 1, 3);
            runs.push(p);
            assert!(runs.len() <= 4);
        }
        assert!(h.alloc(layout(2 * SLICE_SIZE + 1, 8)).is_none());
        assert!(h.alloc_zeroed(layout(9 * SLICE_SIZE, 8)).is_none());
        assert!(h.alloc(layout(1, 1 << 20)).is_none());
        // Small blocks keep working from the page that holds `a` until it is full.
        let mut smalls = vec![a];
        while smalls.len() < 200 {
            match h.alloc(layout(100, 8)) {
                Some(p) => {
                    fill(p, 100, 1);
                    smalls.push(p);
                }
                None => break,
            }
        }
        check(a, 100, 1);
        for &p in &mediums {
            check(p, MEDIUM_MAX_OBJ_SIZE, 2);
        }
        for &p in &runs {
            check(p, SLICE_SIZE + 1, 3);
        }
        // Realloc that cannot be served leaves the block untouched.
        if let Some(&p) = runs.first() {
            assert!(
                h.realloc(p, layout(SLICE_SIZE + 1, 8), 20 * SLICE_SIZE)
                    .is_none()
            );
            check(p, SLICE_SIZE + 1, 3);
        }
        for p in runs {
            h.dealloc(p, layout(SLICE_SIZE + 1, 8));
        }
        for p in mediums {
            h.dealloc(p, layout(MEDIUM_MAX_OBJ_SIZE, 8));
        }
        for p in smalls {
            check(p, 100, 1);
            h.dealloc(p, layout(100, 8));
        }
        // With everything free (pages retired, not yet released) a run the size of the whole
        // usable memory must come back after the retired pages are released.
        let whole = h.free_slices();
        let total = h.memory().size_slices() - (h.memory().heap_base() / SLICE_SIZE + 1);
        assert!(whole <= total);
        let big = h.alloc(layout(total * SLICE_SIZE, 8));
        assert!(
            big.is_some(),
            "retired pages must be released before a refusal"
        );
        h.dealloc(big.unwrap(), layout(total * SLICE_SIZE, 8));
    }

    // Heap base inside the last slice of a memory that cannot grow: nothing is ever usable.
    let mut h: SimHeap = SimHeap::new(2, 2, SLICE_SIZE + 8);
    unsafe {
        assert!(h.alloc(layout(1, 1)).is_none());
        assert!(h.alloc_zeroed(layout(1, 1)).is_none());
        assert!(h.alloc(layout(SLICE_SIZE, 1)).is_none());
        assert!(h.alloc(layout(1, 1 << 20)).is_none());
    }
    assert_eq!(h.free_slices(), 0);
}

/// Interleaving fast-path frees with retirement and collection: a page that oscillates around
/// empty through the direct table (which never resets `retire_expire`) is still released, and
/// its slice reused, once the bin needs a fresh page elsewhere.
#[test]
fn a_page_reused_through_the_direct_table_is_still_released() {
    let mut h = heap(64, 4, 0);
    let l = layout(16, 8);
    unsafe {
        // Retire the page, then pop from it through the fast path (retire_expire stays set),
        // then free again (no transition because the page is already retired).
        let a = h.alloc(l).unwrap();
        h.dealloc(a, l);
        for _ in 0..40 {
            let b = h.alloc(l).unwrap();
            assert_eq!(a, b, "LIFO reuse of the retired page's block");
            h.dealloc(b, l);
        }
        let free_before = h.free_slices();
        // Take every slice the map has for other bins; the retired page must be released
        // before memory grows.
        let end = h.memory().size_slices();
        let mut others = Vec::new();
        let mut size = 24;
        while h.memory().size_slices() == end {
            let p = h.alloc(layout(size, 8)).unwrap();
            others.push((p, layout(size, 8)));
            size += 8;
            assert!(others.len() < 200);
        }
        assert!(h.free_slices() <= free_before);
        for (p, l) in others {
            h.dealloc(p, l);
        }
    }
}

/// Review finding R-2 (footprint, not memory safety). The heap documents that every retired
/// page is released before linear memory grows, whatever its count, and `acquire_run` relies on
/// the forced collection for that. The collection only visits the first `RETIRE_MAX_PAGES`
/// members of the queues in the retired range, and the fast paths never touch `retire_expire`:
/// a page that is retired, then drained through the direct table (no `find_page`, so the
/// countdown is not reset), parked in the full queue by the next search, brought back to the
/// end of its bin queue by one free and then emptied by the rest, keeps `retire_expire != 0`
/// with `used == 0`, so `needs_transition` skips `retire`, no collection reaches it behind
/// three pages in use, and memory grew although a whole slice sat empty in a queue.
///
/// Fixed on 2026-09-02: the search clears the countdown of a page it parks, so the free that
/// empties the page after it comes back goes through `retire` (with four pages in the queue,
/// that releases it at once), and the release before growth walks every bin queue regardless of
/// the range and the window.
#[test]
fn an_emptied_page_behind_three_others_is_released_before_memory_grows() {
    use wasmalloc::bins::PageKind;
    use wasmalloc::page::{self, Page};

    // Four initial slices: exactly four small pages of one bin, then the map is empty.
    let mut h = heap(64, 4, 0);
    let l = layout(16, 8);
    let per_page = bins::blocks_per_page(PageKind::Small, 16);
    let header = |h: &SimHeap, p: NonNull<u8>| -> *mut Page {
        h.memory()
            .ptr(page::header_of(PageKind::Small, p.addr().get()))
            .cast::<Page>()
    };
    let fill_page = |h: &mut SimHeap, n: usize| -> Vec<NonNull<u8>> {
        (0..n).map(|_| unsafe { h.alloc(l).unwrap() }).collect()
    };
    unsafe {
        // Page P: fully extended, then emptied, so it is retired with every block linked.
        let p_blocks = fill_page(&mut h, per_page);
        let p = header(&h, p_blocks[0]);
        for &b in &p_blocks {
            h.dealloc(b, l);
        }
        assert_eq!((*p).used, 0);
        assert_ne!((*p).retire_expire, 0, "P is retired");
        // Drain P through the direct table: every pop is a fast-path pop from its list, so
        // `find_page` never runs and the countdown survives.
        let p_blocks = fill_page(&mut h, per_page);
        assert_eq!(header(&h, p_blocks[0]), p);
        assert_eq!((*p).used as usize, per_page);
        assert_ne!((*p).retire_expire, 0, "the fast path does not un-retire P");
        // The next request finds P full and parks it; Q, R and S follow, each filled so that
        // the one after it is created, S keeping one block.
        let q_blocks = fill_page(&mut h, per_page);
        let q = header(&h, q_blocks[0]);
        assert_ne!(q, p);
        let r_blocks = fill_page(&mut h, per_page);
        let r = header(&h, r_blocks[0]);
        let s_block = h.alloc(l).unwrap();
        let s = header(&h, s_block);
        assert!(s != p && s != q && s != r && r != q);
        assert_eq!(h.free_slices(), 0, "four pages, four slices");
        let end = h.memory().size_slices();
        // One free each brings Q and R back behind S, then P behind them.
        h.dealloc(q_blocks[0], l);
        h.dealloc(r_blocks[0], l);
        h.dealloc(p_blocks[0], l);
        // Before the fix, emptying P took the fast path every time (`used == 0 &&
        // retire_expire != 0`) and left an empty page nobody would release. Now the park
        // cleared the countdown, so the last free retires P, and with four pages in the queue
        // `retire` releases it at once.
        for &b in &p_blocks[1..] {
            h.dealloc(b, l);
        }
        assert_eq!(h.free_slices(), 1, "P's slice is back in the map");
        // A page of another bin needs one slice: P's, without growing memory.
        let other = h.alloc(layout(100, 8)).unwrap();
        let grew = h.memory().size_slices() - end;
        assert_eq!(grew, 0, "P was not released before memory grew");
        assert_eq!(h.free_slices(), 0);
        h.dealloc(other, layout(100, 8));
        h.dealloc(s_block, l);
        for &b in q_blocks[1..].iter().chain(&r_blocks[1..]) {
            h.dealloc(b, l);
        }
    }
}

/// The direct-index shortcut of `realloc` decides on the two sizes alone, so it also fires for
/// a Layout that an in-place shrink left on a block of a larger bin. The block is at least the
/// bin of the Layout the caller holds, so it holds the new size, and the Layout handed back
/// still masks to the block's page. Walked one byte at a time across every direct slot edge
/// between the shrunk size and half the block, where the in-place decision flips and the block
/// moves, with the contents checked at every step; then the same on a medium page, where the
/// shrink may not leave the page kind.
#[test]
fn realloc_shortcut_after_an_in_place_shrink_keeps_contents() {
    let mut h = heap(64, 4, 0);
    for align in [1usize, 8] {
        let big = layout(1024, align);
        unsafe {
            let p = h.alloc(big).unwrap();
            fill(p, 1024, 1);
            // Above half the block: stays in place, in a page of bin 24 with a Layout of bin 22.
            let mut cur = h.realloc(p, big, 600).unwrap();
            assert_eq!(
                cur, p,
                "a shrink to more than half the block stays in place"
            );
            check(cur, 600, 1);
            let mut size = 600;
            let mut tag = 1u8;
            while size > 512 {
                let target = size - 1;
                let same_slot = bins::direct_index(size) == bins::direct_index(target);
                let q = h.realloc(cur, layout(size, align), target).unwrap();
                assert_eq!(
                    q, p,
                    "size {size} to {target} (same direct slot: {same_slot}) keeps the block"
                );
                check(q, target, tag);
                tag = tag.wrapping_add(1);
                fill(q, target, tag);
                // A free with the Layout the caller holds now finds the page: exercised by the
                // realloc's own mask on the next step, and directly at the end.
                cur = q;
                size = target;
            }
            assert_eq!(size, 512);
            // The half bound of the in-place decision is taken from the Layout the caller
            // holds, not from the block: from (512, bin 20) every step below stays in place as
            // long as the target is at least half of the current bin, so the chain walks the
            // 1 KiB block down through every bin of its kind to an 8-byte Layout (review
            // finding R2-3, a footprint remark; the block is always large enough).
            for target in [511usize, 384, 256, 192, 128, 96, 64, 48, 32, 24, 16, 8] {
                let q = h.realloc(cur, layout(size, align), target).unwrap();
                assert_eq!(q, p, "size {size} to {target} stays in the 1 KiB block");
                check(q, target, tag);
                tag = tag.wrapping_add(1);
                fill(q, target, tag);
                cur = q;
                size = target;
            }
            // The Layout the caller holds now names bin 1; the free finds the bin-24 page.
            h.dealloc(cur, layout(8, align));
            // From the original Layout a target below half of its bin moves the block.
            let p = h.alloc(big).unwrap();
            fill(p, 1024, 8);
            let moved = h.realloc(p, big, 511).unwrap();
            assert_ne!(moved, p, "a shrink below half of the Layout's bin moves it");
            check(moved, 511, 8);
            h.dealloc(moved, layout(511, align));
        }
    }
    // Medium pages: a shrink stays in place only above half the Layout's bin and within the
    // kind.
    let l = layout(MEDIUM_MAX_OBJ_SIZE, 8);
    unsafe {
        let p = h.alloc(l).unwrap();
        fill(p, MEDIUM_MAX_OBJ_SIZE, 4);
        let half = MEDIUM_MAX_OBJ_SIZE / 2;
        let q = h.realloc(p, l, half).unwrap();
        assert_eq!(q, p, "exactly half stays in place");
        check(q, half, 4);
        // The half bound is taken from the Layout the caller holds (bin 41, 20 KiB), not from
        // the 40 KiB block it sits in, so one byte less than half of that block still stays.
        let r = h.realloc(q, layout(half, 8), half - 1).unwrap();
        assert_eq!(r, p, "above half of the Layout's own bin stays in place");
        check(r, half - 1, 4);
        // From the original Layout the same target is below half of its bin and moves.
        let p2 = h.alloc(l).unwrap();
        fill(p2, MEDIUM_MAX_OBJ_SIZE, 6);
        let r2 = h.realloc(p2, l, half - 1).unwrap();
        assert_ne!(r2, p2, "below half of the Layout's bin moves");
        check(r2, half - 1, 6);
        h.dealloc(r2, layout(half - 1, 8));
        // Down to a small-page bin from a medium block: the kind changes, so the block moves
        // even though the size is above half of the medium block it sits in.
        let m = h.alloc(layout(12 * 1024, 8)).unwrap();
        fill(m, 12 * 1024, 5);
        let n = h
            .realloc(m, layout(12 * 1024, 8), SMALL_MAX_OBJ_SIZE)
            .unwrap();
        assert_ne!(n, m, "a medium block never serves a small-page Layout");
        check(n, SMALL_MAX_OBJ_SIZE, 5);
        h.dealloc(n, layout(SMALL_MAX_OBJ_SIZE, 8));
        h.dealloc(r, layout(half - 1, 8));
    }
}

/// A retired page of a bin above the direct table keeps its countdown while the queue-head
/// path of `alloc_generic` pops from it, exactly as a page drained through the direct table
/// does (nothing on either path touches `retire_expire`). The page still serves requests, and
/// the release before memory growth frees it: with four initial slices, three pages of other
/// bins fill the map, the fourth is served from the released page's slice, and only the fifth
/// grows memory.
#[test]
fn a_retired_page_at_the_queue_head_is_reused_and_still_released() {
    let mut h = heap(64, 4, 0);
    // Bin 28: 2 KiB blocks, above DIRECT_MAX_SIZE, in a small page.
    let l = layout(2000, 8);
    assert!(l.size() > bins::DIRECT_MAX_SIZE);
    unsafe {
        let a = h.alloc(l).unwrap();
        h.dealloc(a, l);
        for round in 0..40u8 {
            let b = h.alloc(l).unwrap();
            assert_eq!(a, b, "LIFO reuse of the retired page's block");
            fill(b, 2000, round);
            check(b, 2000, round);
            h.dealloc(b, l);
        }
        assert_eq!(h.free_slices(), 3, "the page holds one of the four slices");
        let end = h.memory().size_slices();
        let mut others = Vec::new();
        for size in [24usize, 32, 40, 48] {
            let p = h.alloc(layout(size, 8)).unwrap();
            fill(p, size, size as u8);
            others.push((p, layout(size, 8)));
            assert_eq!(
                h.memory().size_slices(),
                end,
                "the {size}-byte page came from the map, the fourth from the released page"
            );
        }
        assert_eq!(h.free_slices(), 0);
        let fifth = h.alloc(layout(56, 8)).unwrap();
        assert!(
            h.memory().size_slices() > end,
            "only the fifth page grows memory"
        );
        h.dealloc(fifth, layout(56, 8));
        for (p, l) in others {
            check(p, l.size(), l.size() as u8);
            h.dealloc(p, l);
        }
    }
}

/// Review finding R2-1 (footprint, not memory safety). The heap documents that every empty
/// page is released before linear memory is grown, whatever its countdown. `acquire_run` did
/// so, but the in-place growth of a run at the top of the heap took another road,
/// `slices::extend_with_growth`, which grew memory without that release: the retired page in
/// the first slice survived the `memory.grow` that extended the run in the slices after it.
/// Since 2026-09-02 `Heap::realloc` runs the release walk whenever the free slices after the
/// run cannot serve the growth, before memory is grown. The growth step for a heap this small
/// is two slices, so the map holds two free slices afterwards, the released page's and the
/// step's spare one; before the fix it held one.
#[test]
fn in_place_run_growth_releases_every_empty_page_before_memory_grows() {
    let mut h = heap(64, 4, 0);
    let small = layout(16, 8);
    let run = layout(3 * SLICE_SIZE, 8);
    unsafe {
        // Slice 0: a page that is emptied and retired. Slices 1 to 3: a run at the top.
        let a = h.alloc(small).unwrap();
        h.dealloc(a, small);
        let r = h.alloc(run).unwrap();
        fill(r, 3 * SLICE_SIZE, 0x66);
        assert_eq!(
            h.free_slices(),
            0,
            "the page and the run fill the initial memory"
        );
        let end = h.memory().size_slices();
        let r2 = h.realloc(r, run, 4 * SLICE_SIZE).unwrap();
        assert_eq!(r2, r, "the run grows in place through memory.grow");
        check(r2, 3 * SLICE_SIZE, 0x66);
        assert_eq!(
            h.memory().size_slices(),
            end + 2,
            "memory grew by the step of two slices"
        );
        assert_eq!(
            h.free_slices(),
            2,
            "the empty page was released before memory grew: its slice and the step's spare one"
        );
        h.dealloc(r2, layout(4 * SLICE_SIZE, 8));
    }
}

/// The model tester over a profile aimed at the paths this review examined: sizes from 1 to
/// 100 KiB, so the direct-table edge, the queue-head range (1 KiB to 40 KiB) and both page
/// kinds are all hit; alignments 16 to 4096 as often as word alignment, so the fast-path
/// kind test sees rounded sizes; a fifth of the operations reallocs, for the direct-index
/// shortcut and the in-place decisions; sweeps so pages empty, retire and come back.
#[test]
fn model_profile_aimed_at_the_reviewed_paths() {
    let profile = Profile {
        name: "review2",
        ops: [35, 10, 35, 20],
        sizes: [700, 300, 0, 0],
        aligns: [40, 15, 10, 10, 25, 0, 0],
        sweep_every: 4000,
        max_live_bytes: 32 * 1024 * 1024,
        order: Order::Random,
        batch: 0,
    };
    for (seed, (initial, offset)) in [(11u64, (4, 100)), (12, (1, 0)), (13, (64, SLICE_SIZE + 8))] {
        let mut h = heap(4096, initial, offset);
        let stats = model::check(&mut h, seed, 30_000, profile);
        assert_eq!(stats.ops, 30_000, "{stats:?}");
        assert_eq!(
            stats.deallocs,
            stats.allocs + stats.zeroed_allocs,
            "{stats:?}"
        );
        assert!(stats.reallocs > 3_000, "{stats:?}");
    }
}

/// Third review, on the walk `Heap::realloc` runs before a run grows through memory or moves
/// (R2-1's fix). The slice in the run's way is an empty medium page: the walk frees all four of
/// its slices, `try_extend` claims the two the growth needs, and the other two stay free and
/// serve the next page. The run's new tail, which now covers the old page header, is written
/// over in full while the new page's block keeps its bytes: the two are disjoint.
#[test]
fn a_run_grows_into_part_of_a_released_medium_page() {
    use wasmalloc::page;

    let mut h = heap(64, 8, 0);
    let four = layout(4 * SLICE_SIZE, 8);
    let six = layout(6 * SLICE_SIZE, 8);
    let medium = layout(MEDIUM_MAX_OBJ_SIZE, 8);
    let small = layout(16, 8);
    unsafe {
        let r = h.alloc(four).unwrap();
        let start = r.addr().get() / SLICE_SIZE;
        fill(r, four.size(), 0x41);
        // The other four initial slices become a medium page, then an empty, retired one.
        let m = h.alloc(medium).unwrap();
        assert_eq!(
            page::header_of(PageKind::Medium, m.addr().get()) / SLICE_SIZE,
            start + 4
        );
        h.dealloc(m, medium);
        assert_eq!(
            h.free_slices(),
            0,
            "the run and the page fill the initial memory"
        );
        let end = h.memory().size_slices();
        let r2 = h.realloc(r, four, six.size()).unwrap();
        assert_eq!(r2, r, "the run grew in place into the released page");
        check(r2, four.size(), 0x41);
        assert_eq!(h.memory().size_slices(), end, "no memory.grow");
        assert_eq!(h.free_slices(), 2, "the page's other two slices are free");
        fill(r2, six.size(), 0x42);
        // The next small page takes the lowest free slice, right after the grown run.
        let s = h.alloc(small).unwrap();
        assert_eq!(
            page::header_of(PageKind::Small, s.addr().get()) / SLICE_SIZE,
            start + 6
        );
        assert_eq!(h.free_slices(), 1);
        fill(s, 16, 0x43);
        check(r2, six.size(), 0x42);
        fill(r2, six.size(), 0x44);
        check(s, 16, 0x43);
        h.dealloc(s, small);
        h.dealloc(r2, six);
        assert_eq!(
            h.free_slices(),
            7,
            "the run's six slices and the spare one; the small page is retired"
        );
    }
}

/// The slice in the run's way is an empty page at the top of memory and the growth needs more
/// than that slice: the walk frees the page, and `extend_with_growth` claims its slice and
/// grows memory for the rest, all in one realloc that keeps the block where it is. Afterwards
/// the page is gone for good: the next request of its bin gets a page from fresh memory, not
/// the block the old page handed out.
#[test]
fn a_run_grows_through_a_released_page_and_then_through_memory() {
    use wasmalloc::page;

    let mut h = heap(64, 4, 0);
    let three = layout(3 * SLICE_SIZE, 8);
    let six = layout(6 * SLICE_SIZE, 8);
    let small = layout(16, 8);
    unsafe {
        let r = h.alloc(three).unwrap();
        let start = r.addr().get() / SLICE_SIZE;
        fill(r, three.size(), 0x51);
        let a = h.alloc(small).unwrap();
        assert_eq!(
            page::header_of(PageKind::Small, a.addr().get()) / SLICE_SIZE,
            start + 3
        );
        h.dealloc(a, small);
        assert_eq!(h.free_slices(), 0);
        let end = h.memory().size_slices();
        let r2 = h.realloc(r, three, six.size()).unwrap();
        assert_eq!(r2, r, "the run grew in place");
        check(r2, three.size(), 0x51);
        assert_eq!(
            h.memory().size_slices(),
            end + 2,
            "the released slice plus the two missing ones, which is also the step"
        );
        assert_eq!(h.free_slices(), 0);
        fill(r2, six.size(), 0x52);
        let b = h.alloc(small).unwrap();
        assert_eq!(
            page::header_of(PageKind::Small, b.addr().get()) / SLICE_SIZE,
            start + 6,
            "the bin's page comes from fresh memory: the old one is part of the run"
        );
        assert_eq!(h.memory().size_slices(), end + 4);
        check(r2, six.size(), 0x52);
        h.dealloc(b, small);
        h.dealloc(r2, six);
    }
}

/// The walk runs only when the map alone cannot serve the growth: a run that grows into free
/// slices keeps every retired page, so a size class that oscillates around empty keeps its page
/// across the in-place growth of a buffer, which is what retirement is for.
#[test]
fn a_growth_served_by_the_map_keeps_retired_pages() {
    use wasmalloc::page::{self, Page};

    let mut h = heap(64, 8, 0);
    let three = layout(3 * SLICE_SIZE, 8);
    let five = layout(5 * SLICE_SIZE, 8);
    let small = layout(16, 8);
    unsafe {
        let a = h.alloc(small).unwrap();
        let page = h
            .memory()
            .ptr(page::header_of(PageKind::Small, a.addr().get()))
            .cast::<Page>();
        h.dealloc(a, small);
        assert_eq!((*page).used, 0);
        assert_ne!((*page).retire_expire, 0, "retired, not freed");
        let r = h.alloc(three).unwrap();
        assert_eq!(r.addr().get() / SLICE_SIZE, page as usize / SLICE_SIZE + 1);
        fill(r, three.size(), 0x61);
        assert_eq!(h.free_slices(), 4);
        let end = h.memory().size_slices();
        let r2 = h.realloc(r, three, five.size()).unwrap();
        assert_eq!(r2, r);
        check(r2, three.size(), 0x61);
        assert_eq!(h.memory().size_slices(), end);
        assert_eq!(h.free_slices(), 2);
        assert_eq!((*page).used, 0);
        assert_ne!(
            (*page).retire_expire,
            0,
            "the retired page survived a growth the map served"
        );
        let b = h.alloc(small).unwrap();
        assert_eq!(b, a, "the retired page still serves its bin");
        h.dealloc(b, small);
        h.dealloc(r2, five);
    }
}

/// The slice in the run's way is a page in use, so the run has to move. The walk runs anyway
/// and frees an empty page above the blocker, which joins the free tail, and the moved run
/// lands at the bottom of that longer tail without a `memory.grow`. The run's bytes and the
/// live page's block both survive, and the run's old slices are free afterwards.
#[test]
fn a_blocked_run_moves_into_the_tail_a_released_page_extends() {
    use wasmalloc::page;

    let mut h = heap(64, 8, 0);
    let two = layout(2 * SLICE_SIZE, 8);
    let five = layout(5 * SLICE_SIZE, 8);
    let live = layout(24, 8);
    let small = layout(16, 8);
    unsafe {
        let r = h.alloc(two).unwrap();
        let start = r.addr().get() / SLICE_SIZE;
        fill(r, two.size(), 0x71);
        let b = h.alloc(live).unwrap();
        assert_eq!(
            page::header_of(PageKind::Small, b.addr().get()) / SLICE_SIZE,
            start + 2
        );
        fill(b, 24, 0x72);
        let a = h.alloc(small).unwrap();
        assert_eq!(
            page::header_of(PageKind::Small, a.addr().get()) / SLICE_SIZE,
            start + 3
        );
        h.dealloc(a, small);
        assert_eq!(h.free_slices(), 4, "the tail above the retired page");
        let end = h.memory().size_slices();
        // Five slices: the tail alone is one short, the tail plus the released slice fits.
        let r2 = h.realloc(r, two, five.size()).unwrap();
        assert_ne!(r2, r, "a page in use blocks the growth");
        assert_eq!(
            r2.addr().get() / SLICE_SIZE,
            start + 3,
            "the run moved into the released page's slice and the tail"
        );
        check(r2, two.size(), 0x71);
        check(b, 24, 0x72);
        assert_eq!(h.memory().size_slices(), end, "no memory.grow");
        assert_eq!(h.free_slices(), 2, "the old run's slices");
        fill(r2, five.size(), 0x73);
        check(b, 24, 0x72);
        h.dealloc(b, live);
        h.dealloc(r2, five);
    }
}
