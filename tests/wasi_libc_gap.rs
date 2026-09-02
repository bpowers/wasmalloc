//! Review finding R-1 (wasm32-wasip1 only): wasi-libc's dlmalloc and this heap both claim the
//! linker gap.
//!
//! `Heap::ensure_init` reclaims every whole slice between `__heap_base` and the initial end of
//! memory (`slices::initial_free_range`). wasi-libc's dlmalloc does the same: its
//! `try_init_allocator` (dlmalloc/src/malloc.c in wasi-libc, present in the `libc.a` of the
//! wasm32-wasip1 sysroot) makes `[__heap_base, __heap_end)` its first segment the first time
//! any libc `malloc` runs. On wasip1 that malloc is reachable from std: `__wasilibc_populate_preopens`,
//! `__wasilibc_initialize_environ` and `opendir` all call it, so `std::fs` and `std::env` do.
//!
//! With the default link the gap is the tail of one slice and this heap uses none of it, so the
//! two allocators stay disjoint by luck. Any build that widens the initial memory, for example
//! `RUSTFLAGS="-C link-arg=--initial-memory=8388608"`, puts whole slices into the gap; this heap
//! hands them out as pages and runs, dlmalloc hands the same bytes out as chunks, and the two
//! programs' data overlap. This test shows the overlap and the resulting corruption of a live
//! block whenever the gap holds a whole slice, and reports (without failing) when the link left
//! no whole slice to fight over.
//!
//! Reproduce the failure with
//! `RUSTFLAGS="-C link-arg=--initial-memory=8388608" cargo test --target wasm32-wasip1 --test wasi_libc_gap`.
#![cfg(all(target_arch = "wasm32", target_os = "wasi"))]

#[global_allocator]
static ALLOC: wasmalloc::WasmAlloc = wasmalloc::WasmAlloc::new();

unsafe extern "C" {
    fn malloc(size: usize) -> *mut u8;
    static __heap_base: u8;
    static __heap_end: u8;
}

const SLICE: usize = 64 * 1024;

fn memory_end() -> usize {
    core::arch::wasm32::memory_size(0) * SLICE
}

#[test]
fn wasi_libc_malloc_hands_out_slices_the_heap_already_claimed() {
    let heap_base = (&raw const __heap_base).addr();
    let heap_end = (&raw const __heap_end).addr();
    let first_whole = heap_base.next_multiple_of(SLICE);
    // A header-less run of one slice, served from the lowest free slice: inside the gap when
    // the gap has one, which is the case this test is about.
    let mut v = vec![0xABu8; SLICE];
    let v_start = v.as_ptr().addr();
    let v_end = v_start + v.len();
    // Everything from the first whole slice to here is the heap's: free in its slice map or
    // handed out as pages and runs.
    let owned_end = memory_end();
    eprintln!(
        "__heap_base={heap_base:#x} __heap_end={heap_end:#x} first whole slice={first_whole:#x} \
         memory end={owned_end:#x} run=[{v_start:#x}, {v_end:#x})"
    );
    if heap_end < first_whole + SLICE || v_end > heap_end {
        eprintln!(
            "the linker gap holds no whole slice that the run landed in; link with \
             --initial-memory=8388608 (or larger) to reproduce the overlap"
        );
        return;
    }
    // dlmalloc's first segment is [__heap_base, __heap_end); a request that fits it is carved
    // from its bottom, so a request reaching up to the run's end covers the run.
    let n = v_end - heap_base;
    // SAFETY: a plain C malloc call; the block is never freed (dlmalloc's free would write
    // into memory this heap also owns).
    let p = unsafe { malloc(n) }.addr();
    assert_ne!(p, 0, "wasi-libc malloc failed");
    let overlaps = p < owned_end && p + n > first_whole;
    eprintln!(
        "wasi-libc malloc({n:#x}) = [{p:#x}, {:#x}) overlaps heap memory: {overlaps}",
        p + n
    );
    if overlaps && p <= v_start && p + n >= v_end {
        // Writing inside dlmalloc's own block rewrites the heap's live run.
        // SAFETY: `[v_start, v_end)` lies inside `[p, p + n)`, the block malloc just returned,
        // so by malloc's contract these bytes are ours to write; that they are also `v`'s is
        // the bug.
        unsafe { core::ptr::write_bytes((p + (v_start - p)) as *mut u8, 0xCD, v.len()) };
        let intact = v.iter().all(|&b| b == 0xAB);
        eprintln!("live run intact after a write inside the libc block: {intact}");
        assert!(
            intact,
            "wasi-libc's block and a live wasmalloc run are the same bytes"
        );
    }
    assert!(
        !overlaps,
        "wasi-libc malloc returned memory inside the range this heap owns"
    );
    v[0] = 0;
}
