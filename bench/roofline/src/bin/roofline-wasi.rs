//! Self-timing driver for wasm32-wasip1 (and, for sanity checks, native).
//! Runs the same workloads as the cdylib exports with the same protocol as
//! harness.js: warm up until the last three calls agree to within 10 percent
//! (at least 3 calls, at most 12), then time `reps` repetitions and report
//! median and min ns per op. There is no host call to subtract here: the
//! workload is called from inside the module.
//!
//! This driver must not allocate on the heap between the first `reset()` and
//! the last measurement. The floor allocators rewind their bump pointer on
//! `reset()`, so any heap object created after that point would be handed out
//! again by the next workload. All bookkeeping therefore lives in statics and
//! fixed-size stack arrays; the argument strings are parsed and dropped before
//! the first reset, and output is formatted only after the last measurement.
//!
//! Usage: roofline-wasi [--json] [--reps N] [--scale F] [--only a,b]

use std::time::Instant;

use roofline::workloads as w;

struct Workload {
    name: &'static str,
    unit: &'static str,
    // Iterations passed to the function per call at scale 1.
    n: usize,
    // Number of "ops" each iteration performs, for the per-op figure.
    ops_per_iter: usize,
    run: fn(usize) -> u32,
    setup: Option<fn() -> u32>,
    teardown: Option<fn() -> u32>,
}

// Mirrors WORKLOADS in harness.js; keep the two lists in sync.
static WORKLOADS: [Workload; 12] = [
    Workload {
        name: "alloc_free_32",
        unit: "alloc+free pair",
        n: 2_000_000,
        ops_per_iter: 1,
        run: |n| w::alloc_free_fixed(n, 32, 8),
        setup: None,
        teardown: None,
    },
    Workload {
        name: "alloc_free_32_align16",
        unit: "alloc+free pair, align 16",
        n: 2_000_000,
        ops_per_iter: 1,
        run: |n| w::alloc_free_fixed(n, 32, 16),
        setup: None,
        teardown: None,
    },
    Workload {
        name: "batch_lifo_32",
        unit: "alloc+free pair",
        n: 2_000,
        ops_per_iter: w::BATCH,
        run: |n| w::batch_alloc_free(n, 32, true),
        setup: None,
        teardown: None,
    },
    Workload {
        name: "batch_fifo_32",
        unit: "alloc+free pair",
        n: 2_000,
        ops_per_iter: w::BATCH,
        run: |n| w::batch_alloc_free(n, 32, false),
        setup: None,
        teardown: None,
    },
    Workload {
        name: "churn",
        unit: "free+alloc step",
        n: 200_000,
        ops_per_iter: 1,
        run: w::churn,
        setup: Some(w::churn_init),
        teardown: Some(w::churn_fini),
    },
    Workload {
        name: "random_actions",
        unit: "action (3/7 alloc, 3/7 free, 1/7 realloc)",
        n: 100_000,
        ops_per_iter: 1,
        run: w::random_actions,
        setup: None,
        teardown: Some(w::random_actions_fini),
    },
    Workload {
        name: "random_actions_norealloc",
        unit: "action (1/2 alloc, 1/2 free)",
        n: 100_000,
        ops_per_iter: 1,
        run: w::random_actions_norealloc,
        setup: None,
        teardown: Some(w::random_actions_fini),
    },
    Workload {
        name: "vec_push_growth",
        unit: "1 MiB Vec<u8> growth",
        n: 20,
        ops_per_iter: 1,
        run: w::vec_push_growth,
        setup: None,
        teardown: None,
    },
    Workload {
        name: "realloc_doubling",
        unit: "16 B to 1 MiB realloc chain",
        n: 100,
        ops_per_iter: 1,
        run: w::realloc_doubling,
        setup: None,
        teardown: None,
    },
    Workload {
        name: "large_alloc_free",
        unit: "alloc+touch+free, 256K-4M",
        n: 50,
        ops_per_iter: 1,
        run: w::large_alloc_free,
        setup: None,
        teardown: None,
    },
    Workload {
        name: "memory_grow_only",
        unit: "memory.grow 1 MiB, untouched",
        n: 16,
        ops_per_iter: 1,
        run: w::memory_grow_only,
        setup: None,
        teardown: None,
    },
    Workload {
        name: "memory_grow_touch",
        unit: "memory.grow 1 MiB + touch",
        n: 16,
        ops_per_iter: 1,
        run: w::memory_grow_touch,
        setup: None,
        teardown: None,
    },
];

const MIN_WARM: usize = 3;
const MAX_WARM: usize = 12;
const STABLE_RATIO: f64 = 1.10;
const DEFAULT_REPS: usize = 7;
const MAX_REPS: usize = 64;

struct Args {
    json: bool,
    reps: usize,
    scale: f64,
    // Bit i set: run WORKLOADS[i].
    only: u32,
}

fn usage() -> ! {
    eprintln!("usage: roofline-wasi [--json] [--reps N (<= {MAX_REPS})] [--scale F] [--only a,b]");
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut a = Args {
        json: false,
        reps: DEFAULT_REPS,
        scale: 1.0,
        only: u32::MAX,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => a.json = true,
            "--reps" => {
                a.reps = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage());
                if a.reps == 0 || a.reps > MAX_REPS {
                    usage();
                }
            }
            "--scale" => {
                a.scale = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage());
                if !(a.scale > 0.0) {
                    usage();
                }
            }
            "--only" => {
                let list = it.next().unwrap_or_else(|| usage());
                a.only = 0;
                for name in list.split(',') {
                    match WORKLOADS.iter().position(|wl| wl.name == name) {
                        Some(i) => a.only |= 1 << i,
                        None => {
                            eprintln!("unknown workload: {name}");
                            usage();
                        }
                    }
                }
            }
            _ => usage(),
        }
    }
    a
}

#[derive(Clone, Copy)]
struct Row {
    idx: usize,
    n: usize,
    ops: usize,
    median: f64,
    min: f64,
    warm: usize,
    // memory.size before the first warm-up call and after the last timed call
    // (and its teardown); None on native.
    pages_before: Option<usize>,
    pages_after: Option<usize>,
}

fn time_call(wl: &Workload, n: usize, sink: &mut u32) -> f64 {
    roofline::alloc::reset();
    if let Some(s) = wl.setup {
        *sink ^= s();
    }
    let t0 = Instant::now();
    let r = (wl.run)(n);
    let dt = t0.elapsed().as_secs_f64() * 1e9;
    *sink ^= r;
    if let Some(t) = wl.teardown {
        *sink ^= t();
    }
    dt
}

fn measure(wl: &Workload, idx: usize, args: &Args, sink: &mut u32) -> Row {
    let n = ((wl.n as f64) * args.scale).round().max(1.0) as usize;
    let ops = n * wl.ops_per_iter;
    let pages_before = roofline::alloc::memory_pages();

    // Warm-up: the last three call times must agree to within STABLE_RATIO.
    let mut last = [0.0f64; 3];
    let mut warm = 0usize;
    loop {
        last[warm % 3] = time_call(wl, n, sink);
        warm += 1;
        if warm >= MIN_WARM {
            let hi = last.iter().cloned().fold(0.0, f64::max);
            let lo = last.iter().cloned().fold(f64::INFINITY, f64::min).max(1.0);
            if hi / lo < STABLE_RATIO || warm >= MAX_WARM {
                break;
            }
        }
    }

    let mut samples = [0.0f64; MAX_REPS];
    for s in samples.iter_mut().take(args.reps) {
        *s = time_call(wl, n, sink) / ops as f64;
    }
    let samples = &mut samples[..args.reps];
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Row {
        idx,
        n,
        ops,
        median: samples[samples.len() / 2],
        min: samples[0],
        warm,
        pages_before,
        pages_after: roofline::alloc::memory_pages(),
    }
}

fn target() -> &'static str {
    if cfg!(all(target_arch = "wasm32", target_os = "wasi")) {
        "wasm32-wasip1"
    } else if cfg!(target_arch = "wasm32") {
        "wasm32-unknown-unknown"
    } else {
        "native"
    }
}

fn json_pages(p: Option<usize>) -> String {
    match p {
        Some(p) => p.to_string(),
        None => "null".to_string(),
    }
}

fn main() {
    let args = parse_args();
    let pages_at_start = roofline::alloc::memory_pages();
    let mut sink = 0u32;
    let mut rows = [Row {
        idx: 0,
        n: 0,
        ops: 0,
        median: 0.0,
        min: 0.0,
        warm: 0,
        pages_before: None,
        pages_after: None,
    }; WORKLOADS.len()];
    let mut nrows = 0;
    for (i, wl) in WORKLOADS.iter().enumerate() {
        if args.only & (1 << i) == 0 {
            continue;
        }
        rows[nrows] = measure(wl, i, &args, &mut sink);
        nrows += 1;
    }
    let rows = &rows[..nrows];

    // Everything below may allocate freely: no reset() follows.
    if args.json {
        println!("{{");
        println!("  \"engine\": \"self-timed\",");
        println!("  \"target\": \"{}\",", target());
        println!("  \"variant\": \"{}\",", roofline::alloc::NAME);
        println!("  \"allocator\": \"{}\",", roofline::alloc::DETAIL);
        println!("  \"reps\": {},", args.reps);
        println!("  \"scale\": {},", args.scale);
        println!("  \"callOverheadNs\": null,");
        println!("  \"memoryPagesAtStart\": {},", json_pages(pages_at_start));
        println!("  \"results\": [");
        for (i, r) in rows.iter().enumerate() {
            let wl = &WORKLOADS[r.idx];
            let comma = if i + 1 < rows.len() { "," } else { "" };
            println!(
                "    {{\"workload\": \"{}\", \"unit\": \"{}\", \"n\": {}, \"opsPerCall\": {}, \"medianNsPerOp\": {:.3}, \"minNsPerOp\": {:.3}, \"warmupCalls\": {}, \"tier\": \"n/a\", \"pagesBefore\": {}, \"pagesAfter\": {}}}{}",
                wl.name,
                wl.unit,
                r.n,
                r.ops,
                r.median,
                r.min,
                r.warm,
                json_pages(r.pages_before),
                json_pages(r.pages_after),
                comma
            );
        }
        println!("  ],");
        println!("  \"checksum\": {sink}");
        println!("}}");
    } else {
        println!(
            "target: {}  variant: {} ({})",
            target(),
            roofline::alloc::NAME,
            roofline::alloc::DETAIL
        );
        println!(
            "{:<26} {:>10} {:>12} {:>12} {:>6} {:>8}",
            "workload", "ops/call", "median ns/op", "min ns/op", "warm", "pages"
        );
        for r in rows {
            println!(
                "{:<26} {:>10} {:>12.2} {:>12.2} {:>6} {:>8}",
                WORKLOADS[r.idx].name,
                r.ops,
                r.median,
                r.min,
                r.warm,
                r.pages_after.map_or("-".to_string(), |p| p.to_string())
            );
        }
        eprintln!("checksum: {sink}");
    }
}
