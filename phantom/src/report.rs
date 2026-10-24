//! `phantom report`: open-loop truth and server time for every request in a
//! truth log, as a terminal chart and table or a self-contained HTML page.
//!
//! Request `i` is the request with `seq` `i`, the `i`-th send of a schedule
//! with one request every `period`. Its intended send time is
//! `origin + i * period` on phantom's clock, where
//!
//! `origin = min over i of (arrival_ns_i - i * period)`
//!
//! (see [`schedule_origin`] for why), so its open-loop truth is
//!
//! `truth_i = sent_ns_i - (origin + i * period)`
//!
//! and its server time is `sent_ns_i - arrival_ns_i`. Truth is never below
//! server time: `arrival_ns_i` is never before `origin + i * period`.
//! Percentiles are nearest rank.

use std::fmt::{self, Write as _};

use serde_json::Value;

use crate::live::{
    MIN_ARRIVALS_BEFORE, RED_RATIO, STALL_UNOBSERVED, StallCounts, Verdict, fmt_p99, verdict_sentence, wrap,
};

const NS_PER_MS: f64 = 1e6;
const NS_PER_S: f64 = 1e9;

/// Which percentiles the report prints; `None` is the maximum.
pub const QUANTILES: [(&str, Option<f64>); 4] = [
    ("p50", Some(0.50)),
    ("p99", Some(0.99)),
    ("p99.9", Some(0.999)),
    ("max", None),
];

/// The terminal chart is never wider than this many columns.
pub const MAX_CHART_WIDTH: usize = 100;

/// Above this many requests the HTML timeline plots the maximum per column
/// instead of every request.
const HTML_MAX_POINTS: usize = 2_000;

/// A log or rate the report cannot use, with a message that says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportError(String);

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReportError {}

/// Parses `--rate`: requests per second, finite and > 0, into the schedule's
/// period in whole nanoseconds.
pub fn parse_rate(s: &str) -> Result<i64, ReportError> {
    let invalid = || {
        ReportError(format!(
            "invalid rate {s:?}: expected requests per second, a number > 0"
        ))
    };
    let rate = s.parse::<f64>().map_err(|_| invalid())?;
    if !(rate.is_finite() && rate > 0.0) {
        return Err(invalid());
    }
    let period = (NS_PER_S / rate).round();
    if !(1.0..=i64::MAX as f64).contains(&period) {
        return Err(ReportError(format!(
            "invalid rate {s:?}: the period must be at least 1 ns"
        )));
    }
    Ok(period as i64)
}

/// The schedule start on phantom's clock: the minimum over requests of
/// `arrival_ns_i - i * period_ns`, for (i, arrival_ns_i) pairs; `None` for no
/// requests.
///
/// Why the least-lagged request: arrival_ns_i - i * period is the schedule
/// start on phantom's clock PLUS request i's own delay (scheduler lag, client
/// send, loopback, header parsing), and that delay is never negative. Any
/// single request, request 0 included, carries its own delay into every truth
/// value; the minimum over all requests carries only the smallest one. Stalls
/// do not bias it: phantom applies them after arrival, and late sends only
/// raise arrivals.
pub fn schedule_origin(arrivals: impl IntoIterator<Item = (i64, u64)>, period_ns: i64) -> Option<i64> {
    arrivals
        .into_iter()
        .map(|(i, arrival)| arrival as i64 - i * period_ns)
        .min()
}

/// One completed request, as the open-loop truth formula needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completed {
    /// phantom's seq: the request's index `i` in the schedule.
    pub seq: u64,
    /// ns since t0.
    pub arrival_ns: u64,
    /// ns since t0.
    pub sent_ns: u64,
}

/// The schedule origin ([`schedule_origin`]) and each request's open-loop
/// truth, `sent_ns - (origin + seq * period_ns)`, in the order given; `None`
/// for no requests. `phantom report` and the `--live` panel both use it.
pub fn open_loop_truth(requests: &[Completed], period_ns: i64) -> Option<(i64, Vec<i64>)> {
    let origin = schedule_origin(requests.iter().map(|r| (r.seq as i64, r.arrival_ns)), period_ns)?;
    let truth = requests
        .iter()
        .map(|r| r.sent_ns as i64 - (origin + r.seq as i64 * period_ns))
        .collect();
    Some((origin, truth))
}

/// Arrivals further apart than this start a new run, whatever the rate.
pub const MIN_RUN_GAP_NS: u64 = 1_000_000_000;

/// Arrivals further apart than this many median inter-arrival times start a
/// new run.
pub const RUN_GAP_FACTOR: u64 = 20;

/// One run: arrivals with no gap between them long enough to be idle time
/// (see [`find_runs`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// The first arrival, in ns since t0.
    pub start_ns: u64,
    /// The last arrival, in ns since t0.
    pub end_ns: u64,
    /// Arrivals in the run.
    pub arrivals: usize,
}

/// The gap that separates runs: `max(1 s, 20 x median inter-arrival)`.
pub fn run_gap_threshold_ns(median_gap_ns: u64) -> u64 {
    MIN_RUN_GAP_NS.max(RUN_GAP_FACTOR.saturating_mul(median_gap_ns))
}

/// Whether the gap from arrival `prev_ns` to arrival `next_ns` ends a run:
/// it is longer than `threshold_ns`, and a stall window does not explain it.
///
/// A closed-loop client sends nothing while the server is frozen, so its
/// gap across a long stall is a wait, not idle time: the last arrival came
/// within `threshold_ns` of the window's start (or inside it) and the next
/// one at the window's end or within `threshold_ns` after it.
pub fn gap_starts_run(prev_ns: u64, next_ns: u64, threshold_ns: u64, stalls_ns: &[(u64, u64)]) -> bool {
    if next_ns.saturating_sub(prev_ns) <= threshold_ns {
        return false;
    }
    !stalls_ns.iter().any(|&(start, end)| {
        prev_ns < end
            && next_ns >= end
            && prev_ns.saturating_add(threshold_ns) >= start
            && next_ns <= end.saturating_add(threshold_ns)
    })
}

/// The median of the gaps between consecutive values of `sorted`; 0 for
/// fewer than two. For an even number of gaps, the upper of the middle two.
pub fn median_gap_ns(sorted: &[u64]) -> u64 {
    let mut gaps: Vec<u64> = sorted.windows(2).map(|w| w[1] - w[0]).collect();
    gaps.sort_unstable();
    gaps.get(gaps.len() / 2).copied().unwrap_or(0)
}

/// Splits arrivals (ns since t0, any order) into runs at gaps longer than
/// `max(1 s, 20 x the median inter-arrival)`, except gaps a stall window in
/// `stalls_ns` explains ([`gap_starts_run`]). One run for no arrivals or one;
/// none for an empty list.
pub fn find_runs(arrivals_ns: &[u64], stalls_ns: &[(u64, u64)]) -> Vec<Run> {
    let mut sorted = arrivals_ns.to_vec();
    sorted.sort_unstable();
    let threshold = run_gap_threshold_ns(median_gap_ns(&sorted));
    let mut runs: Vec<Run> = Vec::new();
    for &t in &sorted {
        match runs.last_mut() {
            Some(run) if !gap_starts_run(run.end_ns, t, threshold, stalls_ns) => {
                run.end_ns = t;
                run.arrivals += 1;
            }
            _ => runs.push(Run {
                start_ns: t,
                end_ns: t,
                arrivals: 1,
            }),
        }
    }
    runs
}

/// Open-loop truth for requests spread over several runs: each run gets its
/// own schedule origin ([`schedule_origin`] over that run's requests), so
/// idle time between runs is not lateness. Returns `(origin, truth)` for each
/// request, in the order given; `None` for no requests. A request belongs to
/// the last run that started at or before its arrival. For one run this is
/// [`open_loop_truth`], request for request.
pub fn open_loop_truth_by_run(requests: &[Completed], runs: &[Run], period_ns: i64) -> Option<Vec<(i64, i64)>> {
    if requests.is_empty() {
        return None;
    }
    let run_of = |r: &Completed| {
        runs.partition_point(|run| run.start_ns <= r.arrival_ns)
            .saturating_sub(1)
    };
    let mut origins: Vec<Option<i64>> = vec![None; runs.len().max(1)];
    for r in requests {
        let o = r.arrival_ns as i64 - r.seq as i64 * period_ns;
        let slot = &mut origins[run_of(r)];
        *slot = Some(slot.map_or(o, |m| m.min(o)));
    }
    requests
        .iter()
        .map(|r| {
            let origin = origins[run_of(r)]?;
            Some((origin, r.sent_ns as i64 - (origin + r.seq as i64 * period_ns)))
        })
        .collect()
}

/// Nearest rank: the `ceil(q * n)`-th smallest of `sorted` (at least the
/// first); `None` is the maximum. Panics if `sorted` is empty.
pub fn nearest_rank(sorted: &[i64], q: Option<f64>) -> i64 {
    let n = sorted.len();
    let rank = q.map_or(n, |q| ((q * n as f64).ceil() as usize).max(1));
    sorted[rank - 1]
}

/// p50, p99, p99.9 and max of some values, in the order of [`QUANTILES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Percentiles(pub [i64; 4]);

impl Percentiles {
    /// Nearest-rank percentiles of `values`; `None` if there are none.
    pub fn of(values: impl IntoIterator<Item = i64>) -> Option<Self> {
        let mut sorted: Vec<i64> = values.into_iter().collect();
        if sorted.is_empty() {
            return None;
        }
        sorted.sort_unstable();
        Some(Self(QUANTILES.map(|(_, q)| nearest_rank(&sorted, q))))
    }

    /// The p99.
    pub fn p99(&self) -> i64 {
        self.0[1]
    }

    /// The maximum.
    pub fn max(&self) -> i64 {
        self.0[3]
    }
}

/// One completed request; times in ns.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// phantom's seq: the request's index `i` in the schedule.
    pub seq: u64,
    /// `i * period`: the intended send time, from the schedule start.
    pub intended_ns: i64,
    /// `sent_ns - (origin + i * period)`.
    pub truth_ns: i64,
    /// `sent_ns - arrival_ns`.
    pub server_ns: i64,
    /// Streamed requests only.
    pub stream: Option<StreamTimes>,
}

/// Per-token times of one completed streamed request, in ns.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamTimes {
    /// `token_ns[0] - (origin + i * period)`.
    pub ttft_truth_ns: i64,
    /// `token_ns[0] - arrival_ns`.
    pub ttft_server_ns: i64,
    /// The largest gap between consecutive tokens; `None` for one token.
    pub max_gap_ns: Option<i64>,
    /// `(last - first) / (tokens - 1)`; `None` for one token.
    pub mean_itl_ns: Option<f64>,
}

/// Everything the report shows, computed from one truth log.
#[derive(Debug, Clone)]
pub struct Report {
    /// The log's manifest line: the script it was run with.
    pub manifest: Value,
    /// The schedule's period, from `--rate`.
    pub period_ns: i64,
    /// The schedule start on phantom's clock (ns since t0; may be negative).
    pub origin_ns: i64,
    /// Completed requests, in seq order.
    pub requests: Vec<Request>,
    /// Lines with `"completed": false`.
    pub abandoned: usize,
    /// The manifest's stall windows as [start, end) in ns since t0.
    pub stalls_ns: Vec<(u64, u64)>,
    /// The arrival of every request line, completed or not, in ns since t0.
    pub arrivals_ns: Vec<u64>,
    /// The runs in the arrival sequence ([`find_runs`]), in time order.
    pub runs: Vec<Run>,
    /// The arrival rate measured from the log, per second: the median
    /// inter-arrival in the longest run; `None` if no run has two arrivals.
    pub measured_rate_per_s: Option<f64>,
    /// Whether the log's last line was cut off mid-JSON and left out.
    pub ignored_incomplete_line: bool,
}

impl Report {
    /// Reads a truth log (the manifest line, then one line per request) and
    /// computes truth for a schedule of one request every `period_ns`.
    pub fn from_log(text: &str, period_ns: i64) -> Result<Self, ReportError> {
        let mut lines: Vec<(usize, &str)> = text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()).collect();
        let parse = |(n, line): (usize, &str)| {
            serde_json::from_str::<Value>(line).map_err(|e| ReportError(format!("line {}: not JSON: {e}", n + 1)))
        };
        // A log still being written (or cut short) ends mid-line: no final
        // newline and not JSON. Leave that line out; the manifest is never it.
        let ignored_incomplete_line =
            lines.len() > 1 && !text.ends_with('\n') && lines.last().is_some_and(|&l| parse(l).is_err());
        if ignored_incomplete_line {
            lines.pop();
        }
        let mut lines = lines.into_iter();
        let manifest = lines
            .next()
            .map(parse)
            .transpose()?
            .filter(|m| m["type"] == "manifest")
            .ok_or_else(|| ReportError("line 1 is not a phantom manifest".to_owned()))?;
        let stalls_ns: Vec<(u64, u64)> = manifest["stalls"]
            .as_array()
            .map(|stalls| {
                stalls
                    .iter()
                    .filter_map(|s| {
                        let (start, len) = (s["start_ms"].as_u64()?, s["len_ms"].as_u64()?);
                        Some((start * 1_000_000, (start + len) * 1_000_000))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // (seq, arrival_ns, sent_ns, token_ns) of the completed requests.
        let mut completed = Vec::new();
        let mut abandoned = 0;
        let mut arrivals_ns = Vec::new();
        for (n, line) in lines {
            let r = parse((n, line))?;
            if r["type"] != "req" {
                continue;
            }
            arrivals_ns.extend(r["arrival_ns"].as_u64());
            if r["completed"] == false {
                abandoned += 1;
                continue;
            }
            let field = |key: &str| {
                r[key]
                    .as_u64()
                    .ok_or_else(|| ReportError(format!("line {}: no {key}", n + 1)))
            };
            let tokens: Option<Vec<u64>> = (r["stream"] == true)
                .then(|| r["token_ns"].as_array())
                .flatten()
                .map(|t| t.iter().filter_map(Value::as_u64).collect());
            completed.push((field("seq")?, field("arrival_ns")?, field("sent_ns")?, tokens));
        }
        completed.sort_by_key(|c| c.0);
        let times: Vec<Completed> = completed
            .iter()
            .map(|&(seq, arrival_ns, sent_ns, _)| Completed {
                seq,
                arrival_ns,
                sent_ns,
            })
            .collect();
        let runs = find_runs(&arrivals_ns, &stalls_ns);
        let truth = open_loop_truth_by_run(&times, &runs, period_ns)
            .ok_or_else(|| ReportError("the log has no completed request".to_owned()))?;
        // The timeline starts at the first run's schedule origin.
        let origin_ns = truth[0].0;
        let measured_rate_per_s = runs
            .iter()
            .max_by_key(|run| (run.end_ns - run.start_ns, run.arrivals))
            .and_then(|longest| {
                let mut in_run: Vec<u64> = arrivals_ns
                    .iter()
                    .copied()
                    .filter(|t| (longest.start_ns..=longest.end_ns).contains(t))
                    .collect();
                in_run.sort_unstable();
                let gap = median_gap_ns(&in_run);
                (gap > 0).then(|| NS_PER_S / gap as f64)
            });

        let requests = completed
            .into_iter()
            .zip(truth)
            .map(|((seq, arrival, sent, tokens), (origin, truth_ns))| {
                let intended = origin + seq as i64 * period_ns;
                let stream = tokens.filter(|t| !t.is_empty()).map(|t| {
                    let n = t.len();
                    StreamTimes {
                        ttft_truth_ns: t[0] as i64 - intended,
                        ttft_server_ns: t[0] as i64 - arrival as i64,
                        max_gap_ns: t.windows(2).map(|w| w[1] as i64 - w[0] as i64).max(),
                        mean_itl_ns: (n > 1).then(|| (t[n - 1] - t[0]) as f64 / (n - 1) as f64),
                    }
                });
                Request {
                    seq,
                    intended_ns: intended - origin_ns,
                    truth_ns,
                    server_ns: sent as i64 - arrival as i64,
                    stream,
                }
            })
            .collect();
        Ok(Self {
            manifest,
            period_ns,
            origin_ns,
            requests,
            abandoned,
            stalls_ns,
            arrivals_ns,
            runs,
            measured_rate_per_s,
            ignored_incomplete_line,
        })
    }

    /// Whether the load generator kept sending through the first stall
    /// window (the `--live` panel's check, on the logged arrivals); `None`
    /// without a stall.
    pub fn verdict(&self) -> Option<Verdict> {
        Some(self.first_stall_counts()?.verdict())
    }

    fn first_stall_counts(&self) -> Option<StallCounts> {
        let &(start, end) = self.stalls_ns.first()?;
        Some(StallCounts::from_arrivals(start, end, self.arrivals_ns.iter().copied()))
    }

    /// Whether the log has a stall window that no traffic preceded (fewer
    /// than [`MIN_ARRIVALS_BEFORE`] arrivals in the interval before it), so
    /// that a verdict on it would rest on nothing (`--live` shows none).
    pub fn stall_passed_unobserved(&self) -> bool {
        self.first_stall_counts()
            .is_some_and(|c| c.before < MIN_ARRIVALS_BEFORE)
    }

    /// Truth p99 over server-time p99, if server time's is above zero.
    pub fn ratio(&self) -> Option<f64> {
        let server = self.server().p99();
        (server > 0).then(|| self.truth().p99() as f64 / server as f64)
    }

    /// Percentiles of open-loop truth.
    pub fn truth(&self) -> Percentiles {
        Percentiles::of(self.requests.iter().map(|r| r.truth_ns)).expect("a report has a completed request")
    }

    /// Percentiles of server time.
    pub fn server(&self) -> Percentiles {
        Percentiles::of(self.requests.iter().map(|r| r.server_ns)).expect("a report has a completed request")
    }

    fn streams(&self) -> impl Iterator<Item = (u64, &StreamTimes)> {
        self.requests
            .iter()
            .filter_map(|r| r.stream.as_ref().map(|s| (r.seq, s)))
    }

    /// The rows of the percentile table: (name, percentiles).
    pub fn table(&self) -> Vec<(&'static str, Percentiles)> {
        let mut rows = vec![("truth", self.truth()), ("server", self.server())];
        let mut add = |name, values: Vec<i64>| {
            if let Some(p) = Percentiles::of(values) {
                rows.push((name, p));
            }
        };
        add("TTFT truth", self.streams().map(|(_, s)| s.ttft_truth_ns).collect());
        add("TTFT server", self.streams().map(|(_, s)| s.ttft_server_ns).collect());
        add(
            "ITL max gap",
            self.streams().filter_map(|(_, s)| s.max_gap_ns).collect(),
        );
        add(
            "ITL mean",
            self.streams()
                .filter_map(|(_, s)| s.mean_itl_ns.map(|m| m.round() as i64))
                .collect(),
        );
        rows
    }

    /// The streamed request with the largest gap: (seq, its max gap, its mean
    /// ITL), if any request has two tokens.
    pub fn largest_gap(&self) -> Option<(u64, i64, f64)> {
        self.streams()
            .filter_map(|(seq, s)| Some((seq, s.max_gap_ns?, s.mean_itl_ns?)))
            .max_by_key(|&(_, gap, _)| gap)
    }

    /// The x extent of the timeline: from the schedule start to one period
    /// past the last intended send.
    fn span_ns(&self) -> i64 {
        self.requests.last().map_or(0, |r| r.intended_ns) + self.period_ns
    }

    /// Stall windows in timeline coordinates (from the schedule start).
    fn stalls_x(&self) -> impl Iterator<Item = (i64, i64)> + '_ {
        self.stalls_ns
            .iter()
            .map(|&(s, e)| (s as i64 - self.origin_ns, e as i64 - self.origin_ns))
    }

    /// The per-column maxima of (truth, server) over `cols` columns of the
    /// timeline; `None` for a column with no request.
    fn columns(&self, cols: usize) -> Vec<Option<(i64, i64)>> {
        let span = self.span_ns().max(1) as i128;
        let mut out: Vec<Option<(i64, i64)>> = vec![None; cols];
        for r in &self.requests {
            let c = ((r.intended_ns.max(0) as i128 * cols as i128 / span) as usize).min(cols - 1);
            let cell = out[c].get_or_insert((i64::MIN, i64::MIN));
            *cell = (cell.0.max(r.truth_ns), cell.1.max(r.server_ns));
        }
        out
    }

    /// A block chart of latency against intended send time, `width` columns
    /// wide (at least 60) and `rows` rows high, plus a stall row, an axis, x
    /// labels and a legend. Each column is the maximum over its requests.
    pub fn chart(&self, width: usize, rows: usize) -> String {
        let width = width.max(60);
        let rows = rows.max(2);
        let truth_max = self.truth().max().max(1);
        let y_labels: Vec<String> = (0..=rows)
            .map(|r| {
                let labelled = r == 0 || r == rows || r == rows / 2;
                if labelled {
                    fmt_ms(truth_max as f64 * r as f64 / rows as f64)
                } else {
                    String::new()
                }
            })
            .collect();
        let gutter = y_labels
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0)
            .max("stall".len());
        // gutter, a space, the axis character, the plot.
        let plot = width - gutter - 2;
        let cols = self.columns(plot);
        let mut out = String::new();
        out.push_str("latency vs intended send time (max per column)\n");
        for r in (0..rows).rev() {
            let label = &y_labels[r + 1];
            let axis = if label.is_empty() { '│' } else { '┤' };
            let _ = write!(out, "{label:>gutter$} {axis}");
            // A cell is filled when the value reaches its midpoint.
            let threshold = truth_max as f64 * (r as f64 + 0.5) / rows as f64;
            let line: String = cols
                .iter()
                .map(|c| match c {
                    Some((_, server)) if *server as f64 >= threshold => '█',
                    Some((truth, _)) if *truth as f64 >= threshold => '░',
                    _ => ' ',
                })
                .collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        let _ = writeln!(out, "{:>gutter$} ┼{}", y_labels[0], "─".repeat(plot));
        let span = self.span_ns().max(1);
        let stall_row: String = (0..plot)
            .map(|c| {
                let (lo, hi) = (span * c as i64 / plot as i64, span * (c as i64 + 1) / plot as i64);
                if self.stalls_x().any(|(s, e)| s < hi && e > lo) {
                    '█'
                } else {
                    '·'
                }
            })
            .collect();
        let _ = writeln!(out, "{:>gutter$}  {stall_row}", "stall");
        let mut x_axis = vec![' '; plot];
        for (at, label) in [
            (0, fmt_s(0.0)),
            (plot / 2, fmt_s(span as f64 / 2.0)),
            (plot, fmt_s(span as f64)),
        ] {
            let len = label.chars().count();
            let start = at.saturating_sub(len / 2).min(plot - len.min(plot));
            for (k, ch) in label.chars().enumerate() {
                if let Some(cell) = x_axis.get_mut(start + k) {
                    *cell = ch;
                }
            }
        }
        let x_axis: String = x_axis.into_iter().collect();
        let _ = writeln!(out, "{:gutter$}  {}", "", x_axis.trim_end());
        let _ = writeln!(out, "{:gutter$}  █ server time (arrival → sent)", "");
        let _ = writeln!(out, "{:gutter$}  ░ open-loop truth (intended send → sent)", "");
        out
    }

    /// The terminal report: a header, the chart, and the percentile table.
    pub fn text(&self, log_name: &str, width: usize) -> String {
        let width = width.min(MAX_CHART_WIDTH);
        let mut out = String::new();
        let rate = NS_PER_S / self.period_ns as f64;
        let _ = writeln!(
            out,
            "phantom report  {log_name}  rate {} /s (period {})",
            trim_float(rate),
            fmt_ms(self.period_ns as f64)
        );
        let _ = writeln!(
            out,
            "answered {}   abandoned {}   stalls {}   runs {}",
            self.requests.len(),
            self.abandoned,
            if self.stalls_ns.is_empty() {
                "none".to_owned()
            } else {
                self.stalls_ns
                    .iter()
                    .map(|&(s, e)| format!("{}–{}", fmt_s(s as f64), fmt_s(e as f64)))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            self.runs.len()
        );
        let mut notes = Vec::new();
        if self.ignored_incomplete_line {
            notes.push("ignored 1 incomplete final line".to_owned());
        }
        if self.runs.len() > 1 {
            let gaps: Vec<String> = self
                .runs
                .windows(2)
                .map(|w| fmt_s((w[1].start_ns - w[0].end_ns) as f64))
                .collect();
            notes.push(format!(
                "{} separate runs found (gaps of {}); truth is computed per run. For a clean result use a fresh phantom per run.",
                self.runs.len(),
                gaps.join(", ")
            ));
        }
        if let Some(measured) = self.measured_rate_per_s {
            let rate = NS_PER_S / self.period_ns as f64;
            notes.push(format!(
                "arrival rate in the log: {} /s (median inter-arrival, longest run)",
                trim_float(measured)
            ));
            if (measured - rate).abs() > 0.2 * rate {
                notes.push(format!(
                    "warning: the log's arrival rate is {} /s but --rate is {} /s; truth assumes --rate",
                    trim_float(measured),
                    trim_float(rate)
                ));
            }
        }
        if self.stall_passed_unobserved() {
            notes.push(STALL_UNOBSERVED.to_owned());
        }
        for note in notes {
            for line in wrap(&note, width) {
                out.push_str(&line);
                out.push('\n');
            }
        }
        out.push('\n');
        out.push_str(&self.chart(width, 8));
        out.push('\n');
        let rows = self.table();
        let name_w = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
        let _ = write!(out, "{:name_w$}", "");
        for (q, _) in QUANTILES {
            let _ = write!(out, "  {q:>11}");
        }
        out.push('\n');
        for (name, p) in &rows {
            let _ = write!(out, "{name:name_w$}");
            for v in p.0 {
                let _ = write!(out, "  {:>11}", fmt_ms(v as f64));
            }
            out.push('\n');
        }
        if let Some((seq, gap, mean)) = self.largest_gap() {
            let _ = writeln!(
                out,
                "largest gap: seq {seq}, {} between two tokens, while its mean ITL is {}",
                fmt_ms(gap as f64),
                fmt_ms(mean)
            );
        }
        out
    }

    /// One self-contained HTML page: an inline SVG timeline of truth and
    /// server time with the stall windows shaded, the percentile table and
    /// the manifest's script. No scripts, no external resources.
    pub fn html(&self, log_name: &str) -> String {
        let (truth, server) = (self.truth(), self.server());
        let mut out = String::new();
        let _ = write!(
            out,
            r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>phantom report</title>
<style>
:root {{ --bg: #ffffff; --fg: #1b1b1b; --muted: #666666; --grid: #e4e4e4; --truth: #d1242f; --server: #1f5fbf; --ok: #1a7f37; --stall: rgba(0, 0, 0, 0.08); }}
@media (prefers-color-scheme: dark) {{
  :root {{ --bg: #16181b; --fg: #e8e8e8; --muted: #9a9a9a; --grid: #2e3236; --truth: #ff6b6b; --server: #6ea8ff; --ok: #4ac26b; --stall: rgba(255, 255, 255, 0.10); }}
}}
body {{ background: var(--bg); color: var(--fg); font: 15px/1.5 system-ui, sans-serif; margin: 0 auto; max-width: 980px; padding: 16px; }}
h1 {{ font-size: 20px; margin: 0 0 4px; }}
h2 {{ font-size: 16px; margin: 24px 0 8px; }}
p.meta {{ color: var(--muted); margin: 0 0 16px; overflow-wrap: anywhere; }}
svg {{ width: 100%; height: auto; display: block; }}
svg text {{ fill: var(--muted); font-size: 12px; }}
.legend {{ display: flex; flex-wrap: wrap; gap: 16px; margin: 8px 0 0; }}
.key {{ display: inline-block; width: 14px; height: 3px; vertical-align: middle; margin-right: 6px; }}
.scroll {{ overflow-x: auto; }}
table {{ border-collapse: collapse; font-variant-numeric: tabular-nums; }}
th, td {{ padding: 4px 12px; text-align: right; border-bottom: 1px solid var(--grid); white-space: nowrap; }}
th:first-child, td:first-child {{ text-align: left; }}
code {{ overflow-wrap: anywhere; }}
.hero {{ display: flex; flex-wrap: wrap; gap: 16px 48px; margin: 16px 0 8px; }}
.hero .label {{ font-size: 13px; font-weight: 600; letter-spacing: 0.06em; text-transform: uppercase; color: var(--muted); }}
.hero .big {{ font-size: 56px; font-weight: 700; line-height: 1.1; font-variant-numeric: tabular-nums; }}
.hero .sub {{ font-size: 13px; color: var(--muted); }}
p.verdict {{ font-size: 17px; font-weight: 600; margin: 8px 0 20px; }}
</style>
</head>
<body>
<h1>phantom report</h1>
<p class="meta">{log} · rate {rate} /s (period {period}) · answered {answered} · abandoned {abandoned}</p>
"#,
            log = escape(log_name),
            rate = trim_float(NS_PER_S / self.period_ns as f64),
            period = fmt_ms(self.period_ns as f64),
            answered = self.requests.len(),
            abandoned = self.abandoned,
        );
        out.push_str(&self.hero());
        out.push_str(&self.svg());
        let _ = write!(
            out,
            r#"<div class="legend">
<span><span class="key" style="background: var(--truth)"></span>open-loop truth (intended send → sent), p99 {}</span>
<span><span class="key" style="background: var(--server)"></span>server time (arrival → sent), p99 {}</span>
<span><span class="key" style="background: var(--stall); height: 12px"></span>stall window</span>
</div>
"#,
            fmt_ms(truth.p99() as f64),
            fmt_ms(server.p99() as f64)
        );
        out.push_str("<h2>Percentiles (nearest rank)</h2>\n<div class=\"scroll\"><table>\n<tr><th></th>");
        for (q, _) in QUANTILES {
            let _ = write!(out, "<th>{q}</th>");
        }
        out.push_str("</tr>\n");
        for (name, p) in self.table() {
            let _ = write!(out, "<tr><td>{name}</td>");
            for v in p.0 {
                let _ = write!(out, "<td>{}</td>", fmt_ms(v as f64));
            }
            out.push_str("</tr>\n");
        }
        out.push_str("</table></div>\n");
        if let Some((seq, gap, mean)) = self.largest_gap() {
            let _ = writeln!(
                out,
                "<p>Largest gap: seq {seq}, {} between two tokens, while its mean ITL is {}.</p>",
                fmt_ms(gap as f64),
                fmt_ms(mean)
            );
        }
        out.push_str("<h2>Script (from the log's manifest)</h2>\n<div class=\"scroll\"><table>\n");
        if let Some(fields) = self.manifest.as_object() {
            for (key, value) in fields.iter().filter(|(k, _)| *k != "type") {
                let shown = match key.as_str() {
                    "stalls" if !self.stalls_ns.is_empty() => self
                        .stalls_ns
                        .iter()
                        .map(|&(s, e)| format!("{}–{} ms", s / 1_000_000, e / 1_000_000))
                        .collect::<Vec<_>>()
                        .join(", "),
                    _ => match value {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    },
                };
                let _ = writeln!(
                    out,
                    "<tr><td>{}</td><td><code>{}</code></td></tr>",
                    escape(key),
                    escape(&shown)
                );
            }
        }
        out.push_str("</table></div>\n</body>\n</html>\n");
        out
    }

    /// The two p99s side by side, the right one red when it is more than
    /// [`RED_RATIO`] times the left, and the verdict sentence.
    fn hero(&self) -> String {
        let (server, truth) = (self.server().p99(), self.truth().p99());
        let ratio = self.ratio();
        let users = if ratio.is_some_and(|r| r > RED_RATIO) {
            "--truth"
        } else {
            "--ok"
        };
        let mut out = format!(
            r#"<div class="hero">
<div><div class="label">your tool reported</div><div class="big">{}</div><div class="sub">p99 of server time (arrival → sent)</div></div>
<div><div class="label">users waited</div><div class="big" style="color: var({users})">{}</div><div class="sub">p99 of open-loop truth (intended send → sent)</div></div>
</div>
"#,
            fmt_p99(Some(server)),
            fmt_p99(Some(truth)),
        );
        if let Some(verdict) = self.verdict() {
            let color = match verdict {
                Verdict::Stopped => "--truth",
                Verdict::KeptSending => "--ok",
            };
            let _ = writeln!(
                out,
                r#"<p class="verdict" style="color: var({color})">{}</p>"#,
                escape(&verdict_sentence(verdict, ratio))
            );
        }
        out
    }

    /// The timeline: truth and server time against intended send time.
    fn svg(&self) -> String {
        const W: f64 = 960.0;
        const H: f64 = 360.0;
        let (left, right, top, bottom) = (64.0, 16.0, 12.0, 40.0);
        let (pw, ph) = (W - left - right, H - top - bottom);
        let span = self.span_ns().max(1) as f64;
        let y_max = nice_ceil(self.truth().max().max(1) as f64);
        let x = |ns: f64| left + ns.clamp(0.0, span) / span * pw;
        let y = |ns: f64| top + ph - ns / y_max * ph;

        let mut out = String::new();
        let _ = writeln!(
            out,
            r#"<svg viewBox="0 0 {W} {H}" role="img" aria-label="latency against intended send time">"#
        );
        for (s, e) in self.stalls_x() {
            let (x0, x1) = (x(s as f64), x(e as f64));
            if x1 > x0 {
                let _ = writeln!(
                    out,
                    r#"<rect x="{x0:.1}" y="{top}" width="{:.1}" height="{ph}" style="fill: var(--stall)"/>"#,
                    x1 - x0
                );
            }
        }
        let y_step = y_max / 4.0;
        for k in 0..=4 {
            let v = y_step * k as f64;
            let yy = y(v);
            let _ = writeln!(
                out,
                r#"<line x1="{left}" x2="{:.1}" y1="{yy:.1}" y2="{yy:.1}" style="stroke: var(--grid)"/><text x="{:.1}" y="{:.1}" text-anchor="end">{}</text>"#,
                left + pw,
                left - 6.0,
                yy + 4.0,
                fmt_ms(v)
            );
        }
        let x_step = nice_step(span / 6.0);
        let mut t = 0.0;
        while t <= span + 1.0 {
            let xx = x(t);
            let _ = writeln!(
                out,
                r#"<line x1="{xx:.1}" x2="{xx:.1}" y1="{:.1}" y2="{:.1}" style="stroke: var(--grid)"/><text x="{xx:.1}" y="{:.1}" text-anchor="middle">{}</text>"#,
                top + ph,
                top + ph + 4.0,
                top + ph + 18.0,
                fmt_s(t)
            );
            t += x_step;
        }
        let _ = writeln!(
            out,
            r#"<text x="{:.1}" y="{:.1}" text-anchor="middle">intended send time</text>"#,
            left + pw / 2.0,
            H - 4.0
        );
        // Every request, or the maximum per column when there are many.
        let points: Vec<(f64, i64, i64)> = if self.requests.len() <= HTML_MAX_POINTS {
            self.requests
                .iter()
                .map(|r| (r.intended_ns as f64, r.truth_ns, r.server_ns))
                .collect()
        } else {
            let cols = 1_000;
            self.columns(cols)
                .into_iter()
                .enumerate()
                .filter_map(|(c, v)| v.map(|(tr, sv)| ((c as f64 + 0.5) / cols as f64 * span, tr, sv)))
                .collect()
        };
        for (var, pick) in [("--truth", 0), ("--server", 1)] {
            let path: Vec<String> = points
                .iter()
                .map(|&(t, tr, sv)| {
                    let v = if pick == 0 { tr } else { sv };
                    format!("{:.1},{:.1}", x(t), y(v as f64))
                })
                .collect();
            let _ = writeln!(
                out,
                r#"<polyline points="{}" style="fill: none; stroke: var({var}); stroke-width: 1.5; stroke-linejoin: round"/>"#,
                path.join(" ")
            );
        }
        out.push_str("</svg>\n");
        out
    }
}

/// Milliseconds with three significant figures or so: `0.123 ms`, `12.3 ms`,
/// `2500 ms`.
pub fn fmt_ms(ns: f64) -> String {
    let ms = ns / NS_PER_MS;
    match ms.abs() {
        0.0 => "0 ms".to_owned(),
        a if a >= 100.0 => format!("{ms:.0} ms"),
        a if a >= 10.0 => format!("{ms:.1} ms"),
        a if a >= 1.0 => format!("{ms:.2} ms"),
        _ => format!("{ms:.3} ms"),
    }
}

/// Seconds with one decimal: `2.5 s`.
pub fn fmt_s(ns: f64) -> String {
    format!("{:.1} s", ns / NS_PER_S)
}

/// `100` for 100.0, `33.3` for 33.333.
fn trim_float(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

/// The smallest 1, 2 or 5 times a power of ten that is >= v.
fn nice_step(v: f64) -> f64 {
    let p = 10f64.powf(v.log10().floor());
    [1.0, 2.0, 5.0, 10.0]
        .into_iter()
        .map(|m| m * p)
        .find(|&s| s >= v)
        .unwrap_or(10.0 * p)
}

/// A round axis maximum >= v whose quarters are round too.
fn nice_ceil(v: f64) -> f64 {
    let step = nice_step(v / 4.0);
    (v / step).ceil().max(1.0) * step
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::fmt_ratio;

    const MS: u64 = 1_000_000;

    /// Five requests every 10 ms, stall 20:10. Arrivals 0, 10, 25, 30, 40 ms,
    /// so origin = min(0, 0, 5, 0, 0) = 0; seq 2 arrives 5 ms late and is
    /// held to 30 ms; seq 3 is abandoned.
    fn log() -> String {
        let manifest = r#"{"type":"manifest","phantom_version":"0.2.0","seed":1,"service":{"kind":"fixed","ms":1},"stalls":[{"start_ms":20,"len_ms":10}]}"#;
        let req = |seq: u64, arrival: u64, sent: u64| {
            format!(
                r#"{{"type":"req","seq":{seq},"arrival_ns":{},"scripted_service_ns":1000000,"stall_wait_ns":0,"sent_ns":{}}}"#,
                arrival * MS,
                sent * MS
            )
        };
        [
            manifest.to_owned(),
            req(0, 0, 1),
            req(1, 10, 11),
            req(2, 25, 31),
            r#"{"type":"req","seq":3,"completed":false,"arrival_ns":30000000,"abandoned_ns":30500000}"#.to_owned(),
            req(4, 40, 41),
        ]
        .join("\n")
    }

    #[test]
    fn truth_is_measured_from_the_intended_send_time() {
        let report = Report::from_log(&log(), 10 * MS as i64).unwrap();
        assert_eq!(report.origin_ns, 0);
        assert_eq!(report.abandoned, 1);
        assert_eq!(report.stalls_ns, vec![(20 * MS, 30 * MS)]);
        let seqs: Vec<u64> = report.requests.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, [0, 1, 2, 4]);
        let truth: Vec<i64> = report.requests.iter().map(|r| r.truth_ns).collect();
        let server: Vec<i64> = report.requests.iter().map(|r| r.server_ns).collect();
        let ms = |v: &[i64]| v.iter().map(|&x| x * MS as i64).collect::<Vec<_>>();
        assert_eq!(truth, ms(&[1, 1, 11, 1]));
        assert_eq!(server, ms(&[1, 1, 6, 1]));
    }

    #[test]
    fn origin_is_the_least_lagged_request() {
        // Request 0 arrives 3 ms late, request 1 on time: origin is 0, not 3.
        assert_eq!(schedule_origin([(0, 3), (1, 10), (2, 22)], 10), Some(0));
        assert_eq!(schedule_origin([], 10), None);
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let values: Vec<i64> = (1..=1000).collect();
        assert_eq!(
            Percentiles::of(values.clone()),
            Some(Percentiles([500, 990, 999, 1000]))
        );
        assert_eq!(Percentiles::of([7]), Some(Percentiles([7, 7, 7, 7])));
        assert_eq!(Percentiles::of([]), None);
        let report = Report::from_log(&log(), 10 * MS as i64).unwrap();
        let ms = MS as i64;
        // Truth 1, 1, 1, 11 ms: p50 is the 2nd, p99 the 4th.
        assert_eq!(report.truth(), Percentiles([ms, 11 * ms, 11 * ms, 11 * ms]));
        assert_eq!(report.server(), Percentiles([ms, 6 * ms, 6 * ms, 6 * ms]));
    }

    #[test]
    fn streams_report_ttft_and_the_gap_the_mean_hides() {
        let log = [
            r#"{"type":"manifest","stalls":[]}"#,
            // Intended 0: tokens at 5, 10, 115, 120 ms. Gaps 5, 105, 5.
            r#"{"type":"req","seq":0,"arrival_ns":0,"sent_ns":121000000,"stream":true,"token_ns":[5000000,10000000,115000000,120000000]}"#,
        ]
        .join("\n");
        let report = Report::from_log(&log, 10 * MS as i64).unwrap();
        let s = report.requests[0].stream.as_ref().unwrap();
        assert_eq!(s.ttft_truth_ns, 5 * MS as i64);
        assert_eq!(s.max_gap_ns, Some(105 * MS as i64));
        assert_eq!(s.mean_itl_ns, Some(115.0 * MS as f64 / 3.0));
        let (seq, gap, _) = report.largest_gap().unwrap();
        assert_eq!((seq, gap), (0, 105 * MS as i64));
        let names: Vec<&str> = report.table().iter().map(|r| r.0).collect();
        assert_eq!(
            names,
            [
                "truth",
                "server",
                "TTFT truth",
                "TTFT server",
                "ITL max gap",
                "ITL mean"
            ]
        );
    }

    #[test]
    fn chart_fits_the_width_and_has_the_rows_asked_for() {
        let report = Report::from_log(&log(), 10 * MS as i64).unwrap();
        for width in [60, 80, 123] {
            let chart = report.chart(width, 8);
            let lines: Vec<&str> = chart.lines().collect();
            // Title, 8 rows, axis, stall row, x labels, two legend lines.
            assert_eq!(lines.len(), 14, "{chart}");
            let longest = lines.iter().map(|l| l.chars().count()).max().unwrap();
            assert!(longest <= width, "width {width}: {longest}\n{chart}");
            assert!(lines[1].contains("11.0 ms"), "{chart}");
            assert!(lines[9].contains('┼'), "{chart}");
            assert!(lines[10].trim_start().starts_with("stall"), "{chart}");
            assert!(lines[10].contains('█') && lines[10].contains('·'), "{chart}");
        }
        // The top row holds only seq 2's truth: above its server time.
        let chart = report.chart(80, 8);
        let top = chart.lines().nth(1).unwrap();
        assert!(top.contains('░') && !top.contains('█'), "{chart}");
        // The bottom row holds server time.
        assert!(chart.lines().nth(8).unwrap().contains('█'), "{chart}");
    }

    #[test]
    fn html_is_self_contained() {
        let report = Report::from_log(&log(), 10 * MS as i64).unwrap();
        let html = report.html("<gt>.jsonl");
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<svg") && html.contains("<polyline") && html.contains("<rect"));
        assert!(html.contains("prefers-color-scheme: dark"));
        assert!(html.contains("p99 11.0 ms") && html.contains("p99 6.00 ms"));
        assert!(html.contains("20–30 ms"));
        assert!(html.contains("&lt;gt&gt;.jsonl"));
        // The hero, above the chart: 6 ms reported, 11 ms waited (under 3×,
        // so green), and a verdict from the logged arrivals around 20–30 ms.
        let hero = html.find(r#"<div class="hero">"#).unwrap();
        assert!(hero < html.find("<svg").unwrap());
        assert!(html.contains(r#"<div class="big">6.00 ms</div>"#), "{html}");
        assert!(
            html.contains(r#"<div class="big" style="color: var(--ok)">11.0 ms</div>"#),
            "{html}"
        );
        assert!(html.contains("✓ Your load tool kept sending. Its numbers include the freeze."));
        assert!(!html.contains("<script") && !html.contains("http://") && !html.contains("https://"));
    }

    #[test]
    fn hero_turns_red_when_the_tool_stopped_sending() {
        // 100 /s, 1 ms service, freeze 400–600 ms (20 arrivals in the 200 ms
        // before it); a closed-loop client sends nothing during it, so its
        // catch-up requests are up to 200 ms late.
        let mut lines = vec![r#"{"type":"manifest","stalls":[{"start_ms":400,"len_ms":200}]}"#.to_owned()];
        let mut done = 0;
        for seq in 0..200u64 {
            let arrival = (seq * 10 * MS).max(done);
            let held = if (400 * MS..600 * MS).contains(&arrival) {
                600 * MS
            } else {
                arrival
            };
            done = held + MS;
            lines.push(format!(
                r#"{{"type":"req","seq":{seq},"arrival_ns":{arrival},"sent_ns":{done}}}"#
            ));
        }
        let report = Report::from_log(&lines.join("\n"), 10 * MS as i64).unwrap();
        assert_eq!(report.verdict(), Some(Verdict::Stopped));
        let html = report.html("gt.jsonl");
        assert!(
            html.contains(r#"<div class="big" style="color: var(--truth)">"#),
            "{html}"
        );
        let ratio = fmt_ratio(report.ratio().unwrap());
        assert!(
            html.contains(&format!("It will under-report p99 by {ratio}×.")),
            "{html}"
        );
        // No stall in the manifest: no verdict.
        let report =
            Report::from_log(&log().replace(r#"[{"start_ms":20,"len_ms":10}]"#, "[]"), 10 * MS as i64).unwrap();
        assert_eq!(report.verdict(), None);
        assert!(!report.html("gt.jsonl").contains(r#"class="verdict""#));
    }

    #[test]
    fn bad_input_is_refused() {
        assert!(parse_rate("0").is_err());
        assert!(parse_rate("-1").is_err());
        assert!(parse_rate("fast").is_err());
        assert_eq!(parse_rate("100"), Ok(10_000_000));
        assert!(Report::from_log("", 1).is_err());
        assert!(Report::from_log(r#"{"type":"req"}"#, 1).is_err());
        assert!(Report::from_log(r#"{"type":"manifest"}"#, 1).is_err());
    }

    /// Runs of open-loop traffic at 100 req/s (a request every 10 ms inside
    /// each `[from, to)` in ms, seq continuing across runs), with the
    /// jitter and service times of the golden single-run values: arrival
    /// `+ (seq % 7) * 100 µs`, service `2 ms + (seq % 5) * 1 ms`.
    fn runs_log(runs: &[(u64, u64)], stalls: &str) -> String {
        let mut lines = vec![format!(r#"{{"type":"manifest","stalls":{stalls}}}"#)];
        let mut seq = 0u64;
        for &(from, to) in runs {
            for t in (from..to).step_by(10) {
                let arrival = t * MS + (seq % 7) * 100_000;
                let sent = arrival + 2 * MS + (seq % 5) * MS;
                lines.push(format!(
                    r#"{{"type":"req","seq":{seq},"arrival_ns":{arrival},"sent_ns":{sent}}}"#
                ));
                seq += 1;
            }
        }
        lines.join("\n") + "\n"
    }

    const PERIOD: i64 = 10 * MS as i64;

    #[test]
    fn one_run_is_what_it_was_golden() {
        let report = Report::from_log(&runs_log(&[(0, 3_000)], "[]"), PERIOD).unwrap();
        assert_eq!(report.runs.len(), 1);
        assert_eq!(report.origin_ns, 0);
        // Hand-computed: truth = 2 ms + (i % 7) * 0.1 ms + (i % 5) * 1 ms.
        assert_eq!(report.truth().0, [4_300_000, 6_600_000, 6_600_000, 6_600_000]);
        assert_eq!(report.server().0, [4_000_000, 6_000_000, 6_000_000, 6_000_000]);
        // And exactly the single-origin formula, request for request.
        let times: Vec<Completed> = report
            .requests
            .iter()
            .map(|r| Completed {
                seq: r.seq,
                arrival_ns: (r.seq * 10 * MS + (r.seq % 7) * 100_000),
                sent_ns: (r.seq * 10 * MS + (r.seq % 7) * 100_000) + 2 * MS + (r.seq % 5) * MS,
            })
            .collect();
        let (origin, truth) = open_loop_truth(&times, PERIOD).unwrap();
        assert_eq!(origin, report.origin_ns);
        assert_eq!(truth, report.requests.iter().map(|r| r.truth_ns).collect::<Vec<_>>());
        let text = report.text("one.jsonl", 100);
        assert!(
            text.contains("answered 300   abandoned 0   stalls none   runs 1"),
            "{text}"
        );
        assert!(!text.contains("separate runs"), "{text}");
    }

    #[test]
    fn two_runs_with_a_gap_are_scored_per_run() {
        // Two 3 s runs at 100 req/s on one phantom, 6 s apart. Over one
        // schedule the second run is ~6 s late.
        let text = runs_log(&[(0, 3_000), (9_000, 12_000)], "[]");
        let report = Report::from_log(&text, PERIOD).unwrap();
        assert_eq!(report.runs.len(), 2, "{:?}", report.runs);
        assert_eq!((report.runs[0].arrivals, report.runs[1].arrivals), (300, 300));
        // Pooled over both runs, the same numbers as one run alone.
        assert_eq!(report.truth().0, [4_300_000, 6_600_000, 6_600_000, 6_600_000]);
        assert_eq!(report.server().p99(), 6_000_000);
        let one_origin = open_loop_truth(
            &report
                .arrivals_ns
                .iter()
                .zip(&report.requests)
                .map(|(&a, r)| Completed {
                    seq: r.seq,
                    arrival_ns: a,
                    sent_ns: a + 2 * MS + (r.seq % 5) * MS,
                })
                .collect::<Vec<_>>(),
            PERIOD,
        )
        .unwrap()
        .1;
        assert!(
            one_origin.iter().max().unwrap() > &(5_900 * MS as i64),
            "the failure this fixes"
        );
        let out = report.text("two.jsonl", 100);
        assert!(out.contains("runs 2"), "{out}");
        assert!(
            out.replace("\n  ", " ").contains(
                "2 separate runs found (gaps of 6.0 s); truth is computed per run. \
For a clean result use a fresh phantom per run."
            ),
            "{out}"
        );
        // The timeline starts at the first run's origin and covers the gap.
        let last = report.requests.last().unwrap();
        assert!(
            (11_000 * MS as i64..=12_000 * MS as i64).contains(&last.intended_ns),
            "{}",
            last.intended_ns
        );
    }

    #[test]
    fn three_runs() {
        let text = runs_log(&[(0, 2_000), (5_000, 7_000), (30_000, 32_000)], "[]");
        let report = Report::from_log(&text, PERIOD).unwrap();
        assert_eq!(report.runs.len(), 3, "{:?}", report.runs);
        assert_eq!(
            report.runs.iter().map(|r| r.arrivals).collect::<Vec<_>>(),
            [200, 200, 200]
        );
        assert_eq!(report.truth().p99(), 6_600_000);
        let out = report.text("three.jsonl", 100).replace("\n  ", " ");
        assert!(out.contains("3 separate runs found (gaps of 3.0 s, 23.0 s)"), "{out}");
    }

    #[test]
    fn what_ends_a_run() {
        let at = |ms: &[u64]| ms.iter().map(|m| m * MS).collect::<Vec<_>>();
        // 100 req/s: a gap over 1 s ends a run, 1 s itself does not.
        let runs = |extra: u64| find_runs(&at(&[0, 10, 20, 30, 30 + extra, 40 + extra]), &[]);
        assert_eq!(runs(1_000).len(), 1);
        assert_eq!(runs(1_001).len(), 2);
        // 10 req/s: the threshold is 20 x 100 ms = 2 s.
        let slow = |gap: u64| find_runs(&at(&[0, 100, 200, 300, 300 + gap, 400 + gap]), &[]);
        assert_eq!(slow(1_500).len(), 1);
        assert_eq!(slow(2_001).len(), 2);
        assert_eq!(run_gap_threshold_ns(100 * MS), 2_000 * MS);
        assert_eq!(run_gap_threshold_ns(MS), MIN_RUN_GAP_NS);
        assert!(find_runs(&[], &[]).is_empty());
        assert_eq!(find_runs(&[5], &[]).len(), 1);
        // A closed-loop client that sent nothing through a 3 s freeze is
        // waiting, not idle; the same gap with no stall is two runs.
        let gap = at(&[0, 10, 20, 990, 4_001, 4_011]);
        assert_eq!(find_runs(&gap, &[(1_000 * MS, 4_000 * MS)]).len(), 1);
        assert_eq!(find_runs(&gap, &[]).len(), 2);
        // A stall far from the gap does not excuse it.
        assert_eq!(find_runs(&gap, &[(20_000 * MS, 21_000 * MS)]).len(), 2);
        let far = at(&[0, 10, 20, 30, 7_000, 7_010]);
        assert_eq!(find_runs(&far, &[(5_000 * MS, 5_500 * MS)]).len(), 2);
    }

    #[test]
    fn a_log_cut_off_mid_json_loses_only_its_last_line() {
        let whole = runs_log(&[(0, 1_000)], "[]");
        let cut = format!("{whole}{}", r#"{"type":"req","seq":100,"arrival_ns":1000000"#);
        let report = Report::from_log(&cut, PERIOD).unwrap();
        assert!(report.ignored_incomplete_line);
        assert_eq!(report.requests.len(), 100);
        let text = report.text("cut.jsonl", 100);
        assert!(text.contains("ignored 1 incomplete final line"), "{text}");
        assert!(!Report::from_log(&whole, PERIOD).unwrap().ignored_incomplete_line);
        // A bad line that ends in a newline is damage, not a log in progress;
        // so is a bad line in the middle, and a cut manifest.
        assert!(Report::from_log(&format!("{whole}{{oops\n"), PERIOD).is_err());
        assert!(Report::from_log(&whole.replacen("\"seq\":3,", "{", 1), PERIOD).is_err());
        assert!(Report::from_log(r#"{"type":"manifest","stal"#, PERIOD).is_err());
    }

    #[test]
    fn the_logs_rate_is_measured_and_checked_against_rate() {
        let text = runs_log(&[(0, 3_000)], "[]");
        let report = Report::from_log(&text, PERIOD).unwrap();
        let rate = report.measured_rate_per_s.unwrap();
        assert!((95.0..=105.0).contains(&rate), "{rate}");
        let out = report.text("one.jsonl", 100);
        assert!(out.contains("arrival rate in the log: 99.0 /s"), "{out}");
        assert!(!out.contains("warning"), "{out}");
        // Within 20% of --rate: no warning (110 against a measured ~99).
        let near = Report::from_log(&text, (NS_PER_S / 110.0) as i64).unwrap();
        assert!(!near.text("l", 100).contains("warning"));
        // Further than that: a warning with both numbers.
        let off = Report::from_log(&text, (NS_PER_S / 200.0) as i64)
            .unwrap()
            .text("l", 100);
        assert!(
            off.contains("warning: the log's arrival rate is 99.0 /s but --rate is 200 /s"),
            "{off}"
        );
        // The longest run decides, not the idle time between runs.
        let two = Report::from_log(&runs_log(&[(0, 1_000), (9_000, 12_000)], "[]"), PERIOD).unwrap();
        assert!((95.0..=105.0).contains(&two.measured_rate_per_s.unwrap()));
        // One arrival has no rate.
        let single = Report::from_log(&runs_log(&[(0, 10)], "[]"), PERIOD).unwrap();
        assert_eq!(single.measured_rate_per_s, None);
    }

    #[test]
    fn the_terminal_chart_is_at_most_100_columns() {
        let report = Report::from_log(&runs_log(&[(0, 3_000)], "[]"), PERIOD).unwrap();
        for width in [120, 300] {
            let text = report.text("one.jsonl", width);
            let widest = text.lines().map(|l| l.chars().count()).max().unwrap();
            assert!(widest <= 100, "{width}: {widest}\n{text}");
        }
        let text = report.text("one.jsonl", 100);
        assert!(text.lines().any(|l| l.chars().count() == 100), "{text}");
        // Narrower terminals still get what fit before.
        let text = report.text("one.jsonl", 80);
        assert!(text.lines().map(|l| l.chars().count()).max().unwrap() <= 100);
    }

    #[test]
    fn a_stall_with_no_traffic_before_it_is_flagged() {
        let stall = r#"[{"start_ms":5000,"len_ms":500}]"#;
        let report = Report::from_log(&runs_log(&[(0, 3_000), (9_000, 11_000)], stall), PERIOD).unwrap();
        assert!(report.stall_passed_unobserved());
        let out = report.text("l", 100).replace("\n  ", " ");
        assert!(
            out.contains("stall window passed before any traffic arrived; start a fresh phantom for each run"),
            "{out}"
        );
        let busy = Report::from_log(&runs_log(&[(0, 3_000)], r#"[{"start_ms":1000,"len_ms":500}]"#), PERIOD).unwrap();
        assert!(!busy.stall_passed_unobserved());
        assert!(!busy.text("l", 100).contains("stall window passed"));
    }
}
