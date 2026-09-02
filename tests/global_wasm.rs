//! End-to-end check on wasm32: install wasmalloc as the global allocator of this test binary and
//! drive std's collections through it. Runs under wasmtime via the wasm32-wasip1 runner.
#![cfg(target_arch = "wasm32")]

use std::collections::{BTreeMap, HashMap};

#[global_allocator]
static ALLOC: wasmalloc::WasmAlloc = wasmalloc::WasmAlloc::new();

fn memory_pages() -> usize {
    core::arch::wasm32::memory_size(0)
}

#[test]
fn collections_work_and_memory_is_reused() {
    let mut v: Vec<u64> = Vec::new();
    for i in 0..100_000u64 {
        v.push(i * 3);
    }
    assert_eq!(v.iter().sum::<u64>(), 3 * (99_999 * 100_000 / 2));

    let mut m: HashMap<u64, String> = HashMap::new();
    for i in 0..20_000u64 {
        m.insert(i, format!("value-{i}"));
    }
    assert_eq!(m.get(&777).map(String::as_str), Some("value-777"));
    m.retain(|k, _| k % 2 == 0);
    assert_eq!(m.len(), 10_000);

    let mut t: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for i in 0..5_000usize {
        t.insert(format!("{i:08}"), vec![i as u8; i % 300]);
    }
    assert_eq!(
        t.values().map(Vec::len).sum::<usize>(),
        (0..5_000).map(|i| i % 300).sum::<usize>()
    );

    drop(v);
    drop(m);
    drop(t);

    // Churn: repeated build-and-drop must not grow memory without bound.
    let before = memory_pages();
    for round in 0..50 {
        let mut s: Vec<String> = (0..2_000).map(|i| format!("{round}-{i}")).collect();
        s.sort();
        let mut big = vec![0u8; 3 << 20];
        big[round] = 1;
        assert_eq!(big.iter().map(|&b| b as usize).sum::<usize>(), 1);
    }
    let after = memory_pages();
    assert!(
        after <= before + 64,
        "memory grew from {before} to {after} pages during churn"
    );
}

#[test]
fn zeroed_and_aligned_allocations() {
    let z = vec![0u32; 1 << 20];
    assert!(z.iter().all(|&x| x == 0));

    #[repr(align(64))]
    struct Aligned([u8; 100]);
    let boxes: Vec<Box<Aligned>> = (0..1_000)
        .map(|i| Box::new(Aligned([i as u8; 100])))
        .collect();
    for (i, b) in boxes.iter().enumerate() {
        assert_eq!(b.as_ref() as *const Aligned as usize % 64, 0);
        assert!(b.0.iter().all(|&x| x == i as u8));
    }

    #[repr(align(8192))]
    struct PageAligned([u8; 16]);
    let p = Box::new(PageAligned([7; 16]));
    assert_eq!(p.as_ref() as *const PageAligned as usize % 8192, 0);
}
