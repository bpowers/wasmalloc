//! The model-based tester: the `GlobalAlloc` contract, checked one operation at a time.
//!
//! The model keeps a table of the blocks it believes are live: address, `Layout`, and a seed from
//! which every byte it wrote into the block can be recomputed. A weighted random stream of
//! operations drives the allocator under test through [`RawAlloc`], and after each operation the
//! model checks what the contract promises:
//!
//! - `alloc` and `alloc_zeroed` return a block aligned to the Layout that overlaps no live block
//!   (a sorted map of live intervals makes this one logarithmic lookup, not a scan);
//! - `alloc_zeroed` returns zeros;
//! - a block holds exactly what the model wrote until it is freed, checked when the block is
//!   freed or resized and at periodic sweeps that free everything;
//! - `realloc` preserves the first `min(old, new)` bytes and returns an aligned block that
//!   overlaps no live block.
//!
//! Contents are a position-dependent pattern derived from the block's seed. So that debug builds
//! stay fast on multi-megabyte blocks the pattern is dense over the first [`DENSE_PREFIX`] bytes
//! and sampled beyond that: one word every [`STRIDE`] bytes, plus the last word of the block. The
//! sampled positions depend only on the offset, never on the block length, so a prefix of a block
//! can still be verified after `realloc` has changed its length. The price is that a corruption
//! of fewer than [`STRIDE`] bytes in the middle of a block larger than [`DENSE_PREFIX`] can go
//! unnoticed; alignment, overlap and the dense prefix are always checked exactly.
//!
//! A run is reproducible from `(seed, ops, profile)`. The [`Failure`] it returns names the
//! operation that detected the problem and carries the seed. Allocation failure is a failure of
//! the run as well: every profile bounds live bytes ([`Profile::max_live_bytes`]) so that an
//! allocator with enough backing memory never has a legitimate reason to refuse.

use core::alloc::{GlobalAlloc, Layout};
use core::fmt;
use core::ptr::{self, NonNull};
use std::alloc::System;
use std::collections::{BTreeMap, VecDeque};
use std::format;
use std::string::String;

use super::rng::{Entropy, Rng, mix64};

/// The four `GlobalAlloc` operations with `&mut self` receivers, plus a footprint probe.
///
/// The model drives anything implementing this: `std::alloc::System`, the crate's heap over a
/// simulated memory ([`super::sim::SimHeap`]), or a deliberately broken wrapper. The safety
/// contracts are exactly those of [`GlobalAlloc`].
pub trait RawAlloc {
    /// Allocate a block for `layout`, or `None` if memory cannot be obtained.
    ///
    /// # Safety
    ///
    /// `layout.size()` must be non-zero.
    unsafe fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>>;

    /// Allocate a zero-filled block for `layout`.
    ///
    /// # Safety
    ///
    /// As for [`alloc`](Self::alloc).
    unsafe fn alloc_zeroed(&mut self, layout: Layout) -> Option<NonNull<u8>>;

    /// Free a block.
    ///
    /// # Safety
    ///
    /// `ptr` must have been returned by this allocator for `layout` and not freed since.
    unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout);

    /// Resize a block, preserving `min(layout.size(), new_size)` bytes. Returns the possibly
    /// moved block, or `None` if memory cannot be obtained, in which case the old block is left
    /// untouched and still live.
    ///
    /// # Safety
    ///
    /// As for [`dealloc`](Self::dealloc); `new_size` must be non-zero and, rounded up to
    /// `layout.align()`, must not overflow `isize`.
    unsafe fn realloc(
        &mut self,
        ptr: NonNull<u8>,
        layout: Layout,
        new_size: usize,
    ) -> Option<NonNull<u8>>;

    /// Bytes of backing memory the allocator currently holds, if it can tell.
    fn footprint_bytes(&self) -> Option<usize>;
}

impl RawAlloc for System {
    unsafe fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        NonNull::new(unsafe { <System as GlobalAlloc>::alloc(self, layout) })
    }

    unsafe fn alloc_zeroed(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        NonNull::new(unsafe { <System as GlobalAlloc>::alloc_zeroed(self, layout) })
    }

    unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: same contract as this method.
        unsafe { <System as GlobalAlloc>::dealloc(self, ptr.as_ptr(), layout) }
    }

    unsafe fn realloc(
        &mut self,
        ptr: NonNull<u8>,
        layout: Layout,
        new_size: usize,
    ) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        NonNull::new(unsafe {
            <System as GlobalAlloc>::realloc(self, ptr.as_ptr(), layout, new_size)
        })
    }

    fn footprint_bytes(&self) -> Option<usize> {
        None
    }
}

// ----------------------------------------------------------------------------------------------
// Block contents
// ----------------------------------------------------------------------------------------------

/// Bytes at the start of every block whose pattern is written and checked densely.
pub const DENSE_PREFIX: usize = 64 * 1024;
/// Beyond the dense prefix one 8-byte word is written and checked every this many bytes.
pub const STRIDE: usize = 4096;

const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;
const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
/// Upper bound of the small size class, after talc's random-actions benchmark.
const SMALL_MAX: usize = 10_000;

/// The pattern word covering bytes `8 * chunk..8 * chunk + 8` of a block with the given seed.
/// XOR with a multiple of an odd constant makes every chunk of a block distinct in every byte, so
/// a shifted copy or a copy from another block never matches.
#[inline]
fn word(seed: u64, chunk: usize) -> u64 {
    seed ^ (chunk as u64).wrapping_mul(GOLDEN)
}

/// The byte ranges of a `len`-byte block that carry the pattern, clipped to the first `limit`
/// bytes, in increasing order. Every range starts at a multiple of 8. The tail word is included
/// only when the whole block is covered (`limit == len`): it is the one range whose position
/// depends on the block length, and after a `realloc` the length has changed.
struct Spans {
    len: usize,
    limit: usize,
    state: SpanState,
}

enum SpanState {
    Dense,
    Stride(usize),
    Tail,
    Done,
}

fn spans(len: usize, limit: usize) -> Spans {
    debug_assert!(limit <= len);
    Spans {
        len,
        limit,
        state: SpanState::Dense,
    }
}

impl Iterator for Spans {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<(usize, usize)> {
        loop {
            match self.state {
                SpanState::Dense => {
                    self.state = SpanState::Stride(DENSE_PREFIX);
                    let end = self.limit.min(DENSE_PREFIX);
                    if end > 0 {
                        return Some((0, end));
                    }
                }
                SpanState::Stride(start) => {
                    if start + 8 <= self.limit {
                        self.state = SpanState::Stride(start + STRIDE);
                        return Some((start, start + 8));
                    }
                    self.state = SpanState::Tail;
                }
                SpanState::Tail => {
                    self.state = SpanState::Done;
                    if self.limit == self.len && self.len > DENSE_PREFIX {
                        // The word holding the last byte, clipped to the block. It may coincide
                        // with a stride word, which is harmless: same bytes, same expectation.
                        return Some(((self.len - 1) & !7, self.len));
                    }
                }
                SpanState::Done => return None,
            }
        }
    }
}

/// The first byte that did not hold the expected value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mismatch {
    offset: usize,
    expected: u8,
    found: u8,
}

/// Write the pattern of `seed` over the `len`-byte block at `p`.
///
/// # Safety
///
/// `p` must be valid for writes of `len` bytes.
unsafe fn fill(p: *mut u8, len: usize, seed: u64) {
    for (start, end) in spans(len, len) {
        let mut off = start;
        while off + 8 <= end {
            // SAFETY: `off + 8 <= end <= len`, inside the block.
            unsafe {
                p.add(off)
                    .cast::<u64>()
                    .write_unaligned(word(seed, off / 8))
            };
            off += 8;
        }
        if off < end {
            let bytes = word(seed, off / 8).to_ne_bytes();
            // SAFETY: `end - off < 8` bytes starting at `off`, all below `len`.
            unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), p.add(off), end - off) };
        }
    }
}

/// Check the first `limit` bytes of the `len`-byte block at `p` against the pattern of `seed`
/// (`Some(seed)`) or against zero (`None`).
///
/// # Safety
///
/// `p` must be valid for reads of `len` bytes.
unsafe fn verify(
    p: *const u8,
    len: usize,
    limit: usize,
    seed: Option<u64>,
) -> Result<(), Mismatch> {
    for (start, end) in spans(len, limit) {
        let mut off = start;
        while off + 8 <= end {
            let expected = seed.map_or(0, |s| word(s, off / 8));
            // SAFETY: `off + 8 <= end <= limit <= len`, inside the block.
            let found = unsafe { p.add(off).cast::<u64>().read_unaligned() };
            if found != expected {
                return Err(first_mismatch(
                    off,
                    expected.to_ne_bytes(),
                    found.to_ne_bytes(),
                    8,
                ));
            }
            off += 8;
        }
        if off < end {
            let expected = seed.map_or(0, |s| word(s, off / 8)).to_ne_bytes();
            let mut found = [0u8; 8];
            // SAFETY: `end - off < 8` bytes starting at `off`, all below `len`.
            unsafe { ptr::copy_nonoverlapping(p.add(off), found.as_mut_ptr(), end - off) };
            if found[..end - off] != expected[..end - off] {
                return Err(first_mismatch(off, expected, found, end - off));
            }
        }
    }
    Ok(())
}

fn first_mismatch(base: usize, expected: [u8; 8], found: [u8; 8], n: usize) -> Mismatch {
    let i = (0..n)
        .find(|&i| expected[i] != found[i])
        .expect("caller saw a difference");
    Mismatch {
        offset: base + i,
        expected: expected[i],
        found: found[i],
    }
}

// ----------------------------------------------------------------------------------------------
// Profiles
// ----------------------------------------------------------------------------------------------

/// Which live block `dealloc` frees, and the order a sweep frees everything in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// A uniformly random live block.
    Random,
    /// The most recently allocated block (stack-like).
    Lifo,
    /// The oldest live block (queue-like).
    Fifo,
}

/// The shape of an operation stream. Weights are relative; a zero weight disables the entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    /// Name used in failure reports.
    pub name: &'static str,
    /// Weights of `alloc`, `alloc_zeroed`, `dealloc` and `realloc`. With a batch size the
    /// `dealloc` weight is ignored: frees come from the free phase of each batch.
    pub ops: [u32; 4],
    /// Weights of the size classes: 1 to 10000 bytes with every doubling equally likely, 10 KiB
    /// to 100 KiB, 100 KiB to 4 MiB, and 4 MiB to 64 MiB, each uniform.
    pub sizes: [u32; 4],
    /// Weights of the alignment classes: 1 to 8 (uniform over the four powers of two), 16, 32,
    /// 64, 4096, 8192 to 65536 (uniform over the four powers of two), and the fixed request
    /// `Layout::from_size_align(1, 65536)`.
    pub aligns: [u32; 7],
    /// Free every live block (verifying each) every this many operations; 0 never sweeps.
    pub sweep_every: usize,
    /// Live bytes never exceed this: an allocation that would is turned into a free (or clamped
    /// when nothing is live), so an allocator with this much backing memory, plus its own
    /// overhead, never has a reason to fail.
    pub max_live_bytes: usize,
    /// How `dealloc` picks its victim.
    pub order: Order,
    /// Non-zero for batch mode: allocate a random number of blocks up to this, then free every
    /// live block in `order`, and repeat.
    pub batch: usize,
}

const DEFAULT_ALIGNS: [u32; 7] = [75, 8, 6, 5, 3, 2, 1];

impl Profile {
    /// Small blocks only, random frees, no sweeps: the fast paths under steady churn.
    pub const SMALL_CHURN: Profile = Profile {
        name: "small_churn",
        ops: [45, 10, 35, 10],
        sizes: [1, 0, 0, 0],
        aligns: [85, 6, 5, 4, 0, 0, 0],
        sweep_every: 0,
        max_live_bytes: 64 * MIB,
        order: Order::Random,
        batch: 0,
    };

    /// Every size and alignment class in realistic proportions, with plenty of realloc.
    pub const MIXED: Profile = Profile {
        name: "mixed",
        ops: [35, 10, 35, 20],
        sizes: [920, 50, 25, 5],
        aligns: DEFAULT_ALIGNS,
        sweep_every: 5000,
        max_live_bytes: 256 * MIB,
        order: Order::Random,
        batch: 0,
    };

    /// Mostly medium, large and huge blocks: page kinds, singleton runs and memory growth.
    pub const LARGE_HEAVY: Profile = Profile {
        name: "large_heavy",
        ops: [40, 10, 40, 10],
        sizes: [300, 300, 380, 20],
        aligns: [80, 5, 5, 5, 3, 1, 1],
        sweep_every: 500,
        max_live_bytes: 256 * MIB,
        order: Order::Random,
        batch: 0,
    };

    /// Every alignment class about equally often, including the over-aligned singleton requests.
    pub const ALIGN_HEAVY: Profile = Profile {
        name: "align_heavy",
        ops: [40, 10, 40, 10],
        sizes: [900, 80, 20, 0],
        aligns: [20, 15, 15, 15, 15, 10, 10],
        sweep_every: 2000,
        max_live_bytes: 128 * MIB,
        order: Order::Random,
        batch: 0,
    };

    /// Allocate a batch, free it newest-first, repeat: stack-shaped lifetimes.
    pub const LIFO_BATCHES: Profile = Profile {
        name: "lifo_batches",
        ops: [45, 5, 40, 10],
        sizes: [950, 50, 0, 0],
        aligns: DEFAULT_ALIGNS,
        sweep_every: 0,
        max_live_bytes: 64 * MIB,
        order: Order::Lifo,
        batch: 256,
    };

    /// Allocate a batch, free it oldest-first, repeat: queue-shaped lifetimes.
    pub const FIFO_BATCHES: Profile = Profile {
        name: "fifo_batches",
        ops: [45, 5, 40, 10],
        sizes: [950, 50, 0, 0],
        aligns: DEFAULT_ALIGNS,
        sweep_every: 0,
        max_live_bytes: 64 * MIB,
        order: Order::Fifo,
        batch: 256,
    };

    /// Every preset.
    pub const fn all() -> [Profile; 6] {
        [
            Self::SMALL_CHURN,
            Self::MIXED,
            Self::LARGE_HEAVY,
            Self::ALIGN_HEAVY,
            Self::LIFO_BATCHES,
            Self::FIFO_BATCHES,
        ]
    }

    fn validate(&self) {
        assert!(
            self.ops[0] + self.ops[1] > 0,
            "{}: no allocation weight",
            self.name
        );
        assert!(
            self.sizes.iter().sum::<u32>() > 0,
            "{}: no size weight",
            self.name
        );
        assert!(
            self.aligns.iter().sum::<u32>() > 0,
            "{}: no alignment weight",
            self.name
        );
        assert!(self.max_live_bytes > 0, "{}: zero live-byte cap", self.name);
    }
}

// ----------------------------------------------------------------------------------------------
// Results
// ----------------------------------------------------------------------------------------------

/// Counters from a completed run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Operations executed (fewer than requested only when the entropy source ran dry).
    pub ops: usize,
    /// Calls to `alloc`.
    pub allocs: usize,
    /// Calls to `alloc_zeroed`.
    pub zeroed_allocs: usize,
    /// Calls to `dealloc`, including sweeps and the final drain.
    pub deallocs: usize,
    /// Calls to `realloc`.
    pub reallocs: usize,
    /// High-water mark of bytes requested and live at once.
    pub peak_live_bytes: usize,
    /// High-water mark of blocks live at once.
    pub peak_live_blocks: usize,
    /// High-water mark of [`RawAlloc::footprint_bytes`], if the allocator reports one.
    pub peak_footprint_bytes: Option<usize>,
}

impl Stats {
    /// Peak footprint over peak live bytes: how much backing memory the allocator needed per
    /// byte the program held at its busiest. `None` without a footprint or without allocations.
    pub fn fragmentation(&self) -> Option<f64> {
        let footprint = self.peak_footprint_bytes?;
        (self.peak_live_bytes > 0).then(|| footprint as f64 / self.peak_live_bytes as f64)
    }
}

/// What the model caught.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// A returned block was not aligned to its Layout.
    Misaligned,
    /// A returned block overlapped a live block (or wrapped around the address space).
    Overlap,
    /// A live block no longer held what the model wrote.
    Corrupted,
    /// `alloc_zeroed` returned a non-zero byte.
    NotZeroed,
    /// `realloc` did not preserve the first `min(old, new)` bytes.
    ReallocLostBytes,
    /// `alloc`, `alloc_zeroed` or `realloc` returned `None` within the live-byte cap.
    AllocFailed,
}

/// A contract violation, with everything needed to reproduce it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// The violated property.
    pub kind: FailureKind,
    /// Zero-based index of the operation that detected it. A `Corrupted` failure was caused by
    /// some earlier operation; this is merely when the model looked.
    pub op_index: usize,
    /// [`Profile::name`] of the run.
    pub profile: &'static str,
    /// The PRNG seed, when the run came from [`run`] rather than an arbitrary entropy source.
    pub seed: Option<u64>,
    /// Addresses, layouts and bytes involved.
    pub detail: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} at op {} of profile {}",
            self.kind, self.op_index, self.profile
        )?;
        if let Some(seed) = self.seed {
            write!(f, " (seed {seed:#x})")?;
        }
        write!(f, ": {}", self.detail)?;
        if let Some(seed) = self.seed {
            write!(
                f,
                ". Reproduce with model::run(alloc, {seed:#x}, ops, Profile::{})",
                self.profile.to_uppercase()
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for Failure {}

// ----------------------------------------------------------------------------------------------
// The model
// ----------------------------------------------------------------------------------------------

struct Block {
    ptr: NonNull<u8>,
    layout: Layout,
    seed: u64,
}

enum Op {
    Alloc { layout: Layout, zeroed: bool },
    Dealloc,
    Realloc { index: usize, new_size: usize },
}

struct Model<'a, A: RawAlloc> {
    alloc: &'a mut A,
    profile: Profile,
    /// Live blocks in allocation order; realloc moves its block to the back.
    live: VecDeque<Block>,
    /// Live intervals `start -> end`, for the overlap check.
    by_addr: BTreeMap<usize, usize>,
    live_bytes: usize,
    stats: Stats,
    op_index: usize,
    /// Block seeds come from a counter rather than the entropy source so that a fuzzer's bytes
    /// all go into decisions.
    next_seed: u64,
    /// Batch mode: allocations left in the current allocation phase.
    batch_left: usize,
    /// Batch mode: frees left in the current free phase.
    free_left: usize,
}

impl<'a, A: RawAlloc> Model<'a, A> {
    fn new(alloc: &'a mut A, profile: Profile) -> Self {
        Model {
            alloc,
            profile,
            live: VecDeque::new(),
            by_addr: BTreeMap::new(),
            live_bytes: 0,
            stats: Stats::default(),
            op_index: 0,
            next_seed: 0,
            batch_left: 0,
            free_left: 0,
        }
    }

    fn fail(&self, kind: FailureKind, detail: String) -> Failure {
        Failure {
            kind,
            op_index: self.op_index,
            profile: self.profile.name,
            seed: None,
            detail,
        }
    }

    fn fresh_seed(&mut self) -> u64 {
        self.next_seed += 1;
        mix64(self.next_seed)
    }

    // -------------------------------------------------------------------------------------
    // Choosing operations
    // -------------------------------------------------------------------------------------

    fn pick_size<E: Entropy>(&self, e: &mut E) -> Option<usize> {
        Some(match e.weighted(&self.profile.sizes)? {
            0 => {
                // Log-uniform: pick a doubling, then a size within it, so bins of every size get
                // exercised instead of the largest sizes dominating.
                let k = e.below(14)? as usize;
                let lo = 1usize << k;
                let hi = ((1usize << (k + 1)) - 1).min(SMALL_MAX);
                e.range(lo, hi)?
            }
            1 => e.range(10 * KIB, 100 * KIB)?,
            2 => e.range(100 * KIB, 4 * MIB)?,
            _ => e.range(4 * MIB, 64 * MIB)?,
        })
    }

    fn pick_layout<E: Entropy>(&self, e: &mut E) -> Option<Layout> {
        let class = e.weighted(&self.profile.aligns)?;
        if class == 6 {
            return Some(Layout::from_size_align(1, 65536).expect("valid layout"));
        }
        let size = self.pick_size(e)?;
        let align = match class {
            0 => 1usize << e.below(4)?,
            1 => 16,
            2 => 32,
            3 => 64,
            4 => 4096,
            _ => 1usize << e.range(13, 16)?,
        };
        Some(Layout::from_size_align(size, align).expect("sizes stay far below isize::MAX"))
    }

    /// Operation class in batch mode: allocations until the batch is full, then frees until
    /// nothing is live.
    fn batch_class<E: Entropy>(&mut self, e: &mut E) -> Option<usize> {
        if self.batch_left == 0 && self.free_left == 0 {
            self.batch_left = e.range(1, self.profile.batch)?;
        }
        if self.batch_left > 0 {
            let w = [
                self.profile.ops[0],
                self.profile.ops[1],
                0,
                self.profile.ops[3],
            ];
            return e.weighted(&w);
        }
        Some(2)
    }

    fn next_op<E: Entropy>(&mut self, e: &mut E) -> Option<Op> {
        let class = if self.profile.batch > 0 {
            self.batch_class(e)?
        } else {
            e.weighted(&self.profile.ops)?
        };
        // Nothing to free or resize: allocate instead.
        let class = if class >= 2 && self.live.is_empty() {
            0
        } else {
            class
        };
        match class {
            0 | 1 => {
                let mut layout = self.pick_layout(e)?;
                let room = self.profile.max_live_bytes - self.live_bytes;
                if layout.size() > room {
                    if !self.live.is_empty() {
                        return Some(Op::Dealloc);
                    }
                    layout = Layout::from_size_align(room.max(1), layout.align())
                        .expect("smaller than the original layout");
                }
                Some(Op::Alloc {
                    layout,
                    zeroed: class == 1,
                })
            }
            2 => Some(Op::Dealloc),
            _ => {
                let index = e.below(self.live.len() as u32)? as usize;
                let old = self.live[index].layout.size();
                // Half the time a size near the old one, to hit in-place growth and shrinking;
                // otherwise a fresh draw, to hop between size classes and page kinds.
                let new_size = if e.below(2)? == 0 {
                    e.range((old / 2).max(1), old.saturating_mul(2))?
                } else {
                    self.pick_size(e)?
                };
                let room = self.profile.max_live_bytes - self.live_bytes + old;
                Some(Op::Realloc {
                    index,
                    new_size: new_size.min(room),
                })
            }
        }
    }

    // -------------------------------------------------------------------------------------
    // Executing operations
    // -------------------------------------------------------------------------------------

    /// Check a freshly returned block for alignment and overlap, and record its interval.
    fn admit(&mut self, what: &str, ptr: NonNull<u8>, layout: Layout) -> Result<(), Failure> {
        let start = ptr.addr().get();
        if start % layout.align() != 0 {
            return Err(self.fail(
                FailureKind::Misaligned,
                format!("{what} returned {start:#x} for {layout:?}"),
            ));
        }
        let Some(end) = start.checked_add(layout.size()) else {
            return Err(self.fail(
                FailureKind::Overlap,
                format!("{what} returned {start:#x} for {layout:?}, which wraps the address space"),
            ));
        };
        // Live intervals are disjoint, so the only one that can reach into `[start, end)` is the
        // last one starting below `end`.
        if let Some((&s, &e)) = self.by_addr.range(..end).next_back() {
            if e > start {
                return Err(self.fail(
                    FailureKind::Overlap,
                    format!(
                        "{what} returned [{start:#x}, {end:#x}) for {layout:?}, overlapping the \
                         live block [{s:#x}, {e:#x})"
                    ),
                ));
            }
        }
        self.by_addr.insert(start, end);
        Ok(())
    }

    fn record_live(&mut self, block: Block) {
        self.live_bytes += block.layout.size();
        self.live.push_back(block);
        self.stats.peak_live_bytes = self.stats.peak_live_bytes.max(self.live_bytes);
        self.stats.peak_live_blocks = self.stats.peak_live_blocks.max(self.live.len());
        if let Some(f) = self.alloc.footprint_bytes() {
            self.stats.peak_footprint_bytes =
                Some(self.stats.peak_footprint_bytes.map_or(f, |p| p.max(f)));
        }
    }

    fn do_alloc(&mut self, layout: Layout, zeroed: bool) -> Result<(), Failure> {
        let what = if zeroed { "alloc_zeroed" } else { "alloc" };
        // SAFETY: the model never draws a zero size.
        let ptr = unsafe {
            if zeroed {
                self.alloc.alloc_zeroed(layout)
            } else {
                self.alloc.alloc(layout)
            }
        };
        let Some(ptr) = ptr else {
            return Err(self.fail(
                FailureKind::AllocFailed,
                format!(
                    "{what} returned None for {layout:?} with {} bytes live",
                    self.live_bytes
                ),
            ));
        };
        self.admit(what, ptr, layout)?;
        let len = layout.size();
        if zeroed {
            // SAFETY: the allocator returned `len` readable bytes at `ptr`.
            if let Err(m) = unsafe { verify(ptr.as_ptr(), len, len, None) } {
                return Err(self.fail(
                    FailureKind::NotZeroed,
                    format!(
                        "alloc_zeroed returned {:#x} for {layout:?} with byte {} = {:#04x}",
                        ptr.addr().get(),
                        m.offset,
                        m.found
                    ),
                ));
            }
            self.stats.zeroed_allocs += 1;
        } else {
            self.stats.allocs += 1;
        }
        let seed = self.fresh_seed();
        // SAFETY: the allocator returned `len` writable bytes at `ptr`.
        unsafe { fill(ptr.as_ptr(), len, seed) };
        self.record_live(Block { ptr, layout, seed });
        if self.batch_left > 0 {
            self.batch_left -= 1;
            if self.batch_left == 0 {
                self.free_left = self.live.len();
            }
        }
        Ok(())
    }

    /// Verify a block's contents, then free it.
    fn release(&mut self, block: Block) -> Result<(), Failure> {
        let len = block.layout.size();
        // SAFETY: the block is live: `len` readable bytes at `ptr`.
        if let Err(m) = unsafe { verify(block.ptr.as_ptr(), len, len, Some(block.seed)) } {
            return Err(self.fail(
                FailureKind::Corrupted,
                format!(
                    "block {:#x} ({:?}) has byte {} = {:#04x}, expected {:#04x}",
                    block.ptr.addr().get(),
                    block.layout,
                    m.offset,
                    m.found,
                    m.expected
                ),
            ));
        }
        self.by_addr.remove(&block.ptr.addr().get());
        self.live_bytes -= len;
        // SAFETY: the block was returned by this allocator for this layout and is live.
        unsafe { self.alloc.dealloc(block.ptr, block.layout) };
        self.stats.deallocs += 1;
        self.free_left = self.free_left.saturating_sub(1);
        Ok(())
    }

    fn do_dealloc<E: Entropy>(&mut self, e: &mut E) -> Result<bool, Failure> {
        let victim = match self.profile.order {
            Order::Random => {
                let Some(i) = e.below(self.live.len() as u32) else {
                    return Ok(false);
                };
                self.live.swap_remove_back(i as usize)
            }
            Order::Lifo => self.live.pop_back(),
            Order::Fifo => self.live.pop_front(),
        };
        let block = victim.expect("dealloc is only chosen with live blocks");
        self.release(block)?;
        Ok(true)
    }

    fn do_realloc(&mut self, index: usize, new_size: usize) -> Result<(), Failure> {
        let old = self
            .live
            .swap_remove_back(index)
            .expect("index drawn below the live count");
        let old_len = old.layout.size();
        let old_layout = old.layout;
        let old_addr = old.ptr.addr().get();
        // SAFETY: the block is live: `old_len` readable bytes at `ptr`.
        if let Err(m) = unsafe { verify(old.ptr.as_ptr(), old_len, old_len, Some(old.seed)) } {
            return Err(self.fail(
                FailureKind::Corrupted,
                format!(
                    "block {old_addr:#x} ({:?}) has byte {} = {:#04x}, expected {:#04x} before \
                     realloc",
                    old.layout, m.offset, m.found, m.expected
                ),
            ));
        }
        self.by_addr.remove(&old_addr);
        let new_layout =
            Layout::from_size_align(new_size, old.layout.align()).expect("valid layout");
        // SAFETY: the block is live for `old.layout`; `new_size` is non-zero and far below
        // isize::MAX.
        let moved = unsafe { self.alloc.realloc(old.ptr, old.layout, new_size) };
        let Some(ptr) = moved else {
            // The contract leaves the old block live and untouched.
            self.by_addr.insert(old_addr, old_addr + old_len);
            self.live.push_back(old);
            return Err(self.fail(
                FailureKind::AllocFailed,
                format!(
                    "realloc of {old_addr:#x} ({old_layout:?}) to {new_size} returned None with \
                     {} bytes live",
                    self.live_bytes
                ),
            ));
        };
        self.live_bytes -= old_len;
        self.admit("realloc", ptr, new_layout)?;
        let keep = old_len.min(new_size);
        // SAFETY: the new block has `new_size >= keep` readable bytes.
        if let Err(m) = unsafe { verify(ptr.as_ptr(), old_len, keep, Some(old.seed)) } {
            return Err(self.fail(
                FailureKind::ReallocLostBytes,
                format!(
                    "realloc of {old_addr:#x} ({:?}) to {new_size} returned {:#x} with byte {} = \
                     {:#04x}, expected {:#04x}",
                    old.layout,
                    ptr.addr().get(),
                    m.offset,
                    m.found,
                    m.expected
                ),
            ));
        }
        let seed = self.fresh_seed();
        // SAFETY: the new block has `new_size` writable bytes.
        unsafe { fill(ptr.as_ptr(), new_size, seed) };
        self.record_live(Block {
            ptr,
            layout: new_layout,
            seed,
        });
        self.stats.reallocs += 1;
        Ok(())
    }

    /// Free every live block, verifying each, in the profile's order (newest first for
    /// `Random`, which needs no entropy).
    fn drain(&mut self) -> Result<(), Failure> {
        loop {
            let block = match self.profile.order {
                Order::Fifo => self.live.pop_front(),
                Order::Lifo | Order::Random => self.live.pop_back(),
            };
            match block {
                Some(block) => self.release(block)?,
                None => return Ok(()),
            }
        }
    }
}

/// Drive `alloc` with `ops` operations drawn from `entropy` in the shape of `profile`, then free
/// everything. Stops early, without failure, when the entropy source runs dry, which is how a
/// fuzzer's input ends a run.
pub fn run_with<A: RawAlloc, E: Entropy>(
    alloc: &mut A,
    entropy: &mut E,
    ops: usize,
    profile: Profile,
) -> Result<Stats, Failure> {
    profile.validate();
    let mut m = Model::new(alloc, profile);
    for i in 0..ops {
        m.op_index = i;
        let Some(op) = m.next_op(entropy) else { break };
        match op {
            Op::Alloc { layout, zeroed } => m.do_alloc(layout, zeroed)?,
            Op::Dealloc => {
                if !m.do_dealloc(entropy)? {
                    break;
                }
            }
            Op::Realloc { index, new_size } => m.do_realloc(index, new_size)?,
        }
        m.stats.ops = i + 1;
        if profile.sweep_every > 0 && (i + 1) % profile.sweep_every == 0 {
            m.drain()?;
        }
    }
    m.drain()?;
    Ok(m.stats)
}

/// [`run_with`] over the PRNG seeded with `seed`; the failure, if any, carries the seed.
pub fn run<A: RawAlloc>(
    alloc: &mut A,
    seed: u64,
    ops: usize,
    profile: Profile,
) -> Result<Stats, Failure> {
    let mut rng = Rng::new(seed);
    run_with(alloc, &mut rng, ops, profile).map_err(|mut f| {
        f.seed = Some(seed);
        f
    })
}

/// [`run`], panicking with the failure report (seed included) on a contract violation.
pub fn check<A: RawAlloc>(alloc: &mut A, seed: u64, ops: usize, profile: Profile) -> Stats {
    match run(alloc, seed, ops, profile) {
        Ok(stats) => stats,
        Err(failure) => panic!("{failure}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::rng::ByteSource;
    use std::vec;
    use std::vec::Vec;

    fn covered(len: usize, limit: usize) -> Vec<bool> {
        let mut c = vec![false; len];
        for (a, b) in spans(len, limit) {
            assert!(
                a % 8 == 0 && a < b && b <= limit,
                "span [{a}, {b}) for len {len}"
            );
            c[a..b].iter_mut().for_each(|x| *x = true);
        }
        c
    }

    #[test]
    fn spans_are_dense_for_small_blocks_and_sampled_for_large_ones() {
        for len in [1, 7, 8, 9, 100, DENSE_PREFIX - 1, DENSE_PREFIX] {
            assert!(covered(len, len).iter().all(|&x| x), "len {len} not dense");
        }
        let len = DENSE_PREFIX + 3 * STRIDE + 29;
        let c = covered(len, len);
        assert!(c[..DENSE_PREFIX].iter().all(|&x| x));
        let tail = (len - 1) & !7;
        for s in (DENSE_PREFIX..len).step_by(STRIDE) {
            assert!(c[s..s + 8].iter().all(|&x| x), "stride word at {s}");
            if s + 8 < tail {
                assert!(!c[s + 8], "byte after the stride word at {s}");
            }
        }
        assert!(c[tail..].iter().all(|&x| x), "tail bytes");
        assert!(!c[tail - 1], "byte before the tail word");
        // A prefix check covers only whole stride words inside the prefix and never the tail,
        // whose position depended on the old length.
        let p = covered(len, DENSE_PREFIX + STRIDE + 12);
        assert!(
            p[DENSE_PREFIX + STRIDE..DENSE_PREFIX + STRIDE + 8]
                .iter()
                .all(|&x| x)
        );
        assert!(!p[DENSE_PREFIX + STRIDE + 8..].iter().any(|&x| x));
        let q = covered(len, DENSE_PREFIX + STRIDE + 4);
        assert!(!q[DENSE_PREFIX + 8..].iter().any(|&x| x));
    }

    #[test]
    fn fill_and_verify_agree_and_catch_single_byte_changes() {
        let len = DENSE_PREFIX + 2 * STRIDE + 5;
        let mut buf = vec![0u8; len];
        let seed = 0xDEAD_BEEF_CAFE_F00D;
        // SAFETY: `buf` has `len` bytes.
        unsafe { fill(buf.as_mut_ptr(), len, seed) };
        // SAFETY: as above.
        let r = unsafe { verify(buf.as_ptr(), len, len, Some(seed)) };
        assert_eq!(r, Ok(()));
        assert!(buf[..DENSE_PREFIX].iter().any(|&b| b != 0));
        for (offset, sampled) in [
            (0, true),
            (DENSE_PREFIX - 1, true),
            (DENSE_PREFIX + 3, true),
            (DENSE_PREFIX + STRIDE - 1, false),
            (len - 1, true),
        ] {
            let mut copy = buf.clone();
            copy[offset] ^= 0x5A;
            // SAFETY: `copy` has `len` bytes.
            let r = unsafe { verify(copy.as_ptr(), len, len, Some(seed)) };
            if sampled {
                let m = r.expect_err("change at a sampled offset must be caught");
                assert_eq!(m.offset, offset);
                assert_eq!(m.expected, buf[offset]);
                assert_eq!(m.found, copy[offset]);
            } else {
                assert_eq!(r, Ok(()), "offset {offset} is deliberately unsampled");
            }
        }
        // A different seed or a shifted copy never matches.
        // SAFETY: as above.
        assert!(unsafe { verify(buf.as_ptr(), len, len, Some(seed + 1)) }.is_err());
        let mut shifted = vec![0u8; len];
        shifted[8..].copy_from_slice(&buf[..len - 8]);
        // SAFETY: `shifted` has `len` bytes.
        assert!(unsafe { verify(shifted.as_ptr(), len, len, Some(seed)) }.is_err());
    }

    #[test]
    fn prefix_of_a_resized_block_verifies_against_the_old_pattern() {
        let old_len = DENSE_PREFIX + 5 * STRIDE + 100;
        let mut buf = vec![0xFFu8; old_len];
        // SAFETY: `buf` has `old_len` bytes.
        unsafe { fill(buf.as_mut_ptr(), old_len, 7) };
        for keep in [
            1,
            8,
            9,
            1000,
            DENSE_PREFIX,
            DENSE_PREFIX + STRIDE + 8,
            old_len - 1,
            old_len,
        ] {
            let mut shrunk = buf[..keep].to_vec();
            // SAFETY: `shrunk` has `keep` bytes.
            let r = unsafe { verify(shrunk.as_ptr(), old_len, keep, Some(7)) };
            assert_eq!(r, Ok(()));
            shrunk[keep - 1] ^= 1;
            // The last kept byte is checked when it lies in the dense prefix, in a stride word,
            // or in the tail word (only when the whole block is kept).
            let in_stride_word = keep > DENSE_PREFIX && (keep - 1 - DENSE_PREFIX) % STRIDE < 8;
            let caught = keep <= DENSE_PREFIX || in_stride_word || keep == old_len;
            // SAFETY: as above.
            let r = unsafe { verify(shrunk.as_ptr(), old_len, keep, Some(7)) };
            assert_eq!(r.is_err(), caught, "keep {keep}");
        }
    }

    #[test]
    fn zero_check_reads_the_same_positions() {
        let len = DENSE_PREFIX + STRIDE + 8;
        let mut buf = vec![0u8; len];
        // SAFETY: `buf` has `len` bytes.
        assert_eq!(unsafe { verify(buf.as_ptr(), len, len, None) }, Ok(()));
        buf[DENSE_PREFIX + STRIDE + 2] = 1;
        // SAFETY: as above.
        let m = unsafe { verify(buf.as_ptr(), len, len, None) }.unwrap_err();
        assert_eq!(
            (m.offset, m.expected, m.found),
            (DENSE_PREFIX + STRIDE + 2, 0, 1)
        );
    }

    #[test]
    fn every_profile_runs_clean_against_system() {
        for profile in Profile::all() {
            let stats = check(&mut System, 0x5EED, 1500, profile);
            assert!(stats.ops == 1500, "{}: {stats:?}", profile.name);
            assert!(stats.allocs + stats.zeroed_allocs > 0);
            assert!(stats.deallocs >= stats.allocs + stats.zeroed_allocs);
            assert_eq!(stats.peak_footprint_bytes, None);
        }
    }

    #[test]
    fn same_seed_same_stats_different_seed_different_stats() {
        let a = check(&mut System, 1, 3000, Profile::MIXED);
        let b = check(&mut System, 1, 3000, Profile::MIXED);
        let c = check(&mut System, 2, 3000, Profile::MIXED);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn batch_profiles_free_everything_between_batches() {
        for profile in [Profile::LIFO_BATCHES, Profile::FIFO_BATCHES] {
            let stats = check(&mut System, 9, 5000, profile);
            assert!(stats.peak_live_blocks <= profile.batch, "{stats:?}");
            assert!(stats.deallocs > 0);
        }
    }

    #[test]
    fn a_dry_entropy_source_ends_the_run_cleanly() {
        let bytes: Vec<u8> = (0..300u32)
            .flat_map(|i| (i * 0x9E37).to_le_bytes())
            .collect();
        let mut source = ByteSource::new(&bytes);
        let stats = run_with(&mut System, &mut source, usize::MAX, Profile::MIXED).unwrap();
        assert!(stats.ops > 0 && stats.ops < 300);
        assert_eq!(source.remaining() / 4, 0, "the source was consumed");
    }

    #[test]
    fn failure_report_names_seed_profile_and_operation() {
        let f = Failure {
            kind: FailureKind::Overlap,
            op_index: 42,
            profile: "mixed",
            seed: Some(7),
            detail: String::from("x"),
        };
        let s = format!("{f}");
        assert!(
            s.contains("Overlap at op 42 of profile mixed (seed 0x7): x"),
            "{s}"
        );
        assert!(s.contains("Profile::MIXED"), "{s}");
    }
}
