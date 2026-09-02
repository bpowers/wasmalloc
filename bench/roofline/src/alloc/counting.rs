//! wasmalloc's heap over a `Memory` backend that counts `memory.grow` calls.
//!
//! The footprint tables say how many pages a workload ended with, not how many times the
//! allocator asked the engine for them, and on V8 12.4 each `memory.grow` costs about 60 us
//! whatever its size. This variant runs the same heap code as `wasmalloc` behind the harness's
//! own `GlobalAlloc` shim, so its footprint and grow counts are wasmalloc's while its ns/op
//! figures are only close to them (a different static, an extra counter increment per grow).
//! Use it for `grow_calls`, not for the timing tables.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::{self, NonNull};

use wasmalloc::backend::{Memory, WasmMemory};
use wasmalloc::heap::Heap;

pub struct CountingMemory {
    inner: WasmMemory,
    grows: usize,
}

// SAFETY: every call is forwarded to WasmMemory, which upholds the contract; the counter
// changes nothing about the memory handed out.
unsafe impl Memory for CountingMemory {
    #[inline]
    fn heap_base(&self) -> usize {
        self.inner.heap_base()
    }

    #[inline]
    fn size_slices(&self) -> usize {
        self.inner.size_slices()
    }

    #[inline]
    fn grow(&mut self, slices: usize) -> Option<usize> {
        self.grows += 1;
        self.inner.grow(slices)
    }

    #[inline]
    fn ptr(&self, addr: usize) -> *mut u8 {
        self.inner.ptr(addr)
    }
}

pub struct CountingAlloc {
    heap: UnsafeCell<Heap<CountingMemory>>,
}

// SAFETY: single-threaded wasm32, as for wasmalloc::WasmAlloc: allocator calls never overlap.
unsafe impl Sync for CountingAlloc {}

impl CountingAlloc {
    pub const fn new() -> Self {
        CountingAlloc {
            heap: UnsafeCell::new(Heap::new(CountingMemory {
                inner: WasmMemory,
                grows: 0,
            })),
        }
    }

    /// `memory.grow` calls made so far, refused ones included.
    pub fn grow_calls(&self) -> usize {
        // SAFETY: single-threaded; no allocator call is in progress while the harness reads.
        unsafe { (*self.heap.get()).memory().grows }
    }

    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    unsafe fn heap(&self) -> &mut Heap<CountingMemory> {
        unsafe { &mut *self.heap.get() }
    }
}

#[inline(always)]
fn to_raw(p: Option<NonNull<u8>>) -> *mut u8 {
    match p {
        Some(p) => p.as_ptr(),
        None => ptr::null_mut(),
    }
}

unsafe impl GlobalAlloc for CountingAlloc {
    #[inline(always)]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { to_raw(self.heap().alloc(layout)) }
    }

    #[inline(always)]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        unsafe { to_raw(self.heap().alloc_zeroed(layout)) }
    }

    #[inline(always)]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.heap().dealloc(NonNull::new_unchecked(ptr), layout) }
    }

    #[inline(always)]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        unsafe {
            to_raw(
                self.heap()
                    .realloc(NonNull::new_unchecked(ptr), layout, new_size),
            )
        }
    }
}
