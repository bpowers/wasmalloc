//! Allocator selection. Exactly one of the allocator features may be enabled;
//! with none enabled the crate uses std's default global allocator.

pub const WASM_PAGE: usize = 65536;

/// Grow linear memory by `pages` and return the base address of the new region,
/// or `None` if the engine refused.
#[cfg(target_arch = "wasm32")]
#[inline(always)]
pub fn grow_pages(pages: usize) -> Option<usize> {
    let prev = core::arch::wasm32::memory_grow(0, pages);
    if prev == usize::MAX {
        None
    } else {
        Some(prev * WASM_PAGE)
    }
}

/// Non-wasm hosts (native sanity runs of the same driver) get a stand-in for
/// linear memory: one large lazily-committed region from the system allocator,
/// handed out sequentially, so that like wasm's `memory.grow` successive grows
/// are contiguous and pages touched once stay resident and get reused after a
/// `reset`. Anything beyond the reserve comes from a fresh region. This goes to
/// `System` directly rather than through `std::alloc::alloc`, because the
/// `#[global_allocator]` on this target may be one of the floors, whose refill
/// path is exactly what calls us.
#[cfg(not(target_arch = "wasm32"))]
pub fn grow_pages(pages: usize) -> Option<usize> {
    use core::cell::Cell;
    use std::alloc::{GlobalAlloc, Layout, System};

    const RESERVE: usize = 2 << 30;
    thread_local! {
        static NEXT: Cell<usize> = const { Cell::new(0) };
        static END: Cell<usize> = const { Cell::new(0) };
    }

    let bytes = pages * WASM_PAGE;
    let region = |size: usize| -> Option<usize> {
        let layout = Layout::from_size_align(size, WASM_PAGE).ok()?;
        let p = unsafe { System.alloc(layout) };
        if p.is_null() {
            None
        } else {
            Some(p as usize)
        }
    };
    let next = NEXT.with(Cell::get);
    let end = END.with(Cell::get);
    if next == 0 || next + bytes > end {
        if bytes > RESERVE {
            return region(bytes);
        }
        let base = region(RESERVE)?;
        NEXT.with(|c| c.set(base + bytes));
        END.with(|c| c.set(base + RESERVE));
        return Some(base);
    }
    NEXT.with(|c| c.set(next + bytes));
    Some(next)
}

macro_rules! count_enabled {
    ($($f:literal),*) => { 0usize $(+ cfg!(feature = $f) as usize)* };
}

const ENABLED: usize = count_enabled!(
    "bump", "freelist", "sizeclass", "pages", "dlmalloc", "talc", "lol_alloc"
);

#[allow(dead_code)]
const _: () = assert!(
    ENABLED <= 1,
    "enable at most one allocator feature (bump, freelist, sizeclass, pages, dlmalloc, talc, lol_alloc)"
);

// The floor allocators are always compiled so that they can share code
// (pages reuses sizeclass helpers); only `selected` depends on features.
pub mod bump;
pub mod freelist;
pub mod pages;
pub mod sizeclass;

#[cfg(feature = "bump")]
mod selected {
    pub const NAME: &str = "bump";
    pub const DETAIL: &str = "bump pointer over memory.grow, free is a no-op";
    #[global_allocator]
    static GLOBAL: super::bump::Bump = super::bump::Bump::new();
    pub fn reset() {
        GLOBAL.reset();
    }
}

#[cfg(feature = "freelist")]
mod selected {
    pub const NAME: &str = "freelist";
    pub const DETAIL: &str = "one LIFO free list for <=32 B blocks, bump otherwise";
    #[global_allocator]
    static GLOBAL: super::freelist::FreeList = super::freelist::FreeList::new();
    pub fn reset() {
        GLOBAL.reset();
    }
}

#[cfg(feature = "sizeclass")]
mod selected {
    pub const NAME: &str = "sizeclass";
    pub const DETAIL: &str = "64 size classes of 16 B up to 1024 B, class from Layout on free";
    #[global_allocator]
    static GLOBAL: super::sizeclass::SizeClass = super::sizeclass::SizeClass::new();
    pub fn reset() {
        GLOBAL.reset();
    }
}

#[cfg(feature = "pages")]
mod selected {
    pub const NAME: &str = "pages";
    pub const DETAIL: &str = "sizeclass with the class read from a 64 KiB page header on free";
    #[global_allocator]
    static GLOBAL: super::pages::Pages = super::pages::Pages::new();
    pub fn reset() {
        GLOBAL.reset();
    }
}

#[cfg(feature = "talc")]
mod selected {
    pub const NAME: &str = "talc";
    pub const DETAIL: &str = "talc 5.1 WasmDynamicTalc (talc::wasm::new_wasm_dynamic_allocator)";
    // The setup recommended by talc's WebAssembly README for single-threaded wasm.
    #[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
    #[global_allocator]
    static GLOBAL: talc::wasm::WasmDynamicTalc = talc::wasm::new_wasm_dynamic_allocator();
    #[cfg(not(all(target_family = "wasm", not(target_feature = "atomics"))))]
    compile_error!("the talc variant only exists for single-threaded wasm targets");
    pub fn reset() {}
}

#[cfg(feature = "lol_alloc")]
mod selected {
    pub const NAME: &str = "lol_alloc";
    pub const DETAIL: &str = "lol_alloc 0.4 AssumeSingleThreaded<FreeListAllocator>";
    // The setup recommended by lol_alloc's README for single-threaded wasm.
    #[cfg(target_arch = "wasm32")]
    #[global_allocator]
    static GLOBAL: lol_alloc::AssumeSingleThreaded<lol_alloc::FreeListAllocator> =
        unsafe { lol_alloc::AssumeSingleThreaded::new(lol_alloc::FreeListAllocator::new()) };
    #[cfg(not(target_arch = "wasm32"))]
    compile_error!("the lol_alloc variant only exists for wasm32 targets");
    pub fn reset() {}
}

// No feature: std's default. Which allocator that is depends on the target, and
// the harness records it so the tables can say what was actually measured.
#[cfg(not(any(
    feature = "bump",
    feature = "freelist",
    feature = "sizeclass",
    feature = "pages",
    feature = "talc",
    feature = "lol_alloc"
)))]
mod selected {
    #[cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]
    pub const NAME: &str = "dlmalloc";
    #[cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]
    pub const DETAIL: &str = "std default on wasm32-unknown-unknown: dlmalloc-rs (Rust port of dlmalloc)";
    #[cfg(all(target_arch = "wasm32", target_os = "wasi"))]
    pub const NAME: &str = "dlmalloc";
    #[cfg(all(target_arch = "wasm32", target_os = "wasi"))]
    pub const DETAIL: &str = "std default on wasm32-wasip1: wasi-libc malloc (C dlmalloc via System)";
    #[cfg(not(target_arch = "wasm32"))]
    pub const NAME: &str = "system";
    #[cfg(not(target_arch = "wasm32"))]
    pub const DETAIL: &str = "std default on the host: System (libc malloc)";
    pub fn reset() {}
}

pub use selected::{reset, DETAIL, NAME};
