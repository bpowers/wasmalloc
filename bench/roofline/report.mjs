// Turn results/*.json into markdown tables.
//   node report.mjs [results-dir]
//
// Reads the matrix files (<engine>-<tier>-<variant>.json), the tier-up probes
// (tierup-*.json), the flag checks (flagcheck-*.json) and the shim study
// (shim-*.json, shim-inspect.txt) and prints markdown to stdout. Engine and
// tier come from the file name so that the self-timed (wasmtime, native)
// results and the JS-driven ones are treated alike.
import fs from 'node:fs';
import path from 'node:path';

const dir = process.argv[2] || path.join(path.dirname(new URL(import.meta.url).pathname), 'results');

const VARIANTS = ['bump', 'freelist', 'sizeclass', 'pages', 'dlmalloc', 'talc', 'lol_alloc'];
const CONFIGS = [
  ['node-default', 'node 22 (V8 12.4), default tiering'],
  ['node-liftoff', 'node 22 (V8 12.4), --liftoff-only'],
  ['node-turbofan', 'node 22 (V8 12.4), --no-liftoff'],
  ['d8-default', 'd8 (V8 15.2), default tiering'],
  ['d8-liftoff', 'd8 (V8 15.2), --liftoff-only'],
  ['d8-turbofan', 'd8 (V8 15.2), --no-liftoff'],
  ['jsc-default', 'JavaScriptCore, default'],
  ['wasmtime-cranelift', 'wasmtime, Cranelift (wasip1 binary)'],
  ['native-x86_64', 'native x86_64 (host build of the wasip1 binary)'],
];
const WORKLOADS = [
  'alloc_free_32',
  'batch_lifo_32',
  'batch_fifo_32',
  'churn',
  'vec_push_growth',
  'realloc_doubling',
  'large_alloc_free',
  'memory_grow_only',
  'memory_grow_touch',
];

function readJson(file) {
  try {
    return JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch (e) {
    return null;
  }
}

function fmt(x, digits) {
  if (x === null || x === undefined || Number.isNaN(x)) return '-';
  if (digits !== undefined) return x.toFixed(digits);
  if (x >= 100000) return Math.round(x).toLocaleString('en-US');
  if (x >= 1000) return x.toFixed(0);
  if (x >= 100) return x.toFixed(1);
  return x.toFixed(2);
}

function table(header, rows) {
  const out = [];
  out.push('| ' + header.join(' | ') + ' |');
  out.push('|' + header.map((h, i) => (i === 0 ? '---' : '---:')).join('|') + '|');
  for (const r of rows) out.push('| ' + r.join(' | ') + ' |');
  return out.join('\n');
}

// ---------------------------------------------------------------- matrix

const matrix = {}; // config -> variant -> report
for (const [cfg] of CONFIGS) {
  matrix[cfg] = {};
  for (const v of VARIANTS) {
    const r = readJson(path.join(dir, `${cfg}-${v}.json`));
    if (r) matrix[cfg][v] = r;
  }
}

function resultOf(cfg, v, wl) {
  const r = matrix[cfg] && matrix[cfg][v];
  if (!r) return null;
  return r.results.find((x) => x.workload === wl) || null;
}

function tierMark(res) {
  if (!res || !res.tier || res.tier === 'n/a') return '';
  if (res.tier === 'liftoff') return ' L';
  if (!res.tierStable) return ' ~';
  return '';
}

console.log('# Roofline results\n');
console.log(`Generated ${new Date().toISOString()} from ${dir}\n`);

for (const [cfg, title] of CONFIGS) {
  const present = VARIANTS.filter((v) => matrix[cfg][v]);
  if (present.length === 0) continue;
  const any = matrix[cfg][present[0]];
  console.log(`## ${title}\n`);
  console.log(`Engine: ${any.engineVersion || any.engine}; flags: ${any.flags || '(default)'}; ${any.reps} timed calls per cell.`);
  if (any.callOverheadNs) {
    const co = any.callOverheadNs;
    console.log(`JS-to-wasm call of an empty export: ${fmt(co.median)} ns median (subtracted once per timed call).`);
  }
  if (any.allocator) console.log(`Default allocator on this target: ${any.allocator}.`);
  console.log('');
  console.log('Median ns/op (min ns/op in parentheses). "L" marks a function still running Liftoff code; "~" marks a tier change during the timed calls.\n');
  const rows = WORKLOADS.map((wl) => {
    const row = [wl];
    for (const v of present) {
      const res = resultOf(cfg, v, wl);
      row.push(res ? `${fmt(res.medianNsPerOp)} (${fmt(res.minNsPerOp)})${tierMark(res)}` : '-');
    }
    return row;
  });
  console.log(table(['workload', ...present], rows));
  console.log('');
}

// ---------------------------------------------------------------- ratios

const RATIO_WORKLOADS = ['alloc_free_32', 'batch_lifo_32', 'batch_fifo_32', 'churn'];
const ratioRows = [];
for (const [cfg, title] of CONFIGS) {
  for (const wl of RATIO_WORKLOADS) {
    const bump = resultOf(cfg, 'bump', wl);
    const fl = resultOf(cfg, 'freelist', wl);
    const sc = resultOf(cfg, 'sizeclass', wl);
    const pg = resultOf(cfg, 'pages', wl);
    const dl = resultOf(cfg, 'dlmalloc', wl);
    const talc = resultOf(cfg, 'talc', wl);
    const lol = resultOf(cfg, 'lol_alloc', wl);
    if (!sc && !dl) continue;
    // The floor for the 32-byte workloads is the free list; churn's floor is
    // the size-class list (the single free list falls through to bump there).
    const floor = wl === 'churn' ? sc : fl;
    const f = floor ? floor.medianNsPerOp : null;
    const ratio = (r) => (r && f ? fmt(r.medianNsPerOp / f, 1) + 'x' : '-');
    ratioRows.push([
      title.replace(/ \(.*\)/, '').replace(/,.*/, '') + ' ' + cfg.split('-')[1],
      wl,
      fmt(bump && bump.medianNsPerOp),
      fmt(f),
      fmt(sc && sc.medianNsPerOp),
      fmt(pg && pg.medianNsPerOp),
      dl ? `${fmt(dl.medianNsPerOp)} (${ratio(dl)})` : '-',
      talc ? `${fmt(talc.medianNsPerOp)} (${ratio(talc)})` : '-',
      lol ? `${fmt(lol.medianNsPerOp)} (${ratio(lol)})` : '-',
    ]);
  }
}
if (ratioRows.length) {
  console.log('## Incumbents against the floor\n');
  console.log(
    'Median ns/op. The floor column is the single free list for the 32-byte workloads and the size-class free lists for churn; incumbents show their ratio to that floor in parentheses.\n'
  );
  console.log(table(['engine/tier', 'workload', 'bump', 'floor', 'sizeclass', 'pages', 'dlmalloc', 'talc', 'lol_alloc'], ratioRows));
  console.log('');
}

// ---------------------------------------------------------------- V8 12.4 vs 15.2

{
  const rows = [];
  for (const v of VARIANTS) {
    for (const wl of ['alloc_free_32', 'churn']) {
      const a = resultOf('node-turbofan', v, wl);
      const b = resultOf('d8-turbofan', v, wl);
      const la = resultOf('node-liftoff', v, wl);
      const lb = resultOf('d8-liftoff', v, wl);
      if (!a && !b) continue;
      rows.push([
        v,
        wl,
        fmt(a && a.medianNsPerOp),
        fmt(b && b.medianNsPerOp),
        a && b ? fmt(b.medianNsPerOp / a.medianNsPerOp, 2) + 'x' : '-',
        fmt(la && la.medianNsPerOp),
        fmt(lb && lb.medianNsPerOp),
        la && lb ? fmt(lb.medianNsPerOp / la.medianNsPerOp, 2) + 'x' : '-',
      ]);
    }
  }
  if (rows.length) {
    console.log('## V8 12.4 (node 22) against V8 15.2 (d8)\n');
    console.log('Median ns/op; ratio is 15.2 over 12.4 (below 1 means 15.2 is faster).\n');
    console.log(
      table(['variant', 'workload', 'TurboFan 12.4', 'TurboFan 15.2', 'ratio', 'Liftoff 12.4', 'Liftoff 15.2', 'ratio'], rows)
    );
    console.log('');
  }
}

// ---------------------------------------------------------------- tier-up probes and flag checks

{
  const files = fs.readdirSync(dir).filter((f) => /^tierup-.*\.json$/.test(f)).sort();
  const rows = [];
  for (const f of files) {
    const m = /^tierup-(node|d8)-(\w+)-n(\d+)\.json$/.exec(f);
    const r = readJson(path.join(dir, f));
    if (!m || !r || !r.tierup) continue;
    const t = r.tierup;
    rows.push({
      engine: m[1],
      variant: m[2],
      n: parseInt(m[3], 10),
      calls: t.callsUntilTierUp,
      iters: t.itersUntilTierUp,
      gaveUp: t.gaveUpAfterCalls,
      liftoffNs: t.liftoffNsPerOp,
      optNs: t.optimizedNsPerOp,
      version: r.engineVersion,
    });
  }
  if (rows.length) {
    rows.sort((a, b) => a.engine.localeCompare(b.engine) || a.variant.localeCompare(b.variant) || a.n - b.n);
    console.log('## Calls before V8 leaves Liftoff (alloc_free_32(n), fresh process per row)\n');
    console.log(
      'Under default dynamic tiering. "calls" is the first call after which the export was no longer Liftoff code; "iters" is calls times n. Per-op times here include a reset() call and a tier query per call, so compare them with each other, not with the matrix.\n'
    );
    console.log(
      table(
        ['engine', 'variant', 'n per call', 'calls', 'iters', 'Liftoff ns/op', 'optimized ns/op'],
        rows.map((r) => [
          `${r.engine} (${r.version})`,
          r.variant,
          r.n.toLocaleString('en-US'),
          r.calls === null ? `gave up after ${r.gaveUp.toLocaleString('en-US')}` : r.calls.toLocaleString('en-US'),
          r.iters === null ? '-' : r.iters.toLocaleString('en-US'),
          fmt(r.liftoffNs),
          fmt(r.optNs),
        ])
      )
    );
    console.log('');
  }

  const checks = fs.readdirSync(dir).filter((f) => /^flagcheck-.*\.json$/.test(f)).sort();
  const crow = [];
  for (const f of checks) {
    const m = /^flagcheck-(node|d8)-(.*)-(sizeclass|dlmalloc)\.json$/.exec(f);
    const r = readJson(path.join(dir, f));
    if (!m || !r) continue;
    const af = r.results.find((x) => x.workload === 'alloc_free_32');
    const ch = r.results.find((x) => x.workload === 'churn');
    crow.push([
      `${m[1]} (${r.engineVersion})`,
      r.flags,
      m[3],
      af ? `${af.tier} after ${af.warmupCalls} calls (${af.liftoffWarmupCalls} in Liftoff), ${fmt(af.medianNsPerOp)} ns` : '-',
      ch ? `${ch.tier}, ${fmt(ch.medianNsPerOp)} ns` : '-',
    ]);
  }
  if (crow.length) {
    console.log('## Which flags pin Liftoff\n');
    console.log(table(['engine', 'flags', 'variant', 'alloc_free_32 tier observed', 'churn'], crow));
    console.log('');
  }
}

// ---------------------------------------------------------------- shim study

{
  const files = fs.readdirSync(dir).filter((f) => /^shim-.*\.json$/.test(f)).sort();
  const rows = [];
  for (const f of files) {
    const m = /^shim-(node|d8)-(\w+)-(release(?:-nolto|-z|-z-nolto)?)(-wasmopt)?-(\w+)\.json$/.exec(f);
    const r = readJson(path.join(dir, f));
    if (!m || !r) continue;
    const af = r.results.find((x) => x.workload === 'alloc_free_32');
    const ch = r.results.find((x) => x.workload === 'churn');
    rows.push({
      engine: `${m[1]} ${m[2]}`,
      variant: m[5],
      profile: m[3] + (m[4] || ''),
      wasm: r.wasm,
      af,
      ch,
    });
  }
  if (rows.length) {
    rows.sort(
      (a, b) => a.variant.localeCompare(b.variant) || a.engine.localeCompare(b.engine) || a.profile.localeCompare(b.profile)
    );
    console.log('## Shim indirection: build profile against the fast path\n');
    console.log(
      'Median ns/op (min). release = opt-level 3, fat LTO, one codegen unit; -nolto = no LTO, 16 codegen units; -z = opt-level z; -wasmopt = the same file after wasm-opt -O3.\n'
    );
    const size = {};
    try {
      for (const line of fs.readFileSync(path.join(dir, 'shim-inspect.txt'), 'utf8').split('\n')) {
        const m = /^(\w+)-(\S+)\.wasm: (\d+) bytes/.exec(line);
        if (m) size[`${m[1]}-${m[2]}`] = parseInt(m[3], 10);
      }
    } catch (e) {
      // no inspection file
    }
    console.log(
      table(
        ['variant', 'profile', 'bytes', 'engine', 'alloc_free_32', 'churn'],
        rows.map((r) => [
          r.variant,
          r.profile,
          size[`${r.variant}-${r.profile}`] ? size[`${r.variant}-${r.profile}`].toLocaleString('en-US') : '-',
          r.engine,
          r.af ? `${fmt(r.af.medianNsPerOp)} (${fmt(r.af.minNsPerOp)})${tierMark(r.af)}` : '-',
          r.ch ? `${fmt(r.ch.medianNsPerOp)} (${fmt(r.ch.minNsPerOp)})${tierMark(r.ch)}` : '-',
        ])
      )
    );
    console.log('');
    try {
      console.log('### Call structure of the hot loops\n');
      console.log('```');
      console.log(fs.readFileSync(path.join(dir, 'shim-inspect.txt'), 'utf8').trimEnd());
      console.log('```\n');
    } catch (e) {
      // no inspection file
    }
  }
}
