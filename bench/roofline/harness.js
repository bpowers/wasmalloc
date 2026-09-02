// Engine-agnostic benchmark core shared by run.mjs (node), run-shell.js (d8,
// JavaScriptCore) and mirrored by src/bin/roofline-wasi.rs. Classic script:
// it only assigns globalThis.Roofline so it can be loaded everywhere.
//
// Protocol per workload: warm up until two consecutive calls agree within 10
// percent (at least MIN_WARM calls, at most MAX_WARM), then time REPS calls
// and report median and min ns per op. Every timed call is preceded by
// reset() (rewinds the floor allocators; no-op otherwise) and the workload's
// setup, and followed by its teardown; only the workload call itself is timed.
(function () {
  'use strict';

  // n is the argument passed per call; ops(n) is how many alloc/free (or
  // equivalent) operations that call performs, for the per-op figure.
  const WORKLOADS = [
    { name: 'alloc_free_32', n: 2000000, ops: (n) => n, unit: 'alloc+free pair' },
    { name: 'batch_lifo_32', n: 2000, ops: (n) => n * 1000, unit: 'alloc+free pair' },
    { name: 'batch_fifo_32', n: 2000, ops: (n) => n * 1000, unit: 'alloc+free pair' },
    { name: 'churn', n: 200000, ops: (n) => n, unit: 'free+alloc step', setup: 'churn_init', teardown: 'churn_fini' },
    { name: 'vec_push_growth', n: 20, ops: (n) => n, unit: '1 MiB Vec<u8> growth' },
    { name: 'realloc_doubling', n: 100, ops: (n) => n, unit: '16 B to 1 MiB realloc chain' },
    { name: 'large_alloc_free', n: 50, ops: (n) => n, unit: 'alloc+touch+free, 256K-4M' },
    { name: 'memory_grow_only', n: 16, ops: (n) => n, unit: 'memory.grow 1 MiB, untouched' },
    { name: 'memory_grow_touch', n: 16, ops: (n) => n, unit: 'memory.grow 1 MiB + touch' },
  ];

  const REPS = 7;
  const MIN_WARM = 3;
  const MAX_WARM = 12;
  const NOOP_CALLS = 1000000;

  function parseArgs(argv) {
    const opts = { json: false, reps: REPS, scale: 1, only: null, wasm: null, note: null };
    for (let i = 0; i < argv.length; i++) {
      const a = argv[i];
      if (a === '--json') opts.json = true;
      else if (a === '--reps') opts.reps = parseInt(argv[++i], 10);
      else if (a === '--scale') opts.scale = parseFloat(argv[++i]);
      else if (a === '--only') opts.only = argv[++i].split(',');
      else if (a === '--note') opts.note = argv[++i];
      else if (a.startsWith('--')) throw new Error('unknown option ' + a);
      else opts.wasm = a;
    }
    if (!opts.wasm) throw new Error('usage: [--json] [--reps N] [--scale F] [--only a,b] [--note text] file.wasm');
    return opts;
  }

  function instantiate(bytes) {
    const module = new WebAssembly.Module(bytes);
    const instance = new WebAssembly.Instance(module, {});
    return instance.exports;
  }

  function variantName(exports) {
    const ptr = exports.variant_name_ptr();
    const len = exports.variant_name_len();
    const view = new Uint8Array(exports.memory.buffer, ptr, len);
    let s = '';
    for (let i = 0; i < len; i++) s += String.fromCharCode(view[i]);
    return s;
  }

  function median(sorted) {
    return sorted[sorted.length >> 1];
  }

  // Cost of one JS-to-wasm call of an empty export, in ns.
  function measureCallOverhead(exports, env) {
    const noop = exports.noop;
    for (let i = 0; i < NOOP_CALLS; i++) noop();
    const samples = [];
    for (let r = 0; r < 5; r++) {
      const t0 = env.now();
      for (let i = 0; i < NOOP_CALLS; i++) noop();
      samples.push(((env.now() - t0) * 1e6) / NOOP_CALLS);
    }
    samples.sort((a, b) => a - b);
    return { median: median(samples), min: samples[0], tier: env.tierOf(noop) };
  }

  function measure(exports, wl, opts, env, callNs) {
    const fn = exports[wl.name];
    if (typeof fn !== 'function') throw new Error('missing export ' + wl.name);
    const setup = wl.setup ? exports[wl.setup] : null;
    const teardown = wl.teardown ? exports[wl.teardown] : null;
    const n = Math.max(1, Math.round(wl.n * opts.scale));
    const ops = wl.ops(n);
    let checksum = 0;

    const call = () => {
      exports.reset();
      if (setup) checksum ^= setup();
      const t0 = env.now();
      const r = fn(n);
      const t1 = env.now();
      checksum ^= r;
      if (teardown) checksum ^= teardown();
      return (t1 - t0) * 1e6;
    };

    let prev = call();
    let warm = 1;
    for (;;) {
      const cur = call();
      warm++;
      const stable = Math.abs(cur - prev) / Math.max(prev, 1) < 0.1;
      prev = cur;
      if ((warm >= MIN_WARM && stable) || warm >= MAX_WARM) break;
    }

    const samples = [];
    for (let r = 0; r < opts.reps; r++) samples.push((call() - callNs) / ops);
    samples.sort((a, b) => a - b);
    return {
      workload: wl.name,
      unit: wl.unit,
      n,
      opsPerCall: ops,
      medianNsPerOp: median(samples),
      minNsPerOp: samples[0],
      warmupCalls: warm,
      tier: env.tierOf(fn),
      checksum: checksum >>> 0,
    };
  }

  function run(env) {
    const opts = parseArgs(env.args);
    const exports = instantiate(env.readWasm(opts.wasm));
    const variant = variantName(exports);
    const callOverhead = measureCallOverhead(exports, env);
    const results = [];
    for (const wl of WORKLOADS) {
      if (opts.only && !opts.only.includes(wl.name)) continue;
      results.push(measure(exports, wl, opts, env, callOverhead.median));
    }
    const report = {
      engine: env.engine,
      engineVersion: env.version,
      flags: env.flags,
      note: opts.note,
      wasm: opts.wasm,
      variant,
      reps: opts.reps,
      scale: opts.scale,
      callOverheadNs: callOverhead,
      results,
    };
    if (opts.json) {
      env.print(JSON.stringify(report, null, 2));
    } else {
      env.print(formatTable(report));
    }
    return report;
  }

  function pad(s, w, right) {
    s = String(s);
    return right ? s.padStart(w) : s.padEnd(w);
  }

  function formatTable(report) {
    const lines = [];
    lines.push(
      `engine: ${report.engine} ${report.engineVersion || ''}  flags: ${report.flags || '(default)'}  variant: ${report.variant}  wasm: ${report.wasm}`
    );
    lines.push(
      `js->wasm call: median ${report.callOverheadNs.median.toFixed(2)} ns, min ${report.callOverheadNs.min.toFixed(2)} ns (${report.callOverheadNs.tier})`
    );
    lines.push(
      pad('workload', 20) + pad('ops/call', 12, true) + pad('median ns/op', 14, true) + pad('min ns/op', 12, true) + pad('warm', 6, true) + '  tier'
    );
    for (const r of report.results) {
      lines.push(
        pad(r.workload, 20) +
          pad(r.opsPerCall, 12, true) +
          pad(r.medianNsPerOp.toFixed(2), 14, true) +
          pad(r.minNsPerOp.toFixed(2), 12, true) +
          pad(r.warmupCalls, 6, true) +
          '  ' +
          r.tier
      );
    }
    return lines.join('\n');
  }

  globalThis.Roofline = { WORKLOADS, run, formatTable, parseArgs };
})();
