//! Allocator selection. Exactly one of the allocator features may be enabled;
//! with none enabled the crate uses std's default global allocator (dlmalloc).

#[cfg(target_arch = "wasm32")]
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

#[cfg(not(target_arch = "wasm32"))]
pub const WASM_PAGE: usize = 65536;

/// Non-wasm hosts (used only so `cargo check` works natively) get a
/// stand-in that hands out fresh pages from the system allocator.
#[cfg(not(target_arch = "wasm32"))]
pub fn grow_pages(pages: usize) -> Option<usize> {
    use std::alloc::{alloc, Layout};
    let layout = Layout::from_size_align(pages * WASM_PAGE, WASM_PAGE).ok()?;
    let p = unsafe { alloc(layout) };
    if p.is_null() {
        None
    } else {
        Some(p as usize)
    }
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
    #[global_allocator]
    static GLOBAL: super::bump::Bump = super::bump::Bump::new();
    pub fn reset() {
        GLOBAL.reset();
    }
}

#[cfg(feature = "freelist")]
mod selected {
    pub const NAME: &str = "freelist";
    #[global_allocator]
    static GLOBAL: super::freelist::FreeList = super::freelist::FreeList::new();
    pub fn reset() {
        GLOBAL.reset();
    }
}

#[cfg(feature = "sizeclass")]
mod selected {
    pub const NAME: &str = "sizeclass";
    #[global_allocator]
    static GLOBAL: super::sizeclass::SizeClass = super::sizeclass::SizeClass::new();
    pub fn reset() {
        GLOBAL.reset();
    }
}

#[cfg(feature = "pages")]
mod selected {
    pub const NAME: &str = "pages";
    #[global_allocator]
    static GLOBAL: super::pages::Pages = super::pages::Pages::new();
    pub fn reset() {
        GLOBAL.reset();
    }
}

#[cfg(feature = "talc")]
mod selected {
    pub const NAME: &str = "talc";
    // The setup recommended by talc's WebAssembly README for single-threaded wasm.
    #[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
    #[global_allocator]
    static GLOBAL: talc::wasm::WasmDynamicTalc = talc::wasm::new_wasm_dynamic_allocator();
    pub fn reset() {}
}

#[cfg(feature = "lol_alloc")]
mod selected {
    pub const NAME: &str = "lol_alloc";
    // The setup recommended by lol_alloc's README for single-threaded wasm.
    #[cfg(target_arch = "wasm32")]
    #[global_allocator]
    static GLOBAL: lol_alloc::AssumeSingleThreaded<lol_alloc::FreeListAllocator> =
        unsafe { lol_alloc::AssumeSingleThreaded::new(lol_alloc::FreeListAllocator::new()) };
    pub fn reset() {}
}

#[cfg(not(any(
    feature = "bump",
    feature = "freelist",
    feature = "sizeclass",
    feature = "pages",
    feature = "talc",
    feature = "lol_alloc"
)))]
mod selected {
    pub const NAME: &str = "dlmalloc";
    pub fn reset() {}
}

pub use selected::{reset, NAME};
