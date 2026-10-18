//! Percentiles of raw samples, and bootstrap confidence intervals for them.
//!
//! Latency samples from one run are not independent: a stall delays a
//! contiguous run of requests, so slow samples come in clumps. The iid
//! bootstrap resamples single samples, which breaks the clumps apart, so it
//! understates how far a tail percentile could move in another run. The
//! moving-block bootstrap resamples contiguous blocks instead, which keeps
//! dependence shorter than a block inside each block.
//!
//! Every function here takes samples in the order they were SCHEDULED, as
//! `RunResult::samples` stores them. Sorting them first throws away exactly
//! the clumping that the block bootstrap needs.

use std::fmt;

/// Fewest samples that must lie above an estimated percentile before we
/// treat its CI as supported by the data.
pub const MIN_TAIL_SUPPORT: usize = 10;
/// Default number of bootstrap resamples.
pub const DEFAULT_RESAMPLES: usize = 2000;
/// Default confidence level.
pub const DEFAULT_LEVEL: f64 = 0.95;

fn check_quantile(q: f64) {
    assert!((0.0..=1.0).contains(&q), "quantile {q} is not in [0, 1]");
}

/// The `rank`-th smallest (1-based) of `buf`, reordering `buf` in O(n).
fn select(buf: &mut [u64], rank: usize) -> u64 {
    *buf.select_nth_unstable(rank - 1).1
}

/// 1-based nearest rank `max(1, ceil(q * n))`, computed with the same f64
/// arithmetic as `Histogram::value_at_quantile` in hdrhistogram 7.6.0.
///
/// Panics if `n == 0` or `q` is not in [0, 1].
fn rank(n: usize, q: f64) -> usize {
    assert!(n > 0, "rank of an empty sample");
    check_quantile(q);
    // q * n can only round above n when n > 2^53 is not exact in f64.
    ((q * n as f64).ceil() as usize).clamp(1, n)
}

/// The `q` percentile of `samples` by NEAREST RANK: the `max(1, ceil(q * n))`-th
/// smallest of the `n` samples, with no interpolation. `None` if `samples`
/// is empty. The order of `samples` does not matter here.
///
/// This is the same definition `LatencyHistogram::value_at_quantile` uses,
/// with the same f64 rank arithmetic, so both pick the same sample. They can
/// still disagree by HdrHistogram rounding: the histogram stores whole
/// microseconds and reports the HIGHEST value of that sample's bucket, so for
/// a sample of v us it reports r with v <= r < v + v / 1024 (q = 0 is the
/// exception: it reports the bucket's LOWEST value).
///
/// Panics if `q` is not in [0, 1].
pub fn percentile(samples: &[u64], q: f64) -> Option<u64> {
    check_quantile(q);
    if samples.is_empty() {
        return None;
    }
    Some(select(&mut samples.to_vec(), rank(samples.len(), q)))
}

/// Number of samples above the `q` percentile of `n` samples: `n` minus its
/// nearest rank. In exact arithmetic that is `floor(n * (1 - q))` (except at
/// q = 0, where the rank is still 1).
///
/// It is computed from the rank, not as `floor(n * (1.0 - q))` in f64,
/// because `1.0 - q` rounds down for some q: with q = 0.9 that formula gives
/// 9 for n = 100, not 10. The rank arithmetic is exact for the usual
/// percentiles, and it counts exactly the samples above the sample that
/// `percentile` returns.
///
/// Panics if `q` is not in [0, 1].
pub fn tail_support(n: usize, q: f64) -> usize {
    check_quantile(q);
    if n == 0 { 0 } else { n - rank(n, q) }
}

/// Smallest `n` with `tail_support(n, q) >= MIN_TAIL_SUPPORT`, for example
/// 1000 for p99 and 10 000 for p99.9. `None` for q = 1: the max never has a
/// sample above it.
///
/// Panics if `q` is not in [0, 1].
pub fn samples_needed(q: f64) -> Option<usize> {
    check_quantile(q);
    if q == 1.0 {
        return None;
    }
    // Start from the exact-arithmetic answer, then step to the smallest n on
    // which the rank arithmetic agrees.
    let mut n = (MIN_TAIL_SUPPORT as f64 / (1.0 - q)).ceil() as usize;
    while tail_support(n, q) < MIN_TAIL_SUPPORT {
        n += 1;
    }
    while n > 1 && tail_support(n - 1, q) >= MIN_TAIL_SUPPORT {
        n -= 1;
    }
    Some(n)
}

/// Default block length: `ceil(n^(1/3))`, and at least 1.
///
/// Computed in integers (smallest b with b^3 >= n), because `cbrt` in f64 can
/// land a hair above an exact cube root and ceil to one more.
pub fn default_block_len(n: usize) -> usize {
    // The f64 cube root is within one of the answer; fix it up exactly.
    let mut b = (n as f64).cbrt() as usize;
    while b.saturating_pow(3) < n {
        b += 1;
    }
    while b > 1 && (b - 1).pow(3) >= n {
        b -= 1;
    }
    b.max(1)
}

/// Why a CI should not be taken at face value.
#[derive(Debug, Clone, PartialEq)]
pub enum CiWarning {
    /// Fewer than `MIN_TAIL_SUPPORT` samples lie above the estimate, so the
    /// bootstrap can only reshuffle a handful of tail values and its interval
    /// is mostly an artefact of which few samples happened to be slow.
    InsufficientSamples {
        q: f64,
        n: usize,
        /// `tail_support(n, q)`.
        support: usize,
        /// `samples_needed(q)`; `None` if no sample size is enough (q = 1).
        needed: Option<usize>,
    },
    /// The estimate is the largest sample. No resample can contain a value
    /// above it, so `hi` cannot exceed it either: the upper bound is set by
    /// the slowest request seen, not by what a rerun could show.
    EstimateIsSampleMax,
}

impl fmt::Display for CiWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CiWarning::InsufficientSamples { q, n, support, needed } => {
                let p = quantile_label(*q);
                write!(
                    f,
                    "insufficient samples: {p} of {n} samples has {support} above it (want >= {MIN_TAIL_SUPPORT}); "
                )?;
                match needed {
                    Some(needed) => write!(f, "{p} needs >= {needed} samples"),
                    None => write!(f, "no sample size puts {MIN_TAIL_SUPPORT} samples above {p}"),
                }
            }
            CiWarning::EstimateIsSampleMax => f.write_str(
                "estimate is the sample max: no resample can exceed it, so hi is capped at the slowest request seen",
            ),
        }
    }
}

/// "p99.9" for 0.999: q as a percentage, to at most 6 decimals.
fn quantile_label(q: f64) -> String {
    let pct = format!("{:.6}", q * 100.0);
    format!("p{}", pct.trim_end_matches('0').trim_end_matches('.'))
}

/// A percentile estimate with a bootstrap confidence interval, all in the
/// samples' unit.
#[derive(Debug, Clone, PartialEq)]
pub struct PercentileCi {
    /// `percentile(samples, q)` of the samples as given (not of a resample).
    pub estimate: u64,
    pub lo: u64,
    pub hi: u64,
    /// Empty if nothing looked wrong.
    pub warnings: Vec<CiWarning>,
}

/// Circular moving-block bootstrap CI for the `q` percentile of `samples`.
///
/// Each resample is built from blocks of `block_len` consecutive samples,
/// each starting at a uniformly random index and wrapping from the end of
/// `samples` back to the start (circular), so every sample is equally likely
/// to be drawn. Blocks are concatenated and cut to `samples.len()`.
///
/// The interval is the percentile interval: of the `resamples` bootstrap
/// percentiles, sorted, `lo` and `hi` cut off the same number,
/// `floor(resamples * (1 - level) / 2)`, at each end. With the defaults
/// (`DEFAULT_RESAMPLES`, `DEFAULT_LEVEL`) that is 50 of 2000 per side.
///
/// Deterministic: the same arguments always give the same result.
///
/// Defaults: `block_len = default_block_len(n)`, `DEFAULT_RESAMPLES`,
/// `DEFAULT_LEVEL`. `None` if `samples` is empty.
///
/// Panics if `q` is not in [0, 1], `level` is not in (0, 1), or `block_len`
/// or `resamples` is 0.
pub fn mbb_ci(
    samples: &[u64],
    q: f64,
    block_len: usize,
    resamples: usize,
    seed: u64,
    level: f64,
) -> Option<PercentileCi> {
    check_quantile(q);
    assert!(level > 0.0 && level < 1.0, "level {level} is not in (0, 1)");
    assert!(block_len > 0, "block_len must be at least 1");
    assert!(resamples > 0, "resamples must be at least 1");
    let n = samples.len();
    if n == 0 {
        return None;
    }
    let r = rank(n, q);

    let mut rng = SplitMix64(seed);
    let mut resample = Vec::with_capacity(n);
    let mut boot = Vec::with_capacity(resamples);
    for _ in 0..resamples {
        resample.clear();
        while resample.len() < n {
            let mut start = rng.below(n);
            let mut left = block_len.min(n - resample.len());
            // `left <= n`, so the block wraps past the end at most once.
            while left > 0 {
                let end = (start + left).min(n);
                resample.extend_from_slice(&samples[start..end]);
                left -= end - start;
                start = 0;
            }
        }
        boot.push(select(&mut resample, r));
    }
    boot.sort_unstable();
    // `hi` is the nearest-rank (1 + level) / 2 quantile of the bootstrap
    // percentiles and `lo` its mirror image, so both ends drop the same
    // number. Mirroring also avoids computing 1 - level, which rounds in f64.
    let hi_rank = rank(resamples, (1.0 + level) / 2.0);
    let lo_rank = resamples + 1 - hi_rank;

    let estimate = percentile(samples, q).expect("samples is not empty");
    let mut warnings = Vec::new();
    let support = tail_support(n, q);
    if support < MIN_TAIL_SUPPORT {
        warnings.push(CiWarning::InsufficientSamples {
            q,
            n,
            support,
            needed: samples_needed(q),
        });
    }
    if samples.iter().max() == Some(&estimate) {
        warnings.push(CiWarning::EstimateIsSampleMax);
    }
    Some(PercentileCi {
        estimate,
        lo: boot[lo_rank - 1],
        hi: boot[hi_rank - 1],
        warnings,
    })
}

/// iid bootstrap CI: `mbb_ci` with `block_len = 1`, which draws each sample
/// independently and uniformly. For comparison only: on latency samples it
/// ignores the clumping of slow requests, so its interval is too narrow.
pub fn iid_ci(samples: &[u64], q: f64, resamples: usize, seed: u64, level: f64) -> Option<PercentileCi> {
    mbb_ci(samples, q, 1, resamples, seed, level)
}

/// SplitMix64 (Steele, Lea and Flood, 2014): a tiny, fast, seedable PRNG.
/// Not cryptographic; good enough for resampling indices.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` by multiply-and-shift. Its bias is below n / 2^64,
    /// which is negligible for any sample count.
    fn below(&mut self, n: usize) -> usize {
        ((u128::from(self.next_u64()) * n as u128) >> 64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl SplitMix64 {
        /// Uniform in [0, 1), from the top 53 bits.
        fn next_f64(&mut self) -> f64 {
            (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }

        /// Standard normal, by Box-Muller (one of the pair is discarded).
        fn normal(&mut self) -> f64 {
            let u1 = 1.0 - self.next_f64(); // (0, 1], so ln is finite
            let u2 = self.next_f64();
            (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
        }
    }

    /// Stationary AR(1) latency series: x_t = phi * x_{t-1} + sqrt(1 - phi^2) * e_t
    /// with e_t ~ N(0, 1) and x_0 ~ N(0, 1), so every x_t is N(0, 1) but
    /// neighbours are correlated (lag-k correlation phi^k). Latency is
    /// lognormal: 1 ms * exp(0.5 * x_t), in ns.
    fn ar1_latencies_ns(n: usize, phi: f64, seed: u64) -> Vec<u64> {
        let mut rng = SplitMix64(seed);
        let innovation = (1.0 - phi * phi).sqrt();
        let mut x = rng.normal();
        (0..n)
            .map(|_| {
                let ns = (1e6 * (0.5 * x).exp()).round() as u64;
                x = phi * x + innovation * rng.normal();
                ns
            })
            .collect()
    }

    /// 1..=n in a fixed scrambled order (7919 is prime, so `i * 7919 % n` is a
    /// permutation for n not divisible by 7919).
    fn scrambled(n: u64) -> Vec<u64> {
        (0..n).map(|i| i * 7919 % n + 1).collect()
    }

    #[test]
    fn splitmix64_matches_reference_output() {
        // First outputs for seed 0, from the reference implementation.
        let mut rng = SplitMix64(0);
        assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(rng.next_u64(), 0x06C4_5D18_8009_454F);
    }

    /// Same values as the histogram definition test in lib.rs, before bucket
    /// rounding: 50th, 99th and 100th of 1..=100.
    #[test]
    fn percentile_is_nearest_rank() {
        let s = scrambled(100);
        assert_eq!(percentile(&s, 0.0), Some(1), "rank is at least 1");
        assert_eq!(percentile(&s, 0.50), Some(50));
        assert_eq!(percentile(&s, 0.505), Some(51), "ceil, not round or interpolate");
        assert_eq!(percentile(&s, 0.99), Some(99));
        assert_eq!(percentile(&s, 1.0), Some(100));
        assert_eq!(percentile(&[], 0.5), None);
    }

    #[test]
    fn default_block_len_is_ceil_cube_root() {
        let cases = [
            (0, 1),
            (1, 1),
            (2, 2),
            (8, 2),
            (9, 3),
            (1000, 10),
            (1001, 11),
            (5000, 18),
        ];
        for (n, want) in cases {
            assert_eq!(default_block_len(n), want, "n = {n}");
        }
    }

    #[test]
    fn tail_support_and_samples_needed() {
        assert_eq!(tail_support(1000, 0.99), 10);
        assert_eq!(tail_support(1000, 0.999), 1);
        assert_eq!(tail_support(10_000, 0.999), 10);
        // floor(100 * (1.0 - 0.9)) in f64 is 9; the true count is 10.
        assert_eq!(tail_support(100, 0.9), 10);
        assert_eq!(tail_support(0, 0.99), 0);

        assert_eq!(samples_needed(0.5), Some(20));
        assert_eq!(samples_needed(0.9), Some(100));
        assert_eq!(samples_needed(0.99), Some(1000));
        assert_eq!(samples_needed(0.999), Some(10_000));
        assert_eq!(samples_needed(0.9999), Some(100_000));
        assert_eq!(samples_needed(1.0), None);
    }

    fn ci(samples: &[u64], q: f64, seed: u64) -> PercentileCi {
        let b = default_block_len(samples.len());
        mbb_ci(samples, q, b, DEFAULT_RESAMPLES, seed, DEFAULT_LEVEL).expect("non-empty samples")
    }

    fn has_insufficient(ci: &PercentileCi) -> bool {
        ci.warnings
            .iter()
            .any(|w| matches!(w, CiWarning::InsufficientSamples { .. }))
    }

    #[test]
    fn p999_on_1000_samples_is_insufficient() {
        let r = ci(&scrambled(1000), 0.999, 1);
        assert_eq!(r.estimate, 999);
        assert_eq!(
            r.warnings,
            vec![CiWarning::InsufficientSamples {
                q: 0.999,
                n: 1000,
                support: 1,
                needed: Some(10_000)
            }]
        );
        let msg = r.warnings[0].to_string();
        println!("{msg}");
        assert!(msg.contains("p99.9 needs >= 10000 samples"), "{msg}");
    }

    #[test]
    fn p99_on_1000_samples_is_sufficient() {
        let r = ci(&scrambled(1000), 0.99, 1);
        assert_eq!(r.estimate, 990);
        assert!(r.warnings.is_empty(), "support is exactly 10: {:?}", r.warnings);
    }

    #[test]
    fn estimate_at_sample_max_warns() {
        let r = ci(&scrambled(1000), 1.0, 1);
        assert_eq!(r.estimate, 1000);
        assert_eq!(r.hi, 1000, "no resample exceeds the sample max");
        assert!(has_insufficient(&r));
        assert!(r.warnings.contains(&CiWarning::EstimateIsSampleMax));
        let msgs: Vec<String> = r.warnings.iter().map(ToString::to_string).collect();
        println!("{msgs:#?}");
        assert!(msgs[0].contains("no sample size"), "{}", msgs[0]);

        // A tied max also counts: the estimate equals the max VALUE.
        let r = ci(&[5, 7, 7], 0.5, 1);
        assert_eq!(r.estimate, 7);
        assert!(r.warnings.contains(&CiWarning::EstimateIsSampleMax));
    }

    #[test]
    fn ci_brackets_estimate_on_constant_data() {
        let r = ci(&[42; 100], 0.9, 3);
        assert_eq!((r.estimate, r.lo, r.hi), (42, 42, 42));
    }

    #[test]
    fn same_seed_gives_identical_ci() {
        let s = ar1_latencies_ns(1000, 0.9, 11);
        assert_eq!(ci(&s, 0.99, 7), ci(&s, 0.99, 7));
        let iid = |seed| iid_ci(&s, 0.99, DEFAULT_RESAMPLES, seed, DEFAULT_LEVEL);
        assert_eq!(iid(7), iid(7));
    }

    /// A different seed MAY give the same interval; over ten seeds, at least
    /// one must differ, or the seed is ignored.
    ///
    /// This uses the median, not p99: `lo` and `hi` are always sample values,
    /// and with only 10 samples above p99 of 1000, the iid interval landed on
    /// the same two order statistics (982nd and 995th) for every seed tried.
    #[test]
    fn different_seeds_can_give_different_ci() {
        let s = ar1_latencies_ns(1000, 0.9, 11);
        let base = ci(&s, 0.5, 7);
        assert!(
            (8..18).any(|seed| ci(&s, 0.5, seed) != base),
            "seed has no effect on mbb_ci"
        );
        let iid = |seed| iid_ci(&s, 0.5, DEFAULT_RESAMPLES, seed, DEFAULT_LEVEL);
        let base = iid(7);
        assert!((8..18).any(|seed| iid(seed) != base), "seed has no effect on iid_ci");
    }

    #[test]
    fn iid_is_mbb_with_block_len_one() {
        let s = ar1_latencies_ns(500, 0.9, 5);
        assert_eq!(iid_ci(&s, 0.95, 300, 9, 0.9), mbb_ci(&s, 0.95, 1, 300, 9, 0.9));
    }

    /// Coverage on correlated data: does a 95% CI contain the true p99 about
    /// 95% of the time?
    ///
    /// "True" p99 is the nearest-rank p99 of one series of 10M samples (and
    /// should be close to the analytic exp(0.5 * z_0.99) ms). Then for 200
    /// seeds, a fresh series of n = 5000 gets an mbb CI (default block length)
    /// and an iid CI. Slow: run with `cargo test --release -- --ignored`.
    #[test]
    #[ignore = "slow: 200 x 2 x 2000 bootstrap resamples; run with --release --ignored"]
    fn mbb_covers_true_p99_of_ar1_series() {
        const PHI: f64 = 0.9;
        const Q: f64 = 0.99;
        const N: usize = 5000;
        const SEEDS: u64 = 200;
        const LONG_N: usize = 10_000_000;
        // z such that P(Z <= z) = 0.99 for a standard normal.
        const Z_99: f64 = 2.326_347_874_040_841;

        let truth = percentile(&ar1_latencies_ns(LONG_N, PHI, u64::MAX), Q).unwrap();
        let analytic = 1e6 * (0.5 * Z_99).exp();
        let block_len = default_block_len(N);
        println!("phi = {PHI}, n = {N}, block_len = {block_len}, resamples = {DEFAULT_RESAMPLES}, seeds = {SEEDS}");
        println!("true p99 from {LONG_N} samples: {truth} ns (analytic {analytic:.0} ns)");

        let (mut mbb_hits, mut iid_hits) = (0u32, 0u32);
        let (mut mbb_width, mut iid_width) = (0.0, 0.0);
        for seed in 0..SEEDS {
            let s = ar1_latencies_ns(N, PHI, seed);
            let boot_seed = seed ^ 0xB007_B007;
            let m = mbb_ci(&s, Q, block_len, DEFAULT_RESAMPLES, boot_seed, DEFAULT_LEVEL).unwrap();
            let i = iid_ci(&s, Q, DEFAULT_RESAMPLES, boot_seed, DEFAULT_LEVEL).unwrap();
            assert!(m.warnings.is_empty() && i.warnings.is_empty(), "n = {N} supports p99");
            mbb_hits += u32::from(m.lo <= truth && truth <= m.hi);
            iid_hits += u32::from(i.lo <= truth && truth <= i.hi);
            mbb_width += (m.hi - m.lo) as f64;
            iid_width += (i.hi - i.lo) as f64;
        }
        let seeds = SEEDS as f64;
        let mbb_cov = f64::from(mbb_hits) / seeds;
        let iid_cov = f64::from(iid_hits) / seeds;
        println!(
            "mbb: coverage {mbb_cov:.3} ({mbb_hits}/{SEEDS}), mean width {:.0} ns",
            mbb_width / seeds
        );
        println!(
            "iid: coverage {iid_cov:.3} ({iid_hits}/{SEEDS}), mean width {:.0} ns",
            iid_width / seeds
        );
        assert!(mbb_cov >= 0.88, "mbb coverage {mbb_cov} < 0.88");
    }
}
