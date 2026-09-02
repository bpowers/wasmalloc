// Run the wasm32-wasip1 self-timing binary under node's WASI implementation:
//   node run-wasi.mjs path/to/roofline-wasi.wasm [--json] [--reps N] [--scale F]
import fs from 'node:fs';
import process from 'node:process';
import { WASI } from 'node:wasi';

const [wasmPath, ...rest] = process.argv.slice(2);
if (!wasmPath) {
  console.error('usage: node run-wasi.mjs file.wasm [args...]');
  process.exit(2);
}
const wasi = new WASI({ version: 'preview1', args: ['roofline-wasi', ...rest], env: {} });
const module = new WebAssembly.Module(fs.readFileSync(wasmPath));
const instance = new WebAssembly.Instance(module, wasi.getImportObject());
process.exitCode = wasi.start(instance);
