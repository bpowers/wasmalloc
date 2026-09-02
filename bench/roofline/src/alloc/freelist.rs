//! Floor for a single size-class fast path: one intrusive LIFO free list for
//! blocks of up to 32 bytes, carved from the bump region when the list is empty.
//! Anything larger falls through to the bump allocator (and is leaked on free).

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;

use super::bump::Bump;

const BLOCK: usize = 32;
const BLOCK_ALIGN: usize = 16;

pub struct FreeList {
    head: Cell<*mut usize>,
    bump: Bump,
}

unsafe impl Sync for FreeList {}

impl FreeList {
    pub const fn new() -> Self {
        FreeList {
            head: Cell::new(core::ptr::null_mut()),
            bump: Bump::new(),
        }
    }

    pub fn reset(&self) {
        self.head.set(core::ptr::null_mut());
        self.bump.reset();
    }

    #[inline(always)]
    fn fits(layout: &Layout) -> bool {
        layout.size() <= BLOCK && layout.align() <= BLOCK_ALIGN
    }
}

unsafe impl GlobalAlloc for FreeList {
    #[inline(always)]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if Self::fits(&layout) {
            let head = self.head.get();
            if !head.is_null() {
                self.head.set(*head as *mut usize);
                return head as *mut u8;
            }
            return self
                .bump
                .alloc_inline(Layout::from_size_align_unchecked(BLOCK, BLOCK_ALIGN));
        }
        self.bump.alloc_inline(layout)
    }

    #[inline(always)]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if Self::fits(&layout) {
            let node = ptr as *mut usize;
            *node = self.head.get() as usize;
            self.head.set(node);
        }
    }
}
