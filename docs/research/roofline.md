# Allocation-cost roofline for single-threaded wasm32

Measured 2026-09-01 on the reference machine with the harness in `bench/roofline`
(commit e69117b). The allocator measured is the one at that commit; the tuning
commits that followed on main (da65f8e widening the page counters, e38d647
skipping the retire call, abef068 freeing aligned small blocks on the fast path,
9244a47 marking the fast paths `inline(always)`) change the fast-path figures, and
section 12.1 quotes their effect, but the full matrix has not been rerun for them. Every number below is the median of 7 timed calls
after warm-up, in nanoseconds per operation, with the cost of one empty JS-to-wasm
call subtracted; the full tables, including minimums, are regenerated into
`bench/roofline/results/REPORT.md` from the JSON in `bench/roofline/results/`.
`bench/roofline/README.md` has the exact reproduction commands.

Contents

1. Summary
2. Methodology
3. Machine and engines
4. Results per engine and tier
5. wasmalloc against the floor and the incumbents
6. Footprint
7. Module size
8. Shim and profile study
9. Tier-up study
10. V8 12.4 against V8 15.2
11. The roofline
12. Where wasmalloc loses and why
13. Suggestions for the tuning engineer

## 1. Summary

- Under the optimizing tier of current V8 (d8 15.2) a 32-byte alloc+free pair costs
  0.98 ns on the bump floor, 0.55 ns on the free-list floor, 2.93 ns in wasmalloc,
  4.11 ns in dlmalloc-rs (std's default), 8.92 ns in talc. On node 22 (V8 12.4) the
  same five are 1.22, 1.80, 3.61, 7.99 and 12.35 ns. JavaScriptCore and wasmtime
  agree with d8 to within 10 percent.
- wasmalloc is the fastest real allocator on every engine and tier for every
  small-object workload: 1.4x to 2.2x faster than dlmalloc-rs and 3x faster than
  talc on the fast path, 8.5x faster than dlmalloc and 4x faster than talc on
  random churn over 10,000 live objects, and 1.3x faster than talc and 2.6x faster
  than dlmalloc on the talc-style random-actions loop the field compares on.
- The gap to the floor is 2.0x on churn on every engine and 2.0x (node) to 5.3x (d8,
  JSC, wasmtime) on the cache-hot pair. Six harness allocators that reproduce
  wasmalloc's fast-path memory traffic one detail at a time split the 5.3x in two:
  the 16-bit `used` counter in the page header (about 2 ns) and the cold transition
  call taken whenever a free empties its page, which this workload does on every
  free (about 1 ns once the counter is widened). With a 32-bit counter and no
  transition the same traffic runs at 0.84 ns; the tuning branch's measurements of
  the real allocator agree (2.92, then 2.4, then 1.13 ns; section 12.1).
- wasmalloc loses on three things. A 16 B to 1 MiB realloc chain costs 12 us against
  65 ns for dlmalloc and 86 ns for talc, because every doubling changes size class
  and is a copy while boundary-tag allocators extend the top chunk (12.3). Its
  footprint is 1.9x dlmalloc's on churn, 5x on random actions and 7.9x on a 1 MiB
  Vec growth, because each touched bin costs a whole page (64 KiB, 512 KiB or 4 MiB)
  and the growth step is half the heap (12.5). And its module is 46 KB before
  wasm-opt because the static heap's initialiser is one 19 KB data segment (12.6).
- V8 15.2 changes the picture in two ways: `memory.grow` went from 60 to 75 us per
  call to 0.3 us, and the optimizing tier now inlines the empty
  `__rust_no_alloc_shim_is_unstable_v2` call std makes on every allocation, which is
  a large part of why floors and dlmalloc got 2x to 3x faster while wasmalloc, whose
  loop is already memory-bound, gained 1.2x. Liftoff got 8 to 72 percent slower. V8 does not tier up
  a call that is already running, on either version, so a long single call runs
  entirely in Liftoff (section 9).

## 2. Methodology

The harness is one Rust crate built once per allocator (a cargo feature selects the
`#[global_allocator]`), as a `cdylib` for `wasm32-unknown-unknown` driven by
JavaScript, and as a self-timing `wasm32-wasip1` binary for wasmtime that also
builds natively. Workloads go through `std::alloc::{alloc, dealloc, realloc}`, so
the `__rust_alloc` shims and the `#[global_allocator]` dispatch are part of what is
measured, as in a real program. Every allocation is written to and every pointer
is folded into a checksum the caller receives, which stops LLVM's allocation-site
elimination without `black_box` (which spills through the shadow stack on wasm32).

Protocol per workload: `reset()` (rewinds the harness floors; a no-op for real
allocators), the workload's setup, the timed call, the teardown. Warm up until the
last three calls agree within 10 percent (3 to 12 calls, up to 30 while V8 still
reports Liftoff code for the function), then 7 timed calls; report median and
minimum ns per op with the median cost of an empty JS-to-wasm call subtracted from
each timed call.

The workload loops are `#[inline(always)]` into the exported functions. V8 tiers
up and reports the compilation tier per function, and its tiering budget is
charged to the function whose code executes. In the first version of the harness
most loop bodies were separate functions called from a thin export; the body
tiered up after a few calls while the export, called a few dozen times, stayed in
Liftoff, so `%IsLiftoffFunction(export)` said "Liftoff" for code that was running
optimized. The default-tiering tables committed with that version (the
size-class floor only, commit 1082415) marked batch, churn, Vec growth and the
memory-grow workloads "L" for this reason; their timings were in fact optimizing-
tier timings and agree with `--no-liftoff` to within noise. Every table in this
document and in the current REPORT.md was measured with the inlined loops, so an
"L" now means what it says.

Engine runs are pinned to four cores (`taskset -c 4-7`) and wrapped in
`scripts/memlimit`. The matrix took four minutes; the load average stayed below 1
throughout (recorded in the run log). Repeating `large_alloc_free`, the most
memory-bound workload, three times on a quiet machine reproduced the matrix to
within 2 percent; the same run while a fat-LTO build was in progress on other cores
was 25 percent slower, so treat any single result within 5 percent as noise and
anything over 20 percent as real.

### Floors

Four allocators written in the harness give the cheapest code that can serve a
workload, so the incumbents and wasmalloc are compared against a measured floor
rather than against each other:

- `bump`: pointer bump over `memory.grow` in 16 MiB chunks, free is a no-op.
- `freelist`: one intrusive LIFO free list for blocks up to 32 bytes, bump otherwise.
- `sizeclass`: 64 free lists at 16-byte granularity up to 1024 bytes, the class
  computed from the `Layout` on free (no header read).
- `pages`: `sizeclass` with the class read from a 64 KiB page header on free,
  the mimalloc shape.

Four more, `mimic`, `mimic_lean`, `mimic_u32` and `mimic_nozero`, reproduce the
memory traffic of wasmalloc's fast paths with one detail changed each; they exist
for the attribution in section 12.1 and are described there.

The floors leak everything above their size limit (they fall through to bump), so
for the random-actions workloads (sizes to 10 KB) they are a bound, not a target,
and because fresh memory misses the cache they are in fact slower than wasmalloc
there.

### Workloads

| name | one op is | what it exercises |
|---|---|---|
| alloc_free_32 | alloc+free pair | 32 B, align 8, freed immediately: the cache-hot fast path |
| alloc_free_32_align16 | alloc+free pair | the same with align 16 (`v128`); dlmalloc-rs takes its memalign path |
| batch_lifo_32, batch_fifo_32 | alloc+free pair | 1000 allocations then 1000 frees, LIFO or FIFO order |
| churn | free+alloc step | 10,000 live objects of 16..1024 B (multiples of 8); each step frees a random one and allocates a random replacement |
| random_actions | action | talc-style: 3/7 alloc (1..=10000 B, biased small; align 8 in 75 percent, else 16, 32, 64), 3/7 free a random live object, 1/7 realloc one to a fresh random size; below 100 live objects every action allocates |
| random_actions_norealloc | action | the same without the realloc choice (1/2 alloc, 1/2 free) |
| vec_push_growth | 1 MiB Vec | `Vec<u8>::push` from empty to 1 MiB |
| realloc_doubling | chain | realloc by doubling from 16 B to 1 MiB, 16 reallocs, one byte written after each |
| large_alloc_free | alloc+touch+free | 256 KiB, 512 KiB, 1 MiB, 2 MiB, 4 MiB in turn; one byte written per 4 KiB |
| memory_grow_only, memory_grow_touch | grow | `memory.grow` of 1 MiB, untouched or written once per 4 KiB |

`memory.size` is recorded before and after every workload; a separate footprint
step runs each workload in a fresh process so that the pages one workload pulls
in are not attributed to another (section 6).

### Variants

| variant | what is measured |
|---|---|
| dlmalloc | std's default: dlmalloc-rs 0.2.11 on wasm32-unknown-unknown. On wasm32-wasip1 (the wasmtime column) std's default is wasi-libc's C dlmalloc, a different binary; natively it is glibc malloc |
| talc | talc 5.1.0, `talc::wasm::new_wasm_dynamic_allocator()` as its wasm README recommends |
| lol_alloc | lol_alloc 0.4.1, `AssumeSingleThreaded<FreeListAllocator>` |
| wasmalloc | this repository's `WasmAlloc`, built with the harness's profile |

## 3. Machine and engines

- AMD Ryzen 9 9950X (16 cores, SMT on, boost on, `powersave` governor under
  amd-pstate; 48 KiB L1d and 1 MiB L2 per core, 64 MiB L3), 60 GiB, Linux
  7.1.7-200.fc44.x86_64. Runs pinned to CPUs 4-7.
- node v22.22.2 (V8 12.4.254.21-node.39); d8 from jsvu, V8 15.2.124; JavaScriptCore
  r320222 (jsvu); wasmtime 48.0.1 (Cranelift, default settings).
- rustc 1.95.0, targets wasm32-unknown-unknown and wasm32-wasip1; wasm-opt 125;
  wasm-tools 1.258.0.
- Build profile `release`: opt-level 3, fat LTO, one codegen unit, panic=abort,
  debuginfo stripped. The shim study also uses `release-nolto` (16 codegen units,
  no LTO), `release-z` (opt-level z) and `release-z-nolto`.
- V8 flags: `--no-liftoff` for the optimizing tier (TurboFan in 12.4, Turboshaft in
  15.2; both are reported as "turbofan" by `%IsTurboFanFunction`), `--liftoff-only`
  for the baseline tier, default dynamic tiering otherwise. `--liftoff
  --no-wasm-tier-up`, the recipe the v8.dev documentation gives, does not pin
  Liftoff on either version (the flag-check table in REPORT.md shows the function in
  optimized code after 4 calls); `--liftoff --no-wasm-tier-up --no-wasm-dynamic-tiering`
  and `--liftoff-only` do.

## 4. Results per engine and tier

Median ns/op. Full tables with minimums and every variant: REPORT.md.

### V8 12.4 (node 22), optimizing tier (`--no-liftoff`)

| workload | bump | freelist | sizeclass | pages | dlmalloc | talc | lol_alloc | wasmalloc |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| alloc_free_32 | 1.22 | 1.80 | 1.81 | 1.74 | 7.99 | 12.35 | 3.29 | 3.61 |
| alloc_free_32_align16 | 1.23 | 1.80 | 1.80 | 1.74 | 13.56 | 12.35 | 3.30 | 4.98 |
| batch_lifo_32 | 1.52 | 2.09 | 1.88 | 2.21 | 9.03 | 10.09 | 2.67 | 3.55 |
| batch_fifo_32 | 1.55 | 2.08 | 1.96 | 2.21 | 12.59 | 10.00 | 2.77 | 3.48 |
| churn | 2.70 | 3.22 | 3.57 | 4.19 | 61.11 | 32.38 | 10120 | 7.13 |
| random_actions | 19.42 | 19.99 | 19.03 | 18.87 | 43.42 | 22.32 | 49.66 | 16.81 |
| random_actions_norealloc | 5.62 | 6.28 | 8.55 | 8.26 | 38.02 | 19.73 | 50.56 | 12.86 |
| vec_push_growth (us) | 406 | 407 | 406 | 412 | 386 | 387 | 394 | 400 |
| realloc_doubling | 37325 | 36655 | 36617 | 36579 | 65.0 | 86.1 | 11005 | 12275 |
| large_alloc_free | 2228 | 2240 | 2249 | 2224 | 1848 | 2456 | 2193 | 2259 |
| memory_grow_only (us) | 67.6 | 61.8 | 74.4 | 61.1 | 62.7 | 76.4 | 75.5 | 65.2 |

### V8 12.4 (node 22), baseline tier (`--liftoff-only`)

| workload | bump | freelist | sizeclass | pages | dlmalloc | talc | lol_alloc | wasmalloc |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| alloc_free_32 | 2.79 | 3.73 | 3.62 | 3.12 | 10.98 | 18.40 | 7.69 | 4.56 |
| alloc_free_32_align16 | 2.73 | 3.71 | 3.34 | 3.12 | 20.39 | 18.35 | 7.68 | 5.66 |
| batch_lifo_32 | 3.51 | 3.60 | 3.84 | 3.19 | 12.29 | 18.63 | 5.42 | 5.24 |
| batch_fifo_32 | 3.49 | 3.60 | 3.66 | 3.55 | 17.84 | 18.81 | 5.64 | 5.25 |
| churn | 4.41 | 5.47 | 6.29 | 6.76 | 69.18 | 38.24 | 10182 | 10.19 |
| random_actions | 23.21 | 24.05 | 23.94 | 23.22 | 51.85 | 27.14 | 58.48 | 20.90 |
| random_actions_norealloc | 7.04 | 7.75 | 10.78 | 9.94 | 44.94 | 24.05 | 58.47 | 15.83 |
| realloc_doubling | 36313 | 35654 | 36150 | 36426 | 109.8 | 138.8 | 11047 | 12963 |
| large_alloc_free | 1935 | 2062 | 2162 | 2177 | 1935 | 2298 | 1889 | 1965 |

### V8 15.2 (d8), optimizing tier (`--no-liftoff`)

| workload | bump | freelist | sizeclass | pages | dlmalloc | talc | lol_alloc | wasmalloc |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| alloc_free_32 | 0.98 | 0.55 | 0.55 | 0.58 | 4.11 | 8.92 | 1.62 | 2.93 |
| alloc_free_32_align16 | 0.98 | 0.55 | 0.55 | 0.58 | 7.90 | 8.92 | 1.46 | 3.10 |
| batch_lifo_32 | 0.98 | 0.91 | 0.92 | 1.10 | 4.37 | 4.44 | 1.73 | 2.86 |
| batch_fifo_32 | 0.98 | 0.90 | 0.92 | 1.09 | 5.82 | 4.36 | 1.86 | 2.87 |
| churn | 2.38 | 2.87 | 3.20 | 3.62 | 55.79 | 26.18 | 10106 | 6.43 |
| random_actions | 17.03 | 17.79 | 17.06 | 17.23 | 40.61 | 20.10 | 48.33 | 15.85 |
| random_actions_norealloc | 5.50 | 5.85 | 8.10 | 8.12 | 34.47 | 17.70 | 49.00 | 11.68 |
| vec_push_growth (us) | 597 | 503 | 416 | 409 | 387 | 404 | 505 | 401 |
| realloc_doubling | 36470 | 36280 | 36660 | 36820 | 50.0 | 50.0 | 11160 | 11900 |
| large_alloc_free | 2160 | 2260 | 2160 | 2140 | 2160 | 2440 | 2100 | 2180 |
| memory_grow_only | 312 | 250 | 312 | 312 | 312 | 312 | 312 | 312 |

### V8 15.2 (d8), baseline tier (`--liftoff-only`)

| workload | bump | freelist | sizeclass | pages | dlmalloc | talc | lol_alloc | wasmalloc |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| alloc_free_32 | 3.30 | 4.05 | 3.98 | 4.10 | 18.86 | 23.11 | 8.78 | 6.27 |
| alloc_free_32_align16 | 3.28 | 4.05 | 4.02 | 4.12 | 29.84 | 23.11 | 8.81 | 9.45 |
| batch_lifo_32 | 4.06 | 4.17 | 4.18 | 4.03 | 19.91 | 21.89 | 6.00 | 5.80 |
| batch_fifo_32 | 4.02 | 4.18 | 4.19 | 4.12 | 25.43 | 21.62 | 6.38 | 5.79 |
| churn | 5.02 | 6.04 | 6.78 | 7.49 | 74.17 | 43.15 | 10185 | 12.34 |
| random_actions | 23.38 | 24.18 | 22.84 | 22.76 | 54.76 | 30.66 | 59.03 | 23.23 |
| random_actions_norealloc | 7.54 | 8.25 | 10.45 | 10.33 | 47.90 | 27.23 | 59.16 | 17.63 |
| realloc_doubling | 36340 | 36010 | 36110 | 36250 | 140.0 | 170.0 | 10780 | 13590 |
| large_alloc_free | 2380 | 2160 | 2080 | 2080 | 1960 | 2300 | 1900 | 4180 |

### JavaScriptCore (default tiering; JSC's optimizing tier is what the steady state runs)

| workload | bump | freelist | sizeclass | pages | dlmalloc | talc | lol_alloc | wasmalloc |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| alloc_free_32 | 0.84 | 0.55 | 0.55 | 0.55 | 4.67 | 8.45 | 1.08 | 2.80 |
| alloc_free_32_align16 | 0.80 | 0.55 | 0.55 | 0.55 | 10.33 | 8.45 | 1.08 | 2.74 |
| batch_lifo_32 | 0.85 | 0.92 | 0.92 | 1.07 | 4.67 | 7.64 | 1.34 | 2.18 |
| batch_fifo_32 | 0.85 | 0.92 | 0.92 | 1.08 | 9.02 | 7.58 | 1.66 | 2.18 |
| churn | 2.43 | 2.85 | 3.24 | 3.68 | 52.81 | 25.90 | 10121 | 6.14 |
| random_actions | 16.67 | 16.43 | 17.61 | 17.02 | 40.21 | 19.20 | 47.26 | 15.34 |
| random_actions_norealloc | 5.60 | 5.95 | 8.13 | 8.24 | 34.53 | 16.90 | 47.90 | 12.05 |
| realloc_doubling | 36570 | 36333 | 36143 | 37104 | 80.5 | 65.9 | 10869 | 12102 |
| large_alloc_free | 2353 | 2422 | 2402 | 2412 | 2153 | 2251 | 2207 | 2207 |
| memory_grow_only | 366 | 381 | 351 | 351 | 687 | 351 | 336 | 366 |

### wasmtime 48 Cranelift (wasip1 binary; "dlmalloc" here is wasi-libc's C dlmalloc)

| workload | bump | freelist | sizeclass | pages | dlmalloc (C) | talc | lol_alloc | wasmalloc |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| alloc_free_32 | 0.56 | 0.71 | 0.71 | 0.89 | 4.77 | 8.27 | 2.41 | 2.97 |
| alloc_free_32_align16 | 0.56 | 0.71 | 0.71 | 0.89 | 5.40 | 8.27 | 2.41 | 2.96 |
| batch_lifo_32 | 0.87 | 1.04 | 1.03 | 1.29 | 5.02 | 7.48 | 1.63 | 2.86 |
| batch_fifo_32 | 0.84 | 1.07 | 1.06 | 1.27 | 6.81 | 7.27 | 1.53 | 2.85 |
| churn | 2.36 | 2.85 | 3.17 | 3.51 | 49.03 | 26.29 | 10453 | 6.28 |
| random_actions | 17.24 | 17.93 | 17.64 | 17.71 | 38.43 | 20.21 | 52.61 | 15.15 |
| random_actions_norealloc | 5.59 | 5.72 | 8.17 | 8.13 | 33.05 | 17.51 | 52.95 | 11.53 |
| realloc_doubling | 36508 | 36354 | 36512 | 36278 | 37.3 | 60.9 | 10885 | 12431 |
| large_alloc_free | 2564 | 2557 | 2539 | 2519 | 2380 | 2391 | 2407 | 2439 |
| memory_grow_only | 264 | 278 | 276 | 270 | 272 | 264 | 281 | 278 |

Native x86_64 (the host build of the same binary, glibc malloc as "dlmalloc"):
freelist floor 0.36, sizeclass 0.36, pages 0.71, glibc 3.84 on alloc_free_32;
churn 2.71 (sizeclass) against 7.93 (glibc); random_actions 16.2 against 31.9.
Cranelift and the optimizing JS tiers sit within 2x of native on every floor.

Default dynamic tiering (not shown) matches `--no-liftoff` on both V8 versions
for every workload whose function executes enough code to exhaust the tiering
budget; `realloc_doubling` (1600 reallocs per call) and the two `memory_grow`
workloads never do and stay in Liftoff for the whole run. One anomaly worth
recording: on node 22 under default tiering `memory_grow_touch` costs 842 us per
1 MiB grown and touched, against 208 us under `--liftoff-only` and 231 us under
`--no-liftoff`; d8 15.2 shows no such effect (127 to 131 us in all three modes).

## 5. wasmalloc against the floor and the incumbents

Median ns/op; "floor" is the harness allocator in parentheses. The ratio after an
incumbent is its time over wasmalloc's (below 1 means the incumbent is faster).

| engine, tier | workload | floor | wasmalloc | wasmalloc/floor | dlmalloc | talc |
|---|---|---:|---:|---:|---:|---:|
| node 22 opt | alloc_free_32 | 1.80 (freelist) | 3.61 | 2.0x | 7.99 (2.2x) | 12.35 (3.4x) |
| node 22 opt | alloc_free_32_align16 | 1.80 (freelist) | 4.98 | 2.8x | 13.56 (2.7x) | 12.35 (2.5x) |
| node 22 opt | batch_lifo_32 | 2.09 (freelist) | 3.55 | 1.7x | 9.03 (2.5x) | 10.09 (2.8x) |
| node 22 opt | batch_fifo_32 | 2.08 (freelist) | 3.48 | 1.7x | 12.59 (3.6x) | 10.00 (2.9x) |
| node 22 opt | churn | 3.57 (sizeclass) | 7.13 | 2.0x | 61.11 (8.6x) | 32.38 (4.5x) |
| node 22 opt | random_actions | 19.03 (sizeclass, leaks) | 16.81 | 0.9x | 43.42 (2.6x) | 22.32 (1.3x) |
| node 22 opt | random_actions_norealloc | 8.55 (sizeclass, leaks) | 12.86 | 1.5x | 38.02 (3.0x) | 19.73 (1.5x) |
| node 22 opt | vec_push_growth | 406 us (bump) | 400 us | 1.0x | 386 us (0.96x) | 387 us (0.97x) |
| node 22 opt | realloc_doubling | - | 12275 | - | 65.0 (0.005x) | 86.1 (0.007x) |
| node 22 opt | large_alloc_free | 2228 (bump) | 2259 | 1.0x | 1848 (0.82x) | 2456 (1.1x) |
| node 22 Liftoff | alloc_free_32 | 3.73 (freelist) | 4.56 | 1.2x | 10.98 (2.4x) | 18.40 (4.0x) |
| node 22 Liftoff | churn | 6.29 (sizeclass) | 10.19 | 1.6x | 69.18 (6.8x) | 38.24 (3.8x) |
| node 22 Liftoff | random_actions | 23.94 (sizeclass, leaks) | 20.90 | 0.9x | 51.85 (2.5x) | 27.14 (1.3x) |
| d8 15.2 opt | alloc_free_32 | 0.55 (freelist) | 2.93 | 5.3x | 4.11 (1.4x) | 8.92 (3.0x) |
| d8 15.2 opt | alloc_free_32_align16 | 0.55 (freelist) | 3.10 | 5.6x | 7.90 (2.5x) | 8.92 (2.9x) |
| d8 15.2 opt | batch_lifo_32 | 0.91 (freelist) | 2.86 | 3.2x | 4.37 (1.5x) | 4.44 (1.6x) |
| d8 15.2 opt | batch_fifo_32 | 0.90 (freelist) | 2.87 | 3.2x | 5.82 (2.0x) | 4.36 (1.5x) |
| d8 15.2 opt | churn | 3.20 (sizeclass) | 6.43 | 2.0x | 55.79 (8.7x) | 26.18 (4.1x) |
| d8 15.2 opt | random_actions | 17.06 (sizeclass, leaks) | 15.85 | 0.9x | 40.61 (2.6x) | 20.10 (1.3x) |
| d8 15.2 opt | random_actions_norealloc | 8.10 (sizeclass, leaks) | 11.68 | 1.4x | 34.47 (3.0x) | 17.70 (1.5x) |
| d8 15.2 opt | realloc_doubling | - | 11900 | - | 50.0 (0.004x) | 50.0 (0.004x) |
| d8 15.2 opt | large_alloc_free | 2160 (bump) | 2180 | 1.0x | 2160 (1.0x) | 2440 (1.1x) |
| d8 15.2 Liftoff | alloc_free_32 | 4.05 (freelist) | 6.27 | 1.5x | 18.86 (3.0x) | 23.11 (3.7x) |
| d8 15.2 Liftoff | alloc_free_32_align16 | 4.05 (freelist) | 9.45 | 2.3x | 29.84 (3.2x) | 23.11 (2.4x) |
| d8 15.2 Liftoff | churn | 6.78 (sizeclass) | 12.34 | 1.8x | 74.17 (6.0x) | 43.15 (3.5x) |
| JSC | alloc_free_32 | 0.55 (freelist) | 2.80 | 5.1x | 4.67 (1.7x) | 8.45 (3.0x) |
| JSC | churn | 3.24 (sizeclass) | 6.14 | 1.9x | 52.81 (8.6x) | 25.90 (4.2x) |
| JSC | random_actions | 17.61 (sizeclass, leaks) | 15.34 | 0.9x | 40.21 (2.6x) | 19.20 (1.3x) |
| wasmtime | alloc_free_32 | 0.71 (freelist) | 2.97 | 4.2x | 4.77 (1.6x) | 8.27 (2.8x) |
| wasmtime | churn | 3.17 (sizeclass) | 6.28 | 2.0x | 49.03 (7.8x) | 26.29 (4.2x) |
| wasmtime | random_actions | 17.64 (sizeclass, leaks) | 15.15 | 0.9x | 38.43 (2.5x) | 20.21 (1.3x) |

lol_alloc is the one incumbent faster than wasmalloc on the 32-byte pair (1.62 ns
on d8): its single free list is a degenerate LIFO there. On churn it takes 10 us
per step (a linear walk of a 10,000-entry sorted list) and on random actions 48 ns.

## 6. Footprint

`memory.size` in 64 KiB pages after one workload ran alone in a fresh process
(node `--no-liftoff`; footprint does not depend on the tier). Every module starts
at 20 pages (the linker's initial memory: data, 1 MiB shadow stack, the gap to the
page boundary). In parentheses, the pages the workload added over the 20 relative
to what dlmalloc added.

| workload | dlmalloc | talc | lol_alloc | wasmalloc | sizeclass floor |
|---|---:|---:|---:|---:|---:|
| alloc_free_32 | 21 | 21 (1.0x) | 21 (1.0x) | 36 (16x) | 276 (leaks) |
| churn | 104 | 121 (1.2x) | 110 (1.1x) | 181 (1.9x) | 276 |
| random_actions | 32 | 34 (1.2x) | 32 (1.0x) | 81 (5.1x) | 2324 (leaks) |
| random_actions_norealloc | 38 | 41 (1.2x) | 39 (1.1x) | 81 (3.4x) | 2068 (leaks) |
| vec_push_growth | 54 | 54 (1.0x) | 52 (0.9x) | 288 (7.9x) | 788 (leaks) |
| realloc_doubling | 54 | 54 (1.0x) | 52 (0.9x) | 288 (7.9x) | 3348 (leaks) |
| large_alloc_free | 149 | 149 (1.0x) | 144 (1.0x) | 288 (2.1x) | 1300 (leaks) |

The design goal is a peak within about 1.5x of dlmalloc's; wasmalloc misses it on
every workload but the trivial one. Section 12.5 traces each row to a mechanism.

## 7. Module size

Bytes of the harness module for wasm32-unknown-unknown, `release` profile, before
and after `wasm-opt -O3 --all-features`. The module includes the workloads and
what they pull from std, so the difference from `bump` is the allocator's own
contribution.

| variant | release | after wasm-opt -O3 | over bump (wasm-opt) |
|---|---:|---:|---:|
| bump | 16,366 | 9,137 | 0 |
| freelist | 18,479 | 10,921 | 1,784 |
| sizeclass | 18,446 | 10,926 | 1,789 |
| pages | 18,552 | 10,905 | 1,768 |
| talc | 19,952 | 12,492 | 3,355 |
| lol_alloc | 21,064 | 13,316 | 4,179 |
| dlmalloc | 26,010 | 17,234 | 8,097 |
| wasmalloc | 45,757 | 19,051 | 9,914 |

wasmalloc's raw module is 2.3x its wasm-opt'd size where every other variant is
1.5x to 1.7x: `wasm-tools objdump` shows an 18,941-byte data section against 626 to
790 bytes for the others, and a 20,575-byte code section (dlmalloc 19,115, talc
13,818, the floors about 12,500). Section 12.6 explains the data section.

## 8. Shim and profile study

alloc_free_32 and churn, optimizing tier on both V8 versions, for the four build
profiles and for each profile after `wasm-opt -O3`. Median ns/op.

| variant | profile | bytes | node 22 alloc_free_32 | node 22 churn | d8 15.2 alloc_free_32 | d8 15.2 churn |
|---|---|---:|---:|---:|---:|---:|
| wasmalloc | release | 45,757 | 3.63 | 7.26 | 2.93 | 6.46 |
| wasmalloc | release + wasm-opt | 19,051 | 2.96 | 6.62 | 2.98 | 6.65 |
| wasmalloc | release-nolto | 45,969 | 3.45 | 7.35 | 2.94 | 6.55 |
| wasmalloc | release-nolto + wasm-opt | 19,103 | 2.93 | 6.69 | 2.95 | 6.42 |
| wasmalloc | release-z | 44,086 | 8.06 | 13.45 | 4.44 | 10.13 |
| wasmalloc | release-z + wasm-opt | 15,440 | 3.92 | 8.95 | 4.73 | 7.92 |
| wasmalloc | release-z-nolto | 44,331 | 7.27 | 11.78 | 3.35 | 10.00 |
| wasmalloc | release-z-nolto + wasm-opt | 15,804 | 3.89 | 8.84 | 3.60 | 7.58 |
| dlmalloc | release | 26,010 | 8.00 | 60.89 | 4.07 | 55.80 |
| dlmalloc | release + wasm-opt | 17,234 | 6.27 | 59.14 | 4.35 | 54.09 |
| dlmalloc | release-nolto | 25,878 | 7.93 | 60.96 | 4.11 | 55.72 |
| dlmalloc | release-nolto + wasm-opt | 17,234 | 5.75 | 59.12 | 4.35 | 54.03 |
| dlmalloc | release-z | 25,841 | 9.12 | 61.52 | 3.85 | 56.15 |
| dlmalloc | release-z + wasm-opt | 16,899 | 5.56 | 59.24 | 4.20 | 53.91 |
| dlmalloc | release-z-nolto | 25,847 | 9.10 | 61.46 | 3.85 | 55.90 |
| dlmalloc | release-z-nolto + wasm-opt | 16,899 | 5.55 | 59.10 | 4.20 | 53.88 |
| talc | release | 19,952 | 12.17 | 31.72 | 8.43 | 27.59 |
| talc | release + wasm-opt | 12,492 | 8.65 | 28.06 | 9.30 | 25.51 |
| talc | release-nolto | 20,753 | 11.22 | 31.79 | 8.39 | 26.01 |
| talc | release-nolto + wasm-opt | 12,589 | 8.60 | 29.55 | 8.57 | 25.62 |
| talc | release-z | 19,036 | 18.73 | 40.30 | 7.48 | 28.74 |
| talc | release-z + wasm-opt | 10,063 | 13.61 | 34.93 | 7.70 | 26.43 |
| talc | release-z-nolto | 19,044 | 22.55 | 41.18 | 7.45 | 28.69 |
| talc | release-z-nolto + wasm-opt | 10,074 | 12.32 | 34.86 | 7.73 | 26.48 |
| sizeclass | release | 18,446 | 1.80 | 3.55 | 0.55 | 3.25 |
| sizeclass | release + wasm-opt | 10,926 | 0.55 | 3.26 | 0.55 | 3.26 |
| sizeclass | release-nolto | 20,244 | 1.57 | 3.54 | 0.54 | 3.18 |
| sizeclass | release-nolto + wasm-opt | 12,570 | 0.55 | 3.27 | 0.54 | 3.23 |
| sizeclass | release-z | 17,010 | 4.34 | 7.54 | 1.65 | 3.13 |
| sizeclass | release-z + wasm-opt | 9,355 | 2.90 | 4.37 | 1.65 | 3.18 |
| sizeclass | release-z-nolto | 17,128 | 4.27 | 7.22 | 0.89 | 3.17 |
| sizeclass | release-z-nolto + wasm-opt | 9,483 | 1.77 | 3.96 | 0.89 | 3.19 |

What the text format says about who calls whom (`inspect.mjs` over the demangled
module; the full listing is in REPORT.md):

- wasmalloc, `release`: the fast paths are inlined into the workload loops. The
  `alloc_free_32` loop is 109 wasm instructions and calls only
  `Heap::alloc_generic` and `Heap::dealloc_transition` (both cold) plus
  `__rust_no_alloc_shim_is_unstable_v2`, the empty function std's `alloc::alloc`
  calls before every `__rust_alloc`. `__rust_alloc` itself is 67 instructions with
  the fast path inlined and two cold calls; `__rust_dealloc` 56 instructions. The
  align-16 loop calls `dealloc_generic` on every free (section 12.2).
  `__rust_realloc` is 664 instructions with three `panic_bounds_check` calls: the
  queue and direct-table indexing in the realloc path is not proven in range.
- wasmalloc, `release-z`: LLVM at opt-level z declines every hint. The loop calls
  std's `alloc::alloc::alloc` out of line, which calls a 4-instruction `__rust_alloc`,
  which calls `Heap::alloc`: three calls per allocation and the same for free. That
  is the 2.2x (node) and 1.5x (d8) slowdown in the table. `wasm-opt -O3` inlines
  the two shims but not `Heap::alloc`/`Heap::dealloc`, so the recovered figure is
  still 1.2x to 1.6x the `release` one. A consumer building at `-Oz` (simlin does)
  gets this unless the fast paths are `#[inline(always)]`.
- dlmalloc: the loop calls `__rust_alloc` (4 instructions) which calls `__rdl_alloc`
  which calls `Dlmalloc::malloc`; the fast path is never inlined into the caller in
  any profile. wasm-opt inlines the two thin shims, which is worth 28 percent on
  node and nothing on d8 (whose inliner already did it).
- talc: `__rust_alloc` is inlined into the loop but `Talc::allocate` and
  `Talc::deallocate` are calls by design (talc keeps them out of line on wasm for
  size). wasm-opt inlines them on node for a 27 percent gain.
- sizeclass floor: fully inlined; the only call left in the loop is the empty
  `__rust_no_alloc_shim_is_unstable_v2`. Removing it (wasm-opt) takes node from
  1.80 to 0.55 ns, exactly d8's figure: V8 15.2 inlines that call itself, V8 12.4
  does not, and with an opaque call in the loop TurboFan 12.4 cannot keep the list
  head in a register across iterations. The same call costs wasmalloc about 0.7 ns
  on node (3.63 against 2.96 after wasm-opt) and nothing on d8.
- LTO changes nothing beyond 10 percent for any variant at opt-level 3: the fast
  paths are `#[inline]`, which makes them available for cross-crate inlining without
  LTO, and the rest is out of line either way. At opt-level z without LTO talc loses
  another 20 percent on node (22.55 against 18.73) while the others do not move.

## 9. Tier-up study

How many calls of `alloc_free_32(n)` before V8's dynamic tiering swaps in
optimized code, one process per row (the probe includes a `reset()` and a tier
query per call, so its per-op times are not comparable with the matrix):

| n per call | node 22 calls | node 22 iterations | d8 15.2 calls | d8 15.2 iterations |
|---:|---:|---:|---:|---:|
| 1 | 36,410 | 36,410 | 25,416 | 25,416 |
| 10 | 3,595 | 35,950 | 4,399 | 43,990 |
| 100 | 1,000 | 100,000 | 538 | 53,800 |
| 1,000 | 161 | 161,000 | 82 | 82,000 |
| 10,000 | 16 | 160,000 | 9 | 90,000 |
| 100,000 | 2 | 200,000 | 1 | 100,000 |

(wasmalloc rows; the other variants are within a factor of two, in REPORT.md.)
The budget is `--wasm-tiering-budget=13000000`, "a rough approximation of bytes
executed", charged per function at loop back-edges and returns; it comes to
roughly 50,000 (d8) to 200,000 (node) alloc+free pairs executed by one function
before that function is optimized. Optimized code arrives asynchronously and is used from the
next call: a single call of `alloc_free_32(400,000,000)` (1.5 s) ran at 3.87 ns/op
on d8 and 3.86 on node, the Liftoff figure, against 0.57 and 1.85 for the call
after it. Neither V8 version replaces the code of a running wasm function.

Whether Liftoff matters therefore depends on the shape of the program, not only its
lifetime: a page that allocates fewer than about 100,000 objects from any one
function stays in Liftoff for all of them, and a program whose work is one long
call (a compile step, a render) runs that whole call in Liftoff however long it
takes. In Liftoff wasmalloc's advantage over the incumbents is larger than in the
optimizing tier (2.4x to 3x over dlmalloc, 3.7x to 4x over talc on the pair), its
Liftoff-to-optimized ratio is the smallest of the real allocators (1.26x on node,
2.1x on d8, against dlmalloc's 1.4x and 4.6x), and it is within 1.2x (node) to 1.5x
(d8) of the floor. The reasons are structural: the fast path is one straight-line
function inlined into the caller, so Liftoff, which does not inline, pays no call,
while dlmalloc pays three calls and talc two per allocation. The 16-bit `used`
counter that dominates in the optimizing tier (12.1) is invisible in Liftoff:
`mimic_u32` and `mimic` are equal there.

## 10. V8 12.4 (node 22) against V8 15.2 (d8)

| variant | workload | opt 12.4 | opt 15.2 | ratio | Liftoff 12.4 | Liftoff 15.2 | ratio |
|---|---|---:|---:|---:|---:|---:|---:|
| freelist | alloc_free_32 | 1.80 | 0.55 | 0.31x | 3.73 | 4.05 | 1.09x |
| sizeclass | churn | 3.57 | 3.20 | 0.90x | 6.29 | 6.78 | 1.08x |
| wasmalloc | alloc_free_32 | 3.61 | 2.93 | 0.81x | 4.56 | 6.27 | 1.37x |
| wasmalloc | churn | 7.13 | 6.43 | 0.90x | 10.19 | 12.34 | 1.21x |
| wasmalloc | random_actions | 16.81 | 15.85 | 0.94x | 20.90 | 23.23 | 1.11x |
| dlmalloc | alloc_free_32 | 7.99 | 4.11 | 0.51x | 10.98 | 18.86 | 1.72x |
| dlmalloc | churn | 61.11 | 55.79 | 0.91x | 69.18 | 74.17 | 1.07x |
| talc | alloc_free_32 | 12.35 | 8.92 | 0.72x | 18.40 | 23.11 | 1.26x |
| any | memory_grow_only | 61,795 to 76,424 | 250 to 312 | 0.005x | 60,027 to 78,399 | 312 to 625 | 0.005x |

- The optimizing tier improved most where the loop had calls or a register-promotable
  hot word: 15.2 inlines small wasm callees (the empty std shim, dlmalloc's two thin
  shims) and its load elimination keeps the floors' list heads in registers, so
  floors got 3.3x faster and dlmalloc 1.9x. wasmalloc's loop was already call-free
  and its bottleneck is a store-to-load chain through the page header (12.1), so it
  gained 1.2x. On churn, which is dominated by cache misses over 10,000 objects,
  everything gained about 10 percent.
- Liftoff got slower on 15.2 for every variant, 8 to 72 percent; wasmalloc 37
  percent on the pair. Liftoff on 15.2 is also where wasmalloc's align-16 dealloc
  slow path shows most (9.45 against 6.27 ns).
- `memory.grow` costs 60 to 78 us per call on V8 12.4 regardless of size and 0.25 to
  0.6 us on 15.2, in line with wasmtime (0.27 us) and JSC (0.35 us). The growth
  policy in `slices.rs` was written against the 12.4 cost. Node 22 is a current LTS
  and ships 12.4, so the cost is still real for server-side users, but for browsers
  on a current V8 (and for JSC and wasmtime) the geometric step buys almost nothing
  and its footprint overshoot (12.5) is pure cost.
- Both versions tier up at the same budget and neither replaces a running
  function's code. `%IsTurboFanFunction` reports true for Turboshaft code on 15.2.

## 11. The roofline

A 32-byte alloc+free pair costs 0.98 ns on the bump floor and 0.55 ns on the
free-list floor under the optimizing tier on d8 15.2 (JSC: 0.84 and 0.55;
wasmtime: 0.56 and 0.71; node 22: 1.22 and 1.80); dlmalloc-rs 4.11 ns, talc 8.92,
wasmalloc 2.93. The bump floor is above the free-list floor on the JITs because it
touches a fresh cache line every iteration while the free list reuses one.

The floor for churn over 10,000 live objects of 16 to 1024 bytes is 3.20 ns per
free+alloc step (size-class lists, d8 15.2; 3.57 on node); wasmalloc 6.43, talc
26.18, dlmalloc 55.79. The floor for a random 32-byte pair with 16-byte alignment
is the same 0.55 ns; wasmalloc 3.10, dlmalloc 7.90, talc 8.92.

For the random-actions loop with sizes to 10 KB there is no honest floor in the
harness (the size-class floor leaks above 1 KiB and is slower than wasmalloc
because fresh memory misses the cache); wasmalloc's 15.85 ns per action (d8) is the
best figure measured, against talc 20.10 and dlmalloc 40.61.

Relative to the free-list floor wasmalloc's fast path is 2.0x on node 22 and 5.3x on
d8, JSC and wasmtime; relative to the size-class floor its churn is 2.0x
everywhere. Section 12.1 shows the fast-path gap is two details wide: a 16-bit
counter and a cold call on every free that empties a page.

## 12. Where wasmalloc loses and why

### 12.1 The fast path: a 16-bit counter and a cold call on every emptying free

Six harness allocators reproduce the memory traffic of `Heap::alloc` and
`Heap::dealloc` (src/heap.rs lines 164 to 254 at e69117b, src/page.rs
`pop`/`push`) with nothing else around it: a direct table indexed by size points
at the class's current 64 KiB page or at a read-only sentinel; the page header
has the same field layout as `page::Page` (free list head at offset 0,
`used: u16` at 4, `free_is_zero` at 16, `flags` at 19); alloc pops the list and
increments `used`; dealloc masks the pointer to the header, pushes, decrements
`used`, clears `free_is_zero` and, when `used == 0 || flags != 0`, calls a cold
out-of-line transition that only counts the event. No queues, no retirement, one
page per class. The counter in the transition matters: the first version of this
floor had an empty transition, LLVM deleted it together with the test that
guarded it, and the floor then lacked one load, one branch and one call per free
that wasmalloc pays whenever a free empties its page, which in `alloc_free_32` is
every free. The tuning engineer caught this from the text format. The variants:

- `mimic`: exactly the traffic above.
- `mimic_lean`: the free list head only; no `used`, no `free_is_zero`, no test.
- `mimic_u32`: `used` read and written as a 32-bit word at the same offset.
- `mimic_nozero`: no `free_is_zero` store on free.
- `mimic_notest`: no `used == 0 || flags != 0` test and no transition call.
- `mimic_u32_notest`: the 32-bit counter and no test.

| engine, tier | freelist | mimic_lean | mimic_u32_notest | mimic_u32 | mimic_notest | mimic_nozero | mimic | wasmalloc |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| d8 15.2 opt, alloc_free_32 | 0.55 | 0.56 | 0.84 | 1.87 | 2.83 | 2.83 | 2.90 | 2.93 |
| d8 15.2 opt, batch_lifo_32 | 0.91 | 1.00 | 1.28 | 1.49 | 2.83 | 2.84 | 2.85 | 2.86 |
| wasmtime, alloc_free_32 | 0.71 | 0.72 | 0.91 | 1.55 | 2.82 | 2.86 | 2.85 | 2.97 |
| wasmtime, batch_lifo_32 | 1.04 | 1.18 | 1.42 | 1.58 | 2.83 | 2.85 | 2.85 | 2.86 |
| node 22 opt, alloc_free_32 | 1.80 | 1.77 | 2.27 | 3.24 | 2.92 | 3.29 | 3.47 | 3.61 |
| node 22 opt, batch_lifo_32 | 2.09 | 2.22 | 2.67 | 3.08 | 3.58 | 3.71 | 3.77 | 3.55 |
| native x86_64, alloc_free_32 | 0.36 | 0.54 | 0.71 | 1.09 | 1.66 | 1.68 | 1.61 | - |

Reading across: the direct-table indirection is free (`mimic_lean` equals the
floor on every engine); the `free_is_zero` byte store is free (`mimic_nozero`
equals `mimic`); `mimic` reproduces wasmalloc to within 0.15 ns on d8 and
wasmtime and on the batch workloads, so on those engines the remaining two
details are the whole gap:

- The 16-bit `used` counter. With the transition test in place, widening it
  takes d8 from 2.90 to 1.87 ns and wasmtime from 2.85 to 1.55; without the test
  it takes d8 from 2.83 to 0.84 and wasmtime from 2.82 to 0.91.
- The transition. On the pair every free empties the page, so every free takes
  the `used == 0` branch into a cold call (in wasmalloc, `dealloc_transition` then
  `retire`, which returns at once because the page is already retired). With a
  32-bit counter the test and call cost 1.0 ns on d8 (0.84 to 1.87), 0.6 on
  wasmtime and 1.0 on node; with the 16-bit counter they cost almost nothing
  (2.83 to 2.90 on d8), because the counter's forwarding chain hides the call.
  On the batch workloads the page empties once per 1000 frees, so only the
  flags load and the branch remain (0.2 ns on d8, 0.4 on node).

The two effects are not additive, which is why fixing either alone looks
disappointing and fixing both is not: on d8 the counter alone gains 1.0 ns, the
call alone gains 0.07, both together gain 2.06.

Applied to wasmalloc itself on branch `tuning-a` (now on main): widening the
counters (da65f8e) took the d8 pair from 2.92 to 2.4 ns, and skipping the retire
call when the emptied page is already retired (e38d647) took it to 1.13. Those two
numbers bracket the mimics (1.87 and 0.84) with a few tenths of a nanosecond of
real-allocator work left on top, as expected.

Why a u16 costs 2 ns: the loop-carried dependency of the pair runs through the
header, store `used` in dealloc, load it in the next alloc, store, load in the next
dealloc. The free-list head has the same store-to-load chain, and on this CPU that
chain is nearly free for 32-bit and 64-bit accesses (Zen 5 forwards or renames such
pairs at zero latency; the floors run at 2.7 cycles per pair), but not for the
16-bit `movw`/`movzwl` pair Cranelift, LLVM and V8 all emit for a `u16` field.
Each 16-bit forward costs the ordinary 4 to 5 cycles and there are two per pair.
The effect is microarchitectural and may be smaller on other cores; the fix is
free on all of them and is on main.

On node the loop is another 0.15 to 0.7 ns slower than `mimic` and on JSC (2.80
against a `mimic` of 1.60 measured before the transition was made observable) the
remainder is larger; that part is in how those engines compile the slightly larger
loop (109 against 104 wasm instructions), not in the memory traffic. Churn is
unaffected by any of this (every mimic is slower than wasmalloc there because the
mimics have no page queues and leak whole pages, so they miss the cache) and so is
Liftoff, where `mimic_u32` and `mimic` are equal. Folding `free_is_zero` into the
flags byte, roadmap item 2 in the design document, buys no speed.

### 12.2 Alignment 16 takes the slow path on free

`Heap::dealloc` takes its inlined small-page path only when `layout.align() <= WORD`
(src/heap.rs line 235); any larger alignment goes to `dealloc_generic`, a cold
out-of-line call that recomputes the class. `Layout::new::<[u8; 32]>()` with
alignment 16, the `v128` case, therefore costs 4.98 against 3.61 ns on node (+1.4),
3.10 against 2.93 on d8 (+0.2, the call is cheap in the optimizing tier), and 9.45
against 6.27 in Liftoff on d8 (+3.2). Alloc already handles it inline: it rounds the
size up to the alignment and takes the direct table. Dealloc can do the same test:
the page kind depends only on the rounded size, and the small-page mask is valid
for every block of a small page whatever its alignment, so the fast-path condition
can be `align <= MAX_NATURAL_ALIGN && round_up(size, align) <= SMALL_MAX_OBJ_SIZE`
with the rounding compiled away for the constant-`Layout` case. dlmalloc-rs pays 1.7x
to 1.9x for the same alignment (memalign); talc pays nothing.

### 12.3 realloc_doubling: every doubling is a copy

A chain of 16 reallocs from 16 B to 1 MiB costs 12.3 us in wasmalloc, 65 ns in
dlmalloc-rs and 86 ns in talc on node (50 ns for both on d8; 37 ns for wasi-libc's
dlmalloc under wasmtime), 190 to 250 times more. lol_alloc, which has no `realloc`
and lets std allocate, copy and free, costs 11.0 us: wasmalloc's realloc is an
alloc-copy-free at every step.

`Heap::realloc` (src/heap.rs lines 263 to 312) returns the same block when the new
class equals the old one, or when shrinking within the same page kind to at least
half the block; a header-less run grows in place when the slices after it are free
(`SliceMap::try_extend`). A doubling never hits any of these. The bins are tight
(bin 2 is 16 B, bin 4 is 32 B, bin 8 is 64 B, bin 12 is 128 B; every power of two
above 64 B ends a group of four), so each doubling changes class, and the one step
that crosses into the header-less range (512 KiB to 1 MiB) crosses a kind boundary
and must move too. The copied volume is 16 + 32 + ... + 512 KiB, just under 1 MiB
per chain; at 12 us that is 85 GB/s, so the copies are L2-resident memcpy and the
allocator work around them is negligible. The bump floor, which also copies every
step but into fresh memory, costs 37 us. dlmalloc and talc extend the block in place
because it sits below the top chunk or a free neighbour.

Suggestions, each measurable with `realloc_doubling` and `vec_push_growth`:

1. Serve blocks above the medium limit (80 KiB) as header-less slice runs instead of
   4 MiB large pages, and grow runs in place: `try_extend` when the following slices
   are free (already implemented for runs), and when the run is at the end of memory,
   `memory.grow` and extend without copying. This is roadmap item 1 and turns the
   top of the chain (128 KiB to 1 MiB, 7/8 of the bytes copied) into a few hundred
   nanoseconds; it also removes the three 4 MiB pages from the footprint (12.5).
2. Below 80 KiB the copy is bounded by the page kind: 10 KiB per small page block,
   80 KiB per medium. A `Vec` doubling amortizes it and `vec_push_growth` shows the
   whole realloc cost of a 1 MiB growth is 3.5 percent of the pushes (400 against
   386 us), so this part is not worth structural change. What is cheap: when the new
   class has a larger block but the same page kind, `realloc` still allocates from
   the general path; it could take the direct table (sizes to 1 KiB) inline.
3. `__rust_realloc` carries three `panic_bounds_check` calls (section 8); the queue
   index derived from `bin()` is not known to be below `QUEUE_COUNT`. Masking or
   `get_unchecked` with a debug assertion removes the checks and the panic
   machinery they keep alive.

### 12.4 large_alloc_free: at the floor, except on V8 12.4

256 KiB to 4 MiB, one byte written per 4 KiB, freed at once. wasmalloc costs 2259
ns per operation on node (bump floor 2228), 2180 on d8 (floor 2160), 2207 on JSC
(floor 2353), 2439 on wasmtime (floor 2564): it is at the floor everywhere. The
workload is 64 to 1024 cache-line touches 4 KiB apart, every one of them missing L1
because a 4 KiB stride maps to one L1 set, so the allocator's share is small.
dlmalloc is at the same floor on d8, JSC and wasmtime but 18 percent below it on
node 22 (1848 ns), reproducibly (three repeats within 1 percent). dlmalloc hands
back the same address for all five sizes (its top chunk absorbs every free), so the
touched lines are the same lines every time; wasmalloc serves 256 KiB and 512 KiB
from two different 4 MiB large pages (one per bin) and the three larger sizes from
header-less runs at the lowest fitting slices, three address ranges instead of one.
That is the only allocator-side difference, and only V8 12.4's code rewards it; the
allocator work itself (a slice-map scan and claim for the runs, a pop and a retire
for the pages) is under 200 ns of the 2.2 us either way. Not a priority, but the
same change as 12.3 (runs instead of large pages) makes the address pattern
dlmalloc-like for the two page-served sizes.

### 12.5 Footprint: a page per touched bin and a half-heap growth step

Every wasmalloc row in section 6 has a mechanical explanation:

- `alloc_free_32`: 16 pages over dlmalloc's 1. The first `memory.grow` is
  `GrowPolicy::DEFAULT.min_grow` = 16 pages (1 MiB) by design; a single 64 KiB page
  was needed. Harmless in absolute terms (1 MiB), but it is the floor of every
  other row.
- `churn`: 161 pages added against dlmalloc's 84 (1.9x) for 5.2 MB of live data.
  Bin waste is at most 12.5 percent (about 10 pages); 23 bins are touched, each with
  a partially used page (up to 23 pages); the rest is the growth policy. `acquire`
  (src/slices.rs line 511) grows by half the current `memory.size` clamped to 16 to
  1024 pages, so memory went 20, 36, 54, 81, 121, 181: the last step added 60 pages
  when the request needed one. A geometric step of half the heap overshoots by up to
  50 percent; a quarter or an eighth would still cost only a handful of `memory.grow`
  calls per doubling of the heap (on node 22 at 60 us each, a heap growing to 64 MiB
  in eighth-steps pays about 2 ms in total; on V8 15.2, JSC and wasmtime the calls
  cost under 1 us).
- `random_actions`: 61 pages added against 12 (5.1x) for at most 1 to 2 MiB live.
  Sizes 1 to 10,000 B touch bins 1 to 37, and every touched bin owns at least one
  64 KiB small page: 37 pages before any object is counted, plus growth overshoot.
  This is the fixed cost of a small heap that roadmap item 3 names; dlmalloc packs
  the same objects into 12 pages.
- `vec_push_growth` and `realloc_doubling`: 268 pages added against 34 (7.9x) for
  1.5 MiB peak live. The doubling chain touches bins 16 KiB, 32 KiB, 64 KiB (three
  medium pages, 512 KiB each, `MEDIUM_PAGE_SIZE`) and 128 KiB, 256 KiB, 512 KiB
  (three large pages, 4 MiB each, `LARGE_PAGE_SIZE`), plus about ten small pages and
  a 1 MiB run: about 15 MiB of pages holding one block each, kept by retirement
  (`retire`: a queue of one page is always retired rather than freed, for
  `RETIRE_CYCLES / 4` collections) and then by growth overshoot.
- `large_alloc_free`: 268 pages added against 129 (2.1x): the 256 KiB and 512 KiB
  bins each hold a 4 MiB large page (8 MiB for two blocks), the three runs share
  one 4 MiB region, and the growth step rounds up.

Suggestions, each measurable with the footprint step:

1. Disable large pages: blocks above 80 KiB become header-less runs of whole
   slices, so a 128 KiB block costs 128 KiB instead of 4 MiB. The design document
   already notes mimalloc's own doubts about large pages. This alone takes
   `vec_push_growth` from 288 to about 100 pages and `large_alloc_free` from 288 to
   about 150, and it is the same change that fixes realloc (12.3).
2. Take medium pages down to 256 KiB (mimalloc's 32-bit constant) or serve the
   16 KiB to 80 KiB bins from runs too; a 20 KiB object should not pin 512 KiB.
3. Grow by an eighth of the heap with the 1 MiB floor, and release retired pages
   faster while the heap is small (the retire count is 16 collections, and a
   collection happens every 1000 slow-path allocations or when a fresh page is
   needed, so an empty page can sit for up to 16,000 slow-path allocations).
4. For the small-heap fixed cost (one page per bin), consider carving the first
   page of several bins from one slice: a page of 16-byte blocks holds 4,000 of
   them, which no small program needs. Sub-slice pages would require the page
   header address to stay derivable from the Layout (a smaller alignment for the
   smallest bins, for instance), so this is a design change, not a tuning knob.

### 12.6 Module size: the data segment and the panic paths

The raw module is 45,757 bytes, 19,747 more than dlmalloc's, but after wasm-opt the
difference is 1,817 bytes. The data section shows why: wasmalloc's `.data` segment
is 17,688 bytes where every other variant's is 8. The static `WasmAlloc` holds a
`Heap` whose `direct` table is initialised to the address of the sentinel page, a
non-zero value, so LLVM emits the whole struct as one initialised segment: the
16 KiB slice bitmaps and the queue array, all zero, ride along. wasm-opt's memory packing splits the segment around its zero
runs; a consumer that does not run wasm-opt (or runs it without
`--zero-filled-memory`) ships and instantiates the zeros. Two ways out: keep the
bitmaps in a separate zero-initialised static so the linker puts them in BSS, or
make the direct table zero-initialised and test for null on the fast path (one
compare, which 12.1 shows is not what costs).

The code section is 20,575 bytes against dlmalloc's 19,115 and the floors' 12,500;
after wasm-opt wasmalloc's is 16,290. `__rust_realloc` alone is 664 wasm
instructions with three bounds-check panics, and the module's `.rodata` is 1,233
bytes against 592 to 764 for the others, the difference being panic message
strings: "slice index starts at", "attempt to divide by zero" (the
`MAX_EXTEND_SIZE / block_size` in `page::extend`, whose divisor is a runtime
field). Removing the bounds checks (12.3) and giving that division a provably
non-zero divisor removes the strings and the formatting code they pull in.

## 13. Suggestions for the tuning engineer

In order of expected payoff per line changed:

1. Widen `Page::used` (and `capacity`) to 32 bits and keep a free that empties an
   already retired page off the cold path. Both are on main (da65f8e, e38d647):
   the 32-byte pair went from 2.92 to 1.13 ns on d8 in the tuning engineer's
   measurement, against a floor of 0.55; the full matrix has not been rerun yet.
   Measure with alloc_free_32, batch_lifo_32, batch_fifo_32; churn and Liftoff
   should not move.
2. Replace 4 MiB large pages with header-less runs that grow in place (at the end
   of memory by growing memory), roadmap item 1. Expected: realloc_doubling from
   12 us to under 1 us; vec_push_growth footprint from 288 to about 100 pages;
   large_alloc_free footprint from 288 to about 150; large_alloc_free time
   unchanged.
3. Footprint policy: growth step of an eighth of the heap (keep the 1 MiB floor),
   256 KiB medium pages, and faster release of retired pages while the heap is
   small. Expected: churn footprint from 181 toward 130 pages (1.3x dlmalloc),
   random_actions from 81 toward 60. The fixed cost of one 64 KiB page per touched
   bin remains and needs a design change to go below.

Also worth doing, cheap: take the small-page dealloc fast path for alignments up to
4096 (12.2; +1.4 ns per free on node, +3.2 in Liftoff on d8 for `v128` types);
mark `Heap::alloc` and `Heap::dealloc` (or the `GlobalAlloc` methods of `WasmAlloc`)
`#[inline(always)]` so consumers at opt-level z keep the inlined fast path (section
8: 2.2x on node at `-Oz` today); put the slice bitmaps in a zero-initialised static
(12.6); drop the bounds checks in the realloc and queue paths (12.3).

Not worth doing: folding `free_is_zero` into `flags` (no measurable cost);
anything aimed at `large_alloc_free` time (at the floor); the JS-side call
overhead (about 1 ns, subtracted).

## 14. Rerun against main after both tuning passes (2026-09-02)

Same harness and protocol as sections 4 to 6, run against main at commit 800fdc7 (tuning-a and
tuning-b merged: u32 counters, inline retire test, aligned frees on the fast path, header-less runs
above a 40 KiB medium limit with 256 KiB medium pages, in-place run growth through memory.grow,
eighth-of-heap growth step, retired pages released before growth). Median ns per operation; the
full tables including floors, mimics and lol_alloc are in results/REPORT.md.

### node 22 (V8 12.4), optimizing tier

| workload | freelist floor | sizeclass floor | wasmalloc | dlmalloc | talc |
|---|---:|---:|---:|---:|---:|
| alloc_free_32 | 1.80 | 1.87 | 2.54 | 7.98 | 11.73 |
| alloc_free_32_align16 | 1.80 | 1.87 | 2.50 | 13.77 | 11.74 |
| batch_lifo_32 | 1.89 | 1.91 | 2.97 | 9.04 | 10.07 |
| batch_fifo_32 | 1.88 | 1.98 | 2.99 | 12.47 | 10.02 |
| churn | 3.14 | 3.69 | 7.15 | 60.68 | 31.92 |
| random_actions | 19.15 | 22.06 | 14.61 | 43.30 | 22.50 |
| random_actions_norealloc | 6.27 | 9.15 | 10.93 | 37.64 | 19.56 |
| vec_push_growth | 408,229 | 441,256 | 387,948 | 386,173 | 385,937 |
| realloc_doubling | 35,940 | 42,533 | 638.69 | 66.19 | 81.49 |
| large_alloc_free | 2,181 | 2,428 | 2,063 | 1,867 | 2,467 |

### node 22, Liftoff only

| workload | freelist floor | sizeclass floor | wasmalloc | dlmalloc | talc |
|---|---:|---:|---:|---:|---:|
| alloc_free_32 | 3.78 | 3.71 | 3.68 | 10.99 | 18.97 |
| alloc_free_32_align16 | 3.76 | 3.68 | 3.66 | 19.71 | 18.67 |
| batch_lifo_32 | 3.66 | 3.78 | 4.85 | 12.54 | 18.62 |
| batch_fifo_32 | 3.79 | 3.39 | 4.85 | 17.89 | 18.58 |
| churn | 5.46 | 6.23 | 9.89 | 69.16 | 38.58 |
| random_actions | 24.49 | 22.87 | 19.01 | 51.49 | 27.45 |
| random_actions_norealloc | 7.76 | 10.15 | 14.39 | 44.73 | 24.32 |
| vec_push_growth | 1,122,087 | 1,131,780 | 1,105,387 | 1,102,637 | 1,100,050 |
| realloc_doubling | 35,999 | 54,493 | 786.68 | 112.08 | 155.39 |
| large_alloc_free | 2,256 | 2,360 | 1,843 | 1,953 | 2,305 |

### d8 (V8 15.2), optimizing tier

| workload | freelist floor | sizeclass floor | wasmalloc | dlmalloc | talc |
|---|---:|---:|---:|---:|---:|
| alloc_free_32 | 0.55 | 0.56 | 1.13 | 4.09 | 8.43 |
| alloc_free_32_align16 | 0.55 | 0.56 | 1.13 | 7.85 | 8.43 |
| batch_lifo_32 | 0.93 | 0.95 | 1.88 | 4.37 | 6.54 |
| batch_fifo_32 | 0.92 | 0.94 | 1.88 | 5.92 | 6.12 |
| churn | 10.57 | 3.34 | 6.45 | 55.75 | 26.06 |
| random_actions | 21.03 | 20.76 | 14.24 | 40.66 | 20.08 |
| random_actions_norealloc | 5.90 | 8.34 | 10.17 | 34.10 | 17.85 |
| vec_push_growth | 459,700 | 426,950 | 390,250 | 387,500 | 387,150 |
| realloc_doubling | 37,660 | 45,790 | 619.99 | 39.99 | 59.99 |
| large_alloc_free | 2,180 | 2,300 | 2,060 | 2,180 | 2,480 |

### d8 (V8 15.2), Liftoff only

| workload | freelist floor | sizeclass floor | wasmalloc | dlmalloc | talc |
|---|---:|---:|---:|---:|---:|
| alloc_free_32 | 4.03 | 4.09 | 4.62 | 18.61 | 23.45 |
| alloc_free_32_align16 | 4.07 | 4.08 | 4.62 | 29.03 | 23.43 |
| batch_lifo_32 | 4.20 | 4.19 | 5.32 | 19.95 | 21.80 |
| batch_fifo_32 | 4.19 | 4.26 | 5.35 | 25.44 | 21.56 |
| churn | 6.37 | 6.95 | 10.90 | 74.55 | 42.91 |
| random_actions | 27.03 | 32.18 | 20.25 | 55.19 | 30.65 |
| random_actions_norealloc | 8.27 | 10.73 | 14.85 | 48.37 | 27.09 |
| vec_push_growth | 1,132,150 | 1,174,350 | 1,105,750 | 1,117,250 | 1,101,000 |
| realloc_doubling | 37,410 | 41,330 | 799.99 | 139.99 | 169.99 |
| large_alloc_free | 2,080 | 3,140 | 1,840 | 1,980 | 2,260 |

### JavaScriptCore

| workload | freelist floor | sizeclass floor | wasmalloc | dlmalloc | talc |
|---|---:|---:|---:|---:|---:|
| alloc_free_32 | 0.54 | 0.55 | 1.00 | 4.67 | 10.15 |
| alloc_free_32_align16 | 0.55 | 0.55 | 1.01 | 10.31 | 10.15 |
| batch_lifo_32 | 0.93 | 0.94 | 1.35 | 4.72 | 8.62 |
| batch_fifo_32 | 0.93 | 0.93 | 1.35 | 9.07 | 8.59 |
| churn | 3.31 | 3.27 | 6.12 | 52.94 | 25.97 |
| random_actions | 20.50 | 19.50 | 13.58 | 40.11 | 19.37 |
| random_actions_norealloc | 5.96 | 8.02 | 9.94 | 34.51 | 17.09 |
| vec_push_growth | 666,479 | 653,137 | 444,861 | 584,460 | 584,656 |
| realloc_doubling | 41,892 | 37,241 | 676.25 | 80.54 | 65.89 |
| large_alloc_free | 2,944 | 2,471 | 2,212 | 2,417 | 2,246 |

### wasmtime (Cranelift)

| workload | freelist floor | sizeclass floor | wasmalloc | dlmalloc | talc |
|---|---:|---:|---:|---:|---:|
| alloc_free_32 | 0.74 | 0.73 | 1.10 | 4.72 | 8.27 |
| alloc_free_32_align16 | 0.74 | 0.73 | 1.10 | 5.43 | 8.27 |
| batch_lifo_32 | 1.06 | 1.04 | 1.52 | 5.04 | 7.46 |
| batch_fifo_32 | 1.09 | 1.07 | 1.81 | 6.83 | 7.25 |
| churn | 3.00 | 3.21 | 6.07 | 48.32 | 26.22 |
| random_actions | 19.08 | 20.08 | 14.26 | 38.33 | 20.22 |
| random_actions_norealloc | 5.96 | 8.21 | 10.25 | 32.81 | 17.52 |
| vec_push_growth | 412,188 | 414,641 | 561,796 | 381,888 | 563,074 |
| realloc_doubling | 35,795 | 39,489 | 600.90 | 39.40 | 60.90 |
| large_alloc_free | 2,665 | 2,565 | 2,397 | 2,390 | 2,392 |

### Footprint: memory.size in 64 KiB pages after one workload (fresh process, 20 pages at start)

| workload | wasmalloc | dlmalloc | talc |
|---|---:|---:|---:|
| alloc_free_32 | 36 | 21 | 21 |
| alloc_free_32_align16 | 36 | 21 | 21 |
| batch_lifo_32 | 36 | 21 | 21 |
| batch_fifo_32 | 36 | 21 | 21 |
| churn | 132 | 104 | 121 |
| random_actions | 68 | 32 | 34 |
| random_actions_norealloc | 84 | 38 | 41 |
| vec_push_growth | 68 | 54 | 54 |
| realloc_doubling | 68 | 54 | 54 |
| large_alloc_free | 132 | 149 | 149 |

