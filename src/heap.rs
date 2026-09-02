//! The heap: bin queues, the direct table, page lifecycle, and the allocation algorithms.
//!
//! This is mimalloc v3's `theap` (page.c, page-queue.c, alloc.c, free.c) reduced to one thread
//! and one arena, with three changes that Rust's `GlobalAlloc` contract makes possible:
//!
//! - the page kind and the page header address are derived from the `Layout`, so `dealloc`
//!   masks the pointer instead of consulting a page map (see [`page::header_of`]);
//! - blocks above [`bins::MAX_BINNED_OBJ_SIZE`], and blocks with alignment above
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
//! # Page release
//!
//! An empty page is retired rather than freed (mimalloc's `MI_RETIRE_CYCLES` scheme, see
//! [`RETIRE_CYCLES`]) so that a size class that oscillates between empty and one block keeps
//! its page; retired pages age at each collection (every [`GENERIC_COLLECT_PERIOD`] slow-path
//! allocations and before a fresh page is taken) and are released when their count runs out.
//! They are also all released before linear memory is grown, whatever their count: growth is
//! permanent footprint on wasm, a released page is only a page initialisation away.
//!
//! # Fast paths
//!
//! [`Heap::alloc`], [`Heap::alloc_zeroed`] and [`Heap::dealloc`] are the only inlined entry
//! points, and they are `#[inline(always)]` rather than `#[inline]`: at `opt-level = "z"` LLVM
//! declines the hint and turns every allocation into three calls (the std shim, `__rust_alloc`,
//! `Heap::alloc`), 2.5x slower on V8. Everything they call on a miss is `#[cold]
//! #[inline(never)]` so the inlined code stays small enough for V8's baseline tier and for
//! consumers building at `opt-level = "z"`.
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
/// Queues: one per bin, the full queue, and padding to a power of two so that a queue index
/// can be masked (see [`queue_index`]).
const QUEUE_COUNT: usize = (BIN_COUNT + 1).next_power_of_two();
const _: () = assert!(FULL_QUEUE < QUEUE_COUNT && QUEUE_COUNT.is_power_of_two());

/// A queue index that is provably in bounds. Queue indices come from a page header's `bin` byte
/// or from `bins::bin`, both in `1..=MAX_BIN` by the invariants, but the compiler cannot see
/// that and would guard every `queues[..]` with a bounds-check panic path (three of them in
/// `__rust_realloc`, roofline 12.3). Masking costs one instruction on cold paths and turns a
/// broken invariant, which debug builds report here, into a wrong queue rather than an abort.
#[inline(always)]
fn queue_index(qi: usize) -> usize {
    debug_assert!(qi < QUEUE_COUNT, "queue index {qi} out of range");
    qi & (QUEUE_COUNT - 1)
}

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
    #[inline(always)]
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
    #[inline(always)]
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
    #[inline(always)]
    pub unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        let addr = ptr.addr().get();
        let mut size = layout.size();
        let align = layout.align();
        if align <= MAX_NATURAL_ALIGN {
            if align > WORD {
                // The same rounding as `alloc` and `bins::classify`: the page kind is a function
                // of the rounded size, and every block of a small page masks to its header
                // whatever its alignment, so aligned frees need not leave the fast path.
                size = (size + align - 1) & !(align - 1);
            }
            if size <= SMALL_MAX_OBJ_SIZE {
                // Small page: classify(layout) is Bin(b) with kind Small, and bins::block_start
                // plus the small page alignment put every block strictly inside its page.
                let page = self
                    .mem
                    .ptr(page::header_of(PageKind::Small, addr))
                    .cast::<Page>();
                // SAFETY: by the precondition `addr` is a live block of the small page at the
                // masked address, which is a page of this heap.
                unsafe {
                    page::push(page, &self.mem, addr);
                    if needs_transition(page) {
                        self.dealloc_transition(page);
                    }
                }
                return;
            }
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
            (Class::Bin(old), Class::Bin(new)) if fits_in_place(old, new, new_size) => {
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
                // In place through the free slices after the run and, when the run is at the
                // top of the heap, through memory.grow; the copy below is the last resort.
                if slices::extend_with_growth(
                    &mut self.slices,
                    &mut self.mem,
                    start,
                    old_n,
                    new_n - old_n,
                    &self.policy,
                )
                .is_some()
                {
                    return Some(ptr);
                }
            }
            _ => {}
        }
        // The block moves. Every move into a run is a growth (a run only ever shrinks in place),
        // that is, a buffer the program is growing, so the new run goes to the bottom of the
        // free tail at the top of the heap, where the next growth extends it instead of copying
        // it again. A lowest-fit run would land in a hole between pages and move at every step.
        let new = match bins::classify(new_layout) {
            Class::Huge => self.alloc_huge(new_layout, false, true)?,
            // SAFETY: new_layout has non-zero size (caller contract).
            Class::Bin(_) => unsafe { self.alloc(new_layout)? },
        };
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
            Class::Huge => self.alloc_huge(layout, zero, false),
            Class::Bin(bin) => {
                self.generic_count += 1;
                if self.generic_count >= GENERIC_COLLECT_PERIOD {
                    self.generic_count = 0;
                    // SAFETY: heap invariants hold between operations.
                    unsafe { self.collect_retired(false) };
                }
                // SAFETY: heap invariants hold between operations.
                let page = unsafe { self.find_page(bin) }?;
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

    /// A header-less run for `layout`: the lowest fit, or with `top` the bottom of the free
    /// tail when that is long enough (see [`SliceMap::alloc_tail`] and the `slices` module
    /// documentation for why a growing buffer wants the top).
    fn alloc_huge(&mut self, layout: Layout, zero: bool, top: bool) -> Option<NonNull<u8>> {
        debug_assert!(self.initialized);
        let count = huge_slices(layout);
        // Alignments up to a slice are satisfied by slice alignment; larger ones align the run.
        let align = layout.align().div_ceil(SLICE_SIZE).max(1);
        let run = self.acquire_run(count, align, top)?;
        let addr = run.start * SLICE_SIZE;
        let p = self.mem.ptr(addr);
        if zero && !run.zeroed {
            // SAFETY: the run is `count * SLICE_SIZE >= layout.size()` bytes we own.
            unsafe { p.write_bytes(0, layout.size()) };
        }
        // SAFETY: slice addresses of owned memory are non-null.
        Some(unsafe { NonNull::new_unchecked(p) })
    }

    /// Slices for a page or a run: the lowest fit, or with `top` the bottom of the free tail
    /// first when it is long enough, so no memory is grown for a run a hole could hold. When
    /// nothing fits, every retired page is released and the map searched again before memory
    /// is grown: linear memory never shrinks, so a grow is footprint for good, while a released
    /// page costs one page initialisation if its bin comes back. mimalloc only ages its retired
    /// pages at this point; it has an OS to return memory to.
    fn acquire_run(&mut self, count: usize, align: usize, top: bool) -> Option<slices::Run> {
        if top {
            let end = self.mem.size_slices();
            if let Some(run) = self.slices.alloc_tail(count, align, end) {
                return Some(run);
            }
        }
        if let Some(run) = self.slices.alloc(count, align) {
            return Some(run);
        }
        // SAFETY: heap invariants hold between operations.
        unsafe { self.collect_retired(true) };
        slices::acquire(&mut self.slices, &mut self.mem, count, align, &self.policy)
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
                    if needs_transition(page) {
                        self.dealloc_transition(page);
                    }
                }
            }
        }
    }

    /// A free made the page empty (and it is not retired yet), or the page sits in the full
    /// queue: fix the queues. See [`needs_transition`].
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
        // Whatever lies between the heap base and the end of the initial memory is ours by the
        // `Memory` contract (the linker gap on wasm32-unknown-unknown; nothing on wasi, where the
        // backend starts at the end of memory because wasi-libc's malloc owns the gap); use it
        // before paying for a memory.grow. Its contents are not guaranteed zero.
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
        let mut cur = self.queues[queue_index(qi)].first;
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
        let run = self.acquire_run(n, n, false)?;
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
            let count = self.queues[queue_index(qi)].count;
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

    /// Age the retired pages among the first [`RETIRE_MAX_PAGES`] of each queue in the retired
    /// range and release the expired ones (`_mi_theap_collect_retired`). With `force`, release
    /// every retired page seen. A page that was reused and is no longer empty is simply
    /// un-retired.
    ///
    /// Unlike mimalloc, the scan does not stop at the first page that is not retired: a page
    /// that came back into use and was moved to the front of its queue would otherwise hide the
    /// retired pages behind it, and [`acquire_run`](Self::acquire_run) relies on a forced
    /// collection to release those before memory is grown.
    unsafe fn collect_retired(&mut self, force: bool) {
        let (lo, hi) = (self.retired_min, self.retired_max);
        let mut min = QUEUE_COUNT;
        let mut max = 0;
        if lo <= hi {
            for qi in lo..=hi.min(MAX_BIN as usize) {
                let mut cur = self.queues[queue_index(qi)].first;
                let mut seen = 0;
                while cur != 0 && seen < RETIRE_MAX_PAGES {
                    let page = self.page_at(cur);
                    // SAFETY: queue members are live pages of this heap.
                    unsafe {
                        let next = (*page).next;
                        if (*page).retire_expire != 0 {
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
        let first = self.queues[queue_index(bin)].first;
        let target = if first == 0 {
            sentinel()
        } else {
            self.page_at(first)
        };
        // `hi` is below DIRECT_ENTRIES by construction. An index loop with the bound in its
        // condition, rather than a range slice, is what compiles without a panic path.
        let mut i = lo;
        while i <= hi && i < DIRECT_ENTRIES {
            self.direct[i] = target;
            i += 1;
        }
    }

    unsafe fn push_front(&mut self, qi: usize, page: *mut Page) {
        let addr = page as usize;
        let first = self.queues[queue_index(qi)].first;
        // SAFETY: `page` is a live page not in any queue; `first` is a live page or 0.
        unsafe {
            (*page).next = first;
            (*page).prev = 0;
            if first != 0 {
                (*self.page_at(first)).prev = addr;
            }
        }
        let q = &mut self.queues[queue_index(qi)];
        if first == 0 {
            q.last = addr;
        }
        q.first = addr;
        q.count += 1;
        self.update_direct(qi);
    }

    unsafe fn push_back(&mut self, qi: usize, page: *mut Page) {
        let addr = page as usize;
        let last = self.queues[queue_index(qi)].last;
        // SAFETY: as for push_front.
        unsafe {
            (*page).prev = last;
            (*page).next = 0;
            if last != 0 {
                (*self.page_at(last)).next = addr;
            }
        }
        let q = &mut self.queues[queue_index(qi)];
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
            let q = &mut self.queues[queue_index(qi)];
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
        if self.queues[queue_index(qi)].first == page as usize {
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

/// Whether a free that left `page` in its current state has queue work to do: the page sits in
/// the full queue, or it just became empty and is not retired yet.
///
/// An empty page that is already retired (`retire_expire != 0`) would make `retire` return at
/// once, and a page whose single live block oscillates between live and free, the shape of the
/// alloc-then-free microbenchmarks, hits exactly that case on every free. Testing the byte
/// inline saves the cold call there: 1.4 ns per pair on V8's optimizing tier, 1.0 ns under
/// Cranelift.
///
/// # Safety
///
/// `page` must point to a header written by `page::init`.
#[inline(always)]
unsafe fn needs_transition(page: *const Page) -> bool {
    // SAFETY: the header is valid for reads by the precondition.
    unsafe { ((*page).used == 0 && (*page).retire_expire == 0) || (*page).flags != 0 }
}

/// Whether a block of bin `old` keeps serving a request of `new_size` bytes that classifies as
/// bin `new` (the in-place decision of [`Heap::realloc`], kept pure so a proof can quantify over
/// every pair of Layouts). Growing never fits: bins are tight, so `new > old` means the request
/// exceeds the block. Shrinking stays in place only within the page kind, because the next
/// `dealloc` recomputes the kind from the new Layout and masks the address with it, and, as in
/// mimalloc, only while the block stays at least half used.
#[inline]
fn fits_in_place(old: u8, new: u8, new_size: usize) -> bool {
    new == old
        || (new < old
            && bins::kind_of_bin(new) == bins::kind_of_bin(old)
            && new_size >= bins::bin_size(old) / 2)
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

/// The heap invariants listed in the module documentation, as checks that return an error
/// naming the first violation. Test and proof infrastructure: never part of the allocator.
#[cfg(any(test, kani))]
impl<M: Memory, const WORDS: usize> Heap<M, WORDS> {
    /// Invariants 1, 2 and 4 for queue `qi`: every member is a valid page of the right bin (or
    /// a flagged, roomless page in the full queue), the links and the count agree, and the
    /// page's slices are not free in the map.
    ///
    /// The walk follows `next` links, so it relies on the invariant it checks (queue members are
    /// live pages); it stops after `count + 1` members, so a cycle is reported, not looped on.
    pub(crate) fn validate_queue(&self, qi: usize) -> Result<(), &'static str> {
        self.validate_queue_inner(qi, true)
    }

    /// [`validate_queue`](Self::validate_queue) without the slice-map check, for proofs whose
    /// pages are host objects outside any slice map.
    #[cfg(kani)]
    pub(crate) fn validate_queue_links(&self, qi: usize) -> Result<(), &'static str> {
        self.validate_queue_inner(qi, false)
    }

    fn validate_queue_inner(&self, qi: usize, slices: bool) -> Result<(), &'static str> {
        let q = self.queues[queue_index(qi)];
        let mut cur = q.first;
        let mut prev = 0;
        let mut n = 0;
        while cur != 0 {
            if n > q.count {
                return Err("queue longer than its count (cycle or lost page)");
            }
            let page = self.page_at(cur);
            // SAFETY: queue members are live pages of this heap (invariant 1, under test).
            unsafe {
                if (*page).prev != prev {
                    return Err("prev link does not point at the previous member");
                }
                let kind = page::kind(page);
                if cur % kind.page_size() != 0 {
                    return Err("page address is not aligned to its kind");
                }
                page::validate(page, &self.mem)?;
                if qi == FULL_QUEUE {
                    if !page::in_full_queue(page) {
                        return Err("full-queue member is not flagged full");
                    }
                    if page::has_free(page) || page::is_expandable(page) {
                        return Err("full-queue member still has room");
                    }
                } else {
                    if (*page).bin as usize != qi {
                        return Err("page sits in the queue of another bin");
                    }
                    if page::in_full_queue(page) {
                        return Err("bin-queue member is flagged full");
                    }
                }
                if slices {
                    let first_slice = cur / SLICE_SIZE;
                    let mut s = 0;
                    while s < kind.page_size() / SLICE_SIZE {
                        if self.slices.is_free(first_slice + s) {
                            return Err("a slice of a live page is free in the map");
                        }
                        s += 1;
                    }
                }
                prev = cur;
                cur = (*page).next;
            }
            n += 1;
        }
        if q.last != prev {
            return Err("last does not point at the final member");
        }
        if q.count != n {
            return Err("count does not match the members");
        }
        Ok(())
    }

    /// Invariant 3 for direct entry `i`: it is the first page of the queue of `bin(i * WORD)`,
    /// or the sentinel when that queue is empty.
    pub(crate) fn validate_direct_entry(&self, i: usize) -> Result<(), &'static str> {
        let b = bins::bin(i * WORD) as usize;
        let first = self.queues[queue_index(b)].first;
        let expect = if first == 0 {
            sentinel()
        } else {
            self.page_at(first)
        };
        if self.direct[i] != expect {
            return Err("direct entry does not point at its queue's first page");
        }
        Ok(())
    }

    /// Every invariant, every queue and every direct entry. The proofs check one symbolic
    /// queue or entry at a time instead, so this is for tests.
    #[cfg(test)]
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        let mut qi = 0;
        while qi < QUEUE_COUNT {
            self.validate_queue(qi)?;
            qi += 1;
        }
        let mut i = 0;
        while i < DIRECT_ENTRIES {
            self.validate_direct_entry(i)?;
            i += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::undocumented_unsafe_blocks)]
mod tests {
    use super::*;
    use crate::backend::SimMemory;
    use crate::bins::{
        LARGE_MAX_OBJ_SIZE, LARGE_PAGE_SIZE, MAX_BINNED_BIN, MAX_BINNED_OBJ_SIZE,
        MEDIUM_MAX_OBJ_SIZE, bin_size,
    };
    use core::mem::size_of;
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
            ..GrowPolicy::DEFAULT
        });
        Fixture {
            heap: h,
            region,
            region_layout,
        }
    }

    /// Check every heap invariant listed in the module documentation, naming the queue or
    /// entry that fails.
    fn validate(h: &TestHeap) {
        for qi in 0..QUEUE_COUNT {
            h.validate_queue(qi)
                .unwrap_or_else(|e| panic!("queue {qi}: {e}"));
        }
        for i in 0..DIRECT_ENTRIES {
            h.validate_direct_entry(i)
                .unwrap_or_else(|e| panic!("direct entry {i}: {e}"));
        }
        h.validate().unwrap();
    }

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    unsafe fn fill(p: NonNull<u8>, size: usize, byte: u8) {
        unsafe { p.as_ptr().write_bytes(byte, size) };
    }

    /// Every byte of the block is `byte`. Whole words where the block allows it: under Miri
    /// each read is an interpreted operation, and the tests check megabytes.
    unsafe fn check(p: NonNull<u8>, size: usize, byte: u8) {
        let word = usize::from_ne_bytes([byte; size_of::<usize>()]);
        let mut i = 0;
        while i + size_of::<usize>() <= size {
            // SAFETY: inside the block, which the caller owns; blocks are word aligned.
            let w = unsafe { p.as_ptr().add(i).cast::<usize>().read_unaligned() };
            assert_eq!(w, word, "word at {i} corrupted");
            i += size_of::<usize>();
        }
        while i < size {
            // SAFETY: inside the block.
            assert_eq!(unsafe { *p.as_ptr().add(i) }, byte, "byte {i} corrupted");
            i += 1;
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
        for b in 1..=MAX_BINNED_BIN {
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
    fn retired_pages_are_released_before_memory_grows() {
        // Four initial slices, one page each for four bins, then all four blocks freed: four
        // retired pages and an empty map.
        let mut f = heap(64, 4, 0);
        let h = &mut f.heap;
        let sizes = [16usize, 24, 32, 40];
        unsafe {
            let blocks: Vec<_> = sizes
                .iter()
                .map(|&s| h.alloc(layout(s, 8)).unwrap())
                .collect();
            let end = h.mem.size_slices();
            assert_eq!(h.free_slices(), 0);
            for (p, &s) in blocks.iter().zip(&sizes) {
                h.dealloc(*p, layout(s, 8));
            }
            for s in &sizes {
                let qi = bins::bin(*s) as usize;
                assert_eq!(h.queues[qi].count, 1, "page of bin {qi} retired, not freed");
            }
            validate(h);
            // A fifth bin needs a page: the retired ones are released and one of their slices
            // is reused instead of growing memory.
            let e = h.alloc(layout(48, 8)).unwrap();
            assert_eq!(h.mem.size_slices(), end, "no memory.grow");
            for s in &sizes {
                assert_eq!(h.queues[bins::bin(*s) as usize].count, 0);
            }
            assert_eq!(h.free_slices(), 3);
            validate(h);
            // Three of the released bins come back with a fresh page each from the map.
            let again: Vec<_> = sizes[..3]
                .iter()
                .map(|&s| h.alloc(layout(s, 8)).unwrap())
                .collect();
            assert_eq!(h.free_slices(), 0);
            assert_eq!(h.mem.size_slices(), end);
            // And once the map is empty with nothing retired, memory does grow.
            let g = h.alloc(layout(sizes[3], 8)).unwrap();
            assert!(h.mem.size_slices() > end);
            h.dealloc(g, layout(sizes[3], 8));
            h.dealloc(e, layout(48, 8));
            for (p, &s) in again.iter().zip(&sizes) {
                h.dealloc(*p, layout(s, 8));
            }
        }
        validate(h);
    }

    #[test]
    fn a_forced_collection_reaches_a_retired_page_behind_a_page_in_use() {
        let mut f = heap(64, 8, 0);
        let h = &mut f.heap;
        let l = layout(16, 8);
        let per_page = bins::blocks_per_page(PageKind::Small, 16);
        let qi = bins::bin(16) as usize;
        unsafe {
            // Fill pages A and B (each moves to the full queue when the next allocation finds
            // it full) and start page C, which is the queue's only member.
            let a: Vec<_> = (0..per_page).map(|_| h.alloc(l).unwrap()).collect();
            let b: Vec<_> = (0..per_page).map(|_| h.alloc(l).unwrap()).collect();
            let c = h.alloc(l).unwrap();
            let page_a = page::header_of(PageKind::Small, a[0].as_ptr() as usize);
            let page_b = page::header_of(PageKind::Small, b[0].as_ptr() as usize);
            let page_c = page::header_of(PageKind::Small, c.as_ptr() as usize);
            assert_eq!(h.queues[qi].count, 1);
            assert_eq!(h.queues[qi].first, page_c);
            assert_eq!(h.queues[FULL_QUEUE].count, 2);
            // One block of A back: A returns to the end of the bin queue. Then B is emptied: its
            // first free puts it behind A, the last one retires it.
            h.dealloc(a[0], l);
            for p in b {
                h.dealloc(p, l);
            }
            assert_eq!(h.queues[qi].first, page_c);
            assert_eq!(h.queues[qi].last, page_b);
            assert_eq!(h.queues[qi].count, 3);
            assert_eq!((*h.page_at(page_c)).retire_expire, 0);
            assert_eq!((*h.page_at(page_a)).retire_expire, 0);
            assert_ne!((*h.page_at(page_b)).retire_expire, 0);
            validate(h);
            // Two pages in use precede B; the forced collection still releases it.
            h.collect_retired(true);
            assert_eq!(h.queues[qi].count, 2);
            assert_eq!(h.queues[qi].first, page_c);
            assert_eq!(h.queues[qi].last, page_a);
            assert!(h.slices.is_free(page_b / SLICE_SIZE));
            validate(h);
            h.dealloc(c, l);
            for p in &a[1..] {
                h.dealloc(*p, l);
            }
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
            // Shrinking to a binned size must move the block: the new Layout would classify it
            // as a page block and the next dealloc would mask to a page header.
            let tiny = MAX_BINNED_OBJ_SIZE;
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
    fn realloc_grows_a_top_run_through_memory_growth() {
        let mut f = heap(256, 4, 0);
        let h = &mut f.heap;
        unsafe {
            // Three of the four initial slices: the run reaches the end of memory with one
            // free slice after it, which is not enough for the growth below.
            let l = layout(3 * SLICE_SIZE, 8);
            let p = h.alloc(l).unwrap();
            let start = p.as_ptr() as usize / SLICE_SIZE;
            assert_eq!(h.mem.size_slices(), start + 4);
            fill(p, l.size(), 0x11);
            let bigger = 10 * SLICE_SIZE;
            let q = h.realloc(p, l, bigger).unwrap();
            assert_eq!(q, p, "extended in place through memory growth");
            check(q, l.size(), 0x11);
            assert_eq!(
                h.mem.size_slices(),
                start + 10,
                "grew by exactly the missing slices"
            );
            for s in 0..10 {
                assert!(!h.slices.is_free(start + s));
            }
            fill(q, bigger, 0x12);
            validate(h);

            // Growth the region cannot provide moves nothing and fails cleanly.
            let end = h.mem.size_slices();
            assert!(h.realloc(q, layout(bigger, 8), 1000 * SLICE_SIZE).is_none());
            assert_eq!(h.mem.size_slices(), end);
            check(q, bigger, 0x12);
            validate(h);

            // Someone else grows memory first: the fresh region is not contiguous with the
            // run, so the block moves; its contents survive and the region is not lost.
            assert!(h.mem.skip_slices(1));
            let free_before = h.free_slices();
            let r = h.realloc(q, layout(bigger, 8), 12 * SLICE_SIZE).unwrap();
            assert_ne!(r, q, "a non-contiguous region cannot extend the run");
            check(r, bigger, 0x12);
            assert!(h.mem.size_slices() > end + 1);
            assert!(
                !h.slices.is_free(end + 1),
                "the skipped slice never enters the map"
            );
            assert!(
                h.free_slices() >= free_before + 10,
                "the old run is free again"
            );
            h.dealloc(r, layout(12 * SLICE_SIZE, 8));
        }
        validate(h);
    }

    #[test]
    fn a_run_that_cannot_extend_moves_to_the_top_of_the_heap() {
        let mut f = heap(256, 4, 0);
        let h = &mut f.heap;
        unsafe {
            // Grow memory once so the top has room, then free it all.
            let warm = layout(40 * SLICE_SIZE, 8);
            let w = h.alloc(warm).unwrap();
            h.dealloc(w, warm);
            let end = h.mem.size_slices();
            let two = layout(2 * SLICE_SIZE, 8);
            // Over-aligned, so a run of one slice rather than a block in a medium page.
            let one = layout(100, 2 * MAX_NATURAL_ALIGN);
            // A run with another block right after it cannot extend in place.
            let a = h.alloc(two).unwrap();
            let b = h.alloc(one).unwrap();
            assert_eq!(b.as_ptr() as usize, a.as_ptr() as usize + 2 * SLICE_SIZE);
            fill(a, two.size(), 0x21);
            fill(b, one.size(), 0x22);
            let four = layout(4 * SLICE_SIZE, 8);
            let a2 = h.realloc(a, two, four.size()).unwrap();
            assert_ne!(a2, a);
            check(a2, two.size(), 0x21);
            check(b, one.size(), 0x22);
            // The moved run sits at the bottom of the free tail, right after `b`, with the rest
            // of the tail above it, and memory did not grow for it.
            assert_eq!(a2.as_ptr() as usize, b.as_ptr() as usize + SLICE_SIZE);
            assert_eq!(h.mem.size_slices(), end);
            validate(h);
            // From there it grows in place: through the tail first, then through memory.grow
            // once the tail is used up (37 free slices above it, 46 needed, so 9 missing; an
            // eighth of the 44-slice heap is 5, so the need wins).
            fill(a2, four.size(), 0x23);
            let big = layout(50 * SLICE_SIZE, 8);
            let a3 = h.realloc(a2, four, big.size()).unwrap();
            assert_eq!(a3, a2);
            check(a3, four.size(), 0x23);
            assert_eq!(
                h.mem.size_slices(),
                end + 9,
                "grew by exactly the missing slices"
            );
            // The slices the block left behind are free and the lowest fit reuses them.
            let c = h.alloc(two).unwrap();
            assert_eq!(c, a);
            h.dealloc(c, two);
            h.dealloc(b, one);
            h.dealloc(a3, big);
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
                "neither a 4 MiB page nor an 8-slice run fits in 7 slices"
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
        // Miri interprets every access, so it gets a shorter run over the same distribution.
        let steps = if cfg!(miri) { 1_500 } else { 20_000 };
        for step in 0..steps {
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

#[cfg(kani)]
mod verify {
    //! Bounded proofs for the heap.
    //!
    //! The arithmetic harnesses quantify over every Layout (or every direct index) and cost a
    //! few seconds. The structural harnesses run the real heap over [`HeapModel`], a linear
    //! memory of a few small pages that stores only the words the heap may touch, for two or
    //! three symbolic operations and check the module's invariants after each; see
    //! `page::verify` for why a flat buffer is not an option and `slices::verify` for the
    //! unwind discipline (one bound for every loop, so quantify over one symbolic index rather
    //! than looping, and keep every loop short).
    use super::*;
    use crate::bins::{MAX_BINNED_BIN, bin_size, kind_of_bin};
    use crate::page::header_of;

    // --------------------------------------------------------------------------------------
    // Arithmetic: decisions that depend only on the Layout
    // --------------------------------------------------------------------------------------

    /// `realloc` returns the same binned block only when the new Layout classifies into the
    /// same page kind and the block is large enough for the new size rounded up to the
    /// alignment, so the later `dealloc` with the new Layout masks to the right header and the
    /// block holds every byte the caller may use.
    #[kani::proof]
    fn realloc_in_place_keeps_the_kind_and_fits() {
        let old_size: usize = kani::any();
        let new_size: usize = kani::any();
        let shift: u32 = kani::any();
        kani::assume(shift <= 12);
        let align = 1usize << shift;
        let Ok(old) = Layout::from_size_align(old_size, align) else {
            return;
        };
        let Ok(new) = Layout::from_size_align(new_size, align) else {
            return;
        };
        kani::assume(old_size >= 1 && new_size >= 1);
        if let (Class::Bin(o), Class::Bin(n)) = (bins::classify(old), bins::classify(new)) {
            if fits_in_place(o, n, new_size) {
                let rounded = (new_size + align - 1) & !(align - 1);
                assert!(kind_of_bin(o) == kind_of_bin(n));
                assert!(bin_size(o) >= new_size);
                assert!(bin_size(o) >= rounded);
                // dealloc's fast-path test on the new Layout picks the page's actual kind.
                assert!((rounded <= SMALL_MAX_OBJ_SIZE) == (kind_of_bin(o) == PageKind::Small));
            } else {
                // A move: the new block comes from alloc, which classifies the new Layout the
                // same way, so nothing here depends on the old block.
                assert!(n != o);
            }
        }
    }

    /// The direct table tiles `0..DIRECT_ENTRIES` with the ranges of consecutive bins, each
    /// entry belongs to exactly the bin of its size, and the fast path's index lands in the
    /// entry of the request's bin.
    #[kani::proof]
    fn direct_table_tiles_and_matches_bin() {
        let i: usize = kani::any();
        kani::assume(i < DIRECT_ENTRIES);
        let b = bins::bin(i * WORD);
        assert!(b >= 1 && b <= bins::bin(DIRECT_MAX_SIZE));
        let (lo, hi) = direct_range(b);
        assert!(lo <= i && i <= hi && hi < DIRECT_ENTRIES);
        let other: u8 = kani::any();
        kani::assume(other >= 1 && other <= MAX_BIN && other != b);
        let (lo2, hi2) = direct_range(other);
        assert!(lo2 > hi2 || i < lo2 || i > hi2);

        let size: usize = kani::any();
        kani::assume(size <= DIRECT_MAX_SIZE);
        let idx = bins::direct_index(size);
        assert!(idx < DIRECT_ENTRIES);
        assert!(bins::bin(idx * WORD) == bins::bin(size));
    }

    /// A header-less run covers its Layout: at least one slice, enough bytes, and a start that
    /// is aligned for the Layout whenever it is aligned to `alloc_huge`'s run alignment.
    #[kani::proof]
    fn huge_runs_cover_the_layout_and_its_alignment() {
        let size: usize = kani::any();
        let shift: u32 = kani::any();
        kani::assume(shift < usize::BITS - 1);
        let align = 1usize << shift;
        let Ok(layout) = Layout::from_size_align(size, align) else {
            return;
        };
        kani::assume(size >= 1);
        let n = huge_slices(layout);
        assert!(n >= 1);
        assert!(n * SLICE_SIZE >= size);
        let run_align = layout.align().div_ceil(SLICE_SIZE).max(1);
        assert!(run_align.is_power_of_two());
        let start: usize = kani::any();
        kani::assume(start <= crate::backend::MAX_SLICE_INDEX);
        kani::assume(start & (run_align - 1) == 0);
        assert!((start * SLICE_SIZE) & (align - 1) == 0);
    }

    /// The `dealloc` fast path decides "small page" from the size rounded up to the alignment;
    /// that agrees with `classify`, which `alloc` used, for every Layout that reaches the test,
    /// and `alloc`'s direct-table index picks the same bin as `classify`.
    #[kani::proof]
    fn dealloc_fast_path_agrees_with_classify() {
        let size: usize = kani::any();
        let shift: u32 = kani::any();
        kani::assume(shift <= 12);
        let align = 1usize << shift;
        let Ok(layout) = Layout::from_size_align(size, align) else {
            return;
        };
        kani::assume(size >= 1);
        let rounded = if align > WORD {
            (size + align - 1) & !(align - 1)
        } else {
            size
        };
        let small = rounded <= SMALL_MAX_OBJ_SIZE;
        match bins::classify(layout) {
            Class::Bin(b) => {
                assert!(small == (kind_of_bin(b) == PageKind::Small));
                assert!(b <= MAX_BINNED_BIN);
                assert!(bin_size(b) >= size);
            }
            Class::Huge => assert!(!small),
        }
        if rounded <= DIRECT_MAX_SIZE {
            let b = bins::bin(bins::direct_index(rounded) * WORD);
            assert!(bins::classify(layout) == Class::Bin(b));
        }
    }

    // --------------------------------------------------------------------------------------
    // Structure: the real heap over a tiny modelled memory
    // --------------------------------------------------------------------------------------

    /// Blocks per page and block size of bin 36, the bin the structural harnesses use: 8 KiB
    /// blocks, seven per page, block 0 at offset 4096, one block linked per `extend`, no direct
    /// entries, so no loop in the heap runs more than a handful of iterations.
    const BIN: u8 = 36;
    const BS: usize = 8192;
    const BLOCKS: usize = 7;
    const BLOCK_START: usize = 4096;
    const BLOCK_WORDS: usize = 1;

    const _: () = {
        assert!(bins::bin_size(BIN) == BS && bins::block_start(BS) == BLOCK_START);
        assert!(bins::blocks_per_page(PageKind::Small, BS) == BLOCKS);
        assert!(direct_range(BIN).0 > direct_range(BIN).1);
    };

    /// A linear memory of one small page of [`BIN`], storing only what the heap may touch: the
    /// page header, and the first word of every block (its free-list link) in one array. The
    /// rest of a block is never modelled: `alloc_zeroed` clears it on a page that has seen a
    /// free, so the harnesses only zero-allocate from pages that have not, and the bound on
    /// that clear (`layout.size() <= bin_size`) is an arithmetic fact the bins harnesses prove.
    ///
    /// Two facts about CBMC shape this model. Under Kani a pointer is `object << 48 | offset`,
    /// so a `Page` local's address is 64 KiB-aligned and can serve as the page address itself,
    /// which the heap requires because it turns header pointers back into addresses (queue
    /// links, `page::extend`, `free_page`, `header_of`). And every dereference of a pointer
    /// that may target several objects is checked against each of them, and an array indexed
    /// by a symbolic value is flattened whole into the formula, so the model keeps the targets
    /// to two, the header and one seven-word array (see `page::verify::PageModel`; a version
    /// with one object per block cost four times as much, and versions with a 64 KiB buffer or
    /// with whole blocks in the array exhausted memory). A second slice would have
    /// to be another object, whose address is `2^32` slices away, so the model has one slice;
    /// the multi-page queue behaviour is covered by the tests and the ledger.
    ///
    /// Block pointers are therefore not identity-mapped: the harness converts the pointer
    /// `alloc` returns back to the block's address with [`HeapModel::addr_of`] and hands
    /// `dealloc` a pointer carrying that address, which is all `dealloc` reads from it.
    struct HeapModel {
        header: *mut Page,
        words: *mut usize,
        /// Whether the slice is present (initially, as the linker gap, or after one `grow`).
        present: bool,
    }

    impl HeapModel {
        fn page_addr(&self) -> usize {
            self.header.addr()
        }

        /// The address the heap thinks the block returned as `p` has.
        fn addr_of(&self, p: *mut u8) -> usize {
            let off = p.addr().wrapping_sub(self.words.addr());
            assert!(off % (BLOCK_WORDS * WORD) == 0, "not the start of a block");
            let k = off / (BLOCK_WORDS * WORD);
            assert!(k < BLOCKS, "not a block the model hands out");
            self.page_addr() + BLOCK_START + k * BS
        }
    }

    // SAFETY: proof-only backend. `header` points at a `Page` and `words` at
    // `BLOCKS * BLOCK_WORDS` words, both owned by the harness frame for longer than the model
    // lives. `ptr` yields the header for the page address and otherwise block `k`'s first word
    // for the address of block `k`, after asserting the address is exactly that, so every
    // pointer is valid for the access the heap makes through it (a header, a link word, or
    // `BS` bytes).
    unsafe impl Memory for HeapModel {
        fn heap_base(&self) -> usize {
            self.page_addr()
        }

        fn size_slices(&self) -> usize {
            self.page_addr() / SLICE_SIZE + self.present as usize
        }

        fn grow(&mut self, slices: usize) -> Option<usize> {
            if self.present || slices != 1 {
                return None;
            }
            self.present = true;
            Some(self.page_addr() / SLICE_SIZE)
        }

        fn ptr(&self, addr: usize) -> *mut u8 {
            assert!(self.present, "access before memory exists");
            let page = self.page_addr();
            assert!(
                addr >= page && addr - page < SLICE_SIZE,
                "access outside the page"
            );
            let offset = addr - page;
            if offset == 0 {
                return self.header.cast();
            }
            assert!(offset >= BLOCK_START, "access inside the header reserve");
            let rel = offset - BLOCK_START;
            assert!(rel % BS == 0, "access is not to the start of a block");
            let k = rel / BS;
            assert!(k < BLOCKS, "access beyond the page's blocks");
            // SAFETY: `words` has BLOCKS * BLOCK_WORDS elements and `k < BLOCKS`.
            unsafe { self.words.add(k * BLOCK_WORDS).cast() }
        }
    }

    /// One bitmap word: the model has one slice, and a two-word map would need one more
    /// unwinding of every word loop (`SliceMap::init` scans the map) for nothing.
    type ProofHeap = Heap<HeapModel, 1>;

    /// The storage a [`HeapModel`] points into, as locals of the calling harness: the header
    /// is its own object so that it starts at an aligned address (a struct field may not).
    macro_rules! proof_heap {
        ($h:ident, $initial:expr) => {
            let mut header = EMPTY_PAGE;
            let mut words = [0usize; BLOCKS * BLOCK_WORDS];
            let mut $h = proof_heap(&raw mut header, words.as_mut_ptr(), $initial);
        };
    }

    /// A heap over the given storage, whose slice is present from the start when `initial`
    /// (the linker gap, dirty as far as the heap knows) and otherwise arrives through `grow`,
    /// zero. Under CBMC every object starts at `object << 48`, so `header` is 64 KiB-aligned
    /// as the heap requires; the harness checks that rather than assuming it.
    fn proof_heap(header: *mut Page, words: *mut usize, initial: bool) -> ProofHeap {
        assert!(
            header.addr() % SLICE_SIZE == 0,
            "the model's page is not slice aligned"
        );
        let mem = HeapModel {
            header,
            words,
            present: initial,
        };
        let mut h = Heap::new(mem);
        h.set_grow_policy(GrowPolicy {
            min_grow: 1,
            max_grow: 1,
            step_divisor: 8,
        });
        h
    }

    /// A pointer with the address of the block at `addr`, as the program would hold it.
    /// `dealloc` and an in-place `realloc` read only its address.
    fn block_ptr(addr: usize) -> NonNull<u8> {
        NonNull::new(ptr::without_provenance_mut(addr)).unwrap()
    }

    /// Word `w` of the block at `addr`.
    fn word(h: &ProofHeap, addr: usize, w: usize) -> usize {
        // SAFETY: the harness only asks for words of blocks the model stores in full.
        unsafe { h.mem.ptr(addr + w * WORD).cast::<usize>().read() }
    }

    /// The invariants after one operation: the two queues that can hold pages are valid, every
    /// other queue is empty (one symbolic index stands for all of them), one symbolic direct
    /// entry is consistent, and the blocks in use are exactly the harness's `live` blocks.
    fn check(h: &ProofHeap, live: usize) {
        assert!(h.validate_queue(BIN as usize).is_ok());
        assert!(h.validate_queue(FULL_QUEUE).is_ok());
        check_rest(h, live);
    }

    /// [`check`] for harnesses in which no page can reach the full queue (only `find_page`
    /// moves pages there): asserting the queue is empty is much cheaper than walking it.
    fn check_no_full(h: &ProofHeap, live: usize) {
        assert!(h.validate_queue(BIN as usize).is_ok());
        assert!(h.queues[FULL_QUEUE].first == 0 && h.queues[FULL_QUEUE].count == 0);
        check_rest(h, live);
    }

    /// The cheap part of [`check`] for a heap whose only page, if any, is `page`: the full
    /// queue is empty, every other queue is empty, the page (when present) is in no queue link
    /// but its own queue's head, and the live count matches. Used where a queue walk with
    /// `page::validate` would cost more memory than the operation under proof. The direct
    /// table is not checked here: [`BIN`] has no direct entries, so nothing in these harnesses
    /// can change it, and the queue harnesses over [`QBIN`] cover its maintenance.
    fn check_shape(h: &ProofHeap, page: *mut Page, live: usize) {
        assert!(h.queues[FULL_QUEUE].first == 0 && h.queues[FULL_QUEUE].count == 0);
        if !page.is_null() {
            // SAFETY: the page is live.
            unsafe {
                assert!((*page).next == 0 && (*page).prev == 0 && (*page).flags == 0);
                assert!((*page).used as usize == live);
            }
        }
        assert!(used_in(h, BIN as usize) == live);
        let other: usize = kani::any();
        kani::assume(other < QUEUE_COUNT && other != BIN as usize && other != FULL_QUEUE);
        assert!(h.queues[other].first == 0 && h.queues[other].count == 0);
    }

    fn check_rest(h: &ProofHeap, live: usize) {
        let i: usize = kani::any();
        kani::assume(i < DIRECT_ENTRIES);
        assert!(h.validate_direct_entry(i).is_ok());
        let other: usize = kani::any();
        kani::assume(other < QUEUE_COUNT && other != BIN as usize && other != FULL_QUEUE);
        assert!(h.queues[other].first == 0 && h.queues[other].count == 0);
        assert!(used_in(h, BIN as usize) + used_in(h, FULL_QUEUE) == live);
    }

    /// Blocks in use over the pages of queue `q`.
    fn used_in(h: &ProofHeap, q: usize) -> usize {
        let mut used = 0;
        let mut cur = h.queues[q].first;
        while cur != 0 {
            let page = h.page_at(cur);
            // SAFETY: queue members are live pages (invariant 1, validated by the caller).
            unsafe {
                used += (*page).used as usize;
                cur = (*page).next;
            }
        }
        used
    }

    /// The page holding the block returned as `p`, checked to be the model's page serving
    /// [`BIN`]; returns the block's address.
    fn block_of(h: &ProofHeap, p: NonNull<u8>) -> usize {
        let addr = h.mem.addr_of(p.as_ptr());
        assert!(addr % WORD == 0);
        let page = h.page_at(header_of(PageKind::Small, addr));
        assert!(page == h.mem.header);
        // SAFETY: the page is live.
        unsafe {
            assert!((*page).bin == BIN);
            assert!(page::kind(page) == PageKind::Small);
        }
        addr
    }

    /// The page as `fresh_page` leaves it (initialised, at the front of its queue, one block
    /// linked) on a heap whose memory has just grown, so the alloc slow path itself, proved
    /// separately, is not part of every harness.
    fn prepared_page(h: &mut ProofHeap) -> *mut Page {
        h.ensure_init();
        let s = h.mem.grow(1).unwrap();
        h.slices.add_free(s, 1, true);
        let run = h.slices.alloc(1, 1).unwrap();
        let addr = h.mem.page_addr();
        assert!(run.start * SLICE_SIZE == addr && run.zeroed);
        // The page address is passed as the model's own value rather than as the run's
        // arithmetic: the two are equal (asserted), but CBMC cannot simplify the run's
        // multiplication and division back to the object's address, and a symbolic page
        // address makes every header field symbolic.
        // SAFETY: the run is the model's one slice, owned by nothing else.
        let page = unsafe { page::init(&h.mem, addr, PageKind::Small, BIN, true) };
        // SAFETY: a fresh page is in no queue and has unextended blocks.
        unsafe {
            h.push_front(BIN as usize, page);
            assert!(page::extend(page, &h.mem));
        }
        page
    }

    /// Hand out the next block as `alloc_generic` would on this page: extend when the list is
    /// empty, then pop. Returns the block's address.
    fn take(h: &ProofHeap, page: *mut Page) -> usize {
        // SAFETY: `page` is a live page of the heap.
        unsafe {
            if !page::has_free(page) {
                assert!(page::extend(page, &h.mem));
            }
            page::pop(page, &h.mem).unwrap()
        }
    }

    /// Address of block `k` of the model's page.
    fn block(h: &ProofHeap, k: usize) -> usize {
        h.mem.page_addr() + BLOCK_START + k * BS
    }

    /// The first allocation on an empty heap, with or without a linker gap and zeroed or not,
    /// builds a valid page and heap state and hands out block 0. This is the one harness that
    /// runs the alloc slow path (page supply, memory growth, page initialisation) end to end;
    /// the others prepare the page directly, because every copy of that path costs about a
    /// gigabyte of solver memory.
    #[kani::proof]
    #[kani::unwind(2)]
    fn first_allocation_builds_a_valid_heap() {
        let initial: bool = kani::any();
        let zero: bool = kani::any();
        // The model stores one word per block; a zeroed allocation from a dirty page would clear
        // the whole block (an in-bounds write by `bin_size(bin) >= layout.size()`, proved in
        // `bins`), so that combination is left out.
        kani::assume(!(zero && initial));
        proof_heap!(h, initial);
        let layout = Layout::from_size_align(BS, WORD).unwrap();
        // SAFETY: non-zero size.
        let got = unsafe {
            if zero {
                h.alloc_zeroed(layout)
            } else {
                h.alloc(layout)
            }
        };
        let addr = block_of(&h, got.unwrap());
        assert!(addr == block(&h, 0));
        if zero {
            assert!(word(&h, addr, 0) == 0);
        }
        let page = h.mem.header;
        // SAFETY: the page is live.
        unsafe {
            assert!((*page).free_is_zero == !initial);
            assert!((*page).used == 1 && (*page).capacity == 1);
            assert!((*page).retire_expire == 0 && (*page).flags == 0);
        }
        assert!(h.mem.size_slices() == h.mem.page_addr() / SLICE_SIZE + 1);
        assert!(h.free_slices() == 0);
        assert!(h.queues[BIN as usize].first == page.addr());
        check(&h, 1);
    }

    /// Freeing a page's last block retires it: the queue's only page keeps its slice, its free
    /// list holds the block, and the retired range covers its bin.
    #[kani::proof]
    #[kani::unwind(2)]
    fn freeing_the_last_block_retires_the_page() {
        proof_heap!(h, false);
        let page = prepared_page(&mut h);
        let layout = Layout::from_size_align(BS, WORD).unwrap();
        let addr = take(&h, page);
        // SAFETY: `addr` is the live block for `layout`.
        unsafe { h.dealloc(block_ptr(addr), layout) };
        // SAFETY: the page is still live: emptied pages are retired, not freed.
        unsafe {
            assert!((*page).used == 0 && (*page).retire_expire == RETIRE_CYCLES);
            assert!(!(*page).free_is_zero && (*page).free == addr);
        }
        assert!(h.queues[BIN as usize].count == 1);
        assert!(!h.slices.is_free(h.mem.page_addr() / SLICE_SIZE));
        assert!(h.retired_min == BIN as usize && h.retired_max == BIN as usize);
        check_no_full(&h, 0);
    }

    /// Retire a page: prepare it, hand out one block and put it back as `dealloc` does on a
    /// page that just emptied (`page::push`, then `retire`; `dealloc` itself, with the
    /// transition test in between, is proved in `freeing_the_last_block_retires_the_page` and
    /// costs too much solver memory to repeat in the collection harnesses).
    fn retired_page(h: &mut ProofHeap) -> *mut Page {
        let page = prepared_page(h);
        let addr = take(h, page);
        // SAFETY: `addr` is a live block of `page`, a live member of its bin queue.
        unsafe {
            page::push(page, &h.mem, addr);
            h.retire(page);
            assert!((*page).retire_expire == RETIRE_CYCLES && (*page).used == 0);
        }
        assert!(h.retired_min == BIN as usize && h.retired_max == BIN as usize);
        page
    }

    /// An unforced collection only ages a retired page: it stays in its queue with its slice.
    #[kani::proof]
    #[kani::unwind(2)]
    fn an_unforced_collection_ages_a_retired_page() {
        proof_heap!(h, false);
        let page = retired_page(&mut h);
        // SAFETY: invariants hold between operations.
        unsafe { h.collect_retired(false) };
        // SAFETY: the page is still live.
        unsafe { assert!((*page).retire_expire == RETIRE_CYCLES - 1 && (*page).used == 0) };
        let q = h.queues[BIN as usize];
        assert!(q.first == page.addr() && q.last == page.addr() && q.count == 1);
        assert!(!h.slices.is_free(h.mem.page_addr() / SLICE_SIZE));
        assert!(h.retired_min == BIN as usize && h.retired_max == BIN as usize);
        // The collection touched nothing but `retire_expire`, so the page's own validity is
        // the retire harness's; the queue shape and the direct table are asserted directly,
        // which keeps this harness a gigabyte under the memory cap.
        check_shape(&h, page, 0);
    }

    /// A forced collection releases a retired page: the slice is free, the queue and the
    /// retired range are empty, and the direct table is back to the sentinel.
    #[kani::proof]
    #[kani::unwind(2)]
    fn a_forced_collection_frees_a_retired_page() {
        proof_heap!(h, false);
        retired_page(&mut h);
        // SAFETY: invariants hold between operations.
        unsafe { h.collect_retired(true) };
        let q = h.queues[BIN as usize];
        assert!(q.first == 0 && q.last == 0 && q.count == 0);
        assert!(h.slices.is_free(h.mem.page_addr() / SLICE_SIZE));
        assert!(h.retired_min > h.retired_max);
        check_shape(&h, ptr::null_mut(), 0);
    }

    /// On a page with one to three live blocks, freeing any one of them keeps the invariants:
    /// the block heads the free list, the live count matches, and the page is retired exactly
    /// when it emptied.
    #[kani::proof]
    #[kani::unwind(2)]
    fn freeing_any_live_block_preserves_invariants() {
        proof_heap!(h, false);
        let page = prepared_page(&mut h);
        let layout = Layout::from_size_align(BS, WORD).unwrap();
        let k: usize = kani::any();
        kani::assume(k >= 1 && k <= 3);
        // Straight-line rather than a loop, to keep the unwind bound at two.
        assert!(take(&h, page) == block(&h, 0));
        if k >= 2 {
            assert!(take(&h, page) == block(&h, 1));
        }
        if k >= 3 {
            assert!(take(&h, page) == block(&h, 2));
        }
        let i: usize = kani::any();
        kani::assume(i < k);
        // SAFETY: block `i` is live for `layout`.
        unsafe { h.dealloc(block_ptr(block(&h, i)), layout) };
        // SAFETY: the page stays live (retired at most).
        unsafe {
            assert!((*page).used as usize == k - 1);
            assert!((*page).free == block(&h, i) && !(*page).free_is_zero);
            assert!(((*page).retire_expire != 0) == (k == 1));
        }
        assert!(h.queues[BIN as usize].count == 1);
        check_no_full(&h, k - 1);
    }

    /// The model's page with all seven blocks out.
    fn full_page(h: &mut ProofHeap) -> *mut Page {
        let page = prepared_page(h);
        assert!(take(h, page) == block(h, 0));
        assert!(take(h, page) == block(h, 1));
        assert!(take(h, page) == block(h, 2));
        assert!(take(h, page) == block(h, 3));
        assert!(take(h, page) == block(h, 4));
        assert!(take(h, page) == block(h, 5));
        assert!(take(h, page) == block(h, 6));
        // SAFETY: the page is live.
        unsafe {
            assert!(page::is_full(page) && !page::is_expandable(page) && !page::has_free(page));
            assert!(!page::in_full_queue(page) && (*page).used as usize == BLOCKS);
        }
        assert!(h.queues[BIN as usize].first == page.addr());
        page
    }

    /// A page whose blocks are all out is moved to the full queue by the next search for a
    /// block, which then fails to find memory for another page and leaves the heap valid.
    #[kani::proof]
    #[kani::unwind(2)]
    fn the_search_parks_a_full_page_in_the_full_queue() {
        proof_heap!(h, false);
        let page = full_page(&mut h);
        // SAFETY: invariants hold between operations.
        let none = unsafe { h.find_page(BIN) };
        assert!(none.is_none(), "no memory for a second page");
        // SAFETY: the page is live.
        unsafe { assert!(page::in_full_queue(page)) };
        assert!(h.queues[BIN as usize].count == 0 && h.queues[BIN as usize].first == 0);
        let f = h.queues[FULL_QUEUE];
        assert!(f.first == page.addr() && f.last == page.addr() && f.count == 1);
        assert!(!h.slices.is_free(h.mem.page_addr() / SLICE_SIZE));
        assert!(h.validate_queue(FULL_QUEUE).is_ok());
        assert!(used_in(&h, FULL_QUEUE) == BLOCKS);
    }

    /// Freeing any block of a page in the full queue brings the page back to its bin queue
    /// with that block as its only free one, and the next request pops it.
    #[kani::proof]
    #[kani::unwind(2)]
    fn a_free_brings_a_full_page_back_to_its_queue() {
        proof_heap!(h, false);
        let page = full_page(&mut h);
        let layout = Layout::from_size_align(BS, WORD).unwrap();
        // What `find_page` does with a page it finds full (proved above).
        // SAFETY: the page is a live member of its bin queue with no room.
        unsafe { h.move_to_full(BIN as usize, page) };
        assert!(h.queues[FULL_QUEUE].first == page.addr());

        let j: usize = kani::any();
        kani::assume(j < BLOCKS);
        // SAFETY: block `j` is live for `layout`.
        unsafe { h.dealloc(block_ptr(block(&h, j)), layout) };
        // SAFETY: the page is live.
        unsafe {
            assert!(!page::in_full_queue(page) && page::has_free(page));
            assert!((*page).free == block(&h, j) && (*page).used as usize == BLOCKS - 1);
            assert!((*page).retire_expire == 0 && (*page).flags == 0);
            assert!((*page).next == 0 && (*page).prev == 0);
            // The freed block is the whole list.
            assert!(word(&h, block(&h, j), 0) == 0);
        }
        // With one page the queue shape is fully determined; the walk that `check` would do
        // adds nothing here and its list walk over a symbolic block is what breaks the memory
        // budget, so the queues are asserted directly.
        let q = h.queues[BIN as usize];
        assert!(q.first == page.addr() && q.last == page.addr() && q.count == 1);
        let f = h.queues[FULL_QUEUE];
        assert!(f.first == 0 && f.last == 0 && f.count == 0);
        // The next allocation pops the freed block: the page is the queue's first, so the
        // search's candidate, and its list holds exactly that block.
        assert!(take(&h, page) == block(&h, j));
        // SAFETY: the page is live.
        unsafe { assert!((*page).used as usize == BLOCKS && !page::has_free(page)) };
    }

    // --------------------------------------------------------------------------------------
    // Structure: queue operations and the direct table over several pages
    // --------------------------------------------------------------------------------------

    /// Bin 16 (256-byte blocks) serves direct entries 29 to 32, so the direct-table update runs
    /// its loop over four entries; three pages suffice for every link shape.
    const QBIN: u8 = 16;
    const QPAGES: usize = 3;

    const _: () = assert!(direct_range(QBIN).0 == 29 && direct_range(QBIN).1 == 32);

    /// A memory of [`QPAGES`] page headers, each its own object (so each address is 64 KiB
    /// aligned) and nothing else: the queue operations touch only headers.
    struct QueueModel {
        headers: [*mut Page; QPAGES],
    }

    // SAFETY: proof-only backend. Each `headers[k]` points at a `Page` owned by the harness
    // frame; `ptr` yields exactly that pointer for that page's address and nothing for any other
    // address, so every pointer it returns is valid for a header access.
    unsafe impl Memory for QueueModel {
        fn heap_base(&self) -> usize {
            self.headers[0].addr()
        }

        fn size_slices(&self) -> usize {
            self.headers[0].addr() / SLICE_SIZE + 1
        }

        fn grow(&mut self, _slices: usize) -> Option<usize> {
            None
        }

        fn ptr(&self, addr: usize) -> *mut u8 {
            if addr == self.headers[0].addr() {
                self.headers[0].cast()
            } else if addr == self.headers[1].addr() {
                self.headers[1].cast()
            } else if addr == self.headers[2].addr() {
                self.headers[2].cast()
            } else {
                panic!("access to something other than a page header")
            }
        }
    }

    type QueueHeap = Heap<QueueModel, 2>;

    /// Where the harness believes a page is.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Place {
        Out,
        Bin,
        Full,
    }

    /// Put page `k` into the state of a page whose blocks are all out (no free block, nothing
    /// left to extend) or back into the fresh state; both satisfy the page invariants.
    fn set_full(h: &QueueHeap, k: usize, full: bool) {
        let page = h.mem.headers[k];
        // SAFETY: the header was written by `init`.
        unsafe {
            let count = if full { (*page).reserved } else { 0 };
            (*page).used = count;
            (*page).capacity = count;
            (*page).free = 0;
        }
    }

    /// The queue invariants plus the harness's own view: counts per queue, and one symbolic
    /// page's membership and flag.
    fn check_queues(h: &QueueHeap, place: &[Place; QPAGES]) {
        assert!(h.validate_queue_links(QBIN as usize).is_ok());
        assert!(h.validate_queue_links(FULL_QUEUE).is_ok());
        let i: usize = kani::any();
        kani::assume(i < DIRECT_ENTRIES);
        assert!(h.validate_direct_entry(i).is_ok());
        let other: usize = kani::any();
        kani::assume(other < QUEUE_COUNT && other != QBIN as usize && other != FULL_QUEUE);
        assert!(h.queues[other].first == 0 && h.queues[other].count == 0);
        let mut in_bin = 0;
        let mut in_full = 0;
        for &p in place {
            in_bin += (p == Place::Bin) as usize;
            in_full += (p == Place::Full) as usize;
        }
        assert!(h.queues[QBIN as usize].count == in_bin);
        assert!(h.queues[FULL_QUEUE].count == in_full);
        let k: usize = kani::any();
        kani::assume(k < QPAGES);
        let page = h.mem.headers[k];
        // SAFETY: the header was written by `init`.
        unsafe {
            assert!(page::in_full_queue(page) == (place[k] == Place::Full));
            let linked = (*page).next != 0
                || (*page).prev != 0
                || h.queues[QBIN as usize].first == page.addr()
                || h.queues[FULL_QUEUE].first == page.addr();
            assert!(linked == (place[k] != Place::Out));
        }
    }

    /// `STEPS` symbolic queue operations on three pages of [`QBIN`], each drawn from the
    /// operations whose preconditions the current state satisfies, preserve the queue and
    /// direct-table invariants. Pages start out of every queue and change between "has room"
    /// and "all blocks out" as an operation of their own.
    fn queue_ops_preserve_invariants<const STEPS: usize>() {
        let mut h0 = EMPTY_PAGE;
        let mut h1 = EMPTY_PAGE;
        let mut h2 = EMPTY_PAGE;
        let mem = QueueModel {
            headers: [&raw mut h0, &raw mut h1, &raw mut h2],
        };
        let mut h: QueueHeap = Heap::new(mem);
        let mut k = 0;
        while k < QPAGES {
            let addr = h.mem.headers[k].addr();
            assert!(addr % SLICE_SIZE == 0);
            // SAFETY: the address maps to a header object nothing else uses.
            unsafe { page::init(&h.mem, addr, PageKind::Small, QBIN, true) };
            k += 1;
        }
        let mut place = [Place::Out; QPAGES];
        let mut full = [false; QPAGES];
        check_queues(&h, &place);
        for _ in 0..STEPS {
            let k: usize = kani::any();
            kani::assume(k < QPAGES);
            let page = h.mem.headers[k];
            // SAFETY: every operation's precondition is assumed just before it: the page is a
            // valid header, and it is in exactly the queue the operation expects.
            unsafe {
                match kani::any::<u8>() % 7 {
                    0 => {
                        kani::assume(place[k] == Place::Out);
                        h.push_front(QBIN as usize, page);
                        place[k] = Place::Bin;
                        assert!(h.queues[QBIN as usize].first == page.addr());
                    }
                    1 => {
                        kani::assume(place[k] == Place::Out);
                        h.push_back(QBIN as usize, page);
                        place[k] = Place::Bin;
                        assert!(h.queues[QBIN as usize].last == page.addr());
                    }
                    2 => {
                        kani::assume(place[k] == Place::Bin);
                        h.remove(QBIN as usize, page);
                        place[k] = Place::Out;
                    }
                    3 => {
                        kani::assume(place[k] == Place::Bin);
                        h.move_to_front(QBIN as usize, page);
                        assert!(h.queues[QBIN as usize].first == page.addr());
                    }
                    4 => {
                        kani::assume(place[k] == Place::Bin && full[k]);
                        h.move_to_full(QBIN as usize, page);
                        place[k] = Place::Full;
                        assert!(h.queues[FULL_QUEUE].last == page.addr());
                    }
                    5 => {
                        kani::assume(place[k] == Place::Full);
                        h.unfull(page);
                        place[k] = Place::Bin;
                        assert!(h.queues[QBIN as usize].last == page.addr());
                    }
                    _ => {
                        // A page in the full queue must stay roomless (invariant 1).
                        kani::assume(place[k] != Place::Full);
                        full[k] = !full[k];
                        set_full(&h, k, full[k]);
                    }
                }
            }
            check_queues(&h, &place);
        }
    }

    #[kani::proof]
    #[kani::unwind(5)]
    fn two_queue_operations_preserve_the_queues_and_the_direct_table() {
        queue_ops_preserve_invariants::<2>();
    }

    #[kani::proof]
    #[kani::unwind(5)]
    fn three_queue_operations_preserve_the_queues_and_the_direct_table() {
        queue_ops_preserve_invariants::<3>();
    }
}
