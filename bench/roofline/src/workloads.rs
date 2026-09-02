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

/// (a) alloc+free of one `size`-byte object per iteration; cache-hot fast path.
pub fn alloc_free_fixed(iters: usize, size: usize) -> u32 {
    let layout = unsafe { Layout::from_size_align_unchecked(size, 8) };
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
            dealloc(p, Layout::from_size_align_unchecked(sizes[i] as usize, CHURN_ALIGN));
            ptrs[i] = core::ptr::null_mut();
        }
    }
    sum
}

pub const VEC_TARGET: usize = 1 << 20;

/// (d) grow a Vec<u8> from empty to 1 MiB one push at a time. This is the
/// realloc path as a real program exercises it, but note that the per-push
/// capacity check and store dominate; `realloc_doubling` isolates realloc.
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
