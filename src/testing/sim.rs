//! The crate's heap over a simulated linear memory, ready for the model tester.
//!
//! [`SimHeap`] owns a 4 MiB-aligned host region, a [`SimMemory`] over it and a [`Heap`] on top,
//! and frees the region when dropped. Tests and fuzz targets use it wherever they would use
//! `std::alloc::System`, so the same operation stream can be replayed against both.

use core::alloc::Layout;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;

use super::model::RawAlloc;
use crate::backend::testing::HostRegion;
use crate::backend::{Memory, SimMemory};
use crate::bins::SLICE_SIZE;
use crate::heap::Heap;

impl<const WORDS: usize> RawAlloc for Heap<SimMemory, WORDS> {
    unsafe fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        unsafe { Heap::alloc(self, layout) }
    }

    unsafe fn alloc_zeroed(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        unsafe { Heap::alloc_zeroed(self, layout) }
    }

    unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: same contract as this method.
        unsafe { Heap::dealloc(self, ptr, layout) }
    }

    unsafe fn realloc(
        &mut self,
        ptr: NonNull<u8>,
        layout: Layout,
        new_size: usize,
    ) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        unsafe { Heap::realloc(self, ptr, layout, new_size) }
    }

    /// Everything from the heap base to the current end of the simulated memory: the linker
    /// gap the heap reclaims plus every slice it has grown, whether or not it is in use.
    fn footprint_bytes(&self) -> Option<usize> {
        let mem = self.memory();
        Some(mem.size_slices() * SLICE_SIZE - mem.heap_base())
    }
}

/// A [`Heap`] over a simulated linear memory in a host region that is freed on drop.
///
/// `WORDS` sizes the heap's slice bitmap exactly as for [`Heap`]; the default covers 4 GiB, so
/// any region up to that size works with it. Dereferences to the heap.
pub struct SimHeap<const WORDS: usize = 1024> {
    // Declared before `_region` so that the heap, which points into the region, drops first.
    heap: Heap<SimMemory, WORDS>,
    _region: HostRegion,
}

impl<const WORDS: usize> SimHeap<WORDS> {
    /// A heap over a region of `total_slices` slices whose simulated memory is `initial_slices`
    /// long at first and whose heap base sits `heap_base_offset` bytes into the region.
    ///
    /// The region is committed lazily by the OS, so a large `total_slices` only costs address
    /// space until the heap grows into it. Panics if the region does not fit the slice bitmap.
    pub fn new(total_slices: usize, initial_slices: usize, heap_base_offset: usize) -> Self {
        assert!(
            total_slices <= WORDS * 64,
            "region of {total_slices} slices exceeds the {} the slice map can describe",
            WORDS * 64
        );
        let region = HostRegion::new(total_slices);
        // SAFETY: the memory is owned by `heap`, which is declared before `_region` and so
        // drops first; nothing else simulates over this region.
        let mem = unsafe { region.simulate(initial_slices, heap_base_offset) };
        SimHeap {
            heap: Heap::new(mem),
            _region: region,
        }
    }
}

impl<const WORDS: usize> Deref for SimHeap<WORDS> {
    type Target = Heap<SimMemory, WORDS>;

    fn deref(&self) -> &Self::Target {
        &self.heap
    }
}

impl<const WORDS: usize> DerefMut for SimHeap<WORDS> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.heap
    }
}

impl<const WORDS: usize> RawAlloc for SimHeap<WORDS> {
    unsafe fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        unsafe { self.heap.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        unsafe { self.heap.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: same contract as this method.
        unsafe { self.heap.dealloc(ptr, layout) }
    }

    unsafe fn realloc(
        &mut self,
        ptr: NonNull<u8>,
        layout: Layout,
        new_size: usize,
    ) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        unsafe { self.heap.realloc(ptr, layout, new_size) }
    }

    fn footprint_bytes(&self) -> Option<usize> {
        self.heap.footprint_bytes()
    }
}
