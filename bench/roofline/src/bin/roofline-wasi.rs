//! Self-timing driver for wasm32-wasip1 (and, for sanity checks, native).
//! Runs the same workloads as the cdylib exports with the same protocol as
//! run.mjs: warm up until two consecutive calls agree to within 10 percent,
//! then time `reps` repetitions and report median and min ns/op.
//!
//! Usage: roofline-wasi [--json] [--reps N] [--scale F]

use std::time::Instant;

use roofline::workloads as w;

struct Workload {
    name: &'static str,
    // Iterations passed to the function per call.
    n: usize,
    // Number of "ops" one call performs, for the per-op figure.
    ops_per_call: usize,
    run: fn(usize) -> u32,
    setup: Option<fn() -> u32>,
    teardown: Option<fn() -> u32>,
}

fn workloads() -> Vec<Workload> {
    vec![
        Workload {
            name: "alloc_free_32",
            n: 2_000_000,
            ops_per_call: 2_000_000,
            run: |n| w::alloc_free_fixed(n, 32),
            setup: None,
            teardown: None,
        },
        Workload {
            name: "batch_lifo_32",
            n: 2_000,
            ops_per_call: 2_000 * w::BATCH,
            run: |n| w::batch_alloc_free(n, 32, true),
            setup: None,
            teardown: None,
        },
        Workload {
            name: "batch_fifo_32",
            n: 2_000,
            ops_per_call: 2_000 * w::BATCH,
            run: |n| w::batch_alloc_free(n, 32, false),
            setup: None,
            teardown: None,
        },
        Workload {
            name: "churn",
            n: 200_000,
            ops_per_call: 200_000,
            run: w::churn,
            setup: Some(w::churn_init),
            teardown: Some(w::churn_fini),
        },
        Workload {
            name: "vec_push_growth",
            n: 20,
            ops_per_call: 20,
            run: w::vec_push_growth,
            setup: None,
            teardown: None,
        },
        Workload {
            name: "realloc_doubling",
            n: 100,
            ops_per_call: 100,
            run: w::realloc_doubling,
            setup: None,
            teardown: None,
        },
        Workload {
            name: "large_alloc_free",
            n: 50,
            ops_per_call: 50,
            run: w::large_alloc_free,
            setup: None,
            teardown: None,
        },
        Workload {
            name: "memory_grow_only",
            n: 16,
            ops_per_call: 16,
            run: w::memory_grow_only,
            setup: None,
            teardown: None,
        },
        Workload {
            name: "memory_grow_touch",
            n: 16,
            ops_per_call: 16,
            run: w::memory_grow_touch,
            setup: None,
            teardown: None,
        },
    ]
}

struct Args {
    json: bool,
    reps: usize,
    scale: f64,
}

fn parse_args() -> Args {
    let mut a = Args {
        json: false,
        reps: 7,
        scale: 1.0,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => a.json = true,
            "--reps" => a.reps = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.reps),
            "--scale" => a.scale = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.scale),
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    a
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

fn main() {
    let args = parse_args();
    let mut sink = 0u32;
    let mut rows = Vec::new();
    for wl in workloads() {
        let n = ((wl.n as f64) * args.scale).max(1.0) as usize;
        let ops = ((wl.ops_per_call as f64) * args.scale).max(1.0) as usize;
        // Warm-up: at least two calls, until stable, at most eight.
        let mut prev = time_call(&wl, n, &mut sink);
        let mut warm = 1;
        loop {
            let cur = time_call(&wl, n, &mut sink);
            warm += 1;
            let stable = (cur - prev).abs() / prev.max(1.0) < 0.10;
            prev = cur;
            if (warm >= 2 && stable) || warm >= 8 {
                break;
            }
        }
        let mut samples: Vec<f64> = (0..args.reps)
            .map(|_| time_call(&wl, n, &mut sink) / ops as f64)
            .collect();
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = samples[samples.len() / 2];
        let min = samples[0];
        rows.push((wl.name, n, ops, median, min, warm));
    }
    if args.json {
        println!("{{");
        println!("  \"variant\": \"{}\",", roofline::alloc::NAME);
        println!("  \"results\": [");
        for (i, (name, n, ops, median, min, warm)) in rows.iter().enumerate() {
            let comma = if i + 1 < rows.len() { "," } else { "" };
            println!(
                "    {{\"workload\": \"{name}\", \"n\": {n}, \"opsPerCall\": {ops}, \"medianNsPerOp\": {median:.3}, \"minNsPerOp\": {min:.3}, \"warmupCalls\": {warm}}}{comma}"
            );
        }
        println!("  ],");
        println!("  \"checksum\": {sink}");
        println!("}}");
    } else {
        println!("variant: {}", roofline::alloc::NAME);
        println!("{:<20} {:>10} {:>12} {:>12} {:>6}", "workload", "ops/call", "median ns/op", "min ns/op", "warm");
        for (name, _n, ops, median, min, warm) in &rows {
            println!("{name:<20} {ops:>10} {median:>12.2} {min:>12.2} {warm:>6}");
        }
        eprintln!("checksum: {sink}");
    }
}
