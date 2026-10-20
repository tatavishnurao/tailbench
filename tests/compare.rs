//! A/B verdicts on a seeded fake target: `compare` must say Slower only when
//! the slowdown shows up in every run of B, and must stay inconclusive (or
//! refuse) when it does not.
//!
//! Uses tokio's paused clock, like tests/stall.rs, so a 10 s run takes
//! milliseconds of wall time and every result is reproducible from its seeds.

use std::collections::BTreeMap;
use std::time::Duration;

use tailbench::compare::{
    CompareWarning, Comparison, EpisodeOutcome, RecordedRun, SplitOptions, Verdict, compare, compare_split,
};
use tailbench::manifest::{Manifest, Probe};
use tailbench::stats::{CiWarning, SplitMix64};
use tailbench::{RunConfig, RunnerKind, closed_loop, open_loop};
use tokio::time::{Instant, sleep, sleep_until};

// 100 requests/s for 10 s per run.
const INTERVAL: Duration = Duration::from_millis(10);
const COUNT: u64 = 1_000;
const RUNS: usize = 5;

const FLOOR: Duration = Duration::from_millis(1);
/// Median of the lognormal part of the service time, before the run effect.
const MEDIAN_NS: f64 = 4e6;
/// Log-sd of one request's service time around its run's median.
const SIGMA: f64 = 0.5;
/// Log-sd of a run's median around MEDIAN_NS: the between-run variance.
const RUN_SIGMA: f64 = 0.05;

const STALL_AT: Duration = Duration::from_secs(2);
const STALL_FOR: Duration = Duration::from_secs(1);

const P99: f64 = 0.99;

/// Seeded fake server.
///
/// - Seeded per run: the seed fixes this run's median and every service time.
/// - Run effect: the run's median is MEDIAN_NS * exp(RUN_SIGMA * z), z ~ N(0, 1),
///   drawn first, so runs with different seeds differ a little as a whole.
/// - Optional body scale: the run's median is multiplied by it, so every
///   service time but FLOOR is that much slower (`scaled`).
/// - Service time = FLOOR + run median * exp(SIGMA * z_i), z_i ~ N(0, 1),
///   drawn from `SplitMix64` when `call` is invoked (at send time).
/// - On the paused clock a sleep ends on a whole-millisecond tick, so the
///   observed service time is the drawn one rounded UP to whole ms (see
///   `seeded_server_is_reproducible_per_seed`).
/// - Optional stall, the same model as tests/stall.rs: a request arriving in
///   [STALL_AT, STALL_AT + STALL_FOR) after creation waits until the window
///   ends, then takes its service time. No queue: requests are independent.
struct SeededServer {
    rng: SplitMix64,
    run_median_ns: f64,
    stall: Option<(Instant, Instant)>,
}

impl SeededServer {
    fn starting_now(seed: u64, stalled: bool) -> Self {
        Self::scaled(seed, stalled, 1.0)
    }

    fn scaled(seed: u64, stalled: bool, scale: f64) -> Self {
        let mut rng = SplitMix64::new(seed);
        let run_median_ns = scale * MEDIAN_NS * (RUN_SIGMA * rng.normal()).exp();
        let stall = stalled.then(|| {
            let start = Instant::now() + STALL_AT;
            (start, start + STALL_FOR)
        });
        Self {
            rng,
            run_median_ns,
            stall,
        }
    }

    fn service_time(&mut self) -> Duration {
        let ns = self.run_median_ns * (SIGMA * self.rng.normal()).exp();
        FLOOR + Duration::from_nanos(ns.round() as u64)
    }

    fn call(&mut self) -> impl Future<Output = ()> + Send + use<> {
        let arrival = Instant::now();
        let service = self.service_time();
        let stall = self.stall;
        async move {
            if let Some((start, end)) = stall
                && arrival >= start
                && arrival < end
            {
                sleep_until(end).await;
            }
            sleep(service).await;
        }
    }
}

/// A manifest for this machine with the git fields set clean, so verdict
/// lines do not depend on the state of the working tree.
fn clean_manifest(kind: RunnerKind) -> Manifest {
    let config = RunConfig {
        kind,
        interval: INTERVAL,
        count: COUNT,
        seed: None,
        extra: BTreeMap::from([("target".to_owned(), "seeded-fake".to_owned())]),
    };
    let mut m = Manifest::collect(&config);
    m.build_git_dirty = Probe::Known(false);
    m.runtime_git_dirty = Probe::Known(false);
    m.sha_mismatch = Probe::Known(false);
    m
}

/// One open-loop run per seed; run `i` is stalled if `stalled(i)`.
async fn open_runs(
    manifest: &Manifest,
    seeds: impl IntoIterator<Item = u64>,
    stalled: impl Fn(usize) -> bool,
) -> Vec<RecordedRun> {
    open_runs_scaled(manifest, seeds, stalled, 1.0).await
}

/// `open_runs` with every run's body slower by `scale`.
async fn open_runs_scaled(
    manifest: &Manifest,
    seeds: impl IntoIterator<Item = u64>,
    stalled: impl Fn(usize) -> bool,
    scale: f64,
) -> Vec<RecordedRun> {
    let mut runs = Vec::new();
    for (i, seed) in seeds.into_iter().enumerate() {
        let mut server = SeededServer::scaled(seed, stalled(i), scale);
        let result = open_loop(INTERVAL, COUNT, || server.call()).await;
        let mut manifest = manifest.clone();
        manifest.seed = Some(seed);
        runs.push(RecordedRun { result, manifest });
    }
    runs
}

fn seeds(base: u64, n: usize) -> impl Iterator<Item = u64> {
    (0..n as u64).map(move |i| base + i)
}

fn never(_: usize) -> bool {
    false
}

fn always(_: usize) -> bool {
    true
}

fn reason(cmp: &Comparison) -> &str {
    match &cmp.verdict {
        Verdict::Inconclusive { reason } | Verdict::Refused { reason } => reason,
        v => panic!("expected no direction, got {v:?}: {cmp}"),
    }
}

fn is_directional(v: &Verdict) -> bool {
    matches!(v, Verdict::Slower { .. } | Verdict::Faster { .. })
}

#[tokio::test(start_paused = true)]
async fn seeded_server_is_reproducible_per_seed() {
    let m = clean_manifest(RunnerKind::OpenLoop);
    let a = open_runs(&m, [7], never).await;
    let again = open_runs(&m, [7], never).await;
    let other = open_runs(&m, [8], never).await;
    assert_eq!(a[0].result.samples, again[0].result.samples, "same seed, same samples");
    assert_ne!(a[0].result.samples, other[0].result.samples, "seed has no effect");

    // Paused clock: every service time is a whole number of ms >= FLOOR + 1 ms
    // (FLOOR plus a positive lognormal draw, rounded up to the tick).
    let svc: Vec<u64> = a[0].result.samples.iter().map(|s| s.service_ns.unwrap()).collect();
    let mut distinct = svc.clone();
    distinct.sort_unstable();
    distinct.dedup();
    println!(
        "seed 7 distinct service times (ms): {:?}",
        distinct.iter().map(|ns| ns / 1_000_000).collect::<Vec<_>>()
    );
    assert!(svc.iter().all(|ns| ns % 1_000_000 == 0), "service not on whole ms");
    assert!(svc.iter().all(|&ns| ns >= 2_000_000));
}

#[tokio::test(start_paused = true)]
async fn same_distribution_different_seeds_is_inconclusive() {
    let m = clean_manifest(RunnerKind::OpenLoop);
    let a = open_runs(&m, seeds(1, RUNS), never).await;
    let b = open_runs(&m, seeds(101, RUNS), never).await;
    let cmp = compare(&a, &b, P99, Duration::ZERO);
    println!("{cmp}");
    assert!(matches!(cmp.verdict, Verdict::Inconclusive { .. }), "{cmp}");
    assert!(cmp.warnings.is_empty(), "{:?}", cmp.warnings);
}

#[tokio::test(start_paused = true)]
async fn stall_in_every_b_run_is_slower() {
    let m = clean_manifest(RunnerKind::OpenLoop);
    let a = open_runs(&m, seeds(1, RUNS), never).await;
    let b = open_runs(&m, seeds(101, RUNS), always).await;
    let cmp = compare(&a, &b, P99, Duration::from_millis(1));
    println!("{cmp}");
    let Verdict::Slower { delta, lo, hi } = cmp.verdict else {
        panic!("expected Slower: {cmp}");
    };
    assert!(lo > 0 && lo <= delta && delta <= hi, "{cmp}");
}

/// The stall holds 100 of B's 5000 samples, more than the 50 above p99, so
/// the pooled p99 of B lands inside the stall. But that is one event in one
/// run, so a rerun of B would mostly not see it.
#[tokio::test(start_paused = true)]
async fn stall_in_one_of_five_b_runs_is_not_slower() {
    let m = clean_manifest(RunnerKind::OpenLoop);
    let a = open_runs(&m, seeds(1, RUNS), never).await;
    let b = open_runs(&m, seeds(101, RUNS), |i| i == 0).await;
    let cmp = compare(&a, &b, P99, Duration::from_millis(1));
    println!("{cmp}");
    let reason = reason(&cmp);
    assert!(matches!(cmp.verdict, Verdict::Inconclusive { .. }), "{cmp}");
    assert!(
        reason.contains("episode-limited") || reason.contains("crosses 0"),
        "{reason}"
    );
}

/// One run each, with a stall in B's only run: a huge effect, but nothing
/// says how far a rerun would move either p99, so no direction is given.
#[tokio::test(start_paused = true)]
async fn one_run_each_is_inconclusive_between_run_variance_unknown() {
    let m = clean_manifest(RunnerKind::OpenLoop);
    let a = open_runs(&m, seeds(1, 1), never).await;
    let b = open_runs(&m, seeds(101, 1), always).await;
    let cmp = compare(&a, &b, P99, Duration::from_millis(1));
    println!("{cmp}");
    assert!(!is_directional(&cmp.verdict), "{cmp}");
    assert!(reason(&cmp).contains("between-run variance unknown"), "{cmp}");
    for group in [tailbench::compare::Group::A, tailbench::compare::Group::B] {
        let w = CompareWarning::Ci {
            group,
            warning: CiWarning::BetweenRunVarianceUnknown { runs: 1 },
        };
        assert!(cmp.warnings.contains(&w), "missing {w}: {:?}", cmp.warnings);
    }
}

#[tokio::test(start_paused = true)]
async fn environment_differences_warn_but_still_decide() {
    let mut release = clean_manifest(RunnerKind::OpenLoop);
    release.build_profile = Probe::Known("release".to_owned());
    let mut debug = release.clone();
    debug.build_profile = Probe::Known("debug".to_owned());
    let a = open_runs(&release, seeds(1, RUNS), never).await;
    let mut b = open_runs(&debug, seeds(101, RUNS), always).await;
    b[0].manifest.build_git_dirty = Probe::Known(true);
    b[1].manifest.sha_mismatch = Probe::Known(true);

    let cmp = compare(&a, &b, P99, Duration::from_millis(1));
    println!("{cmp}");
    assert!(matches!(cmp.verdict, Verdict::Slower { .. }), "{cmp}");
    let want = [
        CompareWarning::ManifestDiffers {
            field: "build_profile",
            a: vec!["release".to_owned()],
            b: vec!["debug".to_owned()],
        },
        CompareWarning::GitDirty { a: 0, b: 1 },
        CompareWarning::ShaMismatch { a: 0, b: 1 },
    ];
    for w in want {
        assert!(cmp.warnings.contains(&w), "missing {w}: {:?}", cmp.warnings);
    }
    assert!(cmp.to_string().contains("build_profile differs (A: release; B: debug)"));
}

#[tokio::test(start_paused = true)]
async fn closed_loop_input_is_refused() {
    let open_m = clean_manifest(RunnerKind::OpenLoop);
    let closed_m = clean_manifest(RunnerKind::ClosedLoop);
    let mut closed = Vec::new();
    for seed in seeds(101, RUNS) {
        let mut server = SeededServer::starting_now(seed, true);
        let result = closed_loop(INTERVAL, COUNT, || server.call()).await;
        closed.push(RecordedRun {
            result,
            manifest: closed_m.clone(),
        });
    }
    let open = open_runs(&open_m, seeds(1, RUNS), never).await;

    let mixed = compare(&open, &closed, P99, Duration::from_millis(1));
    println!("{mixed}");
    assert!(matches!(mixed.verdict, Verdict::Refused { .. }), "{mixed}");
    assert!(reason(&mixed).contains("runners differ"), "{mixed}");

    let both_closed = compare(&closed, &closed, P99, Duration::from_millis(1));
    println!("{both_closed}");
    assert!(matches!(both_closed.verdict, Verdict::Refused { .. }), "{both_closed}");
    assert!(reason(&both_closed).contains("coordinated omission"), "{both_closed}");
}

/// R for the split tests. A stall in every B run and none in A, at R = 5
/// and equal exposure, gives at best k_A = 0 of k = 5 episodes: two-sided
/// p = 2 / 2^5 = 0.0625, which cannot reject at 0.05. R = 10 can
/// (p = 2 / 2^10).
const SPLIT_RUNS: usize = 10;

/// A stall in every B run and no other change: B has more episodes per
/// minute, and the body gives no direction. The body leaves out every
/// request sent during the 1 s stall: the last ones before its end wait
/// less than T (60 ms) and, left in, made the body read 1.4 ms slower.
#[tokio::test(start_paused = true)]
async fn split_stall_in_every_b_run_is_more_episodes_and_no_body_verdict() {
    let m = clean_manifest(RunnerKind::OpenLoop);
    let a = open_runs(&m, seeds(1, SPLIT_RUNS), never).await;
    let b = open_runs(&m, seeds(101, SPLIT_RUNS), always).await;
    let s = compare_split(&a, &b, P99, SplitOptions::default());
    println!("{s}");
    assert_eq!(s.episodes.outcome, EpisodeOutcome::More, "{s}");
    assert!(s.episodes.p.unwrap() < 0.05, "{s}");
    assert!(matches!(s.body.verdict, Verdict::Inconclusive { .. }), "{s}");
}

/// B's body 1.2x slower and no stalls: the body says Slower, and at
/// T = 10 x median the body has no episodes of its own, so the episode test
/// has too few to run.
#[tokio::test(start_paused = true)]
async fn split_body_regression_without_stalls_is_slower_body() {
    let m = clean_manifest(RunnerKind::OpenLoop);
    let a = open_runs(&m, seeds(1, RUNS), never).await;
    let b = open_runs_scaled(&m, seeds(101, RUNS), never, 1.2).await;
    let s = compare_split(&a, &b, P99, SplitOptions::default());
    println!("{s}");
    assert!(matches!(s.body.verdict, Verdict::Slower { .. }), "{s}");
    let EpisodeOutcome::Inconclusive { reasons } = &s.episodes.outcome else {
        panic!("expected Inconclusive episodes: {s}");
    };
    assert!(reasons.iter().any(|r| r.starts_with("too few episodes")), "{s}");
}

/// Stalls in 2 of 10 runs on both sides, and B's body 1.2x slower: the
/// stalls fill each stalled run's p99, but the body still shows B slower.
#[tokio::test(start_paused = true)]
async fn split_body_regression_under_equal_stalls_is_slower_body() {
    let m = clean_manifest(RunnerKind::OpenLoop);
    let every_fifth = |i: usize| i.is_multiple_of(5);
    let a = open_runs(&m, seeds(1, SPLIT_RUNS), every_fifth).await;
    let b = open_runs_scaled(&m, seeds(101, SPLIT_RUNS), every_fifth, 1.2).await;
    let s = compare_split(&a, &b, P99, SplitOptions::default());
    println!("{s}");
    println!(
        "per-run p99 of all samples: {}",
        compare(&a, &b, P99, Duration::from_millis(1))
    );
    assert!(matches!(s.body.verdict, Verdict::Slower { .. }), "{s}");
}

#[tokio::test(start_paused = true)]
async fn split_closed_loop_input_is_refused_for_both_parts() {
    let open_m = clean_manifest(RunnerKind::OpenLoop);
    let closed_m = clean_manifest(RunnerKind::ClosedLoop);
    let mut closed = Vec::new();
    for seed in seeds(101, RUNS) {
        let mut server = SeededServer::starting_now(seed, true);
        let result = closed_loop(INTERVAL, COUNT, || server.call()).await;
        closed.push(RecordedRun {
            result,
            manifest: closed_m.clone(),
        });
    }
    let open = open_runs(&open_m, seeds(1, RUNS), never).await;
    let s = compare_split(&open, &closed, P99, SplitOptions::default());
    println!("{s}");
    assert!(
        matches!(&s.body.verdict, Verdict::Refused { reason } if reason.contains("coordinated omission")),
        "{s}"
    );
    assert!(
        matches!(&s.episodes.outcome, EpisodeOutcome::Refused { reason } if reason.contains("coordinated omission")),
        "{s}"
    );
    assert_eq!((s.threshold, s.episodes.p), (None, None));
}

/// False verdict rate: A and B from the same distribution (different seeds,
/// R = 5 vs 5), 100 trials. Each trial should be Inconclusive; the fraction
/// that says Slower or Faster is printed, for min_effect 0 and 1 ms.
/// Slow: run with `cargo test --release -- --ignored`.
#[tokio::test(start_paused = true)]
#[ignore = "slow: 100 trials x 10 runs x 2 bootstraps; run with --release --ignored"]
async fn false_verdict_rate_same_distribution() {
    const TRIALS: u64 = 100;
    let m = clean_manifest(RunnerKind::OpenLoop);
    let (mut false_zero, mut false_1ms) = (0u32, 0u32);
    for t in 0..TRIALS {
        let a = open_runs(&m, seeds(t * 1000, RUNS), never).await;
        let b = open_runs(&m, seeds(t * 1000 + 500, RUNS), never).await;
        let zero = compare(&a, &b, P99, Duration::ZERO);
        let one = compare(&a, &b, P99, Duration::from_millis(1));
        if is_directional(&zero.verdict) {
            println!("trial {t}: {zero}");
        }
        false_zero += u32::from(is_directional(&zero.verdict));
        false_1ms += u32::from(is_directional(&one.verdict));
    }
    let rate_zero = f64::from(false_zero) / TRIALS as f64;
    let rate_1ms = f64::from(false_1ms) / TRIALS as f64;
    println!("RUN_SIGMA = {RUN_SIGMA}, R = {RUNS} vs {RUNS}, {TRIALS} trials, p99, nominal 95%");
    println!("false Slower/Faster rate, min_effect 0:    {rate_zero:.2} ({false_zero}/{TRIALS})");
    println!("false Slower/Faster rate, min_effect 1 ms: {rate_1ms:.2} ({false_1ms}/{TRIALS})");
    assert!(rate_zero <= 0.10, "false verdict rate {rate_zero} > 0.10");
}
