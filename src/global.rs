//! The `#[global_allocator]` entry point for wasm32.
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: wasmalloc::WasmAlloc = wasmalloc::WasmAlloc::new();
//! ```
//!
//! One [`Heap`] over [`WasmMemory`] lives inside the static. The program is single-threaded (this
//! crate does not support the `atomics` target feature, see the crate documentation), so the
//! `GlobalAlloc` methods, which take `&self`, hand out `&mut Heap` through an `UnsafeCell`
//! without any synchronisation. The heap never allocates, panics or unwinds while servicing a
//! request, so it cannot re-enter itself.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::{self, NonNull};

use crate::backend::WasmMemory;
use crate::heap::Heap;

/// The global allocator. See the module documentation.
pub struct WasmAlloc {
    heap: UnsafeCell<Heap<WasmMemory>>,
}

// SAFETY: wasm32 without the `atomics` feature has exactly one thread, so no two calls into the
// allocator can overlap; every access to the heap goes through `heap()` from a `&self` method
// that runs to completion before any other allocator call can start.
unsafe impl Sync for WasmAlloc {}

impl WasmAlloc {
    /// An allocator over linear memory 0. Touches nothing until the first allocation.
    pub const fn new() -> Self {
        WasmAlloc {
            heap: UnsafeCell::new(Heap::new(WasmMemory)),
        }
    }

    /// Exclusive access to the heap.
    ///
    /// # Safety
    ///
    /// No other reference to the heap may be live: callers are the `GlobalAlloc` methods, which
    /// never nest (the heap does not allocate), and the program is single-threaded.
    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    unsafe fn heap(&self) -> &mut Heap<WasmMemory> {
        // SAFETY: see the function's contract.
        unsafe { &mut *self.heap.get() }
    }
}

impl Default for WasmAlloc {
    fn default() -> Self {
        Self::new()
    }
}

#[inline(always)]
fn to_raw(p: Option<NonNull<u8>>) -> *mut u8 {
    match p {
        Some(p) => p.as_ptr(),
        None => ptr::null_mut(),
    }
}

// SAFETY: the heap implements the GlobalAlloc contract: blocks are aligned and sized for their
// Layout, never overlap while live, dealloc and realloc recompute the block's page from the
// Layout the caller passes back, and the heap never unwinds.
unsafe impl GlobalAlloc for WasmAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: GlobalAlloc guarantees a non-zero size; single-threaded access (see `heap`).
        unsafe { to_raw(self.heap().alloc(layout)) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as for `alloc`.
        unsafe { to_raw(self.heap().alloc_zeroed(layout)) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: GlobalAlloc guarantees `ptr` was returned by this allocator for `layout`, hence
        // non-null; single-threaded access.
        unsafe { self.heap().dealloc(NonNull::new_unchecked(ptr), layout) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: GlobalAlloc guarantees `ptr` is live for `layout` and that `new_size` rounded
        // up to the alignment does not overflow isize; single-threaded access.
        unsafe {
            to_raw(
                self.heap()
                    .realloc(NonNull::new_unchecked(ptr), layout, new_size),
            )
        }
    }
}
