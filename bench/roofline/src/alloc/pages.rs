//! Floor for a mimalloc-shaped fast path: small blocks live in 64 KiB pages
//! that each hold a single size class, and `dealloc` recovers the class from
//! the page header (pointer masking plus one load) instead of trusting the
//! `Layout`. Allocation pops the class's intrusive LIFO list, else bumps within
//! the class's current page, else takes a fresh page from the bump region.
//! Requests above 1024 bytes (or over-aligned) go to the bump and leak on free.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;

use super::bump::Bump;
use super::sizeclass::{class_of, small, GRANULE, NCLASS};

const PAGE: usize = 65536;
/// Bytes reserved at the start of each page for the header (the class index).
const HEADER: usize = 16;

pub struct Pages {
    heads: [Cell<*mut usize>; NCLASS],
    cur: [Cell<usize>; NCLASS],
    end: [Cell<usize>; NCLASS],
    bump: Bump,
}

unsafe impl Sync for Pages {}

impl Pages {
    pub const fn new() -> Self {
        const NULL: Cell<*mut usize> = Cell::new(core::ptr::null_mut());
        const ZERO: Cell<usize> = Cell::new(0);
        Pages {
            heads: [NULL; NCLASS],
            cur: [ZERO; NCLASS],
            end: [ZERO; NCLASS],
            bump: Bump::new(),
        }
    }

    pub fn reset(&self) {
        for i in 0..NCLASS {
            self.heads[i].set(core::ptr::null_mut());
            self.cur[i].set(0);
            self.end[i].set(0);
        }
        self.bump.reset();
    }

    #[cold]
    #[inline(never)]
    unsafe fn refill(&self, c: usize) -> *mut u8 {
        let page = self
            .bump
            .alloc_inline(Layout::from_size_align_unchecked(PAGE, PAGE));
        if page.is_null() {
            return page;
        }
        *(page as *mut u32) = c as u32;
        let block = (c + 1) * GRANULE;
        let first = page as usize + HEADER;
        self.cur[c].set(first + block);
        self.end[c].set(page as usize + PAGE);
        first as *mut u8
    }
}

unsafe impl GlobalAlloc for Pages {
    #[inline(always)]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if small(&layout) {
            let c = class_of(layout.size());
            let slot = self.heads.get_unchecked(c);
            let head = slot.get();
            if !head.is_null() {
                slot.set(*head as *mut usize);
                return head as *mut u8;
            }
            let block = (c + 1) * GRANULE;
            let cur = self.cur.get_unchecked(c).get();
            if cur + block <= self.end.get_unchecked(c).get() {
                self.cur.get_unchecked(c).set(cur + block);
                return cur as *mut u8;
            }
            return self.refill(c);
        }
        self.bump.alloc_inline(layout)
    }

    #[inline(always)]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if small(&layout) {
            let page = (ptr as usize) & !(PAGE - 1);
            let c = *(page as *const u32) as usize;
            let slot = self.heads.get_unchecked(c);
            let node = ptr as *mut usize;
            *node = slot.get() as usize;
            slot.set(node);
        }
    }
}
