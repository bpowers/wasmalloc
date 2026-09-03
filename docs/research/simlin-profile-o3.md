# Profile of simlin's C-LEARN compile at opt-level 3 (the PR #1038 configuration)

Measured 2026-09-02 on the reference machine (Ryzen 9 9950X, Linux 7.1.7) against simlin
at `third_party/simlin-opt/simlin` branch `wasm-opt-level` (82bc0c3b, PR bpowers/simlin#1038
on top of #1037: `opt-level = 3`, `lto = true`, `panic = "abort"`, `codegen-units = 1` for
wasm32, `wasm-opt -O3`). Workload, engines and profiler as in `simlin-profile.md`: the
`compile` stage of `src/engine/bench/clearn-alloc.mjs` on C-LEARN v77 with Loops That Matter,
fresh instance per iteration, node 24.20.0 (V8 13.6) and node 22.22.2 (V8 12.4), `taskset`
pinned. Allocators: wasmalloc 0.1.0 from crates.io (what the PR's lockfile pins), wasmalloc
main = v0.1.1 (path patch), and std's dlmalloc (the `#[global_allocator]` cfg'd out). Every
number names its command in section 10; scratch files live in
`/tmp/claude-1000/-home-bpowers-src-wasm-clalloc/*/scratchpad/prof-o3/`.

Contents

1. Summary
2. The PR's bundle is not link-time optimised
3. Bundles measured
4. Allocator share of the compile stage
5. Counts and per-operation costs
6. Codegen of the fast paths: out of line, V8-inlined, LLVM-inlined
7. Slow paths
8. The run stage
9. Ranked changes
10. Commands

## 1. Summary

- The premise "at -O3 with fat LTO the fast paths inline into every call site" does not hold
  for the PR's bundle: cargo passes no `-C lto` when a lib target's `crate-type` includes
  `rlib` (simlin's is `["staticlib", "rlib", "cdylib"]`), so `lto = true` is silently ignored
  for the wasm build and `__rust_alloc`/`__rust_dealloc` stay out of line with 5,573 and
  13,562 call sites. The std wrappers around them did get inlined (they are `#[inline]`
  cross-crate), so the call chain is one deep instead of three at -Oz.
- In that configuration wasmalloc's functions are 114.7 ms of a 1165 ms compile on node 24
  (9.8 percent; 117.7 ms with 0.1.0) and 155 ms of 1274 ms on node 22 (12.2 percent). The
  share grew from 6 percent at -Oz because simlin got 40 percent faster and the out-of-line
  shims did not. dlmalloc is 492 ms plus 168 ms of `memory.grow` GC in an 1837 ms compile
  (35.9 percent; 2841 grows). Of the 672 ms between the allocators, 377 ms is allocator
  self time, 164 ms GC, 131 ms callers.
- Building the wasm bundle as a cdylib only (`cargo rustc ... --crate-type cdylib`, one line
  in `build.sh`) turns LTO on. Compile drops 71 to 103 ms (6 to 9 percent), run 50 ms on
  node 24, dispose 3 ms; the engine test-suite passes under both node versions. LLVM then
  inlines the fast paths at 6,378 alloc and 11,877 dealloc sites; 40 percent of allocations
  run through copies whose direct-table index folded to a constant (a 9-instruction path with
  no size or alignment test), 60 percent through copies with a dynamic size. The inlined
  copies are worth 41 ms (node 24) to 59 ms (node 22) of the LTO gain and cost 1.46 MB of raw
  bundle (110 KB brotli): LTO with the fast paths `#[inline(never)]` keeps 30 to 44 ms of
  the gain at +30 KB.
- Slow paths are unchanged in kind: `alloc_generic` 220 k entries (0.68 percent of
  allocations), 141 ns each including 24 ms of first-touch page faults inside
  `page::extend`; three quarters of the entries above 1 KiB are now served from the queue
  head (147 k) as 0.1.1 intended; `dealloc_transition` 42 k at 10 ns; `realloc` 1.43 M at
  12.5 ns including a 145-byte average copy. 0.1.1's realloc early return fires 22 times per
  compile (Rust's `Vec` doubles, which always crosses a bin).
- The run stage is 91 to 93 percent `Vm::eval_bytecode` self time. Its swings (+17 percent
  from `codegen-units=1` on node 24, -2 to -9 percent on node 22 between bundles whose run
  code is identical) are LLVM inlining and code placement inside one 29 k-instruction
  function, not V8 inlining: callee self times are equal to the millisecond.
- 0.1.1 and 0.1.0 compile within 0.5 percent of each other at -O3. 0.1.1's growth policy
  ends this workload at 4857 pages (303.6 MiB) against 4613 (288.3 MiB), a 5 percent higher
  peak that the eighth-of-heap step landed on.

## 2. The PR's bundle is not link-time optimised

`cargo build -v` for the `simlin` crate [C1] shows three `--crate-type` flags (staticlib,
rlib, cdylib), `-C opt-level=3`, `-C panic=abort`, `-C codegen-units=1` and no `-C lto`.
Cargo only requests LTO when every crate type of the target allows it; an `rlib` does not,
so `[profile.release] lto = true` never reaches rustc for this target. The -Oz study's "LLVM
keeps `__rust_alloc` out of line at -Oz" was the same effect.

`cargo rustc -p simlin --lib --release --target wasm32-unknown-unknown --no-default-features
--crate-type cdylib` [C2] passes `-C lto` (cargo's `[profile.release]` value) and produces a
bundle in which the allocator shims are inlined. Two experiments pin the mechanism [C3]: a
6-line cdylib-only crate with a bump `#[global_allocator]` inlines its shim at the default
threshold; the same crate with wasmalloc inlines `Heap::alloc` and `Heap::dealloc` into
`make_box` and `drop_box` with the direct-table index folded to `i32.load offset=...` from
`i32.const 0`. The `#[global_allocator]` macro expands to plain `#[rustc_std_internal_symbol]
#[rustc_allocator] unsafe fn __rust_alloc(size, align)` (nightly `-Zunpretty=expanded`), an
ordinary function LLVM may inline when it sees it.

| build (browser bundle, wasmalloc main) | `-C lto` | `__rust_alloc` sites | `__rust_dealloc` sites | `alloc_generic` sites | code section | stripped | brotli -q 11 | gzip -9 | cargo build |
|---|---|--:|--:|--:|--:|--:|--:|--:|--:|
| PR: `cargo build`, cgu 1 (`wa011`) | no | 5,573 | 13,562 | 3 | 7,720,559 | 8,033,154 | 1,756,524 | 2,577,138 | 100 s |
| `cargo rustc --crate-type cdylib`, cgu 1 (`walto`) | yes | 941 | 3,250 | 6,605 | 9,222,958 | 9,519,200 | 1,870,132 | 2,991,346 | 116 s |
| same, fast paths `#[inline(never)]` (`waltoni`) | yes | 0 (7,753 calls of `Heap::alloc`) | 0 (15,833 of `Heap::dealloc`) | 3 | 7,767,523 | 8,063,652 | 1,759,704 | 2,722,749 | 111 s |
| `cargo rustc --crate-type cdylib`, cgu 16 (`walto16`) | yes | | | | 9,721,748 | 10,025,109 | 1,916,051 | 3,012,398 | 65 s |
| shipped `3-cgu1-browser.wasm` (0.1.0) | no | 5,573 | 13,562 | 3 | 7,718,419 | 8,031,011 | 1,758,330 | 2,579,814 | 97 s |

The names bundles reproduce the shipped code to 0.03 percent (same 6,628 functions) [C4].
The full (node) bundle grows from 9,977,497 to 11,481,594 bytes with LTO. The engine
test-suite (`pnpm test` in `src/engine`, 20 files) passes with the two LTO bundles staged
under node 22 and node 24 [C5].

## 3. Bundles measured

Un-profiled bench, C-LEARN LTM on, 5 iterations after 2 warm-ups, medians in ms [C6]:

| bundle | allocator | LTO | n24 compile | n24 run | n24 dispose | n22 compile | n22 run | n22 dispose | peak MiB |
|---|---|---|--:|--:|--:|--:|--:|--:|--:|
| `wa010` (PR as pinned) | 0.1.0 | no | 1147 | 1120 | 28.6 | 1216 | 1527 | 29.7 | 288.3 |
| `wa011` | main | no | 1142 to 1151 | 1117 to 1142 | 28.5 | 1210 to 1216 | 1328 to 1406 | 29.5 | 303.6 |
| `walto` | main | yes | 1071 to 1074 | 1063 to 1077 | 25.4 | 1104 to 1107 | 1387 to 1440 | 26.4 | 303.6 |
| `waltoni` | main, never-inlined | yes | 1112 | 1078 | 28.6 | 1166 | 1407 | 29.3 | 303.6 |
| `walto16` | main | yes, cgu 16 | 1099 | 1327 | 26.0 | 1138 | 1411 | 26.2 | 303.6 |
| `dl` | dlmalloc | no | 1788 to 1796 | 1119 to 1127 | 65.8 to 67.5 | 1903 to 1909 | 1351 to 1357 | 68.6 to 69.7 | 278.4 |
| `dllto` | dlmalloc | yes | 1717 | 1075 | 63.8 | 1801 | 1379 | 65.3 | 278.4 |

Ranges span the three bench runs that included the bundle. The run column moves by up to 9
percent between bundles whose run-stage code is identical (`wa010` and `wa011` on node 22);
section 8 explains why it is not an allocator effect.

## 4. Allocator share of the compile stage

Profiled compile (10 iterations after 2 warm-ups, 100 us sampling), self ms per iteration
[C7, C8]. The profiler adds about 1.5 percent.

### 4.1 PR configuration (no LTO): shims out of line

| function | main n24 | main n22 | 0.1.0 n24 | 0.1.0 n22 |
|---|--:|--:|--:|--:|
| compile stage | 1165 | 1274 | 1169 | 1271 |
| `__rust_alloc` self (incl.) | 42.7 (58.1) | 70.0 (95.6) | 45.7 (61.4) | 69.9 (95.6) |
| `__rust_dealloc` | 26.8 | 37.6 | 28.0 | 40.7 |
| `__rust_realloc` self (incl.) | 13.1 (17.9) | 15.7 (20.8) | 14.0 (18.2) | 14.6 (20.0) |
| `__rust_alloc_zeroed` | 0.5 | 0.6 | 0.5 | 0.4 |
| `alloc_generic` self (incl.) | 27.2 (31.1) | 4.5 (30.8) | 25.3 (28.9) | 5.4 (31.1) |
| `page::extend` | 2.2 | 24.3 | 2.1 | 24.0 |
| `dealloc_transition`, `collect_retired`, queue ops | 0.9 | 0.9 | 0.8 | 0.9 |
| `acquire_run`, `slices::*` | 1.2 | 1.5 | 1.3 | 1.4 |
| **wasmalloc self total** | **114.7 (9.8%)** | **155.2 (12.2%)** | **117.7 (10.1%)** | **157.4 (12.4%)** |
| `(garbage collector)` | 3.7 | 4.2 | 3.4 | 4.2 |
| std wrappers (`RawVecInner::*`, `Global::*`, `exchange_malloc`) | 5.5 | 6.0 | 6.0 | 5.6 |
| `memcmp`/`memcpy`/`memset` (simlin's, mostly) | 16.2 | 63.8 | 15.4 | 64.0 |

On node 24 V8 inlines `page::extend` into `alloc_generic` (the tiered dump has no TurboFan
block for `extend`; `alloc_generic`'s 6144-byte TurboFan body holds the link loop), so the
24 ms of first-touch page faults that node 22 charges to `extend` sit in `alloc_generic`'s
self time on node 24. The std wrappers fell from 89 ms at -Oz to 6 ms: -O3 inlined them.

dlmalloc, same builds:

| function | n24 | n22 |
|---|--:|--:|
| compile stage | 1837 | 1999 |
| `Dlmalloc::malloc` | 225.4 | 249.4 |
| `Dlmalloc::free` (incl. 209.9 / 222.2) | 122.7 | 134.7 |
| `unlink_chunk`, `insert_large_chunk`, other | 94.9 | 93.8 |
| `__rdl_dealloc` (41 wasm instructions, 15,826 sites, out of line) | 29.4 | 53.7 |
| `__rdl_realloc`, `__rdl_alloc`, `__rdl_alloc_zeroed` | 19.5 | 32.0 |
| **dlmalloc self total** | **492.0 (26.8%)** | **563.7 (28.2%)** |
| `(garbage collector)`, 2841 `memory.grow` calls | 168.1 (9.2%) | 173.0 (8.7%) |

Where the difference goes (dlmalloc minus wasmalloc main, per compile):

| component | n24 | n22 |
|---|--:|--:|
| allocator self time | 377 | 409 |
| GC driven by `memory.grow` (2841 against 41 calls) | 164 | 169 |
| callers (`Expr0::clone` +15.7, `drop_glue::<Expr0>` +9.9, `write_str` +10, `canonicalize` +7, ...) | 131 | 148 |
| compile stage | 672 | 725 |

### 4.2 LTO configuration: fast paths inlined into callers

| function | wasmalloc n24 | wasmalloc n22 | dlmalloc n24 | dlmalloc n22 |
|---|--:|--:|--:|--:|
| compile stage | 1123 | 1161 | 1791 | 1871 |
| residual `__rust_alloc` + `__rust_dealloc` (941 + 3,250 sites left out of line) | 2.2 | 5.2 | | |
| `__rust_realloc` self (incl.) | 13.4 (18.7) | 14.2 (19.6) | `__rdl_realloc` 11.1 (50.4) | 11.6 (53.8) |
| `alloc_generic` self (incl.) | 26.5 (30.0) | 5.1 (30.8) | `malloc` 226.3 | 234.2 |
| `page::extend` | 2.1 | 24.1 | `free` 162.7 | 166.3 |
| transition, collect, queues, slices | 2.0 | 2.3 | other 60.4 | 59.6 |
| **out-of-line allocator self** | **46.1 (4.1%)** | **50.9 (4.4%)** | **462.5 (25.8%)** | **475.0 (25.4%)** |
| `(garbage collector)` | 3.9 | 4.2 | 170.2 | 160.6 |

The inlined fast paths are charged to their callers. Their cost is bounded two ways: the
never-inlined LTO bundle shows `Heap::alloc` at 47.6 ms and `Heap::dealloc` at 40.0 ms self
on node 24 (1161 ms compile), and inlining them buys 41 ms (node 24) to 59 ms (node 22) of
un-profiled compile (`walto` against `waltoni`). So the inlined copies cost roughly 45 ms and
the whole allocator is about 90 ms, 8 to 9 percent, of the 1074 ms LTO compile; the
allocator's share of the LTO gain over the PR bundle is 41 of 71 ms on node 24 and 59 of 103
ms on node 22, the rest is cross-crate inlining of simlin's own code (dlmalloc gains 71 ms
from the same switch; its `__rdl_dealloc` wrapper, 29 ms out of line before, is inlined by it).

Caller-by-caller, dlmalloc LTO minus wasmalloc LTO on node 24: `malloc` +226, GC +166,
`free` +163, `unlink_chunk` +59, `insert_large_chunk` +1; `alloc_generic` -26, `__rust_realloc`
-13; the callers net +86 ms (`Expr0::clone` +17, `drop_glue::<Expr0>` +8, `Vm::new` +6.5,
`intern` +4.5, `canonicalize` +4.3) [C8].

## 5. Counts and per-operation costs

Per compile iteration, identical across iterations, from two instrumented bundles: a
text-level rewrite of the names bundle counting function entries, shim calls per caller and
`memory.grow` (`wa011-cnt`, [C10]; rows without a mark) and source counters compiled into a
copy of main (`wacnt`, [C9]; rows marked (s)). The allocation, free and realloc counts agree
with each other and with the -Oz study to 0.03 percent (LLVM elides a few thousand
allocations differently per build); the rarer queue events differ by up to 20 percent
between the two builds (`dealloc_transition` 42,287 against 34,558, fresh pages 6,227
against 6,074), because a slightly different allocation order fills different pages.

| event | count | note |
|---|--:|---|
| `__rust_alloc` calls | 32,633,481 | 54 with align above 8 |
| `__rust_dealloc` calls | 30,959,471 | |
| `__rust_realloc` calls | 1,432,282 | 1,423,514 growths, 992,298 exact doublings, 8,768 shrinks |
| `__rust_alloc_zeroed` calls | 141,114 | 133,171 of 1 to 8 bytes; 140,827 take the memset |
| `Heap::alloc` entries (shims plus realloc's inner alloc) (s) | 34,065,638 | 33,902,640 at most 1 KiB after rounding |
| direct-table hits (s) | 33,853,773 | 99.86 percent of the requests the table covers |
| `alloc_generic` entries | 220,736 | 1 in 148 allocations |
| of which served by the queue head (bin above the table, head has a block) (s) | 147,101 | new in 0.1.1; no counters, no search |
| of which page searches (`find_page`) (s) | 64,951 | `move_to_full` 32,572, `extend` of the candidate 31,693, `fresh_page` 6,074, `move_to_front` 1,122 |
| of which huge (s) | 92 | `alloc_huge` 154 in all, 62 from realloc |
| `page::extend` entries | 39,037 | 37,767 from `alloc_generic` paths |
| `collect_retired` (s) | 6,138 | 6,074 before a fresh page, 64 periodic (one per 1000 searches) |
| `acquire_run` / `release_empty_pages` / `free_page` (s) | 6,228 / 73 / 3,164 | |
| `dealloc_transition` | 42,287 | (s) 34,558: 29,848 `unfull`, 4,710 `retire` |
| `dealloc_generic` | 2,954 | medium blocks and runs |
| realloc: same direct slot (0.1.1 early return) (s) | 22 | `Vec` doubling always crosses a bin |
| realloc: in place (shrink within kind, run resized) (s) | 8,768 + 32 | |
| realloc: moved, bytes copied | 1,423,460, 206,225,872 | 145 bytes average; 67,363 enter `alloc_generic` |
| `memory.grow` calls, pages (s) | 41, 4,835 | 302 MiB; the text count sees one more grow outside the allocator |

Per-call cost, node 24 and node 22, PR configuration (profile time divided by count):

| path | n24 ns | n22 ns | basis |
|---|--:|--:|---|
| `__rust_alloc` out-of-line call | 1.4 | 2.1 | 42.7 / 70.0 ms self over the 30 M calls not V8-inlined (section 6.2) |
| `__rust_dealloc` | 0.9 | 1.2 | 26.8 / 37.6 ms over 31.0 M |
| `__rust_realloc` incl. copy and callees | 12.5 | 14.5 | 17.9 / 20.8 ms over 1.43 M |
| `alloc_generic` incl. | 141 | 140 | 31.1 / 30.8 ms over 220.7 k, first touch included |
| `alloc_generic` without `extend` | not separable | 29 | node 22 self 4.5 plus queue and slice time over 220.7 k; V8 inlines `extend` on node 24 |
| `page::extend` (8 KiB of fresh blocks) | inside `alloc_generic` | 623 | 24.3 ms over 39.0 k on node 22 |
| `dealloc_transition` incl. | 9 | 9.5 | 0.4 ms over 42.3 k |
| `memory.grow` | about 90,000 | 100,000 | 3.7 / 4.2 ms of GC over 41 |

Under LTO the never-inlined `Heap::alloc` costs 1.4 ns and `Heap::dealloc` 1.2 ns per call on
node 24 (47.6 and 40.0 ms); inlined, both together cost about 45 ms for 66.5 M operations,
0.7 ns each.

## 6. Codegen of the fast paths

Three shapes exist at -O3: the out-of-line shim that every call site of the PR bundle
reaches (6.1); the copies V8 13.6 inlines into hot TurboFan callers (6.2); and, with LTO, the
copies LLVM inlines with the Layout folded (6.3). Sizes: wasm instruction counts exclude
block, end, loop and else; wire bytes are the function body's size in the code section
[C11]; TurboFan bytes are V8's `Body` size in the tiered dump [C12].

### 6.1 The shims, PR configuration, against -Oz

| function | wasm instr. -O3 (main) | -O3 (0.1.0) | -Oz (0.1.0) | wire bytes | TurboFan n24 | TurboFan n22 |
|---|--:|--:|--:|--:|--:|--:|
| `__rust_alloc` | 53, 1 call | 59, 2 calls | 55 | 116 | 232 (256 body) | 184 |
| `__rust_dealloc` | 52, 2 calls | 61, 2 calls | 61 | 118 | 244 (320) | 212 |
| `__rust_alloc_zeroed` | 69 | 75 | 71 | 153 | not tiered up | |
| `__rust_realloc` | 999, 12 calls | 764, 7 calls | 457 | 1,992 | 4,608 | |
| `alloc_generic` | 1,151, 14 calls | 946, 8 calls | 432 | 2,516 | 6,080 | |
| `dealloc_transition` | 454 | 454 | 78 | 1,017 | 2,344 | |
| `dealloc_generic` | 187 | 187 | | 389 | not tiered up | |
| `page::extend` | 95 | 95 | | 282 | inlined into `alloc_generic` | |

`__rust_alloc` on node 24 (index 486) is the -Oz listing of the previous study to the
instruction: frame (5), memory base load, stack check (2), `cmp rdx,9` and the aligned
rounding (folded away by 0.1.1's `direct_size` into one branch instead of two calls of
`alloc_generic`), `cmp rdi,0x401`, three ALU for the index, the table load
`[rbx+rdi+0x14d4bc]`, the `free` load, `test/jnz`, then `next` load, `free` store, `used`
load-add-store, `mov rax,r8; jmp`, epilogue (3). Hot path 31 instructions counted from
`push rbp` to `ret` (30 in the -Oz listing counted the same way: the `direct_size` fold costs
one `jmp` for the word-aligned case), 4 data loads plus 2 frame loads, 2 stores, 4 conditional
branches, 0 calls; the two cold `alloc_generic` calls of 0.1.0 became one. Node 22's version
is the same with the page address folded into `rbx` for the `used` update.

`__rust_dealloc` (467): 0.1.1's `small_limit` mask replaced the 7-instruction rounding select
with `mov; neg; and 0x2800; cmp; jna`, so the hot path is 33 instructions from `push rbp` to
`ret` (the -Oz listing counts 39 the same way): frame and stack check (8), `cmp rcx,0x1000;
jna`, the 5-instruction limit test, `mov; and 0xffff0000`, `used` and `free` loads, `next`
store (with a move), `free_is_zero` byte store, `used` decrement and store, `free` store,
`test/jnz` on `used`, the flags test that V8 still emits as `movzx` plus `cmpb`, epilogue
(3). Gone against -Oz: the rounding select, the prologue's spill of the memory base and the
dead `[rbp-0x20]` spill of the page address (the transition call is the function's tail
now).

The whole -Oz to -O3 change for the shims themselves is therefore 0.1.1's source changes
(two branches and a select removed); their cost per call is the same 1.3 to 1.4 ns on node
24 and 2.1 ns on node 22 as at -Oz. What -O3 changed is the caller: `Box<Expr>::new` is
`box_new_uninit` (11 wasm instructions, 28 bytes) calling `__rust_alloc`, `String::clone` is
38 instructions with one call, `RawVecInner::deallocate` and `Global::deallocate` no longer
exist as functions (the 10.98 M and 6.19 M call chain of the -Oz study is gone), and
`drop_glue::<Expr0>` calls `__rust_dealloc` from 13 sites with `i32.const 32; i32.const 8`
as arguments. The constants are pushed at every site; nothing can fold them across an
out-of-line call, so the alignment test, the size test and the index arithmetic run 32.6 M
times with constant inputs.

### 6.2 What V8 inlines, PR configuration

Tiered dump on node 24 [C12] joined with the per-caller call counts [C10, C13]:

| shim | calls | callers | callers in TurboFan | calls via callers with every site inlined | via partially inlined callers | TurboFan functions holding a copy |
|---|--:|--:|--:|--:|--:|--:|
| `__rust_alloc` | 30,618,774 | 1,125 | 273 | 2,311,679 (7.5%) in 27 functions | 4,909,906 (16.0%) | 95 |
| `__rust_dealloc` | 30,448,079 | 1,701 | 305 | 966,753 (3.2%) in 22 functions | 19,692,332 (64.7%) | 144 |

(At -Oz: 64 and 100 functions, 18 percent of allocations, 0 of frees.) The hottest callers
and what V8 did with them:

| caller | alloc calls | dealloc calls | sites | TurboFan bytes | copies | self ms n24 |
|---|--:|--:|--:|--:|--:|--:|
| `String::clone` | 3,842,486 | | 1 | 384 | 0 | 18.5 |
| `box_new_uninit` | 2,411,391 | | 1 | 128 | 0 | 0.5 |
| `Expr0::clone` | 2,008,028 | | 7 | 1,920 | 0 | 32.6 |
| `BuiltinVisitor::walk` | 1,430,270 | | 23 | 23,680 | 0 | 27.2 |
| `array_operand::rewrite` | 1,352,820 | 1,387,566 | 129 + 140 | 51,712 | 0 | 15.8 |
| `CanonicalStorage::intern` | 1,316,115 | | 3 | 5,184 | 0 | 40.8 |
| `Compiler::intern_name` | 1,231,084 | | 2 | 4,608 | 1 | 12.5 |
| `str::to_lowercase` | 1,054,996 | | 1 | 5,824 | 1 | 13.1 |
| `drop_glue::<Expr0>` | | 6,880,475 | 13 | 1,344 | 1 | 12.5 |
| `drop_glue::<Expr2>` | | 3,208,678 | 7 | 1,216 | 1 | 12.9 |
| `Arc<Interned>::drop_slow` | | 1,265,022 | 4 | 2,688 | 0 | 11.0 |
| `Parser::parse_unary` | 554,650 | | 9 | 12,736 | 3 | 12.2 |

The two hottest allocation sites are not inlined at all: `String::clone` (384 bytes of
TurboFan, 3.84 M calls) and `box_new_uninit` (128 bytes, 2.41 M calls) call the shim through
the jump table. Where V8 does inline, the copy keeps the shim's dynamic shape: inside
`intern_name` the sequence is `test rcx; jnz; cmp rcx,0x401; jnc; lea/shr/and;
mov r9,[r11+r9+0x14d4bc]; mov r12,[r11+r9]; test; jz; ...`, 14 instructions with the
alignment test folded (V8 knows align is 1 there) but the size test and the index arithmetic
kept, because `size` is the string length. V8's inliner decides per call count and callee
size and stops when a caller's budget is spent, which is why `drop_glue::<Expr0>` has one
copy for 13 sites and `array_operand::rewrite` none for 269.

### 6.3 What LLVM inlines with LTO

Counting the executed copies with a text-level rewrite of the LTO bundle (a counter before
every direct-table load and every `free_is_zero` store, [C14]):

| shape | static copies | executions per compile | share of the 33.9 M table lookups |
|---|--:|--:|--:|
| folded index: `i32.const 1349804; i32.load` (address of `direct[bin(32)]`) | 2,854 | 13,517,385 | 40% |
| dynamic index: `(size+7)>>1 & ~3; i32.load offset=1349788` | 3,524 | 20,377,680 | 60% |
| residual out-of-line `__rust_alloc` (among the dynamic) | 941 | 1,200,239 | 3.5% |
| dealloc copies (`i32.const 0; i32.store8 offset=22`) | 11,877 | 32,387,482 | 100% of frees |
| residual out-of-line `__rust_dealloc` (among them) | 3,250 | 2,291,931 | 7.1% |

The three hottest sites and their sequences:

- `Expr0::clone` (4.08 M lookups: 1.52 M folded for `Box<Expr0>` of 32 bytes, 2.56 M dynamic
  for the strings inside). A folded copy is, in wasm, `i32.const 1349804; i32.load; local.tee;
  i32.load; local.tee; if {pop} else {call alloc_generic(8, 32, 0)}`, 17 instructions; in
  TurboFan `mov r14,[rbx+0x1498ac]; mov rdx,[rbx+r14]; test rdx,rdx; jnz pop` then
  `mov rcx,[rbx+rdx]; mov [rbx+r14],rcx; mov rcx,[rbx+r14+4]; add rcx,1; mov [rbx+r14+4],rcx`:
  9 instructions, 4 loads, 2 stores, 1 branch, no test of size or alignment, no frame, no
  stack check. The miss is `mov rdx,0x20; xor rcx,rcx; mov rax,8; call` to `alloc_generic`.
- `box_new_uninit` (2.41 M, dynamic): std keeps this helper out of line
  (`#[rustc_no_mir_inline]`, 290 call sites, `Box::new` for every `Expr` size) and passes
  size and align as arguments, so the index is computed. With LTO, interprocedural constant
  propagation still deleted both range tests: every caller passes a size at most 1 KiB and an
  alignment at most 8, so the 34-instruction wasm body has no `size > 1024` and no alignment
  branch, only the index arithmetic, the pop and the cold call. TurboFan: 51 instructions in
  all, hot path 26 including frame and stack check.
- `String::clone` (3.84 M calls at the PR shape; under LTO it is inlined into its callers,
  so its lookups appear in `Expr0::clone`'s 2.56 M dynamic ones, `CanonicalStorage::intern`'s
  0.88 M and others, while `RawVecInner::finish_grow` keeps 1.48 M of its own): dynamic
  size, alignment 1 folded: `cmp rcx,0x400; ja miss; lea r11,[rcx+7]; shr r11,1;
  and r11,0x7ffffffc; mov r11,[rbx+r11+0x14989c]; mov r12,[rbx+r11]; test; jz miss;` and
  the same 5-instruction pop: 15 instructions, 2 branches. The fallback for a dynamic size
  is therefore one compare-and-branch plus three ALU instructions, about 0.3 ns.
- `drop_glue::<Expr0>` (6.88 M frees, 13 copies): the wasm copy is 33 instructions (`and
  -65536`, `used` load, `next` store, `free_is_zero` store, `used` store, `free` store, the
  `used == 0 && retire_expire == 0 || flags != 0` test as two byte loads, all cold calls
  tail-merged into one block per function); TurboFan: `and rax,0xffff0000; mov rbx,[rbp-0x18];
  mov rdi,[rbx+rax+4]; mov r8,[rbx+rax]; mov r9,[rbp-0x28]; mov [rbx+r9],r8;
  movb [rbx+rax+0x16],0; lea r8,[rdi-1]; mov [rbx+rax+4],r8; mov [rbx+rax],r9; add rdi,0xff;
  jnz; movzx; cmpb; jz; movzx; cmpb; jz`: 18 instructions with two reloads of spilled values,
  3 header loads plus 2 byte loads (each read twice), 4 stores, 3 branches. Size and
  alignment tests folded at every one of the 13 sites.

The 941 and 3,250 sites LLVM left as calls sit in functions like `Expr2::from` (670 k frees),
`lower_from_expr3` (500 k) and `parse_unary` (553 k allocations): drop and error paths that
LLVM's static branch weights mark cold, where the inline threshold falls from 225 to 45 and
a 50-instruction body does not fit. V8 inlines some of them again at run time.

Size: a folded alloc copy is about 40 wire bytes, a dynamic one 60, a dealloc copy 75 to 90
(`drop_glue::<Expr0>` grew from 493 to 1,554 bytes for 13 copies, `Expr0::clone` from 743 to
1,612 for 9 plus an inlined `String::clone`). The 18,255 copies account for the whole 1.46 MB
between `walto` and `waltoni`.

## 7. Slow paths

| path | entries | n24 self / incl. ms | n22 self / incl. ms | ns per entry | what it does here |
|---|--:|--:|--:|--:|---|
| `alloc_generic` | 220,736 | 27.2 / 31.1 | 4.5 / 30.8 | 141 incl. | 147 k queue-head pops (0.1.1), 65 k searches, 92 huge; `find_page`, `fresh_page`, `page::init`, `alloc_huge`, `collect_retired` inlined by LLVM (14 calls remain: `SliceMap::alloc`, `remove`, `extend`, `collect_retired`, `release_empty_pages`, `grow_and_alloc`, plus `memory.fill`) |
| `page::extend` | 39,037 | 2.2 (rest inside `alloc_generic`) | 24.3 | 623 | first touch of 8 KiB of blocks: 311 MB linked per compile, the 302 MiB heap's page faults |
| `find_page` | 64,951 | inside `alloc_generic` | | about 30 without extend | 32,572 `move_to_full`, 31,693 candidate extends, 1,122 `move_to_front`, 6,074 fresh pages |
| `fresh_page` and `acquire_run` | 6,074 / 6,228 | | 1.5 (`slices::*`) | about 240 | 41 grows, 73 `release_empty_pages`, 3,164 `free_page` |
| `collect_retired` | 6,138 | 0.2 | 0.2 | 30 | |
| `dealloc_transition` | 42,287 | 0.4 | 0.4 | 9.5 | 29,848 `unfull`, 4,710 `retire`, 3,168 `remove` |
| `dealloc_generic` | 2,954 | under 0.1 | | | 2,857 medium blocks, 97 runs; never tiers up |
| `__rust_realloc` entry | 1,432,282 | 13.1 / 17.9 | 15.7 / 20.8 | 12.5 | 22 same-slot returns, 8,768 shrinks in place, 1,423,460 moves with a 145-byte copy, 67,363 `alloc_generic` entries, 649 `dealloc_transition`, 1,334 `dealloc_generic` |
| `__rust_alloc_zeroed` | 141,114 | 0.5 | 0.6 | 4 | 140,827 memsets of mostly 1 to 8 bytes, 273 in `alloc_generic` (1.78 MB filled) |

`__rust_realloc` at -O3 is 999 wasm instructions (12 calls) against 457 at -Oz: `classify`,
`fits_in_place` and `huge_slices` are inlined now, and so are one alloc and one dealloc fast
path, `try_extend` twice and the run-growth path with `release_empty_pages`. Its per-call
cost fell from 15.3 to 12.5 ns on node 24; the remaining 12.5 ns is 5 ms of `memory.copy`
(an external C call) and the two classifications, on 1.43 M calls that are 69 percent exact
doublings of a `Vec` or `String`, 27 percent to 40 bytes or less.

## 8. The run stage

Run-stage profiles (LTM on) [C15]:

| bundle | n24 run ms | `eval_bytecode` self | `vm::lookup` | `flat_offset` | n22 run ms | `eval_bytecode` self | `vm::lookup` |
|---|--:|--:|--:|--:|--:|--:|--:|
| `wa010` (-O3, cgu 1) | 1151 | 1053.1 | 58.1 | 14.5 | 1433 | 1335.6 | 59.4 |
| `wa010c16` (-O3, cgu 16) | 1354 | 1259.5 | 58.1 | 12.0 | 1435 | 1335.8 | 59.7 |
| `walto` (LTO, cgu 1) | 1123 | 1027.4 | 57.2 | 13.8 | 1475 | 1373.0 | 61.6 |

`Vm::eval_bytecode` is 91 to 93 percent of the stage and the only frame that moves; every
callee (`lookup`, `flat_offset`, `pow`, `log`, `exp`) is equal to the millisecond. Its code
[C16]:

| bundle | wasm instructions | calls | `panic_bounds_check` calls | TurboFan bytes n24 | instr. n24 | spills n24 | TurboFan bytes n22 | instr. n22 |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| cgu 1 | 29,329 | 481 | 211 | 157,752 | 34,873 | 1,463 | 151,616 | 34,335 |
| cgu 16 | 28,357 | 497 | 221 | 150,928 | 33,592 | 1,339 | 142,352 | 32,547 |

`codegen-units=1` inlines `SmallVec<[u16; 4]>::from_elem` (18 sites) and a few helpers into
the interpreter loop and keeps `RuntimeView` drop glue out of line; cgu 16 does the opposite.
V8 inlines nothing of note either way (1,370 jump-table calls in both). The 206 ms node 24
gain is the interaction of that layout with Turboshaft's register allocation and code
placement of one 150 KB function, and node 22 sees none of it: it runs the two bundles at
the same speed but runs `wa010` and `wa011`, whose run-stage code is byte-identical, 9
percent apart (1527 against 1398 ms in one bench run). Treat run-stage differences under
10 percent on node 22 as placement noise; the node 24 cgu 1 gain is real and LTO keeps it
(1063 to 1077 ms) while LTO with cgu 16 loses it (1327 ms). The simlin-side lever is the
interpreter itself: 211 bounds-check calls and a 29 k-instruction dispatch function.

## 9. Ranked changes

Expected gains are on the 1142 ms (PR configuration) or 1074 ms (LTO) node 24 compile;
node 22 figures in parentheses.

Simlin side, larger than anything in the allocator:

| rank | change | gain | cost | measure with |
|---|---|--:|---|---|
| S1 | Build the wasm bundle as a cdylib so cargo passes `-C lto`: in `build.sh`, `cargo rustc -p simlin --lib --release --target wasm32-unknown-unknown [--no-default-features] --crate-type cdylib` | compile -71 ms, 6% (-103, 9%); run -50 ms on node 24; dispose -3 | +1.46 MB raw, +110 KB brotli, +410 KB gzip; build 100 to 116 s | [C6]; sizes [C4] |
| S2 | S1 with wasmalloc's fast paths `#[inline(never)]` (a wasmalloc feature, A1 below) | compile -30 ms, 2.6% (-44, 3.6%); run -40 | +30 KB raw, +3 KB brotli | [C6] with `waltoni` |
| S3 | Hashing: SipHash-1-3 `write` 28 to 30 ms plus `RandomState::hash_one` 10, FxHash 19 to 28 ms per compile; `canonicalize` 50 to 52, `intern` 41 to 43 | 50 to 100 ms | simlin code | [C7] top frames |
| S4 | `Box<Expr>` churn: `Expr0::clone` 47 ms, `drop_glue::<Expr0>` 16 and `::<Expr2>` 16, 6.9 M frees of 32-byte `Expr0` boxes | tens of ms | simlin AST design | [C10] per-caller counts |

Allocator side:

| rank | change | mechanism | gain | risk | measure with |
|---|---|---|--:|---|---|
| A1 | A cargo feature (or a documented `cfg`) that puts `#[inline(never)]` on `Heap::alloc`, `alloc_zeroed` and `dealloc` for size-sensitive LTO consumers | LLVM then inlines only the trivial shim and callers call the fast path directly; V8 still inlines it at hot sites | none in time; -1.46 MB raw / -110 KB brotli against inlined LTO, for 41 (59) ms | none; measured (`waltoni`) | [C4], [C6] |
| A2 | Shrink the inlined dealloc copy: fold `free_is_zero` into `flags` (roadmap 2) and test `flags` and `retire_expire` with one 16-bit load | 3 wasm instructions and one byte load fewer per copy, about 8 bytes of 75 to 90; 11,877 copies under LTO | 0 to 3 ms; about -100 KB raw under LTO | medium: page invariant 5 and the transition proof change | [C11] on `drop_glue::<Expr0>`, [C4] |
| A3 | Keep `alloc_generic` and `dealloc_transition` unchanged in size and shape | `alloc_generic` is 2.5 KB of wire, 14 calls, past V8's inlining limit by design; adding to the shims costs V8 copies | 0 (guard) | | tiered dump [C12]: 95 alloc, 144 dealloc copies |
| A4 | Hot-block cache per direct index (roadmap 8) | saves the table load, the header traffic and the transition test on a hit | 0 to 15 ms, unchanged verdict from -Oz: 99.86 percent of table-range requests already hit, and under LTO the folded copy is 9 instructions | medium | roofline first |
| A5 | First `memory.grow` step of 1 MiB again for large heaps, or a step derived from the initial memory | 41 grows at 90 us; 0.1.1's 2-slice floor added 4 | under 1 ms | none | `memory_grow` [C9] |

Not worth doing: any change to `realloc`'s entry (12.5 ns of which the copy is most, the
early return fires 22 times), an inline zero for `alloc_zeroed` (0.5 ms), anything about
`page::extend` (24 ms of page faults that dlmalloc pays inside `malloc`), alignment handling
(54 aligned requests). The 0.1.0 to 0.1.1 changes are neutral on this workload at -O3 (1147
against 1148 ms) because -O3 already inlined `classify`; they matter at -Oz.

Flag for the lead: 0.1.1 peaks at 303.6 MiB against 0.1.0's 288.3 MiB here (4857 against
4613 pages, 41 against 37 grows). The eighth-of-heap policy overshoots by up to 12.5 percent
either way; which end of that range a run lands on depends on where the last step falls.

## 10. Commands

All paths relative to the scratchpad `prof-o3/` unless absolute; `$N24` is
`third_party/node-v24.20.0-linux-x64/bin/node`, `node` is 22.22.2; every engine run was
pinned with `taskset`. Worktrees of the simlin clone: `third_party/simlin-opt/simlin` (the
PR branch, untouched), `wt-dl` (global allocator cfg'd out), `wt-main`
(`[patch.crates-io] wasmalloc = { path = "/home/bpowers/src/wasm-clalloc" }` plus
`cargo update -p wasmalloc`), `wt-cnt` (patch to `wasmalloc-cnt`, a copy of main with
counters), `wt-ni` (patch to `wasmalloc-ni`, main with `#[inline(never)]` on the three fast
paths). Cargo target directories live inside those worktrees.

- C1: `touch src/libsimlin/src/lib.rs && CARGO_TARGET_DIR=$PWD/target CARGO_PROFILE_RELEASE_STRIP=debuginfo cargo build -v -p simlin --lib --release --target wasm32-unknown-unknown --no-default-features` in `wt-main`; the `Running` line for `--crate-name simlin`.
- C2: as C1 with `cargo rustc ... --crate-type cdylib -v` into `target-lto` (`walto`), plus `RUSTFLAGS="-C codegen-units=16"` into `target-lto16` (`walto16`); default features into `target-lto-full` for the node bundle; the same in `wt-ni` (`waltoni`), `wt-dl` (`dllto`) and `wt-cnt` (`wacntlto`).
- C3: `third_party/simlin-opt/inl-exp` (bump allocator) and `inl-exp/wa` (wasmalloc path dependency), cdylib, `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`; `wasm-tools demangle` then `wasm-tools print`; `cargo +nightly rustc --release --target wasm32-unknown-unknown -- -Zunpretty=expanded` for the macro expansion.
- C4: every raw build: `wasm-opt X.raw.wasm -o X.opt.wasm -O3 -g --enable-mutable-globals --enable-bulk-memory --enable-bulk-memory-opt --enable-nontrapping-float-to-int`, `wasm-tools demangle`, `wasm-tools print > X.wat`; `wasm-tools objdump` for section sizes; `wasm-tools strip -a` then `brotli -q 11 -c | wc -c` and `gzip -9 -c | wc -c`. Names builds: `CARGO_PROFILE_RELEASE_STRIP=debuginfo`, target dir `target-3-cgu1-names` in the PR worktree (`wa010`), `RUSTFLAGS="-C codegen-units=16"` into `target-3-names` (`wa010c16`).
- C5: in `third_party/simlin-opt/simlin/src/engine`: `cp -r core core.bak`, copy `walto.stripped.wasm` to `core/libsimlin-browser.wasm` and `walto-full.stripped.wasm` to `core/libsimlin.wasm`, `pnpm test`, again with `PATH=$N24dir:$PATH`, then `rm -rf core && mv core.bak core` (verified byte-identical to the PR's raw artifacts afterwards).
- C6: `taskset -c 4-7 $N24 --expose-gc src/engine/bench/clearn-alloc.mjs --ltm on --iters 5 --warmup 2 --json bench-n24-X.json a=... b=...` in the PR worktree (and `node`); runs `lto`, `lto2`, `lto16`, `ni`.
- C7: `taskset -c 4-7 $N24 profile-compile.mjs --iters 10 --warmup 2 --interval 100 --out prof-n24-X.json X.wasm` (the -Oz study's driver with `ENGINE` overridable); node 22 likewise; `--stage run --iters 6` (or 10) for section 8.
- C8: `node report.mjs A.json [B.json] --top N` (allocator groups, top frames, per-function difference; `Heap::alloc`/`dealloc` matchers added for the never-inlined bundle).
- C9: `patch-counters.py` applied to `wasmalloc-cnt/src` (a `counters` module: one `u64` per event, `#[unsafe(no_mangle)] wasmalloc_counters_ptr()` exported from the cdylib); `profile-compile.mjs --counters --noprofile --iters 2 --warmup 1 wacnt.wasm` (`counts-src.txt`; `wacntlto.wasm`, `counts-src-lto.txt`).
- C10: `node instrument.mjs wa011.wasm wa011-cnt.wasm` (the -Oz study's rewriter, crate hash updated) and `profile-compile.mjs --counters --noprofile --iters 2 --warmup 1` (`counts-txt.txt`); `dl-cnt.wasm` likewise for dlmalloc's `memory.grow` count.
- C11: `python3 fnsizes.py X.wasm IDX...` (code-section body sizes); `fn.sh X.wat IDX` for one function's text, instruction counts by `grep -v` of block/end/loop/else lines.
- C12: `taskset -c 12-15 $N24 --print-wasm-code profile-compile.mjs --sync --noprofile --iters 1 --warmup 1 X.wasm > asm/X-all-tiered.txt`; `asmfn2.sh DUMP IDX TurboFan` for one function's optimised code; `--print-wasm-code-function-index=IDX` on node 22 for `asm/wa011-486-n22.txt` and `-467-`.
- C13: `node sites.mjs counts-txt.txt wa011.wat asm/wa011-all-tiered.txt prof-n24-wa011.json '0x14d4bc]' '+0x16],0x0'` (per-caller calls, TurboFan bytes, inlined copies by signature, self ms).
- C14: `node instrument2.mjs walto.wasm walto-cnt2.wasm 1349788` (counters `iaf_f<idx>` before folded table loads, `iad_f<idx>` before dynamic ones, `id_f<idx>` before `free_is_zero` stores, `site_*` for residual shim calls), `profile-compile.mjs --counters --noprofile --iters 1 --warmup 1` (`counts-txt-lto2.txt`); `node sites2.mjs counts-txt-lto.txt walto.wat asm/walto-all-tiered.txt prof-n24-walto.json` for the ranked sites.
- C15: C7 with `--stage run` on `wa010.wasm`, `wa010c16.wasm`, `walto.wasm`, both engines; `node report.mjs` pairs.
- C16: `$N24 --print-wasm-code-function-index=3358 profile-compile.mjs --sync --noprofile --stage run --iters 1 --warmup 1 wa010.wasm` (2009 for `wa010c16`), node 22 likewise; instruction, spill (`[rbp-` stores) and call counts by grep over the TurboFan block.
