# Roofline harness

Measures what an allocation costs on single-threaded wasm32, per engine and per
JIT tier, for four harness "floor" allocators (the cheapest code that can serve a
workload), the incumbents (std's dlmalloc-rs, talc, lol_alloc) and wasmalloc.
The results and their interpretation live in `docs/research/roofline.md`; the
tables are regenerated into `results/REPORT.md`.

## Layout

- `Cargo.toml`: a standalone crate (its own profiles and lockfile). The
  allocator is a cargo feature: `bump`, `freelist`, `sizeclass`, `pages` (floors,
  in `src/alloc/`), `mimic`, `mimic_lean`, `mimic_u32`, `mimic_nozero`, `mimic_notest`,
  `mimic_u32_notest` (floors that
  reproduce wasmalloc's fast-path memory traffic with one detail changed each,
  see `src/alloc/mimic.rs`), `talc`, `lol_alloc`, `wasmalloc`, `wasmalloc_count`
  (wasmalloc's heap behind a `memory.grow`-counting backend, for the `grows` column
  and the `growCalls*` fields; timings are only close to `wasmalloc`'s), or none for std's
  default (`dlmalloc-rs` on wasm32-unknown-unknown, wasi-libc's C dlmalloc on
  wasm32-wasip1, libc malloc natively). The features are mutually exclusive.
- `src/workloads.rs`: the workloads; `src/lib.rs`: the C ABI exports the JS
  drivers call; `src/bin/roofline-wasi.rs`: the self-timing binary for wasmtime
  and native.
- `harness.js`: the protocol (warm-up, timing, tier query, footprint) shared by
  `run.mjs` (node), `run-shell.js` (d8, jsc); `run-wasi.mjs` runs the wasip1
  binary under node's WASI.
- `inspect.mjs`: call structure of the hot functions from the text format.
- `run-all.sh`: builds everything and runs the matrix; `report.mjs` renders
  `results/*.json` into `results/REPORT.md`.

## Prerequisites

Rust 1.95 with the `wasm32-unknown-unknown` and `wasm32-wasip1` targets, node 22,
`~/.jsvu/bin/v8-15.2.124` and `~/.jsvu/bin/jsc` (from `jsvu`),
`~/.wasmtime/bin/wasmtime`, `wasm-opt` (Binaryen 125), `wasm-tools`
(`cargo install wasm-tools --locked`), `jq`, `taskset`. Paths are overridable
through `NODE`, `D8`, `JSC`, `WASMTIME`, `WASM_OPT`, `WASM_TOOLS`.

## Reproduce

From this directory. Every engine run is pinned to CPUs 4-7 (`ROOFLINE_CPUS`,
empty to disable) and wrapped in `scripts/memlimit` (`ROOFLINE_MEM`, default
4G; `ROOFLINE_TIMEOUT`, default 20m). The whole run takes about ten minutes on the
reference machine (lol_alloc's linear free-list walk accounts for a third of
it); `ROOFLINE_VARIANTS` restricts every step to a subset of variants so it can
be split into shorter pieces:

```
./run-all.sh                     # build sizes matrix footprint tierup shim report
./run-all.sh build sizes         # builds only
ROOFLINE_VARIANTS="wasmalloc talc" ./run-all.sh matrix footprint report
./run-all.sh report              # regenerate results/REPORT.md from results/
```

One configuration by hand, table output:

```
cargo build --release --target wasm32-unknown-unknown --lib --features wasmalloc
node --allow-natives-syntax --no-liftoff run.mjs target/wasm32-unknown-unknown/release/roofline.wasm
node --allow-natives-syntax --liftoff-only run.mjs --only alloc_free_32,churn --tier-hint liftoff \
    target/wasm32-unknown-unknown/release/roofline.wasm
~/.jsvu/bin/v8-15.2.124 --allow-natives-syntax --no-liftoff run-shell.js -- --json \
    target/wasm32-unknown-unknown/release/roofline.wasm
~/.jsvu/bin/jsc run-shell.js -- target/wasm32-unknown-unknown/release/roofline.wasm
cargo build --release --target wasm32-wasip1 --bin roofline-wasi --features wasmalloc
~/.wasmtime/bin/wasmtime run target/wasm32-wasip1/release/roofline-wasi.wasm
cargo build --release --bin roofline-wasi --features sizeclass && target/release/roofline-wasi
```

`memlimit` prints one line per run with the command's exit status and, where the
systemd journal recorded it, the run's peak memory; set `MEMLIMIT_QUIET=1` to
silence it.

`--allow-natives-syntax` only enables the per-function tier query
(`%IsLiftoffFunction`); on V8 use `--no-liftoff` for optimizing-tier numbers and
`--liftoff-only` for baseline-tier numbers. `--liftoff --no-wasm-tier-up` does
not pin Liftoff on node 22 (dynamic tiering is a separate flag); the `tierup`
step records what each recipe actually does.

Tier-up probe (calls of `alloc_free_32(n)` until V8 swaps in optimized code, one
process per `n` because V8 caches compiled modules by wire bytes):

```
node --allow-natives-syntax run.mjs --tierup 1000 target/wasm32-unknown-unknown/release/roofline.wasm
```

Call structure of the hot loops:

```
node inspect.mjs target/dist/wasmalloc-release.wasm '^alloc_free_32$' '^churn$' '__rust_alloc$' '__rust_dealloc$'
```

## Protocol

Per workload: `reset()` (rewinds the floor allocators; a no-op for real
allocators), the workload's setup, the timed call, the teardown; warm up until
the last three calls agree within 10 percent (3 to 12 calls, up to 30 while V8
still reports Liftoff code), then 7 timed calls (`ROOFLINE_REPS`); median and
min ns per op are reported with the cost of one empty JS-to-wasm call subtracted
from each timed call. `memory.size` is recorded before and after each workload;
the `footprint` step runs one workload per process so that the pages one
workload pulls in are not attributed to another. The workload loops are inlined
into the exports so that V8's per-function tier applies to the code that runs.

## Workloads

| name | per op | what |
|---|---|---|
| alloc_free_32 | alloc+free pair | 32 B, align 8, immediately freed: the cache-hot fast path |
| alloc_free_32_align16 | alloc+free pair | the same with align 16 (`v128`); dlmalloc-rs takes its memalign path |
| batch_lifo_32, batch_fifo_32 | alloc+free pair | 1000 allocations then 1000 frees in LIFO or FIFO order |
| churn | free+alloc step | 10,000 live objects of 16..1024 B; each step frees a random one and allocates a random replacement |
| random_actions | action | talc-style: 3/7 alloc (1..=10000 B biased small, align 8 mostly, 16/32/64 sometimes), 3/7 free, 1/7 realloc, 100-object floor |
| random_actions_norealloc | action | the same without realloc (1/2 alloc, 1/2 free) |
| vec_push_growth | 1 MiB Vec | `Vec<u8>::push` from empty to 1 MiB |
| realloc_doubling | chain | realloc by doubling from 16 B to 1 MiB (16 reallocs) |
| large_alloc_free | alloc+touch+free | 256 KiB to 4 MiB, one byte touched per 4 KiB |
| memory_grow_only, memory_grow_touch | grow | `memory.grow` of 1 MiB, untouched or touched once per 4 KiB |

Adding a workload means adding it to `src/workloads.rs`, `src/lib.rs`, the
`WORKLOADS` lists in `harness.js` and `src/bin/roofline-wasi.rs`, and
`report.mjs`.
