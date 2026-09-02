# wasmalloc

Last verified: 2026-09-01

A pure-Rust, `no_std` `#[global_allocator]` for single-threaded wasm32, designed after
mimalloc v3. Goal: the fastest allocator for Rust programs running in browsers (V8 first,
JavaScriptCore second) and in wasmtime. Speed is the priority; RSS and code size are
secondary (smaller is welcome, never at the cost of speed). Multi-threaded wasm, native
production use, and a C malloc API are out of scope.

Design document: `docs/design/2026-09-01-wasmalloc.md` (read it before touching `src/`).
Research reports: `docs/research/`. mimalloc v3.5.1 reference source: `~/src/mimalloc-v3`.

## Tech stack

- Rust 1.95 stable (nightly only for Miri and tools that need it); targets
  `wasm32-unknown-unknown`, `wasm32-wasip1`, and the host for tests.
- Engines: node 22 (V8) at `node`, `~/.wasmtime/bin/wasmtime`; `wasm-opt` 125.
- Verification: Kani harnesses, Miri, differential fuzzing (see `docs/research/verification.md`).
  Run Kani ONLY through `scripts/kani` (never bare `cargo kani`): it caps memory with a cgroup
  (default 6 GiB, `KANI_MEM`) and time (default 30 min, `KANI_TIMEOUT`). An uncapped CBMC run
  once took the whole machine down. Wrap any other memory-hungry tool (fuzzers, wasm-opt on
  huge inputs) in `scripts/memlimit -m SIZE -t TIME -- cmd`.

## Commands

All from the repo root (or a worktree root). `wasmtime` lives in `~/.wasmtime/bin`; put it on
PATH first: `export PATH="$HOME/.wasmtime/bin:$PATH"`.

- Gate (must pass before asking for a merge):
  `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test && cargo test --target wasm32-wasip1 && cargo build --release --target wasm32-unknown-unknown && scripts/kani`
- One Kani harness: `scripts/kani --harness <name>` (never bare `cargo kani`).
- Worktree for a task: `git worktree add .worktrees/<task> -b <task> main`, work and commit
  there, then tell the lead the branch name. The lead merges into `main` and removes the worktree.
- Roofline/benchmarks: see `bench/roofline/README.md`.

## Project structure

- `src/` - the `wasmalloc` crate: `bins` (size-class math), `slices` (64 KiB slice bitmap and
  memory growth), `page`, `heap` (queues, direct table, retire), `alloc` (GlobalAlloc impl and
  fast paths), `backend` (the `Memory` trait: wasm `memory.grow` or a host-side simulated
  linear memory).
- `bench/` - benchmark harness (roofline and workloads; allocator chosen by cargo feature).
- `scripts/` - `kani` and `memlimit` wrappers (mandatory for verifier runs).
- `fuzz/`, `verify/` - fuzz targets and Kani harnesses.
- `docs/design/`, `docs/research/`, `docs/soundness-ledger.md`.

## How we work

- Trunk-based. Work in a git worktree on a short-lived branch; the lead reviews and merges to
  `main`. Never commit directly to `main` from a worker branch.
- Merge gate is fast on purpose: `cargo fmt --check`, `cargo clippy -D warnings`, host tests,
  wasm32-wasip1 tests under wasmtime, and the quick Kani harness set. It must stay well under
  a minute. Full benchmarks and long proofs run on demand, never as a gate.
- Commit messages: imperative subject, body explains why. No emoji anywhere (commits, docs,
  code). No "Generated with Claude Code" or "Co-Authored-By" trailers.
- Comments explain why or something non-obvious, never restate the next line of code.
- Benchmark before and after any change to a hot path; record numbers in the PR description.

## Correctness rules (non-negotiable)

- Formal verification first. Every `unsafe` block should be covered by a machine-checked proof
  (a Kani harness over the simulated memory backend, or equivalent) that exercises its
  preconditions. Unsafe code with no proof is the exception, not the norm.
- Soundness ledger for the rest. Any `unsafe` block that formal tools cannot reach gets an
  entry in `docs/soundness-ledger.md`: exact preconditions, the invariants that make them hold,
  a pen-and-paper proof, and a sign-off from a fresh adversarial reviewer agent who did not
  write the code. Changing such a block means updating its entry and getting a fresh review.
- `#![no_std]`, `#![deny(unsafe_op_in_unsafe_fn)]`, minimal and concentrated unsafe surface.
  Prefer `usize` offset arithmetic and `ptr.map_addr`/`with_addr` over int-to-pointer casts so
  Miri (strict provenance) and Kani can follow the code.
- The allocator must never allocate, panic, unwind, or print while servicing a request.
  Debug assertions are fine; they abort in wasm.
- The core is generic over the `Memory` backend so every test, fuzz target and proof runs on
  the host against a 4 MiB-aligned simulated linear memory; wasm-specific code is confined to
  the backend.

## Design invariants (see the design doc for the reasoning)

- Page kind and page header address are pure functions of the `Layout`: small pages are
  64 KiB-aligned, medium 512 KiB-aligned, large 4 MiB-aligned; singleton runs have no header.
  No page map on any hot path.
- Alignment up to 4 KiB is satisfied by construction (round size up to a multiple of align,
  bin it, and start blocks at an offset aligned to the largest power of two dividing the bin
  size). No interior pointers.
- One thread: no atomics, no TLS, no locks, no thread-free lists, no page abandonment.
- Fast paths are tiny and `#[inline]`; slow paths are `#[cold] #[inline(never)]`.

## Boundaries

- Safe to edit: `src/`, `bench/`, `fuzz/`, `verify/`, `docs/`.
- Do not edit `~/src/mimalloc-v3` (read-only reference) or files outside this repo.
