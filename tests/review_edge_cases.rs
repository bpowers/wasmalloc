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
    self, Class, MAX_BINNED_OBJ_SIZE, MAX_NATURAL_ALIGN, MEDIUM_MAX_OBJ_SIZE, SLICE_SIZE,
    SMALL_MAX_OBJ_SIZE, WORD,
};
use wasmalloc::slices::GrowPolicy;
use wasmalloc::testing::model::RawAlloc;
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

/// Every size within a word of each class boundary, with every alignment up to the natural
/// cap: the block is aligned, holds its bytes, survives a realloc by one byte in each
/// direction (which may cross the boundary and change the page kind) and frees cleanly. This
/// is the `dealloc` fast-path condition against `classify` on real memory.
#[test]
fn class_boundaries_with_every_natural_alignment() {
    let mut h = heap(4096, 4, 100);
    let boundaries = [
        WORD,
        8 * WORD,
        1024,
        SMALL_MAX_OBJ_SIZE,
        MEDIUM_MAX_OBJ_SIZE,
        MAX_BINNED_OBJ_SIZE,
        SLICE_SIZE,
    ];
    let mut shift = 0;
    while (1usize << shift) <= MAX_NATURAL_ALIGN {
        let align = 1usize << shift;
        for &b in &boundaries {
            for size in (b.saturating_sub(align + 1)).max(1)..=b + align + 1 {
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
        // A second pass frees with the shrunk Layout right after each shrink.
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
                    check(q, target, 7);
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
        let mut mediums = Vec::new();
        while let Some(p) = h.alloc(layout(MEDIUM_MAX_OBJ_SIZE, 8)) {
            fill(p, MEDIUM_MAX_OBJ_SIZE, 2);
            mediums.push(p);
            assert!(
                mediums.len() <= 12,
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
/// three pages in use, and memory grows although a whole slice sits empty in a queue.
///
/// Ignored because it fails: `cargo test --test review_edge_cases -- --ignored`.
#[test]
#[ignore = "R-2: an emptied page beyond the collection window is never released before growth"]
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
        // Emptying P now takes the fast path every time: `used == 0 && retire_expire != 0`.
        for &b in &p_blocks[1..] {
            h.dealloc(b, l);
        }
        assert_eq!((*p).used, 0);
        assert_ne!((*p).retire_expire, 0);
        // A page of another bin needs one slice. P's slice is the only candidate, and the
        // heap promises to release retired pages before growing memory.
        let other = h.alloc(layout(100, 8)).unwrap();
        let grew = h.memory().size_slices() - end;
        eprintln!(
            "memory grew by {grew} slices with page P empty (retire_expire {})",
            (*p).retire_expire
        );
        assert_eq!(grew, 0, "P was not released before memory grew");
        h.dealloc(other, layout(100, 8));
        h.dealloc(s_block, l);
        for &b in q_blocks[1..].iter().chain(&r_blocks[1..]) {
            h.dealloc(b, l);
        }
    }
}
