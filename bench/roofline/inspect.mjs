// Call structure of the hot functions in a wasm module, for the shim study:
//   node inspect.mjs file.wasm [name-regex ...]
// For each regex, finds the functions whose (demangled) name matches and prints
// the instruction count, the number of call instructions and the callees, so
// the report can say whether an allocator's fast path was inlined into the
// workload loop and into the __rust_alloc/__rust_dealloc shims, or is reached
// through calls. Needs wasm-tools (cargo install wasm-tools --locked).
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const [file, ...patterns] = process.argv.slice(2);
if (!file) {
  console.error('usage: node inspect.mjs file.wasm [name-regex ...]');
  process.exit(2);
}
const WASM_TOOLS = process.env.WASM_TOOLS || 'wasm-tools';

// Demangle the name section first so that Rust paths are readable; the
// legacy-mangled hash suffixes and the __rustc[...] crate prefix are dropped.
const tmp = path.join(os.tmpdir(), `inspect-${process.pid}.wasm`);
execFileSync(WASM_TOOLS, ['demangle', file, '-o', tmp]);
let wat;
try {
  wat = execFileSync(WASM_TOOLS, ['print', tmp], { maxBuffer: 1 << 28 }).toString();
} finally {
  fs.unlinkSync(tmp);
}

function shorten(name) {
  return name
    .replace(/^"|"$/g, '')
    .replace(/::h[0-9a-f]{16}$/, '')
    .replace(/^__rustc\[[0-9a-f]+\]::/, '')
    .replace(/^_ZN.*?__rustc\d+/, '');
}

// Split the text format into top-level function bodies. wasm-tools prints one
// function per `  (func $name ...` block closed by a line that is exactly `  )`.
const funcs = [];
const lines = wat.split('\n');
for (let i = 0; i < lines.length; i++) {
  const m = /^  \(func (\$(?:"[^"]*"|\S+))/.exec(lines[i]);
  if (!m) continue;
  const name = shorten(m[1].slice(1));
  const body = [];
  // An empty function is printed on one line, `(func $noop (;20;) (type 5))`,
  // with balanced parentheses; anything else runs until a line that is `  )`.
  const opens = (lines[i].match(/\(/g) || []).length;
  const closes = (lines[i].match(/\)/g) || []).length;
  let j = i;
  if (opens > closes) {
    for (j = i + 1; j < lines.length && lines[j] !== '  )'; j++) body.push(lines[j].trim());
  }
  funcs.push({ name, body, index: funcs.length });
  i = j;
}

function describe(f) {
  let instrs = 0;
  const callees = [];
  for (const line of f.body) {
    if (line.startsWith('(local ') || line === '' || line === ')' ) continue;
    instrs++;
    const m = /^call (\$(?:"[^"]*"|\S+))/.exec(line);
    if (m) callees.push(shorten(m[1].slice(1)));
    else if (line.startsWith('call_indirect')) callees.push('(indirect)');
    else if (line.startsWith('memory.grow')) callees.push('(memory.grow)');
  }
  const counts = new Map();
  for (const c of callees) counts.set(c, (counts.get(c) || 0) + 1);
  const list = [...counts.entries()].map(([c, n]) => (n > 1 ? `${c} x${n}` : c)).join(', ');
  return `${instrs} instructions, ${callees.length} calls${list ? ': ' + list : ''}`;
}

console.log(`${path.basename(file)}: ${fs.statSync(file).size} bytes, ${funcs.length} functions`);
for (const pat of patterns) {
  const re = new RegExp(pat);
  const hits = funcs.filter((f) => re.test(f.name));
  if (hits.length === 0) {
    console.log(`  ${pat}: no function matches`);
    continue;
  }
  for (const f of hits) console.log(`  ${f.name}: ${describe(f)}`);
}
