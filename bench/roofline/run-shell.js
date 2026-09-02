// Driver for bare engine shells. Run from this directory so load() finds harness.js:
//   d8  [v8 flags]  run-shell.js -- [--json] [--reps N] path/to/roofline.wasm
//   jsc [jsc flags] run-shell.js -- [--json] [--reps N] path/to/roofline.wasm
// Pass --allow-natives-syntax to d8 to get each workload's compilation tier.
// Neither shell exposes its own command-line flags to scripts, so pass
// --flags "..." (and, for jsc, --engine-version "...") to record them.
load('harness.js');

// jsc's shell also defines version() and read(), so test for it first;
// preciseTime() and readFile() are jsc-only, readbuffer() is d8-only.
const isJsc = typeof preciseTime === 'function' && typeof readFile === 'function';
const isD8 = !isJsc && typeof readbuffer === 'function';
if (!isD8 && !isJsc) throw new Error('unrecognized shell (expected d8 or jsc)');

function makeTierOf() {
  if (!isD8) return () => 'n/a';
  try {
    const isLiftoff = new Function('f', 'return %IsLiftoffFunction(f)');
    const isTurbofan = new Function('f', 'return %IsTurboFanFunction(f)');
    return (f) => (isLiftoff(f) ? 'liftoff' : isTurbofan(f) ? 'turbofan' : 'other');
  } catch (e) {
    return () => 'n/a';
  }
}

const env = isD8
  ? {
      engine: 'd8',
      version: `V8 ${version()}`,
      flags: null,
      args: Array.from(arguments),
      now: () => performance.now(),
      print: print,
      readWasm: (p) => new Uint8Array(readbuffer(p)),
      tierOf: makeTierOf(),
    }
  : {
      engine: 'jsc',
      version: 'JavaScriptCore',
      flags: null,
      args: Array.from(arguments),
      // jsc's performance.now() returns 0 in the shell; preciseTime() is seconds.
      now: () => preciseTime() * 1000,
      print: print,
      readWasm: (p) => readFile(p, 'binary'),
      tierOf: () => 'n/a',
    };

globalThis.Roofline.run(env);
