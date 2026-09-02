//! The heap: bin queues, the direct table, page lifecycle, and the allocation algorithms.
//!
//! This is mimalloc v3's `theap` (page.c, page-queue.c, alloc.c, free.c) reduced to one thread
//! and one arena, with three changes that Rust's `GlobalAlloc` contract makes possible:
//!
//! - the page kind and the page header address are derived from the `Layout`, so `dealloc`
//!   masks the pointer instead of consulting a page map (see [`page::header_of`]);
//! - blocks above [`bins::LARGE_MAX_OBJ_SIZE`], and blocks with alignment above
//!   [`bins::MAX_NATURAL_ALIGN`], are header-less runs of slices whose length is recomputed from
//!   the Layout on free;
//! - freed blocks go straight onto the page's `free` list (mimalloc's `local_free` exists for
//!   its deferred-free heartbeat and its cross-thread lists, neither of which we have).
//!
//! # Structure
//!
//! `queues[bin]` is a doubly linked list of the pages serving `bin`, threaded through the
//! `next`/`prev` fields of the page headers, with the page most likely to have free blocks at the
//! front. `queues[FULL_QUEUE]` holds pages with no free and no unextended blocks, so the search
//! never rescans them; a free into such a page moves it back. `direct[direct_index(size)]` points
//! at the first page of the queue for every size up to [`bins::DIRECT_MAX_SIZE`], or at a
//! read-only sentinel page whose free list is empty, so the allocation fast path is one table
//! load, one `free` load, a compare, and a pop, with no bin arithmetic and no initialisation
//! check (mimalloc's `pages_free_direct` and `_mi_theap_empty`).
//!
//! # Fast paths
//!
//! [`Heap::alloc`] and [`Heap::dealloc`] are the only `#[inline]` entry points; everything they
//! call on a miss is `#[cold] #[inline(never)]` so the inlined code stays small enough for V8's
//! baseline tier and for consumers building at `opt-level = "z"`.
//!
//! # Invariants (checked by `validate` in tests)
//!
//! 1. Every page in `queues[b]` for `b <= MAX_BIN` has `bin == b` and is not flagged full;
//!    every page in the full queue is flagged full and has neither free nor unextended blocks.
//! 2. Every page's `page::validate` holds, and its address is a multiple of its kind's page size.
//! 3. `direct[i]` is the first page of the queue for `bin(i * WORD)`, or the sentinel when that
//!    queue is empty.
//! 4. A page's slices are marked allocated in the slice map; freed pages' slices are free.

use core::alloc::Layout;
use core::ptr::{self, NonNull};

use crate::backend::Memory;
use crate::bins::{
    self, BIN_COUNT, Class, DIRECT_ENTRIES, DIRECT_MAX_SIZE, MAX_BIN, MAX_NATURAL_ALIGN, PageKind,
    SLICE_SIZE, SMALL_MAX_OBJ_SIZE, WORD,
};
use crate::page::{self, Page};
use crate::slices::{self, GrowPolicy, SliceMap};

/// Queue index for pages that are full.
const FULL_QUEUE: usize = BIN_COUNT;
const QUEUE_COUNT: usize = BIN_COUNT + 1;

/// Retirement: how many collection rounds an empty page survives before its slices are freed,
/// so a page that oscillates between empty and one block is not freed and re-acquired
/// (mimalloc's `MI_RETIRE_CYCLES`; medium and large pages use a quarter of it).
const RETIRE_CYCLES: u8 = 16;
/// Only retire when the queue holds at most this many pages (`MI_RETIRE_MAX_PAGES`).
const RETIRE_MAX_PAGES: usize = 3;
/// Candidate pages inspected after the first usable one when searching a queue
/// (`mi_option_page_max_candidates`).
const MAX_CANDIDATES: usize = 4;
/// Retired pages are collected every this many slow-path allocations.
const GENERIC_COLLECT_PERIOD: u32 = 1000;

/// The page every direct-table entry points at before its queue has a page. Its free list is
/// empty, so [`page::pop`] returns `None` without writing, and the slow path takes over. It is
/// never linked into a queue and never written.
static EMPTY_PAGE: Page = Page {
    free: 0,
    used: 0,
    capacity: 0,
    reserved: 0,
    block_start: 0,
    block_size: 0,
    free_is_zero: false,
    bin: 0,
    kind: 0,
    flags: 0,
    retire_expire: 0,
    next: 0,
    prev: 0,
};

#[inline(always)]
fn sentinel() -> *mut Page {
    // Only ever read through; see EMPTY_PAGE.
    (&raw const EMPTY_PAGE).cast_mut()
}

/// A doubly linked list of pages, by address (0 = none), plus its length.
#[derive(Clone, Copy, Debug)]
struct PageQueue {
    first: usize,
    last: usize,
    count: usize,
}

const EMPTY_QUEUE: PageQueue = PageQueue {
    first: 0,
    last: 0,
    count: 0,
};

/// The allocator state. One instance lives in a static for the global allocator; tests build
/// them over [`crate::backend::SimMemory`].
///
/// `WORDS` sizes the slice bitmap (see [`SliceMap`]); the default covers all of wasm32.
pub struct Heap<M: Memory, const WORDS: usize = 1024> {
    mem: M,
    slices: SliceMap<WORDS>,
    policy: GrowPolicy,
    direct: [*mut Page; DIRECT_ENTRIES],
    queues: [PageQueue; QUEUE_COUNT],
    /// Bins that may hold retired pages, inclusive; `retired_min > retired_max` means none.
    retired_min: usize,
    retired_max: usize,
    generic_count: u32,
    initialized: bool,
}

impl<M: Memory, const WORDS: usize> Heap<M, WORDS> {
    /// A heap over `mem`. Touches no memory until the first allocation.
    pub const fn new(mem: M) -> Self {
        Heap {
            mem,
            slices: SliceMap::new(),
            policy: GrowPolicy::DEFAULT,
            direct: [sentinel_const(); DIRECT_ENTRIES],
            queues: [EMPTY_QUEUE; QUEUE_COUNT],
            retired_min: QUEUE_COUNT,
            retired_max: 0,
            generic_count: 0,
            initialized: false,
        }
    }

    /// Override the growth policy (tests use small steps).
    pub fn set_grow_policy(&mut self, policy: GrowPolicy) {
        self.policy = policy;
    }

    /// The memory this heap allocates from.
    pub fn memory(&self) -> &M {
        &self.mem
    }

    /// Slices currently free in the map (not handed to any page or run).
    pub fn free_slices(&self) -> usize {
        self.slices.free_count()
    }

    // ------------------------------------------------------------------------------------
    // Fast paths
    // ------------------------------------------------------------------------------------

    /// Allocate a block for `layout`, or `None` if memory cannot be grown.
    ///
    /// # Safety
    ///
    /// `layout.size()` must be non-zero (the `GlobalAlloc` contract).
    #[inline]
    pub unsafe fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        let mut size = layout.size();
        let align = layout.align();
        if align > WORD {
            if align > MAX_NATURAL_ALIGN {
                // SAFETY: same contract as this function.
                return unsafe { self.alloc_generic(layout, false) };
            }
            // Same rounding as bins::classify: the bin of the rounded size is aligned to `align`.
            size = (size + align - 1) & !(align - 1);
        }
        if size <= DIRECT_MAX_SIZE {
            let page = self.direct[bins::direct_index(size)];
            // SAFETY: direct entries are the read-only sentinel (empty list, pop returns None
            // without writing) or headers of live pages of this heap (invariant 3), which pop
            // may modify.
            if let Some(block) = unsafe { page::pop(page, &self.mem) } {
                // SAFETY: a block address is inside a page the heap owns, hence non-null.
                return Some(unsafe { NonNull::new_unchecked(self.mem.ptr(block)) });
            }
        }
        // SAFETY: same contract as this function.
        unsafe { self.alloc_generic(layout, false) }
    }

    /// Allocate a zero-filled block for `layout`.
    ///
    /// # Safety
    ///
    /// As for [`alloc`](Self::alloc).
    #[inline]
    pub unsafe fn alloc_zeroed(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        let mut size = layout.size();
        let align = layout.align();
        if align > WORD {
            if align > MAX_NATURAL_ALIGN {
                // SAFETY: same contract as this function.
                return unsafe { self.alloc_generic(layout, true) };
            }
            size = (size + align - 1) & !(align - 1);
        }
        if size <= DIRECT_MAX_SIZE {
            let page = self.direct[bins::direct_index(size)];
            // SAFETY: as in `alloc`.
            if let Some(block) = unsafe { page::pop(page, &self.mem) } {
                let p = self.mem.ptr(block);
                // SAFETY: the block is `bin_size >= layout.size()` bytes of memory we own. When
                // the page's blocks are known zero only the free-list link word is dirty.
                unsafe {
                    if (*page).free_is_zero {
                        p.cast::<usize>().write(0);
                    } else {
                        p.write_bytes(0, layout.size());
                    }
                    return Some(NonNull::new_unchecked(p));
                }
            }
        }
        // SAFETY: same contract as this function.
        unsafe { self.alloc_generic(layout, true) }
    }

    /// Free a block.
    ///
    /// # Safety
    ///
    /// `ptr` must have been returned by this heap for exactly `layout` (or by `realloc` with a
    /// Layout of this size and alignment) and not freed since.
    #[inline]
    pub unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        let addr = ptr.addr().get();
        if layout.align() <= WORD && layout.size() <= SMALL_MAX_OBJ_SIZE {
            // Small page: classify(layout) is Bin(b) with kind Small, and bins::block_start plus
            // the small page alignment put every block strictly inside its page.
            let page = self
                .mem
                .ptr(page::header_of(PageKind::Small, addr))
                .cast::<Page>();
            // SAFETY: by the precondition `addr` is a live block of the small page at the masked
            // address, which is a page of this heap.
            unsafe {
                page::push(page, &self.mem, addr);
                if (*page).used == 0 || (*page).flags != 0 {
                    self.dealloc_transition(page);
                }
            }
            return;
        }
        // SAFETY: same contract as this function.
        unsafe { self.dealloc_generic(ptr, layout) }
    }

    /// Resize a block. Returns the (possibly moved) block, or `None` if memory cannot be grown,
    /// in which case the old block is untouched.
    ///
    /// # Safety
    ///
    /// As for [`dealloc`](Self::dealloc), and `new_size` must be non-zero and, rounded up to
    /// `layout.align()`, must not overflow `isize`.
    pub unsafe fn realloc(
        &mut self,
        ptr: NonNull<u8>,
        layout: Layout,
        new_size: usize,
    ) -> Option<NonNull<u8>> {
        // SAFETY: caller guarantees the rounded size fits; the alignment is unchanged.
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        match (bins::classify(layout), bins::classify(new_layout)) {
            // Same block if it fits. Shrinking in place is only allowed within the same page
            // kind (the next dealloc recomputes the kind from the new Layout) and, as in
            // mimalloc, only while the block stays at least half used.
            (Class::Bin(old), Class::Bin(new))
                if new == old
                    || (new < old
                        && bins::kind_of_bin(new) == bins::kind_of_bin(old)
                        && new_size >= bins::bin_size(old) / 2) =>
            {
                return Some(ptr);
            }
            (Class::Huge, Class::Huge) => {
                let start = ptr.addr().get() / SLICE_SIZE;
                let old_n = huge_slices(layout);
                let new_n = huge_slices(new_layout);
                if new_n == old_n {
                    return Some(ptr);
                }
                if new_n < old_n {
                    self.slices.shrink(start, old_n, new_n);
                    return Some(ptr);
                }
                if self
                    .slices
                    .try_extend(start, old_n, new_n - old_n)
                    .is_some()
                {
                    return Some(ptr);
                }
            }
            _ => {}
        }
        // SAFETY: new_layout has non-zero size (caller contract).
        let new = unsafe { self.alloc(new_layout)? };
        // SAFETY: both blocks are valid for `min` bytes and distinct.
        unsafe {
            ptr::copy_nonoverlapping(ptr.as_ptr(), new.as_ptr(), layout.size().min(new_size));
            self.dealloc(ptr, layout);
        }
        Some(new)
    }

    // ------------------------------------------------------------------------------------
    // Slow paths
    // ------------------------------------------------------------------------------------

    #[cold]
    #[inline(never)]
    unsafe fn alloc_generic(&mut self, layout: Layout, zero: bool) -> Option<NonNull<u8>> {
        self.ensure_init();
        match bins::classify(layout) {
            Class::Huge => self.alloc_huge(layout, zero),
            Class::Bin(bin) => {
                self.generic_count += 1;
                if self.generic_count >= GENERIC_COLLECT_PERIOD {
                    self.generic_count = 0;
                    // SAFETY: heap invariants hold between operations.
                    unsafe { self.collect_retired(false) };
                }
                // SAFETY: heap invariants hold between operations.
                let mut found = unsafe { self.find_page(bin) };
                if found.is_none() {
                    // Out of memory: release every retired page and try once more, as mimalloc
                    // does, before reporting failure.
                    // SAFETY: as above.
                    unsafe {
                        self.collect_retired(true);
                        found = self.find_page(bin);
                    }
                }
                let page = found?;
                // SAFETY: find_page returns a page of this heap with a non-empty free list.
                let block = unsafe { page::pop(page, &self.mem) };
                debug_assert!(block.is_some());
                let block = block?;
                let p = self.mem.ptr(block);
                // SAFETY: the block is `bin_size(bin) >= layout.size()` bytes we own.
                unsafe {
                    if zero {
                        if (*page).free_is_zero {
                            p.cast::<usize>().write(0);
                        } else {
                            p.write_bytes(0, layout.size());
                        }
                    }
                    Some(NonNull::new_unchecked(p))
                }
            }
        }
    }

    fn alloc_huge(&mut self, layout: Layout, zero: bool) -> Option<NonNull<u8>> {
        let count = huge_slices(layout);
        // Alignments up to a slice are satisfied by slice alignment; larger ones align the run.
        let align = layout.align().div_ceil(SLICE_SIZE).max(1);
        let mut run = slices::acquire(&mut self.slices, &mut self.mem, count, align, &self.policy);
        if run.is_none() {
            // SAFETY: heap invariants hold between operations.
            unsafe { self.collect_retired(true) };
            run = slices::acquire(&mut self.slices, &mut self.mem, count, align, &self.policy);
        }
        let run = run?;
        let addr = run.start * SLICE_SIZE;
        let p = self.mem.ptr(addr);
        if zero && !run.zeroed {
            // SAFETY: the run is `count * SLICE_SIZE >= layout.size()` bytes we own.
            unsafe { p.write_bytes(0, layout.size()) };
        }
        // SAFETY: slice addresses of owned memory are non-null.
        Some(unsafe { NonNull::new_unchecked(p) })
    }

    #[cold]
    #[inline(never)]
    unsafe fn dealloc_generic(&mut self, ptr: NonNull<u8>, layout: Layout) {
        let addr = ptr.addr().get();
        match bins::classify(layout) {
            Class::Huge => {
                debug_assert!(addr % SLICE_SIZE == 0);
                self.slices.free(addr / SLICE_SIZE, huge_slices(layout));
            }
            Class::Bin(bin) => {
                let kind = bins::kind_of_bin(bin);
                let page = self.mem.ptr(page::header_of(kind, addr)).cast::<Page>();
                // SAFETY: by the precondition `addr` is a live block of the page at the masked
                // address, a page of this heap of this kind.
                unsafe {
                    // A block shrunk in place by realloc may sit in a page of a larger bin of the
                    // same kind, so only the kind is checkable here.
                    debug_assert!(bins::kind_of_bin((*page).bin) == kind);
                    page::push(page, &self.mem, addr);
                    if (*page).used == 0 || (*page).flags != 0 {
                        self.dealloc_transition(page);
                    }
                }
            }
        }
    }

    /// A free made the page empty, or the page sits in the full queue: fix the queues.
    #[cold]
    #[inline(never)]
    unsafe fn dealloc_transition(&mut self, page: *mut Page) {
        // SAFETY: `page` is a page of this heap (caller).
        unsafe {
            if page::in_full_queue(page) {
                self.unfull(page);
            }
            if page::all_free(page) {
                self.retire(page);
            }
        }
    }

    // ------------------------------------------------------------------------------------
    // Initialisation and page supply
    // ------------------------------------------------------------------------------------

    fn ensure_init(&mut self) {
        if self.initialized {
            return;
        }
        self.initialized = true;
        let heap_base = self.mem.heap_base();
        self.slices.init(heap_base / SLICE_SIZE);
        // The linker leaves a gap between the heap base and the end of the initial memory; use
        // it before paying for a memory.grow. Its contents are not guaranteed zero.
        let (first, count) = slices::initial_free_range(heap_base, self.mem.size_slices());
        if count > 0 {
            self.slices.add_free(first, count, false);
        }
    }

    /// Find a page of `bin` with a non-empty free list, extending or allocating one if needed.
    ///
    /// # Safety
    ///
    /// Heap invariants must hold.
    unsafe fn find_page(&mut self, bin: u8) -> Option<*mut Page> {
        let qi = bin as usize;
        // mimalloc's next-fit candidate search (page.c:mi_page_queue_find_free_ex): walk the
        // queue, park full pages in the full queue, and among pages with room prefer the fuller
        // one (so emptier pages get a chance to drain and be freed), stopping at the first page
        // with an immediately available block or after MAX_CANDIDATES extra pages.
        let mut candidate: *mut Page = ptr::null_mut();
        let mut limit = MAX_CANDIDATES as isize;
        let mut cur = self.queues[qi].first;
        while cur != 0 {
            let page = self.page_at(cur);
            // SAFETY: queue members are live pages of this heap.
            let (next, available, expandable) = unsafe {
                (
                    (*page).next,
                    page::has_free(page),
                    page::is_expandable(page),
                )
            };
            if !available && !expandable {
                // SAFETY: as above.
                unsafe { self.move_to_full(qi, page) };
            } else {
                if candidate.is_null() {
                    candidate = page;
                    limit = MAX_CANDIDATES as isize;
                } else {
                    // SAFETY: both are live pages of this heap.
                    unsafe {
                        if page::all_free(candidate) {
                            self.free_page(candidate, qi);
                            candidate = page;
                        } else if (*page).used >= (*candidate).used && !mostly_used(page) {
                            candidate = page;
                        }
                    }
                }
                if available || limit <= 0 {
                    break;
                }
            }
            limit -= 1;
            cur = next;
        }

        if !candidate.is_null() {
            // SAFETY: candidate is a live page of this heap with room.
            unsafe {
                if !page::has_free(candidate) && !page::extend(candidate, &self.mem) {
                    debug_assert!(false, "expandable page failed to extend");
                    candidate = ptr::null_mut();
                }
            }
        }
        if candidate.is_null() {
            // SAFETY: invariants hold.
            unsafe { self.collect_retired(false) };
            return self.fresh_page(bin);
        }
        // SAFETY: candidate is a live page of this heap in queue `qi`.
        unsafe {
            self.move_to_front(qi, candidate);
            (*candidate).retire_expire = 0;
        }
        Some(candidate)
    }

    /// Acquire slices for a new page of `bin`, initialise it, and put it at the front of its
    /// queue with an initial free list.
    fn fresh_page(&mut self, bin: u8) -> Option<*mut Page> {
        let kind = bins::kind_of_bin(bin);
        let n = kind.page_size() / SLICE_SIZE;
        let run = slices::acquire(&mut self.slices, &mut self.mem, n, n, &self.policy)?;
        let addr = run.start * SLICE_SIZE;
        // SAFETY: the run is `n` fresh slices aligned to the page size (acquire honours `align`),
        // owned by nothing else; `kind` is the kind of `bin`.
        let page = unsafe { page::init(&self.mem, addr, kind, bin, run.zeroed) };
        // SAFETY: a freshly initialised page is not in any queue and has unextended blocks.
        unsafe {
            self.push_front(bin as usize, page);
            let extended = page::extend(page, &self.mem);
            debug_assert!(extended);
        }
        Some(page)
    }

    // ------------------------------------------------------------------------------------
    // Retirement and page release
    // ------------------------------------------------------------------------------------

    /// The page just became empty. Keep it for a while in case the size class is still active
    /// (mimalloc's `_mi_page_retire`), or release its slices.
    unsafe fn retire(&mut self, page: *mut Page) {
        // SAFETY: caller passes a live page of this heap.
        unsafe {
            if (*page).retire_expire != 0 {
                return;
            }
            let qi = (*page).bin as usize;
            let bsize = (*page).block_size as usize;
            let count = self.queues[qi].count;
            if count <= RETIRE_MAX_PAGES && (count == 1 || bsize < DIRECT_MAX_SIZE) {
                (*page).retire_expire = if bsize <= SMALL_MAX_OBJ_SIZE {
                    RETIRE_CYCLES
                } else {
                    RETIRE_CYCLES / 4
                };
                self.retired_min = self.retired_min.min(qi);
                self.retired_max = self.retired_max.max(qi);
                return;
            }
            self.free_page(page, qi);
        }
    }

    /// Age retired pages at the heads of their queues and release the expired ones
    /// (`_mi_theap_collect_retired`). With `force`, release every retired page now.
    unsafe fn collect_retired(&mut self, force: bool) {
        let (lo, hi) = (self.retired_min, self.retired_max);
        let mut min = QUEUE_COUNT;
        let mut max = 0;
        if lo <= hi {
            for qi in lo..=hi.min(MAX_BIN as usize) {
                let mut cur = self.queues[qi].first;
                let mut seen = 0;
                while cur != 0 && seen < RETIRE_MAX_PAGES {
                    let page = self.page_at(cur);
                    // SAFETY: queue members are live pages of this heap.
                    unsafe {
                        if (*page).retire_expire == 0 {
                            break;
                        }
                        let next = (*page).next;
                        if page::all_free(page) {
                            (*page).retire_expire -= 1;
                            if (*page).retire_expire == 0 || force {
                                self.free_page(page, qi);
                            } else {
                                min = min.min(qi);
                                max = max.max(qi);
                            }
                        } else {
                            (*page).retire_expire = 0;
                        }
                        cur = next;
                    }
                    seen += 1;
                }
            }
        }
        self.retired_min = min;
        self.retired_max = max;
    }

    /// Unlink an empty page from queue `qi` and return its slices to the map.
    unsafe fn free_page(&mut self, page: *mut Page, qi: usize) {
        // SAFETY: caller passes a live, empty page of this heap in queue `qi`.
        unsafe {
            debug_assert!(page::all_free(page));
            self.remove(qi, page);
            let kind = page::kind(page);
            let addr = page as usize;
            self.slices
                .free(addr / SLICE_SIZE, kind.page_size() / SLICE_SIZE);
        }
    }

    // ------------------------------------------------------------------------------------
    // Queues (page-queue.c) and the direct table
    // ------------------------------------------------------------------------------------

    #[inline]
    fn page_at(&self, addr: usize) -> *mut Page {
        self.mem.ptr(addr).cast::<Page>()
    }

    /// Point the direct-table entries of `bin` at the queue's first page (or the sentinel).
    fn update_direct(&mut self, bin: usize) {
        if bin == 0 || bin > MAX_BIN as usize {
            return;
        }
        let (lo, hi) = direct_range(bin as u8);
        if lo > hi {
            return;
        }
        let first = self.queues[bin].first;
        let target = if first == 0 {
            sentinel()
        } else {
            self.page_at(first)
        };
        for entry in &mut self.direct[lo..=hi] {
            *entry = target;
        }
    }

    unsafe fn push_front(&mut self, qi: usize, page: *mut Page) {
        let addr = page as usize;
        let first = self.queues[qi].first;
        // SAFETY: `page` is a live page not in any queue; `first` is a live page or 0.
        unsafe {
            (*page).next = first;
            (*page).prev = 0;
            if first != 0 {
                (*self.page_at(first)).prev = addr;
            }
        }
        let q = &mut self.queues[qi];
        if first == 0 {
            q.last = addr;
        }
        q.first = addr;
        q.count += 1;
        self.update_direct(qi);
    }

    unsafe fn push_back(&mut self, qi: usize, page: *mut Page) {
        let addr = page as usize;
        let last = self.queues[qi].last;
        // SAFETY: as for push_front.
        unsafe {
            (*page).prev = last;
            (*page).next = 0;
            if last != 0 {
                (*self.page_at(last)).next = addr;
            }
        }
        let q = &mut self.queues[qi];
        let was_empty = q.first == 0;
        if was_empty {
            q.first = addr;
        }
        q.last = addr;
        q.count += 1;
        if was_empty {
            self.update_direct(qi);
        }
    }

    unsafe fn remove(&mut self, qi: usize, page: *mut Page) {
        let addr = page as usize;
        // SAFETY: `page` is a live member of queue `qi`; its neighbours are live pages or 0.
        unsafe {
            let (prev, next) = ((*page).prev, (*page).next);
            if prev != 0 {
                (*self.page_at(prev)).next = next;
            }
            if next != 0 {
                (*self.page_at(next)).prev = prev;
            }
            let q = &mut self.queues[qi];
            let was_first = q.first == addr;
            if was_first {
                q.first = next;
            }
            if q.last == addr {
                q.last = prev;
            }
            debug_assert!(q.count > 0);
            q.count -= 1;
            (*page).next = 0;
            (*page).prev = 0;
            if was_first {
                self.update_direct(qi);
            }
        }
    }

    unsafe fn move_to_front(&mut self, qi: usize, page: *mut Page) {
        if self.queues[qi].first == page as usize {
            return;
        }
        // SAFETY: `page` is a live member of queue `qi`.
        unsafe {
            self.remove(qi, page);
            self.push_front(qi, page);
        }
    }

    unsafe fn move_to_full(&mut self, qi: usize, page: *mut Page) {
        // SAFETY: `page` is a live member of queue `qi` with no room.
        unsafe {
            self.remove(qi, page);
            page::set_in_full_queue(page, true);
            self.push_back(FULL_QUEUE, page);
        }
    }

    /// A free made room in a full page: move it back to its bin queue (at the end, as mimalloc
    /// does: putting it in front slows workloads that free one block of many full pages).
    unsafe fn unfull(&mut self, page: *mut Page) {
        // SAFETY: `page` is a live member of the full queue.
        unsafe {
            self.remove(FULL_QUEUE, page);
            page::set_in_full_queue(page, false);
            let qi = (*page).bin as usize;
            self.push_back(qi, page);
        }
    }
}

/// Number of slices a header-less run for `layout` occupies.
#[inline]
fn huge_slices(layout: Layout) -> usize {
    layout.size().div_ceil(SLICE_SIZE).max(1)
}

/// mimalloc's `mi_page_is_mostly_used`: at least 7/8 of the blocks are live.
#[inline]
unsafe fn mostly_used(page: *const Page) -> bool {
    // SAFETY: caller passes a live page.
    unsafe { (*page).used as usize * 8 >= (*page).reserved as usize * 7 }
}

/// Direct-table indices served by `bin`: sizes in `(bin_size(bin - 1), bin_size(bin)]` that are
/// at most `DIRECT_MAX_SIZE`. Empty range (`lo > hi`) when the bin is above the direct limit.
const fn direct_range(bin: u8) -> (usize, usize) {
    let lo = if bin == 1 {
        0
    } else {
        bins::bin_size(bin - 1) / WORD + 1
    };
    let hi = bins::bin_size(bin) / WORD;
    if hi >= DIRECT_ENTRIES {
        (lo, DIRECT_ENTRIES - 1)
    } else {
        (lo, hi)
    }
}

const fn sentinel_const() -> *mut Page {
    (&raw const EMPTY_PAGE).cast_mut()
}

const _: () = {
    // Every direct entry belongs to exactly one bin, and the ranges tile 0..DIRECT_ENTRIES.
    let mut expect = 0;
    let mut b = 1;
    while expect < DIRECT_ENTRIES {
        let (lo, hi) = direct_range(b);
        assert!(lo == expect);
        expect = hi + 1;
        b += 1;
    }
    assert!(bins::bin(DIRECT_MAX_SIZE) == b - 1);
};

#[cfg(test)]
#[allow(clippy::undocumented_unsafe_blocks)]
mod tests {
    use super::*;
    use crate::backend::SimMemory;
    use crate::bins::{LARGE_MAX_OBJ_SIZE, LARGE_PAGE_SIZE, MEDIUM_MAX_OBJ_SIZE, bin_size};
    use std::alloc::{alloc, dealloc};
    use std::vec::Vec;

    /// 1024 slices of bitmap: test regions of up to 64 MiB.
    type TestHeap = Heap<SimMemory, 16>;

    /// A heap over a 4 MiB-aligned host region that is released when the fixture drops.
    struct Fixture {
        heap: TestHeap,
        region: *mut u8,
        region_layout: Layout,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // SAFETY: allocated in `heap()` with this layout; the heap holds no references
            // that outlive the fixture.
            unsafe { dealloc(self.region, self.region_layout) };
        }
    }

    /// A heap over `total` slices of which `initial` are present at start and the heap base is
    /// `offset` bytes into the region.
    fn heap(total: usize, initial: usize, offset: usize) -> Fixture {
        let region_layout = Layout::from_size_align(total * SLICE_SIZE, LARGE_PAGE_SIZE).unwrap();
        // SAFETY: non-zero size.
        let region = unsafe { alloc(region_layout) };
        assert!(!region.is_null());
        // SAFETY: the region is exclusively owned by the fixture for its lifetime.
        let mem = unsafe { SimMemory::from_region(region, region_layout.size(), initial, offset) };
        let mut h = TestHeap::new(mem);
        h.set_grow_policy(GrowPolicy {
            min_grow: 2,
            max_grow: 64,
        });
        Fixture {
            heap: h,
            region,
            region_layout,
        }
    }

    /// Check every heap invariant listed in the module documentation.
    fn validate(h: &TestHeap) {
        for qi in 0..QUEUE_COUNT {
            let q = h.queues[qi];
            let mut cur = q.first;
            let mut prev = 0;
            let mut n = 0;
            while cur != 0 {
                let page = h.page_at(cur);
                // SAFETY: queue members are live pages.
                unsafe {
                    assert_eq!((*page).prev, prev, "prev link broken in queue {qi}");
                    let kind = page::kind(page);
                    assert_eq!(cur % kind.page_size(), 0, "page {cur:#x} misaligned");
                    page::validate(page, &h.mem).unwrap_or_else(|e| panic!("queue {qi}: {e}"));
                    if qi == FULL_QUEUE {
                        assert!(page::in_full_queue(page));
                        assert!(!page::has_free(page) && !page::is_expandable(page));
                    } else {
                        assert_eq!((*page).bin as usize, qi);
                        assert!(!page::in_full_queue(page));
                    }
                    for s in 0..kind.page_size() / SLICE_SIZE {
                        assert!(
                            !h.slices.is_free(cur / SLICE_SIZE + s),
                            "page slice marked free"
                        );
                    }
                    prev = cur;
                    cur = (*page).next;
                }
                n += 1;
            }
            assert_eq!(q.last, prev, "last link broken in queue {qi}");
            assert_eq!(q.count, n, "count wrong in queue {qi}");
        }
        for i in 0..DIRECT_ENTRIES {
            let b = bins::bin(i * WORD) as usize;
            let first = h.queues[b].first;
            let expect = if first == 0 {
                sentinel()
            } else {
                h.page_at(first)
            };
            assert_eq!(h.direct[i], expect, "direct entry {i} (bin {b})");
        }
    }

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    unsafe fn fill(p: NonNull<u8>, size: usize, byte: u8) {
        unsafe { p.as_ptr().write_bytes(byte, size) };
    }

    unsafe fn check(p: NonNull<u8>, size: usize, byte: u8) {
        for i in 0..size {
            assert_eq!(unsafe { *p.as_ptr().add(i) }, byte, "byte {i} corrupted");
        }
    }

    #[test]
    fn small_alloc_free_reuses_lifo() {
        let mut f = heap(64, 4, 100);
        let h = &mut f.heap;
        let l = layout(32, 8);
        unsafe {
            let a = h.alloc(l).unwrap();
            let b = h.alloc(l).unwrap();
            assert_ne!(a, b);
            assert_eq!(a.as_ptr() as usize % 8, 0);
            fill(a, 32, 0xA1);
            fill(b, 32, 0xB2);
            check(a, 32, 0xA1);
            h.dealloc(a, l);
            let c = h.alloc(l).unwrap();
            assert_eq!(a, c, "LIFO reuse of the just-freed block");
            check(b, 32, 0xB2);
            h.dealloc(b, l);
            h.dealloc(c, l);
        }
        validate(h);
    }

    #[test]
    fn uses_the_linker_gap_before_growing() {
        let mut f = heap(64, 4, 100);
        let h = &mut f.heap;
        let before = h.mem.size_slices();
        unsafe {
            let a = h.alloc(layout(64, 8)).unwrap();
            assert_eq!(
                h.mem.size_slices(),
                before,
                "first small page came from the gap"
            );
            h.dealloc(a, layout(64, 8));
        }
        validate(h);
    }

    #[test]
    fn every_bin_allocates_aligned_distinct_blocks_and_recovers_its_page() {
        let mut f = heap(1024, 4, 0);
        let h = &mut f.heap;
        for b in 1..=MAX_BIN {
            let size = bin_size(b);
            let l = layout(size, 8);
            let kind = bins::kind_of_bin(b);
            let natural = (size & size.wrapping_neg()).min(MAX_NATURAL_ALIGN);
            let mut blocks = Vec::new();
            unsafe {
                for i in 0..7 {
                    let p = h.alloc(l).unwrap();
                    let addr = p.as_ptr() as usize;
                    assert_eq!(addr % natural, 0, "bin {b} block {i} alignment");
                    let page = h.page_at(page::header_of(kind, addr));
                    assert_eq!((*page).bin, b);
                    assert!(!blocks.contains(&p));
                    fill(p, size, b);
                    blocks.push(p);
                }
                validate(h);
                for p in blocks {
                    check(p, size, b);
                    h.dealloc(p, l);
                }
            }
            validate(h);
        }
    }

    #[test]
    fn page_exhaustion_adds_pages_and_full_queue_round_trips() {
        let mut f = heap(64, 4, 0);
        let h = &mut f.heap;
        let l = layout(1024, 8);
        let per_page = bins::blocks_per_page(PageKind::Small, 1024);
        let mut blocks = Vec::new();
        unsafe {
            for _ in 0..per_page * 3 + 1 {
                blocks.push(h.alloc(l).unwrap());
            }
            validate(h);
            assert!(h.queues[24].count + h.queues[FULL_QUEUE].count == 4);
            // Free one block from what is now a full page: it must come back to its bin queue.
            let victim = blocks.swap_remove(0);
            h.dealloc(victim, l);
            validate(h);
            let again = h.alloc(l).unwrap();
            blocks.push(again);
            validate(h);
            for p in blocks {
                h.dealloc(p, l);
            }
        }
        validate(h);
        // Everything is free; retired pages linger until collected, then their slices return.
        unsafe { h.collect_retired(true) };
        validate(h);
        assert_eq!(h.queues[24].count, 0);
        assert_eq!(h.queues[FULL_QUEUE].count, 0);
    }

    #[test]
    fn retired_page_is_kept_then_released() {
        let mut f = heap(64, 4, 0);
        let h = &mut f.heap;
        let l = layout(16, 8);
        unsafe {
            let a = h.alloc(l).unwrap();
            let page_addr = page::header_of(PageKind::Small, a.as_ptr() as usize);
            h.dealloc(a, l);
            assert_eq!(h.queues[2].count, 1, "empty page retired, not freed");
            assert_ne!((*h.page_at(page_addr)).retire_expire, 0);
            // Reusing the page clears the retirement.
            let b = h.alloc(l).unwrap();
            assert_eq!(
                page::header_of(PageKind::Small, b.as_ptr() as usize),
                page_addr
            );
            h.dealloc(b, l);
            for _ in 0..RETIRE_CYCLES {
                h.collect_retired(false);
            }
            assert_eq!(
                h.queues[2].count, 0,
                "retired page released after RETIRE_CYCLES"
            );
            assert!(h.slices.is_free(page_addr / SLICE_SIZE));
        }
        validate(h);
    }

    #[test]
    fn huge_alloc_free_and_realloc_in_place() {
        let mut f = heap(256, 4, 0);
        let h = &mut f.heap;
        unsafe {
            // Grow memory once with a bigger run so that free slices exist beyond the block
            // (in-place growth can only claim slices the map already knows about).
            let warm = layout(24 * SLICE_SIZE, 8);
            let w = h.alloc(warm).unwrap();
            h.dealloc(w, warm);

            let l = layout(16 * SLICE_SIZE, 8);
            let p = h.alloc(l).unwrap();
            assert_eq!(p, w, "lowest free run is reused");
            assert_eq!(p.as_ptr() as usize % SLICE_SIZE, 0);
            fill(p, l.size(), 0x5A);
            let start = p.as_ptr() as usize / SLICE_SIZE;
            for s in 0..16 {
                assert!(!h.slices.is_free(start + s));
            }
            // Shrink in place (staying above the large-object limit) frees the tail slices.
            let smaller = 10 * SLICE_SIZE + 5;
            assert_eq!(huge_slices(layout(smaller, 8)), 11);
            let q = h.realloc(p, l, smaller).unwrap();
            assert_eq!(p, q);
            check(q, smaller, 0x5A);
            for s in 11..16 {
                assert!(h.slices.is_free(start + s));
            }
            // Grow in place when the following slices are free; only the old bytes are kept.
            let bigger = 20 * SLICE_SIZE;
            let r = h.realloc(q, layout(smaller, 8), bigger).unwrap();
            assert_eq!(p, r);
            check(r, smaller, 0x5A);
            fill(r, bigger, 0x5B);
            for s in 0..20 {
                assert!(!h.slices.is_free(start + s));
            }
            // Shrinking below the large-object limit must move the block: the new Layout would
            // classify it as a page block and the next dealloc would mask to a page header.
            let tiny = LARGE_MAX_OBJ_SIZE;
            let t = h.realloc(r, layout(bigger, 8), tiny).unwrap();
            assert_ne!(t, r);
            check(t, tiny, 0x5B);
            for s in 0..20 {
                assert!(h.slices.is_free(start + s), "run released after the move");
            }
            h.dealloc(t, layout(tiny, 8));
        }
        validate(h);
    }

    #[test]
    fn large_alignment_is_honoured() {
        let mut f = heap(512, 4, 0);
        let h = &mut f.heap;
        for shift in 0..=18 {
            let align = 1usize << shift;
            for size in [1usize, 24, 1000, 5000, 70_000, 600_000] {
                let l = layout(size, align);
                unsafe {
                    let p = h.alloc(l).unwrap();
                    assert_eq!(p.as_ptr() as usize % align, 0, "size {size} align {align}");
                    fill(p, size, 0x77);
                    h.dealloc(p, l);
                }
            }
        }
        validate(h);
    }

    #[test]
    fn alloc_zeroed_is_zero_on_fresh_and_recycled_blocks() {
        let mut f = heap(512, 4, 0);
        let h = &mut f.heap;
        for size in [
            8usize,
            100,
            1024,
            5000,
            MEDIUM_MAX_OBJ_SIZE,
            300_000,
            5 * SLICE_SIZE + 3,
        ] {
            let l = layout(size, 8);
            unsafe {
                let a = h.alloc_zeroed(l).unwrap();
                check(a, size, 0);
                fill(a, size, 0xFF);
                h.dealloc(a, l);
                let b = h.alloc_zeroed(l).unwrap();
                check(b, size, 0);
                h.dealloc(b, l);
            }
        }
        validate(h);
    }

    #[test]
    fn realloc_preserves_contents_across_classes() {
        let mut f = heap(256, 4, 0);
        let h = &mut f.heap;
        let sizes = [
            8usize, 24, 64, 1000, 1024, 4000, 10_240, 20_000, 90_000, 600_000, 24,
        ];
        unsafe {
            let mut l = layout(sizes[0], 8);
            let mut p = h.alloc(l).unwrap();
            fill(p, l.size(), 0x3C);
            for &s in &sizes[1..] {
                let keep = l.size().min(s);
                p = h.realloc(p, l, s).unwrap();
                check(p, keep, 0x3C);
                fill(p, s, 0x3C);
                l = layout(s, 8);
            }
            h.dealloc(p, l);
        }
        validate(h);
    }

    #[test]
    fn realloc_within_a_bin_returns_the_same_block() {
        let mut f = heap(64, 4, 0);
        let h = &mut f.heap;
        unsafe {
            let p = h.alloc(layout(70, 8)).unwrap();
            assert_eq!(h.realloc(p, layout(70, 8), 80).unwrap(), p);
            assert_eq!(h.realloc(p, layout(80, 8), 65).unwrap(), p);
            // Shrinking to below half moves the block to a smaller class.
            let q = h.realloc(p, layout(65, 8), 24).unwrap();
            assert_ne!(q, p);
            h.dealloc(q, layout(24, 8));
        }
        validate(h);
    }

    #[test]
    fn non_contiguous_growth_is_fine() {
        let mut f = heap(512, 2, 0);
        let h = &mut f.heap;
        let l = layout(MEDIUM_MAX_OBJ_SIZE, 8);
        unsafe {
            let a = h.alloc(l).unwrap();
            h.mem.skip_slices(3);
            let b = h.alloc(layout(LARGE_MAX_OBJ_SIZE, 8)).unwrap();
            h.mem.skip_slices(1);
            let c = h.alloc(l).unwrap();
            fill(a, l.size(), 1);
            fill(b, LARGE_MAX_OBJ_SIZE, 2);
            fill(c, l.size(), 3);
            check(a, l.size(), 1);
            check(b, LARGE_MAX_OBJ_SIZE, 2);
            h.dealloc(a, l);
            h.dealloc(b, layout(LARGE_MAX_OBJ_SIZE, 8));
            h.dealloc(c, l);
        }
        validate(h);
    }

    #[test]
    fn out_of_memory_returns_none_and_keeps_state() {
        let mut f = heap(8, 2, 0);
        let h = &mut f.heap;
        unsafe {
            let a = h.alloc(layout(100, 8)).unwrap();
            assert!(
                h.alloc(layout(LARGE_MAX_OBJ_SIZE, 8)).is_none(),
                "4 MiB page cannot fit"
            );
            assert!(
                h.alloc(layout(9 * SLICE_SIZE, 8)).is_none(),
                "huge run cannot fit"
            );
            validate(h);
            let b = h.alloc(layout(100, 8)).unwrap();
            h.dealloc(a, layout(100, 8));
            h.dealloc(b, layout(100, 8));
        }
        validate(h);
    }

    /// Randomised churn against a tiny in-test model with content checks.
    #[test]
    fn random_churn_keeps_invariants_and_contents() {
        let mut f = heap(1024, 4, 4096);
        let h = &mut f.heap;
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut live: Vec<(NonNull<u8>, Layout, u8)> = Vec::new();
        for step in 0..20_000 {
            let r = next();
            let op = r % 100;
            if op < 45 || live.is_empty() {
                let size = match (r >> 8) % 100 {
                    0..=79 => 1 + (r >> 16) as usize % 256,
                    80..=94 => 1 + (r >> 16) as usize % 20_000,
                    95..=98 => 1 + (r >> 16) as usize % 200_000,
                    _ => 1 + (r >> 16) as usize % (3 * SLICE_SIZE),
                };
                let align = 1usize << ((r >> 40) % 12);
                let l = layout(size, align);
                let byte = (r >> 56) as u8;
                unsafe {
                    let p = if op % 2 == 0 {
                        h.alloc(l).unwrap()
                    } else {
                        let p = h.alloc_zeroed(l).unwrap();
                        check(p, size, 0);
                        p
                    };
                    assert_eq!(p.as_ptr() as usize % align, 0);
                    fill(p, size, byte);
                    live.push((p, l, byte));
                }
            } else if op < 85 {
                let i = (r >> 8) as usize % live.len();
                let (p, l, byte) = live.swap_remove(i);
                unsafe {
                    check(p, l.size(), byte);
                    h.dealloc(p, l);
                }
            } else {
                let i = (r >> 8) as usize % live.len();
                let (p, l, byte) = live[i];
                let new_size = 1 + (r >> 16) as usize % (2 * l.size() + 64);
                unsafe {
                    let q = h.realloc(p, l, new_size).unwrap();
                    check(q, l.size().min(new_size), byte);
                    fill(q, new_size, byte);
                    live[i] = (q, layout(new_size, l.align()), byte);
                }
            }
            if step % 997 == 0 {
                validate(h);
                for (p, l, byte) in &live {
                    unsafe { check(*p, l.size(), *byte) };
                }
            }
        }
        for (p, l, byte) in live {
            unsafe {
                check(p, l.size(), byte);
                h.dealloc(p, l);
            }
        }
        validate(h);
        unsafe { h.collect_retired(true) };
        validate(h);
    }

    #[test]
    fn direct_table_tiles_and_starts_at_sentinel() {
        let f = heap(8, 2, 0);
        for entry in f.heap.direct {
            assert_eq!(entry, sentinel());
        }
        let (lo, hi) = direct_range(24);
        assert_eq!((lo, hi), (113, 128));
        assert_eq!(direct_range(1), (0, 1));
        assert_eq!(direct_range(2), (2, 2));
        assert_eq!(direct_range(9), (9, 10));
    }
}
