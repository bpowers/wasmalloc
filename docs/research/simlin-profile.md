# Profile of simlin's C-LEARN compile under wasmalloc

Measured 2026-09-02 on the reference machine (Ryzen 9 9950X, Linux 7.1.7) against
simlin at `third_party/simlin` branch `wasmalloc` (e6070855, PR bpowers/simlin#1037,
wasmalloc 0.1.0 from crates.io, byte-identical to `src/` on main) and its `main`
(5c406dd5, std's dlmalloc). Workload: the `compile` stage of
`src/engine/bench/clearn-alloc.mjs` (salsa compile plus `Vm::new`) on C-LEARN v77 with
Loops That Matter, through the public `@simlin/engine` API, one fresh instance per
iteration. Engines: node 24.20.0 (V8 13.6.233) and node 22.22.2 (V8 12.4.254). Every
number below names the command that produced it (section 9); the scratch scripts live
under `/tmp/claude-1000/-home-bpowers-src-wasm-clalloc/*/scratchpad/prof/`.

Contents

1. Summary
2. Method: bundles with names, profiler, counters
3. Allocator share of the compile stage
4. Slow-path frequency and per-call cost
5. Codegen of the fast paths in situ
6. Realloc in situ
7. Large zeroed buffers and memory.grow
8. Ranked changes for the tuning engineer
9. Commands

## 1. Summary

- On node 24 wasmalloc's own functions take 115 ms of the 1925 ms compile stage
  (6.0 percent of the profiled run; the un-profiled bench reads 1877 to 1902 ms). V8
  inlines the alloc fast path into callers covering 18 percent of allocations, which
  hides at most another 6 ms. With V8's wasm inlining switched off the allocator is
  165 ms of 2270 ms (7.3 percent); on node 22, which inlines nothing, it is 167 ms of
  2298 ms (7.3 percent). The other 91 to 94 percent of compile is simlin's own code:
  hashing (SipHash and FxHash, 113 ms), identifier canonicalisation and interning
  (97 ms), the lexer and parser, AST clones and drops.
- dlmalloc's functions take 485 ms of its 2642 ms compile (18.4 percent) and its 2844
  `memory.grow` calls cost another 203 ms of V8 garbage-collector time (7.7 percent;
  `memory.grow` still costs 80 to 110 us on V8 13.6). Of the 717 ms the compile stage
  gained by switching allocators, 370 ms is allocator code, 199 ms is `memory.grow`
  and GC, and 148 ms is callers running faster (fewer, cheaper calls and a denser heap).
- Slow paths are rare: `alloc_generic` is entered on 0.66 percent of allocations (1 in
  152), `dealloc_transition` on 0.13 percent of frees (1 in 780). `alloc_generic` costs
  156 ns per entry, 33 ms per compile, of which 26 ms is `page::extend` touching fresh
  memory for the first time (690 ns per 8 KiB extension): the page-fault cost of a
  290 MiB heap, which dlmalloc pays in `malloc` instead. Three quarters of the
  `alloc_generic` entries are allocations of 1 to 40 KiB, which bypass the direct
  table by design and would be served by a queue-head lookup.
- The fast paths compile to about 30 x86 instructions each and cost 1.0 to 1.3 ns per
  call on node 24 (2.2 and 1.3 ns on node 22). A third of those instructions are V8's
  frame, stack check and jump-table call, paid because simlin builds at `opt-level =
  "z"`, where LLVM keeps `__rust_alloc` and `__rust_dealloc` out of line (83 and 350
  call sites) and V8's feedback-driven inliner only inlines the alloc path at a few
  hot sites and the dealloc path nowhere hot. The same `-Oz` costs simlin 24 to 37
  percent of its whole compile: the bundle built at `-Os` compiles in 1428 ms and at
  `-O3` in 1182 ms against 1888 ms, for a 60 to 86 percent larger binary.
- `realloc` is 1.43 M calls per compile, 99.4 percent of them growths, 69 percent exact
  doublings, 63 percent from blocks of at most 40 bytes; 0.6 percent stay in place and
  206 MB is copied. It costs 15 ns per call including the copy, 22 ms per compile. The
  doubling chains above 2 KiB (35 k per compile) are 0.5 percent of compile; no bin
  spacing helps a request that doubles, and the copy is cheap (2.7 ns for 145 bytes,
  15 ns for 4 KiB). Two out-of-line `bins::classify` calls with results returned through
  the shadow stack are the avoidable part, about 6 to 8 ms.
- `alloc_zeroed` skips the memset for the 58 MiB `Vm::new` buffer (served from fresh
  slices), as designed; dlmalloc memsets 63.6 MB per compile for the same requests.
- Expected gains from allocator changes are small: the whole ranked list in section 8
  adds up to 20 to 35 ms (1 to 2 percent of compile). The hot-block cache from the
  roadmap is assessed at 0 to 15 ms on this workload, because 99.3 percent of
  allocations already pop the most recently freed block of their class from the direct
  page, and the per-call cost is dominated by the out-of-line call, which the cache
  does not remove.

## 2. Method

### 2.1 Bundles with a name section

The shipped bundles are stripped (`strip = true`), so a profile shows `wasm-function[N]`.
Both branches were rebuilt with `CARGO_PROFILE_RELEASE_STRIP=debuginfo` (drops DWARF,
keeps the `name` section) into scratch target directories, optimised with the same
`wasm-opt -O3` flags as `build.sh` plus `-g`, and demangled with `wasm-tools demangle`
[C1, C2]. The `strip` setting changes LLVM's symbol hashes and with them a handful of
merge decisions, so the code is not byte-identical to the shipped bundle: the
wasm-opt'd code section is 4,986,102 bytes and 12,252 functions against 4,984,488 and
12,234 for the shipped wasmalloc bundle (0.03 percent), 4,978,710 and 12,226 against
4,977,104 and 12,208 for dlmalloc. Timing is unchanged [C3]:

| bundle | compile median | compile min | run | total |
|---|--:|--:|--:|--:|
| dlmalloc shipped | 2602 | 2589 | 1763 | 4530 |
| dlmalloc with names (`dl.wasm`) | 2613 | 2593 | 1812 | 4595 |
| wasmalloc shipped | 1902 | 1896 | 1756 | 3785 |
| wasmalloc with names (`wa.wasm`) | 1893 | 1893 | 1787 | 3806 |

ms, node 24, LTM on, 3 iterations after 1 warm-up, `taskset -c 4-7`.

### 2.2 Profiler

`profile-compile.mjs` [C4] drives `Project.openVensim`, `model.simulate({}, {enableLtm:
true, engine: 'vm'})` and dispose exactly as the bench does, on a fresh instance per
iteration, and starts V8's sampling profiler (`node:inspector` `Profiler.start`, 100 us
interval) immediately before the compile stage and stops it immediately after, so the
samples cover that stage only. Ten measured iterations follow two warm-ups; samples are
aggregated per call frame (self and inclusive). `report.mjs` [C5] groups frames by
allocator function and prints the tables in section 3. The profiler adds about 1.5
percent to the stage time (1925 ms profiled against 1893 to 1902 unprofiled).

V8 attributes a sample to the function whose optimised code contains the PC, so code
that TurboFan inlined is charged to the caller. Section 3.3 quantifies this with a run
under `--no-wasm-inlining` and with a scan of the generated code.

### 2.3 Exact counts

`instrument.mjs` [C6] rewrites the names bundle at the text level (like the bench's
`--count-grows`): every entry of a listed function bumps an exported `i64` global,
every `call` inside `alloc_generic`, `dealloc_transition`, `dealloc_generic`,
`__rust_realloc` and `__rust_alloc_zeroed` gets a per-site counter, every call of the
four `__rust_*` shims gets a per-caller counter, `memory.fill`/`memory.copy` inside the
allocator go through a hook that counts calls and bytes, `memory.grow` through a hook
that counts calls and pages, and the shim entries bucket their size arguments. No
function index moves. `profile-compile.mjs --counters --noprofile` reads the globals
after the compile stage; counts are per iteration (the instance is fresh each time) and
were identical across iterations.

### 2.4 Machine code

`node --no-liftoff --print-wasm-code` on the driver with synchronous
`new WebAssembly.Module`/`Instance` (the async path never settles under that flag)
dumps every function TurboFan compiles [C7]. A second dump with default tiering shows
the Liftoff and the TurboFan version of every function that tiers up during the
pipeline [C8]; its TurboFan code reflects the inlining decisions V8 makes with Liftoff
call-count feedback, which differ from the `--no-liftoff` dump (section 5.6), so the
tiered dump is the one that describes the bench run. `wasm-tools print` of the names
bundle gives the wasm text with function indices (`fn.sh` extracts one function,
`asmfn.sh` one function's machine code).

## 3. Allocator share of the compile stage

### 3.1 node 24 (V8 13.6), per compile iteration [C4, C5]

wasmalloc bundle: median compile 1925 ms, 127,431 samples, 99.78 percent in wasm, 0.20
percent GC, 0.02 percent JS.

| function | self ms | self % | incl. ms | notes |
|---|--:|--:|--:|---|
| `__rust_alloc` | 34.2 | 1.78 | 45.3 | fast path; 82 percent of allocations enter it out of line |
| `__rust_dealloc` | 29.5 | 1.53 | 29.7 | fast path; every free enters it |
| `__rust_realloc` | 16.4 | 0.85 | 21.9 | `Heap::realloc` inlined; incl. adds the copy and the callees |
| `__rust_alloc_zeroed` | 0.4 | 0.02 | 0.6 | |
| `Heap::alloc_generic` | 31.4 | 1.63 | 33.5 | `find_page`, `fresh_page`, `page::init` inlined by wasm-opt, `page::extend` by V8 |
| `Heap::dealloc_transition` | 0.4 | 0.02 | 0.6 | |
| `Heap::acquire_run` | 1.5 | 0.08 | 1.5 | slice search, release before grow, `memory.grow` |
| queue ops, `collect_retired`, `alloc_huge`, `slices::*` | 0.8 | 0.04 | 0.8 | |
| **wasmalloc total** | **114.6** | **5.95** | | |
| `(garbage collector)` | 3.9 | 0.20 | | 37 `memory.grow` calls |
| std wrappers: `RawVecInner::*`, `Global::*`, `exchange_malloc` | 89.3 | 4.64 | | present with any allocator |
| `memcmp` | 31.6 | 1.64 | | simlin's string compares, not allocation |

dlmalloc bundle: median compile 2642 ms, 155,735 samples, 92.31 percent wasm, 7.67
percent GC.

| function | self ms | self % | incl. ms |
|---|--:|--:|--:|
| `Dlmalloc::malloc` | 238.0 | 9.01 | 239.1 |
| `Dlmalloc::free` | 194.3 | 7.35 | 194.3 |
| `__rdl_dealloc` | 31.5 | 1.19 | 153.3 |
| `__rdl_realloc` | 11.4 | 0.43 | 51.8 |
| `__rust_alloc_zeroed` | 7.6 | 0.29 | 8.0 |
| `__rdl_alloc`, other `Dlmalloc::*` | 2.5 | 0.09 | |
| **dlmalloc total** | **485.3** | **18.37** | |
| `(garbage collector)` | 202.8 | 7.67 | 2844 `memory.grow` calls |
| std wrappers | 90.2 | 4.66 | |

### 3.2 node 22 (V8 12.4) [C4, C5]

| bundle | compile ms | allocator self ms | share | GC ms | largest frames |
|---|--:|--:|--:|--:|---|
| wasmalloc | 2298 | 167.2 | 7.28% | 4.4 | `__rust_alloc` 70.8, `__rust_dealloc` 40.2, `page::extend` 25.6, `__rust_realloc` 17.4, `alloc_generic` 5.3, `bins::classify` 4.3 |
| dlmalloc | 3031 | 536.5 | 17.70% | 186.4 | `malloc` 223.9, `free` 136.7, `unlink_chunk` 54.8, `__rdl_dealloc` 52.5, `insert_large_chunk` 34.9 |

V8 12.4 inlines no wasm function into another, so `page::extend` and `bins::classify`
appear as their own frames and every shim call is out of line. `__rust_alloc` costs
2.17 ns per call there against 1.28 ns for the out-of-line calls on node 24.

### 3.3 What inlining hides on node 24

V8 13.6 inlines wasm callees into wasm callers using Liftoff call counts. In the
tiered dump [C8], the alloc fast path (recognisable by its direct-table load,
`[...+0x15fb04]`) appears inside 64 TurboFan functions and the dealloc fast path (its
`free_is_zero` store, `movb [...+0x16],0x0`) inside 100; joined with the per-caller
call counts [C6, C9]:

| shim | calls per compile | callers | calls through V8-inlined copies | hot inlined callers |
|---|--:|--:|--:|---|
| `__rust_alloc` | 32,639,544 | 78 | 5,977,410 (18.3%) | `String::clone` 3.84 M, `str::to_lowercase` 1.05 M, hashbrown `new_uninitialized` 0.52 M |
| `__rust_alloc_zeroed` | 141,114 | 4 | 0 | |
| `__rust_dealloc` | 30,960,207 | 312 | 0 (three cold callers) | none of the top sites (`RawVecInner::deallocate` 10.98 M, `Global::deallocate` 6.19 M, `Box<Expr3>::drop` 4.39 M, `Box<Expr1>::drop` 2.64 M) |

So the hidden fast-path time is bounded by 6 M inlined allocations at about 1 ns, some
6 ms, and the real allocator share on node 24 is 115 to 121 ms (6.0 to 6.3 percent).
Two cross-checks: with `--no-wasm-inlining` [C10] the compile takes 2270 ms and the
allocator 165.2 ms (`__rust_alloc` 70.2, `__rust_dealloc` 40.8, `page::extend` 26.4,
`__rust_realloc` 14.3, `alloc_generic` 5.7, `classify` 4.0), the same 165 ms node 22
shows; and the caller-by-caller difference between the two bundles (section 3.4) sums
to 148 ms, which cache effects and shorter call chains explain without a hidden
allocator component. The `--no-liftoff` dump [C7] is not representative: without
feedback V8 inlines the alloc path into 384 functions covering 53.7 percent of
allocations and the dealloc path into 223.

Note the size of the inlining effect itself: V8's wasm inlining is worth 15 percent of
simlin's whole compile (2270 to 1925 ms). Small shims are inlinable shims.

### 3.4 Where the 717 ms went (node 24, dlmalloc minus wasmalloc, per compile)

| component | dlmalloc | wasmalloc | difference |
|---|--:|--:|--:|
| allocator functions' self time | 485 | 115 | 370 |
| `(garbage collector)` driven by `memory.grow` (2844 against 37 calls) | 203 | 4 | 199 |
| everything else | 1954 | 1806 | 148 |
| compile stage (profiled) | 2642 | 1925 | 717 |

The 148 ms is spread over simlin's own frames: `Expr0::clone` +15.5 ms, the
`String as fmt::Write` shim +13.9, `BuiltinVisitor::walk` +5.9, `CanonicalStorage::intern`
+5.6, `canonicalize` +5.4, `Expr0` drop glue +5.3, `memcmp` +4.7 [C5 diff table]. Two
effects: dlmalloc's `__rdl_alloc` is inlined into callers and `Dlmalloc::malloc` is not,
so the call chain is one deeper than wasmalloc's, and dlmalloc's 8-byte boundary tags
and unsorted free lists spread live objects over more cache lines. `dispose` shows the
same: 71 ms against 37.

### 3.5 What is not ours

The largest self-time frames in the wasmalloc bundle on node 24, ms per compile [C5]:
`FxHasher::write_str` 53.8, `common::canonicalize` 50.4, `CanonicalStorage::intern`
46.6, `Lexer::next` 46.1, `BuiltinVisitor::walk` 38.7 (222 inclusive),
`Expr0::clone` 36.8, `DefaultHasher::write` 36.6 plus `finish` 22.3 (SipHash-1-3 in std's
`HashMap`), `Context::lower_from_expr3` 35.3, `ast::needs_quoting` 33.7,
`RawVecInner::deallocate` 32.1, `memcmp` 31.6, `Pass1Context::transform_inner` 30.6,
`Expr2::from` 26.0, `Expr3::from_expr2` 23.9, `RawVecInner::try_allocate_in` 23.1,
`CharIndices::next` 20.3. Hashing alone is 113 ms, more than the allocator. Whatever the
allocator does, 1750 ms of the 1877 ms stays.

The one build-level number that dwarfs everything allocator-side: the same wasmalloc
branch built at `-Os` and `-O3` (RUSTFLAGS overriding the `-C opt-level=z` in simlin's
`.cargo/config.toml`, same `wasm-opt -O3`) [C11]:

| opt-level | compile ms | run ms | total ms | stripped bundle bytes |
|---|--:|--:|--:|--:|
| z (shipped) | 1888 | 1765 | 3769 | 5,376,435 |
| s | 1428 | 1333 | 2870 | 8,568,519 |
| 3 | 1182 | 1320 | 2603 | 10,025,106 |

## 4. Slow-path frequency and per-call cost

Counts per compile iteration, wasmalloc bundle, node 24 [C6]; costs from the node 24
profile (inclusive ms divided by count) and, in the last column, node 22 [C4].

| path | entries | rate | node 24 ns per entry | node 22 |
|---|--:|--:|--:|--:|
| `__rust_alloc` | 32,639,544 | | 1.28 (out-of-line calls only; 2.15 with inlining off) | 2.17 |
| `__rust_dealloc` | 30,960,207 | | 0.95 (1.32 with inlining off) | 1.30 |
| `__rust_realloc` (incl. copy and callees) | 1,432,286 | | 15.3 | 18.4 |
| `__rust_alloc_zeroed` | 141,114 | | 4.3 | 4.3 |
| `Heap::alloc_generic` (incl. `extend`) | 215,301 | 1 in 152 allocs | 156 | 156 |
| of which `page::extend` | 38,038 | | about 690 (26 ms) | 673 |
| `Heap::dealloc_transition` | 39,696 | 1 in 780 frees | 15 | 18 |
| `Heap::dealloc_generic` | 2,954 | | | |
| `bins::classify` (out of line at `-Oz`) | 3,082,827 | 2 per realloc, 1 per generic | inlined by V8 | 1.4 |
| `acquire_run` (fresh pages and runs) | 6,155 | | 240 | 260 |
| `collect_retired` | 6,216 | | | |
| `free_page` | 3,091 | | | |
| `memory.grow` | 37 (4,648 pages, 290.5 MiB) | | about 100,000 (section 7) | |

Why `alloc_generic` is entered (per-site counters inside it; the reasons overlap):

| event | count | mechanism |
|---|--:|---|
| entries from `__rust_alloc` for sizes 1025 to 40960 B | 98,364 | `size > DIRECT_MAX_SIZE` goes straight to the generic path |
| entries from `__rust_realloc` for new sizes above 1 KiB | 64,546 | same, on the move path |
| entries from `__rust_realloc`, all sizes | 67,078 | |
| entries from `__rust_alloc_zeroed` | 293 | |
| first page of the queue is full: `move_to_full` (`remove` + `push_back`) | 35,490 | then the next page, now first, has room |
| first page has an empty list but unextended blocks: `page::extend` | 32,037 | |
| no page has room: `fresh_page` (`collect_retired`, `acquire_run`, `push_front`, `extend`) | 6,001 | 6,001 pages of 64 or 256 KiB per compile |
| a later page chosen over the first: `move_to_front` | 1,244 | |
| huge request (`alloc_huge`) | 92 | |
| periodic `collect_retired` (every 1000 entries) | 215 | |

Three quarters of the entries (163 k) are requests above 1 KiB whose bin queue's first
page had a free block; they pay the classify call, the generic counter, the candidate
walk and the `retire_expire` store for nothing the direct table could not have done
with a `bin()` computation. At roughly 35 ns of non-extend work per entry (node 22:
5.3 ms self over 215 k) that is 6 ms per compile.

`page::extend` is the cost of first touch: 38,038 extensions of up to 8 KiB each link
at most 311 MB of blocks, which is the heap's footprint (292 MiB); 690 ns per 8 KiB is
two 4 KiB page faults plus 128 line stores. dlmalloc pays the same faults inside
`malloc` when it carves its top chunk. No allocator avoids it below the peak live size
(300 MiB here).

Why `dealloc_transition` is entered: 32,786 frees into a page parked in the full queue
(`unfull`: `remove` + `push_back`), 2,980 frees that emptied a page with more than three
pages in its queue (`free_page`), 3,930 that retired a page. `dealloc_generic`: 2,857
frees of medium (10 to 40 KiB) blocks and 97 frees of runs.

Size mix of the compile stage on wasm32 (the native harness's histogram is of 64-bit
sizes; pointers halve on wasm32, so the native 56-byte class is the 32-byte class here
and the native 128-byte class the 65 to 96 class) [C6]:

| size (B) | allocs | share | frees |
|---|--:|--:|--:|
| 1 to 8 | 7,188,367 | 22.0% | 6,739,004 |
| 9 to 16 | 3,801,783 | 11.6% | 3,634,699 |
| 17 to 24 | 2,418,968 | 7.4% | 2,240,789 |
| 25 to 32 | 6,225,015 | 19.1% | 5,457,984 |
| 33 to 56 | 1,282,986 | 3.9% | 1,093,173 |
| 57 to 64 | 3,312,252 | 10.1% | 3,279,568 |
| 65 to 96 | 6,167,982 | 18.9% | 6,183,113 |
| 97 to 128 | 539,031 | 1.7% | 672,620 |
| 129 to 256 | 685,965 | 2.1% | 700,550 |
| 257 to 1024 | 918,743 | 2.8% | 848,546 |
| 1025 to 10240 | 97,945 | 0.30% | 108,541 |
| 10241 to 40960 | 419 | | 1,541 |
| above 40960 | 88 | | 79 |

95 percent of allocations are at most 128 bytes and 99.7 percent at most 1 KiB; 54
requests per compile have alignment above 8. The hottest call sites [C6]:

| callee | caller | calls |
|---|---|--:|
| `__rust_alloc` | `alloc::Global::alloc_impl_runtime` (three copies) | 18,557,229 |
| | `alloc::boxed::box_new_uninit` | 4,734,449 |
| | `String::clone` | 3,842,486 |
| | `Box<Expr0>::new_uninit_in` | 1,519,025 |
| | `str::to_lowercase` | 1,054,996 |
| | `Box<str>::from(&str)` | 878,346 |
| | `Box<Expr2>::new_uninit_in` | 751,624 |
| `__rust_dealloc` | `RawVecInner::deallocate` | 10,980,898 |
| | `Global::deallocate` | 6,187,554 |
| | `Box<Expr3>` drop | 4,390,874 |
| | `Box<Expr1>` drop | 2,643,752 |
| | `Box<Expr2>` drop | 1,396,911 |
| `__rust_realloc` | `RawVecInner::finish_grow` (two copies) | 1,403,538 |
| `__rust_alloc_zeroed` | `Global::alloc_impl_runtime` | 141,109 |

## 5. Codegen of the fast paths in situ

All listings are node 24 TurboFan code from the tiered dump [C8] unless stated; sizes
in bytes are the `Body` sizes V8 prints. rbx (or r11, r15) holds the linear-memory
base, loaded from the instance in rsi; r13 is V8's root register (stack limit at
`[r13-0x60]`); the static heap's direct table sits at linear address 0x15fb04, its
queues at 0x15b7e8.

| function | wasm instructions | TurboFan bytes (tiered) | TurboFan bytes (`--no-liftoff`) | Liftoff bytes |
|---|--:|--:|--:|--:|
| `__rust_alloc` | 55 | 256 | 256 | 384 |
| `__rust_dealloc` | 61 | 320 | 704 (with `dealloc_transition` inlined) | 384 |
| `__rust_alloc_zeroed` | 71 | 448 | 448 | 576 |
| `__rust_realloc` | 457 | 1984 | 4608 | 2496 |
| `Heap::alloc_generic` | 432 | 3136 | 4992 | 2304 |
| `Heap::dealloc_transition` | 78 | 576 | 512 | 512 |
| `Dlmalloc::malloc` | 2122 | | 9984 | |
| `Dlmalloc::free` | 363 | | 3136 | |
| `__rdl_dealloc` | 44 | | 448 | |
| `__rdl_realloc` | 446 | | 4416 | |

### 5.1 `__rust_alloc` (index 799), 65 instructions, hot path 30

```
push rbp; mov rbp,rsp; push 8; push rsi; sub rsp,0x18     frame: 5 instructions, 32 bytes of stack
mov rbx,[rsi+0x1f]                                        memory base (load)
cmp rsp,[r13-0x60]; jna stack_overflow                    stack check (load, branch)
cmp edx,8; jna @size                                      align <= 8?
  cmp edx,0x1000; jna @round                              align <= 4096? else call alloc_generic
  @round: lea edi,[rax+rdx-1]; mov r8d,edx; neg r8d; and r8d,edi   size = round_up(size, align)
@size: cmp edi,0x401; jnc @generic                        size <= 1024?
add edi,7; shr edi,1; and edi,0x7ffffffc                  direct index * 4
mov edi,[rbx+rdi+0x15fb04]                                page = direct[idx]          (load 1)
mov r8d,[rbx+rdi]                                         block = page.free           (load 2)
test r8d,r8d; jnz @pop                                    empty? (falls into the cold call below)
@generic: xor ecx,ecx; mov r10d,eax; mov eax,edx; mov edx,r10d; call alloc_generic; jmp @ret
@pop: mov r9d,[rbx+r8]                                    next = block.next           (load 3)
mov [rbx+rdi],r9d                                         page.free = next            (store 1)
mov r9d,[rbx+rdi+4]; add r9d,1; mov [rbx+rdi+4],r9d       page.used += 1              (load 4, store 2)
mov eax,r8d; jmp @ret
@ret: mov rsp,rbp; pop rbp; ret
```

Hot path: 30 instructions, 4 data loads plus 2 frame loads (memory base, stack limit),
2 stores, 4 conditional branches plus 2 unconditional jumps, 0 calls. The allocator's
own work is the 12 instructions from the index computation to `used += 1`; the other 18
are V8's frame, stack check, the two `Layout` tests and the return. The cold call is
laid out inline between the empty test and the pop (V8 does no hot/cold splitting);
both `alloc_generic` calls share the epilogue. Node 22 emits the same 55-instruction
shape (`asm/wa-799-n22.txt`), folding the page address into rbx before the `used`
update, and costs 2.17 ns per call against 1.28 here.

### 5.2 `__rust_dealloc` (index 792), 72 instructions, hot path 33

```
push rbp; mov rbp,rsp; push 8; push rsi; sub rsp,0x20; mov rbx,[rsi+0x1f]; mov [rbp-0x30],rbx   frame + spill of the base
cmp rsp,[r13-0x60]; jna                                   stack check
cmp ecx,0x1001; jnc @generic                              align > 4096 -> dealloc_generic
lea edi,[rcx+rdx-1]; mov r8d,ecx; neg r8d; and r8d,edi    rounded = round_up(size, align)
mov edi,edx; cmp ecx,8; cmova edi,r8d                     size = align > 8 ? rounded : size   (the select)
cmp edi,0x2800; ja @generic                               > 10240 -> dealloc_generic
mov edi,eax; and edi,0xffff0000                           page = ptr & !(64 KiB - 1)
mov r8d,[rbx+rdi+4]                                       used                        (load 1)
mov r9d,[rbx+rdi]                                         free                        (load 2)
mov r11d,eax; mov [rbx+r11],r9d                           block.next = free           (store 1)
movb [rbx+rdi+0x16],0                                     free_is_zero = false        (store 2)
lea r9d,[r8-1]; mov [rbx+rdi+4],r9d                       used - 1                    (store 3)
mov [rbx+rdi],eax                                         free = block                (store 4)
mov [rbp-0x20],rdi                                        spill page for the cold path
add r8d,0xff; jnz @flags                                  used - 1 == 0? (folded into the flags of the add)
  movzx r8d,[rbx+rdi+0x1a]; cmpb [rbx+rdi+0x1a],0; jz @transition    retire_expire == 0?  (two accesses of one byte)
@flags: movzx r8d,[rbx+rdi+0x19]; cmpb [rbx+rdi+0x19],0; jnz @transition   flags != 0?   (two accesses)
mov rsp,rbp; pop rbp; ret
@transition: mov eax,edi (page); call dealloc_transition; ...
```

Hot path: 33 instructions, 5 data loads (`used`, `free`, `flags` twice) plus 2 frame
loads, 4 data stores plus 1 spill, 4 conditional branches. Observations, each with its
cost on this workload:

- The alignment `select` is 7 ALU instructions (`lea, mov, neg, and, mov, cmp, cmova`)
  executed on every free for the 54 aligned requests per compile. Under 0.1 ns.
- `free_is_zero` is one byte store per free, the second of four stores to the same
  header line; roofline 12.1 measured it at nothing on the pair, and here it sits in a
  store buffer that is never the bottleneck. 31 M stores per compile, at most 3 ms.
- `used` is a load, a decrement and a store, then the zero test is taken from the
  decrement's flags; nothing to remove.
- The flags/retire test costs two byte loads because V8 emits `movzx r,[m]` followed by
  `cmpb [m],0` for `i32.load8_u; i32.eqz` and keeps the dead `movzx`. Both hit L1.
  Same pattern in `__rust_alloc_zeroed` at `free_is_zero`.
- The page address is spilled to `[rbp-0x20]` on every free although it is dead on the
  hot path (only the transition call needs it): TurboFan spills at the definition.
- No bounds checks: V8 uses the guard-region trap handler ("Protected instructions"
  lists the 28 memory accesses).

### 5.3 `__rust_alloc_zeroed` (804) and `__rust_realloc` (803)

`__rust_alloc_zeroed` is `__rust_alloc` plus `free_is_zero` (two accesses) and, when the
page is dirty, `memory.fill`, which compiles to an external C call (`call rax` with the
`[r13+0xa0]` frame marker dance, 12 instructions). 140,813 of the 141,114 zeroed
requests per compile take the fill (the page is dirty), 133,171 of them for 1 to 8
bytes; the fill itself costs 2.2 to 2.8 ns for sizes to 512 bytes on both engines [C12],
so the whole function is 0.6 ms per compile. Not worth an inline-store path.

`__rust_realloc` is 457 wasm instructions and 1984 bytes of TurboFan code with the
`Heap::realloc` logic, one inlined copy of the alloc fast path and one of the dealloc
fast path, and 9 calls: `bins::classify` twice, `try_extend` twice, `all_set`,
`add_region`, `release`, `alloc_huge`, `alloc_generic`, `dealloc_transition`,
`dealloc_generic`. At `-Oz` LLVM compiled `classify` (an `#[inline] const fn`) out of
line and returns its 2-byte `Class` through an sret slot on the shadow stack:
`__rust_realloc` decrements `__stack_pointer` by 16, calls `classify` twice, reloads the
class bytes (`movzx r8,[r15+rdi+0x9]`, `movzx rcx,[r15+rdi]`), and TurboFan inlines the
two calls (the `lzcnt` sequences at +0x99 and +0x159) but cannot remove the store-load
round trip through linear memory. The common path then compares bins, takes the
direct-table pop, `memory.copy`, and the dealloc fast path. 15.3 ns per call including
the copy; the shadow-stack frame and the two sret round trips are perhaps 4 ns of it.

### 5.4 Hottest callers (what a call costs at `-Oz`)

`RawVecInner::try_allocate_in` (11360; 23.1 ms self, 1.20 percent): 205 TurboFan
instructions. Its wat calls `Global::alloc_impl_runtime` (11356), which calls
`__rust_alloc`; V8 inlined both into it in the `--no-liftoff` dump (two copies of the
fast path, one for `alloc`, one for `alloc_zeroed`, at +0xb1 and +0x15b) but in the
tiered dump the site stays a call. The function spills six registers around the call
(`[rbp-0x18]` to `[rbp-0x50]`) and reloads them after it.

`RawVecInner::deallocate` (11357/11488; 32.1 ms self, 1.67 percent, 10.98 M calls): 69
instructions; calls `current_memory` (inlined; returns through a 16-byte shadow-stack
slot: `sub [r12],0x10`, three stores, two loads) then `Global::deallocate` (484, 31
instructions, 6.19 M direct calls of its own), which calls `__rust_dealloc`. A `Vec`
drop is three calls deep and touches the shadow stack twice before the allocator runs.

`Global::alloc_impl_runtime` (three copies, 18.6 M calls): a wrapper that tests the
zeroed flag and the size and calls `__rust_alloc`; 5.0 ms self on node 24 with V8
inlining, 27.7 ms without.

This is std at `-Oz`; wasmalloc cannot change it. It sets the floor: an out-of-line
call from V8 is `call rel32` into the jump table, `jmp` to the code, a 5-instruction
frame, a stack check and a 3-instruction epilogue, about 12 instructions and two
control transfers before any allocator work.

### 5.5 Against the roofline floors on the same engine

`bench/roofline` built with the `sizeclass`, `mimic_lean` and `wasmalloc` features (main,
opt-level 3, fat LTO), `alloc_free_32` loop, node 24 `--no-liftoff` [C13]; per
iteration on the hot path:

| variant | instructions | loads | stores | branches | spills/reloads |
|---|--:|--:|--:|--:|--:|
| `sizeclass` (list head at a static address) | 23 | 4 (head, next, head, stack limit) | 4 (head, next, head, workload byte) | 3 (stack, empty, loop) | 1 |
| `wasmalloc` (fast paths inlined by LLVM) | 48 | 9 (direct, free, next, used; used, free, retire_expire, flags, limit) | 7 (free, used, byte; next, free_is_zero, used, free) | 6 (stack, empty, used==0, retire, flags, loop) | 5 |

The `mimic_lean` floor (`asm/roofline-mimic_lean-alloc_free_32.txt`) is the sizeclass
shape plus one direct-table load. The 2x instruction and memory-operation count is
the whole of the 1.13 against 0.55 ns gap roofline section 14 measures; nothing in the
simlin bundle's `__rust_alloc` is worse than this loop except the call.

### 5.6 `--no-liftoff` is not the production code

With `--no-liftoff` V8 compiles without call-count feedback and inlines by static size:
`dealloc_transition` lands inside `__rust_dealloc` (704 bytes), `page::extend` inside
`alloc_generic`, the alloc path into 384 functions. Under real tiering [C8]
`__rust_dealloc` is 320 bytes and calls `dealloc_transition` and `dealloc_generic`, the
alloc path is inlined into 64 functions covering 18 percent of allocations, the dealloc
path into 3 cold ones. Read production codegen from a tiered dump.

## 6. Realloc in situ

Per compile, wasmalloc bundle [C6]:

| quantity | value |
|---|--:|
| `__rust_realloc` calls | 1,432,286 |
| growths / shrinks | 1,423,518 / 8,768 |
| exact doublings (`new == 2 * old`) | 992,302 (69%) |
| doublings to 2 KiB or more | 34,657 |
| stayed in place (shrink within kind, or run extended) | 8,822 (0.6%) |
| `memory.copy` calls / bytes | 1,423,464 / 206,226,032 (145 B average) |
| `try_extend` on runs / through `memory.grow` | 50 / 1 |
| entered `alloc_generic` for the new block | 67,078 |
| entered `alloc_huge` | 62 |

| new size (B) | reallocs | old size (B) | reallocs |
|---|--:|---|--:|
| 1 to 40 | 395,942 | 1 to 40 | 899,169 |
| 41 to 64 | 119,507 | 41 to 64 | 157,009 |
| 65 to 128 | 522,801 | 65 to 128 | 140,016 |
| 129 to 256 | 149,497 | 129 to 256 | 95,671 |
| 257 to 1024 | 179,899 | 257 to 1024 | 107,199 |
| 1025 to 2048 | 31,418 | 1025 to 2048 | 19,473 |
| 2049 to 4096 | 19,473 | 2049 to 4096 | 10,591 |
| 4097 to 8192 | 10,591 | 4097 to 8192 | 1,766 |
| 8193 to 16384 | 1,766 | 8193 to 16384 | 1,288 |
| 16385 to 40960 | 1,298 | 16385 to 40960 | 54 |
| above 40960 | 94 | above 40960 | 50 |

The "16 to 40 B" case of the native histogram is the 1-to-40 bucket here (396 k, 28
percent), a `String` or `Vec<u32>` growing 8 to 16 to 32 to 64; the native "224 B"
case (272 k) is the 65-to-128 bucket on wasm32 (523 k; only 12,425 requests are
exactly 224 bytes and all grow from at most 128). Both are exact doublings between
tight power-of-two bins (16, 32, 64, 128 are bin sizes), so `fits_in_place` is false
and the block moves. dlmalloc grows 42 percent of these in place (833,330 copies for
1,432,282 reallocs, 119 MB) and still spends 51.8 ms in `__rdl_realloc` inclusive
against wasmalloc's 21.9 ms.

Cost: 21.9 ms inclusive per compile, 15.3 ns per call, of which the copies are about
5 ms (1.39 M copies under 512 B at 2.2 to 3.1 ns, 35 k of 2 to 40 KiB at 10 to 60 ns
[C12]), the inlined pop and push about 2 ns, and the rest is the entry: shadow-stack
frame, two out-of-line-compiled `classify` calls with sret round trips, the huge-run
tests. Verdict on the two ideas the lead named:

- A same-kind in-place growth trick cannot apply: the next block in a page is another
  live or free block of the same class, and a growth to the next bin needs a block of a
  different class in a different page. Only runs (above 40 KiB) grow in place: of the
  94 requests above 40 KiB, 51 tried `try_extend` (one of them through `memory.grow`)
  and the counters do not say how many succeeded.
- A different bin spacing for the doubling sizes does not help a caller that requests
  exactly twice the old capacity; a block big enough for the next doubling would have
  to be handed out one doubling early, that is bin(2 * size) for every allocation of
  a `Vec`, which the allocator cannot tell apart from a `Box`. Rust's `Vec` doubles;
  the 35 k doublings at 2 KiB and above cost about 8 ms per compile (0.4 percent) and
  the small ones about 10 ms including their `alloc_generic` entries.

What is cheap: inline `classify` (and `fits_in_place`, `huge_slices`) with
`#[inline(always)]` so `-Oz` consumers get no calls and no sret slot, and compare the
old and new direct indices before classifying when both sizes are at most 1 KiB and
the alignment is at most 8 (the common case here). Expected 6 to 8 ms per compile.

## 7. Large zeroed buffers and memory.grow

Per compile [C6]: `__rust_alloc_zeroed` is called 141,114 times, 133,171 for 1 to 8
bytes, 4 for sizes above 40 KiB: three between 40 KiB and 1 MiB and one between 32 and
64 MiB (the 58 MiB `Vm::new` buffer). `alloc_huge` ran 154 times (92 from
`alloc_generic`, 62 from `realloc`) and issued 3 `memory.fill` calls totalling
1,386,945 bytes: the three sub-MiB zeroed runs landed on dirty slices and were cleared,
the 58 MiB buffer came from fresh slices with `Run::zeroed` set and was not touched.
The 7 MiB buffers are 7 plain `alloc` requests between 1 and 8 MiB; there are no 8 to
24 MiB requests on wasm32 (the native harness counted 64-bit sizes). dlmalloc's
`__rust_alloc_zeroed` filled 63,614,701 bytes for the same 141,114 requests.

`memory.grow` on node 24 costs 78 to 112 us per call whatever the size (1 page or 16
pages, 2000 calls [C14]); node 22 is 91 to 122 us. V8 13.6 has not received the 0.3 us
grow that V8 15.2 (d8) shows in roofline section 10. The wasmalloc bundle grows 37
times per compile (4,648 pages = 290.5 MiB, geometric eighth-of-heap steps), about
3.7 ms, which is the 3.9 ms of `(garbage collector)` in its profile; dlmalloc grows
2,844 times (4,434 pages) for 203 ms of GC, 7.7 percent of its compile. A first step
larger than 1 MiB would save at most 2 ms here.

## 8. Ranked changes for the tuning engineer

Expected gains are on node 24's 1877 to 1925 ms compile; none is large, because the
allocator is 6 to 7 percent of the stage and its fast paths run at 1 to 1.3 ns per call.

| rank | change | mechanism | expected gain | risk | measure with |
|---|---|---|--:|---|---|
| 1 | Lean `realloc` entry: `#[inline(always)]` on `bins::classify`, `fits_in_place`, `huge_slices`; when `align <= 8` and both sizes are at most 1 KiB compare `direct_index`/bin of old and new before any classify; keep one inlined alloc and one dealloc copy | removes two out-of-line calls, the 16-byte shadow-stack frame and two sret round trips from 1.43 M calls; also removes 3.08 M `classify` calls per compile at `-Oz` | 6 to 8 ms (0.3 to 0.4%) | low; realloc is covered by the model tester and the Kani `fits_in_place` proof | `__rust_realloc` self time [C4]; `bins_classify` count [C6] must fall to about 218 k |
| 2 | Serve 1 to 40 KiB requests from the queue head on the fast path: `queues[bin(size)].first` with a `has_free` test before `alloc_generic` (or extend the direct table to the medium limit, 5 KiB of table) | 163 k of the 215 k `alloc_generic` entries per compile are requests above 1 KiB whose bin's first page had a block; each pays about 35 ns of generic work | 5 to 6 ms (0.3%) | low; `find_page` semantics unchanged for the miss case | `alloc_generic` count [C6] 215 k to about 50 k |
| 3 | Keep the shims small enough for V8's inliner: drop the dead spill and the duplicated byte tests if the source can express them, do not add code to `__rust_dealloc` | V8 13.6 inlines the alloc path at hot sites covering 18% of allocations and the dealloc path nowhere; every out-of-line call costs about 12 instructions of frame, stack check and jump table (a third of the fast path) | 0 to 15 ms if more sites inline; unmeasurable from Rust alone | low, but the lever belongs to V8's heuristics (`--wasm-inlining-budget`, callee size) | tiered dump scan [C8, C9]: functions containing the fast-path signature, calls covered |
| 4 | Hot-block cache per direct index (roadmap 8) | saves the direct-table load, the header `free`/`used` traffic and the transition test on a hit; adds one load and one branch to every alloc, a second-level push to every free, 129 words of state, and complicates `alloc_zeroed` and the `used` invariant | 0 to 15 ms (0 to 0.8%); see below | medium: new invariant, new Kani harness, Liftoff and inlining-budget cost of a bigger shim | roofline `alloc_free_32`, `churn`; then [C4] on the bundle |
| 5 | Fold `free_is_zero` into `flags` (roadmap 2) | one byte store fewer per free, 31 M per compile | 0 to 3 ms | low | [C4]; roofline 12.1 predicts nothing |
| 6 | First `memory.grow` step of 4 MiB instead of 1 MiB | 37 grows at about 100 us each; the first few are the small ones | 1 to 2 ms | none | `memory_grow` count [C6] |

Not worth doing on this workload: an inline zero for small `alloc_zeroed` (the fill is
2.2 ns and the function is 0.6 ms per compile); anything about `page::extend` (26 ms of
first-touch page faults for a 290 MiB heap, paid by any allocator); any change to the
alignment handling (54 aligned requests per compile).

On the hot-block cache. The counters say what the fast path already achieves here:
99.34 percent of allocations pop the direct page's free list, whose head is the block
most recently freed into that page, so for the LIFO pattern simlin's compiler produces
(allocate temporaries, drop them, allocate the next) the block handed out is already
the hot one. The cache would save two header loads and two header stores per pair
(about 0.3 ns on the roofline's cache-hot pair, where the pair costs 1.13 ns against a
0.55 ns floor), which over 63 M operations is 10 to 15 ms at best; it does not touch the
out-of-line call overhead that dominates the shim's 1.0 to 1.3 ns, and a longer shim
is a shim V8 inlines less. Its real target is churn over many live objects (roofline
churn at 2x the floor), which simlin's compile is not: the direct-page miss rate is
0.66 percent and 76 percent of those misses are requests above 1 KiB. Rank it after the
first three and measure it on the roofline before touching the bundle.

The biggest levers are not the allocator's: simlin's `-Oz` (section 3.5: 460 to 700 ms
per compile against 3.2 to 4.6 MB of bundle) and simlin's hashing (113 ms in SipHash and
FxHash per compile).

## 9. Commands

All paths relative to the scratchpad `prof/` directory unless absolute; `$N24` is
`third_party/node-v24.20.0-linux-x64/bin/node`, `node` is 22.22.2; every engine run
was pinned with `taskset`.

- C1 (names bundles, both branches; the `main` build in a detached worktree of the
  simlin clone): `CARGO_TARGET_DIR=$S/prof/target-wa CARGO_PROFILE_RELEASE_STRIP=debuginfo cargo build -p simlin --lib --release --target wasm32-unknown-unknown --no-default-features` (in `third_party/simlin`), then `wasm-opt simlin.wasm -o wa-names2.opt.wasm -O3 -g --enable-mutable-globals --enable-bulk-memory --enable-bulk-memory-opt --enable-nontrapping-float-to-int`, `wasm-tools demangle wa-names2.opt.wasm -o wa.wasm`, `wasm-tools print wa.wasm > wa.wat`. Same with `target-main` in `git worktree add --detach $S/prof/simlin-main main` for `dl.wasm`.
- C2 (equivalence of the raw builds): `wasm-tools strip <raw> | cmp` against the shipped `.raw`; `wasm-tools objdump` for section sizes.
- C3: `taskset -c 4-7 $N24 --expose-gc src/engine/bench/clearn-alloc.mjs --ltm on --iters 3 --warmup 1 dl-shipped=... dl-names=prof/dl.wasm wa-shipped=... wa-names=prof/wa.wasm`.
- C4: `taskset -c 4-7 $N24 profile-compile.mjs --iters 10 --warmup 2 --interval 100 --out prof-n24-wa.json wa.wasm` (and `dl.wasm`; `node` for node 22).
- C5: `node report.mjs prof-n24-wa.json prof-n24-dl.json --top 45` (writes the group tables, the top frames and the per-function difference).
- C6: `node instrument.mjs wa.wasm wa-cnt3.wasm && taskset -c 8-11 $N24 profile-compile.mjs --counters --noprofile --iters 2 --warmup 1 wa-cnt3.wasm` (`counts-wa3.txt`; `dl-cnt.wasm` and `counts-dl.txt` likewise; `wa-cnt4.wasm` adds the large-size buckets).
- C7: `taskset -c 16-19 $N24 --no-liftoff --print-wasm-code profile-compile.mjs --sync --noprofile --iters 0 --warmup 1 wa.wasm > asm/wa-all-turbofan.txt`; `asmfn.sh asm/wa-all-turbofan.txt 799` for one function. `--print-wasm-code-function-index=N` alone prints one function (node 22 listings `asm/wa-799-n22.txt`, `asm/wa-792-n22.txt`).
- C8: as C7 without `--no-liftoff` and with `--iters 1`: `asm/wa-all-tiered.txt`.
- C9: `node inlined.mjs`, which joins `counts-wa3.txt`, the `call $__rust_*` sites in `wa.wat`, and the functions whose TurboFan code contains `0x15fb04]` (alloc) or `+0x16],0x0` (dealloc) in the dump (`inlined-alloc-tiered.txt`, `inlined-dealloc-tiered.txt`).
- C10: `taskset -c 4-7 $N24 --no-wasm-inlining profile-compile.mjs --iters 10 --warmup 2 --interval 100 --out prof-n24-wa-noinline.json wa.wasm`.
- C11: C1 with `RUSTFLAGS="-C opt-level=s"` (and `3`) into `target-wa-Os`/`target-wa-O3`, `wasm-tools strip` after wasm-opt, then `taskset -c 4-7 $N24 --expose-gc src/engine/bench/clearn-alloc.mjs --ltm on --iters 3 --warmup 1 Oz=... Os=... O3=...`.
- C12: `bulk.wat`/`bulk.mjs`: loops of `memory.fill` and `memory.copy` of 8 to 4096 bytes at rotating offsets, 20 M iterations, `$N24 --no-liftoff bulk.mjs` and `node --no-liftoff bulk.mjs`.
- C13: `CARGO_TARGET_DIR=$S/prof/target-roofline cargo build --release --target wasm32-unknown-unknown --lib --features <sizeclass|mimic_lean|wasmalloc>` in `bench/roofline`, then `$N24 --no-liftoff --print-wasm-code run.mjs --only alloc_free_32 --reps 1 roofline-<f>.wasm`, `alloc_free_32` block extracted to `asm/roofline-<f>-alloc_free_32.txt`.
- C14: `grow.wat`/`grow.mjs`: an exported `memory.grow` called 2000 times with 1 page and 200 times with 16 pages on fresh instances.
