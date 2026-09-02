//! Floor that reproduces the memory traffic of wasmalloc's fast paths and
//! nothing else, to price the header bookkeeping mimalloc's design carries.
//!
//! Like wasmalloc (src/heap.rs, src/page.rs): a direct table indexed by size
//! points at the current 64 KiB page of the class (or at a read-only empty
//! sentinel), the page header holds the intrusive free list head plus a `used`
//! count, a `free_is_zero` byte and a `flags` byte at the same offsets as
//! wasmalloc's `Page`, allocation pops the list and increments `used`,
//! deallocation masks the pointer to the header, pushes, decrements `used`,
//! clears `free_is_zero`, and calls a cold transition when the page empties or
//! is flagged. Unlike wasmalloc there are no queues, no retirement, no
//! extension batches, and no medium or large pages; anything above 1024 bytes
//! goes to the bump and leaks.
//!
//! Four knobs split the cost of that bookkeeping. With `LEAN` the header holds
//! only the free list head: the `used`, `free_is_zero` and `flags` traffic
//! disappears while the direct-table indirection stays. With `WIDE_USED` the
//! `used` counter is read and written as a 32-bit word (wasmalloc originally
//! used a u16, which on wasm32 costs 16-bit loads and stores). With `NO_ZERO`
//! the `free_is_zero` byte is not cleared on free. With `NO_TEST` the
//! `used == 0 || flags != 0` test and the cold call behind it are dropped.
//!
//! The transition counts its calls in the allocator's state so that LLVM cannot
//! prove it has no effect: an empty cold function is deleted together with the
//! test that guards it, which is what the first version of this floor did, and
//! the loop then lacked one load, one branch and one call per free that
//! wasmalloc pays whenever a free empties its page (as every free in
//! `alloc_free_32` does).

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;

use super::bump::Bump;
use super::sizeclass::{class_of, small, GRANULE, NCLASS};

const PAGE: usize = 65536;
/// Header reserve, as wasmalloc's PAGE_HEADER_RESERVE.
const HEADER: usize = 64;

/// Field order and types follow wasmalloc's `page::Page`, so the offsets the
/// fast paths touch are identical (on wasm32: free 0, used 4, free_is_zero 16,
/// flags 19).
#[repr(C)]
pub struct Hdr {
    free: usize,
    used: u16,
    capacity: u16,
    reserved: u16,
    block_start: u16,
    block_size: u32,
    free_is_zero: bool,
    bin: u8,
    kind: u8,
    flags: u8,
    retire_expire: u8,
    next: usize,
    prev: usize,
}

static EMPTY: Hdr = Hdr {
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

const fn sentinel() -> *mut Hdr {
    (&raw const EMPTY).cast_mut()
}

pub struct Mimic<const LEAN: bool, const WIDE_USED: bool, const NO_ZERO: bool, const NO_TEST: bool>
{
    direct: [Cell<*mut Hdr>; NCLASS],
    transitions: Cell<u32>,
    cur: [Cell<usize>; NCLASS],
    end: [Cell<usize>; NCLASS],
    bump: Bump,
}

unsafe impl<const LEAN: bool, const WIDE_USED: bool, const NO_ZERO: bool, const NO_TEST: bool> Sync
    for Mimic<LEAN, WIDE_USED, NO_ZERO, NO_TEST>
{
}

impl<const LEAN: bool, const WIDE_USED: bool, const NO_ZERO: bool, const NO_TEST: bool>
    Mimic<LEAN, WIDE_USED, NO_ZERO, NO_TEST>
{
    pub const fn new() -> Self {
        const SENTINEL: Cell<*mut Hdr> = Cell::new(sentinel());
        const ZERO: Cell<usize> = Cell::new(0);
        Mimic {
            direct: [SENTINEL; NCLASS],
            transitions: Cell::new(0),
            cur: [ZERO; NCLASS],
            end: [ZERO; NCLASS],
            bump: Bump::new(),
        }
    }

    pub fn reset(&self) {
        for i in 0..NCLASS {
            self.direct[i].set(sentinel());
            self.cur[i].set(0);
            self.end[i].set(0);
        }
        self.bump.reset();
    }

    /// `used`, read as wasmalloc originally did (u16) or as a whole 32-bit word
    /// covering `used` and the unused `capacity` field, at the same offset. The
    /// offset comes from the struct: it is 4 on wasm32 and 8 on the 64-bit host.
    #[inline(always)]
    unsafe fn used(page: *mut Hdr) -> u32 {
        if WIDE_USED {
            page.cast::<u8>()
                .add(core::mem::offset_of!(Hdr, used))
                .cast::<u32>()
                .read()
        } else {
            (*page).used as u32
        }
    }

    #[inline(always)]
    unsafe fn set_used(page: *mut Hdr, v: u32) {
        if WIDE_USED {
            page.cast::<u8>()
                .add(core::mem::offset_of!(Hdr, used))
                .cast::<u32>()
                .write(v);
        } else {
            (*page).used = v as u16;
        }
    }

    /// The free list is empty: carve the next never-used block of the class's
    /// current page, or start a new page. Stands in for wasmalloc's
    /// alloc_generic (extend, queue search, fresh page).
    #[cold]
    #[inline(never)]
    unsafe fn refill(&self, c: usize) -> *mut u8 {
        let block = (c + 1) * GRANULE;
        let mut cur = self.cur[c].get();
        if cur + block > self.end[c].get() {
            let page = self
                .bump
                .alloc_inline(Layout::from_size_align_unchecked(PAGE, PAGE));
            if page.is_null() {
                return page;
            }
            let hdr = page as *mut Hdr;
            hdr.write(Hdr {
                free: 0,
                used: 0,
                capacity: 0,
                reserved: ((PAGE - HEADER) / block) as u16,
                block_start: HEADER as u16,
                block_size: block as u32,
                free_is_zero: true,
                bin: c as u8,
                kind: 0,
                flags: 0,
                retire_expire: 0,
                next: 0,
                prev: 0,
            });
            self.direct[c].set(hdr);
            cur = page as usize + HEADER;
            self.end[c].set(page as usize + PAGE);
        }
        self.cur[c].set(cur + block);
        if !LEAN {
            let page = self.direct[c].get();
            Self::set_used(page, Self::used(page) + 1);
        }
        cur as *mut u8
    }

    /// The page became empty or is flagged: wasmalloc would retire it or move
    /// it between queues. The floor only counts the event, which is enough of a
    /// side effect to keep the call, and the test in front of it, alive.
    #[cold]
    #[inline(never)]
    fn transition(&self, _page: *mut Hdr) {
        self.transitions.set(self.transitions.get().wrapping_add(1));
    }
}

unsafe impl<const LEAN: bool, const WIDE_USED: bool, const NO_ZERO: bool, const NO_TEST: bool>
    GlobalAlloc for Mimic<LEAN, WIDE_USED, NO_ZERO, NO_TEST>
{
    #[inline(always)]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if small(&layout) {
            let c = class_of(layout.size());
            let page = self.direct.get_unchecked(c).get();
            let block = (*page).free;
            if block != 0 {
                (*page).free = *(block as *const usize);
                if !LEAN {
                    Self::set_used(page, Self::used(page) + 1);
                }
                return block as *mut u8;
            }
            return self.refill(c);
        }
        self.bump.alloc_inline(layout)
    }

    #[inline(always)]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if small(&layout) {
            let page = ((ptr as usize) & !(PAGE - 1)) as *mut Hdr;
            let block = ptr as usize;
            if LEAN {
                *(block as *mut usize) = (*page).free;
                (*page).free = block;
                return;
            }
            let used = Self::used(page) - 1;
            *(block as *mut usize) = (*page).free;
            (*page).free = block;
            Self::set_used(page, used);
            if !NO_ZERO {
                (*page).free_is_zero = false;
            }
            if !NO_TEST && (used == 0 || (*page).flags != 0) {
                self.transition(page);
            }
        }
    }
}
