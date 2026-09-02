# Fuzz targets

Coverage-guided fuzzing of the model tester (`wasmalloc::testing::model`) with cargo-fuzz and
libFuzzer. This crate is excluded from the root workspace because it needs nightly and
`libfuzzer-sys`; the merge gate never builds it.

Each target reads one byte to pick a profile and treats the rest of the input as the operation
stream, so every mutation the fuzzer makes is a mutation of the sequence of allocations, frees
and reallocs. A run ends when the input is exhausted and frees every block it still holds.

- `model`: the stream against `std::alloc::System`. A correct allocator must never fail the
  model, so a crash here is a bug in the tester itself.
- `model_heap`: the same stream against `Heap<SimMemory>` over a fresh 256 MiB host region
  (committed lazily). This is the target that hunts allocator bugs.

## Running

Always under the memory cap (an unbounded fuzzer can take the machine down; see `CLAUDE.md`):

```sh
cargo install cargo-fuzz          # once; needs the nightly toolchain
scripts/memlimit -m 4G -t 70s -- cargo +nightly fuzz run model_heap -- -max_total_time=60
scripts/memlimit -m 4G -t 70s -- cargo +nightly fuzz run model      -- -max_total_time=60
```

Run from the repository root. Drop `-max_total_time` for an open-ended session and raise `-t`
to match; add `-jobs=N` to use N cores. libFuzzer's own `-rss_limit_mb` (default 2048) stays
inside the cgroup cap, so a runaway input is reported by libFuzzer rather than OOM-killed.

Crashing inputs land in `fuzz/artifacts/<target>/`. Reproduce and minimise with:

```sh
cargo +nightly fuzz run model_heap fuzz/artifacts/model_heap/crash-<hash>
cargo +nightly fuzz tmin model_heap fuzz/artifacts/model_heap/crash-<hash>
```

The panic message names the violated property, the operation index and the profile; the same
operation stream can be replayed under a debugger by feeding the artifact bytes to
`model::run_with` with a `ByteSource`.

The corpus in `fuzz/corpus/<target>/` is not checked in; a fresh fuzzer finds the interesting
paths within seconds because the input decoding is dense (four bytes per decision).
