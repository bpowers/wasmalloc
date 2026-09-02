//! The in-band page header and the block free list it owns.
//!
//! A *page* is a naturally aligned run of slices (64 KiB, 512 KiB or 4 MiB, see [`PageKind`])
//! carved into equal `block_size` blocks. The header occupies the first
//! [`PAGE_HEADER_RESERVE`] bytes, blocks start at `block_start`, and because a page is aligned
//! to its own size, [`header_of`] finds the header from any block address with one mask. That
//! mask is the reason pages are aligned at all; it replaces mimalloc's page map on every hot
//! path.
//!
//! Free blocks form an intrusive LIFO list threaded through their first `usize`. Blocks are
//! made available lazily (mimalloc's `capacity`/`reserved` split): a fresh page has an empty
//! free list, and [`extend`] links the next few never-used blocks onto it only when the list
//! runs dry, so blocks that are never needed are never written.
//!
//! # Page invariants
//!
//! For a page at address `p` of kind `k` whose header `h` was written by [`init`] and modified
//! only through this module (the heap owns `next`, `prev`, `flags` and `retire_expire`, which
//! the free-list operations never read), the following hold between calls:
//!
//! 1. `p` is a multiple of `k.page_size()`, `p + k.page_size()` does not overflow, and the page
//!    is memory the allocator owns, so [`Memory::ptr`] is valid for every address in it.
//! 2. Geometry: `h.block_size == bin_size(h.bin)`, `h.block_start == block_start(h.block_size)`,
//!    `h.reserved == blocks_per_page(k, h.block_size)` and `k == kind_of_bin(h.bin)`. Block `i`
//!    (`i < h.reserved`) starts at `p + h.block_start + i * h.block_size`; the whole block area
//!    lies inside the page (proved for every bin by `bins`).
//! 3. `h.used <= h.capacity <= h.reserved`.
//! 4. The free list starting at `h.free` and following first-word links visits exactly
//!    `h.capacity - h.used` distinct blocks, all with index below `h.capacity`, and ends at 0.
//!    Every other block with index below `h.capacity` is *live*: handed out by [`pop`] and not
//!    yet returned by [`push`]. Blocks with index at or above `h.capacity` have not been
//!    touched since [`init`] (a reused page may still hold stale data there).
//! 5. If `h.free_is_zero`, every block on the free list is zero except its first word and every
//!    block at or above `h.capacity` is entirely zero. [`push`] clears the flag because a
//!    returned block has arbitrary contents; [`extend`] preserves it because it writes only link
//!    words.
//!
//! `validate` (test and proof builds only) checks 2 to 4; the tests also check 5.
//!
//! # Memory access
//!
//! Header fields are read and written through the raw `*mut Page` as `(*page).field`, never
//! through a `&Page` or `&mut Page`. No reference to the header ever exists, so there is no
//! borrow that a write to block memory (through a pointer of the same provenance, obtained
//! from [`Memory::ptr`]) could invalidate, and nothing relies on aliasing rules beyond "header
//! bytes and block bytes are disjoint". This is clean under both Stacked Borrows and Tree
//! Borrows, and it matches what the fast paths want from the compiler: plain loads and stores
//! at constant offsets from one pointer.
//!
//! Block memory is addressed as `usize` and becomes a pointer only inside [`Memory::ptr`] at the
//! moment of access, so provenance always derives from the backend's base pointer and Miri can
//! follow it under strict provenance.

use crate::backend::Memory;
use crate::bins::{self, MAX_BIN, PAGE_HEADER_RESERVE, PageKind, WORD};
use core::mem::{align_of, offset_of, size_of};

/// Upper bound on the bytes worth of blocks one [`extend`] call links (mimalloc's
/// `MI_MAX_EXTEND_SIZE`): touching more fresh memory than this per extension did not pay off in
/// mimalloc's benchmarks and costs footprint.
pub const MAX_EXTEND_SIZE: usize = 8 * 1024;

/// `flags` bit: the heap has moved this page to its full queue.
pub const FLAG_IN_FULL_QUEUE: u8 = 1;

const KIND_SMALL: u8 = 0;
const KIND_MEDIUM: u8 = 1;
const KIND_LARGE: u8 = 2;

/// The header at the start of every small, medium and large page.
///
/// Fields are public because the heap manipulates the queue links and bookkeeping directly;
/// the free-list fields (`free`, `used`, `capacity`, `free_is_zero`) must only change through
/// [`pop`], [`push`] and [`extend`] so the invariants in the module documentation hold.
/// `reserved`, `block_start`, `block_size`, `bin` and `kind` are constant after [`init`].
///
/// The hot fields come first so that [`pop`] and [`push`] touch only the first 32 bytes.
///
/// The block counters are 32 bits wide although no page holds more than 8188 blocks: `used` is
/// stored by every free and loaded by the next allocation, and a 16-bit store followed by a
/// 16-bit load of the same word is a slow store-to-load forward on current x86 cores (about
/// 2 ns per alloc+free pair on Zen 5, see `docs/research/roofline.md` section 12.1), while
/// 32-bit accesses forward at no cost. `capacity` and `reserved` follow so that the three
/// counts compare and subtract without conversions; the header still fits in 36 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Page {
    /// Address of the first free block, 0 when the list is empty. Each free block stores the
    /// address of the next one in its first `usize`.
    pub free: usize,
    /// Blocks handed out and not yet freed.
    pub used: u32,
    /// Blocks whose free-list links have been initialised (mimalloc's `capacity`).
    pub capacity: u32,
    /// Blocks the page holds in total: `bins::blocks_per_page`.
    pub reserved: u32,
    /// Block size in bytes: `bins::bin_size(bin)`.
    pub block_size: u32,
    /// Offset of block 0 from the page start: `bins::block_start`.
    pub block_start: u16,
    /// Every free-list block is zero except its first word and every block at or above
    /// `capacity` is entirely zero. Set from the slice bitmap at `init`, cleared by `push`.
    pub free_is_zero: bool,
    /// The bin whose blocks this page serves.
    pub bin: u8,
    /// [`PageKind`] discriminant, see [`kind`].
    pub kind: u8,
    /// Heap-owned flag bits, see [`FLAG_IN_FULL_QUEUE`].
    pub flags: u8,
    /// Heap-owned retirement countdown (mimalloc's `retire_expire`).
    pub retire_expire: u8,
    /// Heap-owned bin-queue link: the next page's address, 0 for none.
    pub next: usize,
    /// Heap-owned bin-queue link: the previous page's address, 0 for none.
    pub prev: usize,
}

const _: () = {
    assert!(size_of::<Page>() <= PAGE_HEADER_RESERVE);
    // Pages are 64 KiB aligned, so any alignment up to a word is satisfied trivially.
    assert!(align_of::<Page>() <= WORD);
    // The two fast paths must stay within the first 32 bytes on every target. On wasm32 the
    // offsets are: free 0, used 4, capacity 8, reserved 12, block_size 16, block_start 20,
    // free_is_zero 22, bin 23, kind 24, flags 25, retire_expire 26, next 28, prev 32.
    assert!(offset_of!(Page, free) + size_of::<usize>() <= 32);
    assert!(offset_of!(Page, used) + 4 <= 32);
    assert!(offset_of!(Page, capacity) + 4 <= 32);
    assert!(offset_of!(Page, reserved) + 4 <= 32);
    assert!(offset_of!(Page, block_size) + 4 <= 32);
    assert!(offset_of!(Page, block_start) + 2 <= 32);
    assert!(offset_of!(Page, free_is_zero) < 32);
    assert!(offset_of!(Page, flags) < 32);
    // A misaligned counter would defeat the point of widening it.
    assert!(offset_of!(Page, used) % 4 == 0);
};

/// The page header address for a block address: the mask trick. Pure; only the kind of the
/// page (a function of the block's `Layout`) is needed, never a page map.
#[inline(always)]
pub const fn header_of(kind: PageKind, block_addr: usize) -> usize {
    block_addr & kind.page_mask()
}

/// Byte stored in `Page::kind` for a [`PageKind`].
#[inline]
pub const fn kind_to_u8(kind: PageKind) -> u8 {
    match kind {
        PageKind::Small => KIND_SMALL,
        PageKind::Medium => KIND_MEDIUM,
        PageKind::Large => KIND_LARGE,
    }
}

/// Inverse of [`kind_to_u8`]; `None` for a byte that is not a kind (a corrupt header).
#[inline]
pub const fn kind_from_u8(byte: u8) -> Option<PageKind> {
    match byte {
        KIND_SMALL => Some(PageKind::Small),
        KIND_MEDIUM => Some(PageKind::Medium),
        KIND_LARGE => Some(PageKind::Large),
        _ => None,
    }
}

/// Write a fresh header for a page of `kind` serving `bin` at `page_addr`, and return it.
///
/// Every field is written explicitly because the memory may hold a previous page's header
/// when `zeroed` is false. `zeroed` says whether the whole page is known to be zero (fresh
/// from `memory.grow`, or still marked zero in the slice bitmap) and seeds `free_is_zero`.
///
/// # Safety
///
/// `page_addr` must be a multiple of `kind.page_size()`, `page_addr + kind.page_size()` must
/// not overflow, and the `kind.page_size()` bytes there must be owned by the allocator through
/// `mem` and referenced by nothing else (no live blocks, no other header). `kind` must be
/// `bins::kind_of_bin(bin)` and `bin` must be in `1..=MAX_BIN`.
#[cold]
#[inline(never)]
pub unsafe fn init<M: Memory>(
    mem: &M,
    page_addr: usize,
    kind: PageKind,
    bin: u8,
    zeroed: bool,
) -> *mut Page {
    debug_assert!((1..=MAX_BIN).contains(&bin));
    debug_assert!(bins::kind_of_bin(bin) == kind);
    debug_assert!(page_addr % kind.page_size() == 0);
    debug_assert!(page_addr.checked_add(kind.page_size()).is_some());
    let block_size = bins::bin_size(bin);
    let block_start = bins::block_start(block_size);
    let reserved = bins::blocks_per_page(kind, block_size);
    debug_assert!(reserved <= u32::MAX as usize);
    debug_assert!(block_start <= u16::MAX as usize);
    let page = mem.ptr(page_addr).cast::<Page>();
    let header = Page {
        free: 0,
        used: 0,
        capacity: 0,
        reserved: reserved as u32,
        block_size: block_size as u32,
        block_start: block_start as u16,
        free_is_zero: zeroed,
        bin,
        kind: kind_to_u8(kind),
        flags: 0,
        retire_expire: 0,
        next: 0,
        prev: 0,
    };
    // SAFETY: the caller owns the page through `mem`, so `mem.ptr(page_addr)` is valid for
    // `page_size() >= PAGE_HEADER_RESERVE >= size_of::<Page>()` bytes of writes, and
    // `page_addr` is 64 KiB aligned, far beyond `align_of::<Page>()`. Writing the whole struct
    // creates no reference and reads nothing, so dirty memory is fine.
    unsafe { page.write(header) };
    page
}

/// Pop a block from the free list: the allocation fast path. `None` when the list is empty
/// (the caller then calls [`extend`] and retries, or finds another page).
///
/// Three loads, one compare, two stores. The block's first word still holds its free-list
/// link; the block is otherwise untouched.
///
/// # Safety
///
/// `page` must point to a header written by [`init`] inside memory owned by the allocator
/// through `mem`, and the page invariants in the module documentation must hold.
#[inline(always)]
pub unsafe fn pop<M: Memory>(page: *mut Page, mem: &M) -> Option<usize> {
    // SAFETY: the header is valid for reads and writes by the precondition. `block` is
    // non-zero, so by invariant 4 it is a free block of this page: its first word lies inside
    // the page (invariant 2), so `mem.ptr(block)` is valid for a `usize` read (invariant 1),
    // and it is `WORD`-aligned because `block_start` and every block size are multiples of
    // `WORD`. `used < capacity` when the list is non-empty (invariant 4), so the increment
    // cannot overflow `u32`.
    unsafe {
        let block = (*page).free;
        if block == 0 {
            return None;
        }
        debug_assert!(block_index(page, block).is_some());
        let next = mem.ptr(block).cast::<usize>().read();
        (*page).free = next;
        (*page).used += 1;
        Some(block)
    }
}

/// Push a live block back onto the free list: the deallocation fast path.
///
/// # Safety
///
/// As for [`pop`], and `block` must be a live block of this page: returned by [`pop`] on this
/// page and not pushed since. The block's contents are overwritten from its first word.
#[inline(always)]
pub unsafe fn push<M: Memory>(page: *mut Page, mem: &M, block: usize) {
    // SAFETY: the header is valid by the precondition. `block` is live, so it lies inside the
    // page's block area and is `WORD`-aligned (invariant 2), and `mem.ptr(block)` is valid for a
    // `usize` write (invariant 1). A live block means `used >= 1` (invariant 4), so the
    // decrement cannot underflow.
    unsafe {
        debug_assert!(
            block_index(page, block).is_some(),
            "block is not in the page's handed-out block area or is misaligned"
        );
        debug_assert!((*page).used > 0, "push onto a page with no live blocks");
        // Read `used` before the block store so the compiler need not order the load after a
        // store it cannot prove disjoint (mimalloc does the same in `mi_free_block_local`).
        let used = (*page).used - 1;
        mem.ptr(block).cast::<usize>().write((*page).free);
        (*page).free = block;
        (*page).used = used;
        (*page).free_is_zero = false;
    }
}

/// Link the next batch of never-used blocks onto the free list (mimalloc's lazy
/// `mi_page_extend_free`). Returns false when every block already has a link, that is when
/// `capacity == reserved`.
///
/// At most [`MAX_EXTEND_SIZE`] bytes worth of blocks, and at least one, are linked per call.
/// The new blocks go in front of whatever the list held; only their first words are written,
/// so `free_is_zero` remains accurate.
///
/// # Safety
///
/// As for [`pop`].
#[cold]
#[inline(never)]
pub unsafe fn extend<M: Memory>(page: *mut Page, mem: &M) -> bool {
    // SAFETY: the header is valid by the precondition. The blocks written have indices in
    // `capacity .. capacity + extend <= reserved`, so each first word lies inside the page's
    // block area (invariant 2), is `WORD`-aligned, and is memory `mem.ptr` may write to
    // (invariant 1). Those blocks are untouched by anyone (invariant 4), so overwriting their
    // first words clobbers nothing live. `capacity + extend <= reserved <= u32::MAX`.
    unsafe {
        let capacity = (*page).capacity as usize;
        let reserved = (*page).reserved as usize;
        debug_assert!(capacity <= reserved);
        if capacity >= reserved {
            return false;
        }
        let block_size = (*page).block_size as usize;
        let max_extend = (MAX_EXTEND_SIZE / block_size).max(1);
        let extend = (reserved - capacity).min(max_extend);
        let first = page.addr() + (*page).block_start as usize + capacity * block_size;
        let last = first + (extend - 1) * block_size;
        let mut block = first;
        while block < last {
            let next = block + block_size;
            mem.ptr(block).cast::<usize>().write(next);
            block = next;
        }
        mem.ptr(last).cast::<usize>().write((*page).free);
        (*page).free = first;
        (*page).capacity = (capacity + extend) as u32;
        true
    }
}

/// Every block is handed out: `used == reserved`. Implies the free list is empty and the page
/// cannot be extended.
///
/// # Safety
///
/// `page` must point to a header written by [`init`].
#[inline(always)]
pub unsafe fn is_full(page: *const Page) -> bool {
    // SAFETY: the header is valid for reads by the precondition.
    unsafe { (*page).used == (*page).reserved }
}

/// No block is handed out: `used == 0`.
///
/// # Safety
///
/// `page` must point to a header written by [`init`].
#[inline(always)]
pub unsafe fn all_free(page: *const Page) -> bool {
    // SAFETY: the header is valid for reads by the precondition.
    unsafe { (*page).used == 0 }
}

/// The free list is non-empty, so the next [`pop`] succeeds.
///
/// # Safety
///
/// `page` must point to a header written by [`init`].
#[inline(always)]
pub unsafe fn has_free(page: *const Page) -> bool {
    // SAFETY: the header is valid for reads by the precondition.
    unsafe { (*page).free != 0 }
}

/// Some blocks have no link yet, so [`extend`] would succeed: `capacity < reserved`.
///
/// # Safety
///
/// `page` must point to a header written by [`init`].
#[inline(always)]
pub unsafe fn is_expandable(page: *const Page) -> bool {
    // SAFETY: the header is valid for reads by the precondition.
    unsafe { (*page).capacity < (*page).reserved }
}

/// Whether the heap has parked this page in its full queue.
///
/// # Safety
///
/// `page` must point to a header written by [`init`].
#[inline(always)]
pub unsafe fn in_full_queue(page: *const Page) -> bool {
    // SAFETY: the header is valid for reads by the precondition.
    unsafe { (*page).flags & FLAG_IN_FULL_QUEUE != 0 }
}

/// Record whether the page is in the heap's full queue.
///
/// # Safety
///
/// `page` must point to a header written by [`init`].
#[inline(always)]
pub unsafe fn set_in_full_queue(page: *mut Page, in_full: bool) {
    // SAFETY: the header is valid for reads and writes by the precondition.
    unsafe {
        if in_full {
            (*page).flags |= FLAG_IN_FULL_QUEUE;
        } else {
            (*page).flags &= !FLAG_IN_FULL_QUEUE;
        }
    }
}

/// The page's kind.
///
/// # Safety
///
/// `page` must point to a header written by [`init`].
#[inline(always)]
pub unsafe fn kind(page: *const Page) -> PageKind {
    // SAFETY: the header is valid for reads by the precondition.
    let byte = unsafe { (*page).kind };
    debug_assert!(kind_from_u8(byte).is_some(), "corrupt kind byte");
    match byte {
        KIND_SMALL => PageKind::Small,
        KIND_MEDIUM => PageKind::Medium,
        _ => PageKind::Large,
    }
}

/// Addresses `(start, end)` of the block area: block 0 starts at `start` and block
/// `reserved - 1` ends at `end` (exclusive). `end <= page_addr + page_size`.
///
/// # Safety
///
/// `page` must point to a header written by [`init`].
#[inline]
pub unsafe fn block_area(page: *const Page) -> (usize, usize) {
    // SAFETY: the header is valid for reads by the precondition. The sums stay below
    // `page_addr + page_size`, which `init` requires not to overflow.
    unsafe {
        let start = page.addr() + (*page).block_start as usize;
        let end = start + (*page).reserved as usize * (*page).block_size as usize;
        (start, end)
    }
}

/// Index of `block` in the initialised block area (`0..capacity`), or `None` if it lies
/// outside it or is not on a block boundary. Used by debug assertions and validation.
///
/// # Safety
///
/// `page` must point to a header written by [`init`].
#[inline]
unsafe fn block_index(page: *const Page, block: usize) -> Option<usize> {
    // SAFETY: the header is valid for reads by the precondition.
    let (block_start, block_size, capacity) = unsafe {
        (
            (*page).block_start as usize,
            (*page).block_size as usize,
            (*page).capacity as usize,
        )
    };
    // Offsets rather than absolute addresses so nothing here can overflow.
    let offset = block.wrapping_sub(page.addr()).checked_sub(block_start)?;
    let index = offset / block_size;
    (offset % block_size == 0 && index < capacity).then_some(index)
}

/// Check the page invariants that can be checked from the header and the free list
/// (invariants 2 to 4 of the module documentation). Test and proof infrastructure.
///
/// # Safety
///
/// `page` must point to a header written by [`init`] inside memory owned by the allocator
/// through `mem`. The free list is walked only while its links stay inside the initialised
/// block area, so a corrupt list yields an error rather than an out-of-page read.
#[cfg(any(test, kani))]
pub unsafe fn validate<M: Memory>(page: *mut Page, mem: &M) -> Result<(), &'static str> {
    // SAFETY: the header is valid for reads by the precondition.
    let h = unsafe { page.read() };
    let kind = kind_from_u8(h.kind).ok_or("kind byte is not a PageKind")?;
    if !(1..=MAX_BIN).contains(&h.bin) {
        return Err("bin out of range");
    }
    if bins::kind_of_bin(h.bin) != kind {
        return Err("kind does not match bin");
    }
    let block_size = h.block_size as usize;
    if block_size != bins::bin_size(h.bin) {
        return Err("block_size does not match bin");
    }
    if h.block_start as usize != bins::block_start(block_size) {
        return Err("block_start does not match block_size");
    }
    if h.reserved as usize != bins::blocks_per_page(kind, block_size) {
        return Err("reserved does not match page geometry");
    }
    if page.addr() % kind.page_size() != 0 {
        return Err("page address is not aligned to its kind");
    }
    if h.capacity > h.reserved {
        return Err("capacity exceeds reserved");
    }
    if h.used > h.capacity {
        return Err("used exceeds capacity");
    }
    let expected_len = (h.capacity - h.used) as usize;
    let mut len = 0;
    let mut cur = h.free;
    while cur != 0 {
        // A list with a repeated block cycles forever, so exceeding the expected length also
        // catches duplicates.
        if len == expected_len {
            return Err("free list longer than capacity - used (cycle or lost block)");
        }
        // SAFETY: `block_index` only reads the header; `cur` is then known to be a block
        // boundary below `capacity`, inside the page, so its first word is readable.
        unsafe {
            if block_index(page, cur).is_none() {
                return Err("free link outside the initialised block area or misaligned");
            }
            cur = mem.ptr(cur).cast::<usize>().read();
        }
        len += 1;
    }
    if len != expected_len {
        return Err("used + free list length != capacity");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SimMemory;
    use crate::backend::testing::Region;
    use crate::bins::{
        MAX_NATURAL_ALIGN, MEDIUM_MAX_BIN, SLICE_SIZE, SMALL_MAX_BIN, bin_size, blocks_per_page,
        kind_of_bin,
    };
    use std::vec::Vec;

    const KINDS: [PageKind; 3] = [PageKind::Small, PageKind::Medium, PageKind::Large];

    /// Bins exercised by the per-page tests: the ends of each kind and a spread in between.
    const SAMPLE_BINS: [u8; 18] = [
        1,
        2,
        3,
        7,
        8,
        9,
        16,
        24,
        32,
        36,
        SMALL_MAX_BIN,
        SMALL_MAX_BIN + 1,
        44,
        MEDIUM_MAX_BIN,
        MEDIUM_MAX_BIN + 1,
        53,
        58,
        MAX_BIN,
    ];

    /// Miri is roughly a thousand times slower than native, so the exhaustive loops shrink.
    fn bins_under_test() -> Vec<u8> {
        if cfg!(miri) {
            SAMPLE_BINS.to_vec()
        } else {
            (1..=MAX_BIN).collect()
        }
    }

    /// A naturally aligned page of `kind` obtained through `grow`, as the heap would get it,
    /// so its memory is zero.
    struct Fixture {
        region: Region,
        kind: PageKind,
        page_addr: usize,
    }

    impl Fixture {
        fn new(kind: PageKind) -> Fixture {
            let slices = kind.page_size() / SLICE_SIZE;
            // The region is 4 MiB aligned. Slice 0 is the initial slice, which `from_region`
            // leaves uninitialised, so skip to the next page-aligned slice and grow the page
            // there.
            let mut region = Region::new(2 * slices, 1, 0);
            assert!(region.mem.skip_slices(slices - 1));
            let first = region.mem.grow(slices).unwrap();
            let page_addr = first * SLICE_SIZE;
            assert_eq!(page_addr % kind.page_size(), 0);
            Fixture {
                region,
                kind,
                page_addr,
            }
        }

        fn for_bin(bin: u8) -> Fixture {
            Fixture::new(kind_of_bin(bin))
        }

        fn mem(&self) -> &SimMemory {
            &self.region.mem
        }

        fn init(&self, bin: u8, zeroed: bool) -> *mut Page {
            assert_eq!(kind_of_bin(bin), self.kind);
            // SAFETY: the page is a grown, page-aligned run inside the region and nothing else
            // refers to it.
            unsafe { init(self.mem(), self.page_addr, self.kind, bin, zeroed) }
        }

        /// Fill the whole page with a byte pattern, modelling reuse of a dirty run.
        fn dirty(&self) {
            // SAFETY: the page lies inside the region we own.
            unsafe {
                core::ptr::write_bytes(self.mem().ptr(self.page_addr), 0xAB, self.kind.page_size())
            };
        }
    }

    /// Pop a block, extending the page when the list is empty, as the heap's slow path does.
    fn pop_or_extend(page: *mut Page, mem: &SimMemory) -> Option<usize> {
        // SAFETY: `page` was produced by `Fixture::init` and only this module has touched it.
        unsafe {
            match pop(page, mem) {
                Some(b) => Some(b),
                None => {
                    if extend(page, mem) {
                        let b = pop(page, mem);
                        assert!(b.is_some(), "extend succeeded but the list is still empty");
                        b
                    } else {
                        None
                    }
                }
            }
        }
    }

    fn header(page: *mut Page) -> Page {
        // SAFETY: written by `init`.
        unsafe { page.read() }
    }

    fn check(page: *mut Page, mem: &SimMemory) {
        // SAFETY: written by `init`.
        if let Err(e) = unsafe { validate(page, mem) } {
            panic!("page invariant violated: {e}: {:?}", header(page));
        }
    }

    /// Invariant 5: with `free_is_zero`, free blocks are zero beyond their link word and
    /// never-linked blocks are entirely zero. Word reads keep this fast under Miri.
    fn check_zero(page: *mut Page, mem: &SimMemory) {
        let h = header(page);
        if !h.free_is_zero {
            return;
        }
        let bs = h.block_size as usize;
        let start = page.addr() + h.block_start as usize;
        // SAFETY: all addresses are inside the page's block area, which `grow` zeroed and the
        // tests own.
        unsafe {
            let mut cur = h.free;
            while cur != 0 {
                for off in (WORD..bs).step_by(WORD) {
                    assert_eq!(
                        mem.ptr(cur + off).cast::<usize>().read(),
                        0,
                        "free block dirty"
                    );
                }
                cur = mem.ptr(cur).cast::<usize>().read();
            }
            // Every never-linked block natively; under Miri only the first and the last, since
            // the scan is millions of interpreted reads on a large page.
            let untouched = h.capacity as usize..h.reserved as usize;
            let sampled: Vec<usize> = if cfg!(miri) {
                untouched
                    .clone()
                    .take(1)
                    .chain(untouched.clone().last())
                    .collect()
            } else {
                untouched.collect()
            };
            for i in sampled {
                for off in (0..bs).step_by(WORD) {
                    assert_eq!(
                        mem.ptr(start + i * bs + off).cast::<usize>().read(),
                        0,
                        "never-linked block dirty"
                    );
                }
            }
        }
    }

    fn natural_align(block_size: usize) -> usize {
        (block_size & block_size.wrapping_neg()).min(MAX_NATURAL_ALIGN)
    }

    /// xorshift64: deterministic randomness with no dependencies.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    #[test]
    fn header_fits_the_reserve_with_hot_fields_first() {
        assert!(size_of::<Page>() <= PAGE_HEADER_RESERVE);
        #[cfg(target_pointer_width = "64")]
        assert_eq!(size_of::<Page>(), 48);
        #[cfg(target_pointer_width = "32")]
        assert_eq!(size_of::<Page>(), 36);
        assert_eq!(offset_of!(Page, free), 0);
        assert_eq!(offset_of!(Page, used), size_of::<usize>());
        assert!(offset_of!(Page, free_is_zero) < 32);
        assert!(offset_of!(Page, flags) < 32);
        assert!(offset_of!(Page, block_size) + 4 <= 32);
        for bin in 1..=MAX_BIN {
            assert!(bins::block_start(bin_size(bin)) >= size_of::<Page>());
        }
    }

    #[test]
    fn kind_bytes_round_trip() {
        for k in KINDS {
            assert_eq!(kind_from_u8(kind_to_u8(k)), Some(k));
            let f = Fixture::new(k);
            let bin = match k {
                PageKind::Small => 1,
                PageKind::Medium => SMALL_MAX_BIN + 1,
                PageKind::Large => MAX_BIN,
            };
            let page = f.init(bin, true);
            // SAFETY: written by `init`.
            assert_eq!(unsafe { kind(page) }, k);
        }
        assert_eq!(kind_from_u8(3), None);
        assert_eq!(kind_from_u8(0xAB), None);
    }

    #[test]
    fn init_writes_every_field_on_zeroed_and_dirty_pages() {
        for bin in SAMPLE_BINS {
            let f = Fixture::for_bin(bin);
            for zeroed in [true, false] {
                if !zeroed {
                    f.dirty();
                }
                let page = f.init(bin, zeroed);
                assert_eq!(page.addr(), f.page_addr);
                let h = header(page);
                let bs = bin_size(bin);
                assert_eq!(h.free, 0);
                assert_eq!(h.used, 0);
                assert_eq!(h.capacity, 0);
                assert_eq!(h.reserved as usize, blocks_per_page(f.kind, bs));
                assert_eq!(h.block_start as usize, bins::block_start(bs));
                assert_eq!(h.block_size as usize, bs);
                assert_eq!(h.free_is_zero, zeroed);
                assert_eq!(h.bin, bin);
                assert_eq!(kind_from_u8(h.kind), Some(f.kind));
                assert_eq!(h.flags, 0);
                assert_eq!(h.retire_expire, 0);
                assert_eq!(h.next, 0);
                assert_eq!(h.prev, 0);
                // SAFETY: written by `init`.
                unsafe {
                    assert!(!has_free(page));
                    assert!(all_free(page));
                    assert!(!is_full(page));
                    assert!(is_expandable(page));
                    assert!(!in_full_queue(page));
                    assert_eq!(kind(page), f.kind);
                    assert_eq!(pop(page, f.mem()), None, "fresh page has no linked blocks");
                }
                check(page, f.mem());
                check_zero(page, f.mem());
            }
        }
    }

    #[test]
    fn pop_with_lazy_extension_hands_out_every_block_exactly_once() {
        for bin in bins_under_test() {
            let f = Fixture::for_bin(bin);
            let page = f.init(bin, true);
            let bs = bin_size(bin);
            let reserved = blocks_per_page(f.kind, bs);
            let max_extend = (MAX_EXTEND_SIZE / bs).max(1);
            // SAFETY: written by `init`.
            let (start, end) = unsafe { block_area(page) };
            assert_eq!(start, f.page_addr + bins::block_start(bs));
            assert_eq!(end, start + reserved * bs);
            assert!(end <= f.page_addr + f.kind.page_size());

            let mut seen = std::vec![false; reserved];
            let mut count = 0;
            let mut capacity_before = 0usize;
            while let Some(block) = pop_or_extend(page, f.mem()) {
                let h = header(page);
                if h.capacity as usize != capacity_before {
                    // Each extension links min(remaining, MAX_EXTEND_SIZE / bs) blocks, at least 1.
                    let step = h.capacity as usize - capacity_before;
                    assert_eq!(
                        step,
                        (reserved - capacity_before).min(max_extend),
                        "bin {bin}"
                    );
                    capacity_before = h.capacity as usize;
                }
                assert!(
                    block >= start && block < end,
                    "bin {bin}: block outside area"
                );
                assert_eq!((block - start) % bs, 0, "bin {bin}: block misaligned");
                assert_eq!(block % natural_align(bs), 0, "bin {bin}: natural alignment");
                assert_eq!(block % WORD, 0);
                let index = (block - start) / bs;
                assert!(!seen[index], "bin {bin}: block {index} handed out twice");
                seen[index] = true;
                count += 1;
                assert_eq!(h.used as usize, count);
            }
            assert_eq!(count, reserved, "bin {bin}");
            assert!(seen.iter().all(|&s| s));
            let h = header(page);
            assert_eq!(h.capacity as usize, reserved);
            assert_eq!(h.free, 0);
            // SAFETY: written by `init`.
            unsafe {
                assert!(is_full(page));
                assert!(!is_expandable(page));
                assert!(!has_free(page));
                assert!(!all_free(page));
                assert!(!extend(page, f.mem()));
                assert_eq!(pop(page, f.mem()), None);
            }
            check(page, f.mem());
        }
    }

    #[test]
    fn extend_links_at_most_max_extend_size_and_at_least_one_block() {
        for bin in bins_under_test() {
            let f = Fixture::for_bin(bin);
            let page = f.init(bin, true);
            let bs = bin_size(bin);
            let reserved = blocks_per_page(f.kind, bs);
            // SAFETY: written by `init`.
            assert!(unsafe { extend(page, f.mem()) });
            let h = header(page);
            let expect = (MAX_EXTEND_SIZE / bs).max(1).min(reserved);
            assert_eq!(h.capacity as usize, expect, "bin {bin} ({bs} B)");
            assert!(h.capacity as usize * bs <= MAX_EXTEND_SIZE.max(bs));
            assert_eq!(h.free, f.page_addr + bins::block_start(bs));
            assert_eq!(h.used, 0);
            check(page, f.mem());
            check_zero(page, f.mem());
        }
    }

    #[test]
    fn push_is_lifo_in_sequential_and_random_order() {
        let bin = 4; // 32-byte blocks
        let f = Fixture::for_bin(bin);
        let page = f.init(bin, true);
        let n = 300;
        let blocks: Vec<usize> = (0..n)
            .map(|_| pop_or_extend(page, f.mem()).unwrap())
            .collect();
        check(page, f.mem());

        // SAFETY: every block in `blocks` is live on this page.
        unsafe {
            for &b in &blocks {
                push(page, f.mem(), b);
                check(page, f.mem());
            }
            assert!(all_free(page));
            assert!(!header(page).free_is_zero);
            for &b in blocks.iter().rev() {
                assert_eq!(pop(page, f.mem()), Some(b), "LIFO order");
            }
            assert_eq!(header(page).used as usize, n);

            let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
            let mut order = blocks.clone();
            for i in (1..order.len()).rev() {
                let j = rng.below(i + 1);
                order.swap(i, j);
            }
            for &b in &order {
                push(page, f.mem(), b);
            }
            check(page, f.mem());
            assert!(all_free(page));
            for &b in order.iter().rev() {
                assert_eq!(
                    pop(page, f.mem()),
                    Some(b),
                    "LIFO order after random pushes"
                );
            }
            check(page, f.mem());
            // Pushing everything back leaves the page empty but still fully linked.
            for &b in &order {
                push(page, f.mem(), b);
            }
            let h = header(page);
            assert_eq!(h.used, 0);
            assert_eq!(
                h.capacity as usize,
                n.next_multiple_of(MAX_EXTEND_SIZE / bin_size(bin))
            );
            check(page, f.mem());
        }
    }

    #[test]
    fn random_operation_sequences_preserve_the_invariants() {
        let bins: &[u8] = if cfg!(miri) {
            &[1, 24, 36]
        } else {
            &[
                1,
                5,
                24,
                36,
                SMALL_MAX_BIN + 1,
                44,
                MEDIUM_MAX_BIN + 1,
                MAX_BIN,
            ]
        };
        let steps = if cfg!(miri) { 200 } else { 4000 };
        let mut rng = XorShift(0xD1B5_4A32_D192_ED03);
        for &bin in bins {
            let f = Fixture::for_bin(bin);
            let page = f.init(bin, true);
            let bs = bin_size(bin);
            let reserved = blocks_per_page(f.kind, bs);
            // SAFETY: written by `init`.
            let (start, end) = unsafe { block_area(page) };
            let mut live: Vec<usize> = Vec::new();
            let mut pushed_once = false;
            for step in 0..steps {
                let roll = rng.below(100);
                // SAFETY: `page` is valid; pushed blocks come from `live`, so they are live.
                unsafe {
                    if roll < 45 {
                        match pop(page, f.mem()) {
                            Some(b) => {
                                assert!(b >= start && b < end && (b - start) % bs == 0);
                                assert!(!live.contains(&b), "popped a live block");
                                live.push(b);
                            }
                            None => {
                                if !extend(page, f.mem()) {
                                    assert_eq!(live.len(), reserved, "no blocks left but not full");
                                    let b = live.swap_remove(rng.below(live.len()));
                                    push(page, f.mem(), b);
                                    pushed_once = true;
                                }
                            }
                        }
                    } else if roll < 85 {
                        if !live.is_empty() {
                            let b = live.swap_remove(rng.below(live.len()));
                            push(page, f.mem(), b);
                            pushed_once = true;
                        }
                    } else {
                        let could = is_expandable(page);
                        assert_eq!(extend(page, f.mem()), could);
                    }
                    let h = header(page);
                    assert_eq!(h.used as usize, live.len());
                    assert_eq!(has_free(page), h.free != 0);
                    assert_eq!(is_full(page), live.len() == reserved);
                    assert_eq!(all_free(page), live.is_empty());
                    assert_eq!(is_expandable(page), (h.capacity as usize) < reserved);
                    assert_eq!(h.free_is_zero, !pushed_once);
                }
                check(page, f.mem());
                if bs <= 1024 && (!cfg!(miri) || step % 16 == 0) {
                    check_zero(page, f.mem());
                }
            }
            // Drain and refill once more so the last state is also exercised end to end.
            // SAFETY: as above.
            unsafe {
                for b in live.drain(..) {
                    push(page, f.mem(), b);
                }
                assert!(all_free(page));
                check(page, f.mem());
                let mut count = 0;
                while pop_or_extend(page, f.mem()).is_some() {
                    count += 1;
                }
                assert_eq!(count, reserved);
                assert!(is_full(page));
            }
            check(page, f.mem());
        }
    }

    #[test]
    fn header_of_recovers_the_page_from_every_block_of_every_kind() {
        for bin in bins_under_test() {
            let f = Fixture::for_bin(bin);
            let page = f.init(bin, true);
            let bs = bin_size(bin);
            // SAFETY: written by `init`.
            let k = unsafe { kind(page) };
            assert_eq!(k, f.kind);
            while let Some(block) = pop_or_extend(page, f.mem()) {
                for off in [0, 1, WORD, bs / 2, bs - 1].into_iter().filter(|&o| o < bs) {
                    assert_eq!(
                        header_of(k, block + off),
                        f.page_addr,
                        "bin {bin} offset {off}"
                    );
                }
                assert_eq!(header_of(k, block).cast_signed(), f.page_addr.cast_signed());
            }
            // The mask must not be too coarse: the neighbouring pages map to themselves.
            assert_eq!(header_of(k, f.page_addr - 1), f.page_addr - k.page_size());
            assert_eq!(
                header_of(k, f.page_addr + k.page_size()),
                f.page_addr + k.page_size()
            );
        }
    }

    #[test]
    fn free_is_zero_holds_until_the_first_push() {
        let bin = 12; // 128-byte blocks: several extensions and a cheap zero scan
        let f = Fixture::for_bin(bin);
        let page = f.init(bin, true);
        let bs = bin_size(bin);
        let mut blocks = Vec::new();
        while let Some(block) = pop_or_extend(page, f.mem()) {
            assert!(header(page).free_is_zero);
            // SAFETY: the block is live and inside the page.
            unsafe {
                // alloc_zeroed only needs to clear the link word.
                for off in (WORD..bs).step_by(WORD) {
                    assert_eq!(f.mem().ptr(block + off).cast::<usize>().read(), 0);
                }
                let link = f.mem().ptr(block).cast::<usize>().read();
                assert!(link == 0 || (link >= f.page_addr && link < f.page_addr + SLICE_SIZE));
                f.mem().ptr(block).cast::<usize>().write(0);
                for off in (0..bs).step_by(WORD) {
                    assert_eq!(f.mem().ptr(block + off).cast::<usize>().read(), 0);
                }
            }
            if !cfg!(miri) || blocks.len() % 32 == 0 {
                check_zero(page, f.mem());
            }
            blocks.push(block);
        }
        // SAFETY: `blocks` holds live blocks of this page.
        unsafe {
            let b = blocks.pop().unwrap();
            f.mem().ptr(b + WORD).cast::<usize>().write(0xDEAD_BEEF);
            push(page, f.mem(), b);
            assert!(!header(page).free_is_zero, "a pushed block may be dirty");
            for b in blocks {
                push(page, f.mem(), b);
            }
            assert!(!header(page).free_is_zero, "the flag never comes back");
        }
        check(page, f.mem());

        // A dirty page is never zero, and extend writes only the link words.
        let g = Fixture::for_bin(bin);
        g.dirty();
        let page = g.init(bin, false);
        assert!(!header(page).free_is_zero);
        // SAFETY: written by `init`; the block area is inside the page.
        unsafe {
            assert!(extend(page, g.mem()));
            let h = header(page);
            let mut cur = h.free;
            let mut n = 0;
            while cur != 0 {
                for off in WORD..bs {
                    assert_eq!(
                        g.mem().ptr(cur + off).read(),
                        0xAB,
                        "extend touched a payload byte"
                    );
                }
                cur = g.mem().ptr(cur).cast::<usize>().read();
                n += 1;
            }
            assert_eq!(n, h.capacity as usize);
            let (start, _) = block_area(page);
            for off in 0..bs {
                assert_eq!(
                    g.mem().ptr(start + h.capacity as usize * bs + off).read(),
                    0xAB
                );
            }
        }
        check(page, g.mem());
    }

    #[test]
    fn full_queue_flag_toggles_without_touching_other_fields() {
        let f = Fixture::for_bin(1);
        let page = f.init(1, true);
        let before = header(page);
        // SAFETY: written by `init`.
        unsafe {
            assert!(!in_full_queue(page));
            set_in_full_queue(page, true);
            assert!(in_full_queue(page));
            assert_eq!(header(page).flags, FLAG_IN_FULL_QUEUE);
            set_in_full_queue(page, true);
            assert!(in_full_queue(page));
            set_in_full_queue(page, false);
            assert!(!in_full_queue(page));
            set_in_full_queue(page, false);
            assert!(!in_full_queue(page));
        }
        let after = header(page);
        assert_eq!(std::format!("{before:?}"), std::format!("{after:?}"));
    }

    #[test]
    fn validate_rejects_corrupt_pages() {
        let bin = 4;
        let f = Fixture::for_bin(bin);
        let page = f.init(bin, true);
        let bs = bin_size(bin);
        let a = pop_or_extend(page, f.mem()).unwrap();
        let _b = pop_or_extend(page, f.mem()).unwrap();
        check(page, f.mem());
        let good = header(page);
        // SAFETY: `page` is valid and the region is ours; each corruption is undone.
        unsafe {
            let corrupt = |mutate: &dyn Fn(), expect: &str| {
                mutate();
                let err = validate(page, f.mem()).expect_err(expect);
                assert!(err.contains(expect), "got {err:?}, expected {expect:?}");
                page.write(good);
                let head = f.mem().ptr(good.free).cast::<usize>();
                head.write(good.free + bs);
                check(page, f.mem());
            };
            corrupt(
                &|| (*page).used = good.capacity + 1,
                "used exceeds capacity",
            );
            corrupt(
                &|| (*page).capacity = good.reserved + 1,
                "capacity exceeds reserved",
            );
            corrupt(&|| (*page).block_size += 8, "block_size does not match bin");
            corrupt(&|| (*page).block_start += 8, "block_start does not match");
            corrupt(&|| (*page).reserved -= 1, "reserved does not match");
            corrupt(&|| (*page).bin += 1, "block_size does not match bin");
            corrupt(
                &|| (*page).bin = SMALL_MAX_BIN + 1,
                "kind does not match bin",
            );
            corrupt(&|| (*page).bin = 0, "bin out of range");
            corrupt(&|| (*page).kind = 7, "kind byte");
            corrupt(&|| (*page).used -= 1, "used + free list length");
            corrupt(&|| (*page).used += 1, "free list longer");
            corrupt(&|| (*page).free = a, "free list longer");
            let head = f.mem().ptr(good.free).cast::<usize>();
            corrupt(&|| head.write(good.free), "free list longer");
            corrupt(&|| head.write(good.free + 4), "misaligned");
            corrupt(
                &|| head.write(f.page_addr),
                "outside the initialised block area",
            );
            corrupt(&|| head.write(a), "free list longer");
            corrupt(&|| (*page).free = f.page_addr + SLICE_SIZE, "outside");
        }
    }
}

#[cfg(kani)]
mod verify {
    use super::*;
    use crate::bins::{MAX_NATURAL_ALIGN, SLICE_SIZE, bin_size, blocks_per_page, kind_of_bin};

    fn any_kind() -> PageKind {
        match kani::any::<u8>() % 3 {
            0 => PageKind::Small,
            1 => PageKind::Medium,
            _ => PageKind::Large,
        }
    }

    /// The mask trick: for any page-aligned address and any offset inside the page, the header
    /// is recovered. No overflow assumption is needed because an aligned address leaves room
    /// for a whole page below `usize::MAX`.
    #[kani::proof]
    fn header_of_recovers_the_page_from_any_address_inside_it() {
        let kind = any_kind();
        let page_addr: usize = kani::any();
        let offset: usize = kani::any();
        kani::assume(page_addr % kind.page_size() == 0);
        kani::assume(offset < kind.page_size());
        assert!(header_of(kind, page_addr + offset) == page_addr);
        assert!(header_of(kind, page_addr) == page_addr);
    }

    /// Geometry for every bin: every byte of every block lies inside the page (so `header_of`
    /// recovers the page from it and `extend`'s writes stay in bounds), block starts carry the
    /// natural alignment the size class promises, and the counts fit the header's fields
    /// (`u32` for the block counts, `u16` for `block_start`).
    #[kani::proof]
    fn every_block_of_every_bin_lies_inside_its_page_and_is_aligned() {
        let bin: u8 = kani::any();
        kani::assume(bin >= 1 && bin <= MAX_BIN);
        let kind = kind_of_bin(bin);
        let bs = bin_size(bin);
        let start = bins::block_start(bs);
        let reserved = blocks_per_page(kind, bs);
        assert!(reserved >= 1 && reserved <= u32::MAX as usize);
        assert!(start >= PAGE_HEADER_RESERVE && start <= u16::MAX as usize);
        assert!(bs <= u32::MAX as usize);
        assert!(start + reserved * bs <= kind.page_size());

        let i: u16 = kani::any();
        kani::assume((i as usize) < reserved);
        let j: usize = kani::any();
        kani::assume(j < bs);
        let block_offset = start + i as usize * bs;
        let natural = (bs & bs.wrapping_neg()).min(MAX_NATURAL_ALIGN);
        assert!(block_offset % natural == 0);
        assert!(block_offset % WORD == 0);

        let page_addr: usize = kani::any();
        kani::assume(page_addr % kind.page_size() == 0);
        assert!(header_of(kind, page_addr + block_offset + j) == page_addr);
    }

    /// A `Memory` holding exactly the words `page` may touch: the header and the first word of
    /// each of `BLOCKS` blocks. `ptr` maps a block address to its link word and asserts that the
    /// address is the start of a block below `BLOCKS`, so a stray access fails the proof, and a
    /// symbolic block index selects among `BLOCKS` words instead of 64 Ki bytes. (A flat 64 KiB
    /// buffer made CBMC expand every symbolic-offset write into a case over all its bytes and
    /// exhaust 4 GiB within seconds.)
    struct PageModel<const BLOCKS: usize> {
        header: *mut Page,
        links: *mut usize,
        block_start: usize,
        block_size: usize,
    }

    // SAFETY: proof-only backend. `header` and `links` point to storage owned by the harness
    // frame for longer than the model lives; `ptr` returns the header for the page address and
    // otherwise a link word inside `links`, so every pointer it yields is valid for a `Page` or
    // `usize` access respectively. `grow` never hands out memory.
    unsafe impl<const BLOCKS: usize> Memory for PageModel<BLOCKS> {
        fn heap_base(&self) -> usize {
            self.header.addr()
        }

        fn size_slices(&self) -> usize {
            self.header.addr() / SLICE_SIZE + 1
        }

        fn grow(&mut self, _slices: usize) -> Option<usize> {
            None
        }

        fn ptr(&self, addr: usize) -> *mut u8 {
            let page_addr = self.header.addr();
            if addr == page_addr {
                return self.header.cast();
            }
            let offset = addr - page_addr - self.block_start;
            assert!(
                offset % self.block_size == 0,
                "access is not to a block's first word"
            );
            let index = offset / self.block_size;
            assert!(index < BLOCKS, "access beyond the page's blocks");
            // SAFETY: `links` has `BLOCKS` elements and `index < BLOCKS`.
            unsafe { self.links.add(index).cast() }
        }
    }

    /// Run `STEPS` symbolic operations on a fresh zeroed small page of `bin` and check the
    /// invariants after each. `live` tracks handed-out blocks so pushes are always legitimate.
    fn ops_preserve_invariants<const STEPS: usize, const BLOCKS: usize>(bin: u8) {
        let bs = bin_size(bin);
        assert!(blocks_per_page(PageKind::Small, bs) == BLOCKS);
        let mut header = Page {
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
        let mut links = [0usize; BLOCKS];
        let mem = PageModel::<BLOCKS> {
            header: &mut header,
            links: links.as_mut_ptr(),
            block_start: bins::block_start(bs),
            block_size: bs,
        };
        let page_addr = mem.header.addr();
        // SAFETY: the model maps `page_addr` to `header`, which nothing else uses.
        let page = unsafe { init(&mem, page_addr, PageKind::Small, bin, true) };
        // SAFETY: `page` was written by `init` and is only touched through this module.
        unsafe {
            assert!(validate(page, &mem).is_ok());
            let mut live = [0usize; BLOCKS];
            let mut n = 0usize;
            for _ in 0..STEPS {
                match kani::any::<u8>() % 3 {
                    0 => {
                        if let Some(block) = pop(page, &mem) {
                            assert!(n < BLOCKS);
                            assert!(block_index(page, block).is_some());
                            assert!(header_of(PageKind::Small, block) == page_addr);
                            assert!(block % (bs & bs.wrapping_neg()).min(MAX_NATURAL_ALIGN) == 0);
                            live[n] = block;
                            n += 1;
                        } else {
                            assert!(!has_free(page));
                        }
                    }
                    1 => {
                        if n > 0 {
                            let i: usize = kani::any();
                            kani::assume(i < n);
                            let block = live[i];
                            live[i] = live[n - 1];
                            n -= 1;
                            push(page, &mem, block);
                            assert!(!(*page).free_is_zero);
                        }
                    }
                    _ => {
                        let expandable = is_expandable(page);
                        assert!(extend(page, &mem) == expandable);
                    }
                }
                assert!(validate(page, &mem).is_ok());
                assert!((*page).used as usize == n);
                assert!(is_full(page) == (n == BLOCKS));
                assert!(all_free(page) == (n == 0));
            }
        }
    }

    /// Bin 36: 8 KiB blocks, 7 per page, one block per extension.
    #[kani::proof]
    #[kani::unwind(9)]
    fn four_operations_on_a_page_of_eight_kib_blocks_preserve_invariants() {
        ops_preserve_invariants::<4, 7>(36);
    }

    /// Bin 32: 4 KiB blocks, 15 per page, two blocks per extension, so the link loop in
    /// `extend` runs. Two steps rather than four keep this harness in the quick gate.
    #[kani::proof]
    #[kani::unwind(17)]
    fn two_operations_on_a_page_of_four_kib_blocks_preserve_invariants() {
        ops_preserve_invariants::<2, 15>(32);
    }
}
