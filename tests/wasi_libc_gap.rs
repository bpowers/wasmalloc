//! Review finding R-1 (wasm32-wasip1 and wasip2): wasi-libc's dlmalloc claims the linker gap
//! `[__heap_base, __heap_end)` as its first segment the first time any libc `malloc` runs
//! (`try_init_allocator` in wasi-libc's dlmalloc/src/malloc.c, part of the sysroot's `libc.a`),
//! and std reaches libc `malloc` on wasi even when this crate is the global allocator:
//! `__wasilibc_populate_preopens`, `__wasilibc_initialize_environ` and `opendir` all call it,
//! so `std::fs` and `std::env` do. Until 2026-09-02 `Heap::ensure_init` reclaimed every whole
//! slice of the same gap, and the two allocators handed out the same bytes whenever the gap
//! held a whole slice.
//!
//! On wasi the heap therefore starts at the end of the linear memory that exists at its first
//! allocation (`WasmMemory::heap_base`), leaving the gap, and whatever dlmalloc has grown memory
//! for since, to dlmalloc: every slice this heap owns comes from its own `memory.grow` calls.
//!
//! With the default link the gap is the tail of one slice and the collision could not show, so
//! `build.rs` links every test binary for a wasi target with an 8 MiB initial memory
//! (`--initial-memory=8388608`), which puts about a hundred whole slices into the gap. The test
//! checks that nothing this heap hands out lies in the gap, and that a libc block spanning the
//! gap and the live blocks of this heap never alias each other.
#![cfg(all(target_arch = "wasm32", target_os = "wasi"))]

#[global_allocator]
static ALLOC: wasmalloc::WasmAlloc = wasmalloc::WasmAlloc::new();

unsafe extern "C" {
    fn malloc(size: usize) -> *mut u8;
    fn free(p: *mut u8);
    static __heap_base: u8;
    static __heap_end: u8;
}

const SLICE: usize = 64 * 1024;

/// Half-open address ranges `[start, end)`.
type Range = (usize, usize);

fn disjoint(a: Range, b: Range) -> bool {
    a.1 <= b.0 || b.1 <= a.0
}

fn range_of(bytes: &[u8]) -> Range {
    let start = bytes.as_ptr().addr();
    (start, start + bytes.len())
}

#[test]
fn the_heap_leaves_the_linker_gap_to_wasi_libc() {
    let gap: Range = (
        (&raw const __heap_base).addr(),
        (&raw const __heap_end).addr(),
    );
    let memory_end = core::arch::wasm32::memory_size(0) * SLICE;
    eprintln!(
        "linker gap [{:#x}, {:#x}), memory end {memory_end:#x}",
        gap.0, gap.1
    );
    assert!(
        gap.1 >= gap.0.next_multiple_of(SLICE) + SLICE,
        "the linker gap holds no whole slice, so a collision could not show; build.rs links \
         wasi test binaries with --initial-memory=8388608 so that it does"
    );

    // Header-less runs of one slice and small blocks from a page. Before the fix both came
    // from the lowest free slice, which was the first whole slice of the gap; several of each
    // so that the check does not depend on which slices the test harness's own allocations
    // took first.
    let mut runs: Vec<Vec<u8>> = (0..8u8).map(|i| vec![0xA0 | i; SLICE]).collect();
    let smalls: Vec<Box<[u8; 200]>> = (0..64u8).map(|i| Box::new([i; 200])).collect();
    let blocks: Vec<Range> = runs
        .iter()
        .map(|v| range_of(v))
        .chain(smalls.iter().map(|b| range_of(&b[..])))
        .collect();
    for &b in &blocks {
        assert!(
            disjoint(b, gap),
            "block [{:#x}, {:#x}) lies in the linker gap [{:#x}, {:#x}) that wasi-libc's \
             malloc claims as its first segment",
            b.0,
            b.1,
            gap.0,
            gap.1
        );
    }

    // dlmalloc carves its first blocks from the bottom of the gap, so a request for most of
    // it covers everything this heap used to take from there. When libc has already served
    // requests (std's preopen and environment setup) part of the block may be memory dlmalloc
    // grew instead, which is dlmalloc's own as well: in every case it must not touch a live
    // block of this heap.
    let n = (gap.1 - gap.0) / 4 * 3;
    // SAFETY: a plain C malloc; the block is freed below through the same libc.
    let p = unsafe { malloc(n) };
    assert!(!p.is_null(), "wasi-libc malloc({n:#x}) failed");
    let libc: Range = (p.addr(), p.addr() + n);
    eprintln!(
        "wasi-libc malloc({n:#x}) = [{:#x}, {:#x}), inside the gap: {}",
        libc.0,
        libc.1,
        libc.0 >= gap.0 && libc.1 <= gap.1
    );
    for &b in &blocks {
        assert!(
            disjoint(b, libc),
            "wasi-libc's block [{:#x}, {:#x}) overlaps this heap's live block [{:#x}, {:#x})",
            libc.0,
            libc.1,
            b.0,
            b.1
        );
    }

    // Writing through one allocator's block must leave the other's intact, in both directions;
    // this is the check that showed the corruption before the fix.
    // SAFETY: `[p, p + n)` is the block malloc just returned.
    unsafe { core::ptr::write_bytes(p, 0xCD, n) };
    for (i, v) in runs.iter().enumerate() {
        assert!(
            v.iter().all(|&b| b == 0xA0 | i as u8),
            "run {i} changed under a write inside the libc block"
        );
    }
    for (i, b) in smalls.iter().enumerate() {
        assert!(
            b.iter().all(|&x| x == i as u8),
            "small block {i} changed under a write inside the libc block"
        );
    }
    for v in runs.iter_mut() {
        v.fill(0x11);
    }
    // SAFETY: still the libc block, valid for `n` bytes of reads.
    let libc_bytes = unsafe { core::slice::from_raw_parts(p, n) };
    assert!(
        libc_bytes.iter().all(|&b| b == 0xCD),
        "the libc block changed under writes to this heap's runs"
    );
    // SAFETY: allocated by malloc above and not freed since.
    unsafe { free(p) };
    drop(runs);
    drop(smalls);
}
