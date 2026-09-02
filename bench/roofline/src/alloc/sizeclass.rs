//! Floor for a segregated-free-list fast path where the size class is known at
//! the free site: 64 classes of 16-byte granularity covering 1..=1024 bytes,
//! each an intrusive LIFO list. Blocks come from the bump region. Rust's
//! `GlobalAlloc::dealloc` receives the original `Layout`, so the class is
//! recomputed from `layout.size()` rather than looked up from the pointer.
//! Requests above 1024 bytes (or over-aligned) go to the bump and leak on free.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;

use super::bump::Bump;

pub const GRANULE: usize = 16;
pub const MAX_SMALL: usize = 1024;
pub const NCLASS: usize = MAX_SMALL / GRANULE;

pub struct SizeClass {
    heads: [Cell<*mut usize>; NCLASS],
    bump: Bump,
}

unsafe impl Sync for SizeClass {}

#[inline(always)]
pub fn class_of(size: usize) -> usize {
    // size is >= 1 per GlobalAlloc's contract, so this never underflows.
    ((size + GRANULE - 1) >> 4) - 1
}

#[inline(always)]
pub fn small(layout: &Layout) -> bool {
    layout.size() <= MAX_SMALL && layout.align() <= GRANULE
}

impl SizeClass {
    pub const fn new() -> Self {
        const NULL: Cell<*mut usize> = Cell::new(core::ptr::null_mut());
        SizeClass {
            heads: [NULL; NCLASS],
            bump: Bump::new(),
        }
    }

    pub fn reset(&self) {
        for h in &self.heads {
            h.set(core::ptr::null_mut());
        }
        self.bump.reset();
    }
}

unsafe impl GlobalAlloc for SizeClass {
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
            return self
                .bump
                .alloc_inline(Layout::from_size_align_unchecked(block, GRANULE));
        }
        self.bump.alloc_inline(layout)
    }

    #[inline(always)]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if small(&layout) {
            let slot = self.heads.get_unchecked(class_of(layout.size()));
            let node = ptr as *mut usize;
            *node = slot.get() as usize;
            slot.set(node);
        }
    }
}
