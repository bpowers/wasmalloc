//! Allocation workloads. Every workload is parameterized by an iteration count,
//! goes through the ordinary `std::alloc` entry points (so the
//! `__rust_alloc` shim and `#[global_allocator]` dispatch are part of what is
//! measured, exactly as in a real program), writes at least one byte into
//! every allocation, and folds pointers and bytes into a checksum that is
//! returned to the caller. Mixing the address into the checksum (a ptrtoint
//! use) is what stops LLVM from eliding an alloc/dealloc pair: its allocation
//! site removal only fires when the pointer is used by free, comparisons and
//! stores into the block. `black_box` is deliberately not used on the hot
//! pointers because on wasm32 it spills through the shadow stack, which would
//! add a store and a load to every iteration of the floor.
//!
//! Every timed workload is `#[inline(always)]` so that the exported wrapper in
//! lib.rs *is* the hot loop. V8 tiers up and reports the compilation tier per
//! function: when the loop lives in a separate function the exported wrapper
//! stays in Liftoff forever (it runs a few dozen times) while the loop tiers
//! up, and the harness's tier query then describes the wrong function.
//!
//! Workload bookkeeping (the slot tables) lives in statics rather than on the
//! heap so that the harness can rewind the bump allocator between repetitions
//! without corrupting anything.

use core::alloc::Layout;
use core::hint::black_box;
use std::alloc::{alloc, dealloc, realloc};

use crate::rng::Rng;

#[cold]
#[inline(never)]
fn oom() -> ! {
    std::process::abort()
}

#[inline(always)]
fn mix(sum: u32, p: *mut u8) -> u32 {
    sum.rotate_left(5) ^ (p as usize as u32)
}

/// (a) alloc+free of one `size`-byte, `align`-aligned object per iteration;
/// cache-hot fast path. `align` must be a power of two.
#[inline(always)]
pub fn alloc_free_fixed(iters: usize, size: usize, align: usize) -> u32 {
    let layout = unsafe { Layout::from_size_align_unchecked(size, align) };
    let mut sum = 0u32;
    for i in 0..iters {
        let p = unsafe { alloc(layout) };
        if p.is_null() {
            oom();
        }
        unsafe {
            *p = i as u8;
            sum = mix(sum, p) ^ (*p as u32);
            dealloc(p, layout);
        }
    }
    sum
}

pub const BATCH: usize = 1000;
static mut BATCH_SLOTS: [*mut u8; BATCH] = [core::ptr::null_mut(); BATCH];

/// (b) allocate `BATCH` objects, then free them all; LIFO or FIFO order.
/// One "op" is one alloc+free pair, so callers divide by `rounds * BATCH`.
#[inline(always)]
pub fn batch_alloc_free(rounds: usize, size: usize, lifo: bool) -> u32 {
    let layout = unsafe { Layout::from_size_align_unchecked(size, 8) };
    let mut sum = 0u32;
    let slots = unsafe { &mut *core::ptr::addr_of_mut!(BATCH_SLOTS) };
    for r in 0..rounds {
        for (i, slot) in slots.iter_mut().enumerate() {
            let p = unsafe { alloc(layout) };
            if p.is_null() {
                oom();
            }
            unsafe { *p = (i + r) as u8 };
            *slot = p;
            sum = mix(sum, p);
        }
        if lifo {
            for slot in slots.iter().rev() {
                unsafe { dealloc(*slot, layout) };
            }
        } else {
            for slot in slots.iter() {
                unsafe { dealloc(*slot, layout) };
            }
        }
    }
    black_box(sum)
}

pub const CHURN_LIVE: usize = 10_000;
pub const CHURN_MIN: usize = 16;
pub const CHURN_MAX: usize = 1024;
const CHURN_ALIGN: usize = 8;
static mut CHURN_PTRS: [*mut u8; CHURN_LIVE] = [core::ptr::null_mut(); CHURN_LIVE];
static mut CHURN_SIZES: [u32; CHURN_LIVE] = [0; CHURN_LIVE];
static mut CHURN_RNG: Rng = Rng::new(0x5eed_1234_abcd_0001);

/// Sizes are uniform over the multiples of 8 in 16..=1024, i.e. the layouts an
/// align-8 Rust struct of that size range would request.
#[inline(always)]
fn churn_size(rng: &mut Rng) -> usize {
    CHURN_MIN + 8 * rng.below(((CHURN_MAX - CHURN_MIN) / 8 + 1) as u32) as usize
}

/// (c) setup: populate the live set. Not timed.
pub fn churn_init() -> u32 {
    let mut sum = 0u32;
    unsafe {
        let rng = &mut *core::ptr::addr_of_mut!(CHURN_RNG);
        *rng = Rng::new(0x5eed_1234_abcd_0001);
        let ptrs = &mut *core::ptr::addr_of_mut!(CHURN_PTRS);
        let sizes = &mut *core::ptr::addr_of_mut!(CHURN_SIZES);
        for i in 0..CHURN_LIVE {
            let size = churn_size(rng);
            let p = alloc(Layout::from_size_align_unchecked(size, CHURN_ALIGN));
            if p.is_null() {
                oom();
            }
            *p = i as u8;
            ptrs[i] = p;
            sizes[i] = size as u32;
            sum = mix(sum, p);
        }
    }
    sum
}

/// (c) each step frees one random live object and allocates a replacement of a
/// fresh random size. Timed.
#[inline(always)]
pub fn churn(steps: usize) -> u32 {
    let mut sum = 0u32;
    unsafe {
        let rng = &mut *core::ptr::addr_of_mut!(CHURN_RNG);
        let ptrs = &mut *core::ptr::addr_of_mut!(CHURN_PTRS);
        let sizes = &mut *core::ptr::addr_of_mut!(CHURN_SIZES);
        for step in 0..steps {
            let idx = rng.below(CHURN_LIVE as u32) as usize;
            let old_size = *sizes.get_unchecked(idx) as usize;
            dealloc(
                *ptrs.get_unchecked(idx),
                Layout::from_size_align_unchecked(old_size, CHURN_ALIGN),
            );
            let size = churn_size(rng);
            let p = alloc(Layout::from_size_align_unchecked(size, CHURN_ALIGN));
            if p.is_null() {
                oom();
            }
            *p = step as u8;
            *ptrs.get_unchecked_mut(idx) = p;
            *sizes.get_unchecked_mut(idx) = size as u32;
            sum = mix(sum, p);
        }
    }
    black_box(sum)
}

/// (c) teardown: release the live set. Not timed.
pub fn churn_fini() -> u32 {
    let mut sum = 0u32;
    unsafe {
        let ptrs = &mut *core::ptr::addr_of_mut!(CHURN_PTRS);
        let sizes = &mut *core::ptr::addr_of_mut!(CHURN_SIZES);
        for i in 0..CHURN_LIVE {
            let p = ptrs[i];
            sum ^= *p as u32;
            dealloc(
                p,
                Layout::from_size_align_unchecked(sizes[i] as usize, CHURN_ALIGN),
            );
            ptrs[i] = core::ptr::null_mut();
        }
    }
    sum
}

/// Largest request in the random-actions workload (talc's wasm-perf uses the
/// same bound).
pub const RA_MAX_SIZE: usize = 10_000;
/// Below this many live objects every action is an allocation, so the live set
/// never collapses to nothing (talc's `TARGET_MIN_ACTIVE_ALLOCATIONS`).
pub const RA_FLOOR: usize = 100;
/// Slots in the static live table. The live count is a random walk from the
/// floor with steps of +1 (3/7), -1 (3/7) and 0, so over 100k actions it stays
/// within a few hundred of the floor; reaching the capacity would force frees
/// and is counted in `RA_FORCED`.
const RA_CAP: usize = 16384;
const RA_SEED: u64 = 0x7a1c_0000_5eed_0002;
static mut RA_PTRS: [*mut u8; RA_CAP] = [core::ptr::null_mut(); RA_CAP];
static mut RA_SIZES: [u32; RA_CAP] = [0; RA_CAP];
static mut RA_ALIGN_SHIFTS: [u8; RA_CAP] = [0; RA_CAP];
static mut RA_LEN: usize = 0;
static mut RA_FORCED: u32 = 0;

/// talc's `generate_size` shape: draw an upper bound in 1..=RA_MAX_SIZE, then
/// a size in 1..=bound, so small requests dominate the way they do in real
/// programs while the tail still reaches 10 KB.
#[inline(always)]
fn ra_size(rng: &mut Rng) -> usize {
    let bound = 1 + rng.below(RA_MAX_SIZE as u32);
    1 + rng.below(bound) as usize
}

/// talc's `generate_align`: `8 << tz(u16) / 2`, which is 8 three quarters of
/// the time, then 16 (19%), 32 (4%) and 64 (1%); the tail beyond 64 that talc
/// lets run on (a 1 in 2^16 chance of 2 KiB) is cut off here.
#[inline(always)]
fn ra_align_shift(rng: &mut Rng) -> u8 {
    let tz = (rng.next_u32() as u16).trailing_zeros() / 2;
    3 + tz.min(3) as u8
}

#[inline(always)]
fn random_actions_impl(actions: usize, realloc_on: bool) -> u32 {
    let mut sum = 0u32;
    // Realloc is one choice in seven; without it the remaining six split evenly
    // between allocation and deallocation, as in talc's `--no-realloc` runs.
    let choices: u32 = if realloc_on { 7 } else { 6 };
    unsafe {
        let ptrs = &mut *core::ptr::addr_of_mut!(RA_PTRS);
        let sizes = &mut *core::ptr::addr_of_mut!(RA_SIZES);
        let shifts = &mut *core::ptr::addr_of_mut!(RA_ALIGN_SHIFTS);
        // Every call starts from the empty live set left by the previous fini,
        // so every call performs the identical action sequence.
        let mut len = 0usize;
        let mut forced = 0u32;
        let mut rng = Rng::new(RA_SEED);
        for step in 0..actions {
            let action = if len < RA_FLOOR {
                0
            } else if len >= RA_CAP {
                forced += 1;
                3
            } else {
                rng.below(choices)
            };
            if action < 3 {
                let size = ra_size(&mut rng);
                let shift = ra_align_shift(&mut rng);
                let p = alloc(Layout::from_size_align_unchecked(size, 1 << shift));
                if p.is_null() {
                    oom();
                }
                *p = step as u8;
                *ptrs.get_unchecked_mut(len) = p;
                *sizes.get_unchecked_mut(len) = size as u32;
                *shifts.get_unchecked_mut(len) = shift;
                len += 1;
                sum = mix(sum, p);
            } else if action < 6 {
                let i = rng.below(len as u32) as usize;
                let p = *ptrs.get_unchecked(i);
                let layout = Layout::from_size_align_unchecked(
                    *sizes.get_unchecked(i) as usize,
                    1 << *shifts.get_unchecked(i),
                );
                sum ^= *p as u32;
                dealloc(p, layout);
                len -= 1;
                *ptrs.get_unchecked_mut(i) = *ptrs.get_unchecked(len);
                *sizes.get_unchecked_mut(i) = *sizes.get_unchecked(len);
                *shifts.get_unchecked_mut(i) = *shifts.get_unchecked(len);
            } else {
                let i = rng.below(len as u32) as usize;
                let new_size = ra_size(&mut rng);
                let old = Layout::from_size_align_unchecked(
                    *sizes.get_unchecked(i) as usize,
                    1 << *shifts.get_unchecked(i),
                );
                let p = realloc(*ptrs.get_unchecked(i), old, new_size);
                if p.is_null() {
                    oom();
                }
                *p.add(new_size - 1) = step as u8;
                *ptrs.get_unchecked_mut(i) = p;
                *sizes.get_unchecked_mut(i) = new_size as u32;
                sum = mix(sum, p);
            }
        }
        RA_LEN = len;
        RA_FORCED += forced;
    }
    black_box(sum)
}

/// (f) talc-style random actions: from an empty live set, each action is an
/// allocation (3/7) of a size in 1..=10000 biased small with alignment mostly
/// 8, a free of a random live object (3/7), or a realloc of a random live
/// object to a fresh random size (1/7); below 100 live objects every action
/// allocates. This is the shape the published wasm allocator comparisons use.
/// Timed; `random_actions_fini` releases what is left.
#[inline(always)]
pub fn random_actions(actions: usize) -> u32 {
    random_actions_impl(actions, true)
}

/// (f') the same loop without the realloc choice (1/2 alloc, 1/2 free), to
/// separate the cost of realloc from the cost of alloc and free.
#[inline(always)]
pub fn random_actions_norealloc(actions: usize) -> u32 {
    random_actions_impl(actions, false)
}

/// (f) teardown: free the live set. Not timed.
pub fn random_actions_fini() -> u32 {
    let mut sum = 0u32;
    unsafe {
        let ptrs = &mut *core::ptr::addr_of_mut!(RA_PTRS);
        let sizes = &mut *core::ptr::addr_of_mut!(RA_SIZES);
        let shifts = &mut *core::ptr::addr_of_mut!(RA_ALIGN_SHIFTS);
        for i in 0..RA_LEN {
            let p = ptrs[i];
            sum ^= *p as u32;
            dealloc(
                p,
                Layout::from_size_align_unchecked(sizes[i] as usize, 1 << shifts[i]),
            );
            ptrs[i] = core::ptr::null_mut();
        }
        RA_LEN = 0;
        sum ^= RA_FORCED.rotate_left(16);
    }
    sum
}

pub const VEC_TARGET: usize = 1 << 20;

/// (d) grow a Vec<u8> from empty to 1 MiB one push at a time. This is the
/// realloc path as a real program exercises it, but note that the per-push
/// capacity check and store dominate; `realloc_doubling` isolates realloc.
#[inline(always)]
pub fn vec_push_growth(rounds: usize) -> u32 {
    let mut sum = 0u32;
    for r in 0..rounds {
        let mut v: Vec<u8> = Vec::new();
        for i in 0..VEC_TARGET {
            v.push((i ^ r) as u8);
        }
        let v = black_box(v);
        sum = mix(sum, v.as_ptr() as *mut u8) ^ v[VEC_TARGET - 1] as u32 ^ v.len() as u32;
        drop(v);
    }
    sum
}

pub const REALLOC_START: usize = 16;

/// (d') realloc a block by doubling from 16 bytes to 1 MiB, writing the last
/// byte after each step. One round is 16 reallocs.
#[inline(always)]
pub fn realloc_doubling(rounds: usize) -> u32 {
    let mut sum = 0u32;
    for r in 0..rounds {
        let mut layout = unsafe { Layout::from_size_align_unchecked(REALLOC_START, 8) };
        let mut p = unsafe { alloc(layout) };
        if p.is_null() {
            oom();
        }
        unsafe { *p = r as u8 };
        while layout.size() < VEC_TARGET {
            let new_size = layout.size() * 2;
            p = unsafe { realloc(p, layout, new_size) };
            if p.is_null() {
                oom();
            }
            unsafe { *p.add(new_size - 1) = r as u8 };
            layout = unsafe { Layout::from_size_align_unchecked(new_size, 8) };
            sum = mix(sum, p);
        }
        unsafe { dealloc(p, layout) };
    }
    sum
}

pub const LARGE_SIZES: [usize; 5] = [256 << 10, 512 << 10, 1 << 20, 2 << 20, 4 << 20];
const TOUCH_STRIDE: usize = 4096;

/// (e) alloc, touch one byte per 4 KiB, free; sizes cycle through 256 KiB..4 MiB.
#[inline(always)]
pub fn large_alloc_free(iters: usize) -> u32 {
    let mut sum = 0u32;
    for i in 0..iters {
        let size = LARGE_SIZES[i % LARGE_SIZES.len()];
        let layout = unsafe { Layout::from_size_align_unchecked(size, 16) };
        let p = unsafe { alloc(layout) };
        if p.is_null() {
            oom();
        }
        let mut off = 0;
        while off < size {
            unsafe { *p.add(off) = i as u8 };
            off += TOUCH_STRIDE;
        }
        sum = mix(sum, p) ^ unsafe { *p.add(size - TOUCH_STRIDE) } as u32;
        unsafe { dealloc(p, layout) };
    }
    sum
}

pub const GROW_PAGES: usize = 16;

/// (e'') memory.grow by 1 MiB without touching the new pages, to separate the
/// engine's grow cost from first-touch page faults.
#[inline(always)]
pub fn memory_grow_only(iters: usize) -> u32 {
    let mut sum = 0u32;
    for _ in 0..iters {
        let base = match crate::alloc::grow_pages(GROW_PAGES) {
            Some(b) => b as *mut u8,
            None => oom(),
        };
        sum = mix(sum, base);
    }
    sum
}

/// (e') the engine's memory.grow path in isolation: grow by 1 MiB and touch
/// one byte per 4 KiB of the new region. Memory is never returned, so callers
/// keep the total iteration count modest.
#[inline(always)]
pub fn memory_grow_touch(iters: usize) -> u32 {
    let mut sum = 0u32;
    for i in 0..iters {
        let base = match crate::alloc::grow_pages(GROW_PAGES) {
            Some(b) => b as *mut u8,
            None => oom(),
        };
        let bytes = GROW_PAGES * crate::alloc::WASM_PAGE;
        let mut off = 0;
        while off < bytes {
            unsafe { *base.add(off) = i as u8 };
            off += TOUCH_STRIDE;
        }
        sum = mix(sum, base);
    }
    sum
}
