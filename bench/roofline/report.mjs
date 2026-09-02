// Turn results/*.json into markdown tables.
//   node report.mjs [results-dir]
//
// Reads the matrix files (<engine>-<tier>-<variant>.json), the footprint runs
// (footprint-<variant>-<workload>.json), the size records (size-<variant>.json),
// the tier-up probes (tierup-*.json), the flag checks (flagcheck-*.json) and the
// shim study (shim-*.json, shim-inspect-*.txt) and prints markdown to stdout.
// Engine and tier come from the file name so that the self-timed (wasmtime,
// native) results and the JS-driven ones are treated alike.
import fs from 'node:fs';
import path from 'node:path';

const dir = process.argv[2] || path.join(path.dirname(new URL(import.meta.url).pathname), 'results');

const VARIANTS = ['bump', 'freelist', 'sizeclass', 'pages', 'mimic', 'mimic_lean', 'mimic_u32', 'mimic_nozero', 'dlmalloc', 'talc', 'lol_alloc', 'wasmalloc'];
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
  'alloc_free_32_align16',
  'batch_lifo_32',
  'batch_fifo_32',
  'churn',
  'random_actions',
  'random_actions_norealloc',
  'vec_push_growth',
  'realloc_doubling',
  'large_alloc_free',
  'memory_grow_only',
  'memory_grow_touch',
];
// The floor each workload is compared against: the single free list for the
// 32-byte workloads, the size-class lists for churn and random actions (which
// leak everything above 1 KiB there, so that floor is a bound, not a target),
// and the bump pointer where no floor is honest.
const FLOOR_OF = {
  alloc_free_32: 'freelist',
  alloc_free_32_align16: 'freelist',
  batch_lifo_32: 'freelist',
  batch_fifo_32: 'freelist',
  churn: 'sizeclass',
  random_actions: 'sizeclass',
  random_actions_norealloc: 'sizeclass',
  vec_push_growth: 'bump',
  realloc_doubling: null,
  large_alloc_free: 'bump',
};
const RATIO_WORKLOADS = Object.keys(FLOOR_OF);
const FOOTPRINT_WORKLOADS = RATIO_WORKLOADS;

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

function shortTitle(cfg, title) {
  return title.replace(/ \(.*\)/, '').replace(/,.*/, '') + ' ' + cfg.split('-')[1];
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

// ---------------------------------------------------------------- wasmalloc against floor and incumbents

{
  const rows = [];
  for (const [cfg, title] of CONFIGS) {
    for (const wl of RATIO_WORKLOADS) {
      const wa = resultOf(cfg, 'wasmalloc', wl);
      const dl = resultOf(cfg, 'dlmalloc', wl);
      const talc = resultOf(cfg, 'talc', wl);
      const lol = resultOf(cfg, 'lol_alloc', wl);
      if (!wa && !dl) continue;
      const floorName = FLOOR_OF[wl];
      const floor = floorName ? resultOf(cfg, floorName, wl) : null;
      const f = floor ? floor.medianNsPerOp : null;
      const w = wa ? wa.medianNsPerOp : null;
      const over = (r) => (r && w ? fmt(r.medianNsPerOp / w, 2) + 'x' : '-');
      rows.push([
        shortTitle(cfg, title),
        wl,
        floor ? `${fmt(f)} (${floorName})` : '-',
        wa ? fmt(w) + tierMark(wa) : '-',
        f && w ? fmt(w / f, 2) + 'x' : '-',
        dl ? `${fmt(dl.medianNsPerOp)} (${over(dl)})` : '-',
        talc ? `${fmt(talc.medianNsPerOp)} (${over(talc)})` : '-',
        lol ? `${fmt(lol.medianNsPerOp)} (${over(lol)})` : '-',
      ]);
    }
  }
  if (rows.length) {
    console.log('## wasmalloc against the floor and the incumbents\n');
    console.log(
      'Median ns/op. "floor" is the harness allocator named in parentheses (the single free list for the 32-byte workloads, the size-class lists for churn and random actions, the bump pointer for the growth workloads; the size-class floor leaks everything above 1 KiB, so for random actions it is a bound rather than a target). "wasmalloc/floor" is how far wasmalloc sits above the floor; the incumbent columns show their time and, in parentheses, how many times slower than wasmalloc they are (below 1x means the incumbent is faster).\n'
    );
    console.log(
      table(['engine/tier', 'workload', 'floor', 'wasmalloc', 'wasmalloc/floor', 'dlmalloc', 'talc', 'lol_alloc'], rows)
    );
    console.log('');
  }
}

// ---------------------------------------------------------------- floors against each other

{
  const rows = [];
  for (const [cfg, title] of CONFIGS) {
    for (const wl of ['alloc_free_32', 'alloc_free_32_align16', 'batch_lifo_32', 'batch_fifo_32', 'churn']) {
      const cells = ['bump', 'freelist', 'sizeclass', 'pages', 'mimic_lean', 'mimic_u32', 'mimic_nozero', 'mimic', 'wasmalloc'].map((v) => resultOf(cfg, v, wl));
      if (cells.every((c) => !c)) continue;
      rows.push([shortTitle(cfg, title), wl, ...cells.map((c) => (c ? fmt(c.medianNsPerOp) + tierMark(c) : '-'))]);
    }
  }
  if (rows.length) {
    console.log('## The floors, and the two mimics of wasmalloc\'s fast path\n');
    console.log(
      'Median ns/op of the harness allocators: bump (no free), one LIFO free list, 64 size-class lists keyed by the Layout, size classes recovered from a 64 KiB page header on free, then the mimics of wasmalloc\'s fast path: mimic_lean (a direct table pointing at a page header that holds only the free list head), mimic_u32 (plus a 32-bit used count, the free_is_zero byte and the flags test), mimic_nozero (a 16-bit used count and the flags test, no free_is_zero store) and mimic (wasmalloc\'s exact header traffic: 16-bit used count, free_is_zero store, flags test), against wasmalloc itself.\n'
    );
    console.log(table(['engine/tier', 'workload', 'bump', 'freelist', 'sizeclass', 'pages', 'mimic_lean', 'mimic_u32', 'mimic_nozero', 'mimic', 'wasmalloc'], rows));
    console.log('');
  }
}

// ---------------------------------------------------------------- footprint

{
  const foot = {}; // variant -> workload -> {start, before, after}
  let any = false;
  for (const v of VARIANTS) {
    for (const wl of FOOTPRINT_WORKLOADS) {
      const r = readJson(path.join(dir, `footprint-${v}-${wl}.json`));
      if (!r || !r.results || !r.results.length) continue;
      any = true;
      foot[v] = foot[v] || {};
      foot[v][wl] = { start: r.memoryPagesAtStart, before: r.results[0].pagesBefore, after: r.results[0].pagesAfter };
    }
  }
  if (any) {
    const present = VARIANTS.filter((v) => foot[v]);
    console.log('## Footprint: memory.size after one workload in a fresh process\n');
    console.log(
      'Pages of 64 KiB. Each cell is `memory.size` after the workload ran its warm-up and timed calls in a process of its own (node --no-liftoff; the tier does not matter for footprint). "start" is `memory.size` right after instantiation, before any allocation: the linker-set initial memory. In parentheses, the ratio of the pages the workload added (after minus start) to what dlmalloc added.\n'
    );
    const dl = foot.dlmalloc || {};
    const rows = [];
    rows.push(['start', ...present.map((v) => fmt(Object.values(foot[v])[0].start, 0))]);
    for (const wl of FOOTPRINT_WORKLOADS) {
      const row = [wl];
      for (const v of present) {
        const c = foot[v][wl];
        if (!c) {
          row.push('-');
          continue;
        }
        const added = c.after - c.start;
        const dlAdded = dl[wl] ? dl[wl].after - dl[wl].start : null;
        const ratio = dlAdded ? ` (${fmt(added / dlAdded, 2)}x)` : '';
        row.push(`${fmt(c.after, 0)}${v === 'dlmalloc' ? '' : ratio}`);
      }
      rows.push(row);
    }
    console.log(table(['workload', ...present], rows));
    console.log('');
  }
}

// ---------------------------------------------------------------- sizes

{
  const rows = [];
  let bump = null;
  for (const v of VARIANTS) {
    const r = readJson(path.join(dir, `size-${v}.json`));
    if (!r) continue;
    if (v === 'bump') bump = r;
    rows.push(r);
  }
  if (rows.length) {
    console.log('## Module size\n');
    console.log(
      'Bytes of the wasm32-unknown-unknown harness module (release profile: opt-level 3, fat LTO, one codegen unit, panic=abort, debuginfo stripped) before and after `wasm-opt -O3 --all-features`. The module contains the workloads and the parts of std they pull in, so the difference from the bump variant is the allocator\'s own contribution: its code plus any data segment its static state needs.\n'
    );
    console.log(
      table(
        ['variant', 'release bytes', 'functions', 'wasm-opt -O3 bytes', 'functions', 'over bump (wasm-opt)'],
        rows.map((r) => [
          r.variant,
          r.releaseBytes.toLocaleString('en-US'),
          r.releaseFunctions,
          r.wasmoptBytes.toLocaleString('en-US'),
          r.wasmoptFunctions,
          bump ? (r.wasmoptBytes - bump.wasmoptBytes).toLocaleString('en-US') : '-',
        ])
      )
    );
    console.log('');
  }
}

// ---------------------------------------------------------------- V8 12.4 vs 15.2

{
  const rows = [];
  for (const v of VARIANTS) {
    for (const wl of ['alloc_free_32', 'churn', 'random_actions', 'memory_grow_only']) {
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
    const m = /^flagcheck-(node|d8)-(.*)-(\w+)\.json$/.exec(f);
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
  const inspects = fs
    .readdirSync(dir)
    .filter((f) => /^shim-inspect-.*\.txt$/.test(f))
    .sort()
    .map((f) => fs.readFileSync(path.join(dir, f), 'utf8').trimEnd())
    .filter((s) => s.length > 0);
  if (rows.length) {
    rows.sort(
      (a, b) => a.variant.localeCompare(b.variant) || a.profile.localeCompare(b.profile) || a.engine.localeCompare(b.engine)
    );
    console.log('## Shim indirection: build profile against the fast path\n');
    console.log(
      'Median ns/op (min), optimizing tier on both V8 versions. release = opt-level 3, fat LTO, one codegen unit; -nolto = no LTO, 16 codegen units; -z = opt-level z; -wasmopt = the same file after wasm-opt -O3.\n'
    );
    const size = {};
    for (const text of inspects) {
      for (const line of text.split('\n')) {
        const m = /^(\w+)-(\S+)\.wasm: (\d+) bytes/.exec(line);
        if (m) size[`${m[1]}-${m[2]}`] = parseInt(m[3], 10);
      }
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
  }
  if (inspects.length) {
    console.log('### Call structure of the hot loops and the allocator shims\n');
    console.log(
      'From `wasm-tools print` after demangling: instruction count of each function and the functions it calls. A fast path that is inlined shows up as a loop with no call except the cold slow path (and `__rust_no_alloc_shim_is_unstable_v2`, an empty function std calls on every allocation).\n'
    );
    console.log('```');
    console.log(inspects.join('\n\n'));
    console.log('```\n');
  }
}
