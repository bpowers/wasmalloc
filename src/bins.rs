//! Size classes ("bins") and the page geometry that follows from them.
//!
//! Pure arithmetic, no memory access, no state. This module is the single source of truth
//! for how a [`Layout`] maps to a block size, a page kind, and the offset of the first block
//! inside a page. `alloc`, `dealloc` and `realloc` all recompute the same classification from
//! the Layout they are given, which is what lets the hot paths find a page header with one
//! address mask and never consult a page map.
//!
//! The bin spacing is mimalloc's (`page-queue.c:mi_bin`, `init.c:MI_PAGE_QUEUES_EMPTY`) read
//! with an 8-byte word: exact classes every 8 bytes up to 64 bytes, then four classes per
//! doubling. Unlike C mimalloc we do not skip the 24/40/56-byte classes, because Rust tells us
//! the alignment and can therefore prove those classes are only used by 8-byte-aligned requests.
//!
//! Properties the tests and Kani harnesses pin down (all sizes `s` in `1..=LARGE_MAX_OBJ_SIZE`):
//!
//! - `bin(s)` is in `1..=MAX_BIN` and is monotone in `s`.
//! - `bin_size(bin(s)) >= s`, and `bin(bin_size(b)) == b` (bins are tight).
//! - internal waste is bounded: `bin_size(bin(s)) <= s + max(7, s / 4)`.
//! - alignment by construction: `bin_size(bin(s))` is a multiple of the largest power of two
//!   that divides `s`. Together with [`block_start`] this means a request of size `s` and
//!   alignment `a <= MAX_NATURAL_ALIGN` with `a | s` lands on an `a`-aligned block with no
//!   over-allocation and no interior pointers.

use core::alloc::Layout;

/// Bytes per "word" for bin spacing. mimalloc uses the machine word; we fix 8 so that wasm32 and
/// the 64-bit test host produce identical classes, and because `i64`/`f64` need 8-byte alignment
/// on wasm32 anyway.
pub const WORD: usize = 8;

/// Smallest block. Also the alignment every block has regardless of its class.
pub const MIN_BLOCK_SIZE: usize = WORD;

/// One wasm page: the unit of `memory.grow` and of our slice bitmap.
pub const SLICE_SIZE: usize = 64 * 1024;

/// Page sizes per kind. These are mimalloc's 64-bit constants; every page is aligned to its own
/// size so that `ptr & !(size - 1)` finds its header.
pub const SMALL_PAGE_SIZE: usize = SLICE_SIZE;
/// See [`SMALL_PAGE_SIZE`].
pub const MEDIUM_PAGE_SIZE: usize = 8 * SLICE_SIZE;
/// See [`SMALL_PAGE_SIZE`].
pub const LARGE_PAGE_SIZE: usize = 64 * SLICE_SIZE;

/// Largest block served from each page kind. Each is exactly a bin size (asserted below), chosen
/// so a page holds at least six blocks: mimalloc's `(page - 4 KiB) / 6` snapped down to a bin.
pub const SMALL_MAX_OBJ_SIZE: usize = 10 * 1024;
/// See [`SMALL_MAX_OBJ_SIZE`].
pub const MEDIUM_MAX_OBJ_SIZE: usize = 80 * 1024;
/// See [`SMALL_MAX_OBJ_SIZE`]. Anything larger, or anything with an alignment above
/// [`MAX_NATURAL_ALIGN`], is a header-less singleton run of slices.
pub const LARGE_MAX_OBJ_SIZE: usize = 512 * 1024;

/// Largest alignment satisfied inside a binned page. Requests above this get their own run of
/// slices, which are 64 KiB-aligned (or aligned to the request when it is larger still).
pub const MAX_NATURAL_ALIGN: usize = 4096;

/// Bytes reserved at the start of a page for its header. `block_start` never returns less.
pub const PAGE_HEADER_RESERVE: usize = 32;

/// Largest size served by the direct table (`pages_direct[direct_index(size)]`).
pub const DIRECT_MAX_SIZE: usize = 1024;
/// Entries in the direct table: one per multiple of `WORD` up to `DIRECT_MAX_SIZE`, plus zero.
pub const DIRECT_ENTRIES: usize = DIRECT_MAX_SIZE / WORD + 1;

/// Highest valid bin. Bin 0 is never produced by [`bin`]; it exists so that the direct table
/// can map a zero-size request to the smallest class without a branch.
pub const MAX_BIN: u8 = 60;
/// Number of bins including the unused bin 0.
pub const BIN_COUNT: usize = MAX_BIN as usize + 1;
/// Sentinel returned by [`bin`] for sizes above [`LARGE_MAX_OBJ_SIZE`]. Not a valid bin.
pub const BIN_HUGE: u8 = MAX_BIN + 1;

/// Largest bin of each page kind.
pub const SMALL_MAX_BIN: u8 = bin(SMALL_MAX_OBJ_SIZE);
/// See [`SMALL_MAX_BIN`].
pub const MEDIUM_MAX_BIN: u8 = bin(MEDIUM_MAX_OBJ_SIZE);
/// See [`SMALL_MAX_BIN`].
pub const LARGE_MAX_BIN: u8 = bin(LARGE_MAX_OBJ_SIZE);

const _: () = {
    assert!(bin_size(SMALL_MAX_BIN) == SMALL_MAX_OBJ_SIZE);
    assert!(bin_size(MEDIUM_MAX_BIN) == MEDIUM_MAX_OBJ_SIZE);
    assert!(bin_size(LARGE_MAX_BIN) == LARGE_MAX_OBJ_SIZE);
    assert!(LARGE_MAX_BIN == MAX_BIN);
    assert!((bin(DIRECT_MAX_SIZE) as usize) < BIN_COUNT);
    assert!(PAGE_HEADER_RESERVE % WORD == 0);
    assert!(MAX_NATURAL_ALIGN <= SLICE_SIZE);
};

/// Which kind of page a bin's blocks live in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind {
    /// 64 KiB page, blocks up to [`SMALL_MAX_OBJ_SIZE`].
    Small,
    /// 512 KiB page, blocks up to [`MEDIUM_MAX_OBJ_SIZE`].
    Medium,
    /// 4 MiB page, blocks up to [`LARGE_MAX_OBJ_SIZE`].
    Large,
}

impl PageKind {
    /// Page size in bytes; every page of this kind is aligned to this value.
    #[inline]
    pub const fn page_size(self) -> usize {
        match self {
            PageKind::Small => SMALL_PAGE_SIZE,
            PageKind::Medium => MEDIUM_PAGE_SIZE,
            PageKind::Large => LARGE_PAGE_SIZE,
        }
    }

    /// Mask that turns any address inside a page of this kind into the page's start address.
    #[inline]
    pub const fn page_mask(self) -> usize {
        !(self.page_size() - 1)
    }
}

/// How a request is served.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// From a page of the given bin; the block size is `bin_size(bin)`.
    Bin(u8),
    /// From a header-less run of whole slices (see the `slices` module).
    Huge,
}

/// Round `size` up to a multiple of the power of two `align`. Caller guarantees `align` is a
/// power of two and `size + align` does not overflow.
#[inline]
const fn round_up_pow2(size: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (size + align - 1) & !(align - 1)
}

/// Bin for a block of `size` bytes: `1..=MAX_BIN`, or [`BIN_HUGE`] above [`LARGE_MAX_OBJ_SIZE`].
/// Size 0 maps to bin 1.
#[inline]
pub const fn bin(size: usize) -> u8 {
    if size <= 8 * WORD {
        if size <= WORD {
            1
        } else {
            size.div_ceil(WORD) as u8
        }
    } else if size > LARGE_MAX_OBJ_SIZE {
        BIN_HUGE
    } else {
        // Sizes above 64 bytes get four bins per doubling. With w = wsize - 1 and b the index of
        // its top bit, the two bits below the top bit pick the quarter; the -3 accounts for the
        // eight exact bins that precede (mimalloc's derivation, page-queue.c:82-95).
        let w = size.div_ceil(WORD) - 1;
        let b = (usize::BITS - 1 - w.leading_zeros()) as usize;
        (((b << 2) + ((w >> (b - 2)) & 3)) - 3) as u8
    }
}

/// Block size of a bin in bytes. `bin` must be in `1..=MAX_BIN`.
#[inline]
pub const fn bin_size(bin: u8) -> usize {
    debug_assert!(bin >= 1 && bin <= MAX_BIN);
    if bin <= 8 {
        bin as usize * WORD
    } else {
        // Inverse of the formula in `bin`: bin + 3 = 4b + m, and the largest wsize in the bin
        // is (5 + m) << (b - 2).
        let k = bin as usize + 3;
        let b = k >> 2;
        let m = k & 3;
        ((5 + m) << (b - 2)) * WORD
    }
}

/// Page kind holding blocks of `bin`. `bin` must be in `1..=MAX_BIN`.
#[inline]
pub const fn kind_of_bin(bin: u8) -> PageKind {
    debug_assert!(bin >= 1 && bin <= MAX_BIN);
    if bin <= SMALL_MAX_BIN {
        PageKind::Small
    } else if bin <= MEDIUM_MAX_BIN {
        PageKind::Medium
    } else {
        PageKind::Large
    }
}

/// Index into the direct table for sizes up to [`DIRECT_MAX_SIZE`].
#[inline]
pub const fn direct_index(size: usize) -> usize {
    debug_assert!(size <= DIRECT_MAX_SIZE);
    size.div_ceil(WORD)
}

/// Classify a request. This is the one function that decides how a Layout is served; alloc,
/// dealloc and realloc must all go through it so they agree.
///
/// Alignments above [`MAX_NATURAL_ALIGN`] and sizes above [`LARGE_MAX_OBJ_SIZE`] become
/// [`Class::Huge`]. Otherwise the size is rounded up to a multiple of the alignment (a no-op for
/// well-formed type layouts, whose size is already a multiple of their alignment) and binned;
/// the alignment property documented at the top of the module then guarantees the block is
/// aligned.
#[inline]
pub const fn classify(layout: Layout) -> Class {
    let align = layout.align();
    if align > MAX_NATURAL_ALIGN {
        return Class::Huge;
    }
    // Layout guarantees size <= isize::MAX, so adding align - 1 (< 4096) cannot overflow.
    let size = if align > WORD {
        round_up_pow2(layout.size(), align)
    } else {
        layout.size()
    };
    if size > LARGE_MAX_OBJ_SIZE {
        Class::Huge
    } else {
        Class::Bin(bin(size))
    }
}

/// Block size a binned request will actually occupy: `bin_size(bin)` for `Class::Bin`. Callers
/// use it for `realloc` decisions without touching the page header.
#[inline]
pub const fn class_block_size(class: Class) -> Option<usize> {
    match class {
        Class::Bin(b) => Some(bin_size(b)),
        Class::Huge => None,
    }
}

/// Offset of the first block in a page whose blocks are `block_size` bytes.
///
/// The header occupies the first [`PAGE_HEADER_RESERVE`] bytes; the first block starts at the
/// next multiple of the largest power of two dividing `block_size`, capped at
/// [`MAX_NATURAL_ALIGN`]. Since pages are aligned to their size (a multiple of 64 KiB) and every
/// block size is a multiple of the alignment it can be asked for, every block in the page is
/// then aligned to every alignment its class serves.
#[inline]
pub const fn block_start(block_size: usize) -> usize {
    debug_assert!(block_size >= MIN_BLOCK_SIZE && block_size % WORD == 0);
    let natural = block_size & block_size.wrapping_neg();
    let align = if natural > MAX_NATURAL_ALIGN {
        MAX_NATURAL_ALIGN
    } else {
        natural
    };
    round_up_pow2(PAGE_HEADER_RESERVE, align)
}

/// Number of blocks a fresh page of `kind` holds for `block_size`.
#[inline]
pub const fn blocks_per_page(kind: PageKind, block_size: usize) -> usize {
    (kind.page_size() - block_start(block_size)) / block_size
}

#[cfg(test)]
mod tests {
    use super::*;

    /// mimalloc's `MI_PAGE_QUEUES_EMPTY` table (init.c) read with an 8-byte word. Index 0 is a
    /// placeholder, as in mimalloc.
    const REFERENCE_BIN_SIZES: [usize; BIN_COUNT] = [
        8, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512,
        640, 768, 896, 1024, 1280, 1536, 1792, 2048, 2560, 3072, 3584, 4096, 5120, 6144, 7168,
        8192, 10240, 12288, 14336, 16384, 20480, 24576, 28672, 32768, 40960, 49152, 57344, 65536,
        81920, 98304, 114688, 131072, 163840, 196608, 229376, 262144, 327680, 393216, 458752,
        524288,
    ];

    fn largest_pow2_divisor(s: usize) -> usize {
        s & s.wrapping_neg()
    }

    #[test]
    fn bin_sizes_match_mimalloc_table() {
        for b in 1..=MAX_BIN {
            assert_eq!(bin_size(b), REFERENCE_BIN_SIZES[b as usize], "bin {b}");
        }
    }

    #[test]
    fn bins_are_tight_and_monotone() {
        let mut prev = bin(1);
        assert_eq!(prev, 1);
        assert_eq!(bin(0), 1);
        for s in 1..=LARGE_MAX_OBJ_SIZE {
            let b = bin(s);
            assert!((1..=MAX_BIN).contains(&b), "size {s} -> bin {b}");
            assert!(b >= prev, "bin not monotone at {s}");
            assert!(bin_size(b) >= s, "bin {b} too small for {s}");
            let waste_bound = if s <= 64 { 7 } else { s / 4 };
            assert!(
                bin_size(b) <= s + waste_bound,
                "size {s} -> {} wastes too much",
                bin_size(b)
            );
            prev = b;
        }
        assert_eq!(bin(LARGE_MAX_OBJ_SIZE + 1), BIN_HUGE);
        assert_eq!(bin(usize::MAX), BIN_HUGE);
        for b in 1..=MAX_BIN {
            assert_eq!(bin(bin_size(b)), b, "round trip for bin {b}");
            if b < MAX_BIN {
                assert_eq!(bin(bin_size(b) + 1), b + 1, "next size after bin {b}");
            }
        }
    }

    #[test]
    fn bin_size_is_multiple_of_natural_alignment() {
        for s in 1..=LARGE_MAX_OBJ_SIZE {
            let bs = bin_size(bin(s));
            assert_eq!(bs % largest_pow2_divisor(s), 0, "size {s} -> bin size {bs}");
        }
    }

    #[test]
    fn kind_boundaries() {
        assert_eq!(kind_of_bin(1), PageKind::Small);
        assert_eq!(kind_of_bin(SMALL_MAX_BIN), PageKind::Small);
        assert_eq!(kind_of_bin(SMALL_MAX_BIN + 1), PageKind::Medium);
        assert_eq!(kind_of_bin(MEDIUM_MAX_BIN), PageKind::Medium);
        assert_eq!(kind_of_bin(MEDIUM_MAX_BIN + 1), PageKind::Large);
        assert_eq!(kind_of_bin(MAX_BIN), PageKind::Large);
        assert_eq!(bin(SMALL_MAX_OBJ_SIZE), SMALL_MAX_BIN);
        assert_eq!(kind_of_bin(bin(SMALL_MAX_OBJ_SIZE + 1)), PageKind::Medium);
        assert_eq!(kind_of_bin(bin(MEDIUM_MAX_OBJ_SIZE + 1)), PageKind::Large);
        for kind in [PageKind::Small, PageKind::Medium, PageKind::Large] {
            assert!(kind.page_size().is_power_of_two());
            assert_eq!(kind.page_size() % SLICE_SIZE, 0);
            assert_eq!(
                0x1234_5678usize & kind.page_mask(),
                0x1234_5678 / kind.page_size() * kind.page_size()
            );
        }
    }

    #[test]
    fn every_page_holds_at_least_six_blocks_and_first_block_is_aligned() {
        for b in 1..=MAX_BIN {
            let bs = bin_size(b);
            let kind = kind_of_bin(b);
            let start = block_start(bs);
            assert!(start >= PAGE_HEADER_RESERVE);
            let natural = largest_pow2_divisor(bs).min(MAX_NATURAL_ALIGN);
            assert_eq!(start % natural, 0, "bin {b}");
            assert!(start < kind.page_size());
            assert!(
                blocks_per_page(kind, bs) >= 6,
                "bin {b} ({bs} B) in {kind:?}"
            );
            assert!(blocks_per_page(kind, bs) <= u16::MAX as usize);
        }
        assert_eq!(
            blocks_per_page(PageKind::Small, 8),
            (SMALL_PAGE_SIZE - 32) / 8
        );
    }

    #[test]
    fn classify_serves_alignment_by_construction() {
        for shift in 0..=12 {
            let align = 1usize << shift;
            for size in 1..=(LARGE_MAX_OBJ_SIZE / 16) {
                let layout = Layout::from_size_align(size, align).unwrap();
                match classify(layout) {
                    Class::Bin(b) => {
                        let bs = bin_size(b);
                        assert!(bs >= size);
                        assert_eq!(bs % align, 0, "size {size} align {align} -> {bs}");
                        assert_eq!(
                            block_start(bs) % align,
                            0,
                            "block_start for {bs} vs align {align}"
                        );
                    }
                    Class::Huge => panic!("size {size} align {align} should be binned"),
                }
            }
        }
        // Alignment above the natural cap, or size above the large cap, is a singleton run.
        assert_eq!(
            classify(Layout::from_size_align(1, 8192).unwrap()),
            Class::Huge
        );
        assert_eq!(
            classify(Layout::from_size_align(LARGE_MAX_OBJ_SIZE + 1, 1).unwrap()),
            Class::Huge
        );
        assert_eq!(
            classify(Layout::from_size_align(LARGE_MAX_OBJ_SIZE, 1).unwrap()),
            Class::Bin(MAX_BIN)
        );
        // Rounding up to the alignment can push a request over the large cap.
        assert_eq!(
            classify(Layout::from_size_align(LARGE_MAX_OBJ_SIZE - 1, 4096).unwrap()),
            Class::Bin(MAX_BIN)
        );
        assert_eq!(
            classify(Layout::from_size_align(LARGE_MAX_OBJ_SIZE + 1, 4096).unwrap()),
            Class::Huge
        );
        // Small alignments never change the class.
        for size in 1..=DIRECT_MAX_SIZE {
            let a1 = classify(Layout::from_size_align(size, 1).unwrap());
            let a8 = classify(Layout::from_size_align(size, 8).unwrap());
            assert_eq!(a1, a8);
            assert_eq!(a1, Class::Bin(bin(size)));
        }
    }

    #[test]
    fn direct_index_covers_the_small_range() {
        assert_eq!(direct_index(0), 0);
        assert_eq!(direct_index(1), 1);
        assert_eq!(direct_index(8), 1);
        assert_eq!(direct_index(9), 2);
        assert_eq!(direct_index(DIRECT_MAX_SIZE), DIRECT_ENTRIES - 1);
        for size in 0..=DIRECT_MAX_SIZE {
            let idx = direct_index(size);
            assert!(idx < DIRECT_ENTRIES);
            // Every size sharing a direct slot shares a bin, so one page pointer per slot works.
            assert_eq!(bin(size), bin(idx * WORD));
        }
    }
}

#[cfg(kani)]
mod verify {
    use super::*;

    #[kani::proof]
    fn bin_covers_and_bounds_waste() {
        let s: usize = kani::any();
        kani::assume(s >= 1 && s <= LARGE_MAX_OBJ_SIZE);
        let b = bin(s);
        assert!(b >= 1 && b <= MAX_BIN);
        let bs = bin_size(b);
        assert!(bs >= s);
        let waste_bound = if s <= 64 { 7 } else { s / 4 };
        assert!(bs <= s + waste_bound);
        assert!(bs % (s & s.wrapping_neg()) == 0);
    }

    #[kani::proof]
    fn bins_are_tight() {
        let b: u8 = kani::any();
        kani::assume(b >= 1 && b <= MAX_BIN);
        assert!(bin(bin_size(b)) == b);
        if b < MAX_BIN {
            assert!(bin(bin_size(b) + 1) == b + 1);
        }
    }

    #[kani::proof]
    fn bin_is_monotone() {
        let s: usize = kani::any();
        kani::assume(s < LARGE_MAX_OBJ_SIZE);
        assert!(bin(s) <= bin(s + 1));
    }

    #[kani::proof]
    fn classify_aligns_by_construction() {
        let size: usize = kani::any();
        let shift: u32 = kani::any();
        kani::assume(size >= 1 && size <= LARGE_MAX_OBJ_SIZE && shift <= 12);
        let align = 1usize << shift;
        let layout = Layout::from_size_align(size, align).unwrap();
        if let Class::Bin(b) = classify(layout) {
            let bs = bin_size(b);
            assert!(bs >= size);
            assert!(bs % align == 0);
            assert!(block_start(bs) % align == 0);
            assert!(block_start(bs) + bs <= kind_of_bin(b).page_size());
        }
    }
}
