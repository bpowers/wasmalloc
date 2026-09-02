// Node driver. Usage:
//   node [v8 flags] run.mjs [--json] [--reps N] [--scale F] [--only a,b] path/to/roofline.wasm
// Pass --allow-natives-syntax to get the compilation tier of each workload
// function in the output (Liftoff vs TurboFan/Turboshaft).
import fs from 'node:fs';
import process from 'node:process';
import { performance } from 'node:perf_hooks';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
require('./harness.js');

function makeTierOf() {
  try {
    const isLiftoff = new Function('f', 'return %IsLiftoffFunction(f)');
    const isTurbofan = new Function('f', 'return %IsTurboFanFunction(f)');
    return (f) => (isLiftoff(f) ? 'liftoff' : isTurbofan(f) ? 'turbofan' : 'other');
  } catch (e) {
    return () => 'n/a';
  }
}

const env = {
  engine: 'node',
  version: `node ${process.versions.node} / V8 ${process.versions.v8}`,
  // --allow-natives-syntax only enables the tier query; it is not an engine
  // configuration worth recording.
  flags: process.execArgv.filter((f) => f !== '--allow-natives-syntax').join(' ') || '(default)',
  args: process.argv.slice(2),
  now: () => performance.now(),
  print: (s) => process.stdout.write(s + '\n'),
  readWasm: (p) => fs.readFileSync(p),
  tierOf: makeTierOf(),
};

globalThis.Roofline.run(env);
