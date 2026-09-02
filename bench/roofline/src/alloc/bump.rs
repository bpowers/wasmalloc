//! The absolute floor: a bump pointer over a contiguous region obtained from
//! memory.grow. `dealloc` is a no-op. `reset` rewinds the pointer to the start
//! of the region so a harness can reuse memory between repetitions once every
//! allocation has been released.
//!
//! The region is grown in large chunks so that memory.grow is rare enough not
//! to show up in the per-operation cost; the harness measures memory.grow
//! separately.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;

/// Grow by at least this much each time the region is exhausted.
const CHUNK: usize = 16 << 20;

pub struct Bump {
    cur: Cell<usize>,
    end: Cell<usize>,
    start: Cell<usize>,
}

// Single-threaded wasm: there is exactly one thread, so a Cell-based static is
// fine. This is the same assumption lol_alloc's AssumeSingleThreaded makes.
unsafe impl Sync for Bump {}

impl Bump {
    pub const fn new() -> Self {
        Bump {
            cur: Cell::new(0),
            end: Cell::new(0),
            start: Cell::new(0),
        }
    }

    pub fn reset(&self) {
        self.cur.set(self.start.get());
    }

    #[inline(always)]
    pub unsafe fn alloc_inline(&self, layout: Layout) -> *mut u8 {
        let align = layout.align();
        let size = layout.size();
        let aligned = (self.cur.get() + align - 1) & !(align - 1);
        let new_cur = aligned + size;
        if new_cur > self.end.get() {
            return self.alloc_slow(layout);
        }
        self.cur.set(new_cur);
        aligned as *mut u8
    }

    #[cold]
    #[inline(never)]
    unsafe fn alloc_slow(&self, layout: Layout) -> *mut u8 {
        let need = layout.size() + layout.align();
        let bytes = if need > CHUNK { need } else { CHUNK };
        let pages = (bytes + super::WASM_PAGE - 1) / super::WASM_PAGE;
        let base = match super::grow_pages(pages) {
            Some(b) => b,
            None => return core::ptr::null_mut(),
        };
        if base != self.end.get() || self.end.get() == 0 {
            // Either the very first chunk, or something else grew memory in
            // between (not the case in this harness, but stay correct):
            // abandon the old region and start a fresh one.
            self.start.set(base);
            self.cur.set(base);
        }
        self.end.set(base + pages * super::WASM_PAGE);
        // Retry on the fast path; guaranteed to fit now.
        let align = layout.align();
        let aligned = (self.cur.get() + align - 1) & !(align - 1);
        self.cur.set(aligned + layout.size());
        aligned as *mut u8
    }
}

unsafe impl GlobalAlloc for Bump {
    #[inline(always)]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.alloc_inline(layout)
    }

    #[inline(always)]
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}
