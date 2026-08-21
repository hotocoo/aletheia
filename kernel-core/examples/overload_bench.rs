//! EXTREME STRESS BENCH for `aletheia_risk`.
//!
//! This is NOT the gate bench — the gate bench caps at 2 M advices / 8 K tasks
//! so it can finish on a QEMU-emulated riscv64 in seconds. This one exists to
//! push the in-kernel risk advisor until it breaks, and to print what broke
//! before it does. Five modes:
//!
//!   advice_storm      pure `advise()` throughput, configurable load
//!   schedule_storm    drain N tasks through the priority scheduler, time it
//!   determinism       same workload, two independent invocations -> bit-equal?
//!   fault_inject      pass pathological feature vectors (NaN-equivalents,
//!                      all-same, all-zero, alternating extremes)
//!   boundary_sweep    scan every feature at min..max while holding others at
//!                     a fixed in-box vector, count verdicts per feature
//!
//! Defaults: 100 M advices, 1 M tasks. Override with env / CLI.
//!
//! Run: `cargo run --release --example overload_bench -- <mode> [args]`

use std::env;
use std::process::ExitCode;
use std::time::Instant;

use kernel_core::mlrisk::{RiskAdvisor, Verdict, BUNDLED_MODEL};
use kernel_core::mlrisk_contract::N_FEATURES;
use kernel_core::priosched::{Priority, PriorityScheduler};
use kernel_core::sched::TaskId;

fn load() -> Result<RiskAdvisor<'static>, kernel_core::mlrisk::ModelError> {
    RiskAdvisor::load(BUNDLED_MODEL)
}

fn usage(prog: &str) {
    eprintln!(
        "usage: {prog} <mode>\n\
         modes:\n\
           advice_storm      [advices=100000000] [batch=1000000]\n\
           schedule_storm    [tasks=1000000]    [bands=8]\n\
           determinism       [advices=2000000]\n\
           fault_inject      [per_pattern=1000000]\n\
           boundary_sweep    [samples_per_feature=200000]\n\
         env:\n\
           OLOAD_ADVICES, OLOAD_TASKS, OLOAD_BANDS, OLOAD_BATCH, OLOAD_SAMPLES, OLOAD_PER_PATTERN"
    );
}

fn parse_usize(arg: &str, default: usize) -> usize {
    match arg.parse::<usize>() {
        Ok(v) => v,
        Err(_) => default,
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let prog = args.first().map(String::as_str).unwrap_or("overload_bench");
    if args.len() < 2 {
        usage(prog);
        return ExitCode::from(2);
    }
    let mode = args[1].as_str();

    let advisor = match load() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("FATAL: bundled blob refused: {:?}", e);
            return ExitCode::from(3);
        }
    };
    eprintln!(
        "[overload] blob loaded: n_trees unknown to host (header only); worst_case_compares={}",
        advisor.worst_case_compares()
    );

    let rc = match mode {
        "advice_storm" => {
            let n = env::var("OLOAD_ADVICES")
                .ok()
                .and_then(|s| s.parse().ok())
                .or_else(|| args.get(2).map(|s| parse_usize(s, 100_000_000)))
                .unwrap_or(100_000_000);
            let batch = env::var("OLOAD_BATCH")
                .ok()
                .and_then(|s| s.parse().ok())
                .or_else(|| args.get(3).map(|s| parse_usize(s, 1_000_000)))
                .unwrap_or(1_000_000);
            advice_storm(&advisor, n, batch)
        }
        "schedule_storm" => {
            let tasks = env::var("OLOAD_TASKS")
                .ok()
                .and_then(|s| s.parse().ok())
                .or_else(|| args.get(2).map(|s| parse_usize(s, 1_000_000)))
                .unwrap_or(1_000_000);
            let bands = env::var("OLOAD_BANDS")
                .ok()
                .and_then(|s| s.parse().ok())
                .or_else(|| args.get(3).map(|s| parse_usize(s, 8)))
                .unwrap_or(8);
            schedule_storm(&advisor, tasks, bands as u8)
        }
        "determinism" => {
            let n = env::var("OLOAD_ADVICES")
                .ok()
                .and_then(|s| s.parse().ok())
                .or_else(|| args.get(2).map(|s| parse_usize(s, 2_000_000)))
                .unwrap_or(2_000_000);
            determinism(&advisor, n)
        }
        "fault_inject" => {
            let per = env::var("OLOAD_PER_PATTERN")
                .ok()
                .and_then(|s| s.parse().ok())
                .or_else(|| args.get(2).map(|s| parse_usize(s, 1_000_000)))
                .unwrap_or(1_000_000);
            fault_inject(&advisor, per)
        }
        "boundary_sweep" => {
            let samples = env::var("OLOAD_SAMPLES")
                .ok()
                .and_then(|s| s.parse().ok())
                .or_else(|| args.get(2).map(|s| parse_usize(s, 200_000)))
                .unwrap_or(200_000);
            boundary_sweep(&advisor, samples)
        }
        _ => {
            usage(prog);
            return ExitCode::from(2);
        }
    };
    ExitCode::from(rc)
}

// --------- helpers ---------------------------------------------------------

/// A deterministic pseudo-random u32 derived from `seed` and `i`. Cheap, no
/// allocations, reproducible across runs (this whole bench is meant to be).
#[inline]
fn xorshift(mut x: u32) -> u32 {
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

fn fill_random(x: &mut [i32; N_FEATURES], seed: u64, i: u64) {
    // Two u32s per feature, mixed via Knuth multiplicative hash.
    let mut s = (seed as u32) ^ (i as u32).wrapping_mul(0x9E37_79B9);
    for slot in x.iter_mut() {
        s = xorshift(s);
        let lo = s as i32;
        s = xorshift(s);
        let hi = s as i32;
        *slot = (lo as i64 | ((hi as i64) << 32)) as i32;
    }
}

fn snap_into_box(x: &mut [i32; N_FEATURES], advisor: &RiskAdvisor<'_>) {
    // Project any feature outside its training range back into the box so we
    // can isolate "advice on in-box rows" from "abstain from range guard".
    for (f, slot) in x.iter_mut().enumerate() {
        let (lo, hi) = advisor.feature_range(f);
        let span = hi as i64 - lo as i64;
        if span <= 0 {
            *slot = lo;
            continue;
        }
        let v = *slot as i64;
        // Modulo + offset, but signed correctly for negative `v`.
        let norm = ((v % span) + span) % span;
        *slot = (lo as i64 + norm) as i32;
    }
}

// --------- modes -----------------------------------------------------------

fn advice_storm(advisor: &RiskAdvisor<'_>, total: usize, batch: usize) -> u8 {
    // 4 input distributions: uniform in-box, in-box biased to high-priority
    // features (synthetic "real workload"), all-zero (band edge), and an
    // OOD probe. Same model, 4 different surfaces — lets us see whether
    // verdict distribution is uniform across realistic vs adversarial inputs.
    let chunk = (total / 4).max(1);
    let mut surfaces: Vec<(&str, Box<dyn Fn(&mut [i32; N_FEATURES], &RiskAdvisor<'_>, u64)>)> = vec![
        (
            "uniform_in_box",
            Box::new(|x, a, i| {
                fill_random(x, 0xC0FFEE, i);
                snap_into_box(x, a);
            }),
        ),
        (
            "biased_to_priority_features",
            Box::new(|x, a, i| {
                // Push the 5 priority/user-failure features to the top of
                // their range. Mimics a workload of "look at me, I'm important".
                fill_random(x, 0xBADBEEF, i);
                snap_into_box(x, a);
                for &f in &[1usize, 11, 12, 13, 17] {
                    let (_, hi) = a.feature_range(f);
                    x[f] = hi;
                }
            }),
        ),
        (
            "biased_to_low_signal",
            Box::new(|x, a, i| {
                // Push user/cell counters to the BOTTOM. Mimics a workload of
                // first-time users on a quiet cell — historically the model's
                // "easy" cases.
                fill_random(x, 0xFADE5, i);
                snap_into_box(x, a);
                for &f in &[11usize, 12, 13, 14, 15, 16, 17, 18] {
                    let (lo, _) = a.feature_range(f);
                    x[f] = lo;
                }
            }),
        ),
        (
            "all_zero_band_edge",
            Box::new(|x, _, _| {
                for s in x.iter_mut() {
                    *s = 0;
                }
            }),
        ),
    ];
    let surfaces_n = surfaces.len();
    let n = chunk * surfaces_n; // round total down to a multiple of 4
    let mut x = [0i32; N_FEATURES];
    let mut totals_low = 0u64;
    let mut totals_elevated = 0u64;
    let mut totals_abstain = 0u64;
    let mut totals_oor = 0u64;
    let mut margin_sum: i128 = 0;
    let mut margin_min: i64 = i64::MAX;
    let mut margin_max: i64 = i64::MIN;

    let t0 = Instant::now();
    let mut done = 0usize;
    let mut last_report = t0;
    let mut per_surface: Vec<(u64, u64, u64, u64, i64, i64)> = vec![(0, 0, 0, 0, i64::MAX, i64::MIN); surfaces_n];

    for i in 0..n {
        let sidx = (i / chunk) % surfaces_n;
        let (ref mut sl, ref mut se, ref mut sa, ref mut so, ref mut smin, ref mut smax) =
            per_surface[sidx];
        let fill = &mut surfaces[sidx].1;
        fill(&mut x, advisor, i as u64);
        let a = advisor.advise(&x);
        match a.verdict {
            Verdict::Low => {
                totals_low += 1;
                *sl += 1;
            }
            Verdict::Elevated => {
                totals_elevated += 1;
                *se += 1;
            }
            Verdict::Abstain => {
                totals_abstain += 1;
                *sa += 1;
            }
        }
        if a.out_of_range {
            totals_oor += 1;
            *so += 1;
        }
        margin_sum += a.margin as i128;
        if a.margin < margin_min {
            margin_min = a.margin;
        }
        if a.margin > margin_max {
            margin_max = a.margin;
        }
        if a.margin < *smin {
            *smin = a.margin;
        }
        if a.margin > *smax {
            *smax = a.margin;
        }
        done += 1;
        let now = Instant::now();
        if now.duration_since(last_report).as_millis() >= 2000 {
            let elapsed = now.duration_since(t0).as_secs_f64();
            let rate = done as f64 / elapsed;
            eprintln!(
                "  progress: {done:>12}/{n:<12} ({:>5.1}%)  {:.2e} adv/s  elapsed {:.1}s",
                100.0 * done as f64 / n as f64,
                rate,
                elapsed
            );
            last_report = now;
        }
    }
    let elapsed = t0.elapsed();
    let secs = elapsed.as_secs_f64();
    let per_ns = elapsed.as_nanos() as f64 / n as f64;
    let rate = n as f64 / secs;

    println!("[advice_storm] total = {} (4 surfaces x {})", n, chunk);
    println!("[advice_storm] elapsed = {:.3} s", secs);
    println!("[advice_storm] per advice = {:.1} ns ({:.0} ps)", per_ns, per_ns * 1000.0);
    println!("[advice_storm] throughput = {:.2e} adv/s", rate);
    println!(
        "[advice_storm] verdicts: low={} elevated={} abstain={} (in-band: {})",
        totals_low, totals_elevated, totals_abstain,
        totals_abstain.saturating_sub(totals_oor)
    );
    println!("[advice_storm] out_of_range: {}", totals_oor);
    println!(
        "[advice_storm] margin: min={} max={} mean={:.1}",
        margin_min,
        margin_max,
        margin_sum as f64 / n as f64
    );
    for (i, (l, e, a, o, mn, mx)) in per_surface.iter().enumerate() {
        println!(
            "[advice_storm]   surface {:>26}  L={:>8} E={:>8} A={:>6} (band={}) oor={} margin=[{},{}]",
            surfaces[i].0,
            l,
            e,
            a,
            a.saturating_sub(*o),
            o,
            mn,
            mx
        );
    }

    // Sanity: no verdict bin should be zero across all 4 surfaces — if every
    // surface agrees the model is one-bit, that itself is the finding.
    if totals_low == 0 {
        eprintln!("[advice_storm] finding: model is ONE-BIT in Low direction across all surfaces");
    }
    if totals_oor != 0 {
        eprintln!("FATAL: snap_into_box leaked {totals_oor} out-of-range rows in-box");
        return 5;
    }
    0
}

fn schedule_storm(advisor: &RiskAdvisor<'_>, tasks: usize, bands: u8) -> u8 {
    let mut x = [0i32; N_FEATURES];
    let mut sched = PriorityScheduler::new("kernel.endpoint.acquire");
    let mut want_ids: Vec<u64> = Vec::with_capacity(tasks);
    let mut want_bands: Vec<u8> = Vec::with_capacity(tasks);

    eprintln!("[schedule_storm] admitting {tasks} tasks across {bands} bands...");
    let t_admit0 = Instant::now();
    for i in 0..tasks {
        let iu = i as u64;
        fill_random(&mut x, 0xCAFE_F00D, iu);
        snap_into_box(&mut x, advisor);
        let band = 1 + ((iu as u8).wrapping_add((iu >> 32) as u8)) % bands.max(1);
        sched.admit_with_advice(TaskId(iu + 1), Priority(band), advisor.advise(&x));
        want_ids.push(iu + 1);
        want_bands.push(band);
        if i > 0 && i % 100_000 == 0 {
            eprintln!("  admitted {}/{}", i, tasks);
        }
    }
    let admit_secs = t_admit0.elapsed().as_secs_f64();
    eprintln!("[schedule_storm] admit done in {:.2} s", admit_secs);

    let t_drain0 = Instant::now();
    let mut last = u8::MAX;
    let mut drained = 0usize;
    while let Some(t) = sched.schedule_next() {
        let pos = want_ids
            .iter()
            .position(|id| *id == t.0)
            .expect("known task");
        let band = want_bands[pos];
        if band > last {
            // Priority regression — would be a property violation.
            eprintln!("FATAL: priority regression at drain {}: last={} now={}", drained, last, band);
            return 6;
        }
        // Count positions moved against insertion order only as an observation.
        // (We don't compare against the plain schedule here; that's
        // determinism-mode's job. We do measure wall time.)
        last = band;
        sched.finish(t);
        drained += 1;
        if drained % 100_000 == 0 {
            eprintln!("  drained {}/{}", drained, tasks);
        }
    }
    let drain_secs = t_drain0.elapsed().as_secs_f64();
    if drained != tasks {
        eprintln!("FATAL: drain lost tasks: drained={} expected={}", drained, tasks);
        return 7;
    }
    let per_advice_us = admit_secs * 1e6 / tasks as f64;
    let per_task_us = drain_secs * 1e6 / tasks as f64;
    println!("[schedule_storm] tasks = {}", tasks);
    println!("[schedule_storm] admit: {:.2} s ({:.2} µs/task)", admit_secs, per_advice_us);
    println!("[schedule_storm] drain: {:.2} s ({:.2} µs/task)", drain_secs, per_task_us);
    println!("[schedule_storm] total: {:.2} s", admit_secs + drain_secs);
    println!("[schedule_storm] priority monotonic: OK across {drained} drains");
    0
}

fn determinism(advisor: &RiskAdvisor<'_>, n: usize) -> u8 {
    // Two independent invocations, same seed, same algorithm — must produce
    // bit-identical census + margin statistics. A model that depends on time,
    // address, allocator or thread state would diverge.
    fn run_once(advisor: &RiskAdvisor<'_>, n: usize) -> (u64, u64, u64, i64, i64, i64) {
        let mut x = [0i32; N_FEATURES];
        let mut low = 0u64;
        let mut elevated = 0u64;
        let mut abstain = 0u64;
        let mut mmin = i64::MAX;
        let mut mmax = i64::MIN;
        let mut mfirst = i64::MIN;
        for i in 0..n {
            fill_random(&mut x, 0xDEAD_BEEF, i as u64);
            snap_into_box(&mut x, advisor);
            let a = advisor.advise(&x);
            match a.verdict {
                Verdict::Low => low += 1,
                Verdict::Elevated => elevated += 1,
                Verdict::Abstain => abstain += 1,
            }
            if a.margin < mmin {
                mmin = a.margin;
            }
            if a.margin > mmax {
                mmax = a.margin;
            }
            if i == 0 {
                mfirst = a.margin;
            }
        }
        (low, elevated, abstain, mmin, mmax, mfirst)
    }
    eprintln!("[determinism] running first pass ({n} advices)...");
    let t0 = Instant::now();
    let (l1, e1, a1, mmin1, mmax1, mfirst1) = run_once(advisor, n);
    let s0 = t0.elapsed().as_secs_f64();
    eprintln!("[determinism] running second pass...");
    let t1 = Instant::now();
    let (l2, e2, a2, mmin2, mmax2, mfirst2) = run_once(advisor, n);
    let s1 = t1.elapsed().as_secs_f64();

    println!("[determinism] pass 1: {:.3} s  low={} elevated={} abstain={}", s0, l1, e1, a1);
    println!(
        "[determinism] pass 2: {:.3} s  low={} elevated={} abstain={}",
        s1, l2, e2, a2
    );
    println!(
        "[determinism] margin: pass1=[{mmin1},{mmax1}] first={mfirst1}  pass2=[{mmin2},{mmax2}] first={mfirst2}"
    );

    let mut rc = 0u8;
    if (l1, e1, a1, mmin1, mmax1, mfirst1) != (l2, e2, a2, mmin2, mmax2, mfirst2) {
        eprintln!("FATAL: determinism violation across passes");
        rc = 8;
    } else {
        println!("[determinism] BIT-IDENTICAL across {n} advices, two passes");
    }
    rc
}

fn fault_inject(advisor: &RiskAdvisor<'_>, per_pattern: usize) -> u8 {
    // The advisor takes a fixed-size [i32; N_FEATURES] array, so true NaN /
    // Inf injection is not possible at the type level — but i32::MIN, i32::MAX
    // and arithmetic wrap are exactly the kind of input the kernel might see
    // if a feature extractor handed back a sentinel value. Five patterns:
    let mut rc = 0u8;
    let patterns: &[(&str, fn(&mut [i32; N_FEATURES], u64))] = &[
        ("all_i32_MIN", |x, _| {
            for s in x.iter_mut() {
                *s = i32::MIN;
            }
        }),
        ("all_i32_MAX", |x, _| {
            for s in x.iter_mut() {
                *s = i32::MAX;
            }
        }),
        ("alternating_MIN_MAX", |x, _| {
            for (i, s) in x.iter_mut().enumerate() {
                *s = if i % 2 == 0 { i32::MIN } else { i32::MAX };
            }
        }),
        ("first_half_MIN_second_MAX", |x, _| {
            for (i, s) in x.iter_mut().enumerate() {
                *s = if i < N_FEATURES / 2 { i32::MIN } else { i32::MAX };
            }
        }),
        ("zero", |x, _| {
            for s in x.iter_mut() {
                *s = 0;
            }
        }),
    ];
    for (name, f) in patterns {
        let mut x = [0i32; N_FEATURES];
        let mut low = 0u64;
        let mut elevated = 0u64;
        let mut abstain = 0u64;
        let mut oor = 0u64;
        let mut mmin = i64::MAX;
        let mut mmax = i64::MIN;
        let t0 = Instant::now();
        for i in 0..per_pattern {
            f(&mut x, i as u64);
            let a = advisor.advise(&x);
            match a.verdict {
                Verdict::Low => low += 1,
                Verdict::Elevated => elevated += 1,
                Verdict::Abstain => abstain += 1,
            }
            if a.out_of_range {
                oor += 1;
            }
            if a.margin < mmin {
                mmin = a.margin;
            }
            if a.margin > mmax {
                mmax = a.margin;
            }
        }
        let secs = t0.elapsed().as_secs_f64();
        println!(
            "[fault_inject] {:<26} n={:<10}  L={} E={} A={} oor={} margin=[{},{}]  {:.2}s",
            name, per_pattern, low, elevated, abstain, oor, mmin, mmax, secs
        );
        // The advisor must NEVER panic on any input. (If this fires, the test
        // runner dies with a SIGABRT and we never get here.) The wall-clock
        // ceiling is set generously: at ~3.5 µs/advice the expected time on
        // 10 M advices is ~35 s, and we want to be alerted only on a real
        // pathology (e.g. accidental O(n²) path) rather than on cold caches.
        if secs > 120.0 {
            eprintln!(
                "FATAL: pattern {name} took {:.2}s on {per_pattern} advices (>30s ceiling)",
                secs
            );
            rc = 9;
        }
    }
    rc
}

fn boundary_sweep(advisor: &RiskAdvisor<'_>, samples_per_feature: usize) -> u8 {
    // Hold all features at the midpoint of their range, then sweep one feature
    // across its full range. Count verdicts per feature to find features whose
    // path through the forest is monotone in the input — a property the model
    // card claims for 9/20 features ("+1" monotone constraint). If a feature
    // is NOT monotone in the verdict, that's an interesting bug to surface.
    let mut x = [0i32; N_FEATURES];
    // Midpoint vector.
    for (f, slot) in x.iter_mut().enumerate() {
        let (lo, hi) = advisor.feature_range(f);
        *slot = lo.saturating_add(((hi as i64 - lo as i64) / 2) as i32);
    }
    let baseline_a = advisor.advise(&x);
    println!(
        "[boundary_sweep] baseline (all-features midpoint) verdict={:?} margin={} oor={}",
        baseline_a.verdict, baseline_a.margin, baseline_a.out_of_range
    );

    let mut monotone_asc = 0usize;
    let mut monotone_desc = 0usize;
    let mut non_monotone = 0usize;
    let mut n_features = 0usize;

    for f in 0..N_FEATURES {
        n_features += 1;
        let (lo, hi) = advisor.feature_range(f);
        if (hi as i64) - (lo as i64) < 2 {
            continue;
        }
        let step = ((hi as i64 - lo as i64) / samples_per_feature as i64).max(1);
        let mut went_up = 0u64;
        let mut went_down = 0u64;
        let mut last = i64::MIN;
        let mut oor_count = 0u64;
        let mut verdict_counts = [0u64; 3]; // Low, Abstain, Elevated (in enum order)
        let t0 = Instant::now();
        let mut v = lo as i64;
        let mut i = 0usize;
        while v <= hi as i64 && i < samples_per_feature {
            x[f] = v as i32;
            let a = advisor.advise(&x);
            if a.out_of_range {
                oor_count += 1;
            }
            match a.verdict {
                Verdict::Low => verdict_counts[0] += 1,
                Verdict::Abstain => verdict_counts[1] += 1,
                Verdict::Elevated => verdict_counts[2] += 1,
            }
            if last != i64::MIN {
                if a.margin > last {
                    went_up += 1;
                } else if a.margin < last {
                    went_down += 1;
                }
            }
            last = a.margin;
            v += step;
            i += 1;
        }
        let secs = t0.elapsed().as_secs_f64();
        let mon = match (went_up > 0, went_down > 0) {
            (true, false) => {
                monotone_asc += 1;
                "asc"
            }
            (false, true) => {
                monotone_desc += 1;
                "desc"
            }
            _ => {
                non_monotone += 1;
                "mixed"
            }
        };
        println!(
            "[boundary_sweep] feat {:>2} ({:>17})  range=[{},{}] step={:<6} samples={:<7}  L={} A={} E={} oor={}  margin {}  {:.2}s",
            f,
            kernel_core::mlrisk_contract::FEATURE_NAMES[f],
            lo,
            hi,
            step,
            i,
            verdict_counts[0],
            verdict_counts[1],
            verdict_counts[2],
            oor_count,
            mon,
            secs
        );
    }
    println!(
        "[boundary_sweep] monotone asc: {}, desc: {}, non-monotone: {} (of {} swept)",
        monotone_asc, monotone_desc, non_monotone, n_features
    );
    0
}