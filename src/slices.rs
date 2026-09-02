//! The free-slice bitmap and the `memory.grow` policy.
//!
//! Everything above the [`Memory`] backend gets its memory here, in whole 64 KiB slices: one
//! slice per small page, eight per medium page, sixty-four per large page, and an arbitrary run
//! for a singleton. A [`SliceMap`] records which slices the allocator owns and has not handed
//! out (`free`) and which of those still hold the zeros `memory.grow` gave them (`zero`, always a
//! subset of `free`). It is mimalloc's `slices_free` / `slices_dirty` pair (arena.c, bitmap.c)
//! without atomics, chunks, or per-chunk size bins.
//!
//! Indices are absolute slice numbers (`address / SLICE_SIZE`); the map stores a `base` and
//! converts. Alignment is always on the absolute index, because page headers are found by
//! masking absolute addresses. `base` is rounded down to a multiple of 64 so that one word of
//! the bitmap is exactly a 64-slice-aligned (4 MiB-aligned) stretch of memory. That makes the
//! three page sizes trivial to find: a small page is any set bit, a medium page is any all-ones
//! byte, a large page is any all-ones word. Other requests go through a general first-fit scan
//! over maximal runs of free slices, which is correct for any count and any power-of-two
//! alignment and only needs to be fast enough for singleton allocations.
//!
//! Searches return the lowest-addressed fit. Together with the growth policy in [`acquire`] this
//! keeps the heap dense at the bottom of memory so that the top can be grown in large steps:
//! `memory.grow` costs tens of microseconds in V8 regardless of size (see
//! `docs/research/landscape.md`), so growth is geometric in the heap size, and the linker gap
//! between `__heap_base` and the initial `memory.size` is reclaimed via [`initial_free_range`]
//! before the first grow.
//!
//! The exception is a run that is being grown by `realloc`. [`extend_with_growth`] extends a
//! run in place through the free slices after it and, when the run reaches the end of memory,
//! through `memory.grow` itself, so a buffer at the top of the heap grows without a copy. A run
//! that cannot be extended is moved with [`SliceMap::alloc_tail`] to the bottom of the free
//! tail, the free slices that reach the end of memory, so that it lands at the top with the
//! rest of the tail above it and its next growths are in place; this is dlmalloc's top chunk.
//! A slice map full of holes (retired pages waiting to be released) would otherwise put every
//! doubling of a growing buffer into a new hole and copy it again at the next step. Placing
//! the run at the highest fit instead, with nothing free above it, makes every growth a
//! `memory.grow` of half the heap and runs memory to the 4 GiB limit within a few rounds.
//!
//! Invariants maintained by every method (checked by the tests and the Kani harnesses):
//! - `zero` implies `free`;
//! - every word below `hint` has no free bit;
//! - a returned run starts at a multiple of its alignment, its slices were all free before and
//!   none is free afterwards, other bits are untouched, and no aligned run of the same length
//!   exists at a lower address.
//!
//! Slice [`MAX_SLICE_INDEX`] (65535, the last 64 KiB of a 4 GiB memory) is never handed out: a
//! run ending there would end at address 2^32, which overflows a wasm32 `usize` and is an
//! address no allocated object may reach. [`SliceMap::add_free`] drops it and [`acquire`] never
//! grows memory onto it, so nothing above this module has to know.
//!
//! No unsafe code: this module never touches memory, only bookkeeping about it.

use crate::backend::{MAX_SLICE_INDEX, Memory};
use crate::bins::SLICE_SIZE;

/// Bits per bitmap word.
const BITS: usize = 64;

/// A run of slices handed out by [`SliceMap::alloc`] or [`acquire`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// Absolute index of the first slice.
    pub start: usize,
    /// Whether every slice of the run was still in the known-zero state, so a zeroed allocation
    /// can skip clearing it.
    pub zeroed: bool,
}

/// Free-slice bitmap over `64 * WORDS` consecutive slices.
///
/// The default of 1024 words covers 65536 slices, the whole wasm32 address space, in 16 KiB of
/// static state; tests and proofs use small `WORDS`. See the module documentation for the index
/// convention and the invariants.
#[derive(Clone)]
pub struct SliceMap<const WORDS: usize = 1024> {
    /// Bit set: the slice belongs to the allocator and is not handed out.
    free: [u64; WORDS],
    /// Bit set: the slice is free and still all zero (never handed out since `memory.grow`).
    zero: [u64; WORDS],
    /// Absolute index of the slice that bit 0 stands for; a multiple of 64.
    base: usize,
    /// Every word below this index has no free bit, so searches start here. Any value from 0
    /// (claims nothing, always valid) to `WORDS` (nothing is free) satisfies the invariant;
    /// [`new`](Self::new) leaves it at 0 so that the map's zero value is its initial state, and
    /// [`init`](Self::init) tightens it to `WORDS`.
    hint: usize,
}

impl<const WORDS: usize> Default for SliceMap<WORDS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const WORDS: usize> SliceMap<WORDS> {
    /// Number of slices the map can describe.
    pub const CAPACITY: usize = WORDS * BITS;

    /// An empty map: nothing free, base 0. Call [`init`](Self::init) before anything else.
    ///
    /// Every field is zero, so a map embedded in a static needs no initialiser data: the 16 KiB
    /// of bitmaps of the global allocator's map would otherwise be emitted as a data segment
    /// that the module carries and copies at instantiation (`docs/research/roofline.md` 12.6).
    /// `hint == 0` is a valid, merely loose, hint (see the field); `init` sets the tight one.
    pub const fn new() -> Self {
        const {
            assert!(
                WORDS >= 1 && WORDS * BITS <= MAX_SLICE_INDEX + 1,
                "a slice map never needs more than the wasm32 address space"
            )
        };
        SliceMap {
            free: [0; WORDS],
            zero: [0; WORDS],
            base: 0,
            hint: 0,
        }
    }

    /// Set the first slice the map describes. Runs once, before any other call.
    ///
    /// `base_slice` is rounded down to a multiple of 64 so that word boundaries fall on 64-slice
    /// boundaries of the address space (see the module documentation); the map then covers
    /// `[base(), limit())`. On wasm32 the default map spans the whole address space from slice 0,
    /// so nothing is lost; a test that passes an unaligned base to a small map must size it with
    /// the rounding in mind.
    pub fn init(&mut self, base_slice: usize) {
        debug_assert!(self.free.iter().all(|&w| w == 0), "init after use");
        let base = base_slice & !(BITS - 1);
        debug_assert!(base.checked_add(Self::CAPACITY).is_some());
        self.base = base;
        self.hint = WORDS;
    }

    /// Absolute index of the first slice the map describes.
    #[inline]
    pub fn base(&self) -> usize {
        self.base
    }

    /// One past the highest absolute slice index the map describes.
    #[inline]
    pub fn limit(&self) -> usize {
        self.base + Self::CAPACITY
    }

    /// One past the highest slice the map may ever hand out: [`limit`](Self::limit), except that
    /// a map spanning slice [`MAX_SLICE_INDEX`] stops short of it (see the module documentation).
    /// A map lying entirely above that index, such as one over a simulated memory at a high host
    /// address, is unaffected.
    #[inline]
    pub fn usable_limit(&self) -> usize {
        if self.base <= MAX_SLICE_INDEX {
            self.limit().min(MAX_SLICE_INDEX)
        } else {
            self.limit()
        }
    }

    /// Give `[start, start + count)` to the map as free slices. They must not be free already.
    /// `zeroed` says the memory is known to be all zero (fresh from `memory.grow`); the linker gap
    /// at startup is not. Slice [`MAX_SLICE_INDEX`] is dropped from the range, and an empty range
    /// is a no-op.
    pub fn add_free(&mut self, start: usize, count: usize, zeroed: bool) {
        let count = if start <= MAX_SLICE_INDEX {
            count.min(MAX_SLICE_INDEX - start)
        } else {
            count
        };
        if count == 0 {
            return;
        }
        let rel = self.rel(start, count);
        self.release(rel, count);
        if zeroed {
            set_bits(&mut self.zero, rel, count);
        }
    }

    /// Take the lowest-addressed run of `count` free slices whose absolute start is a multiple
    /// of `align` (a power of two, in slices). `count >= 1`. Returns `None` when no such run
    /// exists; the map is unchanged in that case.
    #[inline]
    pub fn alloc(&mut self, count: usize, align: usize) -> Option<Run> {
        debug_assert!(count >= 1);
        debug_assert!(align.is_power_of_two());
        self.advance_hint();
        let rel = self.find(count, align)?;
        let zeroed = self.claim(rel, count);
        Some(Run {
            start: self.base + rel,
            zeroed,
        })
    }

    /// Take `count` free slices from the bottom of the free tail: the maximal run of free slices
    /// that ends at the absolute slice index `below` (callers pass the end of memory). The run
    /// starts at the lowest slice of the tail whose absolute index is a multiple of `align` (a
    /// power of two, in slices), so whatever the tail holds beyond it stays free above the run.
    /// `count >= 1`. Returns `None` when the tail is empty or too short for that; the map is
    /// unchanged then. Nothing above the end of memory is ever free, so a `below` past the end
    /// of the map is harmless.
    pub fn alloc_tail(&mut self, count: usize, align: usize, below: usize) -> Option<Run> {
        debug_assert!(count >= 1);
        debug_assert!(align.is_power_of_two());
        let rel = self.find_tail(count, align, below)?;
        let zeroed = self.claim(rel, count);
        Some(Run {
            start: self.base + rel,
            zeroed,
        })
    }

    /// Return `[start, start + count)`, which must be handed out, to the map. The memory is
    /// dirty now, so the slices lose their zero bits for good (until `memory.grow` gives fresh
    /// ones).
    pub fn free(&mut self, start: usize, count: usize) {
        debug_assert!(count >= 1);
        let rel = self.rel(start, count);
        self.release(rel, count);
    }

    /// Grow the handed-out run `[start, start + count)` in place by `extra` slices if the
    /// slices right after it are all free. Returns whether the claimed tail was all zero
    /// (vacuously true for `extra == 0`), or `None` with the map unchanged when the tail is not
    /// entirely free or would leave the map.
    pub fn try_extend(&mut self, start: usize, count: usize, extra: usize) -> Option<bool> {
        debug_assert!(count >= 1);
        let rel = self.rel(start, count);
        debug_assert!(
            all_clear(&self.free, rel, count),
            "extending a run that is not handed out"
        );
        let tail = rel + count;
        if extra > Self::CAPACITY - tail || !all_set(&self.free, tail, extra) {
            return None;
        }
        Some(self.claim(tail, extra))
    }

    /// Free the tail `[start + new_count, start + count)` of the handed-out run
    /// `[start, start + count)`. `new_count <= count`; zero frees the whole run.
    pub fn shrink(&mut self, start: usize, count: usize, new_count: usize) {
        debug_assert!(new_count <= count);
        let rel = self.rel(start, count);
        debug_assert!(
            all_clear(&self.free, rel, count),
            "shrinking a run that is not handed out"
        );
        if new_count < count {
            self.release(rel + new_count, count - new_count);
        }
    }

    /// Whether slice `idx` is owned by the allocator and not handed out.
    pub fn is_free(&self, idx: usize) -> bool {
        self.bit_is_free(self.rel(idx, 1))
    }

    /// Whether slice `idx` is free and known to be all zero.
    pub fn is_zero(&self, idx: usize) -> bool {
        let rel = self.rel(idx, 1);
        (self.zero[rel / BITS] >> (rel % BITS)) & 1 == 1
    }

    /// Whether every slice of `[start, start + count)` is free; vacuously true for `count == 0`.
    /// The range must lie inside the map.
    pub fn run_is_free(&self, start: usize, count: usize) -> bool {
        let rel = self.rel(start, count);
        all_set(&self.free, rel, count)
    }

    /// Number of free slices.
    pub fn free_count(&self) -> usize {
        self.free.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Whether [`alloc`](Self::alloc) with these arguments would succeed.
    pub fn has_run(&self, count: usize, align: usize) -> bool {
        debug_assert!(count >= 1);
        debug_assert!(align.is_power_of_two());
        self.find(count, align).is_some()
    }

    /// Relative index of `start`, checking (in debug builds) that `[start, start + count)` lies
    /// inside the map. Indices outside the map are a caller bug.
    #[inline]
    fn rel(&self, start: usize, count: usize) -> usize {
        debug_assert!(start >= self.base, "slice {start} is below the map");
        let rel = start - self.base;
        debug_assert!(
            rel <= Self::CAPACITY && count <= Self::CAPACITY - rel,
            "slices {start}+{count} reach beyond the map"
        );
        rel
    }

    /// Move `hint` past words with no free bit so that searches start at a candidate word.
    #[inline]
    fn advance_hint(&mut self) {
        while self.hint < WORDS && self.free[self.hint] == 0 {
            self.hint += 1;
        }
    }

    /// Relative index of the lowest aligned run of `count` free slices. The three page sizes get
    /// dedicated word scans; everything else takes the general first-fit scan.
    #[inline]
    fn find(&self, count: usize, align: usize) -> Option<usize> {
        if count == align {
            if count == 1 {
                return self.find_single();
            }
            if count == 8 {
                return self.find_byte();
            }
            if count == 64 {
                return self.find_word();
            }
        }
        self.find_general(count, align)
    }

    /// Lowest free slice.
    #[inline]
    fn find_single(&self) -> Option<usize> {
        for w in self.hint..WORDS {
            let b = self.free[w];
            if b != 0 {
                return Some(w * BITS + b.trailing_zeros() as usize);
            }
        }
        None
    }

    /// Lowest 8-aligned run of 8 free slices. `base` is a multiple of 64, so such a run is
    /// exactly an all-ones byte of a word.
    #[inline]
    fn find_byte(&self) -> Option<usize> {
        for w in self.hint..WORDS {
            let x = full_bytes(self.free[w]);
            if x != 0 {
                return Some(w * BITS + x.trailing_zeros() as usize);
            }
        }
        None
    }

    /// Lowest 64-aligned run of 64 free slices: an all-ones word.
    #[inline]
    fn find_word(&self) -> Option<usize> {
        for w in self.hint..WORDS {
            if self.free[w] == u64::MAX {
                return Some(w * BITS);
            }
        }
        None
    }

    /// First fit over maximal runs of free slices, lowest address first.
    fn find_general(&self, count: usize, align: usize) -> Option<usize> {
        if count > Self::CAPACITY {
            return None;
        }
        let mut pos = self.hint * BITS;
        loop {
            let lo = self.next_set(pos)?;
            let hi = self.next_clear(lo);
            // The lowest aligned start inside this maximal run. A later aligned start would leave
            // even less room before `hi`, so if this one does not fit nothing in the run does.
            let start = self.align_up(lo, align)?;
            if start <= hi && hi - start >= count {
                return Some(start);
            }
            pos = hi;
        }
    }

    /// Lowest relative index at or above `rel` whose absolute index is a multiple of `align`
    /// (a power of two).
    #[inline]
    fn align_up(&self, rel: usize, align: usize) -> Option<usize> {
        let over = (self.base + rel) & (align - 1);
        if over == 0 {
            Some(rel)
        } else {
            rel.checked_add(align - over)
        }
    }

    /// Relative index of the lowest aligned start of `count` slices inside the free tail ending
    /// at the absolute index `below`, if the tail is that long.
    fn find_tail(&self, count: usize, align: usize, below: usize) -> Option<usize> {
        let end = below.saturating_sub(self.base).min(Self::CAPACITY);
        if end == 0 || !self.bit_is_free(end - 1) {
            return None;
        }
        let lo = self.prev_clear(end - 1).map_or(0, |c| c + 1);
        let start = self.align_up(lo, align)?;
        (start <= end && end - start >= count).then_some(start)
    }

    #[inline]
    fn bit_is_free(&self, rel: usize) -> bool {
        (self.free[rel / BITS] >> (rel % BITS)) & 1 == 1
    }

    /// Highest non-free slice strictly below `pos` (`pos <= CAPACITY`).
    #[inline]
    fn prev_clear(&self, pos: usize) -> Option<usize> {
        if pos == 0 {
            return None;
        }
        let mut w = (pos - 1) / BITS;
        let mut b = !self.free[w] & (u64::MAX >> (BITS - 1 - (pos - 1) % BITS));
        loop {
            if b != 0 {
                return Some(w * BITS + (BITS - 1 - b.leading_zeros() as usize));
            }
            if w == 0 {
                return None;
            }
            w -= 1;
            b = !self.free[w];
        }
    }

    /// Lowest free slice at or above `pos` (`pos <= CAPACITY`).
    #[inline]
    fn next_set(&self, pos: usize) -> Option<usize> {
        let mut w = pos / BITS;
        if w >= WORDS {
            return None;
        }
        let mut b = self.free[w] & (u64::MAX << (pos % BITS));
        loop {
            if b != 0 {
                return Some(w * BITS + b.trailing_zeros() as usize);
            }
            w += 1;
            if w >= WORDS {
                return None;
            }
            b = self.free[w];
        }
    }

    /// Lowest non-free slice at or above `pos` (`pos < CAPACITY`), or `CAPACITY`.
    #[inline]
    fn next_clear(&self, pos: usize) -> usize {
        let mut w = pos / BITS;
        // Bits below `pos` are treated as set so they cannot end the run early.
        let mut b = self.free[w] | !(u64::MAX << (pos % BITS));
        loop {
            if b != u64::MAX {
                return w * BITS + (!b).trailing_zeros() as usize;
            }
            w += 1;
            if w >= WORDS {
                return Self::CAPACITY;
            }
            b = self.free[w];
        }
    }

    /// Hand out `[rel, rel + count)`, which must be free. Returns whether all of it was zero.
    #[inline]
    fn claim(&mut self, rel: usize, count: usize) -> bool {
        debug_assert!(all_set(&self.free, rel, count));
        let mut zeroed = true;
        for (w, m) in word_masks(rel, count) {
            self.free[w] &= !m;
            zeroed &= self.zero[w] & m == m;
            self.zero[w] &= !m;
        }
        zeroed
    }

    /// Mark `[rel, rel + count)`, which must not be free, as free and dirty.
    #[inline]
    fn release(&mut self, rel: usize, count: usize) {
        debug_assert!(all_clear(&self.free, rel, count), "slices freed twice");
        debug_assert!(all_clear(&self.zero, rel, count));
        set_bits(&mut self.free, rel, count);
        self.hint = self.hint.min(rel / BITS);
    }
}

/// `n` consecutive bits starting at `bit`, with `1 <= n` and `bit + n <= 64`.
#[inline]
const fn mask(bit: usize, n: usize) -> u64 {
    debug_assert!(n >= 1 && bit + n <= BITS);
    // Shifting u64::MAX right by 64 - n (never 64 itself) sidesteps the undefined 1 << 64 that
    // the obvious (1 << n) - 1 hits for n == 64.
    (u64::MAX >> (BITS - n)) << bit
}

/// Bit `8k` of the result is set exactly when byte `k` of `b` is all ones; other bits are clear.
/// Folding each byte onto its lowest bit (bits 4 apart, then 2, then 1) computes the AND of all
/// eight bits, which is exact for every byte, unlike the carry-based trick mimalloc uses.
#[inline]
const fn full_bytes(b: u64) -> u64 {
    let x = b & (b >> 4);
    let x = x & (x >> 2);
    let x = x & (x >> 1);
    x & 0x0101_0101_0101_0101
}

/// The words a relative range touches, in order, each with the mask of its bits in the range.
struct WordMasks {
    pos: usize,
    end: usize,
}

impl Iterator for WordMasks {
    type Item = (usize, u64);

    #[inline]
    fn next(&mut self) -> Option<(usize, u64)> {
        if self.pos >= self.end {
            return None;
        }
        let w = self.pos / BITS;
        let bit = self.pos % BITS;
        let n = (BITS - bit).min(self.end - self.pos);
        self.pos += n;
        Some((w, mask(bit, n)))
    }
}

#[inline]
fn word_masks(rel: usize, count: usize) -> WordMasks {
    WordMasks {
        pos: rel,
        end: rel + count,
    }
}

#[inline]
fn all_set(bits: &[u64], rel: usize, count: usize) -> bool {
    word_masks(rel, count).all(|(w, m)| bits[w] & m == m)
}

#[inline]
fn all_clear(bits: &[u64], rel: usize, count: usize) -> bool {
    word_masks(rel, count).all(|(w, m)| bits[w] & m == 0)
}

#[inline]
fn set_bits(bits: &mut [u64], rel: usize, count: usize) {
    for (w, m) in word_masks(rel, count) {
        bits[w] |= m;
    }
}

/// How much to grow linear memory when the map runs dry, in slices.
///
/// Each `memory.grow` costs the same tens of microseconds in V8 12.4 whether it asks for one
/// page or a thousand (under a microsecond on V8 15.2, JavaScriptCore and wasmtime), so the step
/// is a fraction of the current heap, clamped to a range; a request larger than the step is
/// always granted in full. The fraction trades footprint for calls: a step of half the heap
/// overshoots the peak by up to 50 percent and costs about three calls per doubling of the heap,
/// an eighth overshoots by at most 12.5 percent and costs about six, which for a heap growing to
/// 64 MiB is some 2 ms of `memory.grow` in total on V8 12.4 and negligible elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrowPolicy {
    /// Smallest step.
    pub min_grow: usize,
    /// Largest step (a single request may still exceed it).
    pub max_grow: usize,
    /// The step is the heap size divided by this (an eighth for 8), before clamping. Non-zero.
    pub step_divisor: usize,
}

impl GrowPolicy {
    /// An eighth of the heap, between 1 MiB and 64 MiB per step.
    pub const DEFAULT: GrowPolicy = GrowPolicy {
        min_grow: 16,
        max_grow: 1024,
        step_divisor: 8,
    };

    /// The growth step for a heap of `heap` slices: the policy's fraction of it, clamped to the
    /// policy's range.
    #[inline]
    pub fn step(&self, heap: usize) -> usize {
        debug_assert!(self.step_divisor > 0);
        (heap / self.step_divisor)
            .max(self.min_grow)
            .min(self.max_grow)
    }
}

impl Default for GrowPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// [`SliceMap::alloc`], growing linear memory when the map has no fitting run.
///
/// The request is sized from the current end of memory: the slack up to the next aligned slice
/// plus `count`, or the geometric step of `policy` if that is larger. Should the fresh region
/// start elsewhere (something else grew memory in between, which the [`Memory`] contract allows)
/// that slack is wrong, so memory is grown once more by `count + align - 1`, which fits an
/// aligned run wherever it lands. Returns `None` when growth would push the end of memory past
/// [`SliceMap::usable_limit`], or when the engine refuses both the step and the bare request;
/// slices that were grown along the way stay in the map for later requests.
pub fn acquire<const WORDS: usize, M: Memory>(
    map: &mut SliceMap<WORDS>,
    mem: &mut M,
    count: usize,
    align: usize,
    policy: &GrowPolicy,
) -> Option<Run> {
    if let Some(run) = map.alloc(count, align) {
        return Some(run);
    }
    grow_and_alloc(map, mem, count, align, policy)
}

#[cold]
#[inline(never)]
fn grow_and_alloc<const WORDS: usize, M: Memory>(
    map: &mut SliceMap<WORDS>,
    mem: &mut M,
    count: usize,
    align: usize,
    policy: &GrowPolicy,
) -> Option<Run> {
    debug_assert!(count >= 1);
    debug_assert!(align.is_power_of_two());
    let limit = map.usable_limit();
    let end = mem.size_slices();
    // Fresh memory normally starts at the current end, so the request only needs the slack from
    // there to the next slice whose absolute index is aligned. `-end mod align` is that slack.
    let pad = end.wrapping_neg() & (align - 1);
    let needed = pad.checked_add(count)?;
    let room = limit.saturating_sub(end);
    if needed > room {
        return None;
    }
    let want = needed.max(growth_step(map, end, policy)).min(room);
    let (start, got) = match mem.grow(want) {
        Some(start) => (start, want),
        None if want > needed => (mem.grow(needed)?, needed),
        None => return None,
    };
    add_region(map, start, got);
    if let Some(run) = map.alloc(count, align) {
        return Some(run);
    }
    // The region did not start at `end`, so the slack was computed for the wrong place. The worst
    // case over every possible start is `count + align - 1`.
    let worst = count.checked_add(align - 1)?;
    if worst > limit.saturating_sub(mem.size_slices()) {
        return None;
    }
    let start = mem.grow(worst)?;
    add_region(map, start, worst);
    let run = map.alloc(count, align);
    debug_assert!(
        run.is_some() || start + worst > map.usable_limit(),
        "a region sized for any start cannot fail to serve the request"
    );
    run
}

/// The geometric step for growing memory whose end is at absolute slice `end`. The heap size it
/// is taken from runs from the map's base, not from slice 0: on the host the simulated memory
/// sits at an arbitrary absolute slice index, and on wasm the two differ only by the map's
/// rounded base.
#[inline]
fn growth_step<const WORDS: usize>(
    map: &SliceMap<WORDS>,
    end: usize,
    policy: &GrowPolicy,
) -> usize {
    policy.step(end.saturating_sub(map.base()))
}

/// [`SliceMap::try_extend`], growing linear memory when the run is at the top of the heap.
///
/// Grows the handed-out run `[start, start + count)` by `extra >= 1` slices in place. When the
/// slices right after it are free the map alone serves the request. Otherwise, if every slice
/// from the run's end to the current end of memory is free (there may be none), memory is grown
/// by what is missing or by the geometric step of `policy`, whichever is larger, the fresh
/// region is added to the map and the tail is claimed, so a buffer at the top of the heap grows
/// without a copy however often it is resized.
///
/// Returns whether the claimed tail was all zero, or `None` with the run unchanged when a taken
/// slice lies between the run and the end of memory, when growth is refused or would pass
/// [`SliceMap::usable_limit`], or when the fresh region did not start at the end of memory
/// (something else grew memory in between; the region stays in the map for later requests and
/// the caller moves the block instead).
pub fn extend_with_growth<const WORDS: usize, M: Memory>(
    map: &mut SliceMap<WORDS>,
    mem: &mut M,
    start: usize,
    count: usize,
    extra: usize,
    policy: &GrowPolicy,
) -> Option<bool> {
    if let Some(zeroed) = map.try_extend(start, count, extra) {
        return Some(zeroed);
    }
    grow_and_extend(map, mem, start, count, extra, policy)
}

#[cold]
#[inline(never)]
fn grow_and_extend<const WORDS: usize, M: Memory>(
    map: &mut SliceMap<WORDS>,
    mem: &mut M,
    start: usize,
    count: usize,
    extra: usize,
    policy: &GrowPolicy,
) -> Option<bool> {
    debug_assert!(count >= 1 && extra >= 1);
    let tail = start + count;
    let end = mem.size_slices();
    let limit = map.usable_limit();
    // A handed-out run lies inside memory and inside the map, so `tail <= end`; if the end of
    // memory has passed the map (something else grew it), the slices up there are not the
    // map's and cannot be free.
    if end < tail || end > limit {
        return None;
    }
    let have = end - tail;
    // `try_extend` failed, so if the whole extension fits below the end of memory a taken slice
    // is in the way; and only a run with nothing but free slices above it is at the top.
    if have >= extra || !map.run_is_free(tail, have) {
        return None;
    }
    let need = extra - have;
    let room = limit - end;
    if need > room {
        return None;
    }
    let want = need.max(growth_step(map, end, policy)).min(room);
    let (region, got) = match mem.grow(want) {
        Some(region) => (region, want),
        None if want > need => (mem.grow(need)?, need),
        None => return None,
    };
    add_region(map, region, got);
    if region != end {
        return None;
    }
    // The tail up to `end` was free and the region continues it for at least `need` slices.
    let extended = map.try_extend(start, count, extra);
    debug_assert!(
        extended.is_some(),
        "a contiguous region must complete the tail"
    );
    extended
}

/// Record a grown region as free, zeroed slices. The part the map cannot hand out, if any (only
/// when something else grew memory first and pushed the region up), is lost.
fn add_region<const WORDS: usize>(map: &mut SliceMap<WORDS>, start: usize, count: usize) {
    let end = start.saturating_add(count).min(map.usable_limit());
    if start < end {
        map.add_free(start, end - start, true);
    }
}

/// The linker gap: slices between `heap_base` and the initial end of memory, which the heap
/// reclaims at startup instead of paying a `memory.grow` for its first page.
///
/// Returns the first wholly usable slice, `ceil(heap_base / SLICE_SIZE)`, and how many slices
/// follow it up to `size_slices` (zero when the heap base sits in the last slice).
pub fn initial_free_range(heap_base: usize, size_slices: usize) -> (usize, usize) {
    let first = heap_base.div_ceil(SLICE_SIZE);
    (first, size_slices.saturating_sub(first))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SimMemory;
    use crate::backend::testing::Region;
    use std::vec;
    use std::vec::Vec;

    const W: usize = 4;
    const N: usize = W * BITS;

    /// Tiny deterministic generator so the model test needs no dependencies.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn coin(&mut self) -> bool {
            self.next() & 1 == 1
        }
    }

    /// The specification, written the slow way: the lowest relative index whose absolute index
    /// is a multiple of `align` and that starts `count` free slices.
    fn reference_find(free: &[bool], base: usize, count: usize, align: usize) -> Option<usize> {
        (0..free.len().saturating_sub(count - 1))
            .filter(|&s| (base + s) % align == 0)
            .find(|&s| free[s..s + count].iter().all(|&f| f))
    }

    /// The specification of `alloc_tail`: the lowest aligned index in the maximal run of free
    /// slices ending at `below`, provided `count` slices fit between it and `below`.
    fn reference_find_tail(
        free: &[bool],
        base: usize,
        count: usize,
        align: usize,
        below: usize,
    ) -> Option<usize> {
        let end = below.min(free.len());
        let lo = (0..end).rev().take_while(|&i| free[i]).last()?;
        let start = (lo..end).find(|&s| (base + s) % align == 0)?;
        (end - start >= count).then_some(start)
    }

    /// A map whose bits match the model, built through the public API.
    fn map_from(free: &[bool], zero: &[bool], base: usize) -> SliceMap<W> {
        let mut m = SliceMap::<W>::new();
        m.init(base);
        let mut i = 0;
        while i < N {
            if !free[i] {
                i += 1;
                continue;
            }
            let start = i;
            while i < N && free[i] && zero[i] == zero[start] {
                i += 1;
            }
            m.add_free(m.base() + start, i - start, zero[start]);
        }
        m
    }

    fn check_invariants<const K: usize>(m: &SliceMap<K>) {
        assert!(m.hint <= K);
        for w in 0..K {
            assert_eq!(
                m.zero[w] & !m.free[w],
                0,
                "zero bit without free bit in word {w}"
            );
            if w < m.hint {
                assert_eq!(m.free[w], 0, "free bit below the hint in word {w}");
            }
        }
    }

    fn assert_matches(m: &SliceMap<W>, free: &[bool], zero: &[bool]) {
        for i in 0..N {
            assert_eq!(m.is_free(m.base() + i), free[i], "free bit {i}");
            assert_eq!(m.is_zero(m.base() + i), zero[i], "zero bit {i}");
        }
        assert_eq!(m.free_count(), free.iter().filter(|&&f| f).count());
        check_invariants(m);
    }

    /// Absolute index of the first slice of a region created with `initial` slices.
    fn region_start(mem: &SimMemory, initial: usize) -> usize {
        mem.size_slices() - initial
    }

    #[test]
    fn empty_map_serves_nothing() {
        let mut m = SliceMap::<W>::new();
        // The zero value is the initial state (a static map then needs no data segment), and
        // it already serves nothing: the loose hint makes the search scan every word once.
        assert_eq!((m.base, m.hint), (0, 0));
        assert!(m.free.iter().chain(m.zero.iter()).all(|&w| w == 0));
        assert_eq!(m.alloc(1, 1), None);
        assert_eq!(m.hint, W, "the search moved the hint past the empty words");
        check_invariants(&m);
        m.init(0);
        assert_eq!(m.hint, W);
        assert_eq!(m.alloc(1, 1), None);
        assert_eq!(m.alloc(8, 8), None);
        assert_eq!(m.alloc(64, 64), None);
        assert_eq!(m.alloc(3, 2), None);
        assert!(!m.has_run(1, 1));
        assert_eq!(m.free_count(), 0);
        assert_eq!((m.base(), m.limit()), (0, N));
        check_invariants(&m);
        assert_eq!(SliceMap::<1024>::CAPACITY, MAX_SLICE_INDEX + 1);
        assert_eq!(SliceMap::<2>::default().free_count(), 0);
    }

    #[test]
    fn init_rounds_the_base_down_to_a_word() {
        let mut m = SliceMap::<W>::new();
        m.init(100);
        assert_eq!((m.base(), m.limit()), (64, 64 + N));
        m.add_free(64, N, true);
        assert_eq!(
            m.alloc(1, 1),
            Some(Run {
                start: 64,
                zeroed: true
            })
        );
        // Alignment is on the absolute index: 128 is the first multiple of 128 in the map.
        assert_eq!(
            m.alloc(1, 128),
            Some(Run {
                start: 128,
                zeroed: true
            })
        );
        assert_eq!(
            m.alloc(64, 64),
            Some(Run {
                start: 192,
                zeroed: true
            })
        );
        assert_eq!(
            m.alloc(1, 256),
            Some(Run {
                start: 256,
                zeroed: true
            })
        );
        // 0 is below the map and 512 above it.
        assert_eq!(m.alloc(1, 512), None);
        check_invariants(&m);
    }

    #[test]
    fn single_slices_come_out_lowest_first() {
        let mut m = SliceMap::<W>::new();
        m.init(0);
        m.add_free(0, N, true);
        for i in 0..N {
            assert_eq!(
                m.alloc(1, 1),
                Some(Run {
                    start: i,
                    zeroed: true
                })
            );
            assert_eq!(m.hint, i / BITS);
        }
        assert_eq!(m.alloc(1, 1), None);
        assert_eq!(m.hint, W);
        // Frees pull the hint back; the slices come out lowest first and dirty.
        for i in [200, 3, 77, 67] {
            m.free(i, 1);
        }
        assert_eq!(m.hint, 0);
        for i in [3, 67, 77, 200] {
            assert_eq!(
                m.alloc(1, 1),
                Some(Run {
                    start: i,
                    zeroed: false
                })
            );
        }
        assert_eq!(m.alloc(1, 1), None);
        check_invariants(&m);
    }

    /// Every configuration of up to two handed-out slices, against the reference search, for
    /// the three page shapes and a few general ones.
    #[test]
    fn searches_match_the_reference_for_every_pair_of_holes() {
        const SHAPES: [(usize, usize); 8] = [
            (1, 1),
            (8, 8),
            (64, 64),
            (2, 2),
            (3, 1),
            (9, 8),
            (65, 1),
            (128, 128),
        ];
        let mut free = [true; N];
        let mut zero = [true; N];
        for (i, z) in zero.iter_mut().enumerate() {
            *z = i % 5 != 0;
        }
        for a in 0..N {
            for b in a..N {
                free.fill(true);
                free[a] = false;
                free[b] = false;
                let template = map_from(&free, &zero, 0);
                for (count, align) in SHAPES {
                    let expect = reference_find(&free, 0, count, align);
                    assert_eq!(template.has_run(count, align), expect.is_some());
                    let mut m = template.clone();
                    let got = m.alloc(count, align);
                    assert_eq!(
                        got.map(|r| r.start),
                        expect,
                        "holes {a} {b}, count {count} align {align}"
                    );
                    if let Some(r) = got {
                        assert_eq!(r.zeroed, zero[r.start..r.start + count].iter().all(|&z| z));
                        assert!((r.start..r.start + count).all(|i| !m.is_free(i)));
                        assert_eq!(m.free_count(), template.free_count() - count);
                    }
                    check_invariants(&m);
                    for below in [N + 9, N, N - 3, 130, 64, 7] {
                        let expect = reference_find_tail(&free, 0, count, align, below);
                        let mut m = template.clone();
                        let got = m.alloc_tail(count, align, below);
                        assert_eq!(
                            got.map(|r| r.start),
                            expect,
                            "holes {a} {b}, count {count} align {align} below {below}"
                        );
                        if let Some(r) = got {
                            assert_eq!(r.zeroed, zero[r.start..r.start + count].iter().all(|&z| z));
                            assert!((r.start..r.start + count).all(|i| !m.is_free(i)));
                            assert_eq!(m.free_count(), template.free_count() - count);
                        }
                        check_invariants(&m);
                    }
                }
            }
        }
    }

    #[test]
    fn general_runs_honour_absolute_alignment_and_cross_words() {
        let mut m = SliceMap::<W>::new();
        // Multiples of 128 and 256 fall inside the map but not at its first word.
        m.init(192);
        m.add_free(192, N, true);
        assert_eq!(
            m.alloc(1, 128),
            Some(Run {
                start: 256,
                zeroed: true
            })
        );
        assert_eq!(
            m.alloc(1, 128),
            Some(Run {
                start: 384,
                zeroed: true
            })
        );
        // 256 is taken and 512 lies beyond the map.
        assert_eq!(m.alloc(1, 256), None);
        assert_eq!(
            m.alloc(100, 1),
            Some(Run {
                start: 257,
                zeroed: true
            })
        );
        assert_eq!(
            m.alloc(30, 2),
            Some(Run {
                start: 192,
                zeroed: true
            })
        );
        assert_eq!(
            m.alloc(27, 4),
            Some(Run {
                start: 224,
                zeroed: true
            })
        );
        m.free(256, 1);
        assert_eq!(
            m.alloc(2, 1),
            Some(Run {
                start: 222,
                zeroed: true
            })
        );
        assert_eq!(
            m.alloc(2, 1),
            Some(Run {
                start: 251,
                zeroed: true
            })
        );
        // Mixes fresh slices with the dirty 256.
        assert_eq!(
            m.alloc(4, 1),
            Some(Run {
                start: 253,
                zeroed: false
            })
        );
        // 357..384 and 385..448 are separated by the taken 384.
        assert_eq!(m.alloc(140, 1), None);
        assert!(!m.has_run(91, 1));
        m.free(384, 1);
        assert!(m.has_run(91, 1));
        assert_eq!(
            m.alloc(91, 1),
            Some(Run {
                start: 357,
                zeroed: false
            })
        );
        assert_eq!(m.free_count(), 0);
        check_invariants(&m);
    }

    #[test]
    fn alloc_tail_takes_the_bottom_of_the_free_tail() {
        let mut m = SliceMap::<W>::new();
        m.init(192);
        // Free: [192, 256) and [260, 300); the end of memory is 300.
        m.add_free(192, 64, true);
        m.add_free(260, 40, false);
        // The tail is [260, 300): the run goes to its bottom and leaves the rest above.
        assert_eq!(
            m.alloc_tail(3, 1, 300),
            Some(Run {
                start: 260,
                zeroed: false
            })
        );
        // Alignment moves the start up inside the tail.
        assert_eq!(
            m.alloc_tail(3, 8, 300),
            Some(Run {
                start: 264,
                zeroed: false
            })
        );
        assert!(m.is_free(263));
        // Too long for the 33-slice tail, whatever lies below: nothing.
        assert_eq!(m.alloc_tail(40, 1, 300), None);
        assert_eq!(m.alloc_tail(34, 1, 300), None);
        assert_eq!(
            m.alloc_tail(33, 1, 300),
            Some(Run {
                start: 267,
                zeroed: false
            })
        );
        assert_eq!(m.free_count(), 64 + 1);
        // The slice below `below` must be free for a tail to exist at all.
        assert_eq!(m.alloc_tail(1, 1, 298), None);
        assert_eq!(m.alloc_tail(1, 1, 300), None);
        // A `below` inside a run of free slices: the tail is the part of that run below it.
        assert_eq!(
            m.alloc_tail(10, 8, 250),
            Some(Run {
                start: 192,
                zeroed: true
            })
        );
        assert_eq!(
            m.alloc_tail(10, 128, 250),
            None,
            "256 is the one multiple of 128 in the tail and it lies above `below`"
        );
        assert_eq!(
            m.alloc_tail(1, 8, 250),
            Some(Run {
                start: 208,
                zeroed: true
            })
        );
        // A `below` past the end of the map, or at zero, is harmless: the tail then ends at the
        // map's end, where nothing is free.
        assert_eq!(m.alloc_tail(1, 1, usize::MAX), None);
        assert_eq!(m.alloc_tail(1, 1, 0), None);
        m.free(299, 1);
        assert_eq!(m.alloc_tail(1, 1, usize::MAX), None);
        assert_eq!(
            m.alloc_tail(1, 1, 300),
            Some(Run {
                start: 299,
                zeroed: false
            })
        );
        check_invariants(&m);
    }

    #[test]
    fn zero_bits_follow_fresh_memory_only() {
        let mut m = SliceMap::<W>::new();
        m.init(0);
        m.add_free(0, 16, true);
        m.add_free(16, 16, false);
        assert_eq!(
            m.alloc(8, 8),
            Some(Run {
                start: 0,
                zeroed: true
            })
        );
        assert_eq!(
            m.alloc(8, 8),
            Some(Run {
                start: 8,
                zeroed: true
            })
        );
        assert_eq!(
            m.alloc(8, 8),
            Some(Run {
                start: 16,
                zeroed: false
            })
        );
        m.free(8, 8);
        assert!(m.is_free(8) && !m.is_zero(8));
        assert_eq!(
            m.alloc(8, 8),
            Some(Run {
                start: 8,
                zeroed: false
            })
        );
        m.free(0, 8);
        m.free(16, 8);
        m.add_free(32, 32, true);
        // A run mixing dirty and fresh slices is not zeroed; the next one is all fresh.
        assert_eq!(
            m.alloc(16, 1),
            Some(Run {
                start: 16,
                zeroed: false
            })
        );
        assert_eq!(
            m.alloc(16, 1),
            Some(Run {
                start: 32,
                zeroed: true
            })
        );
        for i in 0..N {
            assert!(!m.is_zero(i) || m.is_free(i));
        }
        check_invariants(&m);
    }

    #[test]
    fn try_extend_and_shrink_move_the_end_of_a_run() {
        let mut m = SliceMap::<W>::new();
        m.init(0);
        m.add_free(0, 128, true);
        assert_eq!(
            m.alloc(10, 1),
            Some(Run {
                start: 0,
                zeroed: true
            })
        );
        assert_eq!(m.try_extend(0, 10, 0), Some(true));
        assert_eq!(m.free_count(), 118);
        // The tail crosses into the second word.
        assert_eq!(m.try_extend(0, 10, 60), Some(true));
        assert_eq!(m.free_count(), 58);
        m.shrink(0, 70, 65);
        assert_eq!(m.free_count(), 63);
        assert!(m.is_free(65) && !m.is_zero(65));
        assert_eq!(m.try_extend(0, 65, 6), Some(false));
        assert_eq!(m.free_count(), 57);
        let other = m.alloc(1, 1).unwrap();
        assert_eq!(other.start, 71);
        assert_eq!(m.try_extend(0, 71, 1), None);
        assert_eq!(m.free_count(), 56);
        m.free(71, 1);
        assert_eq!(m.try_extend(0, 71, 57), Some(false));
        assert_eq!(m.free_count(), 0);
        // Slices that were never added, and slices beyond the map, both refuse.
        assert_eq!(m.try_extend(0, 128, 1), None);
        assert_eq!(m.try_extend(0, 128, 200), None);
        m.add_free(128, 128, true);
        assert_eq!(m.try_extend(0, 128, 128), Some(true));
        assert_eq!(m.try_extend(0, 256, 1), None);
        m.shrink(0, 256, 0);
        assert_eq!(m.free_count(), N);
        assert_eq!(m.hint, 0);
        assert_eq!(
            m.alloc(64, 64),
            Some(Run {
                start: 0,
                zeroed: false
            })
        );
        check_invariants(&m);
    }

    #[test]
    fn random_operations_match_a_boolean_model() {
        const STEPS: usize = 8000;
        // Multiples of 128 and 256 fall inside the map but not at its first word.
        let base = 192;
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        let mut free = vec![false; N];
        let mut zero = vec![false; N];
        let mut managed = vec![false; N];
        let mut live: Vec<(usize, usize)> = Vec::new();
        let mut m = SliceMap::<W>::new();
        m.init(base);
        const COUNTS: [usize; 3] = [1, 8, 64];
        const ALIGNS: [usize; 9] = [1, 2, 4, 8, 16, 32, 64, 128, 256];
        let (mut hits, mut misses, mut extended, mut blocked) = (0, 0, 0, 0);

        for step in 0..STEPS {
            match rng.below(100) {
                0..=39 => {
                    let count = if rng.coin() {
                        COUNTS[rng.below(3)]
                    } else {
                        1 + rng.below(70)
                    };
                    let align = if rng.coin() {
                        count.min(64)
                    } else {
                        ALIGNS[rng.below(9)]
                    };
                    let align = if align.is_power_of_two() { align } else { 1 };
                    // One allocation in four takes the bottom of the free tail below a random end.
                    let high = rng.below(4) == 0;
                    let below = rng.below(N + 8);
                    let expect = if high {
                        reference_find_tail(&free, base, count, align, below)
                    } else {
                        reference_find(&free, base, count, align)
                    };
                    assert_eq!(
                        m.has_run(count, align),
                        reference_find(&free, base, count, align).is_some(),
                        "step {step}"
                    );
                    let got = if high {
                        m.alloc_tail(count, align, base + below)
                    } else {
                        m.alloc(count, align)
                    };
                    assert_eq!(
                        got.map(|r| r.start - base),
                        expect,
                        "step {step}: alloc({count}, {align}) high {high} below {below}"
                    );
                    match got {
                        Some(r) => {
                            hits += 1;
                            let rel = r.start - base;
                            assert_eq!(r.zeroed, zero[rel..rel + count].iter().all(|&z| z));
                            for i in rel..rel + count {
                                free[i] = false;
                                zero[i] = false;
                            }
                            live.push((rel, count));
                        }
                        None => misses += 1,
                    }
                }
                40..=64 if !live.is_empty() => {
                    let (rel, count) = live.swap_remove(rng.below(live.len()));
                    m.free(base + rel, count);
                    free[rel..rel + count].fill(true);
                }
                65..=76 if !live.is_empty() => {
                    let k = rng.below(live.len());
                    let (rel, count) = live[k];
                    let extra = rng.below(12);
                    let tail = rel + count;
                    let expect = if tail + extra <= N && free[tail..tail + extra].iter().all(|&f| f)
                    {
                        Some(zero[tail..tail + extra].iter().all(|&z| z))
                    } else {
                        None
                    };
                    assert_eq!(
                        m.try_extend(base + rel, count, extra),
                        expect,
                        "step {step}"
                    );
                    if expect.is_some() {
                        extended += 1;
                        for i in tail..tail + extra {
                            free[i] = false;
                            zero[i] = false;
                        }
                        live[k].1 += extra;
                    } else {
                        blocked += 1;
                    }
                }
                77..=88 if !live.is_empty() => {
                    let k = rng.below(live.len());
                    let (rel, count) = live[k];
                    let new_count = rng.below(count + 1);
                    m.shrink(base + rel, count, new_count);
                    free[rel + new_count..rel + count].fill(true);
                    if new_count == 0 {
                        live.swap_remove(k);
                    } else {
                        live[k].1 = new_count;
                    }
                }
                89..=99 => {
                    // Hand a stretch of never-managed slices to the map, fresh or dirty.
                    let start = rng.below(N);
                    if managed[start] {
                        continue;
                    }
                    let max_len = 1 + rng.below(40);
                    let mut end = start;
                    while end < N && !managed[end] && end - start < max_len {
                        end += 1;
                    }
                    let zeroed = rng.coin();
                    m.add_free(base + start, end - start, zeroed);
                    for i in start..end {
                        managed[i] = true;
                        free[i] = true;
                        zero[i] = zeroed;
                    }
                }
                _ => {}
            }
            assert_matches(&m, &free, &zero);
        }
        assert!(hits > 500 && misses > 100, "hits {hits} misses {misses}");
        assert!(
            extended > 50 && blocked > 50,
            "extended {extended} blocked {blocked}"
        );
        assert!(
            managed.iter().all(|&x| x),
            "every slice was eventually managed"
        );
    }

    #[test]
    fn acquire_reclaims_the_linker_gap_before_growing() {
        // The heap base lies inside the second slice, so only the third is a whole free slice.
        let mut r = Region::new(64, 3, SLICE_SIZE + 100);
        let first = region_start(&r.mem, 3);
        let mut m = SliceMap::<W>::new();
        m.init(first);
        let gap = initial_free_range(r.mem.heap_base(), r.mem.size_slices());
        assert_eq!(gap, (first + 2, 1));
        m.add_free(gap.0, gap.1, false);

        let run = acquire(&mut m, &mut r.mem, 1, 1, &GrowPolicy::DEFAULT).unwrap();
        assert_eq!(
            run,
            Run {
                start: first + 2,
                zeroed: false
            }
        );
        assert_eq!(
            r.mem.size_slices(),
            first + 3,
            "no growth while the gap serves"
        );

        // The map is empty now: grow by the minimum step and serve from the fresh region.
        let run = acquire(&mut m, &mut r.mem, 1, 1, &GrowPolicy::DEFAULT).unwrap();
        assert_eq!(
            run,
            Run {
                start: first + 3,
                zeroed: true
            }
        );
        assert_eq!(r.mem.size_slices(), first + 3 + 16);
        assert_eq!(m.free_count(), 15);
        for i in first + 4..first + 19 {
            assert!(m.is_zero(i));
        }
        check_invariants(&m);
    }

    #[test]
    fn grow_policy_step_is_the_clamped_fraction() {
        let p = GrowPolicy::DEFAULT;
        assert_eq!((p.min_grow, p.max_grow, p.step_divisor), (16, 1024, 8));
        assert_eq!(p.step(0), 16);
        assert_eq!(p.step(127), 16);
        assert_eq!(p.step(128), 16);
        assert_eq!(p.step(136), 17);
        assert_eq!(p.step(8192), 1024);
        assert_eq!(p.step(usize::MAX), 1024);
        let half = GrowPolicy {
            step_divisor: 2,
            ..p
        };
        assert_eq!(half.step(100), 50);
        assert_eq!(GrowPolicy::default(), p);
    }

    #[test]
    fn acquire_grows_geometrically_within_the_policy() {
        let mut r = Region::new(N, 2, 0);
        let first = region_start(&r.mem, 2);
        let mut m = SliceMap::<W>::new();
        m.init(first);
        // An eighth of the heap reaches the cap of 12 once the heap holds 96 slices.
        let policy = GrowPolicy {
            min_grow: 2,
            max_grow: 12,
            step_divisor: 8,
        };
        let mut heap = 2;
        let mut steps = Vec::new();
        for _ in 0..150 {
            let before = r.mem.size_slices();
            let run = acquire(&mut m, &mut r.mem, 1, 1, &policy).expect("the region has room");
            let after = r.mem.size_slices();
            assert!(run.start < after);
            if after != before {
                let step = after - before;
                assert_eq!(step, (heap / 8).clamp(2, 12), "step at heap size {heap}");
                assert_eq!(
                    run,
                    Run {
                        start: before,
                        zeroed: true
                    }
                );
                heap += step;
                steps.push(step);
            }
        }
        assert_eq!(steps[0], 2);
        assert!(steps.contains(&12), "the step reached the cap");
        assert!(steps.windows(2).all(|w| w[0] <= w[1]), "steps never shrink");
        assert_eq!(r.mem.size_slices(), first + heap);
        check_invariants(&m);
    }

    #[test]
    fn acquire_handles_non_contiguous_growth() {
        let mut r = Region::new(128, 1, 0);
        let first = region_start(&r.mem, 1);
        let mut m = SliceMap::<W>::new();
        m.init(first);
        let policy = GrowPolicy {
            min_grow: 4,
            max_grow: 4,
            step_divisor: 8,
        };
        let a = acquire(&mut m, &mut r.mem, 1, 1, &policy).unwrap();
        assert_eq!(a.start, first + 1);
        assert_eq!(r.mem.size_slices(), first + 5);
        // Someone else takes the next three slices; the map still has three of its own.
        assert!(r.mem.skip_slices(3));
        for i in 0..3 {
            let run = acquire(&mut m, &mut r.mem, 1, 1, &policy).unwrap();
            assert_eq!(run.start, first + 2 + i);
        }
        assert_eq!(r.mem.size_slices(), first + 8);
        let b = acquire(&mut m, &mut r.mem, 1, 1, &policy).unwrap();
        assert_eq!(
            b.start,
            first + 8,
            "the fresh region starts after the skipped slices"
        );
        assert_eq!(r.mem.size_slices(), first + 12);
        for i in first + 5..first + 8 {
            assert!(!m.is_free(i), "skipped slices never enter the map");
        }
        // An aligned request from an unaligned end of memory: the growth covers the slack up to
        // the next aligned slice plus the run, and nothing more.
        assert!(r.mem.skip_slices(1));
        let c = acquire(&mut m, &mut r.mem, 8, 8, &policy).unwrap();
        assert_eq!(
            c,
            Run {
                start: first + 16,
                zeroed: true
            }
        );
        assert_eq!(r.mem.size_slices(), first + 13 + 3 + 8);
        assert_eq!(m.free_count(), 3 + 3);
        check_invariants(&m);
    }

    #[test]
    fn acquire_sizes_growth_from_the_current_end() {
        // Room for exactly one 64-aligned run after the initial four slices.
        let mut r = Region::new(128, 4, 0);
        let first = region_start(&r.mem, 4);
        let mut m = SliceMap::<W>::new();
        m.init(first);
        let run = acquire(&mut m, &mut r.mem, 64, 64, &GrowPolicy::DEFAULT).unwrap();
        assert_eq!(
            run,
            Run {
                start: first + 64,
                zeroed: true
            }
        );
        assert_eq!(
            r.mem.size_slices(),
            first + 128,
            "grew by the slack plus the run"
        );
        assert_eq!(r.mem.remaining_slices(), 0);
        assert_eq!(m.free_count(), 60);
        check_invariants(&m);
    }

    /// A memory where someone else grows by `skip` slices right before each of our grows: the
    /// non-contiguity the `Memory` contract allows, at the one moment `acquire` cannot observe.
    struct Interposed<'a> {
        mem: &'a mut SimMemory,
        skip: usize,
        grows: usize,
    }

    // SAFETY: every call is forwarded to SimMemory, which upholds the contract; skipping slices
    // first only makes the returned region start later, which the contract permits.
    unsafe impl Memory for Interposed<'_> {
        fn heap_base(&self) -> usize {
            self.mem.heap_base()
        }

        fn size_slices(&self) -> usize {
            self.mem.size_slices()
        }

        fn grow(&mut self, slices: usize) -> Option<usize> {
            self.grows += 1;
            assert!(self.mem.skip_slices(self.skip));
            self.mem.grow(slices)
        }

        fn ptr(&self, addr: usize) -> *mut u8 {
            self.mem.ptr(addr)
        }
    }

    #[test]
    fn acquire_grows_again_when_the_fresh_region_lands_elsewhere() {
        let mut r = Region::new(128, 4, 0);
        let first = region_start(&r.mem, 4);
        let mut m = SliceMap::<W>::new();
        m.init(first);
        let policy = GrowPolicy {
            min_grow: 1,
            max_grow: 1,
            step_divisor: 8,
        };
        // From end first + 4 an 8-aligned run needs 4 + 8 slices, but the region lands at
        // first + 9 and holds no aligned run; the retry asks for 15, lands at first + 26 and does.
        let mut mem = Interposed {
            mem: &mut r.mem,
            skip: 5,
            grows: 0,
        };
        let run = acquire(&mut m, &mut mem, 8, 8, &policy).unwrap();
        assert_eq!(
            run,
            Run {
                start: first + 32,
                zeroed: true
            }
        );
        assert_eq!(mem.grows, 2);
        assert_eq!(mem.size_slices(), first + 41);
        assert_eq!(m.free_count(), 12 + 6 + 1, "both regions stay in the map");
        for i in (first + 4..first + 9).chain(first + 21..first + 26) {
            assert!(!m.is_free(i), "skipped slice {i} never enters the map");
        }
        check_invariants(&m);
    }

    #[test]
    fn acquire_keeps_the_first_region_when_the_retry_is_refused() {
        // The region outgrows the map, so the map limit is what refuses the retry.
        let mut r = Region::new(N + 64, 4, 0);
        let first = region_start(&r.mem, 4);
        let mut m = SliceMap::<W>::new();
        m.init(first);
        // From end first + 4 a 64-aligned run needs 60 + 64 slices; the region lands at
        // first + 65 and its aligned run would end at first + 192, past the region end at 189.
        // The worst-case retry of 127 slices does not fit under the map limit.
        let mut mem = Interposed {
            mem: &mut r.mem,
            skip: 61,
            grows: 0,
        };
        assert_eq!(
            acquire(&mut m, &mut mem, 64, 64, &GrowPolicy::DEFAULT),
            None
        );
        assert_eq!(mem.grows, 1);
        assert_eq!(mem.size_slices(), first + 189);
        assert_eq!(m.free_count(), 124, "the first region stays in the map");
        assert!(!m.has_run(64, 64));
        assert!(m.has_run(60, 1));
        check_invariants(&m);
    }

    #[test]
    fn acquire_refuses_what_the_map_cannot_describe() {
        // The region is larger than the map, so growth must stop at the map, not the region.
        let mut r = Region::new(N + 64, 2, 0);
        let first = region_start(&r.mem, 2);
        let mut m = SliceMap::<W>::new();
        m.init(first);
        let policy = GrowPolicy {
            min_grow: 100,
            max_grow: 100,
            step_divisor: 8,
        };
        assert_eq!(acquire(&mut m, &mut r.mem, N + 1, 1, &policy), None);
        assert_eq!(acquire(&mut m, &mut r.mem, N - 1, 1, &policy), None);
        // The next 4-aligned slice is first + 4, leaving room for only N - 4.
        assert_eq!(acquire(&mut m, &mut r.mem, N - 3, 4, &policy), None);
        assert_eq!(
            r.mem.size_slices(),
            first + 2,
            "refused requests do not grow"
        );

        let a = acquire(&mut m, &mut r.mem, 200, 1, &policy).unwrap();
        assert_eq!(
            a,
            Run {
                start: first + 2,
                zeroed: true
            }
        );
        assert_eq!(r.mem.size_slices(), first + 202);
        // The step is capped by the room left in the map.
        let b = acquire(&mut m, &mut r.mem, 1, 1, &policy).unwrap();
        assert_eq!(
            b,
            Run {
                start: first + 202,
                zeroed: true
            }
        );
        assert_eq!(
            r.mem.size_slices(),
            first + N,
            "growth stops at the map limit"
        );
        assert_eq!(m.free_count(), 53);
        assert_eq!(acquire(&mut m, &mut r.mem, 60, 1, &policy), None);
        assert_eq!(r.mem.size_slices(), first + N);
        assert_eq!(r.mem.remaining_slices(), 64);
        let c = acquire(&mut m, &mut r.mem, 53, 1, &policy).unwrap();
        assert_eq!(c.start, first + 203);
        assert_eq!(acquire(&mut m, &mut r.mem, 1, 1, &policy), None);
        check_invariants(&m);
    }

    #[test]
    fn acquire_falls_back_to_the_bare_request_when_the_step_fails() {
        let mut r = Region::new(12, 2, 0);
        let first = region_start(&r.mem, 2);
        let mut m = SliceMap::<W>::new();
        m.init(first);
        let policy = GrowPolicy::DEFAULT;
        let a = acquire(&mut m, &mut r.mem, 1, 1, &policy).unwrap();
        assert_eq!(
            a,
            Run {
                start: first + 2,
                zeroed: true
            }
        );
        assert_eq!(
            r.mem.size_slices(),
            first + 3,
            "grew by exactly the request"
        );
        // From end first + 3 an 8-aligned run needs 5 + 8 slices; only 9 are left.
        assert_eq!(acquire(&mut m, &mut r.mem, 8, 8, &policy), None);
        assert_eq!(r.mem.size_slices(), first + 3);
        let b = acquire(&mut m, &mut r.mem, 9, 1, &policy).unwrap();
        assert_eq!(
            b,
            Run {
                start: first + 3,
                zeroed: true
            }
        );
        assert_eq!(r.mem.remaining_slices(), 0);
        assert_eq!(acquire(&mut m, &mut r.mem, 1, 1, &policy), None);
        check_invariants(&m);
    }

    #[test]
    fn extend_with_growth_grows_memory_only_for_a_run_at_the_top() {
        let mut r = Region::new(64, 4, 0);
        let first = region_start(&r.mem, 4);
        let mut m = SliceMap::<W>::new();
        m.init(first);
        m.add_free(first, 4, false);
        let policy = GrowPolicy {
            min_grow: 2,
            max_grow: 64,
            step_divisor: 2,
        };
        let run = acquire(&mut m, &mut r.mem, 3, 1, &policy).unwrap();
        assert_eq!(run.start, first);
        // One free slice sits between the run and the end of memory; the extension needs five,
        // so memory grows by the four missing ones (the step, 2, is smaller).
        assert_eq!(
            extend_with_growth(&mut m, &mut r.mem, first, 3, 5, &policy),
            Some(false),
            "the dirty gap slice makes the tail dirty"
        );
        assert_eq!(r.mem.size_slices(), first + 8);
        assert_eq!(m.free_count(), 0);
        for i in 0..8 {
            assert!(!m.is_free(first + i));
        }
        // A pure map extension needs no growth.
        m.add_free(first + 8, 2, true);
        assert_eq!(r.mem.grow(2), Some(first + 8));
        assert_eq!(
            extend_with_growth(&mut m, &mut r.mem, first, 8, 2, &policy),
            Some(true)
        );
        assert_eq!(r.mem.size_slices(), first + 10);
        // With the run at the top and nothing free after it, growth takes the geometric step
        // when that exceeds the need, and the surplus stays in the map.
        assert_eq!(
            extend_with_growth(&mut m, &mut r.mem, first, 10, 1, &policy),
            Some(true)
        );
        assert_eq!(r.mem.size_slices(), first + 15, "step of 5 (half of 10)");
        assert_eq!(m.free_count(), 4);
        // A block after the run blocks it, whatever lies above.
        let other = m.alloc(1, 1).unwrap();
        assert_eq!(other.start, first + 11);
        assert_eq!(
            extend_with_growth(&mut m, &mut r.mem, first, 11, 1, &policy),
            None
        );
        assert_eq!(r.mem.size_slices(), first + 15);
        assert_eq!(m.free_count(), 3);
        m.free(first + 11, 1);
        // Non-contiguous growth (someone else grows right before we do): the region is kept,
        // the run is not extended, and the slices the other party took never enter the map.
        let mut mem = Interposed {
            mem: &mut r.mem,
            skip: 2,
            grows: 0,
        };
        assert_eq!(
            extend_with_growth(&mut m, &mut mem, first, 11, 8, &policy),
            None
        );
        assert_eq!(mem.grows, 1);
        assert_eq!(
            mem.size_slices(),
            first + 17 + 7,
            "grew by half the heap (7) for the missing four"
        );
        assert_eq!(m.free_count(), 4 + 7);
        assert!(!m.is_free(first + 15) && !m.is_free(first + 16));
        for i in first + 17..first + 24 {
            assert!(m.is_free(i) && m.is_zero(i));
        }
        assert!(all_clear(&m.free, 0, 11), "the run is untouched");
        // Those taken slices now sit between the run and the end of memory, so the run is no
        // longer at the top and cannot grow through memory at all.
        assert_eq!(
            extend_with_growth(&mut m, &mut mem, first, 11, 8, &policy),
            None
        );
        assert_eq!(mem.grows, 1, "no growth for a run that is not at the top");
        check_invariants(&m);
    }

    #[test]
    fn extend_with_growth_refuses_what_memory_or_the_map_cannot_give() {
        // The region ends 6 slices after the run.
        let mut r = Region::new(10, 4, 0);
        let first = region_start(&r.mem, 4);
        let mut m = SliceMap::<W>::new();
        m.init(first);
        m.add_free(first, 4, true);
        let policy = GrowPolicy::DEFAULT;
        let run = m.alloc(4, 1).unwrap();
        assert_eq!(run.start, first);
        assert_eq!(
            extend_with_growth(&mut m, &mut r.mem, first, 4, 7, &policy),
            None,
            "the region has 6 slices left"
        );
        assert_eq!(r.mem.size_slices(), first + 4);
        // The step (16) is refused, so the bare need is taken.
        assert_eq!(
            extend_with_growth(&mut m, &mut r.mem, first, 4, 6, &policy),
            Some(true)
        );
        assert_eq!(r.mem.size_slices(), first + 10);
        assert_eq!(m.free_count(), 0);
        check_invariants(&m);

        // Growth stops at the map's usable limit: a run ending at the last usable slice cannot
        // grow, and one two slices below it grows by at most two.
        let mut m = SliceMap::<W>::new();
        m.init(MAX_SLICE_INDEX + 1 - N);
        let mut mem = PaperMemory {
            size: MAX_SLICE_INDEX - 2,
            capacity: MAX_SLICE_INDEX + 1,
        };
        m.add_free(MAX_SLICE_INDEX - 6, 4, true);
        assert_eq!(m.alloc(4, 1).map(|r| r.start), Some(MAX_SLICE_INDEX - 6));
        assert_eq!(
            extend_with_growth(&mut m, &mut mem, MAX_SLICE_INDEX - 6, 4, 3, &policy),
            None
        );
        assert_eq!(mem.size, MAX_SLICE_INDEX - 2);
        assert_eq!(
            extend_with_growth(&mut m, &mut mem, MAX_SLICE_INDEX - 6, 4, 2, &policy),
            Some(true)
        );
        assert_eq!(mem.size, MAX_SLICE_INDEX, "never grows onto the last slice");
        assert_eq!(
            extend_with_growth(&mut m, &mut mem, MAX_SLICE_INDEX - 6, 6, 1, &policy),
            None
        );
        check_invariants(&m);
    }

    #[test]
    fn initial_free_range_starts_at_the_first_whole_slice() {
        assert_eq!(initial_free_range(0, 17), (0, 17));
        assert_eq!(initial_free_range(1, 17), (1, 16));
        assert_eq!(initial_free_range(SLICE_SIZE, 17), (1, 16));
        assert_eq!(initial_free_range(SLICE_SIZE + 1, 17), (2, 15));
        assert_eq!(initial_free_range(16 * SLICE_SIZE + 100, 17), (17, 0));
        assert_eq!(initial_free_range(40 * SLICE_SIZE, 17), (40, 0));
    }

    /// A memory that grows on paper only, placed anywhere in the slice index space.
    struct PaperMemory {
        size: usize,
        capacity: usize,
    }

    // SAFETY: never hands out a pointer, so nothing about memory validity can be violated; the
    // tests using it only exercise the map's bookkeeping.
    unsafe impl Memory for PaperMemory {
        fn heap_base(&self) -> usize {
            0
        }

        fn size_slices(&self) -> usize {
            self.size
        }

        fn grow(&mut self, slices: usize) -> Option<usize> {
            let end = self.size.checked_add(slices)?;
            if end > self.capacity {
                return None;
            }
            self.size = end;
            Some(end - slices)
        }

        fn ptr(&self, _addr: usize) -> *mut u8 {
            unreachable!("the paper memory has no bytes")
        }
    }

    #[test]
    fn the_last_slice_of_the_address_space_is_never_usable() {
        // A map whose top word ends exactly at the end of a 4 GiB memory.
        let mut m = SliceMap::<W>::new();
        m.init(MAX_SLICE_INDEX + 1 - N);
        assert_eq!(m.limit(), MAX_SLICE_INDEX + 1);
        assert_eq!(m.usable_limit(), MAX_SLICE_INDEX);
        m.add_free(m.base(), N, true);
        assert_eq!(m.free_count(), N - 1);
        assert!(m.is_free(MAX_SLICE_INDEX - 1) && !m.is_free(MAX_SLICE_INDEX));
        m.add_free(MAX_SLICE_INDEX, 1, false);
        assert_eq!(
            m.free_count(),
            N - 1,
            "adding only the last slice is a no-op"
        );
        for _ in 0..3 {
            assert!(m.alloc(64, 64).is_some());
        }
        assert_eq!(m.alloc(64, 64), None, "the top word is one slice short");
        let tail = m.alloc(63, 1).unwrap();
        assert_eq!(tail.start, MAX_SLICE_INDEX - 63);
        assert_eq!(m.try_extend(tail.start, 63, 1), None);
        assert_eq!(m.free_count(), 0);
        check_invariants(&m);

        // Growth stops at the last usable slice even when memory could reach 4 GiB.
        let mut m = SliceMap::<W>::new();
        m.init(MAX_SLICE_INDEX + 1 - N);
        let mut mem = PaperMemory {
            size: MAX_SLICE_INDEX - 5,
            capacity: MAX_SLICE_INDEX + 1,
        };
        let policy = GrowPolicy::DEFAULT;
        let run = acquire(&mut m, &mut mem, 1, 1, &policy).unwrap();
        assert_eq!(run.start, MAX_SLICE_INDEX - 5);
        assert_eq!(
            mem.size, MAX_SLICE_INDEX,
            "the step is cut to the usable room"
        );
        assert_eq!(acquire(&mut m, &mut mem, 8, 8, &policy), None);
        for i in 1..5 {
            assert_eq!(
                acquire(&mut m, &mut mem, 1, 1, &policy).map(|r| r.start),
                Some(MAX_SLICE_INDEX - 5 + i)
            );
        }
        assert_eq!(acquire(&mut m, &mut mem, 1, 1, &policy), None);
        assert_eq!(
            mem.size, MAX_SLICE_INDEX,
            "memory never grows onto the last slice"
        );
        check_invariants(&m);

        // A map entirely above the index (a simulated memory at a high host address) has no hole.
        let mut m = SliceMap::<W>::new();
        m.init(MAX_SLICE_INDEX + 1);
        assert_eq!(m.usable_limit(), m.limit());
        m.add_free(m.base(), N, true);
        assert_eq!(m.free_count(), N);
    }
}

#[cfg(kani)]
mod verify {
    //! Bounded proofs on a two-word map (128 slices).
    //!
    //! Kani has one unwind bound per harness, and every loop whose start is symbolic (a search
    //! from `hint`, a word iterator from a symbolic index) is unrolled to that bound, so the
    //! harnesses keep every loop short: universally quantified checks use one arbitrary
    //! position instead of a loop over all 128, range checks use word masks written
    //! independently of the implementation, and the general search runs on maps built from a
    //! few runs so that its first-fit loop visits a handful of runs.
    use super::*;

    const W: usize = 2;
    const N: usize = W * BITS;

    fn bit(words: &[u64; W], rel: usize) -> bool {
        (words[rel / BITS] >> (rel % BITS)) & 1 == 1
    }

    /// Bits of word `w` inside the relative range `[s, s + count)`, with `s + count <= N`.
    fn range_mask(s: usize, count: usize, w: usize) -> u64 {
        let lo = s.max(w * BITS);
        let hi = (s + count).min((w + 1) * BITS);
        if lo >= hi {
            return 0;
        }
        (u64::MAX >> (BITS - (hi - lo))) << (lo - w * BITS)
    }

    /// Every slice of `[s, s + count)` has its bit set (vacuously true for an empty range).
    fn all_in(words: &[u64; W], s: usize, count: usize) -> bool {
        (0..W).all(|w| words[w] & range_mask(s, count, w) == range_mask(s, count, w))
    }

    /// No slice of `[s, s + count)` has its bit set.
    fn none_in(words: &[u64; W], s: usize, count: usize) -> bool {
        (0..W).all(|w| words[w] & range_mask(s, count, w) == 0)
    }

    fn same(a: &[u64; W], b: &[u64; W]) -> bool {
        (0..W).all(|w| a[w] == b[w])
    }

    /// A field-by-field copy; `Clone` would go through a byte loop that costs unwinding.
    fn snapshot(m: &SliceMap<W>) -> SliceMap<W> {
        SliceMap {
            free: m.free,
            zero: m.zero,
            base: m.base,
            hint: m.hint,
        }
    }

    fn invariants(m: &SliceMap<W>) {
        assert!(m.hint <= W);
        for w in 0..W {
            assert!(m.zero[w] & !m.free[w] == 0);
            assert!(w >= m.hint || m.free[w] == 0);
        }
    }

    /// Base 0 or 64: enough for absolute and relative alignment to differ at align 128.
    fn any_base() -> usize {
        if kani::any() { 0 } else { BITS }
    }

    /// A map with arbitrary contents that satisfy the invariants.
    fn any_map() -> SliceMap<W> {
        let mut m = SliceMap::<W>::new();
        m.init(any_base());
        for w in 0..W {
            m.free[w] = kani::any();
            m.zero[w] = kani::any::<u64>() & m.free[w];
        }
        let hint: usize = kani::any();
        kani::assume(hint <= W);
        for w in 0..W {
            kani::assume(w >= hint || m.free[w] == 0);
        }
        m.hint = hint;
        m
    }

    /// A map whose free bits are the union of up to `runs` runs, so the general search visits at
    /// most that many maximal runs, with arbitrary zero bits under them.
    fn any_map_with_runs(runs: usize) -> SliceMap<W> {
        let mut m = SliceMap::<W>::new();
        m.init(any_base());
        for _ in 0..runs {
            let start: usize = kani::any();
            let len: usize = kani::any();
            kani::assume(start <= N && len <= N - start);
            for w in 0..W {
                m.free[w] |= range_mask(start, len, w);
            }
        }
        for w in 0..W {
            m.zero[w] = kani::any::<u64>() & m.free[w];
        }
        m.hint = 0;
        m
    }

    /// `alloc(count, align)` against its specification on `before`: the result is the lowest
    /// aligned run of free slices, exactly its bits are cleared, and `zeroed` is right.
    fn check_alloc(before: SliceMap<W>, count: usize, align: usize) {
        let mut m = snapshot(&before);
        let got = m.alloc(count, align);
        // One arbitrary position stands for all of them.
        let s: usize = kani::any();
        kani::assume(s < N);
        let s_aligned = (before.base + s) & (align - 1) == 0 && s + count <= N;
        match got {
            Some(run) => {
                assert!(run.start >= before.base && run.start - before.base <= N - count);
                let rel = run.start - before.base;
                assert!(run.start & (align - 1) == 0);
                assert!(all_in(&before.free, rel, count));
                assert!(run.zeroed == all_in(&before.zero, rel, count));
                if s < rel && s_aligned {
                    assert!(!all_in(&before.free, s, count));
                }
                let inside = s >= rel && s < rel + count;
                assert!(bit(&m.free, s) == (bit(&before.free, s) && !inside));
                assert!(bit(&m.zero, s) == (bit(&before.zero, s) && !inside));
            }
            None => {
                if s_aligned {
                    assert!(!all_in(&before.free, s, count));
                }
                assert!(same(&m.free, &before.free) && same(&m.zero, &before.zero));
            }
        }
        invariants(&m);
    }

    #[kani::proof]
    #[kani::unwind(4)]
    fn slices_alloc_small_page() {
        check_alloc(any_map(), 1, 1);
    }

    #[kani::proof]
    #[kani::unwind(4)]
    fn slices_alloc_medium_page() {
        check_alloc(any_map(), 8, 8);
    }

    #[kani::proof]
    #[kani::unwind(4)]
    fn slices_alloc_large_page() {
        check_alloc(any_map(), 64, 64);
    }

    #[kani::proof]
    #[kani::unwind(4)]
    fn slices_alloc_general_short_runs() {
        let count: usize = kani::any();
        let shift: u32 = kani::any();
        kani::assume(count >= 1 && count <= 6 && shift <= 7);
        check_alloc(any_map_with_runs(2), count, 1 << shift);
    }

    #[kani::proof]
    #[kani::unwind(4)]
    fn slices_alloc_general_word_spanning_runs() {
        let count: usize = kani::any();
        let shift: u32 = kani::any();
        kani::assume(count >= 63 && count <= 70 && shift <= 7);
        check_alloc(any_map_with_runs(2), count, 1 << shift);
    }

    #[kani::proof]
    #[kani::unwind(4)]
    fn slices_free_then_alloc_round_trips() {
        let (count, align) = match kani::any::<u8>() % 3 {
            0 => (1, 1),
            1 => (8, 8),
            _ => (64, 64),
        };
        let before = any_map();
        let mut m = snapshot(&before);
        let Some(run) = m.alloc(count, align) else {
            return;
        };
        m.free(run.start, count);
        assert!(same(&m.free, &before.free));
        let rel = run.start - before.base;
        let s: usize = kani::any();
        kani::assume(s < N);
        let inside = s >= rel && s < rel + count;
        assert!(bit(&m.zero, s) == (bit(&before.zero, s) && !inside));
        invariants(&m);
        let again = m.alloc(count, align);
        assert!(
            again
                == Some(Run {
                    start: run.start,
                    zeroed: false
                })
        );
    }

    /// `alloc_tail(count, align, base + below)` against its specification on `before`: with
    /// `end = min(below, N)` and `lo` the start of the maximal free run ending at `end`, the
    /// result is the lowest aligned index at or above `lo` if `count` slices fit between it and
    /// `end`, exactly its bits are cleared, and `zeroed` is right.
    #[kani::proof]
    #[kani::unwind(4)]
    fn slices_alloc_tail_takes_the_bottom_of_the_free_tail() {
        let before = any_map_with_runs(2);
        let count: usize = kani::any();
        let shift: u32 = kani::any();
        let below: usize = kani::any();
        kani::assume(count >= 1 && count <= 70 && shift <= 7 && below <= N + 2);
        let align = 1usize << shift;
        let end = if below < N { below } else { N };
        // The tail's start: one arbitrary candidate `lo` is checked to be it.
        let lo: usize = kani::any();
        kani::assume(lo <= end);
        kani::assume(all_in(&before.free, lo, end - lo));
        kani::assume(lo == 0 || !bit(&before.free, lo - 1));
        let over = (before.base + lo) & (align - 1);
        let start = if over == 0 { lo } else { lo + align - over };
        let expect = if end > lo && start <= end && end - start >= count {
            Some(start)
        } else {
            None
        };
        let mut m = snapshot(&before);
        let got = m.alloc_tail(count, align, before.base + below);
        assert!(got.map(|r| r.start - before.base) == expect);
        let s: usize = kani::any();
        kani::assume(s < N);
        match got {
            Some(run) => {
                let rel = run.start - before.base;
                assert!(run.zeroed == all_in(&before.zero, rel, count));
                let inside = s >= rel && s < rel + count;
                assert!(bit(&m.free, s) == (bit(&before.free, s) && !inside));
                assert!(bit(&m.zero, s) == (bit(&before.zero, s) && !inside));
            }
            None => {
                assert!(same(&m.free, &before.free) && same(&m.zero, &before.zero));
            }
        }
        invariants(&m);
    }

    #[kani::proof]
    #[kani::unwind(6)]
    fn slices_try_extend_claims_exactly_the_tail() {
        let before = any_map();
        let start: usize = kani::any();
        let count: usize = kani::any();
        let extra: usize = kani::any();
        kani::assume(count >= 1 && count <= 3 && start <= N - count && extra <= 4);
        kani::assume(none_in(&before.free, start, count));
        let tail = start + count;
        let fits = tail + extra <= N && all_in(&before.free, tail, extra);
        let mut m = snapshot(&before);
        let s: usize = kani::any();
        kani::assume(s < N);
        match m.try_extend(before.base + start, count, extra) {
            Some(zeroed) => {
                assert!(fits);
                let inside = s >= tail && s < tail + extra;
                assert!(bit(&m.free, s) == (bit(&before.free, s) && !inside));
                assert!(bit(&m.zero, s) == (bit(&before.zero, s) && !inside));
                assert!(zeroed == all_in(&before.zero, tail, extra));
                invariants(&m);
                m.shrink(before.base + start, count + extra, count);
                assert!(same(&m.free, &before.free));
            }
            None => {
                assert!(!fits);
                assert!(same(&m.free, &before.free) && same(&m.zero, &before.zero));
            }
        }
        invariants(&m);
    }

    /// One operation with arbitrary arguments that satisfy its preconditions. Allocation uses
    /// the page shapes; the general search has its own harnesses.
    fn any_op(m: &mut SliceMap<W>) {
        let start: usize = kani::any();
        let count: usize = kani::any();
        kani::assume(count >= 1 && count <= 4 && start <= N - count);
        let handed_out = none_in(&m.free, start, count);
        match kani::any::<u8>() % 5 {
            0 => {
                let n = if kani::any() { 1 } else { 8 };
                let _ = m.alloc(n, n);
            }
            1 => {
                kani::assume(handed_out);
                m.free(m.base + start, count);
            }
            2 => {
                kani::assume(handed_out);
                m.add_free(m.base + start, count, kani::any());
            }
            3 => {
                kani::assume(handed_out);
                let _ = m.try_extend(m.base + start, count, kani::any::<usize>() % 4);
            }
            _ => {
                kani::assume(handed_out);
                let new_count: usize = kani::any();
                kani::assume(new_count <= count);
                m.shrink(m.base + start, count, new_count);
            }
        }
    }

    #[kani::proof]
    #[kani::unwind(4)]
    fn slices_invariants_hold_across_operations() {
        let mut m = any_map_with_runs(2);
        for _ in 0..2 {
            any_op(&mut m);
            invariants(&m);
        }
    }

    /// A memory that grows on paper only. `acquire` must never touch memory, so `ptr` panics.
    struct PaperMemory {
        size: usize,
        capacity: usize,
        /// Slices someone else claims before the next grow, making it non-contiguous.
        skip: usize,
        /// End of memory `acquire` may never ask to pass (the map limit here; on wasm32 also
        /// the address space). Someone else's growth may still pass it, so the check is on
        /// each request, not on the resulting size.
        limit: usize,
    }

    // SAFETY: never hands out a pointer, so nothing about memory validity can be violated.
    unsafe impl Memory for PaperMemory {
        fn heap_base(&self) -> usize {
            0
        }

        fn size_slices(&self) -> usize {
            self.size
        }

        fn grow(&mut self, slices: usize) -> Option<usize> {
            assert!(self.size + slices <= self.limit);
            if kani::any() {
                return None;
            }
            let start = self.size + self.skip;
            let end = start.checked_add(slices)?;
            if end > self.capacity {
                return None;
            }
            self.size = end;
            self.skip = 0;
            Some(start)
        }

        fn ptr(&self, _addr: usize) -> *mut u8 {
            panic!("acquire must not touch memory")
        }
    }

    /// `extend_with_growth` on a handed-out run below the end of memory: it extends exactly the
    /// tail, grows memory only when the run is at the top (every slice between the run and the
    /// end of memory free) and the map alone cannot serve, never touches the run's own slices,
    /// and keeps a non-contiguous region.
    #[kani::proof]
    #[kani::unwind(5)]
    fn slices_extend_with_growth_extends_only_a_top_run() {
        let mut m = any_map_with_runs(2);
        let size: usize = kani::any();
        let capacity: usize = kani::any();
        let skip: usize = if kani::any() { 0 } else { 2 };
        kani::assume(size >= m.base && size <= m.limit());
        kani::assume(capacity >= size && capacity <= m.limit() + 8);
        let used = size - m.base;
        kani::assume(none_in(&m.free, used, N - used));
        let start: usize = kani::any();
        let count: usize = kani::any();
        let extra: usize = kani::any();
        kani::assume(count >= 1 && count <= 3 && extra >= 1 && extra <= 4);
        kani::assume(start <= used && count <= used - start);
        kani::assume(none_in(&m.free, start, count));
        let before = snapshot(&m);
        let tail = start + count;
        let have = used - tail;
        let at_top = all_in(&before.free, tail, have);
        let fits_in_map = extra <= have && all_in(&before.free, tail, extra);
        let mut mem = PaperMemory {
            size,
            capacity,
            skip,
            limit: m.usable_limit(),
        };
        let policy = GrowPolicy {
            min_grow: kani::any::<usize>() % 4,
            max_grow: kani::any::<usize>() % 4,
            step_divisor: if kani::any() { 2 } else { 8 },
        };
        let base = m.base;
        let got = extend_with_growth(&mut m, &mut mem, base + start, count, extra, &policy);
        let grown = mem.size - base;
        let s: usize = kani::any();
        kani::assume(s < N);
        let inside = s >= tail && s < tail + extra;
        let fresh = s >= used && s < grown;
        match got {
            Some(zeroed) => {
                assert!(none_in(&m.free, tail, extra));
                assert!(tail + extra <= grown);
                if fits_in_map {
                    assert!(mem.size == size);
                    assert!(zeroed == all_in(&before.zero, tail, extra));
                } else {
                    assert!(at_top && mem.size > size && grown >= tail + extra);
                }
                if inside {
                    assert!(!bit(&m.free, s) && !bit(&m.zero, s));
                } else if fresh {
                    assert!(bit(&m.free, s) && bit(&m.zero, s));
                } else {
                    assert!(bit(&m.free, s) == bit(&before.free, s));
                    assert!(bit(&m.zero, s) == bit(&before.zero, s));
                }
            }
            None => {
                assert!(!fits_in_map);
                assert!(mem.size == size || at_top);
                if s < used {
                    assert!(bit(&m.free, s) == bit(&before.free, s));
                    assert!(bit(&m.zero, s) == bit(&before.zero, s));
                }
            }
        }
        assert!(none_in(&m.free, start, count));
        invariants(&m);
    }

    #[kani::proof]
    #[kani::unwind(4)]
    fn slices_acquire_stays_inside_memory_and_the_map() {
        let mut m = any_map_with_runs(2);
        let size: usize = kani::any();
        let capacity: usize = kani::any();
        let skip: usize = kani::any();
        kani::assume(size >= m.base && size <= m.limit());
        kani::assume(capacity >= size && capacity <= m.limit() + 8 && skip <= 3);
        // The map cannot describe slices beyond the end of memory as free.
        let used = size - m.base;
        kani::assume(none_in(&m.free, used, N - used));
        let mut mem = PaperMemory {
            size,
            capacity,
            skip,
            limit: m.usable_limit(),
        };
        let (count, align) = match kani::any::<u8>() % 3 {
            0 => (1, 1),
            1 => (8, 8),
            _ => (64, 64),
        };
        let policy = GrowPolicy {
            min_grow: kani::any::<usize>() % 8,
            max_grow: kani::any::<usize>() % 8,
            step_divisor: if kani::any() { 2 } else { 8 },
        };
        let had_run = m.has_run(count, align);
        let run = acquire(&mut m, &mut mem, count, align, &policy);
        assert!(!had_run || mem.size == size);
        if let Some(run) = run {
            assert!(run.start & (align - 1) == 0);
            assert!(run.start >= m.base && run.start + count <= mem.size);
            assert!(none_in(&m.free, run.start - m.base, count));
        }
        invariants(&m);
    }
}
