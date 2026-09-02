//! How the allocator sees linear memory.
//!
//! The core is generic over a [`Memory`] so that the same code runs against real wasm linear
//! memory and against a simulated linear memory on the host (tests, fuzzing, Miri, Kani).
//! Everything above this module works in *slices* (64 KiB, one wasm page) and in addresses,
//! never in raw pointers, until the moment it touches memory; [`Memory::ptr`] is where an
//! address becomes a pointer with provenance.
//!
//! Sizes are expressed in slices rather than bytes because a full 4 GiB linear memory has an end
//! address of `2^32`, which does not fit in a wasm32 `usize`. Slice indices are always below
//! `2^16`, so `index * SLICE_SIZE` is a valid address for every real slice.

use crate::bins::SLICE_SIZE;

/// Highest slice index that can exist in a 32-bit linear memory.
pub const MAX_SLICE_INDEX: usize = (1 << 16) - 1;

/// A linear memory that only grows.
///
/// # Safety
///
/// Implementors must guarantee:
/// - `grow(n)` returns the index of the first of `n` fresh, zero-filled, contiguous slices that
///   no one else will touch, or `None`. The new slices need not be contiguous with the previous
///   end (someone else may grow the same memory between calls), and every returned index is at
///   most [`MAX_SLICE_INDEX`].
/// - `size_slices()` is the current end of memory in slices and never decreases.
/// - `heap_base()` is the first address the allocator may use; all memory from there to the end
///   of memory, except what `grow` has not yet handed out, is exclusively the allocator's.
/// - `ptr(addr)` yields a pointer valid for reads and writes at every address the allocator
///   owns, carrying provenance for the whole owned region, so pointer arithmetic that stays
///   inside owned memory is defined behaviour.
/// - `ptr(addr).addr() == addr`: the pointer's address is the address asked for. The heap
///   turns page header pointers back into addresses (queue links, `page::extend`, `free_page`,
///   `header_of`), so a backend that placed memory elsewhere would break it.
pub unsafe trait Memory {
    /// First address the allocator may use (the linker's `__heap_base` on wasm).
    fn heap_base(&self) -> usize;

    /// Current size of linear memory in slices.
    fn size_slices(&self) -> usize;

    /// Grow by `slices` and return the index of the first new slice, or `None` on failure.
    /// The new slices are zero-filled.
    fn grow(&mut self, slices: usize) -> Option<usize>;

    /// A pointer for `addr` with provenance over the allocator's memory.
    fn ptr(&self, addr: usize) -> *mut u8;
}

/// Real wasm linear memory number 0.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Default)]
pub struct WasmMemory;

#[cfg(target_arch = "wasm32")]
// SAFETY: `memory.grow` returns fresh zero pages by the wasm specification and they are exclusively
// ours because nothing else in a single-threaded Rust wasm program allocates linear memory
// (std's own allocator is replaced by this crate). `__heap_base` is set by wasm-ld to the end of
// data and stack. Linear memory is one allocation, so an exposed-provenance pointer to any
// address in it is valid.
unsafe impl Memory for WasmMemory {
    #[inline]
    fn heap_base(&self) -> usize {
        unsafe extern "C" {
            static __heap_base: u8;
        }
        core::ptr::addr_of!(__heap_base) as usize
    }

    #[inline]
    fn size_slices(&self) -> usize {
        core::arch::wasm32::memory_size(0)
    }

    #[inline]
    fn grow(&mut self, slices: usize) -> Option<usize> {
        // memory.grow returns the previous size in pages, or usize::MAX on failure. The previous
        // size is the index of the first new page.
        let prev = core::arch::wasm32::memory_grow(0, slices);
        if prev == usize::MAX { None } else { Some(prev) }
    }

    #[inline]
    fn ptr(&self, addr: usize) -> *mut u8 {
        core::ptr::with_exposed_provenance_mut(addr)
    }
}

/// A simulated linear memory inside a caller-provided region, for the host.
///
/// Addresses are real host addresses, so the page-mask lookups behave exactly as on wasm
/// provided the region is aligned to the largest page size it will hold: `from_region` only
/// requires slice (64 KiB) alignment so that small Kani harnesses can use a 64 KiB buffer, and
/// `testing::Region` provides 4 MiB alignment for everything else. `grow` zero-fills the new
/// slices to match wasm semantics, and `skip_slices` lets tests model another party growing
/// memory so the allocator sees a non-contiguous region.
#[derive(Debug)]
pub struct SimMemory {
    base: *mut u8,
    /// Slice index of the region start (`base` address / SLICE_SIZE).
    first_slice: usize,
    /// Total slices the region can provide.
    capacity_slices: usize,
    /// Current simulated `memory.size`, as an absolute slice index of the end.
    end_slice: usize,
    heap_base: usize,
}

impl SimMemory {
    /// Wrap `len` bytes at `base` as a linear memory whose initial size covers `initial_slices`
    /// slices and whose heap starts `heap_base_offset` bytes into the region.
    ///
    /// The region must be aligned to `SLICE_SIZE`, `len` must be a multiple of `SLICE_SIZE`,
    /// `initial_slices * SLICE_SIZE <= len`, and `heap_base_offset` must be below the initial
    /// size. Panics otherwise (this is test infrastructure).
    ///
    /// # Safety
    ///
    /// `base..base + len` must be valid for reads and writes for the lifetime of the returned
    /// value and not accessed by anyone else.
    pub unsafe fn from_region(
        base: *mut u8,
        len: usize,
        initial_slices: usize,
        heap_base_offset: usize,
    ) -> Self {
        let addr = base as usize;
        assert!(addr % SLICE_SIZE == 0, "region must be slice aligned");
        assert!(len % SLICE_SIZE == 0, "region length must be whole slices");
        assert!(
            initial_slices * SLICE_SIZE <= len,
            "initial size exceeds region"
        );
        assert!(
            heap_base_offset < initial_slices * SLICE_SIZE,
            "heap base beyond initial size"
        );
        let first_slice = addr / SLICE_SIZE;
        assert!(
            first_slice + len / SLICE_SIZE - 1 <= MAX_SLICE_INDEX
                || cfg!(not(target_pointer_width = "32"))
        );
        SimMemory {
            base,
            first_slice,
            capacity_slices: len / SLICE_SIZE,
            end_slice: first_slice + initial_slices,
            heap_base: addr + heap_base_offset,
        }
    }

    /// Model someone else growing memory: the next `slices` slices are consumed and never
    /// handed to the allocator, so its next `grow` is not contiguous with the previous one.
    pub fn skip_slices(&mut self, slices: usize) -> bool {
        if self.end_slice + slices > self.first_slice + self.capacity_slices {
            return false;
        }
        self.end_slice += slices;
        true
    }

    /// Slices still available to `grow`.
    pub fn remaining_slices(&self) -> usize {
        self.first_slice + self.capacity_slices - self.end_slice
    }
}

// SAFETY: the region is exclusively ours by the contract of `from_region`; `grow` hands out
// disjoint, zero-filled slices strictly below `capacity`; `ptr` derives from `base`, which has
// provenance over the whole region.
unsafe impl Memory for SimMemory {
    fn heap_base(&self) -> usize {
        self.heap_base
    }

    fn size_slices(&self) -> usize {
        self.end_slice
    }

    fn grow(&mut self, slices: usize) -> Option<usize> {
        let end = self.end_slice.checked_add(slices)?;
        if end > self.first_slice + self.capacity_slices {
            return None;
        }
        let first_new = self.end_slice;
        let offset = (first_new - self.first_slice) * SLICE_SIZE;
        // SAFETY: the new slices lie inside the region (checked above) and nothing else refers to
        // them; wasm hands out zeroed pages, so zero them here to keep the two backends identical.
        unsafe { core::ptr::write_bytes(self.base.add(offset), 0, slices * SLICE_SIZE) };
        self.end_slice = end;
        Some(first_new)
    }

    #[inline]
    fn ptr(&self, addr: usize) -> *mut u8 {
        debug_assert!(addr >= self.base as usize);
        debug_assert!(addr < (self.first_slice + self.capacity_slices) * SLICE_SIZE);
        self.base.with_addr(addr)
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    //! Host regions for simulated memories: 4 MiB-aligned so that every page kind's address mask
    //! behaves exactly as on wasm.
    use super::*;
    use crate::bins::LARGE_PAGE_SIZE;
    use std::alloc::{Layout, alloc, dealloc};

    /// A 4 MiB-aligned region of host memory, freed on drop.
    ///
    /// The region comes from the host allocator, so its pages are committed lazily by the OS: a
    /// multi-gigabyte region costs nothing until a simulated memory grows into it.
    pub struct HostRegion {
        ptr: *mut u8,
        layout: Layout,
    }

    impl HostRegion {
        /// A region of `total_slices` slices. Panics if the host cannot provide it.
        pub fn new(total_slices: usize) -> Self {
            let layout =
                Layout::from_size_align(total_slices * SLICE_SIZE, LARGE_PAGE_SIZE).unwrap();
            // SAFETY: the layout has a non-zero size.
            let ptr = unsafe { alloc(layout) };
            assert!(!ptr.is_null(), "failed to allocate test region");
            HostRegion { ptr, layout }
        }

        /// Start of the region.
        pub fn as_ptr(&self) -> *mut u8 {
            self.ptr
        }

        /// Length of the region in bytes.
        pub fn len(&self) -> usize {
            self.layout.size()
        }

        /// Whether the region is empty (never: a region holds at least one slice).
        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }

        /// A simulated linear memory over the whole region, initially `initial_slices` long
        /// with its heap `heap_base_offset` bytes in.
        ///
        /// # Safety
        ///
        /// The returned memory must be dropped before the region, and no other `SimMemory` may
        /// be live over the same region at the same time.
        pub unsafe fn simulate(&self, initial_slices: usize, heap_base_offset: usize) -> SimMemory {
            // SAFETY: the region is valid for its whole length while `self` lives, and the
            // caller promises exclusivity and that the memory does not outlive the region.
            unsafe {
                SimMemory::from_region(self.ptr, self.len(), initial_slices, heap_base_offset)
            }
        }
    }

    impl Drop for HostRegion {
        fn drop(&mut self) {
            // SAFETY: allocated in `new` with this layout.
            unsafe { dealloc(self.ptr, self.layout) };
        }
    }

    /// A [`HostRegion`] together with the `SimMemory` over it.
    pub struct Region {
        // Declared after `mem` so that the memory is dropped before the region it points into.
        /// The simulated linear memory over the region.
        pub mem: SimMemory,
        _region: HostRegion,
    }

    impl Region {
        /// A region of `total_slices` slices whose simulated memory starts at `initial_slices`
        /// and whose heap begins `heap_base_offset` bytes in. Panics if the host cannot
        /// provide the region.
        pub fn new(total_slices: usize, initial_slices: usize, heap_base_offset: usize) -> Self {
            let region = HostRegion::new(total_slices);
            // SAFETY: `mem` is the only memory over the region and drops before it (field order).
            let mem = unsafe { region.simulate(initial_slices, heap_base_offset) };
            Region {
                mem,
                _region: region,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Region;
    use super::*;

    #[test]
    fn sim_grows_zeroed_and_contiguously_until_skipped() {
        let mut r = Region::new(16, 2, 100);
        let base_slice = r.mem.base as usize / SLICE_SIZE;
        assert_eq!(r.mem.size_slices(), base_slice + 2);
        assert_eq!(r.mem.heap_base(), r.mem.base as usize + 100);

        // Dirty a future slice, then grow into it: grow must return it zeroed.
        // SAFETY: inside the region we own.
        unsafe { *r.mem.base.add(2 * SLICE_SIZE + 17) = 0xAB };
        let s = r.mem.grow(3).unwrap();
        assert_eq!(s, base_slice + 2);
        assert_eq!(r.mem.size_slices(), base_slice + 5);
        // SAFETY: the grown slice is valid memory.
        assert_eq!(unsafe { *r.mem.ptr(s * SLICE_SIZE + 17) }, 0);

        assert!(r.mem.skip_slices(1));
        let t = r.mem.grow(1).unwrap();
        assert_eq!(t, base_slice + 6, "grow after a skip is not contiguous");
        assert_eq!(r.mem.remaining_slices(), 16 - 7);
        assert_eq!(r.mem.grow(100), None);
        assert_eq!(r.mem.grow(9).unwrap(), base_slice + 7);
        assert_eq!(r.mem.grow(1), None);
    }

    #[test]
    fn sim_pointers_round_trip_addresses() {
        let r = Region::new(2, 2, 0);
        let addr = r.mem.base as usize + SLICE_SIZE + 8;
        let p = r.mem.ptr(addr);
        assert_eq!(p as usize, addr);
        // SAFETY: inside the region.
        unsafe {
            p.cast::<u32>().write(0xDEAD_BEEF);
            assert_eq!(r.mem.ptr(addr).cast::<u32>().read(), 0xDEAD_BEEF);
        }
    }
}
