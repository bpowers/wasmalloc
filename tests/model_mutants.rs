//! Deliberately broken allocators, to show that the model catches each kind of bug, and how
//! quickly. Every mutant wraps `System` and misbehaves in exactly one way; the tests assert the
//! failure kind and a bound on the number of operations before detection.

use std::alloc::{Layout, System};
use std::collections::HashMap;
use std::ptr::{self, NonNull};

use wasmalloc::testing::model::{self, FailureKind, Profile, RawAlloc};

/// The mutant's fallback: `System` through the same trait, so the only difference between a
/// mutant and a correct allocator is the one line that misbehaves.
struct Sys;

impl Sys {
    unsafe fn alloc(layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: same contract as the caller's.
        unsafe { RawAlloc::alloc(&mut System, layout) }
    }

    unsafe fn alloc_zeroed(layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: same contract as the caller's.
        unsafe { RawAlloc::alloc_zeroed(&mut System, layout) }
    }

    unsafe fn dealloc(ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { RawAlloc::dealloc(&mut System, ptr, layout) }
    }

    unsafe fn realloc(ptr: NonNull<u8>, layout: Layout, new_size: usize) -> Option<NonNull<u8>> {
        // SAFETY: same contract as the caller's.
        unsafe { RawAlloc::realloc(&mut System, ptr, layout, new_size) }
    }
}

/// `realloc` as allocate, copy `keep` bytes, free: the mutants that do not touch realloc use it
/// with the full `min(old, new)`, so their realloc inherits their alloc's misbehaviour.
unsafe fn realloc_by_copy<A: RawAlloc>(
    a: &mut A,
    ptr: NonNull<u8>,
    layout: Layout,
    new_size: usize,
    keep: usize,
) -> Option<NonNull<u8>> {
    let new_layout = Layout::from_size_align(new_size, layout.align()).unwrap();
    // SAFETY: `new_size` is non-zero (caller contract).
    let new = unsafe { a.alloc(new_layout)? };
    // SAFETY: `keep <= min(old, new)` bytes are valid in both blocks, which are distinct.
    unsafe {
        ptr::copy_nonoverlapping(ptr.as_ptr(), new.as_ptr(), keep);
        a.dealloc(ptr, layout);
    }
    Some(new)
}

// ----------------------------------------------------------------------------------------------

/// Off by eight for every alignment above eight.
struct Misaligned;

impl Misaligned {
    fn padded(layout: Layout) -> Layout {
        Layout::from_size_align(layout.size() + 8, layout.align()).unwrap()
    }
}

impl RawAlloc for Misaligned {
    unsafe fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: non-zero size (caller contract).
        let p = unsafe { Sys::alloc(Self::padded(layout))? };
        if layout.align() <= 8 {
            return Some(p);
        }
        // SAFETY: the padded block is eight bytes longer than the request.
        Some(unsafe { p.add(8) })
    }

    unsafe fn alloc_zeroed(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: non-zero size (caller contract).
        let p = unsafe { Sys::alloc_zeroed(Self::padded(layout))? };
        if layout.align() <= 8 {
            return Some(p);
        }
        // SAFETY: the padded block is eight bytes longer than the request.
        Some(unsafe { p.add(8) })
    }

    unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: `ptr` is eight bytes into the padded block when the alignment is above eight
        // (see `alloc`), and the padded block otherwise.
        unsafe {
            let base = if layout.align() <= 8 { ptr } else { ptr.sub(8) };
            Sys::dealloc(base, Self::padded(layout));
        }
    }

    unsafe fn realloc(
        &mut self,
        ptr: NonNull<u8>,
        layout: Layout,
        new_size: usize,
    ) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        unsafe { realloc_by_copy(self, ptr, layout, new_size, layout.size().min(new_size)) }
    }

    fn footprint_bytes(&self) -> Option<usize> {
        None
    }
}

// ----------------------------------------------------------------------------------------------

/// Every `PERIOD`th allocation returns a block that is still live, if one is big enough and
/// suitably aligned. Blocks are reference counted so the mutant itself never double-frees.
struct DoubleHandOut {
    count: usize,
    /// Live blocks, oldest first.
    live: Vec<NonNull<u8>>,
    /// Address to (layout it was really allocated with, times handed out and not yet freed).
    refs: HashMap<usize, (Layout, usize)>,
}

impl DoubleHandOut {
    const PERIOD: usize = 25;

    fn new() -> Self {
        DoubleHandOut {
            count: 0,
            live: Vec::new(),
            refs: HashMap::new(),
        }
    }

    fn duplicate(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        self.count += 1;
        if self.count % Self::PERIOD != 0 {
            return None;
        }
        let p = *self.live.iter().rev().find(|p| {
            let addr = p.addr().get();
            let (real, _) = self.refs[&addr];
            real.size() >= layout.size() && addr % layout.align() == 0
        })?;
        self.refs.get_mut(&p.addr().get()).unwrap().1 += 1;
        Some(p)
    }

    fn record(&mut self, p: NonNull<u8>, layout: Layout) {
        self.live.push(p);
        self.refs.insert(p.addr().get(), (layout, 1));
    }
}

impl RawAlloc for DoubleHandOut {
    unsafe fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        if let Some(dup) = self.duplicate(layout) {
            return Some(dup);
        }
        // SAFETY: same contract as this method.
        let p = unsafe { Sys::alloc(layout)? };
        self.record(p, layout);
        Some(p)
    }

    unsafe fn alloc_zeroed(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        if let Some(dup) = self.duplicate(layout) {
            return Some(dup);
        }
        // SAFETY: same contract as this method.
        let p = unsafe { Sys::alloc_zeroed(layout)? };
        self.record(p, layout);
        Some(p)
    }

    unsafe fn dealloc(&mut self, ptr: NonNull<u8>, _layout: Layout) {
        let addr = ptr.addr().get();
        let (real, refs) = self
            .refs
            .get_mut(&addr)
            .expect("freeing a block we handed out");
        *refs -= 1;
        if *refs == 0 {
            let real = *real;
            self.refs.remove(&addr);
            self.live.retain(|p| p.addr().get() != addr);
            // SAFETY: the last reference to a block we allocated from System with `real`.
            unsafe { Sys::dealloc(ptr, real) };
        }
    }

    unsafe fn realloc(
        &mut self,
        ptr: NonNull<u8>,
        layout: Layout,
        new_size: usize,
    ) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        unsafe { realloc_by_copy(self, ptr, layout, new_size, layout.size().min(new_size)) }
    }

    fn footprint_bytes(&self) -> Option<usize> {
        None
    }
}

// ----------------------------------------------------------------------------------------------

/// `realloc` copies one byte too few.
struct ReallocDropsLastByte;

impl RawAlloc for ReallocDropsLastByte {
    unsafe fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        unsafe { Sys::alloc(layout) }
    }

    unsafe fn alloc_zeroed(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        unsafe { Sys::alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: same contract as this method.
        unsafe { Sys::dealloc(ptr, layout) }
    }

    unsafe fn realloc(
        &mut self,
        ptr: NonNull<u8>,
        layout: Layout,
        new_size: usize,
    ) -> Option<NonNull<u8>> {
        let keep = layout.size().min(new_size) - 1;
        // SAFETY: same contract as this method; `keep` is below `min(old, new)`.
        unsafe { realloc_by_copy(self, ptr, layout, new_size, keep) }
    }

    fn footprint_bytes(&self) -> Option<usize> {
        None
    }
}

// ----------------------------------------------------------------------------------------------

/// `alloc_zeroed` is plain `alloc`.
struct SkipsZeroing;

impl RawAlloc for SkipsZeroing {
    unsafe fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        unsafe { Sys::alloc(layout) }
    }

    unsafe fn alloc_zeroed(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        unsafe { Sys::alloc(layout) }
    }

    unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: same contract as this method.
        unsafe { Sys::dealloc(ptr, layout) }
    }

    unsafe fn realloc(
        &mut self,
        ptr: NonNull<u8>,
        layout: Layout,
        new_size: usize,
    ) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        unsafe { Sys::realloc(ptr, layout, new_size) }
    }

    fn footprint_bytes(&self) -> Option<usize> {
        None
    }
}

// ----------------------------------------------------------------------------------------------

/// Every `PERIOD`th free flips the first byte of some other live block, the way a stray
/// free-list write into a live block would.
struct Scribbler {
    frees: usize,
    live: Vec<NonNull<u8>>,
}

impl Scribbler {
    const PERIOD: usize = 10;

    fn new() -> Self {
        Scribbler {
            frees: 0,
            live: Vec::new(),
        }
    }
}

impl RawAlloc for Scribbler {
    unsafe fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        let p = unsafe { Sys::alloc(layout)? };
        self.live.push(p);
        Some(p)
    }

    unsafe fn alloc_zeroed(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        let p = unsafe { Sys::alloc_zeroed(layout)? };
        self.live.push(p);
        Some(p)
    }

    unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        self.live.retain(|&p| p != ptr);
        self.frees += 1;
        if self.frees % Self::PERIOD == 0 {
            if let Some(victim) = self.live.last() {
                // SAFETY: `victim` is a live block of at least one byte.
                unsafe { victim.write(victim.read() ^ 0x80) };
            }
        }
        // SAFETY: same contract as this method.
        unsafe { Sys::dealloc(ptr, layout) }
    }

    unsafe fn realloc(
        &mut self,
        ptr: NonNull<u8>,
        layout: Layout,
        new_size: usize,
    ) -> Option<NonNull<u8>> {
        // SAFETY: same contract as this method.
        unsafe { realloc_by_copy(self, ptr, layout, new_size, layout.size().min(new_size)) }
    }

    fn footprint_bytes(&self) -> Option<usize> {
        None
    }
}

// ----------------------------------------------------------------------------------------------

/// Run `make()` against `profile` for several seeds; every run must fail with one of `kinds`
/// within `bound` operations. Returns the latest detection seen, for the record.
fn detect<A: RawAlloc>(
    make: impl Fn() -> A,
    profile: Profile,
    kinds: &[FailureKind],
    bound: usize,
) -> usize {
    let mut worst = 0;
    for seed in 1..=5 {
        let mut mutant = make();
        let f =
            model::run(&mut mutant, seed, bound, profile).expect_err("the mutant must be caught");
        assert!(kinds.contains(&f.kind), "{f}");
        assert!(f.op_index < bound, "{f}");
        assert_eq!(f.seed, Some(seed));
        worst = worst.max(f.op_index);
    }
    eprintln!(
        "{kinds:?} against {}: caught within {worst} ops",
        profile.name
    );
    worst
}

#[test]
fn misaligned_blocks_are_caught_on_the_first_aligned_request() {
    detect(
        || Misaligned,
        Profile::ALIGN_HEAVY,
        &[FailureKind::Misaligned],
        20,
    );
    detect(
        || Misaligned,
        Profile::MIXED,
        &[FailureKind::Misaligned],
        200,
    );
}

#[test]
fn a_block_handed_out_twice_is_caught_as_an_overlap() {
    detect(
        DoubleHandOut::new,
        Profile::SMALL_CHURN,
        &[FailureKind::Overlap],
        4 * DoubleHandOut::PERIOD,
    );
    detect(
        DoubleHandOut::new,
        Profile::MIXED,
        &[FailureKind::Overlap],
        200,
    );
}

#[test]
fn realloc_losing_its_last_byte_is_caught() {
    detect(
        || ReallocDropsLastByte,
        Profile::MIXED,
        &[FailureKind::ReallocLostBytes],
        200,
    );
    detect(
        || ReallocDropsLastByte,
        Profile::SMALL_CHURN,
        &[FailureKind::ReallocLostBytes],
        200,
    );
}

#[test]
fn alloc_zeroed_that_does_not_zero_is_caught_once_memory_is_recycled() {
    detect(
        || SkipsZeroing,
        Profile::SMALL_CHURN,
        &[FailureKind::NotZeroed],
        5000,
    );
}

#[test]
fn a_write_into_a_live_block_is_caught_when_the_block_is_next_checked() {
    // The scribble lands on the newest live block, which during a realloc is the block realloc
    // just allocated: the model then sees the damage as lost bytes rather than corruption.
    let kinds = [FailureKind::Corrupted, FailureKind::ReallocLostBytes];
    detect(Scribbler::new, Profile::SMALL_CHURN, &kinds, 500);
    detect(Scribbler::new, Profile::LIFO_BATCHES, &kinds, 500);
}
