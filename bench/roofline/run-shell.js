// Driver for bare engine shells. Run from this directory so load() finds harness.js:
//   d8  [v8 flags]  run-shell.js -- [--json] [--reps N] path/to/roofline.wasm
//   jsc [jsc flags] run-shell.js -- [--json] [--reps N] path/to/roofline.wasm
load('harness.js');

const isD8 = typeof readbuffer === 'function' || (typeof version === 'function' && typeof read === 'function');
const isJsc = typeof preciseTime === 'function';
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
      now: () => preciseTime() * 1000,
      print: print,
      readWasm: (p) => readFile(p, 'binary'),
      tierOf: () => 'n/a',
    };

globalThis.Roofline.run(env);
